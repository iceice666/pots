# 里程碑與設計決策

[← 產品提案索引](product.md)

## MVP 里程碑

M0 到 M7 全部在單機版上完成，M8 才搬成自架平台。依序建置，每個里程碑結束時都是可以自己天天用的版本。M4 結束時應該能跑通「一句話 → PR → CI → 自動修 → 請求核准 merge」。

1. M0 骨架：axum + sqlx + WebSocket，`projects` 與 `stream_items` ，GPUI 桌面客戶端的對話頁。對話 agent 用統一的 run_agent 與 chat profile 串流回覆，先用完整原始歷史，不做記憶。接 ChatGPT Plan provider：官方 SIWC 授權、Responses 串流與 rig 相容層；驗收工具呼叫／結果往返、多輪歷史、重啟與並行 refresh、撤銷及額度不足，API fallback 必須事先授權。

2. M1 Observational Memory：`ConversationMemory` 實作、背景 Observer 與 Reflector、記憶面板。驗收：對話累積超過 100k tokens 後，agent 仍能正確回答早期做過的決定。

3. M2 任務樹：`goals` 、`tasks` 、`task_events` 、`agent_runs` 佇列；`create_goal` 、 `create_task` 、`get_status` 工具；Planner 與 Goal brief 交接；Artifact（Spec、 Brief、Ledger、Report）與編輯面板；狀態面板與 Goal 卡片。

4. M3 Watcher 與等待：connector 介面與事件管線，GitHub 為第一個 connector （webhook 與輪詢備援）、`subscriptions` 、`wait_for` 、喚醒與 sweeper；自我排程與 listener（單機版以輪詢實作）。

5. M4 Coding worker：接 Claude Code adapter（rig worker 為備援），原版 CLI 由使用者官方登入，在容器與 worktree 裡改 code、跑測試、開 PR；MCP 回報與平台端授權、串流事件、中斷、重建容器後續接為驗收項目。session ID 與官方 session 資料一併持久化；不做 Codex adapter。CI 失敗自動修，最多 3 次；quick_fix 快速通道、subagent、team 貼文。

6. M5 核准與通知：`approvals` 、平台執行 merge、Inbox、桌面通知、靜音時段。

7. M6 預算與觀測：階層預算、熔斷、trace、「為什麼」面板、「你不在期間」摘要卡片、總管 agent；其他 connector：GitLab、郵件、狀態頁、HTTP 探測、通用 webhook、 RSS。

8. M7 託管電腦：`pots-node` daemon、配對、Node Hub 與心跳、Node connector；先做 coding（Container 與主機模式），再做 operator profile、display lease、`screen_observe`／`ui_act`、截圖 trace、人類接手與緊急停止，最後做 `gui_checkpoint`。macOS 佈建腳本與權限檢查一起完成。驗收：在一台重灌過的 Mac 上照 setup 完成佈建；agent 操作一個桌面 app 完成任務，中途卡住時推播並由你接手後交還；node 斷線、重開機、權限被撤銷時都能正確回報。依賴 M4（worker）與 M5（核准）。詳見[託管電腦](computer-nodes.md)。

M8 自架化：補上 Postgres 後端與 `PgQueue` 、passkey 或 forward-auth、GitHub App webhook、手機客戶端與推播（形式待定）、docker compose，再用 `export` 與 `import` 把單機資料搬過去（不含憑證）。驗收 ChatGPT 自架 host ID、受保護授權移轉與單一 refresh owner，以及 Claude Code 專屬執行環境中的官方登入與續接。

M5 之前的 merge 一律由你在 GitHub 上手動完成，所以即使核准流程還沒做，前面的里程碑也是安全的。

## 已決定事項

產品發行與部署。→ 決定：開源，由使用者在本機或自行架設執行；不以平台代管或轉售訂閱用量為前提。

操作電腦與瀏覽器。→ 決定：把閒置的電腦當作 agent 的「身體」（託管電腦／Node），Pots 服務不跑在上面，由 `pots-node` daemon 主動連回 Pots。v1 只支援一台 macOS 專用機，可以清空、不登入個人帳號；用途是操作 GUI 桌面應用程式與 coding，不包含管理電腦本身。見[託管電腦](computer-nodes.md)。

帳號整合的兩個層級。→ 決定：Claude Code adapter 是外部 worker runtime，執行原版 CLI、使用者走官方登入；ChatGPT Plan provider 是自有 rig loop 的推論後端，透過 SIWC 授權直接呼叫 Responses API。兩者分開配置，不接管 Claude 訂閱 token，不依賴 Codex CLI／app-server。rig worker 作為備援，見 [Worker adapter](agent-runtime.md#worker-adapter內建-rig-與外部-coding-cli)。付費 API fallback 須使用者預先授權與預算上限。
 Project agent 要兼任 orchestrator，還是拆成「對話者」與「規劃者」兩個 agent？合一比較簡單，拆開則讓對話不被長時間規劃卡住。→ 決定：拆分為對話 agent 與 Planner，以 Goal brief 交接。 Observer 與 Reflector 用哪個模型？需要實測壓縮品質與每天成本。→ 決定：選品質與成本合適的模型並實測；以 ChatGPT Plan 可用模型為優先候選，其他 API 模型需另有授權與預算，不把 Claude Code 訂閱當作 Observer／Reflector 的 provider。跨 project 的個人偏好（例如 commit 風格）放在哪裡？可能需要一個「使用者層級」的記憶區塊。→ 決定：USER.md，agent 只能提案修改。 Worker 沙箱用 Docker 還是更輕量的方案（例如 bubblewrap）？→ 決定： Docker/Podman。 v1 之後是否支援多人？會影響核准者設定與通知路由。→ 決定：v1 不做，但預留擴充：所有紀錄帶 user_id、身分用 Principal::Human(UserId)、approvals 帶核准者欄位、通知路由查表而不寫死。 前端用什麼框架？原規劃 Leptos PWA，必要時用 Tauri 包成桌面 App；但 Tauri 在 Linux 上體驗不佳，而 Artifact 需要所見即所得編輯。Slint 搭配 sred 編輯器的原型因中文輸入法問題未通過（sred 把文件繪成圖片，Slint 端接不到組字事件）。→ 決定：桌面客戶端用 GPUI（GPUI Kit），Artifact 用 gpui-kit-markdown-editor；原型在 [`prototypes/gpui-editor/`](../prototypes/gpui-editor/)，已在 macOS 實測中文輸入法，見[前端](ux-and-frontend.md#前端)。

仍待確認：

- ChatGPT Plan：官方開源／本機與自架 VM 路線已核對，但 preview 的資格、模型權限、工具格式、參數與 rig 相容性仍須實測；不把登入成功當作推論權限，也不把訂閱當無限背景用量。

- Claude Code：依官方原版 binary 與使用者自行登入的邊界設計；平台承載須遵守適用商業條款。容器登入、憑證更新、非互動 hooks／MCP 安全、中斷與 session 持久化仍待原型驗證。正式發布前重查[官方文件與條款](references.md#帳號與模型整合)。

- GPUI 客戶端在 Linux 與 Windows 上的中文輸入、字型與縮放尚未實測；Linux 是替換 Tauri 的主要理由，應優先驗證。

- 手機與 Web 客戶端的形式：Android 可用 [gpui-mobile](https://github.com/itsbalamurali/gpui-mobile) 共用 GPUI 介面，但仍在實驗階段；另一個方案是輕量 Web 核准頁（PWA 加 Web Push）。「核准必須能在手機上完成」的目標不變，推播方式隨此決定。

- GPUI 仍是預發布版，編輯器為 0.1.x，兩者都要釘精確版本並追蹤 API 變動。

- 託管電腦：macOS 螢幕錄製權限的定期重新授權與 `forceBypassScreenCaptureAlert` 的安裝方式、ChatGPT Plan provider 的影像輸入支援、AX 樹在常見 app 的覆蓋率、node 上的容器效能，見[託管電腦：待驗證](computer-nodes.md#待驗證)。

