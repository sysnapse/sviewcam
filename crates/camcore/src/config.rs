//! 錄影設定：保留窗、段長、預配置大小。
//!
//! 保留窗（retention window）預設 5 分鐘，上限 15 分鐘：超過上限一律夾到上限並
//! 產生 **一則** 警告（見 [`Warning`]），不得靜默。

use std::path::PathBuf;

/// 預設保留窗：5 分鐘。
pub const DEFAULT_WINDOW_MS: u64 = 5 * 60 * 1000;
/// 保留窗硬上限：15 分鐘（成功指標表的預算推導就以此為上限）。
pub const HARD_MAX_WINDOW_MS: u64 = 15 * 60 * 1000;
/// 資料目錄大小上限：1.62e9 bytes（十進位，AC5 用同一個數字）。
pub const DEFAULT_MAX_BYTES: u64 = 1_620_000_000;
/// 單段長度：2 秒（GOP 對齊的切點單位）。
pub const DEFAULT_SEGMENT_MS: u64 = 2_000;
/// 等不到 IDR 時，進入「延後強切」的門檻：4 秒。
pub const DEFAULT_FORCE_CUT_MS: u64 = 4_000;
/// 延後強切的硬上限倍數：段長最多 `FORCE_CUT_LIMIT_FACTOR × force_cut_ms`。
///
/// 預設 4 → 單段硬上限 16 秒（自段首起算）。超過才退回「從非 keyframe 硬切」，
/// 並把該段標成不可獨立回放。
pub const FORCE_CUT_LIMIT_FACTOR: u64 = 4;
/// 預配置單段檔案大小：8 MiB（2 s × 最高碼率仍留餘裕）。
pub const DEFAULT_PREALLOC_BYTES: u64 = 8 * 1024 * 1024;

/// 一則使用者可見的設定警告。
///
/// 走既有 log／事件通道（`eprintln!`／索引 `events` 表）輸出，不塞在資料流裡，
/// 因此 AC2 能只靠「警告行數 == 1」斷言。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// 穩定代碼，供測試與 UI 對照。
    pub code: &'static str,
    /// 人可讀訊息。
    pub message: String,
}

impl Warning {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// 保留窗設定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionConfig {
    /// 保留窗長度（毫秒）。
    pub window_ms: u64,
    /// 上限（毫秒），超過即夾住。
    pub max_window_ms: u64,
    /// 資料目錄大小上限（bytes）。
    pub max_bytes: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            window_ms: DEFAULT_WINDOW_MS,
            max_window_ms: HARD_MAX_WINDOW_MS,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl RetentionConfig {
    /// 從「分鐘」建立設定，並回報被夾住的警告。
    ///
    /// - `None` → 用預設 5 分鐘。
    /// - `Some(0)` → 視為未指定，用預設值並警告。
    /// - 超過 [`HARD_MAX_WINDOW_MS`] → 夾到上限並警告一次。
    pub fn from_minutes(minutes: Option<u64>) -> (Self, Vec<Warning>) {
        let mut warnings = Vec::new();
        let requested = match minutes {
            None => DEFAULT_WINDOW_MS,
            Some(m) => m.saturating_mul(60_000),
        };
        let window_ms = if requested == 0 {
            warnings.push(Warning::new(
                "retention.zero",
                format!(
                    "保留窗 0 分鐘無意義，改用預設 {} 分鐘",
                    DEFAULT_WINDOW_MS / 60_000
                ),
            ));
            DEFAULT_WINDOW_MS
        } else if requested > HARD_MAX_WINDOW_MS {
            warnings.push(Warning::new(
                "retention.clamped",
                format!(
                    "保留窗 {} 分鐘超過上限，夾到 {} 分鐘（磁碟預算 1.62e9 bytes）",
                    requested / 60_000,
                    HARD_MAX_WINDOW_MS / 60_000
                ),
            ));
            HARD_MAX_WINDOW_MS
        } else {
            requested
        };
        (
            Self {
                window_ms,
                max_window_ms: HARD_MAX_WINDOW_MS,
                max_bytes: DEFAULT_MAX_BYTES,
            },
            warnings,
        )
    }

    /// 修剪判斷：給最舊段 `oldest_t_start` 與最新段 `latest_t_end`，
    /// 回傳是否該刪掉最舊段。
    ///
    /// 窗長落在 `(window, window + segment]`：只有「超過 window 再一個段」才刪，
    /// 因此保留窗不會縮到比要求短。
    pub fn should_trim(&self, oldest_t_start: u64, latest_t_end: u64, segment_ms: u64) -> bool {
        latest_t_end.saturating_sub(oldest_t_start) > self.window_ms + segment_ms
    }
}

/// 環形緩衝設定。
#[derive(Debug, Clone)]
pub struct RingConfig {
    /// 資料目錄（段檔與 `index.sqlite` 都放這裡）。
    pub dir: PathBuf,
    /// 段長（毫秒）。
    pub segment_ms: u64,
    /// 等不到 IDR 時的延後強切門檻（毫秒）。
    ///
    /// 段滿 [`Self::segment_ms`] 後若仍無 IDR，等超過此值就進入延後期；段長硬上限
    /// 另由 [`Self::force_cut_limit_ms`] 決定（預設 4×）。
    pub force_cut_ms: u64,
    /// 單段預配置 bytes。
    pub prealloc_bytes: u64,
    /// 保留窗設定。
    pub retention: RetentionConfig,
    /// 視訊寬（寫進 MKV Video 元素；預設取 SDP `x-dimensions`）。
    pub width: u32,
    /// 視訊高。
    pub height: u32,
}

impl RingConfig {
    /// 以目錄為根建立預設設定。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            segment_ms: DEFAULT_SEGMENT_MS,
            force_cut_ms: DEFAULT_FORCE_CUT_MS,
            prealloc_bytes: DEFAULT_PREALLOC_BYTES,
            retention: RetentionConfig::default(),
            width: 2560,
            height: 1440,
        }
    }

    /// 套用保留窗分鐘數，回傳可能產生的警告。
    pub fn with_retention_minutes(mut self, minutes: Option<u64>) -> (Self, Vec<Warning>) {
        let (retention, warnings) = RetentionConfig::from_minutes(minutes);
        self.retention = retention;
        (self, warnings)
    }

    /// 單段硬上限（自段首起算）：`FORCE_CUT_LIMIT_FACTOR × force_cut_ms`。
    pub fn force_cut_limit_ms(&self) -> u64 {
        self.force_cut_ms.saturating_mul(FORCE_CUT_LIMIT_FACTOR)
    }

    /// 索引檔路徑。
    pub fn index_path(&self) -> PathBuf {
        self.dir.join("index.sqlite")
    }
}
