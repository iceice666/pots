# pots-desktop

Pots 桌面客戶端（GPUI + GPUI Kit）。預設連到本機的內嵌服務（[`pots-server`](../pots-server/)），也可以用 `--mock` 只看假資料的畫面。畫面與行為依 [`proposal/ux-and-frontend.md`](../../proposal/ux-and-frontend.md)。

## 執行

GPUI 需要比 Rust 1.95 新的編譯器（已用 1.98 測試）。系統 `cargo` 太舊時改用 rustup 的 stable：

```sh
cargo run -p pots-desktop                 # 連本機服務；沒在跑就以子行程啟動
cargo run -p pots-desktop -- --mock       # 假資料，不連服務
cargo run -p pots-desktop -- --serve      # 只跑服務，不開視窗

# 系統 cargo 太舊時
PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" cargo run -p pots-desktop
```

### 本機服務

- 資料目錄預設是系統的應用程式資料目錄下的 `pots`（macOS：`~/Library/Application Support/pots`），可用 `POTS_DATA_DIR` 或 `--data-dir` 覆寫。
- app 讀資料目錄的 `server.json` 找服務；連不上就用 `--serve` 啟動自己當子行程（獨立的 process group），日誌寫到資料目錄的 `server.log`。關掉視窗後服務繼續跑，用 `kill` 或 ctrl-c 結束；目前還沒有系統匣或選單列。
- 常駐的服務是舊版（協定版本不同，或執行檔在服務啟動後被重新編譯、更新過）時，app 會讓它正常結束再啟動新的，所以重新編譯後直接開 app 就好。
- 第一次使用：Inbox 會出現「設定模型 provider」，按「開啟設定」（或左下角「設定」）打開設定視窗，在「模型 Provider」登入 ChatGPT，或在清單下面的「新增 provider」選一種（每一格說明適合接哪些服務），那一區會換成表單：
  - **ChatGPT 方案**：按「登入」開瀏覽器授權，再「設為使用中」。模型留空就用帳號清單的第一個。
  - **OpenAI 相容 API**：base URL 到 `/v1` 為止；「快速填入」可以直接帶入 OpenAI、OpenRouter、Ollama（`http://localhost:11434/v1`）的 base URL 與名稱，本機服務可以不填 key。
  - **Anthropic 相容 API**：base URL 例如 `https://api.anthropic.com`，需要 key。
  - **OpenCode Zen／Go**：內建模型目錄，填 API key 後直接從清單挑模型；base URL 與預設 header 收在「進階設定」。
  名稱與 base URL 已經預填，游標在 API key。模型欄旁邊的「測試連線」會列出可用的模型，用下面的下拉選單搜尋後選一個；改了 base URL 或 key，舊的結果會收掉。還沒有使用中的 provider 時按鈕是「儲存並設為使用中」，這時沒選模型不能儲存。API key 只送給本機服務、存在資料目錄，畫面只顯示末四碼（輸入時可以按眼睛圖示檢查）。
- 左欄「新增專案」建立 project，之後就能在頻道對話。M9 的 onboarding 精靈會一併指定工作資料夾：GitHub repo、本機 git repo 的絕對路徑，或「稍後再設定」；沒設定時頻道會提示，自動化分頁也能改。斷線時自動重連，重連後補上錯過的事件。

### Live 模式目前有的

| 區塊 | 狀態 |
| --- | --- |
| 實例與 project | 本機實例、建立 project、project 列表 |
| 頻道對話 | 傳訊息（排隊／改方向）、串流回覆、工具摘要、停止；右欄的成員列表（見下面假資料的說明，live 模式一樣） |
| Inbox | 服務的核准（merge PR、刪除分支、listener 提案、USER.md／Project Notes 的修改）：理由、具體動作（PR、sha、merge 方式）、提案的 diff、有效期限，「核准」、「拒絕」（可以填理由）、「看 diff」（開 PR）、「在頻道中查看」；需要你處理的任務（重試、取消）；還沒設定 provider、ChatGPT 需要登入時的提示。對話裡的核准卡片是同一筆 |
| 通知 | 新的核准、需要你處理的任務、Goal 完成時送系統通知（`notify-rust`）；設定的「通知」類別可以關掉、設定靜音時段（期間累積，結束時合併成一則）。視窗在前景而且正看著那個 project 時不另外通知；送出後 10 分鐘內回到視窗會開啟那一筆 |
| 記憶 | 頻道的「記憶」分頁：Observer 整理出的觀察與「目前在做什麼」，可以釘選、修正（Enter 儲存）、刪除；整理失敗時顯示原因 |
| Goal 與任務 | agent 開 Goal 時回覆下面附 Goal 卡片（狀態、任務數、Planner 的最後一句話；快速修改的 Goal 標「快速修改」），Planner 的里程碑、開工與 Planner 設的排程記在 Goal 面板的「動態」分頁（任務分頁上方顯示最新一筆），對話裡只出現 Goal 完成、失敗與快速修改卡住的卡片（點了開「動態」）。右欄 Goal 面板：任務依狀態分組，每列是現在怎樣了（進行中是 worker 最後一句進度、需要你的原因、完成的結果、失敗的次數與錯誤）與執行者、分支、PR（點了用系統瀏覽器開）；Brief 全文、Ledger；Team 分頁是 Planner 與 worker 的發現、問題、決定、阻礙與交接（新的在上面，唯讀）。任務 thread 的「取消任務」會送到服務；「留言給 Planner」與直接對 worker 說話還沒接上 |
| Artifacts | 頻道「Artifacts」分頁與右欄編輯器：修改先是草稿（「未儲存」），按「儲存」帶著編輯開始時的版本送出；期間 agent 改過時服務以 `##` 區段合併，同一段兩邊都改過會拒絕並保留草稿。沒有草稿時，agent 的修改直接顯示。「版本紀錄」列出每一版的作者、時間與 diff，可以還原。改 Goal 的 Brief 會叫醒 Planner |
| 設定 | 左下角打開設定視窗，左邊是類別，Esc 或點背景關閉。「模型 Provider」：新增、編輯、測試連線、刪除、切換使用中，ChatGPT 登入與登出；API provider 可以在「進階設定」編輯每次請求的額外 header，OpenCode Zen／Go 有內建模型目錄與預設 header；「角色」：對話 agent、總管、Planner、內建 worker、Explorer 與 Reviewer subagent 各自的 provider、模型與 thinking（模型預設／關閉／低／中／高），沒設定的跟使用中的 provider 相同；「編輯」在卡片裡展開表單，選了特定 provider 才能填模型（內建目錄直接列出，其他 provider 用已存的設定向服務列出，可以搜尋），儲存送 `SetRoleModel`。thinking 依 provider 轉成 OpenAI 的 reasoning effort 或 Anthropic 的 thinking，模型不支援時改回「模型預設」；Claude Code worker 用「Worker」設定的模型，不看這裡；「記憶」：整理記憶用的 provider 與模型（預設跟對話相同）；「GitHub」：fine-grained token（只顯示末四碼）、API base URL（改了要重新輸入 token）、webhook secret、測試連線、來源狀態；「通知」：桌面通知與靜音時段；「USER.md」：跨 project 的使用者偏好（直接編輯，agent 只能提案）；「Worker」：後端（內建 worker／Claude Code）、容器執行檔與 image（`docker build -t pots-worker:latest docker/worker`；worker 一律在容器裡執行，沒有本機模式，project 有環境時改用環境建出的 image）、Claude Code 模型、同時執行的 worker 數，以及登入 Claude Code 的指令（可複製；你在終端機自己執行 `/login`，Pots 不碰憑證）；「關於」：版本與實例資訊 |
| 自動化 | 頻道「自動化」分頁：監看的 repo（編輯後 Enter 儲存；只影響外部事件，工作資料夾在同一頁上方或 onboarding 設定）、核准後 merge 的方式（squash／merge commit／rebase）、暫停／恢復這個 project 的自動化、這個 project 的排程與 listener。「環境」區塊管 worker 的 project 環境：沒有環境時 worker 用「Worker」設定的 image；「分析並建立環境」（已有版本時是「重新分析」，要先設定工作資料夾）可以附一段給環境 worker 的說明，送 `SetupProjectEnv`，環境 worker（上限 120 分鐘）讀 repo、寫出安裝／準備腳本、驗證指令、環境變數與快取目錄，驗證通過就直接套用，不用核准；分析中顯示「環境分析進行中…」與「查看進度」（開那個 Goal，Goal 上標「環境分析」，沒有 Planner）。使用中的版本顯示摘要與建立時間，「顯示規格」展開唯讀的腳本、`K=V` 環境變數與快取；版本歷史每一版可以「使用這一版」，「不用環境」改回「Worker」的 image（送 `UseProjectEnv`）。之後的 worker 每次先跑準備腳本，60 分鐘的計時從準備完才開始。左欄「排程」「Listener」進實例的自動化頁：「全部暫停」與暫停橫幅的「立即恢復」、排程的啟用開關與刪除、listener 的啟用／停用／刪除、agent 提案的 listener 要在核准卡片或 Inbox 核准才生效、「新增 listener」表單（事件種類、label／author／branch／conclusion 條件、開 Goal／快速修正／通知對話、每小時上限）、外部來源的健康狀態。listener 開的 Goal 在對話裡是 Goal 卡片；排程與 listener 送給對話 agent 的通知是對話裡的一則事件。等待中的任務在 Goal 面板顯示在等什麼與逾時時間 |
| 用量 | 左欄「用量」：目前實例所有 project 的 `CostReport`（24 小時／7 天／30 天）。總 token、run 數、估計金額（單價估計與 CLI 回報分開）、平均每天；訂閱／API／CLI 與主要工作／記憶整理的比例；依 project、Goal、模型、agent 分組。「更新單價」從 models.dev 重抓單價後重新要報表。沒連上服務時用假資料 |
| 其他 | 總管在 live 模式是空的（M6）；「改時間」「編輯條件」還沒接上 |

## 已有的畫面（`--mock` 假資料）

| 區塊 | 可以做的事 |
| --- | --- |
| 實例直列 | 切換「我的筆電」（桌面版）與「家用伺服器」（多設備版）、開全域 Inbox；徽章是待處理數。只連一個實例時不顯示 |
| 左欄 | 目前實例的 project 列表（待核准數、「需要你」、未讀）、總管、Inbox、用量、排程與 Listener（批次暫停時顯示「已暫停」） |
| 頻道 | agent 狀態、預算、停止、「成員」切換；對話流（訊息、工具摘要可展開、事件、Goal／核准／Artifact／「你不在期間」卡片；處理完的核准與問題、已結束的 Goal、沒有動作的通知預設收成一行，點一下展開，展開狀態不保存）；輸入框（Enter 送出、Shift+Enter 換行，「排隊」或「改方向」） |
| 成員列表 | 頻道標題列的人像按鈕（打開時是選取狀態）切換右欄的成員列表，像 Discord 的成員欄：依「工作中」「等待中」「待命」分組（「工作中 — 2」），成員是你、對話 agent、每個進行中 Goal 的 Planner（快速修改與環境分析沒有）與進行中／等待中／需要你的任務的 worker（已規劃與結束的不列）。每列是頭像與狀態點、名稱與 AGENT／PLANNER／WORKER 標籤、狀態或進度（等待中是原因）、實際用的模型與 thinking（Claude Code worker 是「Claude Code · 模型」）。點 worker 開任務 thread、點 Planner 開那個 Goal 的面板（右欄換成那個內容）、點對話 agent 開設定的「角色」。成員列表開著時換 project 會留著 |
| Goal 面板 | 任務依狀態分組（進度、需要你、失敗的原因、PR）、最新動態與最新 Team 貼文、「動態」分頁（OAuth 的 Goal 有假的進展）、Brief、Team 分頁（含交接對象）；留言給 Planner；Brief 與 Ledger 分頁可以開啟對應的 artifact。dotfiles 的 Goal 是快速修改 |
| Artifacts | 頻道「Artifacts」分頁列出 Spec、Brief、Ledger、Report、Project Notes；點列表或對話裡的 artifact 卡片，右欄開編輯器（gpui-kit-markdown-editor）。編輯器依 `##` 區段切開：你改過的區段標出時間，agent 正在寫的區段唯讀；按「儲存」才寫回（假資料只改本地，沒有版本紀錄）；Project Notes 顯示待核准的提案數 |
| 自動化 | 左欄「排程」「Listener」進實例的自動化頁：依專案篩選、批次暫停橫幅（立即恢復）、排程表（開關啟用）、listener 表；點一列在右欄看條件、動作、上限與觸發紀錄，可以停用、刪除；待核准的 listener 可以直接核准或拒絕，和 Inbox 那筆同步。頻道「自動化」分頁是同一份資料，只看這個 project（含套用到所有專案的 listener） |
| 任務 thread | 狀態、分支、耗時與花費、進度步驟、這個任務的對話；取消任務、直接對 worker 說話 |
| Inbox | 全域或單一實例、依類型篩選；核准、拒絕、再試一次等動作會更新所有徽章與對話中的核准卡 |
| 記憶 | auth-service 有一份假的記憶（含釘選與修正過的觀察）；釘選、修正、刪除只改本地狀態 |
| 總管 | 左欄「總管」或 ⌘⇧O（其他平台 Ctrl+Shift+O）切換過去，和頻道一樣佔中欄；右欄是專案概況（點了進該 project）與總管記得的事。在總管畫面切換實例會留在新實例的總管 |

「看 diff」「看 trace」「看提案」「編輯條件」「改時間」「加入實例」、實例選單等按鈕還沒實作，點了會在底部提示。

自動化的開關、刪除、暫停與恢復先改本地狀態，同時記進 `Store::outbox`；連著服務時由 `Automation` 事件整份替換。換環境版本（「使用這一版」「不用環境」）一樣先改本地，由 `ProjectEnv` 事件整份替換。

## 結構

- `src/model.rs`：客戶端的整體狀態 `Store`；view model 與即時協定在 [`pots-proto`](../pots-proto/)。使用者動作把要送出的指令記在 `Store::outbox`，live 模式下每次重繪前送給服務。
- `src/connection.rs`：一個實例的 WebSocket 連線，跑在自己的執行緒與 tokio runtime；斷線自動重連並帶 `Resume { last_seq }`。
- `src/sync.rs`：把 `ServerEvent` 套用到 `Store`（快照換掉暫用的實例 ID、串流文字、provider 與登入狀態轉成 Inbox 提示、`Memory` 整份替換 project 的記憶、`GoalChanged`／`ArtifactChanged` 整筆替換 Goal 與 artifact、`Automation` 整份替換排程／listener／暫停／來源、`Github`、`Worker` 與 `ProjectRepos`、`ApprovalChanged` 與任務樹轉成 Inbox 項目與要推播的事、`Notifications`、`UserMd`、`ProjectMergeMethod`、UTC 轉本地時間）。
- `src/notify.rs`：桌面通知與靜音時段（累積、結束時合併）。
- `src/mock.rs`：假資料（家用伺服器的 Planner 用 Anthropic、思考高，Explorer 用便宜的模型、不思考）。
- `src/members.rs`：從 project、provider 與 worker 設定推導成員列表與分組的純函式。
- `src/app.rs`：根視圖 `PotsApp`，持有 `Store` 與 UI 狀態（目前畫面、右欄、篩選、輸入框）。右欄一次只顯示一種內容，由 `PotsApp::panel`（`Panel`：Goal、任務、artifact、成員列表等）決定。
- `src/views/`：各區塊的 render；`memory.rs` 是「記憶」分頁，`artifact.rs` 是 artifact 列表與編輯器，`automation.rs` 是自動化頁與詳細面板，`automation_forms.rs` 是 GitHub 設定、新增 listener、監看 repo 與暫停的表單，`project_env.rs` 是頻道「自動化」分頁的「環境」區塊，`worker_settings.rs` 是設定視窗的「Worker」類別，`roles.rs` 是「角色」類別，`member_list.rs` 是右欄的成員列表，`preferences.rs` 是「通知」與「USER.md」類別和拒絕核准的對話框，`widgets.rs` 是手刻的小元件，`theme.rs` 是色票。

## 驗證

```sh
cargo test -p pots-desktop                       # model、事件套用與假資料的單元測試
cargo test -p pots-desktop --test screenshots    # 用離屏 renderer 輸出主要畫面的 PNG，順便檢查編輯器的儲存與唯讀區段
cargo run -p pots-desktop --example probe -- <資料目錄> project 名稱 send 你好   # 不開視窗直接和服務說話
```

截圖裡的 `11-live-*` 到 `26-live-*` 會在背景起一個真的服務（模型換成腳本，GitHub API 指到沒有服務的本機 port），走一次「還沒設定 provider → 設定視窗新增 OpenAI 相容 API、測試連線、儲存 → 建立 project → 傳訊息 → 收到串流回覆 → agent 開 Goal、Planner 拆任務 → 在編輯器改 Brief 並儲存 → 版本紀錄 → 設定 GitHub token → Worker 設定（不合法的容器執行檔被拒絕、改好後儲存）→ 監看 repo、agent 建立排程 → 新增 listener → agent 提案 listener（Inbox 的核准、推播、對話裡的卡片）→ 拒絕並填理由 → 設定靜音時段 → 編輯 USER.md」。

截圖寫到 `target/tmp/screenshots/`，需要 Metal（macOS）或 WGPU（Linux）。這只是產生圖檔方便檢查版面，不是 golden-image 比對。

`Cargo.lock` 以 gpui-kit 0.9.0 自帶的 lockfile 為起點：開啟 `test-support` 時 Cargo 會把 `gpui-fast` 等弱依賴也解析進 lockfile，用其他起點容易出現 `taffy` 等版本衝突。
