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

agent 忙碌時插話。 輸入框旁有兩個動作：預設「排隊」，等本輪結束再處理；「改方向」則發出 steer 事件，中斷目前的執行並讓 agent 重新規劃。對話頂端顯示 agent 狀態：閒置、思考中、等待 CI、等你核准。

記憶透明化。 提供「agent 記得什麼」面板，可以查看、修正、釘選或刪除觀察。修正會讓快取前綴失效一次，可以接受。

完整歷史永遠可捲。 UI 顯示的是原始訊息，OM 只是 agent 看到的衍生視圖，兩者分開存。

## 參考 UI

專案內的 [`ui/`](../ui/) 提供靜態 UI 設計稿，作為畫面配置、資訊層級與視覺呈現的參考。可從[畫面總覽](../ui/index.html)瀏覽，預覽方式與檔案說明見 [`ui/README.md`](../ui/README.md)。

| 畫面 | 參考稿 |
| --- | --- |
| Project 主對話與 Goal thread | [頻道畫面](../ui/channel.html) |
| Worker 任務與執行細節 | [任務 thread](../ui/task.html) |
| Artifact 文件編輯 | [Artifact 編輯](../ui/artifact.html) |
| 全域待核准事項 | [桌面 Inbox](../ui/inbox.html)、[手機 Inbox](../ui/mobile-inbox.html) |
| 跨 project 的總管對話 | [總管畫面](../ui/overseer.html) |
| 排程與 listener 管理 | [自動化管理](../ui/automations.html) |

這些畫面僅供設計參考，不代表功能已實作，也不取代本提案的行為、資料與權限規格。現有設計稿採固定畫布，手機稿亦非完整的響應式應用；實作仍須依下列桌面／手機配置調整。

## 前端

桌面客戶端用 Rust 的 [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui)，元件庫用 [GPUI Kit](https://github.com/longbridge/gpui-kit)，是原生程式而非 WebView。客戶端只透過下方的[即時協定](#即時協定)與 HTTP API 和服務溝通：單機版連 localhost，自架版連伺服器網址。

Artifact 編輯器用 [gpui-kit-markdown-editor](https://docs.rs/crate/gpui-kit-markdown-editor/latest)：區塊式所見即所得，直接以 Markdown 讀寫，支援標題、巢狀清單、待辦、表格、程式碼區塊、跨區塊選取與輸入法。評估用的原型在 [`prototypes/gpui-editor/`](../prototypes/gpui-editor/)，已在 macOS 實測中文輸入法可用；選型過程見[已決定事項](roadmap-and-decisions.md#已決定事項)。

手機與 Web 客戶端的形式尚未決定（見[待確認事項](roadmap-and-decisions.md#已決定事項)）。下表的手機欄位是配置目標，不代表已選定實作方式。

### 介面配置

| 區塊 | 桌面 | 手機 | 內容 |
| --- | --- | --- | --- |
| Project 列表 | 左欄 | 底部分頁「Projects」 | 每個 project 一列，待核准數與未讀徽章 |
| 對話流 | 中欄 | 點進 project | 連續對話 + 內嵌卡片，頂端顯示 agent 狀態 |

| 區塊 | 桌面 | 手機 | 內容 |
| --- | --- | --- | --- |
| 狀態面板 | 右欄 | 對話頁的上拉抽屜 | 進行中的 Goal、任務樹、關聯 PR 與 CI 結果 |
| Inbox | 頂部圖示 | 底部分頁「Inbox」 | 所有 project 的待核准，依時間排序 |
| 記憶面板 | 狀態面板的分頁 | 同左 | 觀察紀錄，可編輯、釘選、刪除 |
| Artifact 面板 | 右欄（取代狀態面板） | 全螢幕頁 | Markdown 編輯器、版本與 diff、 agent 正在寫入的提示 |
| 總管 | 快捷鍵叫出的浮動對話框 | 底部分頁「總管」 | 跨 project 的永久對話 |
| 自動化 | 狀態面板的分頁 | 同左 | 排程與 listener 列表，可暫停、刪除、看觸發紀錄 |
| 電腦 | 狀態面板的分頁 | 同左 | 託管電腦的連線狀態、lease 持有者、即時縮圖、權限檢查；「接手」、「交還」與「停止」 |

### 即時協定

每個客戶端一條 WebSocket，伺服器推送帶序號的事件：

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

