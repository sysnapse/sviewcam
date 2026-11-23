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
use camcore::snapshot::{self, SnapshotRequest};
use camcore::{RingConfig, SegmentIndex};
use camipc::{path::resolve_socket, Client};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

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
    /// 問 camd 的狀態（相機、錄影、索引筆數）
    Status(StatusArgs),
    /// 列出 camd 索引內的段
    ListSegments(ListSegmentsArgs),
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

#[derive(Args, Debug)]
struct StatusArgs {
    /// 控制 socket 路徑（預設依 SVCAM_SOCKET／XDG_RUNTIME_DIR／HOME 解析）
    #[arg(long)]
    socket: Option<PathBuf>,

    /// 以 JSON 印出協議回應（stdout 只有 JSON；人看的摘要走 stderr）
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ListSegmentsArgs {
    /// 控制 socket 路徑（預設依 SVCAM_SOCKET／XDG_RUNTIME_DIR／HOME 解析）
    #[arg(long)]
    socket: Option<PathBuf>,

    /// 起點，相對 camd 最新一段 t_end 的負時間（例如 -3m、-90s）；省略＝不限
    #[arg(long, allow_hyphen_values = true)]
    from: Option<String>,

    /// 終點，相對 camd 最新一段 t_end 的負時間（例如 -1m、0）；省略＝不限
    #[arg(long, allow_hyphen_values = true)]
    to: Option<String>,

    /// 最多回幾筆（1–1000，預設 100）
    #[arg(long, default_value_t = 100)]
    limit: u32,

    /// 以 JSON 印出（stdout 只有 JSON；人看的摘要走 stderr）
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let flag = Arc::new(AtomicBool::new(false));
    let _ = CANCEL_FLAG.set(flag.clone());
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }

    let cli = Cli::parse();
    match cli.command {
        Command::Export(args) => run_export(args, &flag),
        Command::Snapshot(args) => run_snapshot(args, &flag),
        Command::Status(args) => run_status(args),
        Command::ListSegments(args) => run_list_segments(args),
    }
}

/// 連上 camd 並送一個請求；連線／協議錯誤一律 exit 2（規格見 README）。
fn camd_call(
    socket: Option<PathBuf>,
    method: &str,
    params: Value,
) -> Result<(Value, PathBuf), ExitCode> {
    let path = match resolve_socket(socket.as_deref()) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("錯誤：{message}");
            return Err(ExitCode::from(2));
        }
    };
    let mut client = match Client::connect(&path) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("錯誤：連不上 camd（socket: {}）：{e}", path.display());
            return Err(ExitCode::from(2));
        }
    };
    let response = match client.call(method, params) {
        Ok(response) => response,
        Err(e) => {
            eprintln!("錯誤：與 camd 通訊失敗（socket: {}）：{e}", path.display());
            return Err(ExitCode::from(2));
        }
    };
    match camipc::client::into_result(response) {
        Ok(result) => Ok((result, path)),
        Err(error) => {
            eprintln!(
                "錯誤：camd 回 {}：{}（socket: {}）",
                error.code.as_str(),
                error.message,
                path.display()
            );
            Err(ExitCode::from(2))
        }
    }
}

fn run_status(args: StatusArgs) -> ExitCode {
    let (result, path) = match camd_call(args.socket, "status", json!({})) {
        Ok(value) => value,
        Err(code) => return code,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
        return ExitCode::SUCCESS;
    }
    let camd = &result["camd"];
    let camera = &result["camera"];
    let recording = &result["recording"];
    eprintln!(
        "camd：pid={} version={} uptime={:.1}s",
        camd["pid"],
        camd["version"].as_str().unwrap_or("?"),
        camd["uptime_ms"].as_u64().unwrap_or(0) as f64 / 1000.0
    );
    eprintln!("socket：{}", path.display());
    eprintln!(
        "相機：{}（reconnects={}）{}",
        camera["state"].as_str().unwrap_or("?"),
        camera["reconnects"],
        camera["last_error"]
            .as_str()
            .map(|e| format!(" last_error={e}"))
            .unwrap_or_default()
    );
    eprintln!(
        "錄影：segments={} total_bytes={} latest_end_ms={} current_segment={}",
        recording["segments"],
        recording["total_bytes"],
        recording["latest_end_ms"],
        recording["current_segment"]["segment_id"]
            .as_str()
            .unwrap_or("-")
    );
    ExitCode::SUCCESS
}

fn run_list_segments(args: ListSegmentsArgs) -> ExitCode {
    // 相對時間的錨點是 camd 回報的最新 t_end，不是 wall clock。
    let anchor = if args.from.is_some() || args.to.is_some() {
        let (status, _) = match camd_call(args.socket.clone(), "status", json!({})) {
            Ok(value) => value,
            Err(code) => return code,
        };
        match status["recording"]["latest_end_ms"].as_u64() {
            Some(anchor) => Some(anchor),
            None => {
                eprintln!("錯誤：索引內沒有已閉合的段，無法用相對時間換算（camd 回報 latest_end_ms=null）");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };

    let mut params = serde_json::Map::new();
    params.insert("limit".to_string(), json!(args.limit));
    let mut from_ms = None;
    let mut to_ms = None;
    if let (Some(text), Some(anchor)) = (&args.from, anchor) {
        match parse_offset(text) {
            Ok(off) => match anchor.checked_sub(off) {
                Some(value) => {
                    from_ms = Some(value);
                    params.insert("from".to_string(), json!(value));
                }
                None => {
                    eprintln!("錯誤：相對時間超出可表示的範圍");
                    return ExitCode::from(2);
                }
            },
            Err(e) => {
                eprintln!("錯誤：{e}");
                return ExitCode::from(2);
            }
        }
    }
    if let (Some(text), Some(anchor)) = (&args.to, anchor) {
        match parse_offset(text) {
            Ok(off) => match anchor.checked_sub(off) {
                Some(value) => {
                    to_ms = Some(value);
                    params.insert("to".to_string(), json!(value));
                }
                None => {
                    eprintln!("錯誤：相對時間超出可表示的範圍");
                    return ExitCode::from(2);
                }
            },
            Err(e) => {
                eprintln!("錯誤：{e}");
                return ExitCode::from(2);
            }
        }
    }

    let (result, _path) = match camd_call(args.socket, "list_segments", Value::Object(params)) {
        Ok(value) => value,
        Err(code) => return code,
    };
    if args.json {
        let mut out = serde_json::Map::new();
        out.insert("anchor_ms".to_string(), json!(anchor));
        out.insert("from_ms".to_string(), json!(from_ms));
        out.insert("to_ms".to_string(), json!(to_ms));
        out.insert("limit".to_string(), json!(args.limit));
        out.insert("segments".to_string(), result["segments"].clone());
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Object(out)).unwrap()
        );
        return ExitCode::SUCCESS;
    }
    let Some(rows) = result["segments"].as_array() else {
        eprintln!("錯誤：camd 回應缺少 segments");
        return ExitCode::from(2);
    };
    for row in rows {
        eprintln!(
            "{} name={} t_start={} t_end={} bytes={} first_block_keyframe={} gap_ms={}",
            row["segment_id"].as_str().unwrap_or("?"),
            row["name"].as_str().unwrap_or("?"),
            row["t_start"],
            row["t_end"],
            row["bytes"],
            row["first_block_keyframe"],
            row["gap_ms"]
        );
    }
    eprintln!(
        "共 {} 段（anchor={:?} from={:?} to={:?}）",
        rows.len(),
        anchor,
        from_ms,
        to_ms
    );
    ExitCode::SUCCESS
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
    let (Some(from_ms), Some(to_ms)) = (anchor.checked_sub(from_off), anchor.checked_sub(to_off))
    else {
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
        ExportRange { from_ms, to_ms },
        &args.output,
        &options,
    ) {
        Ok(report) => {
            // 警告與 gap 一律走 stderr；--json 只影響 stdout 的資料格式。
            for g in &report.gaps {
                eprintln!("{}", g.stderr_line());
            }
            for w in &report.warnings {
                eprintln!("export: {w}");
            }
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
                        "padded_before_ms": o.padded_before_ms,
                        "padded_after_ms": o.padded_after_ms,
                        "segments": o.segments,
                    })).collect::<Vec<_>>(),
                    "gaps": report.gaps.iter().map(|g| serde_json::json!({
                        "at_ms": g.at_ms,
                        "gap_ms": g.gap_ms,
                        "prev_segment": g.prev_segment,
                        "next_segment": g.next_segment,
                        "detail": g.detail,
                    })).collect::<Vec<_>>(),
                    "excluded_segments": report.excluded_segments.iter().map(|e| serde_json::json!({
                        "id": e.id,
                        "reason": e.reason,
                    })).collect::<Vec<_>>(),
                    "warnings": report.warnings,
                });
                println!("{}", serde_json::to_string_pretty(&json).unwrap());
            } else {
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

fn run_snapshot(args: SnapshotArgs, flag: &Arc<AtomicBool>) -> ExitCode {
    let index = match open_index(&args.dir) {
        Ok(index) => index,
        Err(e) => return fail(&e.to_string()),
    };
    let Some(anchor) = latest_end(&index) else {
        return fail("索引內沒有已閉合的段，無法截圖");
    };
    let off = match parse_offset(&args.at) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let Some(at_ms) = anchor.checked_sub(off) else {
        return fail("相對時間超出可表示的範圍");
    };
    let request = SnapshotRequest {
        at_ms,
        output: args.output,
        ffmpeg: args.ffmpeg,
        force: args.force,
        cancel: Some(flag.clone()),
    };
    match snapshot::snapshot(&index, &args.dir, &request) {
        Ok(report) => {
            if args.json {
                let json = serde_json::json!({
                    "snapshot": {
                        "at_ms": report.at_ms,
                        "actual_ms": report.actual_ms,
                        "path": report.path,
                        "segment": report.segment,
                        "segment_idr_at_ms": report.segment_idr_at_ms,
                        "width": report.width,
                        "height": report.height,
                    }
                });
                println!("{}", serde_json::to_string_pretty(&json).unwrap());
            } else {
                println!("{}", report.path.display());
                eprintln!(
                    "snapshot: at={} ms actual={} ms segment={} idr={} ms",
                    report.at_ms, report.actual_ms, report.segment, report.segment_idr_at_ms
                );
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
    let n: u64 = num.parse().map_err(|_| format!("時間數字無效：{text:?}"))?;
    let mult = match suffix {
        "s" | "" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        other => return Err(format!("時間單位只支援 s/m/h（實得 {other:?}）：{text:?}")),
    };
    n.checked_mul(mult)
        .ok_or_else(|| format!("時間太大：{text:?}"))
}

fn fail(message: &str) -> ExitCode {
    eprintln!("錯誤：{message}");
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::parse_offset;

    #[test]
    fn parses_negative_relative_times() {
        assert_eq!(parse_offset("-90s"), Ok(90_000));
        assert_eq!(parse_offset("-3m"), Ok(180_000));
        assert_eq!(parse_offset("-1h"), Ok(3_600_000));
        assert_eq!(parse_offset("0"), Ok(0));
        assert_eq!(parse_offset("-0"), Ok(0));
    }

    #[test]
    fn rejects_positive_and_bad_units() {
        assert!(parse_offset("3m").is_err());
        assert!(parse_offset("-3d").is_err());
        assert!(parse_offset("--3m").is_err());
        assert!(parse_offset("-").is_err());
    }
}
