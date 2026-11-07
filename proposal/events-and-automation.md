# 外部事件與自動化

[← 產品提案索引](product.md)

## 外部事件：Source Connector 與 Watcher

Watcher 是確定性的 Rust 程式碼，透過一組 source connector 接收外部事件：GitHub、 GitLab、郵件、服務狀態頁、HTTP 探測、第三方監測、通用 webhook 與 RSS。每個 connector 只負責「收進來、驗證、正規化」；去重、比對、喚醒與 listener 共用同一條管線，新增來源不必改其他地方。只有需要判斷的地方才呼叫 LLM，例如分類 CI 失敗是不是 flaky、判斷一封郵件需不需要處理。

### Connector 介面

```rust
pub enum Ingest {
    Push,   // webhook：GitHub App、GitLab webhook、通用 webhook
    Poll,   // 定期拉取：API、狀態頁、RSS、HTTP 探測
    Stream, // 長連線：IMAP IDLE
}
pub enum Trust {
    Owned,   // 自己的 CI、自己的探測結果
    Members, // 只有有權限的人能產生：repo 協作者加的 label、內部系統
    Public,  // 任何人都能寫：公開 issue、郵件、外部狀態頁的文字
}
#[async_trait]
pub trait SourceConnector: Send + Sync {
    fn kind(&self) -> SourceKind; // Github | Gitlab | Imap | StatusPage | HttpProbe | Webhook | Rss
    fn ingest(&self) -> &[Ingest]; // 支援的接收方式，依偏好排序
    fn trust(&self, raw: &RawEvent) -> Trust;
    async fn verify(&self, req: &PushRequest) -> Result<()>; // 驗簽或驗 token
    async fn poll(&self, subs: &[Subscription], cursor: Option<Cursor>)
        -> Result<(Vec<RawEvent>, Cursor)>;
    async fn stream(&self, tx: mpsc::Sender<RawEvent>) -> Result<()>; // 斷線由 registry 重連
    fn normalize(&self, raw: RawEvent) -> Result<Vec<ExternalEvent>>;
    async fn snapshot(&self, r: &ResourceKey) -> Result<ResourceState>; // 重啟後補漏
    fn read_tools(&self) -> Vec<ToolName>;   // 加進唯讀基礎集
    fn effect_tools(&self) -> Vec<ToolName>; // 對外動作，一律走核准
}
```

不支援的接收方式使用預設實作回傳 `Unsupported` 。每個 source 是 `sources` 表的一列，同一種 connector 可以有多個 source（例如公司的自架 GitLab 和個人的 GitHub）。

| Connector | 接收方式 | 主要事件 | 唯讀工具 | 對外動作（需核准） | 備註 |
| --- | --- | --- | --- | --- | --- |
| GitHub | App webhook；單機版輪詢 | PR、 issue、 review、留言、check suite | `get_change` 、 `get_ci_log` | merge、留言 | 驗 `X-Hub-Signature-256` ，用 `X-GitHub-Delivery` 去重 |
| GitLab（含自架實例） | project 或 group webhook；單機版輪詢 | MR、 issue、留言、 pipeline | `get_change` 、 `get_ci_log` | merge、留言 | 驗 `X-Gitlab-Token` ；MR 與 PR 共用通用的 Change 模型 |
| 郵件 | IMAP IDLE，兩種模式都能用 | 指定資料夾或 label 的新郵件 | `search_mail` 、 `read_mail` | `reply_mail` 、 `send_mail` | 只收白名單資料夾；檢查 DKIM 與 SPF 結果；附件不自動開啟 |
| 狀態頁 | 輪詢 JSON 或 RSS；有 webhook 就用 | incident 開啟、更新、解決；元件狀態變化 | `get_source_status` | 無 | 追蹤你依賴的上游服務，例如 LLM provider、雲端 |
| HTTP 探測 | 平台定時請求指定 URL | 失敗、恢復、延遲超標 | `get_source_status` | 無 | 用連續失敗次數防抖 |
| 第三方監測（例如 Downdetector） | 只用對方正式提供的 API 或 feed | 回報量異常 | `get_source_status` | 無 | 先確認服務條款與是否需要付費方案；沒有正式管道就不接 |
| 通用 webhook | 每個 source 一個 URL 與 secret；單機版需 tunnel | 自訂 | 無 | 無 | 用設定把 JSON 欄位對應成事件 |
| RSS／Atom | 輪詢 | 新項目 | 無 | 無 | release note、公告 |

通用 webhook 的欄位對應寫在設定檔，用 JSON pointer 指定：

```toml
[[sources]]
id    = "sentry"
kind  = "webhook"
trust = "members"
[sources.map]
kind     = "alert.triggered"
resource = "/data/issue/project"
summary  = "/data/issue/title"
dedupe   = "/data/issue/id"
```

### 正規化與去重

所有 connector 都輸出同一種事件；PR 與 MR 共用 Change，`RepoId` 帶上 host （`github.com/owner/repo` 、`gitlab.example.com/group/project` ）。

```rust
pub struct ExternalEvent {
    pub source: SourceId,        // 例：github:personal、gitlab:company、 mail:work、status:openai
    pub kind: EventKind,
    pub resource: ResourceKey,   // 例：change:github.com/o/r#42、mail-thread: <id>、service:openai/api
    pub dedupe_key: String,
    pub occurred_at: Timestamp,
    pub trust: Trust,
    pub summary: String,         // 平台依欄位產生，可信
    pub body: Untrusted<String>, // 他人寫的內容
    pub links: Vec<Url>,
}
pub enum EventKind {
    Change(ChangeEvent), // PR 與 MR：Opened、Reviewed、Commented、Merged
    Issue(IssueEvent),   // Opened、Labeled、Commented
    Ci(CiEvent),         // Finished { sha, conclusion }
    Mail(MailEvent),     // Received { from, subject, thread }
    Status(StatusEvent), // IncidentOpened { severity }、Updated、Resolved、 ProbeFailed、ProbeRecovered
    Custom(String),      // 通用 webhook 的自訂種類
}
pub fn wakes(cond: &WaitCond, ev: &ExternalEvent) -> bool {
    match (cond, &ev.kind) {
        (WaitCond::CiFinished { repo, sha }, EventKind::Ci(CiEvent::Finished { sha: s, .. })) =>
            ev.resource.repo() == Some(repo) && sha == s,
        (WaitCond::ChangeReviewed { change },EventKind::Change(ChangeEvent::Reviewed { .. })) =>
            &ev.resource == change,
        (WaitCond::ChangeMerged { change },EventKind::Change(ChangeEvent::Merged)) =>
            &ev.resource == change,
        (WaitCond::Resource { key, kinds }, kind) =>
            &ev.resource == key && kinds.contains(kind),
        _ => false,
    }
}
```

- 去重：`external_events.dedupe_key` 加 unique 索引。同一個來源同時開著 webhook 與輪詢也不會重複。

- 依種類防抖：

- CI：同一個 sha 的所有 check suite 或 pipeline 結束才發一個彙總事件。

- HTTP 探測：連續 3 次失敗才發 ProbeFailed，連續 2 次成功才發 ProbeRecovered。

- 狀態頁：同一個 incident 5 分鐘內的多次更新合併成一則。

- 郵件：同一個 thread 短時間內的連續來信合併成一則。

- 游標：每個 source 在 `sources.cursor` 記錄輪詢進度，重啟後接續；IMAP 用 UIDVALIDITY 加 UID。

- connector 健康狀態：token 過期、連線斷線、連續輪詢失敗時，狀態面板標示該 source 異常並推播通知，不會默默停止收事件。

- 不可信內容：`body` 一律包在 `Untrusted<T>` 裡，送進 agent 時標明來源與信任等級。

### 關聯、喚醒與逾時

1. Worker 開 PR 或 push 時，平台自動寫入 `subscriptions` （任務 → `gh:owner/repo@sha` 、`gh:owner/repo#pr` ）。分支名稱也帶上 task id 作為備援關聯。

2. 事件進來時，用 `resource_key` 查訂閱，再對處於 `Waiting` 的任務跑 `wakes` ，匹配到就建立喚醒用的 `agent_runs` 。

3. 沒有匹配任務的事件只進 project 的活動紀錄，不廣播給任何 agent。

4. 一個每分鐘執行的 sweeper 掃描 `deadline` 已過的 `Waiting` 任務，以「逾時」喚醒它們。

5. 服務啟動時重新檢查所有 `Waiting` 任務，補上停機期間漏掉的事件（輪詢一次對應資源的現況）。

### CI 修復迴圈上限

CI 失敗時喚醒對應 worker 去修，但同一個任務最多試 `max_ci_fix` 次（預設 3）。超過就轉成 `NeedsHuman(RepeatedCiFailure)` ，推播給人類。

### 自我排程

agent 可以替自己設定時間醒來，醒來時在原本的對話或 Goal 裡帶著完整 context 繼續。例如每天早上整理前一天失敗的 CI、merge 後兩天提醒你移除 feature flag、每 10 分鐘檢查一次長時間的遷移作業。

```rust
pub enum ScheduleSpec {
    Once { at: Timestamp },
    Every { cron: String, until: Option<Timestamp> }, // 用 croner 解析
}
pub enum ScheduleOwner {
    Chat(ProjectId),    // 在 project 對話裡醒來，帶 OM 記憶
    Planner(GoalId),    // 在 Goal 下醒來
    Global,             // 總管 agent
}
pub struct Schedule {
    id: ScheduleId,
    owner: ScheduleOwner,
    spec: ScheduleSpec,
    prompt: String,     // 醒來時要做什麼，作為觸發訊息
    next_at: Timestamp,
}
```

- 機制。 sweeper 每分鐘取出 `next_at` 已到的排程，建立 `agent_runs` 並計算下一次時間。任務內的單次等待則用 `WaitCond::At` ，走既有的喚醒流程。

- 上限。 週期排程最短間隔 10 分鐘，每個 project 最多 10 個；每次觸發計入預算。週期排程預設 30 天後過期，可由你延長。

- 透明。 agent 建立排程時對話裡出現一張卡片；狀態面板有「排程」分頁，可以暫停、刪除、改時間。

- 單機版的錯過觸發。 電腦睡眠期間錯過的觸發，醒來後每個排程只補跑一次，不逐次補。

### Listener：事件自動開工

除了喚醒等待中的任務，符合條件的外部事件也可以直接開一個新 Goal。listener 由你在 project 設定中建立，或由對話 agent 用 `create_listener` 提案、你核准後生效。

```rust
pub struct EventFilter {
    pub source: Option<SourceId>,
    pub kinds: KindSet,           // 例：Issue::Labeled、Ci::Finished、 Status::IncidentOpened
    pub resource: Option<Glob>,   // 例：change:github.com/me/*、 service:openai/*
    pub fields: Vec<FieldMatch>,  // 例：label = "agent"、severity >= major、 from in [...]
}
pub enum ListenerAction {
    CreateGoal { template: String }, // 模板 + 事件內容 → Goal brief
    QuickFix   { template: String }, // 小修改直接走快速通道
    NotifyChat,                      // 只在 project 對話裡告訴對話 agent
}
pub struct Listener {
    id: ListenerId,
    project: ProjectId,
    filter: EventFilter,
    action: ListenerAction,
    budget_per_trigger: Budget,
    max_per_hour: u32,
}
```

- 路由順序。 事件進來後先匹配等待中的任務，再匹配 listener；都沒對上才只寫活動紀錄。

- 同一個資源回到同一個 Goal。 listener 開 Goal 時自動訂閱該資源（issue、PR 或 MR、郵件 thread、incident），之後的事件都進同一個 Goal，不會開新的。

- 依信任等級的預設值。 Owned 與 Members 來源可以直接 CreateGoal 或 QuickFix。 Public 來源（郵件、公開 issue）預設只能 NotifyChat；要讓它自動開工，必須加上白名單條件（label、寄件者）並經過你核准。

- 防失控。 每次觸發有預算上限，每小時觸發次數有上限；超過就暫停該 listener 並通知你。

- 單機版。 GitHub、GitLab、狀態頁、RSS 用輪詢，郵件用 IMAP IDLE，HTTP 探測照常執行；通用 webhook 需要 tunnel，或等自架版再接。

- 範例。

- GitHub 或 GitLab 上被加了 `agent` label 的 issue → CreateGoal。

- main 分支 CI 失敗 → CreateGoal「修好 main」。

- 上游 LLM provider 的狀態頁開了 major incident → NotifyChat，對話 agent 檢查哪些任務受影響並暫停它們。

- 白名單寄件者寄來的特定主旨郵件 → NotifyChat，由對話 agent 讀信後建議要不要開 Goal。

