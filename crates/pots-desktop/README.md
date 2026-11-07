# pots-desktop

Pots 桌面客戶端（GPUI + GPUI Kit）。目前只有 UI，資料是假的，還沒接上任何服務。畫面依 [`ui/`](../../ui/) 的設計稿實作，行為依 [`proposal/ux-and-frontend.md`](../../proposal/ux-and-frontend.md)。

## 執行

GPUI 需要比 Rust 1.95 新的編譯器（已用 1.98 測試）。系統 `cargo` 太舊時改用 rustup 的 stable：

```sh
cargo run -p pots-desktop

# 系統 cargo 太舊時
PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" cargo run -p pots-desktop
```

## 已有的畫面

| 區塊 | 可以做的事 |
| --- | --- |
| 實例直列 | 切換「我的筆電」（桌面版）與「家用伺服器」（多設備版）、開全域 Inbox；徽章是待處理數。只連一個實例時不顯示 |
| 左欄 | 目前實例的 project 列表（待核准數、「需要你」、未讀）、總管、Inbox、排程與 Listener |
| 頻道 | agent 狀態、預算、停止；對話流（訊息、工具摘要可展開、事件、Goal／核准／Artifact／「你不在期間」卡片）；輸入框（Enter 送出、Shift+Enter 換行，「排隊」或「改方向」） |
| Goal 面板 | 任務依狀態分組、最新 Team 貼文、Brief、Team 分頁；留言給 Planner |
| 任務 thread | 狀態、分支、耗時與花費、進度步驟、這個任務的對話；取消任務、直接對 worker 說話 |
| Inbox | 全域或單一實例、依類型篩選；核准、拒絕、再試一次等動作會更新所有徽章與對話中的核准卡 |
| 總管 | 左欄「總管」或 ⌘⇧O（其他平台 Ctrl+Shift+O）切換過去，和頻道一樣佔中欄；右欄是專案概況（點了進該 project）與總管記得的事。在總管畫面切換實例會留在新實例的總管 |

Artifacts、自動化、記憶、Ledger 分頁，以及「看 diff」「看 trace」「加入實例」等按鈕還沒實作，點了會在底部提示。

## 結構

- `src/model.rs`：view model 與即時協定（`ServerEvent`、`ClientCommand`），型別依 `proposal/architecture.md` 命名。`Store` 是客戶端的整體狀態，使用者動作會把要送出的指令記在 `Store::outbox`。
- `src/mock.rs`：假資料，內容取自設計稿。
- `src/app.rs`：根視圖 `PotsApp`，持有 `Store` 與 UI 狀態（目前畫面、右欄、篩選、輸入框）。
- `src/views/`：各區塊的 render；`widgets.rs` 是依設計稿手刻的小元件，`theme.rs` 是色票。

## 驗證

```sh
cargo test -p pots-desktop                       # model 與假資料的單元測試
cargo test -p pots-desktop --test screenshots    # 用離屏 renderer 輸出主要畫面的 PNG
```

截圖寫到 `target/tmp/screenshots/`，需要 Metal（macOS）或 WGPU（Linux）。這只是產生圖檔方便檢查版面，不是 golden-image 比對。

`Cargo.lock` 以 gpui-kit 0.9.0 自帶的 lockfile 為起點：開啟 `test-support` 時 Cargo 會把 `gpui-fast` 等弱依賴也解析進 lockfile，用其他起點容易出現 `taffy` 等版本衝突。
