//! AC③：第二個 camd 必須非零 exit 並說清楚「已在執行（pid N）」；被 `kill -9`
//! 留下的 stale 鎖要能接手。
//!
//! 這條測試不需要相機：`--uri` 指向關閉的埠，camd 只會停在 Disconnected 並重試。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use camipc::Client;

fn camd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_camd"))
}

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("camd-test-{tag}-{}-{nanos}", std::process::id()))
}

fn spawn_camd(dir: &Path, socket: &Path) -> Child {
    Command::new(camd_bin())
        .arg("--dir")
        .arg(dir)
        .arg("--socket")
        .arg(socket)
        .arg("--uri")
        .arg("rtsp://127.0.0.1:1/test.264")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn camd")
}

fn wait_for_socket(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if socket.exists() {
            // socket 檔出現後再等一下，確保 accept 迴圈已啟動。
            std::thread::sleep(Duration::from_millis(100));
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn second_instance_is_rejected_and_names_the_holder() {
    let dir = unique_dir("second");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("camd.sock");

    let mut first = spawn_camd(&dir, &socket);
    assert!(
        wait_for_socket(&socket, Duration::from_secs(10)),
        "第一支 camd 沒有在 10 秒內 bind socket"
    );

    // 先證明第一支真的在服務。
    let mut client = Client::connect(&socket).expect("連上第一支 camd");
    let status = client.call("status", serde_json::json!({})).unwrap();
    assert!(status.result.is_some(), "第一支 camd 應回應 status");

    let second = Command::new(camd_bin())
        .arg("--dir")
        .arg(&dir)
        .arg("--socket")
        .arg(&socket)
        .arg("--uri")
        .arg("rtsp://127.0.0.1:1/test.264")
        .output()
        .expect("spawn second camd");
    let stderr = String::from_utf8_lossy(&second.stderr);

    assert_eq!(
        second.status.code(),
        Some(1),
        "第二實例 exit code 應為 1；stderr={stderr}"
    );
    assert!(
        stderr.contains("已在執行"),
        "第二實例 stderr 應含「已在執行」；實得：{stderr}"
    );
    assert!(
        stderr.contains(&format!("pid {}", first.id())),
        "第二實例 stderr 應帶第一個 camd 的 pid {}；實得：{stderr}",
        first.id()
    );

    // 第一支必須活著：再用另一條連線問一次。
    let mut client = Client::connect(&socket).expect("第一支 camd 應仍存活");
    let pong = client.call("ping", serde_json::json!({})).unwrap();
    assert_eq!(pong.result.unwrap()["pong"], serde_json::json!(true));

    let _ = first.kill();
    let _ = first.wait();
    cleanup(&dir);
}

#[test]
fn stale_lock_from_killed_daemon_is_taken_over() {
    let dir = unique_dir("stale");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("camd.sock");

    let mut first = spawn_camd(&dir, &socket);
    assert!(
        wait_for_socket(&socket, Duration::from_secs(10)),
        "第一支 camd 沒有在 10 秒內 bind socket"
    );
    // kill -9：不跑 Drop，鎖與 socket 都留在磁碟上。
    first.kill().unwrap();
    let _ = first.wait();
    assert!(dir.join("camd.lock").exists(), "kill -9 後應留下鎖檔");

    let mut second = spawn_camd(&dir, &socket);
    let started = wait_for_socket(&socket, Duration::from_secs(10));
    if !started {
        let _ = second.kill();
        let _ = second.wait();
        cleanup(&dir);
        panic!("stale 鎖沒有被接手：新 camd 未 bind socket");
    }

    let mut client = Client::connect(&socket).expect("接手後的 camd 應可連線");
    let status = client.call("status", serde_json::json!({})).unwrap();
    assert!(status.result.is_some());

    let _ = second.kill();
    let _ = second.wait();
    cleanup(&dir);
}
