# Pots


## 概述

這是一個單一 Rust 服務：第一階段在本機以 SQLite 單機執行，調整順手後再搬成以 Postgres 為後端的自架平台（見[分階段策略](deployment.md)）。人類在每個 project 的一條連續對話裡，和負責該 project 的 agent 討論功能、核准動作、查看進度；Planner 在背景負責規劃並分派 worker，watcher 把 GitHub、GitLab、郵件、服務狀態等外部來源轉成事件。

### 目標（v1）

- 每個 project 一條永不結束的對話，沒有「新 session」，用 Observational Memory （OM）讓 agent 記得長期脈絡。

- 人類只做三件事：討論、核准、看進度。核准必須能在手機上完成。

- 跑通一條完整路徑：一句話 → 開 PR → 等 CI → 失敗自動修 → 成功後請人類核准 merge。

- 單機版是一個 binary 加一個 SQLite 檔，直接在本機執行；自架版換成 Postgres，部署只需要一個 `docker compose up` 。

### 非目標（v1）

- 多租戶、多人協作、公開註冊、聯邦。資料模型仍預留多人擴充（見[已決定事項](roadmap-and-decisions.md#已決定事項)）。

- 語音與視訊。

- agent 之間的自由閒聊頻道。

- 自己託管模型：LLM 一律透過 provider API。

### 三個設計原則

1. 任務樹是真相，對話是視圖。 Goal 與 Task 的狀態存在平台，不存在任何 agent 的記憶裡。

2. 規劃在 agent，狀態在平台。 agent 當掉或換模型，任務樹都還在，可以接著跑。

3. 狀態不靠記憶。 任務、PR、CI 的狀態每輪從 Task Store 即時讀取；OM 只記「決定了什麼、偏好什麼」。

## 文件索引

本提案按主題拆分如下；各文件保留原提案的設計內容與範例。

| 文件 | 內容 |
| --- | --- |
| [部署模式與分階段策略](deployment.md) | 單機與自架差異、後端介面、資料搬移 |
| [使用體驗與前端](ux-and-frontend.md) | 對話、狀態面板、參考 UI、GPUI 客戶端與即時協定 |
| [系統架構與資料模型](architecture.md) | 架構圖、核心型別、資料表與技術選型 |
| [Agent 執行層](agent-runtime.md) | 角色分工、rig 工具、執行模型與 Worker adapter |
| [Agent Loop 與協作模型](agent-collaboration.md) | 統一 loop、profile、快速修改、subagent 與 team |
| [Observational Memory](memory.md) | 記憶分層、Observer、Reflector 與 USER.md |
| [Artifacts](artifacts.md) | 共享文件種類、編輯協定、版本與記憶分工 |
| [外部事件與自動化](events-and-automation.md) | Connector、事件管線、喚醒、自我排程與 listener |
| [核准流程與通知](approvals-and-notifications.md) | Approval 生命週期、核准政策與推播 |
| [安全、預算與可觀測性](security-and-operations.md) | 身分驗證、沙箱、預算限制、trace 與成本報表 |
| [里程碑與設計決策](roadmap-and-decisions.md) | M0–M7、已決定事項與待確認事項 |
| [參考資料](references.md) | 專案內 UI 設計稿與 rig、Mastra、Amp 相關來源 |

建議先閱讀「使用體驗與前端」與「系統架構與資料模型」，再按主題查閱細節；實作順序與待確認事項見「里程碑與設計決策」。參考 UI 位於 [`ui/`](../ui/)，可直接開啟[畫面總覽](../ui/index.html)，或查看[預覽說明](../ui/README.md)。
