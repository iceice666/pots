# Agent 執行層

[← 產品提案索引](product.md)

## Agent 執行層（rig）

對話 agent、Planner 與內建 worker 用 rig 實作，外部 coding CLI 透過 adapter 接入，平台能力一律以 rig `Tool` 暴露；watcher 是確定性的 Rust 程式碼，不是 LLM agent。

### 角色

| 角色 | 實作 | 觸發 | 並行度 |
| --- | --- | --- | --- |
| 對話 agent | rig agent，掛 OM | 人類訊息、里程碑、升級事件 | 每個 project 同時 1 個 run |
| Planner | rig agent，不掛 OM；context 只有 Goal brief 與任務樹 | Goal 建立、子任務結束、steer | 每個 Goal 同時 1 個 run |
| Worker | rig agent，或外部 coding CLI （Claude Code、Codex CLI） | 被分派、被喚醒 | 多個，每個任務一個 worktree 與容器 |
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

### Worker adapter：內建 rig 與外部 coding CLI

Worker 有兩種後端，由 project 設定選擇。外部 CLI（Claude Code、Codex CLI）能力較強，而且以訂閱登入時不按 token 另外計費；rig worker 作為備援與輕量任務使用。

```rust
pub enum WorkerBackend {
    Rig(RigWorkerConfig),            // 在行程內執行
    ClaudeCode { image: String },    // 在容器內以非互動模式執行
    CodexCli   { image: String },
}
pub enum WorkerEvent {
    Progress(String),
    ToolCall { name: String },
    Suspended(WaitCond),
    Finished(Outcome),
    Failed(String),
}
/// 把各 CLI 的串流 JSON 輸出轉成統一事件
pub trait OutputParser {
    fn parse_line(&mut self, line: &str) -> Option<WorkerEvent>;
}
```

- 控制面走 MCP。 平台以 `rmcp` 提供一個 MCP server，CLI 透過它呼叫 `wait_for` 、 `report_progress` 、`complete_task` 。這也是之後「把整個平台包成 MCP server」的第一個使用者。

- 執行。 每個任務一個 Docker/Podman 容器，掛載該任務的 worktree，CLI 以非互動模式執行並輸出串流 JSON，由 `OutputParser` 轉成 `WorkerEvent` 。具體旗標以各 CLI 當前版本的文件為準。

- 等待 CI。 不讓 CLI 行程掛著等幾十分鐘。CLI 呼叫 `wait_for` 後平台結束該次執行並記下 session id，事件到了再以續接 session 的方式啟動新一次執行。

- 成本與用量。 訂閱登入沒有逐 token 帳單，但有用量上限。這類 worker 的預算改以執行時間與次數計算；用量用盡時任務排隊等待，或依設定改派 rig worker。

- 憑證。 訂閱登入的憑證檔以唯讀方式掛進容器，不進任何 prompt。正式使用前確認該訂閱的條款允許這種自動化用法。

- 權限。 CLI 自身的權限設定只允許 worktree 內的檔案操作與白名單指令；merge、部署等動作一律走平台的核准流程，CLI 拿不到對應權限。

### rig 版本注意

rig-core 仍在 1.0 之前，撰寫時最新為 0.45；近期版本的 import 路徑是 `rig_core::` ，較舊的範例寫 `rig::` 。API 變動頻繁，建議釘死精確版本，並把 rig 包在自己的 `llm` 模組後面，升級時只改一處。上面範例的方法名稱（`memory` 、`conversation` 、`default_max_turns` ）以釘

選版本的文件為準。

