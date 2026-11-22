//! k18／BACKLOG-02：用 RTP timestamp 標定真實 fps 的端到端驗收。
//!
//! 測試跑產品路徑：`replay` CLI → session（真的收 RTP）→ `Report.fps` → `--report` JSON。
//! 合成 fixture 的 timestamp 是整數（90000 ticks/s、25 fps → 每幀 3600 ticks），
//! 所以期望值可以收緊到「恰好 25.000」，不是 ±0.1 的寬鬆範圍。

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_replay")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/m1")
        .join(format!("{name}.jsonl"))
}

fn data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sview-k18-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建立暫存目錄");
    dir
}

fn run_report(name: &str) -> serde_json::Value {
    let dir = data_dir(name);
    let report = dir.join("report.json");
    let out = Command::new(bin())
        .arg("--input")
        .arg(fixture(name))
        .arg("--report")
        .arg(&report)
        .arg("--check=false")
        .output()
        .expect("執行 replay");
    assert!(
        out.status.success(),
        "replay 應成功（status {:?}）\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&report).expect("報告應存在");
    serde_json::from_str(&text).expect("報告是 JSON")
}

#[test]
fn k18_ac1_timestamp_fps_is_exactly_25() {
    let json = run_report("k18_fps");
    let fps = &json["fps"];
    assert!(
        fps.is_object(),
        "報告缺少 fps 物件（基線 7cc3eb8 沒有這個欄位；新程式必須有）：{json:#}"
    );

    // 樣本範圍：250 幀、首 135000、末 1031400、跨度 9960 ms。
    assert_eq!(fps["frames"].as_u64(), Some(250), "frame 數：{fps:#}");
    assert_eq!(fps["span_ms"].as_u64(), Some(9_960), "樣本跨度：{fps:#}");
    assert_eq!(fps["first_timestamp"].as_u64(), Some(135_000));
    assert_eq!(fps["last_timestamp"].as_u64(), Some(1_031_400));

    // 精度：恰好 25.000（f64 可精確表示 25）。
    assert_eq!(
        fps["mean_fps"].as_f64(),
        Some(25.0),
        "mean 必須恰好 25.000：{fps:#}"
    );
    assert_eq!(
        fps["median_fps"].as_f64(),
        Some(25.0),
        "median 必須恰好 25.000：{fps:#}"
    );
    assert_eq!(fps["mean_interval_ms"].as_f64(), Some(40.0));
    assert_eq!(fps["median_interval_ms"].as_f64(), Some(40.0));
    assert_eq!(fps["min_interval_ms"].as_f64(), Some(40.0));
    assert_eq!(fps["max_interval_ms"].as_f64(), Some(40.0));
    assert_eq!(
        fps["variable"].as_bool(),
        Some(false),
        "這個 fixture 是 CFR：{fps:#}"
    );
    assert_eq!(fps["default_duration_ns"].as_u64(), Some(40_000_000));

    // marker 只做一致性檢查：合成器每個 RTP 封包都設 marker（含 SPS/PPS），
    // marker 數目必然大於幀數；fps 仍然恰好 25 代表沒有把 marker 當幀邊界。
    assert!(
        fps["marker_mismatch"].as_u64().unwrap_or(0) > 0,
        "fixture 的 marker 多於幀數，mismatch 應 >0：{fps:#}"
    );
}

/// 短 session：不足 MIN_FRAMES（30 幀）時不得給出 fps。
/// 19 幀的 timestamp 差仍然可算出 25，但樣本不足，正確行為是 `fps: null`，
/// 不能回 0、NaN，也不能 panic。
#[test]
fn k18_ac2_short_session_fps_is_null() {
    let json = run_report("k18_short");
    assert!(
        json["fps"].is_null(),
        "少於 30 幀的 session 必須回 fps=null：{json:#}"
    );
    let text = json.to_string();
    assert!(!text.contains("NaN"), "報告不得出現 NaN：{text}");
    assert!(!text.contains("inf"), "報告不得出現 inf：{text}");
}

/// 重連：第一個 session 40 幀、第二個 250 幀，結果必須取第二個（幀數最多），
/// 且 first_timestamp 必須是第二個 session 的基準，不得沿用第一個 session 的 135000。
#[test]
fn k18_ac3_reconnect_uses_window_with_most_frames() {
    let json = run_report("k18_reconnect");
    let fps = &json["fps"];
    assert!(fps.is_object(), "重連後應仍有有效 fps 物件：{json:#}");
    assert_eq!(
        fps["frames"].as_u64(),
        Some(250),
        "應取 250 幀那個窗：{fps:#}"
    );
    assert_eq!(fps["first_timestamp"].as_u64(), Some(315_000));
    assert_eq!(fps["last_timestamp"].as_u64(), Some(1_211_400));
    assert_eq!(fps["median_fps"].as_f64(), Some(25.0));
    assert_eq!(fps["mean_fps"].as_f64(), Some(25.0));
    assert_ne!(
        fps["first_timestamp"].as_u64(),
        Some(135_000),
        "第一個 session 的 timestamp 基線不得污染結果：{fps:#}"
    );
}
