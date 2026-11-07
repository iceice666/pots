# 部署模式與分階段策略

[← 產品提案索引](product.md)

## 分階段：先單機，後自架

第一階段在自己的電腦上跑單機版，把對話、記憶、任務樹與 worker 流程調順；第二階段再加上登入、對外網路與手機推播，變成自架平台。兩個階段是同一份程式碼，差異收在五個介面後面，由設定檔切換。

| 面向 | 單機版（第一階段） | 自架版（第二階段） |
| --- | --- | --- |
| 執行方式 | 本機執行一個 binary，桌面客戶端連 localhost | 同一個 binary，以 docker compose 部署在伺服器 |
| 資料庫 | SQLite（WAL 模式），一個檔案 | Postgres |
| Pots 應用程式存取與登入 | 只綁 127.0.0.1，不需 Pots 登入；模型 provider 仍須另行授權 | 反向代理加 TLS，passkey 或 forward-auth；不取代模型 provider 授權 |
| 外部事件 | 輪詢與 IMAP IDLE（GitHub 用 PAT 或 `gh` 的登入） | 各來源優先用 webhook，輪詢作備援 |
| 通知 | 作業系統桌面通知 | 手機推播（方式待定，候選為 Web Push） |
| 工作佇列 | 行程內排程器，SQLite 表作持久化 | `FOR UPDATE SKIP LOCKED` ，可多個 runner |
| 憑證 | 靜態 API key 使用受保護設定或 OS keychain；可輪替 OAuth token 使用可寫入的持久 secret store | 靜態 API key 使用 secret 檔等注入；可輪替 OAuth token 使用可寫入的持久 secret store |
| Worker 沙箱 | 本機 Docker/Podman | 伺服器上的 Docker/Podman |

### 模型授權與持久化邊界

Pots 是使用者自行執行的開源軟體；上述自架模式不代表 Pots 營運平台代管、轉售訂閱或共用訂閱憑證。啟動 binary／Compose 只完成服務部署，還須完成各 provider 的官方授權。`Backends.auth` 只處理 Pots 的存取控制，不能代替下列模型授權。

- **ChatGPT Plan provider（rig）**：採官方 Sign in with ChatGPT（SIWC）開源 token-sharing 流程，由 Pots 管理取得的 OAuth 狀態，直接呼叫 Responses API；不透過 Codex CLI，也不讀取 Codex 私有憑證。每份授權只設一個 refresh owner（多 runner 時仍須協調成單一更新者），在 token 輪替時原子更新持久 secret store，避免併發 refresh 或重啟後使用舊 refresh token。不可把會輪替的 token 僅放在唯讀環境變數或唯讀掛載內。
- **Host ID**：綁定持久 runtime 安裝實例並跨重啟保存，不為每個 task container 重新建立。自架 VM 使用自己的 Host ID，不複製本機 runtime 的 Host ID。
- **自架登入**：SIWC 的瀏覽器登入使用本機 loopback callback，不能把伺服器網址當成 callback 任意替換。自架 VM 依[官方指南](https://developers.openai.com/siwc/token-sharing-open-source/self-hosted-vms)，在本機完成瀏覽器流程，再透過 SSH 將授權安全匯入 VM，並依指南使用 VM 自己的 Host ID；這是獨立、明確的授權設定流程，不是一般資料搬移。
- **Claude Code runtime adapter**：登入、token 更新與 session storage 由原始官方 Claude Code CLI 管理；Pots 啟動 CLI 並串接 runtime 結果，不把訂閱 token 抽出成 rig provider 憑證。CLI 的官方登入狀態與 session 儲存須有受保護、可寫入的持久化位置，跨 runtime 重啟保留，不能簡化成通用的唯讀 credentials 掛載。
- **資料與秘密分離**：API key、OAuth access／refresh token、Claude CLI 登入狀態等秘密，不納入一般 `export`／`import` 或專案 artifact。搬家時另外完成官方授權或明確的安全匯入；日誌與錯誤訊息也不得洩露 token。

以上為設計要求，不代表端到端授權、輪替與自架流程已驗證；SIWC preview 限制與官方文件見[參考資料](references.md#帳號與模型整合)。

### 切換點

```rust
pub struct Backends {
    pub store:    Arc<dyn Store>,       // 任務樹、對話流、OM、核准
    pub queue:    Arc<dyn RunQueue>,    // agent_runs 的取用與 lease
    pub sources:  Arc<SourceRegistry>,   // 各 connector：webhook、輪詢或長連線
    pub notifier: Arc<dyn Notifier>,    // 桌面通知或手機推播
    pub auth:     Arc<dyn Auth>,        // Pots 存取：本機免登入、passkey 或 forward-auth；非 provider OAuth
}
pub enum Mode {
    Local  { db_path: PathBuf },
    Hosted { database_url: String, public_url: Url, auth: AuthMode },
}
impl Backends {
    pub async fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        match &cfg.mode {
            Mode::Local { db_path } => {
                let db = SqlitePool::connect(&sqlite_url(db_path)).await?;
                Ok(Self {
                    store:    Arc::new(SqlStore::sqlite(db.clone())),
                    queue:    Arc::new(InProcQueue::new(db)),
                    sources:  SourceRegistry::build(&cfg.sources,Prefer::Poll)?,
                    notifier: Arc::new(DesktopNotifier),
                    auth:     Arc::new(LocalOnly),
                })
            }
            Mode::Hosted { database_url, auth, .. } => {
                let db = PgPool::connect(database_url).await?;
                Ok(Self {
                    store:    Arc::new(SqlStore::postgres(db.clone())),
                    queue:    Arc::new(PgQueue::new(db)),
                    sources:  SourceRegistry::build(&cfg.sources,Prefer::Push)?,
                    notifier: Arc::new(WebPushNotifier::new(cfg.vapid()?)),
                    auth:     auth.build()?,
                })
            }
        }
    }
}
```

其他模組（agent、OM、watcher 的比對邏輯、客戶端）只依賴這五個介面，不知道自己跑在哪個模式。介面中的 async 方法用 `async-trait` 或回傳 boxed future，才能放進 `dyn` 。

### 讓搬家便宜的規則

- SQL 只用兩邊都支援的寫法。 UUID 與時間存成 TEXT 或 i64，JSON 存 TEXT 由應用層 serde 處理；不用 Postgres 陣列，`repos[]` 、`matched_tasks[]` 改成關聯表。

- migration 分兩個目錄（`migrations/sqlite` 、`migrations/postgres` ），同一套整合測試對兩個後端都跑。

- 從第一天就帶 `user_id` 。 單機版固定為本機使用者，自架版不必回頭補欄位。

- 匯出與匯入。 提供 `export` 與 `import` 指令，以 JSONL 搬移應用資料，包含原始對話、觀察紀錄與 USER.md，搬上自架版後 agent 的記憶不中斷。一般搬移不含 provider token、API key、CLI 登入狀態等秘密，也不複製 runtime Host ID；目標 runtime 須另行完成授權與秘密設定。

- 客戶端不分模式。 同一個 GPUI 桌面客戶端在單機版連 localhost，在自架版連伺服器網址，畫面與協定相同。

### 何時進入第二階段

出現以下任一需求時再搬：想在手機上即時核准、想讓 agent 在電腦關機時繼續跑、或想收 webhook 而不是輪詢。在那之前若只是偶爾想從其他裝置連線，可以把單機版綁在 Tailscale 等私有網路介面上，不必對外開放。

[託管電腦](computer-nodes.md)不必等到第二階段：單機版綁定私有網路介面後，node 就能主動連入。但 node 不會自己做決定，Pots 停止時 node 也會閒置；如果希望電腦關機時 agent 繼續操作 node，就要讓 Pots 本身跑在常開的機器上。

