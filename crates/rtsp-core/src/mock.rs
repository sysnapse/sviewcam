//! 合成 RTP 源（scripted mock）：無相機時用來驗協議時序、重連與 depacket。
//!
//! 它不是「假裝成功」的 stub：每一條請求都會被逐項檢查（User-Agent 尾隨空格、
//! DESCRIBE 帶 `SViewWifi`、SETUP 的 `client_port`、PLAY 的結尾斜線與 `Range`），
//! 違規會記入 `violations`，測試據此失敗。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::link::{Link, LinkFactory};
use crate::rtsp::Response;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndMode {
    /// 模擬相機在開流後主動重設連線（文檔 §10）。
    Rst,
    /// 資料用完但連線正常結束（重放 fixture 尾段）。
    End,
}

#[derive(Debug, Clone)]
pub struct ConnPlan {
    pub session_id: String,
    pub packets: Vec<Vec<u8>>,
    pub end: EndMode,
    /// 覆寫 SDP（預設用文檔 §5 樣本）。
    pub sdp: Option<String>,
    /// 模擬 DESCRIBE 非 200。
    pub describe_status: Option<u16>,
    /// PLAY 成功之後永不送 RTP（recv_rtp 永遠 pending），用來測 recv_timeout 分支：
    /// 相機握手全綠卻不送流，N1 要求這種會話不得被記成「已續流」。
    pub hang: bool,
}

impl ConnPlan {
    pub fn new(session_id: impl Into<String>, packets: Vec<Vec<u8>>) -> Self {
        Self {
            session_id: session_id.into(),
            packets,
            end: EndMode::Rst,
            sdp: None,
            describe_status: None,
            hang: false,
        }
    }

    /// 握手全綠但永不送 RTP（recv_timeout 前零 RTP）。
    pub fn hang(mut self) -> Self {
        self.hang = true;
        self
    }

    pub fn end(mut self, mode: EndMode) -> Self {
        self.end = mode;
        self
    }
}

pub fn default_sdp(token: &str) -> String {
    format!(
        "v=0\r\n\
o=-2209971456 2209971456 IN IP4 192.168.1.1\r\n\
s=Unnamed\r\n\
i=N/A\r\n\
c=IN IP4 192.168.1.11\r\n\
t=0 0\r\n\
a=recvonly\r\n\
m=video 5004 RTP/AVP 96\r\n\
b=RR:0\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1;profile-level-id=640029;sprop-parameter-sets=AAAAAWdNADKWNUBQAWtNwEBAQIA=,AAAAAWjuMbI=;\r\n\
a=control:trackID=0\r\n\
a=x-dimensions:2560,1440\r\n\
a=framerate:30\r\n\
a=bitrate:240000\r\n\
a=SViewWifi:{token}\r\n"
    )
}

pub struct MockFactory {
    plans: Mutex<VecDeque<ConnPlan>>,
    /// 頭 N 次 connect 直接失敗（測退避與重試上限）。
    fail_first: usize,
    attempts: Mutex<usize>,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub violations: Arc<Mutex<Vec<String>>>,
    pub client_ports: Arc<Mutex<Vec<u16>>>,
}

impl MockFactory {
    pub fn new(plans: Vec<ConnPlan>) -> Self {
        Self {
            plans: Mutex::new(plans.into()),
            fail_first: 0,
            attempts: Mutex::new(0),
            requests: Arc::new(Mutex::new(Vec::new())),
            violations: Arc::new(Mutex::new(Vec::new())),
            client_ports: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn fail_first(mut self, n: usize) -> Self {
        self.fail_first = n;
        self
    }

    pub fn violations(&self) -> Vec<String> {
        self.violations.lock().unwrap().clone()
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_methods(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter_map(|r| r.split_whitespace().next().map(str::to_string))
            .collect()
    }
}

struct MockLink {
    plan: ConnPlan,
    idx: usize,
    pending: VecDeque<Response>,
    client_port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    violations: Arc<Mutex<Vec<String>>>,
    closed: bool,
}

fn header_of(request: &str) -> Vec<(String, String)> {
    request
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.to_string()))
        .collect()
}

fn cseq_of(request: &str) -> String {
    header_of(request)
        .into_iter()
        .find(|(k, _)| k == "cseq")
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| "0".into())
}

impl MockLink {
    fn record_violation(&self, msg: impl Into<String>) {
        self.violations.lock().unwrap().push(msg.into());
    }

    fn respond(&mut self, req: &str) -> Result<Response> {
        self.requests.lock().unwrap().push(req.to_string());
        let mut lines = req.split("\r\n");
        let start = lines.next().unwrap_or_default();
        let parts: Vec<&str> = start.split(' ').collect();
        if parts.len() != 3 || parts[2] != "RTSP/1.0" {
            return Err(Error::Protocol(format!("bad request line: {start}")));
        }
        let (method, uri) = (parts[0], parts[1]);
        if !req.contains("User-Agent: SViewWCam V202203 \r\n") {
            self.record_violation(format!("{method}: User-Agent 缺少尾部空格或值不對"));
        }
        let headers = header_of(req);
        let has = |name: &str| headers.iter().any(|(k, _)| k == name);
        let cseq = cseq_of(req);

        match method {
            "OPTIONS" => {
                if has("sviewwifi") {
                    self.record_violation("OPTIONS 不應帶 SViewWifi");
                }
                Ok(Response {
                    status: 200,
                    reason: "OK".into(),
                    headers: vec![
                        ("CSeq".into(), cseq),
                        ("Public".into(), "OPTIONS,DESCRIBE,SETUP,PLAY,PAUSE,TEARDOWN".into()),
                    ],
                    body: String::new(),
                })
            }
            "DESCRIBE" => {
                if !has("accept") || !req.contains("Accept: application/sdp") {
                    self.record_violation("DESCRIBE 缺少 Accept: application/sdp");
                }
                if !has("sviewwifi") {
                    self.record_violation("DESCRIBE 缺少 SViewWifi 權杖");
                }
                if let Some(status) = self.plan.describe_status {
                    return Ok(Response { status, reason: "ERR".into(), headers: vec![], body: String::new() });
                }
                let body = self.plan.sdp.clone().unwrap_or_else(|| default_sdp("01234567890123456789012345678901"));
                Ok(Response {
                    status: 200,
                    reason: "OK".into(),
                    headers: vec![
                        ("CSeq".into(), cseq),
                        ("Content-Base".into(), "rtsp://192.168.1.1/test.264/".into()),
                        ("Content-Type".into(), "application/sdp".into()),
                        ("Content-Length".into(), body.len().to_string()),
                        ("Server".into(), "sunshine/1.11".into()),
                    ],
                    body,
                })
            }
            "SETUP" => {
                let expect = format!("client_port={}-{}", self.client_port, self.client_port + 1);
                if !headers
                    .iter()
                    .any(|(k, v)| k == "transport" && v.contains(&expect))
                {
                    self.record_violation(format!("SETUP 未申請連號埠 {expect}"));
                }
                if uri.ends_with('/') {
                    // 相機接受，但文檔記錄官方 App 打在 URL 根；記錄不視為違規。
                }
                Ok(Response {
                    status: 200,
                    reason: "OK".into(),
                    headers: vec![
                        ("CSeq".into(), cseq),
                        ("Session".into(), format!("{};timeout=60", self.plan.session_id)),
                        ("Transport".into(), format!(
                            "RTP/AVP;unicast;client_port={}-{};destination=192.168.245.65;source=192.168.245.96;server_port=0-0",
                            self.client_port,
                            self.client_port + 1
                        )),
                    ],
                    body: String::new(),
                })
            }
            "PLAY" => {
                if !uri.ends_with('/') {
                    self.record_violation("PLAY 的 URL 未帶結尾斜線");
                }
                if !has("session") {
                    self.record_violation("PLAY 缺少 Session");
                }
                if !req.contains("Range: npt=0-") {
                    self.record_violation("PLAY 缺少 Range: npt=0-");
                }
                Ok(Response {
                    status: 200,
                    reason: "OK".into(),
                    headers: vec![
                        ("CSeq".into(), cseq),
                        ("Session".into(), self.plan.session_id.clone()),
                        ("Range".into(), "npt=0-".into()),
                    ],
                    body: String::new(),
                })
            }
            "TEARDOWN" => Ok(Response {
                status: 200,
                reason: "OK".into(),
                headers: vec![("CSeq".into(), cseq)],
                body: String::new(),
            }),
            other => Err(Error::Protocol(format!("unexpected method {other}"))),
        }
    }
}

#[async_trait]
impl Link for MockLink {
    fn client_rtp_port(&self) -> u16 {
        self.client_port
    }

    async fn send(&mut self, request: &str) -> Result<()> {
        if self.closed {
            return Err(Error::Closed("mock link closed".into()));
        }
        let resp = self.respond(request)?;
        self.pending.push_back(resp);
        Ok(())
    }

    async fn recv_response(&mut self) -> Result<Response> {
        self.pending.pop_front().ok_or_else(|| Error::Protocol("no pending response".into()))
    }

    async fn recv_rtp(&mut self) -> Result<Vec<u8>> {
        if self.plan.hang {
            std::future::pending::<()>().await;
        }
        match self.plan.packets.get(self.idx) {
            Some(p) => {
                self.idx += 1;
                Ok(p.clone())
            }
            None => {
                self.closed = true;
                match self.plan.end {
                    EndMode::Rst => Err(Error::Closed("simulated RST from camera".into())),
                    EndMode::End => Err(Error::EndOfInput),
                }
            }
        }
    }

    async fn close(&mut self) {
        self.closed = true;
    }
}

#[async_trait]
impl LinkFactory for MockFactory {
    async fn connect(&self) -> Result<(Box<dyn Link>, u16)> {
        let mut attempts = self.attempts.lock().unwrap();
        *attempts += 1;
        let n = *attempts;
        drop(attempts);
        if n <= self.fail_first {
            return Err(Error::Closed(format!("simulated connect failure #{n}")));
        }
        let plan = self
            .plans
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(Error::EndOfInput)?;
        let client_port = 20_000u16;
        self.client_ports.lock().unwrap().push(client_port);
        Ok((
            Box::new(MockLink {
                plan,
                idx: 0,
                pending: VecDeque::new(),
                client_port,
                requests: self.requests.clone(),
                violations: self.violations.clone(),
                closed: false,
            }),
            client_port,
        ))
    }
}

/// 把一個 NAL 切成 FU-A 片段（測試合成流用）。
pub fn fu_a_fragments(nal: &[u8], max_fragment: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if nal.len() < 2 {
        return out;
    }
    let indicator = 0x7c; // F=0 NRI=3 type=28
    let typ = nal[0] & 0x1f;
    let body = &nal[1..];
    let chunks: Vec<&[u8]> = body.chunks(max_fragment).collect();
    let n = chunks.len();
    for (i, chunk) in chunks.iter().enumerate() {
        let mut p = vec![indicator, 0];
        if i == 0 {
            p[1] |= 0x80;
        }
        if i == n - 1 {
            p[1] |= 0x40;
        }
        p[1] |= typ;
        p.extend_from_slice(chunk);
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp;

    #[test]
    fn fu_a_split_is_reassemblable_shape() {
        let nal = vec![0x65, 1, 2, 3, 4, 5];
        let frags = fu_a_fragments(&nal, 2);
        assert_eq!(frags.len(), 3);
        assert_eq!(frags[0][1] & 0x80, 0x80);
        assert_eq!(frags[2][1] & 0x40, 0x40);
        assert_eq!(frags[0][1] & 0x1f, 5);
        assert_eq!(rtsp::USER_AGENT.trim_end(), "SViewWCam V202203");
    }
}
