//! 錄製封包重放：把一次（或多段）真機會話餵回同一條狀態機，令無相機時也能跑全鏈路。
//!
//! fixture 是 JSON Lines（`.sviewcap.jsonl`），一行一個事件：
//! ```text
//! {"type":"meta","note":"...","expected":{"reconnects":2,"seq_gaps":1,"connections":3}}
//! {"type":"conn","index":0}
//! {"type":"rtsp_s2c","t_ms":1,"data":"RTSP/1.0 200 OK\r\n..."}
//! {"type":"rtp","t_ms":500,"data":"<base64 RTP datagram>"}
//! {"type":"rst","t_ms":61200}
//! {"type":"synth_stream","t_ms":1500,"duration_ms":390000,"bitrate_bps":500000,"fps":25,"gop_ms":2000}
//! ```
//! `synth_stream` 不是錄下來的封包，而是重放時**確定性**合成 RTP（走同一條
//! [`crate::rtp`] 組包／FU-A 路徑），讓 30 分鐘、12 Mbps 這種量級不必把 2.7 GB
//! base64 塞進 JSONL。
//! `rtsp_c2s` 行只作記錄（保留客戶端原話以便對照），重放時不驅動狀態。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;

use crate::clock::VirtualClock;
use crate::error::{Error, Result};
use crate::link::{Link, LinkFactory};
use crate::rtp;
use crate::rtsp::{self, Response};

#[derive(Debug, Clone, Default)]
pub struct FixtureMeta {
    pub name: Option<String>,
    pub note: Option<String>,
    pub expected_reconnects: Option<u32>,
    pub expected_seq_gaps: Option<usize>,
    pub expected_nals: Option<u64>,
    pub expected_connections: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepKind {
    Response(String),
    Rtp(Vec<u8>),
    Rst,
    /// 即時合成串流（`synth_stream`）：不把 2.7 GB base64 塞進 JSONL，
    /// 由 [`SynthSpec`] 在重放時確定性生成 RTP。
    Synth(SynthSpec),
}

/// 合成串流規格（fixture `{"type":"synth_stream", ...}`）。
///
/// 確定性要求：同一份 fixture 每次產生的位元組完全相同——不使用 wall clock、
/// 不使用亂數，切片內容只由 frame 序號推導。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthSpec {
    /// 首個 frame 的時間（毫秒，虛擬時鐘）。
    pub start_ms: u64,
    /// 串流長度（毫秒）。
    pub duration_ms: u64,
    /// 目標碼率（bps）。
    pub bitrate_bps: u64,
    /// 幀率。
    pub fps: u32,
    /// GOP 長度（毫秒）；IDR 邊界由 frame 時間推導。
    pub gop_ms: u64,
}

impl SynthSpec {
    /// RTP payload 上限（留給 IP/UDP 標頭）。
    pub const MAX_RTP_PAYLOAD: usize = 1400;
    /// 合成串流的 SSRC。
    pub const SSRC: u32 = 0x1122_3344;
    /// 合成串流的 payload type。
    pub const PAYLOAD_TYPE: u8 = 96;
    /// 決定性 SPS（與 fixture SDP 的 sprop 一致）。
    pub const SPS: [u8; 16] = [
        0x67, 0x4D, 0x00, 0x32, 0x96, 0x35, 0x40, 0x50, 0x01, 0x6B, 0x4D, 0xC0, 0x40, 0x40, 0x40,
        0x80,
    ];
    /// 決定性 PPS。
    pub const PPS: [u8; 4] = [0x68, 0xEE, 0x31, 0xB2];

    /// 總 frame 數。
    pub fn frames(&self) -> u64 {
        self.duration_ms.saturating_mul(self.fps as u64) / 1000
    }

    /// 第 `i` 個 frame 的時間（毫秒）。
    pub fn frame_ms(&self, i: u64) -> u64 {
        self.start_ms + i.saturating_mul(1000) / self.fps as u64
    }

    /// 每個 frame 的 slice bytes（依碼率推導）。
    pub fn frame_bytes(&self) -> usize {
        // bitrate_bps 是 bits/sec：每個 frame 的位元組 = bps / (fps * 8)
        (self.bitrate_bps / (self.fps as u64 * 8)).max(16) as usize
    }

    /// 第 `i` 個 frame 是否為 IDR（GOP 對齊）。
    pub fn is_idr(&self, i: u64) -> bool {
        if self.gop_ms == 0 {
            i == 0
        } else {
            (i.saturating_mul(1000) / self.fps as u64) % self.gop_ms == 0
        }
    }

    /// 決定性切片內容（線性同餘，只由 frame 序號推導）。
    fn slice_bytes(&self, i: u64) -> Vec<u8> {
        let len = self.frame_bytes();
        let mut out = Vec::with_capacity(len);
        let mut s = i.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        while out.len() < len {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            out.push((s >> 33) as u8);
        }
        out
    }

    /// 第 `i` 個 frame 的 RTP 封包（IDR 前帶 SPS/PPS；大 NAL 走 FU-A）。
    pub fn frame_packets(&self, i: u64, seq: &mut u16) -> Vec<Vec<u8>> {
        let t_ms = self.frame_ms(i);
        let ts = (t_ms.saturating_mul(90)) as u32;
        let mut out = Vec::new();
        if self.is_idr(i) {
            self.push_nal(&Self::SPS, seq, ts, &mut out);
            self.push_nal(&Self::PPS, seq, ts, &mut out);
        }
        let mut nal = Vec::with_capacity(self.frame_bytes() + 1);
        nal.push(if self.is_idr(i) { 0x65 } else { 0x41 });
        nal.extend_from_slice(&self.slice_bytes(i));
        self.push_nal(&nal, seq, ts, &mut out);
        out
    }

    fn push_nal(&self, nal: &[u8], seq: &mut u16, ts: u32, out: &mut Vec<Vec<u8>>) {
        if nal.len() <= Self::MAX_RTP_PAYLOAD {
            out.push(rtp::build(*seq, ts, Self::SSRC, true, Self::PAYLOAD_TYPE, nal));
            *seq = seq.wrapping_add(1);
            return;
        }
        let body = &nal[1..];
        let chunk = Self::MAX_RTP_PAYLOAD - 2;
        let count = body.len().div_ceil(chunk);
        for k in 0..count {
            let start = k * chunk;
            let end = (start + chunk).min(body.len());
            let mut fu = Vec::with_capacity(2 + end - start);
            fu.push((nal[0] & 0xe0) | 28);
            let mut hdr = nal[0] & 0x1f;
            if k == 0 {
                hdr |= 0x80;
            }
            if k == count - 1 {
                hdr |= 0x40;
            }
            fu.push(hdr);
            fu.extend_from_slice(&body[start..end]);
            let marker = k == count - 1;
            out.push(rtp::build(*seq, ts, Self::SSRC, marker, Self::PAYLOAD_TYPE, &fu));
            *seq = seq.wrapping_add(1);
        }
    }
}

#[derive(Debug, Clone)]
pub struct Step {
    pub t_ms: u64,
    pub kind: StepKind,
}

#[derive(Debug, Clone, Default)]
pub struct ReplayFixture {
    pub meta: FixtureMeta,
    pub connections: Vec<Vec<Step>>,
}

impl ReplayFixture {
    pub fn parse(text: &str) -> Result<Self> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut fixture = ReplayFixture::default();
        let mut current: Option<Vec<Step>> = None;

        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let v: Value = serde_json::from_str(line)
                .map_err(|e| Error::Protocol(format!("fixture line {}: {e}", lineno + 1)))?;
            let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
            let t_ms = v.get("t_ms").and_then(Value::as_u64).unwrap_or(0);
            match kind {
                "meta" => {
                    fixture.meta.name = v.get("name").and_then(Value::as_str).map(str::to_string);
                    fixture.meta.note = v.get("note").and_then(Value::as_str).map(str::to_string);
                    if let Some(exp) = v.get("expected") {
                        fixture.meta.expected_reconnects =
                            exp.get("reconnects").and_then(Value::as_u64).map(|n| n as u32);
                        fixture.meta.expected_seq_gaps =
                            exp.get("seq_gaps").and_then(Value::as_u64).map(|n| n as usize);
                        fixture.meta.expected_nals =
                            exp.get("nals").and_then(Value::as_u64);
                        fixture.meta.expected_connections =
                            exp.get("connections").and_then(Value::as_u64).map(|n| n as u32);
                    }
                }
                "conn" => {
                    if let Some(steps) = current.take() {
                        fixture.connections.push(steps);
                    }
                    current = Some(Vec::new());
                }
                "rtsp_s2c" => {
                    let data = v
                        .get("data")
                        .and_then(Value::as_str)
                        .ok_or_else(|| Error::Protocol(format!("line {}: rtsp_s2c without data", lineno + 1)))?;
                    let mut bytes = data.as_bytes().to_vec();
                    if v.get("encoding").and_then(Value::as_str) == Some("base64") {
                        bytes = b64.decode(data).map_err(|e| Error::Protocol(e.to_string()))?;
                    }
                    let text = String::from_utf8(bytes)
                        .map_err(|e| Error::Protocol(format!("rtsp_s2c not utf8: {e}")))?;
                    current
                        .get_or_insert_with(Vec::new)
                        .push(Step { t_ms, kind: StepKind::Response(text) });
                }
                "rtsp_c2s" => { /* 只作記錄，重放時不驅動 */ }
                "rtp" => {
                    let data = v
                        .get("data")
                        .and_then(Value::as_str)
                        .ok_or_else(|| Error::Protocol(format!("line {}: rtp without data", lineno + 1)))?;
                    let bytes = b64.decode(data).map_err(|e| Error::Protocol(e.to_string()))?;
                    current.get_or_insert_with(Vec::new).push(Step { t_ms, kind: StepKind::Rtp(bytes) });
                }
                "rst" => current.get_or_insert_with(Vec::new).push(Step { t_ms, kind: StepKind::Rst }),
                "synth_stream" => {
                    let duration_ms = v
                        .get("duration_ms")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| Error::Protocol(format!("line {}: synth_stream without duration_ms", lineno + 1)))?;
                    if duration_ms == 0 {
                        return Err(Error::Protocol(format!("line {}: synth_stream duration_ms must be > 0", lineno + 1)));
                    }
                    let fps = v.get("fps").and_then(Value::as_u64).unwrap_or(25) as u32;
                    if fps == 0 {
                        return Err(Error::Protocol(format!("line {}: synth_stream fps must be > 0", lineno + 1)));
                    }
                    let bitrate_bps = v
                        .get("bitrate_bps")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| Error::Protocol(format!("line {}: synth_stream without bitrate_bps", lineno + 1)))?;
                    let gop_ms = v.get("gop_ms").and_then(Value::as_u64).unwrap_or(2_000);
                    current.get_or_insert_with(Vec::new).push(Step {
                        t_ms,
                        kind: StepKind::Synth(SynthSpec {
                            start_ms: t_ms,
                            duration_ms,
                            bitrate_bps,
                            fps,
                            gop_ms,
                        }),
                    });
                }
                "end" => {
                    if let Some(steps) = current.take() {
                        fixture.connections.push(steps);
                    }
                }
                other => {
                    return Err(Error::Protocol(format!("fixture line {}: unknown type {other}", lineno + 1)))
                }
            }
        }
        if let Some(steps) = current.take() {
            fixture.connections.push(steps);
        }
        if fixture.connections.is_empty() {
            return Err(Error::Protocol("fixture contains no connections".into()));
        }
        Ok(fixture)
    }

    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Protocol(format!("read {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn step_count(&self) -> usize {
        self.connections.iter().map(|c| c.len()).sum()
    }
}

pub struct ReplayFactory {
    fixture: ReplayFixture,
    clock: Arc<VirtualClock>,
    cursor: Mutex<usize>,
    client_port: u16,
}

impl ReplayFactory {
    pub fn new(fixture: ReplayFixture, clock: Arc<VirtualClock>) -> Self {
        Self { fixture, clock, cursor: Mutex::new(0), client_port: 20_000 }
    }

    pub fn fixture(&self) -> &ReplayFixture {
        &self.fixture
    }
}

struct ReplayLink {
    steps: Vec<Step>,
    idx: usize,
    pending: VecDeque<Response>,
    clock: Arc<VirtualClock>,
    client_port: u16,
    /// 合成串流已產生的 frame 數。
    synth_pos: u64,
    /// 已生成、待送出的合成 RTP 封包（時間, bytes）。
    synth_queue: VecDeque<(u64, Vec<u8>)>,
    /// 合成串流的下一個 RTP 序號。
    synth_seq: u16,
}

#[async_trait]
impl Link for ReplayLink {
    fn client_rtp_port(&self) -> u16 {
        self.client_port
    }

    async fn send(&mut self, request: &str) -> Result<()> {
        // 重放不驗證請求內容：請求正確性由 mock／真機測；這裡只把下一個 s2c 交給客戶端。
        let _ = request;
        while self.idx < self.steps.len() {
            match &self.steps[self.idx].kind {
                StepKind::Response(raw) => {
                    let t = self.steps[self.idx].t_ms;
                    let resp = rtsp::parse_response(raw)?;
                    self.clock.advance_to(t);
                    self.pending.push_back(resp);
                    self.idx += 1;
                    return Ok(());
                }
                _ => break,
            }
        }
        Ok(())
    }

    async fn recv_response(&mut self) -> Result<Response> {
        self.pending
            .pop_front()
            .ok_or_else(|| Error::Protocol("replay: no response queued for this request".into()))
    }

    async fn recv_rtp(&mut self) -> Result<Vec<u8>> {
        loop {
            if let Some((t, data)) = self.synth_queue.pop_front() {
                self.clock.advance_to(t);
                return Ok(data);
            }
            if self.idx >= self.steps.len() {
                return Err(Error::EndOfInput);
            }
            let t = self.steps[self.idx].t_ms;
            match &self.steps[self.idx].kind {
                StepKind::Rtp(data) => {
                    let data = data.clone();
                    self.clock.advance_to(t);
                    self.idx += 1;
                    return Ok(data);
                }
                StepKind::Rst => {
                    self.clock.advance_to(t);
                    self.idx += 1;
                    return Err(Error::Closed("fixture: camera RST".into()));
                }
                StepKind::Response(raw) => {
                    let resp = rtsp::parse_response(raw)?;
                    self.clock.advance_to(t);
                    self.pending.push_back(resp);
                    self.idx += 1;
                }
                StepKind::Synth(spec) => {
                    // 下一個明確步驟的時間是合成串流的上限（讓 fixture 的 rst 能準時打斷）
                    let limit = self.steps.get(self.idx + 1).map(|s| s.t_ms);
                    let total = spec.frames();
                    let mut produced = false;
                    while self.synth_pos < total {
                        let frame_ms = spec.frame_ms(self.synth_pos);
                        if let Some(limit) = limit {
                            if frame_ms >= limit {
                                break;
                            }
                        }
                        for pkt in spec.frame_packets(self.synth_pos, &mut self.synth_seq) {
                            self.synth_queue.push_back((frame_ms, pkt));
                        }
                        self.synth_pos += 1;
                        produced = true;
                        break;
                    }
                    if !produced {
                        self.idx += 1;
                        self.synth_pos = 0;
                    }
                }
            }
        }
    }
}

#[async_trait]
impl LinkFactory for ReplayFactory {
    async fn connect(&self) -> Result<(Box<dyn Link>, u16)> {
        let mut cursor = self.cursor.lock().unwrap();
        let steps = self
            .fixture
            .connections
            .get(*cursor)
            .cloned()
            .ok_or(Error::EndOfInput)?;
        *cursor += 1;
        Ok((
            Box::new(ReplayLink {
                steps,
                idx: 0,
                pending: VecDeque::new(),
                clock: self.clock.clone(),
                client_port: self.client_port,
                synth_pos: 0,
                synth_queue: VecDeque::new(),
                synth_seq: 1,
            }),
            self.client_port,
        ))
    }
}
