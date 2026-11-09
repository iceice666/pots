# Pots


## 概述

Pots 是開源、由使用者在本機或自行管理的伺服器執行的單一 Rust 服務，不提供平台代管或訂閱額度轉售。有兩種長期並存的形態：桌面版是內嵌服務的桌面 app；多設備版自架在常開的機器上。同一個客戶端可以同時連兩者，每個 project 歸屬其中一個實例（見[部署形態](deployment.md)）。人類在每個 project 的一條連續對話裡，和負責該 project 的 agent 討論功能、核准動作、查看進度；Planner 在背景負責規劃並分派 worker，watcher 把 GitHub、GitLab、郵件、服務狀態等外部來源轉成事件。你也可以把閒置的電腦交給 Pots 當成[託管電腦（Node）](computer-nodes.md)，讓 agent 在上面操作桌面應用程式與寫程式。

### 目標（v1）

- 每個 project 一條永不結束的對話，沒有「新 session」，用 Observational Memory （OM）讓 agent 記得長期脈絡。

- 人類只做三件事：討論、核准、看進度。核准必須能在手機上完成。

- 跑通一條完整路徑：一句話 → 開 PR → 等 CI → 失敗自動修 → 成功後請人類核准 merge。

- 桌面版是一個 app 加一個 SQLite 檔，安裝後即可使用；多設備版是同一份程式碼的 `pots-server`，可以用單一 binary 加 SQLite，或用 Postgres 搭配 `docker compose up` 啟動。這是程式部署方式，不代表模型立即可用：仍須完成所選 provider 的授權；Claude Code adapter 須由使用者透過官方 CLI 登入，ChatGPT Plan provider 須完成官方 Sign in with ChatGPT（SIWC）授權。

### 非目標（v1）

- 多租戶、多人協作、公開註冊、聯邦，以及實例之間的資料同步（並存靠 project 歸屬與移交）。資料模型仍預留多人擴充（見[已決定事項](roadmap-and-decisions.md#已決定事項)）。

- 語音與視訊。

- agent 之間的自由閒聊頻道。

- 自己託管模型：內建 agent（pots-llm 的 loop）透過 provider API 呼叫模型，另可用 Claude Code runtime adapter 執行官方 CLI。ChatGPT Plan 是內建 agent 的 provider，以官方 SIWC token-sharing 路徑直接呼叫 Responses API，不是 Codex runtime。

- Codex runtime／CLI 整合，以及平台代管模型訂閱、共用訂閱憑證或轉售使用者的訂閱額度。

### 三個設計原則

1. 任務樹是真相，對話是視圖。 Goal 與 Task 的狀態存在平台，不存在任何 agent 的記憶裡。

2. 規劃在 agent，狀態在平台。 agent 當掉或換模型，任務樹都還在，可以接著跑。

3. 狀態不靠記憶。 任務、PR、CI 的狀態每輪從 Task Store 即時讀取；OM 只記「決定了什麼、偏好什麼」。

## 文件索引

本提案按主題拆分如下；各文件保留原提案的設計內容與範例。

| 文件 | 內容 |
| --- | --- |
| [Onboarding 與工作資料夾](onboarding.md) | 新增專案精靈、工作資料夾與監看來源、本機資料夾的套用 |
| [部署形態與多實例並存](deployment.md) | 桌面版與多設備版、後端設定、客戶端多連線、project 移交 |
| [使用體驗與前端](ux-and-frontend.md) | 對話、狀態面板、介面配置、GPUI 客戶端與即時協定 |
| [系統架構與資料模型](architecture.md) | 架構圖、核心型別、資料表與技術選型 |
| [Agent 執行層](agent-runtime.md) | 角色分工、pots-llm 模型層與工具、執行模型與 Worker adapter |
| [Agent Loop 與協作模型](agent-collaboration.md) | 統一 loop、profile、快速修改、subagent 與 team |
| [Observational Memory](memory.md) | 記憶分層、Observer、Reflector 與 USER.md |
| [Artifacts](artifacts.md) | 共享文件種類、編輯協定、版本與記憶分工 |
| [託管電腦（Node）](computer-nodes.md) | 閒置電腦作為 agent 的身體：computer use、coding、display lease、人類接手、macOS 佈建 |
| [外部事件與自動化](events-and-automation.md) | Connector、事件管線、喚醒、自我排程與 listener |
| [核准流程與通知](approvals-and-notifications.md) | Approval 生命週期、核准政策與推播 |
| [安全、預算與可觀測性](security-and-operations.md) | 身分驗證、沙箱、預算限制、trace 與成本報表 |
| [里程碑與設計決策](roadmap-and-decisions.md) | M0–M9、已決定事項與待確認事項 |
| [參考資料](references.md) | GPUI、Mastra、Amp、官方 provider 授權文件 |

建議先閱讀「使用體驗與前端」與「系統架構與資料模型」，再按主題查閱細節；實作順序與待確認事項見「里程碑與設計決策」。桌面客戶端的畫面見 [`crates/pots-desktop/`](../crates/pots-desktop/)。
