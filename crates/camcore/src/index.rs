//! SQLite 段索引。
//!
//! schema（見 [`SegmentIndex::open`]）刻意只留 M1 需要的欄位；`events` 表記錄
//! 所有「非正常」事件：重連 gap、防呆補記、復原修復／丟棄、索引重建。任何補記都要
//! 落地成 event，不得靜默。

use std::path::Path;

use rusqlite::{params, Connection};

use crate::error::Result;

/// 索引內的一筆段資料。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRow {
    /// 段 id（等於檔名去掉 `.mkv`，例如 `seg-000000012000`）。
    pub segment_id: String,
    /// 檔名（含 `.mkv`）。
    pub name: String,
    /// 首個 block 時間（host 單調時鐘，毫秒）。
    pub t_start: u64,
    /// 最後一個 block 時間（毫秒）。
    pub t_end: u64,
    /// 檔案實際 bytes。
    pub bytes: u64,
    /// 首個 keyframe block 的位元組位移。
    pub idr_offset: u64,
    /// 本段開始前的重連／空窗長度（毫秒）。
    ///
    /// `Some(0)`＝確定連續；`Some(n)`＝已知 n ms 空窗；`None`＝**未知**（復原掃描
    /// 收編的段沒有 gap 記錄，不猜 0，免得把斷流謊報成連續）。
    pub gap_ms: Option<u64>,
    /// 是否已正常收段。
    pub closed: bool,
    /// 是否由啟動復原掃描修復。
    pub repaired: bool,
}

/// 索引事件（gap、補記、復原動作）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    /// 事件發生時間（毫秒）。
    pub at_ms: u64,
    /// 事件種類：`gap`、`implicit_gap`、`repaired`、`discarded`、`pruned`、`index_rebuilt`。
    pub kind: String,
    /// 補充說明（可讀）。
    pub detail: String,
}

/// 目前索引 schema 版本（存在 `PRAGMA user_version`）。
///
/// - v1：M1 首版，`segments.gap_ms INTEGER NOT NULL`（沒有 user_version，視為 0/1）。
/// - v2：`gap_ms` 可 NULL（復原收編段「未知」不再假裝 0）。
///
/// 之後 schema 只會繼續長欄位，所以升級一律走 [`SegmentIndex::migrate`]：
/// 讀 `user_version` → 逐級搬資料 → 寫回版本。不要用「臨時判斷某欄長相」的 one-off。
pub const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS segments (
    segment_id  TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    t_start     INTEGER NOT NULL,
    t_end       INTEGER NOT NULL,
    bytes       INTEGER NOT NULL,
    idr_offset  INTEGER NOT NULL,
    gap_ms      INTEGER,
    closed      INTEGER NOT NULL,
    repaired    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS segments_by_start ON segments(t_start);
CREATE TABLE IF NOT EXISTS events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms   INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    detail  TEXT NOT NULL
);
"#;

/// 段索引（單一資料目錄對應一個索引檔）。
pub struct SegmentIndex {
    conn: Connection,
}

impl SegmentIndex {
    /// 開啟（或建立）索引檔。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// 記憶體索引（測試用）。
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// 把既有索引升級到 [`SCHEMA_VERSION`]。
    ///
    /// 流程固定是「讀 `user_version` → 判斷要不要搬資料 → 建立缺少的表 → 寫回版本」，
    /// 每多一版就多一個 `upgrade_N_to_M`，不做事後的臨時判斷。重複開啟是幂等的：
    /// 版本已經是當前值就直接建表收工，`schema_upgrade` 事件只會寫一次。
    fn migrate(conn: &Connection) -> Result<()> {
        let version = Self::user_version(conn)?;
        if version > SCHEMA_VERSION {
            return Err(crate::error::Error::Config(format!(
                "索引 schema 版本 {version} 比程式支援的 {SCHEMA_VERSION} 新，請升級程式（不要降級寫入）"
            )));
        }

        // 沒有 user_version 的舊檔：靠欄位形狀認出 v1（gap_ms NOT NULL）。
        let legacy_v1 = version < 2
            && Self::has_table(conn, "segments")?
            && Self::column_not_null(conn, "segments", "gap_ms")?;

        if legacy_v1 {
            let kept = Self::upgrade_1_to_2(conn)?;
            // 建表（events 可能也缺）後才能寫事件
            conn.execute_batch(SCHEMA)?;
            conn.execute(
                "INSERT INTO events (at_ms, kind, detail) VALUES (?1, ?2, ?3)",
                params![
                    0i64,
                    "schema_upgrade",
                    format!("索引 schema 由 v1 升級到 v{SCHEMA_VERSION}：gap_ms 改為可 NULL，保留 {kept} 筆段")
                ],
            )?;
        } else {
            conn.execute_batch(SCHEMA)?;
        }

        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    }

    fn user_version(conn: &Connection) -> Result<i64> {
        Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    fn has_table(conn: &Connection, table: &str) -> Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    fn column_not_null(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let name: String = r.get(1)?;
            let not_null: i64 = r.get(3)?;
            if name == column {
                return Ok(not_null != 0);
            }
        }
        Ok(false)
    }

    /// v1 → v2：`gap_ms` 由 NOT NULL 改成可 NULL，既有資料原樣搬過去。回傳保留的段數。
    fn upgrade_1_to_2(conn: &Connection) -> Result<i64> {
        let kept: i64 = conn.query_row("SELECT COUNT(*) FROM segments", [], |r| r.get(0))?;
        conn.execute_batch("ALTER TABLE segments RENAME TO segments_v1")?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(
            "INSERT INTO segments (segment_id, name, t_start, t_end, bytes, idr_offset, gap_ms, closed, repaired)
             SELECT segment_id, name, t_start, t_end, bytes, idr_offset, gap_ms, closed, repaired
             FROM segments_v1",
        )?;
        conn.execute_batch("DROP TABLE segments_v1")?;
        Ok(kept)
    }

    /// 寫入或覆蓋一筆段資料。
    pub fn upsert(&self, row: &SegmentRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO segments (segment_id, name, t_start, t_end, bytes, idr_offset, gap_ms, closed, repaired)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(segment_id) DO UPDATE SET
                name=excluded.name, t_start=excluded.t_start, t_end=excluded.t_end,
                bytes=excluded.bytes, idr_offset=excluded.idr_offset, gap_ms=excluded.gap_ms,
                closed=excluded.closed, repaired=excluded.repaired",
            params![
                row.segment_id,
                row.name,
                row.t_start as i64,
                row.t_end as i64,
                row.bytes as i64,
                row.idr_offset as i64,
                row.gap_ms.map(|v| v as i64),
                row.closed as i64,
                row.repaired as i64,
            ],
        )?;
        Ok(())
    }

    /// 全部段，依 `t_start` 排序。
    pub fn rows(&self) -> Result<Vec<SegmentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT segment_id, name, t_start, t_end, bytes, idr_offset, gap_ms, closed, repaired
             FROM segments ORDER BY t_start ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SegmentRow {
                    segment_id: r.get(0)?,
                    name: r.get(1)?,
                    t_start: r.get::<_, i64>(2)? as u64,
                    t_end: r.get::<_, i64>(3)? as u64,
                    bytes: r.get::<_, i64>(4)? as u64,
                    idr_offset: r.get::<_, i64>(5)? as u64,
                    gap_ms: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                    closed: r.get::<_, i64>(7)? != 0,
                    repaired: r.get::<_, i64>(8)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 依 `segment_id` 取一筆。
    pub fn get(&self, segment_id: &str) -> Result<Option<SegmentRow>> {
        Ok(self
            .rows()?
            .into_iter()
            .find(|r| r.segment_id == segment_id))
    }

    /// 段數。
    pub fn count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM segments", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// 段檔總 bytes。
    pub fn total_bytes(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COALESCE(SUM(bytes), 0) FROM segments", [], |r| {
                r.get(0)
            })?;
        Ok(n as u64)
    }

    /// 最舊的一段。
    pub fn oldest(&self) -> Result<Option<SegmentRow>> {
        Ok(self.rows()?.into_iter().next())
    }

    /// 最新段的 `t_end`。
    pub fn latest_end(&self) -> Result<Option<u64>> {
        let v: Option<i64> = self
            .conn
            .query_row("SELECT MAX(t_end) FROM segments", [], |r| r.get(0))?;
        Ok(v.map(|v| v as u64))
    }

    /// 刪掉一筆段資料。
    pub fn delete(&self, segment_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM segments WHERE segment_id = ?1", params![segment_id])?;
        Ok(())
    }

    /// 是否已有任何段（判斷要不要重建索引）。
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.count()? == 0)
    }

    /// 清空段表（重建索引前用）。
    pub fn clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM segments", [])?;
        Ok(())
    }

    /// 寫一筆事件。
    pub fn insert_event(&self, at_ms: u64, kind: &str, detail: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (at_ms, kind, detail) VALUES (?1, ?2, ?3)",
            params![at_ms as i64, kind, detail],
        )?;
        Ok(())
    }

    /// 全部事件，依寫入順序。
    pub fn events(&self) -> Result<Vec<EventRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT at_ms, kind, detail FROM events ORDER BY id ASC")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(EventRow {
                    at_ms: r.get::<_, i64>(0)? as u64,
                    kind: r.get(1)?,
                    detail: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 某種類的事件數。
    pub fn count_events(&self, kind: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE kind = ?1",
            params![kind],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn row(id: &str, t_start: u64, t_end: u64) -> SegmentRow {
        SegmentRow {
            segment_id: id.to_string(),
            name: format!("{id}.mkv"),
            t_start,
            t_end,
            bytes: 1000,
            idr_offset: 200,
            gap_ms: None,
            closed: true,
            repaired: false,
        }
    }

    #[test]
    fn upsert_and_query_roundtrip() {
        let idx = SegmentIndex::open_in_memory().unwrap();
        idx.upsert(&row("seg-000000002000", 2000, 4000)).unwrap();
        idx.upsert(&row("seg-000000000000", 0, 2000)).unwrap();
        let rows = idx.rows().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].t_start, 0, "須依 t_start 排序");
        assert_eq!(idx.total_bytes().unwrap(), 2000);
        assert_eq!(idx.latest_end().unwrap(), Some(4000));
        assert_eq!(idx.oldest().unwrap().unwrap().segment_id, "seg-000000000000");
        // upsert 覆蓋同一 id
        let mut r = row("seg-000000000000", 0, 2000);
        r.gap_ms = Some(1500);
        idx.upsert(&r).unwrap();
        assert_eq!(idx.count().unwrap(), 2);
        assert_eq!(idx.rows().unwrap()[0].gap_ms, Some(1500));
        idx.delete("seg-000000000000").unwrap();
        assert_eq!(idx.count().unwrap(), 1);
    }

    /// 造一個 v1 的索引檔：`gap_ms INTEGER NOT NULL`、沒有 `user_version`。
    fn make_legacy_v1_db(path: &Path) -> i64 {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE segments (
                 segment_id  TEXT PRIMARY KEY,
                 name        TEXT NOT NULL,
                 t_start     INTEGER NOT NULL,
                 t_end       INTEGER NOT NULL,
                 bytes       INTEGER NOT NULL,
                 idr_offset  INTEGER NOT NULL,
                 gap_ms      INTEGER NOT NULL,
                 closed      INTEGER NOT NULL,
                 repaired    INTEGER NOT NULL
             );
             CREATE TABLE events (
                 id      INTEGER PRIMARY KEY AUTOINCREMENT,
                 at_ms   INTEGER NOT NULL,
                 kind    TEXT NOT NULL,
                 detail  TEXT NOT NULL
             );
             INSERT INTO segments VALUES
                 ('seg-000000002000', 'seg-000000002000.mkv', 2000, 4000, 1000, 200, 1500, 1, 0);",
        )
        .unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 0, "v1 的索引檔沒有 user_version");
        version
    }

    fn temp_db(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "sview-index-{tag}-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_file(&p).ok();
        p
    }

    #[test]
    fn opens_legacy_not_null_schema_and_accepts_null_gap() {
        let path = temp_db("legacy");
        make_legacy_v1_db(&path);

        let idx = SegmentIndex::open(&path).unwrap();
        let rows = idx.rows().unwrap();
        assert_eq!(rows.len(), 1, "升級不該丟資料");
        assert_eq!(rows[0].gap_ms, Some(1500), "既有 gap_ms 要原樣保留");

        // 舊 schema 最大的坑：寫 None 會 NOT NULL constraint failed
        let mut r = rows[0].clone();
        r.segment_id = "seg-000000004000".to_string();
        r.name = "seg-000000004000.mkv".to_string();
        r.gap_ms = None;
        idx.upsert(&r).expect("升級後必須能寫 gap_ms = NULL");
        assert_eq!(idx.get("seg-000000004000").unwrap().unwrap().gap_ms, None);

        assert_eq!(
            idx.count_events("schema_upgrade").unwrap(),
            1,
            "升級要留一筆 schema_upgrade 事件"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn migration_is_idempotent() {
        let path = temp_db("idem");
        make_legacy_v1_db(&path);

        for _ in 0..3 {
            let idx = SegmentIndex::open(&path).unwrap();
            assert_eq!(idx.count().unwrap(), 1, "重複開啟不該重複搬或丟列");
            assert_eq!(
                idx.count_events("schema_upgrade").unwrap(),
                1,
                "schema_upgrade 只該寫一次"
            );
        }
        let version: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn fresh_index_is_at_current_version() {
        let idx = SegmentIndex::open_in_memory().unwrap();
        let version: i64 = idx
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(idx.count_events("schema_upgrade").unwrap(), 0);
    }

    #[test]
    fn events_are_recorded_in_order() {
        let idx = SegmentIndex::open_in_memory().unwrap();
        idx.insert_event(61000, "gap", "RST 後 1500 ms 無 RTP").unwrap();
        idx.insert_event(120000, "implicit_gap", "相鄰 NAL 間隔 400 ms")
            .unwrap();
        let events = idx.events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "gap");
        assert_eq!(events[1].at_ms, 120000);
        assert_eq!(idx.count_events("gap").unwrap(), 1);
    }
}
