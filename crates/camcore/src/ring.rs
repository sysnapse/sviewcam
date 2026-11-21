//! 環形緩衝：把 NAL 串流切成 2 秒 MKV 段、寫索引、滿窗刪最舊。
//!
//! 切段規則：
//!
//! - **GOP 對齊**：段長滿 [`RingConfig::segment_ms`] 後，等到下一個 IDR（NAL type 5）
//!   才切；切點段的第一個 block 一定帶 SPS+PPS。
//! - **延後強切**：段滿 [`RingConfig::segment_ms`] 後若一直等不到 IDR，超過
//!   [`RingConfig::force_cut_ms`] 先記一則 `forced_cut_deferred` 並繼續等；段長達
//!   [`RingConfig::force_cut_limit_ms`]（預設 4×）硬上限才真的硬切，記一則
//!   `forced_cut`，且新段從非 keyframe 開始、索引標成不可獨立回放（不靜默）。
//! - **重連 gap**：session 在空窗收口時呼叫 [`rtsp_core::sink::NalSink::gap`]，本模組
//!   立刻關閉當前段（段尾停在 gap 起點），下一段的 `gap_ms` 記下空窗長度，並**等到
//!   IDR 才開段**——斷流前後永遠不會被串成同一段，段首也一定可獨立解碼。等待期間
//!   只記一則 `waiting_idr` event（重連後 P 幀續流的常見情況），不靜默丟包。
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
    /// 本段第一個 block 是否 keyframe（正常開段／GOP 切為 true；上限硬切為 false）。
    first_is_keyframe: bool,
}

/// MKV 環形緩衝寫入器（實作 [`NalSink`]）。
pub struct RingWriter {
    cfg: RingConfig,
    index: SegmentIndex,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    cur: Option<Current>,
    /// 正在等 IDR 才開段（重連後／串流開頭的第一個 NAL 不是 IDR）。
    waiting_idr: bool,
    last_ms: Option<u64>,
    pending_gap: Option<(u64, String)>,
    /// 已進入延後強切期（每段只記一則 `forced_cut_deferred`）。
    deferred_since: Option<u64>,
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
            waiting_idr: false,
            last_ms: None,
            pending_gap: None,
            deferred_since: None,
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
            if keyframe {
                self.start_segment(at_ms, true)?;
            } else {
                // 段的開頭必須是 IDR；P 幀續流就先等，記一則事件讓 hook 漏報看得見
                self.note_waiting_idr(at_ms)?;
                return Ok(());
            }
        } else {
            let t_start = self.cur.as_ref().map(|c| c.t_start).unwrap_or(at_ms);
            let elapsed = at_ms.saturating_sub(t_start);
            if elapsed >= self.cfg.segment_ms {
                if keyframe {
                    self.close_segment(at_ms)?;
                    self.start_segment(at_ms, true)?;
                } else if elapsed >= self.cfg.force_cut_ms {
                    let limit = self.cfg.force_cut_limit_ms();
                    if self.deferred_since.is_none() {
                        self.deferred_since = Some(at_ms);
                        self.index.insert_event(
                            at_ms,
                            "forced_cut_deferred",
                            &format!(
                                "段 {t_start} 已滿 {} ms 仍無 IDR，於 {at_ms} 延後強切等 IDR（段長硬上限 {limit} ms）",
                                self.cfg.force_cut_ms
                            ),
                        )?;
                    }
                    // 到了硬上限還沒有 IDR，只能從當前非 keyframe 硬切；新段不可獨立回放，
                    // 索引會如實記 first_block_keyframe=false。
                    if elapsed >= limit {
                        self.index.insert_event(
                            at_ms,
                            "forced_cut",
                            &format!(
                                "段 {t_start} 已滿 {} ms 仍無 IDR，於 {at_ms} 強切（已達 {limit} ms 上限，新段從非 keyframe 開始，不可獨立回放）",
                                self.cfg.force_cut_ms
                            ),
                        )?;
                        self.close_segment(at_ms)?;
                        self.start_segment(at_ms, false)?;
                    }
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

    fn note_waiting_idr(&mut self, at_ms: u64) -> Result<()> {
        if self.waiting_idr {
            return Ok(());
        }
        self.waiting_idr = true;
        self.index.insert_event(
            at_ms,
            "waiting_idr",
            &format!("{at_ms}：開段前先等到 IDR（當前是 P 幀續流，不從非 keyframe 起段）"),
        )?;
        Ok(())
    }

    fn start_segment(&mut self, at_ms: u64, first_is_keyframe: bool) -> Result<()> {
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

        self.waiting_idr = false;
        self.deferred_since = None;
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
            first_is_keyframe,
        });
        Ok(())
    }

    fn close_segment(&mut self, t_end_ms: u64) -> Result<()> {
        let Some(mut cur) = self.cur.take() else {
            return Ok(());
        };
        self.deferred_since = None;
        let sum = cur.writer.finish()?;
        fs::rename(&cur.part, &cur.final_path)?;
        self.index.upsert(&SegmentRow {
            segment_id: cur.segment_id.clone(),
            name: cur.name.clone(),
            t_start: cur.t_start,
            t_end: sum.t_end.max(t_end_ms).max(cur.t_start),
            bytes: sum.bytes,
            idr_offset: sum.idr_offset,
            first_block_keyframe: Some(cur.first_is_keyframe),
            gap_ms: Some(cur.gap_ms),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RingConfig;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "camcore-ring-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    const SPS: [u8; 5] = [0x67, 0x64, 0x00, 0x28, 0xAC];
    const PPS: [u8; 4] = [0x68, 0xEE, 0x3C, 0x80];
    const IDR: [u8; 4] = [0x65, 0x11, 0x22, 0x33];
    const P: [u8; 4] = [0x41, 0x11, 0x22, 0x33];

    /// 段的開頭必須是 IDR：串流開頭先來 P 幀時要等，不能拿 P 幀當首 block。
    #[test]
    fn waits_for_idr_before_opening_segment() {
        let dir = tmp_dir("wait-idr");
        let (mut w, _report) = RingWriter::open(RingConfig::new(&dir)).unwrap();
        w.nal_at(&SPS, 0);
        w.nal_at(&PPS, 0);
        // 串流第一個 slice 是 P 幀（例如重連後續流）：不得開段
        w.nal_at(&P, 500);
        assert!(w.take_error().is_none(), "等 IDR 不該算錯誤");
        assert_eq!(w.index().count().unwrap(), 0, "P 幀不得開段");
        let events = w.index().events().unwrap();
        assert_eq!(
            events.iter().filter(|e| e.kind == "waiting_idr").count(),
            1,
            "等 IDR 要留一則事件（不得靜默）：{events:?}"
        );

        // IDR 到了才開段，且段首就是 IDR
        w.nal_at(&IDR, 900);
        w.nal_at(&P, 940);
        w.finish().unwrap();
        assert!(w.take_error().is_none());
        let rows = w.index().rows().unwrap();
        assert_eq!(rows.len(), 1, "只該有一段：{rows:?}");
        assert_eq!(rows[0].t_start, 900, "段的起點是 IDR 的時間，不是 P 幀的 500");
        let info = crate::mkv::scan(dir.join(&rows[0].name)).unwrap();
        assert_eq!(info.first_block_keyframe, Some(true), "段首必須是 keyframe");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 延後強切：段滿 `segment_ms` 仍無 IDR 時不立刻硬切，先記事件繼續等；
    /// 等到 IDR 才切，段首仍是 keyframe，且等待期間的 P 幀全部留在原段、不丟畫面。
    #[test]
    fn forced_cut_defers_until_idr_without_losing_frames() {
        let dir = tmp_dir("forced-cut-defer");
        let (mut w, _report) = RingWriter::open(RingConfig::new(&dir)).unwrap();
        w.nal_at(&SPS, 0);
        w.nal_at(&PPS, 0);
        w.nal_at(&IDR, 0);
        // 每 100 ms 一個 P 幀（間隔 < IMPLICIT_GAP_MS=250，不會誤觸防呆），到 5900 ms
        for t in (100..=5900).step_by(100) {
            w.nal_at(&P, t);
        }
        // 4000 ms 進延後期；6000 ms 的 IDR 才收段
        w.nal_at(&IDR, 6000);
        w.nal_at(&P, 6040);
        assert!(w.take_error().is_none());
        w.finish().unwrap();
        assert!(w.take_error().is_none());

        let rows = w.index().rows().unwrap();
        assert_eq!(rows.len(), 2, "延後到 IDR 才切，只該兩段：{rows:?}");
        assert_eq!(rows[0].t_start, 0, "{rows:?}");
        assert_eq!(rows[0].t_end, 6000, "切點是 IDR 的時間，不是 force_cut 門檻");
        assert_eq!(rows[1].t_start, 6000, "新段從 IDR 開始");
        assert_eq!(rows[1].gap_ms, Some(0), "延後強切不是斷流，不該記 gap");
        assert_eq!(rows[0].first_block_keyframe, Some(true), "原段首是 IDR");
        assert_eq!(rows[1].first_block_keyframe, Some(true), "新段首是 IDR");
        let info = crate::mkv::scan(dir.join(&rows[1].name)).unwrap();
        assert_eq!(info.first_block_keyframe, Some(true), "新段首 block 必須是 keyframe");

        let events = w.index().events().unwrap();
        let deferred: Vec<_> = events
            .iter()
            .filter(|e| e.kind == "forced_cut_deferred")
            .collect();
        assert_eq!(deferred.len(), 1, "延後只記一筆：{events:?}");
        assert!(
            deferred[0].detail.contains("4000"),
            "事件要帶門檻時間：{:?}",
            deferred[0]
        );
        assert!(
            !events.iter().any(|e| e.kind == "forced_cut"),
            "有在期限內等到 IDR，不該真的硬切：{events:?}"
        );
        assert!(
            !events.iter().any(|e| e.kind == "implicit_gap"),
            "間隔都在 250 ms 內，不該有 implicit_gap：{events:?}"
        );
        // 等待期間的 P 幀沒有被丟掉：第一段實際 bytes 遠大於只有 header 的空段
        assert!(rows[0].bytes > 1000, "P 幀續流要留在原段：{:?}", rows[0]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 硬上限：一直沒有 IDR，段長到 4×force_cut_ms 才真的硬切；新段首不是 keyframe，
    /// 索引必須如實記 `first_block_keyframe=false`（不可獨立回放），不得靜默。
    #[test]
    fn forced_cut_falls_back_at_hard_limit_and_flags_segment() {
        let dir = tmp_dir("forced-cut-limit");
        let (mut w, _report) = RingWriter::open(RingConfig::new(&dir)).unwrap();
        w.nal_at(&SPS, 0);
        w.nal_at(&PPS, 0);
        w.nal_at(&IDR, 0);
        // 之後再也沒有 IDR；每 100 ms 一個 P 幀到 17500 ms
        for t in (100..=17500).step_by(100) {
            w.nal_at(&P, t);
        }
        assert!(w.take_error().is_none());
        w.finish().unwrap();
        assert!(w.take_error().is_none());

        let rows = w.index().rows().unwrap();
        assert_eq!(rows.len(), 2, "16 s 上限應只切一次：{rows:?}");
        assert_eq!(rows[0].t_start, 0);
        assert_eq!(rows[0].t_end, 16000, "硬上限是 4×force_cut_ms=16000");
        assert_eq!(rows[0].first_block_keyframe, Some(true));
        assert_eq!(rows[1].t_start, 16000, "新段從硬切的那個 P 幀開始");
        assert_eq!(
            rows[1].first_block_keyframe,
            Some(false),
            "非 keyframe 起段要如實標記：{rows:?}"
        );
        let info = crate::mkv::scan(dir.join(&rows[1].name)).unwrap();
        assert_eq!(
            info.first_block_keyframe,
            Some(false),
            "scanner 獨立重算也必須是 false"
        );

        let events = w.index().events().unwrap();
        assert_eq!(
            events.iter().filter(|e| e.kind == "forced_cut").count(),
            1,
            "硬切恰一筆：{events:?}"
        );
        let cut = events
            .iter()
            .find(|e| e.kind == "forced_cut")
            .expect("要有 forced_cut");
        assert!(cut.detail.contains("16000"), "事件要帶上限：{:?}", cut);
        assert!(
            cut.detail.contains("不可獨立回放"),
            "事件要說明後果：{:?}",
            cut
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == "forced_cut_deferred")
                .count(),
            1,
            "延後也只記一筆：{events:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 防呆：相鄰 NAL 間隔超過 250 ms 卻沒收到 gap 事件 → 補記並切段，
    /// 切點落在前一個 NAL、補記的 gap 落到下一段的 `gap_ms`。
    #[test]
    fn implicit_gap_inside_segment_closes_at_previous_nal() {
        let dir = tmp_dir("implicit-gap");
        let (mut w, _report) = RingWriter::open(RingConfig::new(&dir)).unwrap();
        w.nal_at(&SPS, 0);
        w.nal_at(&PPS, 0);
        w.nal_at(&IDR, 0);
        w.nal_at(&P, 100);
        // 300 ms 間隔但沒呼叫 sink.gap()：防呆要自己補記並切段
        w.nal_at(&P, 400);
        assert!(w.take_error().is_none());
        // 續流先來 P 幀 → 不開段；等 IDR 才開，補記的 gap 進新段
        w.nal_at(&P, 440);
        w.nal_at(&IDR, 800);
        w.finish().unwrap();
        assert!(w.take_error().is_none());

        let rows = w.index().rows().unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].t_start, 0);
        assert_eq!(rows[0].t_end, 100, "切點在前一個 NAL，不是晚到的那個");
        assert_eq!(rows[1].t_start, 800, "補記 gap 後仍要等到 IDR 才開段");
        assert_eq!(rows[1].gap_ms, Some(300), "補記的 gap 要落到下一段的索引上");

        let events = w.index().events().unwrap();
        let implicit: Vec<_> = events
            .iter()
            .filter(|e| e.kind == "implicit_gap")
            .collect();
        assert_eq!(implicit.len(), 1, "補記恰一筆：{events:?}");
        assert!(implicit[0].detail.contains("300"), "{:?}", implicit[0]);
        assert!(
            events.iter().any(|e| e.kind == "gap" && e.detail.contains("300")),
            "開新段時要把補記的 gap 寫進事件：{events:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 重連 gap：關段於空窗起點、下一段記 gap_ms、且一定等到 IDR 才開段。
    #[test]
    fn gap_closes_segment_and_waits_for_idr() {
        let dir = tmp_dir("gap-idr");
        let (mut w, _report) = RingWriter::open(RingConfig::new(&dir)).unwrap();
        w.nal_at(&SPS, 0);
        w.nal_at(&PPS, 0);
        w.nal_at(&IDR, 1000);
        w.nal_at(&P, 1040);
        // 空窗：1000..3500 沒流
        w.gap(3600, 2560, "fixture: camera RST");
        assert!(w.take_error().is_none());
        // 空窗收口＝前一段立刻落地（段尾停在空窗起點）
        let first = w.index().rows().unwrap();
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].t_end, 1040, "段尾停在空窗起點（最後一個 block 是 1040）");
        // 續流先來 P 幀 → 繼續等，不准開第二段
        w.nal_at(&P, 3600);
        assert_eq!(w.index().count().unwrap(), 1, "P 幀續流不得開新段");
        w.nal_at(&IDR, 4000);
        w.nal_at(&P, 4040);
        w.finish().unwrap();
        let rows = w.index().rows().unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[1].t_start, 4000);
        assert_eq!(rows[1].gap_ms, Some(2560), "空窗長度要落在索引上");
        let info = crate::mkv::scan(dir.join(&rows[1].name)).unwrap();
        assert_eq!(info.first_block_keyframe, Some(true));
        let _ = fs::remove_dir_all(&dir);
    }
}
