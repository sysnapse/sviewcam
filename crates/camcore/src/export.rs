//! M2 片段匯出與截圖。
//!
//! 匯出刻意只用「完整段檔」當輸入：M1 的每個段檔第一個 SimpleBlock 是 IDR，
//! 所以切點永遠落在 GOP 邊界，不會切出半個 GOP；`-c copy` 不重編碼。
//! 段與段之間若有 gap（`SegmentRow.gap_ms`），一定分檔輸出並在 stderr 列出時點，
//! 不把空窗靜默接成連續畫面。
//!
//! ## 可獨立解碼的判定
//!
//! 一個段能不能當輸出的起點，只看 `SegmentRow.first_block_keyframe`：
//! `Some(true)` 可以、`Some(false)` 不行、`None`（v3 以前的列）現場用
//! `mkv::scan()` 掃段檔第一個 SimpleBlock 的 flags。**不得改用 `idr_offset` 判定**：
//! k24 的 4×`force_cut_ms` 硬切上限會在沒有 IDR 時直接切段，這種段沒有 keyframe、
//! `idr_offset` 是 0；但 `idr_offset` 的語義是「檔案內第一個 keyframe 的位移」，
//! 不是「段首就是 keyframe」，且舊資料／未知來源的值不保證被回填。選段只信旗標與
//! 現場掃描結果，不從 `idr_offset` 推論可解性。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
    /// 實際輸出起點比請求起點早多少 ms（只有第一個輸出可能非 0）。
    pub padded_before_ms: u64,
    /// 實際輸出終點比請求終點晚多少 ms（只有最後一個輸出可能非 0）。
    pub padded_after_ms: u64,
    pub segments: Vec<String>,
}

/// 因不可獨立解碼（首 block 不是 keyframe）而被排除的段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedSegment {
    pub id: String,
    pub reason: String,
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

impl ExportGap {
    /// 文字模式的 stderr 行；未知 gap 不可以長得像已知 gap。
    pub fn stderr_line(&self) -> String {
        let at = format!(
            "at t={} between {} and {}; writing separate output",
            self.at_ms,
            self.prev_segment.as_deref().unwrap_or("?"),
            self.next_segment
        );
        match self.gap_ms {
            Some(ms) => format!("export: gap {ms} ms {at}"),
            None => format!("export: gap {} {at}", self.detail),
        }
    }
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
    /// 被排除的 P-first 段（沒有併入任何輸出）。
    pub excluded_segments: Vec<ExcludedSegment>,
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
    let mut excluded: Vec<ExcludedSegment> = Vec::new();
    let mut run_open = false;

    for row in selected {
        let playable = playable_first_block(dir, row)?;
        if !playable {
            // P-first 段不能獨立解碼：它是 run 的終止符，不是成員。
            // 連讀都讀不開的段併進來只會產生定格／時戳錯亂的檔，而且 ffmpeg
            // 不一定報錯，所以在這裡就明確排除並留痕。
            excluded.push(ExcludedSegment {
                id: row.segment_id.clone(),
                reason: "首個 block 不是 keyframe，不可獨立解碼".into(),
            });
            run_open = false;
            continue;
        }

        if run_open {
            let current = runs.last_mut().expect("run 非空");
            let prev = *current.last().expect("run 非空");
            match classify_gap(prev, row) {
                GapKind::Continuous => {
                    current.push(row);
                    continue;
                }
                GapKind::Gap {
                    gap_ms,
                    detail,
                } => {
                    gaps.push(ExportGap {
                        at_ms: row.t_start,
                        gap_ms,
                        prev_segment: Some(prev.name.clone()),
                        next_segment: row.name.clone(),
                        detail,
                    });
                }
            }
        }

        runs.push(vec![row]);
        run_open = true;
    }

    if runs.is_empty() {
        return Err(Error::Export(
            "範圍內的段都不可獨立解碼（首個 block 不是 keyframe）".into(),
        ));
    }
    if !excluded.is_empty() {
        let ids = excluded
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        warnings.push(format!(
            "注意：{} 個段因不可獨立解碼被排除（時間軸不連續）：{}",
            excluded.len(),
            ids
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
        // 先把每個段 remux 成「時間軸從 0 開始」的暫存檔。`replay --record-dir`
        // 產的段檔 `format.start_time` 是 N/A，concat demuxer 不會自動 rebase
        // 檔內絕對 pts，直接串會讓每個檔多加一次 offset（輸出 duration 暴增、
        // 中間出現定格）。setts 只改時間戳，不重編碼。
        let mut zero_based = Vec::with_capacity(run.len());
        for row in run {
            let source = fs::canonicalize(dir.join(&row.name)).map_err(|e| {
                Error::Export(format!("找不到段檔 {}：{e}", dir.join(&row.name).display()))
            })?;
            let dest = temps.temp_path(&parent, "zero.mkv");
            remux_zero_based(&options.ffmpeg, &source, &dest, options)?;
            let info = crate::mkv::scan(&dest)?;
            // concat 的 `duration` 是「下一段從本檔檔首算的位移」。scan 的
            // `t_end_ms` 是最後一個 block 的 pts，不含最後一幀的顯示長度；
            // 少算這段會讓下一個檔提早進場，整條時間軸被壓縮（播放變快）。
            // 幀時長由段內 (t_end-t_start)/(blocks-1) 推得；段只有一格時退回 40 ms。
            let span_ms = info.t_end_ms.saturating_sub(info.t_start_ms);
            let blocks = info.blocks.max(1);
            let frame_ms = if blocks > 1 {
                span_ms as f64 / (blocks - 1) as f64
            } else {
                40.0
            };
            let dur_secs = (span_ms as f64 + frame_ms) / 1000.0;
            zero_based.push((dest, dur_secs));
        }
        write_concat_list(&list, &zero_based)?;
        let mut cmd = Command::new(&options.ffmpeg);
        cmd.arg("-hide_banner")
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
            // M1 的每段檔在 keyframe 前自帶 SPS/PPS；concat 後 MP4 muxer 會把
            // 這些 in-band 參數集與 extradata 疊在一起而產生壞 NAL。移除重複的
            // 參數集（extradata 仍保留）即可無損修好，不解碼、不重編碼。
            // 注意：ffmpeg 的 `-bsf:v` 是覆蓋不是串接；要串接必須寫在同一參數內用逗號。
            .arg("-bsf:v")
            .arg("filter_units=remove_types=7,filter_units=remove_types=8")
            .arg("-movflags")
            .arg("+faststart")
            .arg("-avoid_negative_ts")
            .arg("make_zero")
            .arg("-f")
            .arg("mp4")
            .arg(&tmp);
        let cmd_out = run_ffmpeg(cmd, options)?;
        temps.remove(&list);
        for (path, _) in &zero_based {
            temps.remove(path);
        }
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
        let first = run.first().expect("run 非空");
        let last = run.last().expect("run 非空");
        let padded_before_ms = if run_idx == 0 {
            first.t_start.abs_diff(range.from_ms)
        } else {
            0
        };
        let padded_after_ms = if run_idx + 1 == total {
            last.t_end.abs_diff(range.to_ms)
        } else {
            0
        };
        if padded_before_ms > 4_000 || padded_after_ms > 4_000 {
            warnings.push(format!(
                "警告：輸出長度 {} ms，超出請求範圍（+4 秒上界）：輸出 {}..{} ms，請求 {}..{} ms，padding before={} ms after={} ms",
                last.t_end.saturating_sub(first.t_start),
                first.t_start,
                last.t_end,
                range.from_ms,
                range.to_ms,
                padded_before_ms,
                padded_after_ms
            ));
        }
        outputs.push(ExportOutput {
            path: final_path.clone(),
            t_start_ms: first.t_start,
            t_end_ms: last.t_end,
            padded_before_ms,
            padded_after_ms,
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
        excluded_segments: excluded,
        warnings,
    })
}

/// 執行 ffmpeg 並在 SIGINT 時真的把 child 砍掉；同時把 stdout/stderr 讀乾，
/// 否則 ffmpeg 的警告會塞滿 pipe 而卡死（`try_wait` 不會替我們讀 pipe）。
fn run_ffmpeg(mut cmd: Command, options: &ExportOptions) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Export(format!("執行 ffmpeg 失敗：{e}")))?;
    let mut stdout = child.stdout.take().expect("stdout pipe");
    let mut stderr = child.stderr.take().expect("stderr pipe");
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr, &mut buf);
        buf
    });
    let status = loop {
        if cancelled(options) {
            let _ = child.kill();
            let status = child
                .wait()
                .map_err(|e| Error::Export(format!("等待 ffmpeg 失敗：{e}")))?;
            let _ = out_thread.join();
            let _ = err_thread.join();
            let _ = status;
            return Err(Error::Export("已取消（SIGINT），未留下半成品".into()));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(Error::Export(format!("等待 ffmpeg 失敗：{e}"))),
        }
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
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
            gap_ms: None,
            detail: format!("未知（段邊界差 {delta} ms）；不靜默拼接"),
        },
    }
}

fn write_concat_list(path: &Path, segments: &[(PathBuf, f64)]) -> Result<()> {
    let mut text = String::new();
    for (segment, dur_secs) in segments {
        text.push_str(&format!("file '{}'\n", escape_concat_path(segment)));
        text.push_str(&format!("duration {:.6}\n", dur_secs));
    }
    fs::write(path, text)?;
    Ok(())
}

/// 把單一段 remux 成 pts/dts 從 0 開始的暫存檔（不重編碼）。
///
/// `setts` 是 bitstream filter，對 `-c copy` 生效；只平移時間戳，不動 NAL。
fn remux_zero_based(
    ffmpeg: &Path,
    source: &Path,
    dest: &Path,
    options: &ExportOptions,
) -> Result<()> {
    let mut cmd = Command::new(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-nostdin")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(source)
        .arg("-map")
        .arg("0:v:0")
        .arg("-c")
        .arg("copy")
        .arg("-bsf:v")
        .arg("setts=pts=PTS-STARTPTS:dts=DTS-STARTPTS")
        .arg("-f")
        .arg("matroska")
        .arg(dest);
    let out = run_ffmpeg(cmd, options)?;
    if !out.status.success() {
        return Err(Error::Export(format!(
            "ffmpeg 重基準段檔失敗（{}）：{}",
            source.display(),
            tail(&out.stderr)
        )));
    }
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
                assert_eq!(gap_ms, None, "未知就是未知，不得填成推測值");
                assert!(detail.contains("未知"));
            }
            _ => panic!("未知 gap 必須分檔"),
        }
    }

    #[test]
    fn unknown_gap_stderr_says_unknown_not_a_number() {
        let gap = ExportGap {
            at_ms: 9_500,
            gap_ms: None,
            prev_segment: Some("seg-000000005500.mkv".into()),
            next_segment: "seg-000000007500.mkv".into(),
            detail: "未知（段邊界差 2000 ms）；不靜默拼接".into(),
        };
        let line = gap.stderr_line();
        assert!(line.contains("未知"), "未知 gap 必須標示：{line}");
        assert!(line.contains("2000"), "要帶實際邊界差：{line}");
    }

    #[test]
    fn overlapping_boundary_is_split_not_joined() {
        let a = row("seg-000000000000.mkv", 0, 3_000, Some(0));
        let b = row("seg-000000002000.mkv", 2_000, 4_000, Some(0));
        assert!(matches!(kind(&a, &b), GapKind::Gap { .. }));
    }

    // ---- 可獨立解碼的判定：只看 first_block_keyframe，不看 idr_offset ----

    fn avc() -> crate::mkv::AvcConfig {
        crate::mkv::AvcConfig {
            sps: vec![0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9],
            pps: vec![0x68, 0xEE, 0x3C, 0x80],
        }
    }

    fn scan_tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("camcore-export-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建立暫存目錄");
        dir
    }

    /// 寫一個兩 block 的小段檔；`first_keyframe` 決定段首是 IDR（0x65）還是 P（0x41）。
    fn write_scan_segment(dir: &Path, first_keyframe: bool) -> String {
        let name = "seg-000000000000.mkv".to_string();
        let path = dir.join(&name);
        let mut writer = crate::mkv::MkvWriter::create(&path, &avc(), 640, 480, 0).unwrap();
        let first = if first_keyframe { 0x65 } else { 0x41 };
        writer
            .append_block(
                &crate::mkv::avcc_frame(&[first, 1, 2, 3]),
                0,
                first_keyframe,
            )
            .unwrap();
        writer
            .append_block(&crate::mkv::avcc_frame(&[0x41, 4]), 40, false)
            .unwrap();
        writer.finish().unwrap();
        name
    }

    /// k24 硬切段的形狀：`first_block_keyframe=Some(false)` 且 `idr_offset=0`。
    /// 旗標已明確不可解，不得因為 `idr_offset` 是 0 或目錄不存在而放行／去掃描。
    #[test]
    fn p_first_flag_with_zero_idr_offset_is_not_playable() {
        let mut r = row("seg-000000016000.mkv", 16_000, 32_000, Some(0));
        r.first_block_keyframe = Some(false);
        r.idr_offset = 0;
        assert!(!playable_first_block(Path::new("/nonexistent-k25"), &r).unwrap());
    }

    /// `None`（v3 以前的列）要現場掃描：段首不是 keyframe 就不可解。
    #[test]
    fn null_flag_scans_and_rejects_p_first_segment() {
        let dir = scan_tmp_dir("scan-pfirst");
        let name = write_scan_segment(&dir, false);
        let mut r = row(&name, 0, 80, Some(0));
        r.first_block_keyframe = None;
        r.idr_offset = 0;
        assert!(
            !playable_first_block(&dir, &r).unwrap(),
            "NULL 要掃描：段首非 keyframe 必須判為不可解"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 對照組：同一條掃描路徑掃到段首 IDR 要放行，證明上一條不是永遠 false。
    #[test]
    fn null_flag_scans_and_accepts_keyframe_segment() {
        let dir = scan_tmp_dir("scan-kf");
        let name = write_scan_segment(&dir, true);
        let mut r = row(&name, 0, 80, Some(0));
        r.first_block_keyframe = None;
        r.idr_offset = 0;
        assert!(
            playable_first_block(&dir, &r).unwrap(),
            "掃描到段首 IDR 必須可解"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
