//! RTSP 請求組裝與回應解析（文檔 §4.4 的實例逐字對齊）。

use crate::error::{Error, Result};

/// 官方 App 的 User-Agent；值後方有一個空格，PC 實測沿用（文檔 §4.2）。
pub const USER_AGENT: &str = "SViewWCam V202203 ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Options,
    Describe,
    Setup,
    Play,
    Teardown,
}

impl Method {
    pub fn as_str(&self) -> &'static str {
        match self {
            Method::Options => "OPTIONS",
            Method::Describe => "DESCRIBE",
            Method::Setup => "SETUP",
            Method::Play => "PLAY",
            Method::Teardown => "TEARDOWN",
        }
    }
}

/// 文檔 §4.1：PLAY 的 URL 必須帶結尾斜線，TEARDOWN 不帶。
pub fn with_trailing_slash(uri: &str) -> String {
    if uri.ends_with('/') {
        uri.to_string()
    } else {
        format!("{uri}/")
    }
}

fn head(method: Method, uri: &str, cseq: u32) -> String {
    format!(
        "{} {} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: {}\r\n",
        method.as_str(),
        uri,
        cseq,
        USER_AGENT
    )
}

pub fn options(uri: &str, cseq: u32) -> String {
    let mut s = head(Method::Options, uri, cseq);
    s.push_str("\r\n");
    s
}

/// DESCRIBE 額外帶 `Accept: application/sdp` 與 `SViewWifi` 權杖；
/// 其餘方法不帶權杖仍然 200（文檔 §4.2）。
pub fn describe(uri: &str, cseq: u32, token: &str) -> String {
    let mut s = head(Method::Describe, uri, cseq);
    s.push_str("Accept: application/sdp\r\n");
    s.push_str(&format!("SViewWifi: {token}\r\n"));
    s.push_str("\r\n");
    s
}

/// SETUP 打的 client_port 是「RTP-RTCP」連號；相機回傳的
/// `destination` / `source` / `server_port` 全部不可信（文檔 §4.3）。
pub fn setup(uri: &str, cseq: u32, rtp_port: u16) -> String {
    let mut s = head(Method::Setup, uri, cseq);
    s.push_str(&format!(
        "Transport: RTP/AVP;unicast;client_port={rtp_port}-{}\r\n",
        rtp_port + 1
    ));
    s.push_str("\r\n");
    s
}

pub fn play(uri: &str, cseq: u32, session: &str) -> String {
    let mut s = head(Method::Play, &with_trailing_slash(uri), cseq);
    s.push_str(&format!("Session: {session}\r\n"));
    s.push_str("Range: npt=0-\r\n");
    s.push_str("\r\n");
    s
}

pub fn teardown(uri: &str, cseq: u32, session: &str) -> String {
    let mut s = head(Method::Teardown, uri, cseq);
    s.push_str(&format!("Session: {session}\r\n"));
    s.push_str("\r\n");
    s
}

#[derive(Debug, Clone, Default)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_ok(&self) -> bool {
        self.status == 200
    }

    pub fn session(&self) -> Option<&str> {
        self.header("Session")
    }
}

/// 解析一個完整 RTSP 訊息（已含 header 與依 Content-Length 讀完的 body）。
pub fn parse_response(raw: &str) -> Result<Response> {
    let (head, body) = match raw.find("\r\n\r\n") {
        Some(i) => (&raw[..i], &raw[i + 4..]),
        None => return Err(Error::Protocol("rtsp response without header terminator".into())),
    };
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| Error::Protocol("empty rtsp response".into()))?;
    let mut parts = status_line.splitn(3, ' ');
    let proto = parts.next().unwrap_or_default();
    if !proto.starts_with("RTSP/") {
        return Err(Error::Protocol(format!("bad status line: {status_line}")));
    }
    let status: u16 = parts
        .next()
        .ok_or_else(|| Error::Protocol(format!("bad status line: {status_line}")))?
        .parse()
        .map_err(|_| Error::Protocol(format!("bad status code: {status_line}")))?;
    let reason = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| Error::Protocol(format!("bad header line: {line}")))?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    Ok(Response { status, reason, headers, body: body.to_string() })
}

/// 由 header 區塊推算整個訊息長度（header + Content-Length body）。
/// 回傳 `None` 表示 header 尚未收齊。
pub fn message_len(buf: &[u8]) -> Option<usize> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head_len = end + 4;
    let head = String::from_utf8_lossy(&buf[..end]);
    let mut content_len = 0usize;
    for line in head.split("\r\n").skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("Content-Length") {
                content_len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    Some(head_len + content_len)
}

/// 從 `Transport` 回應標頭取相機回報的 client_port（只用於日誌；RTP 實際埠以我方為準）。
pub fn transport_client_port(transport: &str) -> Option<u16> {
    let idx = transport.find("client_port=")?;
    let rest = &transport[idx + "client_port=".len()..];
    let val: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    val.split('-').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_url_gets_trailing_slash() {
        assert_eq!(with_trailing_slash("rtsp://h/test.264"), "rtsp://h/test.264/");
        assert_eq!(with_trailing_slash("rtsp://h/test.264/"), "rtsp://h/test.264/");
    }

    #[test]
    fn parses_response_with_content_length() {
        let raw = "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: 5\r\n\r\nhello";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.header("cseq"), Some("2"));
        assert_eq!(r.body, "hello");
        assert_eq!(message_len(raw.as_bytes()), Some(raw.len()));
    }

    #[test]
    fn transport_port_is_readable() {
        let t = "RTP/AVP;unicast;client_port=20000-20001;destination=192.168.245.65;source=192.168.245.96;server_port=0-0";
        assert_eq!(transport_client_port(t), Some(20000));
    }
}
