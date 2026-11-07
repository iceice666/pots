//! 資料庫存取。M0 只有 SQLite；SQL 都是可攜寫法，之後加 Postgres 時把這層換成 trait
//! 或 `SqlStore::postgres`（`proposal/deployment.md#切換點`）。
//!
//! 查詢一律在執行期檢查（`sqlx::query`），建置時不需要 `DATABASE_URL`。
//! 寫入方法收 `&mut SqliteConnection`，讓呼叫端把狀態變更與事件日誌放在同一個 transaction。

use std::path::Path;
use std::time::Duration;

use pots_llm::Item;
use pots_proto::{
    Author, InstanceId, ItemId, ProjectId, ServerEvent, StreamItemKind, StreamItemView, Timestamp,
};
use serde::Serialize;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePool, SqlitePoolOptions,
    SqliteSynchronous,
};
use sqlx::{Connection, Row};
use uuid::Uuid;

use crate::Error;

/// 桌面版固定的本機使用者。所有屬於使用者的列從第一天就帶 `user_id`。
pub const LOCAL_USER_ID: Uuid = Uuid::from_u64_pair(0, 1);

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/sqlite");

#[derive(Clone, Debug)]
pub struct ProjectRow {
    pub id: ProjectId,
    pub name: String,
    pub instructions: String,
    pub agent_name: String,
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

    pub async fn begin(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, Error> {
        Ok(self.pool.begin().await?)
    }

    // -----------------------------------------------------------------------
    // instance

    /// 讀取實例身分；第一次啟動時建立。
    pub async fn ensure_instance(&self, default_name: &str) -> Result<InstanceRow, Error> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
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
            "SELECT id, name, instructions, agent_name, created_at FROM projects \
             WHERE user_id = ? AND residency = 'active' ORDER BY created_at, id",
        )
        .bind(LOCAL_USER_ID.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(project_from_row).collect()
    }

    pub async fn project(&self, id: ProjectId) -> Result<Option<ProjectRow>, Error> {
        let row = sqlx::query(
            "SELECT id, name, instructions, agent_name, created_at FROM projects \
             WHERE id = ? AND user_id = ?",
        )
        .bind(id.to_string())
        .bind(LOCAL_USER_ID.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(project_from_row).transpose()
    }

    pub async fn insert_project(conn: &mut SqliteConnection, p: &ProjectRow) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO projects (id, user_id, name, instructions, agent_name, residency, created_at) \
             VALUES (?, ?, ?, ?, ?, 'active', ?)",
        )
        .bind(p.id.to_string())
        .bind(LOCAL_USER_ID.to_string())
        .bind(&p.name)
        .bind(&p.instructions)
        .bind(&p.agent_name)
        .bind(p.created_at.0)
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

    /// 上次異常結束時還在跑的 run 一律標成失敗。
    pub async fn fail_stale_runs(conn: &mut SqliteConnection, error: &str) -> Result<u64, Error> {
        let done = sqlx::query(
            "UPDATE agent_runs SET state = 'failed', error = ?, finished_at = ? WHERE state = 'running'",
        )
        .bind(error)
        .bind(Timestamp::now().0)
        .execute(conn)
        .await?;
        Ok(done.rows_affected())
    }

    // -----------------------------------------------------------------------
    // om_raw

    /// 追加一筆原始歷史，`seq` 接在這個 project 的最後一筆之後。
    pub async fn append_om_raw(
        conn: &mut SqliteConnection,
        project: ProjectId,
        run_id: Option<Uuid>,
        item: &Item,
    ) -> Result<(), Error> {
        sqlx::query(
            "INSERT INTO om_raw (project_id, seq, run_id, item, created_at) \
             VALUES (?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM om_raw WHERE project_id = ?), ?, ?, ?)",
        )
        .bind(project.to_string())
        .bind(project.to_string())
        .bind(run_id.map(|r| r.to_string()))
        .bind(to_json(item)?)
        .bind(Timestamp::now().0)
        .execute(conn)
        .await?;
        Ok(())
    }

    /// 這個 project 的完整原始歷史，依時間排序。
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

fn project_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ProjectRow, Error> {
    Ok(ProjectRow {
        id: parse_id(row.try_get("id")?)?,
        name: row.try_get("name")?,
        instructions: row.try_get("instructions")?,
        agent_name: row.try_get("agent_name")?,
        created_at: Timestamp(row.try_get("created_at")?),
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

fn to_json(v: &impl Serialize) -> Result<String, Error> {
    Ok(serde_json::to_string(v)?)
}

fn parse_id<T: std::str::FromStr>(s: String) -> Result<T, Error> {
    s.parse()
        .map_err(|_| Error::Corrupt(format!("資料庫裡的 ID 格式不對：{s}")))
}

/// 事件序列化成 JSON，資料庫與 WebSocket 共用同一份字串。
pub fn event_json(ev: &ServerEvent) -> Result<String, Error> {
    to_json(ev)
}
