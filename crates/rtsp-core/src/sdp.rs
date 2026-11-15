//! 最小 SDP 解析（只取實際會用的欄位，文檔 §5）。
//!
//! 紀律：
//! - `profile-level-id`、`a=bitrate`、`a=framerate` 都不可信（文檔 §5 實測不一致），
//!   所以刻意不解析成決策依據；只保留作日誌。
//! - `sprop-parameter-sets` 的 base64 本身已含 Annex-B 起始碼，這裡一律剝掉。

use base64::Engine;

use crate::rtp::strip_leading_start_code;

#[derive(Debug, Clone, Default)]
pub struct SdpInfo {
    pub sprop: Vec<Vec<u8>>,
    pub dimensions: Option<(u32, u32)>,
    pub control_base: Option<String>,
    pub token: Option<String>,
    pub raw: String,
}

impl SdpInfo {
    /// SPS（NAL type 7）與 PPS（type 8），已剝起始碼。
    pub fn parameter_sets(&self) -> Vec<&[u8]> {
        self.sprop
            .iter()
            .filter(|b| !b.is_empty())
            .map(|b| b.as_slice())
            .collect()
    }
}

pub fn parse(text: &str) -> SdpInfo {
    let mut info = SdpInfo { raw: text.to_string(), ..Default::default() };
    let b64 = base64::engine::general_purpose::STANDARD;

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("a=control:") {
            // 相機的 SETUP 打在 URL 根，control 只記錄不採用（文檔 §5）。
            info.control_base = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("a=x-dimensions:") {
            // 注意：`x-dimensions` 不是標準屬性，出現與否均不影響開流。
            if let Some((w, h)) = rest.split_once(',') {
                if let (Ok(w), Ok(h)) = (w.trim().parse(), h.trim().parse()) {
                    info.dimensions = Some((w, h));
                }
            }
        } else if let Some(rest) = line.strip_prefix("a=SViewWifi:") {
            info.token = Some(rest.trim().to_string());
        } else if let Some(pos) = line.find("sprop-parameter-sets=") {
            let tail = &line[pos + "sprop-parameter-sets=".len()..];
            let tail = tail.split(';').next().unwrap_or(tail);
            for item in tail.split(',') {
                let item = item.trim();
                if item.is_empty() {
                    continue;
                }
                if let Ok(bytes) = b64.decode(item) {
                    info.sprop.push(strip_leading_start_code(&bytes).to_vec());
                }
            }
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDP: &str = "v=0\r\n\
o=-2209971456 2209971456 IN IP4 192.168.1.1\r\n\
s=Unnamed\r\n\
c=IN IP4 192.168.1.11\r\n\
a=recvonly\r\n\
m=video 5004 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1;profile-level-id=640029;sprop-parameter-sets=AAAAAWdNADKWNUBQAWtNwEBAQIA=,AAAAAWjuMbI=;\r\n\
a=control:trackID=0\r\n\
a=x-dimensions:2560,1440\r\n\
a=framerate:30\r\n\
a=bitrate:240000\r\n\
a=SViewWifi:12345678901234567890123456789012\r\n";

    #[test]
    fn parses_sprop_without_double_start_code() {
        let info = parse(SDP);
        assert_eq!(info.sprop.len(), 2);
        // 原始 base64 以 00 00 00 01 開頭，剝掉後第一 byte 應為 SPS NAL header 0x67
        assert_eq!(info.sprop[0][0] & 0x1f, 7, "SPS NAL type");
        assert_eq!(info.sprop[1][0] & 0x1f, 8, "PPS NAL type");
        assert!(!info.sprop[0].starts_with(&[0, 0, 0, 1]));
    }

    #[test]
    fn parses_dimensions_and_token() {
        let info = parse(SDP);
        assert_eq!(info.dimensions, Some((2560, 1440)));
        assert_eq!(info.control_base.as_deref(), Some("trackID=0"));
        assert_eq!(info.token.as_deref().map(str::len), Some(32));
    }
}
