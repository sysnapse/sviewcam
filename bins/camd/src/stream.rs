//! 拉流 task：包住 `rtsp-core` 的 session 狀態機，把 NAL 餵進 `RingWriter`。
//!
//! 這裡刻意不退出 process：連不上相機、斷流、重連風暴都只改狀態並退避重試
//! （1 s→2 s→4 s→…→30 s，上限由 `ReconnectPolicy::max` 決定）。

use std::sync::Arc;
use std::time::Duration;

use rtsp_core::link::TcpLinkFactory;
use rtsp_core::sink::NalSink;
use rtsp_core::{run as run_session, Event, ReconnectPolicy, SessionConfig};

use crate::{CameraState, Shared};

/// 把 URI 拆成 host／port（port 預設 554）；路徑留在 `SessionConfig::uri`。
pub fn parse_rtsp_uri(uri: &str) -> Result<(String, u16), String> {
    let rest = uri
        .strip_prefix("rtsp://")
        .ok_or_else(|| format!("URI 必須以 rtsp:// 開頭：{uri}"))?;
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return Err(format!("URI 缺少主機：{uri}"));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port: u16 = port
                .parse()
                .map_err(|_| format!("URI 的埠不是合法數字：{port}"))?;
            Ok((host.to_string(), port))
        }
        _ => Ok((authority.to_string(), rtsp_core::DEFAULT_RTSP_PORT)),
    }
}

pub(crate) async fn run(uri: String, token: String, shared: Arc<Shared>) {
    // 整個 camd 生命週期共用一個時鐘：session 重連時 `at_ms` 才不會從 0 重來
    // （重來會讓索引時間倒退、段與段重疊）。
    let clock = rtsp_core::RealClock::new();
    loop {
        let (host, port) = match parse_rtsp_uri(&uri) {
            Ok(parsed) => parsed,
            Err(message) => {
                set_camera(&shared, |camera| {
                    camera.state = CameraState::Disconnected;
                    camera.last_error = Some(message);
                });
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };

        let mut cfg = SessionConfig::new(uri.clone(), token.clone());
        cfg.duration = None;
        cfg.reconnect = ReconnectPolicy {
            max_attempts: 0,
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            factor: 2.0,
            ..ReconnectPolicy::default()
        };

        let mut factory = TcpLinkFactory::new(host);
        factory.rtsp_port = port;

        let mut sink = RecordingSink {
            shared: Arc::clone(&shared),
            restart_prev_end: None,
            restart_gap_reported: false,
        };
        let mut on_event = |event: Event| apply_event(&shared, event);
        let _report = run_session(&factory, &cfg, &clock, &mut sink, &mut on_event).await;

        set_camera(&shared, |camera| {
            camera.state = CameraState::Disconnected;
            camera.last_error = Some("串流停止，準備重試".to_string());
        });
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

fn apply_event(shared: &Shared, event: Event) {
    match event {
        Event::Connected { .. } | Event::Playing { .. } => set_camera(shared, |camera| {
            camera.state = CameraState::Connected;
            camera.last_error = None;
        }),
        Event::Reconnected { .. } => set_camera(shared, |camera| {
            camera.state = CameraState::Connected;
            camera.reconnects = camera.reconnects.saturating_add(1);
            camera.last_error = None;
        }),
        Event::Rst { reason, .. } => set_camera(shared, |camera| {
            camera.state = CameraState::Disconnected;
            camera.last_error = Some(format!("斷流：{reason}"));
        }),
        Event::ConnectFailed { attempt, error } => {
            let waiting_for_nic =
                error.contains("unreachable") || error.contains("Network is unreachable");
            set_camera(shared, |camera| {
                camera.state = if waiting_for_nic {
                    CameraState::WaitingForNic
                } else {
                    CameraState::Disconnected
                };
                camera.last_error = Some(format!("連線失敗 #{attempt}：{error}"));
            });
        }
        Event::Connecting { .. } => {}
        Event::Finished { reason } => set_camera(shared, |camera| {
            camera.state = CameraState::Disconnected;
            camera.last_error = Some(format!("結束：{reason}"));
        }),
    }
}

fn set_camera(shared: &Shared, update: impl FnOnce(&mut crate::CameraInfo)) {
    if let Ok(mut camera) = shared.camera.lock() {
        update(&mut camera);
    }
}

/// 把 NAL 餵給錄影 writer，順便更新「最後一幀」與相機狀態。
struct RecordingSink {
    shared: Arc<Shared>,
    /// 重啟前索引的最後時間；在第一個 IDR 時用來回報空窗。
    restart_prev_end: Option<u64>,
    restart_gap_reported: bool,
}

impl NalSink for RecordingSink {
    fn nal(&mut self, nal: &[u8]) {
        if let Ok(mut ring) = self.shared.ring.lock() {
            ring.nal(nal);
        }
    }

    fn nal_at(&mut self, nal: &[u8], at_ms: u64) {
        // host 時鐘 → 跨行程單調的串流時間軸；重啟後第一次映射會回報空窗。
        let (at_ms, gap) = {
            let Ok(mut timeline) = self.shared.timeline.lock() else {
                return;
            };
            timeline.map(at_ms)
        };
        if let Some((_elapsed, last_at_ms)) = gap {
            self.restart_prev_end = Some(last_at_ms);
        }
        let keyframe = nal.first().map(|b| b & 0x1f) == Some(5);
        if let Ok(mut ring) = self.shared.ring.lock() {
            // 空窗要等**第一個 IDR** 才算得準：串流從 P 幀續流時，ring 會等 IDR 才開段，
            // 那段等待也是真實空窗的一部分（不能只報 wall 時差 3 秒、磁碟卻是 5 秒）。
            if keyframe && !self.restart_gap_reported {
                if let Some(prev_end) = self.restart_prev_end.take() {
                    let gap_ms = at_ms.saturating_sub(prev_end);
                    if gap_ms > 0 {
                        let reason = format!(
                            "camd 重啟前的空窗（上次寫入 {prev_end} ms，這次等到 IDR 才開段）"
                        );
                        ring.gap(at_ms, gap_ms, &reason);
                    }
                    self.restart_gap_reported = true;
                }
            }
            ring.nal_at(nal, at_ms);
        }
    }

    fn gap(&mut self, at_ms: u64, gap_ms: u64, reason: &str) {
        let at_ms = match self.shared.timeline.lock() {
            Ok(mut timeline) => timeline.map(at_ms).0,
            Err(_) => return,
        };
        if let Ok(mut ring) = self.shared.ring.lock() {
            ring.gap(at_ms, gap_ms, reason);
        }
        set_camera(&self.shared, |camera| {
            camera.state = CameraState::Disconnected;
            camera.last_error = Some(format!("空窗 {gap_ms} ms：{reason}"));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_port_and_default() {
        assert_eq!(
            parse_rtsp_uri("rtsp://192.168.1.1:554/test.264").unwrap(),
            ("192.168.1.1".to_string(), 554)
        );
        assert_eq!(
            parse_rtsp_uri("rtsp://127.0.0.1:1/test.264").unwrap(),
            ("127.0.0.1".to_string(), 1)
        );
        assert_eq!(
            parse_rtsp_uri("rtsp://camera/test.264").unwrap(),
            ("camera".to_string(), 554)
        );
        assert!(parse_rtsp_uri("http://camera").is_err());
    }
}
