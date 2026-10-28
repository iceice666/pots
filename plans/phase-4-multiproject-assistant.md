# Phase 4：多專案與個人助理

## 1. 目標

幕僚長 agent 能跨專案派工，並**讀取**、彙整郵件、行事曆與學習平台（LMS）的日常資訊；首批 connector 以 Gmail、Google Calendar 與 Moodle 為參考實作；這些服務的寫入皆由使用者執行，agent 負責準備內容。
本階段同時加入 macOS node（`node-mac`）與行動版 client。

建議分為四個子階段：**4a 記憶與幕僚長 → 4b 行動版與 `node-mac` → 4c Connectors（唯讀）→ 4d 寫入交接 UI**。

## 2. 範圍

### 4a 多專案記憶與幕僚長

- `MemoryStore` 的 global / per_project 分區；在 B 頻道無法讀取 A 的分區
- `sqlite-vec` retrieval（舊訊息、文件）；記憶庫整檔加密
- 幕僚長（Ava）：預設對話對象、可跨專案、開設頻道、以 `InviteTarget::Role` 挑選或新建成員、彙整進度；本身不操作機器
- Capability：worker agent 預設不得再邀請 worker agent

### 4b 行動版與 `node-mac`

- 行動版 client（以 UniFFI 重用 core）：優先實作核准與交接畫面，以裝置金鑰簽章
- 推播：Agent Host 主動連線至 APNs / FCM，payload 僅含「有更新」，不帶內容；app 收到後經 overlay 網路向 host 拉取內容。無需 Notification Service Extension 解密
- 行動裝置常駐連上 overlay 網路（或 on-demand）；未連線時 app 顯示「無法連線至 host」，不影響已簽署的核准
- `node-mac`：Linux sandbox 使用 Apple `container` 或 OrbStack；macOS 工作（`cap=xcode`）使用 Tart VM，每個任務從 golden image clone；須設定為不休眠（例如 `pmset`）
- macOS 單機版（見架構文件 3.6 節）：與 `node-mac` 共用 sandbox driver；`_pots-host` / `_pots-node` role account + launchd daemon，桌面 client 以使用者帳號執行（裝置金鑰存於 Keychain）；Linux sandbox 使用 OrbStack（Apple `container` 不支援 USB passthrough），且以 `_pots-node` 執行，避免掛載使用者的 home；bench device 經 OrbStack 的 serial 轉發傳入 sandbox；以 nix-darwin module 部署

### 4c Connectors（唯讀）

- MCP（`rmcp`）於 Agent Host 上執行，`ConnectorManifest` 僅登記讀取型 tool，並依實際情況標記 `untrusted_output`
- Token 以與其他 secret 相同的方式僅解密至 Agent Host，且一律申請最小的唯讀 scope：例如 Gmail `gmail.readonly`、Calendar readonly；無法限制為唯讀的 token（例如部分 Moodle 部署）僅暴露讀取函式，屬軟體層限制，須記錄為例外
- Reader agent：讀取不可信內容，僅產出摘要；可下達 node 指令的 agent 僅取得摘要
- 不開放公開入口：優先使用服務提供的增量同步（例如 Gmail `history.list`、Calendar `syncToken`），由 Agent Host 定時輪詢；無增量 API 者以可設定的間隔輪詢（例如 LMS 每 30 分鐘）
- 若需更高即時性，再評估主動外連的推送方式（例如 Gmail Pub/Sub pull subscription）

### 4d 寫入交接（由使用者執行）

- Agent 產出：信件草稿文字、行程建議、作業檔案（程式碼交付已於 P2 以 `GitHubPublish` 實作，不屬於本節）
- Client 一鍵交接：複製草稿並開啟郵件撰寫頁、開啟行事曆新增頁（預先填入）、開啟 LMS 作業頁
- 行動版支援 `GitHubPublish` 的 diff 預覽與簽章，以及 `AutoModeGrant` 的檢視與撤銷
- 稽核紀錄「agent 準備了哪些內容、使用者是否採用」

### 自動核准（auto mode）

Auto mode 已於 P2 實作，僅限 `GitHubPublish`（見架構文件 6.8 節）。本階段不擴大其範圍：connector 維持唯讀，沒有可自動核准的寫入；node 跨邊界動作（含 `node-mac`）一律須裝置簽章。本階段僅於行動版加入 grant 的檢視與撤銷，並依 P2–P4 的統計結果調整 grant 的預設條件。

## 3. 前置待確認事項

- [ ] 若使用者選擇啟用 `require_zdr`，確認所選 provider 端點與帳號的 ZDR 資格；非一般使用的前置硬條件
- [ ] LMS connector 的可行性：以 Moodle 為例，機構是否啟用 Web Services、個人能否取得 token，以及 token 能否限制為唯讀；無法取得時 connector 應可停用，不影響其他功能
- [ ] 選定 LMS MCP 實作（例如 moodle-mcp）：直接採用或以 Rust 重寫
- [ ] macOS 單機版：OrbStack 能否於 role account 下以 launchd 執行；容器內存取轉發的 serial 裝置是否穩定（OrbStack 有已知問題）；secret 以 nix-darwin + sops 解密的方式

## 4. 里程碑

- [ ] M4.1 專案記憶分區 + retrieval + 記憶庫加密
- [ ] M4.2 幕僚長 agent
- [ ] M4.3 行動版 client + 推播（空通知）+ 手機核准
- [ ] M4.4 `node-mac`（Linux sandbox + macOS / Xcode Tart VM）
- [ ] M4.5 MCP host + 唯讀 manifest
- [ ] M4.6 郵件 connector（唯讀；參考實作 Gmail）
- [ ] M4.7 行事曆 connector（唯讀；參考實作 Google Calendar）
- [ ] M4.8 LMS connector（僅暴露讀取函式；參考實作 Moodle）
- [ ] M4.9 Reader agent 與讀寫分離
- [ ] M4.10 Client 寫入交接（草稿、行程、作業）+ 行動版 `GitHubPublish` 核准與 grant 撤銷
- [ ] M4.11 macOS 單機版（nix-darwin module、role account 分離、OrbStack sandbox、bench device serial 轉發；依賴 M4.4）

## 5. 關卡

**路線圖關卡**

- 幕僚長能處理郵件、行事曆與 LMS 的日常工作；其寫入皆由使用者執行，系統內除 GitHub 寫入 token 外不存在寫入憑證

**補充驗收測試**

- 於 B 專案頻道詢問 A 專案的內容，非幕僚長 agent 無法取得
- Prompt injection 測試集（惡意信件、行事曆描述、LMS 公告）：未造成任何外部寫入（包括未經簽章且不在 auto mode grant 範圍內的 GitHub push / 開 PR）、未將資料送往任務範圍以外、未提升 agent 權限
- 檢查 Agent Host 上所有 token 的實際 scope，確認皆僅具讀取權限（GitHub 寫入 token、無法限制為唯讀的 LMS token 與 APNs / FCM 的例外須記錄；GitHub token 須確認僅限指定 repo、不含 Workflows / Administration、尚未過期）
- 擷取推播 payload，確認不含訊息內容、預覽、agent 或頻道名稱
- macOS 單機版通過 Phase 2 的單機版與 bench device 驗收測試（身分分離、未登記裝置不可見、upload 與 serial 可用）；容器內看不到使用者的 home
- 統計 P2–P4 期間每日的使用者簽章次數、自動核准次數與退回人工的比例，作為調整 grant 預設條件的依據
