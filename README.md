# SViewCam

Rust 桌面客戶端（目標：接 WiFi 相機、看即時串流、截圖、抓「前 X 分鐘」）。
本 repo 現階段只有 **M0 協議驗證 spike**（TICKET-20261115-01）：CLI，無 GUI、無錄影。

## 現況

| 階段 | 內容 | 狀態 |
|---|---|---|
| M0 | `rtsp-core` ＋ `tools/probe` ＋ `tools/replay` ＋ 合成 fixture | in-dev（fixture 標準 1–2 已可驗；真機標準 3–5 待相機測試窗） |
| M1–M5 | 環形緩衝、匯出、camd 常駐、egui GUI、杰理骨架 | ticket draft |

## 目標架構（v3 規劃）

- **adapter trait**（協議層）：只含開流／取 SDP／設參數等相機協議操作，實作有 `sview`（主力）與 `jl`（杰理原型，預設停用）。
- **core**：拉流、磁碟分段環形緩衝、截圖、匯出。環形緩衝刻意**不放** adapter trait——兩台相機都沒有回放能力，緩衝只能客戶端做，放在 core 才不會每換一台機就重寫一次錄影。
- **GUI**：egui。

`rtsp-core` 現時以 `Link` / `LinkFactory` 抽象傳輸層，真機與 fixture 共用同一條重連狀態機；M1 之後把 `sview` 收攏成 adapter trait 的第一個實作即可。

## 快速開始

```sh
cargo test --workspace                 # 全部測試（無相機可跑）
cargo run -p replay -- \
  --input fixtures/sview-m0.jsonl \
  --report /tmp/report.json --annexb /tmp/session.h264

# 真機（需要相機熱點 + 權杖）
SVIEW_TOKEN=<32 位數字> cargo run -p probe -- \
  --host 192.168.1.1 --duration 1800 --out /tmp/probe.json
```

`probe` 結束會印 JSON 統計：`rtp_packets`、`rtp_bytes`、`seq_gaps`、`gaps`（重連空窗）、`reconnects`。
沒有 RTP 時 exit code 1；權杖缺失時 exit 2（不會重試）。

`reconnects` 的定義是「斷流後**成功續流**的重連次數」：本會話至少收到一個 RTP 才計入，
與 `gaps` 的收口點（RST→本會話首個 RTP）是同一個事件。相機 PLAY 200 之後不送流的會話
即使握手全綠也不算續流——它會落入 RST 三類分類（多半是即時拒絕）並計入 `disconnects`，
而不是被誤記成「已重連成功」。

## 故障注入 harness（重連鑑別力）

驗收標準 2 要求「故障注入 harness 能檢出錯誤重連實作」，這要靠變異測試證明，
**不是**靠「測試全綠」推論。`scripts/mutant_harness.sh` 把
`crates/rtsp-core/src/session.rs` 的重連決策逐個改成已知錯誤的實作（每次只改一件事、
從暫存副本重建），跑 `cargo test -p rtsp-core --no-fail-fast`，判定規則是：

1. `good`（未變異）必須全綠，否則後續一切檢出無意義；
2. 每個 mutant 必須讓**指定的那一條測試**紅，只紅在別處不算檢出；
3. 注入前會先核對錨點在原始碼裡恰好一個，錨點失效直接 exit 4——否則「注入點消失」會被
   誤報成「測試抓不到」或反過來讓 mutant 靜默退化成 good；
4. 編譯不過**不算檢出**，另報 `COMPILE_ERROR` 並 exit 4，避免把「改壞到編不過」冒充成
   「測試抓到」；
5. 結束以 sha256 核對原始碼已還原（還原失敗 exit 3）。

| mutant | 注入的錯誤 | 必須紅的測試 |
|---|---|---|
| `naive` | 斷流後不重連，把 RST 當正常結束 | `rst_classes::midstream_rst_resets_backoff_after_rtp_success` |
| `mutantA` | 重連但不睡退避（立刻重連） | `rst_classes::midstream_rst_doubles_when_reconnect_gets_no_rtp` |
| `mutantB` | 只要不連號就當缺口（不容忍向後／亂序） | `rst_classes::backward_seq_jump_within_session_is_not_counted_as_loss` |
| `mutantC` | 不分類：定時回收被當中途 RST（白等一次退避） | `rst_classes::timed_recycle_reconnects_immediately_with_zero_backoff` |

四個 mutant 分別對應 protocol 文檔 §11.2（2026-11-15 14:3x 裁定為三類觸發）的
定時回收、即時拒絕、中途 RST，加上序號處理。

### 兩條由 harness 逼出來的修正（不是先想好的）

1. **mutantB 原本逃逸**。原因是舊測試只覆蓋「重連後序號重置」（那發生在會話邊界，
   `last_seq` 本來就該重置）與「向前缺口」，**沒有任何一條測試碰到「同一會話內序號向後跳」**。
   RTP 走 UDP，亂序屬預期行為，而 §12 未證清單並沒有排除它。補了
   `backward_seq_jump_within_session_is_not_counted_as_loss` 與
   `forward_gap_within_session_is_counted_once` 兩條後才發現：原本的實作把 `last_seq`
   無條件更新成最新收到的序號，於是向後那包會把 high-water mark 拉回去，
   令下一個正常包被**重複**計成缺口。現行實作（`session.rs` 序號段）只認「向前」：
   重複包與向後包計入 `out_of_order`，不動 mark。
2. **環形距離的符號陷阱**。`p.seq.wrapping_sub(prev) as i32` 對 u16 來說永遠非負，
   所以「`ahead < 0` 視為向後」那種分支是死碼，寫的時候看著合理、跑起來永遠不成立。
   現行以 u16 距離 `d` 判斷：`d == 1` 連號、`1 < d < 0x4000` 向前缺口、
   `d == 0 || d >= 0xC000` 亂序／重複、`0x4000 <= d < 0xC000` 屬「無法區分丟了大半圈與
   來源重置序號」的 §12 未證情況，選擇推進 mark 但不計丟包（留在舊 mark 會令之後每包
   都判成異常，卡死檢測是更壞的失敗模式）。

### 跑法

```sh
sh scripts/mutant_harness.sh              # good + 4 個 mutant，全部檢出才 exit 0
sh scripts/mutant_harness.sh mutantB      # 只跑一個
sh scripts/verify_all.sh                  # workspace 測試 → replay → harness → git 狀態
```

只用 POSIX sh ＋ busybox 工具（cargo、awk、sed、grep、sha256sum）。**刻意不用 python3**：
devbox 的 rust 模板是 alpine，容器內以 uid 1000 執行、`apk add` 會被拒，因此 python 版
harness 在驗收環境跑不起來；`fixtures/gen_sview_m0.py`（重生 fixture）仍需要 python3，
要在有 python3 的機器上跑，commit 的是生成的 `.jsonl`。


## 重連策略：按 RST 類別分別退避（protocol 文檔 §11.2）

單一退避不夠：相機有兩類已知 RST，加上時序尚未解釋的觸發。`session.rs` 在每次斷流時先分類，
再按類別決定退避（`RstClass`、`ReconnectPolicy`）：

| 類別 | 判別 | 動作 |
|---|---|---|
| `TimedRecycle` 定時回收 | 距上次 PLAY 成功 ≥ 45 s | 立即重連、退避歸零（設計內行為，不是故障） |
| `ImmediateReject` 即時拒絕 | 距 PLAY < 10 s 且本會話從未收到 RTP | 0.5→1→2→4 s（夾在 `max` 10 s 之下）；連續 5 次即停並報錯 |
| `MidStream` 中途 RST | 其他，**含未解釋的觸發**（例如觀測到的 ~31 s 短拉） | 1 s 起、每次加倍、成功收流後重置 |

另外有重連風暴上限：10 分鐘內超過 40 次重連即停止並上報，不讓它 quietly 燒 CPU。
「成功」的定義是**收到本會話第一個 RTP**，不是 PLAY 成功——即時拒絕的會話同樣會 PLAY 成功
然後 <10 s 被 RST，用 PLAY 當成功會令 5 次上限永遠不觸發。

報告多了四個欄位：`rst_timed_recycle`、`rst_immediate_reject`、`rst_midstream`、`backoff_ms`。

**門檻是設定值，不是寫死的常數。** `ReconnectPolicy` 的 `timed_recycle_after`（預設 45 s）、
`immediate_reject_before`（預設 10 s）、`immediate_ladder`、`immediate_limit`（5）、
`midstream_initial`（1 s）、`storm_window`／`storm_max_reconnects`（10 分鐘／40 次）全部由呼叫方提供，
`session.rs` 內沒有任何 `60` 秒硬編碼。實測 Δ 分桶與三類的對應：

| 實測 Δ | 落到哪一類 | 備註 |
|---|---|---|
| <10 s 且從未收 RTP | `ImmediateReject` | 即時拒絕，走階梯退避 |
| 約 31 s（Mine 觀測 5 次，exit 0，成因未定案） | `MidStream` | 未解釋的觸發不另設假設；要分開處理就把 `immediate_reject_before` 調成 32 s 之類，屬設定變更 |
| 約 60 s（59–64 s 定時回收） | `TimedRecycle` | 由 `timed_recycle_after` 判定，不看「60」這個數字 |
| 其他（有收流後被斷） | `MidStream` | 1 s 起、加倍、成功後重置 |

**不要寫死 60 秒**：60–64 s 只是「≥45 s」這個門檻下的一個觀測值；若日後實測發現回收週期改變，
改設定即可，不用改狀態機。

## 權杖紀律

SViewWifi 權杖由使用者從自己的配對資料／抓包取得：

- 只從環境變數讀（`probe --token-env`，預設 `SVIEW_TOKEN`）；
- 不寫入 repo、不寫入報告、不寫入日誌；
- `fixtures/` 只用合成樣本，真機錄製含權杖者不得 commit。

## 協議要點（來自技術文檔）

1. RTSP over TCP 554，路徑 `/test.264`；**DESCRIBE 必須帶 `SViewWifi` 頭**，所以 ffplay/ffmpeg 帶不了，須自寫客戶端。
2. SETUP 回應的 `destination` / `source` / `server_port=0-0` 全部不可信；RTP 實際打去我方申請的 `client_port`。
3. PLAY 的 URL 要帶結尾斜線，並帶 `Range: npt=0-`。
4. `sprop-parameter-sets` 的 base64 本身已含 `00 00 00 01`，寫 Annex-B 前必須先剝。
5. 相機開流後約 60–70 秒主動 RST；重連是核心功能，不是異常處理。
6. 上行 UDP 13000 有 16-byte 心跳（約 2 秒），模板 `28…79 00 00 00`。

## 文件

- 技術文檔：《自用 Wi-Fi 相機串流協議還原（SViewWifi）》2026-11-11
- 規劃：SViewCam Rust 架構規劃 v3
- Ticket：`sviewcam-tickets-2026-11-15.md`（Product + Develop 流程）
