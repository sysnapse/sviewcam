//! AC1/②/④ 的「管線證據」：用 ffmpeg 現生 2560×1440 H.264，灌進既有 ring，
//! 再走 camctl export／snapshot。餵入端是合成的 1440p，不是 SView 相機。
//!
//! 需要 CI 的 ffmpeg/ffprobe；devbox 的 rust 模板沒有，但可用 arm64 靜態版塞 PATH。

use std::path::{Path, PathBuf};
use std::process::Command;

use camcore::config::RingConfig;
use camcore::ring::RingWriter;
use camcore::SegmentIndex;
use rtsp_core::sink::NalSink;

fn camctl() -> &'static str {
    env!("CARGO_BIN_EXE_camctl")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sview-k14-pipe-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建立暫存目錄");
    dir
}

/// 把 Annex B 切成 NAL（不含起始碼）。x264 的輸出只有 VCL NAL 代表一幀。
fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (idx, &start) in starts.iter().enumerate() {
        let mut end = starts.get(idx + 1).copied().unwrap_or(data.len());
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            out.push(&data[start..end]);
        }
    }
    out
}

fn generate_and_record(dir: &Path) -> usize {
    let raw = dir.join("pipeline.h264");
    let status = Command::new("sh")
        .arg(repo_root().join("fixtures/m2/gen_1440p.sh"))
        .arg(&raw)
        .status()
        .expect("執行 fixtures/m2/gen_1440p.sh");
    assert!(status.success(), "ffmpeg 產生 1440p 失敗");
    let data = std::fs::read(&raw).expect("讀 Annex B");
    let nals = split_annexb(&data);
    assert!(nals.len() > 100, "Annex B 太小：{} bytes", data.len());

    let cfg = RingConfig::new(dir);
    let (mut ring, _) = RingWriter::open(cfg).expect("開啟 ring");
    let vcl_per_loop = nals
        .iter()
        .filter(|nal| matches!(nal[0] & 0x1f, 1 | 5))
        .count();
    assert_eq!(vcl_per_loop, 250, "10 秒 × 25 fps 應有 250 個 VCL");
    let mut frames = 0u64;
    // 10 秒素材重播 19 次＝190 秒時間軸；每次重播都從 IDR 開始，是真可解碼的段。
    for _ in 0..19 {
        for nal in &nals {
            let is_vcl = matches!(nal[0] & 0x1f, 1 | 5);
            if is_vcl {
                frames += 1;
            }
            let at = 1_500 + (frames.saturating_sub(1)) * 40;
            ring.nal_at(nal, at);
        }
    }
    ring.finish().expect("收尾");
    assert_eq!(frames, 190 * 25, "應有 190 秒 × 25 fps 的 VCL");
    ring.index().rows().expect("索引 rows").len()
}

fn require_ffmpeg() {
    let out = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .expect("本機無 ffmpeg：AC①/②/④ 未驗（CI 用系統 ffmpeg；本機請跑 fixtures/m2/fetch_ffmpeg.sh）");
    assert!(out.status.success(), "ffmpeg -version 失敗");
}

/// 用 ffmpeg 的 trace_headers 數 SPS/PPS。MP4 的 extradata（avcC）會被算一次，
/// 所以乾淨的輸出應該是各 1 次；in-band 沒清乾淨每段 keyframe 會再多一次。
fn trace_parameter_sets(path: &Path) -> (usize, usize) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "trace", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-c",
            "copy",
            "-bsf:v",
            "trace_headers",
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("執行 ffmpeg trace_headers");
    assert!(out.status.success(), "ffmpeg trace_headers 失敗");
    let text = String::from_utf8_lossy(&out.stderr);
    let sps = text
        .lines()
        .filter(|line| line.contains("Sequence Parameter Set"))
        .count();
    let pps = text
        .lines()
        .filter(|line| line.contains("Picture Parameter Set"))
        .count();
    (sps, pps)
}

fn frame_pts_ms(segment: &Path) -> Vec<u64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(segment)
        .output()
        .expect("執行 ffprobe frame pts");
    assert!(out.status.success(), "ffprobe frame pts 失敗");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<f64>().ok())
        .map(|secs| (secs * 1000.0).round() as u64)
        .collect()
}

fn packet_pts(path: &Path) -> Vec<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("執行 ffprobe packets");
    assert!(out.status.success(), "ffprobe packets 失敗");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<f64>().ok())
        .collect()
}

fn ffmpeg_decode_errors(path: &Path) -> String {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .expect("執行 ffmpeg decode check");
    assert!(out.status.success(), "ffmpeg decode 失敗");
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn open_index(dir: &Path) -> SegmentIndex {
    SegmentIndex::open(RingConfig::new(dir).index_path()).expect("開索引")
}

fn ffprobe(path: &Path) -> serde_json::Value {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,width,height",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .expect("執行 ffprobe");
    assert!(out.status.success(), "ffprobe 失敗");
    serde_json::from_slice(&out.stdout).expect("ffprobe JSON")
}

#[test]
fn ac1_export_real_1440p_pipeline() {
    require_ffmpeg();
    let dir = temp_dir("ac1");
    let rows = generate_and_record(&dir);
    assert!(rows >= 80, "190 秒應切成至少 80 段，實得 {rows}");
    let output = dir.join("export.mp4");
    let out = Command::new(camctl())
        .args(["export", "--dir"])
        .arg(&dir)
        .args(["--from", "-100s", "--to", "-40s", "--output"])
        .arg(&output)
        .args(["--json"])
        .output()
        .expect("執行 camctl export");
    assert!(
        out.status.success(),
        "export 失敗：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let probe = ffprobe(&output);
    assert_eq!(probe["streams"][0]["codec_name"], "h264");
    assert_eq!(probe["streams"][0]["width"], 2560);
    assert_eq!(probe["streams"][0]["height"], 1440);
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("export stdout 必須是 JSON");
    let first = &report["outputs"][0];
    let first_start = first["t_start_ms"].as_u64().expect("t_start_ms");
    let rows = open_index(&dir).rows().expect("索引 rows");
    assert!(
        rows.iter().any(|r| r.t_start == first_start && r.first_block_keyframe == Some(true)),
        "輸出起點不是 keyframe 段首：{first_start}"
    );

    // B1′：用輸出段內的「幀數 × 40 ms」當獨立期望值，而不是拿匯出端
    // 自己算的 scan t_end-t_start；段邊界也必須留下至少半個幀距。
    let mut seg_blocks = Vec::new();
    for name in first["segments"].as_array().expect("segments[]") {
        let name = name.as_str().expect("segment name");
        let info = camcore::mkv::scan(&dir.join(name)).expect("scan source segment");
        seg_blocks.push(info.blocks as u64);
    }
    let frames: u64 = seg_blocks.iter().sum();
    let pts = packet_pts(&output);
    assert_eq!(
        pts.len() as u64,
        frames,
        "輸出 frame 數必須等於段內 frame 數之和（不得靠丟幀湊時間軸）"
    );
    let expected_ms = frames as f64 * 40.0;
    let duration: f64 = probe["format"]["duration"]
        .as_str()
        .expect("duration 字串")
        .parse()
        .expect("duration 數字");
    assert!(
        (duration * 1000.0 - expected_ms).abs() <= 500.0,
        "輸出 duration 必須是幀數×40ms ±0.5s：duration={duration:.3}s frames={frames} expected={expected_ms:.0}ms"
    );
    let mut boundary = 0usize;
    for blocks in seg_blocks.iter().take(seg_blocks.len().saturating_sub(1)) {
        boundary += *blocks as usize;
        let gap = pts[boundary] - pts[boundary - 1];
        assert!(
            gap >= 0.020,
            "第 {boundary} 幀的段邊界只有 {gap:.4}s（<半個幀距 20 ms），時間軸被壓縮"
        );
    }
    let errors = ffmpeg_decode_errors(&output);
    assert!(
        errors.is_empty(),
        "ffmpeg -v error 解碼輸出必須 0 行：\n{errors}"
    );

    // AC②：ffprobe 直接看 MP4 的第一個 frame，不能只看索引宣稱。
    let key = Command::new("ffprobe")
        .args([
            "-v", "error", "-select_streams", "v:0", "-read_intervals", "%+#1",
            "-show_entries", "frame=key_frame", "-of", "csv=p=0",
        ])
        .arg(&output)
        .output()
        .expect("執行 ffprobe frame");
    assert!(key.status.success(), "ffprobe frame 失敗");
    assert_eq!(
        String::from_utf8_lossy(&key.stdout).trim(),
        "1",
        "MP4 首幀不是 keyframe"
    );

    // F1：in-band SPS/PPS 必須是 0；轉 Annex B 後只剩 extradata 那一份，
    // 所以 type 7/8 各恰 1 次。只 remove8 的錯誤寫法會留下多個 SPS。
    let (sps, pps) = trace_parameter_sets(&output);
    assert_eq!(
        (sps, pps),
        (1, 1),
        "in-band SPS/PPS 未清乾淨（trace_headers 的 extradata 各算 1 次；實際 SPS={sps}、PPS={pps}）"
    );

    // AC④：截圖 IDR 對齊＋0.5 秒誤差＋尺寸；JSON 要帶所屬段段首 IDR 時間。
    let snap_path = dir.join("snapshot.jpg");
    let snap = Command::new(camctl())
        .args(["snapshot", "--dir"])
        .arg(&dir)
        .args(["--at", "-90s", "--output"])
        .arg(&snap_path)
        .args(["--json"])
        .output()
        .expect("執行 camctl snapshot");
    assert!(
        snap.status.success(),
        "snapshot 失敗：{}",
        String::from_utf8_lossy(&snap.stderr)
    );
    let snap_json: serde_json::Value =
        serde_json::from_slice(&snap.stdout).expect("snapshot stdout 必須是 JSON");
    let snap = &snap_json["snapshot"];
    let at_ms = snap["at_ms"].as_u64().expect("at_ms");
    let actual_ms = snap["actual_ms"].as_u64().expect("actual_ms");
    assert!(
        actual_ms.abs_diff(at_ms) <= 500,
        "截圖時間誤差 {} ms > 500 ms",
        actual_ms.abs_diff(at_ms)
    );
    assert_eq!(snap["width"], 2560);
    assert_eq!(snap["height"], 1440);
    let idr = snap["segment_idr_at_ms"].as_u64().expect("segment_idr_at_ms");
    assert!(
        rows.iter().any(|r| r.t_start == idr && r.first_block_keyframe == Some(true)),
        "snapshot 的段首 IDR {idr} 不是 keyframe 段首"
    );
    let jpeg = std::fs::read(&snap_path).expect("讀 JPEG");
    assert_eq!(&jpeg[..2], &[0xff, 0xd8], "不是 JPEG：{:?}", &jpeg[..2]);

    // AC④ 強化：actual_ms 必須是該段真實的 frame pts，不是自己算出的同一個數。
    let seg_name = snap["segment"].as_str().expect("snapshot segment");
    let seg_row = rows
        .iter()
        .find(|r| r.name == seg_name)
        .unwrap_or_else(|| panic!("索引找不到 snapshot 段 {seg_name}"));
    let pts = frame_pts_ms(&dir.join(seg_name));
    // ffprobe 對 MKV 的 pts 已是絕對時間（含段首 t_start），所以直接比對。
    assert!(
        pts.contains(&actual_ms),
        "actual_ms={actual_ms} 不是段 {seg_name}（t_start={}）的任何 frame pts；前幾個={:?}",
        seg_row.t_start,
        &pts.iter().take(5).collect::<Vec<_>>()
    );

    let duration: f64 = probe["format"]["duration"]
        .as_str()
        .expect("duration 字串")
        .parse()
        .expect("duration 數字");
    assert!(
        (56.0..=64.0).contains(&duration),
        "60 秒片段誤差應 ≤4 秒，實得 {duration}"
    );

    // AC⑤：export 與 snapshot 成功後都不得留下暫存檔。
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .expect("讀目錄")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with(".k14-") || name.ends_with(".concat") || name.ends_with(".part")
        })
        .collect();
    assert!(leftovers.is_empty(), "留下暫存檔：{leftovers:?}");
}
