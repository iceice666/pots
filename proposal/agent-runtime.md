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

M6a 的實作：總管是 `kind = 'overseer'` 的內部 project（固定 ID，`Store::ensure_overseer` 在啟動時建立），共用同一套 `stream_items`、`om_raw`／`om_observations` 與 `agent_runs`，但 `list_projects` 與快照只帶 `kind = 'project'`，所以它不會出現在側欄。`profiles/global.toml` 是 `Om` 記憶的 `global` profile；`Scope::Global` 讓唯讀工具看得到所有 project，`create_goal` 多一個 `project` 參數（名稱或 ID），`cancel_goal` 可以取消任一 Goal，`pause_automations`／`resume_automations` 不給 project 就是全部，`schedule` 建 `ScheduleOwner::Global` 的排程（觸發時在同一條總管對話醒來，`post_system_trigger` 走 `StreamScope::Overseer`）。桌面端的訊息用 `ClientCommand::SendOverseer`／`StopOverseer`，事件用 `StreamScope::Overseer`，快照的 `overseer` 帶對話、各 project 概況與總管自己的記憶。`archive_goal` 在 M6b 補上（只能封存已結束的 Goal，總管可以封存任一 project 的 Goal）；跨實例總管與總管的核准代決還沒有。

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

M1 起對話 agent 的歷史由 OM 組成：記憶區塊加上 `om_raw` 裡還沒觀察的尾巴，見[Observational Memory](memory.md)。

### 執行模型

1. 每一次 agent 執行是 `agent_runs` 的一列。Runner pool 用 `FOR UPDATE SKIP LOCKED` 取工作，持有 lease 並定期心跳。

2. lease 過期（程序當掉）時，任務回到 `Planned` 重新分派，`attempts` 加一。

3. `wait_for` 之後必須結束本輪：pots-llm 的 `Hook`（Gate）對它回 `ToolDecision::AllowThenStop`，loop 執行工具後即以 `RunOutcome::Suspended` 結束，而不是靠 prompt 要求模型自己停。

4. 喚醒時建立新的 run，帶入「你在等什麼、結果是什麼」作為觸發訊息。

M2 先把 Planner 的 run 改走持久化佇列（`crates/pots-server/src/planner.rs`）：觸發和改狀態寫在同一個 transaction，同一個 Goal 已有排隊中的 run 時把觸發併進去；每個 Goal 同時最多一個 `running`，整個實例最多並行 4 個。SQLite 沒有 `SKIP LOCKED`，由單一 runner 在 `BEGIN IMMEDIATE` 的 transaction 裡挑選並標記。steer 中斷這個 Goal 正在跑的 run；服務關閉時被中斷的 run 回到 `queued`（不算一次重試），當掉留下的 `running` 在下次啟動時放回佇列，取走超過 3 次就標成失敗。對話 agent 仍由記憶體裡的 lane 排程（每個 project 一個），只在開始時寫一列 `running`。

Planner 的里程碑不寫進對話 agent 的 `om_raw`：寫成使用者訊息會讓 Observer 誤認成使用者說的話，寫在 run 進行中也會拆散工具呼叫與結果。改成對話 agent 每次 run 的 instructions 帶著「進行中的 Goal」與「最近 5 張里程碑」，它在下一輪就看得到並轉述；細節仍以 `get_status` 為準。

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
- **推論端點。** `POST https://api.openai.com/v1/responses`，只帶 `Authorization: Bearer <access token>`；是公開的 Responses API，不是 ChatGPT 的 `backend-api`。`GET /v1/models` 回傳 `models` 陣列（不是一般 API 的 `data`），取 `visibility == "list"` 的項目、保留伺服器順序；設定視窗沒有指定模型（也沒有 `POTS_CHAT_MODEL`）時，Pots 選清單的第一個。模型列舉不當作 entitlement 保證，以實際請求結果判斷。
- **Responses 相容層。** 請求一律 `store: false`、`stream: true`，完整必要歷史放入 `input`，不用 `previous_response_id`；指示放頂層 `instructions`，不送 system message item。所有 Pots 工具放進名為 `pots` 的 namespace，回送 `function_call` 時補上 `namespace`，工具結果以 `function_call_output` 回送。reasoning item 存成 `Item::Opaque`，有加密內容時拿掉 `id` 原樣回送。只有 `response.completed` 代表成功；`response.failed`、`response.incomplete` 或提前結束的串流都是失敗。
- **能力限制。** 不送 `temperature`、`top_p`、`max_output_tokens`、`max_tool_calls`、`metadata`、`truncation`、`user`、`background`、`conversation`、`prompt` 等不支援的參數（省略而非送 `null`）；會改變語意的要求須明確報錯或在配置時拒絕。不依賴 hosted MCP、file search、Code Interpreter 或 Responses `tool_search`；pots 本機工具與 MCP 的執行不等於這些 hosted tools。
- **實作。** provider 與 OAuth 都在 `pots-llm` 裡直接用 `reqwest` 實作，請求 body 與 SSE 解碼是純函式，可以單獨測試。驗收包含串流回答、工具往返、多輪歷史、取消、斷流、並行 refresh、重啟、撤銷與額度不足。
- **失敗與預算。** 錯誤碼對應到 `LlmError`：額度用完（`subscription_sharing_usage_limit_exceeded`）→ `UsageLimit`；不符資格、路由不支援或 scope 未授權 → `NotEntitled`；暫時無法使用 → `Unavailable`（保留憑證，稍後有限重試）；不支援的能力 → `Unsupported`（重送同一請求沒有用）；身分失效 → `NeedsLogin`。plan 錯誤只停止推論，絕不自動改走其他計費路徑；訂閱額度不等於 API 餘額或無限用量。額度不足時排隊或升級給人類，API fallback 須先有使用者授權與金額上限。

M0 已依上述約定實作，並對假的 OAuth 與 Responses 伺服器測試。尚未對正式服務驗證的有：實際登入流程、`include: ["reasoning.encrypted_content"]` 是否被接受（做成選項，預設開啟）、`/models` 的實際回應格式，以及各模型對工具與 namespace 的實際行為。官方 preview 與資格可能變動，發佈前重查[官方來源](references.md#帳號與模型整合)。

### 模型 provider 設定

每個實例在設定視窗管理自己的模型 provider，對話 agent 用「使用中」的那一個。provider 的非秘密設定存在 `model_connections`，使用中的 provider 與 ChatGPT 的模型存在 `instance_settings`；客戶端只看得到 API key 的末四碼。整理記憶（Observer／Reflector）可以在「記憶」類別另選 provider 與模型，沒設定時跟對話 agent 相同（[Observational Memory](memory.md#整理記憶的模型)）。

| 種類 | 實作 | 端點 | 授權 |
| --- | --- | --- | --- |
| ChatGPT 方案（內建一筆） | `responses::ResponsesProvider` | `POST https://api.openai.com/v1/responses` | SIWC 登入，見上節 |
| OpenAI 相容 API | `openai_compat::OpenAiCompatProvider` | `POST {base_url}/chat/completions`，base URL 到 `/v1` 為止 | `Authorization: Bearer <key>`；key 可以留空，給 Ollama 這類本機服務 |
| OpenAI Responses API | `responses::ResponsesProvider::with_api_key` | `POST {base_url}/responses`，base URL 到 `/v1` 為止 | `Authorization: Bearer <key>`；key 必填 |
| Anthropic 相容 API | `anthropic::AnthropicProvider` | `POST {base_url}/v1/messages` | `x-api-key` 與 `anthropic-version: 2023-06-01`；key 必填 |
| OpenCode Zen／Go（內建目錄） | `preset_provider::PresetProvider` 依模型轉給上面三種實作 | Anthropic 模型是 `{base_url}/v1/messages`，OpenAI 模型是 `{base_url}/v1/chat/completions` 或 `/responses` | `x-api-key` 或 `Authorization: Bearer`（看模型的 API），加上 `x-opencode-client`；`x-opencode-session` 由 Pots 每次對話自動帶 |

- **對應。** `instructions` 在 OpenAI 相容 API 是第一則 `system` 訊息，在 Responses 與 Anthropic 是頂層 `instructions`／`system`（ChatGPT plan 的工具放 `pots` namespace；OpenAI 平台 API key 的工具平整地放在 `tools`，`function_call` 不帶 `namespace`）。同一輪的文字與工具呼叫合成一則 assistant 訊息；工具結果在 OpenAI 是 `tool` 角色，在 Anthropic 是 user 訊息裡的 `tool_result`，相鄰同角色的訊息合併。其他 provider 的 `Item::Opaque` 略過。Anthropic 請求必帶 `max_tokens`，預設 8192。
- **串流。** OpenAI 相容 API 以 `data: [DONE]` 或 `finish_reason` 結束；Anthropic 以 `message_stop` 結束。之前就斷掉的串流是 `Interrupted`。
- **工具與推理。** Responses API 允許工具與推理並用，所以 GPT-5.4 以後的模型優先用「OpenAI Responses API」；推理不另送 `reasoning`，用模型預設值。GPT-5.4 以後的模型在 Chat Completions 預設會推理，但不接受「工具 + reasoning」，會回 400 `Function tools with reasoning_effort are not supported`。請求帶工具且被這樣拒絕時，改成 `reasoning_effort: "none"` 重送一次並記住（之後帶工具的請求直接關掉推理）；模型連 `none` 都不接受時回 `Unsupported`，提示改用 Responses API 或換模型。
- **錯誤。** 401／403 → `InvalidCredentials`（請你到設定檢查 key）；400／404／422 → `Unsupported`；429 → `Unavailable`，帳單或額度不足另外標示；5xx 與 Anthropic 的 529 → `Unavailable`。錯誤訊息帶服務回的說明，不帶 key。
- **測試連線。** 設定視窗的「測試連線」呼叫 `GET {base_url}/models`（Anthropic 是 `/v1/models`，會跟著分頁），回傳可用的模型讓你挑；不會儲存。
- **秘密。** API key 存在 `data_dir/auth/providers/<id>.json`（`0600`、原子寫入），刪除 provider 時一起刪掉；不進事件日誌、快照、日誌或錯誤訊息。
- **沒有自動 fallback。** 使用中的 provider 失敗就失敗，不會改用其他 provider；換 provider 一律由你在設定視窗切換。

#### OpenCode 與額外 header

pi-ai 的內建 provider 有些不是單純的 OpenAI／Anthropic 端點：OpenCode Zen／Go 的同一個 base URL 下，Claude 走 Anthropic Messages、GPT 走 Responses、其他模型走 Chat Completions，而且要求 `x-opencode-session` 這類請求 header。Pots 的做法：

- **內建目錄。** `crates/pots-proto/data/provider-presets.json` 是從 pi-ai 1.4.0 的模型資料產生，只留 Pots 支援的三種 chat API。`ProviderKind::OpenCodeZen`／`OpenCodeGo` 用 `ProviderKind::preset()` 找目錄；`PresetProvider` 依 `ModelRequest.model` 的 `api` 選實作，並把 base URL 補成該 API 的路徑（Anthropic 用目錄根目錄，OpenAI 補 `/v1`）。不在目錄裡的模型在儲存與推論時都回 `Unsupported`，避免用錯 API。
- **模型清單。** 這兩種 provider 的「列出模型」直接回內建目錄，不連網、也不驗證 key；真正的 key 驗證發生在第一次推論。要更新目錄時，用 pi-ai 的 `opencode.json`／`opencode-go.json` 重跑產生步驟（見 `crates/pots-proto/data/README.md`），只留 `anthropic-messages`、`openai-completions`、`openai-responses`。
- **額外 header。** 所有 API provider 都能在設定填 header 名稱／值，存在 `model_connections.headers`（JSON，不是 `0600` 秘密檔）。值會出現在快照與設定畫面，**不要放 API key**；驗證會拒絕重複名稱、控制字元與 `host`／`content-length` 這類 hop-by-hop header。`Debug` 只印名稱。它們排在 provider 的認證 header 之後，所以可以覆寫預設值（例如自訂的 `Authorization`）。
- **Session header。** `ModelRequest.session_id` 由 run 帶入：對話是 `project:<id>`、Planner 是 `goal:<id>`、內建 worker 是 `task:<id>`、subagent 是 `run:<id>`。`PresetProvider` 把內建實作設成 `with_session_header("x-opencode-session")`，同一個對話跨輪次會帶同一個值；使用者自己設了同名 header 時以使用者的為準。Session ID 不存進資料庫。
- **預設 header。** 新增 OpenCode provider 時表單會預填 `x-opencode-client: pots`，使用者可以改或刪。其他 pi-ai provider 之後可以只加 preset 資料與按鈕，不需要再改推論層。
- **限制。** OpenCode 目錄裡的 Google 模型（`google-generative-ai`）與 classifier 不在 Pots 的支援範圍；OpenCode Zen／Go 的實際 key 權限、模型路由與 header 行為仍要對正式服務手動驗證。成本報表把 Zen 記成 `opencode`、Go 記成 `opencode-go`，models.dev 沒有的話使用者可在單價表覆寫。

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

### M4 的實作

M4 依上面的設計實作在 `crates/pots-server/src/worker/`（加上 `0006_worker.sql`、`team.rs`、`profiles/worker.toml`、`explorer.toml`、`reviewer.toml`），協定 v6。Claude Code 的接法參考 OpenClaw（`extensions/anthropic/cli-runtime-args.ts`、`cli-transport.ts`）、Claude Agent SDK 與 vibe-kanban 的實作：都是直接跑官方 CLI 的雙向 stream-json，沒有走 ACP。和你確認過的範圍與取捨：

- **派工與佇列。** Planner 的 `create_task` 與對話 agent 的 `quick_fix` 在同一個 transaction 裡排一個 `worker` 的 `agent_runs`；喚醒（`wait_for` 成立或逾時）、team 的交接也排在同一個佇列，同一個任務排隊中的觸發會合併。每個任務同時最多一個 run，整個實例最多「同時執行的 worker 數」個（預設 2）。取走時任務變成 `Running`；worker 以 `complete_task`、`fail_task`、`escalate` 或 `wait_for` 結束一輪，沒有呼叫就停了（或出錯、超過一小時的執行上限）時任務轉成 `NeedsHuman`。任務結束（完成、失敗、需要決定）時叫醒 Planner。取消任務、Goal 結束會中斷正在跑的 run；服務關閉時被中斷的 run 放回佇列（不算一次重試），當機留下的在啟動時放回，取走超過 3 次就轉 `NeedsHuman`。`NeedsHuman` 與失敗的任務你可以按「重試」（`RetryTask`）放回佇列，worker 從工作目錄的現況接著做。project 還沒有設定 repo 時，任務只記在任務樹上、不派工。
- **Repo 與工作目錄。** repo 來源抽成 `RepoSource`（之後接 GitLab 與沒有 remote 的本機 repo），M4 只有 GitHub。平台用 M3 的 GitHub token 在資料目錄維護 bare 鏡像（`repos/`），每個任務一個獨立的 git repo（`worktrees/<任務>`，分支 `pots/<標題>-<任務 ID 後 8 碼>`）。不用 `git worktree`：它的 `.git` 指向鏡像的絕對路徑，掛進容器就失效，容器也能改到其他任務的 ref。工作目錄不信任：平台在主機上完全不對它執行 git——hooks、`core.fsmonitor` 之外，alternates、指向別處的 gitfile 與符號連結的 ref 都會讓主機上的 git 讀到其他 repo 的物件，連 `git fetch file://` 也不行。要拿 worker 的 commit 時，由執行環境裡的 git 打一個只含基準分支之後 commit 的 bundle，寫到這次 run 的輸出目錄，平台以不跟隨符號連結的方式讀出、複製到自己的目錄，在鏡像裡以 `transfer.fsckObjects` fetch；比較與 push 都在鏡像裡做。平台的 git 清空環境變數、隔離使用者的全域與系統設定；token 只以環境變數帶給平台自己的 git，不寫進設定、不進參數列，也不交給容器。M9 把「工作資料夾」從監看的 repo 分出來：`WorkDir` 每個 project 最多一個，GitHub 或本機 git repo；本機專案平台一樣維護自己的鏡像（`repos/local/<sha256 前 16 bytes>.git`）與任務分支，`open_pr` 只記下變更，核准後才以 `merge --ff-only` 套用回你的資料夾，worker 不會直接改你的即時檔案。對話 agent 多了 `set_workdir`，和人類設定走同一份驗證；對話、Planner 與 worker 的 instructions 都會說明實際工作位置，Planner 的 `create_task` 舊 `repo` 參數只能是同一個資料夾。
- **開 PR 與 CI。** `open_pr` 先在執行環境裡 commit 還沒 commit 的修改，取回任務分支、檢查快速修改的範圍（最多 5 個檔案、200 行），由平台 push（只會強制更新自己的 `pots/` 分支）並用 GitHub API 開 PR（同一個分支已經有 PR 就沿用），回傳完整的 sha。改到 `.github/` 底下的（workflow、composite action）不推：推上去就會以 repo 的秘密執行 worker 寫的東西，交給你決定；這只是盡力而為（CI 呼叫的腳本、build 檔也會被執行），建議 GitHub token 不要給 Workflows 權限，讓 GitHub 自己擋。任務在這期間被取消就不推。worker 接著用 `wait_for`（`ci_finished`）等 CI；這次不做自動修：CI 通過就 `complete_task`，失敗就 `fail_task` 交回 Planner，由 Planner 決定重試或回報你。merge 一律由你在 GitHub 上手動做（M5 之前；M5 起 CI 失敗由 worker 自己修、通過後申請核准、由平台 merge，見[核准的實作](approvals-and-notifications.md#m5-的實作)）。
- **執行環境。** 設定的「Worker」類別選容器或本機。容器：每個 worker run 一個 Docker／Podman 容器（repo 附 `docker/worker/Dockerfile`，釘選 Claude Code 的版本），只掛工作目錄（`/workspace`）、輸出目錄（`/run/pots-out`）與這次 run 的設定（`/run/pots`，唯讀）；Claude Code 後端另外掛共用的 Claude Code 設定目錄（資料目錄的 `claude-config`，掛在 `/claude-config` 當 `CLAUDE_CONFIG_DIR`：官方登入與 session）與任務自己的 home（`worker-homes/<任務>`，掛在 `/home/pots`，跨 run 保留）——home 一個任務一份，worker 寫進 `~/.gitconfig`、shell rc 的東西影響不到其他任務，內建 worker 讀不到 Claude 的登入，拿掉所有 capability、`no-new-privileges`、限制行程數、以你的 uid 執行，不掛 Docker socket，指令逾時在容器裡以 `timeout -s KILL` 執行，run 結束就移除（所以「重建容器後續接」是每次喚醒的常態）；工作目錄在容器裡固定是 `/workspace/<任務>`，所以依 cwd 分目錄的官方 session 換了容器也找得到，不同任務的 session 與自動記憶也不會在同一個目錄。平台自己在容器裡跑的 git（commit、打 bundle）不讀 home 的全域設定、不跑 hooks 與 fsmonitor。容器帶這次啟動的標籤，啟動時只清其他次啟動留下的。容器還可以連外網：預設拒絕外網的白名單延後。本機：直接在任務的工作目錄執行，沒有沙箱（指令只拿到少數環境變數，逾時或取消時殺掉整個行程群組），Claude Code 用你自己的登入與設定。容器連回平台用 `host.docker.internal`（Docker Desktop、OrbStack 內建），服務只綁 loopback，所以 Linux 主機上的容器模式要等多設備版。
- **內建 worker。** 用對話 agent 使用中的模型 provider；工具有程式碼的唯讀工具（`list_files`、`read_file`、`grep`、`git_log`、`git_diff`）、`edit_file`、`run`、`open_pr` 與平台工具。讀寫工作目錄的指令一律在執行環境裡跑。
- **Claude Code adapter。** 參數照 OpenClaw：`--print --input-format stream-json --output-format stream-json --verbose --permission-prompt-tool stdio --permission-mode default --setting-sources user --strict-mcp-config --mcp-config <檔>`，加上 `--append-system-prompt`（worker 規則）、`--allowedTools`（平台工具）、`--disallowedTools`（背景常駐與排程類）與 `--session-id=`／`--resume=`，不經 shell。容器裡共用的 home 不可信（之前的 run 可能寫進 hooks），所以改成 `--setting-sources=`（不讀任何 settings.json）加上平台產生的 `--settings`（`disableAllHooks`）；本機用 `--setting-sources=user`。prompt 從 stdin 送入；worker 規則之外另外說明 Claude Code 該用自己的 Read／Edit／Bash／Task，平台工具是 `mcp__pots__*`。原生工具的權限經 `can_use_tool` 回到平台：容器就是沙箱，一律放行，只擋 `git push` 與背景執行的 Bash（擋 push 是盡力而為，真正的保護是容器裡沒有憑證）。平台工具（worker profile 去掉 CLI 自己就有的改檔、執行、subagent）經 `rmcp` 的 streamable HTTP MCP server（`/mcp`）提供：每個 run 一個只在這次有效的 capability token，工具清單與範圍由平台決定；不經 gateway 的本機 token 與 `Host` 檢查，改有自己的 Host 白名單（loopback 與 `host.docker.internal`）與 capability，帶 `Origin` 的一律拒絕。`wait_for` 只能等 project 監看的 repo。結束工具成功時平台送 `interrupt`，等 CLI 收尾，逾時才 SIGTERM／SIGKILL 整個行程群組（容器是移除容器）；一輪結束卻沒有呼叫結束工具時提醒一次。登入失效、額度用完、CLI 錯誤依結構化欄位分類（不只看英文字串），任務轉 `NeedsHuman` 並說明要怎麼做。
- **續接。** session ID 在啟動 CLI 之前寫進 `worker_sessions`（連同官方 session 所在的 home 與工作目錄）。被喚醒、服務重啟後用 `--resume` 接著做；容器模式先確認專屬 home 裡的 session 檔還在，CLI 回報找不到 session 時（error result 或 stderr，比對 CLI 的英文訊息）開新的、帶完整 context。
- **登入。** Pots 不讀、不轉存 Claude 憑證。設定的「Worker」類別給一行指令（容器：掛專屬 home 跑 image 裡的 `claude`），你在終端機跑、在 Claude Code 裡 `/login` 完成官方登入。
- **快速通道。** 對話 agent 的 `quick_fix`（與 listener 的「快速修正」）開一個 `quick` 種類的 Goal（Brief 是描述）加一個任務，直接派給 worker、不排 Planner。任務完成或失敗時 Goal 跟著結束，結果以里程碑卡片貼回對話；你取消它的任務時 Goal 一起取消。worker 用 `escalate` 時升級成一般 Goal（Brief 補上「升級原因」——以引用區塊標明是 worker 的說法——與分支）並叫醒 Planner。快速修改沒有 Planner：對話 agent 不能 steer 它（要改就取消再開），你改 Brief、外部事件都不排 Planner，也不訂閱資源，worker 也不能發問題給 Planner。
- **Subagent。** 對話 agent、Planner 與內建 worker 有 `run_subagent`：一次最多 4 個並行的 explorer 或 reviewer，只有唯讀工具，不能再開 subagent。worker 開的讀自己的工作目錄，其他的讀 repo 預設分支的鏡像；每個是一筆 `agent_runs`（`parent_run_id`），用量算進 Goal。
- **Team 貼文。** `post_to_team`／`read_team`（`team_posts`、`team_reads`）：worker 發 finding、question、blocker、handoff，Planner 發 decision、finding、handoff。沒有指定對象的 question 與 blocker 叫醒 Planner；handoff 與 Planner 指定任務的 decision 叫醒那個任務（等待中的回到佇列，worker 會被告知原本在等什麼；Planner 的回答也能叫醒用 escalate 交回、在等它決定的任務，但重新登入、重試用完這類要你處理的不會）。worker 遇到要決定的事就發 question 並 `escalate`，Planner 以 decision 回答時被叫醒接著做；每次 run 開始時觸發訊息帶上上次之後的新貼文；同一個 Goal 每小時最多 30 則。桌面版在 Goal 面板的 Team 分頁。
- **延後與已知限制。** CI 失敗自動修（最多 3 次）與「可以 merge」的核准卡片（M5 已實作）、桌面版的任務 thread（直接對 worker 說話、完整串流）、每個 project 各自選 worker 後端（目前是整個實例一份設定）、容器的對外網路白名單、訂閱額度的預算與 Claude Code 用量的記帳（M6）、清掉結束很久的工作目錄。同一個實例的 Claude Code worker 共用一個 Claude Code 設定目錄（登入與官方 session 檔），任務與 project 之間讀得到、也改得到彼此的 session 紀錄；改成每個 project 一份（並驗證登入與 refresh）要對真的 Claude Code 做原型。`--setting-sources=`（空值）的行為與 `CLAUDE_CONFIG_DIR` 下的登入檔位置也要對真的 CLI 確認。本機執行沒有沙箱，listener 觸發的快速修改也會在本機跑。team 貼文在 run 開始時就記成已讀（run 沒能開始時不會重送）。

以假的 GitHub API、本機 bare repo 當 remote、假的模型與假的 Claude Code CLI（`pots-fake-claude`：重播劇本、經 MCP 呼叫平台工具、寫官方格式的 session 檔）驗證（`tests/worker.rs`：一句話到 Goal、Planner 派工、內建 worker 改檔、在 team 發問、開 PR、等 CI、被喚醒完成、Planner 結束 Goal；Claude Code 的快速修改經 MCP 開 PR 與等待、push 被擋、喚醒後 `--resume` 同一個 session；取消時中斷 CLI、快速修改跟著取消；需要重新登入、登入後重試；服務重啟後續接；Planner 的決定叫醒 worker、改到 `.github` 的 PR 不推；快速修改升級；explorer 讀鏡像），加上 adapter 對假 CLI 的單元測試（中斷、提醒、不肯結束的行程、錯誤分類、過長的行）、MCP server 的授權測試、repo 的 bundle 取回（符號連結的 bundle 不讀）與本機執行環境的測試。對真的 Claude Code（登入、容器內的 session 續接與事件格式）、真的 GitHub 與 Docker 容器模式待使用者手動驗證。

### 為什麼不用 rig

早期設計以 rig 為 agent 框架，M0 實作時改為自有的 `pots-llm`：

- rig-core 0.46 重寫成 Model／Wire／Transport，agent runtime 拆到 rig-agent，原本的 builder、`ConversationMemory`、`PromptHook` 寫法都不再適用。
- SIWC 對 Responses 請求的限制多（`store: false`、`stream: true`、不送 system item、工具要放 namespace、許多參數會被拒絕），直接組請求比較容易保證。評估時也發現 rig 從正規化的 tool call 重建 `function_call` 時會丟掉 `namespace`。

provider 仍是可插拔的：其他 API provider 實作 `ModelProvider` 即可接上同一個 loop。之後 rig 穩定、或需要大量 provider 時可以重新評估，接法是寫一個包住 rig 的 `ModelProvider`，不影響 loop 與工具。

