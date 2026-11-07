# 系統架構與資料模型

[← 產品提案索引](product.md)

## 系統架構總覽

整個平台是一個 Rust 服務加 Postgres。人類訊息經 Gateway、外部事件經 Watcher 進入，全部變成 `agent_runs` 佇列裡的工作，再由 Run 排程交給對話 agent、Planner 或 worker。

```mermaid
flowchart TB
    human["人類（GPUI 桌面客戶端）<br/>對話、核准、看進度<br/>手機客戶端待定"]
    sources["外部來源<br/>GitHub、GitLab、郵件、狀態頁"]

    subgraph service["單一 Rust 服務"]
        gateway["Gateway（axum）<br/>WebSocket、HTTP、通知"]
        scheduler["Run 排程<br/>agent_runs 佇列與 lease"]
        watcher["Watcher<br/>驗簽、正規化、去重、匹配"]
        agents["對話 agent ＋ Planner<br/>對話掛 OM，Planner 管任務樹"]
        workers["Workers<br/>rig 或 coding CLI，各自容器"]
        approver["核准執行器<br/>只執行 hash 相符的動作"]
        om["OM 背景壓縮<br/>Observer / Reflector"]
        sweeper["逾時 Sweeper<br/>每分鐘喚醒過期的等待"]
    end

    db[("SQLite 或 Postgres（唯一真相）<br/>任務樹、對話流、事件、核准、OM 記憶")]
    llm["LLM providers<br/>API key 只在伺服器端"]

    human <-->|WebSocket| gateway
    sources -->|"webhook / 輪詢 / IMAP"| watcher
    gateway --> scheduler
    watcher --> scheduler
    sweeper -.->|喚醒| scheduler
    scheduler --> agents
    scheduler -->|"派工、喚醒"| workers
    agents <-->|"載入 / 追加"| om
    service <-->|所有狀態讀寫| db
    service -->|模型呼叫| llm
```

系統架構 · 單一服務加 SQLite 或 Postgres

agent 之間不直接互相呼叫：它們只透過工具讀寫 Postgres 中的任務樹，由排程器決定下一個跑誰。核准後的 merge 等動作由核准執行器呼叫 GitHub，不經過 agent。

## 核心資料模型

Goal 與 Task 構成任務樹，是平台唯一的真相來源；對話流（StreamItem）、agent 的記憶（OM）、外部事件各自獨立存放。ID 一律用 UUIDv7，時間有序，分頁直接用 `before=<id>` 。

### 型別

```rust
pub enum Principal { Human(UserId), Agent(AgentId), System }
pub enum TaskState {
    Planned,
    Running    { run: RunId, lease_until: Timestamp },
    Waiting    { on: WaitCond, deadline: Timestamp },
    NeedsHuman(Escalation),
    Done(Outcome),
    Failed     { attempts: u8, error: String },
    Cancelled  { by: Principal },
}
pub enum WaitCond {
    CiFinished  { repo: RepoId, sha: Sha },
    ChangeReviewed { change: ChangeRef },  // PR 或 MR
    ChangeMerged   { change: ChangeRef },
    Resource       { key: ResourceKey, kinds: KindSet }, // 任何來源的通用等待
    Subtasks(Vec<TaskId>),
    At(Timestamp),                 // 自我排程的單次喚醒
    Approval(ApprovalId),
}
pub enum StreamItem {
    Human(Msg),
    Agent(Msg),               // 可串流：status = streaming | done | failed
    Card(Card),
    Event(EventRef),          // 只給人看，不進 OM
}
pub enum Card {
    GoalCreated { goal: GoalId },
    Milestone   { goal: GoalId, text: String },
    Approval    { id: ApprovalId },
    CatchUp     { since: Timestamp, summary: String },
    Artifact    { id: ArtifactId, change: ArtifactChange }, // 建立或重大更新
    Triggered   { listener: ListenerId, goal: GoalId },     // listener 自動開工
}
pub enum MemoryPolicy { Observe, ObserveOutcome, Skip }
pub fn memory_policy(item: &StreamItem) -> MemoryPolicy {
    match item {
        StreamItem::Human(_) | StreamItem::Agent(_)       => MemoryPolicy::Observe,
        StreamItem::Card(Card::Approval { .. })           => MemoryPolicy::ObserveOutcome,
        StreamItem::Card(Card::CatchUp { .. })            => MemoryPolicy::Skip,
        StreamItem::Card(_)                               => MemoryPolicy::Observe,
        StreamItem::Event(_)                              => MemoryPolicy::Skip,
    }
}
```

### 資料表（SQLite 與 Postgres 共用）

| 表 | 用途 | 關鍵欄位 |
| --- | --- | --- |
| `projects` | 一個 project 對應一個對話 agent 與一條對話 | `id` , `name` , `instructions` , `repos[]` , `budget` |
| `stream_items` | UI 顯示的完整對話流 | `id` , `project_id` , `thread_of` , `author` , `kind` , `body jsonb` , `status` |
| `goals` | 人類意圖的根節點 | `id` , `project_id` , `title` , `state` , `brief, budget_tokens` , `spent_tokens` |
| `tasks` | 任務樹節點 | `id` , `goal_id` , `parent_id` , `assignee` , `state jsonb` , `attempts` , `spent_tokens` |
| `task_events` | 任務狀態變化的只增日誌（稽核與重播） | `task_id` , `seq` , `from` , `to` , `cause` |
| `agent_runs` | agent 執行佇列（`FOR UPDATE SKIP LOCKED` ） | `id` , `agent` , `task_id?` , `trigger` , `state` , `lease_until` |
| `subscriptions` | 任務訂閱的外部資源 | `task_id` , `resource_key` （如 `gh:owner/repo@sha` ） |

| 表 | 用途 | 關鍵欄位 |
| --- | --- | --- |
| `external_events` | 正規化後的外部事件 | `id` , `dedupe_key unique` , `kind` , `payload` , `matched_tasks[]` |
| `approvals` | 核准請求 | `id` , `task_id` , `action jsonb` , `action_hash` , `state` , `expires_at` , `decided_by` |
| `om_raw` | rig 寫入的原始訊息（含工具呼叫） | `project_id` , `seq` , `message jsonb` , `tokens` , `observed bool` |
| `om_observations` | Observer 產出的觀察 | `project_id` , `generation` , `seq` , `date` , `priority` , `text` , `pinned` |
| `push_subscriptions` | Web Push 訂閱（候選方案，推播方式待定） | `user_id` , `endpoint` , `p256dh` , `auth` |
| `artifacts` | 人與 agent 共同編輯的文件（spec、brief、ledger、 report） | `id, project_id, goal_id?, kind, title, rev, body_md` |
| `artifact_revisions` | 每次修改的版本與作者，用於 diff 與還原 | `artifact_id, rev, author, body_md, at` |
| `schedules` | agent 自己設定的單次或週期喚醒 | `id, owner, spec, prompt, next_at, expires_at, enabled` |
| `listeners` | 符合條件的外部事件自動開 Goal 的規則 | `id, project_id, filter, action, budget_per_trigger, max_per_hour, enabled` |
| `team_posts` | Goal 內 team 成員的短訊（發現、問題、決定、阻礙、交接） | `id, goal_id, author_task?, kind, to_task?, body, at` |
| `sources` | 外部來源的設定、輪詢進度與健康狀態 | `id, kind, config, trust, ingest, cursor, health, enabled` |

`stream_items` 給人看，`om_raw` 給 agent 用，兩者以 `run_id` 關聯。這樣 UI 可以保留完整歷史，OM 也可以自由壓縮，互不干擾。

## Crate 選型

大多是成熟、維護中的 crate。需要特別管理版本的是 rig，以及仍在預發布階段的 GPUI 相關 crate。

| 領域 | Crate | 備註 |
| --- | --- | --- |
| 非同步執行期 | `tokio` |  |
| HTTP / WebSocket | `axum` （`ws` feature） | 客戶端的即時協定與 HTTP API |
| 資料庫 | `sqlx` （Postgres） | 單機版 SQLite、自架版 Postgres，兩個 feature 都開；編譯期檢查 SQL，內建 migration |
| LLM 與 agent | `rig-core` | 釘精確版本，包在自己的 `llm` 模組後 |
| 記憶策略（選用） | `rig-memory` | 只參考其介面，OM 自己實作 |
| JSON Schema | `schemars` | 工具參數定義 |
| GitHub API | `octocrab` | GitHub App 認證與 REST |
| Webhook 驗簽 | `hmac` + `sha2` |  |
| Web Push | `web-push` | VAPID；手機推播候選方案 |
| 登入 | `webauthn-rs` | passkey |
| ID | `uuid` （v7） | 時間有序 |
| 可觀測性 | `tracing` 、`tracing-opentelemetry` |  |

| 領域 | Crate | 備註 |
| --- | --- | --- |
| 桌面客戶端 | `gpui-kit` （GPUI） | GPUI 為預發布版，釘精確版本；需較新的 Rust 編譯器 |
| Artifact 編輯器 | `gpui-kit-markdown-editor` | 區塊式所見即所得 Markdown；0.1.x，釘精確版本 |
| MCP（後期） | `rmcp` | 把平台包成 MCP server，讓外部 agent 加入 |
| Git 操作 | `tokio::process` 呼叫 `git` | worktree 等操作直接用 CLI 最穩 |
| 桌面通知（單機版） | `notify-rust` | macOS、Linux、Windows 原生通知 |
| 憑證儲存（單機版） | `keyring` | API key 存 OS keychain |
| 排程 | `croner` | 解析 cron 字串、計算下一次觸發 |
| Artifact diff | `similar` | 產生「人類修改」的文字 diff |
| GitLab API | `reqwest` | 直接呼叫 REST，涵蓋自架實例 |
| 收信 | `async-imap` 、`mail-parser` | IMAP IDLE 長連線與郵件解析 |
| 寄信 | `lettre` | SMTP，核准後才由平台寄出 |
| RSS／Atom | `feed-rs` | 狀態頁與公告 feed |

