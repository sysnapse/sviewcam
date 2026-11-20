//! 環形緩衝：把 NAL 串流切成 2 秒 MKV 段、寫索引、滿窗刪最舊。
//!
//! 切段規則：
//!
//! - **GOP 對齊**：段長滿 [`RingConfig::segment_ms`] 後，等到下一個 IDR（NAL type 5）
//!   才切；切點段的第一個 block 一定帶 SPS+PPS。
//! - **強切**：若一直等不到 IDR 而段已滿 [`RingConfig::force_cut_ms`]，就在此處硬切
//!   並記一則 `forced_cut` event（不靜默）。
//! - **重連 gap**：session 在空窗收口時呼叫 [`rtsp_core::sink::NalSink::gap`]，本模組
//!   立刻關閉當前段（段尾停在 gap 起點），下一段 `gap_ms` 記下空窗長度並強制以 IDR
//!   開段——斷流前後永遠不會被串成同一段。
//! - **防呆補記**：相鄰 NAL 間隔超過 [`IMPLICIT_GAP_MS`] 卻沒收到 `gap` 事件，代表
//!   hook 有路漏了；仍會切段並寫 `implicit_gap` event（有記錄，不靜默）。

use std::fs;
use std::path::PathBuf;

use rtsp_core::rtp::{nal_type, strip_leading_start_code};
use rtsp_core::sink::NalSink;

use crate::config::RingConfig;
use crate::error::{Error, Result};
use crate::index::{SegmentIndex, SegmentRow};
use crate::mkv::{AvcConfig, MkvWriter};
use crate::recover::{recover, segment_name, RecoveryReport, SEGMENT_EXT};

/// 相鄰 NAL 間隔超過此值就視為漏報 gap（毫秒）。
pub const IMPLICIT_GAP_MS: u64 = 250;

struct Current {
    writer: MkvWriter,
    part: PathBuf,
    final_path: PathBuf,
    segment_id: String,
    name: String,
    t_start: u64,
    blocks: usize,
    gap_ms: u64,
    sps_prefix: Vec<u8>,
}

/// MKV 環形緩衝寫入器（實作 [`NalSink`]）。
pub struct RingWriter {
    cfg: RingConfig,
    index: SegmentIndex,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    cur: Option<Current>,
    last_ms: Option<u64>,
    pending_gap: Option<(u64, String)>,
    error: Option<Error>,
}

impl RingWriter {
    /// 開啟資料目錄並先跑一次啟動復原掃描。
    pub fn open(cfg: RingConfig) -> Result<(Self, RecoveryReport)> {
        fs::create_dir_all(&cfg.dir)?;
        let mut index = SegmentIndex::open(cfg.index_path())?;
        let report = recover(&cfg.dir, &mut index)?;
        let mut me = Self {
            cfg,
            index,
            sps: None,
            pps: None,
            cur: None,
            last_ms: None,
            pending_gap: None,
            error: None,
        };
        // 索引重建後若已超過窗，先修剪一次
        me.trim()?;
        Ok((me, report))
    }

    /// 索引（測試與 CLI 用來讀段與事件）。
    pub fn index(&self) -> &SegmentIndex {
        &self.index
    }

    /// 設定。
    pub fn config(&self) -> &RingConfig {
        &self.cfg
    }

    /// 取出第一個寫入錯誤（`NalSink` 不能回錯，所以錯誤暫存在這裡；
    /// 寫入失敗的降級策略屬 M3）。
    pub fn take_error(&mut self) -> Option<Error> {
        self.error.take()
    }

    /// 是否有寫入錯誤。
    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }

    /// 收工：關閉當前段。
    pub fn finish(&mut self) -> Result<()> {
        let t_end = self.last_ms.unwrap_or(0);
        self.close_segment(t_end)
    }

    fn handle_nal(&mut self, nal: &[u8], at_ms: u64) -> Result<()> {
        let nal = strip_leading_start_code(nal);
        let nt = nal_type(nal);
        match nt {
            // SPS / PPS：只快取，段開頭會自動帶上
            7 => {
                self.sps = Some(nal.to_vec());
                return Ok(());
            }
            8 => {
                self.pps = Some(nal.to_vec());
                return Ok(());
            }
            // AUD / SEI：M1 只寫視訊 slice，略過
            6 | 9 => return Ok(()),
            _ => {}
        }
        let keyframe = nt == 5;

        // 防呆：間隔過大卻沒收到 gap 事件 → 補記 + 切段
        if let Some(prev) = self.last_ms {
            let d = at_ms.saturating_sub(prev);
            if d > IMPLICIT_GAP_MS && self.cur.is_some() {
                self.index.insert_event(
                    at_ms,
                    "implicit_gap",
                    &format!("相鄰 NAL 間隔 {d} ms 卻沒收到 gap 事件，補記並切段"),
                )?;
                self.pending_gap = Some((
                    d,
                    format!("implicit gap：相鄰 NAL 間隔 {d} ms（hook 未回報），已補記"),
                ));
                self.close_segment(prev)?;
            }
        }
        self.last_ms = Some(at_ms);

        if self.cur.is_none() {
            self.start_segment(at_ms)?;
        } else {
            let t_start = self.cur.as_ref().map(|c| c.t_start).unwrap_or(at_ms);
            let elapsed = at_ms.saturating_sub(t_start);
            if elapsed >= self.cfg.segment_ms {
                if keyframe {
                    self.close_segment(at_ms)?;
                    self.start_segment(at_ms)?;
                } else if elapsed >= self.cfg.force_cut_ms {
                    self.index.insert_event(
                        at_ms,
                        "forced_cut",
                        &format!(
                            "段 {t_start} 已滿 {} ms 仍無 IDR，於 {at_ms} 強切（新段從非 keyframe 開始）",
                            self.cfg.force_cut_ms
                        ),
                    )?;
                    self.close_segment(at_ms)?;
                    self.start_segment(at_ms)?;
                }
            }
        }

        let cur = self
            .cur
            .as_mut()
            .ok_or_else(|| Error::Config("開段失敗".into()))?;
        let mut payload = Vec::with_capacity(nal.len() + 64);
        if cur.blocks == 0 {
            payload.extend_from_slice(&cur.sps_prefix);
        }
        AvcConfig::append_avcc(nal, &mut payload);
        cur.writer.append_block(&payload, at_ms, keyframe)?;
        cur.blocks += 1;
        Ok(())
    }

    fn handle_gap(&mut self, at_ms: u64, gap_ms: u64, reason: &str) -> Result<()> {
        if gap_ms == 0 {
            return Ok(());
        }
        if self.cur.is_none() {
            // 還沒開始錄，空窗不影響任何段；仍留記錄
            self.index
                .insert_event(at_ms, "gap", &format!("{gap_ms} ms 空窗（尚未開段）：{reason}"))?;
            return Ok(());
        }
        let end = at_ms.saturating_sub(gap_ms);
        self.index.insert_event(
            at_ms,
            "gap",
            &format!("{gap_ms} ms 空窗收口，於 {end} 關段：{reason}"),
        )?;
        self.close_segment(end)?;
        self.pending_gap = Some((gap_ms, reason.to_string()));
        Ok(())
    }

    fn start_segment(&mut self, at_ms: u64) -> Result<()> {
        let (Some(sps), Some(pps)) = (self.sps.clone(), self.pps.clone()) else {
            return Err(Error::Config(
                "還沒收到 SPS/PPS，無法開始錄影段".into(),
            ));
        };
        let avc = AvcConfig { sps, pps };
        let name = segment_name(at_ms);
        let part = self.cfg.dir.join(format!("{name}.part"));
        let final_path = self.cfg.dir.join(&name);
        let writer = MkvWriter::create(
            &part,
            &avc,
            self.cfg.width,
            self.cfg.height,
            self.cfg.prealloc_bytes,
        )?;
        let mut sps_prefix = Vec::with_capacity(avc.sps.len() + avc.pps.len() + 8);
        AvcConfig::append_avcc(&avc.sps, &mut sps_prefix);
        AvcConfig::append_avcc(&avc.pps, &mut sps_prefix);

        let (gap_ms, reason) = self.pending_gap.take().unwrap_or((0, String::new()));
        if gap_ms > 0 {
            self.index.insert_event(
                at_ms,
                "gap",
                &format!("段 {name} 之前有 {gap_ms} ms 斷流：{reason}"),
            )?;
        }
        self.cur = Some(Current {
            writer,
            part,
            final_path,
            segment_id: name.trim_end_matches(SEGMENT_EXT).to_string(),
            name,
            t_start: at_ms,
            blocks: 0,
            gap_ms,
            sps_prefix,
        });
        Ok(())
    }

    fn close_segment(&mut self, t_end_ms: u64) -> Result<()> {
        let Some(mut cur) = self.cur.take() else {
            return Ok(());
        };
        let sum = cur.writer.finish()?;
        fs::rename(&cur.part, &cur.final_path)?;
        self.index.upsert(&SegmentRow {
            segment_id: cur.segment_id.clone(),
            name: cur.name.clone(),
            t_start: cur.t_start,
            t_end: sum.t_end.max(t_end_ms).max(cur.t_start),
            bytes: sum.bytes,
            idr_offset: sum.idr_offset,
            gap_ms: cur.gap_ms,
            closed: true,
            repaired: false,
        })?;
        self.trim()?;
        Ok(())
    }

    /// 修剪：窗長超過 `window + 一個段` 或總 bytes 超過上限，就刪最舊段。
    fn trim(&mut self) -> Result<()> {
        loop {
            let rows = self.index.rows()?;
            if rows.len() < 2 {
                break;
            }
            let oldest = &rows[0];
            let latest_end = rows[rows.len() - 1].t_end;
            let by_window = self
                .cfg
                .retention
                .should_trim(oldest.t_start, latest_end, self.cfg.segment_ms);
            let by_bytes = self.index.total_bytes()? > self.cfg.retention.max_bytes;
            if !(by_window || by_bytes) {
                break;
            }
            let path = self.cfg.dir.join(&oldest.name);
            if path.exists() {
                fs::remove_file(&path)?;
            }
            self.index.delete(&oldest.segment_id)?;
        }
        Ok(())
    }
}

impl NalSink for RingWriter {
    fn nal(&mut self, nal: &[u8]) {
        let at = self.last_ms.unwrap_or(0);
        self.nal_at(nal, at);
    }

    fn nal_at(&mut self, nal: &[u8], at_ms: u64) {
        if self.error.is_some() {
            return;
        }
        if let Err(e) = self.handle_nal(nal, at_ms) {
            self.error = Some(e);
        }
    }

    fn gap(&mut self, at_ms: u64, gap_ms: u64, reason: &str) {
        if self.error.is_some() {
            return;
        }
        if let Err(e) = self.handle_gap(at_ms, gap_ms, reason) {
            self.error = Some(e);
        }
    }
}
