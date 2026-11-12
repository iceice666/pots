//! 任務樹：`goals`、`tasks` 與 `task_events`（`proposal/architecture.md#核心資料模型`）。
//!
//! 任務樹是平台唯一的真相來源。所有變更都在 [`LogTxn`] 裡做，同一個 transaction 記一筆
//! `GoalChanged`（整個 Goal 連同任務樹替換），客戶端不用自己拼狀態。
//!
//! M3 加上 `Waiting`：`wait_for` 把任務掛起，外部事件或逾時時由 watcher 喚醒回 `Planned`
//!（`crate::watcher`）。M4 加上 worker（`crate::worker`）：任務建立時排一個 worker run，
//! 執行中是 `Running`，worker 以 `complete_task`、`fail_task`、`escalate` 結束成 `Done`、
//! `Failed` 或 `NeedsHuman`。M5 加上「等你核准」（`Waiting { Approval }`，`crate::approvals`）：
//! 核准後平台 merge、任務 `Done`；拒絕時叫醒 worker；逾時轉 `NeedsHuman`。

use pots_proto::{
    ArtifactKind, Card, GoalActivity, GoalId, GoalState, GoalView, ProjectId, ServerEvent, TaskId,
    TaskStateView, TaskStep, TaskView, Timestamp,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnection, SqliteRow};
use uuid::Uuid;

use crate::Error;
use crate::artifacts;
use crate::hub::LogTxn;
use crate::store::{LOCAL_USER_ID, parse_id, to_json};
use crate::watcher::WaitCond;

/// Goal 標題與任務標題最多幾個字。
pub const MAX_TITLE_CHARS: usize = 120;
/// 一個 Goal 同時最多幾個未結束的任務（防止 Planner 失控）。
pub const MAX_OPEN_TASKS_PER_GOAL: usize = 50;
/// 一個 Goal 最多幾個任務（含已取消的；重新規劃會取消再建立）。
pub const MAX_TASKS_PER_GOAL: usize = 300;

/// 誰做的（`proposal/architecture.md` 的 `Principal`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name")]
pub enum Principal {
    Human,
    /// agent profile 名稱，例如 `chat`、`planner`。
    Agent(String),
    System,
}

impl Principal {
    pub fn agent(name: &str) -> Self {
        Self::Agent(name.to_string())
    }

    /// 顯示名稱。
    pub fn label(&self) -> String {
        match self {
            Self::Human => "你".into(),
            Self::Agent(name) if name == "chat" => "對話 agent".into(),
            Self::Agent(name) if name == "planner" => "Planner".into(),
            Self::Agent(name) => name.clone(),
            Self::System => "系統".into(),
        }
    }
}

/// 任務狀態。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum TaskState {
    /// 等 worker 取走（排隊中的 worker run）。
    Planned,
    /// worker 正在跑。
    Running,
    /// 等外部事件、子任務或時間；`deadline` 一到由 sweeper 以「逾時」喚醒。
    Waiting {
        on: WaitCond,
        deadline: Timestamp,
    },
    /// worker 卡住、需要 Planner 或你決定（`escalate`、沒有回報就結束、需要重新登入）。
    NeedsHuman {
        reason: String,
    },
    Done {
        outcome: String,
    },
    Failed {
        error: String,
    },
    Cancelled {
        by: Principal,
    },
}

impl TaskState {
    /// 還沒結束（Goal 結束時會被取消）。
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            Self::Planned | Self::Running | Self::Waiting { .. } | Self::NeedsHuman { .. }
        )
    }

    pub fn view(&self) -> TaskStateView {
        match self {
            Self::Planned => TaskStateView::Planned,
            Self::Running => TaskStateView::Running,
            Self::NeedsHuman { reason } => TaskStateView::NeedsHuman {
                reason: reason.clone(),
            },
            Self::Done { outcome } => TaskStateView::Done {
                outcome: outcome.clone(),
            },
            Self::Failed { error } => TaskStateView::Failed {
                attempts: 1,
                error: error.clone(),
            },
            Self::Waiting { on, deadline } => TaskStateView::Waiting {
                on: format!(
                    "{}（{} 逾時）",
                    on.describe(),
                    crate::automation::local(*deadline)
                ),
            },
            Self::Cancelled { .. } => TaskStateView::Cancelled,
        }
    }

    /// 給 agent 看的狀態名稱。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Running => "running",
            Self::Waiting { .. } => "waiting",
            Self::NeedsHuman { .. } => "needs_human",
            Self::Done { .. } => "done",
            Self::Failed { .. } => "failed",
            Self::Cancelled { .. } => "cancelled",
        }
    }
}

/// 狀態變化的原因（`task_events.cause`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cause {
    pub by: Principal,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct GoalRow {
    pub id: GoalId,
    pub project: ProjectId,
    pub title: String,
    pub state: GoalState,
    pub planner_note: String,
    pub created_at: Timestamp,
    /// 封存的 Goal 不出現在 project 列表與快照；成本報表仍算得到。
    pub archived_at: Option<Timestamp>,
}

#[derive(Clone, Debug)]
pub struct TaskRow {
    pub id: TaskId,
    pub goal: GoalId,
    pub parent: Option<TaskId>,
    pub title: String,
    pub summary: String,
    pub assignee: String,
    pub state: TaskState,
    /// `github.com/owner/repo` 或 `local:/abs/path`；建立時決定。
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// 任務分支從哪個分支開出來（`checkout.base`）。
    pub base: Option<String>,
    /// `github.com/owner/repo#42`。
    pub pr: Option<String>,
    pub pr_url: Option<String>,
    pub progress: String,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

impl TaskRow {
    /// 新任務（`Planned`）。
    pub fn new(goal: GoalId, title: String, summary: String) -> Self {
        Self {
            id: TaskId::new(),
            goal,
            parent: None,
            title,
            summary,
            assignee: String::new(),
            state: TaskState::Planned,
            repo: None,
            branch: None,
            base: None,
            pr: None,
            pr_url: None,
            progress: String::new(),
            started_at: None,
            finished_at: None,
        }
    }
}

fn goal_state_columns(state: &GoalState) -> (&'static str, Option<&str>) {
    match state {
        GoalState::Draft => ("draft", None),
        GoalState::Active => ("active", None),
        GoalState::Done { summary } => ("done", Some(summary)),
        GoalState::Cancelled { reason } => ("cancelled", Some(reason)),
    }
}

macro_rules! goal_columns {
    () => {
        "id, project_id, title, state, outcome, planner_note, created_at, archived_at"
    };
}

fn goal_from_row(row: &SqliteRow) -> Result<GoalRow, Error> {
    let state: String = row.try_get("state")?;
    let outcome: Option<String> = row.try_get("outcome")?;
    let outcome = outcome.unwrap_or_default();
    let state = match state.as_str() {
        "draft" => GoalState::Draft,
        "active" => GoalState::Active,
        "done" => GoalState::Done { summary: outcome },
        "cancelled" => GoalState::Cancelled { reason: outcome },
        _ => return Err(Error::Corrupt(format!("Goal 狀態不對：{state}"))),
    };
    Ok(GoalRow {
        id: parse_id(row.try_get("id")?)?,
        project: parse_id(row.try_get("project_id")?)?,
        title: row.try_get("title")?,
        state,
        planner_note: row.try_get("planner_note")?,
        created_at: Timestamp(row.try_get("created_at")?),
        archived_at: row.try_get::<Option<i64>, _>("archived_at")?.map(Timestamp),
    })
}

pub async fn insert_goal(
    conn: &mut SqliteConnection,
    goal: &GoalRow,
    created_by: &Principal,
) -> Result<(), Error> {
    let (state, outcome) = goal_state_columns(&goal.state);
    sqlx::query(
        "INSERT INTO goals (id, project_id, user_id, title, kind, state, outcome, created_by, \
         planner_note, budget_tokens, created_at, updated_at) VALUES (?, ?, ?, ?, 'feature', ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(goal.id.to_string())
    .bind(goal.project.to_string())
    .bind(LOCAL_USER_ID.to_string())
    .bind(&goal.title)
    .bind(state)
    .bind(outcome)
    .bind(to_json(created_by)?)
    .bind(&goal.planner_note)
    // 預設不限預算（NULL）；agent 建立時可以用 `set_goal_budget` 指定。
    .bind(None::<i64>)
    .bind(goal.created_at.0)
    .bind(goal.created_at.0)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn goal(conn: &mut SqliteConnection, id: GoalId) -> Result<Option<GoalRow>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        goal_columns!(),
        " FROM goals WHERE id = ?"
    ))
    .bind(id.to_string())
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(goal_from_row).transpose()
}

/// project 的 Goal，依建立時間。
pub async fn goals(conn: &mut SqliteConnection, project: ProjectId) -> Result<Vec<GoalRow>, Error> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        goal_columns!(),
        " FROM goals WHERE project_id = ? AND archived_at IS NULL ORDER BY created_at, id"
    ))
    .bind(project.to_string())
    .fetch_all(conn)
    .await?;
    rows.iter().map(goal_from_row).collect()
}

/// 封存一個已結束的 Goal：從 project 列表與快照消失，但資料與成本仍留著。
/// 呼叫端在同一個 transaction 裡記一筆 `GoalArchived`。
pub async fn set_goal_archived(
    conn: &mut SqliteConnection,
    id: GoalId,
    at: Timestamp,
) -> Result<(), Error> {
    sqlx::query("UPDATE goals SET archived_at = ?, updated_at = ? WHERE id = ?")
        .bind(at.0)
        .bind(at.0)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn set_goal_state(
    conn: &mut SqliteConnection,
    id: GoalId,
    state: &GoalState,
) -> Result<(), Error> {
    let (s, outcome) = goal_state_columns(state);
    sqlx::query("UPDATE goals SET state = ?, outcome = ?, updated_at = ? WHERE id = ?")
        .bind(s)
        .bind(outcome)
        .bind(Timestamp::now().0)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// 改 Goal 的種類（`feature` 或 `quick`）。
pub async fn set_goal_kind(
    conn: &mut SqliteConnection,
    id: GoalId,
    kind: &str,
) -> Result<(), Error> {
    sqlx::query("UPDATE goals SET kind = ? WHERE id = ?")
        .bind(kind)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn set_planner_note(
    conn: &mut SqliteConnection,
    id: GoalId,
    note: &str,
) -> Result<(), Error> {
    sqlx::query("UPDATE goals SET planner_note = ?, updated_at = ? WHERE id = ?")
        .bind(note)
        .bind(Timestamp::now().0)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn add_spent(conn: &mut SqliteConnection, id: GoalId, tokens: u64) -> Result<(), Error> {
    sqlx::query("UPDATE goals SET spent_tokens = spent_tokens + ? WHERE id = ?")
        .bind(tokens as i64)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// 建立 Goal 時指定的預算（至少下限，沒有上限）。
pub async fn set_goal_budget(
    conn: &mut SqliteConnection,
    id: GoalId,
    tokens: u64,
) -> Result<(), Error> {
    let tokens = tokens.clamp(crate::budget::MIN_GOAL_BUDGET_TOKENS, i64::MAX as u64);
    sqlx::query("UPDATE goals SET budget_tokens = ?, updated_at = ? WHERE id = ?")
        .bind(tokens as i64)
        .bind(Timestamp::now().0)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// 任務自己的用量；Goal 的用量另外記，熔斷看的是 project。
pub async fn add_task_spent(
    conn: &mut SqliteConnection,
    id: pots_proto::TaskId,
    tokens: u64,
) -> Result<(), Error> {
    sqlx::query("UPDATE tasks SET spent_tokens = spent_tokens + ? WHERE id = ?")
        .bind(tokens as i64)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

macro_rules! task_columns {
    () => {
        "id, goal_id, parent_id, title, summary, assignee, state, repo, branch, base_branch, \
         pr, pr_url, progress, started_at, finished_at"
    };
}

fn task_from_row(row: &SqliteRow) -> Result<TaskRow, Error> {
    let parent: Option<String> = row.try_get("parent_id")?;
    let state: String = row.try_get("state")?;
    let started: Option<i64> = row.try_get("started_at")?;
    let finished: Option<i64> = row.try_get("finished_at")?;
    Ok(TaskRow {
        id: parse_id(row.try_get("id")?)?,
        goal: parse_id(row.try_get("goal_id")?)?,
        parent: parent.map(parse_id).transpose()?,
        title: row.try_get("title")?,
        summary: row.try_get("summary")?,
        assignee: row.try_get("assignee")?,
        state: serde_json::from_str(&state)?,
        repo: row.try_get("repo")?,
        branch: row.try_get("branch")?,
        base: row.try_get("base_branch")?,
        pr: row.try_get("pr")?,
        pr_url: row.try_get("pr_url")?,
        progress: row.try_get("progress")?,
        started_at: started.map(Timestamp),
        finished_at: finished.map(Timestamp),
    })
}

/// 新增任務並記一筆建立事件。
pub async fn insert_task(
    conn: &mut SqliteConnection,
    task: &TaskRow,
    cause: &Cause,
) -> Result<(), Error> {
    let now = Timestamp::now().0;
    sqlx::query(
        "INSERT INTO tasks (id, goal_id, parent_id, title, summary, assignee, state, repo, \
         created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(task.id.to_string())
    .bind(task.goal.to_string())
    .bind(task.parent.map(|p| p.to_string()))
    .bind(&task.title)
    .bind(&task.summary)
    .bind(&task.assignee)
    .bind(to_json(&task.state)?)
    .bind(&task.repo)
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    insert_event(conn, task.id, None, &task.state, cause).await
}

async fn insert_event(
    conn: &mut SqliteConnection,
    task: TaskId,
    from: Option<&TaskState>,
    to: &TaskState,
    cause: &Cause,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO task_events (task_id, seq, from_state, to_state, cause, at) VALUES \
         (?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM task_events WHERE task_id = ?), ?, ?, ?, ?)",
    )
    .bind(task.to_string())
    .bind(task.to_string())
    .bind(from.map(to_json).transpose()?)
    .bind(to_json(to)?)
    .bind(to_json(cause)?)
    .bind(Timestamp::now().0)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn tasks(conn: &mut SqliteConnection, goal: GoalId) -> Result<Vec<TaskRow>, Error> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        task_columns!(),
        " FROM tasks WHERE goal_id = ? ORDER BY created_at, id"
    ))
    .bind(goal.to_string())
    .fetch_all(conn)
    .await?;
    rows.iter().map(task_from_row).collect()
}

pub async fn task(conn: &mut SqliteConnection, id: TaskId) -> Result<Option<TaskRow>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        task_columns!(),
        " FROM tasks WHERE id = ?"
    ))
    .bind(id.to_string())
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(task_from_row).transpose()
}

/// 改任務狀態並記一筆事件。
pub async fn set_task_state(
    conn: &mut SqliteConnection,
    task: &TaskRow,
    to: &TaskState,
    cause: &Cause,
) -> Result<(), Error> {
    let now = Timestamp::now().0;
    sqlx::query(
        "UPDATE tasks SET state = ?, updated_at = ?, \
         started_at = CASE WHEN ? THEN COALESCE(started_at, ?) ELSE started_at END, \
         finished_at = CASE WHEN ? THEN ? ELSE NULL END WHERE id = ?",
    )
    .bind(to_json(to)?)
    .bind(now)
    .bind(matches!(to, TaskState::Running))
    .bind(now)
    .bind(!to.is_open())
    .bind(now)
    .bind(task.id.to_string())
    .execute(&mut *conn)
    .await?;
    // 結束了：被喚醒、還在等 worker 的 run 也不用跑了。
    if !to.is_open() {
        sqlx::query(
            "UPDATE agent_runs SET state = 'cancelled', finished_at = ? \
             WHERE agent = 'worker' AND task_id = ? AND state = 'queued'",
        )
        .bind(Timestamp::now().0)
        .bind(task.id.to_string())
        .execute(&mut *conn)
        .await?;
    }
    // 不再等待了（喚醒、取消）：訂閱一起清掉。
    if !matches!(to, TaskState::Waiting { .. }) {
        sqlx::query("DELETE FROM subscriptions WHERE task_id = ?")
            .bind(task.id.to_string())
            .execute(&mut *conn)
            .await?;
    }
    insert_event(conn, task.id, Some(&task.state), to, cause).await
}

/// 取消 Goal 裡所有還沒結束的任務，回傳取消了幾個。
pub async fn cancel_open_tasks(
    conn: &mut SqliteConnection,
    goal: GoalId,
    cause: &Cause,
) -> Result<usize, Error> {
    let open: Vec<TaskRow> = tasks(conn, goal)
        .await?
        .into_iter()
        .filter(|t| t.state.is_open())
        .collect();
    let to = TaskState::Cancelled {
        by: cause.by.clone(),
    };
    for t in &open {
        set_task_state(conn, t, &to, cause).await?;
    }
    Ok(open.len())
}

/// 任務的進度步驟（由 `task_events` 產生）。
async fn steps(conn: &mut SqliteConnection, task: TaskId) -> Result<Vec<TaskStep>, Error> {
    let rows = sqlx::query(
        "SELECT from_state, to_state, cause, at FROM task_events WHERE task_id = ? ORDER BY seq",
    )
    .bind(task.to_string())
    .fetch_all(conn)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let from: Option<String> = row.try_get("from_state")?;
        let to: String = row.try_get("to_state")?;
        let to: TaskState = serde_json::from_str(&to)?;
        let cause: String = row.try_get("cause")?;
        let cause: Cause = serde_json::from_str(&cause)?;
        let was_waiting = from
            .as_deref()
            .and_then(|f| serde_json::from_str::<TaskState>(f).ok())
            .is_some_and(|f| matches!(f, TaskState::Waiting { .. }));
        let what = match (&from, &to) {
            (None, _) => "建立".to_string(),
            (_, TaskState::Cancelled { .. }) => "取消".to_string(),
            (_, TaskState::Waiting { on, .. }) => format!("開始等待{}", on.describe()),
            (_, TaskState::Planned) if was_waiting => "喚醒".to_string(),
            (_, TaskState::Planned) => "回到佇列".to_string(),
            (_, TaskState::Running) => "開始執行".to_string(),
            (_, TaskState::NeedsHuman { reason }) => {
                format!("需要決定：{}", one_line(reason, 200))
            }
            (_, TaskState::Done { .. }) => "完成".to_string(),
            (_, TaskState::Failed { error }) => format!("失敗：{}", one_line(error, 200)),
        };
        let mut text = format!("{what}（{}）", cause.by.label());
        if !cause.reason.is_empty() {
            text.push('：');
            text.push_str(&cause.reason);
        }
        out.push(TaskStep {
            text,
            at: Some(Timestamp(row.try_get("at")?)),
        });
    }
    Ok(out)
}

async fn task_view(conn: &mut SqliteConnection, t: &TaskRow) -> Result<TaskView, Error> {
    Ok(TaskView {
        id: t.id,
        goal: t.goal,
        title: t.title.clone(),
        assignee: if t.assignee.is_empty() {
            "尚未指派".into()
        } else {
            t.assignee.clone()
        },
        summary: t.summary.clone(),
        state: t.state.view(),
        branch: t.branch.clone(),
        elapsed_min: t
            .started_at
            .map(|s| {
                let end = t.finished_at.unwrap_or_else(Timestamp::now);
                ((end.0 - s.0).max(0) / 60) as u32
            })
            .unwrap_or(0),
        cost_cents: 0,
        steps: steps(conn, t.id).await?,
        thread: Vec::new(),
        pr: t.pr.clone(),
        pr_url: t.pr_url.clone(),
        progress: t.progress.clone(),
        started_at: t.started_at,
        finished_at: t.finished_at,
        spent_tokens: sqlx::query_scalar::<_, i64>("SELECT spent_tokens FROM tasks WHERE id = ?")
            .bind(t.id.to_string())
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or(0)
            .max(0) as u64,
    })
}

/// 記下任務的分支、PR 與進度（`None` 表示不改）。
pub async fn set_task_work(
    conn: &mut SqliteConnection,
    id: TaskId,
    branch: Option<&str>,
    // `(change, url)`；本機資料夾沒有 PR 網址。`change` 是 `github.com/o/r#42` 或 `local:/path`。
    pr: Option<(&str, Option<&str>)>,
    progress: Option<&str>,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE tasks SET branch = COALESCE(?, branch), pr = COALESCE(?, pr), \
         pr_url = COALESCE(?, pr_url), progress = COALESCE(?, progress), updated_at = ? WHERE id = ?",
    )
    .bind(branch)
    .bind(pr.map(|p| p.0))
    .bind(pr.and_then(|p| p.1))
    .bind(progress)
    .bind(Timestamp::now().0)
    .bind(id.to_string())
    .execute(conn)
    .await?;
    Ok(())
}

/// 記下任務分支的基準分支（`checkout.base`）；本機資料夾套用時要確認你的資料夾還在那個分支。
pub async fn set_task_base(
    conn: &mut SqliteConnection,
    id: TaskId,
    base: &str,
) -> Result<(), Error> {
    sqlx::query("UPDATE tasks SET base_branch = ?, updated_at = ? WHERE id = ?")
        .bind(base)
        .bind(Timestamp::now().0)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// 改任務的指派（例如「worker · Claude Code」）。
pub async fn set_assignee(
    conn: &mut SqliteConnection,
    id: TaskId,
    assignee: &str,
) -> Result<(), Error> {
    sqlx::query("UPDATE tasks SET assignee = ? WHERE id = ?")
        .bind(assignee)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// 沒有 Planner 的 Goal 種類：快速修改與環境 worker 都只有一個任務、直接派給 worker。
pub fn kind_without_planner(kind: &str) -> bool {
    matches!(kind, "quick" | "env")
}

/// Goal 的種類：`feature`、`quick` 或 `env`（環境 worker）。
pub async fn goal_kind(conn: &mut SqliteConnection, id: GoalId) -> Result<String, Error> {
    Ok(sqlx::query_scalar("SELECT kind FROM goals WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional(conn)
        .await?
        .unwrap_or_else(|| "feature".into()))
}

/// Goal 的完整檢視（任務樹與 Brief 全文）。
pub async fn goal_view(conn: &mut SqliteConnection, g: &GoalRow) -> Result<GoalView, Error> {
    let budget: Option<(Option<i64>, i64)> =
        sqlx::query_as("SELECT budget_tokens, spent_tokens FROM goals WHERE id = ?")
            .bind(g.id.to_string())
            .fetch_optional(&mut *conn)
            .await?;
    let budget_used_pct = match budget {
        Some((Some(budget), spent)) if budget > 0 => {
            ((spent.max(0) * 100 / budget).clamp(0, 100)) as u8
        }
        _ => 0,
    };
    let mut views = Vec::new();
    for t in tasks(conn, g.id).await? {
        views.push(task_view(conn, &t).await?);
    }
    let brief = artifacts::for_goal(conn, g.id, ArtifactKind::Brief)
        .await?
        .map(|a| a.body)
        .unwrap_or_default();
    let kind = goal_kind(conn, g.id).await?;
    Ok(GoalView {
        id: g.id,
        title: g.title.clone(),
        state: g.state.clone(),
        planner_note: g.planner_note.clone(),
        budget_used_pct,
        tasks: views,
        team: crate::team::views(conn, g.id).await?,
        activity: activity(conn, g.id).await?,
        brief,
        quick: kind == "quick",
        env: kind == "env",
    })
}

/// 一筆動態要不要同時貼到對話流。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// 只記在 Goal 的「動態」：Planner 的進展、開工、排程。
    Goal,
    /// 也貼一張里程碑卡片到對話流：Goal 結束、需要使用者處理。
    Chat,
}

/// 記一筆 Goal 的動態，並記一筆 `GoalChanged`。
pub async fn record_activity(
    txn: &mut LogTxn<'_>,
    project: ProjectId,
    goal: GoalId,
    text: &str,
    reach: Reach,
    run_id: Option<Uuid>,
) -> Result<(), Error> {
    let now = Timestamp::now();
    sqlx::query(
        "INSERT INTO goal_activity (goal_id, seq, text, at) \
         SELECT ?, COALESCE(MAX(seq), 0) + 1, ?, ? FROM goal_activity WHERE goal_id = ?",
    )
    .bind(goal.to_string())
    .bind(text)
    .bind(now.0)
    .bind(goal.to_string())
    .execute(txn.conn())
    .await?;
    if reach == Reach::Chat {
        let card = Card::Milestone {
            goal,
            text: text.to_string(),
        };
        crate::tools::post_card(txn, project, card, run_id).await?;
    }
    push_goal(txn, goal).await
}

/// Goal 的動態，舊的在前。
pub async fn activity(
    conn: &mut SqliteConnection,
    goal: GoalId,
) -> Result<Vec<GoalActivity>, Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT text, at FROM goal_activity WHERE goal_id = ? ORDER BY seq")
            .bind(goal.to_string())
            .fetch_all(conn)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(text, at)| GoalActivity {
            at: Timestamp(at),
            text,
        })
        .collect())
}

/// project 最近 `limit` 筆動態（新的在前），給對話 agent 的 Goal 概況用。
pub async fn recent_activity(
    conn: &mut SqliteConnection,
    project: ProjectId,
    limit: u32,
) -> Result<Vec<(GoalId, GoalActivity)>, Error> {
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT a.goal_id, a.text, a.at FROM goal_activity a JOIN goals g ON g.id = a.goal_id \
         WHERE g.project_id = ? ORDER BY a.at DESC, a.seq DESC LIMIT ?",
    )
    .bind(project.to_string())
    .bind(i64::from(limit))
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|(goal, text, at)| {
            Ok((
                parse_id(goal)?,
                GoalActivity {
                    at: Timestamp(at),
                    text,
                },
            ))
        })
        .collect()
}

/// project 的所有 Goal 檢視，快照用。
pub async fn project_goals(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<Vec<GoalView>, Error> {
    let mut out = Vec::new();
    for g in goals(conn, project).await? {
        out.push(goal_view(conn, &g).await?);
    }
    Ok(out)
}

/// CI 又失敗了一次：回傳連續失敗的次數。
pub async fn bump_ci_failures(conn: &mut SqliteConnection, id: TaskId) -> Result<u32, Error> {
    let n: i64 = sqlx::query_scalar(
        "UPDATE tasks SET ci_failures = ci_failures + 1 WHERE id = ? RETURNING ci_failures",
    )
    .bind(id.to_string())
    .fetch_one(conn)
    .await?;
    Ok(n as u32)
}

/// CI 通過：連續失敗的次數歸零。
pub async fn reset_ci_failures(conn: &mut SqliteConnection, id: TaskId) -> Result<(), Error> {
    sqlx::query("UPDATE tasks SET ci_failures = 0 WHERE id = ?")
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// 記一筆 `GoalChanged`（Goal 不存在或已封存就不記）。任務已經不在等的核准在這裡撤回
///（任務狀態改了之後一定會記一筆 `GoalChanged`）。
pub async fn push_goal(txn: &mut LogTxn<'_>, id: GoalId) -> Result<(), Error> {
    crate::approvals::sync_goal(txn, id).await?;
    let Some(g) = goal(txn.conn(), id).await? else {
        return Ok(());
    };
    if g.archived_at.is_some() {
        return Ok(());
    }
    let view = goal_view(txn.conn(), &g).await?;
    let project = g.project;
    txn.push(|seq| ServerEvent::GoalChanged {
        seq,
        project,
        goal: view,
    })
    .await
}

/// 檢查長度並去掉頭尾空白；不合法時回說明。
pub fn check_text(what: &str, text: &str, max: usize, required: bool) -> Result<String, String> {
    let t = text.trim();
    if required && t.is_empty() {
        return Err(format!("{what}不能是空的"));
    }
    if t.chars().count() > max {
        return Err(format!("{what}最多 {max} 個字"));
    }
    Ok(t.to_string())
}

/// 標題類的單行文字：同 [`check_text`]（必填），另外不能換行。標題會放進其他 agent 的
/// instructions 與觸發訊息，換行可能偽造出標題或任務樹的項目。
pub fn check_line(what: &str, text: &str, max: usize) -> Result<String, String> {
    let t = check_text(what, text, max, true)?;
    if t.contains(['\n', '\r']) {
        return Err(format!("{what}不能換行"));
    }
    Ok(t)
}

/// 放進 prompt 的一行：連續的空白（含換行）合成一個空格，再截到 `max` 個字。
pub fn one_line(text: &str, max: usize) -> String {
    truncate(&text.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

/// 截到 `max` 個字，超過時加「…」。
pub fn truncate(text: &str, max: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        let mut s: String = t.chars().take(max).collect();
        s.push('…');
        s
    }
}
