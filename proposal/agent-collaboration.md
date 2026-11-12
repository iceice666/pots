# Agent Loop 與協作模型

[← 產品提案索引](product.md)

## 統一 Agent Loop 與協作模型

所有 agent 共用同一個 loop。對話 agent、Planner、worker、總管與 subagent 的差別只在 profile：拿到哪些工具、用哪個模型、記憶怎麼接。唯讀工具每個 profile 都有。

### 一個 loop

每次執行都是同一個函式：組 context、跑 pots-llm 的多輪工具迴圈（`agent::run`）、由平台的 gate 在每次工具呼叫前後把關、寫回結果。

```rust
pub struct AgentProfile {
    pub name: ProfileName,          // chat | planner | worker | operator | global | explorer | reviewer
    pub model: ModelRef,
    pub role_prompt: String,
    pub extra_tools: Vec<ToolName>, // 唯讀基礎集之外的授權
    pub memory: MemoryMode,         // Om { conv } | Stateless
    pub spawn: SpawnRights,         // 能否開 subagent、能否建立 team 任務
    pub max_turns: Option<usize>,   // None：不限制輪數
}
pub enum ToolClass {
    Read,    // 所有 profile 都有
    Write,   // 改平台狀態或 worktree
    Spawn,   // 開 subagent 或 team 任務
    Suspend, // wait_for、schedule：執行後結束本輪
    Effect,  // 對外副作用：自動轉成核准請求
}
pub async fn run_agent(
    p: &AgentProfile,
    scope: Scope,      // Project | Goal | Task | Global
    trigger: Trigger,
    b: &Backends,
) -> Result<RunOutcome> {
    let ctx  = assemble_context(p, &scope, &trigger, b).await?; // preamble + 觸發訊息
    let gate = Arc::new(Gate::new(p, &scope, b.budget_for(&scope)));
    let config = pots_llm::agent::RunConfig {
        provider:     b.llm.provider(&p.model)?,      // Arc<dyn ModelProvider>
        model:        p.model.slug.clone(),
        instructions: Some(ctx.preamble),
        tools:        b.registry.toolset(READ_BASE.iter().chain(&p.extra_tools), &scope),
        hook:         gate.clone(),
        max_turns:    p.max_turns,
    };
    let mut history = match &p.memory {
        MemoryMode::Om { conv } => b.om.load(conv).await?, // 觀察紀錄 + 未觀察的原始尾巴
        MemoryMode::Stateless   => Vec::new(),
    };
    history.push(Item::User { text: ctx.trigger_msg });
    // 每個 AgentEvent::Item 由 sink 寫進 om_raw，TextDelta 轉成 UI 串流
    let (events, sink) = b.run_sink(&scope, &p.memory);
    let result = pots_llm::agent::run(&config, history, events, b.cancel_for(&scope)).await;
    sink.flush().await?;
    Ok(gate.finish(result)) // Done | Suspended | NeedsApproval | Cancelled
}
```

Gate 實作 pots-llm 的 `Hook`（`before_turn`、`before_tool` 回 `ToolDecision`、`after_tool`），負責所有「不能靠 prompt」的規則：

- 授權。 不在 profile 內的工具根本不交給模型（`toolset` 只組 profile 列出的），模型亂叫的名稱由 loop 直接回錯誤，所以不需要再 `Deny`。scope 由工具本身檢查並以文字錯誤回給模型，例如 Planner 只能動自己的 Goal、`write_artifact` 只能寫 profile 允許的種類、worker 只能讀寫自己的 worktree。

- 預算與取消。 每輪開始前（`before_turn`）檢查預算與 steer 事件，有就乾淨結束；取消走 loop 的 cancellation token，在每個 await 點生效。

- 工具類別。 Suspend 類回 `AllowThenStop`，執行後結束本輪；Effect 類回 `Stop`，不執行，改建核准請求後結束本輪。M2 的 `complete_goal` 也回 `AllowThenStop`：Goal 結束了，Planner 這一輪不該再做事。

- 追蹤。 每次呼叫寫進 trace（`after_tool`）。

外部 coding CLI 用自己的 loop，是唯一的例外；但它拿到的平台工具一樣由 worker profile 決定，透過 MCP 提供，gate 規則在 MCP server 端執行。

### Profile 一覽

| Profile | 記憶 | 唯讀基礎集之外的工具 | 可開 |
| --- | --- | --- | --- |
| chat | OM（project 對話） | `create_goal` 、`steer_goal` 、 `cancel_goal` 、 `archive_goal` 、`quick_fix` 、 `write_artifact` （Spec、Report）、 `artifact_history` 、 `restore_artifact` 、 `archive_artifact` （要核准）、 `propose_user_md_edit` 、 `propose_project_note` 、 `ask_user` 、 `schedule` 、`create_listener` 、 `list_automations` 、 `update_schedule` 、 `update_listener` 、 `delete_schedule` 、 `delete_listener` （刪除要核准） | subagent |
| planner | 無狀態（讀 Brief、 Ledger、任務樹） | `create_task` 、`cancel_task` 、 `post_to_team` 、 `request_approval` 、 `post_milestone` 、`escalate` 、 `complete_goal` 、`write_artifact` （Ledger）、 `propose_project_note` 、 `ask_user` 、`schedule` | subagent、 team 任務 |
| worker | 無狀態（讀任務 thread） | `edit_file` 、`run` 、`open_pr` 、 `wait_for` 、`post_to_team` 、 `report_progress` 、 `complete_task` 、`fail_task` 、 `escalate` 、`write_artifact` （Report）、 `ask_user` | 唯讀 subagent |
| operator | 無狀態（讀任務 thread） | worker 的工具，加上 `screen_observe` 、`ui_act` 、 `wait_ui` 、`app_open` 、`app_quit` 、 `node_run` 、`node_file_get` 、`node_file_put` 、 `gui_checkpoint` ；見[託管電腦](computer-nodes.md#能力與工具) | 不可 |
| global | OM（全域對話） | `create_goal` （任一 project）、 `cancel_goal` （任一 project）、 `archive_goal` 、 `pause_automations` 、 `resume_automations` 、 `schedule` （全域） | subagent |
| explorer | 無狀態 | 無 | 不可 |
| reviewer | 無狀態 | 無（只回傳審查意見） | 不可 |

Profile 存成設定檔（`crates/pots-server/profiles/*.toml`，編進執行檔）：名稱、記憶模式、`max_turns`（選填，不寫就不限制輪數；內建 profile 都不寫）、唯讀基礎集之外的工具、`write_artifact` 可寫的種類與 role prompt。模型不寫在 profile 裡：每個角色（chat、global、planner、worker、explorer、reviewer）用的 provider、模型與思考強度在設定視窗的「角色」類別調整，整個實例共用，沒設定時跟使用中的 provider 相同（見[模型 provider 設定](agent-runtime.md#模型-provider-設定)）。資料目錄的 `profiles/<name>.toml` 可以整份取代內建的；工具與種類依 agent 檢查（例如 chat 不能拿 `create_task`），不合法時服務不啟動。M2 的執行期只認得 chat 與 planner，新增一種 agent 仍要加它的觸發與 scope；之後的 profile 也照這個格式。

### 唯讀基礎工具

每個 profile 都有，查詢範圍受 scope 限制：

- 平台狀態：`get_status` 、`list_goals` 、`read_artifact` 、`read_team` 、`read_run` （其他 run 的摘要與 trace）

- 歷史：`recall` （自己的原始歷史）、`search_history` （scope 內的對話、artifact、 team 貼文）

- 程式碼：`list_files` 、`read_file` 、`grep` 、`git_log` 、`git_diff`

- 外部來源：`get_change` （PR 或 MR）、`get_ci_log` 、`search_mail` 、`read_mail` 、 `get_source_status` ；實際有哪些，由 project 啟用的 connector 決定

worker 讀自己的 worktree；其他 profile 讀的是每個 repo 預設分支的唯讀鏡像，由平台定期 fetch。

### 工作分級：小修改怎麼分配

對話 agent 收到請求後先分級，小修改走不經 Planner 的快速通道。

| 等級 | 例子 | 處理方式 | 留下的紀錄 |
| --- | --- | --- | --- |
| 回答 | 「這個函式在哪裡用到？」「CI 為什麼掛？」 | 對話 agent 用唯讀工具自己查，或開 explorer subagent | 只在對話中 |
| 快速修改 | 改錯字、調設定、單一檔案的小 bug | `quick_fix` 直接派一個 worker，不經 Planner、不寫 Brief | 一個 Quick Goal 加一個任務 |
| 功能 | 新功能、跨模組重構 | `create_goal` 先開草稿，你看過 Brief 按「開始」後 Planner 拆任務交給 team | 完整 Goal、 Brief、Ledger |

1. `quick_fix` 只帶一段描述與目標 repo。平台建立 kind 為 Quick 的 Goal 與單一任務，對話裡出現一張精簡卡片。

2. worker 照常在容器與 worktree 裡修改、開 PR、等 CI；merge 一樣要核准。

3. 快速通道有範圍上限：預設改動不超過 5 個檔案、200 行，且不需要設計決定。worker 發現超出時呼叫 `escalate` ，平台把它升級成一般 Goal、補建 Brief 交給 Planner；已做的修改留在分支上。

4. 同一個 repo 同時有多個快速修改時各自開 PR，不合併，方便逐一核准與回退。

5. 在輸入框用 `/fix` 或 `/goal` 開頭，可以直接指定快速修改或開 Goal，覆蓋對話 agent 的判斷。

6. 對話 agent 用 `create_goal` 開的 Goal 先是草稿（`GoalState::Draft`，協定 v17）：Brief 已經寫好，但不排 Planner。你在 Goal 卡片或 Goal 面板看 Brief，可以直接在編輯器改（不會 steer），也可以在對話裡和 agent 再討論，它用 `steer_goal` 帶 `edits` 改草稿的 Brief（只給 `reason` 會被拒絕）。按「開始」（`StartGoal`）才改成進行中、以 `Started` 觸發 Planner，拿到的是當下的 Brief；「捨棄」（`DiscardGoal`）等於取消。對話 agent 不能替你開始。`/goal` 也走草稿，因為 Brief 還是 agent 寫的；`quick_fix`、listener 與總管開的 Goal 直接開始。草稿和進行中的 Goal 一起算進每個 project 的上限，也出現在對話 agent 每輪指示的「草稿 Goal」。以 `tests/tasks.rs` 的 `draft_goals_wait_for_start` 與截圖流程的 `15a-live-goal-draft` 驗證；Android 還沒做。

### 主 agent、subagent 與 team

```mermaid
flowchart TB
    main["主 agent（Planner）<br/>拆任務、協調、做決定"]

    subgraph subagents["Subagents（同步）"]
        explorer["explorer · 查程式碼、讀 log"]
        reviewer["reviewer · 審查 diff"]
    end

    subgraph team["Team（非同步）"]
        workerA["worker · 任務 A"]
        workerB["worker · 任務 B"]
    end

    subgraph shared["共享管道"]
        tasks["任務樹<br/>狀態：誰在做什麼"]
        artifacts["Artifact<br/>Brief、Ledger、Report"]
        posts["Team 貼文<br/>發現、問題、決定"]
        notes["Project Notes<br/>跨 Goal 的專案知識"]
    end

    main <-->|"派出 / 回傳結論"| subagents
    main -->|create_task| team
    main <-->|讀寫| shared
    subagents -.->|唯讀| shared
    team <-->|讀寫| shared

    style main fill:#e8f0fc,stroke:#2f6fd6,stroke-width:2px
```

協作模型 · 主 agent、subagent、team 與共享管道

|  | 主 agent | Subagent | Team 成員 |
| --- | --- | --- | --- |
| 是誰 | 擁有對話或 Goal 的 agent：chat、planner、 global | 主 agent 在一輪內開出的臨時助手 | Planner 用 `create_task` 建立的 worker |
| 生命週期 | 永久（對話）或跟著 Goal | 單次 run，回傳結果即結束 | 跟著任務，可掛起、喚醒、跨好幾天 |
| 同步性 | — | 同步：主 agent 等它回來，可並行多個 | 非同步：透過任務樹與事件 |
| context | 自己的記憶或 Brief | 只有主 agent 給的說明 | 任務描述、Brief、 team 貼文 |
| 能做什麼 | 依 profile | 預設唯讀（explorer、 reviewer） | 依 worker profile，可寫入 |
| 結果去哪 | 對話或 artifact | 只回到主 agent 的 context | 任務結果、Report、 team 貼文 |
| 平台紀錄 | `agent_runs` | 子 run （`parent_run_id` ），不建任務 | `tasks` 與 `agent_runs` |

- subagent 是用來保持主 agent 的 context 乾淨。 讀大量程式碼、整理 CI log、審查 diff，把結論帶回來；主 agent 收到的是結論，不是過程。

- 上限。 subagent 不能再開 subagent（深度 1），每輪最多並行 4 個，預算從主 agent 的 run 扣。

- 何時改用 team。 需要寫入、需要等外部事件、或要跨超過一輪的工作，一律不用 subagent，改成 team 任務。

- team 成員不直接對話。 只透過下面的管道交換資訊，由 Planner 協調。

### 資訊分享管道

| 管道 | 內容 | 誰寫 | 誰讀 | 特性 |
| --- | --- | --- | --- | --- |
| 任務樹 | 狀態：誰在做什麼、卡在哪 | 平台（依工具呼叫） | 所有人 | 唯一真相，用 `get_status` 讀 |
| Artifact | 長篇共享知識： Spec、Brief、 Ledger、Report | 依 profile | 所有人 | 有版本，你可直接編輯 |
| Team 貼文 | Goal 內的短訊：發現、問題、決定、阻礙 | Planner、 worker | 同一 Goal 的成員 | 有型別、只增不改 |
| Subagent 回傳 | 單次查詢的結論 | subagent | 只有開它的主 agent | 私有，不落地 |
| Project Notes | 跨 Goal 的專案知識：怎麼跑測試、踩過的坑 | agent 提案，你核准 | 所有 profile（放在 preamble） | USER.md 的 project 版 |

```rust
pub enum TeamPost {
    Finding  { text: String },                     // 例：「auth 測試要先起 redis」
    Question { to: Option<TaskId>, text: String }, // 沒有 to 就是問 Planner
    Decision { text: String },                     // 只有 Planner 能發
    Blocker  { text: String },
    Handoff  { to: TaskId, text: String },
}
```

- 投遞。 成員每輪開始時收到「上次之後的新貼文」。只有 Question 與 Blocker 會喚醒 Planner，Handoff 會喚醒被交接的任務，Finding 不喚醒任何人。

- 防迴圈。 worker 的問題一律由 Planner 回答（Decision），worker 不會因為其他 worker 的貼文被喚醒；同一個 Goal 的貼文有速率上限。

- 沉澱。 Goal 結束時，Planner 把有長期價值的 Finding 整理成 Project Notes 提案。

- 和對話的關係。 team 貼文不進 OM，Planner 的里程碑記在 Goal 面板的「動態」分頁，對話裡只看得到 Goal 的結果；你想看細節時在 Goal 面板的 Team 分頁裡看。

