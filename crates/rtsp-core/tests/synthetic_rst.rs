//! 驗收標準 1【fixture】：Given 內建合成 RTP 源（含一次模擬 RST），
//! When 跑重連狀態機，Then 重連 ≥1、Annex-B NAL 計數與預期一致、協議請求全部合規。

use std::time::Duration;

use rtsp_core::mock::{fu_a_fragments, ConnPlan, EndMode, MockFactory};
use rtsp_core::rtp;
use rtsp_core::session::{run, SessionConfig};
use rtsp_core::sink::{NalSink, VecSink};
use rtsp_core::VirtualClock;

const SSRC: u32 = 0x1122_3344;

fn single(seq: u16, ts: u32, nal: &[u8]) -> Vec<u8> {
    rtp::build(seq, ts, SSRC, false, 96, nal)
}

fn fu_a(seq_start: u16, ts: u32, nal: &[u8], frag: usize) -> Vec<Vec<u8>> {
    fu_a_fragments(nal, frag)
        .into_iter()
        .enumerate()
        .map(|(i, payload)| {
            rtp::build(seq_start + i as u16, ts, SSRC, false, 96, &payload)
        })
        .collect()
}

fn config() -> SessionConfig {
    let mut cfg = SessionConfig::new("rtsp://192.168.1.1:554/test.264", "01234567890123456789012345678901");
    cfg.duration = Some(Duration::from_secs(60));
    cfg.heartbeat = None;
    cfg.recv_timeout = Duration::from_secs(3600);
    cfg
}

#[tokio::test]
async fn reconnects_after_rst_and_counts_nals() {
    // 第一條連線：single NAL → FU-A 三片（同一幀）→ single NAL，之後模擬相機 RST
    let mut conn1 = vec![single(1, 1000, &[0x65, 0x01, 0x02])];
    conn1.extend(fu_a(2, 2000, &[0x65, 0xaa, 0xbb, 0xcc, 0xdd, 0xee], 2));
    conn1.push(single(5, 3000, &[0x41, 0x03]));

    // 第二條連線：序號重置（容忍），並在中途刻意斷一個序號缺口（丟 4 包）
    let conn2 = vec![
        single(100, 4000, &[0x65, 0x04]),
        single(101, 5000, &[0x41, 0x05]),
        single(106, 6000, &[0x41, 0x06]),
    ];

    let factory = MockFactory::new(vec![
        ConnPlan::new("sess-1", conn1).end(EndMode::Rst),
        ConnPlan::new("sess-2", conn2).end(EndMode::End),
    ]);

    let clock = VirtualClock::new();
    let mut sink = VecSink::new();
    let mut events = Vec::new();
    let report = run(&factory, &config(), &clock, &mut sink, &mut |e| events.push(format!("{e:?}"))).await;

    assert!(factory.violations().is_empty(), "協議違規：{:?}", factory.violations());

    // 驗收點：重連 ≥1
    assert_eq!(report.disconnects, 1, "應觀察到一次斷流");
    assert_eq!(report.connects, 2, "應完成兩次 PLAY");
    assert_eq!(report.reconnects, 1, "應成功重連一次");

    // Annex-B NAL 計數：conn1 = single + FU-A 重組 + single = 3；conn2 = 3 → 共 6
    assert_eq!(report.nals, 6, "NAL 計數不符（單包 2、FU-A 1、第二段 3）");
    assert!(report.keyframes >= 3, "keyframe 計數偏低：{}", report.keyframes);

    // 序號缺口：100→101 正常，101→106 缺 4 包
    assert_eq!(report.seq_gaps.len(), 1, "應只有一個序號缺口：{:?}", report.seq_gaps);
    assert_eq!(report.seq_gaps[0].lost, 4);
    assert_eq!(report.lost_packets, 4);

    // 重連空窗有記錄，不得假連續
    assert_eq!(report.gaps.len(), 1, "重連空窗應有一筆記錄：{:?}", report.gaps);
    assert!(report.gaps[0].ms > 0, "空窗時長應 > 0");

    // 請求時序：每條連線都必須 OPTIONS→DESCRIBE→SETUP→PLAY
    let methods = factory.request_methods();
    assert_eq!(
        methods,
        vec![
            "OPTIONS", "DESCRIBE", "SETUP", "PLAY", "OPTIONS", "DESCRIBE", "SETUP", "PLAY"
        ],
        "RTSP 請求時序不符：{methods:?}"
    );

    // Annex-B 不可出現雙起始碼；sprop 2 組 + 6 個 NAL = 8 個起始碼
    assert_eq!(VecSink::count_start_codes(&sink.data), 8);
    assert!(
        !sink.data.windows(8).any(|w| w == [0, 0, 0, 1, 0, 0, 0, 1]),
        "出現重複起始碼：sprop 未剝掉原有 start code"
    );
    assert_eq!(report.sprop_sets, 2);

    // 統計自洽
    assert!(report.rtp_packets >= 6);
    assert_eq!(report.incomplete_frames, 0);
    assert!(report.errors.is_empty(), "不應有錯誤：{:?}", report.errors);
}

#[tokio::test]
async fn rst_while_no_rtp_counts_as_gap_not_silent() {
    // 連上但一個包都沒有，之後 RST：不得當成正常結束。
    let factory = MockFactory::new(vec![
        ConnPlan::new("sess-a", vec![]).end(EndMode::Rst),
        ConnPlan::new("sess-b", vec![single(1, 1, &[0x65, 0x09])]).end(EndMode::End),
    ]);
    let clock = VirtualClock::new();
    let mut sink = VecSink::new();
    let mut noop = |_e: rtsp_core::Event| {};
    let report = run(&factory, &config(), &clock, &mut sink, &mut noop).await;

    assert_eq!(report.disconnects, 1);
    assert_eq!(report.reconnects, 1);
    assert_eq!(report.rtp_packets, 1);
    assert!(report.gaps[0].ms > 0, "零包斷流的空窗亦要記錄");
    assert!(factory.violations().is_empty());
}

#[tokio::test]
async fn gives_up_after_reconnect_budget() {
    let factory = MockFactory::new(vec![ConnPlan::new("s", vec![]).end(EndMode::Rst)]).fail_first(10);
    let clock = VirtualClock::new();
    let mut sink = VecSink::new();
    let mut noop = |_e: rtsp_core::Event| {};
    let mut cfg = config();
    cfg.reconnect.max_attempts = 3;
    let report = run(&factory, &cfg, &clock, &mut sink, &mut noop).await;

    assert_eq!(report.connect_failures, 3, "重試上限 3 次即停");
    assert_eq!(report.connects, 0);
    assert!(report.finished.contains("reconnect budget"));
}

#[tokio::test]
async fn token_is_sent_only_in_describe() {
    let factory = MockFactory::new(vec![ConnPlan::new("s", vec![]).end(EndMode::End)]);
    let clock = VirtualClock::new();
    let mut sink = VecSink::new();
    let mut noop = |_e: rtsp_core::Event| {};
    let _ = run(&factory, &config(), &clock, &mut sink, &mut noop).await;
    let reqs = factory.requests();
    let with_token: Vec<&String> = reqs.iter().filter(|r| r.contains("SViewWifi:")).collect();
    assert_eq!(with_token.len(), 1, "權杖只應出現在 DESCRIBE");
    assert!(with_token[0].starts_with("DESCRIBE"));
}

#[test]
fn sink_helpers_are_sane() {
    let mut sink = VecSink::new();
    sink.nal(&[0, 0, 0, 1, 0x67, 0x11]);
    sink.nal(&[0x68, 0x22]);
    assert_eq!(sink.data, vec![0, 0, 0, 1, 0x67, 0x11, 0, 0, 0, 1, 0x68, 0x22]);
}
