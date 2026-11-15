//! 驗收標準 2 的 RST 觸發分類部分（protocol 文檔 §11.2，2026-11-15 14:3x 裁定為三類）：
//! 定時回收、即時拒絕、中途 RST 必須分別判定並分別退避。
//! 另測重連風暴上限（§11.2 硬規則 3）、§12 未證事項不寫死 60 秒的約束，
//! 以及 RTP 序號兩個方向（向前缺口／向後亂序）的差別處理——後者是變異測試 mutantB 逼出來的。
//!
//! 這些測試靠 [`PacedFactory`] 令虛擬時鐘隨收包推進，因此「距上次 PLAY 多久」可控；
//! 沒有它，所有斷流都會落在 age≈0，分類測試就變成假綠。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rtsp_core::mock::{ConnPlan, EndMode, MockFactory};
use rtsp_core::rtp;
use rtsp_core::rtsp::Response;
use rtsp_core::session::{run, SessionConfig};
use rtsp_core::sink::VecSink;
use rtsp_core::{Error, Link, LinkFactory, Result, VirtualClock};

const SSRC: u32 = 0x1122_3344;

fn single(seq: u16, ts: u32, nal: &[u8]) -> Vec<u8> {
    rtp::build(seq, ts, SSRC, false, 96, nal)
}

fn config() -> SessionConfig {
    let mut cfg = SessionConfig::new("rtsp://192.168.1.1:554/test.264", "01234567890123456789012345678901");
    cfg.duration = Some(Duration::from_secs(7200));
    cfg.heartbeat = None;
    cfg.recv_timeout = Duration::from_secs(3600);
    cfg
}

/// 每收一個 RTP 包就推進虛擬時鐘，令「距上次 PLAY 多久」可精確控制。
struct PacedFactory {
    inner: MockFactory,
    clock: Arc<VirtualClock>,
    per_packet_ms: u64,
    /// 每次連線在收第一個 RTP 之前先推進的時間（ms）。用於模擬「已連線、PLAY 成功，
    /// 但在收到任何 RTP 之前就斷流」。PacedFactory 原本只靠收包推進時鐘，這種連線的
    /// age 會是 0，一律被判成即時拒絕，因而測不到中途 RST 的加倍退避。
    idle_ms: Vec<u64>,
    conn_index: AtomicUsize,
}

impl PacedFactory {
    fn new(plans: Vec<ConnPlan>, clock: Arc<VirtualClock>, per_packet_ms: u64) -> Self {
        let n = plans.len();
        Self {
            inner: MockFactory::new(plans),
            clock,
            per_packet_ms,
            idle_ms: vec![0; n],
            conn_index: AtomicUsize::new(0),
        }
    }

    fn paced(
        plans: Vec<ConnPlan>,
        clock: Arc<VirtualClock>,
        per_packet_ms: u64,
        idle_ms: Vec<u64>,
    ) -> Self {
        Self { inner: MockFactory::new(plans), clock, per_packet_ms, idle_ms, conn_index: AtomicUsize::new(0) }
    }
}

struct PacedLink {
    inner: Box<dyn Link>,
    clock: Arc<VirtualClock>,
    per_packet_ms: u64,
    idle_ms: u64,
    idle_spent: bool,
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
        // 先推進時間再收：RST 那一刻的 age 因而包含「這段串流已持續多久」。
        if self.idle_ms > 0 && !self.idle_spent {
            self.idle_spent = true;
            self.clock.advance(self.idle_ms);
        } else {
            self.clock.advance(self.per_packet_ms);
        }
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
        let idle_ms = self.idle_ms.get(i).copied().unwrap_or(0);
        Ok((
            Box::new(PacedLink {
                inner,
                clock: self.clock.clone(),
                per_packet_ms: self.per_packet_ms,
                idle_ms,
                idle_spent: false,
            }),
            port,
        ))
    }
}

async fn drive(
    factory: &dyn LinkFactory,
    clock: &Arc<VirtualClock>,
    cfg: &SessionConfig,
) -> rtsp_core::Report {
    let mut sink = VecSink::new();
    let mut noop = |_e: rtsp_core::Event| {};
    run(factory, cfg, clock, &mut sink, &mut noop).await
}

/// 定時回收（§11.2 第一列）：距 PLAY ≥45 s 被 RST → 立即重連、退避歸零。
/// 這是設計內行為，不是故障；若把它當「即時拒絕」退避，就會白白多等。
#[tokio::test]
async fn timed_recycle_reconnects_immediately_with_zero_backoff() {
    // conn1：12 個包 × 5 s = 60 s 後被 RST（與文檔觀測的 59–64 s 同級）。
    let conn1: Vec<Vec<u8>> = (0..12).map(|i| single(i as u16 + 1, 1000 * (i as u32 + 1), &[0x41, 0x01])).collect();
    let conn2 = vec![single(1, 1, &[0x65, 0x02])];

    let clock = Arc::new(VirtualClock::new());
    let factory = PacedFactory::new(
        vec![
            ConnPlan::new("sess-1", conn1).end(EndMode::Rst),
            ConnPlan::new("sess-2", conn2).end(EndMode::End),
        ],
        clock.clone(),
        5000,
    );

    let report = drive(&factory, &clock, &config()).await;

    assert_eq!(report.connects, 2, "應重連一次：{}", report.finished);
    assert_eq!(report.rst_timed_recycle, 1, "應判為定時回收");
    assert_eq!(report.rst_immediate_reject, 0);
    assert_eq!(report.rst_midstream, 0);
    assert_eq!(report.backoff_ms, vec![0], "定時回收不得白等退避");
    assert_eq!(report.gaps.len(), 1);
    // N1：空窗收口在「本會話首個 RTP」，故值＝退避 0 ＋ 本 fixture 的首包間隔（PacedFactory 5000 ms）；
    // 退避是否歸零仍由上面的 backoff_ms 斷言。
    assert_eq!(report.gaps[0].ms, 5000, "定時回收不睡退避，空窗只含 RST→首個 RTP 的 5000 ms");
}

/// 即時拒絕（§11.2 第二列）：PLAY 後 <10 s 被 RST 且從未收到 RTP →
/// 退避 0.5→1→2→4 s；連續 5 次即停止並報錯，不得無限重試。
#[tokio::test]
async fn immediate_reject_uses_ladder_then_stops_after_five() {
    let plans: Vec<ConnPlan> = (0..8)
        .map(|i| ConnPlan::new(format!("sess-{i}"), vec![]).end(EndMode::Rst))
        .collect();

    let clock = Arc::new(VirtualClock::new());
    let factory = PacedFactory::new(plans, clock.clone(), 1000);

    let report = drive(&factory, &clock, &config()).await;

    assert_eq!(report.rst_immediate_reject, 5, "應判為即時拒絕 5 次：{}", report.finished);
    assert_eq!(report.rst_timed_recycle, 0);
    assert_eq!(report.rst_midstream, 0);
    assert_eq!(
        report.backoff_ms,
        vec![500, 1000, 2000, 4000],
        "退避階梯應為 0.5→1→2→4 s，第 5 次即停"
    );
    assert!(
        report.finished.contains("immediate reject"),
        "第 5 次連續即時拒絕應停下報錯，實得：{}",
        report.finished
    );
    assert!(!report.errors.is_empty(), "停機原因要入 errors");
    assert_eq!(report.connects, 5, "只應連到第 5 次就停");
}

/// 中途 RST（§11.2 第三列）其中一條規則：已收 RTP 但在 45 s 內被斷 → 1 s 起，
/// 「重連成功後重置計數」。這裡把成功定義為「新會話收到第一個 RTP」，不是 PLAY 成功
/// （即時拒絕的會話照樣 PLAY 成功），因此兩次都收到 RTP 的重連之後，退避應回到 1 s 而不是 2 s。
/// 未解釋的觸發（例如 ~31 s 短拉）亦落入此類，不假設它已被解釋。
#[tokio::test]
async fn midstream_rst_resets_backoff_after_rtp_success() {
    let s1 = vec![single(1, 1000, &[0x41, 0x01]), single(2, 2000, &[0x41, 0x02])]; // ~2 s 後 RST
    let s2 = vec![single(1, 1000, &[0x41, 0x03]), single(2, 2000, &[0x41, 0x04])]; // 再 ~2 s 後 RST
    let s3 = vec![single(1, 1000, &[0x65, 0x05])];

    let clock = Arc::new(VirtualClock::new());
    let factory = PacedFactory::new(
        vec![
            ConnPlan::new("sess-1", s1).end(EndMode::Rst),
            ConnPlan::new("sess-2", s2).end(EndMode::Rst),
            ConnPlan::new("sess-3", s3).end(EndMode::End),
        ],
        clock.clone(),
        1000,
    );

    let report = drive(&factory, &clock, &config()).await;

    assert_eq!(report.rst_midstream, 2, "兩次都應判為中途 RST：{}", report.finished);
    assert_eq!(report.rst_timed_recycle, 0);
    assert_eq!(report.rst_immediate_reject, 0);
    assert_eq!(
        report.backoff_ms,
        vec![1000, 1000],
        "§11.2：中途 RST 退避在重連成功（收到 RTP）後重置，第二次的空窗不得變 2 s"
    );
    assert_eq!(report.gaps.len(), 2);
    // N1：空窗＝退避 ＋ RST→首個 RTP 的首包間隔（PacedFactory 1000 ms），故 1000+1000。
    // 「空窗不得隨退避加倍」仍由兩者相等證明。
    assert_eq!(report.gaps[0].ms, 2000, "空窗＝退避 1000 ＋ 首包間隔 1000");
    assert_eq!(report.gaps[1].ms, 2000);
}

/// 中途 RST（§11.2 第三列）的另一條規則：接連斷流、中間那一次沒有收到任何 RTP 時，退避要加倍。
/// 與上一條合起來才真正限制退避行為（只斷言 [1000, 1000] 會被「根本不退避」的實作通過）。
/// 未收 RTP 但 age 落在 10–45 s 之間属「其他」，不得誤判為即時拒絕。
#[tokio::test]
async fn midstream_rst_doubles_when_reconnect_gets_no_rtp() {
    let s1 = vec![single(1, 1000, &[0x41, 0x01]), single(2, 2000, &[0x41, 0x02])]; // ~2 s 後 RST
    // 第二次重連：PLAY 成功但 20 s 內一顆 RTP 都沒有就再被 RST → 仍属中途，不是即時拒絕。
    let s2: Vec<Vec<u8>> = vec![];
    let s3 = vec![single(1, 1000, &[0x65, 0x03])];

    let clock = Arc::new(VirtualClock::new());
    let factory = PacedFactory::paced(
        vec![
            ConnPlan::new("sess-1", s1).end(EndMode::Rst),
            ConnPlan::new("sess-2", s2).end(EndMode::Rst),
            ConnPlan::new("sess-3", s3).end(EndMode::End),
        ],
        clock.clone(),
        1000,
        vec![0, 20_000, 0],
    );

    let report = drive(&factory, &clock, &config()).await;

    assert_eq!(report.rst_midstream, 2, "雨次都要判中途 RST：{}", report.finished);
    assert_eq!(report.rst_immediate_reject, 0, "無 RTP 但 age=20 s 不是即時拒絕");
    assert_eq!(report.rst_timed_recycle, 0);
    assert_eq!(report.backoff_ms, vec![1000, 2000], "第一次 1 s 起，未收 RTP 則第二次加倍");
    // N1：第二段重連「PLAY 成功但零 RTP」不算續流，故不另記一次空窗——
    // 這次斷流與下一次成功收流合成一段連續空窗（一次 RST→首個 RTP）。
    assert_eq!(report.gaps.len(), 1, "零 RTP 的重連不另記空窗");
    assert_eq!(report.gaps[0].ms, 3000, "最終退避 2000 ＋ 首包間隔 1000");
}

/// 同一會話內序號向後跳（RTP 走 UDP，會亂序到達）不得算成丟包。
/// 這條是變異測試逼出來的：mutantB（把任何非連號都當缺口）原本能逃逸，因為舊測試
/// 只覆蓋「重連後序號重置」（那在會話邊界，`last_seq` 本來就重置）與「向前缺口」。
/// §12 未證清單並未排除亂序，所以容錯行為要有斷言，不能假設真機不會亂序。
#[tokio::test]
async fn backward_seq_jump_within_session_is_not_counted_as_loss() {
    // seq 1,2,3 然後回到 2（亂序補包）再到 4：只有 3→4 是連號，向後那一步要靜默接受。
    let pkts = vec![
        single(1, 1000, &[0x65, 0x01]),
        single(2, 1100, &[0x41, 0x02]),
        single(3, 1200, &[0x41, 0x03]),
        single(2, 1100, &[0x41, 0x02]), // 亂序：比上一包還小
        single(4, 1300, &[0x41, 0x04]),
    ];

    let clock = Arc::new(VirtualClock::new());
    let factory = PacedFactory::new(
        vec![ConnPlan::new("sess-1", pkts).end(EndMode::End)],
        clock.clone(),
        0,
    );

    let report = drive(&factory, &clock, &config()).await;

    assert!(report.seq_gaps.is_empty(), "向後跳不得記成缺口：{:?}", report.seq_gaps);
    assert_eq!(report.lost_packets, 0, "向後跳不得記成丟包");
    assert_eq!(report.out_of_order, 1, "那包亂序要單獨計數，不是靜默吞掉");
    assert_eq!(report.rtp_packets, 5, "五包都要收到");
    assert_eq!(report.rst_midstream, 0);
}

/// 向前缺口（真的少收包）仍要如實記錄，且與向後跳的處理不同。
/// 與上一條合起來才把「缺口」這詞的兩個方向都鎖住：只斷言向前缺口會被 mutantB 逃逸。
#[tokio::test]
async fn forward_gap_within_session_is_counted_once() {
    let pkts = vec![
        single(1, 1000, &[0x65, 0x01]),
        single(2, 1100, &[0x41, 0x02]),
        single(6, 1200, &[0x41, 0x03]), // 向前跳 4：丟 3 包
    ];

    let clock = Arc::new(VirtualClock::new());
    let factory = PacedFactory::new(
        vec![ConnPlan::new("sess-1", pkts).end(EndMode::End)],
        clock.clone(),
        0,
    );

    let report = drive(&factory, &clock, &config()).await;

    assert_eq!(report.seq_gaps.len(), 1, "應恰好一個缺口：{:?}", report.seq_gaps);
    assert_eq!(report.seq_gaps[0].lost, 3);
    assert_eq!(report.lost_packets, 3);
}

/// 重連風暴上限（§11.2 硬規則 3）：10 分鐘內超過 40 次重連即停止並上報。
#[tokio::test]
async fn reconnect_storm_cap_stops_and_reports() {
    let plans: Vec<ConnPlan> = (0..60)
        .map(|i| {
            ConnPlan::new(
                format!("sess-{i}"),
                vec![single(1, 1000, &[0x41, 0x01]), single(2, 2000, &[0x41, 0x02])],
            )
            .end(EndMode::Rst)
        })
        .collect();

    let clock = Arc::new(VirtualClock::new());
    let factory = PacedFactory::new(plans, clock.clone(), 1000);

    let report = drive(&factory, &clock, &config()).await;

    assert_eq!(report.rst_midstream, 41, "第 41 次重連觸頂：{}", report.finished);
    assert!(
        report.finished.contains("reconnect storm"),
        "應以重連風暴收尾，實得：{}",
        report.finished
    );
    let total: u64 = report.backoff_ms.iter().sum();
    assert!(total <= 600_000, "觸頂應在 10 分鐘窗內，實得 {total} ms");
}

/// 分類優先用 age，不用「有沒有 RTP」單一條件：有收流但 ≥45 s 仍是定時回收。
#[tokio::test]
async fn classification_prefers_age_over_rtp_presence() {
    let conn1: Vec<Vec<u8>> = (0..10).map(|i| single(i as u16 + 1, 1000, &[0x41, 0x01])).collect(); // 10 s
    let clock = Arc::new(VirtualClock::new());
    let factory = PacedFactory::new(
        vec![
            ConnPlan::new("sess-1", conn1).end(EndMode::Rst),
            ConnPlan::new("sess-2", vec![single(1, 1, &[0x65, 0x01])]).end(EndMode::End),
        ],
        clock.clone(),
        10_000, // 每包 10 s → 10 包 = 100 s
    );

    let report = drive(&factory, &clock, &config()).await;
    assert_eq!(report.rst_timed_recycle, 1);
    assert_eq!(report.rst_midstream, 0);
    assert_eq!(report.backoff_ms, vec![0]);
}

/// 錯誤型別測試：確認分類 enum 的序列化字串穩定（報告與看板要用）。
#[test]
fn rst_class_names_are_stable() {
    assert_eq!(rtsp_core::RstClass::TimedRecycle.as_str(), "timed_recycle");
    assert_eq!(rtsp_core::RstClass::ImmediateReject.as_str(), "immediate_reject");
    assert_eq!(rtsp_core::RstClass::MidStream.as_str(), "midstream");
    let _ = Error::Closed("x".into());
}
