# Agent 執行層

[← 產品提案索引](product.md)

## Agent 執行層（pots-llm）

對話 agent、Planner 與內建 worker 用 workspace crate `pots-llm` 實作：`ModelProvider` trait 抽象模型推論，`agent::run` 是所有 profile 共用的多輪工具迴圈。ChatGPT Plan provider 提供模型推論，Claude Code adapter 接入外部 coding agent runtime。這是兩種不同層級的整合，不引入 Codex runtime，也不把 Claude 訂閱 token 接成 Pots 的 provider。平台能力以 `pots_llm::agent::Tool` 暴露給內建 agent，以 MCP 暴露給 Claude Code；watcher 是確定性的 Rust 程式碼，不是 LLM agent。

### 角色

| 角色 | 實作 | 觸發 | 並行度 |
| --- | --- | --- | --- |
| 對話 agent | 內建 agent（pots-llm 的 loop），掛 OM | 人類訊息、里程碑、升級事件 | 每個 project 同時 1 個 run |
| Planner | 內建 agent，不掛 OM；context 只有 Goal brief 與任務樹 | Goal 建立、子任務結束、steer | 每個 Goal 同時 1 個 run |
| Worker | 內建 worker，或 Claude Code adapter | 被分派、被喚醒 | 多個，每個任務一個 worktree 與容器；可指定在[託管電腦](computer-nodes.md)上執行 |
| Operator | 內建 agent（需支援影像輸入的模型），或 Claude Code 透過 MCP | 被分派、被喚醒 | 每台 node 同時 1 個（display lease） |
| Watcher | 純 Rust（webhook 或輪詢） | 外部事件 | 不限 |
| 總管 agent | 內建 agent，掛自己的 OM（全域對話） | 人類在總管對話中的訊息、自己的排程 | 全域同時 1 個 run |

### 對話 agent 與 Planner 的分工

拆成兩個 agent，讓對話永遠能即時回應：長時間的規劃與重新規劃都在 Planner 的 run 裡進行，不會卡住你和對話 agent 的討論。

- 交接靠 Goal brief。 討論收斂時，對話 agent 呼叫 `create_goal` ，把目標、約束、已決定事項寫成一份結構化 brief。Planner 只讀 brief 與任務樹，不讀對話。

- 改方向。 你在對話中改變主意時，對話 agent 呼叫 `steer_goal` 更新 brief 並發出 steer 事件，Planner 被喚醒後重新規劃。

- 回報。 Planner 的里程碑與升級事件以卡片形式回到對話流，對話 agent 可以在下一輪看到並轉述。

- 好處。 Planner 的 context 小而穩定，容易快取；brief 本身也是你可以檢視、修改的「這個 Goal 到底要做什麼」。

### 總管 agent：跨 project 的入口

總管 agent 是所有 project 之上的一條永久對話，用來問「整體狀況」和派工到某個 project，本身不執行工作。

- 入口。 桌面客戶端是左欄「總管」進入的獨立畫面，任何頁面都可以用快捷鍵切過去；手機版是底部的「總管」分頁。

- 範圍。 「所有 project」指的是同一個實例裡的 project。客戶端同時連到多個實例時，每個實例有自己的總管與 OM；v1 不做跨實例總管（見[多實例並存](deployment.md#客戶端多連線)）。

- 記憶。 有自己的 OM，conversation id 固定為 `global` ，記得你跨 project 的討論與習慣。

- 典型問題。 「今天有什麼卡住？」「上週那個 500 錯誤的調查在哪個 project？」「把 api 那邊的 flaky test 交給它修」「暫停所有 project 的排程到週一」。

- 只透過任務樹派工。 它用 `create_goal` 在指定 project 開 Goal，該 project 的對話裡同時出現一張 Goal 卡片並註明來自總管。它不直接指揮 Planner 或 worker，維持「任務樹是唯一真相」。

- 取消 Goal。 用 `cancel_goal` 取消任一 project 的 Goal，該 project 的對話裡出現取消卡片並註明來自總管。個別任務的取消仍由該 Goal 的 Planner 負責。

- 暫停自動化。 `pause_automations` 暫停指定 project 或所有 project 的排程與 listener，可帶 `until` 指定自動恢復時間；`resume_automations` 提前恢復。暫停期間錯過的觸發不補跑，自動化頁面顯示暫停橫幅，你也可以直接按「立即恢復」。

- 自己的排程。 用 `schedule` 建立 owner 為 `Global` 的排程，例如每天早上的跨 project 摘要；醒來時在總管對話裡帶著 OM 繼續。

- 權限。 可以讀所有 project 的狀態與歷史，但核准一律仍在原 project 的核准卡片或 Inbox 完成。

### 平台工具

各種 agent 拿到哪些工具由 profile 決定，見[統一 Agent Loop 與協作模型](agent-collaboration.md)的 Profile 一覽與唯讀基礎工具。

`get_status` 是所有狀態問題的唯一答案來源，preamble 明確要求「關於任務、PR、CI 的問題先呼叫 `get_status` ，不要從記憶回答」。

### pots-llm：模型介面與共用 loop

模型層是 workspace crate [`crates/pots-llm`](../crates/pots-llm/)，不依賴 agent 框架。

- `provider`：與模型無關的型別。`Item` 是歷史的單位：`User`、`Assistant`、`ToolCall`、`ToolResult`，以及 provider 專屬、須原樣回送的 `Opaque`（例如 Responses 的 reasoning item）。`ModelRequest` 帶 `model`、`instructions`、完整的 `input: Vec<Item>` 與 `tools: Vec<ToolSpec>`。`ModelProvider::stream` 回傳 `StreamEvent` 串流：`TextDelta`、`ItemDone(Item)`、`Completed { usage }`。沒收到 `Completed` 就結束的串流是錯誤，不算成功。

- `LlmError`：`NeedsLogin`、`NotEntitled`、`UsageLimit`、`Unavailable`、`Unsupported`、`Interrupted`、`Cancelled`、`Other`。只有 `Unavailable` 與 `Interrupted` 值得重試同一個請求；訊息不含 token。

- `responses`：ChatGPT Plan 用的 Responses provider。`siwc`：SIWC 登入、憑證儲存與單一 refresh owner。

- `agent::run(config, history, events, cancel)`：一輪是一次模型串流。輸出項目收到 `Completed` 後才依序以 `AgentEvent::Item` 送出並加入歷史；串流失敗時這一輪的項目全部丟棄，歷史裡不會留下沒有結果的工具呼叫。模型要求工具時，依序問 `Hook::before_tool`、執行工具、把 `ToolResult` 放回歷史，再開下一輪；只回文字就以 `RunOutcome::Done` 結束。

- `Hook`：`before_turn` 回 `false` 就乾淨結束（`Stopped`，用於預算、cancel、steer）；`before_tool` 回 `ToolDecision::{Allow, Deny, AllowThenStop, Stop}`；`after_tool` 用於追蹤。`AllowThenStop` 執行後結束本輪，`Stop` 不執行並結束本輪；同一輪剩下的呼叫各補一個「未執行」結果，run 以 `Suspended` 結束。用完 `max_turns` 是 `MaxTurns`。

- 取消在每個 await 點生效；工具階段被取消時，還沒有結果的呼叫先補上「已取消」結果，持久化的歷史保持完整。

- 伺服器把每個 `AgentEvent::Item` 序列化寫進 `om_raw`，`TextDelta` 轉成 UI 的串流事件。

換 provider 只需實作 `ModelProvider`；loop、`Hook`、工具與 OM 不受影響。

### 工具實作範例

```rust
use pots_llm::agent::{Tool, ToolError};
use pots_llm::ToolSpec;
use schemars::JsonSchema;
use serde::Deserialize;
#[derive(Deserialize, JsonSchema)]
pub struct WaitArgs {
    task_id: TaskId,
    on: WaitCond,
    timeout_minutes: u32,
}
pub struct WaitFor { store: TaskStore }
#[async_trait::async_trait]
impl Tool for WaitFor {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "wait_for".into(),
            description: "掛起任務直到條件成立（CI 結束、PR 被 review、子任務完成）。 \
                          呼叫後本輪即結束，等待期間不消耗 token。".into(),
            parameters: serde_json::to_value(schemars::schema_for!(WaitArgs))
                .expect("schema"),
        }
    }
    // 參數是模型給的 JSON 字串；錯誤會轉成文字送回模型，讓它自己修正
    async fn call(&self, arguments: &str) -> Result<String, ToolError> {
        let a: WaitArgs = serde_json::from_str(arguments)
            .map_err(|e| ToolError(format!("參數錯誤：{e}")))?;
        let deadline = now() + minutes(a.timeout_minutes);
        self.store.suspend(a.task_id, a.on, deadline).await
            .map_err(|e| ToolError(e.to_string()))?;
        Ok("suspended".into())
    }
}
```

「呼叫後本輪即結束」由 Gate 對 Suspend 類工具回 `ToolDecision::AllowThenStop` 保證，不靠工具本身或 prompt。

### 組裝對話 agent 與 Planner

下例示意角色與工具組裝。provider 由設定選擇：ChatGPT Plan 是 `ResponsesProvider`（SIWC 授權），其他 API provider 各自實作 `ModelProvider`；ChatGPT Plan 的相容性要求見下節。

```rust
use std::sync::Arc;
use pots_llm::{Item, ModelProvider};
use pots_llm::agent::{self, RunConfig};
let provider: Arc<dyn ModelProvider> = llm.provider(&cfg.provider)?;
let (events, rx) = tokio::sync::mpsc::unbounded_channel(); // AgentEvent → UI 串流與 om_raw
// 對話 agent：掛 OM，負責和人類討論、建立與調整 Goal
let chat = RunConfig {
    provider: provider.clone(),
    model: cfg.chat_model.clone(),
    instructions: Some(compose_preamble(&user_md, &project.instructions)),
    tools: vec![
        Arc::new(GetStatus::new(store.clone())),
        Arc::new(CreateGoal::new(store.clone())),
        Arc::new(SteerGoal::new(store.clone())),
        Arc::new(Recall::new(db.clone())),
    ],
    hook: Arc::new(Gate::new(&CHAT, &scope, budget)),
    max_turns: 8,
};
// 歷史 = OM 的記憶區塊 + 未觀察的原始尾巴 + 觸發訊息，見 memory.md
let mut history = om.load(project.id).await?;
history.push(Item::User { text: human_text });
let result = agent::run(&chat, history, events.clone(), cancel.clone()).await?;
// Planner：不掛 OM，每次 run 都從 Goal brief 與任務樹重建 context
let planner = RunConfig {
    provider,
    model: cfg.planner_model.clone(),
    instructions: Some(compose_preamble(&user_md, PLANNER_INSTRUCTIONS)),
    tools: vec![
        Arc::new(GetStatus::new(store.clone())),
        Arc::new(CreateTask::new(store.clone())),
        Arc::new(CancelTask::new(store.clone())),
        Arc::new(RequestApproval::new(store.clone())),
        Arc::new(PostMilestone::new(store.clone())),
    ],
    hook: Arc::new(Gate::new(&PLANNER, &scope, budget)),
    max_turns: 12,
};
let history = vec![Item::User { text: render_brief_and_tree(&goal, &trigger) }];
agent::run(&planner, history, events, cancel).await?;
```

M0 還沒有 OM：對話 agent 的歷史直接取該 project 在 `om_raw` 的全部項目。

### 執行模型

1. 每一次 agent 執行是 `agent_runs` 的一列。Runner pool 用 `FOR UPDATE SKIP LOCKED` 取工作，持有 lease 並定期心跳。

2. lease 過期（程序當掉）時，任務回到 `Planned` 重新分派，`attempts` 加一。

3. `wait_for` 之後必須結束本輪：pots-llm 的 `Hook`（Gate）對它回 `ToolDecision::AllowThenStop`，loop 執行工具後即以 `RunOutcome::Suspended` 結束，而不是靠 prompt 要求模型自己停。

4. 喚醒時建立新的 run，帶入「你在等什麼、結果是什麼」作為觸發訊息。

### ChatGPT Plan provider：內建 agent 的模型推論後端

pots 採開源、本機或使用者自行架設的模式。透過官方 **Sign in with ChatGPT（SIWC）** 取得 ChatGPT plan usage 授權，直接呼叫 Responses API；不讀取 Codex 的 `auth.json`、不使用 ChatGPT 網頁 cookie，也不依賴 Codex CLI 或 app-server。這不授予存取使用者既有 ChatGPT 對話的權限。收費或平台代管服務不在本次範圍，若改變產品模式需重新確認資格。

- **責任邊界。** Provider（`pots_llm::responses::ResponsesProvider`）只把 `ModelRequest` 映射為 Responses 請求，把 SSE 轉成 `StreamEvent`（文字增量、完整的輸出項目、usage 與完成），錯誤轉成 `LlmError`；工具執行、多輪 loop、OM 與任務狀態仍由 pots 管理。對話 agent、Planner、總管、Observer／Reflector 與內建 worker 可依角色選用此 provider，但須驗證模型與能力，不假設所有角色皆可直接替換。
- **授權。** 使用系統瀏覽器與官方 OAuth／PKCE 流程；登入成功不等於取得訂閱推論權限。依官方文件核對（2026-11-07）的約定：
  - 動態註冊就是瀏覽器授權請求本身：首次帶 `client_id=dynamic_agent_client`、`agent_name_hint=Pots` 與 `ext_agent_host_id`，issued `client_id`（`oaiapp_...`）從 callback 帶回，之後沿用。沒有另外的註冊 POST，`dynamic_agent_client` 不是要保存的 ID。
  - scope 為 `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`，`resource=https://api.openai.com/v1`；PKCE S256，每次登入產生新的 `state` 與 `nonce`。
  - callback 是 loopback `http://127.0.0.1:<port>/auth/callback`，只有 port 可以變，不用 `localhost`；授權請求與 code exchange 帶完全相同的 URI。
  - Host ID 格式 `urn:uuid:<v4>`，第一次登入前產生，與憑證分開存在 `data_dir/host-id`，登出也不刪。
  - ID token 以 JWKS 驗證 RS256 簽章、issuer、audience（issued client ID）、期限與 nonce；帳號身分以驗證過的 `sub` 為準，不是 email。推論前確認 token 回應實際授予 `chatgpt.tokens.use.direct`。
  - refresh 會輪替 refresh token：同一份授權只有一個 refresh owner，新的 access／refresh token、期限與 scopes 一起原子寫入。
  - 登出時撤銷 refresh token，清掉本機 token，保留 issued client ID 與 Host ID；撤銷未確認時明確告知使用者。
- **憑證。** 帳號授權、憑證儲存／refresh、推論 adapter 分層。行程內由 `SiwcAuth` 以一把 async mutex 序列化 refresh，多個 run 不各自更新同一份憑證；推論回 401 時強制 refresh 一次再重送。M0 的憑證是資料目錄裡的 `auth/chatgpt-credentials.json`（`0600`，原子寫入）。秘密不進 prompt、資料匯出或 trace，部署方式見[部署](deployment.md)。
- **推論端點。** `POST https://api.openai.com/v1/responses`，只帶 `Authorization: Bearer <access token>`；是公開的 Responses API，不是 ChatGPT 的 `backend-api`。`GET /v1/models` 回傳 `models` 陣列（不是一般 API 的 `data`），取 `visibility == "list"` 的項目、保留伺服器順序；設定頁沒有指定模型（也沒有 `POTS_CHAT_MODEL`）時，Pots 選清單的第一個。模型列舉不當作 entitlement 保證，以實際請求結果判斷。
- **Responses 相容層。** 請求一律 `store: false`、`stream: true`，完整必要歷史放入 `input`，不用 `previous_response_id`；指示放頂層 `instructions`，不送 system message item。所有 Pots 工具放進名為 `pots` 的 namespace，回送 `function_call` 時補上 `namespace`，工具結果以 `function_call_output` 回送。reasoning item 存成 `Item::Opaque`，有加密內容時拿掉 `id` 原樣回送。只有 `response.completed` 代表成功；`response.failed`、`response.incomplete` 或提前結束的串流都是失敗。
- **能力限制。** 不送 `temperature`、`top_p`、`max_output_tokens`、`max_tool_calls`、`metadata`、`truncation`、`user`、`background`、`conversation`、`prompt` 等不支援的參數（省略而非送 `null`）；會改變語意的要求須明確報錯或在配置時拒絕。不依賴 hosted MCP、file search、Code Interpreter 或 Responses `tool_search`；pots 本機工具與 MCP 的執行不等於這些 hosted tools。
- **實作。** provider 與 OAuth 都在 `pots-llm` 裡直接用 `reqwest` 實作，請求 body 與 SSE 解碼是純函式，可以單獨測試。驗收包含串流回答、工具往返、多輪歷史、取消、斷流、並行 refresh、重啟、撤銷與額度不足。
- **失敗與預算。** 錯誤碼對應到 `LlmError`：額度用完（`subscription_sharing_usage_limit_exceeded`）→ `UsageLimit`；不符資格、路由不支援或 scope 未授權 → `NotEntitled`；暫時無法使用 → `Unavailable`（保留憑證，稍後有限重試）；不支援的能力 → `Unsupported`（重送同一請求沒有用）；身分失效 → `NeedsLogin`。plan 錯誤只停止推論，絕不自動改走其他計費路徑；訂閱額度不等於 API 餘額或無限用量。額度不足時排隊或升級給人類，API fallback 須先有使用者授權與金額上限。

M0 已依上述約定實作，並對假的 OAuth 與 Responses 伺服器測試。尚未對正式服務驗證的有：實際登入流程、`include: ["reasoning.encrypted_content"]` 是否被接受（做成選項，預設開啟）、`/models` 的實際回應格式，以及各模型對工具與 namespace 的實際行為。官方 preview 與資格可能變動，發佈前重查[官方來源](references.md#帳號與模型整合)。

### 模型 provider 設定

每個實例在設定頁管理自己的模型 provider，對話 agent 用「使用中」的那一個。provider 的非秘密設定存在 `model_connections`，使用中的 provider 與 ChatGPT 的模型存在 `instance_settings`；客戶端只看得到 API key 的末四碼。

| 種類 | 實作 | 端點 | 授權 |
| --- | --- | --- | --- |
| ChatGPT 方案（內建一筆） | `responses::ResponsesProvider` | `POST https://api.openai.com/v1/responses` | SIWC 登入，見上節 |
| OpenAI 相容 API | `openai_compat::OpenAiCompatProvider` | `POST {base_url}/chat/completions`，base URL 到 `/v1` 為止 | `Authorization: Bearer <key>`；key 可以留空，給 Ollama 這類本機服務 |
| Anthropic 相容 API | `anthropic::AnthropicProvider` | `POST {base_url}/v1/messages` | `x-api-key` 與 `anthropic-version: 2023-06-01`；key 必填 |

- **對應。** `instructions` 在 OpenAI 相容 API 是第一則 `system` 訊息，在 Anthropic 是頂層 `system`。同一輪的文字與工具呼叫合成一則 assistant 訊息；工具結果在 OpenAI 是 `tool` 角色，在 Anthropic 是 user 訊息裡的 `tool_result`，相鄰同角色的訊息合併。其他 provider 的 `Item::Opaque` 略過。Anthropic 請求必帶 `max_tokens`，預設 8192。
- **串流。** OpenAI 相容 API 以 `data: [DONE]` 或 `finish_reason` 結束；Anthropic 以 `message_stop` 結束。之前就斷掉的串流是 `Interrupted`。
- **錯誤。** 401／403 → `InvalidCredentials`（請你到設定檢查 key）；400／404／422 → `Unsupported`；429 → `Unavailable`，帳單或額度不足另外標示；5xx 與 Anthropic 的 529 → `Unavailable`。錯誤訊息帶服務回的說明，不帶 key。
- **測試連線。** 設定頁的「測試連線」呼叫 `GET {base_url}/models`（Anthropic 是 `/v1/models`，會跟著分頁），回傳可用的模型讓你挑；不會儲存。
- **秘密。** API key 存在 `data_dir/auth/providers/<id>.json`（`0600`、原子寫入），刪除 provider 時一起刪掉；不進事件日誌、快照、日誌或錯誤訊息。
- **沒有自動 fallback。** 使用中的 provider 失敗就失敗，不會改用其他 provider；換 provider 一律由你在設定頁切換。

### Worker adapter：內建 worker 與外部 coding CLI

Worker 有兩種後端，由 project 設定選擇；執行位置（Pots 主機或某台 node）另由 `ExecTarget` 決定，見[託管電腦](computer-nodes.md#coding-任務)。Claude Code adapter 執行官方 coding agent，內建 worker 用於備援與輕量任務。模型 provider 與 worker backend 分開配置；選擇 ChatGPT Plan 不會啟動 Codex，選擇 Claude Code 也不會把它變成 Pots 的 `ModelProvider`。

```rust
pub enum WorkerBackend {
    Builtin(BuiltinWorkerConfig),    // pots-llm 的 loop，在行程內執行
    ClaudeCode { image: String },    // 在容器內以非互動模式執行
}
pub enum WorkerEvent {
    Progress(String),
    ToolCall { name: String },
    Suspended(WaitCond),
    Finished(Outcome),
    Failed(String),
}
/// 把 Claude Code 的串流 JSON 輸出轉成統一事件；不是完整生命週期介面
pub trait OutputParser {
    fn parse_line(&mut self, line: &str) -> Option<WorkerEvent>;
}
```

- 控制面走 MCP。 平台以 `rmcp` 提供一個 MCP server，CLI 透過它呼叫 `wait_for` 、 `report_progress` 、`complete_task` 。這也是之後「把整個平台包成 MCP server」的第一個使用者。

- 執行。 每個任務一個 Docker/Podman 容器，掛載該任務的 worktree，執行未修改的官方 binary，例如 `claude -p <prompt> --output-format stream-json --verbose`。由 `OutputParser` 轉成 `WorkerEvent`，adapter 另管啟動、中斷、逾時、退出碼與最終結果。釘選 CLI 版本並驗證事件格式；訂閱登入不使用不讀 OAuth／keychain 的 `--bare` 模式。

- 等待 CI。 不讓 CLI 行程掛著等幾十分鐘。CLI 呼叫 `wait_for` 後，由平台持久化等待條件並受控結束本輪；保存 session ID、官方 session 資料、worktree、必要設定與 run 對應，事件到了再續接。不能只保存 ID 就刪掉容器內歷史。中斷與重試須核對已完成操作，避免重複副作用。

- 成本與用量。 區分訂閱額度、API 計費與可能的額外付費用量，不把 CLI 的成本估計當帳單。訂閱 worker 以執行時間、次數與並行數限制預算；額度不足時排隊或交給人類，只有預先授權的 fallback 才可改派產生 API 費用的內建 worker。

- 登入與憑證。 使用者在原版 Claude Code 走 Anthropic 官方登入；CLI 自行管理登入狀態，pots 不提供 Claude token 匯入、不代收／解析／轉送 Claude.ai 憑證，也不將它們供內建 agent 使用。CLI 的登入與 session 儲存由使用者專屬執行環境持久化，不採「所有憑證唯讀掛載」的通用方案。容器登入、refresh 與重建續接須做原型驗證。

- 條款邊界。 依官方規範執行未修改的 Claude Code，不限制其內建驗證方法；使用者以自己的帳號或 API key 登入，pots 不代付、轉售或中介其用量。平台承載 Claude Code 須遵守適用商業條款；個人訂閱不當作無限制的自動化服務帳號，正式使用前重查[官方規範](references.md#帳號與模型整合)。

- 權限。 CLI 權限設定、容器檔案與網路隔離、MCP 伺服器端授權共同限制能力；不把提示詞或 CLI allowlist 當唯一沙箱。merge、部署等動作一律走平台的核准流程，CLI 拿不到對應權限。非互動模式可能載入 repo 的 hooks／MCP，啟動前須檢查設定，避免未信任的專案內容取得額外能力。

### 為什麼不用 rig

早期設計以 rig 為 agent 框架，M0 實作時改為自有的 `pots-llm`：

- rig-core 0.46 重寫成 Model／Wire／Transport，agent runtime 拆到 rig-agent，原本的 builder、`ConversationMemory`、`PromptHook` 寫法都不再適用。
- SIWC 對 Responses 請求的限制多（`store: false`、`stream: true`、不送 system item、工具要放 namespace、許多參數會被拒絕），直接組請求比較容易保證。評估時也發現 rig 從正規化的 tool call 重建 `function_call` 時會丟掉 `namespace`。

provider 仍是可插拔的：其他 API provider 實作 `ModelProvider` 即可接上同一個 loop。之後 rig 穩定、或需要大量 provider 時可以重新評估，接法是寫一個包住 rig 的 `ModelProvider`，不影響 loop 與工具。

