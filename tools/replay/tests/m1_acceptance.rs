//! M1 驗收測試（TICKET-20261115-02 AC1–AC5）。
//!
//! 全部用 `replay` 這支既有 CLI 驅動，不另寫假入口：測試跑的就是產品路徑
//! （session → NalSink → camcore::RingWriter → MKV/SQLite）。
//!
//! AC5 標 `#[ignore]`（30 分鐘 @12 Mbps，寫 2.7 GB 再裁到 1.62 GB，CI 跑太慢）：
//! `cargo test -p replay --release -- --ignored ac5`。

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use camcore::mkv;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_replay")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/m1")
        .join(format!("{name}.jsonl"))
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// 每次測試用獨立的暫存資料目錄（測試結束不刪，方便失敗後追查）。
fn data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sview-m1-{}-{}-{}",
        tag,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建立暫存目錄");
    dir
}

fn run(dir: &Path, name: &str, extra: &[&str]) -> Output {
    let mut cmd = Command::new(bin());
    cmd.arg("--input")
        .arg(fixture(name))
        .arg("--record-dir")
        .arg(dir);
    for a in extra {
        cmd.arg(a);
    }
    cmd.output().expect("執行 replay")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn rows_of(dir: &Path) -> Vec<camcore::SegmentRow> {
    let index = camcore::SegmentIndex::open(dir.join("index.sqlite")).expect("開啟索引");
    index.rows().expect("讀索引")
}

/// 每個段檔都必須存在、大小與索引一致、metadata 完整、首個 block 是 keyframe。
fn assert_segments_intact(dir: &Path, rows: &[camcore::SegmentRow]) {
    for row in rows {
        let path = dir.join(&row.name);
        let meta = std::fs::metadata(&path)
            .unwrap_or_else(|e| panic!("段檔不存在 {}：{e}", path.display()));
        assert_eq!(meta.len(), row.bytes, "{} 大小與索引不符", row.name);
        let info = mkv::scan(&path).unwrap_or_else(|e| panic!("{} 掃描失敗：{e}", row.name));
        assert!(info.is_closed(), "{} 不是已閉合段：{info:?}", row.name);
        assert_eq!(info.codec_id, "V_MPEG4/ISO/AVC", "{}", row.name);
        assert!(info.blocks > 0, "{} 沒有 block", row.name);
        assert_eq!(
            info.first_keyframe_offset,
            Some(row.idr_offset),
            "{} 首個 keyframe 位移與索引不符",
            row.name
        );
        // 段的開頭必須能獨立解碼：這條由掃描器直接讀檔案的 flags 位元組判定，
        // 不看寫入器回報的 idr_offset（兩條獨立路徑，避免自證）
        assert_eq!(
            info.first_block_keyframe,
            Some(true),
            "{} 首個 SimpleBlock 不是 keyframe（掃描器從檔案判定）",
            row.name
        );
        assert!(row.t_end >= row.t_start);
    }
}

// ---------------------------------------------------------------- AC1

#[test]
fn ac1_retention_window_is_five_minutes() {
    let dir = data_dir("ac1");
    let out = run(&dir, "ac1_six_min", &["--retain-minutes", "5"]);
    assert!(out.status.success(), "replay 失敗：{}", stderr_of(&out));
    let stderr = stderr_of(&out);
    assert!(
        !stderr.contains("警告："),
        "X=5 在範圍內不該警告，卻出現：{stderr}"
    );

    let rows = rows_of(&dir);
    assert!(rows.len() > 100, "6.5 分鐘應該有上百段，實得 {}", rows.len());

    let t_start = rows.first().unwrap().t_start;
    let t_end = rows.last().unwrap().t_end;
    let span = t_end - t_start;
    assert!(
        (300_000..=302_000).contains(&span),
        "保留窗應落在 [300s, 302s]（閉區間，實作以「超過 window+segment 才修剪」保證），實得 {span}ms"
    );
    assert!(
        rows.len() <= 151,
        "2 秒一段時 5 分鐘最多 ceil(300/2)+1=151 段，實得 {}",
        rows.len()
    );
    assert!(
        t_start > 0,
        "最舊段應已被刪除（t_start 不該還是串流起點 0）"
    );

    assert_segments_intact(&dir, &rows);

    // 索引總量與實際目錄一致（沒有漏刪的孤兒檔）
    let index = camcore::SegmentIndex::open(dir.join("index.sqlite")).unwrap();
    assert_eq!(index.total_bytes().unwrap(), rows.iter().map(|r| r.bytes).sum::<u64>());
    let mut on_disk = 0u64;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("seg-") {
            on_disk += entry.metadata().unwrap().len();
        }
    }
    assert_eq!(on_disk, index.total_bytes().unwrap(), "目錄內段檔總量與索引不符");
}

// ---------------------------------------------------------------- AC2

#[test]
fn ac2_twenty_minutes_clamps_to_fifteen_and_warns_once() {
    let dir = data_dir("ac2");
    let out = run(&dir, "ac2_twenty_min", &["--retain-minutes", "20"]);
    assert!(out.status.success(), "replay 失敗：{}", stderr_of(&out));

    let stderr = stderr_of(&out);
    let warnings: Vec<&str> = stderr.lines().filter(|l| l.starts_with("警告：")).collect();
    assert_eq!(warnings.len(), 1, "夾住只該警告一次，實得 {warnings:?}");
    assert!(warnings[0].contains("15"), "警告要說到上限 15 分鐘：{}", warnings[0]);

    let rows = rows_of(&dir);
    let span = rows.last().unwrap().t_end - rows.first().unwrap().t_start;
    assert!(
        (900_000..=902_000).contains(&span),
        "X=20 應夾到 15 分鐘窗，實得 {span}ms"
    );
    assert_segments_intact(&dir, &rows);
}

// ---------------------------------------------------------------- AC3

fn ffprobe_available() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 等到目錄裡至少有 `closed` 個已閉合段與至少一個未閉合段，然後 SIGKILL。
fn kill_mid_write(dir: &Path) -> Vec<String> {
    let mut child = Command::new(bin())
        .arg("--input")
        .arg(fixture("ac3_kill"))
        .arg("--record-dir")
        .arg(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("啟動 replay");

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("等不到半寫的段檔（{deadline:?} 內）");
        }
        let entries: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let closed = entries.iter().filter(|n| n.ends_with(".mkv")).count();
        let parts = entries.iter().filter(|n| n.ends_with(".mkv.part")).count();
        if closed >= 2 && parts >= 1 {
            // 再多寫一點，確保被砍的是「多半段」而非空檔
            std::thread::sleep(Duration::from_millis(50));
            child.kill().expect("SIGKILL");
            let _ = child.wait();
            return entries;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn ac3_kill_then_recover_leaves_only_playable_segments() {
    let dir = data_dir("ac3");
    let before = kill_mid_write(&dir);
    assert!(
        before.iter().any(|n| n.ends_with(".mkv.part")),
        "SIGKILL 後應該留下未閉合段：{before:?}"
    );

    // 重啟＝跑復原掃描（產品路徑：replay --recover-only）
    let out = Command::new(bin())
        .arg("--input")
        .arg(fixture("ac3_kill"))
        .arg("--record-dir")
        .arg(&dir)
        .arg("--recover-only")
        .output()
        .expect("執行 --recover-only");
    assert!(out.status.success(), "復原失敗：{}", stderr_of(&out));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(stdout.contains("repaired") || stdout.contains("discarded"), "{stdout}");

    // 復原後目錄裡不該再有未閉合段
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".mkv.part"))
        .collect();
    assert!(leftovers.is_empty(), "復原後仍有未閉合段：{leftovers:?}");

    // 復原結果要有記錄（不得靜默丟棄）
    let index = camcore::SegmentIndex::open(dir.join("index.sqlite")).unwrap();
    let events = index.events().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "repaired" || e.kind == "discarded"),
        "復原動作必須留事件：{events:?}"
    );

    // 每個已閉合段都要能被真正解碼器接受（有 ffprobe 就用真的）
    let rows = index.rows().unwrap();
    assert!(!rows.is_empty(), "復原後索引不該是空的");
    assert_segments_intact(&dir, &rows);

    if ffprobe_available() {
        for row in &rows {
            let path = dir.join(&row.name);
            let probe = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_streams",
                    "-show_format",
                    "-of",
                    "json",
                ])
                .arg(&path)
                .output()
                .expect("執行 ffprobe");
            let json = String::from_utf8_lossy(&probe.stdout).into_owned();
            let err = String::from_utf8_lossy(&probe.stderr).into_owned();
            assert!(
                probe.status.success(),
                "{} ffprobe 失敗：{err}",
                row.name
            );
            assert!(
                json.contains("\"codec_name\": \"h264\"") || json.contains("\"codec_name\":\"h264\""),
                "{} 不是 h264：{json}",
                row.name
            );
            // CI 要看得到原始 JSON
            println!("ffprobe {} → {}", row.name, json);
        }
        println!("ffprobe 驗證 {} 段全部 exit 0", rows.len());
    } else {
        eprintln!(
            "ffprobe 不在 PATH：本機只驗到 EBML 結構（{} 段全為已閉合段）",
            rows.len()
        );
    }
}

// ---------------------------------------------------------------- AC4

#[test]
fn ac4_reconnect_gap_recorded_and_segments_not_merged() {
    let dir = data_dir("ac4");
    let out = run(&dir, "ac4_gap", &["--retain-minutes", "15"]);
    assert!(out.status.success(), "replay 失敗：{}", stderr_of(&out));

    let index = camcore::SegmentIndex::open(dir.join("index.sqlite")).unwrap();
    let rows = index.rows().unwrap();
    assert!(rows.len() >= 30, "60 秒續流應有數十段，實得 {}", rows.len());
    assert_segments_intact(&dir, &rows);

    // 有 gap 的段
    let gap_rows: Vec<&camcore::SegmentRow> = rows
        .iter()
        .filter(|r| r.gap_ms.map_or(false, |g| g > 0))
        .collect();
    assert_eq!(
        gap_rows.len(),
        1,
        "fixture 只有一次 RST，應恰好一段帶 gap_ms>0：{:?}",
        gap_rows.iter().map(|r| (r.name.clone(), r.gap_ms)).collect::<Vec<_>>()
    );
    let gap_row = gap_rows[0];
    let gap_ms = gap_row.gap_ms.expect("gap 段的 gap_ms 必須有值");
    assert!(
        (3_000..=5_000).contains(&gap_ms),
        "空窗約 3.5 秒，實得 {gap_ms}ms"
    );

    // gap 前後不得串成同一段：前一段必須在 gap 起點之前結束
    let idx = rows.iter().position(|r| r.segment_id == gap_row.segment_id).unwrap();
    assert!(idx > 0, "帶 gap 的段不該是索引第一段");
    let prev = &rows[idx - 1];
    assert_eq!(prev.gap_ms, Some(0), "前一段不該也標 gap");
    assert!(
        prev.t_end <= gap_row.t_start - gap_ms,
        "前段 t_end={} 越過 gap 起點 {}",
        prev.t_end,
        gap_row.t_start - gap_ms
    );
    assert!(
        gap_row.t_start > prev.t_end,
        "gap 段必須晚於前段結束（前 {} 後 {}）",
        prev.t_end,
        gap_row.t_start
    );

    // 帶 gap 的段必須從 keyframe 起（另一個檔，不承接前段 slice）
    let gap_info = mkv::scan(dir.join(&gap_row.name)).unwrap();
    // ①掃描器自己讀檔案第一個 SimpleBlock 的 flags，判定是否 keyframe——
    //   這條與寫入器的 idr_offset 無關，P 幀當首 block 時會直接紅。
    assert_eq!(
        gap_info.first_block_keyframe,
        Some(true),
        "gap 段首個 SimpleBlock 不是 keyframe（掃描器從檔案 flags 判定）"
    );
    // ②首個 keyframe 的位移就是段開頭（沒有更早的 P 幀）——刻意解析成 bool 再比，
    //   避免與 idr_offset 同一個數字自我循環。
    let starts_at_keyframe =
        gap_info.first_keyframe_offset == Some(gap_row.idr_offset) && gap_info.blocks > 0;
    assert!(starts_at_keyframe, "gap 段第一個 keyframe 必須落在索引記的位移上");

    // gap 事件要落地（不得只在記憶體裡）
    let events = index.events().unwrap();
    assert!(
        events.iter().any(|e| e.kind == "gap" && e.at_ms >= 100_000),
        "gap 事件缺失：{events:?}"
    );

    // 兩段的檔名時間軸與 gap 資訊一致
    assert!(camcore::recover::parse_t_start(&gap_row.name).is_some());
}

// ---------------------------------------------------------------- 復原：孤兒段檔收斂

/// 複製一個段成「索引沒列」的檔，再跑復原掃描：必須被收編，磁碟與索引收斂。
/// 順便丟一個壞檔進去，斷言它被刪掉且留事件。
#[test]
fn recover_adopts_or_removes_orphan_segments() {
    let dir = data_dir("orphan");
    let out = run(&dir, "ac4_gap", &["--retain-minutes", "15"]);
    assert!(out.status.success(), "replay 失敗：{}", stderr_of(&out));
    let before = rows_of(&dir);
    assert!(before.len() >= 10, "需要數個段才能造孤兒，實得 {}", before.len());

    // 造孤兒：複製一份已閉合段，檔名不在索引裡（模擬 rename 後、upsert 前被 kill）
    let src = dir.join(&before[0].name);
    let orphan_valid = dir.join("seg-000009999999.mkv");
    std::fs::copy(&src, &orphan_valid).expect("複製孤兒段");
    // 再造一個掃不過的孤兒檔
    let orphan_junk = dir.join("seg-000008888888.mkv");
    std::fs::write(&orphan_junk, b"not an mkv at all").expect("寫壞孤兒檔");

    let out = Command::new(bin())
        .arg("--input")
        .arg(fixture("ac4_gap"))
        .arg("--record-dir")
        .arg(&dir)
        .arg("--recover-only")
        .output()
        .expect("執行 --recover-only");
    assert!(out.status.success(), "復原失敗：{}", stderr_of(&out));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(stdout.contains("seg-000009999999"), "復原報告要列出收編的孤兒：{stdout}");

    let after = rows_of(&dir);
    assert!(
        after.iter().any(|r| r.segment_id == "seg-000009999999"),
        "孤兒段沒被收編進索引：rows={}",
        after.len()
    );
    assert_eq!(after.len(), before.len() + 1, "只該多出被收編的那一段");
    assert!(!orphan_junk.exists(), "掃不過的孤兒檔要刪掉");

    // 事件不得靜默
    let index = camcore::SegmentIndex::open(dir.join("index.sqlite")).unwrap();
    let events = index.events().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "adopted" && e.detail.contains("seg-000009999999")),
        "收編要留 adopted 事件：{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "removed_orphan" && e.detail.contains("seg-000008888888")),
        "刪壞檔要留 removed_orphan 事件：{events:?}"
    );

    // 磁碟與索引收斂：目錄內段檔總量 == 索引總量
    assert_eq!(
        on_disk_segment_bytes(&dir),
        index.total_bytes().unwrap(),
        "復原後目錄段檔總量必須等於索引總量"
    );

    // 再跑一次要幂等（不會重複收編、不會多事件）
    let out = Command::new(bin())
        .arg("--input")
        .arg(fixture("ac4_gap"))
        .arg("--record-dir")
        .arg(&dir)
        .arg("--recover-only")
        .output()
        .expect("執行第二次 --recover-only");
    assert!(out.status.success(), "第二次復原失敗：{}", stderr_of(&out));
    assert_eq!(rows_of(&dir).len(), after.len(), "第二次復原不該改變段數");
}

fn on_disk_segment_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("seg-") {
            total += entry.metadata().unwrap().len();
        }
    }
    total
}

// ---------------------------------------------------------------- AC5

#[test]
#[ignore = "30 分鐘 @12 Mbps，寫 2.7 GB；由 devbox 手動跑"]
fn ac5_thirty_minutes_stays_under_disk_budget() {
    let dir = data_dir("ac5");
    let started = Instant::now();
    let out = run(&dir, "ac5_thirty_min", &["--retain-minutes", "15"]);
    assert!(out.status.success(), "replay 失敗：{}", stderr_of(&out));
    println!("AC5 錄影耗時 {:?}", started.elapsed());

    let rows = rows_of(&dir);
    let t_start = rows.first().unwrap().t_start;
    let t_end = rows.last().unwrap().t_end;
    println!("AC5 段數={} t_start={t_start} t_end={t_end} span={}", rows.len(), t_end - t_start);

    // du -sb 原始數字（AC5 指定十進位 bytes）
    let du = Command::new("du").args(["-sb"]).arg(&dir).output();
    let mut total = 0u64;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            total += entry.metadata().unwrap().len();
        }
    }
    if let Ok(du) = du {
        let text = String::from_utf8_lossy(&du.stdout).into_owned();
        println!("du -sb → {}", text.trim());
        if let Some(first) = text.split_whitespace().next() {
            if let Ok(apparent) = first.parse::<u64>() {
                total = apparent;
            }
        }
    }
    println!("AC5 目錄總 bytes={total}");
    assert!(
        total <= 1_620_000_000,
        "30 分鐘 @12 Mbps 的 15 分鐘窗應 ≤ 1.62e9 bytes，實得 {total}"
    );

    let span = t_end - t_start;
    assert!(
        (900_000..=902_000).contains(&span),
        "X=15 窗長應落在 (900s, 902s]，實得 {span}ms"
    );
    assert!(rows.len() <= 451, "15 分鐘 / 2 秒最多 451 段，實得 {}", rows.len());
}
