//! 傳輸層抽象：真機用 [`TcpLinkFactory`]（TCP 554 控制面 + UDP RTP + UDP 13000 心跳），
//! fixture 用 [`crate::replay::ReplayFactory`]。
//!
//! 把 Link 抽出來是為了令重連狀態機（session）與傳輸解耦：同一段邏輯可以
//! 在無相機的情況下用錄製封包完整重跑。

use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

use crate::error::{Error, Result};
use crate::rtsp::{self, Response};

/// 16-byte 心跳模板（文檔 §6.1：`28000000010000000000000079000000`）。
pub const HEARTBEAT_TEMPLATE: [u8; 16] = [
    0x28, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x79, 0x00, 0x00, 0x00,
];

#[async_trait]
pub trait Link: Send {
    /// 我方在 SETUP 申請的 RTP 埠（相機回傳的 server_port 不可信）。
    fn client_rtp_port(&self) -> u16;
    async fn send(&mut self, request: &str) -> Result<()>;
    async fn recv_response(&mut self) -> Result<Response>;
    async fn recv_rtp(&mut self) -> Result<Vec<u8>>;
    /// 上行 UDP 13000 心跳；不需要心跳的實作（fixture）保持 no-op。
    async fn heartbeat(&mut self) -> Result<()> {
        Ok(())
    }
    async fn close(&mut self) {}
}

#[async_trait]
pub trait LinkFactory: Send + Sync {
    async fn connect(&self) -> Result<(Box<dyn Link>, u16)>;
}

#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    pub port: u16,
    pub payload: [u8; 16],
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self { port: crate::DEFAULT_HEARTBEAT_PORT, payload: HEARTBEAT_TEMPLATE }
    }
}

pub struct TcpLinkFactory {
    pub host: String,
    pub rtsp_port: u16,
    pub heartbeat: Option<HeartbeatConfig>,
    /// 由哪個本機位址收 RTP（多網卡時要指去相機熱點那張網卡）。
    pub bind_ip: Option<std::net::IpAddr>,
    pub connect_timeout: Duration,
}

impl TcpLinkFactory {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            rtsp_port: crate::DEFAULT_RTSP_PORT,
            heartbeat: Some(HeartbeatConfig::default()),
            bind_ip: None,
            connect_timeout: Duration::from_secs(5),
        }
    }
}

struct TcpLink {
    stream: TcpStream,
    buf: Vec<u8>,
    udp: UdpSocket,
    rtp_port: u16,
    heartbeat: Option<(UdpSocket, SocketAddr, [u8; 16])>,
}

#[async_trait]
impl Link for TcpLink {
    fn client_rtp_port(&self) -> u16 {
        self.rtp_port
    }

    async fn send(&mut self, request: &str) -> Result<()> {
        self.stream.write_all(request.as_bytes()).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn recv_response(&mut self) -> Result<Response> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(len) = rtsp::message_len(&self.buf) {
                if self.buf.len() >= len {
                    let msg = String::from_utf8_lossy(&self.buf[..len]).to_string();
                    self.buf.drain(..len);
                    return rtsp::parse_response(&msg);
                }
            }
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(Error::Closed("rtsp control connection closed".into()));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    async fn recv_rtp(&mut self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 65_536];
        let (n, _from) = self.udp.recv_from(&mut buf).await?;
        buf.truncate(n);
        Ok(buf)
    }

    async fn heartbeat(&mut self) -> Result<()> {
        if let Some((sock, target, payload)) = &self.heartbeat {
            sock.send_to(payload, *target).await?;
        }
        Ok(())
    }

    async fn close(&mut self) {
        let _ = self.stream.shutdown().await;
    }
}

#[async_trait]
impl LinkFactory for TcpLinkFactory {
    async fn connect(&self) -> Result<(Box<dyn Link>, u16)> {
        let addr = format!("{}:{}", self.host, self.rtsp_port);
        let stream = tokio::time::timeout(self.connect_timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| Error::Protocol(format!("rtsp connect timeout to {addr}")))??;
        stream.set_nodelay(true).ok();

        // 申請連號的 RTP/RTCP 埠；相機把 RTP 打去這裡（不管 SDP 與 Transport 說甚麼）。
        let (udp, rtp_port) = bind_udp_pair(self.bind_ip)?;

        let heartbeat = match &self.heartbeat {
            Some(hb) => {
                let bind = match self.bind_ip {
                    Some(ip) => SocketAddr::new(ip, 0),
                    None => "0.0.0.0:0".parse().unwrap(),
                };
                let sock = UdpSocket::bind(bind).await?;
                let target: SocketAddr = format!("{}:{}", self.host, hb.port)
                    .parse()
                    .map_err(|e| Error::Protocol(format!("bad heartbeat target: {e}")))?;
                Some((sock, target, hb.payload))
            }
            None => None,
        };

        Ok((
            Box::new(TcpLink { stream, buf: Vec::with_capacity(8192), udp, rtp_port, heartbeat }),
            rtp_port,
        ))
    }
}

fn bind_udp_pair(bind_ip: Option<std::net::IpAddr>) -> Result<(UdpSocket, u16)> {
    // tokio 的 UdpSocket::bind 是 async，這裡改用 std 同步 bind 再轉換，
    // 避免在非 async fn 內 await 的麻煩。
    use std::net::UdpSocket as StdUdp;
    let ip = bind_ip.unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let mut last_err = None;
    for port in (20_000u16..40_000).step_by(2) {
        let sock = StdUdp::bind(SocketAddr::new(ip, port));
        match sock {
            Ok(rtp) => {
                // RTCP 埠必須一併佔住，之後只用到 RTP 那一個。
                if StdUdp::bind(SocketAddr::new(ip, port + 1)).is_ok() {
                    rtp.set_read_timeout(None).ok();
                    rtp.set_nonblocking(true)?;
                    let sock = UdpSocket::from_std(rtp)?;
                    return Ok((sock, port));
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(Error::Io(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no free udp port pair")
    })))
}
