# pots-server

Pots 服務：桌面版內嵌與多設備版共用的 gateway、儲存、agent run、Observational Memory 與任務樹。設計見 [`proposal/deployment.md`](../../proposal/deployment.md)、[`proposal/architecture.md`](../../proposal/architecture.md)、[`proposal/memory.md`](../../proposal/memory.md)、[`proposal/agent-collaboration.md`](../../proposal/agent-collaboration.md) 與 [`proposal/artifacts.md`](../../proposal/artifacts.md)。目前只支援桌面版的組合：SQLite、只綁 loopback、本機 token。

## 執行

```sh
cargo run -p pots-server -- [--data-dir <路徑>] [--port <port>]
```

桌面 app 用 `pots-desktop --serve` 跑同一個服務（`service::run_blocking`）。`RUST_LOG` 調整日誌等級。

## 模型 provider

對話用「使用中的 provider」，在設定裡切換（`SaveProvider`／`DeleteProvider`／`SetActiveProvider`／`TestProvider` 指令，狀態是快照與 `Providers` 事件裡的 `ProvidersView`）：

- **ChatGPT 方案**（內建，ID 固定）：SIWC 授權。模型以設定指定的為準，其次是 `POTS_CHAT_MODEL`，都沒有就用帳號可用清單的第一個。
- **OpenAI 相容／Anthropic 相容 API**：使用者自己新增，填 base URL、API key 與模型。OpenAI 相容的 key 可以不填（例如本機的 Ollama），Anthropic 相容的一定要填。

第一次啟動時沒有使用中的 provider（要到設定選）；但如果 ChatGPT 已經登入，就直接選 ChatGPT。還沒選 provider 就傳訊息，回覆會失敗並提示到設定新增。

整理記憶（Observer／Reflector）可以另選 provider 與模型（`SetMemoryModel`，狀態在 `ProvidersView::memory`），沒設定時跟對話相同。

## Observational Memory

對話 agent 的歷史是「記憶區塊 + 還沒觀察的 `om_raw` 尾巴」（`src/memory.rs`）。未觀察的超過約 3 萬 token 時，背景 Observer 把較早的部分（至少留最近 12 則）整理成觀察；觀察超過約 4 萬 token 時 Reflector 重寫成新的一代，釘選的保留。閾值在 `Config::memory`（`OmConfig`）。客戶端用 `PinObservation`／`EditObservation`／`DeleteObservation` 修改，記憶一變就送 `Memory` 事件；快照裡每個 project 也帶著它。

## 任務樹與 Planner

- 對話 agent 用 `create_goal` 開 Goal（同時建立 Goal 的 Brief artifact），`steer_goal` 改 Brief 並叫醒 Planner，`cancel_goal` 取消；也可以用 `write_artifact` 寫 Spec。每個 profile 都有唯讀的 `get_status`、`list_goals`、`read_artifact`。
- Planner（`src/planner.rs`）每個 Goal 同時一個 run，不掛記憶：每次從 Brief、Ledger、任務樹與觸發原因重建 context，用 `create_task`、`cancel_task`、`write_artifact`（Ledger）、`post_milestone`、`complete_goal` 工作。它的 run 排在 `agent_runs` 裡（`queued` → `running` 並持有 lease、定期心跳）；服務關閉或當掉時放回佇列，重試超過 3 次標成失敗。叫醒它的只有 Goal 建立、steer 與你改 Brief。
- M2 還沒有 worker：任務建立後停在 `Planned`，只會被 Planner、你（`Cancel { task }`）或 Goal 的結束取消。
- Planner 的里程碑是對話流裡的 `Card::Milestone`；對話 agent 的 instructions 帶著進行中的 Goal 與最近的里程碑（不寫進 `om_raw`）。
- Artifact（`src/artifacts.rs`）：agent 寫入帶 `base_rev`，以 `##` 區段編輯，版本不符時回衝突與期間的 diff；`read_artifact` 附上你在它上次讀取後的修改。你用 `SaveArtifact` 儲存（期間 agent 改過時以區段合併，同一段兩邊都改過就拒絕），`ArtifactHistory` 列出版本與 diff，`RestoreArtifact` 還原。

### Agent profile

對話 agent 與 Planner 的 profile 是 [`profiles/chat.toml`](profiles/chat.toml) 與 [`profiles/planner.toml`](profiles/planner.toml)，編進執行檔。資料目錄的 `profiles/<name>.toml` 存在時整份取代內建的（可以改 role prompt、`max_turns`、拿掉工具）；工具與可寫的 artifact 種類依 agent 檢查，不合法時服務不啟動。沒列在 profile 裡的工具不會交給模型。

## 資料目錄

| 檔案 | 內容 |
| --- | --- |
| `pots.db` | SQLite（WAL）：instance、projects、stream_items、events、agent_runs、om_raw、om_observations、om_state、model_connections、instance_settings、goals、tasks、task_events、artifacts、artifact_revisions、artifact_reads |
| `profiles/<name>.toml` | （選用）取代內建的 agent profile |
| `pots.lock` | 執行期間持有的檔案鎖；同一個目錄只能有一個服務 |
| `server.json` | 位址、啟動 token、pid、instance ID（`0600`）；正常結束時刪除 |
| `host-id` | SIWC 的 host ID，登出也保留 |
| `auth/chatgpt-credentials.json` | ChatGPT 授權（`0600`） |
| `auth/providers/<id>.json` | API provider 的 key（`0600`）；不進資料庫、日誌與事件，事件只帶末四碼 |

目錄本身是 `0700`。

## 存取控制

每個請求（含 WebSocket 升級）都要帶 `Authorization: Bearer <server.json 的 token>`，`/healthz` 除外。`Host` 必須是 `127.0.0.1:<port>` 或 `localhost:<port>`，帶 `Origin` 標頭的請求一律拒絕，擋掉瀏覽器頁面的跨站請求與 DNS rebinding。

## 即時協定與 run

- 客戶端連上後第一個指令是 `Resume { last_seq }`：伺服器補送事件日誌裡錯過的事件，`last_seq` 為 0 或差太多時改送快照；正在串流的訊息另外補送目前的全文。
- 每個 project 同時只跑一個 chat run，歷史由 Observational Memory 組成。執行中送來的訊息排隊，下一輪一起處理；「改方向」中斷目前的 run 後重來；「停止」中斷並清掉排隊。
- 服務當掉後重啟時，沒跑完的對話 run 標為失敗，串流到一半的訊息標上「服務中斷」；Planner 的 run 放回佇列接著跑。
- Goal 與 artifact 在快照裡（`ProjectSnapshot::goals`、`artifacts`），有變就送一筆 `GoalChanged`（整個 Goal 連同任務樹）或 `ArtifactChanged`。

## 驗證

```sh
cargo test -p pots-server   # 單元測試，加上用腳本化模型跑完整協定的整合測試（M2 的在 tests/tasks.rs）

# M1 的驗收對真的模型跑一次（會花 token）：早期做決定 → 對話累積超過 10 萬 token → 再問那個決定
POTS_OM_API_KEY=sk-... cargo run -p pots-server --example om_acceptance -- \
    --kind openai --base-url https://api.openai.com/v1 --model gpt-4.1-mini [--memory-model gpt-4.1-nano]
cargo run -p pots-server --example om_acceptance -- --kind chatgpt   # 第一次會開瀏覽器登入
```
