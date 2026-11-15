//! `probe`：對真機（或任何跑同一協議的來源）開流 N 秒，結束印統計。
//!
//! 權杖只從環境變數讀（預設 `SVIEW_TOKEN`），不落盤、不進報告、不進日誌。

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use rtsp_core::link::{HeartbeatConfig, TcpLinkFactory};
use rtsp_core::sink::{FileSink, NalSink, NullSink};
use rtsp_core::{
    run, Event, RealClock, ReconnectPolicy, SessionConfig, DEFAULT_RTSP_PORT, DEFAULT_STREAM_PATH,
};

#[derive(Parser, Debug)]
#[command(about = "SViewWifi M0 spike probe: open the stream, collect stats", version)]
struct Args {
    /// 相機位址（Soft-AP 下是 192.168.1.1）
    #[arg(long, default_value = rtsp_core::DEFAULT_CAMERA_HOST)]
    host: String,

    #[arg(long, default_value_t = DEFAULT_RTSP_PORT)]
    port: u16,

    #[arg(long, default_value = DEFAULT_STREAM_PATH)]
    path: String,

    /// 收流秒數
    #[arg(long, default_value_t = 30)]
    duration: u64,

    /// 讀取 SViewWifi 權杖的環境變數名稱
    #[arg(long, default_value = "SVIEW_TOKEN")]
    token_env: String,

    /// 收 RTP 的本機位址（多網卡時指去熱點那張網卡）
    #[arg(long)]
    bind_ip: Option<String>,

    /// UDP 13000 心跳間隔；0 = 不發
    #[arg(long, default_value_t = 2000)]
    heartbeat_ms: u64,

    /// 收包靜默多久視為斷流
    #[arg(long, default_value_t = 5000)]
    recv_timeout_ms: u64,

    /// 重連上限；0 = 不限
    #[arg(long, default_value_t = 50)]
    max_reconnects: u32,

    /// 寫出 JSON 報告
    #[arg(long)]
    out: Option<PathBuf>,

    /// 另存 Annex-B 碼流（除錯用）
    #[arg(long)]
    annexb: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();

    // 標準 4：權杖缺失要明確報錯、非零 exit、不 panic、不做無謂重試。
    let token = match std::env::var(&args.token_env) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            eprintln!(
                "錯誤：SViewWifi 權杖缺失（環境變數 {} 未設或為空）。\n\
                 權杖由使用者從自己的配對資料／抓包取得，例如：\n  \
                 SVIEW_TOKEN=<32 位數字> probe --host {} --duration 600",
                args.token_env, args.host
            );
            return std::process::ExitCode::from(2);
        }
    };

    let uri = format!("rtsp://{}:{}{}", args.host, args.port, args.path);
    let clock = RealClock::new();

    let mut cfg = SessionConfig::new(uri.clone(), token);
    cfg.duration = Some(Duration::from_secs(args.duration));
    cfg.recv_timeout = Duration::from_millis(args.recv_timeout_ms);
    cfg.heartbeat = if args.heartbeat_ms == 0 {
        None
    } else {
        Some(Duration::from_millis(args.heartbeat_ms))
    };
    cfg.reconnect = ReconnectPolicy { max_attempts: args.max_reconnects, ..Default::default() };

    let mut factory = TcpLinkFactory::new(args.host.clone());
    factory.rtsp_port = args.port;
    factory.heartbeat = if args.heartbeat_ms == 0 {
        None
    } else {
        Some(HeartbeatConfig::default())
    };
    if let Some(ip) = &args.bind_ip {
        match ip.parse() {
            Ok(v) => factory.bind_ip = Some(v),
            Err(e) => {
                eprintln!("錯誤：--bind-ip 無效：{e}");
                return std::process::ExitCode::from(2);
            }
        }
    }

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
    let t0 = std::time::Instant::now();
    let stamp = move || t0.elapsed().as_millis();

    let mut log = |ev: Event| match ev {
        Event::Connecting { attempt } => println!("[{}] 連線第 {attempt} 次", stamp()),
        Event::Connected { rtp_port } => println!("[{}] TCP 已連；RTP 埠 {rtp_port}", stamp()),
        Event::Playing { session, sprop_sets } => {
            println!("[{}] PLAY 成功（Session {session}，sprop {sprop_sets} 組）", stamp())
        }
        Event::Rst { at_ms, reason } => {
            println!("[{}] 斷流 t+{at_ms}ms：{reason}", stamp())
        }
        Event::Reconnected { after_ms, gap_ms } => {
            println!("[{}] 已重連完成 t+{after_ms}ms（空窗 {gap_ms}ms）", stamp())
        }
        Event::ConnectFailed { attempt, error } => {
            println!("[{}] 第 {attempt} 次連線失敗：{error}", stamp())
        }
        Event::Finished { reason } => println!("[{}] 結束：{reason}", stamp()),
    };

    let report = run(&factory, &cfg, &clock, sink, &mut log).await;

    let json = serde_json::to_string_pretty(&report).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    if let Some(path) = &args.out {
        if let Err(e) = std::fs::write(path, &json) {
            eprintln!("錯誤：寫報告失敗 {}：{e}", path.display());
        }
    }
    println!("{json}");

    // 沒有任何 RTP = 開流失敗，讓 CI／腳本能以 exit code 判斷。
    if report.rtp_packets == 0 {
        return std::process::ExitCode::from(1);
    }
    std::process::ExitCode::SUCCESS
}

