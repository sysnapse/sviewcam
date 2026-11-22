//! 重連狀態機：連線 → 握手 → 收 RTP → 斷流（相機約 60–70 秒 RST）→ 退避 → 重連，直到
//! 指定時長用完或重試耗盡。所有時間以 host 單調時鐘為準；相機時鐘停於 1970，一律不用。

use std::collections::VecDeque;
use std::time::Duration;

use serde::Serialize;

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::fps::{FpsEstimator, FpsStats};
use crate::link::{Link, LinkFactory};
use crate::rtp::{self, Depacketizer};
use crate::rtsp;
use crate::sdp::{self, SdpInfo};
use crate::sink::NalSink;

/// 斷流／RST 的分類（protocol 文檔 §11.2）。退避按類別分開，不是單一退避：
/// 單一退避會令「即時拒絕」變成快速重試迴圈，也會令「定時回收」白等一次退避。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RstClass {
    /// 定時回收：距上次 PLAY 成功 >= `timed_recycle_after`。設計內行為，立即重連、退避歸零。
    TimedRecycle,
    /// 即時拒絕：距 PLAY < `immediate_reject_before` 且本會話從未收到 RTP。
    ImmediateReject,
    /// 中途 RST：其他情況，含時序未被解釋的觸發（例如觀測到的 ~31 s 短拉）。
    MidStream,
}

impl RstClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            RstClass::TimedRecycle => "timed_recycle",
            RstClass::ImmediateReject => "immediate_reject",
            RstClass::MidStream => "midstream",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    pub max_attempts: u32,
    pub initial: Duration,
    pub max: Duration,
    pub factor: f64,
    /// 定時回收門檻：距上次 PLAY 成功 >= 此值（§11.2：45 s）。
    pub timed_recycle_after: Duration,
    /// 即時拒絕門檻：距 PLAY < 此值且會話從未收到 RTP（§11.2：10 s）。
    pub immediate_reject_before: Duration,
    /// 即時拒絕退避階梯（§11.2：0.5→1→2→4 s），實際等待會再夾在 `max` 之下。
    pub immediate_ladder: Vec<Duration>,
    /// 連續即時拒絕上限：達到即停止並報錯（§11.2：5 次）。
    pub immediate_limit: u32,
    /// 中途 RST 起始退避（§11.2：1 s 起），成功收流後重置。
    pub midstream_initial: Duration,
    /// 重連風暴視窗（§11.2：10 分鐘）。
    pub storm_window: Duration,
    /// 視窗內重連次數上限，超過即停止並上報（§11.2：40 次）。
    pub storm_max_reconnects: u32,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 50,
            initial: Duration::from_millis(500),
            max: Duration::from_secs(10),
            factor: 2.0,
            timed_recycle_after: Duration::from_secs(45),
            immediate_reject_before: Duration::from_secs(10),
            immediate_ladder: vec![
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
            immediate_limit: 5,
            midstream_initial: Duration::from_secs(1),
            storm_window: Duration::from_secs(600),
            storm_max_reconnects: 40,
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
    /// 斷流後**成功續流**的重連次數：本會話至少收到一個 RTP 才算。
    /// 握手成功但零 RTP 的會話（相機 PLAY 後不送流）不算續流，計入 `rst_*` 分類與 `disconnects`。
    /// 舊語義為 `connects - 1`（握手成功即計），SVW-19 起改為與空窗收口同一事件。
    pub reconnects: u32,
    pub connect_failures: u32,
    /// 觀察到的斷流次數（RST／靜默）。
    pub disconnects: u32,
    pub rtp_packets: u64,
    /// 同一會話內收到的**向後**序號包數（亂序或重複）。RTP 走 UDP，亂序屬預期行為，
    /// 既不是缺口也不是丟包，單獨計數才不會混進 lost_packets。
    pub out_of_order: u64,
    pub rtp_bytes: u64,
    pub lost_packets: u64,
    pub nals: u64,
    pub keyframes: u64,
    pub incomplete_frames: u32,
    pub seq_gaps: Vec<SeqGap>,
    pub gaps: Vec<StreamGap>,
    pub max_gap_ms: u64,
    pub bitrate_bps: f64,
    /// 由 RTP timestamp 標定的真實幀率（BACKLOG-02）。`None`＝樣本不足或沒有視訊。
    pub fps: Option<FpsStats>,
    pub finished: String,
    pub errors: Vec<String>,
    pub sprop_sets: usize,
    /// 定時回收次數（§11.2）。
    pub rst_timed_recycle: u32,
    /// 即時拒絕次數（§11.2）。
    pub rst_immediate_reject: u32,
    /// 中途 RST 次數（§11.2）。
    pub rst_midstream: u32,
    /// 每次斷流後實際等待的退避（毫秒，按時序）。定時回收為 0。
    pub backoff_ms: Vec<u64>,
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
            out_of_order: 0,
            rtp_bytes: 0,
            lost_packets: 0,
            nals: 0,
            keyframes: 0,
            incomplete_frames: 0,
            seq_gaps: Vec::new(),
            gaps: Vec::new(),
            max_gap_ms: 0,
            bitrate_bps: 0.0,
            fps: None,
            finished: String::new(),
            errors: Vec::new(),
            sprop_sets: 0,
            rst_timed_recycle: 0,
            rst_immediate_reject: 0,
            rst_midstream: 0,
            backoff_ms: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Connecting { attempt: u32 },
    Connected { rtp_port: u16 },
    Playing { session: String, sprop_sets: usize },
    Rst { at_ms: u64, reason: String, class: RstClass },
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
    let mut fps = FpsEstimator::new();
    let mut attempt: u32 = 0;
    let mut backoff = cfg.reconnect.initial;
    let mut gap_open_at: Option<u64> = None;
    let mut gap_reason = String::new();
    let mut wrote_sprop = false;
    let mut finished = "duration elapsed".to_string();
    // §11.2 分類所需的狀態：上次 PLAY 成功的時刻、本會話收到的 RTP 數、連續即時拒絕次數。
    let mut last_play_at: u64 = t0;
    let mut session_rtp: u64 = 0;
    let mut immediate_streak: u32 = 0;
    let mut midstream_backoff = cfg.reconnect.midstream_initial;
    let mut storm_times: VecDeque<u64> = VecDeque::new();

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
        // RTP timestamp 基準每個 session 可能重來；先收掉上一段窗，已完成的樣本保留。
        fps.session_boundary();
        // SVW-19（N1）：reconnects 的語義是「斷流後**成功續流**的重連」，不是「握手成功」。
        // 相機會在 PLAY 200 之後直接 RST、或乾脆不送流；若在握手成功時就遞增，這些從未收到
        // 任何 RTP 的會話會被誤記成已重連成功（零 RTP 情境實測 5 段連線得 reconnects=4）。
        // 故延後到下方 `session_rtp == 0`「本會話首個 RTP」分支才遞增——與空窗收口點同一事件，
        // 兩者對「續流成功」的定義才不會分歧。session_is_reconnect 記住這段是不是重連會話。
        let session_is_reconnect = rep.connects > 1;
        if rep.sprop_sets == 0 {
            rep.sprop_sets = info.sprop.len();
        }
        // 相機的 sprop 已含起始碼：寫入前剝掉，避免出現 `00 00 00 01 00 00 00 01`。
        if !wrote_sprop && !info.sprop.is_empty() {
            for ps in info.parameter_sets() {
                sink.nal_at(ps, clock.now_ms());
            }
            wrote_sprop = true;
        }
        last_play_at = clock.now_ms();
        session_rtp = 0;
        on_event(Event::Playing {
            session: session_id.clone(),
            sprop_sets: info.sprop.len(),
        });

        backoff = cfg.reconnect.initial;
        // N1（PM 2026-11-15 R7 裁定）：空窗收口點不在 PLAY 200，而在本會話首個 RTP 到達時
        // （見下方 `session_rtp == 0` 分支）。R1 定義的空窗是「RST→本會話首個 RTP」；
        // 若在 PLAY 成功即收口，握手全綠但永不送 RTP 的相機會被誤記成已續流。

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
                            if session_rtp == 0 {
                                // 收到本會話第一個 RTP 才算「續流成功」：重置即時拒絕與中途 RST 計數。
                                // 只有 PLAY 成功不算——即時拒絕的會話同樣會 PLAY 成功然後 <10 s 被 RST。
                                immediate_streak = 0;
                                midstream_backoff = cfg.reconnect.midstream_initial;
                                // SVW-19（N1）：reconnects 也在這裡遞增，與空窗收口同一事件；
                                // 握手成功但零 RTP 的會話不計入「成功續流」。
                                if session_is_reconnect {
                                    rep.reconnects += 1;
                                }
                                // N1：空窗收口點＝本會話首個 RTP 到達時（R1 語義）。量到的 ms 是
                                // 「RST→首個 RTP」；PLAY 成功但永不送 RTP 的會話不會走到這裡，
                                // 故其空窗不記錄、不發 Reconnected，改由 recv_timeout 分類計入失敗。
                                if let Some(at) = gap_open_at.take() {
                                    let now = clock.now_ms();
                                    let ms = now.saturating_sub(at);
                                    // 錄影 sink 用這個通知在段邊界留 gap 記錄（M1）。
                                    // 順序在 NAL 之前：本會話首個 NAL 屬於「空窗之後」，不得併進前一節點。
                                    sink.gap(now, ms, &gap_reason);
                                    rep.gaps.push(StreamGap {
                                        at_ms: at.saturating_sub(t0),
                                        ms,
                                        reason: gap_reason.clone(),
                                    });
                                    rep.max_gap_ms = rep.max_gap_ms.max(ms);
                                    on_event(Event::Reconnected {
                                        after_ms: now.saturating_sub(t0),
                                        gap_ms: ms,
                                    });
                                }
                            }
                            session_rtp += 1;
                            // 序號只認「向前」：向後（亂序／重複）不動 high-water mark，
                            // 否則舊包會把 mark 拉回去，令下一個正常包被重複計成缺口。
                            // 這是變異測試 mutantB 逼出來的修正（見 README「harness」一節）。
                            // 序號只認「向前」：向後（亂序／重複）不動 high-water mark，
                            // 否則舊包會把 mark 拉回去，令下一個正常包被重複計成缺口。
                            // 這是變異測試 mutantB 逼出來的修正（詳見 README「harness」一節）。
                            // d 為 u16 環形距離：0 = 重複，1 = 連號，大的正數要分辨
                            // 「向前跳很多」與「向後走一點」，以 0xC000 為界（半圈以上算向後）。
                            match last_seq {
                                Some(prev) => {
                                    let d = p.seq.wrapping_sub(prev);
                                    if d == 1 {
                                        last_seq = Some(p.seq); // 連號
                                    } else if d > 1 && d < 0x4000 {
                                        let lost = (d - 1) as u64;
                                        rep.lost_packets += lost;
                                        rep.seq_gaps.push(SeqGap {
                                            from_seq: prev,
                                            to_seq: p.seq,
                                            lost: lost as u32,
                                            at_ms: clock.now_ms().saturating_sub(t0),
                                        });
                                        last_seq = Some(p.seq);
                                    } else if d == 0 || d >= 0xC000 {
                                        // 重複包，或 UDP 亂序送到的舊包（環形距離超過半圈）：
                                        // 只記方向，不動 mark。
                                        rep.out_of_order += 1;
                                    } else {
                                        // 0x4000 <= d < 0xC000：號稱「丟了大半圈」，實際上比較可能是
                                        // 來源重置序號。兩者無法區分，屬 §12 未證。M0 選擇把 mark
                                        // 推過去但不計丟包：留在舊 mark 會令之後每包都判成異常，
                                        // 把檢測卡死是更壞的失敗模式。
                                        last_seq = Some(p.seq);
                                    }
                                }
                                None => last_seq = Some(p.seq),
                            }

                            if p.payload_type == 96 {
                                fps.push(p.timestamp, p.marker);
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
                                            sink.nal_at(&nal, clock.now_ms());
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

        // §11.2：先分類，再按類別決定退避。分類只看「距上次 PLAY 多久」與「本會話有沒有收過 RTP」，
        // 不假設觸發原因已被解釋——未解釋的觸發一律落入 MidStream。
        let age_ms = now.saturating_sub(last_play_at);
        let class = if age_ms >= cfg.reconnect.timed_recycle_after.as_millis() as u64 {
            RstClass::TimedRecycle
        } else if age_ms < cfg.reconnect.immediate_reject_before.as_millis() as u64
            && session_rtp == 0
        {
            RstClass::ImmediateReject
        } else {
            RstClass::MidStream
        };
        match class {
            RstClass::TimedRecycle => rep.rst_timed_recycle += 1,
            RstClass::ImmediateReject => rep.rst_immediate_reject += 1,
            RstClass::MidStream => rep.rst_midstream += 1,
        }

        let mut delay = Duration::ZERO;
        let mut fatal: Option<String> = None;
        match class {
            RstClass::TimedRecycle => {
                // 設計內行為：立即重連，退避歸零（§11.2）。
                immediate_streak = 0;
                midstream_backoff = cfg.reconnect.midstream_initial;
            }
            RstClass::ImmediateReject => {
                immediate_streak += 1;
                if immediate_streak >= cfg.reconnect.immediate_limit {
                    fatal = Some(format!(
                        "immediate reject x{}: camera rejected {} consecutive sessions before any RTP",
                        immediate_streak, immediate_streak
                    ));
                } else {
                    let i = (immediate_streak as usize - 1)
                        .min(cfg.reconnect.immediate_ladder.len().saturating_sub(1));
                    delay = cfg
                        .reconnect
                        .immediate_ladder
                        .get(i)
                        .copied()
                        .unwrap_or(cfg.reconnect.max)
                        .min(cfg.reconnect.max);
                }
            }
            RstClass::MidStream => {
                delay = midstream_backoff.min(cfg.reconnect.max);
                midstream_backoff = next_backoff(midstream_backoff, &cfg.reconnect);
            }
        }

        on_event(Event::Rst { at_ms: now.saturating_sub(t0), reason: reason.clone(), class });
        // 已開著的空窗不覆寫（PM 2026-11-15 R8 裁定）：連續斷流期間相機一直沒送流，對外空窗是
        // 連續的一段，不得因中間那次「PLAY 成功但零 RTP」的會話而把前段時間丟掉，否則相機反覆
        // PLAY-不送流時，gaps 只報最後一次退避，看起來比實際健康。空窗由第一次 RST 開，
        // 直到本會話首個 RTP 才收口（見上方 session_rtp == 0 分支）。
        if gap_open_at.is_none() {
            gap_open_at = Some(now);
            gap_reason = reason;
        }
        let _ = link.close().await;

        // 重連風暴上限：視窗內重連次數超過上限即停止並上報，不要 quietly 燒 CPU（§11.2）。
        storm_times.push_back(now);
        while let Some(front) = storm_times.front() {
            if now.saturating_sub(*front) > cfg.reconnect.storm_window.as_millis() as u64 {
                storm_times.pop_front();
            } else {
                break;
            }
        }
        let storm = cfg.reconnect.storm_max_reconnects > 0
            && storm_times.len() as u32 > cfg.reconnect.storm_max_reconnects;

        if let Some(f) = fatal {
            rep.errors.push(f.clone());
            finished = f;
            break 'outer;
        }
        if storm {
            finished = format!(
                "reconnect storm: {} reconnects within {} ms",
                storm_times.len(),
                cfg.reconnect.storm_window.as_millis()
            );
            rep.errors.push(finished.clone());
            break 'outer;
        }

        rep.backoff_ms.push(delay.as_millis() as u64);
        clock.sleep(delay).await;
        // 連線／握手失敗那條路仍用通用退避；兩者語義不同，不共用同一個計數。
        backoff = cfg.reconnect.initial;
    }

    rep.incomplete_frames = depack.incomplete;
    rep.fps = fps.finish();
    rep.duration_ms = clock.now_ms().saturating_sub(t0);
    if rep.duration_ms > 0 {
        rep.bitrate_bps = (rep.rtp_bytes as f64 * 8.0) / (rep.duration_ms as f64 / 1000.0);
    }
    rep.finished = finished.clone();
    on_event(Event::Finished { reason: finished });
    rep
}
