# Agent 指揮平台 UI


## 預覽

直接用瀏覽器開啟 `ui/index.html`，點選畫面標題可以單獨開啟該頁。

也可以在專案根目錄執行：

```sh
python3 -m http.server 8000 --bind 127.0.0.1 --directory ui
```

然後開啟 <http://127.0.0.1:8000/>。結束預覽時按 `Ctrl+C`。

| 檔案 | 畫面 | 原始預覽尺寸 |
| --- | --- | --- |
| `index.html` | 所有畫面總覽 | — |
| `channel.html` | 頻道 + Goal thread | 1440 × 900 |
| `task.html` | Worker 任務 thread | 1440 × 900 |
| `artifact.html` | Artifact 編輯 | 1440 × 900 |
| `inbox.html` | Inbox | 1440 × 900 |
| `mobile-inbox.html` | 手機 Inbox | 390 × 844 |
| `overseer.html` | 總管對話 | 1440 × 900 |
| `automations.html` | 自動化管理 | 1440 × 900 |

## 結構與修改

- `*.html`：可直接編輯的畫面結構，保留原本的文字、語意標籤、表單與 SVG。
- `styles/base.css`：原始字型宣告與基礎樣式，各頁共用。
- `styles/ui.css`：從原本 inline style 抽出的 281 組去重樣式，以 `.ui-001` 等 class 對應元素；相同 class 可能被多頁共用，修改會一起生效。
- `styles/instances.css`：多實例的實例直列、側欄頂端的實例切換與 Inbox 的實例標示，使用具名 class。
- `styles/gallery.css`：總覽頁樣式。
- `fonts/`：原檔內含的 Inter WOFF2，已按內容去重；`LICENSE.txt` 保留原始授權說明。

已將預覽器的 `sc-raw-*` 標籤還原成原生表格與下拉選單，並將 SVG 屬性還原成 `viewBox`。樣式使用重複 class selector，避免抽出 inline style 後被原稿的 `a:hover` 規則意外覆蓋。

移除了原來的解壓縮器、DC / React 預覽 runtime 與空的 Component 腳本。畫面本來沒有業務互動，因此沒有另外產生不必要的 JS；核准、傳訊、編輯、自動化等按鈕仍只是靜態展示。輸入框與 checkbox 保留瀏覽器原生行為，不會儲存資料。

原本的 `Main.dc.html`、`Task.dc.html` 等連結已改成這裡對應的 HTML。原稿有些標籤不同的連結本來就指向同一頁，這裡仍保留其目的地；手機頁的 `#home` 等連結也維持原稿佔位，不代表已實作分頁。

桌面稿畫的是同時連兩個實例的情境（見[多實例並存](../proposal/deployment.md#客戶端多連線)）：「我的筆電」是桌面版，「家用伺服器」是多設備版，各頁目前選取的是家用伺服器；`inbox.html` 是合併所有實例的全域 Inbox。只連一個實例時客戶端不顯示直列，稿中沒有另外畫這個情境。手機只連得到多設備版，所以手機稿只顯示家用伺服器的項目。

總覽保留原始固定畫布尺寸。手機頁本身仍是 390 × 844 的設計稿，沒有另行改寫成響應式應用程式。

## 字型與離線限制

原始打包檔只內含一份 Inter 字型子集，其餘 Inter / Noto Sans TC 子集仍指向 Google Fonts。這裡沒有下載額外資源，保留這些 URL；無網路時 UI 結構仍可直接顯示，但未內含的字元會使用系統後備字型，字寬與換行可能不同。
