//! 串流時間軸的跨行程延續。
//!
//! `rtsp-core` 給 sink 的 `at_ms` 是 host 時鐘（本次 `RealClock` 開始以來的毫秒），
//! 所以 camd 一重啟，時間軸就會從 0 重來；若直接餵給 `RingWriter`，索引會出現
//! t_start 比前一段 t_end 還早的**重疊段**，而重啟前後那段空窗則被吞掉。
//!
//! 這裡把「最後寫入索引的串流時間」與「當時的 wall clock」記在資料目錄，重啟後
//! 用 wall 時差把時間軸接回來：索引時間單調、空窗如實回報（`RingWriter::gap`），
//! 不假裝連續。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 時間軸狀態檔（放資料目錄，跟索引一起被保留窗管理之外；很小，只有兩個數字）。
pub const STATE_FILE: &str = "camd.timeline";
/// 每隔這麼久把「目前串流時間＋wall 時間」寫回狀態檔；`kill -9` 最多掉這麼多毫秒。
const SAVE_INTERVAL_MS: u64 = 1000;

/// 上次寫入索引的串流時間與當時的 wall clock（毫秒，UNIX epoch）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineState {
    pub last_at_ms: u64,
    pub wall_ms: u64,
}

/// 目前的 wall clock（毫秒，UNIX epoch）。
pub fn now_wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

/// 讀回上次的時間軸狀態；檔案不存在或內容壞掉都回 `None`（當成第一次啟動）。
pub fn load(dir: &Path) -> Option<TimelineState> {
    let data = fs::read_to_string(state_path(dir)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&data).ok()?;
    let last_at_ms = value.get("last_at_ms")?.as_u64()?;
    let wall_ms = value.get("wall_ms")?.as_u64()?;
    Some(TimelineState {
        last_at_ms,
        wall_ms,
    })
}

/// 寫回時間軸狀態（先寫 `.tmp` 再 rename，避免半寫檔被下次啟動讀到）。
pub fn save(dir: &Path, state: TimelineState) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let final_path = state_path(dir);
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    let body = format!(
        "{{\"last_at_ms\":{},\"wall_ms\":{}}}\n",
        state.last_at_ms, state.wall_ms
    );
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &final_path)
}

/// 把一次又一次連線的 host 時鐘轉成**單調**的串流時間軸。
///
/// - 第一次啟動（沒有狀態檔）：直接沿用 `at_ms`。
/// - 重啟：接在 `last_at_ms` 之後，並把「兩次之間經過的 wall 時間」當成真實空窗；
///   交回的第一個 gap 由呼叫端在開段前交給 `RingWriter::gap`，下一段的 `gap_ms`
///   才不會是假的 0。
pub struct TimelineClock {
    dir: PathBuf,
    /// 上次落盤的狀態（重啟後第一幀用它算空窗）。
    previous: Option<TimelineState>,
    /// `(映射後的串流時間, 當時的 host at_ms)`：同一行程內所有 session 共用。
    anchor: Option<(u64, u64)>,
    /// 開段前要回報的空窗（只有重啟後第一次、且真的 >0 才會有值）。
    pending_gap: Option<(u64, u64)>,
    last_save_wall: u64,
    /// 最後一次映射的 host at_ms（收工落盤用）。
    last_clock: Option<u64>,
}

impl TimelineClock {
    pub fn new(dir: PathBuf, previous: Option<TimelineState>) -> Self {
        Self {
            dir,
            previous,
            anchor: None,
            pending_gap: None,
            last_save_wall: 0,
            last_clock: None,
        }
    }

    /// 把 host `at_ms` 映射成索引時間；回傳 `(串流時間, 這次要回報的空窗)`。
    ///
    /// 空窗只在重啟後第一次呼叫時回傳，之後都是 `None`。
    pub fn map(&mut self, at_ms: u64) -> (u64, Option<(u64, u64)>) {
        self.last_clock = Some(at_ms);
        let wall = now_wall_ms();
        let (mapped, gap) = match self.anchor {
            Some((mapped_anchor, clock_anchor)) => (
                mapped_anchor.saturating_add(at_ms.saturating_sub(clock_anchor)),
                None,
            ),
            None => {
                let (mapped, gap) = match self.previous {
                    Some(prev) => {
                        let elapsed = wall.saturating_sub(prev.wall_ms);
                        (
                            prev.last_at_ms.saturating_add(elapsed),
                            Some((elapsed, prev.last_at_ms)),
                        )
                    }
                    None => (at_ms, None),
                };
                self.anchor = Some((mapped, at_ms));
                if let Some((elapsed, last)) = gap {
                    self.pending_gap = Some((elapsed, last));
                }
                (mapped, gap)
            }
        };
        // 最多每秒落盤一次；`kill -9` 讓最後 1 秒以內的時間軸遺失（當成空窗補回）。
        if wall.saturating_sub(self.last_save_wall) >= SAVE_INTERVAL_MS {
            self.last_save_wall = wall;
            let _ = save(
                &self.dir,
                TimelineState {
                    last_at_ms: mapped,
                    wall_ms: wall,
                },
            );
        }
        (mapped, gap)
    }

    /// 收工時把最新時間落盤。
    pub fn flush(&mut self) {
        let (Some((mapped_anchor, clock_anchor)), Some(at_ms)) = (self.anchor, self.last_clock)
        else {
            return;
        };
        let mapped = mapped_anchor.saturating_add(at_ms.saturating_sub(clock_anchor));
        let wall = now_wall_ms();
        self.last_save_wall = wall;
        let _ = save(
            &self.dir,
            TimelineState {
                last_at_ms: mapped,
                wall_ms: wall,
            },
        );
    }

    /// 取出（並清掉）重啟後要回報的空窗。
    pub fn take_pending_gap(&mut self) -> Option<(u64, u64)> {
        self.pending_gap.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "camd-timeline-{tag}-{}-{}",
            std::process::id(),
            now_wall_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = temp_dir("roundtrip");
        let state = TimelineState {
            last_at_ms: 4200,
            wall_ms: now_wall_ms(),
        };
        save(&dir, state).unwrap();
        assert_eq!(load(&dir), Some(state));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_resumes_after_last_time_and_reports_gap() {
        let dir = temp_dir("restart");
        let wall = now_wall_ms();
        // 假裝上一次在串流時間 4200 ms、wall 是 3 秒前落盤。
        save(
            &dir,
            TimelineState {
                last_at_ms: 4200,
                wall_ms: wall.saturating_sub(3000),
            },
        )
        .unwrap();
        let mut clock = TimelineClock::new(dir.clone(), load(&dir));
        let (first, gap) = clock.map(1000);
        let (elapsed, last) = gap.expect("重啟後第一次映射要回報空窗");
        assert_eq!(last, 4200);
        assert!(elapsed >= 3000, "空窗至少是 wall 時差，得到 {elapsed}");
        assert_eq!(first, 4200 + elapsed, "第一幀要接在上次時間之後");
        // 同一行程內後續呼叫單調遞增、不再回報空窗。
        let (second, gap2) = clock.map(1040);
        assert_eq!(second, first + 40);
        assert!(gap2.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_ever_start_uses_host_clock_unchanged() {
        let dir = temp_dir("fresh");
        let mut clock = TimelineClock::new(dir.clone(), None);
        let (first, gap) = clock.map(900);
        assert_eq!(first, 900);
        assert!(gap.is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
