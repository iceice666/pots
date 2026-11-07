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
- 第一次使用：Inbox 會出現「設定模型 provider」，按「開啟設定」（或左下角「設定」）打開設定視窗，在「模型 Provider」選一個：
  - **ChatGPT 方案**：按「登入」開瀏覽器授權，再「設為使用中」。模型留空就用帳號清單的第一個。
  - **OpenAI 相容 API**：base URL 到 `/v1` 為止（OpenAI、OpenRouter、Ollama 的 `http://localhost:11434/v1` 等），本機服務可以不填 key。
  - **Anthropic 相容 API**：base URL 例如 `https://api.anthropic.com`，需要 key。
  「測試連線」會列出可用的模型，點一個填入。API key 只送給本機服務、存在資料目錄，畫面只顯示末四碼。
- 左欄「新增專案」建立 project，之後就能在頻道對話。斷線時自動重連，重連後補上錯過的事件。

### Live 模式目前有的

| 區塊 | 狀態 |
| --- | --- |
| 實例與 project | 本機實例、建立 project、project 列表 |
| 頻道對話 | 傳訊息（排隊／改方向）、串流回覆、工具摘要、停止 |
| Inbox | 還沒設定 provider、ChatGPT 需要登入時的提示 |
| 設定 | 左下角打開設定視窗，左邊是類別，Esc 或點背景關閉。「模型 Provider」：新增、編輯、測試連線、刪除、切換使用中，ChatGPT 登入與登出；「關於」：版本與實例資訊 |
| 其他 | Goal、任務、Artifact、自動化、總管在 live 模式是空的，等之後的里程碑 |

## 已有的畫面（`--mock` 假資料）

| 區塊 | 可以做的事 |
| --- | --- |
| 實例直列 | 切換「我的筆電」（桌面版）與「家用伺服器」（多設備版）、開全域 Inbox；徽章是待處理數。只連一個實例時不顯示 |
| 左欄 | 目前實例的 project 列表（待核准數、「需要你」、未讀）、總管、Inbox、排程與 Listener（批次暫停時顯示「已暫停」） |
| 頻道 | agent 狀態、預算、停止；對話流（訊息、工具摘要可展開、事件、Goal／核准／Artifact／「你不在期間」卡片）；輸入框（Enter 送出、Shift+Enter 換行，「排隊」或「改方向」） |
| Goal 面板 | 任務依狀態分組、最新 Team 貼文、Brief、Team 分頁；留言給 Planner；Brief 與 Ledger 分頁可以開啟對應的 artifact |
| Artifacts | 頻道「Artifacts」分頁列出 Spec、Brief、Ledger、Report、Project Notes；點列表或對話裡的 artifact 卡片，右欄開編輯器（gpui-kit-markdown-editor）。編輯器依 `##` 區段切開：你改過的區段標出時間，agent 正在寫的區段唯讀；Project Notes 顯示待核准的提案數 |
| 自動化 | 左欄「排程」「Listener」進實例的自動化頁：依專案篩選、批次暫停橫幅（立即恢復）、排程表（開關啟用）、listener 表；點一列在右欄看條件、動作、上限與觸發紀錄，可以停用、刪除；待核准的 listener 可以直接核准或拒絕，和 Inbox 那筆同步。頻道「自動化」分頁是同一份資料，只看這個 project（含套用到所有專案的 listener） |
| 任務 thread | 狀態、分支、耗時與花費、進度步驟、這個任務的對話；取消任務、直接對 worker 說話 |
| Inbox | 全域或單一實例、依類型篩選；核准、拒絕、再試一次等動作會更新所有徽章與對話中的核准卡 |
| 總管 | 左欄「總管」或 ⌘⇧O（其他平台 Ctrl+Shift+O）切換過去，和頻道一樣佔中欄；右欄是專案概況（點了進該 project）與總管記得的事。在總管畫面切換實例會留在新實例的總管 |

「記憶」分頁還沒實作，只顯示說明。「看 diff」「看 trace」「版本紀錄」「比較差異」「看提案」「新增 listener」「編輯條件」「改時間」「加入實例」、實例選單等按鈕還沒實作，點了會在底部提示。

Artifact 的編輯與自動化的開關、刪除、恢復只改本地狀態：這些走 HTTP API，提案裡還沒定義，所以不會記進 `Store::outbox`。

## 結構

- `src/model.rs`：客戶端的整體狀態 `Store`；view model 與即時協定在 [`pots-proto`](../pots-proto/)。使用者動作把要送出的指令記在 `Store::outbox`，live 模式下每次重繪前送給服務。
- `src/connection.rs`：一個實例的 WebSocket 連線，跑在自己的執行緒與 tokio runtime；斷線自動重連並帶 `Resume { last_seq }`。
- `src/sync.rs`：把 `ServerEvent` 套用到 `Store`（快照換掉暫用的實例 ID、串流文字、provider 與登入狀態轉成 Inbox 提示、UTC 轉本地時間）。
- `src/mock.rs`：假資料。
- `src/app.rs`：根視圖 `PotsApp`，持有 `Store` 與 UI 狀態（目前畫面、右欄、篩選、輸入框）。
- `src/views/`：各區塊的 render；`artifact.rs` 是 artifact 列表與編輯器，`automation.rs` 是自動化頁與詳細面板，`widgets.rs` 是手刻的小元件，`theme.rs` 是色票。

## 驗證

```sh
cargo test -p pots-desktop                       # model、事件套用與假資料的單元測試
cargo test -p pots-desktop --test screenshots    # 用離屏 renderer 輸出主要畫面的 PNG，順便檢查編輯器寫回與唯讀區段
cargo run -p pots-desktop --example probe -- <資料目錄> project 名稱 send 你好   # 不開視窗直接和服務說話
```

截圖裡的 `11-live-*` 到 `14-live-*` 會在背景起一個真的服務（模型換成腳本），走一次「還沒設定 provider → 設定視窗新增 OpenAI 相容 API、測試連線、儲存 → 建立 project → 傳訊息 → 收到串流回覆」。

截圖寫到 `target/tmp/screenshots/`，需要 Metal（macOS）或 WGPU（Linux）。這只是產生圖檔方便檢查版面，不是 golden-image 比對。

`Cargo.lock` 以 gpui-kit 0.9.0 自帶的 lockfile 為起點：開啟 `test-support` 時 Cargo 會把 `gpui-fast` 等弱依賴也解析進 lockfile，用其他起點容易出現 `taffy` 等版本衝突。
