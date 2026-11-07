# 里程碑與設計決策

[← 產品提案索引](product.md)

## MVP 里程碑

M0 到 M6 全部在單機版上完成，M7 才搬成自架平台。依序建置，每個里程碑結束時都是可以自己天天用的版本。M4 結束時應該能跑通「一句話 → PR → CI → 自動修 → 請求核准 merge」。

1. M0 骨架：axum + sqlx + WebSocket，`projects` 與 `stream_items` ，GPUI 桌面客戶端的對話頁。對話 agent 用統一的 run_agent 與 chat profile 串流回覆，先用完整原始歷史，不做記憶。

2. M1 Observational Memory：`ConversationMemory` 實作、背景 Observer 與 Reflector、記憶面板。驗收：對話累積超過 100k tokens 後，agent 仍能正確回答早期做過的決定。

3. M2 任務樹：`goals` 、`tasks` 、`task_events` 、`agent_runs` 佇列；`create_goal` 、 `create_task` 、`get_status` 工具；Planner 與 Goal brief 交接；Artifact（Spec、 Brief、Ledger、Report）與編輯面板；狀態面板與 Goal 卡片。

4. M3 Watcher 與等待：connector 介面與事件管線，GitHub 為第一個 connector （webhook 與輪詢備援）、`subscriptions` 、`wait_for` 、喚醒與 sweeper；自我排程與 listener（單機版以輪詢實作）。

5. M4 Coding worker：先接 Claude Code 或 Codex CLI（rig worker 為備援），在容器與 worktree 裡改 code、跑測試、開 PR；CI 失敗自動修，最多 3 次；quick_fix 快速通道、 subagent、team 貼文。

6. M5 核准與通知：`approvals` 、平台執行 merge、Inbox、桌面通知、靜音時段。

7. M6 預算與觀測：階層預算、熔斷、trace、「為什麼」面板、「你不在期間」摘要卡片、總管 agent；其他 connector：GitLab、郵件、狀態頁、HTTP 探測、通用 webhook、 RSS。

M7 自架化：補上 Postgres 後端與 `PgQueue` 、passkey 或 forward-auth、GitHub App webhook、手機客戶端與推播（形式待定）、docker compose，再用 `export` 與 `import` 把單機資料搬過去。

M5 之前的 merge 一律由你在 GitHub 上手動完成，所以即使核准流程還沒做，前面的里程碑也是安全的。

## 已決定事項

Coding worker 用內建的 rig agent，還是包一個現成的 coding agent CLI？後者能力較強，但控制與計費較難整合。→ 決定：支援 Claude Code 與 Codex CLI（訂閱登入），rig worker 作為備援，見 [Worker adapter](agent-runtime.md#worker-adapter內建-rig-與外部-coding-cli)。 Project agent 要兼任 orchestrator，還是拆成「對話者」與「規劃者」兩個 agent？合一比較簡單，拆開則讓對話不被長時間規劃卡住。→ 決定：拆分為對話 agent 與 Planner，以 Goal brief 交接。 Observer 與 Reflector 用哪個模型？需要實測壓縮品質與每天成本。→ 決定：Sonnet、 Luna 或 Flash 級別。跨 project 的個人偏好（例如 commit 風格）放在哪裡？可能需要一個「使用者層級」的記憶區塊。→ 決定：USER.md，agent 只能提案修改。 Worker 沙箱用 Docker 還是更輕量的方案（例如 bubblewrap）？→ 決定： Docker/Podman。 v1 之後是否支援多人？會影響核准者設定與通知路由。→ 決定：v1 不做，但預留擴充：所有紀錄帶 user_id、身分用 Principal::Human(UserId)、approvals 帶核准者欄位、通知路由查表而不寫死。 前端用什麼框架？原規劃 Leptos PWA，必要時用 Tauri 包成桌面 App；但 Tauri 在 Linux 上體驗不佳，而 Artifact 需要所見即所得編輯。Slint 搭配 sred 編輯器的原型因中文輸入法問題未通過（sred 把文件繪成圖片，Slint 端接不到組字事件）。→ 決定：桌面客戶端用 GPUI（GPUI Kit），Artifact 用 gpui-kit-markdown-editor；原型在 [`prototypes/gpui-editor/`](../prototypes/gpui-editor/)，已在 macOS 實測中文輸入法，見[前端](ux-and-frontend.md#前端)。

仍待確認：

- Claude Code 與 Codex CLI 的訂閱條款是否允許這種自動化用法，需各自確認。

- GPUI 客戶端在 Linux 與 Windows 上的中文輸入、字型與縮放尚未實測；Linux 是替換 Tauri 的主要理由，應優先驗證。

- 手機與 Web 客戶端的形式：Android 可用 [gpui-mobile](https://github.com/itsbalamurali/gpui-mobile) 共用 GPUI 介面，但仍在實驗階段；另一個方案是輕量 Web 核准頁（PWA 加 Web Push）。「核准必須能在手機上完成」的目標不變，推播方式隨此決定。

- GPUI 仍是預發布版，編輯器為 0.1.x，兩者都要釘精確版本並追蹤 API 變動。

