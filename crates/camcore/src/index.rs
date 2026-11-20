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
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
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
