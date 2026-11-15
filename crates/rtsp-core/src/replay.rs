//! 錄製封包重放：把一次（或多段）真機會話餵回同一條狀態機，令無相機時也能跑全鏈路。
//!
//! fixture 是 JSON Lines（`.sviewcap.jsonl`），一行一個事件：
//! ```text
//! {"type":"meta","note":"...","expected":{"reconnects":2,"seq_gaps":1,"connections":3}}
//! {"type":"conn","index":0}
//! {"type":"rtsp_s2c","t_ms":1,"data":"RTSP/1.0 200 OK\r\n..."}
//! {"type":"rtp","t_ms":500,"data":"<base64 RTP datagram>"}
//! {"type":"rst","t_ms":61200}
//! ```
//! `rtsp_c2s` 行只作記錄（保留客戶端原話以便對照），重放時不驅動狀態。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;

use crate::clock::VirtualClock;
use crate::error::{Error, Result};
use crate::link::{Link, LinkFactory};
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
        while self.idx < self.steps.len() {
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
            }
        }
        Err(Error::EndOfInput)
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
            }),
            self.client_port,
        ))
    }
}
