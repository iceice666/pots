//! 核准（M5，`proposal/approvals-and-notifications.md`）。
//!
//! - **申請**：worker 或 Planner 用 `request_approval` 申請 merge 任務的 PR（任務轉
//!   `Waiting { Approval }`）；對話 agent 用 `create_listener` 提案的 listener、`propose_user_md_edit`
//!   與 `propose_project_note` 的修改也是核准；merge 成功後平台自動開一筆「刪除任務分支」（不推播）。
//!   平台存下結構化的動作與它的 SHA-256（`action_hash`），之後不再改變。
//! - **決定**：你核准或拒絕（`ClientCommand::Decide`，可附理由）。拒絕時把理由帶給 agent
//!   （merge：叫醒 worker）。核准後由平台執行，不是 agent：先比對 hash，merge 時帶核准的 head sha
//!   （PR 之後有新的 commit 就不 merge），寫入 USER.md、Project Notes 時要求內容還是提案時的版本。
//!   需要連網的動作（merge、刪分支）在 commit 之後另外執行（[`Exec`]），結果再記一筆。
//! - **逾時**：預設 72 小時沒有決定就 `Expired`；在等它的任務轉 `NeedsHuman`（sweeper 呼叫 [`expire_due`]）。
//! - **撤回**：在等的任務被取消、被交接叫醒，或 listener 被刪掉、被你直接改掉時，還沒決定的核准
//!   自動撤回（[`sync_goal`]、[`sync_listeners`]）。
//!
//! 每次狀態變化都記一筆只增不改的 `approval_events`，並送 `ApprovalChanged`。推播由客戶端依
//! `notify` 與通知設定決定（`proposal/approvals-and-notifications.md#通知策略`）。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pots_llm::agent::ToolError;
use pots_proto::{
    ApprovalActionView, ApprovalId, ApprovalState, ApprovalView, ArtifactId, ArtifactKind, Card,
    GoalId, ListenerId, ListenerSpec, MergeMethod, ProjectId, ScheduleId, ServerEvent, TaskId,
    Timestamp,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnection, SqliteRow};

use crate::App;
use crate::Error;
use crate::artifacts::{self, Edit};
use crate::automation::{self, ListenerDbState};
use crate::hub::{Hub, LogTxn};
use crate::store::{LOCAL_USER_ID, Store, parse_id, to_json};
use crate::tasks::{self, Principal, TaskRow, TaskState};
use crate::tools::{Scope, ToolCtx, bad, internal};
use crate::watcher::{self, WaitCond, Watcher};
use crate::worker::repo::RepoSource;
use crate::worker::{self, Ends, TaskEnd, Worker};

/// 沒有決定的核准多久後逾時。
pub const TTL_SECS: i64 = 72 * 3600;
/// 你決定時附的理由、agent 給的理由最多幾個字。
pub const MAX_NOTE_CHARS: usize = 1000;
/// 快照帶最近幾天內有變化的核准。
const RECENT_SECS: i64 = 7 * 86_400;
const MAX_RECENT: i64 = 200;

/// 要核准的具體動作。存成 JSON，`action_hash` 是它的 SHA-256；建立後不再改變。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// merge 任務的 PR；只在 head 還是 `sha` 時 merge。
    MergePr {
        /// `github.com/owner/repo`
        repo: String,
        number: u64,
        sha: String,
        method: MergeMethod,
        branch: String,
        base: String,
        url: String,
    },
    /// 刪除平台推上去的任務分支（`pots/` 開頭）。
    DeleteBranch { repo: String, branch: String },
    /// 把本機資料夾的任務分支套用進你的資料夾；只在任務分支還是 `sha` 時執行。
    ApplyChanges {
        /// 本機資料夾的絕對路徑。
        path: String,
        branch: String,
        /// 任務分支從哪個分支開出來；你的資料夾要還在那個分支上。
        base: String,
        sha: String,
    },
    /// 啟用 agent 提案的 listener（條件與動作就是這份 `spec`）。
    CreateListener {
        listener: ListenerId,
        spec: ListenerSpec,
    },
    /// 把 USER.md 換成 `body`；只在目前還是 `base_rev` 時寫入。
    EditUserMd { base_rev: u32, body: String },
    /// 把 project 的 Project Notes 換成 `body`；只在目前還是 `base_rev` 時寫入（還沒有時 `artifact`
    /// 是 `None`、`base_rev` 是 0，核准時建立）。
    EditProjectNotes {
        project: ProjectId,
        artifact: Option<ArtifactId>,
        base_rev: u32,
        body: String,
    },
    /// Goal 的預算用完了；核准就追加 `tokens` 並叫醒 Planner（M6）。
    IncreaseBudget { goal: GoalId, tokens: u64 },
    /// project 的每小時用量超過上限；核准就解除熔斷並重新排程（M6）。
    ResumeProject { project: ProjectId },
    /// 刪除排程（對話 agent 申請）。`summary` 是申請時的描述，顯示用。
    DeleteSchedule {
        schedule: ScheduleId,
        summary: String,
    },
    /// 刪除 listener（對話 agent 申請）。
    DeleteListener {
        listener: ListenerId,
        summary: String,
    },
    /// 封存 artifact（對話 agent 申請）：從列表移除，資料仍留著。
    ArchiveArtifact {
        artifact: ArtifactId,
        /// 不能叫 `kind`（和 serde 的 tag 衝突）。
        artifact_kind: ArtifactKind,
        title: String,
    },
    /// agent 問你一個問題（`ask_user`）。「核准」是回答（答案存在 `note`），「拒絕」是略過；
    /// 回答後叫醒問的 agent：worker 的任務、Planner，或對話 agent（`task`、`goal` 都沒有時）。
    AskUser {
        question: String,
        options: Vec<String>,
        allow_other: bool,
    },
}

impl Action {
    pub fn hash(&self) -> String {
        let json = serde_json::to_string(self).expect("動作可以序列化");
        hex::encode(Sha256::digest(json.as_bytes()))
    }

    /// 要另外執行的（連網的 merge／刪分支、動檔案的套用）：commit 之後在另一個 task 裡做，
    /// 不會卡住日誌的 transaction。
    fn is_remote(&self) -> bool {
        matches!(
            self,
            Self::MergePr { .. } | Self::DeleteBranch { .. } | Self::ApplyChanges { .. }
        )
    }
}

fn state_str(s: ApprovalState) -> &'static str {
    match s {
        ApprovalState::Pending => "pending",
        ApprovalState::Executing => "executing",
        ApprovalState::Executed => "executed",
        ApprovalState::Rejected => "rejected",
        ApprovalState::Expired => "expired",
        ApprovalState::Failed => "failed",
        ApprovalState::Cancelled => "cancelled",
    }
}

fn parse_state(s: &str) -> Result<ApprovalState, Error> {
    Ok(match s {
        "pending" => ApprovalState::Pending,
        "executing" => ApprovalState::Executing,
        "executed" => ApprovalState::Executed,
        "rejected" => ApprovalState::Rejected,
        "expired" => ApprovalState::Expired,
        "failed" => ApprovalState::Failed,
        "cancelled" => ApprovalState::Cancelled,
        _ => return Err(Error::Corrupt(format!("核准狀態不對：{s}"))),
    })
}

/// `approvals` 的一列。
#[derive(Clone, Debug)]
pub struct ApprovalRow {
    pub id: ApprovalId,
    pub project: ProjectId,
    pub goal: Option<GoalId>,
    pub task: Option<TaskId>,
    pub action: Action,
    pub hash: String,
    pub title: String,
    pub reason: String,
    pub requested_by: Principal,
    pub notify: bool,
    pub state: ApprovalState,
    pub note: Option<String>,
    pub result: Option<String>,
    pub decided_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
}

macro_rules! columns {
    () => {
        "id, project_id, goal_id, task_id, action, action_hash, title, reason, requested_by, notify, \
         state, note, result, decided_at, created_at, expires_at"
    };
}

fn from_row(row: &SqliteRow) -> Result<ApprovalRow, Error> {
    let goal: Option<String> = row.try_get("goal_id")?;
    let task: Option<String> = row.try_get("task_id")?;
    let action: String = row.try_get("action")?;
    let by: String = row.try_get("requested_by")?;
    let state: String = row.try_get("state")?;
    let decided: Option<i64> = row.try_get("decided_at")?;
    Ok(ApprovalRow {
        id: parse_id(row.try_get("id")?)?,
        project: parse_id(row.try_get("project_id")?)?,
        goal: goal.map(parse_id).transpose()?,
        task: task.map(parse_id).transpose()?,
        action: serde_json::from_str(&action)?,
        hash: row.try_get("action_hash")?,
        title: row.try_get("title")?,
        reason: row.try_get("reason")?,
        requested_by: serde_json::from_str(&by)?,
        notify: row.try_get::<i64, _>("notify")? != 0,
        state: parse_state(&state)?,
        note: row.try_get("note")?,
        result: row.try_get("result")?,
        decided_at: decided.map(Timestamp),
        created_at: Timestamp(row.try_get("created_at")?),
        expires_at: Timestamp(row.try_get("expires_at")?),
    })
}

pub async fn get(
    conn: &mut SqliteConnection,
    id: ApprovalId,
) -> Result<Option<ApprovalRow>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM approvals WHERE id = ?"
    ))
    .bind(id.to_string())
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(from_row).transpose()
}

/// 還沒決定、會擋住任務的核准（`goal` 或 `task` 擇一）。
async fn pending_blocking(
    conn: &mut SqliteConnection,
    goal: Option<GoalId>,
    task: Option<TaskId>,
) -> Result<Vec<ApprovalRow>, Error> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM approvals WHERE state = 'pending' AND task_id IS NOT NULL \
         AND (goal_id = ? OR task_id = ?) ORDER BY created_at, id"
    ))
    .bind(goal.map(|g| g.to_string()))
    .bind(task.map(|t| t.to_string()))
    .fetch_all(conn)
    .await?;
    rows.iter().map(from_row).collect()
}

/// Planner 在這個 Goal 問了、還沒回答的問題。
async fn pending_planner_questions(
    conn: &mut SqliteConnection,
    goal: GoalId,
) -> Result<Vec<ApprovalRow>, Error> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM approvals WHERE state = 'pending' AND task_id IS NULL AND goal_id = ? \
         AND action LIKE '{\"kind\":\"ask_user\"%' ORDER BY created_at, id"
    ))
    .bind(goal.to_string())
    .fetch_all(conn)
    .await?;
    rows.iter().map(from_row).collect()
}

/// 申請一筆核准時的內容。
pub struct NewApproval {
    pub project: ProjectId,
    pub goal: Option<GoalId>,
    /// 會等這筆核准的任務（呼叫端負責讓它進入 `Waiting { Approval }`）。
    pub task: Option<TaskId>,
    pub action: Action,
    pub title: String,
    pub reason: String,
    pub by: Principal,
    pub notify: bool,
}

/// 寫入一筆待決的核准並記 `ApprovalChanged`。卡片由呼叫端貼（對話 agent 附在回覆下面）。
pub async fn insert(
    txn: &mut LogTxn<'_>,
    new: NewApproval,
    now: Timestamp,
) -> Result<ApprovalRow, Error> {
    let row = ApprovalRow {
        id: ApprovalId::new(),
        project: new.project,
        goal: new.goal,
        task: new.task,
        hash: new.action.hash(),
        action: new.action,
        title: tasks::one_line(&new.title, 200),
        reason: tasks::truncate(&new.reason, MAX_NOTE_CHARS),
        requested_by: new.by,
        notify: new.notify,
        state: ApprovalState::Pending,
        note: None,
        result: None,
        decided_at: None,
        created_at: now,
        expires_at: Timestamp(now.0 + TTL_SECS),
    };
    sqlx::query(
        "INSERT INTO approvals (id, user_id, project_id, goal_id, task_id, action, action_hash, title, \
         reason, requested_by, notify, state, created_at, expires_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?)",
    )
    .bind(row.id.to_string())
    .bind(LOCAL_USER_ID.to_string())
    .bind(row.project.to_string())
    .bind(row.goal.map(|g| g.to_string()))
    .bind(row.task.map(|t| t.to_string()))
    .bind(to_json(&row.action)?)
    .bind(&row.hash)
    .bind(&row.title)
    .bind(&row.reason)
    .bind(to_json(&row.requested_by)?)
    .bind(i64::from(row.notify))
    .bind(now.0)
    .bind(row.expires_at.0)
    .bind(now.0)
    .execute(txn.conn())
    .await?;
    log_event(
        txn.conn(),
        row.id,
        ApprovalState::Pending,
        &row.requested_by,
        None,
        now,
    )
    .await?;
    push(txn, &row).await?;
    Ok(row)
}

async fn log_event(
    conn: &mut SqliteConnection,
    id: ApprovalId,
    state: ApprovalState,
    by: &Principal,
    detail: Option<&str>,
    now: Timestamp,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO approval_events (approval_id, seq, state, by_principal, detail, at) VALUES \
         (?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM approval_events WHERE approval_id = ?), ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(id.to_string())
    .bind(state_str(state))
    .bind(to_json(by)?)
    .bind(detail)
    .bind(now.0)
    .execute(conn)
    .await?;
    Ok(())
}

/// 改狀態、記一筆事件並送 `ApprovalChanged`。`decision` 是你的決定（核准者與理由）。
async fn transition(
    txn: &mut LogTxn<'_>,
    row: &ApprovalRow,
    to: ApprovalState,
    by: &Principal,
    decision: Option<Option<&str>>,
    result: Option<&str>,
    now: Timestamp,
) -> Result<ApprovalRow, Error> {
    let mut new = row.clone();
    new.state = to;
    if let Some(note) = decision {
        new.note = note.map(str::to_string);
        new.decided_at = Some(now);
        sqlx::query("UPDATE approvals SET note = ?, decided_by = ?, decided_at = ? WHERE id = ?")
            .bind(note)
            .bind(to_json(by)?)
            .bind(now.0)
            .bind(row.id.to_string())
            .execute(txn.conn())
            .await?;
    }
    if let Some(r) = result {
        new.result = Some(tasks::truncate(r, 2000));
    }
    sqlx::query("UPDATE approvals SET state = ?, result = ?, updated_at = ? WHERE id = ?")
        .bind(state_str(to))
        .bind(&new.result)
        .bind(now.0)
        .bind(row.id.to_string())
        .execute(txn.conn())
        .await?;
    let detail = result.or(decision.flatten());
    log_event(txn.conn(), row.id, to, by, detail, now).await?;
    push(txn, &new).await?;
    Ok(new)
}

/// 記一筆 `ApprovalChanged`。
pub async fn push(txn: &mut LogTxn<'_>, row: &ApprovalRow) -> Result<(), Error> {
    let approval = Box::new(view(txn.conn(), row).await?);
    txn.push(|seq| ServerEvent::ApprovalChanged { seq, approval })
        .await
}

/// 給客戶端看的檢視（修改提案附上和提案時版本的 diff）。
pub async fn view(conn: &mut SqliteConnection, row: &ApprovalRow) -> Result<ApprovalView, Error> {
    let action = match &row.action {
        Action::MergePr {
            repo,
            number,
            sha,
            method,
            branch,
            base,
            url,
        } => ApprovalActionView::MergePr {
            pr: format!("{repo}#{number}"),
            url: (!url.is_empty()).then(|| url.clone()),
            sha: sha.clone(),
            method: *method,
            branch: branch.clone(),
            base: base.clone(),
        },
        Action::DeleteBranch { repo, branch } => ApprovalActionView::DeleteBranch {
            repo: repo.clone(),
            branch: branch.clone(),
        },
        Action::ApplyChanges {
            path,
            branch,
            base,
            sha,
        } => ApprovalActionView::ApplyChanges {
            path: path.clone(),
            branch: branch.clone(),
            base: base.clone(),
            sha: sha.clone(),
        },
        Action::CreateListener { listener, spec } => ApprovalActionView::CreateListener {
            listener: *listener,
            summary: automation::listener_title(spec),
        },
        Action::EditUserMd { base_rev, body } => {
            let old = crate::user_md::revision(conn, *base_rev)
                .await?
                .unwrap_or_default();
            ApprovalActionView::EditUserMd {
                base_rev: *base_rev,
                diff: artifacts::diff(&old, body, &format!("USER.md v{base_rev}"), "提案"),
            }
        }
        Action::EditProjectNotes {
            artifact,
            base_rev,
            body,
            ..
        } => {
            let old = match artifact {
                Some(a) => artifacts::revision_body(conn, *a, *base_rev)
                    .await?
                    .unwrap_or_default(),
                None => String::new(),
            };
            ApprovalActionView::EditProjectNotes {
                artifact: *artifact,
                base_rev: *base_rev,
                diff: artifacts::diff(&old, body, &format!("Project Notes v{base_rev}"), "提案"),
            }
        }
        Action::IncreaseBudget { goal, tokens } => ApprovalActionView::IncreaseBudget {
            goal: *goal,
            tokens: *tokens,
        },
        Action::ResumeProject { project } => {
            ApprovalActionView::ResumeProject { project: *project }
        }
        Action::DeleteSchedule { schedule, summary } => ApprovalActionView::DeleteSchedule {
            schedule: *schedule,
            summary: summary.clone(),
        },
        Action::DeleteListener { listener, summary } => ApprovalActionView::DeleteListener {
            listener: *listener,
            summary: summary.clone(),
        },
        Action::ArchiveArtifact {
            artifact,
            artifact_kind,
            title,
        } => ApprovalActionView::ArchiveArtifact {
            artifact: *artifact,
            kind: *artifact_kind,
            title: title.clone(),
        },
        Action::AskUser {
            question,
            options,
            allow_other,
        } => ApprovalActionView::AskUser {
            question: question.clone(),
            options: options.clone(),
            allow_other: *allow_other,
        },
    };
    Ok(ApprovalView {
        id: row.id,
        project: row.project,
        goal: row.goal,
        task: row.task,
        title: row.title.clone(),
        reason: row.reason.clone(),
        requested_by: row.requested_by.label(),
        action,
        action_hash: row.hash[..row.hash.len().min(12)].to_string(),
        state: row.state,
        note: row.note.clone(),
        result: row.result.clone(),
        notify: row.notify,
        created_at: row.created_at,
        expires_at: row.expires_at,
        decided_at: row.decided_at,
    })
}

/// 快照用：還沒決定的，加上最近有變化的（依建立時間）。
pub async fn recent(
    conn: &mut SqliteConnection,
    now: Timestamp,
) -> Result<Vec<ApprovalView>, Error> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM (SELECT * FROM approvals WHERE state IN ('pending', 'executing') OR updated_at > ? \
         ORDER BY created_at DESC, id DESC LIMIT ?) ORDER BY created_at, id"
    ))
    .bind(now.0 - RECENT_SECS)
    .bind(MAX_RECENT)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let row = from_row(r)?;
        out.push(view(conn, &row).await?);
    }
    Ok(out)
}

/// 這個 project 最近的核准（新的在前），給對話 agent 一句話的結果
///（`proposal/memory.md`：核准卡片只以一句結果進入 agent 的視野）。
pub async fn recent_outcomes(
    conn: &mut SqliteConnection,
    project: ProjectId,
    limit: i64,
) -> Result<Vec<String>, Error> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM approvals WHERE project_id = ? ORDER BY updated_at DESC, id DESC LIMIT ?"
    ))
    .bind(project.to_string())
    .bind(limit)
    .fetch_all(conn)
    .await?;
    let mut out = Vec::new();
    for r in &rows {
        let row = from_row(r)?;
        if let (Action::AskUser { question, .. }, ApprovalState::Executed, Some(answer)) =
            (&row.action, row.state, &row.note)
        {
            out.push(format!(
                "- 問使用者「{}」，使用者回答：{}",
                tasks::one_line(question, 150),
                tasks::one_line(answer, 300)
            ));
            continue;
        }
        let mut line = format!(
            "- {}：{}",
            tasks::one_line(&row.title, 150),
            row.state.label()
        );
        if let Some(note) = &row.note {
            line.push_str(&format!("（使用者的理由：{}）", tasks::one_line(note, 200)));
        }
        if let Some(r) = &row.result
            && row.state != ApprovalState::Pending
        {
            line.push_str(&format!("；{}", tasks::one_line(r, 200)));
        }
        out.push(line);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 撤回與逾時

/// 在等的任務已經不在等這筆核准（取消、被交接叫醒、Goal 結束）：撤回。`tasks::push_goal` 每次都呼叫。
pub async fn sync_goal(txn: &mut LogTxn<'_>, goal: GoalId) -> Result<(), Error> {
    // Planner 的問題（沒有任務）：Goal 結束就不再需要。
    let active = tasks::goal(txn.conn(), goal)
        .await?
        .is_some_and(|g| g.state.is_active());
    if !active {
        for row in pending_planner_questions(txn.conn(), goal).await? {
            transition(
                txn,
                &row,
                ApprovalState::Cancelled,
                &Principal::System,
                None,
                Some("Goal 已經結束"),
                Timestamp::now(),
            )
            .await?;
        }
    }
    let pending = pending_blocking(txn.conn(), Some(goal), None).await?;
    for row in pending {
        let Some(task) = row.task else { continue };
        let waiting = tasks::task(txn.conn(), task).await?.is_some_and(|t| {
            matches!(&t.state, TaskState::Waiting { on: WaitCond::Approval { id, .. }, .. } if *id == row.id)
        });
        if !waiting {
            transition(
                txn,
                &row,
                ApprovalState::Cancelled,
                &Principal::System,
                None,
                Some("任務不再等這筆核准"),
                Timestamp::now(),
            )
            .await?;
        }
    }
    Ok(())
}

/// listener 被刪掉、被你直接修改或啟用時，它的提案撤回。改 listener 的指令都呼叫。
pub async fn sync_listeners(txn: &mut LogTxn<'_>) -> Result<(), Error> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM approvals WHERE state = 'pending' AND action LIKE '{\"kind\":\"create_listener\"%'"
    ))
    .fetch_all(txn.conn())
    .await?;
    let rows: Vec<ApprovalRow> = rows.iter().map(from_row).collect::<Result<_, _>>()?;
    for row in rows {
        let Action::CreateListener { listener, .. } = &row.action else {
            continue;
        };
        let still = automation::listener(txn.conn(), *listener)
            .await?
            .is_some_and(|l| l.state == ListenerDbState::Proposed && l.approval == Some(row.id));
        if !still {
            transition(
                txn,
                &row,
                ApprovalState::Cancelled,
                &Principal::System,
                None,
                Some("listener 已經被刪除或修改"),
                Timestamp::now(),
            )
            .await?;
        }
    }
    Ok(())
}

/// 到期還沒決定的核准轉 `Expired`；在等它的任務轉 `NeedsHuman`，listener 提案改成停用。
/// 回傳 commit 之後要通知的事。
pub async fn expire_due(txn: &mut LogTxn<'_>, now: Timestamp) -> Result<Ends, Error> {
    let due = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM approvals WHERE state = 'pending' AND expires_at <= ? ORDER BY created_at, id"
    ))
    .bind(now.0)
    .fetch_all(txn.conn())
    .await?;
    let due: Vec<ApprovalRow> = due.iter().map(from_row).collect::<Result<_, _>>()?;
    let mut ends = Ends::default();
    for row in due {
        let e = expire_one(txn, &row, now).await?;
        ends.planner |= e.planner;
        ends.closed = ends.closed.or(e.closed);
    }
    Ok(ends)
}

async fn expire_one(
    txn: &mut LogTxn<'_>,
    row: &ApprovalRow,
    now: Timestamp,
) -> Result<Ends, Error> {
    let hours = TTL_SECS / 3600;
    transition(
        txn,
        row,
        ApprovalState::Expired,
        &Principal::System,
        None,
        Some(&format!("{hours} 小時內沒有決定")),
        now,
    )
    .await?;
    let mut ends = Ends::default();
    match &row.action {
        Action::MergePr { .. } => {
            if let Some(t) = waiting_task(txn.conn(), row).await? {
                ends = worker::end_task(
                    txn,
                    &t,
                    TaskEnd::NeedsHuman(format!(
                        "「{}」的核准 {hours} 小時內沒有決定，已經逾時；PR 還開著，可以重試讓 worker 重新申請",
                        row.title
                    )),
                    &Principal::System,
                    None,
                )
                .await?;
            }
        }
        Action::ApplyChanges { .. } => {
            if let Some(t) = waiting_task(txn.conn(), row).await? {
                ends = worker::end_task(
                    txn,
                    &t,
                    TaskEnd::NeedsHuman(format!(
                        "「{}」的核准 {hours} 小時內沒有決定，已經逾時；變更還在任務分支上，可以重試讓 worker 重新申請",
                        row.title
                    )),
                    &Principal::System,
                    None,
                )
                .await?;
            }
        }
        Action::CreateListener { listener, .. } => {
            if let Some(l) = automation::listener(txn.conn(), *listener).await?
                && l.approval == Some(row.id)
            {
                automation::set_listener_state(txn.conn(), l.id, ListenerDbState::Disabled, now)
                    .await?;
                automation::set_listener_approval(txn.conn(), l.id, None).await?;
                automation::push(txn, now).await?;
            }
        }
        Action::IncreaseBudget { goal, .. } => {
            Store::deny_goal_budget(txn.conn(), *goal).await?;
            tasks::push_goal(txn, *goal).await?;
        }
        Action::ResumeProject { project } => {
            crate::budget::cooldown_breaker(txn.conn(), *project, now).await?;
        }
        Action::AskUser { question, .. } => {
            let d = deliver(
                txn,
                row,
                question,
                Reply::NoAnswer(format!("{hours} 小時內沒有回答，已經逾時")),
            )
            .await?;
            ends.planner = d.planner;
        }
        _ => {}
    }
    Ok(ends)
}

/// 你對問題的回應。
enum Reply {
    Answer(String),
    /// 沒有答案：略過、逾時；內容是給 agent 看的說明。
    NoAnswer(String),
}

/// 回應送到之後 commit 時要叫醒的。
#[derive(Default)]
struct Delivered {
    worker: bool,
    planner: bool,
    /// 對話 agent 的系統觸發：(project, 對話流裡的項目, 給 agent 的文字)。
    chat: Option<(ProjectId, pots_proto::ItemId, String)>,
}

/// 把你對問題的回應交給問的 agent：worker 的任務被叫醒、Planner 排一個 run、對話 agent（只有回答時）
/// 收到一則系統觸發。
async fn deliver(
    txn: &mut LogTxn<'_>,
    row: &ApprovalRow,
    question: &str,
    reply: Reply,
) -> Result<Delivered, Error> {
    let mut out = Delivered::default();
    let q = tasks::one_line(question, 300);
    if row.task.is_some() {
        if let Some(t) = waiting_task(txn.conn(), row).await?
            && let Some(cond) = waiting_cond(&t)
        {
            // 你的答案是使用者說的話，照原文轉給 worker。
            let text = match &reply {
                Reply::Answer(a) => format!("使用者回答了你的問題「{q}」：{a}"),
                Reply::NoAnswer(why) => format!(
                    "你的問題「{q}」沒有得到回答：{why}。依現有資訊自己決定；真的做不下去就 escalate"
                ),
            };
            watcher::wake_task(txn, &t, &cond, &text).await?;
            out.worker = true;
        }
    } else if let Some(goal) = row.goal {
        let active = tasks::goal(txn.conn(), goal)
            .await?
            .is_some_and(|g| g.state.is_active());
        if active {
            let (answer, outcome) = match reply {
                Reply::Answer(a) => (Some(a), String::new()),
                Reply::NoAnswer(why) => (None, why),
            };
            crate::planner::enqueue(
                txn.conn(),
                row.project,
                goal,
                &crate::planner::PlannerTrigger::Answered {
                    question: question.to_string(),
                    answer,
                    outcome,
                },
            )
            .await?;
            out.planner = true;
        }
    } else if let Reply::Answer(a) = reply {
        let text = format!("（問題卡片的回答）使用者回答了你之前問的「{q}」：\n\n{a}");
        let item = crate::runs::post_system_trigger(
            txn,
            row.project,
            &format!(
                "你回答了「{}」：{}",
                tasks::one_line(question, 100),
                tasks::one_line(&a, 200)
            ),
            &text,
        )
        .await?;
        out.chat = Some((row.project, item, text));
    }
    Ok(out)
}

/// 還在等這筆核准的任務。
async fn waiting_task(
    conn: &mut SqliteConnection,
    row: &ApprovalRow,
) -> Result<Option<TaskRow>, Error> {
    let Some(id) = row.task else {
        return Ok(None);
    };
    Ok(tasks::task(conn, id).await?.filter(|t| {
        matches!(&t.state, TaskState::Waiting { on: WaitCond::Approval { id, .. }, .. } if *id == row.id)
    }))
}

// ---------------------------------------------------------------------------
// 決定與執行

/// 執行核准的動作需要的東西（`App` 的一部分，可以帶進另開的 task）。
#[derive(Clone)]
pub struct Exec {
    pub hub: Arc<Hub>,
    pub store: Store,
    pub watcher: Watcher,
    pub worker: Worker,
}

impl Exec {
    pub fn from_app(app: &App) -> Self {
        Self {
            hub: app.hub.clone(),
            store: app.store.clone(),
            watcher: app.watcher.clone(),
            worker: app.worker.clone(),
        }
    }

    fn now(&self) -> Timestamp {
        self.watcher.clock().now()
    }

    /// 服務啟動時：上次核准了、還沒執行完的動作接著執行（merge 會先查 PR 是不是已經 merge）。
    pub async fn resume(&self) -> Result<(), Error> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM approvals WHERE state = 'executing' ORDER BY created_at",
        )
        .fetch_all(&mut *self.store.acquire().await?)
        .await?;
        for id in ids {
            let exec = self.clone();
            let id: ApprovalId = parse_id(id)?;
            tokio::spawn(async move { exec.run(id).await });
        }
        Ok(())
    }

    /// 執行一筆已核准（`Executing`）的連網動作，記下結果與後續。
    pub async fn run(&self, id: ApprovalId) {
        if let Err(e) = self.run_inner(id).await {
            tracing::error!(%id, error = %e, "無法執行核准的動作");
        }
    }

    async fn run_inner(&self, id: ApprovalId) -> Result<(), Error> {
        let row = {
            let mut conn = self.store.acquire().await?;
            get(&mut conn, id).await?
        };
        let Some(row) = row.filter(|r| r.state == ApprovalState::Executing) else {
            return Ok(());
        };
        let outcome = if row.action.hash() != row.hash {
            Err("動作的內容和核准時不一致，沒有執行".to_string())
        } else {
            match &row.action {
                Action::MergePr {
                    repo,
                    number,
                    sha,
                    method,
                    ..
                } => self.merge(repo, *number, sha, *method).await,
                Action::DeleteBranch { repo, branch } => self.delete_branch(repo, branch).await,
                Action::ApplyChanges {
                    path,
                    branch,
                    base,
                    sha,
                } => self.apply(path, branch, base, sha, row.task).await,
                _ => Err("這個動作不需要另外執行".to_string()),
            }
        };
        let now = self.now();
        let mut txn = self.hub.begin().await?;
        let Some(row) = get(txn.conn(), id)
            .await?
            .filter(|r| r.state == ApprovalState::Executing)
        else {
            return txn.commit().await;
        };
        let mut ends = Ends::default();
        let mut kick = false;
        match outcome {
            Ok(result) => {
                transition(
                    &mut txn,
                    &row,
                    ApprovalState::Executed,
                    &Principal::System,
                    None,
                    Some(&result),
                    now,
                )
                .await?;
                if let Action::MergePr {
                    repo,
                    number,
                    branch,
                    ..
                } = &row.action
                {
                    if let Some(t) = waiting_task(txn.conn(), &row).await? {
                        ends = worker::end_task(
                            &mut txn,
                            &t,
                            TaskEnd::Done(format!("你核准後平台 {result}")),
                            &Principal::System,
                            None,
                        )
                        .await?;
                    }
                    // merge 後的任務分支：開一筆刪除的核准（不推播）。
                    if branch.starts_with("pots/") {
                        let action = Action::DeleteBranch {
                            repo: repo.clone(),
                            branch: branch.clone(),
                        };
                        insert(
                            &mut txn,
                            NewApproval {
                                project: row.project,
                                goal: row.goal,
                                task: None,
                                title: format!("刪除分支 {branch}（PR #{number} 已 merge）"),
                                reason: "PR 已經 merge，任務分支不再需要".into(),
                                action,
                                by: Principal::System,
                                notify: false,
                            },
                            now,
                        )
                        .await?;
                    }
                }
                if matches!(row.action, Action::ApplyChanges { .. })
                    && let Some(t) = waiting_task(txn.conn(), &row).await?
                {
                    ends = worker::end_task(
                        &mut txn,
                        &t,
                        TaskEnd::Done(format!("你核准後平台{result}")),
                        &Principal::System,
                        None,
                    )
                    .await?;
                }
            }
            Err(error) => {
                transition(
                    &mut txn,
                    &row,
                    ApprovalState::Failed,
                    &Principal::System,
                    None,
                    Some(&error),
                    now,
                )
                .await?;
                if let Some(t) = waiting_task(txn.conn(), &row).await?
                    && let Some(cond) = waiting_cond(&t)
                {
                    watcher::wake_task(
                        &mut txn,
                        &t,
                        &cond,
                        &format!(
                            "使用者核准了，但平台沒有完成：{error}。請處理後重新 request_approval"
                        ),
                    )
                    .await?;
                    kick = true;
                }
            }
        }
        let worker = self.worker.clone();
        txn.commit_then(|| {
            ends.notify(&worker, None);
            if kick {
                worker.kick();
            }
        })
        .await
    }

    async fn merge(
        &self,
        repo: &str,
        number: u64,
        sha: &str,
        method: MergeMethod,
    ) -> Result<String, String> {
        let gh = self.watcher.github();
        let config = {
            let mut conn = self.store.acquire().await.map_err(|e| e.to_string())?;
            watcher::github_config(&mut conn)
                .await
                .map_err(|e| e.to_string())?
        };
        let pr = gh
            .pull(&config, repo, number)
            .await
            .map_err(|e| format!("查不到 PR #{number}：{e}"))?;
        if pr.merged {
            return Ok(format!("PR #{number} 已經 merge 了"));
        }
        if !pr.open {
            return Err(format!("PR #{number} 已經關閉，沒有 merge"));
        }
        if !pr.head_sha.eq_ignore_ascii_case(sha) {
            return Err(format!(
                "PR #{number} 在申請之後有新的 commit（現在是 {}，核准的是 {}），沒有 merge",
                &pr.head_sha[..pr.head_sha.len().min(7)],
                &sha[..sha.len().min(7)]
            ));
        }
        if let Ok(Some(ci)) = gh.ci_status(&config, repo, sha).await
            && ci.conclusion != "success"
        {
            return Err(format!(
                "{} 的 CI 失敗，沒有 merge",
                &sha[..sha.len().min(7)]
            ));
        }
        if gh.ci_running(&config, repo, sha).await.unwrap_or(false) {
            return Err(format!(
                "{} 的 CI 還在跑，沒有 merge",
                &sha[..sha.len().min(7)]
            ));
        }
        let merged = gh
            .merge_pull(&config, repo, number, sha, method)
            .await
            .map_err(|e| format!("GitHub 拒絕 merge PR #{number}：{e}"))?;
        let short = &merged[..merged.len().min(7)];
        Ok(if short.is_empty() {
            format!("用 {} merge 了 PR #{number}", method.label())
        } else {
            format!("用 {} merge 了 PR #{number}（{short}）", method.label())
        })
    }

    /// 把本機資料夾的任務分支 fast-forward 進你的資料夾。只在任務分支還是核准的 `sha`、
    /// 資料夾乾淨、而且可以直接 fast-forward 時動手；任何一個不成立就失敗並說明怎麼做。
    async fn apply(
        &self,
        path: &str,
        branch: &str,
        base: &str,
        sha: &str,
        task: Option<TaskId>,
    ) -> Result<String, String> {
        let Some(task) = task else {
            return Err("這筆核准沒有任務".into());
        };
        let source = RepoSource::Local {
            path: path.to_string(),
        };
        let repos = self.worker.repos();
        let mirror = repos.mirror(&source);
        let current = repos
            .task_sha(&mirror, task)
            .await
            .map_err(|e| format!("找不到任務分支：{e}"))?;
        if !current.eq_ignore_ascii_case(sha) {
            return Err(format!(
                "任務分支在申請之後有新的 commit（現在是 {}，核准的是 {}），沒有套用",
                short(&current),
                short(sha)
            ));
        }
        let dir = std::path::Path::new(path);
        let status = crate::workdir::status(dir).await?;
        if status.branch != base {
            return Err(format!(
                "{path} 現在在 {} 分支，核准的是從 {base} 開出來的變更：先切回 {base} 再核准套用",
                status.branch
            ));
        }
        if status.dirty {
            return Err(format!(
                "{path} 有未 commit 的修改：先 commit 或 stash，再重新核准套用"
            ));
        }
        // 任務分支在鏡像裡；先抓進你的 repo 的暫時 ref（只動 .git，不動工作區），再 fast-forward。
        let temp = format!("refs/pots/apply/{task}");
        let fetched = crate::workdir::git(
            dir,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                &mirror.display().to_string(),
                &format!("+refs/pots/tasks/{task}:{temp}"),
            ],
        )
        .await;
        if let Err(e) = fetched {
            return Err(format!("無法讀取任務分支：{e}"));
        }
        let applied = apply_fetched(dir, &temp).await;
        // 不論成敗都收掉暫時 ref。
        let _ = crate::workdir::git(dir, &["update-ref", "-d", &temp]).await;
        match applied {
            Ok(None) => Ok(format!("把 {branch} 套用到 {path}（{}）", short(sha))),
            Ok(Some(tip)) => Ok(format!(
                "把 {branch} 的 commit 接到你的 {} 後面，套用到 {path}（{}，原本是 {}）",
                status.branch,
                short(&tip),
                short(sha)
            )),
            Err(e) => Err(format!("無法把 {branch} 套用到 {path}：{e}")),
        }
    }

    async fn delete_branch(&self, repo: &str, branch: &str) -> Result<String, String> {
        if !branch.starts_with("pots/") {
            return Err("只會刪除平台建立的任務分支（pots/ 開頭）".into());
        }
        let gh = self.watcher.github();
        let config = {
            let mut conn = self.store.acquire().await.map_err(|e| e.to_string())?;
            watcher::github_config(&mut conn)
                .await
                .map_err(|e| e.to_string())?
        };
        match gh.delete_branch(&config, repo, branch).await {
            Ok(true) => Ok(format!("刪除了分支 {branch}")),
            Ok(false) => Ok(format!("分支 {branch} 已經不在了")),
            Err(e) => Err(format!("無法刪除分支 {branch}：{e}")),
        }
    }
}

/// 把抓進暫時 ref 的任務分支套用到你的資料夾目前的分支：能 fast-forward 就直接快轉（回傳 `None`）；
/// 你的分支上有任務分支沒有的 commit 時，先把任務的 commit 接到你的分支後面（不動工作區），
/// 再快轉過去，回傳新的頂端。
async fn apply_fetched(dir: &std::path::Path, temp: &str) -> Result<Option<String>, String> {
    let ff = crate::workdir::git(dir, &["merge-base", "--is-ancestor", "HEAD", temp])
        .await
        .is_ok();
    let (target, rebased) = if ff {
        (temp.to_string(), None)
    } else {
        let tip = crate::workdir::replay_onto_head(dir, temp).await?;
        (tip.clone(), Some(tip))
    };
    crate::workdir::git(dir, &["merge", "--ff-only", "--quiet", &target])
        .await
        .map_err(|e| {
            format!("fast-forward 失敗（工作區衝突，或你的分支剛剛又有新的 commit）：{e}")
        })?;
    Ok(rebased)
}

/// commit 的前 7 碼。
fn short(sha: &str) -> String {
    sha[..sha.len().min(7)].to_string()
}

fn waiting_cond(t: &TaskRow) -> Option<WaitCond> {
    match &t.state {
        TaskState::Waiting { on, .. } => Some(on.clone()),
        _ => None,
    }
}

/// 你核准或拒絕一筆核准。
pub async fn decide(
    app: &App,
    id: ApprovalId,
    approve: bool,
    note: Option<String>,
) -> Result<(), String> {
    let note = match note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) if n.chars().count() > MAX_NOTE_CHARS => {
            return Err(format!("理由最多 {MAX_NOTE_CHARS} 個字"));
        }
        Some(n) => Some(n.to_string()),
        None => None,
    };
    let exec = Exec::from_app(app);
    let now = exec.now();
    let result: Result<Result<bool, String>, Error> = async {
        let mut txn = app.hub.begin().await?;
        let Some(row) = get(txn.conn(), id).await? else {
            return Ok(Err("找不到這筆核准".into()));
        };
        if app.store.project(row.project).await?.is_none() {
            return Ok(Err("找不到這筆核准".into()));
        }
        if row.state != ApprovalState::Pending {
            // 重送目前的狀態，讓客戶端蓋掉它先改的。
            push(&mut txn, &row).await?;
            txn.commit().await?;
            return Ok(Err(format!("這筆核准已經{}", row.state.label())));
        }
        if row.expires_at.0 <= now.0 {
            let ends = expire_one(&mut txn, &row, now).await?;
            let worker = app.worker.clone();
            txn.commit_then(|| ends.notify(&worker, None)).await?;
            return Ok(Err("這筆核准已經逾時".into()));
        }
        if row.action.hash() != row.hash {
            transition(
                &mut txn,
                &row,
                ApprovalState::Failed,
                &Principal::Human,
                Some(note.as_deref()),
                Some("動作的內容和申請時不一致，沒有執行"),
                now,
            )
            .await?;
            txn.commit().await?;
            return Ok(Err("動作的內容和申請時不一致，沒有執行".into()));
        }
        if let Action::AskUser {
            question,
            options,
            allow_other,
        } = &row.action
        {
            let reply = if approve {
                let Some(answer) = note.clone() else {
                    return Ok(Err("要寫下答案".into()));
                };
                if !allow_other && !options.contains(&answer) {
                    return Ok(Err("要選其中一個選項".into()));
                }
                transition(
                    &mut txn,
                    &row,
                    ApprovalState::Executed,
                    &Principal::Human,
                    Some(Some(&answer)),
                    Some("已回答"),
                    now,
                )
                .await?;
                Reply::Answer(answer)
            } else {
                transition(
                    &mut txn,
                    &row,
                    ApprovalState::Rejected,
                    &Principal::Human,
                    Some(note.as_deref()),
                    None,
                    now,
                )
                .await?;
                Reply::NoAnswer(match &note {
                    Some(n) => format!("使用者略過了，理由：{n}"),
                    None => "使用者略過了（沒有附理由）".into(),
                })
            };
            let d = deliver(&mut txn, &row, question, reply).await?;
            let (worker, planner, runs) =
                (app.worker.clone(), app.planner.clone(), app.runs.clone());
            txn.commit_then(|| {
                if d.worker {
                    worker.kick();
                }
                if d.planner {
                    planner.kick();
                }
                if let Some((project, item, text)) = d.chat {
                    runs.enqueue(project, crate::runs::Trigger { item, text }, false);
                }
            })
            .await?;
            return Ok(Ok(false));
        }
        if !approve {
            let row = transition(
                &mut txn,
                &row,
                ApprovalState::Rejected,
                &Principal::Human,
                Some(note.as_deref()),
                None,
                now,
            )
            .await?;
            let kick = on_rejected(&mut txn, &row, now).await?;
            let worker = app.worker.clone();
            txn.commit_then(|| {
                if kick {
                    worker.kick();
                }
            })
            .await?;
            return Ok(Ok(false));
        }
        if row.action.is_remote() {
            transition(
                &mut txn,
                &row,
                ApprovalState::Executing,
                &Principal::Human,
                Some(note.as_deref()),
                None,
                now,
            )
            .await?;
            txn.commit().await?;
            return Ok(Ok(true));
        }
        // 不用連網的動作：在同一個 transaction 裡執行。
        let outcome = execute_local(&mut txn, &row, now).await?;
        let (to, result) = match &outcome {
            Ok(r) => (ApprovalState::Executed, r.as_str()),
            Err(e) => (ApprovalState::Failed, e.as_str()),
        };
        transition(
            &mut txn,
            &row,
            to,
            &Principal::Human,
            Some(note.as_deref()),
            Some(result),
            now,
        )
        .await?;
        let kick_planner = matches!(
            row.action,
            Action::IncreaseBudget { .. } | Action::ResumeProject { .. }
        );
        txn.commit().await?;
        if kick_planner {
            app.planner.kick();
        }
        Ok(match outcome {
            Ok(_) => Ok(false),
            Err(e) => Err(e),
        })
    }
    .await;
    match result {
        Err(e) => {
            tracing::error!(error = %e, "核准失敗");
            Err("伺服器內部錯誤".into())
        }
        Ok(Err(e)) => Err(e),
        Ok(Ok(true)) => {
            tokio::spawn(async move { exec.run(id).await });
            Ok(())
        }
        Ok(Ok(false)) => Ok(()),
    }
}

/// 拒絕之後：merge 叫醒 worker（帶著你的理由），listener 提案撤掉。回傳要不要叫醒 worker。
async fn on_rejected(
    txn: &mut LogTxn<'_>,
    row: &ApprovalRow,
    now: Timestamp,
) -> Result<bool, Error> {
    match &row.action {
        Action::MergePr { .. } => {
            if let Some(t) = waiting_task(txn.conn(), row).await?
                && let Some(cond) = waiting_cond(&t)
            {
                // 你的理由是使用者說的話，照原文轉給 worker。
                let why = row
                    .note
                    .as_deref()
                    .map(|n| format!("，理由：{n}"))
                    .unwrap_or_else(|| "（沒有附理由）".into());
                watcher::wake_task(
                    txn,
                    &t,
                    &cond,
                    &format!("使用者拒絕了 merge{why}。照理由修改後再 open_pr、等 CI，再 request_approval"),
                )
                .await?;
                return Ok(true);
            }
        }
        Action::ApplyChanges { .. } => {
            if let Some(t) = waiting_task(txn.conn(), row).await?
                && let Some(cond) = waiting_cond(&t)
            {
                let why = row
                    .note
                    .as_deref()
                    .map(|n| format!("，理由：{n}"))
                    .unwrap_or_else(|| "（沒有附理由）".into());
                watcher::wake_task(
                    txn,
                    &t,
                    &cond,
                    &format!("使用者拒絕了套用{why}。照理由修改後再 open_pr，再 request_approval"),
                )
                .await?;
                return Ok(true);
            }
        }
        Action::CreateListener { listener, .. } => {
            if let Some(l) = automation::listener(txn.conn(), *listener).await?
                && l.approval == Some(row.id)
            {
                automation::delete_listener(txn.conn(), l.id).await?;
                automation::push(txn, now).await?;
            }
        }
        Action::IncreaseBudget { goal, .. } => {
            Store::deny_goal_budget(txn.conn(), *goal).await?;
            tasks::push_goal(txn, *goal).await?;
        }
        Action::ResumeProject { project } => {
            crate::budget::cooldown_breaker(txn.conn(), *project, now).await?;
        }
        _ => {}
    }
    Ok(false)
}

/// 執行不用連網的動作。回傳結果說明，或沒有執行的原因。
async fn execute_local(
    txn: &mut LogTxn<'_>,
    row: &ApprovalRow,
    now: Timestamp,
) -> Result<Result<String, String>, Error> {
    match &row.action {
        Action::CreateListener { listener, spec } => {
            let Some(l) = automation::listener(txn.conn(), *listener).await? else {
                return Ok(Err("這個 listener 已經被刪除".into()));
            };
            if l.state != ListenerDbState::Proposed || l.approval != Some(row.id) || l.spec != *spec
            {
                return Ok(Err("listener 在提案之後被改過，沒有啟用".into()));
            }
            automation::set_listener_state(txn.conn(), l.id, ListenerDbState::Active, now).await?;
            automation::set_listener_approval(txn.conn(), l.id, None).await?;
            automation::push(txn, now).await?;
            Ok(Ok("listener 已啟用".into()))
        }
        Action::EditUserMd { base_rev, body } => {
            let current = crate::user_md::current(txn.conn()).await?;
            if current.rev != *base_rev {
                return Ok(Err(format!(
                    "USER.md 在提案之後被改過（現在是 v{}），沒有寫入",
                    current.rev
                )));
            }
            match crate::user_md::save(txn, *base_rev, body, &row.requested_by).await? {
                Ok(v) => Ok(Ok(format!("USER.md 已更新為 v{}", v.rev))),
                Err(e) => Ok(Err(e)),
            }
        }
        Action::EditProjectNotes {
            project,
            artifact,
            base_rev,
            body,
        } => {
            let current = crate::user_md::project_notes(txn.conn(), *project).await?;
            let current = match (artifact, current) {
                (Some(a), Some(c)) if c.id == *a && c.rev == *base_rev => c,
                (None, None) => crate::user_md::ensure_project_notes(txn, *project).await?,
                _ => {
                    return Ok(Err("Project Notes 在提案之後被改過，沒有寫入".into()));
                }
            };
            match artifacts::write_approved(txn.conn(), &current, body, &row.requested_by).await? {
                Some(new) => {
                    artifacts::push_changed(txn, &new).await?;
                    Ok(Ok(format!("Project Notes 已更新為 v{}", new.rev)))
                }
                None => Ok(Ok("Project Notes 的內容沒有變".into())),
            }
        }
        Action::IncreaseBudget { goal, tokens } => {
            let Some(g) = tasks::goal(txn.conn(), *goal).await? else {
                return Ok(Err("找不到這個 Goal".into()));
            };
            sqlx::query(
                "UPDATE goals SET budget_tokens = COALESCE(budget_tokens, 0) + ?, budget_denied = 0, \
                 updated_at = ? WHERE id = ?",
            )
            .bind(*tokens as i64)
            .bind(now.0)
            .bind(goal.to_string())
            .execute(txn.conn())
            .await?;
            crate::planner::enqueue(
                txn.conn(),
                g.project,
                *goal,
                &crate::planner::PlannerTrigger::BudgetApproved { tokens: *tokens },
            )
            .await?;
            tasks::push_goal(txn, *goal).await?;
            Ok(Ok(format!("追加了 {tokens} tokens")))
        }
        Action::ResumeProject { project } => {
            sqlx::query("UPDATE projects SET breaker_until = NULL WHERE id = ?")
                .bind(project.to_string())
                .execute(txn.conn())
                .await?;
            for g in tasks::goals(txn.conn(), *project).await? {
                if g.state.is_active() {
                    crate::planner::enqueue(
                        txn.conn(),
                        *project,
                        g.id,
                        &crate::planner::PlannerTrigger::BreakerCleared,
                    )
                    .await?;
                }
            }
            crate::tools::post_card(
                txn,
                *project,
                pots_proto::Card::Notice {
                    title: "預算熔斷已解除".into(),
                    body: "已恢復這個 project 的 run。".into(),
                    actions: Vec::new(),
                },
                None,
            )
            .await?;
            Ok(Ok("已恢復這個 project 的 run".into()))
        }
        Action::DeleteSchedule { schedule, .. } => {
            match automation::schedule(txn.conn(), *schedule).await? {
                Some(s) if s.owner.project() == Some(row.project) => {}
                _ => return Ok(Err("這個排程已經不在了".into())),
            }
            automation::delete_schedule(txn.conn(), *schedule).await?;
            automation::push(txn, now).await?;
            Ok(Ok("排程已刪除".into()))
        }
        Action::DeleteListener { listener, .. } => {
            match automation::listener(txn.conn(), *listener).await? {
                Some(l) if l.spec.project == row.project => {}
                _ => return Ok(Err("這個 listener 已經不在了".into())),
            }
            automation::delete_listener(txn.conn(), *listener).await?;
            // 刪掉的是還在等核准的提案時，那筆核准撤回。
            sync_listeners(txn).await?;
            automation::push(txn, now).await?;
            Ok(Ok("listener 已刪除".into()))
        }
        Action::ArchiveArtifact { artifact, .. } => {
            match artifacts::get(txn.conn(), *artifact).await? {
                Some(a) if a.project == row.project => {}
                _ => return Ok(Err("這份 artifact 已經不在了".into())),
            }
            artifacts::set_archived(txn.conn(), *artifact, now).await?;
            let (project, artifact) = (row.project, *artifact);
            txn.push(|seq| ServerEvent::ArtifactArchived {
                seq,
                project,
                artifact,
            })
            .await?;
            Ok(Ok("artifact 已封存".into()))
        }
        Action::MergePr { .. } | Action::DeleteBranch { .. } | Action::ApplyChanges { .. } => {
            Ok(Err("這個動作要另外執行".into()))
        }
        Action::AskUser { .. } => Ok(Err("問題要用回答處理".into())),
    }
}

// ---------------------------------------------------------------------------
// agent 的工具

/// 卡片放哪裡：對話 agent 附在這次回覆下面，其他貼到 project 的對話流。
async fn place_card(txn: &mut LogTxn<'_>, ctx: &ToolCtx, id: ApprovalId) -> Result<bool, Error> {
    if ctx.scope == Scope::Project {
        return Ok(true);
    }
    crate::tools::post_card(txn, ctx.project, Card::Approval { id }, Some(ctx.run_id)).await?;
    Ok(false)
}

/// `request_approval`：申請 merge 任務的 PR。worker 申請自己的任務（這一輪就結束），Planner 申請自己
/// Goal 裡已經結束或需要決定、有 PR 的任務。
pub async fn request_merge(
    ctx: &ToolCtx,
    task_arg: Option<&str>,
    method: Option<&str>,
    reason: &str,
) -> Result<String, ToolError> {
    if ctx.scope.task().is_some() && ctx.finished.load(Ordering::SeqCst) {
        return Err(bad("這一輪已經結束，沒有執行"));
    }
    let reason = tasks::check_text("reason ", reason, MAX_NOTE_CHARS, true).map_err(bad)?;
    let id: TaskId = match (task_arg.filter(|s| !s.trim().is_empty()), ctx.scope) {
        (Some(s), Scope::Goal(_)) => crate::tools::parse_id("task_id", s)?,
        (_, Scope::Task { task, .. }) => task,
        (None, Scope::Goal(_)) => return Err(bad("要給 task_id")),
        (_, Scope::Project | Scope::Global) => {
            return Err(bad("request_approval 只有 worker 與 Planner 能用"));
        }
    };
    let method = match method.map(str::trim).filter(|s| !s.is_empty()) {
        Some(m) => {
            Some(MergeMethod::parse(m).ok_or_else(|| bad("method 要是 squash、merge 或 rebase"))?)
        }
        None => None,
    };
    // 1. 先讀（不持有日誌鎖）：任務、PR、有沒有已經在等的核准。
    let check = |t: &TaskRow, ctx: &ToolCtx| -> Result<(), ToolError> {
        if Some(t.goal) != ctx.scope.goal() {
            return Err(bad(format!("你負責的 Goal 沒有任務 {}", t.id)));
        }
        let ok = match ctx.scope {
            Scope::Task { .. } => t.state == TaskState::Running,
            _ => matches!(
                t.state,
                TaskState::Done { .. } | TaskState::NeedsHuman { .. } | TaskState::Failed { .. }
            ),
        };
        if !ok {
            return Err(bad(format!(
                "任務「{}」現在是 {}，不能申請 merge{}",
                t.title,
                t.state.name(),
                if ctx.scope.task().is_none() {
                    "（Planner 只能替已經結束或需要決定的任務申請；進行中的由 worker 自己申請）"
                } else {
                    ""
                }
            )));
        }
        if t.pr.is_none() {
            return Err(bad("這個任務還沒有 PR：先 open_pr"));
        }
        Ok(())
    };
    let (task, merge_default) = {
        let mut conn = ctx.store.acquire().await.map_err(internal)?;
        let task = tasks::task(&mut conn, id)
            .await
            .map_err(internal)?
            .ok_or_else(|| bad(format!("沒有任務 {id}")))?;
        check(&task, ctx)?;
        let goal = tasks::goal(&mut conn, task.goal).await.map_err(internal)?;
        if !goal.is_some_and(|g| g.state.is_active()) {
            return Err(bad("Goal 已經結束"));
        }
        if !pending_blocking(&mut conn, None, Some(id))
            .await
            .map_err(internal)?
            .is_empty()
        {
            return Err(bad("這個任務已經有一筆等使用者核准的 merge"));
        }
        let m = project_merge_method(&mut conn, ctx.project)
            .await
            .map_err(internal)?;
        (task, m)
    };
    // 本機資料夾：沒有 PR 與 CI，核准的是「把任務分支套用到資料夾」。
    if let Some(path) = task
        .pr
        .as_deref()
        .and_then(|c| c.strip_prefix("local:"))
        .map(str::to_string)
    {
        let workdir = {
            let mut conn = ctx.store.acquire().await.map_err(internal)?;
            crate::store::Store::workdir(&mut conn, ctx.project)
                .await
                .map_err(internal)?
        };
        if !matches!(&workdir, Some(pots_proto::WorkDir::Local { path: p }) if p == &path) {
            return Err(bad(
                "這個 project 的工作資料夾換過了：先重新 open_pr 再申請".to_string(),
            ));
        }
        let branch = task
            .branch
            .clone()
            .ok_or_else(|| bad("這個任務還沒有分支：先 open_pr"))?;
        let base = task
            .base
            .clone()
            .ok_or_else(|| bad("這個任務沒有記下基準分支：先重新 open_pr"))?;
        let repos = ctx.worker.repos();
        let mirror = repos.mirror(&RepoSource::Local { path: path.clone() });
        let sha = repos
            .task_sha(&mirror, task.id)
            .await
            .map_err(|e| bad(format!("找不到任務分支：{e}")))?;
        let action = Action::ApplyChanges {
            path: path.clone(),
            branch: branch.clone(),
            base,
            sha: sha.clone(),
        };
        let title = format!("套用變更到 {path}：{}", tasks::one_line(&task.title, 120));
        let (approval, _) = request_approval_card(
            ctx,
            &task,
            action,
            title,
            &reason,
            format!("套用變更到 {path}"),
            |t| check(t, ctx),
        )
        .await?;
        return Ok(json!({
            "approval_id": approval.to_string(),
            "path": path,
            "branch": branch,
            "sha": sha,
            "note": "已送出核准請求：使用者核准後平台會把分支套用到資料夾、任務自動完成；拒絕時你會被叫醒並看到理由。這一輪到此結束。",
        })
        .to_string());
    }
    let change = task.pr.clone().unwrap_or_default();
    let (repo, number) = crate::watcher::event::parse_change(&change)
        .map_err(|e| bad(format!("PR 格式不對：{e}")))?;
    // 2. 查 PR 與 CI（連網）。
    let Some(watcher) = ctx.worker.watcher() else {
        return Err(bad("服務還沒準備好"));
    };
    let gh = watcher.github();
    let config = {
        let mut conn = ctx.store.acquire().await.map_err(internal)?;
        watcher::github_config(&mut conn).await.map_err(internal)?
    };
    let pr = gh
        .pull(&config, &repo, number)
        .await
        .map_err(|e| bad(format!("查不到 PR #{number}：{e}")))?;
    if pr.merged {
        return Err(bad(format!("PR #{number} 已經 merge 了")));
    }
    if !pr.open {
        return Err(bad(format!("PR #{number} 已經關閉")));
    }
    match gh.ci_status(&config, &repo, &pr.head_sha).await {
        Ok(Some(ci)) if ci.conclusion != "success" => {
            return Err(bad(format!(
                "PR #{number} 的 CI 失敗了：先修好、open_pr、等 CI 通過再申請"
            )));
        }
        Ok(_) => {}
        Err(e) => return Err(bad(format!("查不到 CI 狀態：{e}"))),
    }
    if gh
        .ci_running(&config, &repo, &pr.head_sha)
        .await
        .map_err(|e| bad(format!("查不到 CI 狀態：{e}")))?
    {
        return Err(bad(format!(
            "PR #{number} 的 CI 還在跑：先用 wait_for（ci_finished）等它結束再申請"
        )));
    }
    let method = method.unwrap_or(merge_default);
    let title = format!(
        "merge PR #{number}：{}",
        tasks::one_line(
            if pr.title.is_empty() {
                &task.title
            } else {
                &pr.title
            },
            120
        )
    );
    let action = Action::MergePr {
        repo: repo.clone(),
        number,
        sha: pr.head_sha.clone(),
        method,
        branch: pr.head_ref.clone(),
        base: pr.base_ref.clone(),
        url: pr.url.clone(),
    };
    // 3. 寫入：再檢查一次任務（期間可能被取消），任務轉 `Waiting { Approval }`。
    let (approval, _) = request_approval_card(
        ctx,
        &task,
        action,
        title,
        &reason,
        format!("merge PR #{number}"),
        |t| check(t, ctx),
    )
    .await?;
    if ctx.scope.task().is_some() {
        // 和 wait_for 一樣：等核准時這一輪就結束。
        ctx.finish();
    }
    Ok(json!({
        "approval_id": approval.to_string(),
        "pr": change,
        "sha": pr.head_sha,
        "method": method.as_str(),
        "note": "已送出核准請求：使用者核准後平台會 merge，任務自動完成；拒絕時你會被叫醒並看到理由。這一輪到此結束。",
    })
    .to_string())
}

/// 寫入核准、把任務掛成「等這筆核准」、貼卡片。`verify` 在 transaction 裡再跑一次（期間可能被取消）。
async fn request_approval_card(
    ctx: &ToolCtx,
    task: &TaskRow,
    action: Action,
    title: String,
    reason: &str,
    what: String,
    verify: impl FnOnce(&TaskRow) -> Result<(), ToolError>,
) -> Result<(ApprovalId, bool), ToolError> {
    let ask = matches!(action, Action::AskUser { .. });
    let id = task.id;
    let now = ctx.clock.now();
    let result: Result<Result<(ApprovalId, bool), ToolError>, Error> = async {
        let mut txn = ctx.hub.begin().await?;
        let Some(task) = tasks::task(txn.conn(), id).await? else {
            return Ok(Err(bad(format!("沒有任務 {id}"))));
        };
        if let Err(e) = verify(&task) {
            return Ok(Err(e));
        }
        let row = insert(
            &mut txn,
            NewApproval {
                project: ctx.project,
                goal: Some(task.goal),
                task: Some(task.id),
                action,
                title,
                reason: reason.to_string(),
                by: ctx.actor.clone(),
                notify: true,
            },
            now,
        )
        .await?;
        watcher::suspend_task(
            &mut txn,
            &task,
            ctx.project,
            WaitCond::Approval {
                id: row.id,
                what,
                ask,
            },
            row.expires_at,
            &ctx.actor,
            now,
        )
        .await?;
        let attach = place_card(&mut txn, ctx, row.id).await?;
        txn.commit().await?;
        Ok(Ok((row.id, attach)))
    }
    .await;
    let (approval, attach) = result.map_err(internal)??;
    if attach {
        ctx.cards
            .lock()
            .unwrap()
            .push(Card::Approval { id: approval });
    }
    Ok((approval, attach))
}

/// `propose_user_md_edit`／`propose_project_note`：對話 agent（與 Planner）提案修改，你核准後才寫入。
pub async fn propose_edit(
    ctx: &ToolCtx,
    user_md: bool,
    edits: &[Edit],
    reason: &str,
) -> Result<String, ToolError> {
    let reason = tasks::check_text("reason ", reason, MAX_NOTE_CHARS, true).map_err(bad)?;
    let now = ctx.clock.now();
    let result: Result<Result<(ApprovalId, bool), ToolError>, Error> = async {
        let mut txn = ctx.hub.begin().await?;
        let (action, title, filter) = if user_md {
            let current = crate::user_md::current(txn.conn()).await?;
            let body = match artifacts::apply_edits(&current.body, edits, true)
                .and_then(|b| crate::user_md::check(&b))
            {
                Ok(b) => b,
                Err(e) => return Ok(Err(bad(e))),
            };
            if body == current.body {
                return Ok(Err(bad("這些編輯沒有改變 USER.md")));
            }
            (
                Action::EditUserMd {
                    base_rev: current.rev,
                    body,
                },
                "修改 USER.md".to_string(),
                "{\"kind\":\"edit_user_md\"%".to_string(),
            )
        } else {
            let notes = crate::user_md::project_notes(txn.conn(), ctx.project).await?;
            let base = notes.as_ref().map(|n| n.body.clone()).unwrap_or_default();
            let body = match artifacts::apply_edits(&base, edits, true) {
                Ok(b) if b.chars().count() > artifacts::MAX_BODY_CHARS => {
                    return Ok(Err(bad("Project Notes 太長")));
                }
                Ok(b) => b,
                Err(e) => return Ok(Err(bad(e))),
            };
            if body.trim() == base.trim() {
                return Ok(Err(bad("這些編輯沒有改變 Project Notes")));
            }
            (
                Action::EditProjectNotes {
                    project: ctx.project,
                    artifact: notes.as_ref().map(|n| n.id),
                    base_rev: notes.as_ref().map_or(0, |n| n.rev),
                    body,
                },
                "修改 Project Notes".to_string(),
                format!(
                    "{{\"kind\":\"edit_project_notes\",\"project\":\"{}\"%",
                    ctx.project
                ),
            )
        };
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM approvals WHERE state = 'pending' AND action LIKE ?",
        )
        .bind(&filter)
        .fetch_one(txn.conn())
        .await?;
        if pending > 0 {
            return Ok(Err(bad(format!(
                "已經有一筆「{title}」等使用者核准；等它決定後再提案"
            ))));
        }
        let row = insert(
            &mut txn,
            NewApproval {
                project: ctx.project,
                goal: ctx.scope.goal(),
                task: None,
                action,
                title,
                reason,
                by: ctx.actor.clone(),
                notify: true,
            },
            now,
        )
        .await?;
        let attach = place_card(&mut txn, ctx, row.id).await?;
        txn.commit().await?;
        Ok(Ok((row.id, attach)))
    }
    .await;
    let (id, attach) = result.map_err(internal)??;
    if attach {
        ctx.cards.lock().unwrap().push(Card::Approval { id });
    }
    Ok(json!({
        "approval_id": id.to_string(),
        "note": "已提案，使用者會看到一張 diff 卡片；核准後才會寫入。",
    })
    .to_string())
}

/// 對話 agent 申請刪除排程、listener 或封存 artifact。同一個對象已經有一筆等核准的申請時回
/// `Ok(Err(說明))`。卡片由呼叫端在 commit 之後附上（回傳的 `bool`：要不要附在這次回覆下面）。
pub async fn propose_removal(
    txn: &mut LogTxn<'_>,
    ctx: &ToolCtx,
    action: Action,
    title: String,
    reason: &str,
    now: Timestamp,
) -> Result<Result<(ApprovalId, bool), String>, Error> {
    let reason = match tasks::check_text("reason ", reason, MAX_NOTE_CHARS, true) {
        Ok(r) => r,
        Err(e) => return Ok(Err(e)),
    };
    // 動作的 JSON 以 kind 與對象的 ID 開頭（serde 依欄位宣告的順序）。
    let prefix = match &action {
        Action::DeleteSchedule { schedule, .. } => {
            format!("{{\"kind\":\"delete_schedule\",\"schedule\":\"{schedule}\"%")
        }
        Action::DeleteListener { listener, .. } => {
            format!("{{\"kind\":\"delete_listener\",\"listener\":\"{listener}\"%")
        }
        Action::ArchiveArtifact { artifact, .. } => {
            format!("{{\"kind\":\"archive_artifact\",\"artifact\":\"{artifact}\"%")
        }
        _ => return Ok(Err("這個動作不能由對話 agent 申請".into())),
    };
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM approvals WHERE state = 'pending' AND action LIKE ?",
    )
    .bind(&prefix)
    .fetch_one(txn.conn())
    .await?;
    if pending > 0 {
        return Ok(Err(format!(
            "已經有一筆「{title}」等使用者核准；等它決定後再申請"
        )));
    }
    let row = insert(
        txn,
        NewApproval {
            project: ctx.project,
            goal: None,
            task: None,
            action,
            title,
            reason,
            by: ctx.actor.clone(),
            notify: true,
        },
        now,
    )
    .await?;
    let attach = place_card(txn, ctx, row.id).await?;
    Ok(Ok((row.id, attach)))
}

/// `create_listener` 的提案：listener 先存成 `pending`，等這筆核准。
pub async fn propose_listener(
    txn: &mut LogTxn<'_>,
    ctx: &ToolCtx,
    listener: ListenerId,
    spec: &ListenerSpec,
    now: Timestamp,
) -> Result<ApprovalId, Error> {
    let row = insert(
        txn,
        NewApproval {
            project: ctx.project,
            goal: None,
            task: None,
            action: Action::CreateListener {
                listener,
                spec: spec.clone(),
            },
            title: format!("啟用 listener：{}", automation::listener_title(spec)),
            reason: "對話 agent 提案；核准後符合條件的外部事件會自動開工或通知對話".into(),
            by: ctx.actor.clone(),
            notify: true,
        },
        now,
    )
    .await?;
    automation::set_listener_approval(txn.conn(), listener, Some(row.id)).await?;
    Ok(row.id)
}

/// 問題最多幾個字。
pub const MAX_QUESTION_CHARS: usize = 1000;
/// 最多幾個選項、每個選項最多幾個字。
pub const MAX_OPTIONS: usize = 6;
pub const MAX_OPTION_CHARS: usize = 100;
/// 對話 agent 同時最多幾個還沒回答的問題。
const MAX_CHAT_QUESTIONS: i64 = 3;

/// `ask_user`：問使用者一個問題，使用者在卡片上回答或略過。worker 的任務轉成等回答（這一輪就結束），
/// 回答後被叫醒；Planner 繼續做別的，回答後由新的 run 收到；對話 agent 的卡片附在這次回覆下面，
/// 回答以系統觸發送回對話。
pub async fn ask(
    ctx: &ToolCtx,
    question: &str,
    options: &[String],
    allow_other: bool,
    reason: Option<&str>,
) -> Result<String, ToolError> {
    if ctx.scope == Scope::Global {
        return Err(bad("總管不能用 ask_user：直接在對話裡問"));
    }
    if ctx.scope.task().is_some() && ctx.finished.load(Ordering::SeqCst) {
        return Err(bad("這一輪已經結束，沒有執行"));
    }
    let question =
        tasks::check_text("question ", question, MAX_QUESTION_CHARS, true).map_err(bad)?;
    let reason = match reason {
        Some(r) => tasks::check_text("reason ", r, MAX_NOTE_CHARS, false).map_err(bad)?,
        None => String::new(),
    };
    let mut opts: Vec<String> = Vec::new();
    for o in options {
        let o = o.trim();
        if o.is_empty() || opts.iter().any(|x| x == o) {
            continue;
        }
        if o.contains('\n') || o.chars().count() > MAX_OPTION_CHARS {
            return Err(bad(format!(
                "每個選項最多 {MAX_OPTION_CHARS} 個字、只能有一行"
            )));
        }
        opts.push(o.to_string());
    }
    if opts.len() > MAX_OPTIONS {
        return Err(bad(format!("選項最多 {MAX_OPTIONS} 個")));
    }
    // 沒有選項時只能自由回答。
    let allow_other = allow_other || opts.is_empty();
    let action = Action::AskUser {
        question: question.clone(),
        options: opts.clone(),
        allow_other,
    };
    let title = tasks::one_line(&question, 200);
    let id = match ctx.scope {
        Scope::Task { task, .. } => {
            let row = {
                let mut conn = ctx.store.acquire().await.map_err(internal)?;
                let row = tasks::task(&mut conn, task)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| bad(format!("沒有任務 {task}")))?;
                let goal = tasks::goal(&mut conn, row.goal).await.map_err(internal)?;
                if !goal.is_some_and(|g| g.state.is_active()) {
                    return Err(bad("Goal 已經結束"));
                }
                row
            };
            let check = |t: &TaskRow| -> Result<(), ToolError> {
                if t.state != TaskState::Running {
                    return Err(bad(format!("任務現在是 {}，不能發問", t.state.name())));
                }
                Ok(())
            };
            check(&row)?;
            let (id, _) = request_approval_card(
                ctx,
                &row,
                action,
                title,
                &reason,
                tasks::one_line(&question, 120),
                check,
            )
            .await?;
            ctx.finish();
            id
        }
        Scope::Goal(goal) => {
            let now = ctx.clock.now();
            let result: Result<Result<ApprovalId, ToolError>, Error> = async {
                let mut txn = ctx.hub.begin().await?;
                let active = tasks::goal(txn.conn(), goal)
                    .await?
                    .is_some_and(|g| g.state.is_active());
                if !active {
                    return Ok(Err(bad("Goal 已經結束")));
                }
                if let Some(q) = pending_planner_questions(txn.conn(), goal).await?.first() {
                    return Ok(Err(bad(format!(
                        "已經有一個問題在等使用者回答：「{}」；等它回答後再問",
                        q.title
                    ))));
                }
                let row = insert(
                    &mut txn,
                    NewApproval {
                        project: ctx.project,
                        goal: Some(goal),
                        task: None,
                        action,
                        title,
                        reason,
                        by: ctx.actor.clone(),
                        notify: true,
                    },
                    now,
                )
                .await?;
                place_card(&mut txn, ctx, row.id).await?;
                txn.commit().await?;
                Ok(Ok(row.id))
            }
            .await;
            result.map_err(internal)??
        }
        Scope::Project => {
            let now = ctx.clock.now();
            let result: Result<Result<ApprovalId, ToolError>, Error> = async {
                let mut txn = ctx.hub.begin().await?;
                let pending: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM approvals WHERE state = 'pending' AND project_id = ? \
                     AND goal_id IS NULL AND task_id IS NULL AND action LIKE '{\"kind\":\"ask_user\"%'",
                )
                .bind(ctx.project.to_string())
                .fetch_one(txn.conn())
                .await?;
                if pending >= MAX_CHAT_QUESTIONS {
                    return Ok(Err(bad(format!(
                        "已經有 {pending} 個問題在等使用者回答；等使用者回答後再問，或直接在回覆裡問"
                    ))));
                }
                let row = insert(
                    &mut txn,
                    NewApproval {
                        project: ctx.project,
                        goal: None,
                        task: None,
                        action,
                        title,
                        reason,
                        by: ctx.actor.clone(),
                        notify: true,
                    },
                    now,
                )
                .await?;
                txn.commit().await?;
                Ok(Ok(row.id))
            }
            .await;
            let id = result.map_err(internal)??;
            ctx.cards.lock().unwrap().push(Card::Approval { id });
            id
        }
        Scope::Global => unreachable!(),
    };
    let note = match ctx.scope {
        Scope::Task { .. } => {
            "已經問了使用者；任務轉成等回答，這一輪到此結束。使用者回答（或略過、逾時）後你會被叫醒並看到答案。"
        }
        Scope::Goal(_) => {
            "已經問了使用者；可以先做不依賴答案的事。使用者回答（或略過、逾時）後會用新的觸發叫醒你。"
        }
        _ => {
            "問題卡片會附在這次回覆下面：回覆裡用一句話帶過就好，不要再把問題和選項重寫一遍，然後結束這一輪；使用者回答後你會收到答案。"
        }
    };
    Ok(json!({
        "question_id": id.to_string(),
        "options": opts,
        "allow_other": allow_other,
        "note": note,
    })
    .to_string())
}

/// project 預設的 merge 方式。
pub async fn project_merge_method(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<MergeMethod, Error> {
    let m: Option<String> = sqlx::query_scalar("SELECT merge_method FROM projects WHERE id = ?")
        .bind(project.to_string())
        .fetch_optional(conn)
        .await?;
    Ok(m.as_deref()
        .and_then(MergeMethod::parse)
        .unwrap_or_default())
}

pub async fn set_project_merge_method(
    conn: &mut SqliteConnection,
    project: ProjectId,
    method: MergeMethod,
) -> Result<(), Error> {
    sqlx::query("UPDATE projects SET merge_method = ? WHERE id = ?")
        .bind(method.as_str())
        .bind(project.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_covers_every_field() {
        let a = Action::MergePr {
            repo: "github.com/me/app".into(),
            number: 42,
            sha: "a".repeat(40),
            method: MergeMethod::Squash,
            branch: "pots/x".into(),
            base: "main".into(),
            url: String::new(),
        };
        let mut b = a.clone();
        if let Action::MergePr { method, .. } = &mut b {
            *method = MergeMethod::Merge;
        }
        assert_ne!(a.hash(), b.hash());
        assert_eq!(a.hash(), a.clone().hash());
        assert_eq!(a.hash().len(), 64);
        assert!(a.is_remote());
        // `sync_listeners` 與提案的重複檢查依 JSON 的開頭比對種類。
        let json = serde_json::to_string(&Action::EditUserMd {
            base_rev: 0,
            body: String::new(),
        })
        .unwrap();
        assert!(json.starts_with("{\"kind\":\"edit_user_md\""), "{json}");
        let json = serde_json::to_string(&Action::EditProjectNotes {
            project: ProjectId::from_u64(3),
            artifact: None,
            base_rev: 0,
            body: String::new(),
        })
        .unwrap();
        assert!(
            json.starts_with(&format!(
                "{{\"kind\":\"edit_project_notes\",\"project\":\"{}\"",
                ProjectId::from_u64(3)
            )),
            "{json}"
        );
    }
}
