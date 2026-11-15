//! 驗收標準 2【fixture】：Given 錄製封包 fixture，When 重放，Then 重連次數與序號缺口數
//! 與 fixture 已知值一致。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rtsp_core::replay::{ReplayFactory, ReplayFixture};
use rtsp_core::session::{run, SessionConfig};
use rtsp_core::sink::VecSink;
use rtsp_core::VirtualClock;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sview-m0.jsonl")
}

#[tokio::test]
async fn replays_fixture_and_matches_declared_expectations() {
    let fixture = ReplayFixture::load(&fixture_path()).expect("fixture parses");
    let meta = fixture.meta.clone();
    assert_eq!(fixture.connections.len(), 3, "fixture 應有三段連線");

    let clock = Arc::new(VirtualClock::new());
    let factory = ReplayFactory::new(fixture, clock.clone());

    let mut cfg = SessionConfig::new("rtsp://fixture/replay", "fixture-token");
    cfg.duration = Some(Duration::from_secs(3600));
    cfg.heartbeat = None;
    cfg.recv_timeout = Duration::from_secs(3600);

    let mut sink = VecSink::new();
    let mut rst_at = Vec::new();
    let mut events = 0usize;
    let report = run(&factory, &cfg, &clock, &mut sink, &mut |e| {
        events += 1;
        if let rtsp_core::Event::Rst { at_ms, .. } = e {
            rst_at.push(at_ms);
        }
    })
    .await;

    assert_eq!(report.connects, meta.expected_connections.unwrap(), "連線次數");
    assert_eq!(report.reconnects, meta.expected_reconnects.unwrap(), "重連次數");
    assert_eq!(report.seq_gaps.len(), meta.expected_seq_gaps.unwrap(), "序號缺口數");
    assert_eq!(report.nals, meta.expected_nals.unwrap(), "NAL 計數");

    assert_eq!(rst_at.len(), 2, "應記錄兩次 RST 時刻：{rst_at:?}");
    assert_eq!(report.gaps.len(), 2, "每次斷流都要留空窗記錄");
    // fixture 兩次 RST 都在 PLAY 後約 60 s，屬 §11.2「定時回收」：設計內行為，
    // 正確做法是立即重連（退避歸零），因此空窗為 0 才是對的，不能要求 > 0。
    assert_eq!(
        report.rst_timed_recycle, 2,
        "兩次都應判為定時回收：timed={} immediate={} mid={}",
        report.rst_timed_recycle, report.rst_immediate_reject, report.rst_midstream
    );
    assert_eq!(report.rst_immediate_reject, 0);
    assert_eq!(report.rst_midstream, 0);
    assert_eq!(report.backoff_ms, vec![0, 0], "定時回收不得白等退避");
    assert_eq!(report.lost_packets, 4, "缺口應為 4 個封包");
    assert_eq!(report.seq_gaps[0].lost, 4);
    assert!(events > 0);
    // 解析度只取自 SDP 供日誌，全程可用
    assert_eq!(report.sprop_sets, 2);
    assert!(report.rtp_bytes > 0);
    assert!(report.errors.is_empty(), "重放不應有錯誤：{:?}", report.errors);
}

#[test]
fn fixture_has_no_real_token_or_capture() {
    // 防止有人把真機錄製（可能含權杖）誤 commit 進 fixtures 目錄。
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    for entry in std::fs::read_dir(&dir).expect("fixtures dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("pcap"),
            "{} 疑似真機錄製，請確認不含權杖",
            path.display()
        );
    }
}
