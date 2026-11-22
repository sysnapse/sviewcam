//! k14 匯出端到端測試（需要 CI 的 ffmpeg；devbox 的 rust 模板沒有 ffmpeg）。
//!
//! 這裡刻意用 `replay` 這支產品 CLI 產生真的 `seg-*.mkv`＋`index.sqlite`，
//! 不另外寫假錄影器；`replay` 的二進位由 `cargo test --workspace` 一起建置。

use std::path::{Path, PathBuf};
use std::process::Command;

use camcore::{RingConfig, SegmentIndex};

fn camctl() -> &'static str {
    env!("CARGO_BIN_EXE_camctl")
}

fn replay() -> PathBuf {
    Path::new(camctl())
        .parent()
        .expect("target/debug")
        .join("replay")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/m1")
        .join(format!("{name}.jsonl"))
}

fn fixture_m2(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/m2")
        .join(format!("{name}.jsonl"))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sview-k14-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建立暫存目錄");
    dir
}

fn require_ffmpeg() {
    let out = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .expect("本機無 ffmpeg：AC③ 未驗（CI 用 ubuntu-latest 的系統 ffmpeg；本機請跑 fixtures/m2/fetch_ffmpeg.sh）");
    assert!(out.status.success(), "ffmpeg -version 失敗");
}

fn open_index(dir: &Path) -> SegmentIndex {
    SegmentIndex::open(RingConfig::new(dir).index_path()).expect("開索引")
}

fn run_replay_fixture(dir: &Path, input: &Path) {
    let out = Command::new(replay())
        .arg("--input")
        .arg(input)
        .arg("--record-dir")
        .arg(dir)
        .output()
        .expect("執行 replay");
    assert!(
        out.status.success(),
        "replay 失敗：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run_replay_named(dir: &Path, name: &str) {
    run_replay_fixture(dir, &fixture(name));
}

fn run_replay(dir: &Path) {
    run_replay_named(dir, "ac4_gap");
}

fn run_export(dir: &Path, output: &Path, from: &str, to: &str) -> std::process::Output {
    Command::new(camctl())
        .arg("export")
        .arg("--dir")
        .arg(dir)
        .arg("--from")
        .arg(from)
        .arg("--to")
        .arg(to)
        .arg("--output")
        .arg(output)
        .arg("--json")
        .output()
        .expect("執行 camctl export")
}

#[test]
fn ac3_gap_is_split_into_separate_outputs_and_reported() {
    require_ffmpeg();
    let dir = temp_dir("ac3");
    run_replay(&dir);
    let output = dir.join("export.mp4");
    let out = run_export(&dir, &output, "-100s", "-40s");
    assert!(
        out.status.success(),
        "camctl export 失敗：rc={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout 必須是 JSON");
    let outputs = report["outputs"].as_array().expect("outputs[]");
    assert_eq!(outputs.len(), 2, "跨 gap 必須剛好兩個輸出：{report}");
    let gaps = report["gaps"].as_array().expect("gaps[]");
    assert_eq!(gaps.len(), 1, "必須只有一個 gap：{report}");
    assert_eq!(gaps[0]["gap_ms"], 3500, "gap 長度：{report}");
    assert_eq!(gaps[0]["at_ms"], 103500, "gap 收口時間：{report}");
    // AC②：輸出起點必須正好是某個「首 block 是 keyframe」的段首，
    // 不是 ffmpeg 自己挑的 keyframe。
    let rows = open_index(&dir).rows().expect("索引 rows");
    let first_start = outputs[0]["t_start_ms"].as_u64().expect("t_start_ms");
    assert!(
        rows.iter().any(|r| r.t_start == first_start && r.first_block_keyframe == Some(true)),
        "第一個輸出的 t_start {} 不是可獨立解碼段的段首：{:?}",
        first_start,
        rows.iter()
            .map(|r| (r.t_start, r.first_block_keyframe))
            .collect::<Vec<_>>()
    );
    for item in outputs {
        let path = PathBuf::from(item["path"].as_str().expect("output path"));
        assert!(path.is_file(), "輸出檔不存在：{}", path.display());
        assert!(
            path.metadata().expect("metadata").len() > 0,
            "輸出檔是空的：{}",
            path.display()
        );
        assert!(
            item["segments"].as_array().is_some_and(|s| !s.is_empty()),
            "輸出必須列出段：{item}"
        );
    }
    // AC5 的一部分：成功後不得留下 concat 清單或 .k14-* 暫存。
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .expect("讀輸出目錄")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".k14-") || name.ends_with(".concat"))
        .collect();
    assert!(leftovers.is_empty(), "留下暫存檔：{leftovers:?}");
}


fn ffprobe_duration(path: &Path) -> f64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .expect("執行 ffprobe duration");
    assert!(out.status.success(), "ffprobe duration 失敗");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .expect("duration 必須是數字")
}

fn ffprobe_packet_pts_s(path: &Path) -> Vec<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("執行 ffprobe packets");
    assert!(out.status.success(), "ffprobe packets 失敗");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<f64>().ok())
        .collect()
}


#[test]
fn replay_recorded_export_has_honest_packet_timeline() {
    require_ffmpeg();
    let dir = temp_dir("honest-timeline");
    // 這是 `replay --record-dir` 產的錄影，不是 pipeline 產的；它們的段檔
    // `format.start_time` 是 N/A，concat demuxer 不會自動 rebase 絕對 pts。
    run_replay(&dir);
    let output = dir.join("honest.mp4");
    let out = run_export(&dir, &output, "-100s", "-40s");
    assert!(
        out.status.success(),
        "camctl export 失敗：rc={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout 必須是 JSON");
    let outputs = report["outputs"].as_array().expect("outputs[]");
    assert_eq!(outputs.len(), 2, "跨 gap 必須兩個輸出：{report}");

    for item in outputs {
        let path = PathBuf::from(item["path"].as_str().expect("output path"));
        // 期望長度用「段內幀數 × 40 ms」算，不用 mkv::scan 的 t_end-t_start：
        // 後者與匯出端算 duration 的來源重疊，會變成自己驗自己。
        let mut seg_blocks = Vec::new();
        for name in item["segments"].as_array().expect("segments[]") {
            let name = name.as_str().expect("segment name");
            let info = camcore::mkv::scan(&dir.join(name)).expect("scan source segment");
            seg_blocks.push(info.blocks as u64);
        }
        let frames: u64 = seg_blocks.iter().sum();
        let expected_ms = frames as f64 * 40.0; // ac4_gap 是 25 fps
        let duration_ms = ffprobe_duration(&path) * 1000.0;
        let delta = (duration_ms - expected_ms).abs();
        assert!(
            delta <= 500.0,
            "輸出 duration 必須是幀數×40ms ±0.5s：path={} duration={duration_ms:.0}ms frames={frames} expected={expected_ms:.0}ms delta={delta:.0}ms\nreport={report}",
            path.display()
        );
        let pts = ffprobe_packet_pts_s(&path);
        assert_eq!(
            pts.len() as u64,
            frames,
            "輸出 frame 數必須等於各段 frame 數之和（不得靠丟幀湊時間軸）：{}",
            path.display()
        );
        assert!(
            pts.windows(2).all(|w| w[0] < w[1]),
            "PTS 必須嚴格遞增：{:?}",
            pts.windows(2).find(|w| w[0] >= w[1])
        );
        let max_gap = pts
            .windows(2)
            .map(|w| w[1] - w[0])
            .fold(0.0_f64, f64::max);
        assert!(
            max_gap <= 0.1,
            "逐 packet 最大 pts 間隔必須 <=0.1s，實際 {max_gap:.4}s：{}",
            path.display()
        );
        // 段邊界不得貼成 1 ms：每個邊界至少要有半個幀距（20 ms）。
        let mut boundary = 0usize;
        for blocks in seg_blocks.iter().take(seg_blocks.len().saturating_sub(1)) {
            boundary += *blocks as usize;
            let gap = pts[boundary] - pts[boundary - 1];
            assert!(
                gap >= 0.020,
                "第 {boundary} 幀的段邊界只有 {gap:.4}s（<半個幀距 20 ms），時間軸被壓縮：{}",
                path.display()
            );
        }
    }
}

#[test]
fn p_first_segments_are_excluded_not_merged() {
    require_ffmpeg();
    let dir = temp_dir("pfirst");
    // k24_no_idr：0..45s 只有第一段 keyframe，後兩段是 16 秒硬切出的 P-first。
    run_replay_named(&dir, "k24_no_idr");
    let output = dir.join("run.mp4");
    let out = run_export(&dir, &output, "-30s", "0");
    assert!(
        out.status.success(),
        "camctl export 失敗：rc={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout 必須是 JSON");
    let outputs = report["outputs"].as_array().expect("outputs[]");
    assert_eq!(
        outputs.len(),
        1,
        "只有第一個 kf 段可獨立解碼；P-first 段應被排除而不是併入：{report}"
    );
    let first = &outputs[0];
    assert_eq!(first["t_start_ms"], 1500, "應從 kf 段首開始：{first}");
    assert_eq!(first["t_end_ms"], 17500, "P-first 段不得被算進輸出：{first}");
    assert!(
        first.get("merged_p_first_segments").is_none(),
        "已不再有「併入」欄位：{first}"
    );
    let excluded = report["excluded_segments"]
        .as_array()
        .expect("excluded_segments[]");
    assert_eq!(excluded.len(), 2, "兩個 P-first 段都要列出來：{excluded:?}");
    let ids: Vec<&str> = excluded
        .iter()
        .map(|e| e["id"].as_str().unwrap_or(""))
        .collect();
    assert!(ids.contains(&"seg-000000017500"), "缺 17500：{ids:?}");
    assert!(ids.contains(&"seg-000000033500"), "缺 33500：{ids:?}");
    assert!(
        excluded.iter().all(|e| e["reason"].as_str().unwrap_or("").contains("不可獨立解碼")),
        "每個排除都要有原因：{excluded:?}"
    );

    // duration 必須是真正內容的 16 秒，不是 run 累加出來的 47.96 秒。
    let duration = ffprobe_duration(&output);
    assert!(
        (15.0..=17.0).contains(&duration),
        "輸出 duration 應約 16 秒，實際 {duration}"
    );
    let pts = ffprobe_packet_pts_s(&output);
    assert!(pts.len() > 300, "輸出幀數太少：{}", pts.len());
    assert!(
        pts.windows(2).all(|w| w[0] < w[1]),
        "PTS 必須嚴格遞增（不得有時戳跳躍）：{:?}",
        pts.windows(2).find(|w| w[0] >= w[1])
    );
    assert!(
        pts.windows(2).all(|w| w[1] - w[0] < 0.2),
        "PTS 不應出現定格級跳躍：{:?}",
        pts.windows(2).find(|w| w[1] - w[0] >= 0.2)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("2 個段因不可獨立解碼被排除（時間軸不連續）"),
        "stderr 必須明示排除：{stderr}"
    );
    assert!(stderr.contains("seg-000000017500"), "stderr 缺 id：{stderr}");
    assert!(stderr.contains("seg-000000033500"), "stderr 缺 id：{stderr}");
    assert!(stderr.contains("輸出長度"), "padding 警告應說輸出長度：{stderr}");
}

#[test]
fn range_starting_in_pfirst_skips_to_later_keyframe() {
    require_ffmpeg();
    let dir = temp_dir("pfirst-later-kf");
    // GOP 30 s：16 s 硬切出 P-first 段，下一個 IDR 在 31.5 s。
    run_replay_fixture(&dir, &fixture_m2("k14_pfirst_then_kf"));
    let output = dir.join("run.mp4");
    // anchor 是最後段 t_end＝41.5 s；-20 s 起點 21.5 s 落在 P-first 段內，
    // 後面還有 31.5 s 開始的 kf 段。
    let out = run_export(&dir, &output, "-20s", "0");
    assert!(
        out.status.success(),
        "範圍內還有 kf 段時必須 rc=0：rc={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout 必須是 JSON");
    let outputs = report["outputs"].as_array().expect("outputs[]");
    assert_eq!(outputs.len(), 1, "P-first 段後應接一個 kf run：{report}");
    assert_eq!(outputs[0]["t_start_ms"], 31500, "必須從後面的 kf 段開始：{report}");
    assert_eq!(outputs[0]["t_end_ms"], 41460);
    let excluded = report["excluded_segments"]
        .as_array()
        .expect("excluded_segments[]");
    assert_eq!(excluded.len(), 1, "應只排除起點那個 P-first 段：{excluded:?}");
    assert_eq!(excluded[0]["id"], "seg-000000017500");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("1 個段因不可獨立解碼被排除") && stderr.contains("seg-000000017500"),
        "stderr 必須明示被跳過的 P-first：{stderr}"
    );
    let duration = ffprobe_duration(&output);
    assert!(
        (9.0..=11.0).contains(&duration),
        "只應輸出 31.5..41.5 的 10 秒 kf 段，實際 {duration}"
    );
}

#[test]
fn range_without_any_keyframe_segment_is_rejected() {
    require_ffmpeg();
    let dir = temp_dir("no-kf");
    run_replay_named(&dir, "k24_no_idr");
    let output = dir.join("none.mp4");
    // -10s..0 只覆蓋最後兩個 P-first 段，沒有任何 kf 段可起 run。
    let out = run_export(&dir, &output, "-10s", "0");
    assert_eq!(
        out.status.code(),
        Some(2),
        "沒有 kf 段時必須 rc=2；stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("都不可獨立解碼"),
        "錯誤訊息要說清楚不可解碼：{stderr}"
    );
    assert!(!output.exists(), "失敗時不得留下輸出檔");
}

#[test]
fn unknown_gap_is_reported_as_unknown_in_text_stderr() {
    require_ffmpeg();
    let dir = temp_dir("unknown-gap");
    run_replay(&dir);

    // 把已知的 3500 ms gap 改成「未知」（模擬舊索引／沒有 gap 資訊的段）。
    let index = open_index(&dir);
    let mut rows = index.rows().expect("讀索引");
    let gap_row = rows
        .iter_mut()
        .find(|r| r.gap_ms.is_some_and(|ms| ms > 0))
        .expect("ac4_gap 應有已知 gap 列");
    gap_row.gap_ms = None;
    let row = gap_row.clone();
    index.upsert(&row).expect("寫回未知 gap");

    let output = dir.join("split.mp4");
    let out = Command::new(camctl())
        .arg("export")
        .arg("--dir")
        .arg(&dir)
        .arg("--from")
        .arg("-100s")
        .arg("--to")
        .arg("-40s")
        .arg("--output")
        .arg(&output)
        .output()
        .expect("執行 camctl export");
    assert!(
        out.status.success(),
        "camctl export 失敗：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("gap 未知（段邊界差") && stderr.contains("不靜默拼接"),
        "未知 gap 必須在文字 stderr 明說是推測，不能印成確定值：{stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.lines().count(),
        2,
        "未知 gap 應分成兩個輸出：{stdout}"
    );
}
