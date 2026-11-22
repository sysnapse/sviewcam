//! `camcore` — SViewCam 錄影核心（M1）。
//!
//! 範圍（TICKET-20261115-02）：環形緩衝（2 s MKV 段）、SQLite 索引、斷流 gap 記錄、
//! 啟動復原掃描；M2 另含 MP4 匯出與回溯截圖。**不含** GUI（M4）、camd 服務化（M3）。
//!
//! 時間基準一律是 host 單調時鐘（[`rtsp_core::clock::Clock`]），永不使用相機時鐘
//! （相機時鐘停於 1970-01-12）。

pub mod config;
pub mod error;
pub mod export;
pub mod index;
pub mod mkv;
pub mod recover;
pub mod ring;
pub mod snapshot;

pub use config::{
    RetentionConfig, RingConfig, Warning, DEFAULT_MAX_BYTES, DEFAULT_WINDOW_MS, HARD_MAX_WINDOW_MS,
};
pub use error::{Error, Result};
pub use index::{SegmentIndex, SegmentRow};
pub use mkv::{AvcConfig, MkvInfo, MkvWriter, SegmentSummary};
pub use recover::{recover, RecoveryReport};
pub use ring::RingWriter;
