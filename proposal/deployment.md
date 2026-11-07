# 部署模式與分階段策略

[← 產品提案索引](product.md)

## 分階段：先單機，後自架

第一階段在自己的電腦上跑單機版，把對話、記憶、任務樹與 worker 流程調順；第二階段再加上登入、對外網路與手機推播，變成自架平台。兩個階段是同一份程式碼，差異收在五個介面後面，由設定檔切換。

| 面向 | 單機版（第一階段） | 自架版（第二階段） |
| --- | --- | --- |
| 執行方式 | 本機執行一個 binary，桌面客戶端連 localhost | 同一個 binary，以 docker compose 部署在伺服器 |
| 資料庫 | SQLite（WAL 模式），一個檔案 | Postgres |
| 存取與登入 | 只綁 127.0.0.1，不需登入 | 反向代理加 TLS，passkey 或 forward-auth |
| 外部事件 | 輪詢與 IMAP IDLE（GitHub 用 PAT 或 `gh` 的登入） | 各來源優先用 webhook，輪詢作備援 |
| 通知 | 作業系統桌面通知 | 手機推播（方式待定，候選為 Web Push） |
| 工作佇列 | 行程內排程器，SQLite 表作持久化 | `FOR UPDATE SKIP LOCKED` ，可多個 runner |
| 憑證 | 本機設定檔或 OS keychain | 環境變數或 secret 檔 |
| Worker 沙箱 | 本機 Docker/Podman | 伺服器上的 Docker/Podman |

### 切換點

```rust
pub struct Backends {
    pub store:    Arc<dyn Store>,       // 任務樹、對話流、OM、核准
    pub queue:    Arc<dyn RunQueue>,    // agent_runs 的取用與 lease
    pub sources:  Arc<SourceRegistry>,   // 各 connector：webhook、輪詢或長連線
    pub notifier: Arc<dyn Notifier>,    // 桌面通知或手機推播
    pub auth:     Arc<dyn Auth>,        // 本機免登入、passkey 或 forward-auth
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

- 匯出與匯入。 提供 `export` 與 `import` 指令，以 JSONL 搬移所有資料，包含原始對話、觀察紀錄與 USER.md，搬上自架版後 agent 的記憶不中斷。

- 客戶端不分模式。 同一個 GPUI 桌面客戶端在單機版連 localhost，在自架版連伺服器網址，畫面與協定相同。

### 何時進入第二階段

出現以下任一需求時再搬：想在手機上即時核准、想讓 agent 在電腦關機時繼續跑、或想收 webhook 而不是輪詢。在那之前若只是偶爾想從其他裝置連線，可以把單機版綁在 Tailscale 等私有網路介面上，不必對外開放。

