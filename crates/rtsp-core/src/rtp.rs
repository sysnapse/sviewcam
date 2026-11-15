//! RTP 解析與 H.264 depacket（RFC 6184：single NAL / STAP-A / FU-A）→ Annex-B。

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct RtpPacket<'a> {
    pub payload_type: u8,
    pub marker: bool,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: &'a [u8],
}

pub fn parse(buf: &[u8]) -> Result<RtpPacket<'_>> {
    if buf.len() < 12 {
        return Err(Error::Protocol(format!("rtp packet too short: {} bytes", buf.len())));
    }
    if buf[0] >> 6 != 2 {
        return Err(Error::Protocol("rtp version != 2".into()));
    }
    let padding = buf[0] & 0x20 != 0;
    let extension = buf[0] & 0x10 != 0;
    let csrc_count = (buf[0] & 0x0f) as usize;
    let marker = buf[1] & 0x80 != 0;
    let payload_type = buf[1] & 0x7f;
    let seq = u16::from_be_bytes([buf[2], buf[3]]);
    let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);

    let mut offset = 12 + 4 * csrc_count;
    if extension {
        if buf.len() < offset + 4 {
            return Err(Error::Protocol("rtp extension header truncated".into()));
        }
        let words = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        offset += 4 + 4 * words;
    }
    if buf.len() < offset {
        return Err(Error::Protocol("rtp payload offset beyond packet".into()));
    }
    let mut end = buf.len();
    if padding {
        let pad = *buf.last().unwrap() as usize;
        if pad > 0 && pad <= end - offset {
            end -= pad;
        }
    }
    Ok(RtpPacket {
        payload_type,
        marker,
        seq,
        timestamp,
        ssrc,
        payload: &buf[offset..end],
    })
}

/// 組一個最小 RTP 封包（測試與 fixture 用）。
pub fn build(seq: u16, timestamp: u32, ssrc: u32, marker: bool, payload_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + payload.len());
    out.push(0x80);
    out.push((if marker { 0x80 } else { 0 }) | (payload_type & 0x7f));
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn strip_leading_start_code(data: &[u8]) -> &[u8] {
    if data.starts_with(&[0, 0, 0, 1]) {
        &data[4..]
    } else if data.starts_with(&[0, 0, 1]) {
        &data[3..]
    } else {
        data
    }
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1f).unwrap_or(0)
}

pub fn is_keyframe(nal: &[u8]) -> bool {
    matches!(nal_type(nal), 5 | 7)
}

/// 把 NAL（不含起始碼）寫成 Annex-B；每個 NAL 前只加一個 4-byte 起始碼。
pub fn write_annexb(nals: &[Vec<u8>], out: &mut Vec<u8>) {
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        let body = strip_leading_start_code(nal);
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(body);
    }
}

#[derive(Debug, Default)]
pub struct Depacketizer {
    fu: Option<FuBuffer>,
    /// 因換 timestamp 或中途出現 single NAL 而丟棄的不完整 FU 幀數。
    pub incomplete: u32,
}

#[derive(Debug)]
struct FuBuffer {
    timestamp: u32,
    data: Vec<u8>,
}

impl Depacketizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 餵入一個 RTP payload，回傳本包還原出的完整 NAL（可能 0 個或多個）。
    pub fn push(&mut self, payload: &[u8], timestamp: u32) -> Result<Vec<Vec<u8>>> {
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        let typ = payload[0] & 0x1f;
        match typ {
            1..=23 => {
                if self.fu.take().is_some() {
                    self.incomplete += 1;
                }
                Ok(vec![payload.to_vec()])
            }
            24 => {
                // STAP-A：一個包內多個 NAL
                let mut out = Vec::new();
                let mut i = 1;
                while i + 2 <= payload.len() {
                    let size = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
                    i += 2;
                    if i + size > payload.len() {
                        return Err(Error::Protocol("STAP-A length exceeds payload".into()));
                    }
                    out.push(payload[i..i + size].to_vec());
                    i += size;
                }
                Ok(out)
            }
            28 => {
                // FU-A：首包帶 NAL header 的 F|NRI + 原始 type
                if payload.len() < 3 {
                    return Err(Error::Protocol("FU-A too short".into()));
                }
                let fu_header = payload[1];
                let start = fu_header & 0x80 != 0;
                let end = fu_header & 0x40 != 0;
                let inner_type = fu_header & 0x1f;
                let reconstructed_header = (payload[0] & 0xe0) | inner_type;
                if start {
                    if self.fu.take().is_some() {
                        self.incomplete += 1;
                    }
                    let mut data = Vec::with_capacity(payload.len() * 4);
                    data.push(reconstructed_header);
                    data.extend_from_slice(&payload[2..]);
                    self.fu = Some(FuBuffer { timestamp, data });
                } else {
                    match self.fu.as_mut() {
                        Some(buf) if buf.timestamp == timestamp => buf.data.extend_from_slice(&payload[2..]),
                        Some(_) => {
                            self.fu = None;
                            self.incomplete += 1;
                        }
                        None => {}
                    }
                }
                if end {
                    if let Some(buf) = self.fu.take() {
                        return Ok(vec![buf.data]);
                    }
                }
                Ok(Vec::new())
            }
            25..=27 | 29..=31 => Ok(Vec::new()),
            _ => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rtp_header() {
        let pkt = build(65535, 1234, 0xdead_beef, true, 96, &[0x65, 1, 2, 3]);
        let p = parse(&pkt).unwrap();
        assert_eq!(p.seq, 65535);
        assert_eq!(p.timestamp, 1234);
        assert_eq!(p.ssrc, 0xdead_beef);
        assert!(p.marker);
        assert_eq!(p.payload_type, 96);
        assert_eq!(p.payload, &[0x65, 1, 2, 3]);
    }

    #[test]
    fn reassembles_fu_a() {
        let nal = vec![0x65, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
        let mut d = Depacketizer::new();
        // start
        let mut p1 = vec![0x7c, 0x85];
        p1.extend_from_slice(&nal[1..3]);
        assert!(d.push(&p1, 7).unwrap().is_empty());
        let mut p2 = vec![0x7c, 0x45];
        p2.extend_from_slice(&nal[3..]);
        let out = d.push(&p2, 7).unwrap();
        assert_eq!(out, vec![nal]);
    }

    #[test]
    fn reassembles_stap_a() {
        let mut payload = vec![24u8];
        for nal in [vec![0x67, 1], vec![0x68, 2]] {
            payload.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            payload.extend_from_slice(&nal);
        }
        let out = Depacketizer::new().push(&payload, 1).unwrap();
        assert_eq!(out, vec![vec![0x67, 1], vec![0x68, 2]]);
    }

    #[test]
    fn dropped_fu_is_counted_not_emitted() {
        let mut d = Depacketizer::new();
        let mut p1 = vec![0x7c, 0x85, 0x65];
        p1.push(0x01);
        assert!(d.push(&p1, 10).unwrap().is_empty());
        // 新 timestamp 直接來 FU 中段：舊的不完整幀應被丟棄並計數
        let p2 = vec![0x7c, 0x45, 0x02];
        assert!(d.push(&p2, 20).unwrap().is_empty());
        assert_eq!(d.incomplete, 1);
    }

    #[test]
    fn annexb_never_doubles_start_code() {
        let mut out = Vec::new();
        write_annexb(&[vec![0, 0, 0, 1, 0x67, 0x11]], &mut out);
        assert_eq!(out, vec![0, 0, 0, 1, 0x67, 0x11]);
    }
}
