# 使用體驗與前端

[← 產品提案索引](product.md)

## 使用情境與 UX 原則

三種使用情境各有一個入口，但都掛在同一條 project 對話旁邊，不切換「模式」。

| 情境 | 入口 | 資料來源 | 行為 |
| --- | --- | --- | --- |
| 和 agent 討論功能 | 主對話流（每個 project 一條） | OM 記憶 + 最近原始訊息 + 即時狀態 | 討論收斂時 agent 開一張草稿 Goal 卡片，你看過 Brief 按「開始」 |
| 核准 | 對話中的內嵌卡片、全域 Inbox、手機推播 | Task Store | 一鍵核准或拒絕，可附一句理由 |
| 看進度 | 側邊狀態面板（任務樹、動態、 PR、CI） | Task Store 即時查詢 | 對話裡只出現 Goal 卡片、核准與 Goal 的結果 |

沒有 session，但工作有結構。 主對話永遠是同一條。當討論收斂成具體工作，agent 開一張 Goal 卡片，後續細節掛在卡片的 thread 下，主流不被洗版。

進度不灌進對話。 對話裡只放要你知道或決定的事：Goal 卡片（狀態與 Planner 的最後一句話會隨時更新）、需要核准、需要你處理、Goal 完成或失敗。Planner 的里程碑（規劃完成、改派重試）、開工與 Planner 叫醒自己的排程只記在 Goal 面板的「動態」分頁，不貼到對話、也不通知。

看得到 agent 在做事。 對話 agent 呼叫工具前會先用一句話說它要做什麼（「我先查一下目前的 Goal 狀態。」）。這些旁白在回覆串流中以淡色顯示在訊息裡；回覆完成後訊息只留最後的回答，旁白收進「為什麼這樣回答」面板，和模型輪、工具呼叫依序排在一起。最後一輪沒有文字時（例如回答寫在結束 run 的工具呼叫旁），最後一段旁白當成回答。

離開後回來。 對話流底部插入一張「你不在期間」摘要卡片，內容由事件與任務狀態產生，不依賴 OM。閾值：距上次已讀超過 2 小時且期間有里程碑。

agent 忙碌時插話。 輸入框旁有兩個動作：預設「排隊」，等本輪結束再處理；「改方向」則發出 steer 事件，中斷目前的執行並讓 agent 重新規劃。對話頂端顯示 agent 狀態：閒置、思考中、規劃中、等待 CI、等你核准；agent 在跑的時候旁邊有「停止」。

記憶透明化。 提供「agent 記得什麼」面板，可以查看、修正、釘選或刪除觀察。修正會讓快取前綴失效一次，可以接受。修正與刪除是軟性的：舊內容留在資料庫作稽核，模型只看得到修正後的版本。

完整歷史永遠可捲。 UI 顯示的是原始訊息，OM 只是 agent 看到的衍生視圖，兩者分開存。

## 桌面客戶端現況

桌面畫面以 [`crates/pots-desktop/`](../crates/pots-desktop/) 為準。它預設連到本機的內嵌服務：M0 接上的是 project 列表與建立、頻道對話（排隊／改方向、串流回覆、工具摘要、停止）與 Inbox 裡的 ChatGPT 登入；M1 接上頻道的「記憶」分頁（查看、釘選、修正、刪除觀察）與設定的「記憶」類別（整理記憶的模型）；M2 接上 Goal 卡片與里程碑、右欄 Goal 面板（任務、Brief、Ledger、取消任務）、「Artifacts」分頁與編輯器（草稿按「儲存」才送出、版本紀錄與還原）；M4 接上 Goal 面板的任務狀態（執行中、需要決定、完成、失敗）、指派、分支與 PR 連結、worker 的最後一句進度、「重試」、Team 分頁、快速修改的標籤，以及設定的「Worker」類別（後端、執行環境、image、Claude Code 的登入指令）；M5 接上 Inbox 與對話裡的核准卡片（核准、拒絕附理由、看 PR 與提案的 diff）、「需要你處理」的任務、系統通知、設定的「通知」（靜音時段）與「USER.md」類別，以及頻道「自動化」分頁的 merge 方式。M6a 接上總管（live 的全域對話、專案概況與記憶）、每一則 agent 訊息的「為什麼這樣回答」面板（觸發來源、記憶版本、模型、用量與工具順序）、真實的 Goal 預算百分比，以及服務產生的「你不在期間」摘要卡片。M9 接上 project onboarding 與工作資料夾（進行中）：新增專案改成精靈（名稱加上 GitHub repo 或本機 git repo 的絕對路徑，先驗證再建立，也可以「稍後再設定」），沒設定時頻道顯示提示，按「設定」在同一個對話框補上，自動化分頁也能改；設定後對話 agent 也知道工作資料夾，使用者也可以直接請它用 `set_workdir` 設定。`--mock` 模式用假資料展示其他畫面：實例直列、任務 thread 的 worker 對話、完整的 Inbox、總管與自動化；各畫面可以做什麼、哪些按鈕還沒實作，見該目錄的 README。用 `cargo test -p pots-desktop --test screenshots` 可以輸出主要畫面的截圖。

下方[介面配置](#介面配置)的桌面欄位與客戶端一致；還沒實作的區塊（電腦、project 移交）是預定的位置。手機欄位對應 [`android/`](../android/) 的客戶端，目前也是假資料。

## 前端

桌面客戶端用 Rust 的 [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui)，元件庫用 [GPUI Kit](https://github.com/longbridge/gpui-kit)，是原生程式而非 WebView。客戶端只透過下方的[即時協定](#即時協定)與 HTTP API 和服務溝通。桌面版的 app 自己內嵌一個服務並經 loopback 連線，另外也可以同時連上任意數量的遠端實例，連線管理見[客戶端多連線](deployment.md#客戶端多連線)。

Artifact 編輯器用 [gpui-kit-markdown-editor](https://docs.rs/crate/gpui-kit-markdown-editor/latest)：區塊式所見即所得，直接以 Markdown 讀寫，支援標題、巢狀清單、待辦、表格、程式碼區塊、跨區塊選取與輸入法。評估用的原型在 [`prototypes/gpui-editor/`](../prototypes/gpui-editor/)，已在 macOS 實測中文輸入法可用；選型過程見[已決定事項](roadmap-and-decisions.md#已決定事項)。

手機客戶端是原生 Android app（Kotlin 加 Jetpack Compose），在 [`android/`](../android/)，推播經 ntfy（見[手機推播](approvals-and-notifications.md#手機推播)）。它和桌面客戶端走同一套即時協定與 HTTP API。下表的手機欄位對應 Android 客戶端的配置。

### 介面配置

| 區塊 | 桌面 | 手機 | 內容 |
| --- | --- | --- | --- |
| 實例直列 | 最左側窄欄，只連一個實例時隱藏 | Projects 分頁頂端的切換器，只連一個實例時隱藏 | 全域 Inbox、各實例圖示（待處理徽章、離線警示）、加入實例 |
| 左欄 | 目前實例 | 底部分頁「Projects」 | 頂端是實例名稱與形態，點下去是實例選單；接著是總管、這個實例的 Inbox、專案列表、自動化（排程、Listener）與電腦，最下方是「設定」。每個 project 一列，標示待核准數、「需要你」或未讀；已移交的 project 顯示新位置 |
| 新增專案 | 左欄的「新增專案」打開精靈 | 尚未接上 | 名稱、工作資料夾（GitHub repo、本機 git repo 的絕對路徑，或「稍後再設定」）；建立前驗證，失敗不留半個 project。沒設定時 project 顯示提示 |
| 對話流 | 中欄，project 頻道的「對話」分頁 | 點進 project | 連續對話 + 內嵌卡片，頂端顯示 agent 狀態、本週預算與「停止」 |

| 區塊 | 桌面 | 手機 | 內容 |
| --- | --- | --- | --- |
| 狀態面板 | 右欄 | 對話頁的上拉抽屜 | Goal 面板（任務依狀態分組、Brief、Ledger、Team 分頁，留言給 Planner）；點任務進任務 thread（狀態、分支、PR 與 CI、進度、直接對 worker 說話） |
| 成員 | 右欄，由頻道標題列的「成員」按鈕切換，和狀態面板二選一 | 尚未接上 | 像 Discord／Slack 的成員欄：你、對話 agent、每個進行中 Goal 的 Planner、手上有任務的 worker，分成「工作中」「等待中」「待命」。每一列顯示狀態或最後一句進度，以及它用的模型與思考強度。點 worker 進任務 thread、點 Planner 開 Goal 面板、點對話 agent 開設定的「角色」。完全由客戶端從 project、provider 與 worker 設定推導，沒有額外的協定 |
| Inbox | 實例直列最上方（全域）；左欄裡的 Inbox 只看目前實例 | 底部分頁「Inbox」 | 所有已連線實例、所有 project 的待處理事項，依時間排序並標示所屬實例，可依實例與類型（核准、需要你處理、找你的訊息）篩選 |
| 記憶 | project 頻道的「記憶」分頁 | 同左 | 觀察紀錄與「目前在做什麼」，可修正、釘選、刪除；整理失敗時顯示原因 |
| Artifact | 列表是 project 頻道的「Artifacts」分頁；編輯器在右欄，取代狀態面板 | 全螢幕頁 | Markdown 編輯器，以 `##` 區段標示你改過的區段與 agent 正在寫入的區段；版本紀錄與 diff |
| 總管 | 左欄入口的獨立畫面，和對話流一樣佔中欄，右欄是專案概況與記憶；任何畫面都可用快捷鍵切過去 | 底部分頁「總管」 | 同一個實例內跨 project 的永久對話，依目前選取的實例切換 |
| 自動化 | 左欄「排程」「Listener」進入的實例層級畫面，佔中欄，可依專案篩選；project 頻道的「自動化」分頁只看這個 project。點一列在右欄看細節 | 同左 | 排程與 listener 列表、批次暫停狀態；可停用、刪除、看條件與觸發紀錄，待核准的 listener 可以直接核准 |
| 電腦 | 左欄入口的實例層級畫面，佔中欄 | 同左 | 託管電腦的連線狀態、lease 持有者、即時縮圖、權限檢查；「接手」、「交還」與「停止」 |
| 設定 | 左欄最下方的入口，打開蓋在主畫面上的設定視窗：左邊是類別（模型 Provider、角色、記憶、關於），右邊是內容；點背景、右上角或按 Esc 關閉 | 「設定」分頁 | 屬於目前實例的設定。模型 provider：內建的 ChatGPT 方案（登入、登出、模型）與新增的 OpenAI／Anthropic 相容 API（base URL、API key、模型、測試連線），選一個「使用中」。角色：對話 agent、總管、Planner、內建 worker、explorer 與 reviewer 各自的 provider、模型與思考強度（模型預設／關閉／低／中／高），預設跟使用中的 provider 相同。記憶：整理記憶（Observer／Reflector）用的 provider 與模型，預設跟對話 agent 相同。還沒有使用中的 provider 時，Inbox 有一筆「設定模型 provider」指向這裡 |

### 即時協定

客戶端對每個實例各開一條 WebSocket，伺服器推送事件。序號是每個實例各自的，客戶端為每條連線分別記錄 `last_seq`。型別在 [`crates/pots-proto`](../crates/pots-proto/)：

```rust
pub enum StreamScope { Project(ProjectId), Overseer, Task(TaskId) } // 事件屬於哪條對話流
/// project 的工作資料夾：程式碼在哪、worker 在哪裡改（每個 project 一個）。
pub enum WorkDir { Github { repo: String }, Local { path: String } }
#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ServerEvent {
    Snapshot      { seq: u64, instance: InstanceSnapshot },              // 整份替換
    ProjectAdded  { seq: u64, project: ProjectSnapshot },
    ProjectWorkdir { seq: u64, project: ProjectId, workdir: Option<WorkDir> }, // 工作資料夾整份替換
    Item          { seq: u64, scope: StreamScope, item: StreamItemView },
    Delta         { scope: StreamScope, item_id: ItemId, text: String },  // 不記日誌
    StreamingText { scope: StreamScope, item_id: ItemId, text: String },  // 不記日誌
    ItemUpdated   { seq: u64, scope: StreamScope, item: StreamItemView }, // 例如工具摘要
    ItemDone      { seq: u64, scope: StreamScope, item: StreamItemView }, // 最後的完整內容
    AgentState    { seq: u64, project: ProjectId, state: AgentState },
    TaskChanged   { seq: u64, task: TaskView },
    Inbox         { seq: u64, pending: u32 },
    ModelAuth     { seq: u64, state: ModelAuthView },  // SignedOut | Pending { url } | SignedIn | Error
    LoginUrl      { url: String },                      // 只回給按下登入的連線；不記日誌
    Providers     { seq: u64, providers: ProvidersView },  // provider 設定與使用中的那個；不含 key
    ProviderTest  { request: u64, result: Result<Vec<ModelOption>, String> }, // 只回給測試的連線；不記日誌
    Memory        { seq: u64, project: ProjectId, memory: MemoryView }, // project 的記憶整份替換
    GoalChanged   { seq: u64, project: ProjectId, goal: GoalView },     // Goal 連同任務樹整筆替換
    ArtifactChanged { seq: u64, project: ProjectId, artifact: ArtifactView }, // 整筆替換
    ArtifactHistory { artifact: ArtifactId, revisions: Vec<ArtifactRevisionView> }, // 只回給問的連線；不記日誌
    Error         { message: String },                  // 指令被拒絕或失敗；不記日誌
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ClientCommand {
    Resume           { last_seq: u64 },   // 連線後的第一個指令
    CreateProject    { name: String, workdir: Option<WorkDir> }, // 建立前驗證；不帶也能建立
    SetProjectWorkdir { project: ProjectId, workdir: Option<WorkDir> }, // None 清除
    Send             { project: ProjectId, id: ItemId, text: String, mode: SendMode }, // Queue | Steer
    Stop             { project: ProjectId },
    Decide           { approval: ApprovalId, approve: bool, note: Option<String> },
    Cancel           { task: TaskId },
    StartModelLogin,                      // 伺服器回 LoginUrl，並廣播 ModelAuth { Pending }
    CancelModelLogin,
    ModelLogout,
    SaveProvider      { draft: ProviderDraft },     // 新增或修改；api_key 為 None 時沿用已存的 key
    DeleteProvider    { id: ProviderId },
    SetActiveProvider { id: ProviderId },
    TestProvider      { request: u64, draft: ProviderDraft }, // 列出模型驗證設定，不儲存
    SetMemoryModel    { provider: Option<ProviderId>, model: String }, // 整理記憶的模型；None 跟對話相同
    SetRoleModel      { role: ModelRole, provider: Option<ProviderId>, model: String, thinking: ThinkingLevel }, // 角色的模型；None 跟使用中的相同
    PinObservation    { project: ProjectId, id: ObservationId, pinned: bool },
    EditObservation   { project: ProjectId, id: ObservationId, text: String }, // 舊內容保留作稽核
    DeleteObservation { project: ProjectId, id: ObservationId },
    SaveArtifact      { artifact: ArtifactId, base_rev: u32, body: String }, // base_rev 過期時以區段合併
    ArtifactHistory   { artifact: ArtifactId },     // 伺服器回 ArtifactHistory
    RestoreArtifact   { artifact: ArtifactId, rev: u32 }, // 新增一個內容等於舊版的版本
}
```

- 事件日誌：帶 `seq` 的事件寫進事件日誌，`Resume` 時依序補送；`Delta`、`StreamingText`、`LoginUrl`、`ProviderTest`、`Error` 只對當下的連線有意義，不記日誌。API key 只會從客戶端送到服務（`SaveProvider`、`TestProvider`），任何事件都不帶 key，只帶末四碼。登入網址可能帶 `id_token_hint`，所以日誌裡的 `ModelAuth { Pending }` 不存網址，客戶端開瀏覽器只依 `LoginUrl`。

- 記憶：快照裡每個 project 帶著 `MemoryView`；Observer、Reflector 或你的修改讓記憶變了就送一筆 `Memory` 整份替換。觀察的修改被拒絕時，除了 `Error` 也重送一次目前的 `Memory`，客戶端先改的內容會被蓋回去。

- 任務樹與 artifact：快照裡每個 project 帶著所有 Goal（含任務樹與 Brief 全文）與 artifact；任何變化都送一筆 `GoalChanged` 或 `ArtifactChanged` 整筆替換，客戶端不用自己拼狀態。Goal 的進展是 `GoalView::activity`（`goal_activity` 表），在 Goal 面板的「動態」分頁；只有 Goal 結束與快速修改需要你處理時，同一句話也以 `Card::Milestone` 貼到對話流。`SaveArtifact` 被拒絕（同一段兩邊都改過）時只回 `Error`，草稿留在客戶端；`Cancel { task }` 取消一個還沒結束的任務。

- 工作資料夾：快照的 `ProjectSnapshot` 帶 `workdir`；建立、設定或 agent 的 `set_workdir` 成功後送一筆 `ProjectWorkdir` 整份替換。自動化分頁的 `ProjectRepos` 只影響監看，不影響 worker 在哪工作。本機資料夾的核准是 `ApplyChanges`（路徑、分支、sha），核准後平台才線性套用（岔開時把任務 commit 接到你的分支後面）。

- 核准：快照帶著還沒決定與最近有變化的核准（`ApprovalView`：具體動作、hash、狀態、理由與結果），之後每次變化送一筆 `ApprovalChanged` 整筆替換；`Decide { approval, approve, note }` 核准或拒絕，已決定或逾時時回 `Error` 並重送目前的狀態。通知設定、USER.md 與 project 的 merge 方式也在快照裡，改了分別送 `Notifications`、`UserMd`、`ProjectMergeMethod`。

- 訊息狀態：人類訊息進入 agent 歷史時（被某一輪接手，或被「停止」清掉），伺服器記一筆 `ItemUpdated`，客戶端據此收掉「排隊中」。

- 斷線重連：客戶端帶 `last_seq` 重連，伺服器從事件日誌補送。`last_seq` 為 0、太舊（日誌已不含下一筆）或超過伺服器目前的序號時，改送 `Snapshot`，客戶端整份替換。

- 串流：`Delta` 不寫入事件日誌；`ItemDone` 帶最後的完整訊息，重播時不需要 `Delta`。重連時仍在串流中的訊息以 `StreamingText` 送出目前的全文，取代客戶端手上的內容。一輪接著工具呼叫時，那一輪的文字搬進 `Msg.narration`，以 `ItemUpdated` 送出清空後的內文；服務端的串流位置是送過的總長度，內文變短也不會讓之後的 `Delta` 被當成重複丟掉。

- 去重：`Send` 帶客戶端產生的訊息 ID，伺服器用它存這則訊息。客戶端在看到同 ID 的 `Item` 之前都保留這個指令，重連後再送一次；伺服器依 ID 去重。

- 歷史分頁：用 HTTP `GET /projects/:id/items?before=<id>` 以游標向上載入，搭配虛擬捲動。

