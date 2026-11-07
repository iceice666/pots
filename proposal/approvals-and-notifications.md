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

