//! 啟動復原掃描（AC3）。
//!
//! 程式在寫段途中被 `kill -9` 時，磁碟上會留下：
//!
//! - `seg-<t_start>.mkv.part`：預配置過、超過實際資料長度的未閉合段。
//! - 索引裡可能缺這一段（甚至整個索引檔都沒 flush 完）。
//!
//! 復原規則：
//!
//! 1. 每個 `.part` 用 EBML scanner 找「最後一個完整 Cluster 結尾」，截掉其後的
//!    半截元素與預配置零尾巴，改名成 `.mkv`，寫成 `repaired` 段並留 event。
//! 2. 連 header／軌道都不完整（連一個完整 Cluster 都沒有）→ 刪檔，留 `discarded`
//!    event，絕不讓半截檔冒充可播放段。
//! 3. 索引有段但檔案不見了 → 刪索引列並留 `pruned` event。
//! 4. 索引完全缺失或為空 → 按檔名時序掃描所有 `.mkv` 重建索引，留 `index_rebuilt`
//!    event。

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::Result;
use crate::index::{SegmentIndex, SegmentRow};
use crate::mkv;

/// 復原結果摘要。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// 被修復（截斷＋改名）的段檔名。
    pub repaired: Vec<String>,
    /// 被丟棄的未閉合段檔名。
    pub discarded: Vec<String>,
    /// 被清掉的孤兒索引列。
    pub pruned: Vec<String>,
    /// 是否重建了索引。
    pub index_rebuilt: bool,
    /// 復原後索引段數。
    pub rows: usize,
}

/// 段檔名前綴。
pub const SEGMENT_PREFIX: &str = "seg-";
/// 段檔副檔名。
pub const SEGMENT_EXT: &str = ".mkv";
/// 未閉合段副檔名。
pub const SEGMENT_PART_EXT: &str = ".mkv.part";

/// 由檔名解析段起始時間（毫秒）。
pub fn parse_t_start(file_name: &str) -> Option<u64> {
    let rest = file_name.strip_prefix(SEGMENT_PREFIX)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok()
}

/// 由 `t_start` 產生段檔名。
pub fn segment_name(t_start: u64) -> String {
    format!("{SEGMENT_PREFIX}{t_start:012}{SEGMENT_EXT}")
}

/// 對 `dir` 執行復原掃描，結果寫進（並回報）`index`。
pub fn recover(dir: &Path, index: &mut SegmentIndex) -> Result<RecoveryReport> {
    fs::create_dir_all(dir)?;
    let mut report = RecoveryReport::default();

    // ---- 1/2. 未閉合段 ----
    let mut parts: Vec<String> = list_files(dir, SEGMENT_PART_EXT);
    parts.sort();
    for file_name in parts {
        let part_path = dir.join(&file_name);
        let final_name = file_name
            .strip_suffix(".part")
            .unwrap_or(&file_name)
            .to_string();
        let final_path = dir.join(&final_name);
        match mkv::scan(&part_path) {
            Ok(info) => {
                let end = info.complete_end();
                match end {
                    Some(end) if info.codec_id == "V_MPEG4/ISO/AVC" && end > 0 => {
                        // 截掉半截元素與預配置零尾巴
                        {
                            let f = fs::OpenOptions::new().write(true).open(&part_path)?;
                            f.set_len(end)?;
                            f.sync_all()?;
                        }
                        fs::rename(&part_path, &final_path)?;
                        let t_start =
                            parse_t_start(&final_name).unwrap_or(info.t_start_ms);
                        index.upsert(&SegmentRow {
                            segment_id: final_name
                                .trim_end_matches(SEGMENT_EXT)
                                .to_string(),
                            name: final_name.clone(),
                            t_start,
                            t_end: info.t_end_ms,
                            bytes: end,
                            idr_offset: info.first_keyframe_offset.unwrap_or(0),
                            gap_ms: 0,
                            closed: true,
                            repaired: true,
                        })?;
                        index.insert_event(
                            info.t_end_ms,
                            "repaired",
                            &format!(
                                "{final_name}：截到最後完整 Cluster（{end} bytes，原檔 {} bytes）",
                                info.bytes
                            ),
                        )?;
                        report.repaired.push(final_name);
                    }
                    _ => {
                        let _ = fs::remove_file(&part_path);
                        index.insert_event(
                            info.t_end_ms,
                            "discarded",
                            &format!(
                                "{final_name}：沒有完整 Cluster（{} bytes），整段丟棄",
                                info.bytes
                            ),
                        )?;
                        report.discarded.push(final_name);
                    }
                }
            }
            Err(e) => {
                let _ = fs::remove_file(&part_path);
                index.insert_event(
                    0,
                    "discarded",
                    &format!("{final_name}：EBML 結構不完整（{e}），整段丟棄"),
                )?;
                report.discarded.push(final_name);
            }
        }
    }

    // ---- 3. 索引孤兒 ----
    for row in index.rows()? {
        if !dir.join(&row.name).exists() {
            index.delete(&row.segment_id)?;
            index.insert_event(
                row.t_end,
                "pruned",
                &format!("{}：索引有段但檔案已不存在，清掉索引列", row.segment_id),
            )?;
            report.pruned.push(row.segment_id);
        }
    }

    // ---- 4. 索引重建 ----
    if index.is_empty()? {
        let mut files = list_files(dir, SEGMENT_EXT);
        files.sort();
        let mut rebuilt = 0usize;
        for file_name in &files {
            match mkv::scan(dir.join(file_name)) {
                Ok(info) => {
                    let t_start = parse_t_start(file_name).unwrap_or(info.t_start_ms);
                    index.upsert(&SegmentRow {
                        segment_id: file_name.trim_end_matches(SEGMENT_EXT).to_string(),
                        name: file_name.clone(),
                        t_start,
                        t_end: info.t_end_ms,
                        bytes: info.bytes,
                        idr_offset: info.first_keyframe_offset.unwrap_or(0),
                        gap_ms: 0,
                        closed: info.is_closed(),
                        repaired: !info.is_closed(),
                    })?;
                    rebuilt += 1;
                }
                Err(e) => {
                    index.insert_event(
                        0,
                        "rebuilt_skip",
                        &format!("{file_name}：掃描失敗（{e}），不進索引"),
                    )?;
                }
            }
        }
        if rebuilt > 0 {
            report.index_rebuilt = true;
            index.insert_event(
                0,
                "index_rebuilt",
                &format!("索引缺失，按檔名時序重建 {rebuilt} 段"),
            )?;
        }
    }

    report.rows = index.count()?;
    Ok(report)
}

fn list_files(dir: &Path, suffix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(SEGMENT_PREFIX) && name.ends_with(suffix) {
            // `.mkv` 也會 match `.mkv.part` 的尾端，先排除
            if suffix == SEGMENT_EXT && name.ends_with(SEGMENT_PART_EXT) {
                continue;
            }
            out.push(name);
        }
    }
    out
}

/// 把一段資料寫成檔案（測試與工具用）。
pub fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    Ok(())
}
