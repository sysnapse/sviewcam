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
    // 正確做法是立即重連（退避歸零）。空窗本身仍非 0——它是 RST→首個 RTP 的 1500 ms
    // （fixture 腳本化的 1 s 握手 + 500 ms 到首包），與退避無關，故另以 backoff_ms 斷言退避。
    assert_eq!(
        report.rst_timed_recycle, 2,
        "兩次都應判為定時回收：timed={} immediate={} mid={}",
        report.rst_timed_recycle, report.rst_immediate_reject, report.rst_midstream
    );
    assert_eq!(report.rst_immediate_reject, 0);
    assert_eq!(report.rst_midstream, 0);
    assert_eq!(report.backoff_ms, vec![0, 0], "定時回收不得白等退避");
    // R4（PM 2026-11-15 第 3 輪裁定）：直接斷言空窗值本身，不可只靠條數或由 backoff 間接推論。
    // N1（PM 2026-11-15 R7 裁定，本票跟進）：空窗收口點已由 PLAY 200 移到本會話首個 RTP，
    // 故 report.gaps[i].ms 量到的是 R1 定義的「RST→本會話首個 RTP」：
    // 61000→62500、122000→123500，即 1500 ms（此前量到的是 RST→PLAY 的 ~1003 ms）。
    // 時間來源是 VirtualClock（見本檔頂端 use rtsp_core::VirtualClock），虛擬毫秒、完全確定，
    // 故期望值可以收緊到 1400..1600：收口點若退回 PLAY（1003）或退回 RST（0）都會紅。
    // 上限 1600 / 下限 1400 同時抓「任何 ≥100 ms 的退避被誤加」與「空窗追蹤恒 0」。
    for (i, g) in report.gaps.iter().enumerate() {
        assert!(
            (1400..1600).contains(&g.ms),
            "第 {i} 次斷流空窗應為 RST→首個 RTP 的 1500 ms（±100），實測 {g:?}"
        );
    }
    assert!(
        (1400..1600).contains(&report.max_gap_ms),
        "最大空窗應為 1500 ms（±100），實測 {}",
        report.max_gap_ms
    );
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
