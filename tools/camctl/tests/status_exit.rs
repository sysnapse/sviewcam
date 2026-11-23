//! `camctl status`／`list-segments` 的出口碼契約。
//!
//! 這幾條不需要 camd 或 ffmpeg：用一個只回固定 JSON 的假 socket 伺服器覆蓋
//! 「成功」「`PROTO_VERSION`」「`BAD_PARAMS`」三種回應，再加一條「socket 不存在」。
//! 出口碼表見 README：0＝成功、2＝參數／環境／協議錯誤。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

fn camctl_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_camctl"))
}

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "camctl-status-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

/// 起一個假 camd：accept 之後回一行固定 JSON 就關連線。
fn fake_server(socket: &Path, reply: &'static str) -> thread::JoinHandle<()> {
    let listener = UnixListener::bind(socket).expect("bind fake socket");
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut writer = stream;
            let _ = writer.write_all(format!("{reply}\n").as_bytes());
            let _ = writer.flush();
        }
    })
}

fn run(cmd: &mut Command) -> (i32, String, String) {
    let output = cmd.output().expect("run camctl");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

const STATUS_OK: &str = r#"{"id":1,"result":{"proto_version":1,"camd":{"pid":42},"recording":{"segments":2,"total_bytes":1234,"latest_end_ms":9000},"camera":{"state":"Connected"},"fps":null}}"#;

#[test]
fn status_success_prints_json_and_exits_zero() {
    let dir = unique_dir("ok");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("camd.sock");
    let server = fake_server(&socket, STATUS_OK);

    let (code, stdout, stderr) = run(Command::new(camctl_bin())
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json"));
    assert_eq!(code, 0, "成功要 rc=0；stderr={stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("--json 必須是合法 JSON（{e}）：{stdout}"));
    assert_eq!(
        value.get("proto_version").and_then(|v| v.as_u64()),
        Some(1),
        "--json 要原樣輸出協議內容，得到 {stdout}"
    );
    // 人看摘要走 stderr，--json 模式仍會印一行摘要；stdout 必須只有 JSON。
    assert!(!stdout.contains("camd pid"), "stdout 不得混人話：{stdout}");

    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn proto_version_error_exits_two_and_mentions_version() {
    let dir = unique_dir("proto");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("camd.sock");
    let server = fake_server(
        &socket,
        r#"{"id":1,"error":{"code":"PROTO_VERSION","message":"協議版本不符","data":{"expected":1,"got":99}}}"#,
    );

    let (code, _stdout, stderr) = run(Command::new(camctl_bin())
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json"));
    assert_eq!(code, 2, "PROTO_VERSION 要 rc=2；stderr={stderr}");
    assert!(
        stderr.contains("PROTO_VERSION"),
        "錯誤碼要出現在 stderr：{stderr}"
    );

    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_params_exits_two() {
    let dir = unique_dir("params");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("camd.sock");
    let server = fake_server(
        &socket,
        r#"{"id":1,"error":{"code":"BAD_PARAMS","message":"from 必須 <= to"}}"#,
    );

    let (code, _stdout, stderr) = run(Command::new(camctl_bin())
        .arg("list-segments")
        .arg("--socket")
        .arg(&socket)
        .arg("--json"));
    assert_eq!(code, 2, "BAD_PARAMS 要 rc=2；stderr={stderr}");
    assert!(stderr.contains("BAD_PARAMS"), "stderr={stderr}");

    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_socket_exits_two_and_prints_resolved_path() {
    let dir = unique_dir("missing");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("camd.sock");

    let (code, _stdout, stderr) = run(Command::new(camctl_bin())
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json"));
    assert_eq!(code, 2, "socket 不存在要 rc=2；stderr={stderr}");
    assert!(
        stderr.contains(&socket.display().to_string()),
        "錯誤訊息要印解析出的路徑：{stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
