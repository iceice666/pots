# pots-server

Pots 服務：桌面版內嵌與多設備版共用的 gateway、儲存、agent run、Observational Memory、任務樹、外部事件與 coding worker。設計見 [`proposal/deployment.md`](../../proposal/deployment.md)、[`proposal/architecture.md`](../../proposal/architecture.md)、[`proposal/memory.md`](../../proposal/memory.md)、[`proposal/agent-collaboration.md`](../../proposal/agent-collaboration.md)、[`proposal/artifacts.md`](../../proposal/artifacts.md) 、[`proposal/events-and-automation.md`](../../proposal/events-and-automation.md) 與 [`proposal/agent-runtime.md`](../../proposal/agent-runtime.md#m4-的實作)。目前只支援桌面版的組合：SQLite、只綁 loopback、本機 token。

## 執行

```sh
cargo run -p pots-server -- [--data-dir <路徑>] [--port <port>]
```

桌面 app 用 `pots-desktop --serve` 跑同一個服務（`service::run_blocking`）。`RUST_LOG` 調整日誌等級。

## 模型 provider

對話用「使用中的 provider」，在設定裡切換（`SaveProvider`／`DeleteProvider`／`SetActiveProvider`／`TestProvider` 指令，狀態是快照與 `Providers` 事件裡的 `ProvidersView`）：

- **ChatGPT 方案**（內建，ID 固定）：SIWC 授權。模型以設定指定的為準，其次是 `POTS_CHAT_MODEL`，都沒有就用帳號可用清單的第一個。
- **OpenAI 相容／OpenAI Responses／Anthropic 相容 API**：使用者自己新增，填 base URL、API key 與模型。OpenAI 相容的 key 可以不填（例如本機的 Ollama），其他種類一定要填。
- **OpenCode Zen／OpenCode Go**：內建 pi-ai 的模型目錄（`crates/pots-proto/data/provider-presets.json`），依模型自動選 Anthropic Messages、Responses 或 Chat Completions；新增時預填 `x-opencode-client: pots`，`x-opencode-session` 由每個 run 的 `session_id` 自動帶。可以另外編輯每次請求的 header（`model_connections.headers`；值不是秘密，不要放 API key）。

第一次啟動時沒有使用中的 provider（要到設定選）；但如果 ChatGPT 已經登入，就直接選 ChatGPT。還沒選 provider 就傳訊息，回覆會失敗並提示到設定新增。

整理記憶（Observer／Reflector）可以另選 provider 與模型（`SetMemoryModel`，狀態在 `ProvidersView::memory`），沒設定時跟對話相同。

## Observational Memory

對話 agent 的歷史是「記憶區塊 + 還沒觀察的 `om_raw` 尾巴」（`src/memory.rs`）。未觀察的超過約 3 萬 token 時，背景 Observer 把較早的部分（至少留最近 12 則）整理成觀察；觀察超過約 4 萬 token 時 Reflector 重寫成新的一代，釘選的保留。閾值在 `Config::memory`（`OmConfig`）。客戶端用 `PinObservation`／`EditObservation`／`DeleteObservation` 修改，記憶一變就送 `Memory` 事件；快照裡每個 project 也帶著它。

## 任務樹與 Planner

- 對話 agent 用 `create_goal` 開 Goal（同時建立 Goal 的 Brief artifact），`steer_goal` 改 Brief 並叫醒 Planner，`cancel_goal` 取消；也可以用 `write_artifact` 寫 Spec。每個 profile 都有唯讀的 `get_status`、`list_goals`、`read_artifact`。
- Planner（`src/planner.rs`）每個 Goal 同時一個 run，不掛記憶：每次從 Brief、Ledger、任務樹與觸發原因重建 context，用 `create_task`、`cancel_task`、`write_artifact`（Ledger）、`post_milestone`、`complete_goal` 工作。它的 run 排在 `agent_runs` 裡（`queued` → `running` 並持有 lease、定期心跳）；服務關閉或當掉時放回佇列，重試超過 3 次標成失敗。叫醒它的有 Goal 建立、steer、你改 Brief、任務結束（完成、失敗、需要決定）與 worker 在 team 發的問題或阻礙。
- 任務建立時派給 worker（見「Coding worker」）；project 還沒設定 repo 時只記在任務樹上。
- Planner 的里程碑是對話流裡的 `Card::Milestone`；對話 agent 的 instructions 帶著進行中的 Goal 與最近的里程碑（不寫進 `om_raw`）。
- Artifact（`src/artifacts.rs`）：agent 寫入帶 `base_rev`，以 `##` 區段編輯，版本不符時回衝突與期間的 diff；`read_artifact` 附上你在它上次讀取後的修改。你用 `SaveArtifact` 儲存（期間 agent 改過時以區段合併，同一段兩邊都改過就拒絕），`ArtifactHistory` 列出版本與 diff，`RestoreArtifact` 還原。

## 外部事件、等待與自動化

- **GitHub**（`src/watcher/github.rs`）：設定視窗的 GitHub 類別存 fine-grained personal access token（`SaveGithub`，`TestGithub` 查帳號），每個 project 用 `SetProjectRepos` 設定監看的 repo。桌面版每 60 秒輪詢 repo 的 Events API（帶 ETag）；有公開網址時也可以設 repo webhook 打 `POST /webhooks/github`，以 webhook secret 驗 `X-Hub-Signature-256`、`X-GitHub-Delivery` 去重。webhook 與輪詢產生同樣的 `dedupe_key`，同時開著也只處理一次。第一次輪詢一個 repo 只記下進度，不把過去的事件當新的。
- **事件**（`src/watcher/event.rs`）：PR 開啟、review、留言、merge，issue 開啟、加 label、留言，以及同一個 sha 的 check suite 全部結束後的 `ci.finished`。他人寫的內容標成不可信，送進 agent 時放在引用區塊裡。
- **路由**（`src/watcher/mod.rs`）：先喚醒訂閱這個資源、處於 `Waiting` 的任務，再送給訂閱它的 Goal（listener 開的 Goal：同一個資源回到同一個 Goal），都沒有才比對 listener；全部沒對上只留在 `external_events`。
- **等待**：`wait_for` 把任務改成 `Waiting { on, deadline }` 並寫入訂閱；事件來了、`At` 的時間到了、`Subtasks` 都結束了或逾時，任務回到 `Planned`，並排一個 `worker` 的 `agent_runs`。每一輪輪詢都查一次等待中資源的現況，所以重啟或漏掉事件時也會補上。worker 的 profile 有 `wait_for`（只能等 project 監看的 repo）；Planner 的覆寫 profile 可以加上。
- **排程**（`src/automation.rs`）：對話 agent 與 Planner 用 `schedule` 設單次或週期（五個欄位的 cron，本機時區）喚醒：週期最短 10 分鐘、每個 project 最多 10 個、預設 30 天過期。sweeper 每分鐘觸發到期的排程；錯過幾次都只補一次。全域排程（總管）在 M6 之前觸發時只記一筆失敗。
- **Listener**：你用 `SaveListener` 建立的直接生效；對話 agent 用 `create_listener` 提案的要等你核准（`Decide`；M5 之前留下的提案用 `SetListenerEnabled` 啟用）。動作是開 Goal、快速修正（直接派一個 worker）或通知對話。可能來自任何人的事件要自動開工必須有白名單欄位（label、author）；每小時超過上限就暫停並在對話裡通知。每次觸發的預算在 M6。
- **暫停**：`PauseAutomations`／`ResumeAutomations`（一個 project 或全部，可以指定自動恢復時間）；暫停期間錯過的觸發不補跑。`pause_automations`／`resume_automations` 工具留給 M6 的總管。
- 自動化頁的資料（排程、listener、暫停、來源健康狀態）在快照的 `automation`，有變就整筆送 `Automation`。來源連續失敗或 token 無效時標成異常，並在監看 repo 的 project 對話裡貼一張通知。
- 排程、逾時與每小時上限用 `Config::watcher.clock`；測試用 `ManualClock` 撥時間。

## Coding worker

- **派工**（`src/worker/mod.rs`）：任務建立、被喚醒、收到交接或你 `RetryTask` 時排一個 `worker` 的 `agent_runs`；同時最多「Worker」設定的個數。worker 以 `complete_task`、`fail_task`、`escalate` 或 `wait_for` 結束一輪，沒有就結束時任務轉 `NeedsHuman`。
- **程式碼**（`src/worker/repo.rs`）：平台用 GitHub token 維護 bare 鏡像（`repos/`），每個任務一個獨立的 git repo（`worktrees/<任務>`、分支 `pots/…`）。平台在主機上不對工作目錄執行 git：worker 的 commit 由執行環境打成 bundle 交回，在鏡像裡檢查、push、開 PR（`open_pr`）。
- **執行環境**（`src/worker/exec.rs`）：一律是容器（`docker build -t pots-worker:latest docker/worker`）；本機執行只留給整合測試（`WorkerConfig::defaults`），資料庫裡舊的本機設定讀出時當成容器。
- **Project 環境**（`src/project_env.rs`、`src/worker/env_setup.rs`、`profiles/env.toml`、`0014_project_env.sql`）：`SetupProjectEnv` 開一個 `env` 種類的 Goal，派環境 worker 分析 repo；它用 `try_env` 建衍生 image（`pots-env-<project>:<hash>`，資料目錄的 `env-builds/`）並在新容器跑 `prepare`、`verify`，通過的規格才能用 `apply_env` 套用成新版本（不用核准，`UseProjectEnv` 可以換回其他版本或不用環境）。一般 worker 的 run 用使用中版本的 image、環境變數與快取（`worker-caches/<project>/`），先跑 `prepare`（上限 20 分鐘）；執行時間上限（60 分鐘，環境 worker 120 分鐘）從 `prepare` 之後才開始算。設計見 [`proposal/agent-runtime.md`](../../proposal/agent-runtime.md#project-環境由環境-worker-建立)。
- **後端**：內建 worker（pots-llm 的 loop，用使用中的模型 provider）或 Claude Code（`src/worker/claude.rs`：未修改的官方 CLI、雙向 stream-json；平台工具經 `src/worker/mcp.rs` 的 MCP server `/mcp`，每個 run 一個 capability token）。設定是 `SaveWorkerSettings`／`Worker` 事件；Claude Code 由你照設定裡的指令自己官方登入。
- **快速修改、subagent 與 team**：對話 agent 的 `quick_fix`、`run_subagent`（explorer／reviewer，唯讀），Planner 與 worker 的 `post_to_team`／`read_team`（`src/team.rs`）。
- `pots-fake-claude` 是測試用的假 Claude Code CLI（劇本格式見 `src/bin/pots-fake-claude.rs`），不是給使用者的程式。
- **CI 自動修**（`src/watcher/mod.rs`）：worker 等到的 CI 失敗時被叫醒自己修，連續失敗 3 次任務轉 `NeedsHuman`。

### Agent profile

對話 agent、Planner、worker 與 subagent 的 profile 是 [`profiles/chat.toml`](profiles/chat.toml)、[`profiles/planner.toml`](profiles/planner.toml)、[`profiles/worker.toml`](profiles/worker.toml)、[`profiles/explorer.toml`](profiles/explorer.toml) 與 [`profiles/reviewer.toml`](profiles/reviewer.toml)，編進執行檔。資料目錄的 `profiles/<name>.toml` 存在時整份取代內建的（可以改 role prompt、加上 `max_turns` 限制單次 run 的模型輪數、拿掉工具；內建的 profile 都不寫 `max_turns`，也就是不限制）；工具與可寫的 artifact 種類依 agent 檢查，不合法時服務不啟動。沒列在 profile 裡的工具不會交給模型。

## 核准與通知

- **核准**（`src/approvals.rs`）：merge PR（worker 與 Planner 的 `request_approval`）、merge 後刪除任務分支、agent 提案的 listener、USER.md 與 Project Notes 的提案。動作存成 JSON 與它的 SHA-256，你用 `Decide` 核准或拒絕（可附理由），平台只執行 hash 一致的動作：merge 帶核准時的 head sha，連網的動作在 commit 後另外執行、重啟時接著做。等核准的任務是 `Waiting { Approval }`；72 小時逾時轉 `NeedsHuman`；任務不再等、listener 被刪時自動撤回。每次變化記一筆 `approval_events` 並送 `ApprovalChanged`。
- **USER.md 與 Project Notes**（`src/user_md.rs`）：你用 `SaveUserMd` 直接編輯 USER.md、在 Artifacts 分頁編輯 Project Notes（每個 project 建立時一份）；放進對話 agent、Planner 與 worker 的指示。
- **通知設定**（`src/notifications.rs`）：`SaveNotifications`／`Notifications` 事件；系統通知由桌面客戶端發出。merge 的預設方式是 project 設定（`SetMergeMethod`）。

## 資料目錄

| 檔案 | 內容 |
| --- | --- |
| `pots.db` | SQLite（WAL）：instance、projects、stream_items、events、agent_runs、om_raw、om_observations、om_state、model_connections、instance_settings、goals、tasks、task_events、artifacts、artifact_revisions、artifact_reads、project_repos、sources、external_events、source_deliveries、subscriptions、schedules、listeners、automation_triggers、automation_pauses、worker_sessions、team_posts、team_reads |
| `profiles/<name>.toml` | （選用）取代內建的 agent profile |
| `pots.lock` | 執行期間持有的檔案鎖；同一個目錄只能有一個服務 |
| `server.json` | 位址、啟動 token、pid、instance ID（`0600`）；正常結束時刪除 |
| `host-id` | SIWC 的 host ID，登出也保留 |
| `auth/chatgpt-credentials.json` | ChatGPT 授權（`0600`） |
| `auth/providers/<id>.json` | API provider 的 key（`0600`）；不進資料庫、日誌與事件，事件只帶末四碼 |
| `auth/sources/github.json` | GitHub token 與 webhook secret（`0600`）；客戶端只看得到 token 末四碼 |
| `repos/`、`worktrees/` | repo 的 bare 鏡像與每個任務的工作目錄 |
| `worker-runs/` | 執行中 worker run 的設定（MCP 設定 `0600`）與輸出，run 結束就刪 |
| `claude-config/`、`worker-homes/` | 容器裡 Claude Code 的設定目錄（你自己官方登入、session）與每個任務的 home |

目錄本身是 `0700`。

## 存取控制

每個請求（含 WebSocket 升級）都要帶 `Authorization: Bearer <server.json 的 token>`，`/healthz` 除外。`/mcp`（Claude Code worker 的平台工具）不檢查本機 token 與 gateway 的 `Host`，改驗每個 worker run 的 capability token，另有自己的 Host 白名單（loopback 與 `host.docker.internal`）。`POST /webhooks/github` 也不檢查 token 與 `Host`（GitHub 經 tunnel 或反向代理呼叫），改以 webhook secret 驗簽；沒有設定 secret 時一律 404。`Host` 必須是 `127.0.0.1:<port>` 或 `localhost:<port>`，帶 `Origin` 標頭的請求一律拒絕，擋掉瀏覽器頁面的跨站請求與 DNS rebinding。

## 即時協定與 run

- 客戶端連上後第一個指令是 `Resume { last_seq }`：伺服器補送事件日誌裡錯過的事件，`last_seq` 為 0 或差太多時改送快照；正在串流的訊息另外補送目前的全文。
- 每個 project 同時只跑一個 chat run，歷史由 Observational Memory 組成。執行中送來的訊息排隊，下一輪一起處理；「改方向」中斷目前的 run 後重來；「停止」中斷並清掉排隊。
- 服務當掉後重啟時，沒跑完的對話 run 標為失敗，串流到一半的訊息標上「服務中斷」；Planner 與 worker 的 run 放回佇列接著跑（Claude Code 以 `--resume` 續接 session）。
- Goal 與 artifact 在快照裡（`ProjectSnapshot::goals`、`artifacts`），有變就送一筆 `GoalChanged`（整個 Goal 連同任務樹）或 `ArtifactChanged`。

## 驗證

```sh
cargo test -p pots-server   # 單元測試，加上用腳本化模型跑完整協定的整合測試（M2 在 tests/tasks.rs，M3 在 tests/watcher.rs：假的 GitHub API 與手動時鐘，M4 在 tests/worker.rs：本機 bare repo 當 remote、假的 Claude Code CLI；project 環境也在這裡，以本機執行代替容器）

# M1 的驗收對真的模型跑一次（會花 token）：早期做決定 → 對話累積超過 10 萬 token → 再問那個決定
POTS_OM_API_KEY=sk-... cargo run -p pots-server --example om_acceptance -- \
    --kind openai --base-url https://api.openai.com/v1 --model gpt-4.1-mini [--memory-model gpt-4.1-nano]
cargo run -p pots-server --example om_acceptance -- --kind chatgpt   # 第一次會開瀏覽器登入
```
