# 安全、預算與可觀測性

[← 產品提案索引](product.md)

## 安全、預算與可觀測性

每則進入 agent 的外部文字都當作不可信輸入，每個 Goal 都有硬性預算，每次 agent 執行都有完整 trace。

### 安全

- 身分驗證：桌面版只綁 loopback，免登入，但客戶端必須帶啟動時產生的本機 token（檔案權限 `0600`），服務也會檢查 `Host` 與 `Origin`，擋下瀏覽器跨站請求與 DNS rebinding。只要接受非本機連線就一定要登入，私有網路也一樣；唯一例外是 node 專用 listener，它只接受已配對 node 的簽章。v1 多設備版為單人使用，用 passkey （WebAuthn）登入，不存密碼；原生客戶端先在瀏覽器登入，再換得可撤銷的裝置 token（伺服器只存 hash）。也可以關閉內建登入，改接反向代理的 forward-auth （Authelia、Authentik 等）：代理驗證後以 Remote-User 等標頭傳入身分，服務只信任來自代理的請求（只綁內網位址或驗證代理來源），WebSocket 升級請求同樣經過 forward-auth。GitHub webhook 路徑須在代理設定為略過，改由 HMAC 驗簽保護。

- 憑證隔離：API key、ChatGPT SIWC token 與 GitHub App 私鑰只在受保護的 keychain／secret store 中（SIWC 憑證與設定視窗輸入的 API key 目前都是資料目錄裡 `0600` 的檔案，原子寫入；客戶端只看得到 key 的末四碼）；API key 可由環境變數提供。秘密不進 prompt、log、trace、任務資料、一般 export 或 project 移交包；每個實例各自持有自己的憑證，不跨實例共用。worker 拿到的 GitHub token 只限該任務的 repo，且有效期短。Claude Code 登入狀態由官方 CLI 管理，不由 pots 匯入或作為內建 agent 的 provider 憑證。

- ChatGPT 授權：這是模型用量授權，不是 pots 的 passkey／forward-auth 登入。驗證 OAuth／PKCE、state、OIDC nonce、ID token 與 granted scopes；帳號／workspace 註冊分開儲存，未完成驗證不得覆蓋目前帳號。每個註冊由單一 refresh owner 協調更新，access token、輪替的 refresh token 與期限原子寫入；檔案採 owner-only 權限（Unix `0600`），多 runner 也必須共用更新鎖或專責程序。撤銷／登出後停止使用該連線並清除本機憑證；上游授權撤銷依官方流程處理，不把刪除本機檔案當作上游撤銷。

- Claude Code 邊界：僅執行原版 binary，使用者走官方登入；遵守平台承載與個人用量條款，不代收 Claude.ai token、不轉售用量。使用者專屬 CLI 狀態與 task worktree 分開持久化；憑證更新與 session 續接不可依賴唯讀掛載假設。

- Worker 沙箱：每個任務一個 git worktree，`run` 工具在 Docker/Podman 容器內執行（不用 bubblewrap，省去自行調校 seccomp 等設定），預設拒絕對外網路，依角色只開必要的套件倉庫、官方驗證／推論端點與平台 MCP 端點。ChatGPT provider 在 pots 服務端執行，不把其 token 交給任務容器；Claude Code 容器需自己的官方連線。禁止取得宿主 Docker socket 與其他任務的秘密。M4 的實作：每個任務一個獨立的 git repo（不是 `git worktree`），平台不在不信任的工作目錄裡執行會讀它設定的 git 指令；容器拿掉所有 capability、`no-new-privileges`、以你的 uid 執行，GitHub token 只在平台這邊的 git 與 API 用；對外網路的白名單還沒做（容器可以連外網），也還有「本機（沒有沙箱）」的選項。見[M4 的實作](agent-runtime.md#m4-的實作)。

- 託管電腦：node 是專用機、可以清空，不放個人資料，也不登入你的個人帳號，這是 GUI 操作免核准的前提。node 主動連出、只走私有網路，以 Ed25519 金鑰配對；node 上不存模型憑證，GitHub token 照舊短效且限定 repo。effectful app 裡的不可逆操作走 `gui_checkpoint`，執行前驗證畫面；每一步的截圖都寫進 trace。詳見[託管電腦](computer-nodes.md#核准與-gui-的副作用)。

- Prompt injection：PR 留言、CI log、issue 內容、node 截圖中的畫面文字等他人寫的內容以 `Untrusted<T>` 包裝，送進 agent 時加上來源標記。平台無法根治，所以真正的防線是「有副作用的動作一律要核准，且由平台執行」。

- 稽核：`task_events` 與 `approvals` 只增不改，記錄誰在什麼時候做了什麼。

### 預算

- 階層式：Goal 有總預算（token、金額或執行時間／次數），建立子任務時從中分配，用完就轉 `NeedsHuman`。訂閱額度與 API 金額分帳，並限制 worker 並行數；不可用本地 token 計數推算剩餘訂閱額度。

- 結構上限：任務樹深度預設 3 層、每個節點最多 8 個子任務、每次 run 最多 12 個多輪回合。

- 熔斷：單一 project 在 1 小時內耗用超過設定值，暂停所有 run 並推播。

- 用量來源：以 provider／CLI 回傳的 usage 記錄 token（含可取得的快取資訊），缺少資料標為未知，不填零。OM 的 Observer 與 Reflector 也計入。區分 API 費用、訂閱用量與 CLI 成本估計；usage 不是實際帳單，也不保證等於訂閱額度扣除量。

- 額度與 fallback：區分需要登入、撤銷、訂閱額度不足、暫時限流、模型／workspace 不允許。額度不足時排隊或轉 `NeedsHuman`，不以無限重試消耗資源；切換成付費 API 或額外付費用量須有使用者預先授權與金額上限。

### 可觀測性

- Trace：用 `tracing` 加 OpenTelemetry。一個 span 對應一次 agent run，子 span 是每次模型呼叫與工具呼叫。內建 agent 用 pots-llm 的 `Hook` 與 `AgentEvent`，Claude Code 用 adapter／MCP 事件記錄工具參數與結果；寫入前遮蔽憑證與敏感標頭，OAuth code／token exchange 不記 body。

- 「為什麼」面板：在 UI 的任何一則 agent 訊息上，可以展開看到它的觸發來源、當時的記憶區塊版本、工具呼叫順序。

- 成本報表：依 project、Goal、agent、provider／runtime 彙總 token、時間、次數與可取得的費用，區分 OM 開銷與主要工作；訂閱用量與 API 金額分開呈現，估計值明確標示。

### M6a 的實作

M6a 依上面的設計實作在 `crates/pots-server/src/budget.rs`（加上 `0008_budget_trace.sql`），協定 v8；桌面版是總管 live、「為什麼」面板、Goal 卡片上的真實預算百分比與 Inbox 的預算核准。和你確認過的取捨：

- **用量來源。** `pots_llm` 的 `Hook` 多了 `after_turn`，`Gate` 在每輪模型呼叫結束、處理工具之前同步記帳，所以下一輪的 `before_turn` 看得到剛用掉的 token。用量寫進 `agent_runs.input_tokens`／`output_tokens`，並累加到 `goals.spent_tokens` 與 `tasks.spent_tokens`；舊的「跑完才加一次」拿掉了，不會重複計算。缺少 usage 的 runtime（Claude Code）維持 0，不推算。
- **階層預算。** Goal 預設 40 萬 token；`create_goal` 可以指定 `budget_tokens`，夾在 2 萬到 400 萬之間。沒有 Goal 的對話（chat、總管）不限制，只記用量。結構上限（深度、子任務數、每輪回合數）照舊。
- **停下來與追加。** 預算用完時 `before_turn` 回 `false`（每輪讀 Goal 的總用量，不是 run 開始時的快照，同一個 Goal 的多個 Planner／worker／subagent run 不會合起來超過上限），run 以 `Stopped` 結束，`agent_runs.stop_reason` 記 `budget`；Planner 與 worker 的完成路徑看到它就把備註改成「預算用完」，並建立一筆 `IncreaseBudget` 核准（同一個 Goal 只會有一筆待決），Planner 不再被叫醒。核准後 `goals.budget_tokens` 追加原本的一半（至少 10 萬）、叫醒 Planner；拒絕或 72 小時逾時記下 `goals.budget_denied`，之後不再自動申請。對話 agent 的超預算只會停在回覆，不會產生核准。
- **熔斷。** 一個 project 過去一小時（依 `agent_runs.started_at`）的 token 總和超過上限就熔斷，預設 200 萬，`projects.hourly_budget_tokens` 可以覆寫（還沒有設定 UI）。熔斷時把 `projects.breaker_until` 標起來、建立一筆不綁任務的 `ResumeProject` 核准並在 project 對話貼一張通知；排隊中的 run 留在佇列（claim 跳過熔斷中的 project），正在跑的在下一輪前停下（`stop_reason = breaker`）。核准後清掉熔斷、替所有進行中的 Goal 重排 Planner；拒絕或逾時維持暫停，冷卻一小時後 sweeper 再建一筆核准（不會永遠卡住），重啟也不會自己恢復。用量用 `run_trace` 每一輪模型的時間算滾動的一小時，不是 run 的開始時間。chat 的 `Send` 在熔斷期間照常擋下（run 一輪都開始不了）。
- **trace。** `run_trace` 每個 run 一列一列記模型輪與工具呼叫：模型輪帶 turn、input／output tokens 與 token 總和（熔斷用），工具帶名稱、遮蔽後的參數與結果（JSON 逐欄遮蔽、其他文字遮蔽 `key=value` 與 `Bearer …` 再截短）與成功與否；`agent_runs` 另記 `model` 與 run 開始時的 `memory_revision`（chat 與總管才有）。遮蔽只處理明顯的欄位名稱（key／token／secret／password／authorization／cookie），不是完整的秘密掃描；`tracing` 的 span 照舊，OpenTelemetry 匯出還沒做。
- **「為什麼」面板。** `ClientCommand::Why { item }` 依訊息的 `run_id` 回傳觸發來源（使用者訊息、排程、外部事件等）、記憶版本、模型、用量、停止原因與 trace 步驟；只回給問的連線，不寫事件日誌。
- **「你不在期間」。** 客戶端打開 project 頻道時送 `MarkRead`；距上次看到超過兩小時而且期間有里程碑（新 Goal、完成的 Goal／任務、待決核准、需要你處理的任務）時，插入一張 `Card::CatchUp`。摘要由平台從資料庫產生，不含他人寫的內容，也不進 OM（`MemoryPolicy::Skip`）。目前只有一個 `last_read_at`，沒有逐裝置的已讀狀態。
- **還沒做。** 成本報表、金錢計價（訂閱用量與 API 金額分帳）、worker／Claude Code 的執行時間與次數預算、熔斷上限的設定 UI、`pause_automations` 與熔斷的互動（熔斷不會自動恢復，暫停也不會觸發熔斷）、OpenTelemetry 匯出。見 [M6b](roadmap-and-decisions.md)。

以假的模型與手動時鐘驗證（`tests/budget.rs`）：Planner 用掉超過小預算 → 停下並申請 `IncreaseBudget` → 核准後追加並跑完；`Why` 回傳觸發、記憶版本與 trace；離開三小時後的 `MarkRead` 產生 catch-up 卡片；總管用 `create_goal` 在指定 project 開 Goal。

