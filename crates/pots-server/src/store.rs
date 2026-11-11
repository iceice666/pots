//! 資料庫存取。M0 只有 SQLite；SQL 都是可攜寫法，之後加 Postgres 時把這層換成 trait
//! 或 `SqlStore::postgres`（`proposal/deployment.md#切換點`）。
//!
//! 查詢一律在執行期檢查（`sqlx::query`），建置時不需要 `DATABASE_URL`。
//! 寫入方法收 `&mut SqliteConnection`，讓呼叫端把狀態變更與事件日誌放在同一個 transaction。

use std::path::Path;
use std::time::Duration;

use pots_llm::Item;
use pots_proto::{
    Author, InstanceId, ItemId, ObservationId, Priority, ProjectId, ProviderHeader, ProviderId,
    ProviderKind, ServerEvent, StreamItemKind, StreamItemView, StreamScope, Timestamp, WorkDir,
};
use serde::Serialize;
use sqlx::Row;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{
    Sqlite, SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePool,
    SqlitePoolOptions, SqliteSynchronous,
};
use uuid::Uuid;

use crate::Error;

/// 桌面版固定的本機使用者。所有屬於使用者的列從第一天就帶 `user_id`。
pub const LOCAL_USER_ID: Uuid = Uuid::from_u64_pair(0, 1);

/// 總管對話的儲存位置。它是一列 `kind = 'overseer'` 的 project，和 project 共用對話流、
/// 記憶與 run 儲存，但不出現在 project 列表與 project 快照裡。
pub const OVERSIGHT_PROJECT_ID: ProjectId = ProjectId::from_u64(0x0e5e_4a11);

/// 對話流屬於哪一條（總管用 `Overseer`，其餘是 project）。
pub fn stream_scope(project: ProjectId) -> StreamScope {
    if project == OVERSIGHT_PROJECT_ID {
        StreamScope::Overseer
    } else {
        StreamScope::Project(project)
    }
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/sqlite");

#[derive(Clone, Debug)]
pub struct ProjectRow {
    pub id: ProjectId,
    pub name: String,
    pub instructions: String,
    /// 這個 project 的負責人（對話 agent）顯示的名字。預設是 [`default_agent_name`]。
    pub agent_name: String,
    /// 工作資料夾；`None` 表示 onboarding 還沒設定。
    pub workdir: Option<WorkDir>,
    pub created_at: Timestamp,
}

/// 新 project 負責人的預設名字。`0013_project_lead.sql` 也用同一個格式轉換舊資料。
pub fn default_agent_name(project: &str) -> String {
    format!("{} 負責人", project.trim())
}

/// `model_connections` 的一列。不含 API key。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionRow {
    pub id: ProviderId,
    pub kind: ProviderKind,
    pub name: String,
    pub base_url: String,
    pub model: String,
    /// 每次請求額外送出的 header；值不是秘密。
    pub headers: Vec<ProviderHeader>,
    /// API key 檔案相對於資料目錄的路徑。
    pub secret_ref: Option<String>,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug)]
pub struct InstanceRow {
    pub id: InstanceId,
    pub name: String,
    pub created_at: Timestamp,
}

/// `stream_items.status`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemStatus {
    /// 人類訊息已存，還沒寫進 `om_raw`（排隊中）。
    Pending,
    Streaming,
    Done,
    Failed,
}

impl ItemStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Streaming => "streaming",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// `agent_runs.state`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl RunState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// `run_trace` 的一列。
#[derive(Clone, Debug, PartialEq)]
pub struct TraceRow {
    pub seq: i64,
    /// `model`、`text`（模型在工具呼叫之間說的話）或 `tool`。
    pub kind: String,
    pub name: String,
    pub detail: serde_json::Value,
    pub ok: bool,
    pub at: Timestamp,
}

/// `om_raw` 的一列。
#[derive(Clone, Debug)]
pub struct RawRow {
    pub seq: i64,
    pub item: Item,
    pub tokens: u64,
    pub created_at: Timestamp,
}

/// 觀察是誰寫的（`om_observations.source`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationSource {
    Observer,
    Reflector,
    /// 你修正過的。
    Human,
}

impl ObservationSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Reflector => "reflector",
            Self::Human => "human",
        }
    }

    fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "observer" => Ok(Self::Observer),
            "reflector" => Ok(Self::Reflector),
            "human" => Ok(Self::Human),
            _ => Err(Error::Corrupt(format!("觀察的來源不對：{s}"))),
        }
    }
}

/// `om_observations` 的狀態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationState {
    Active,
    /// 被你修正後的版本取代。
    Superseded,
    Deleted,
}

impl ObservationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Deleted => "deleted",
        }
    }

    fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "active" => Ok(Self::Active),
            "superseded" => Ok(Self::Superseded),
            "deleted" => Ok(Self::Deleted),
            _ => Err(Error::Corrupt(format!("觀察的狀態不對：{s}"))),
        }
    }
}

/// `om_observations` 的一列。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationRow {
    pub id: ObservationId,
    pub generation: i64,
    pub seq: i64,
    pub date: String,
    pub priority: Priority,
    pub text: String,
    pub pinned: bool,
    pub source: ObservationSource,
    pub state: ObservationState,
    pub replaces: Option<ObservationId>,
}

/// `om_state` 的一列；沒有這列時是 [`Default`]。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OmStateRow {
    pub generation: i64,
    pub current_task: String,
    pub revision: i64,
    pub reflected_tokens: u64,
    pub last_error: Option<String>,
}

fn priority_str(p: Priority) -> &'static str {
    match p {
        Priority::High => "high",
        Priority::Medium => "medium",
        Priority::Low => "low",
    }
}

fn parse_priority(s: &str) -> Result<Priority, Error> {
    match s {
        "high" => Ok(Priority::High),
        "medium" => Ok(Priority::Medium),
        "low" => Ok(Priority::Low),
        _ => Err(Error::Corrupt(format!("觀察的優先序不對：{s}"))),
    }
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// 開啟（必要時建立）SQLite 資料庫並跑 migration。WAL 模式。
    pub async fn open_sqlite(path: &Path) -> Result<Self, Error> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// 寫入用的 transaction。一律 `BEGIN IMMEDIATE`：一開始就拿寫入鎖（拿不到時依
    /// busy_timeout 等待）。預設的 deferred transaction 先讀後寫時，若另一條連線正在寫，
    /// WAL 下讀鎖升級會直接回 SQLITE_BUSY，busy_timeout 也救不了。
    pub async fn begin(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, Error> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }

    /// 只讀的 transaction：幾個查詢看到同一個快照（WAL），不拿寫入鎖。
    pub async fn read(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, Error> {
        Ok(self.pool.begin().await?)
    }

    /// 一條連線，給收 `&mut SqliteConnection` 的讀取方法用。
    pub async fn acquire(&self) -> Result<PoolConnection<Sqlite>, Error> {
        Ok(self.pool.acquire().await?)
    }

    // -----------------------------------------------------------------------
    // instance

    /// 讀取實例身分；第一次啟動時建立。
    pub async fn ensure_instance(&self, default_name: &str) -> Result<InstanceRow, Error> {
        let mut tx = self.begin().await?;
        let row = sqlx::query("SELECT id, name, created_at FROM instance LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?;
        let inst = match row {
            Some(row) => InstanceRow {
                id: parse_id(row.try_get("id")?)?,
                name: row.try_get("name")?,
                created_at: Timestamp(row.try_get("created_at")?),
            },
            None => {
                let inst = InstanceRow {
                    id: InstanceId::new(),
                    name: default_name.to_string(),
                    created_at: Timestamp::now(),
                };
                sqlx::query("INSERT INTO instance (id, name, created_at) VALUES (?, ?, ?)")
                    .bind(inst.id.to_string())
                    .bind(&inst.name)
                    .bind(inst.created_at.0)
                    .execute(&mut *tx)
                    .await?;
                inst
            }
        };
        tx.commit().await?;
        Ok(inst)
    }

    // -----------------------------------------------------------------------
    // projects

    pub async fn list_projects(&self) -> Result<Vec<ProjectRow>, Error> {
        let rows = sqlx::query(
            "SELECT id, name, instructions, agent_name, workdir_kind, workdir, created_at \
             FROM projects WHERE user_id = ? AND residency = 'active' AND kind = 'project' \
             ORDER BY created_at, id",
        )
        .bind(LOCAL_USER_ID.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(project_from_row).collect()
    }

    pub async fn project(&self, id: ProjectId) -> Result<Option<ProjectRow>, Error> {
        let row = sqlx::query(
            "SELECT id, name, instructions, agent_name, workdir_kind, workdir, created_at \
             FROM projects WHERE id = ? AND user_id = ?",
        )
        .bind(id.to_string())
        .bind(LOCAL_USER_ID.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(project_from_row).transpose()
    }

    /// 建立總管對話的內部 project（只在第一次啟動時寫入）。
    pub async fn ensure_overseer(&self) -> Result<(), Error> {
        sqlx::query(
            "INSERT OR IGNORE INTO projects (id, user_id, name, instructions, agent_name, \
             residency, kind, created_at) VALUES (?, ?, '總管', '', '總管', 'active', 'overseer', ?)",
        )
        .bind(OVERSIGHT_PROJECT_ID.to_string())
        .bind(LOCAL_USER_ID.to_string())
        .bind(Timestamp::now().0)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 用 UUID 或名稱找 project（總管派工用）。
    pub async fn find_project(&self, selector: &str) -> Result<Option<ProjectRow>, Error> {
        if let Ok(id) = selector.trim().parse::<ProjectId>()
            && let Some(p) = self.project(id).await?
            && self.project_kind(id).await?.as_deref() == Some("project")
        {
            return Ok(Some(p));
        }
        let name = selector.trim();
        Ok(self
            .list_projects()
            .await?
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(name)))
    }

    pub async fn insert_project(conn: &mut SqliteConnection, p: &ProjectRow) -> Result<(), Error> {
        let (kind, value) = workdir_columns(p.workdir.as_ref());
        sqlx::query(
            "INSERT INTO projects (id, user_id, name, instructions, agent_name, residency, \
             workdir_kind, workdir, created_at) \
             VALUES (?, ?, ?, ?, ?, 'active', ?, ?, ?)",
        )
        .bind(p.id.to_string())
        .bind(LOCAL_USER_ID.to_string())
        .bind(&p.name)
        .bind(&p.instructions)
        .bind(&p.agent_name)
        .bind(kind)
        .bind(value)
        .bind(p.created_at.0)
        .execute(conn)
        .await?;
        Ok(())
    }

    /// 一個 project 的工作資料夾（不含總管的內部 project）。
    pub async fn workdir(
        conn: &mut SqliteConnection,
        id: ProjectId,
    ) -> Result<Option<WorkDir>, Error> {
        let row = sqlx::query("SELECT workdir_kind, workdir FROM projects WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(conn)
            .await?;
        row.as_ref()
            .map(workdir_from_row)
            .transpose()
            .map(Option::flatten)
    }

    /// 設定或清除 project 的工作資料夾。
    pub async fn set_workdir(
        conn: &mut SqliteConnection,
        id: ProjectId,
        workdir: Option<&WorkDir>,
    ) -> Result<(), Error> {
        let (kind, value) = workdir_columns(workdir);
        sqlx::query("UPDATE projects SET workdir_kind = ?, workdir = ? WHERE id = ?")
            .bind(kind)
            .bind(value)
            .bind(id.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    /// 幫 project 的負責人改名字。
    pub async fn set_agent_name(
        conn: &mut SqliteConnection,
        id: ProjectId,
        name: &str,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE projects SET agent_name = ? WHERE id = ?")
            .bind(name)
            .bind(id.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // stream_items

    /// 最近 `limit` 筆，由舊到新。
    pub async fn latest_items(
        &self,
        project: ProjectId,
        limit: u32,
    ) -> Result<Vec<StreamItemView>, Error> {
        let rows = sqlx::query(
            "SELECT id, body, created_at FROM stream_items WHERE project_id = ? \
             ORDER BY ord DESC LIMIT ?",
        )
        .bind(project.to_string())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        let mut items = rows
            .iter()
            .map(item_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        items.reverse();
        Ok(items)
    }

    /// `before` 之前的 `limit` 筆，由舊到新。`before` 不屬於這個 project 時回 `None`。
    pub async fn items_before(
        &self,
        project: ProjectId,
        before: ItemId,
        limit: u32,
    ) -> Result<Option<Vec<StreamItemView>>, Error> {
        let ord: Option<i64> =
            sqlx::query_scalar("SELECT ord FROM stream_items WHERE id = ? AND project_id = ?")
                .bind(before.to_string())
                .bind(project.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let Some(ord) = ord else {
            return Ok(None);
        };
        let rows = sqlx::query(
            "SELECT id, body, created_at FROM stream_items WHERE project_id = ? AND ord < ? \
             ORDER BY ord DESC LIMIT ?",
        )
        .bind(project.to_string())
        .bind(ord)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        let mut items = rows
            .iter()
            .map(item_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        items.reverse();
        Ok(Some(items))
    }

    /// 這個 ID 的 item 屬於哪個 project。
    pub async fn item_project(
        conn: &mut SqliteConnection,
        id: ItemId,
    ) -> Result<Option<ProjectId>, Error> {
        let pid: Option<String> =
            sqlx::query_scalar("SELECT project_id FROM stream_items WHERE id = ?")
                .bind(id.to_string())
                .fetch_optional(conn)
                .await?;
        pid.map(parse_id).transpose()
    }

    pub async fn item(
        conn: &mut SqliteConnection,
        id: ItemId,
    ) -> Result<Option<StreamItemView>, Error> {
        let row = sqlx::query("SELECT id, body, created_at FROM stream_items WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(conn)
            .await?;
        row.as_ref().map(item_from_row).transpose()
    }

    pub async fn insert_item(
        conn: &mut SqliteConnection,
        project: ProjectId,
        item: &StreamItemView,
        status: ItemStatus,
        run_id: Option<Uuid>,
    ) -> Result<(), Error> {
        let (kind, author) = kind_and_author(&item.kind);
        sqlx::query(
            "INSERT INTO stream_items (id, project_id, ord, author, kind, body, status, run_id, created_at) \
             VALUES (?, ?, (SELECT COALESCE(MAX(ord), 0) + 1 FROM stream_items WHERE project_id = ?), \
             ?, ?, ?, ?, ?, ?)",
        )
        .bind(item.id.to_string())
        .bind(project.to_string())
        .bind(project.to_string())
        .bind(author.map(|a| to_json(&a)).transpose()?)
        .bind(kind)
        .bind(to_json(&item.kind)?)
        .bind(status.as_str())
        .bind(run_id.map(|r| r.to_string()))
        .bind(item.at.0)
        .execute(conn)
        .await?;
        Ok(())
    }

    pub async fn update_item(
        conn: &mut SqliteConnection,
        item: &StreamItemView,
        status: ItemStatus,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE stream_items SET body = ?, status = ? WHERE id = ?")
            .bind(to_json(&item.kind)?)
            .bind(status.as_str())
            .bind(item.id.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    /// 人類訊息寫進 `om_raw` 之後標成 done。
    pub async fn mark_consumed(
        conn: &mut SqliteConnection,
        item: ItemId,
        run_id: Option<Uuid>,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE stream_items SET status = 'done', run_id = ? WHERE id = ?")
            .bind(run_id.map(|r| r.to_string()))
            .bind(item.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    /// 人類與 agent 訊息的數量（不含卡片與事件）。
    pub async fn count_messages(&self, project: ProjectId) -> Result<u64, Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM stream_items WHERE project_id = ? AND kind IN ('human', 'agent')",
        )
        .bind(project.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(n as u64)
    }

    /// 最近 `limit` 張卡片，由新到舊。
    pub async fn recent_cards(
        conn: &mut SqliteConnection,
        project: ProjectId,
        limit: u32,
    ) -> Result<Vec<StreamItemView>, Error> {
        let rows = sqlx::query(
            "SELECT id, body, created_at FROM stream_items WHERE project_id = ? AND kind = 'card' \
             ORDER BY ord DESC LIMIT ?",
        )
        .bind(project.to_string())
        .bind(i64::from(limit))
        .fetch_all(conn)
        .await?;
        rows.iter().map(item_from_row).collect()
    }

    /// 某個狀態的所有 item，依 project 與順序。給啟動時的復原用。
    pub async fn items_with_status(
        conn: &mut SqliteConnection,
        status: ItemStatus,
    ) -> Result<Vec<(ProjectId, StreamItemView)>, Error> {
        let rows = sqlx::query(
            "SELECT project_id, id, body, created_at FROM stream_items WHERE status = ? \
             ORDER BY project_id, ord",
        )
        .bind(status.as_str())
        .fetch_all(conn)
        .await?;
        rows.iter()
            .map(|row| Ok((parse_id(row.try_get("project_id")?)?, item_from_row(row)?)))
            .collect()
    }

    // -----------------------------------------------------------------------
    // events

    pub async fn insert_event(
        conn: &mut SqliteConnection,
        seq: u64,
        json: &str,
        at: Timestamp,
    ) -> Result<(), Error> {
        sqlx::query("INSERT INTO events (seq, body, at) VALUES (?, ?, ?)")
            .bind(seq as i64)
            .bind(json)
            .bind(at.0)
            .execute(conn)
            .await?;
        Ok(())
    }

    /// `(最舊, 最新)` 的事件序號；沒有事件時是 `None`。
    pub async fn event_bounds(&self) -> Result<Option<(u64, u64)>, Error> {
        let row = sqlx::query("SELECT MIN(seq) AS lo, MAX(seq) AS hi FROM events")
            .fetch_one(&self.pool)
            .await?;
        let lo: Option<i64> = row.try_get("lo")?;
        let hi: Option<i64> = row.try_get("hi")?;
        Ok(lo.zip(hi).map(|(lo, hi)| (lo as u64, hi as u64)))
    }

    /// `after` 之後的事件，依序號排序。
    pub async fn events_after(&self, after: u64) -> Result<Vec<(u64, String)>, Error> {
        let rows = sqlx::query("SELECT seq, body FROM events WHERE seq > ? ORDER BY seq")
            .bind(after as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                let seq: i64 = row.try_get("seq")?;
                Ok((seq as u64, row.try_get("body")?))
            })
            .collect()
    }

    /// 只留最新的 `keep` 筆事件。
    pub async fn prune_events(&self, keep: u64) -> Result<u64, Error> {
        let done = sqlx::query("DELETE FROM events WHERE seq <= (SELECT MAX(seq) FROM events) - ?")
            .bind(keep as i64)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }

    // -----------------------------------------------------------------------
    // agent_runs

    pub async fn insert_run(
        conn: &mut SqliteConnection,
        id: Uuid,
        project: ProjectId,
        agent: &str,
        trigger: &serde_json::Value,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO agent_runs (id, project_id, agent, trigger, state, started_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(project.to_string())
        .bind(agent)
        .bind(trigger.to_string())
        .bind(RunState::Running.as_str())
        .bind(Timestamp::now().0)
        .execute(conn)
        .await?;
        Ok(())
    }

    pub async fn finish_run(
        conn: &mut SqliteConnection,
        id: Uuid,
        state: RunState,
        error: Option<&str>,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE agent_runs SET state = ?, error = ?, finished_at = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(error)
            .bind(Timestamp::now().0)
            .bind(id.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    /// 累加一個 run 的模型用量；`after_turn` 每輪呼叫一次。
    pub async fn add_run_usage(
        conn: &mut SqliteConnection,
        id: Uuid,
        input: u64,
        output: u64,
    ) -> Result<(), Error> {
        sqlx::query(
            "UPDATE agent_runs SET input_tokens = input_tokens + ?, output_tokens = output_tokens + ? \
             WHERE id = ?",
        )
        .bind(input as i64)
        .bind(output as i64)
        .bind(id.to_string())
        .execute(conn)
        .await?;
        Ok(())
    }

    /// 記下 run 用的模型與成本維度（「為什麼」面板與成本報表）。
    pub async fn set_run_meta(
        store: &Store,
        id: Uuid,
        meta: &crate::budget::RunMeta,
    ) -> Result<(), Error> {
        sqlx::query(
            "UPDATE agent_runs SET model = ?, provider = ?, billing = ?, pricing_provider = ?, \
             runtime = ? WHERE id = ?",
        )
        .bind(&meta.model)
        .bind(&meta.provider)
        .bind(&meta.billing)
        .bind(&meta.pricing_provider)
        .bind(&meta.runtime)
        .bind(id.to_string())
        .execute(&store.pool)
        .await?;
        Ok(())
    }

    /// 加上 CLI 回報的費用（同一個 run 可能重試多次，累加）。
    pub async fn add_run_cost(
        conn: &mut SqliteConnection,
        id: Uuid,
        cost: f64,
    ) -> Result<(), Error> {
        if !cost.is_finite() || cost < 0.0 {
            return Ok(());
        }
        sqlx::query("UPDATE agent_runs SET cost_usd = COALESCE(cost_usd, 0) + ? WHERE id = ?")
            .bind(cost)
            .bind(id.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    /// Observer／Reflector 的一次呼叫：寫成一列已結束的 run，成本報表才能分開算 OM 開銷。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_om_run(
        conn: &mut SqliteConnection,
        project: ProjectId,
        agent: &str,
        route: &crate::providers::Route,
        usage: pots_llm::provider::Usage,
        started: Timestamp,
        finished: Timestamp,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO agent_runs (id, project_id, agent, trigger, state, started_at, finished_at, \
             model, provider, billing, pricing_provider, runtime, input_tokens, output_tokens) \
             VALUES (?, ?, ?, ?, 'done', ?, ?, ?, ?, ?, ?, 'builtin', ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(project.to_string())
        .bind(agent)
        .bind(r#"{"kind":"om"}"#)
        .bind(started.0)
        .bind(finished.0)
        .bind(&route.model)
        .bind(&route.label)
        .bind(route.billing)
        .bind(&route.pricing_provider)
        .bind(usage.input_tokens as i64)
        .bind(usage.output_tokens as i64)
        .execute(conn)
        .await?;
        Ok(())
    }

    /// run 開始時的記憶版本（chat 與總管有 OM，其他 profile 沒有）。
    pub async fn set_run_memory_revision(
        conn: &mut SqliteConnection,
        id: Uuid,
        revision: i64,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE agent_runs SET memory_revision = ? WHERE id = ?")
            .bind(revision)
            .bind(id.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    /// 記下 run 為什麼停下（`budget`、`breaker`）；完成路徑據此改成申請預算或暫停。
    pub async fn set_run_stop_reason(
        conn: &mut SqliteConnection,
        id: Uuid,
        reason: &str,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE agent_runs SET stop_reason = ? WHERE id = ?")
            .bind(reason)
            .bind(id.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    /// 一個 project 在 `since` 之後用掉多少 token（熔斷）。看每一輪模型的 trace，
    /// 而不是 run 的開始時間，長時間的 run 也照樣算進滾動的一小時。
    pub async fn project_usage_since(
        &self,
        project: ProjectId,
        since: Timestamp,
    ) -> Result<u64, Error> {
        let used: Option<i64> = sqlx::query_scalar(
            "SELECT SUM(t.tokens) FROM run_trace t JOIN agent_runs r ON r.id = t.run_id \
             WHERE r.project_id = ? AND t.kind = 'model' AND t.at >= ?",
        )
        .bind(project.to_string())
        .bind(since.0)
        .fetch_one(&self.pool)
        .await?;
        Ok(used.unwrap_or_default().max(0) as u64)
    }

    pub async fn project_breaker_until(
        &self,
        project: ProjectId,
    ) -> Result<Option<Timestamp>, Error> {
        let until: Option<Option<i64>> =
            sqlx::query_scalar("SELECT breaker_until FROM projects WHERE id = ?")
                .bind(project.to_string())
                .fetch_optional(&self.pool)
                .await?;
        Ok(until.flatten().map(Timestamp))
    }

    pub async fn project_hourly_budget(&self, project: ProjectId) -> Result<Option<u64>, Error> {
        let limit: Option<Option<i64>> =
            sqlx::query_scalar("SELECT hourly_budget_tokens FROM projects WHERE id = ?")
                .bind(project.to_string())
                .fetch_optional(&self.pool)
                .await?;
        Ok(limit.flatten().map(|v| v.max(0) as u64))
    }

    pub async fn project_kind(&self, project: ProjectId) -> Result<Option<String>, Error> {
        Ok(sqlx::query_scalar("SELECT kind FROM projects WHERE id = ?")
            .bind(project.to_string())
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Goal 的預算、已用與「追加被拒絕過」。
    pub async fn goal_budget(
        &self,
        goal: pots_proto::GoalId,
    ) -> Result<Option<(Option<u64>, u64, bool)>, Error> {
        let row = sqlx::query(
            "SELECT budget_tokens, spent_tokens, budget_denied FROM goals WHERE id = ?",
        )
        .bind(goal.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let budget: Option<i64> = row.try_get("budget_tokens")?;
        let spent: i64 = row.try_get("spent_tokens")?;
        let denied: i64 = row.try_get("budget_denied")?;
        Ok(Some((
            budget.map(|v| v.max(0) as u64),
            spent.max(0) as u64,
            denied != 0,
        )))
    }

    /// 追加被拒絕（或逾時）：不再重複申請。
    pub async fn deny_goal_budget(
        conn: &mut SqliteConnection,
        goal: pots_proto::GoalId,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE goals SET budget_denied = 1, updated_at = ? WHERE id = ?")
            .bind(Timestamp::now().0)
            .bind(goal.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    pub async fn mark_project_read(&self, project: ProjectId, at: Timestamp) -> Result<(), Error> {
        sqlx::query("UPDATE projects SET last_read_at = ? WHERE id = ?")
            .bind(at.0)
            .bind(project.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn project_last_read(&self, project: ProjectId) -> Result<Option<Timestamp>, Error> {
        let read: Option<Option<i64>> =
            sqlx::query_scalar("SELECT last_read_at FROM projects WHERE id = ?")
                .bind(project.to_string())
                .fetch_optional(&self.pool)
                .await?;
        Ok(read.flatten().map(Timestamp))
    }

    /// 記一筆 trace；`seq` 接在該 run 的最後一筆之後。
    #[allow(clippy::too_many_arguments)]
    pub async fn append_trace(
        conn: &mut SqliteConnection,
        run: Uuid,
        kind: &str,
        name: &str,
        detail: &serde_json::Value,
        ok: bool,
        tokens: u64,
        at: Timestamp,
    ) -> Result<i64, Error> {
        let seq: i64 = sqlx::query_scalar(
            "INSERT INTO run_trace (run_id, seq, kind, name, detail, ok, tokens, at) VALUES \
             (?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM run_trace WHERE run_id = ?), ?, ?, ?, ?, ?, ?) \
             RETURNING seq",
        )
        .bind(run.to_string())
        .bind(run.to_string())
        .bind(kind)
        .bind(name)
        .bind(detail.to_string())
        .bind(i64::from(ok))
        .bind(tokens as i64)
        .bind(at.0)
        .fetch_one(conn)
        .await?;
        Ok(seq)
    }

    pub async fn trace(&self, run: Uuid) -> Result<Vec<TraceRow>, Error> {
        let rows = sqlx::query(
            "SELECT seq, kind, name, detail, ok, at FROM run_trace WHERE run_id = ? ORDER BY seq",
        )
        .bind(run.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let detail: String = row.try_get("detail")?;
            out.push(TraceRow {
                seq: row.try_get("seq")?,
                kind: row.try_get("kind")?,
                name: row.try_get("name")?,
                detail: serde_json::from_str(&detail).unwrap_or(serde_json::Value::Null),
                ok: row.try_get::<i64, _>("ok")? != 0,
                at: Timestamp(row.try_get("at")?),
            });
        }
        Ok(out)
    }

    /// 上次異常結束時還在跑的 chat（與 subagent）run 一律標成失敗。Planner 與 worker 的 run 在
    /// 持久化佇列裡，由 [`crate::planner::requeue_stale`]、[`crate::worker::requeue_stale`] 放回佇列。
    pub async fn fail_stale_runs(conn: &mut SqliteConnection, error: &str) -> Result<u64, Error> {
        let done = sqlx::query(
            "UPDATE agent_runs SET state = 'failed', error = ?, finished_at = ? \
             WHERE state = 'running' AND agent NOT IN ('planner', 'worker')",
        )
        .bind(error)
        .bind(Timestamp::now().0)
        .execute(conn)
        .await?;
        Ok(done.rows_affected())
    }

    // -----------------------------------------------------------------------
    // om_raw

    /// 追加一筆原始歷史，`seq` 接在這個 project 的最後一筆之後，順便記下估計的 token 數。
    pub async fn append_om_raw(
        conn: &mut SqliteConnection,
        project: ProjectId,
        run_id: Option<Uuid>,
        item: &Item,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO om_raw (project_id, seq, run_id, item, created_at, tokens) \
             VALUES (?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM om_raw WHERE project_id = ?), ?, ?, ?, ?)",
        )
        .bind(project.to_string())
        .bind(project.to_string())
        .bind(run_id.map(|r| r.to_string()))
        .bind(to_json(item)?)
        .bind(Timestamp::now().0)
        .bind(crate::memory::tokens::item(item) as i64)
        .execute(conn)
        .await?;
        Ok(())
    }

    /// 升級前寫入、還沒有 token 數的原始歷史：補算。回傳補了幾筆。
    pub async fn backfill_om_tokens(&self) -> Result<u64, Error> {
        let rows = sqlx::query("SELECT project_id, seq, item FROM om_raw WHERE tokens IS NULL")
            .fetch_all(&self.pool)
            .await?;
        if rows.is_empty() {
            return Ok(0);
        }
        let mut tx = self.begin().await?;
        for row in &rows {
            let project: String = row.try_get("project_id")?;
            let seq: i64 = row.try_get("seq")?;
            let item: String = row.try_get("item")?;
            let tokens = match serde_json::from_str::<Item>(&item) {
                Ok(item) => crate::memory::tokens::item(&item),
                Err(_) => crate::memory::tokens::estimate(&item),
            };
            sqlx::query("UPDATE om_raw SET tokens = ? WHERE project_id = ? AND seq = ?")
                .bind(tokens as i64)
                .bind(project)
                .bind(seq)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(rows.len() as u64)
    }

    /// 還沒整理進觀察的原始歷史，依時間排序。
    pub async fn om_unobserved(
        conn: &mut SqliteConnection,
        project: ProjectId,
    ) -> Result<Vec<RawRow>, Error> {
        let rows = sqlx::query(
            "SELECT seq, item, tokens, created_at FROM om_raw \
             WHERE project_id = ? AND observed = FALSE ORDER BY seq",
        )
        .bind(project.to_string())
        .fetch_all(conn)
        .await?;
        rows.iter()
            .map(|row| {
                let item: String = row.try_get("item")?;
                let item: Item = serde_json::from_str(&item)?;
                let tokens: Option<i64> = row.try_get("tokens")?;
                Ok(RawRow {
                    seq: row.try_get("seq")?,
                    tokens: tokens.map_or_else(|| crate::memory::tokens::item(&item), |t| t as u64),
                    item,
                    created_at: Timestamp(row.try_get("created_at")?),
                })
            })
            .collect()
    }

    /// 還沒整理的原始歷史共幾個 token。
    pub async fn om_unobserved_tokens(
        conn: &mut SqliteConnection,
        project: ProjectId,
    ) -> Result<u64, Error> {
        let n: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(COALESCE(SUM(tokens), 0) AS BIGINT) FROM om_raw \
             WHERE project_id = ? AND observed = FALSE",
        )
        .bind(project.to_string())
        .fetch_one(conn)
        .await?;
        Ok(n.unwrap_or(0).max(0) as u64)
    }

    /// 把 `first..=last` 這段還沒觀察的原始歷史標成已觀察，回傳標了幾筆。
    /// 只標 `observed = FALSE` 的列：同一段已經被觀察過時回傳的數量會少於預期，
    /// 呼叫端據此放棄這次的結果（冪等鍵是 `(project_id, 最後一則 seq)`）。
    pub async fn om_mark_observed(
        conn: &mut SqliteConnection,
        project: ProjectId,
        first: i64,
        last: i64,
    ) -> Result<u64, Error> {
        let done = sqlx::query(
            "UPDATE om_raw SET observed = TRUE \
             WHERE project_id = ? AND seq >= ? AND seq <= ? AND observed = FALSE",
        )
        .bind(project.to_string())
        .bind(first)
        .bind(last)
        .execute(conn)
        .await?;
        Ok(done.rows_affected())
    }

    pub async fn om_state(
        conn: &mut SqliteConnection,
        project: ProjectId,
    ) -> Result<OmStateRow, Error> {
        let row = sqlx::query(
            "SELECT generation, current_task, revision, reflected_tokens, last_error \
             FROM om_state WHERE project_id = ?",
        )
        .bind(project.to_string())
        .fetch_optional(conn)
        .await?;
        let Some(row) = row else {
            return Ok(OmStateRow::default());
        };
        let reflected: i64 = row.try_get("reflected_tokens")?;
        Ok(OmStateRow {
            generation: row.try_get("generation")?,
            current_task: row.try_get("current_task")?,
            revision: row.try_get("revision")?,
            reflected_tokens: reflected.max(0) as u64,
            last_error: row.try_get("last_error")?,
        })
    }

    pub async fn save_om_state(
        conn: &mut SqliteConnection,
        project: ProjectId,
        state: &OmStateRow,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO om_state \
             (project_id, generation, current_task, revision, reflected_tokens, last_error, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (project_id) DO UPDATE SET generation = excluded.generation, \
             current_task = excluded.current_task, revision = excluded.revision, \
             reflected_tokens = excluded.reflected_tokens, last_error = excluded.last_error, \
             updated_at = excluded.updated_at",
        )
        .bind(project.to_string())
        .bind(state.generation)
        .bind(&state.current_task)
        .bind(state.revision)
        .bind(state.reflected_tokens as i64)
        .bind(state.last_error.as_deref())
        .bind(Timestamp::now().0)
        .execute(conn)
        .await?;
        Ok(())
    }

    /// 某一代目前有效的觀察，依順序。
    pub async fn om_observations(
        conn: &mut SqliteConnection,
        project: ProjectId,
        generation: i64,
    ) -> Result<Vec<ObservationRow>, Error> {
        let rows = sqlx::query(
            "SELECT id, generation, seq, date, priority, text, pinned, source, state, replaces \
             FROM om_observations WHERE project_id = ? AND generation = ? AND state = 'active' \
             ORDER BY seq, created_at, id",
        )
        .bind(project.to_string())
        .bind(generation)
        .fetch_all(conn)
        .await?;
        rows.iter().map(observation_from_row).collect()
    }

    /// 一條觀察（不論狀態）；不屬於這個 project 時是 `None`。
    pub async fn om_observation(
        conn: &mut SqliteConnection,
        project: ProjectId,
        id: ObservationId,
    ) -> Result<Option<ObservationRow>, Error> {
        let row = sqlx::query(
            "SELECT id, generation, seq, date, priority, text, pinned, source, state, replaces \
             FROM om_observations WHERE id = ? AND project_id = ?",
        )
        .bind(id.to_string())
        .bind(project.to_string())
        .fetch_optional(conn)
        .await?;
        row.as_ref().map(observation_from_row).transpose()
    }

    /// 這一代下一條觀察的 `seq`。
    pub async fn om_next_seq(
        conn: &mut SqliteConnection,
        project: ProjectId,
        generation: i64,
    ) -> Result<i64, Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM om_observations \
             WHERE project_id = ? AND generation = ?",
        )
        .bind(project.to_string())
        .bind(generation)
        .fetch_one(conn)
        .await?;
        Ok(n)
    }

    pub async fn insert_observation(
        conn: &mut SqliteConnection,
        project: ProjectId,
        row: &ObservationRow,
        through_seq: Option<i64>,
    ) -> Result<(), Error> {
        let now = Timestamp::now().0;
        sqlx::query(
            "INSERT INTO om_observations \
             (id, project_id, generation, seq, date, priority, text, pinned, source, state, \
              replaces, through_seq, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(row.id.to_string())
        .bind(project.to_string())
        .bind(row.generation)
        .bind(row.seq)
        .bind(&row.date)
        .bind(priority_str(row.priority))
        .bind(&row.text)
        .bind(row.pinned)
        .bind(row.source.as_str())
        .bind(row.state.as_str())
        .bind(row.replaces.map(|r| r.to_string()))
        .bind(through_seq)
        .bind(now)
        .bind(now)
        .execute(conn)
        .await?;
        Ok(())
    }

    pub async fn set_observation_state(
        conn: &mut SqliteConnection,
        id: ObservationId,
        state: ObservationState,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE om_observations SET state = ?, updated_at = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(Timestamp::now().0)
            .bind(id.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    pub async fn set_observation_pinned(
        conn: &mut SqliteConnection,
        id: ObservationId,
        pinned: bool,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE om_observations SET pinned = ?, updated_at = ? WHERE id = ?")
            .bind(pinned)
            .bind(Timestamp::now().0)
            .bind(id.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    /// 某一代所有的觀察（含被取代與刪除的），給測試與稽核用。
    #[cfg(test)]
    pub async fn om_generation_rows(
        &self,
        project: ProjectId,
        generation: i64,
    ) -> Result<Vec<ObservationRow>, Error> {
        let rows = sqlx::query(
            "SELECT id, generation, seq, date, priority, text, pinned, source, state, replaces \
             FROM om_observations WHERE project_id = ? AND generation = ? ORDER BY seq, created_at, id",
        )
        .bind(project.to_string())
        .bind(generation)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(observation_from_row).collect()
    }

    // -----------------------------------------------------------------------
    // model_connections 與 instance_settings

    /// 本機使用者的 API provider，依建立時間。
    pub async fn list_connections(&self) -> Result<Vec<ConnectionRow>, Error> {
        let rows = sqlx::query(
            "SELECT id, kind, name, base_url, model, headers, secret_ref, created_at \
             FROM model_connections WHERE user_id = ? ORDER BY created_at, id",
        )
        .bind(LOCAL_USER_ID.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: String = row.try_get("id")?;
            let kind: String = row.try_get("kind")?;
            // 不認得的種類（例如較新版本寫的）略過，不讓整個服務起不來。
            let kind = match serde_json::from_value(serde_json::Value::String(kind.clone())) {
                Ok(k) if k != ProviderKind::ChatGpt => k,
                _ => {
                    tracing::warn!(%id, %kind, "略過不認得種類的 provider");
                    continue;
                }
            };
            let headers: String = row.try_get("headers")?;
            let headers = match serde_json::from_str::<Vec<ProviderHeader>>(&headers) {
                Ok(headers) => headers,
                Err(e) => {
                    tracing::warn!(%id, error = %e, "provider 的 headers 格式不對，當成空的");
                    Vec::new()
                }
            };
            out.push(ConnectionRow {
                id: parse_id(id)?,
                kind,
                name: row.try_get("name")?,
                base_url: row.try_get("base_url")?,
                model: row.try_get("model")?,
                headers,
                secret_ref: row.try_get("secret_ref")?,
                created_at: Timestamp(row.try_get("created_at")?),
            });
        }
        Ok(out)
    }

    /// 新增或整列取代（保留原本的 `created_at`）。
    pub async fn upsert_connection(
        conn: &mut SqliteConnection,
        row: &ConnectionRow,
    ) -> Result<(), Error> {
        let kind = match serde_json::to_value(row.kind)? {
            serde_json::Value::String(s) => s,
            other => return Err(Error::Corrupt(format!("provider 種類序列化錯誤：{other}"))),
        };
        let headers = serde_json::to_string(&row.headers)?;
        sqlx::query(
            "INSERT INTO model_connections \
             (id, user_id, kind, name, base_url, model, headers, secret_ref, created_at, \
              updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, name = excluded.name, \
             base_url = excluded.base_url, model = excluded.model, \
             headers = excluded.headers, secret_ref = excluded.secret_ref, \
             updated_at = excluded.updated_at",
        )
        .bind(row.id.to_string())
        .bind(LOCAL_USER_ID.to_string())
        .bind(kind)
        .bind(&row.name)
        .bind(&row.base_url)
        .bind(&row.model)
        .bind(&headers)
        .bind(row.secret_ref.as_deref())
        .bind(row.created_at.0)
        .bind(Timestamp::now().0)
        .execute(conn)
        .await?;
        Ok(())
    }

    pub async fn delete_connection(
        conn: &mut SqliteConnection,
        id: ProviderId,
    ) -> Result<(), Error> {
        sqlx::query("DELETE FROM model_connections WHERE id = ? AND user_id = ?")
            .bind(id.to_string())
            .bind(LOCAL_USER_ID.to_string())
            .execute(conn)
            .await?;
        Ok(())
    }

    pub async fn setting(&self, key: &str) -> Result<Option<String>, Error> {
        Ok(
            sqlx::query_scalar("SELECT value FROM instance_settings WHERE key = ?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn set_setting(
        conn: &mut SqliteConnection,
        key: &str,
        value: &str,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO instance_settings (key, value) VALUES (?, ?) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(conn)
        .await?;
        Ok(())
    }

    /// 這個 project 的完整原始歷史，依時間排序。
    #[cfg(test)]
    pub async fn om_raw_history(&self, project: ProjectId) -> Result<Vec<Item>, Error> {
        let rows: Vec<String> =
            sqlx::query_scalar("SELECT item FROM om_raw WHERE project_id = ? ORDER BY seq")
                .bind(project.to_string())
                .fetch_all(&self.pool)
                .await?;
        rows.iter()
            .map(|s| serde_json::from_str(s).map_err(Error::from))
            .collect()
    }
}

fn observation_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ObservationRow, Error> {
    let priority: String = row.try_get("priority")?;
    let source: String = row.try_get("source")?;
    let state: String = row.try_get("state")?;
    let replaces: Option<String> = row.try_get("replaces")?;
    Ok(ObservationRow {
        id: parse_id(row.try_get("id")?)?,
        generation: row.try_get("generation")?,
        seq: row.try_get("seq")?,
        date: row.try_get("date")?,
        priority: parse_priority(&priority)?,
        text: row.try_get("text")?,
        pinned: row.try_get("pinned")?,
        source: ObservationSource::parse(&source)?,
        state: ObservationState::parse(&state)?,
        replaces: replaces.map(parse_id).transpose()?,
    })
}

fn project_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ProjectRow, Error> {
    Ok(ProjectRow {
        id: parse_id(row.try_get("id")?)?,
        name: row.try_get("name")?,
        instructions: row.try_get("instructions")?,
        agent_name: row.try_get("agent_name")?,
        workdir: workdir_from_row(row)?,
        created_at: Timestamp(row.try_get("created_at")?),
    })
}

/// 工作資料夾的兩個欄位（`workdir_kind`、`workdir`）；`None` 時兩欄都是 NULL。
fn workdir_columns(workdir: Option<&WorkDir>) -> (Option<&'static str>, Option<&str>) {
    match workdir {
        Some(WorkDir::Github { repo }) => (Some("github"), Some(repo)),
        Some(WorkDir::Local { path }) => (Some("local"), Some(path)),
        None => (None, None),
    }
}

fn workdir_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Option<WorkDir>, Error> {
    let kind: Option<String> = row.try_get("workdir_kind")?;
    let value: Option<String> = row.try_get("workdir")?;
    Ok(match (kind.as_deref(), value) {
        (Some("github"), Some(repo)) => Some(WorkDir::Github { repo }),
        (Some("local"), Some(path)) => Some(WorkDir::Local { path }),
        _ => None,
    })
}

fn item_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<StreamItemView, Error> {
    let body: String = row.try_get("body")?;
    Ok(StreamItemView {
        id: parse_id(row.try_get("id")?)?,
        at: Timestamp(row.try_get("created_at")?),
        kind: serde_json::from_str(&body)?,
    })
}

fn kind_and_author(kind: &StreamItemKind) -> (&'static str, Option<Author>) {
    match kind {
        StreamItemKind::Human(m) => ("human", Some(m.author.clone())),
        StreamItemKind::Agent(m) => ("agent", Some(m.author.clone())),
        StreamItemKind::Card(_) => ("card", None),
        StreamItemKind::Event { .. } => ("event", None),
        StreamItemKind::DayDivider(_) => ("divider", None),
    }
}

pub(crate) fn to_json(v: &impl Serialize) -> Result<String, Error> {
    Ok(serde_json::to_string(v)?)
}

pub(crate) fn parse_id<T: std::str::FromStr>(s: String) -> Result<T, Error> {
    s.parse()
        .map_err(|_| Error::Corrupt(format!("資料庫裡的 ID 格式不對：{s}")))
}

/// 事件序列化成 JSON，資料庫與 WebSocket 共用同一份字串。
pub fn event_json(ev: &ServerEvent) -> Result<String, Error> {
    to_json(ev)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 升級前所有 project 都叫「Pots」：`0013_project_lead.sql` 換成各自的負責人，
    /// 總管與使用者自己改過的名字不動。
    #[tokio::test]
    async fn existing_projects_get_their_own_lead() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().in_memory(true))
            .await
            .unwrap();
        let mut before = sqlx::migrate!("./migrations/sqlite");
        before.migrations = before
            .migrations
            .iter()
            .filter(|m| m.version < 13)
            .cloned()
            .collect();
        before.run(&pool).await.unwrap();
        for (id, name, agent, kind) in [
            ("a", "auth-service", "Pots", "project"),
            ("b", "web", "小幫手", "project"),
            ("c", "總管", "Pots", "overseer"),
        ] {
            sqlx::query(
                "INSERT INTO projects (id, user_id, name, agent_name, kind, created_at) \
                 VALUES (?, 'u', ?, ?, ?, 0)",
            )
            .bind(id)
            .bind(name)
            .bind(agent)
            .bind(kind)
            .execute(&pool)
            .await
            .unwrap();
        }

        MIGRATOR.run(&pool).await.unwrap();
        let names: Vec<String> = sqlx::query_scalar("SELECT agent_name FROM projects ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            names,
            [
                default_agent_name("auth-service"),
                "小幫手".into(),
                "Pots".into()
            ]
        );
    }
}
