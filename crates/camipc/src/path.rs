//! socket 路徑解析：`--socket` > `$SVCAM_SOCKET` > `$XDG_RUNTIME_DIR/sviewcam/camd.sock`
//! > `$HOME/.local/state/sviewcam/camd.sock`。camd 與 camctl 共用同一份規則。

use std::path::{Path, PathBuf};

/// 解析控制 socket 路徑；都解析不出來時回可讀的錯誤字串（呼叫端轉成 exit 2）。
pub fn resolve_socket(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Ok(value) = std::env::var("SVCAM_SOCKET") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return Ok(Path::new(&dir).join("sviewcam").join("camd.sock"));
        }
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => Ok(Path::new(&home)
            .join(".local")
            .join("state")
            .join("sviewcam")
            .join("camd.sock")),
        _ => Err("無法決定 socket 路徑：HOME 未設定；請設 --socket 或 SVCAM_SOCKET".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 環境變數是 process 全域的，這幾條只能序列跑；用一把鎖圍住整個序列。
    #[test]
    fn precedence_is_explicit_then_env_then_xdg_then_home() {
        let _guard = GUARD.lock().unwrap();
        let saved = [
            std::env::var("SVCAM_SOCKET").ok(),
            std::env::var("XDG_RUNTIME_DIR").ok(),
            std::env::var("HOME").ok(),
        ];
        std::env::remove_var("SVCAM_SOCKET");
        std::env::remove_var("XDG_RUNTIME_DIR");
        std::env::set_var("HOME", "/home/tester");

        let explicit = Path::new("/tmp/explicit.sock");
        assert_eq!(
            resolve_socket(Some(explicit)).unwrap(),
            explicit.to_path_buf()
        );

        std::env::set_var("SVCAM_SOCKET", "/tmp/env.sock");
        assert_eq!(
            resolve_socket(None).unwrap(),
            PathBuf::from("/tmp/env.sock")
        );

        std::env::remove_var("SVCAM_SOCKET");
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        assert_eq!(
            resolve_socket(None).unwrap(),
            PathBuf::from("/run/user/1000/sviewcam/camd.sock")
        );

        std::env::remove_var("XDG_RUNTIME_DIR");
        assert_eq!(
            resolve_socket(None).unwrap(),
            PathBuf::from("/home/tester/.local/state/sviewcam/camd.sock")
        );

        match saved {
            [svcam, xdg, home] => {
                restore("SVCAM_SOCKET", svcam);
                restore("XDG_RUNTIME_DIR", xdg);
                restore("HOME", home);
            }
        }
    }

    fn restore(key: &str, value: Option<String>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
