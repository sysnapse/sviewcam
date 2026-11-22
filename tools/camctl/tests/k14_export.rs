//! k14 匯出端到端測試（需要 CI 的 ffmpeg；devbox 的 rust 模板沒有 ffmpeg）。
//!
//! 這裡刻意用 `replay` 這支產品 CLI 產生真的 `seg-*.mkv`＋`index.sqlite`，
//! 不另外寫假錄影器；`replay` 的二進位由 `cargo test --workspace` 一起建置。

use std::path::{Path, PathBuf};
use std::process::Command;

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

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sview-k14-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建立暫存目錄");
    dir
}

fn run_replay(dir: &Path) {
    let out = Command::new(replay())
        .arg("--input")
        .arg(fixture("ac4_gap"))
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
