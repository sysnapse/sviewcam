//! 最小 EBML／Matroska 寫入器與掃描器（單視訊軌 H.264/AVC）。
//!
//! 為什麼自寫：`matroska` 只能解析不能寫、`mkv` crate 已死、`mkv-element` 換不到
//! segment/cluster 串流語義、`ebml-webm` 對 unknown-size 截斷不透明。而 AC3 的
//! 復原驗證本來就需要 EBML scanner，寫與讀共用同一套編碼，少一個依賴面。
//!
//! 檔案佈局（刻意極簡，工具與 ffprobe 都吃得下）：
//!
//! ```text
//! EBML header
//! Segment (unknown size)
//!   Info   (TimecodeScale = 1e6 → 毫秒)
//!   Tracks (TrackType=1 video, CodecID=V_MPEG4/ISO/AVC, CodecPrivate=AVCC)
//!   Cluster* (known size, Timecode + SimpleBlock*)
//! ```
//!
//! 段檔案預配置 [`DEFAULT_PREALLOC_BYTES`] bytes，收段時截到實際長度；因此「檔案
//! 尾端有一段零填充」只會出現在未收段的 `.mkv.part` 上，正是復原掃描要處理的情形。
//!
//! [`DEFAULT_PREALLOC_BYTES`]: crate::config::DEFAULT_PREALLOC_BYTES

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{Error, Result};

// ---- EBML element ids（含長度標記位元）------------------------------------------------

const ID_EBML: u32 = 0x1A45_DFA3;
const ID_DOCTYPE: u32 = 0x4282;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_A966;
const ID_TIMECODE_SCALE: u32 = 0x2AD7B1;
const ID_MUXING_APP: u32 = 0x4D80;
const ID_WRITING_APP: u32 = 0x5741;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_UID: u32 = 0x73C5;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_FLAG_LACING: u32 = 0x9C;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63A2;
const ID_VIDEO: u32 = 0xE0;
const ID_PIXEL_WIDTH: u32 = 0xB0;
const ID_PIXEL_HEIGHT: u32 = 0xBA;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_CLUSTER_TIMECODE: u32 = 0xE7;
const ID_SIMPLE_BLOCK: u32 = 0xA3;

/// 單一 Cluster 的最長時間跨度（毫秒）。超過即開新 Cluster。
const CLUSTER_MAX_MS: u64 = 30_000;
/// 視訊軌編號（只支援單軌）。
const TRACK_NUMBER: u64 = 1;

// ---- 編碼工具 -----------------------------------------------------------------------

fn id_len(id: u32) -> usize {
    if id >= 0x0100_0000 {
        4
    } else if id >= 0x0001_0000 {
        3
    } else if id >= 0x0000_0100 {
        2
    } else {
        1
    }
}

fn push_id(out: &mut Vec<u8>, id: u32) {
    let n = id_len(id);
    for i in (0..n).rev() {
        out.push((id >> (8 * i)) as u8);
    }
}

/// 可容納 `n` 的最短 vint 寬度。
fn size_len(n: u64) -> usize {
    for w in 1..=8usize {
        // 寬度 w 可表示 0 ..= 2^(7w)-2（全 1 保留給 unknown size）
        if n < (1u64 << (7 * w)) - 1 {
            return w;
        }
    }
    8
}

fn push_size(out: &mut Vec<u8>, n: u64) {
    let w = size_len(n);
    for i in (0..w).rev() {
        let mut b = ((n >> (8 * i)) & 0xFF) as u8;
        if i == w - 1 {
            b |= 0x80u8 >> (w - 1);
        }
        out.push(b);
    }
}

fn uint_bytes(v: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    let mut started = false;
    for i in (0..8).rev() {
        let b = ((v >> (8 * i)) & 0xFF) as u8;
        if b != 0 || started || i == 0 {
            started = true;
            out.push(b);
        }
    }
    out
}

fn push_elem(out: &mut Vec<u8>, id: u32, body: &[u8]) {
    push_id(out, id);
    push_size(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn push_elem_uint(out: &mut Vec<u8>, id: u32, v: u64) {
    push_elem(out, id, &uint_bytes(v));
}

fn push_elem_str(out: &mut Vec<u8>, id: u32, s: &str) {
    push_elem(out, id, s.as_bytes());
}

// ---- AVCC / H.264 -------------------------------------------------------------------

/// 一段 H.264 串流需要的參數集（Annex-B 格式，可帶或不帶 start code）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfig {
    /// SPS（type 7）NAL，不含 start code。
    pub sps: Vec<u8>,
    /// PPS（type 8）NAL，不含 start code。
    pub pps: Vec<u8>,
}

impl AvcConfig {
    /// 從 Annex-B 位元組建構（自動剝掉 3／4 bytes start code）。
    pub fn from_annexb(sps: &[u8], pps: &[u8]) -> Self {
        Self {
            sps: rtsp_core::rtp::strip_leading_start_code(sps).to_vec(),
            pps: rtsp_core::rtp::strip_leading_start_code(pps).to_vec(),
        }
    }

    /// 建 AVCDecoderConfigurationRecord（Matroska `CodecPrivate`）。
    pub fn codec_private(&self) -> Result<Vec<u8>> {
        if self.sps.len() < 4 {
            return Err(Error::Mkv(format!("SPS 太短（{} bytes）", self.sps.len())));
        }
        let mut out = Vec::with_capacity(11 + self.sps.len() + self.pps.len());
        out.push(1); // configurationVersion
        out.extend_from_slice(&self.sps[1..4]); // profile / compat / level
        out.push(0xFF); // 4-byte NAL length
        out.push(0xE1); // numOfSequenceParameterSets = 1
        out.extend_from_slice(&(self.sps.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.sps);
        out.push(1); // numOfPictureParameterSets
        out.extend_from_slice(&(self.pps.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.pps);
        Ok(out)
    }

    /// 把一個 NAL 以 AVCC 長度前綴追加進 `out`（Matroska SimpleBlock 的內容）。
    pub fn append_avcc(nal: &[u8], out: &mut Vec<u8>) {
        let nal = rtsp_core::rtp::strip_leading_start_code(nal);
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
}

/// 把一個 Annex-B NAL 轉成 AVCC 幀（4-byte 長度前綴）。
pub fn avcc_frame(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len() + 4);
    AvcConfig::append_avcc(nal, &mut out);
    out
}

// ---- 寫入器 -------------------------------------------------------------------------

/// 收段後的實際檔案資訊（寫進 SQLite 索引）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentSummary {
    /// 實際檔案 bytes（已截掉預配置尾巴）。
    pub bytes: u64,
    /// 首個 block 的時間（毫秒，host 時鐘）。
    pub t_start: u64,
    /// 最後一個 block 的時間（毫秒）。
    pub t_end: u64,
    /// 首個 keyframe block 在檔案內的位元組位移（0 表示沒有 keyframe）。
    pub idr_offset: u64,
    /// block 數量。
    pub blocks: usize,
}

struct Cluster {
    timecode: u64,
    body: Vec<u8>,
    blocks: usize,
    keyframe_body_off: Option<u64>,
}

/// 一個 MKV 段檔的寫入器（單軌 H.264）。
pub struct MkvWriter {
    file: File,
    written: u64,
    cluster: Option<Cluster>,
    t_start: Option<u64>,
    t_end: u64,
    blocks: usize,
    first_keyframe_offset: Option<u64>,
}

impl MkvWriter {
    /// 建立段檔並寫好 header／Tracks，之後預配置到 `prealloc_bytes`。
    pub fn create(
        path: impl AsRef<Path>,
        avc: &AvcConfig,
        width: u32,
        height: u32,
        prealloc_bytes: u64,
    ) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path.as_ref())?;

        let codec_private = avc.codec_private()?;
        let mut head = Vec::with_capacity(512);

        // EBML header
        let mut ebml = Vec::new();
        push_elem_str(&mut ebml, ID_DOCTYPE, "matroska");
        push_elem_uint(&mut ebml, 0x4287, 4); // DocTypeVersion
        push_elem_uint(&mut ebml, 0x4285, 2); // DocTypeReadVersion
        push_elem(&mut head, ID_EBML, &ebml);

        // Segment（unknown size：0x01 FF FF FF FF FF FF FF）
        push_id(&mut head, ID_SEGMENT);
        head.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);

        // Info
        let mut info = Vec::new();
        push_elem_uint(&mut info, ID_TIMECODE_SCALE, 1_000_000);
        push_elem_str(&mut info, ID_MUXING_APP, "camcore 0.1");
        push_elem_str(&mut info, ID_WRITING_APP, "camcore 0.1");
        push_elem(&mut head, ID_INFO, &info);

        // Tracks
        let mut video = Vec::new();
        push_elem_uint(&mut video, ID_PIXEL_WIDTH, width as u64);
        push_elem_uint(&mut video, ID_PIXEL_HEIGHT, height as u64);
        let mut track = Vec::new();
        push_elem_uint(&mut track, ID_TRACK_NUMBER, TRACK_NUMBER);
        push_elem_uint(&mut track, ID_TRACK_UID, 1);
        push_elem_uint(&mut track, ID_TRACK_TYPE, 1); // video
        push_elem_uint(&mut track, ID_FLAG_LACING, 0);
        push_elem_str(&mut track, ID_CODEC_ID, "V_MPEG4/ISO/AVC");
        push_elem(&mut track, ID_CODEC_PRIVATE, &codec_private);
        push_elem(&mut track, ID_VIDEO, &video);
        let mut tracks = Vec::new();
        push_elem(&mut tracks, ID_TRACK_ENTRY, &track);
        push_elem(&mut head, ID_TRACKS, &tracks);

        file.write_all(&head)?;
        let written = head.len() as u64;
        if prealloc_bytes > written {
            file.set_len(prealloc_bytes)?;
        }
        Ok(Self {
            file,
            written,
            cluster: None,
            t_start: None,
            t_end: 0,
            blocks: 0,
            first_keyframe_offset: None,
        })
    }

    /// 目前寫入的位元組數（不含預配置尾巴）。
    pub fn written(&self) -> u64 {
        self.written
    }

    /// 追加一個 block。
    ///
    /// - `payload`：AVCC 幀（可多個 NAL：`[len][nal]...`），通常是 SPS+PPS+IDR。
    /// - `at_ms`：host 時鐘毫秒（單調）。
    /// - `keyframe`：是否為可獨立解碼點（IDR）。
    pub fn append_block(&mut self, payload: &[u8], at_ms: u64, keyframe: bool) -> Result<()> {
        if self.t_start.is_none() {
            self.t_start = Some(at_ms);
        }
        let need_new = match &self.cluster {
            None => true,
            Some(c) => at_ms.saturating_sub(c.timecode) >= CLUSTER_MAX_MS,
        };
        if need_new {
            self.flush_cluster()?;
            self.cluster = Some(Cluster {
                timecode: at_ms,
                body: Vec::with_capacity(16 * 1024),
                blocks: 0,
                keyframe_body_off: None,
            });
        }

        let cluster = self.cluster.as_mut().expect("cluster 剛建立");
        if cluster.body.is_empty() {
            let mut tc = Vec::new();
            push_elem_uint(&mut tc, ID_CLUSTER_TIMECODE, cluster.timecode);
            cluster.body.extend_from_slice(&tc);
        }
        let rel = at_ms.saturating_sub(cluster.timecode).min(i16::MAX as u64) as i16;
        if keyframe && cluster.keyframe_body_off.is_none() {
            cluster.keyframe_body_off = Some(cluster.body.len() as u64);
        }

        let mut block = Vec::with_capacity(payload.len() + 8);
        block.push(0x80 | (TRACK_NUMBER as u8)); // track number vint
        block.extend_from_slice(&rel.to_be_bytes());
        block.push(if keyframe { 0x80 } else { 0x00 });
        block.extend_from_slice(payload);
        push_elem(&mut cluster.body, ID_SIMPLE_BLOCK, &block);

        cluster.blocks += 1;
        self.blocks += 1;
        if at_ms > self.t_end {
            self.t_end = at_ms;
        }
        Ok(())
    }

    fn flush_cluster(&mut self) -> Result<()> {
        let Some(cluster) = self.cluster.take() else {
            return Ok(());
        };
        let size = cluster.body.len() as u64;
        let header = {
            let mut h = Vec::with_capacity(12);
            push_id(&mut h, ID_CLUSTER);
            push_size(&mut h, size);
            h
        };
        let cluster_off = self.written;
        if self.first_keyframe_offset.is_none() {
            if let Some(off) = cluster.keyframe_body_off {
                self.first_keyframe_offset = Some(cluster_off + header.len() as u64 + off);
            }
        }
        self.file.seek(SeekFrom::Start(cluster_off))?;
        self.file.write_all(&header)?;
        self.file.write_all(&cluster.body)?;
        self.written += header.len() as u64 + size;
        Ok(())
    }

    /// 收段：flush 最後一個 cluster、截掉預配置尾巴。
    pub fn finish(&mut self) -> Result<SegmentSummary> {
        self.flush_cluster()?;
        self.file.set_len(self.written)?;
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(SegmentSummary {
            bytes: self.written,
            t_start: self.t_start.unwrap_or(0),
            t_end: self.t_end,
            idr_offset: self.first_keyframe_offset.unwrap_or(0),
            blocks: self.blocks,
        })
    }
}

// ---- 掃描器 -------------------------------------------------------------------------

/// 掃描一個 MKV 檔得到的結構資訊。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MkvInfo {
    /// `DocType`（應為 `matroska`）。
    pub doc_type: String,
    /// `TimecodeScale`（奈秒）；1000000 表示時間單位是毫秒。
    pub timecode_scale: u64,
    /// 軌道 codec id（應為 `V_MPEG4/ISO/AVC`）。
    pub codec_id: String,
    /// 視訊寬。
    pub width: u32,
    /// 視訊高。
    pub height: u32,
    /// 完整 Cluster 數。
    pub clusters: usize,
    /// block 數（只算完整 Cluster 內的）。
    pub blocks: usize,
    /// keyframe block 數。
    pub keyframe_blocks: usize,
    /// 首個 Cluster 時間（毫秒）。
    pub t_start_ms: u64,
    /// 最後一個 block 時間（毫秒）。
    pub t_end_ms: u64,
    /// 檔案總 bytes。
    pub bytes: u64,
    /// 是否在尾端被截斷（有未完成元素）。
    pub truncated: bool,
    /// 最後一個**完整** Cluster 的結尾位元組位移（復原的關鍵）。
    pub last_complete_cluster_end: Option<u64>,
    /// 首個 keyframe SimpleBlock 元素的絕對位元組位移。
    pub first_keyframe_offset: Option<u64>,
}

impl MkvInfo {
    /// 是否為可用的已閉合段：header/tracks 正常、至少一個完整 cluster、未被截斷。
    pub fn is_closed(&self) -> bool {
        self.doc_type == "matroska"
            && self.codec_id == "V_MPEG4/ISO/AVC"
            && self.clusters > 0
            && !self.truncated
            && self.last_complete_cluster_end.is_some()
    }

    /// 最後一個完整 Cluster 結尾；沒有完整 Cluster 時為 `None`。
    pub fn complete_end(&self) -> Option<u64> {
        self.last_complete_cluster_end
    }
}

fn read_id(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let b = *buf
        .get(*pos)
        .ok_or_else(|| Error::Mkv("讀 id 時遇到檔尾".into()))?;
    let len = if b & 0x80 != 0 {
        1
    } else if b & 0x40 != 0 {
        2
    } else if b & 0x20 != 0 {
        3
    } else if b & 0x10 != 0 {
        4
    } else {
        return Err(Error::Mkv(format!("非法 EBML id 首字节 {b:#04x}")));
    };
    if *pos + len > buf.len() {
        return Err(Error::Mkv("id 被截斷".into()));
    }
    let mut id: u32 = 0;
    for i in 0..len {
        id = (id << 8) | buf[*pos + i] as u32;
    }
    *pos += len;
    Ok(id)
}

/// 讀 vint size；`None` 代表 unknown size。
fn read_size(buf: &[u8], pos: &mut usize) -> Result<Option<u64>> {
    let b = *buf
        .get(*pos)
        .ok_or_else(|| Error::Mkv("讀 size 時遇到檔尾".into()))?;
    let len = if b & 0x80 != 0 {
        1
    } else if b & 0x40 != 0 {
        2
    } else if b & 0x20 != 0 {
        3
    } else if b & 0x10 != 0 {
        4
    } else if b & 0x08 != 0 {
        5
    } else if b & 0x04 != 0 {
        6
    } else if b & 0x02 != 0 {
        7
    } else if b & 0x01 != 0 {
        8
    } else {
        return Err(Error::Mkv(format!("非法 size 首字节 {b:#04x}")));
    };
    if *pos + len > buf.len() {
        return Err(Error::Mkv("size 被截斷".into()));
    }
    let mut value: u64 = (b as u64) & (0xFFu64 >> len);
    let all_ones: u64 = (1u64 << (7 * len)) - 1;
    for i in 1..len {
        value = (value << 8) | buf[*pos + i] as u64;
    }
    *pos += len;
    if value == all_ones {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn read_uint(buf: &[u8]) -> u64 {
    let mut v = 0u64;
    for b in buf {
        v = (v << 8) | *b as u64;
    }
    v
}

fn read_str(buf: &[u8]) -> String {
    let s: Vec<u8> = buf.iter().copied().take_while(|b| *b != 0).collect();
    String::from_utf8_lossy(&s).into_owned()
}

/// 掃描檔案。
pub fn scan(path: impl AsRef<Path>) -> Result<MkvInfo> {
    let bytes = std::fs::read(path.as_ref())?;
    scan_bytes(&bytes)
}

/// 掃描記憶體中的位元組（段檔都是 MB 級，不做串流讀取；12 Mbps × 30 分鐘的
/// 總量落在磁碟、不進記憶體）。
pub fn scan_bytes(bytes: &[u8]) -> Result<MkvInfo> {
    let mut pos = 0usize;
    let mut info = MkvInfo {
        bytes: bytes.len() as u64,
        ..Default::default()
    };

    // EBML header
    let id = read_id(bytes, &mut pos)?;
    if id != ID_EBML {
        return Err(Error::Mkv(format!("首元素不是 EBML header（{id:#010x}）")));
    }
    let size = read_size(bytes, &mut pos)?
        .ok_or_else(|| Error::Mkv("EBML header 不得是 unknown size".into()))?;
    let head_end = pos
        .checked_add(size as usize)
        .ok_or_else(|| Error::Mkv("EBML header 長度溢位".into()))?;
    if head_end > bytes.len() {
        return Err(Error::Mkv("EBML header 被截斷".into()));
    }
    {
        let mut p = pos;
        while p < head_end {
            let cid = read_id(bytes, &mut p)?;
            let csz = read_size(bytes, &mut p)?;
            let Some(csz) = csz else {
                return Err(Error::Mkv("EBML header 子元素 unknown size".into()));
            };
            let end = p
                .checked_add(csz as usize)
                .ok_or_else(|| Error::Mkv("子元素長度溢位".into()))?;
            if end > head_end {
                return Err(Error::Mkv("EBML header 子元素被截斷".into()));
            }
            if cid == ID_DOCTYPE {
                info.doc_type = read_str(&bytes[p..end]);
            }
            p = end;
        }
    }
    pos = head_end;

    // Segment
    let sid = read_id(bytes, &mut pos)?;
    if sid != ID_SEGMENT {
        return Err(Error::Mkv(format!("第二個元素不是 Segment（{sid:#010x}）")));
    }
    let seg_size = read_size(bytes, &mut pos)?;
    let seg_end = match seg_size {
        Some(n) => (pos as u64)
            .checked_add(n)
            .ok_or_else(|| Error::Mkv("Segment 長度溢位".into()))?
            .min(bytes.len() as u64) as usize,
        None => bytes.len(),
    };

    // Segment 子元素
    while pos < seg_end {
        let eid = match read_id(bytes, &mut pos) {
            Ok(v) => v,
            Err(_) => {
                info.truncated = true;
                break;
            }
        };
        let esz = match read_size(bytes, &mut pos) {
            Ok(v) => v,
            Err(_) => {
                info.truncated = true;
                break;
            }
        };
        let Some(esz) = esz else {
            // unknown-size 的 Cluster 尾段：我們自己的寫入器不會產生，視為截斷。
            info.truncated = true;
            break;
        };
        let end = match (pos as u64).checked_add(esz) {
            Some(v) => v.min(u64::MAX) as usize,
            None => {
                info.truncated = true;
                break;
            }
        };
        if end > bytes.len() {
            info.truncated = true;
            break;
        }
        match eid {
            ID_INFO => parse_info(bytes, pos, end, &mut info)?,
            ID_TRACKS => parse_tracks(bytes, pos, end, &mut info)?,
            ID_CLUSTER => {
                parse_cluster(bytes, pos, end, &mut info)?;
                info.last_complete_cluster_end = Some(end as u64);
            }
            _ => {}
        }
        pos = end;
    }

    Ok(info)
}

fn parse_info(bytes: &[u8], start: usize, end: usize, info: &mut MkvInfo) -> Result<()> {
    let mut p = start;
    while p < end {
        let id = read_id(bytes, &mut p)?;
        let sz = read_size(bytes, &mut p)?
            .ok_or_else(|| Error::Mkv("Info 子元素 unknown size".into()))?;
        let cend = p + sz as usize;
        if cend > end {
            return Err(Error::Mkv("Info 子元素被截斷".into()));
        }
        if id == ID_TIMECODE_SCALE {
            info.timecode_scale = read_uint(&bytes[p..cend]);
        }
        p = cend;
    }
    Ok(())
}

fn parse_tracks(bytes: &[u8], start: usize, end: usize, info: &mut MkvInfo) -> Result<()> {
    let mut p = start;
    while p < end {
        let id = read_id(bytes, &mut p)?;
        let Some(sz) = read_size(bytes, &mut p)? else {
            return Err(Error::Mkv("Tracks 子元素 unknown size".into()));
        };
        let cend = p + sz as usize;
        if cend > end {
            return Err(Error::Mkv("Tracks 子元素被截斷".into()));
        }
        if id == ID_TRACK_ENTRY {
            parse_track_entry(bytes, p, cend, info)?;
        }
        p = cend;
    }
    Ok(())
}

fn parse_track_entry(bytes: &[u8], start: usize, end: usize, info: &mut MkvInfo) -> Result<()> {
    let mut p = start;
    while p < end {
        let id = read_id(bytes, &mut p)?;
        let Some(sz) = read_size(bytes, &mut p)? else {
            return Err(Error::Mkv("TrackEntry 子元素 unknown size".into()));
        };
        let cend = p + sz as usize;
        if cend > end {
            return Err(Error::Mkv("TrackEntry 子元素被截斷".into()));
        }
        match id {
            ID_CODEC_ID => info.codec_id = read_str(&bytes[p..cend]),
            ID_VIDEO => {
                let mut vp = p;
                while vp < cend {
                    let vid = read_id(bytes, &mut vp)?;
                    let Some(vsz) = read_size(bytes, &mut vp)? else {
                        break;
                    };
                    let vend = vp + vsz as usize;
                    if vend > cend {
                        break;
                    }
                    match vid {
                        ID_PIXEL_WIDTH => info.width = read_uint(&bytes[vp..vend]) as u32,
                        ID_PIXEL_HEIGHT => info.height = read_uint(&bytes[vp..vend]) as u32,
                        _ => {}
                    }
                    vp = vend;
                }
            }
            _ => {}
        }
        p = cend;
    }
    Ok(())
}

fn parse_cluster(bytes: &[u8], start: usize, end: usize, info: &mut MkvInfo) -> Result<()> {
    let mut p = start;
    let mut timecode: Option<u64> = None;
    let mut max_rel: i64 = 0;
    let mut blocks = 0usize;
    let mut keyframes = 0usize;
    while p < end {
        let elem_start = p;
        let id = read_id(bytes, &mut p)?;
        let Some(sz) = read_size(bytes, &mut p)? else {
            return Err(Error::Mkv("Cluster 子元素 unknown size".into()));
        };
        let cend = p + sz as usize;
        if cend > end {
            return Err(Error::Mkv("Cluster 子元素被截斷".into()));
        }
        match id {
            ID_CLUSTER_TIMECODE => timecode = Some(read_uint(&bytes[p..cend])),
            ID_SIMPLE_BLOCK => {
                if cend <= p + 3 {
                    return Err(Error::Mkv("SimpleBlock 太短".into()));
                }
                // 跳過 track vint（單軌固定 1 byte）
                let rel = i16::from_be_bytes([bytes[p + 1], bytes[p + 2]]) as i64;
                let flags = bytes[p + 3];
                if flags & 0x80 != 0 {
                    keyframes += 1;
                    if info.first_keyframe_offset.is_none() {
                        info.first_keyframe_offset = Some(elem_start as u64);
                    }
                }
                if rel > max_rel {
                    max_rel = rel;
                }
                blocks += 1;
            }
            _ => {}
        }
        p = cend;
    }
    let tc = timecode.ok_or_else(|| Error::Mkv("Cluster 缺 Timecode".into()))?;
    if info.clusters == 0 {
        info.t_start_ms = tc;
    }
    let end_ms = tc.saturating_add(max_rel.max(0) as u64);
    if end_ms > info.t_end_ms {
        info.t_end_ms = end_ms;
    }
    info.clusters += 1;
    info.blocks += blocks;
    info.keyframe_blocks += keyframes;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avc() -> AvcConfig {
        AvcConfig {
            sps: vec![0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9],
            pps: vec![0x68, 0xEE, 0x3C, 0x80],
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("camcore-mkv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn vint_roundtrip_sizes() {
        for n in [0u64, 1, 126, 127, 128, 16_382, 16_383, 1 << 20, 1 << 40] {
            let mut buf = Vec::new();
            push_size(&mut buf, n);
            let mut pos = 0;
            let back = read_size(&buf, &mut pos).unwrap();
            assert_eq!(back, Some(n), "n={n}");
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn writer_then_scan_roundtrip() {
        let path = tmp("roundtrip.mkv");
        let mut w = MkvWriter::create(&path, &avc(), 2560, 1440, 1 << 20).unwrap();
        let mut payload = Vec::new();
        AvcConfig::append_avcc(&avc().sps, &mut payload);
        AvcConfig::append_avcc(&avc().pps, &mut payload);
        AvcConfig::append_avcc(&[0x65, 1, 2, 3], &mut payload);
        w.append_block(&payload, 0, true).unwrap();
        for i in 1..10u64 {
            w.append_block(&avcc_frame(&[0x41, i as u8]), i * 40, false)
                .unwrap();
        }
        let sum = w.finish().unwrap();
        assert_eq!(sum.t_start, 0);
        assert_eq!(sum.t_end, 360);
        assert!(sum.idr_offset > 0, "keyframe 位移應被記錄");

        let info = scan(&path).unwrap();
        assert_eq!(info.doc_type, "matroska");
        assert_eq!(info.codec_id, "V_MPEG4/ISO/AVC");
        assert_eq!(info.width, 2560);
        assert_eq!(info.height, 1440);
        assert_eq!(info.clusters, 1);
        assert_eq!(info.blocks, 10);
        assert_eq!(info.keyframe_blocks, 1);
        assert_eq!(info.t_start_ms, 0);
        assert_eq!(info.t_end_ms, 360);
        assert_eq!(info.bytes, sum.bytes);
        assert!(!info.truncated);
        assert!(info.is_closed());
        assert_eq!(info.last_complete_cluster_end, Some(sum.bytes));
        assert_eq!(info.first_keyframe_offset, Some(sum.idr_offset));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn scan_handles_mid_simpleblock_truncation() {
        let avc = avc();
        // 檔案 C：單一 cluster，用來取得「cluster 1 結尾」的位移
        let c_path = tmp("one-cluster.mkv");
        let mut wc = MkvWriter::create(&c_path, &avc, 640, 480, 0).unwrap();
        wc.append_block(&avcc_frame(&[0x65, 9, 9, 9]), 0, true).unwrap();
        let first = wc.finish().unwrap();

        // 檔案 B：兩個 cluster（第二個在 40 s，超過 CLUSTER_MAX_MS）
        let path = tmp("mid-block.mkv");
        let mut w = MkvWriter::create(&path, &avc, 640, 480, 0).unwrap();
        w.append_block(&avcc_frame(&[0x65, 9, 9, 9]), 0, true).unwrap();
        w.append_block(&avcc_frame(&[0x41, 1, 2, 3, 4, 5, 6, 7]), 40_000, false)
            .unwrap();
        let full = w.finish().unwrap();
        assert!(full.bytes > first.bytes + 12);

        // 截在第二個 cluster 的 SimpleBlock 中間
        let bytes = std::fs::read(&path).unwrap();
        let cut = bytes.len() - 12;
        let part = tmp("mid-block.mkv.part");
        std::fs::write(&part, &bytes[..cut]).unwrap();
        let info = scan(&part).unwrap();
        assert!(info.truncated, "截斷檔必須標記 truncated");
        assert_eq!(info.clusters, 1);
        assert_eq!(info.blocks, 1);
        assert_eq!(info.last_complete_cluster_end, Some(first.bytes));
        assert_eq!(info.first_keyframe_offset, Some(first.idr_offset));
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&c_path);
    }

    #[test]
    fn scan_flags_truncated_tracks_dir() {
        let path = tmp("mid-tracks.mkv");
        let mut w = MkvWriter::create(&path, &avc(), 640, 480, 0).unwrap();
        w.append_block(&avcc_frame(&[0x65, 1, 2, 3]), 0, true).unwrap();
        w.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        // 截在 Segment 內：沒有一個完整 cluster，掃描要標 truncated 且沒有 complete_end
        let part = tmp("mid-tracks.mkv.part");
        std::fs::write(&part, &bytes[..40]).unwrap();
        let info = scan(&part).unwrap();
        assert!(info.truncated);
        assert_eq!(info.clusters, 0);
        assert_eq!(info.complete_end(), None);
        assert!(!info.is_closed());
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn scan_rejects_non_ebml_header() {
        let path = tmp("garbage.mkv");
        std::fs::write(&path, b"Not an EBML file at all").unwrap();
        let err = scan(&path).unwrap_err();
        assert!(format!("{err}").contains("EBML"), "err={err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn scan_handles_empty_tail_after_cluster() {
        let path = tmp("zero-tail.mkv");
        let mut w = MkvWriter::create(&path, &avc(), 640, 480, 64 * 1024).unwrap();
        w.append_block(&avcc_frame(&[0x65, 1, 2, 3]), 0, true).unwrap();
        let sum = w.finish().unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        // 預配置留下的零尾巴（模擬未收段）
        bytes.extend_from_slice(&[0u8; 32]);
        let part = tmp("zero-tail.mkv.part");
        std::fs::write(&part, &bytes).unwrap();
        let info = scan(&part).unwrap();
        assert!(info.truncated);
        assert_eq!(info.last_complete_cluster_end, Some(sum.bytes));
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_file(&path);
    }
}
