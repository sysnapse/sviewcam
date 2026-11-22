//! 回溯截圖：從指定時間所屬的段取一張 JPEG。
//!
//! 「IDR 對齊」的定義是**解碼起點**對齊該段段首 IDR，不是要求 JPEG 本身是 IDR：
//! `-ss` 放在 `-i` 之前，ffmpeg 會從段首 keyframe 開始解碼、丟掉 offset 之前的
//! 顯示幀，輸出 `at` 之後的最近顯示幀。2 秒 GOP 下要求 JPEG 本身是 IDR 會與
//! 0.5 秒時間誤差互斥，因此不採那個解讀。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::export::check_ffmpeg;
use crate::index::{SegmentIndex, SegmentRow};

/// 截圖請求。
#[derive(Debug, Clone)]
pub struct SnapshotRequest {
    /// 目標時間（毫秒，與段索引同一時間基準）。
    pub at_ms: u64,
    /// 輸出 JPEG。
    pub output: PathBuf,
    /// ffmpeg 執行檔。
    pub ffmpeg: PathBuf,
    /// 覆蓋既有輸出。
    pub force: bool,
    /// Ctrl-C 取消旗標。
    pub cancel: Option<Arc<AtomicBool>>,
}

impl SnapshotRequest {
    /// 建立一個使用 PATH 上 `ffmpeg` 的請求。
    pub fn new(at_ms: u64, output: impl Into<PathBuf>) -> Self {
        Self {
            at_ms,
            output: output.into(),
            ffmpeg: PathBuf::from("ffmpeg"),
            force: false,
            cancel: None,
        }
    }
}

/// 截圖結果；`actual_ms` 由 ffmpeg `showinfo` 的 `pts_time` 反推，不採信 stdout 人話。
#[derive(Debug, Clone)]
pub struct SnapshotReport {
    pub at_ms: u64,
    pub actual_ms: u64,
    pub path: PathBuf,
    pub segment: String,
    /// 所屬段的段首 IDR 時間（解碼起點）。
    pub segment_idr_at_ms: u64,
    pub width: u32,
    pub height: u32,
}

/// 從 `dir` 的錄影索引取 `at_ms` 的畫面。
pub fn snapshot(index: &SegmentIndex, dir: &Path, req: &SnapshotRequest) -> Result<SnapshotReport> {
    check_ffmpeg(&req.ffmpeg)?;
    let rows: Vec<SegmentRow> = index.rows()?.into_iter().filter(|r| r.closed).collect();
    if rows.is_empty() {
        return Err(Error::Export("索引內沒有已閉合的段，無法截圖".into()));
    }
    let available_from = rows.first().expect("非空").t_start;
    let available_to = rows.last().expect("非空").t_end;
    if req.at_ms < available_from || req.at_ms > available_to {
        return Err(Error::Export(format!(
            "截圖時間 {} ms 不在可用範圍 [{} ms, {} ms] 內（保留窗可能已修剪）",
            req.at_ms, available_from, available_to
        )));
    }
    let row = rows
        .iter()
        .find(|r| r.t_start <= req.at_ms && req.at_ms <= r.t_end)
        .ok_or_else(|| {
            Error::Export(format!(
                "截圖時間 {} ms 落在沒有段的空窗；可用範圍 [{} ms, {} ms]",
                req.at_ms, available_from, available_to
            ))
        })?;
    if row.t_end <= row.t_start {
        return Err(Error::Export(format!("段 {} 的時間範圍無效", row.name)));
    }
    match row.first_block_keyframe {
        Some(true) => {}
        Some(false) => {
            return Err(Error::Export(format!(
                "段 {} 首 block 不是 keyframe，不可獨立解碼，拒絕截圖",
                row.name
            )))
        }
        None => {
            let info = crate::mkv::scan(dir.join(&row.name))?;
            if info.first_block_keyframe != Some(true) {
                return Err(Error::Export(format!(
                    "段 {} 首 block 不是 keyframe（掃描確認），拒絕截圖",
                    row.name
                )));
            }
        }
    }

    let output = &req.output;
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
    let final_path = parent.join(
        output
            .file_name()
            .ok_or_else(|| Error::Export("輸出檔名無效".into()))?,
    );
    if final_path.exists() && !req.force {
        return Err(Error::Export(format!(
            "輸出檔已存在：{}（要覆蓋請加 --force）",
            final_path.display()
        )));
    }

    let tmp = parent.join(format!(".k14-{}-snapshot.tmp", std::process::id()));
    let _guard = TempFile(tmp.clone());
    // 時間剛好落在 t_end 時取最後一個可解碼的毫秒，避免 -ss 指到檔尾。
    let offset = req
        .at_ms
        .saturating_sub(row.t_start)
        .min(row.t_end.saturating_sub(row.t_start).saturating_sub(1));
    let offset_secs = offset as f64 / 1000.0;
    let mut cmd = Command::new(&req.ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-nostdin")
        .arg("-loglevel")
        .arg("info")
        .arg("-y")
        .arg("-copyts")
        .arg("-ss")
        .arg(format!("{offset_secs:.3}"))
        .arg("-i")
        .arg(dir.join(&row.name))
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg("showinfo")
        .arg("-q:v")
        .arg("2")
        .arg("-f")
        .arg("image2")
        .arg(&tmp);
    let out = run_with_cancel(cmd, req.cancel.as_deref())?;
    if !out.status.success() {
        return Err(Error::Export(format!(
            "ffmpeg 截圖失敗：{}",
            tail(&out.stderr)
        )));
    }
    if !tmp.exists() {
        return Err(Error::Export("ffmpeg 回報成功但沒有輸出 JPEG".into()));
    }
    let actual_ms = parse_pts_time(&out.stderr).ok_or_else(|| {
        Error::Export("ffmpeg showinfo 沒有 pts_time，無法確認實際截圖時間".into())
    })?;
    let bytes = fs::read(&tmp)?;
    let (width, height) = jpeg_dimensions(&bytes)
        .ok_or_else(|| Error::Export("無法從 JPEG 標頭讀出尺寸".into()))?;
    fs::rename(&tmp, &final_path)?;
    Ok(SnapshotReport {
        at_ms: req.at_ms,
        actual_ms,
        path: final_path,
        segment: row.name.clone(),
        segment_idr_at_ms: row.t_start,
        width,
        height,
    })
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn run_with_cancel(mut cmd: Command, cancel: Option<&AtomicBool>) -> Result<Output> {
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
        if cancel.is_some_and(|f| f.load(Ordering::SeqCst)) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
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

fn tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

fn parse_pts_time(stderr: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(stderr);
    for line in text.lines() {
        if let Some(pos) = line.find("pts_time:") {
            let rest = &line[pos + "pts_time:".len()..];
            let token: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                .collect();
            if let Ok(secs) = token.parse::<f64>() {
                if secs >= 0.0 && secs.is_finite() {
                    return Some((secs * 1000.0).round() as u64);
                }
            }
        }
    }
    None
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut i = 2;
    while i + 9 < bytes.len() {
        if bytes[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        if marker == 0xd8 || marker == 0xd9 || (0xd0..=0xd7).contains(&marker) {
            i += 2;
            continue;
        }
        if i + 4 > bytes.len() {
            return None;
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if len < 2 || i + 2 + len > bytes.len() {
            return None;
        }
        // SOF0..SOF15（排除 C4 DHT、C8 JPG、CC DAC）。
        if (0xc0..=0xcf).contains(&marker)
            && marker != 0xc4
            && marker != 0xc8
            && marker != 0xcc
        {
            let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]);
            let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]);
            return Some((w as u32, h as u32));
        }
        i += 2 + len;
    }
    None
}
