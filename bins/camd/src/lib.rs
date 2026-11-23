//! `camd`：SViewCam 常駐服務（M3 切片 1）。
//!
//! 啟動順序固定：取單實例鎖 → [`camcore::recover`] 復原掃描（`RingWriter::open`
//! 內含）→ bind 控制 socket → 開始拉流。控制 socket 只講 [`camipc`] 的協議 v1，
//! 方法只有 `status`／`list_segments`／`ping`。
//!
//! 時間基準：索引的 `t_start`／`t_end`（串流毫秒）是唯一權威；`uptime_ms` 與
//! `last_frame_uptime_ms` 是 host 時間，兩者不得互換。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use camcore::{RingConfig, RingWriter};
use camipc::{ErrorCode, ErrorObj, Method, RpcHandler};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::net::UnixListener;

pub mod lock;
mod stream;
mod timeline;

pub use lock::{Lock, LockError};
pub use stream::parse_rtsp_uri;

/// 啟動參數。
#[derive(Debug, Clone)]
pub struct Options {
    /// 錄影資料目錄（段檔與 `index.sqlite`）。
    pub dir: PathBuf,
    /// 控制 socket；`None` 走 `camipc::resolve_socket` 的環境規則。
    pub socket: Option<PathBuf>,
    /// RTSP URI（相機）。
    pub uri: String,
    /// SViewWifi 權杖；只從環境變數來，不落盤、不進日誌。
    pub token: String,
    /// 保留窗分鐘數（`None` 用預設）。
    pub retention_minutes: Option<u64>,
    /// 寫進 MKV 的視訊寬。
    pub width: u32,
    /// 寫進 MKV 的視訊高。
    pub height: u32,
}

/// SIGTERM／SIGINT 的收工旗標（信號處理器只寫這個，然後主迴圈收工）。
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_shutdown_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// 啟動失敗：`code` 是 camd 的 exit code（1＝已在執行，2＝參數／環境／復原錯誤）。
#[derive(Debug)]
pub struct StartupError {
    pub code: u8,
    pub message: String,
}

impl StartupError {
    pub fn already_running(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }
}

/// 相機連線狀態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraState {
    Starting,
    Connected,
    Disconnected,
    WaitingForNic,
}

impl CameraState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Connected => "Connected",
            Self::Disconnected => "Disconnected",
            Self::WaitingForNic => "WaitingForNic",
        }
    }
}

/// 相機與錄影的共用狀態（控制 socket 的 handler 與拉流 task 共用）。
pub struct Shared {
    pub pid: u32,
    pub started: Instant,
    pub socket_path: PathBuf,
    pub data_dir: PathBuf,
    pub camera: Mutex<CameraInfo>,
    /// 錄影 writer 與控制端共用一把鎖：status 讀索引時不另開 SQLite 連線，
    /// 避免兩個連線對同一個索引同時讀寫。
    pub ring: Mutex<RingWriter>,
    /// 串流時間軸（跨行程延續；`kill -9` 後重啟也不得讓時間倒退）。
    pub timeline: Mutex<timeline::TimelineClock>,
}

/// 相機狀態快照。
pub struct CameraInfo {
    pub state: CameraState,
    /// 最後一個 NAL 抵達時的 host uptime（毫秒）；`None`＝本次啟動還沒收過幀。
    pub last_frame_uptime_ms: Option<u64>,
    pub reconnects: u32,
    pub last_error: Option<String>,
}

impl Shared {
    pub fn new(
        socket_path: PathBuf,
        data_dir: PathBuf,
        ring: RingWriter,
        timeline: timeline::TimelineClock,
    ) -> Self {
        Self {
            pid: std::process::id(),
            started: Instant::now(),
            socket_path,
            data_dir,
            camera: Mutex::new(CameraInfo {
                state: CameraState::Starting,
                last_frame_uptime_ms: None,
                reconnects: 0,
                last_error: None,
            }),
            ring: Mutex::new(ring),
            timeline: Mutex::new(timeline),
        }
    }

    pub fn uptime_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// 控制 socket 的 handler。
struct Daemon {
    shared: Arc<Shared>,
}

impl RpcHandler for Daemon {
    fn handle(&self, method: Method, params: &Value) -> Result<Value, ErrorObj> {
        match method {
            Method::Ping => Ok(json!({
                "pong": true,
                "uptime_ms": self.shared.uptime_ms(),
                "proto_version": camipc::PROTO_VERSION,
            })),
            Method::Status => self.status(),
            Method::ListSegments => self.list_segments(params),
        }
    }
}

impl Daemon {
    fn status(&self) -> Result<Value, ErrorObj> {
        let camera = self
            .shared
            .camera
            .lock()
            .map_err(|_| internal("相機狀態鎖損壞"))?;
        let mut ring = self
            .shared
            .ring
            .lock()
            .map_err(|_| internal("錄影 writer 鎖損壞"))?;
        let index = ring.index();
        let segments = index
            .count()
            .map_err(|e| internal(&format!("讀索引失敗：{e}")))?;
        let total_bytes = index
            .total_bytes()
            .map_err(|e| internal(&format!("讀索引 bytes 失敗：{e}")))?;
        let latest_end = index
            .latest_end()
            .map_err(|e| internal(&format!("讀索引最新段失敗：{e}")))?;
        let last_error = ring.take_error().map(|e| e.to_string());

        Ok(json!({
            "camd": {
                "version": env!("CARGO_PKG_VERSION"),
                "pid": self.shared.pid,
                "uptime_ms": self.shared.uptime_ms(),
                "socket": self.shared.socket_path.to_string_lossy(),
                "data_dir": self.shared.data_dir.to_string_lossy(),
            },
            "proto_version": camipc::PROTO_VERSION,
            "camera": {
                "state": camera.state.as_str(),
                "last_frame_uptime_ms": camera.last_frame_uptime_ms,
                "reconnects": camera.reconnects,
                "next_retry_ms": Value::Null,
                "last_error": camera.last_error,
            },
            "recording": {
                "current_segment": current_segment(&self.shared.data_dir),
                "segments": segments,
                "total_bytes": total_bytes,
                "latest_end_ms": latest_end,
                "last_error": last_error,
            },
            // 切片 1 明確回 null：k18 的 fps 吃 RTP 90 kHz timestamp，sink 目前只有
            // 串流毫秒，硬算會得到另一套定義的數字。等切片 2 把 RTP timestamp 帶進
            // sink 再填。
            "fps": Value::Null,
        }))
    }

    fn list_segments(&self, params: &Value) -> Result<Value, ErrorObj> {
        if !params.is_null() && !params.is_object() {
            return Err(bad_params("params 必須是物件"));
        }
        let from = optional_u64(params, "from")?;
        let to = optional_u64(params, "to")?;
        if let (Some(from), Some(to)) = (from, to) {
            if from > to {
                return Err(bad_params(format!("from（{from}）不得大於 to（{to}）")));
            }
        }
        let limit = match params.get("limit") {
            None | Some(Value::Null) => 100usize,
            Some(value) => {
                let raw = value
                    .as_u64()
                    .ok_or_else(|| bad_params("limit 必須是正整數"))?;
                if raw == 0 || raw > 1000 {
                    return Err(bad_params("limit 必須介於 1 與 1000 之間"));
                }
                raw as usize
            }
        };

        let ring = self
            .shared
            .ring
            .lock()
            .map_err(|_| internal("錄影 writer 鎖損壞"))?;
        let rows = ring
            .index()
            .rows()
            .map_err(|e| internal(&format!("讀索引失敗：{e}")))?;
        drop(ring);

        let filtered: Vec<_> = rows
            .into_iter()
            .filter(|row| from.map_or(true, |from| row.t_end >= from))
            .filter(|row| to.map_or(true, |to| row.t_start <= to))
            .collect();
        let total = filtered.len();
        let start = total.saturating_sub(limit);
        let segments: Vec<Value> = filtered[start..]
            .iter()
            .map(|row| {
                json!({
                    "segment_id": row.segment_id,
                    "name": row.name,
                    "t_start_ms": row.t_start,
                    "t_end_ms": row.t_end,
                    "bytes": row.bytes,
                    "first_block_keyframe": row.first_block_keyframe,
                    "gap_ms": row.gap_ms,
                })
            })
            .collect();

        Ok(json!({ "segments": segments, "count": segments.len(), "total": total }))
    }
}

/// 掃資料目錄裡尚未收尾的段檔（`.mkv.part`），給 status 的 `current_segment`。
fn current_segment(dir: &std::path::Path) -> Value {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Value::Null;
    };
    let mut newest: Option<(String, String, u64)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".mkv.part") {
            continue;
        }
        // segment_id 一律是不含副檔名的 id（與 list_segments 的 rows 同一個值）；
        // name 才是磁碟上的真實檔名（`.mkv.part`）。
        let segment_id = name.trim_end_matches(".mkv.part").to_string();
        let t_start = camcore::recover::parse_t_start(&name).unwrap_or(0);
        if newest
            .as_ref()
            .map_or(true, |(_, _, best)| t_start >= *best)
        {
            newest = Some((segment_id, name, t_start));
        }
    }
    match newest {
        Some((segment_id, name_on_disk, t_start)) => json!({
            "segment_id": segment_id,
            "name": name_on_disk,
            "t_start_ms": t_start,
        }),
        None => Value::Null,
    }
}

fn optional_u64(params: &Value, key: &str) -> Result<Option<u64>, ErrorObj> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| bad_params(format!("{key} 必須是非負整數（串流毫秒）"))),
    }
}

fn bad_params(message: impl Into<String>) -> ErrorObj {
    ErrorObj::new(ErrorCode::BadParams, message)
}

fn internal(message: &str) -> ErrorObj {
    ErrorObj::new(ErrorCode::Internal, message.to_string())
}

/// 啟動服務；回傳時代表已正常收工（SIGTERM／SIGINT）。
pub async fn run(opts: Options) -> Result<(), StartupError> {
    let socket_path =
        camipc::resolve_socket(opts.socket.as_deref()).map_err(StartupError::config)?;

    if let Some(parent) = socket_path.parent() {
        // 只有「這次自己新建」的目錄才設 0700。已存在的父目錄（例如 `--socket /tmp/camd.sock`
        // 的 `/tmp`）一個位元都不動：改它的權限會影響整台機器上所有使用者。
        let existed = parent.exists();
        std::fs::create_dir_all(parent).map_err(|e| {
            StartupError::config(format!("建立 socket 目錄 {} 失敗：{e}", parent.display()))
        })?;
        if !existed {
            set_private_dir(parent)?;
        }
    }

    let lock_path = socket_path.with_file_name(lock::LOCK_FILE_NAME);
    let _lock = match lock::acquire(&lock_path) {
        Ok(lock) => lock,
        Err(LockError::AlreadyRunning { pid, lock_path }) => {
            return Err(StartupError::already_running(format!(
                "camd 已在執行（pid {pid}，socket {}，鎖 {}）",
                socket_path.display(),
                lock_path.display()
            )));
        }
        Err(LockError::Io(e)) => {
            return Err(StartupError::config(format!(
                "建立單實例鎖 {} 失敗：{e}",
                lock_path.display()
            )));
        }
    };

    let (cfg, warnings) =
        RingConfig::new(opts.dir.clone()).with_retention_minutes(opts.retention_minutes);
    for warning in &warnings {
        eprintln!("警告：{}", warning.message);
    }
    let cfg = RingConfig {
        width: opts.width,
        height: opts.height,
        ..cfg
    };

    // ② 先復原再 bind：socket 一出現就代表索引已經掃完。
    // 時間軸狀態在 recover 之前讀：上一輪（可能被 kill -9）的最後時間與 wall clock
    // 是重啟後唯一的空窗依據，不受本輪 recover 改寫索引影響。
    let previous_timeline = timeline::load(&opts.dir);
    let (ring, report) = RingWriter::open(cfg).map_err(|e| {
        StartupError::config(format!("啟動復原掃描失敗（{}）：{e}", opts.dir.display()))
    })?;
    eprintln!(
        "recover: rows={} repaired={} discarded={} adopted={} removed_orphans={} pruned={} index_rebuilt={}",
        report.rows,
        report.repaired.len(),
        report.discarded.len(),
        report.adopted.len(),
        report.removed_orphans.len(),
        report.pruned.len(),
        report.index_rebuilt
    );

    if socket_path.exists() {
        std::fs::remove_file(&socket_path).map_err(|e| {
            StartupError::config(format!(
                "移除殘留 socket {} 失敗：{e}",
                socket_path.display()
            ))
        })?;
    }
    let listener = UnixListener::bind(&socket_path).map_err(|e| {
        StartupError::config(format!("bind socket {} 失敗：{e}", socket_path.display()))
    })?;
    set_private_socket(&socket_path)?;
    eprintln!(
        "camd 已啟動：pid={} socket={} data={}",
        std::process::id(),
        socket_path.display(),
        opts.dir.display()
    );

    // 空窗的基準用索引自己記到的最後時間（recover 後的權威值），wall 時差只拿來
    // 估空窗長度；這樣新段 t_start 與 `gap_ms` 一定同源，不會「報 3172、磁碟 5011」。
    let index_end = ring.index().latest_end().ok().flatten();
    let timeline_previous = match (previous_timeline, index_end) {
        (Some(state), Some(end)) => Some(timeline::TimelineState {
            last_at_ms: end,
            wall_ms: state.wall_ms,
        }),
        _ => None,
    };
    let timeline = timeline::TimelineClock::new(opts.dir.clone(), timeline_previous);
    let shared = Arc::new(Shared::new(
        socket_path.clone(),
        opts.dir.clone(),
        ring,
        timeline,
    ));
    let handler: Arc<dyn RpcHandler> = Arc::new(Daemon {
        shared: Arc::clone(&shared),
    });

    // `rtsp_core::run` 的 future 帶 `&mut dyn FnMut(Event)`，不是 `Send`，所以拉流
    // 放在自己的執行緒＋current-thread runtime 跑；控制 socket 留在主 runtime。
    let stream_shared = Arc::clone(&shared);
    let stream_uri = opts.uri.clone();
    let stream_token = opts.token.clone();
    std::thread::Builder::new()
        .name("camd-stream".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    eprintln!("錯誤：拉流執行緒建立 runtime 失敗：{e}");
                    return;
                }
            };
            runtime.block_on(stream::run(stream_uri, stream_token, stream_shared));
        })
        .map_err(|e| StartupError::config(format!("建立拉流執行緒失敗：{e}")))?;

    install_shutdown_handlers()?;
    let shutdown = async {
        while !SHUTDOWN.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    tokio::select! {
        _ = camipc::server::serve(listener, handler) => {}
        _ = shutdown => {}
    }

    // 拉流執行緒會隨 process 結束一起收；先關當前段再刪 socket。
    if let Ok(mut ring) = shared.ring.lock() {
        let _ = ring.finish();
    }
    // 最後一次串流時間落盤：下一次啟動的空窗才有依據（`kill -9` 最多差 1 秒）。
    if let Ok(mut timeline) = shared.timeline.lock() {
        timeline.flush();
    }
    let _ = std::fs::remove_file(&socket_path);
    eprintln!("camd 已收工");
    Ok(())
}

/// 註冊 SIGTERM／SIGINT：只設旗標，實際收工在主迴圈（async 安全的做法）。
fn install_shutdown_handlers() -> Result<(), StartupError> {
    #[cfg(unix)]
    {
        let handler = on_shutdown_signal as extern "C" fn(libc::c_int);
        unsafe {
            if libc::signal(libc::SIGTERM, handler as libc::sighandler_t) == libc::SIG_ERR {
                return Err(StartupError::config("註冊 SIGTERM 失敗"));
            }
            if libc::signal(libc::SIGINT, handler as libc::sighandler_t) == libc::SIG_ERR {
                return Err(StartupError::config("註冊 SIGINT 失敗"));
            }
        }
    }
    Ok(())
}

/// socket 目錄 0700（只給同使用者）。
fn set_private_dir(path: &std::path::Path) -> Result<(), StartupError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            StartupError::config(format!(
                "設定 socket 目錄權限失敗（{}）：{e}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

/// socket 檔 0600。
fn set_private_socket(path: &std::path::Path) -> Result<(), StartupError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            StartupError::config(format!("設定 socket 權限失敗（{}）：{e}", path.display()))
        })?;
    }
    Ok(())
}
