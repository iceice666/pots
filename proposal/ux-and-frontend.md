# 使用體驗與前端

[← 產品提案索引](product.md)

## 使用情境與 UX 原則

三種使用情境各有一個入口，但都掛在同一條 project 對話旁邊，不切換「模式」。

| 情境 | 入口 | 資料來源 | 行為 |
| --- | --- | --- | --- |
| 和 agent 討論功能 | 主對話流（每個 project 一條） | OM 記憶 + 最近原始訊息 + 即時狀態 | 討論收斂時 agent 開一張 Goal 卡片 |
| 核准 | 對話中的內嵌卡片、全域 Inbox、手機推播 | Task Store | 一鍵核准或拒絕，可附一句理由 |
| 看進度 | 側邊狀態面板（任務樹、 PR、CI） | Task Store 即時查詢 | 對話裡只出現里程碑卡片 |

沒有 session，但工作有結構。 主對話永遠是同一條。當討論收斂成具體工作，agent 開一張 Goal 卡片，後續細節掛在卡片的 thread 下，主流不被洗版。

進度不灌進對話。 對話裡只放里程碑：Goal 已建立、PR 已開、需要核准、Goal 完成或失敗。細節在狀態面板。

離開後回來。 對話流底部插入一張「你不在期間」摘要卡片，內容由事件與任務狀態產生，不依賴 OM。閾值：距上次已讀超過 2 小時且期間有里程碑。

agent 忙碌時插話。 輸入框旁有兩個動作：預設「排隊」，等本輪結束再處理；「改方向」則發出 steer 事件，中斷目前的執行並讓 agent 重新規劃。對話頂端顯示 agent 狀態：閒置、思考中、規劃中、等待 CI、等你核准；agent 在跑的時候旁邊有「停止」。

記憶透明化。 提供「agent 記得什麼」面板，可以查看、修正、釘選或刪除觀察。修正會讓快取前綴失效一次，可以接受。

完整歷史永遠可捲。 UI 顯示的是原始訊息，OM 只是 agent 看到的衍生視圖，兩者分開存。

## 桌面客戶端現況

桌面畫面以 [`crates/pots-desktop/`](../crates/pots-desktop/) 為準。它目前用假資料、還沒接上服務，已有實例直列、頻道、Goal 與任務面板、Artifact 編輯、Inbox、總管與自動化；各畫面可以做什麼、哪些按鈕還沒實作，見該目錄的 README。用 `cargo test -p pots-desktop --test screenshots` 可以輸出主要畫面的截圖。

下方[介面配置](#介面配置)的桌面欄位與客戶端一致；還沒實作的區塊（記憶、電腦、project 移交）是預定的位置。手機欄位只是配置目標。

## 前端

桌面客戶端用 Rust 的 [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui)，元件庫用 [GPUI Kit](https://github.com/longbridge/gpui-kit)，是原生程式而非 WebView。客戶端只透過下方的[即時協定](#即時協定)與 HTTP API 和服務溝通。桌面版的 app 自己內嵌一個服務並經 loopback 連線，另外也可以同時連上任意數量的遠端實例，連線管理見[客戶端多連線](deployment.md#客戶端多連線)。

Artifact 編輯器用 [gpui-kit-markdown-editor](https://docs.rs/crate/gpui-kit-markdown-editor/latest)：區塊式所見即所得，直接以 Markdown 讀寫，支援標題、巢狀清單、待辦、表格、程式碼區塊、跨區塊選取與輸入法。評估用的原型在 [`prototypes/gpui-editor/`](../prototypes/gpui-editor/)，已在 macOS 實測中文輸入法可用；選型過程見[已決定事項](roadmap-and-decisions.md#已決定事項)。

手機與 Web 客戶端的形式尚未決定（見[待確認事項](roadmap-and-decisions.md#已決定事項)）。下表的手機欄位是配置目標，不代表已選定實作方式。

### 介面配置

| 區塊 | 桌面 | 手機 | 內容 |
| --- | --- | --- | --- |
| 實例直列 | 最左側窄欄，只連一個實例時隱藏 | Projects 分頁頂端的切換器，只連一個實例時隱藏 | 全域 Inbox、各實例圖示（待處理徽章、離線警示）、加入實例 |
| 左欄 | 目前實例 | 底部分頁「Projects」 | 頂端是實例名稱與形態，點下去是實例選單；接著是總管、這個實例的 Inbox、專案列表、自動化（排程、Listener）與電腦。每個 project 一列，標示待核准數、「需要你」或未讀；已移交的 project 顯示新位置 |
| 對話流 | 中欄，project 頻道的「對話」分頁 | 點進 project | 連續對話 + 內嵌卡片，頂端顯示 agent 狀態、本週預算與「停止」 |

| 區塊 | 桌面 | 手機 | 內容 |
| --- | --- | --- | --- |
| 狀態面板 | 右欄 | 對話頁的上拉抽屜 | Goal 面板（任務依狀態分組、Brief、Ledger、Team 分頁，留言給 Planner）；點任務進任務 thread（狀態、分支、PR 與 CI、進度、直接對 worker 說話） |
| Inbox | 實例直列最上方（全域）；左欄裡的 Inbox 只看目前實例 | 底部分頁「Inbox」 | 所有已連線實例、所有 project 的待處理事項，依時間排序並標示所屬實例，可依實例與類型（核准、需要你處理、找你的訊息）篩選 |
| 記憶 | project 頻道的「記憶」分頁 | 同左 | 觀察紀錄，可編輯、釘選、刪除 |
| Artifact | 列表是 project 頻道的「Artifacts」分頁；編輯器在右欄，取代狀態面板 | 全螢幕頁 | Markdown 編輯器，以 `##` 區段標示你改過的區段與 agent 正在寫入的區段；版本紀錄與 diff |
| 總管 | 左欄入口的獨立畫面，和對話流一樣佔中欄，右欄是專案概況與記憶；任何畫面都可用快捷鍵切過去 | 底部分頁「總管」 | 同一個實例內跨 project 的永久對話，依目前選取的實例切換 |
| 自動化 | 左欄「排程」「Listener」進入的實例層級畫面，佔中欄，可依專案篩選；project 頻道的「自動化」分頁只看這個 project。點一列在右欄看細節 | 同左 | 排程與 listener 列表、批次暫停狀態；可停用、刪除、看條件與觸發紀錄，待核准的 listener 可以直接核准 |
| 電腦 | 左欄入口的實例層級畫面，佔中欄 | 同左 | 託管電腦的連線狀態、lease 持有者、即時縮圖、權限檢查；「接手」、「交還」與「停止」 |

### 即時協定

客戶端對每個實例各開一條 WebSocket，伺服器推送帶序號的事件。序號是每個實例各自的，客戶端為每條連線分別記錄 `last_seq`：

```rust
#[derive(Serialize)]
#[serde(tag = "t")]
pub enum ServerEvent {
    Item        { seq: u64, item: StreamItemView },
    Delta       { seq: u64, item_id: ItemId, text: String },   // agent 串流
    ItemDone    { seq: u64, item_id: ItemId, status: MsgStatus },
    AgentState  { seq: u64, project: ProjectId, state: AgentState },
    TaskChanged { seq: u64, task: TaskView },
    Inbox       { seq: u64, pending: u32 },
}
#[derive(Deserialize)]
#[serde(tag = "t")]
pub enum ClientCommand {
    Resume  { last_seq: u64 },
    Send    { project: ProjectId, text: String, mode: SendMode }, // Queue | Steer
    Decide  { approval: ApprovalId, approve: bool, note: Option<String> },
    Cancel  { task: TaskId },
}
```

- 斷線重連：客戶端帶 `last_seq` 重連，伺服器從事件日誌補送。差距太大時改為重拉快照。

- 串流：`Delta` 不寫入資料庫的事件日誌，只寫最後的 `ItemDone` 。重連時串流中的訊息以目前的完整文字重送。

- 歷史分頁：用 HTTP `GET /projects/:id/items?before=<id>` 以游標向上載入，搭配虛擬捲動。

