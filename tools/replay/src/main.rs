//! `replay`：把錄製封包（`.sviewcap.jsonl`）餵回同一條重連狀態機，無相機也能跑全鏈路。
//!
//! exit code：0 正常；3 fixture 內 `expected` 與實跑不符；4 fixture 解析失敗。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rtsp_core::replay::{ReplayFactory, ReplayFixture};
use rtsp_core::sink::{FileSink, NalSink, NullSink};
use rtsp_core::{run, Event, SessionConfig, VirtualClock};

#[derive(Parser, Debug)]
#[command(about = "Replay a recorded SView session through the real state machine", version)]
struct Args {
    /// fixture 路徑（JSON Lines）
    #[arg(long)]
    input: PathBuf,

    /// JSON 報告輸出路徑
    #[arg(long)]
    report: Option<PathBuf>,

    /// 另存 Annex-B 碼流
    #[arg(long)]
    annexb: Option<PathBuf>,

    /// 與 fixture 的 expected 比對（預設開啟；--check=false 可關）
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    check: bool,

    /// 重放用的虛擬 RTP 埠
    #[arg(long, default_value_t = 20000)]
    client_port: u16,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();

    let fixture = match ReplayFixture::load(&args.input) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("錯誤：fixture 解析失敗 {}：{e}", args.input.display());
            return std::process::ExitCode::from(4);
        }
    };

    let clock = Arc::new(VirtualClock::new());
    let factory = ReplayFactory::new(fixture.clone(), clock.clone());

    let mut cfg = SessionConfig::new("rtsp://fixture/replay", "fixture-token");
    // 重放不靠牆鐘，duration 設得很長；尾段以 EndOfInput 收尾。
    cfg.duration = Some(Duration::from_secs(3600));
    cfg.heartbeat = None;
    cfg.recv_timeout = Duration::from_secs(3600);

    let mut file_sink: Option<FileSink> = match &args.annexb {
        Some(path) => match FileSink::create(path) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("錯誤：無法建立 Annex-B 檔 {}：{e}", path.display());
                return std::process::ExitCode::from(2);
            }
        },
        None => None,
    };
    let mut null_sink = NullSink;
    let sink: &mut dyn NalSink = match file_sink.as_mut() {
        Some(s) => s,
        None => &mut null_sink,
    };

    let mut log = |ev: Event| match ev {
        Event::Rst { at_ms, reason } => println!("斷流 t+{at_ms}ms：{reason}"),
        Event::Reconnected { after_ms, gap_ms } => {
            println!("重連完成 t+{after_ms}ms（空窗 {gap_ms}ms）")
        }
        Event::ConnectFailed { attempt, error } => println!("連線失敗 #{attempt}：{error}"),
        Event::Finished { reason } => println!("結束：{reason}"),
        _ => {}
    };

    let report = run(&factory, &cfg, &clock, sink, &mut log).await;
    let json = serde_json::to_string_pretty(&report).expect("report serialises");
    if let Some(path) = &args.report {
        if let Err(e) = std::fs::write(path, &json) {
            eprintln!("錯誤：寫報告失敗 {}：{e}", path.display());
            return std::process::ExitCode::from(2);
        }
    }
    println!("{json}");

    let meta = &factory.fixture().meta;
    println!(
        "fixture: {} 段連線、{} 個事件{}",
        factory.fixture().connections.len(),
        factory.fixture().step_count(),
        meta.note.as_deref().map(|n| format!("（{n}）")).unwrap_or_default()
    );

    if args.check {
        let mut diffs = Vec::new();
        if let Some(exp) = meta.expected_reconnects {
            if report.reconnects != exp {
                diffs.push(format!("reconnects 期望 {exp}，實得 {}", report.reconnects));
            }
        }
        if let Some(exp) = meta.expected_seq_gaps {
            if report.seq_gaps.len() != exp {
                diffs.push(format!("seq_gaps 期望 {exp}，實得 {}", report.seq_gaps.len()));
            }
        }
        if let Some(exp) = meta.expected_nals {
            if report.nals != exp {
                diffs.push(format!("nals 期望 {exp}，實得 {}", report.nals));
            }
        }
        if let Some(exp) = meta.expected_connections {
            if report.connects != exp {
                diffs.push(format!("connects 期望 {exp}，實得 {}", report.connects));
            }
        }
        if !diffs.is_empty() {
            eprintln!("fixture 期望不符：\n  {}", diffs.join("\n  "));
            return std::process::ExitCode::from(3);
        }
        println!("fixture 期望全部符合。");
    }

    std::process::ExitCode::SUCCESS
}
