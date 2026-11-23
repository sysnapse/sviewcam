//! fixture 健檢：`synth_stream` step 的 `t_ms` 必須嚴格遞增。
//!
//! replay 的合成串流以「下一個明確 step 的 `t_ms`」為上限（`rtsp-core` 的
//! `StepKind::Synth` 分支）。同一個 `t_ms` 寫第二個 `synth_stream` 時，第一個
//! 整段被跳過、對輸出沒有任何影響——fixture 看起來有兩段串流，實際只跑一段。
//! 這種行是死的，用這條測試擋住同類復發。

use std::path::{Path, PathBuf};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/m1")
}

#[test]
fn synth_stream_steps_have_strictly_increasing_t_ms() {
    let dir = fixtures_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("讀 {}：{e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "{} 下沒有 fixture", dir.display());

    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for path in files {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("讀 {name}：{e}"));
        let mut prev: Option<(u64, usize)> = None;
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{name} 第 {} 行不是 JSON：{e}", lineno + 1));
            if v.get("type").and_then(|t| t.as_str()) != Some("synth_stream") {
                continue;
            }
            let t_ms = v
                .get("t_ms")
                .and_then(|t| t.as_u64())
                .unwrap_or_else(|| panic!("{name} 第 {} 行 synth_stream 缺 t_ms", lineno + 1));
            checked += 1;
            if let Some((prev_t, prev_line)) = prev {
                if t_ms <= prev_t {
                    failures.push(format!(
                        "{name}: 第 {} 行 t_ms={t_ms} 未嚴格遞增（上一筆第 {prev_line} 行 t_ms={prev_t}）",
                        lineno + 1
                    ));
                }
            }
            prev = Some((t_ms, lineno + 1));
        }
    }

    // 路徑若指錯，測試會什麼都沒掃到而假綠；用最低數量釘住。
    assert!(
        checked >= 10,
        "只掃到 {checked} 個 synth_stream，fixtures 路徑可能不對：{}",
        dir.display()
    );
    assert!(
        failures.is_empty(),
        "synth_stream 的 t_ms 必須嚴格遞增（同 t_ms 的第二段會被 replay 整段跳過）：\n{}",
        failures.join("\n")
    );
}
