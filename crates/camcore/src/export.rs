//! M2 片段匯出與截圖。
//!
//! 匯出刻意只用「完整段檔」當輸入：M1 的每個段檔第一個 SimpleBlock 是 IDR，
//! 所以切點永遠落在 GOP 邊界，不會切出半個 GOP；`-c copy` 不重編碼。
//! 段與段之間若有 gap（`SegmentRow.gap_ms`），一定分檔輸出並在 stderr 列出時點，
//! 不把空窗靜默接成連續畫面。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::index::{SegmentIndex, SegmentRow};

/// 匯出時間範圍（host 單調時鐘毫秒，絕對值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportRange {
    pub from_ms: u64,
    pub to_ms: u64,
}

/// 單一輸出檔。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOutput {
    pub path: PathBuf,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
    pub segments: Vec<String>,
}

/// 匯出時遇到的 gap（每個 gap 都對應一次分檔）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportGap {
    /// 下一段的開始時間（也是空窗結束的時間點）。
    pub at_ms: u64,
    /// 空窗長度；`None` 表示索引沒有 gap 記錄且時間對不上（未知，不猜 0）。
    pub gap_ms: Option<u64>,
    pub prev_segment: Option<String>,
    pub next_segment: String,
    pub detail: String,
}

/// 匯出結果（camctl 的 `--json` 直接由此組出）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// 相對時間的基準：最後一段的 `t_end`。
    pub anchor_ms: u64,
    /// 目前索引內最早段的 `t_start`。
    pub available_from_ms: u64,
    /// 目前索引內最後一段的 `t_end`。
    pub available_to_ms: u64,
    pub from_ms: u64,
    pub to_ms: u64,
    pub outputs: Vec<ExportOutput>,
    pub gaps: Vec<ExportGap>,
    pub warnings: Vec<String>,
}

/// 匯出選項。
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// ffmpeg 執行檔（PATH 或絕對路徑）。
    pub ffmpeg: PathBuf,
    /// 是否覆蓋既有輸出檔。
    pub force: bool,
    /// SIGINT 取消旗標（由 CLI 安裝 handler；library 只讀）。
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            ffmpeg: PathBuf::from("ffmpeg"),
            force: false,
            cancel: None,
        }
    }
}

/// 檢查 ffmpeg 存在且版本 >= 4.4。
///
/// 回傳 `(major, minor, 原始版本字串)`。在建立任何暫存檔／輸出檔之前呼叫。
pub fn check_ffmpeg(ffmpeg: &Path) -> Result<(u32, u32, String)> {
    let output = Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-version")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            Error::Export(format!(
                "找不到可用的 ffmpeg（{}）：{e}；需要 >= 4.4",
                ffmpeg.display()
            ))
        })?;
    if !output.status.success() {
        return Err(Error::Export(format!(
            "ffmpeg -version 失敗（{}）：{}",
            ffmpeg.display(),
            tail(&output.stderr)
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next().unwrap_or("").trim();
    let Some(pos) = first.find("version ") else {
        return Err(Error::Export(format!(
            "無法解析 ffmpeg 版本（{}）：{first:?}；需要 >= 4.4",
            ffmpeg.display()
        )));
    };
    let token = first[pos + "version ".len()..].trim();
    let token = token.trim_start_matches('n');
    let mut parts = token.split('.');
    let major = parts.next().and_then(parse_leading_u32);
    let minor = parts.next().and_then(parse_leading_u32);
    let (Some(major), Some(minor)) = (major, minor) else {
        return Err(Error::Export(format!(
            "無法解析 ffmpeg 版本（{}）：{first:?}；需要 >= 4.4",
            ffmpeg.display()
        )));
    };
    if (major, minor) < (4, 4) {
        return Err(Error::Export(format!(
            "ffmpeg 版本 {major}.{minor} 太舊（{first}）；需要 >= 4.4"
        )));
    }
    Ok((major, minor, first.to_string()))
}

fn parse_leading_u32(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// 匯出指定範圍。`dir` 是錄影目錄，段檔與 `index.sqlite` 都在裡面。
pub fn export(
    dir: &Path,
    index: &SegmentIndex,
    range: ExportRange,
    output: &Path,
    options: &ExportOptions,
) -> Result<ExportReport> {
    if range.from_ms >= range.to_ms {
        return Err(Error::Export(format!(
            "匯出範圍無效：from={}ms 必須小於 to={}ms",
            range.from_ms, range.to_ms
        )));
    }
    // CP2 紅燈：先在骨架階段明確失敗，讓 AC3 測試在 CI 上紅；下一個 commit 實作。
    let _ = (dir, index, output, options);
    return Err(Error::Export("尚未實作（CP2 紅燈）".into()));
    #[allow(unreachable_code)]
    check_ffmpeg(&options.ffmpeg)?;

    let all: Vec<SegmentRow> = index
        .rows()?
        .into_iter()
        .filter(|row| row.closed)
        .collect();
    if all.is_empty() {
        return Err(Error::Export("索引內沒有已閉合的段".into()));
    }
    let available_from = all.first().map(|r| r.t_start).unwrap_or(0);
    let available_to = index
        .latest_end()?
        .or_else(|| all.last().map(|r| r.t_end))
        .ok_or_else(|| Error::Export("索引內沒有可用的段".into()))?;

    if range.from_ms < available_from {
        return Err(Error::Export(format!(
            "要求起點 {} 早於可用範圍；目前最早可得 {}（{}），最晚 {}（{}）",
            range.from_ms,
            available_from,
            ms_to_relative(available_from, available_to),
            available_to,
            "latest"
        )));
    }
    if range.to_ms > available_to {
        return Err(Error::Export(format!(
            "要求終點 {} 晚於目前最新段 {}；可用範圍 {}..{}",
            range.to_ms,
            available_to,
            available_from,
            available_to
        )));
    }

    let selected: Vec<&SegmentRow> = all
        .iter()
        .filter(|row| row.t_end > range.from_ms && row.t_start < range.to_ms)
        .collect();
    if selected.is_empty() {
        return Err(Error::Export(format!(
            "範圍 {}..{} 內沒有任何段；可用範圍 {}..{}",
            range.from_ms, range.to_ms, available_from, available_to
        )));
    }

    let mut warnings = Vec::new();
    let mut runs: Vec<Vec<&SegmentRow>> = Vec::new();
    let mut gaps: Vec<ExportGap> = Vec::new();

    for row in selected {
        let playable = playable_first_block(dir, row)?;
        if runs.is_empty() {
            if !playable {
                warnings.push(format!(
                    "{} 首個 block 不是 keyframe，跳過它（不可獨立解碼）",
                    row.name
                ));
                continue;
            }
            runs.push(vec![row]);
            continue;
        }

        let current = runs.last_mut().expect("runs 非空");
        let prev = *current.last().expect("run 非空");
        match classify_gap(prev, row) {
            GapKind::Continuous => current.push(row),
            GapKind::Gap {
                gap_ms,
                detail,
            } => {
                gaps.push(ExportGap {
                    at_ms: row.t_start,
                    gap_ms,
                    prev_segment: Some(prev.name.clone()),
                    next_segment: row.name.clone(),
                    detail: detail.clone(),
                });
                if playable {
                    runs.push(vec![row]);
                } else {
                    warnings.push(format!(
                        "gap 後的 {} 首個 block 不是 keyframe，跳過它（不可獨立解碼）",
                        row.name
                    ));
                }
            }
        }
    }

    if runs.is_empty() {
        return Err(Error::Export(
            "範圍內的段都不可獨立解碼（首個 block 不是 keyframe）".into(),
        ));
    }

    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(Error::Export(format!(
            "輸出目錄不存在：{}",
            parent.display()
        )));
    }
    let parent = fs::canonicalize(parent)?;
    let total = runs.len();
    let finals: Vec<PathBuf> = (1..=total).map(|n| numbered_output(output, &parent, n, total)).collect();
    for path in &finals {
        if path.exists() && !options.force {
            return Err(Error::Export(format!(
                "輸出檔已存在：{}（要覆蓋請加 --force）",
                path.display()
            )));
        }
    }

    let mut temps = TempFiles::default();
    let mut outputs = Vec::with_capacity(total);
    for (run_idx, run) in runs.iter().enumerate() {
        check_cancel(options)?;
        let final_path = &finals[run_idx];
        let list = temps.temp_path(&parent, "concat");
        let tmp = temps.temp_path(&parent, "mp4");
        write_concat_list(&list, dir, run)?;
        let cmd_out = Command::new(&options.ffmpeg)
            .arg("-hide_banner")
            .arg("-nostdin")
            .arg("-loglevel")
            .arg("error")
            .arg("-y")
            .arg("-f")
            .arg("concat")
            .arg("-safe")
            .arg("0")
            .arg("-i")
            .arg(&list)
            .arg("-map")
            .arg("0:v:0")
            .arg("-c")
            .arg("copy")
            .arg("-movflags")
            .arg("+faststart")
            .arg("-avoid_negative_ts")
            .arg("make_zero")
            .arg("-f")
            .arg("mp4")
            .arg(&tmp)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::Export(format!("執行 ffmpeg 失敗：{e}")))?;
        temps.remove(&list);
        if !cmd_out.status.success() {
            temps.remove(&tmp);
            return Err(Error::Export(format!(
                "ffmpeg 匯出失敗（run {}）：{}",
                run_idx + 1,
                tail(&cmd_out.stderr)
            )));
        }
        if !tmp.exists() {
            temps.remove(&tmp);
            return Err(Error::Export(format!(
                "ffmpeg 回報成功但沒有輸出檔：{}",
                tmp.display()
            )));
        }
        if cancelled(options) {
            temps.remove(&tmp);
            return Err(Error::Export("已取消（SIGINT），未留下半成品".into()));
        }
        fs::rename(&tmp, final_path)?;
        temps.remove(&tmp);
        outputs.push(ExportOutput {
            path: final_path.clone(),
            t_start_ms: run.first().expect("run 非空").t_start,
            t_end_ms: run.last().expect("run 非空").t_end,
            segments: run.iter().map(|r| r.name.clone()).collect(),
        });
    }

    Ok(ExportReport {
        anchor_ms: available_to,
        available_from_ms: available_from,
        available_to_ms: available_to,
        from_ms: range.from_ms,
        to_ms: range.to_ms,
        outputs,
        gaps,
        warnings,
    })
}

fn numbered_output(output: &Path, parent: &Path, n: usize, total: usize) -> PathBuf {
    if total <= 1 {
        return output.to_path_buf();
    }
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".to_string());
    let ext = output
        .extension()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mp4".to_string());
    parent.join(format!("{stem}.{n}.{ext}"))
}

fn playable_first_block(dir: &Path, row: &SegmentRow) -> Result<bool> {
    if let Some(value) = row.first_block_keyframe {
        return Ok(value);
    }
    // v2 以前的列沒有這個欄位；現場掃描一次，不把未知臆測成 true。
    let info = crate::mkv::scan(dir.join(&row.name))?;
    Ok(info.first_block_keyframe == Some(true))
}

enum GapKind {
    Continuous,
    Gap {
        gap_ms: Option<u64>,
        detail: String,
    },
}

fn classify_gap(prev: &SegmentRow, next: &SegmentRow) -> GapKind {
    if next.t_start < prev.t_end {
        return GapKind::Gap {
            gap_ms: None,
            detail: format!(
                "段邊界重疊/倒退：prev.t_end={} > next.t_start={}",
                prev.t_end, next.t_start
            ),
        };
    }
    let delta = next.t_start - prev.t_end;
    match next.gap_ms {
        Some(0) if delta == 0 => GapKind::Continuous,
        Some(n) if n > 0 => {
            let note = if n == delta {
                String::new()
            } else {
                format!("（索引 gap_ms={n}，但段邊界差 {delta}）")
            };
            GapKind::Gap {
                gap_ms: Some(n),
                detail: format!("{n} ms{note}"),
            }
        }
        Some(_) => GapKind::Gap {
            gap_ms: Some(delta),
            detail: format!("索引記 0，但段邊界差 {delta} ms（以實際邊界為準）"),
        },
        None if delta == 0 => GapKind::Continuous,
        None => GapKind::Gap {
            gap_ms: Some(delta),
            detail: format!("gap 未知；段邊界差 {delta} ms（不靜默拼接）"),
        },
    }
}

fn write_concat_list(path: &Path, dir: &Path, run: &[&SegmentRow]) -> Result<()> {
    let mut text = String::new();
    for row in run {
        let abs = fs::canonicalize(dir.join(&row.name)).map_err(|e| {
            Error::Export(format!("找不到段檔 {}：{e}", dir.join(&row.name).display()))
        })?;
        text.push_str(&format!("file '{}'\n", escape_concat_path(&abs)));
        let secs = (row.t_end.saturating_sub(row.t_start)) as f64 / 1000.0;
        text.push_str(&format!("duration {secs:.3}\n"));
    }
    fs::write(path, text)?;
    Ok(())
}

fn escape_concat_path(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "'\\''")
}

fn check_cancel(options: &ExportOptions) -> Result<()> {
    if cancelled(options) {
        Err(Error::Export("已取消（SIGINT），未留下半成品".into()))
    } else {
        Ok(())
    }
}

fn cancelled(options: &ExportOptions) -> bool {
    options
        .cancel
        .as_ref()
        .map(|flag| flag.load(Ordering::SeqCst))
        .unwrap_or(false)
}

fn tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

/// 同目錄暫存檔；正常結束時清掉，Drop 時也會清（Ctrl-C 由 CLI 先等待 ffmpeg 結束）。
#[derive(Default)]
struct TempFiles {
    paths: Vec<PathBuf>,
}

impl TempFiles {
    fn temp_path(&mut self, parent: &Path, tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".k14-{}-{n}.{tag}", std::process::id()));
        self.paths.push(path.clone());
        path
    }

    fn remove(&mut self, path: &Path) {
        let _ = fs::remove_file(path);
        self.paths.retain(|p| p != path);
    }
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

fn ms_to_relative(ms: u64, anchor: u64) -> String {
    if anchor >= ms {
        format!("-{:.1}s", (anchor - ms) as f64 / 1000.0)
    } else {
        format!("+{:.1}s", (ms - anchor) as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, t_start: u64, t_end: u64, gap_ms: Option<u64>) -> SegmentRow {
        SegmentRow {
            segment_id: name.trim_end_matches(".mkv").to_string(),
            name: name.to_string(),
            t_start,
            t_end,
            bytes: 1,
            idr_offset: 0,
            first_block_keyframe: Some(true),
            gap_ms,
            closed: true,
            repaired: false,
        }
    }

    fn kind(prev: &SegmentRow, next: &SegmentRow) -> GapKind {
        classify_gap(prev, next)
    }

    #[test]
    fn gap_some_zero_with_equal_boundary_is_continuous() {
        let a = row("seg-000000000000.mkv", 0, 2_000, Some(0));
        let b = row("seg-000000002000.mkv", 2_000, 4_000, Some(0));
        assert!(matches!(kind(&a, &b), GapKind::Continuous));
    }

    #[test]
    fn gap_some_zero_with_broken_boundary_is_split() {
        let a = row("seg-000000000000.mkv", 0, 2_000, Some(0));
        let b = row("seg-000000002500.mkv", 2_500, 4_500, Some(0));
        match kind(&a, &b) {
            GapKind::Gap { gap_ms, detail } => {
                assert_eq!(gap_ms, Some(500));
                assert!(detail.contains("500"));
            }
            _ => panic!("必須分檔"),
        }
    }

    #[test]
    fn gap_some_n_is_split_and_preserves_declared_value() {
        let a = row("seg-000000000000.mkv", 0, 2_000, Some(0));
        let b = row("seg-000000002500.mkv", 2_500, 4_500, Some(500));
        match kind(&a, &b) {
            GapKind::Gap { gap_ms, detail } => {
                assert_eq!(gap_ms, Some(500));
                assert!(!detail.contains("但段邊界差"));
            }
            _ => panic!("必須分檔"),
        }
    }

    #[test]
    fn gap_unknown_with_nonzero_delta_is_not_silently_joined() {
        let a = row("seg-000000000000.mkv", 0, 2_000, Some(0));
        let b = row("seg-000000002500.mkv", 2_500, 4_500, None);
        match kind(&a, &b) {
            GapKind::Gap { gap_ms, detail } => {
                assert_eq!(gap_ms, Some(500));
                assert!(detail.contains("未知"));
            }
            _ => panic!("未知 gap 必須分檔"),
        }
    }

    #[test]
    fn overlapping_boundary_is_split_not_joined() {
        let a = row("seg-000000000000.mkv", 0, 3_000, Some(0));
        let b = row("seg-000000002000.mkv", 2_000, 4_000, Some(0));
        assert!(matches!(kind(&a, &b), GapKind::Gap { .. }));
    }
}
