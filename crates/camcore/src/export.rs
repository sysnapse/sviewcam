//! 匯出介面佔位（M2）。
//!
//! M1 只保證段檔與索引可被匯出器消費，**不實作** MP4 匯出與截圖；這裡刻意只留
//! 空殼，避免提前做 M2 的 CLI。

use crate::index::SegmentRow;

/// 匯出請求（M2 會在此擴充時間範圍、目的地與格式）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExportRequest {
    /// 要匯出的段（依 `t_start` 排序）。
    pub segments: Vec<SegmentRow>,
}

/// 匯出器介面（M2 實作）。
pub trait Exporter {
    /// 把請求的段匯出成目標檔；M1 尚無實作。
    fn export(&self, request: &ExportRequest) -> crate::error::Result<std::path::PathBuf>;
}
