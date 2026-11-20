//! SVW M1：`NalSink` 新增的 `nal_at` / `gap` 一律有 default 實作。
//!
//! 這支測試釘住「沒有 override 的 sink，行為與改動前完全一致」：
//! 只實作 `nal()` 的 sink 收到 `nal_at` 時，必須一樣拿到 NAL（時間戳被丟棄），
//! 而 `gap()` 預設是 no-op、不得 panic。

use rtsp_core::sink::NalSink;

/// 只實作 `nal()`，完全不知道 `nal_at` / `gap` 存在——就是既有 sink 的樣子。
struct OnlyNal(Vec<Vec<u8>>);

impl NalSink for OnlyNal {
    fn nal(&mut self, nal: &[u8]) {
        self.0.push(nal.to_vec());
    }
}

#[test]
fn default_nal_at_forwards_to_nal() {
    let mut sink = OnlyNal(Vec::new());
    sink.nal_at(&[0x65, 0xAA], 1_234);
    sink.nal_at(&[0x41, 0xBB], 1_240);
    assert_eq!(sink.0, vec![vec![0x65, 0xAA], vec![0x41, 0xBB]]);
}

#[test]
fn default_gap_is_noop() {
    let mut sink = OnlyNal(Vec::new());
    sink.gap(9_000, 1_500, "fixture: camera RST");
    assert!(sink.0.is_empty(), "gap 預設不得偽造任何 NAL");
}

/// `nal_at` 的預設路徑不得改變既有 sink 收到的內容（含空 NAL 的處理責任仍在 sink 端）。
#[test]
fn default_nal_at_keeps_nals_byte_identical() {
    let nals: Vec<Vec<u8>> = vec![
        vec![0x67, 0x4D, 0x00, 0x32], // SPS
        vec![0x68, 0xEE, 0x3C, 0x80], // PPS
        vec![0x65, 0x88, 0x84, 0x00], // IDR
    ];
    let mut a = OnlyNal(Vec::new());
    let mut b = OnlyNal(Vec::new());
    for (i, nal) in nals.iter().enumerate() {
        a.nal(nal);
        b.nal_at(nal, i as u64 * 40);
    }
    assert_eq!(a.0, b.0);
    assert_eq!(a.0, nals);
}
