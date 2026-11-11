# 核准流程與通知

[← 產品提案索引](product.md)

## 核准流程與通知

有副作用的動作一律走平台原生的 Approval，且核准的是具體動作的內容，不是 agent 的一句描述。只有需要你做決定的事才推播。

### Approval 生命週期

1. agent 呼叫 `request_approval` ，帶入結構化的 `action` （例如 `MergePr { pr, method }` ）。平台計算 `action_hash` 並存下。

2. 任務進入 `Waiting { on: Approval(id) }` ，對話出現核准卡片，Inbox 加一筆，推播送出。

3. 你核准或拒絕（可附理由）。拒絕理由會帶給 agent 作為喚醒訊息。

4. 執行由平台負責，不是 agent：平台只執行 `action_hash` 與核准時一致的動作。agent 無法在核准後偷改參數。

5. 逾時（預設 72 小時）自動變成 `Expired` ，任務轉為 `NeedsHuman` 。

### 需要核准的動作（預設）

| 動作 | 預設 | 說明 |
| --- | --- | --- |
| merge PR | 要核准 | 核心關卡 |
| 部署、發佈 | 要核准 |  |
| 刪除分支、force push | 要核准 | 不可逆 |
| 超出 Goal 預算 | 要核准 | 核准即追加預算 |
| 開 PR、push 到任務分支 | 不用 | 可在 project 設定中收緊 |
| 建立子任務 | 不用 | 受預算與深度上限約束 |
| 建立或放寬 listener | 要核准 | listener 會在你不在時自動花預算 |
| agent 刪除排程或 listener | 要核准 | 停用、啟用、改時間不用（和你在自動化頁做的一樣，對話裡有卡片） |
| agent 封存 artifact | 要核准 | 從列表移除，資料與版本紀錄仍留著；還原舊版不用 |

| 動作 | 預設 | 說明 |
| --- | --- | --- |
| 建立自我排程 | 不用 | 受最短間隔、數量與預算上限約束，對話裡會出現卡片 |
| 寄出或回覆郵件 | 要核准 | 以你的名義對外發言，不可撤回 |
| node 上 effectful app 的送出／付款／刪除 | 要核准 | `gui_checkpoint`，核准卡片附截圖，執行前驗證畫面一致，見[託管電腦](computer-nodes.md#核准與-gui-的副作用) |
| node 開啟允許清單外的 app | 要核准 | 核准後可以加入允許清單 |

### 通知策略

- 立即推播：新的核准請求、`NeedsHuman` （重複失敗、預算用完、agent 卡住、node 畫面需要人類接手）、Goal 完成、node 離線或權限失效。

- 不推播：一般進度、CI 結果、子任務完成。這些進「你不在期間」摘要。

- 靜音時段：可設定，期間的核准請求累積在 Inbox，結束時合併成一則。

### 手機推播

手機客戶端是原生 Android app（[`android/`](../android/)），推播走 [UnifiedPush](https://unifiedpush.org/)，用 [ntfy](https://ntfy.sh/) 當 distributor，不需要 FCM 與 Google 帳號（見[已決定事項](roadmap-and-decisions.md#已決定事項)）。

- **登記。** 手機上裝 ntfy app，伺服器可以是公開的 ntfy.sh 或自架的 ntfy。Pots app 透過 UnifiedPush 向 ntfy 登記，拿到一個 endpoint URL，連同 Web Push 的 `p256dh`、`auth` 金鑰送給 Pots 實例，存進 `push_subscriptions`。每台裝置、每個實例各自登記一次；裝置 token 被撤銷時，該裝置的訂閱一併刪除。
- **發送。** 實例用 VAPID 簽章，把內容依 RFC 8291 加密後 POST 到 endpoint。ntfy 只轉送密文，看不到內容；即使用公開的 ntfy.sh，topic 名稱外流也只會收到看不懂的訊息。實例只需要能連出到 ntfy 伺服器，不需要對外網址。
- **內容。** 推播只帶 ID 與一句摘要，例如核准 ID、project 名稱與動作標題；diff、程式碼與秘密一律不放。Android app 收到後自己產生系統通知，點擊開 `pots://approval/{id}`，再向實例拉取完整內容。
- **失效。** endpoint 回 404 或 410 時刪除該筆訂閱，並在實例的「裝置」頁標示需要重新登記；ntfy 沒有安裝或沒有選為 distributor 時，app 在設定頁提示。

iOS 不在 v1 範圍。

通知上的動作按鈕並非所有平台都支援，所以不依賴它：點擊通知一律開到該筆核准的專頁，頁面上就是大的「核准」與「拒絕」按鈕，一次點擊完成。支援動作按鈕的平台再額外加上。

桌面版沒有對外網址，改用作業系統的桌面通知（`notify-rust` ），點擊後在桌面客戶端開啟該筆核准。手機推播只有多設備版才有。客戶端同時連到多設備版實例時，桌面客戶端也會把該實例的核准以原生通知顯示；同一筆核准在任一裝置決定後，其他裝置的通知就收回或標為已處理。


### M5 的實作

M5 依上面的設計實作在 `crates/pots-server/src/approvals.rs`、`user_md.rs`、`notifications.rs`（加上 `0007_approvals.sql`）與 `watcher/`（CI 自動修、逾時），協定 v7；桌面版是 Inbox、對話裡的核准卡片、拒絕的理由對話框、設定的「通知」與「USER.md」類別、頻道「自動化」分頁的 merge 方式，以及 `notify-rust` 的系統通知。和你確認過的範圍與取捨：

- **要核准的動作。** M5 接上五種：merge PR（`MergePr`）、merge 後刪除任務分支（`DeleteBranch`）、啟用 agent 提案的 listener（`CreateListener`）、修改 USER.md（`EditUserMd`）與 Project Notes（`EditProjectNotes`）。部署與發佈、寄信、node 的 `gui_checkpoint` 還沒有對應的 connector，超出預算的核准在 M6。開 PR、push 到任務分支、建子任務、自我排程照預設不用核准；「在 project 設定收緊」還沒做。
- **申請與 hash。** worker（含 Claude Code，經 MCP）在 CI 通過後用 `request_approval`（`action: merge_pr`）申請 merge 自己任務的 PR，這一輪就結束；Planner 也有這個工具，可以替自己 Goal 裡已經結束或需要決定、有 PR 的任務申請。平台在申請時查 PR（要開著、沒有 merge）與 CI（失敗或還在跑的不給申請），把 repo、編號、當時的 head sha、merge 方式與分支存成結構化的動作，算出 SHA-256（`action_hash`），之後不再改變；狀態變化另記在只增不改的 `approval_events`。merge 方式預設用 project 的設定（頻道「自動化」分頁選 squash、merge commit 或 rebase，預設 squash），agent 可以另外指定，卡片上看得到。同一個任務同時只能有一筆等核准的 merge；同一份 USER.md、同一個 project 的 Project Notes 同時只能有一筆提案。
- **等待與撤回。** 申請 merge 的任務轉 `Waiting { Approval }`，期限就是核准的期限。在等的任務被你取消、被交接叫醒、Goal 結束時，還沒決定的核准自動撤回（`Cancelled`）；listener 提案在你刪掉、直接修改它時撤回。agent 提案的 listener 先存成 `pending`，等核准才生效；M5 之前留下的提案沒有核准，仍可以按「啟用」。
- **決定與執行。** 你在核准卡片或 Inbox 按「核准」或「拒絕」（`Decide`），拒絕時可以填一句理由。執行由平台負責，先重算 hash 和存下的比對：merge 帶核准時的 sha 呼叫 GitHub 的 merge API（PR 之後有新的 commit 時 GitHub 會拒絕；平台也先查一次 head 與 CI，CI 失敗或還在跑都不 merge，已經 merge 的視為成功），連網的動作在 commit 之後另外執行，服務重啟時接著做；USER.md 與 Project Notes 只在內容還是提案時的版本時寫入，listener 只在條件還是提案時的內容時啟用。merge 成功後任務標成完成、叫醒 Planner（快速修改直接結束 Goal），並自動開一筆不推播的「刪除分支」核准（只刪平台的 `pots/` 分支）。拒絕 merge 時 worker 帶著你的理由被叫醒；merge 沒有執行（head 變了、CI 失敗、GitHub 拒絕）時 worker 帶著原因被叫醒；拒絕 listener 提案就把它刪掉。已經決定過、已經逾時的不能再決定。
- **逾時。** 72 小時沒有決定就 `Expired`（sweeper 每輪檢查）：在等的任務轉 `NeedsHuman`（可以「重試」讓 worker 重新申請），listener 提案改成停用。
- **CI 失敗自動修。** M4 延後的自動修在這裡做：worker 等到的 CI 失敗時，平台把它叫醒、告訴它這是第幾次失敗，worker 自己找原因、修好再 `open_pr` 與 `wait_for`；連續失敗 3 次任務轉 `NeedsHuman`（並通知你），CI 通過時歸零；只算 worker 用 `ci_finished` 等的 CI。你按「重試」時，worker 會看到任務之前停在哪裡（例如核准逾時，重新申請就好）。CI 通過時的喚醒訊息提醒 worker 用 `request_approval` 申請 merge。
- **USER.md 與 Project Notes。** USER.md 存在實例（`user_md_revisions`，每次修改一版），你在設定的「USER.md」類別直接編輯（帶著開始編輯時的版本，期間被改過就拒絕）；對話 agent 用 `propose_user_md_edit` 提案。每個 project 建立時有一份 Project Notes（artifact），你在 Artifacts 分頁直接編輯；對話 agent 與 Planner 用 `propose_project_note` 提案。提案以 `##` 區段編輯套用在目前的內容上，卡片附 diff。對話 agent、Planner 與內建 worker 的 instructions 最前面是 USER.md，Project Notes 接在 project 指示後面；Claude Code 一樣經 `--append-system-prompt` 帶入，沒有寫成 CLAUDE.md：本機模式的設定目錄是你自己的，容器的設定目錄由所有任務共用、而且 worker 寫得到，都不適合放平台的檔案。
- **核准卡片與記憶。** 卡片貼在 project 的對話流（對話 agent 的提案附在它的回覆下面），只給人看；對話 agent 每輪的指示帶著這個 project 最近 5 筆核准的一句結果（「merge PR #42：已核准；用 squash merge 了 PR #42」），不寫進 Observational Memory。
- **Inbox 與通知。** Inbox 是服務的核准（快照帶還沒決定的與最近 7 天有變化的），加上客戶端從任務樹整理的「需要你處理」任務（重試、取消；交給 Planner 決定的 escalate 不算）。桌面通知由客戶端發出（服務是常駐的子行程，沒有視窗）：新的核准（刪分支除外）、需要你處理的任務、Goal 完成；一般進度、CI 結果不通知。設定的「通知」類別可以關掉通知、設定靜音時段（本機時間，可以跨午夜），期間的通知先累積，結束後合併成一則。視窗在前景而且正看著那個 project 時不另外通知。
- **M6a 的預算核准。** 「超出 Goal 預算」在 M6a 接上：預算用完時平台自動建立 `IncreaseBudget` 核准（同一個 Goal 同時只有一筆待決），核准後追加預算並叫醒 Planner；拒絕或逾時記下 `budget_denied`，不再自動申請。熔斷也走核准：`ResumeProject` 核准後清掉熔斷並重排所有進行中的 Goal，拒絕或逾時維持暫停。兩者都會推播（`notify: true`），卡片在 project 對話與 Inbox。見[安全、預算與可觀測性](security-and-operations.md#m6a-的實作)。
- **對話 agent 管理自動化與 artifact（協定 v13）。** 對話 agent 用 `list_automations` 看這個 project 的排程與 listener，`update_schedule`（停用／啟用、改下一次與過期時間）與 `update_listener`（停用／啟用）直接生效、附一張卡片；等核准的 listener 提案不能用它啟用，停用就是撤回提案。刪除走核准：`delete_schedule`（`DeleteSchedule`）、`delete_listener`（`DeleteListener`），核准時對象還在這個 project 才刪；`archive_artifact`（`ArchiveArtifact`，只限 profile 的 `artifact_kinds`，chat 是 Spec 與 Report）核准後設 `archived_at`（`0012_artifact_archive.sql`）並送 `ArtifactArchived`，之後列表、快照與 agent 的工具都看不到它。`artifact_history` 列出版本與 diff 或某一版全文，`restore_artifact` 帶目前版本還原成舊版（新增一版，作者是 agent）。同一個對象同時只能有一筆等核准的申請；逾時或拒絕就什麼都不做。
- **延後與已知限制。** 點通知本身沒有接上（macOS 的 `NSUserNotificationCenter` 要阻塞主執行緒才收得到點擊）：改成送出通知後 10 分鐘內視窗被叫到前景時開啟那一筆；沒有打包成 app 前，macOS 把通知顯示成「Finder」送的。已決定的核准不會收回其他裝置的通知（多設備在 M8）。手機推播（UnifiedPush／ntfy）在 M8。核准者固定是本機使用者（`decided_by` 記 `Principal`，留給多人）。merge 前平台只查 CI 是否失敗或還在跑（申請時也一樣），必要的檢查與 review 由 GitHub 的 branch protection 擋。Project Notes 的提案數（`ArtifactView::proposals`）還沒填。

以假的 GitHub API（PR 的 head 讀本機的 bare remote、merge 帶 sha 檢查、刪分支、CI 可設成失敗）、手動時鐘與假的模型驗證（`tests/approvals.rs`：worker 申請 merge → 核准 → 平台用 project 設定的方式與核准的 sha merge → 任務完成、Planner 結束 Goal → 刪分支的核准；拒絕時 worker 帶著理由被叫醒、重新申請，head 變了時不 merge、交回 worker；CI 連續失敗 3 次轉 `NeedsHuman`；72 小時逾時、重試後重新申請、取消任務時撤回；listener 提案要核准、拒絕就刪掉、刪掉時撤回；USER.md 與 Project Notes 的提案、核准、進到 instructions、提案後被改過時不寫入；通知設定的檢查），加上桌面版的事件套用與通知的單元測試和截圖流程（`22-live-*` 到 `26-live-*`）。對真的 GitHub merge 與 macOS 系統通知待使用者手動驗證。
