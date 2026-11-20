//! `replay`：把錄製封包（`.sviewcap.jsonl`）餵回同一條重連狀態機，無相機也能跑全鏈路。
//!
//! exit code：0 正常；2 寫檔／錄影失敗；3 fixture 內 `expected` 與實跑不符；
//! 4 fixture 解析失敗。
//!
//! M1 新增（TICKET-20261115-02）：
//! - `--record-dir <dir>`：把 NAL 串流寫成 2 秒 MKV 段 + SQLite 索引（`camcore`）。
//! - `--retain-minutes <n>`：保留窗（預設 5 分鐘、上限 15 分鐘，夾住時印**一則**警告）。
//! - `--recover-only`：只跑啟動復原掃描並印結果（不重放）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use camcore::{RingConfig, RingWriter};
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

    /// 錄影資料目錄（MKV 段 + index.sqlite）
    #[arg(long)]
    record_dir: Option<PathBuf>,

    /// 保留窗分鐘數（預設 5，上限 15）
    #[arg(long)]
    retain_minutes: Option<u64>,

    /// 只跑啟動復原掃描（需 --record-dir）
    #[arg(long)]
    recover_only: bool,

    /// 錄影視訊寬（寫進 MKV Video 元素）
    #[arg(long, default_value_t = 2560)]
    width: u32,

    /// 錄影視訊高
    #[arg(long, default_value_t = 1440)]
    height: u32,

    /// 與 fixture 的 expected 比對（預設開啟；--check=false 可關）
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    check: bool,

    /// 重放用的虛擬 RTP 埠
    #[arg(long, default_value_t = 20000)]
    client_port: u16,
}

/// 同時把 NAL 交給錄影 sink 與 Annex-B 檔。
struct Tee<'a> {
    recording: &'a mut dyn NalSink,
    annexb: Option<&'a mut FileSink>,
}

impl NalSink for Tee<'_> {
    fn nal(&mut self, nal: &[u8]) {
        self.recording.nal(nal);
        if let Some(f) = self.annexb.as_deref_mut() {
            f.nal(nal);
        }
    }

    fn nal_at(&mut self, nal: &[u8], at_ms: u64) {
        self.recording.nal_at(nal, at_ms);
        if let Some(f) = self.annexb.as_deref_mut() {
            f.nal_at(nal, at_ms);
        }
    }

    fn gap(&mut self, at_ms: u64, gap_ms: u64, reason: &str) {
        self.recording.gap(at_ms, gap_ms, reason);
        if let Some(f) = self.annexb.as_deref_mut() {
            f.gap(at_ms, gap_ms, reason);
        }
    }
}

fn main() -> std::process::ExitCode {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(real_main())
}

async fn real_main() -> std::process::ExitCode {
    let args = Args::parse();

    // ---- 只跑復原掃描 ----
    if args.recover_only {
        let Some(dir) = &args.record_dir else {
            eprintln!("錯誤：--recover-only 需要 --record-dir");
            return std::process::ExitCode::from(2);
        };
        match camcore::recover::recover(dir, &mut open_index(dir)) {
            Ok(report) => {
                println!("{}", serde_json::to_string_pretty(&recovery_json(&report)).unwrap());
                return std::process::ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("錯誤：復原掃描失敗 {}：{e}", dir.display());
                return std::process::ExitCode::from(2);
            }
        }
    }

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

    // ---- 錄影 sink ----
    let mut ring: Option<RingWriter> = None;
    if let Some(dir) = &args.record_dir {
        let (mut ring_cfg, cfg_warnings) =
            RingConfig::new(dir).with_retention_minutes(args.retain_minutes);
        ring_cfg.width = args.width;
        ring_cfg.height = args.height;
        for w in &cfg_warnings {
            // 走 stderr 的警告通道，不混進資料流；夾住只印一次
            eprintln!("警告：{}（{}）", w.message, w.code);
        }
        match RingWriter::open(ring_cfg) {
            Ok((writer, recovered)) => {
                if !recovered.repaired.is_empty()
                    || !recovered.discarded.is_empty()
                    || !recovered.pruned.is_empty()
                    || recovered.index_rebuilt
                {
                    eprintln!(
                        "復原掃描：修復 {}、丟棄 {}、清孤兒 {}、重建索引 {}",
                        recovered.repaired.len(),
                        recovered.discarded.len(),
                        recovered.pruned.len(),
                        recovered.index_rebuilt
                    );
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&recovery_json(&recovered)).unwrap()
                    );
                }
                ring = Some(writer);
            }
            Err(e) => {
                eprintln!("錯誤：無法開啟錄影目錄 {}：{e}", dir.display());
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
    let sink: &mut dyn NalSink = match (ring.as_mut(), file_sink.as_mut()) {
        (Some(r), Some(f)) => &mut Tee {
            recording: r,
            annexb: Some(f),
        },
        (Some(r), None) => r,
        (None, Some(f)) => f,
        (None, None) => &mut null_sink,
    };

    let mut log = |ev: Event| match ev {
        Event::Rst { at_ms, reason, class } => {
            println!("斷流 t+{at_ms}ms（{}）：{reason}", class.as_str())
        }
        Event::Reconnected { after_ms, gap_ms } => {
            println!("重連完成 t+{after_ms}ms（空窗 {gap_ms}ms）")
        }
        Event::ConnectFailed { attempt, error } => println!("連線失敗 #{attempt}：{error}"),
        Event::Finished { reason } => println!("結束：{reason}"),
        _ => {}
    };

    let report = run(&factory, &cfg, &clock, sink, &mut log).await;

    // ---- 收工：關閉最後一段 ----
    if let Some(writer) = ring.as_mut() {
        if let Err(e) = writer.finish() {
            eprintln!("錯誤：收段失敗：{e}");
            return std::process::ExitCode::from(2);
        }
        if let Some(e) = writer.take_error() {
            eprintln!("錯誤：錄影失敗：{e}");
            return std::process::ExitCode::from(2);
        }
        match recording_json(writer) {
            Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
            Err(e) => {
                eprintln!("錯誤：讀索引失敗：{e}");
                return std::process::ExitCode::from(2);
            }
        }
    }

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

fn open_index(dir: &std::path::Path) -> camcore::SegmentIndex {
    let cfg = RingConfig::new(dir);
    camcore::SegmentIndex::open(cfg.index_path()).expect("open index")
}

fn recovery_json(report: &camcore::RecoveryReport) -> serde_json::Value {
    serde_json::json!({
        "repaired": report.repaired,
        "discarded": report.discarded,
        "pruned": report.pruned,
        "index_rebuilt": report.index_rebuilt,
        "rows": report.rows,
    })
}

fn recording_json(writer: &RingWriter) -> camcore::Result<serde_json::Value> {
    let index = writer.index();
    let rows = index.rows()?;
    let events = index.events()?;
    let t_start = rows.first().map(|r| r.t_start).unwrap_or(0);
    let t_end = rows.last().map(|r| r.t_end).unwrap_or(0);
    Ok(serde_json::json!({
        "recording": {
            "segments": rows.len(),
            "bytes": index.total_bytes()?,
            "t_start_ms": t_start,
            "t_end_ms": t_end,
            "span_ms": t_end.saturating_sub(t_start),
            "gap_segments": rows.iter().filter(|r| r.gap_ms > 0).count(),
            "event_count": events.len(),
            "events": events.iter().map(|e| serde_json::json!({
                "at_ms": e.at_ms, "kind": e.kind, "detail": e.detail,
            })).collect::<Vec<_>>(),
            "segments_detail": rows.iter().map(|r| serde_json::json!({
                "segment_id": r.segment_id, "t_start": r.t_start, "t_end": r.t_end,
                "bytes": r.bytes, "idr_offset": r.idr_offset, "gap_ms": r.gap_ms,
            })).collect::<Vec<_>>(),
        }
    }))
}
