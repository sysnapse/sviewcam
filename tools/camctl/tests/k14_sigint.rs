//! Ctrl-C：camctl 必須真的把 ffmpeg child 砍掉、回 130、且不留暫存檔。
//!
//! 用假 ffmpeg（睡 30 秒）是唯一能穩定卡住子行程的方法；真 ffmpeg 對小片段
//! 可能幾十毫秒就跑完，測試會變成在賽跑。

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn camctl() -> &'static str {
    env!("CARGO_BIN_EXE_camctl")
}

fn replay() -> PathBuf {
    Path::new(camctl())
        .parent()
        .expect("target/debug")
        .join("replay")
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/m1/ac4_gap.jsonl")
}

#[test]
fn sigint_returns_130_and_leaves_no_temp_files() {
    let dir = std::env::temp_dir().join(format!("sview-k14-int-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建立目錄");
    let rec = Command::new(replay())
        .arg("--input")
        .arg(fixture())
        .arg("--record-dir")
        .arg(&dir)
        .output()
        .expect("replay");
    assert!(rec.status.success(), "replay 失敗");

    let fake = dir.join("fake-ffmpeg.sh");
    let marker = dir.join("ffmpeg-spawned");
    std::fs::write(
        &fake,
        "#!/bin/sh\ncase \"$*\" in *-version*) echo \"ffmpeg version 7.0.2\"; exit 0;; esac\ntouch \"$K14_FAKE_MARKER\"\nexec sleep 30\n",
    )
    .expect("寫假 ffmpeg");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let out_path = dir.join("cancelled.mp4");
    let child = Command::new(camctl())
        .arg("export")
        .arg("--dir")
        .arg(&dir)
        .args(["--from", "-100s", "--to", "-40s", "--output"])
        .arg(&out_path)
        .arg("--ffmpeg")
        .arg(&fake)
        .env("K14_FAKE_MARKER", &marker)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn camctl");

    // 等它真的 spawn 假 ffmpeg（腳本會 touch marker）再送 SIGINT，避免賽跑。
    // 不能只等 `.k14-*`：remux 階段的路徑只是預定檔名，假 ffmpeg 不會建立它。
    let mut saw = false;
    for _ in 0..100 {
        if marker.exists() {
            saw = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(saw, "camctl 還沒 spawn 假 ffmpeg");
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    let out = child.wait_with_output().expect("等待 camctl");
    assert_eq!(out.status.code(), Some(130), "SIGINT 應回 130");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("已取消（SIGINT）"), "stderr={stderr}");
    assert!(!out_path.exists(), "取消後不該留下輸出檔");
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .expect("讀目錄")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with(".k14-") || name.ends_with(".concat") || name.ends_with(".part")
        })
        .collect();
    assert!(leftovers.is_empty(), "取消後留下暫存：{leftovers:?}");
}
