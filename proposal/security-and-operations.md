# 安全、預算與可觀測性

[← 產品提案索引](product.md)

## 安全、預算與可觀測性

每則進入 agent 的外部文字都當作不可信輸入，每個 Goal 都有硬性預算，每次 agent 執行都有完整 trace。

### 安全

- 身分驗證：單機版只綁 127.0.0.1、不需登入；自架版為單人或小團隊，用 passkey （WebAuthn）登入，不存密碼。也可以關閉內建登入，改接反向代理的 forward-auth （Authelia、Authentik 等）：代理驗證後以 Remote-User 等標頭傳入身分，服務只信任來自代理的請求（只綁內網位址或驗證代理來源），WebSocket 升級請求同樣經過 forward-auth。GitHub webhook 路徑須在代理設定為略過，改由 HMAC 驗簽保護。

- 憑證隔離：LLM API key 與 GitHub App 私鑰只在伺服器環境變數或密鑰檔中，不進任何 prompt。worker 拿到的 GitHub token 只限該任務的 repo，且有效期短。

- Worker 沙箱：每個任務一個 git worktree，`run` 工具在 Docker/Podman 容器內執行（不用 bubblewrap，省去自行調校 seccomp 等設定），預設關閉對外網路，只開套件倉庫白名單。

- Prompt injection：PR 留言、CI log、issue 內容等他人寫的文字以 `Untrusted<T>` 包裝，送進 agent 時加上來源標記。平台無法根治，所以真正的防線是「有副作用的動作一律要核准，且由平台執行」。

- 稽核：`task_events` 與 `approvals` 只增不改，記錄誰在什麼時候做了什麼。

### 預算

- 階層式：Goal 有總預算（token 或金額），建立子任務時從中分配，用完就轉 `NeedsHuman` 。

- 結構上限：任務樹深度預設 3 層、每個節點最多 8 個子任務、每次 run 最多 12 個多輪回合。

- 熔斷：單一 project 在 1 小時內耗用超過設定值，暂停所有 run 並推播。

- 計費來源：用 provider 回傳的 usage 記帳（含快取命中），不自己估算。OM 的 Observer 與 Reflector 也計入。

### 可觀測性

- Trace：用 `tracing` 加 OpenTelemetry。一個 span 對應一次 agent run，子 span 是每次模型呼叫與工具呼叫。用 rig 的 `PromptHook` 記錄工具參數與結果。

- 「為什麼」面板：在 UI 的任何一則 agent 訊息上，可以展開看到它的觸發來源、當時的記憶區塊版本、工具呼叫順序。

- 成本報表：依 project、Goal、agent 彙總的 token 與金額，區分 OM 開銷與主要工作。

