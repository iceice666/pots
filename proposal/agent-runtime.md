# Agent 執行層

[← 產品提案索引](product.md)

## Agent 執行層（rig）

對話 agent、Planner 與內建 worker 用 rig 實作；ChatGPT Plan provider 提供模型推論，Claude Code adapter 接入外部 coding agent runtime。這是兩種不同層級的整合，不引入 Codex runtime，也不把 Claude 訂閱 token 接成 rig provider。平台能力以 rig `Tool` 暴露給內建 agent，以 MCP 暴露給 Claude Code；watcher 是確定性的 Rust 程式碼，不是 LLM agent。

### 角色

| 角色 | 實作 | 觸發 | 並行度 |
| --- | --- | --- | --- |
| 對話 agent | rig agent，掛 OM | 人類訊息、里程碑、升級事件 | 每個 project 同時 1 個 run |
| Planner | rig agent，不掛 OM；context 只有 Goal brief 與任務樹 | Goal 建立、子任務結束、steer | 每個 Goal 同時 1 個 run |
| Worker | rig agent，或 Claude Code adapter | 被分派、被喚醒 | 多個，每個任務一個 worktree 與容器 |
| Watcher | 純 Rust（webhook 或輪詢） | 外部事件 | 不限 |
| 總管 agent | rig agent，掛自己的 OM（全域對話） | 人類在總管對話中的訊息、自己的排程 | 全域同時 1 個 run |

### 對話 agent 與 Planner 的分工

拆成兩個 agent，讓對話永遠能即時回應：長時間的規劃與重新規劃都在 Planner 的 run 裡進行，不會卡住你和對話 agent 的討論。

- 交接靠 Goal brief。 討論收斂時，對話 agent 呼叫 `create_goal` ，把目標、約束、已決定事項寫成一份結構化 brief。Planner 只讀 brief 與任務樹，不讀對話。

- 改方向。 你在對話中改變主意時，對話 agent 呼叫 `steer_goal` 更新 brief 並發出 steer 事件，Planner 被喚醒後重新規劃。

- 回報。 Planner 的里程碑與升級事件以卡片形式回到對話流，對話 agent 可以在下一輪看到並轉述。

- 好處。 Planner 的 context 小而穩定，容易快取；brief 本身也是你可以檢視、修改的「這個 Goal 到底要做什麼」。

### 總管 agent：跨 project 的入口

總管 agent 是所有 project 之上的一條永久對話，用來問「整體狀況」和派工到某個 project，本身不執行工作。

- 入口。 桌面版在任何頁面用快捷鍵或浮動按鈕叫出，手機版是底部的「總管」分頁。

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

### 工具實作範例

```rust
use rig_core::{completion::ToolDefinition, tool::Tool};
use schemars::JsonSchema;
use serde::Deserialize;
#[derive(Deserialize, JsonSchema)]
pub struct WaitArgs {
    task_id: TaskId,
    on: WaitCond,
    timeout_minutes: u32,
}
pub struct WaitFor { store: TaskStore }
impl Tool for WaitFor {
    const NAME: &'static str = "wait_for";
    type Error = PlatformError;
    type Args = WaitArgs;
    type Output = String;
    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.into(),
            description: "掛起任務直到條件成立（CI 結束、PR 被 review、子任務完成）。 \
                          呼叫後本輪即結束，等待期間不消耗 token。".into(),
            parameters: serde_json::to_value(schemars::schema_for!(WaitArgs))
                .expect("schema"),
        }
    }
    async fn call(&self, a: WaitArgs) -> Result<String, PlatformError> {
        let deadline = now() + minutes(a.timeout_minutes);
        self.store.suspend(a.task_id, a.on, deadline).await?;
        Ok("suspended".into())
    }
}
```

### 組裝對話 agent 與 Planner

下例只示意 rig 的角色與工具組裝，使用 Anthropic API client 不代表以 Claude 訂閱推論。正式組裝由 `llm` 模組選擇 ChatGPT Plan provider 或使用者配置的 API provider；ChatGPT Plan 的相容性要求見下節。

```rust
use rig_core::{client::{CompletionClient, ProviderClient},completion::Prompt, providers::anthropic};
let client = anthropic::Client::from_env()?;
// 對話 agent：掛 OM，負責和人類討論、建立與調整 Goal
let chat_agent = client
    .agent(&cfg.chat_model)
    .preamble(&compose_preamble(&user_md, &project.instructions))
    .tool(GetStatus::new(store.clone()))
    .tool(CreateGoal::new(store.clone()))
    .tool(SteerGoal::new(store.clone()))
    .tool(Recall::new(db.clone()))
    .memory(om.clone())              // 見 memory.md 的 Observational Memory 章節
    .default_max_turns(8)
    .build();
let reply = chat_agent
    .prompt(human_text)
    .conversation(project.id.to_string())
    .await?;
// Planner：不掛 OM，每次 run 都從 Goal brief 與任務樹重建 context
let planner = client
    .agent(&cfg.planner_model)
    .preamble(&compose_preamble(&user_md, PLANNER_INSTRUCTIONS))
    .tool(GetStatus::new(store.clone()))
    .tool(CreateTask::new(store.clone()))
    .tool(CancelTask::new(store.clone()))
    .tool(RequestApproval::new(store.clone()))
    .tool(PostMilestone::new(store.clone()))
    .default_max_turns(12)
    .build();
```

### 執行模型

1. 每一次 agent 執行是 `agent_runs` 的一列。Runner pool 用 `FOR UPDATE SKIP LOCKED` 取工作，持有 lease 並定期心跳。

2. lease 過期（程序當掉）時，任務回到 `Planned` 重新分派，`attempts` 加一。

3. `wait_for` 之後必須結束本輪：用 rig 的 `PromptHook` 在工具呼叫後中止多輪迴圈，而不是靠 prompt 要求模型自己停。

4. 喚醒時建立新的 run，帶入「你在等什麼、結果是什麼」作為觸發訊息。

### ChatGPT Plan provider：rig 的模型推論後端

pots 採開源、本機或使用者自行架設的模式。透過官方 **Sign in with ChatGPT（SIWC）** 取得 ChatGPT plan usage 授權，直接呼叫 Responses API；不讀取 Codex 的 `auth.json`、不使用 ChatGPT 網頁 cookie，也不依賴 Codex CLI 或 app-server。這不授予存取使用者既有 ChatGPT 對話的權限。收費或平台代管服務不在本次範圍，若改變產品模式需重新確認資格。

- **責任邊界。** Provider 只將 rig 請求映射為模型請求，把 SSE 轉成文字、工具呼叫、usage 與完成／錯誤事件；工具執行、多輪 loop、OM 與任務狀態仍由 pots 管理。對話 agent、Planner、總管、Observer／Reflector 與 rig worker 可依角色選用此 provider，但須驗證模型與能力，不假設所有角色皆可直接替換。
- **授權。** 使用系統瀏覽器與官方 OAuth／PKCE 流程。先保存穩定的 `ext_agent_host_id`；首次以動態註冊取得 issued `client_id`，往後沿用該帳號／workspace 註冊。每次登入驗證 `state`、OIDC `nonce`、ID token 簽章／issuer／audience／期限，以及實際授予的 `chatgpt.tokens.use.direct` scope；登入成功不等於取得訂閱推論權限。
- **憑證。** 帳號授權、憑證儲存／refresh、推論 adapter 分層。每個註冊的 refresh 由單一 owner 協調，原子替換 access token、輪替後的 refresh token、期限與 scopes；多個 run 不各自更新同一份憑證。秘密不進 prompt、資料匯出或 trace，部署方式見[部署](deployment.md)。
- **Responses 相容層。** 目前 preview 的 HTTP 請求須 `store: false`、`stream: true`，完整必要歷史放入 `input` 陣列，不依賴 `previous_response_id`；使用 `instructions` 或 developer message，不送明確的 system message item。依官方限制處理 function/custom tool 的 namespace 或 `additional_tools`，工具結果由 pots 回送。
- **能力限制。** 不照搬一般 API provider 的參數；目前 `temperature`、`max_output_tokens` 等不支援，會改變語意的要求須明確報錯或在配置時拒絕。不依賴 hosted MCP、file search、Code Interpreter 或 Responses `tool_search`；pots 本機工具與 MCP 的執行不等於這些 hosted tools。模型列舉不當作 entitlement 保證，以實際請求結果判斷。
- **實作順序。** 先檢查釘選 rig 版本的 request、SSE 與 tool call 介面；能重用則重用，否則在 `llm` 模組實作專用 provider，不只是替既有 API client 換 token。驗收包含串流回答、工具往返、多輪歷史、取消、斷流、並行 refresh、重啟、撤銷與額度不足。
- **失敗與預算。** 區分需要登入、授權撤銷、額度不足、暫時限流、模型／workspace 不允許；訂閱額度不等於 API 餘額或無限用量。額度不足時排隊或升級給人類，API fallback 須先有使用者授權與金額上限。

上述為設計與待驗證項目，非已完成的帳號測試。官方 preview 與資格可能變動，實作時重查[官方來源](references.md#帳號與模型整合)。

### Worker adapter：內建 rig 與外部 coding CLI

Worker 有兩種後端，由 project 設定選擇：Claude Code adapter 執行官方 coding agent，rig worker 用於備援與輕量任務。模型 provider 與 worker backend 分開配置；選擇 ChatGPT Plan 不會啟動 Codex，選擇 Claude Code 也不會把它變成 rig 的 completion provider。

```rust
pub enum WorkerBackend {
    Rig(RigWorkerConfig),            // 在行程內執行
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

- 成本與用量。 區分訂閱額度、API 計費與可能的額外付費用量，不把 CLI 的成本估計當帳單。訂閱 worker 以執行時間、次數與並行數限制預算；額度不足時排隊或交給人類，只有預先授權的 fallback 才可改派產生 API 費用的 rig worker。

- 登入與憑證。 使用者在原版 Claude Code 走 Anthropic 官方登入；CLI 自行管理登入狀態，pots 不提供 Claude token 匯入、不代收／解析／轉送 Claude.ai 憑證，也不將它們供 rig 使用。CLI 的登入與 session 儲存由使用者專屬執行環境持久化，不採「所有憑證唯讀掛載」的通用方案。容器登入、refresh 與重建續接須做原型驗證。

- 條款邊界。 依官方規範執行未修改的 Claude Code，不限制其內建驗證方法；使用者以自己的帳號或 API key 登入，pots 不代付、轉售或中介其用量。平台承載 Claude Code 須遵守適用商業條款；個人訂閱不當作無限制的自動化服務帳號，正式使用前重查[官方規範](references.md#帳號與模型整合)。

- 權限。 CLI 權限設定、容器檔案與網路隔離、MCP 伺服器端授權共同限制能力；不把提示詞或 CLI allowlist 當唯一沙箱。merge、部署等動作一律走平台的核准流程，CLI 拿不到對應權限。非互動模式可能載入 repo 的 hooks／MCP，啟動前須檢查設定，避免未信任的專案內容取得額外能力。

### rig 版本注意

rig-core 仍在 1.0 之前，撰寫時最新為 0.45；近期版本的 import 路徑是 `rig_core::` ，較舊的範例寫 `rig::` 。API 變動頻繁，建議釘死精確版本，並把 rig 包在自己的 `llm` 模組後面，升級時只改一處。上面範例的方法名稱（`memory` 、`conversation` 、`default_max_turns` ）以釘

選版本的文件為準。

