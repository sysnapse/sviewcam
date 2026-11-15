//! N1（PM 2026-11-15 R7 裁定，票 SVW-18）：空窗（gap）的收口點必須是
//! 「RST → 本會話首個 RTP」，不是「RST → PLAY 200」。
//!
//! 兩件事要釘住：
//! 1. 握手全綠、但 recv_timeout 前一個 RTP 都沒有 → 不得記空窗、不得發 Event::Reconnected
//!    （否則「相機 PLAY 成功卻不送流」會被誤記成已續流，正是 R1 要擋的模式）；
//! 2. 真的續流時，gaps[i].ms 要等於「退避 + 到首個 RTP 的時間」，
//!    而不是「退避」（= 回到 PLAY 收口）或 0（= 在 RST 當下收口）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rtsp_core::mock::{ConnPlan, EndMode, MockFactory};
use rtsp_core::rtp;
use rtsp_core::rtsp::Response;
use rtsp_core::session::{run, SessionConfig};
use rtsp_core::sink::VecSink;
use rtsp_core::{Error, Event, Link, LinkFactory, Result, VirtualClock};

const SSRC: u32 = 0x1122_3344;

fn single(seq: u16, ts: u32, nal: &[u8]) -> Vec<u8> {
    rtp::build(seq, ts, SSRC, false, 96, nal)
}

fn config() -> SessionConfig {
    let mut cfg = SessionConfig::new(
        "rtsp://192.168.1.1:554/test.264",
        "01234567890123456789012345678901",
    );
    cfg.duration = Some(Duration::from_secs(7200));
    cfg.heartbeat = None;
    // recv_timeout 走真實 tokio 計時器（VirtualClock 只管 now_ms），故用毫秒級；
    // 下面的 hang 連線會永遠 pending，等這個 timeout 觸發。
    cfg.recv_timeout = Duration::from_millis(20);
    cfg
}

/// 每次 recv_rtp 前推進虛擬時鐘 `per_packet_ms`，令「距上次 PLAY 多久」與
/// 「首個 RTP 幾時到」都可精確控制（同 rst_classes.rs 的 PacedFactory，但只留需要的部分）。
struct PacedFactory {
    inner: MockFactory,
    clock: Arc<VirtualClock>,
    /// 每條連線各自的收包時間間隔（ms）；最後一個值重複用於之後的連線。
    strides: Vec<u64>,
    conn_index: AtomicUsize,
    /// 每次 connect 之後、交給 session 之前先推進的時間（模擬退避以外的額外延遲）。
    connect_extra_ms: Vec<u64>,
}

impl PacedFactory {
    fn new(
        plans: Vec<ConnPlan>,
        clock: Arc<VirtualClock>,
        strides: Vec<u64>,
        connect_extra_ms: Vec<u64>,
    ) -> Self {
        Self {
            inner: MockFactory::new(plans),
            clock,
            strides,
            conn_index: AtomicUsize::new(0),
            connect_extra_ms,
        }
    }

    fn stride_for(&self, i: usize) -> u64 {
        self.strides.get(i).copied().or_else(|| self.strides.last().copied()).unwrap_or(0)
    }
}

struct PacedLink {
    inner: Box<dyn Link>,
    clock: Arc<VirtualClock>,
    per_packet_ms: u64,
}

#[async_trait]
impl Link for PacedLink {
    fn client_rtp_port(&self) -> u16 {
        self.inner.client_rtp_port()
    }

    async fn send(&mut self, request: &str) -> Result<()> {
        self.inner.send(request).await
    }

    async fn recv_response(&mut self) -> Result<Response> {
        self.inner.recv_response().await
    }

    async fn recv_rtp(&mut self) -> Result<Vec<u8>> {
        // 先推進時間再收：這正是「首個 RTP 於 PLAY 之後 N ms 才到」的建模。
        self.clock.advance(self.per_packet_ms);
        self.inner.recv_rtp().await
    }

    async fn close(&mut self) {
        self.inner.close().await
    }
}

#[async_trait]
impl LinkFactory for PacedFactory {
    async fn connect(&self) -> Result<(Box<dyn Link>, u16)> {
        let (inner, port) = self.inner.connect().await?;
        let i = self.conn_index.fetch_add(1, Ordering::SeqCst);
        if let Some(ms) = self.connect_extra_ms.get(i).copied() {
            self.clock.advance(ms);
        }
        Ok((
            Box::new(PacedLink { inner, clock: self.clock.clone(), per_packet_ms: self.stride_for(i) }),
            port,
        ))
    }
}

/// 握手全綠但永不送 RTP（recv_timeout 前零 RTP）：不得記空窗、不得發 Reconnected。
/// 舊行為（收口在 PLAY 200）會在第二段起每段都 push 一次空窗並發 Reconnected。
#[tokio::test]
async fn play_ok_without_any_rtp_records_no_gap_and_no_reconnected() {
    let plans: Vec<ConnPlan> = (0..5)
        .map(|i| ConnPlan::new(format!("sess-{i}"), vec![]).hang())
        .collect();
    let clock = Arc::new(VirtualClock::new());
    let factory = MockFactory::new(plans);

    let mut sink = VecSink::new();
    let mut log: Vec<String> = Vec::new();
    let report = run(&factory, &config(), &clock, &mut sink, &mut |e| {
        log.push(match e {
            Event::Reconnected { gap_ms, .. } => format!("reconnected:{gap_ms}"),
            Event::Rst { reason, .. } => format!("rst:{reason}"),
            Event::Playing { .. } => "playing".into(),
            Event::Finished { reason } => format!("finished:{reason}"),
            Event::ConnectFailed { .. } => "connect_failed".into(),
            Event::Connecting { .. } => "connecting".into(),
            Event::Connected { .. } => "connected".into(),
        })
    })
    .await;

    assert!(report.connects >= 2, "應至少重連一次：{:?}", report.finished);
    assert_eq!(report.rtp_packets, 0, "這個情境一個 RTP 都沒有");
    assert!(report.gaps.is_empty(), "零 RTP 的會話不得留下空窗記錄：{:?}", report.gaps);
    assert_eq!(report.max_gap_ms, 0, "沒有續流就不該有最大空窗");
    assert!(
        !log.iter().any(|l| l.starts_with("reconnected")),
        "不得發 Event::Reconnected：{log:?}"
    );
    // 每段都是 PLAY 後 10 s 內、零 RTP 即斷 → 全部算即時拒絕，即「不算成功續流」。
    assert_eq!(
        report.rst_immediate_reject, report.connects,
        "零 RTP 的中斷都應歸入即時拒絕：{:?}",
        report.finished
    );
}

/// 空窗長度＝RST→本會話首個 RTP（＝退避 + 首包延遲），不是 RST→PLAY。
/// conn1 收 2 包（每包 +5000 ms → age 10 s，屬中途 RST，退避 1000 ms），
/// conn2 首個 RTP 於 PLAY 後再 +200 ms 到 → 空窗應為 1200 ms。
/// 回到 PLAY 收口會得 1000，當下收口會得 0。
#[tokio::test]
async fn gap_is_measured_from_rst_to_first_rtp() {
    let conn1 = vec![
        single(1, 1000, &[0x41, 0x01]),
        single(2, 2000, &[0x41, 0x02]),
    ];
    let conn2 = vec![
        single(1, 3000, &[0x65, 0x03]),
        single(2, 4000, &[0x65, 0x04]),
    ];

    let clock = Arc::new(VirtualClock::new());
    let factory = PacedFactory::new(
        vec![
            ConnPlan::new("sess-1", conn1).end(EndMode::Rst),
            ConnPlan::new("sess-2", conn2).end(EndMode::End),
        ],
        clock.clone(),
        // conn1 每包 5000 ms（令 age=10 s 落在中途 RST）；conn2 首包延遲 200 ms。
        vec![5000, 200],
        vec![0, 0],
    );

    let mut cfg = config();
    cfg.recv_timeout = Duration::from_secs(3600);

    let mut sink = VecSink::new();
    let mut reconnected: Vec<u64> = Vec::new();
    let report = run(&factory, &cfg, &clock, &mut sink, &mut |e| {
        if let Event::Reconnected { gap_ms, .. } = e {
            reconnected.push(gap_ms);
        }
    })
    .await;

    assert_eq!(report.connects, 2, "應重連一次：{:?}", report.finished);
    assert_eq!(report.rst_midstream, 1, "age=10 s 應判中途 RST：{:?}", report.finished);
    assert_eq!(report.gaps.len(), 1, "只有一次斷流");
    assert_eq!(
        report.gaps[0].ms, 1200,
        "空窗應為 RST→首個 RTP（退避 1000 + 首包 200），實測 {:?}",
        report.gaps[0]
    );
    assert_eq!(report.max_gap_ms, 1200);
    assert_eq!(reconnected, vec![1200], "Reconnected 應帶同一個空窗值");
}
