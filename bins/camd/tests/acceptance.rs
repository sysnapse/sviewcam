//! M3 切片 1 的驗收測試（AC①／②／④／⑤）。
//!
//! 全部用**真 process**：`camd` 是受測程式，`camd-replay` 是假相機。測試自己帶
//! deadline（每條 < 60 秒）並在結束時把子行程收乾淨，不留下 `kill -9` 的孤兒。
//!
//! 需要 `camctl`、`camd-replay` 已建置：CI 先跑 `cargo build --workspace --bins`；
//! 本機同上。找不到二元檔時測試**大聲失敗**（不是 skip）。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use camcore::{mkv, RingConfig, SegmentIndex};
use camipc::Client;
use serde_json::Value;

/// 每條測試的硬上限。
const TEST_BUDGET: Duration = Duration::from_secs(60);

fn bin(name: &str) -> PathBuf {
    let env_key = format!("CAMD_ACC_{}_BIN", name.to_uppercase().replace('-', "_"));
    if let Ok(path) = std::env::var(&env_key) {
        return PathBuf::from(path);
    }
    let exe = std::env::current_exe().expect("current_exe");
    // 測試二進位可能被放在 target/debug/deps 或 target/debug/build/<pkg>/<hash>；
    // 一律往上找 `target` 目錄再取 `target/debug`。
    let mut dirs = Vec::new();
    for ancestor in exe.ancestors() {
        if ancestor.file_name().is_some_and(|n| n == "target") {
            dirs.push(ancestor.join("debug"));
            break;
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    dirs.push(manifest.join("../../target/debug"));
    for dir in dirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    exe.parent().unwrap().join(name)
}

/// 檢查二元檔存在；不存在就大聲失敗並說明怎麼建。
fn require_bin(name: &str) -> PathBuf {
    let path = bin(name);
    assert!(
        path.is_file(),
        "找不到 {name}（{}）：先跑 `cargo build --locked --workspace --bins`",
        path.display()
    );
    path
}

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("camd-acc-{tag}-{}-{nanos}", std::process::id()))
}

/// `camd` 子行程：測試結束（含 panic）時一定被殺掉。
struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Daemon {
    fn start(dir: &Path, socket: &Path, uri: &str) -> Self {
        let child = Command::new(require_bin("camd"))
            .arg("--dir")
            .arg(dir)
            .arg("--socket")
            .arg(socket)
            .arg("--uri")
            .arg(uri)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn camd");
        let mut daemon = Self {
            child,
            socket: socket.to_path_buf(),
        };
        daemon.wait_socket(Duration::from_secs(15));
        daemon
    }

    fn wait_socket(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.socket.exists() {
                std::thread::sleep(Duration::from_millis(100));
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("camd 在 bind socket 前退出：{status:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        panic!("camd 沒有在 {timeout:?} 內 bind socket");
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// 停掉 daemon 並等它真的結束：磁碟證據要在「沒有 writer 同時在寫」時收集，
    /// 否則會讀到剛 rename、但最後一個 cluster 還沒 flush 完的檔案（CI 上實際踩過）。
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `camd-replay` 假相機：從 port file 讀回實際埠。
struct Replay {
    child: Child,
    port: u16,
}

impl Replay {
    fn start(dir: &Path) -> Self {
        Self::spawn(dir, None)
    }

    /// 送真的 H.264（ffmpeg 現生）：AC② 要驗「已落盤段可播」，合成的 bitstream
    /// 不是可解碼視訊，只能拿來驗時間軸，不能拿來驗解碼。
    fn start_real_h264(dir: &Path) -> Self {
        let source = dir.join("source.h264");
        let status = Command::new(ffmpeg_bin())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=25",
                "-t",
                "8",
                "-c:v",
                "libx264",
                "-g",
                "50",
                "-pix_fmt",
                "yuv420p",
                "-f",
                "h264",
            ])
            .arg(&source)
            .status()
            .expect("跑 ffmpeg 生成測試視訊");
        assert!(status.success(), "ffmpeg 生成測試視訊失敗");
        Self::spawn(dir, Some(&source))
    }

    fn spawn(dir: &Path, h264: Option<&Path>) -> Self {
        let port_file = dir.join("replay.port");
        let mut command = Command::new(require_bin("camd-replay"));
        command.arg("--port-file").arg(&port_file);
        if let Some(path) = h264 {
            command.arg("--h264").arg(path);
        }
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn camd-replay");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(&port_file) {
                if let Ok(port) = text.trim().parse::<u16>() {
                    return Self { child, port };
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("camd-replay 沒有在 10 秒內寫出 port file");
    }

    fn uri(&self) -> String {
        format!("rtsp://127.0.0.1:{}/test.264", self.port)
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn status(socket: &Path) -> Value {
    let mut client = Client::connect(socket).unwrap_or_else(|e| panic!("連 camd 失敗：{e}"));
    camipc::client::into_result(client.call("status", serde_json::json!({})).unwrap())
        .unwrap_or_else(|e| panic!("status 失敗：{e:?}"))
}

fn wait_segments(socket: &Path, at_least: usize, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    let mut last = Value::Null;
    while Instant::now() < deadline {
        match Client::connect(socket) {
            Ok(mut client) => {
                if let Ok(resp) = client.call("status", serde_json::json!({})) {
                    if let Ok(value) = camipc::client::into_result(resp) {
                        let segments = value["recording"]["segments"].as_u64().unwrap_or(0);
                        last = value;
                        if segments >= at_least as u64 {
                            return last;
                        }
                    }
                }
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("等不到 {at_least} 個已閉合段；最後 status={last}");
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn ffmpeg_bin() -> String {
    std::env::var("CAMD_ACC_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string())
}

/// 全解碼一次，回傳任何錯誤行。用 `rawvideo` 而不是 `-f null`：`-f null` 會把
/// **主機時鐘抖動**（實測段內幀距 39／41 ms）造成的非整數時間戳拿去 CFR 換算，
/// 印出「non monotonically increasing dts to muxer」的 muxer 警告（解碼成功、幀數
/// 也對）；那是 M1 以到達時鐘記時間的既有性狀，不是檔案壞掉。解碼錯誤照樣會噴。
fn ffmpeg_decode_errors(path: &Path) -> Vec<String> {
    let exe = ffmpeg_bin();
    let output = Command::new(&exe)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-y"])
        .arg(std::env::temp_dir().join(format!(
            "camd-acc-decode-{}-{}.yuv",
            std::process::id(),
            path.file_name().unwrap_or_default().to_string_lossy()
        )))
        .output()
        .unwrap_or_else(|e| {
            panic!("需要 {exe} 驗 AC②（CI 有；本機請跑 fixtures/m2/fetch_ffmpeg.sh）：{e}")
        });
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.to_string())
        .collect()
}

/// 解出來的幀數；用來確認沒有靜默丟幀（錯誤行 0 也要幀數對）。
fn ffmpeg_count_frames(path: &Path) -> u64 {
    let exe = ffmpeg_bin();
    let command = if exe == "ffmpeg" {
        "ffprobe".to_string()
    } else {
        exe
    };
    let output = Command::new(&command)
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("需要 ffprobe 數 AC② 幀數：{e}"));
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or_else(|_| panic!("ffprobe 幀數解析失敗：{:?}", output.stdout))
}

/// 段檔診斷：解碼失敗時要能在 CI log 裡看出檔案長怎樣（大小、前後 byte），
/// 才不會只剩一句「解碼有錯誤」。
fn describe_segment(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let head = &bytes[..bytes.len().min(48)];
    let tail_start = bytes.len().saturating_sub(48);
    let tail = &bytes[tail_start..];
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join("");
    format!(
        "path={} size={} head={} tail={} blocks={:?}",
        path.display(),
        bytes.len(),
        hex(head),
        hex(tail),
        mkv::scan(path).map(|i| (i.t_start_ms, i.t_end_ms, i.blocks, i.bytes)),
    )
}

/// AC①：控制連線被強制斷開後，錄影與索引繼續推進。
#[test]
fn control_disconnect_does_not_stop_recording() {
    let dir = unique_dir("ac1");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("camd.sock");
    let replay = Replay::start(&dir);
    let mut daemon = Daemon::start(&dir, &socket, &replay.uri());

    let before = wait_segments(&socket, 1, TEST_BUDGET);
    let n0 = before["recording"]["segments"].as_u64().unwrap();
    let end0 = before["recording"]["latest_end_ms"].as_u64().unwrap();

    // 一條連線做完 status 之後直接 drop（不送 TEARDOWN），模擬使用者關掉 camctl。
    {
        let mut client = Client::connect(&socket).unwrap();
        let _ = client.call("status", serde_json::json!({})).unwrap();
    }

    std::thread::sleep(Duration::from_secs(4));
    let after = status(&socket);
    let n1 = after["recording"]["segments"].as_u64().unwrap();
    let end1 = after["recording"]["latest_end_ms"].as_u64().unwrap_or(0);
    assert!(
        n1 > n0,
        "斷線後索引應繼續推進：before segments={n0}，after={n1}"
    );
    assert!(
        end1 > end0,
        "斷線後最新 t_end 應前進：before={end0}，after={end1}"
    );
    assert!(daemon.alive(), "camd 不應因控制連線斷開而退出");
    cleanup(&dir);
}

/// AC②：錄影中 `kill -9` → 重啟 → 索引無假連續，已落盤段可播。
#[test]
fn kill9_then_restart_recovers_with_honest_gap_and_playable_segments() {
    let dir = unique_dir("ac2");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("camd.sock");
    let replay = Replay::start_real_h264(&dir);
    let mut first = Daemon::start(&dir, &socket, &replay.uri());
    let before = wait_segments(&socket, 1, TEST_BUDGET);
    let end0 = before["recording"]["latest_end_ms"]
        .as_u64()
        .expect("已閉合段必須有 latest_end_ms");
    first.child.kill().unwrap();
    let _ = first.child.wait();
    drop(first);

    // 空窗：沒有 camd 在錄，但假相機的時間照樣前進（和真相機一致）。
    std::thread::sleep(Duration::from_secs(3));
    let mut second = Daemon::start(&dir, &socket, &replay.uri());

    // 等到重啟後真的錄出一段新的（latest_end 前進 > 2 秒），不是只等到復原的舊列。
    let deadline = Instant::now() + TEST_BUDGET;
    let mut after = Value::Null;
    while Instant::now() < deadline {
        after = status(&socket);
        let end = after["recording"]["latest_end_ms"].as_u64().unwrap_or(0);
        if end > end0 + 2_000 {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let end1 = after["recording"]["latest_end_ms"].as_u64().unwrap_or(0);
    assert!(
        end1 > end0 + 2_000,
        "重啟後應錄出新段：before latest_end={end0}，after={end1}"
    );

    let index = SegmentIndex::open(RingConfig::new(&dir).index_path()).unwrap();
    let rows = index.rows().unwrap();
    assert!(rows.len() >= 2, "至少要有兩段：{rows:?}");
    let pair = rows
        .windows(2)
        .find(|pair| pair[1].t_start > end0 && pair[1].t_start > pair[0].t_end)
        .unwrap_or_else(|| panic!("重啟後應有空窗（gap）造成的時間差：{rows:?}"));
    let (prev, next) = (&pair[0], &pair[1]);
    let gap = next.t_start - prev.t_end;
    assert_eq!(
        next.gap_ms,
        Some(gap),
        "重啟後第一段的 gap_ms 必須等於與前一段的實際時間差（gap={gap}）"
    );
    assert!(gap >= 2_000, "空窗應包含停機的 3 秒：實得 {gap} ms");

    // 先停掉第二個 daemon 再讀檔：避免與 writer 的 flush 賽跑（CI 上讀到半個 slice）。
    second.stop();

    // 磁碟證據（不是拿索引自證）：段檔名稱的 t_start、掃描器的 t_start/t_end、
    // 首 block keyframe flag，以及 ffmpeg 全解碼。
    for (label, row) in [("prev", prev), ("next", next)] {
        let path = dir.join(&row.name);
        assert_eq!(
            camcore::recover::parse_t_start(&row.name),
            Some(row.t_start),
            "{label} 段檔名稱的 t_start 必須等於索引值（{}）",
            row.name
        );
        let info = mkv::scan(&path).unwrap();
        assert_eq!(info.t_start_ms, row.t_start, "{label} 掃描 t_start");
        // 索引的 t_end 是「收段邊界」（下一個 IDR 的時間），檔案最後一個 block 的 pts
        // 早一幀（25 fps＝40 ms）；修復段則是截到最後完整 Cluster，也最多差一個 cluster。
        // 兩者都不得差超過一幀，否則就是索引在說謊。
        assert!(
            info.t_end_ms <= row.t_end && row.t_end - info.t_end_ms <= 41,
            "{label} 掃描 t_end 應與索引相差不超過一幀（含 1 ms 邊界）：scan={}，index={}（{row:?}）",
            info.t_end_ms,
            row.t_end
        );
        assert_eq!(
            info.first_block_keyframe,
            Some(true),
            "{label} 段首必須是 keyframe（掃描器讀 flags）"
        );
        let errors = ffmpeg_decode_errors(&path);
        if !errors.is_empty() {
            // 解碼失敗時把段檔留在 target/ 下，CI 用 artifact 上傳，才有東西可追。
            let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
            let keep = target.join(format!("k15-fail-{label}.mkv"));
            let _ = std::fs::create_dir_all(&target);
            let _ = std::fs::copy(&path, &keep);
            panic!(
                "{label} 全解碼有錯誤：{errors:?}\n診斷：{:?}\n已保留：{}",
                describe_segment(&path),
                keep.display()
            );
        }
        assert_eq!(
            ffmpeg_count_frames(&path),
            info.blocks as u64,
            "{label} 解出的幀數必須等於段內 block 數（不得靜默丟幀）"
        );
    }
    let prev_info = mkv::scan(dir.join(&prev.name)).unwrap();
    let next_info = mkv::scan(dir.join(&next.name)).unwrap();
    let disk_gap = next_info.t_start_ms - prev_info.t_end_ms;
    assert!(
        disk_gap.abs_diff(gap) <= 41,
        "磁碟上的時間差必須與索引的 gap_ms 一致（磁碟 {disk_gap}、索引 {gap}；容許修復段截到最後完整 cluster 造成的 41 ms 差）"
    );
    cleanup(&dir);
}

/// AC④：`proto_version` 錯誤回結構化 `PROTO_VERSION`，同一條連線仍可繼續服務。
#[test]
fn wrong_proto_version_is_structured_and_connection_survives() {
    let dir = unique_dir("ac4");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("camd.sock");
    let mut daemon = Daemon::start(&dir, &socket, "rtsp://127.0.0.1:1/test.264");

    let mut client = Client::connect(&socket).unwrap();
    let response = client
        .call_with_version(99, "status", serde_json::json!({}))
        .unwrap();
    let error = response.error.expect("版本不符必須回 error");
    assert_eq!(error.code.as_str(), "PROTO_VERSION", "錯誤碼：{error:?}");
    assert_eq!(
        error.data,
        Some(serde_json::json!({ "expected": 1, "got": 99 }))
    );

    // 同一條連線繼續送正常請求。
    let ok = client.call("status", serde_json::json!({})).unwrap();
    let value = camipc::client::into_result(ok).expect("版本錯誤後同一條連線應能正常 status");
    assert_eq!(value["proto_version"], serde_json::json!(1));
    assert!(daemon.alive(), "camd 不應因版本錯誤退出");
    cleanup(&dir);
}

/// AC⑤：無相機時 camd 不退出，status 看得到 Disconnected／WaitingForNic。
#[test]
fn no_camera_keeps_daemon_alive_and_reports_disconnected() {
    let dir = unique_dir("ac5");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("camd.sock");
    // 127.0.0.1:1 幾乎必然 connection refused。
    let mut daemon = Daemon::start(&dir, &socket, "rtsp://127.0.0.1:1/test.264");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut state = String::new();
    while Instant::now() < deadline {
        let value = status(&socket);
        state = value["camera"]["state"].as_str().unwrap_or("").to_string();
        if state == "Disconnected" || state == "WaitingForNic" {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        state == "Disconnected" || state == "WaitingForNic",
        "相機連不上時 status 應顯示 Disconnected／WaitingForNic，實得 {state:?}"
    );

    std::thread::sleep(Duration::from_secs(2));
    let value = status(&socket);
    assert_eq!(value["camd"]["pid"], serde_json::json!(daemon.pid()));
    assert!(daemon.alive(), "相機連不上時 camd 必須繼續存活（退避重試）");
    cleanup(&dir);
}

fn call(socket: &Path, method: &str, params: Value) -> Value {
    let mut client = Client::connect(socket).unwrap_or_else(|e| panic!("連 camd 失敗：{e}"));
    camipc::client::into_result(client.call(method, params).unwrap())
        .unwrap_or_else(|e| panic!("{method} 失敗：{e:?}"))
}

/// B2：`status.current_segment` 的 `segment_id` 不含副檔名，`name` 是磁碟上真的檔名。
#[test]
fn current_segment_id_and_name_match_the_closed_row() {
    let dir = unique_dir("curseg");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("camd.sock");
    let replay = Replay::start(&dir);
    let mut daemon = Daemon::start(&dir, &socket, &replay.uri());

    // 錄到第一段閉合、第二段正在寫，才會有 non-null current_segment。
    wait_segments(&socket, 1, TEST_BUDGET);
    let mut current = Value::Null;
    let deadline = Instant::now() + TEST_BUDGET;
    while Instant::now() < deadline {
        let status = status(&socket);
        let seg = status["recording"]["current_segment"].clone();
        if seg.is_object() {
            current = seg;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        current.is_object(),
        "錄影中 status.current_segment 必須有值：{current}"
    );
    let segment_id = current["segment_id"].as_str().expect("segment_id");
    let name = current["name"].as_str().expect("name");
    let t_start = current["t_start_ms"].as_u64().expect("t_start_ms");

    assert!(
        !segment_id.ends_with(".mkv") && !segment_id.ends_with(".part"),
        "segment_id 不得帶副檔名：{segment_id}"
    );
    assert_eq!(
        name,
        format!("{segment_id}.mkv.part"),
        "name 必須是磁碟上的真實檔名"
    );
    assert!(
        dir.join(name).is_file(),
        "status 回報的 name 必須真的存在：{}",
        dir.join(name).display()
    );

    // 等這一段閉合，索引裡的 id 必須與剛才的 segment_id 相同。
    let deadline = Instant::now() + TEST_BUDGET;
    let mut row = Value::Null;
    while Instant::now() < deadline {
        let listed = call(&socket, "list_segments", serde_json::json!({ "limit": 100 }));
        if let Some(found) = listed["segments"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["segment_id"] == segment_id))
        {
            row = found.clone();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        row.is_object(),
        "閉合後 list_segments 必須有同一個 segment_id：{segment_id}"
    );
    assert_eq!(row["t_start_ms"].as_u64(), Some(t_start), "同一段的 t_start_ms");
    assert_eq!(
        row["name"].as_str(),
        Some(format!("{segment_id}.mkv").as_str()),
        "閉合後 name 應是 .mkv（不含 .part）"
    );
    daemon.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

fn dir_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// B3：已存在的父目錄一個位元都不動；只有自己新建的目錄才是 0700。socket 一律 0600。
#[test]
fn socket_dir_permissions_leave_existing_parent_alone() {
    // 情境 1：父目錄已存在（0755，像 /tmp），不能被改成 0700。
    let dir = unique_dir("perm-existing");
    std::fs::create_dir_all(&dir).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let socket = dir.join("camd.sock");
    let mut daemon = Daemon::start(&dir, &socket, "rtsp://127.0.0.1:1/test.264");
    assert_eq!(dir_mode(&dir), 0o755, "已存在的目錄權限不得被改動");
    assert_eq!(file_mode(&socket), 0o600, "socket 必須是 0600");
    daemon.stop();
    let _ = std::fs::remove_dir_all(&dir);

    // 情境 2：父目錄不存在 → camd 自己建立，必須是 0700。
    let dir = unique_dir("perm-new");
    let nested = dir.join("nested");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = nested.join("camd.sock");
    let mut daemon = Daemon::start(&dir, &socket, "rtsp://127.0.0.1:1/test.264");
    assert_eq!(dir_mode(&nested), 0o700, "自己新建的目錄必須是 0700");
    assert_eq!(file_mode(&socket), 0o600, "socket 必須是 0600");
    daemon.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
