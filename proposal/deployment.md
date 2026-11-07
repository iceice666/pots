# 部署形態與多實例並存

[← 產品提案索引](product.md)

## 兩種形態，長期並存

Pots 有兩種部署形態，兩者長期並存，桌面版不是搬上伺服器前的過渡期。兩者是同一份程式碼、同一個客戶端協定，差異收在[後端介面](#切換點)後面，由設定檔決定。

- **桌面版（單機）**：GPUI 桌面 app 內嵌 Pots 服務，安裝後即可使用。資料存在本機 SQLite，服務只接受本機連線，電腦睡眠時 agent 也跟著停。
- **多設備版**：獨立的 `pots-server` binary，以單一 binary 或 docker compose 部署在常開的機器上（NAS、家用伺服器、VPS）。提供登入、對外網址與手機推播，手機與多台電腦都能連線。

同一個桌面客戶端可以[同時連多個實例](#多實例並存)，例如本機的桌面版加上一台自架的多設備版。每個 project 只屬於其中一個實例，需要時可以[移交](#project-移交)到另一個實例。兩個實例之間不同步資料。

| 面向 | 桌面版 | 多設備版（輕量） | 多設備版（完整） |
| --- | --- | --- | --- |
| 執行方式 | GPUI app 內嵌服務，以子行程啟動 | 單一 `pots-server` binary（systemd、launchd、NAS 套件） | docker compose |
| 資料庫 | SQLite（WAL 模式），一個檔案 | SQLite | Postgres |
| 工作佇列 | 行程內排程器，SQLite 表作持久化 | 同左（單一 runner） | `FOR UPDATE SKIP LOCKED`，可多個 runner |
| 網路 | 只綁 loopback | 私有網路（例如 `tailscale serve`）或反向代理加 TLS | 反向代理加 TLS |
| Pots 存取與登入 | 啟動時產生的本機 token，免登入；模型 provider 仍須另行授權 | passkey 或 forward-auth；原生客戶端用可撤銷的裝置 token；不取代模型 provider 授權 | 同左 |
| 外部事件 | 輪詢與 IMAP IDLE（GitHub 用 PAT 或 `gh` 的登入） | 輪詢；有公開網址時優先用 webhook | 各來源優先用 webhook，輪詢作備援 |
| 通知 | 作業系統桌面通知 | Android 推播（UnifiedPush，經 ntfy）；已連線的桌面客戶端顯示原生通知 | 同左 |
| 憑證 | 靜態 API key 存在受保護設定或 OS keychain；會輪替的 OAuth token 存在可寫入的持久 secret store | 靜態 API key 用 secret 檔等方式注入；會輪替的 OAuth token 存在可寫入的持久 secret store | 同左 |
| Worker 沙箱 | 本機 Docker/Podman | 伺服器上的 Docker/Podman | 同左 |

表中三欄是三組預設組合，不是寫死的模式。各項後端可以獨立選擇，例如家裡 NAS 上跑「SQLite 加 passkey 加 ntfy 推播」就是合理的多設備版，不必為了多設備就上 Postgres。不合理的組合在啟動時直接拒絕，規則見[設定驗證](#設定驗證)。

### 桌面版：app 內嵌服務

- **同一個執行檔，以子行程啟動。** 桌面 app 用 `--serve` 參數啟動自己的子行程來跑服務，客戶端照樣經由 loopback 的 WebSocket 與 HTTP API 連線，不走行程內的捷徑。這樣桌面版和多設備版共用同一條程式路徑與協定，UI 當掉或重新開啟也不會中斷正在跑的 agent run。
- **常駐。** 關閉視窗時服務繼續在背景執行，系統匣或選單列顯示狀態，從那裡才能真正結束服務。結束前若還有進行中的 run，要先列出來給你確認。
- **單一實例。** 資料目錄用檔案鎖保護。同一個資料目錄只能有一個服務；第二個 app 開啟時直接連上已在跑的那一個。
- **本機存取保護。** 服務綁定 `127.0.0.1` 的隨機 port。啟動 token 與 port 寫在資料目錄的 `server.json`（Unix `0600`，原子寫入），除了 `/healthz`，每個請求（含 WebSocket 升級）都要帶 `Authorization: Bearer <token>`，以固定時間比較。`Host` 必須剛好是 `127.0.0.1:<port>` 或 `localhost:<port>`，擋下 DNS rebinding；帶任何 `Origin` 標頭的請求一律拒絕——原生客戶端不送 `Origin`，瀏覽器一定會送，這同時擋下跨站請求。「只綁 127.0.0.1」本身不足以構成存取控制。
- **Node 專用 listener（選用）。** 桌面版要接[託管電腦](computer-nodes.md)時，可以在私有網路介面上另開一個 listener。這個 listener 只接受已配對 node 的 Ed25519 簽章連線，不提供客戶端 API，所以桌面版不必為了 node 而對外開放登入。

### 模型授權與持久化邊界

Pots 是使用者自行執行的開源軟體；上述部署形態不代表 Pots 營運平台代管、轉售訂閱或共用訂閱憑證。啟動 app、binary 或 Compose 只完成服務部署，還須完成各 provider 的官方授權。`Backends.auth` 只處理 Pots 的存取控制，不能代替下列模型授權。

**每個實例各自授權。** 桌面版和多設備版是兩個 runtime 安裝實例，ChatGPT 授權、Host ID 與 Claude Code 登入狀態各有一份，彼此不共用、不同步。

- **ChatGPT Plan provider（pots-llm）**：採官方 Sign in with ChatGPT（SIWC）開源 token-sharing 流程，由 Pots 管理取得的 OAuth 狀態，直接呼叫 Responses API；不透過 Codex CLI，也不讀取 Codex 私有憑證。每份授權只設一個 refresh owner：多 runner 時要協調成單一更新者，token 輪替時原子更新持久 secret store，避免併發 refresh 或重啟後拿舊的 refresh token 去用。會輪替的 token 不可只放在唯讀環境變數或唯讀掛載內。兩個實例同時使用時，必須是兩份獨立授權，不可複製同一份 refresh token 給兩個實例。M0 把憑證存在資料目錄的 `auth/chatgpt-credentials.json`（`0600`，原子寫入），Host ID 另存在 `host-id`。
- **Host ID**：綁定持久的 runtime 安裝實例，跨重啟保存，不為每個 task container 重新建立。自架 VM 使用自己的 Host ID，不複製桌面版的 Host ID。
- **自架登入**：SIWC 的瀏覽器登入使用本機 loopback callback，不能把伺服器網址當成 callback 任意替換。自架 VM 依[官方指南](https://developers.openai.com/siwc/token-sharing-open-source/self-hosted-vms)，在本機完成瀏覽器流程，再透過 SSH 把憑證檔安全匯入 VM，並依指南使用 VM 自己的 Host ID（不覆蓋 VM 的 `host-id`）；之後由 VM 負責 refresh，本機不再使用同一份授權。這是一個獨立、明確的授權設定流程，不是一般的資料搬移或 project 移交。
- **Claude Code runtime adapter**：登入、token 更新與 session storage 由原始官方 Claude Code CLI 管理；Pots 啟動 CLI 並串接 runtime 結果，不把訂閱 token 抽出成 Pots 的 provider 憑證。CLI 的官方登入狀態與 session 儲存需要受保護、可寫入的持久位置，跨 runtime 重啟保留，不能簡化成通用的唯讀 credentials 掛載。
- **資料與秘密分離**：API key、OAuth access／refresh token、Claude CLI 登入狀態、客戶端裝置 token 等秘密，不納入一般 `export`／`import`、project 移交包或專案 artifact。換實例時要在目標實例另外完成官方授權，或走明確的安全匯入流程；日誌與錯誤訊息也不得洩露 token。

以上為設計要求，不代表端到端授權、輪替與自架流程已驗證；SIWC preview 限制與官方文件見[參考資料](references.md#帳號與模型整合)。

### 切換點

```rust
pub struct Backends {
    pub instance: InstanceId,           // 實例的永久 ID，初始化時產生
    pub store:    Arc<dyn Store>,       // 任務樹、對話流、OM、核准
    pub queue:    Arc<dyn RunQueue>,    // agent_runs 的取用與 lease
    pub sources:  Arc<SourceRegistry>,  // 各 connector：webhook、輪詢或長連線
    pub notifier: Arc<dyn Notifier>,    // 桌面通知、手機推播與已連線客戶端，可多個並用
    pub auth:     Arc<dyn Auth>,        // Pots 存取：本機 token、passkey 或 forward-auth；非 provider OAuth
}
pub struct Config {
    pub store:  StoreConfig,            // Sqlite { path } | Postgres { url }
    pub listen: ListenConfig,           // Loopback | Bind { addr, public_url: Option<Url> }
    pub auth:   AuthConfig,             // LocalToken | Passkey | ForwardAuth { trusted_proxy }
    pub notify: Vec<NotifyConfig>,      // Desktop | UnifiedPush { vapid } | ConnectedClients
    pub ingest: Prefer,                 // Poll | Push
    pub sources: Vec<SourceConfig>,     // 各 connector 的非秘密設定
    pub nodes:  Option<NodeListener>,   // node 專用 listener
}
impl Backends {
    pub async fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        cfg.validate()?;  // 拒絕不合理的組合
        let (store, queue): (Arc<dyn Store>, Arc<dyn RunQueue>) = match &cfg.store {
            StoreConfig::Sqlite { path } => {
                let db = SqlitePool::connect(&sqlite_url(path)).await?;
                (Arc::new(SqlStore::sqlite(db.clone())), Arc::new(InProcQueue::new(db)))
            }
            StoreConfig::Postgres { url } => {
                let db = PgPool::connect(url).await?;
                (Arc::new(SqlStore::postgres(db.clone())), Arc::new(PgQueue::new(db)))
            }
        };
        Ok(Self {
            instance: store.instance_id().await?,
            sources:  SourceRegistry::build(&cfg.sources, cfg.effective_ingest())?,
            notifier: Arc::new(FanOutNotifier::build(&cfg.notify)?),
            auth:     cfg.auth.build()?,
            store,
            queue,
        })
    }
}
```

「桌面版」和「多設備版」只是預先填好的 `Config`，例如 `Config::desktop(data_dir)`。其他模組（agent、OM、watcher 的比對邏輯、客戶端）只依賴這些介面，不知道自己跑在哪種形態。介面中的 async 方法用 `async-trait` 或回傳 boxed future，才能放進 `dyn`。

### 設定驗證

- 只要 `listen` 不是 loopback，`auth` 就不可為 `LocalToken`；即使是私有網路，非本機連線也必須登入。node 專用 listener 不受此限，因為它只接受 node 簽章。
- passkey 需要 HTTPS 與固定網域：私有網路可以用 `tailscale serve` 的憑證，對外則透過反向代理。
- `ingest = Push` 需要 `public_url`，否則啟動時報錯；沒有公開網址的實例一律輪詢。UnifiedPush 推播只需要能連出到 ntfy 伺服器，不需要 `public_url`；但手機收到推播後要能連回實例，所以 `listen` 是 loopback 時不能啟用。
- `ForwardAuth` 必須設定 `trusted_proxy`，只接受來自該代理的身分標頭。
- SQLite 只能搭配單一 runner；要多個 runner 就必須用 Postgres。

## 多實例並存

### 實例身分與 project 歸屬

- 每個實例初始化時產生永久的 `instance_id`（UUIDv7），存在資料庫的 `instance` 表裡，客戶端用它辨識連線對象。Project 等 ID 都是 UUIDv7，跨實例也不會撞號，客戶端以 `(instance_id, project_id)` 當索引。
- **每個 project 同一時間只歸屬一個實例。** 只有擁有它的實例會排程 agent run、監看它的外部來源、執行它的核准、操作它的 node。這條規則避免兩個實例監看同一個 repo 時重複派工、重複留言、搶著 merge。
- 使用者層級的資料（USER.md、總管對話與總管排程、託管電腦的配對）屬於實例，不屬於 project，每個實例各自一份。

### 客戶端多連線

- 客戶端維護一份連線清單：桌面版會自動加入本機的內嵌服務，另外可以加入任意數量的遠端實例。每個連線各自有一條 WebSocket、自己的 `last_seq` 與重連狀態，一個斷線不影響其他連線。
- 連遠端實例時，客戶端開瀏覽器到該實例完成 passkey 或 forward-auth 登入，再經 loopback callback 換到一組裝置 token。token 存在 OS keychain，在實例的「裝置」頁可以撤銷。
- 切換實例用最左側的**實例直列**（類似 Discord 的伺服器列），不用下拉選單，也不在同一個側欄裡依實例分組。下拉選單會把其他實例的待核准與斷線狀態藏起來；側欄分組則會讓總管、自動化、電腦這些屬於實例的項目每組重複一份。
  - 直列由上到下是：全域 Inbox、各實例圖示（桌面版的內嵌服務固定排第一）、「加入實例」。實例圖示上顯示待處理數（核准與需要你處理的事，不含找你的訊息），斷線時加上警示。
  - 只連一個實例時整條直列隱藏，只用桌面版的人看到的畫面不變。
  - 直列右邊是「目前實例」的側欄：總管、這個實例的 Inbox、專案、自動化、電腦。側欄頂端顯示實例名稱與形態（桌面版或多設備版），點下去是這個實例的選單：裝置管理、移交 project、中斷連線。
- 全域 Inbox 合併所有連線的待核准，每筆標示所屬實例，可以依實例篩選；決定只送回該實例。側欄裡的 Inbox 就是篩選成該實例的同一個畫面。
- 總管是各實例自己的：客戶端依目前選取的實例叫出對應的總管。v1 不做跨實例總管。
- 手機通常只連得到多設備版（桌面版只綁本機），所以手機不放直列；連到兩個以上實例時，在頂端放實例切換器。

### Project 移交

移交取代原規劃中「M8 把整份單機資料搬上自架平台」的做法：可以只移交一個 project，雙向都可以，例如把需要 24 小時跑的 project 移到伺服器，或把只想放在本機的 project 移回桌面版。

1. **凍結。** 來源實例把 project 標為 `moving`：停止派工，進行中的 run 中斷後回到 `Planned` 或 `NeedsHuman`，停用該 project 的 subscriptions、schedules 與 listeners，待決的核准一律作廢。
2. **打包。** 以 JSONL 匯出該 project 的應用資料：原始對話、OM 觀察、任務樹與 `task_events`、artifact 與版本、schedules、listeners，以及 sources 的非秘密設定。移交包帶有 `transfer_id` 與來源 `instance_id`。
3. **匯入。** 目標實例匯入並驗證移交包，重新請你啟用 sources 與自動化，並補上缺少的秘密（例如目標實例的 GitHub 授權）。作廢的核准在目標實例重新發出，由目標實例的核准執行器執行。
4. **封存。** 匯入成功並回報來源後，來源實例把 project 標為 `moved`，只保留唯讀紀錄與指向新實例的連結，不再派工。匯入失敗時，來源實例可以解除凍結。
5. **取回。** 要移回時，走同一套流程反方向進行。來源實例不會自己解除 `moved`，避免兩邊同時運作。

不隨 project 移交的東西：worker 的容器與 worktree、Claude Code 的 session（官方 session 存在來源 runtime 上，目標實例會重新開 worker，從任務分支接著做）、node 配對、`run_media` 截圖，以及所有秘密。

### 讓移交便宜的規則

- SQL 只用兩邊都支援的寫法。UUID 與時間存成 TEXT 或 i64，JSON 存 TEXT 由應用層 serde 處理；不用 Postgres 陣列，`repos[]`、`matched_tasks[]` 改成關聯表。
- migration 分兩個目錄（`migrations/sqlite`、`migrations/postgres`），同一套整合測試對兩個後端都跑；桌面版的預設組合也在 CI 裡跑。
- 從第一天就帶 `user_id` 與 `instance_id`。桌面版的 user 固定為本機使用者，以後不必回頭補欄位。
- 匯出格式就是移交包格式。整個實例的 `export`／`import` 等於一次移交全部 project，再加上 USER.md 與總管資料；一樣不含 provider token、API key、CLI 登入狀態等秘密，也不複製 runtime Host ID。
- 客戶端不分形態。同一個 GPUI 客戶端連桌面版的內嵌服務或遠端實例，畫面與協定都一樣。

### 什麼時候需要多設備版

出現以下任一需求時，就加開一個多設備版實例，再把相關 project 移交過去：想在手機上即時核准、想讓 agent 在電腦關機時繼續跑、想收 webhook 而不是輪詢。其他 project 可以繼續留在桌面版。

[託管電腦](computer-nodes.md)不需要多設備版：桌面版開啟 node 專用 listener 後，node 就能主動連入。但 node 不會自己做決定，Pots 停止時 node 也會閒置；如果希望電腦關機時 agent 繼續操作 node，就要把 node 配對到跑在常開機器上的實例。
