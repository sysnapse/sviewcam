//! NAL 輸出：probe 寫 Annex-B 檔或只計數。

use std::fs::File;
use std::io::{BufWriter, Write};

use crate::rtp::strip_leading_start_code;

pub trait NalSink: Send {
    /// `nal` 不含 Annex-B 起始碼。
    fn nal(&mut self, nal: &[u8]);
}

pub struct NullSink;

impl NalSink for NullSink {
    fn nal(&mut self, _nal: &[u8]) {}
}

/// 收在記憶體裡，測試用來驗 Annex-B 內容（例如不得出現雙起始碼）。
pub struct VecSink {
    pub data: Vec<u8>,
}

impl VecSink {
    pub fn new() -> Self {
        Self { data: Vec::new() }
    }

    pub fn count_start_codes(data: &[u8]) -> usize {
        data.windows(4).filter(|w| *w == [0, 0, 0, 1]).count()
    }
}

impl Default for VecSink {
    fn default() -> Self {
        Self::new()
    }
}

impl NalSink for VecSink {
    fn nal(&mut self, nal: &[u8]) {
        if nal.is_empty() {
            return;
        }
        self.data.extend_from_slice(&[0, 0, 0, 1]);
        self.data.extend_from_slice(strip_leading_start_code(nal));
    }
}

pub struct FileSink {
    writer: BufWriter<File>,
    pub bytes: u64,
}

impl FileSink {
    pub fn create(path: &std::path::Path) -> std::io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self { writer: BufWriter::new(file), bytes: 0 })
    }

    pub fn finish(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

impl NalSink for FileSink {
    fn nal(&mut self, nal: &[u8]) {
        if nal.is_empty() {
            return;
        }
        let body = strip_leading_start_code(nal);
        let _ = self.writer.write_all(&[0, 0, 0, 1]);
        let _ = self.writer.write_all(body);
        self.bytes += 4 + body.len() as u64;
    }
}
