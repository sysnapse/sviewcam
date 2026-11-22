//! `camctl`：M2 片段匯出與回溯截圖 CLI。
//!
//! exit code：0 成功（含跨 gap 分檔）；2 參數／環境／範圍／解碼錯誤；
//! 130 使用者按 Ctrl-C 取消。
//!
//! 相對時間一律以「索引內最後一段的 `t_end`」為基準，不是 wall clock：
//! 錄影時間是 host 單調時鐘，重開程式後不能拿 `SystemTime` 對應；用最後一段
//! 才不會在錄影停止後把 `--from -3m` 指到錯誤的位置。`--help` 有寫明。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use camcore::export::{self, ExportOptions, ExportRange};
use camcore::{RingConfig, SegmentIndex};
use clap::{Args, Parser, Subcommand};

static CANCEL_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

extern "C" fn on_sigint(_: libc::c_int) {
    if let Some(flag) = CANCEL_FLAG.get() {
        flag.store(true, Ordering::SeqCst);
    }
}

#[derive(Parser, Debug)]
#[command(
    about = "SViewCam 片段匯出與截圖（M2）",
    version,
    after_help = "相對時間以錄影索引最後一段的 t_end 為基準；例如 --from -3m --to -1m。"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 把錄影緩衝的指定範圍匯出成 MP4（-c copy，不重編碼）
    Export(ExportArgs),
    /// 從錄影緩衝取指定時間的畫面成 JPEG
    Snapshot(SnapshotArgs),
}

#[derive(Args, Debug)]
struct ExportArgs {
    /// 錄影資料目錄（內含 seg-*.mkv 與 index.sqlite）
    #[arg(long)]
    dir: PathBuf,

    /// 起點，相對最後一段 t_end 的負時間（例如 -3m、-90s）
    #[arg(long, allow_hyphen_values = true)]
    from: String,

    /// 終點，相對最後一段 t_end 的負時間（例如 -1m、0）
    #[arg(long, allow_hyphen_values = true)]
    to: String,

    /// 輸出 MP4；跨 gap 時會變成 <stem>.1.mp4、<stem>.2.mp4
    #[arg(short = 'o', long)]
    output: PathBuf,

    /// 以 JSON 印出結果（stdout 只有 JSON；人看的訊息走 stderr）
    #[arg(long)]
    json: bool,

    /// 覆蓋既有輸出檔
    #[arg(long)]
    force: bool,

    /// ffmpeg 執行檔（預設 PATH 的 ffmpeg）
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,
}

#[derive(Args, Debug)]
struct SnapshotArgs {
    /// 錄影資料目錄（內含 seg-*.mkv 與 index.sqlite）
    #[arg(long)]
    dir: PathBuf,

    /// 目標時間，相對最後一段 t_end 的負時間（例如 -90s）
    #[arg(long, allow_hyphen_values = true)]
    at: String,

    /// 輸出 JPEG
    #[arg(short = 'o', long)]
    output: PathBuf,

    /// 以 JSON 印出結果（stdout 只有 JSON；人看的訊息走 stderr）
    #[arg(long)]
    json: bool,

    /// 覆蓋既有輸出檔
    #[arg(long)]
    force: bool,

    /// ffmpeg 執行檔（預設 PATH 的 ffmpeg）
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,
}

fn main() -> ExitCode {
    let flag = Arc::new(AtomicBool::new(false));
    let _ = CANCEL_FLAG.set(flag.clone());
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as libc::sighandler_t);
    }

    let cli = Cli::parse();
    match cli.command {
        Command::Export(args) => run_export(args, &flag),
        Command::Snapshot(args) => run_snapshot(args),
    }
}

fn run_export(args: ExportArgs, flag: &Arc<AtomicBool>) -> ExitCode {
    let index = match open_index(&args.dir) {
        Ok(index) => index,
        Err(e) => return fail(&e.to_string()),
    };
    let Some(anchor) = latest_end(&index) else {
        return fail("索引內沒有已閉合的段，無法匯出");
    };
    let from_off = match parse_offset(&args.from) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let to_off = match parse_offset(&args.to) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let (Some(from_ms), Some(to_ms)) = (anchor.checked_sub(from_off), anchor.checked_sub(to_off)) else {
        return fail("相對時間超出可表示的範圍");
    };
    let options = ExportOptions {
        ffmpeg: args.ffmpeg,
        force: args.force,
        cancel: Some(flag.clone()),
    };
    match export::export(
        &args.dir,
        &index,
        ExportRange {
            from_ms,
            to_ms,
        },
        &args.output,
        &options,
    ) {
        Ok(report) => {
            if args.json {
                let json = serde_json::json!({
                    "anchor_ms": report.anchor_ms,
                    "available": {
                        "first_t_start_ms": report.available_from_ms,
                        "last_t_end_ms": report.available_to_ms,
                    },
                    "range": {
                        "from_ms": report.from_ms,
                        "to_ms": report.to_ms,
                    },
                    "outputs": report.outputs.iter().map(|o| serde_json::json!({
                        "path": o.path,
                        "t_start_ms": o.t_start_ms,
                        "t_end_ms": o.t_end_ms,
                        "segments": o.segments,
                    })).collect::<Vec<_>>(),
                    "gaps": report.gaps.iter().map(|g| serde_json::json!({
                        "at_ms": g.at_ms,
                        "gap_ms": g.gap_ms,
                        "prev_segment": g.prev_segment,
                        "next_segment": g.next_segment,
                        "detail": g.detail,
                    })).collect::<Vec<_>>(),
                    "warnings": report.warnings,
                });
                println!("{}", serde_json::to_string_pretty(&json).unwrap());
            } else {
                for g in &report.gaps {
                    eprintln!(
                        "export: gap {} ms at t={} between {} and {}; writing separate output",
                        g.gap_ms
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".to_string()),
                        g.at_ms,
                        g.prev_segment.as_deref().unwrap_or("?"),
                        g.next_segment
                    );
                }
                for w in &report.warnings {
                    eprintln!("export: 警告：{w}");
                }
                for o in &report.outputs {
                    println!("{}", o.path.display());
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if flag.load(Ordering::SeqCst) {
                eprintln!("錯誤：已取消（SIGINT），未留下半成品");
                ExitCode::from(130)
            } else {
                fail(&e.to_string())
            }
        }
    }
}

fn run_snapshot(args: SnapshotArgs) -> ExitCode {
    let _ = args;
    fail("snapshot 尚未實作（CP2 骨架；CP3 補上 IDR 對齊的截圖）")
}

fn open_index(dir: &std::path::Path) -> camcore::Result<SegmentIndex> {
    SegmentIndex::open(RingConfig::new(dir).index_path())
}

fn latest_end(index: &SegmentIndex) -> Option<u64> {
    index.latest_end().ok().flatten()
}

/// 把 `-3m`／`-90s`／`0` 解析成正的毫秒位移；正值拒絕，避免把絕對 epoch 混進來。
fn parse_offset(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if text == "0" {
        return Ok(0);
    }
    let Some(rest) = text.strip_prefix('-') else {
        return Err(format!(
            "時間必須是負的相對時間（例如 -3m、-90s、0），實得 {text:?}"
        ));
    };
    if rest.is_empty() {
        return Err("時間缺少數字".to_string());
    }
    let split = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (num, suffix) = rest.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("時間數字無效：{text:?}"))?;
    let mult = match suffix {
        "s" | "" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        other => {
            return Err(format!(
                "時間單位只支援 s/m/h（實得 {other:?}）：{text:?}"
            ))
        }
    };
    n.checked_mul(mult)
        .ok_or_else(|| format!("時間太大：{text:?}"))
}

fn fail(message: &str) -> ExitCode {
    eprintln!("錯誤：{message}");
    ExitCode::from(2)
}
