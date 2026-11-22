//! 真實幀率標定（BACKLOG-02）：只用 RTP timestamp（90 kHz）推算 fps。
//!
//! 口徑（k18）：
//!
//! - **一幀＝一個 access unit**：同一個 RTP timestamp 的所有封包都屬於同一幀
//!   （SPS／PPS、單幀多 slice、FU-A 分段都共用 timestamp），timestamp 變了才是下一幀。
//! - **marker bit 只做一致性檢查**，不當幀邊界；相機的 marker、切片數、解碼幀數互相
//!   不自洽，任何一個都不能當 fps 的分母。
//! - **32-bit 回繞**用 `wrapping_sub` 自然處理：正常向前是小的正距離，真正的向後／
//!   亂序封包是 `>= 2^31` 的巨大距離，只計 `out_of_order`，**不前進 `prev`**。
//! - 相鄰 timestamp 間隔超過 5 秒視為 discontinuity（重連／長時間丟包），結束當前窗，
//!   不把巨大間隔當成「一幀」。
//! - 樣本窗：10 秒或 300 幀（先到者）；少於 30 幀的窗不採用。
//!
//! fps 的 AC 主值用 median interval（對亂序、單次長間隔較穩）；mean 一併輸出對照。
//! 變動幀率時輸出分佈與 `variable` 旗標，讓 M2 能標「近似」而不是假裝 CFR。

use serde::Serialize;

/// RTP timestamp 時鐘：90 kHz（SDP `a=rtpmap:96 H264/90000`）。
pub const RTP_CLOCK_HZ: u64 = 90_000;
/// 樣本窗長度（毫秒）。
pub const TARGET_WINDOW_MS: u64 = 10_000;
/// 樣本窗最多幀數。
pub const MAX_FRAMES: u64 = 300;
/// 少於此幀數的窗不採用。
pub const MIN_FRAMES: u64 = 30;
/// 相鄰 timestamp 超過此距離（ticks）視為 discontinuity：5 秒。
pub const MAX_INTERVAL_TICKS: u64 = 5 * RTP_CLOCK_HZ;

/// 一次標定的結果（序列化進 `Report.fps`，只增欄位、不改既有欄位）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FpsStats {
    /// 樣本窗內的相異 RTP timestamp 數（＝幀數）。
    pub frames: u64,
    /// 首尾 timestamp 的 ticks 跨度換算成毫秒。
    pub span_ms: u64,
    /// 樣本窗第一個 RTP timestamp（原始 32-bit 值）。
    pub first_timestamp: u32,
    /// 樣本窗最後一個 RTP timestamp（原始 32-bit 值，可能已回繞）。
    pub last_timestamp: u32,
    /// 平均 fps：`90000 * (frames - 1) / span_ticks`。
    pub mean_fps: f64,
    /// 中位數 fps：`90000 / median_interval_ticks`，k18 AC 的主值。
    pub median_fps: f64,
    /// 平均幀間隔（毫秒）。
    pub mean_interval_ms: f64,
    /// 中位數幀間隔（毫秒）。
    pub median_interval_ms: f64,
    /// 5% 分位幀間隔（毫秒）。
    pub p05_interval_ms: f64,
    /// 95% 分位幀間隔（毫秒）。
    pub p95_interval_ms: f64,
    /// 最小幀間隔（毫秒）。
    pub min_interval_ms: f64,
    /// 最大幀間隔（毫秒）。
    pub max_interval_ms: f64,
    /// 變動幀率：`p95 - p05 > 10% * p50`。
    pub variable: bool,
    /// 樣本窗內 marker=true 的封包數（僅供一致性檢查）。
    pub marker_true_packets: u64,
    /// `marker_true_packets` 與幀數的差（0 表示 marker 數剛好等於幀數）。
    pub marker_mismatch: u64,
    /// 向後／亂序封包數（不前進 `prev`）。
    pub out_of_order: u64,
    /// discontinuity 次數（>5 s 的 timestamp 跳躍）。
    pub discontinuities: u64,
    /// 樣本窗是否達到 10 s 或 300 幀的目標（stream 結束時可能為 false，但仍是有效樣本）。
    pub window_complete: bool,
    /// 建議的 MKV `DefaultDuration`（奈秒）；M2 由此讓 ffprobe 不再只靠 1000/1。
    pub default_duration_ns: u64,
}

impl FpsStats {
    /// 把此統計寫成一行可讀摘要（報告與 CI log 用）。
    pub fn summary(&self) -> String {
        format!(
            "fps median={:.3} mean={:.3} frames={} span={}ms window=[{}..{}] p05={:.3}ms p95={:.3}ms variable={} default_duration={}ns",
            self.median_fps,
            self.mean_fps,
            self.frames,
            self.span_ms,
            self.first_timestamp,
            self.last_timestamp,
            self.p05_interval_ms,
            self.p95_interval_ms,
            self.variable,
            self.default_duration_ns
        )
    }
}

/// RTP timestamp → fps 估計器。
#[derive(Debug, Default)]
pub struct FpsEstimator {
    first_ts: Option<u32>,
    last_ts: Option<u32>,
    /// 已接受的幀間隔（ticks），長度 = frames - 1。
    intervals: Vec<u64>,
    /// 從窗首累積的 ticks（已 unwrap）。
    unwrapped: u64,
    marker_true: u64,
    out_of_order: u64,
    discontinuities: u64,
    /// 目前窗是否已收滿（10 s 或 300 幀），收滿後不再收新樣本。
    window_done: bool,
    /// 目前最佳窗（幀數最多；同幀數取較早完成的）。
    best: Option<FpsStats>,
}

impl FpsEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 餵入一個 RTP 視訊封包。`timestamp` 是 32-bit RTP timestamp，
    /// `marker` 只計數、不影響幀邊界。
    pub fn push(&mut self, timestamp: u32, marker: bool) {
        if self.window_done {
            return;
        }
        if self.first_ts.is_none() {
            self.begin_window(timestamp, marker);
            return;
        }

        self.marker_true += u64::from(marker);
        let prev = self.last_ts.expect("窗已開始");
        let d = timestamp.wrapping_sub(prev);

        if d == 0 {
            // 同一個 access unit 的另一個封包。
            return;
        }
        if d >= (1u32 << 31) {
            // 向後／亂序：不前進 prev，否則下一個正常包會被算成巨大間隔。
            self.out_of_order += 1;
            return;
        }
        if u64::from(d) > MAX_INTERVAL_TICKS {
            // 重連／長時間丟包：結束這個窗，從新 timestamp 重新開始，不當成一幀。
            self.discontinuities += 1;
            self.finish_current();
            self.begin_window(timestamp, marker);
            return;
        }

        self.unwrapped += u64::from(d);
        self.intervals.push(u64::from(d));
        self.last_ts = Some(timestamp);

        if self.unwrapped >= TARGET_WINDOW_MS * (RTP_CLOCK_HZ / 1000) || self.frames() >= MAX_FRAMES
        {
            self.finish_current();
            self.window_done = true;
        }
    }

    /// session 邊界：RTP timestamp 基準會換，結束當前窗並清狀態（已有的最佳窗保留）。
    pub fn session_boundary(&mut self) {
        self.finish_current();
        self.window_done = false;
    }

    /// 收工：結束當前窗，回傳目前最佳樣本。
    pub fn finish(&mut self) -> Option<FpsStats> {
        self.finish_current();
        self.best.clone()
    }

    /// 目前窗的幀數（含第一幀）。
    pub fn frames(&self) -> u64 {
        self.intervals.len() as u64 + 1
    }

    fn begin_window(&mut self, timestamp: u32, marker: bool) {
        self.first_ts = Some(timestamp);
        self.last_ts = Some(timestamp);
        self.intervals.clear();
        self.unwrapped = 0;
        self.marker_true = u64::from(marker);
        self.window_done = false;
    }

    fn finish_current(&mut self) {
        let Some(first) = self.first_ts else {
            return;
        };
        let frames = self.frames();
        if frames < MIN_FRAMES {
            self.reset_current();
            return;
        }

        let stats = self.build_stats(first);
        let replace = self.best.as_ref().map_or(true, |b| stats.frames > b.frames);
        if replace {
            self.best = Some(stats);
        }
        self.reset_current();
    }

    fn reset_current(&mut self) {
        self.first_ts = None;
        self.last_ts = None;
        self.intervals.clear();
        self.unwrapped = 0;
        self.marker_true = 0;
        self.window_done = false;
    }

    fn build_stats(&self, first: u32) -> FpsStats {
        let frames = self.frames();
        let interval_count = frames - 1;
        let span_ticks = self.unwrapped;
        let span_ms = span_ticks / (RTP_CLOCK_HZ / 1000);

        let mean_fps = if span_ticks == 0 {
            0.0
        } else {
            (RTP_CLOCK_HZ as f64) * (interval_count as f64) / (span_ticks as f64)
        };
        let mean_interval_ms = if interval_count == 0 {
            0.0
        } else {
            (span_ticks as f64) / (interval_count as f64) / (RTP_CLOCK_HZ as f64 / 1000.0)
        };

        let mut sorted = self.intervals.clone();
        sorted.sort_unstable();
        let median_ticks = median(&sorted);
        let median_fps = if median_ticks > 0.0 {
            (RTP_CLOCK_HZ as f64) / median_ticks
        } else {
            0.0
        };
        let median_interval_ms = median_ticks / (RTP_CLOCK_HZ as f64 / 1000.0);
        let p05 = percentile_ms(&sorted, 0.05);
        let p95 = percentile_ms(&sorted, 0.95);
        let min = sorted.first().map(|v| *v as f64).unwrap_or(0.0) / (RTP_CLOCK_HZ as f64 / 1000.0);
        let max = sorted.last().map(|v| *v as f64).unwrap_or(0.0) / (RTP_CLOCK_HZ as f64 / 1000.0);
        let variable = p95 - p05 > 0.10 * median_interval_ms;

        FpsStats {
            frames,
            span_ms,
            first_timestamp: first,
            last_timestamp: self.last_ts.unwrap_or(first),
            mean_fps,
            median_fps,
            mean_interval_ms,
            median_interval_ms,
            p05_interval_ms: p05,
            p95_interval_ms: p95,
            min_interval_ms: min,
            max_interval_ms: max,
            variable,
            marker_true_packets: self.marker_true,
            marker_mismatch: self.marker_true.abs_diff(frames),
            out_of_order: self.out_of_order,
            discontinuities: self.discontinuities,
            window_complete: span_ticks >= TARGET_WINDOW_MS * (RTP_CLOCK_HZ / 1000)
                || frames >= MAX_FRAMES,
            default_duration_ns: if median_fps > 0.0 {
                (1_000_000_000.0 / median_fps).round() as u64
            } else {
                0
            },
        }
    }
}

fn median(sorted_ticks: &[u64]) -> f64 {
    if sorted_ticks.is_empty() {
        return 0.0;
    }
    let n = sorted_ticks.len();
    if n % 2 == 1 {
        sorted_ticks[n / 2] as f64
    } else {
        (sorted_ticks[n / 2 - 1] as f64 + sorted_ticks[n / 2] as f64) / 2.0
    }
}

/// 最近秩百分位（nearest-rank）；輸入已排序的 ticks。
fn percentile_ms(sorted_ticks: &[u64], q: f64) -> f64 {
    if sorted_ticks.is_empty() {
        return 0.0;
    }
    let idx = (((sorted_ticks.len() - 1) as f64) * q).round() as usize;
    (sorted_ticks[idx] as f64) / (RTP_CLOCK_HZ as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS0: u32 = 135_000;
    const STEP: u32 = 3_600; // 25 fps @ 90 kHz

    fn feed_constant(est: &mut FpsEstimator, start: u32, frames: u64) {
        for i in 0..frames {
            let ts = start.wrapping_add((i as u32).wrapping_mul(STEP));
            est.push(ts, true);
        }
    }

    #[test]
    fn constant_25fps_is_exactly_25() {
        let mut est = FpsEstimator::new();
        feed_constant(&mut est, TS0, 250);
        let stats = est.finish().expect("應有統計");
        assert_eq!(stats.frames, 250);
        assert_eq!(stats.span_ms, 9_960);
        assert_eq!(stats.first_timestamp, TS0);
        assert_eq!(stats.last_timestamp, TS0 + 249 * STEP);
        assert_eq!(stats.mean_fps, 25.0, "mean 必須恰好 25.000");
        assert_eq!(stats.median_fps, 25.0, "median 必須恰好 25.000");
        assert_eq!(stats.mean_interval_ms, 40.0);
        assert_eq!(stats.median_interval_ms, 40.0);
        assert!(!stats.variable);
        assert_eq!(stats.default_duration_ns, 40_000_000);
    }

    #[test]
    fn timestamp_wrap_still_exactly_25fps() {
        let mut est = FpsEstimator::new();
        // 從 2^32 - 7200 開始，兩幀後就會回繞。
        feed_constant(&mut est, u32::MAX - 7_200 + 1, 250);
        let stats = est.finish().expect("應有統計");
        assert_eq!(stats.frames, 250);
        assert_eq!(stats.span_ms, 9_960);
        assert_eq!(stats.mean_fps, 25.0);
        assert_eq!(stats.median_fps, 25.0);
        assert_eq!(stats.default_duration_ns, 40_000_000);
        assert!(
            stats.last_timestamp < stats.first_timestamp,
            "測試必須真的跨過 32-bit 回繞"
        );
    }

    #[test]
    fn out_of_order_packet_does_not_advance_prev() {
        let mut est = FpsEstimator::new();
        est.push(TS0, true);
        est.push(TS0 + STEP, true);
        est.push(TS0 + 2 * STEP, true);
        // 向後亂序包：若 prev 被推進，下一個正常包會變成 ~2^32 的巨大間隔。
        est.push(TS0 + 1_000, true);
        est.push(TS0 + 3 * STEP, true);
        for i in 4..250u64 {
            est.push(TS0 + (i as u32) * STEP, true);
        }
        let stats = est.finish().expect("應有統計");
        assert_eq!(stats.out_of_order, 1);
        assert_eq!(stats.frames, 250, "亂序包不得被算成新幀或吞掉後面的幀");
        assert_eq!(stats.mean_fps, 25.0);
        assert_eq!(stats.median_fps, 25.0);
    }

    #[test]
    fn marker_count_is_not_frame_count() {
        let mut est = FpsEstimator::new();
        for i in 0..250u32 {
            let ts = TS0 + i * STEP;
            // 每幀三個封包都 marker=true（SPS/PPS/slice 各自 marker 的合成情境）。
            est.push(ts, true);
            est.push(ts, true);
            est.push(ts, true);
        }
        let stats = est.finish().expect("應有統計");
        assert_eq!(stats.frames, 250);
        assert_eq!(stats.median_fps, 25.0);
        assert_eq!(stats.marker_true_packets, 750);
        assert_eq!(stats.marker_mismatch, 500, "marker 數不得當幀數");
    }

    #[test]
    fn vfr_marks_variable_not_cfr() {
        let mut est = FpsEstimator::new();
        let mut ts = TS0;
        est.push(ts, true);
        // 40 ms 與 60 ms 交錯：p05=40、p95=60，差距 >10% p50。
        for i in 0..299u64 {
            let step = if i % 2 == 0 { 3_600 } else { 5_400 };
            ts = ts.wrapping_add(step);
            est.push(ts, true);
        }
        let stats = est.finish().expect("應有統計");
        assert!(stats.variable, "VFR 必須標成 variable：{stats:?}");
        assert!(stats.p95_interval_ms - stats.p05_interval_ms > 0.10 * stats.median_interval_ms);
    }

    #[test]
    fn discontinuity_starts_new_window_without_giant_interval() {
        let mut est = FpsEstimator::new();
        est.push(TS0, true);
        est.push(TS0 + STEP, true);
        // 跳 6 秒：不得把 540000 ticks 當成一幀；這個 2 幀窗太小會丟棄。
        est.push(TS0 + STEP + 540_000, true);
        let mut ts = TS0 + STEP + 540_000;
        for _ in 0..248 {
            ts = ts.wrapping_add(STEP);
            est.push(ts, true);
        }
        let stats = est.finish().expect("應有統計");
        assert_eq!(stats.discontinuities, 1);
        assert_eq!(stats.frames, 249);
        assert_eq!(stats.mean_fps, 25.0);
        assert_eq!(stats.median_fps, 25.0);
    }

    #[test]
    fn fewer_than_min_frames_yields_no_fps() {
        let mut est = FpsEstimator::new();
        // 29 幀：差一個才到 MIN_FRAMES，整窗應被丟棄，回 None（不是 0、不是 NaN）。
        feed_constant(&mut est, TS0, 29);
        assert!(est.finish().is_none(), "少於 30 幀不得產生 fps");
    }

    #[test]
    fn single_timestamp_yields_no_fps() {
        let mut est = FpsEstimator::new();
        est.push(TS0, true);
        assert!(est.finish().is_none(), "只有一個 timestamp 不得產生 fps");
    }

    #[test]
    fn reconnect_keeps_window_with_most_frames_and_resets_baseline() {
        // 第一個窗 250 幀，第二個 100 幀：保留第一個。
        let mut est = FpsEstimator::new();
        feed_constant(&mut est, TS0, 250);
        est.session_boundary();
        feed_constant(&mut est, TS0.wrapping_add(1_000_000), 100);
        let first = est.finish().expect("應保留第一個窗");
        assert_eq!(first.frames, 250);
        assert_eq!(first.first_timestamp, TS0);

        // 第一個窗 100 幀，第二個 250 幀：取第二個，且基準是第二個的 timestamp。
        let mut est = FpsEstimator::new();
        feed_constant(&mut est, TS0, 100);
        est.session_boundary();
        let second_start = TS0.wrapping_add(900_000);
        feed_constant(&mut est, second_start, 250);
        let second = est.finish().expect("應保留第二個窗");
        assert_eq!(second.frames, 250);
        assert_eq!(second.first_timestamp, second_start);
        assert_eq!(second.median_fps, 25.0);
    }
}
