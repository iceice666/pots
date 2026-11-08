# pots-server

Pots 服務：桌面版內嵌與多設備版共用的 gateway、儲存、agent run、Observational Memory、任務樹與外部事件。設計見 [`proposal/deployment.md`](../../proposal/deployment.md)、[`proposal/architecture.md`](../../proposal/architecture.md)、[`proposal/memory.md`](../../proposal/memory.md)、[`proposal/agent-collaboration.md`](../../proposal/agent-collaboration.md)、[`proposal/artifacts.md`](../../proposal/artifacts.md) 與 [`proposal/events-and-automation.md`](../../proposal/events-and-automation.md)。目前只支援桌面版的組合：SQLite、只綁 loopback、本機 token。

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
- M4 之前沒有 worker：任務建立後停在 `Planned`，只會被 Planner、你（`Cancel { task }`）或 Goal 的結束取消；`wait_for` 讓它等待的見下一節。
- Planner 的里程碑是對話流裡的 `Card::Milestone`；對話 agent 的 instructions 帶著進行中的 Goal 與最近的里程碑（不寫進 `om_raw`）。
- Artifact（`src/artifacts.rs`）：agent 寫入帶 `base_rev`，以 `##` 區段編輯，版本不符時回衝突與期間的 diff；`read_artifact` 附上你在它上次讀取後的修改。你用 `SaveArtifact` 儲存（期間 agent 改過時以區段合併，同一段兩邊都改過就拒絕），`ArtifactHistory` 列出版本與 diff，`RestoreArtifact` 還原。

## 外部事件、等待與自動化

- **GitHub**（`src/watcher/github.rs`）：設定視窗的 GitHub 類別存 fine-grained personal access token（`SaveGithub`，`TestGithub` 查帳號），每個 project 用 `SetProjectRepos` 設定監看的 repo。桌面版每 60 秒輪詢 repo 的 Events API（帶 ETag）；有公開網址時也可以設 repo webhook 打 `POST /webhooks/github`，以 webhook secret 驗 `X-Hub-Signature-256`、`X-GitHub-Delivery` 去重。webhook 與輪詢產生同樣的 `dedupe_key`，同時開著也只處理一次。第一次輪詢一個 repo 只記下進度，不把過去的事件當新的。
- **事件**（`src/watcher/event.rs`）：PR 開啟、review、留言、merge，issue 開啟、加 label、留言，以及同一個 sha 的 check suite 全部結束後的 `ci.finished`。他人寫的內容標成不可信，送進 agent 時放在引用區塊裡。
- **路由**（`src/watcher/mod.rs`）：先喚醒訂閱這個資源、處於 `Waiting` 的任務，再送給訂閱它的 Goal（listener 開的 Goal：同一個資源回到同一個 Goal），都沒有才比對 listener；全部沒對上只留在 `external_events`。
- **等待**：`wait_for` 把任務改成 `Waiting { on, deadline }` 並寫入訂閱；事件來了、`At` 的時間到了、`Subtasks` 都結束了或逾時，任務回到 `Planned`，並排一個 `worker` 的 `agent_runs`（M4 之前沒有 worker，會留在佇列）。每一輪輪詢都查一次等待中資源的現況，所以重啟或漏掉事件時也會補上。內建 profile 還沒有 `wait_for`（M4 給 worker）；Planner 的覆寫 profile 可以加上。
- **排程**（`src/automation.rs`）：對話 agent 與 Planner 用 `schedule` 設單次或週期（五個欄位的 cron，本機時區）喚醒：週期最短 10 分鐘、每個 project 最多 10 個、預設 30 天過期。sweeper 每分鐘觸發到期的排程；錯過幾次都只補一次。全域排程（總管）在 M6 之前觸發時只記一筆失敗。
- **Listener**：你用 `SaveListener` 建立的直接生效；對話 agent 用 `create_listener` 提案的要等你 `SetListenerEnabled` 啟用（M5 之前代替核准）。動作是開 Goal、快速修正（M4 之前開一個 quick Goal）或通知對話。可能來自任何人的事件要自動開工必須有白名單欄位（label、author）；每小時超過上限就暫停並在對話裡通知。每次觸發的預算在 M6。
- **暫停**：`PauseAutomations`／`ResumeAutomations`（一個 project 或全部，可以指定自動恢復時間）；暫停期間錯過的觸發不補跑。`pause_automations`／`resume_automations` 工具留給 M6 的總管。
- 自動化頁的資料（排程、listener、暫停、來源健康狀態）在快照的 `automation`，有變就整筆送 `Automation`。來源連續失敗或 token 無效時標成異常，並在監看 repo 的 project 對話裡貼一張通知。
- 排程、逾時與每小時上限用 `Config::watcher.clock`；測試用 `ManualClock` 撥時間。

### Agent profile

對話 agent 與 Planner 的 profile 是 [`profiles/chat.toml`](profiles/chat.toml) 與 [`profiles/planner.toml`](profiles/planner.toml)，編進執行檔。資料目錄的 `profiles/<name>.toml` 存在時整份取代內建的（可以改 role prompt、`max_turns`、拿掉工具）；工具與可寫的 artifact 種類依 agent 檢查，不合法時服務不啟動。沒列在 profile 裡的工具不會交給模型。

## 資料目錄

| 檔案 | 內容 |
| --- | --- |
| `pots.db` | SQLite（WAL）：instance、projects、stream_items、events、agent_runs、om_raw、om_observations、om_state、model_connections、instance_settings、goals、tasks、task_events、artifacts、artifact_revisions、artifact_reads、project_repos、sources、external_events、source_deliveries、subscriptions、schedules、listeners、automation_triggers、automation_pauses |
| `profiles/<name>.toml` | （選用）取代內建的 agent profile |
| `pots.lock` | 執行期間持有的檔案鎖；同一個目錄只能有一個服務 |
| `server.json` | 位址、啟動 token、pid、instance ID（`0600`）；正常結束時刪除 |
| `host-id` | SIWC 的 host ID，登出也保留 |
| `auth/chatgpt-credentials.json` | ChatGPT 授權（`0600`） |
| `auth/providers/<id>.json` | API provider 的 key（`0600`）；不進資料庫、日誌與事件，事件只帶末四碼 |
| `auth/sources/github.json` | GitHub token 與 webhook secret（`0600`）；客戶端只看得到 token 末四碼 |

目錄本身是 `0700`。

## 存取控制

每個請求（含 WebSocket 升級）都要帶 `Authorization: Bearer <server.json 的 token>`，`/healthz` 除外。`POST /webhooks/github` 也不檢查 token 與 `Host`（GitHub 經 tunnel 或反向代理呼叫），改以 webhook secret 驗簽；沒有設定 secret 時一律 404。`Host` 必須是 `127.0.0.1:<port>` 或 `localhost:<port>`，帶 `Origin` 標頭的請求一律拒絕，擋掉瀏覽器頁面的跨站請求與 DNS rebinding。

## 即時協定與 run

- 客戶端連上後第一個指令是 `Resume { last_seq }`：伺服器補送事件日誌裡錯過的事件，`last_seq` 為 0 或差太多時改送快照；正在串流的訊息另外補送目前的全文。
- 每個 project 同時只跑一個 chat run，歷史由 Observational Memory 組成。執行中送來的訊息排隊，下一輪一起處理；「改方向」中斷目前的 run 後重來；「停止」中斷並清掉排隊。
- 服務當掉後重啟時，沒跑完的對話 run 標為失敗，串流到一半的訊息標上「服務中斷」；Planner 的 run 放回佇列接著跑。
- Goal 與 artifact 在快照裡（`ProjectSnapshot::goals`、`artifacts`），有變就送一筆 `GoalChanged`（整個 Goal 連同任務樹）或 `ArtifactChanged`。

## 驗證

```sh
cargo test -p pots-server   # 單元測試，加上用腳本化模型跑完整協定的整合測試（M2 在 tests/tasks.rs，M3 在 tests/watcher.rs：假的 GitHub API 與手動時鐘）

# M1 的驗收對真的模型跑一次（會花 token）：早期做決定 → 對話累積超過 10 萬 token → 再問那個決定
POTS_OM_API_KEY=sk-... cargo run -p pots-server --example om_acceptance -- \
    --kind openai --base-url https://api.openai.com/v1 --model gpt-4.1-mini [--memory-model gpt-4.1-nano]
cargo run -p pots-server --example om_acceptance -- --kind chatgpt   # 第一次會開瀏覽器登入
```
