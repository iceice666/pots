# pots-server

Pots 服務：桌面版內嵌與多設備版共用的 gateway、儲存與 agent run。設計見 [`proposal/deployment.md`](../../proposal/deployment.md) 與 [`proposal/architecture.md`](../../proposal/architecture.md)。目前（M0）只支援桌面版的組合：SQLite、只綁 loopback、本機 token。

## 執行

```sh
cargo run -p pots-server -- [--data-dir <路徑>] [--port <port>]
```

桌面 app 用 `pots-desktop --serve` 跑同一個服務（`service::run_blocking`）。模型用 ChatGPT 方案（SIWC 授權），`POTS_CHAT_MODEL` 可以指定模型 slug，沒指定就用帳號可用清單的第一個。`RUST_LOG` 調整日誌等級。

## 資料目錄

| 檔案 | 內容 |
| --- | --- |
| `pots.db` | SQLite（WAL）：instance、projects、stream_items、events、agent_runs、om_raw |
| `pots.lock` | 執行期間持有的檔案鎖；同一個目錄只能有一個服務 |
| `server.json` | 位址、啟動 token、pid、instance ID（`0600`）；正常結束時刪除 |
| `host-id` | SIWC 的 host ID，登出也保留 |
| `auth/chatgpt-credentials.json` | ChatGPT 授權（`0600`） |

目錄本身是 `0700`。

## 存取控制

每個請求（含 WebSocket 升級）都要帶 `Authorization: Bearer <server.json 的 token>`，`/healthz` 除外。`Host` 必須是 `127.0.0.1:<port>` 或 `localhost:<port>`，帶 `Origin` 標頭的請求一律拒絕，擋掉瀏覽器頁面的跨站請求與 DNS rebinding。

## 即時協定與 run

- 客戶端連上後第一個指令是 `Resume { last_seq }`：伺服器補送事件日誌裡錯過的事件，`last_seq` 為 0 或差太多時改送快照；正在串流的訊息另外補送目前的全文。
- 每個 project 同時只跑一個 chat run，帶完整的原始歷史（M0 還沒有記憶壓縮）。執行中送來的訊息排隊，下一輪一起處理；「改方向」中斷目前的 run 後重來；「停止」中斷並清掉排隊。
- 服務當掉後重啟時，沒跑完的 run 標為失敗，串流到一半的訊息標上「服務中斷」。

## 驗證

```sh
cargo test -p pots-server   # 單元測試，加上用腳本化模型跑完整協定的整合測試
```
