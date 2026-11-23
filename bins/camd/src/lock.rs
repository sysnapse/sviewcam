//! 單實例鎖：socket 同目錄的 `camd.lock`，內容是 pid。
//!
//! 第二個 camd 必須非零 exit 並說清楚「已在執行（pid N）」；pid 不存在（上一個被
//! `kill -9` 或機器重開）視為 stale，接手。`Drop` 在正常結束時刪鎖；SIGKILL 留下
//! 的鎖由下一支用 stale 規則接手。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 鎖檔名（與 socket 同目錄）。
pub const LOCK_FILE_NAME: &str = "camd.lock";

/// 取鎖結果。
#[derive(Debug)]
pub enum LockError {
    /// 已有活著的 camd 持有鎖。
    AlreadyRunning {
        pid: u32,
        lock_path: PathBuf,
    },
    Io(std::io::Error),
}

/// 持有中的鎖；正常 drop 時刪檔。
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
    pid: u32,
}

impl Lock {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 取得鎖：`create_new` 寫 pid；已存在時讀 pid 判斷 stale（只接手一次）。
pub fn acquire(lock_path: &Path) -> Result<Lock, LockError> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(LockError::Io)?;
    }
    let pid = std::process::id();
    match try_create(lock_path, pid) {
        Ok(()) => Ok(Lock {
            path: lock_path.to_path_buf(),
            pid,
        }),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let holder = fs::read_to_string(lock_path)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok());
            match holder {
                Some(other) if pid_alive(other) => Err(LockError::AlreadyRunning {
                    pid: other,
                    lock_path: lock_path.to_path_buf(),
                }),
                // stale（pid 不存在或鎖檔壞掉）：刪掉再搶一次。
                _ => {
                    fs::remove_file(lock_path).map_err(LockError::Io)?;
                    try_create(lock_path, pid).map_err(LockError::Io)?;
                    Ok(Lock {
                        path: lock_path.to_path_buf(),
                        pid,
                    })
                }
            }
        }
        Err(err) => Err(LockError::Io(err)),
    }
}

fn try_create(path: &Path, pid: u32) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    writeln!(file, "{pid}")
}

/// pid 是否還活著。Linux 用 `/proc`；其他平台沒有 `/proc` 時保守地當成活著
/// （寧可拒絕啟動，也不要兩個 camd 搶同一個目錄）。
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let proc_root = Path::new("/proc");
    if proc_root.is_dir() {
        proc_root.join(pid.to_string()).is_dir()
    } else {
        true
    }
}
