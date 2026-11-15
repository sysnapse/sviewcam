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
