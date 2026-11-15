# fixtures

重放樣本，格式見 `crates/rtsp-core/src/replay.rs` 的模組註解（JSON Lines）。

| 檔案 | 來源 | 用途 | 已知值 |
|---|---|---|---|
| `sview-m0.jsonl` | **合成**（`gen_sview_m0.py` 產生） | 驗重連狀態機、depacket、序號缺口統計 | 3 段連線、2 次重連、1 個序號缺口（lost 4）、42 個 NAL |

規則：

- **真機錄製含 SViewWifi 權杖者不得 commit**。真機錄製請先剝掉權杖相關欄位，或只用來做本機驗證。
- 新增 fixture 請在 `meta.expected` 寫明 `connections` / `reconnects` / `seq_gaps` / `nals`；`replay --check`（預設開啟）會逐項比對，不符即 exit 3。
- 重新產生合成樣本：`python3 fixtures/gen_sview_m0.py > fixtures/sview-m0.jsonl`
