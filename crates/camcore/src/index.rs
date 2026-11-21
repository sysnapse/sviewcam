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
    /// 段的第一個 block 是否為 keyframe。
    ///
    /// `Some(true)`＝可獨立解碼；`Some(false)`＝延後強切到達硬上限後，從非 keyframe
    /// 硬切出來的段（不可獨立回放）；`None`＝**未知**（v2 以前的舊列或掃描不到），
    /// M2 一律當「不保證可獨立回放」，不得臆測成 true。
    pub first_block_keyframe: Option<bool>,
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
/// - v3：新增 `first_block_keyframe`（可 NULL；NULL＝未知，不臆測）。
///
/// 之後 schema 只會繼續長欄位，所以升級一律走 [`SegmentIndex::migrate`]：
/// 讀 `user_version` → 逐級搬資料 → 寫回版本。不要用「臨時判斷某欄長相」的 one-off。
pub const SCHEMA_VERSION: i64 = 3;

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
    repaired    INTEGER NOT NULL,
    first_block_keyframe INTEGER
);
CREATE INDEX IF NOT EXISTS segments_by_start ON segments(t_start);
CREATE TABLE IF NOT EXISTS events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms   INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    detail  TEXT NOT NULL
);
"#;

/// v2 的 table 形狀（`gap_ms` 可 NULL、沒有 `first_block_keyframe`）。
///
/// 只給 `upgrade_1_to_2` 建立中間表用；正常建表與最新 schema 一律用 [`SCHEMA`]。
const SCHEMA_V2: &str = r#"
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
    /// 流程固定是「讀 `user_version` → 修掉上次沒做完的殘留 → 判斷要不要搬資料 →
    /// 建立缺少的表 → 寫回版本」，每多一版就多一個 `upgrade_N_to_M`，不做事後的臨時判斷。
    /// 整段在同一個 transaction 裡：升級本身就是為了救 crash 窗，不能自己再開一個新的
    /// crash 窗（中途失敗一律 rollback，檔案回到升級前的樣子）。重複開啟幂等，
    /// `schema_upgrade` 事件只寫一次。
    fn migrate(conn: &Connection) -> Result<()> {
        let version = Self::user_version(conn)?;
        if version > SCHEMA_VERSION {
            return Err(crate::error::Error::Config(format!(
                "索引 schema 版本 {version} 比程式支援的 {SCHEMA_VERSION} 新，請升級程式（不要降級寫入）"
            )));
        }

        let tx = conn.unchecked_transaction()?;

        // 上次升級若在 ALTER RENAME 之後斷掉，資料會卡在 segments_v1。
        let repaired_leftover = Self::repair_leftover_upgrade(&tx)?;

        // 靠欄位形狀認出 v1（gap_ms NOT NULL）而不是只看 user_version：上面若把
        // segments_v1 還原回來，表是 v1 形狀、版本號卻可能已經寫成 2，只看版本會漏升級。
        let legacy_v1 = Self::has_table(&tx, "segments")?
            && Self::column_not_null(&tx, "segments", "gap_ms")?;

        if legacy_v1 {
            let kept = Self::upgrade_1_to_2(&tx)?;
            // 這裡的第二次建表是必要的，不是重複：搬資料時 `segments_by_start` 還掛在
            // 改名後的舊表上，`CREATE INDEX IF NOT EXISTS` 會因撞名跳過，等舊表 DROP
            // 掉索引就一起沒了；所以要等舊表消失後再建一次。少了這行，升級後查詢就
            // 默默變成全表掃描而測試不會紅。
            tx.execute_batch(SCHEMA)?;
            tx.execute(
                "INSERT INTO events (at_ms, kind, detail) VALUES (?1, ?2, ?3)",
                params![
                    0i64,
                    "schema_upgrade",
                    format!("索引 schema 由 v1 升級到 v2：gap_ms 改為可 NULL，保留 {kept} 筆段")
                ],
            )?;
        } else {
            tx.execute_batch(SCHEMA)?;
        }

        // v2 → v3：只新增欄位，既有列保持 NULL（未知），不臆測成 true。
        if !Self::has_column(&tx, "segments", "first_block_keyframe")? {
            tx.execute(
                "ALTER TABLE segments ADD COLUMN first_block_keyframe INTEGER",
                [],
            )?;
            tx.execute(
                "INSERT INTO events (at_ms, kind, detail) VALUES (?1, ?2, ?3)",
                params![
                    0i64,
                    "schema_upgrade",
                    "索引 schema 由 v2 升級到 v3：新增 first_block_keyframe（既有列 NULL＝未知，recover 掃到時補真值）"
                ],
            )?;
        }

        if let Some(detail) = repaired_leftover {
            tx.execute(
                "INSERT INTO events (at_ms, kind, detail) VALUES (?1, ?2, ?3)",
                params![0i64, "schema_repair", detail],
            )?;
        }

        if version < SCHEMA_VERSION {
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }

        tx.commit()?;
        Ok(())
    }

    /// 收拾「上次升級做到一半」留下的 `segments_v1`。
    ///
    /// 兩個可能的斷點（`ALTER RENAME` 後、`DROP` 前），一律**取聯集**而不是比筆數
    /// 二選一：半搬時新表可能有舊表沒有的列，以舊表為準會把那些列整份丟掉（Verifier
    /// 實測 `seg-000000200000` 消失）。資料只會多不會少，所以：
    ///
    /// 1. 建 `segments_merged`（v2 形狀，`gap_ms` 可 NULL）；
    /// 2. 先搬現有 `segments`（若存在），再 `INSERT OR IGNORE` `segments_v1`；
    ///    同 `segment_id` 以 `segments` 那列為準（新表時間戳／`repaired` 較新）；
    /// 3. 兩張舊表都 DROP，`segments_merged` 改名回 `segments`。
    ///
    /// `gap_ms` 全程保值：`None` 代表「未知」，不得 `COALESCE` 成 0（那是「確定連續」）。
    /// 回傳要寫進 `schema_repair` 事件的說明（沒有殘留就是 `None`）。
    fn repair_leftover_upgrade(conn: &Connection) -> Result<Option<String>> {
        if !Self::has_table(conn, "segments_v1")? {
            return Ok(None);
        }
        let old_rows = Self::count_table(conn, "segments_v1")?;
        let has_segments = Self::has_table(conn, "segments")?;
        let new_rows = if has_segments {
            Self::count_table(conn, "segments")?
        } else {
            0
        };
        let conflicts = if has_segments {
            conn.query_row(
                "SELECT COUNT(*) FROM segments s
                 JOIN segments_v1 v ON s.segment_id = v.segment_id",
                [],
                |r| r.get(0),
            )?
        } else {
            0
        };

        conn.execute_batch(SEGMENTS_MERGED_DDL)?;
        if has_segments {
            conn.execute_batch(
                "INSERT INTO segments_merged
                 SELECT segment_id, name, t_start, t_end, bytes, idr_offset, gap_ms, closed, repaired
                 FROM segments",
            )?;
            conn.execute_batch(
                "INSERT OR IGNORE INTO segments_merged
                 SELECT segment_id, name, t_start, t_end, bytes, idr_offset, gap_ms, closed, repaired
                 FROM segments_v1",
            )?;
            conn.execute_batch("DROP TABLE segments")?;
        } else {
            // 只做到 ALTER RENAME 就中斷：新表根本還沒建，整份資料都在 segments_v1。
            conn.execute_batch(
                "INSERT INTO segments_merged
                 SELECT segment_id, name, t_start, t_end, bytes, idr_offset, gap_ms, closed, repaired
                 FROM segments_v1",
            )?;
        }
        conn.execute_batch("DROP TABLE segments_v1")?;
        conn.execute_batch("ALTER TABLE segments_merged RENAME TO segments")?;

        let total = Self::count_table(conn, "segments")?;
        Ok(Some(format!(
            "上次 schema 升級中途中斷：舊表 {old_rows} 筆、新表 {new_rows} 筆、同 id 衝突 {conflicts} 筆（取新表那列），聯集後 {total} 筆"
        )))
    }

    fn count_table(conn: &Connection, table: &str) -> Result<i64> {
        Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
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

    fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let name: String = r.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// v1 → v2：`gap_ms` 由 NOT NULL 改成可 NULL，既有資料原樣搬過去。回傳保留的段數。
    fn upgrade_1_to_2(conn: &Connection) -> Result<i64> {
        let kept: i64 = conn.query_row("SELECT COUNT(*) FROM segments", [], |r| r.get(0))?;
        conn.execute_batch("ALTER TABLE segments RENAME TO segments_v1")?;
        conn.execute_batch(SCHEMA_V2)?;
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
            "INSERT INTO segments (segment_id, name, t_start, t_end, bytes, idr_offset, gap_ms, closed, repaired, first_block_keyframe)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(segment_id) DO UPDATE SET
                name=excluded.name, t_start=excluded.t_start, t_end=excluded.t_end,
                bytes=excluded.bytes, idr_offset=excluded.idr_offset, gap_ms=excluded.gap_ms,
                closed=excluded.closed, repaired=excluded.repaired,
                first_block_keyframe=excluded.first_block_keyframe",
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
                row.first_block_keyframe.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    /// 全部段，依 `t_start` 排序。
    pub fn rows(&self) -> Result<Vec<SegmentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT segment_id, name, t_start, t_end, bytes, idr_offset, gap_ms, closed, repaired, first_block_keyframe
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
                    first_block_keyframe: r.get::<_, Option<i64>>(9)?.map(|v| v != 0),
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

/// 殘骸修復用的暫存表（v2 形狀）：先把兩邊的列聯集進來，再換掉舊表。
const SEGMENTS_MERGED_DDL: &str = r#"
CREATE TABLE segments_merged (
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
"#;

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
            first_block_keyframe: Some(true),
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
             CREATE INDEX segments_by_start ON segments(t_start);
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

    /// 造一個 v2 的索引檔：`gap_ms` 可 NULL、沒有 `first_block_keyframe`、`user_version=2`。
    fn make_legacy_v2_db(path: &Path) -> i64 {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute(
            "INSERT INTO segments VALUES
                 ('seg-000000002000', 'seg-000000002000.mkv', 2000, 4000, 1000, 200, 1500, 1, 0)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 2i64).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
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
        assert_eq!(
            rows[0].first_block_keyframe, None,
            "v1 舊列沒有旗標可搬，必須是 NULL＝未知，不得臆測成 true"
        );

        // 舊 schema 最大的坑：寫 None 會 NOT NULL constraint failed
        let mut r = rows[0].clone();
        r.segment_id = "seg-000000004000".to_string();
        r.name = "seg-000000004000.mkv".to_string();
        r.gap_ms = None;
        idx.upsert(&r).expect("升級後必須能寫 gap_ms = NULL");
        assert_eq!(idx.get("seg-000000004000").unwrap().unwrap().gap_ms, None);

        assert_eq!(
            idx.count_events("schema_upgrade").unwrap(),
            2,
            "v1→v2、v2→v3 各留一筆 schema_upgrade 事件"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn migration_is_idempotent() {
        let path = temp_db("idem");
        make_legacy_v1_db(&path);

        let mut first_upgrade_events = None;
        for _ in 0..3 {
            let idx = SegmentIndex::open(&path).unwrap();
            assert_eq!(idx.count().unwrap(), 1, "重複開啟不該重複搬或丟列");
            let upgrades = idx.count_events("schema_upgrade").unwrap();
            assert_eq!(upgrades, 2, "v1→v2、v2→v3 各一筆，重複開啟不再寫");
            assert!(
                first_upgrade_events.is_none_or(|n| n == upgrades),
                "schema_upgrade 事件數不得隨開啟次數增加"
            );
            first_upgrade_events = Some(upgrades);
        }
        let version: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        std::fs::remove_file(&path).ok();
    }

    /// v2 → v3：只新增 nullable 欄位，既有列保持 NULL（未知），資料與 gap 原樣保留；
    /// 升級後能寫 Some(false)（不可獨立回放）與 None。
    #[test]
    fn upgrades_v2_to_v3_without_guessing_flag() {
        let path = temp_db("v2v3");
        make_legacy_v2_db(&path);

        let idx = SegmentIndex::open(&path).unwrap();
        let rows = idx.rows().unwrap();
        assert_eq!(rows.len(), 1, "升級不該丟資料");
        assert_eq!(rows[0].gap_ms, Some(1500), "既有 gap_ms 要原樣保留");
        assert_eq!(
            rows[0].first_block_keyframe, None,
            "舊列沒有掃描過，旗標必須是 NULL＝未知"
        );

        let mut r = rows[0].clone();
        r.segment_id = "seg-000000004000".to_string();
        r.name = "seg-000000004000.mkv".to_string();
        r.first_block_keyframe = Some(false);
        idx.upsert(&r).expect("v3 必須能寫 first_block_keyframe=false");
        assert_eq!(
            idx.get("seg-000000004000")
                .unwrap()
                .unwrap()
                .first_block_keyframe,
            Some(false)
        );
        r.segment_id = "seg-000000006000".to_string();
        r.name = "seg-000000006000.mkv".to_string();
        r.first_block_keyframe = None;
        idx.upsert(&r).expect("v3 必須能寫 NULL（未知）");
        assert_eq!(
            idx.get("seg-000000006000")
                .unwrap()
                .unwrap()
                .first_block_keyframe,
            None
        );

        let version: i64 = idx
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(
            idx.count_events("schema_upgrade").unwrap(),
            1,
            "v2→v3 恰一筆 schema_upgrade"
        );
        std::fs::remove_file(&path).ok();
    }

    /// v2 → v3 重複開啟冪等：不重複寫事件、不重複改資料。
    #[test]
    fn migration_v2_to_v3_is_idempotent() {
        let path = temp_db("v2v3-idem");
        make_legacy_v2_db(&path);

        for _ in 0..3 {
            let idx = SegmentIndex::open(&path).unwrap();
            let rows = idx.rows().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].gap_ms, Some(1500));
            assert_eq!(rows[0].first_block_keyframe, None);
            assert_eq!(
                idx.count_events("schema_upgrade").unwrap(),
                1,
                "schema_upgrade 不得隨開啟次數增加"
            );
        }
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

    fn index_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("PRAGMA index_list({table})")).unwrap();
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows
    }

    #[test]
    fn migrated_legacy_db_keeps_segments_by_start_index() {
        let path = temp_db("idxkeep");
        make_legacy_v1_db(&path);
        let idx = SegmentIndex::open(&path).unwrap();
        let names = index_names(&idx.conn, "segments");
        assert!(
            names.iter().any(|n| n == "segments_by_start"),
            "升級後索引不能默默消失：{names:?}"
        );
        std::fs::remove_file(&path).ok();
    }

    /// 模擬「ALTER RENAME 之後斷掉」：只剩 segments_v1，user_version 卻已經寫成 2。
    #[test]
    fn repairs_interrupted_upgrade_after_rename() {
        let path = temp_db("halfrename");
        make_legacy_v1_db(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("ALTER TABLE segments RENAME TO segments_v1")
                .unwrap();
            conn.pragma_update(None, "user_version", 2i64).unwrap();
        }

        let idx = SegmentIndex::open(&path).expect("殘留 segments_v1 要能自癒，不能 rows=0");
        let rows = idx.rows().unwrap();
        assert_eq!(rows.len(), 1, "資料不能卡在 segments_v1 被當成空的索引");
        assert_eq!(rows[0].gap_ms, Some(1500), "gap_ms 不得靜默丟失");
        assert!(!rows[0].repaired, "repaired 旗標不得靜默丟失");
        assert_eq!(
            idx.count_events("schema_repair").unwrap(),
            1,
            "自癒要留 schema_repair 事件"
        );
        let leftover: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='segments_v1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0, "成功後不得留下 segments_v1");
        std::fs::remove_file(&path).ok();
    }

    /// 模擬「搬一半斷掉」：新表只有部分列，舊表才是完整的。
    #[test]
    fn repairs_half_copied_upgrade() {
        let path = temp_db("halfcopy");
        make_legacy_v1_db(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "ALTER TABLE segments RENAME TO segments_v1;
                 CREATE TABLE segments (
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
                 INSERT INTO segments SELECT * FROM segments_v1 WHERE segment_id = 'nope';",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2i64).unwrap();
        }

        let idx = SegmentIndex::open(&path).unwrap();
        assert_eq!(idx.count().unwrap(), 1, "半搬的結果要還原成舊表的完整資料");
        assert_eq!(idx.rows().unwrap()[0].gap_ms, Some(1500));
        std::fs::remove_file(&path).ok();
    }

    /// 殘骸修復不得只看筆數：半搬時新表獨有的列（舊表沒有）必須保住。
    ///
    /// 造「舊表 80 筆、新表 11 筆＝10 筆重疊＋1 筆獨有」的殘骸——筆數比較會判「舊表比較多、
    /// 以舊表為準」，於是那 1 筆獨有的列整份消失（Verifier 實測 `seg-000000200000` 不見）。
    /// 另外把一個重疊 id 在新表的值改掉，驗證同 id 取新表那列。
    #[test]
    fn repair_keeps_rows_unique_to_new_table() {
        let path = temp_db("union");
        let v1_schema = "CREATE TABLE segments (
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
             CREATE INDEX segments_by_start ON segments(t_start);
             CREATE TABLE events (
                 id      INTEGER PRIMARY KEY AUTOINCREMENT,
                 at_ms   INTEGER NOT NULL,
                 kind    TEXT NOT NULL,
                 detail  TEXT NOT NULL
             );";
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(v1_schema).unwrap();
            for i in 0..80u64 {
                let t = i * 2000;
                conn.execute(
                    "INSERT INTO segments VALUES (?1, ?2, ?3, ?4, ?5, 200, 1500, 1, 0)",
                    params![
                        format!("seg-{t:012}"),
                        format!("seg-{t:012}.mkv"),
                        t as i64,
                        (t + 2000) as i64,
                        (1000 + i) as i64
                    ],
                )
                .unwrap();
            }
        }
        {
            // 半搬狀態：舊表改名、新表（v2 形狀）已寫入 10 筆重疊 + 1 筆獨有。
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("ALTER TABLE segments RENAME TO segments_v1;").unwrap();
            conn.execute_batch(SEGMENTS_MERGED_DDL.replace("segments_merged", "segments").as_str())
                .unwrap();
            // 只有 10 筆重疊被搬進新表（模擬搬到一半就斷電）——比筆數的話這裡會判「舊表多」。
            conn.execute_batch(
                "INSERT INTO segments
                 SELECT segment_id, name, t_start, t_end, bytes, idr_offset, gap_ms, closed, repaired
                 FROM segments_v1 LIMIT 10",
            )
            .unwrap();
            // 同 id 衝突：新表那列的時間比較新（模擬升級後又更新過）
            conn.execute("UPDATE segments SET t_end = 9999 WHERE segment_id = 'seg-000000000000'", [])
                .unwrap();
            // 只有新表有的列，gap_ms 是 NULL（未知）——不得被丟掉，也不得被寫成 0
            conn.execute(
                "INSERT INTO segments VALUES ('seg-000000200000', 'seg-000000200000.mkv', 200000, 202000, 1500, 300, NULL, 1, 0)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2i64).unwrap();
        }

        let idx = SegmentIndex::open(&path).unwrap();
        assert_eq!(
            idx.count().unwrap(),
            81,
            "聯集後不得丟掉任何一邊獨有的列（舊表 80 筆 + 新表獨有 1 筆）"
        );
        let unique = idx.get("seg-000000200000").unwrap().expect("新表獨有的列不見了");
        assert_eq!(unique.t_start, 200_000);
        assert_eq!(unique.t_end, 202_000);
        assert_eq!(unique.gap_ms, None, "未知的 gap 不得被寫成 0（那是「確定連續」）");
        assert!(unique.closed);
        assert!(!unique.repaired);

        let conflicted = idx.get("seg-000000000000").unwrap().unwrap();
        assert_eq!(conflicted.t_end, 9999, "同 id 衝突要取新表那列");

        // 索引總 bytes 必須等於磁碟上實際段檔大小總和（造一個對應的資料目錄）
        let dir = std::env::temp_dir().join(format!(
            "sview-union-bytes-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for row in idx.rows().unwrap() {
            std::fs::write(dir.join(&row.name), vec![0u8; row.bytes as usize]).unwrap();
        }
        let on_disk: u64 = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert_eq!(on_disk, idx.total_bytes().unwrap(), "索引 bytes 與磁碟不符");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_newer_schema_version() {
        let path = temp_db("newer");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 99i64).unwrap();
        }
        let err = match SegmentIndex::open(&path) {
            Ok(_) => panic!("版本比程式新必須拒絕開啟"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("比程式支援的 3 新"),
            "錯誤訊息要說清楚：{msg}"
        );
        std::fs::remove_file(&path).ok();
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
