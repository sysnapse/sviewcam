//! 重連狀態機：連線 → 握手 → 收 RTP → 斷流（相機約 60–70 秒 RST）→ 退避 → 重連，直到
//! 指定時長用完或重試耗盡。所有時間以 host 單調時鐘為準；相機時鐘停於 1970，一律不用。

use std::time::Duration;

use serde::Serialize;

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::link::{Link, LinkFactory};
use crate::rtp::{self, Depacketizer};
use crate::rtsp;
use crate::sdp::{self, SdpInfo};
use crate::sink::NalSink;

#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    pub max_attempts: u32,
    pub initial: Duration,
    pub max: Duration,
    pub factor: f64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 50,
            initial: Duration::from_millis(500),
            max: Duration::from_secs(5),
            factor: 1.5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// 例如 `rtsp://192.168.1.1:554/test.264`（PLAY 會自動補結尾斜線）。
    pub uri: String,
    /// SViewWifi 權杖；只來自使用者提供的環境變數，不落盤、不入報告。
    pub token: String,
    /// `None` 表示一直跑到重試耗盡。
    pub duration: Option<Duration>,
    pub reconnect: ReconnectPolicy,
    /// `None` 表示不發 UDP 13000 心跳。
    pub heartbeat: Option<Duration>,
    /// 收包靜默多久視為斷流。
    pub recv_timeout: Duration,
}

impl SessionConfig {
    pub fn new(uri: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            token: token.into(),
            duration: Some(Duration::from_secs(30)),
            reconnect: ReconnectPolicy::default(),
            heartbeat: Some(Duration::from_secs(2)),
            recv_timeout: Duration::from_secs(5),
        }
    }

    /// 嗅探到的 MediaMTX/SDP token 與參數不同步時，這裡不嘗試修正。
    pub fn with_duration(mut self, d: Duration) -> Self {
        self.duration = Some(d);
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SeqGap {
    pub from_seq: u16,
    pub to_seq: u16,
    pub lost: u32,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamGap {
    /// 相對本次運行開始的毫秒。
    pub at_ms: u64,
    pub ms: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub uri: String,
    pub duration_ms: u64,
    /// TCP 連線並完成 OPTIONS→PLAY 的次數。
    pub connects: u32,
    /// `connects - 1`：斷流後成功續流的次數。
    pub reconnects: u32,
    pub connect_failures: u32,
    /// 觀察到的斷流次數（RST／靜默）。
    pub disconnects: u32,
    pub rtp_packets: u64,
    pub rtp_bytes: u64,
    pub lost_packets: u64,
    pub nals: u64,
    pub keyframes: u64,
    pub incomplete_frames: u32,
    pub seq_gaps: Vec<SeqGap>,
    pub gaps: Vec<StreamGap>,
    pub max_gap_ms: u64,
    pub bitrate_bps: f64,
    pub finished: String,
    pub errors: Vec<String>,
    pub sprop_sets: usize,
}

impl Report {
    fn new(uri: &str) -> Self {
        Self {
            uri: uri.to_string(),
            duration_ms: 0,
            connects: 0,
            reconnects: 0,
            connect_failures: 0,
            disconnects: 0,
            rtp_packets: 0,
            rtp_bytes: 0,
            lost_packets: 0,
            nals: 0,
            keyframes: 0,
            incomplete_frames: 0,
            seq_gaps: Vec::new(),
            gaps: Vec::new(),
            max_gap_ms: 0,
            bitrate_bps: 0.0,
            finished: String::new(),
            errors: Vec::new(),
            sprop_sets: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Connecting { attempt: u32 },
    Connected { rtp_port: u16 },
    Playing { session: String, sprop_sets: usize },
    Rst { at_ms: u64, reason: String },
    Reconnected { after_ms: u64, gap_ms: u64 },
    ConnectFailed { attempt: u32, error: String },
    Finished { reason: String },
}

fn next_backoff(current: Duration, policy: &ReconnectPolicy) -> Duration {
    let next = current.mul_f64(policy.factor);
    if next > policy.max {
        policy.max
    } else {
        next
    }
}

async fn handshake(link: &mut dyn Link, cfg: &SessionConfig) -> Result<(SdpInfo, String)> {
    let uri = cfg.uri.as_str();
    let mut cseq = 0u32;
    let mut next = || {
        cseq += 1;
        cseq
    };

    link.send(&rtsp::options(uri, next())).await?;
    let r = link.recv_response().await?;
    if !r.is_ok() {
        return Err(Error::Status { status: r.status, body: r.body.clone() });
    }

    link.send(&rtsp::describe(uri, next(), &cfg.token)).await?;
    let r = link.recv_response().await?;
    if !r.is_ok() {
        return Err(Error::Status { status: r.status, body: r.body.clone() });
    }
    let info = sdp::parse(&r.body);

    let port = link.client_rtp_port();
    link.send(&rtsp::setup(uri, next(), port)).await?;
    let r = link.recv_response().await?;
    if !r.is_ok() {
        return Err(Error::Status { status: r.status, body: r.body.clone() });
    }
    let session = r
        .session()
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .ok_or_else(|| Error::Protocol("SETUP response without Session header".into()))?;

    link.send(&rtsp::play(uri, next(), &session)).await?;
    let r = link.recv_response().await?;
    if !r.is_ok() {
        return Err(Error::Status { status: r.status, body: r.body.clone() });
    }
    Ok((info, session))
}

/// 跑一次完整會話（含所有重連），回傳統計。
#[allow(unused_assignments)]
pub async fn run(
    factory: &dyn LinkFactory,
    cfg: &SessionConfig,
    clock: &dyn Clock,
    sink: &mut dyn NalSink,
    on_event: &mut dyn FnMut(Event),
) -> Report {
    let t0 = clock.now_ms();
    let mut rep = Report::new(&cfg.uri);
    let mut depack = Depacketizer::new();
    let mut attempt: u32 = 0;
    let mut backoff = cfg.reconnect.initial;
    let mut gap_open_at: Option<u64> = None;
    let mut gap_reason = String::new();
    let mut wrote_sprop = false;
    let mut finished = "duration elapsed".to_string();

    'outer: loop {
        if let Some(d) = cfg.duration {
            if clock.now_ms().saturating_sub(t0) >= d.as_millis() as u64 {
                break;
            }
        }
        if cfg.reconnect.max_attempts > 0
            && rep.connect_failures + rep.disconnects >= cfg.reconnect.max_attempts
        {
            finished = format!("reconnect budget exhausted ({} attempts)", cfg.reconnect.max_attempts);
            break;
        }

        attempt += 1;
        on_event(Event::Connecting { attempt });

        let connected = factory.connect().await;
        let (mut link, rtp_port) = match connected {
            Ok(v) => v,
            Err(Error::EndOfInput) => {
                finished = "input exhausted".into();
                break 'outer;
            }
            Err(e) => {
                rep.connect_failures += 1;
                rep.errors.push(format!("connect: {e}"));
                on_event(Event::ConnectFailed { attempt, error: e.to_string() });
                clock.sleep(backoff).await;
                backoff = next_backoff(backoff, &cfg.reconnect);
                continue;
            }
        };
        on_event(Event::Connected { rtp_port });

        let (info, session_id) = match handshake(link.as_mut(), cfg).await {
            Ok(v) => v,
            Err(Error::EndOfInput) => {
                finished = "input exhausted".into();
                let _ = link.close().await;
                break 'outer;
            }
            Err(e) => {
                rep.connect_failures += 1;
                rep.errors.push(format!("handshake: {e}"));
                on_event(Event::ConnectFailed { attempt, error: e.to_string() });
                let _ = link.close().await;
                clock.sleep(backoff).await;
                backoff = next_backoff(backoff, &cfg.reconnect);
                continue;
            }
        };

        rep.connects += 1;
        if rep.connects > 1 {
            rep.reconnects += 1;
        }
        if rep.sprop_sets == 0 {
            rep.sprop_sets = info.sprop.len();
        }
        // 相機的 sprop 已含起始碼：寫入前剝掉，避免出現 `00 00 00 01 00 00 00 01`。
        if !wrote_sprop && !info.sprop.is_empty() {
            for ps in info.parameter_sets() {
                sink.nal(ps);
            }
            wrote_sprop = true;
        }
        on_event(Event::Playing {
            session: session_id.clone(),
            sprop_sets: info.sprop.len(),
        });

        backoff = cfg.reconnect.initial;
        if let Some(at) = gap_open_at.take() {
            let now = clock.now_ms();
            let ms = now.saturating_sub(at);
            rep.gaps.push(StreamGap { at_ms: at.saturating_sub(t0), ms, reason: gap_reason.clone() });
            rep.max_gap_ms = rep.max_gap_ms.max(ms);
            on_event(Event::Reconnected { after_ms: now.saturating_sub(t0), gap_ms: ms });
        }

        let mut last_seq: Option<u16> = None;
        let mut last_hb = clock.now_ms();
        let mut disconnect_reason: Option<String> = None;

        loop {
            if let Some(d) = cfg.duration {
                if clock.now_ms().saturating_sub(t0) >= d.as_millis() as u64 {
                    let _ = link.send(&rtsp::teardown(&cfg.uri, 99, &session_id)).await;
                    let _ = link.close().await;
                    break 'outer;
                }
            }
            if let Some(iv) = cfg.heartbeat {
                if clock.now_ms().saturating_sub(last_hb) >= iv.as_millis() as u64 {
                    let _ = link.heartbeat().await;
                    last_hb = clock.now_ms();
                }
            }

            match tokio::time::timeout(cfg.recv_timeout, link.recv_rtp()).await {
                Ok(Ok(datagram)) => {
                    let len = datagram.len() as u64;
                    match rtp::parse(&datagram) {
                        Ok(p) => {
                            rep.rtp_packets += 1;
                            rep.rtp_bytes += len;
                            if let Some(prev) = last_seq {
                                let expected = prev.wrapping_add(1);
                                if p.seq != expected {
                                    let ahead = p.seq.wrapping_sub(prev) as i32;
                                    if ahead > 1 && ahead < 0x4000 {
                                        let lost = (ahead - 1) as u32;
                                        rep.lost_packets += lost as u64;
                                        rep.seq_gaps.push(SeqGap {
                                            from_seq: prev,
                                            to_seq: p.seq,
                                            lost,
                                            at_ms: clock.now_ms().saturating_sub(t0),
                                        });
                                    }
                                }
                            }
                            last_seq = Some(p.seq);
                            if p.payload_type == 96 {
                                match depack.push(p.payload, p.timestamp) {
                                    Ok(nals) => {
                                        for nal in nals {
                                            if nal.is_empty() {
                                                continue;
                                            }
                                            rep.nals += 1;
                                            if rtp::is_keyframe(&nal) {
                                                rep.keyframes += 1;
                                            }
                                            sink.nal(&nal);
                                        }
                                    }
                                    Err(e) => rep.errors.push(format!("depacket: {e}")),
                                }
                            }
                        }
                        Err(e) => rep.errors.push(format!("rtp: {e}")),
                    }
                }
                Ok(Err(Error::EndOfInput)) => {
                    finished = "input exhausted".into();
                    break 'outer;
                }
                Ok(Err(e)) => {
                    disconnect_reason = Some(e.to_string());
                    break;
                }
                Err(_) => {
                    disconnect_reason = Some(format!(
                        "no rtp for {} ms",
                        cfg.recv_timeout.as_millis()
                    ));
                    break;
                }
            }
        }

        rep.disconnects += 1;
        let now = clock.now_ms();
        let reason = disconnect_reason.unwrap_or_else(|| "unknown".to_string());
        on_event(Event::Rst { at_ms: now.saturating_sub(t0), reason: reason.clone() });
        gap_open_at = Some(now);
        gap_reason = reason;
        let _ = link.close().await;
        clock.sleep(backoff).await;
        backoff = next_backoff(backoff, &cfg.reconnect);
    }

    rep.incomplete_frames = depack.incomplete;
    rep.duration_ms = clock.now_ms().saturating_sub(t0);
    if rep.duration_ms > 0 {
        rep.bitrate_bps = (rep.rtp_bytes as f64 * 8.0) / (rep.duration_ms as f64 / 1000.0);
    }
    rep.finished = finished.clone();
    on_event(Event::Finished { reason: finished });
    rep
}
