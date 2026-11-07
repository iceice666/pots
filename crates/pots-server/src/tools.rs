//! 平台工具（`proposal/agent-collaboration.md#唯讀基礎工具`、`#profile-一覽`）。
//!
//! 每個 run 依 profile 組出工具：唯讀基礎集（`get_status`、`list_goals`、`read_artifact`）
//! 一律都有，其他只給 profile 授權的。模型拿不到的工具就呼叫不到（loop 對不認得的名稱直接
//! 回錯誤），所以 [`Gate`] 不需要再擋授權；範圍（只能動自己的 project、Planner 只能動自己的
//! Goal、只能寫 profile 允許的 artifact 種類）由各工具檢查，錯誤以文字回給模型。
//!
//! 狀態變更都在 [`LogTxn`] 裡做，和事件同一個 transaction；需要叫醒或打斷 Planner 的，
//! 在 commit 之後（`commit_then`）通知它。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pots_llm::ToolSpec;
use pots_llm::agent::{Hook, Tool, ToolError};
use pots_proto::{
    ArtifactId, ArtifactKind, Card, GoalId, GoalState, ItemId, ProjectId, ServerEvent,
    StreamItemKind, StreamItemView, StreamScope, TaskId, Timestamp,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Error;
use crate::artifacts::{self, ArtifactRow, Edit, WriteOutcome};
use crate::hub::{Hub, LogTxn};
use crate::planner::{self, Planner, PlannerTrigger};
use crate::profiles::{AgentProfile, ToolName};
use crate::runs::format_utc;
use crate::store::{ItemStatus, Store};
use crate::tasks::{self, Cause, GoalRow, Principal, TaskRow, TaskState};

/// 里程碑最多幾個字。
const MAX_MILESTONE_CHARS: usize = 500;
/// 理由、摘要最多幾個字。
const MAX_REASON_CHARS: usize = 1000;
/// 任務 summary 最多幾個字。
const MAX_SUMMARY_CHARS: usize = 2000;
/// 一次 run 最多貼幾張里程碑（避免洗版使用者的對話流）。
const MAX_MILESTONES_PER_RUN: usize = 3;
/// 一次 run 最多開幾個 Goal。
const MAX_GOALS_PER_RUN: usize = 3;
/// 一個 project 同時最多幾個進行中的 Goal。
const MAX_ACTIVE_GOALS: usize = 20;

/// 工具作用的範圍。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// 對話 agent：整個 project。
    Project,
    /// Planner：project 裡的一個 Goal。
    Goal(GoalId),
}

/// 一個 run 的工具共用的 context。
pub struct ToolCtx {
    pub hub: Arc<Hub>,
    pub store: Store,
    pub planner: Planner,
    pub project: ProjectId,
    pub scope: Scope,
    pub actor: Principal,
    /// `artifact_reads` 的讀者，例如 `chat:<project>`。
    pub reader: String,
    /// `write_artifact` 可以寫的種類。
    pub artifact_kinds: Vec<ArtifactKind>,
    pub run_id: Uuid,
    /// 工具產生、要附在這次回覆下面的卡片（對話 agent 用）。
    pub cards: Arc<Mutex<Vec<Card>>>,
    /// 有沒有使用者訊息在排隊（對話 agent 的 `get_status` 用）。
    pub queued: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// 這個 run 該結束了（例如 `complete_goal` 成功）；[`Gate`] 在下一輪開始前停下。
    pub finished: Arc<AtomicBool>,
    /// 這個 run 已經貼了幾張里程碑、開了幾個 Goal。
    pub milestones: AtomicUsize,
    pub goals_created: AtomicUsize,
}

impl ToolCtx {
    /// 這個 run 的 [`Gate`]。
    pub fn gate(&self) -> Gate {
        Gate {
            finished: self.finished.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    GetStatus,
    ListGoals,
    ReadArtifact,
    Granted(ToolName),
}

struct PlatformTool {
    kind: Kind,
    ctx: Arc<ToolCtx>,
}

/// 依 profile 組出這個 run 的工具。
pub fn build(profile: &AgentProfile, ctx: Arc<ToolCtx>) -> Vec<Arc<dyn Tool>> {
    let mut kinds = vec![Kind::GetStatus, Kind::ListGoals, Kind::ReadArtifact];
    kinds.extend(profile.tools.iter().map(|t| Kind::Granted(*t)));
    kinds
        .into_iter()
        .map(|kind| {
            Arc::new(PlatformTool {
                kind,
                ctx: ctx.clone(),
            }) as Arc<dyn Tool>
        })
        .collect()
}

/// 平台的 gate：每次工具呼叫前後把關（`proposal/agent-collaboration.md#一個-loop`）。
///
/// 授權由「只把 profile 的工具交給模型」保證。這裡處理「成功後要結束這個 run」的工具
/// （`complete_goal`）：工具成功時設 `finished`，下一輪開始前停下。不在 `before_tool` 回
/// `AllowThenStop`，否則參數錯誤時模型也沒機會重試。預算、熔斷與 trace 在 M6。
pub struct Gate {
    finished: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Hook for Gate {
    async fn before_turn(&self, _turn: usize) -> bool {
        !self.finished.load(Ordering::SeqCst)
    }
}

fn internal(e: Error) -> ToolError {
    tracing::error!(error = %e, "工具執行失敗");
    ToolError("伺服器內部錯誤，請稍後再試".into())
}

fn parse<T: DeserializeOwned>(arguments: &str) -> Result<T, ToolError> {
    let text = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    serde_json::from_str(text).map_err(|e| ToolError(format!("參數錯誤：{e}")))
}

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError(msg.into())
}

fn parse_id<T: std::str::FromStr>(what: &str, s: &str) -> Result<T, ToolError> {
    s.trim()
        .parse()
        .map_err(|_| bad(format!("{what} 格式不對：{s}")))
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn edits_schema() -> Value {
    json!({
        "type": "array",
        "description": "依序套用的編輯。以 `## ` 標題區段為單位：replace_section 取代既有區段的內文，\
            append_section 在最後新增區段，replace_all 整份取代（只能在新建時用）。",
        "items": {
            "type": "object",
            "properties": {
                "op": {"type": "string", "enum": ["replace_section", "append_section", "replace_all"]},
                "heading": {"type": "string", "description": "區段標題，不含 `## `；replace_all 不用"},
                "body_md": {"type": "string", "description": "區段內文（不含標題行，不能再有 `## ` 標題）；replace_all 時是整份 Markdown"},
            },
            "required": ["op", "body_md"],
            "additionalProperties": false,
        },
    })
}

fn kind_name(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Spec => "Spec",
        ArtifactKind::Brief => "Brief",
        ArtifactKind::Ledger => "Ledger",
        ArtifactKind::Report => "Report",
        ArtifactKind::ProjectNotes => "ProjectNotes",
    }
}

fn goal_state_name(state: &GoalState) -> &'static str {
    match state {
        GoalState::Active => "active",
        GoalState::Done { .. } => "done",
        GoalState::Cancelled { .. } => "cancelled",
    }
}

fn artifact_ref(a: &Option<ArtifactRow>) -> Value {
    match a {
        Some(a) => json!({"artifact_id": a.id.to_string(), "rev": a.rev}),
        None => Value::Null,
    }
}

fn task_json(t: &TaskRow) -> Value {
    json!({
        "id": t.id.to_string(),
        "title": t.title,
        "summary": t.summary,
        "state": t.state.name(),
        "parent_id": t.parent.map(|p| p.to_string()),
    })
}

fn conflict_json(current_rev: u32, changes: String, human: bool) -> String {
    json!({
        "conflict": {
            "current_rev": current_rev,
            "changed_by_human": human,
            "changes": changes,
            "hint": "base_rev 不是目前的版本：先吸收這些修改（必要時 read_artifact），再用 current_rev 重送。",
        }
    })
    .to_string()
}

/// 在 project 的對話流貼一張卡片。
pub async fn post_card(
    txn: &mut LogTxn<'_>,
    project: ProjectId,
    card: Card,
    run_id: Option<Uuid>,
) -> Result<(), Error> {
    let item = StreamItemView {
        id: ItemId::new(),
        at: Timestamp::now(),
        kind: StreamItemKind::Card(card),
    };
    Store::insert_item(txn.conn(), project, &item, ItemStatus::Done, run_id).await?;
    txn.push(|seq| ServerEvent::Item {
        seq,
        scope: StreamScope::Project(project),
        item,
    })
    .await
}

#[async_trait::async_trait]
impl Tool for PlatformTool {
    fn spec(&self) -> ToolSpec {
        let (name, description, parameters) = match self.kind {
            Kind::GetStatus => (
                "get_status",
                match self.ctx.scope {
                    Scope::Project => {
                        "查詢這個 project 的即時狀態：名稱、現在時間（UTC）、是否有使用者訊息在排隊、\
                        進行中的 Goal 與它們的任務樹、Brief／Ledger 的版本，以及 artifact 清單。\
                        關於 Goal 與任務的問題一律以這裡為準。"
                    }
                    Scope::Goal(_) => {
                        "查詢你負責的 Goal 的即時狀態：Goal 狀態、任務樹、Brief／Ledger 的版本與現在時間（UTC）。"
                    }
                },
                object(json!({}), &[]),
            ),
            Kind::ListGoals => (
                "list_goals",
                "列出這個 project 的 Goal（預設只有進行中的）與各自的任務數。",
                object(
                    json!({"include_closed": {"type": "boolean", "description": "也列出已完成與已取消的 Goal"}}),
                    &[],
                ),
            ),
            Kind::ReadArtifact => (
                "read_artifact",
                "讀取一份 artifact 的全文與目前版本（rev）。如果使用者在你上次讀取之後改過，會附上那些修改的 diff。\
                寫入或修改前一定要先讀。",
                object(json!({"artifact_id": {"type": "string"}}), &["artifact_id"]),
            ),
            Kind::Granted(ToolName::CreateGoal) => (
                "create_goal",
                "開一個 Goal，把討論收斂的工作交給 Planner。brief_md 是交接用的 Brief：目標、約束、\
                已決定事項與未決問題，用 `## ` 分段。Planner 不讀對話，只讀 Brief。",
                object(
                    json!({
                        "title": {"type": "string", "description": "簡短的 Goal 標題"},
                        "brief_md": {"type": "string", "description": "Brief 的 Markdown 全文"},
                    }),
                    &["title", "brief_md"],
                ),
            ),
            Kind::Granted(ToolName::SteerGoal) => (
                "steer_goal",
                "使用者改變方向時修改 Goal 的 Brief 並叫醒 Planner 重新規劃（Planner 正在跑會被中斷）。\
                edits 改 Brief 的區段，要帶 base_rev（先 read_artifact）；只想提醒 Planner 時可以只給 reason。",
                object(
                    json!({
                        "goal_id": {"type": "string"},
                        "reason": {"type": "string", "description": "為什麼改方向，會轉告 Planner"},
                        "base_rev": {"type": "integer", "description": "Brief 目前的版本；有 edits 時必填"},
                        "edits": edits_schema(),
                    }),
                    &["goal_id", "reason"],
                ),
            ),
            Kind::Granted(ToolName::CancelGoal) => (
                "cancel_goal",
                "取消一個進行中的 Goal：還沒結束的任務一起取消，Planner 停止。",
                object(
                    json!({"goal_id": {"type": "string"}, "reason": {"type": "string"}}),
                    &["goal_id", "reason"],
                ),
            ),
            Kind::Granted(ToolName::WriteArtifact) => {
                let kinds: Vec<&str> = self
                    .ctx
                    .artifact_kinds
                    .iter()
                    .map(|k| kind_name(*k))
                    .collect();
                (
                    "write_artifact",
                    "新建或修改 artifact。修改：給 artifact_id 與 base_rev（read_artifact 拿到的 rev）；\
                    期間有人改過會回 conflict，吸收後用 current_rev 重送。新建：不給 artifact_id，給 kind 與 title，\
                    edits 用 replace_all 或 append_section。",
                    object(
                        json!({
                            "artifact_id": {"type": "string", "description": "要修改的 artifact；新建時不給"},
                            "base_rev": {"type": "integer"},
                            "kind": {"type": "string", "enum": kinds, "description": "新建時的種類"},
                            "title": {"type": "string", "description": "新建時的標題"},
                            "goal_id": {"type": "string", "description": "新建時掛在哪個 Goal；不給就掛在 project"},
                            "edits": edits_schema(),
                        }),
                        &["edits"],
                    ),
                )
            }
            Kind::Granted(ToolName::CreateTask) => (
                "create_task",
                "在你負責的 Goal 建立一個任務。summary 寫清楚要做什麼與完成條件。",
                object(
                    json!({
                        "title": {"type": "string"},
                        "summary": {"type": "string"},
                        "parent_id": {"type": "string", "description": "上層任務（拆子任務時）"},
                    }),
                    &["title", "summary"],
                ),
            ),
            Kind::Granted(ToolName::CancelTask) => (
                "cancel_task",
                "取消你負責的 Goal 裡一個還沒結束的任務。",
                object(
                    json!({"task_id": {"type": "string"}, "reason": {"type": "string"}}),
                    &["task_id", "reason"],
                ),
            ),
            Kind::Granted(ToolName::PostMilestone) => (
                "post_milestone",
                "在使用者的對話流貼一張里程碑卡片，回報值得讓使用者知道的進展（一句話）。",
                object(json!({"text": {"type": "string"}}), &["text"]),
            ),
            Kind::Granted(ToolName::CompleteGoal) => (
                "complete_goal",
                "Goal 的目標都已達成時呼叫：Goal 標成完成，還沒結束的任務一起取消，對話流貼一張完成卡片。\
                呼叫後這一輪就結束。",
                object(
                    json!({"summary": {"type": "string", "description": "完成了什麼"}}),
                    &["summary"],
                ),
            ),
        };
        ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    /// 在另一個 task 裡執行：run 被取消時 loop 會丟掉這個 future，但工具的寫入（`LogTxn` 的
    /// commit 與事件序號）不能停在一半，否則記憶體裡的序號會落後資料庫。
    async fn call(&self, arguments: &str) -> Result<String, ToolError> {
        let tool = PlatformTool {
            kind: self.kind,
            ctx: self.ctx.clone(),
        };
        let arguments = arguments.to_string();
        tokio::spawn(async move { tool.dispatch(&arguments).await })
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "工具的 task 異常結束");
                Err(ToolError("伺服器內部錯誤，請稍後再試".into()))
            })
    }
}

impl PlatformTool {
    async fn dispatch(&self, arguments: &str) -> Result<String, ToolError> {
        match self.kind {
            Kind::GetStatus => self.get_status().await,
            Kind::ListGoals => self.list_goals(parse(arguments)?).await,
            Kind::ReadArtifact => self.read_artifact(parse(arguments)?).await,
            Kind::Granted(ToolName::CreateGoal) => self.create_goal(parse(arguments)?).await,
            Kind::Granted(ToolName::SteerGoal) => self.steer_goal(parse(arguments)?).await,
            Kind::Granted(ToolName::CancelGoal) => self.cancel_goal(parse(arguments)?).await,
            Kind::Granted(ToolName::WriteArtifact) => self.write_artifact(parse(arguments)?).await,
            Kind::Granted(ToolName::CreateTask) => self.create_task(parse(arguments)?).await,
            Kind::Granted(ToolName::CancelTask) => self.cancel_task(parse(arguments)?).await,
            Kind::Granted(ToolName::PostMilestone) => self.post_milestone(parse(arguments)?).await,
            Kind::Granted(ToolName::CompleteGoal) => self.complete_goal(parse(arguments)?).await,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListGoalsArgs {
    #[serde(default)]
    include_closed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArtifactArgs {
    artifact_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateGoalArgs {
    title: String,
    brief_md: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SteerGoalArgs {
    goal_id: String,
    reason: String,
    #[serde(default)]
    base_rev: Option<u32>,
    #[serde(default)]
    edits: Vec<Edit>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelGoalArgs {
    goal_id: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArtifactArgs {
    #[serde(default)]
    artifact_id: Option<String>,
    #[serde(default)]
    base_rev: Option<u32>,
    #[serde(default)]
    kind: Option<ArtifactKind>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    goal_id: Option<String>,
    edits: Vec<Edit>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTaskArgs {
    title: String,
    summary: String,
    #[serde(default)]
    parent_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelTaskArgs {
    task_id: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MilestoneArgs {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteGoalArgs {
    summary: String,
}

impl PlatformTool {
    fn goal_scope(&self) -> Result<GoalId, ToolError> {
        match self.ctx.scope {
            Scope::Goal(g) => Ok(g),
            Scope::Project => Err(bad("這個工具只能在 Goal 的範圍內使用")),
        }
    }

    /// 這個 project 的 Goal；Planner 只能指定自己的 Goal。
    async fn own_goal(
        &self,
        conn: &mut sqlx::SqliteConnection,
        id: GoalId,
    ) -> Result<GoalRow, ToolError> {
        if let Scope::Goal(own) = self.ctx.scope
            && own != id
        {
            return Err(bad("你只能操作自己負責的 Goal"));
        }
        match tasks::goal(conn, id).await.map_err(internal)? {
            Some(g) if g.project == self.ctx.project => Ok(g),
            _ => Err(bad(format!("這個 project 沒有 Goal {id}"))),
        }
    }

    async fn active_goal(
        &self,
        conn: &mut sqlx::SqliteConnection,
        id: GoalId,
    ) -> Result<GoalRow, ToolError> {
        let g = self.own_goal(conn, id).await?;
        if !g.state.is_active() {
            return Err(bad(format!("Goal「{}」已經{}", g.title, g.state.label())));
        }
        Ok(g)
    }

    async fn get_status(&self) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let mut tx = ctx.store.read().await.map_err(internal)?;
        let now = format_utc(Timestamp::now());
        match ctx.scope {
            Scope::Goal(goal) => {
                let g = self.own_goal(&mut tx, goal).await?;
                let tasks = tasks::tasks(&mut tx, goal).await.map_err(internal)?;
                let brief = artifacts::for_goal(&mut tx, goal, ArtifactKind::Brief)
                    .await
                    .map_err(internal)?;
                let ledger = artifacts::for_goal(&mut tx, goal, ArtifactKind::Ledger)
                    .await
                    .map_err(internal)?;
                Ok(json!({
                    "now": now,
                    "goal": {
                        "id": g.id.to_string(),
                        "title": g.title,
                        "state": goal_state_name(&g.state),
                        "brief": artifact_ref(&brief),
                        "ledger": artifact_ref(&ledger),
                    },
                    "tasks": tasks.iter().map(task_json).collect::<Vec<_>>(),
                })
                .to_string())
            }
            Scope::Project => {
                let project = ctx
                    .store
                    .project(ctx.project)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| bad("找不到這個 project"))?;
                let messages = ctx
                    .store
                    .count_messages(ctx.project)
                    .await
                    .map_err(internal)?;
                let goals = tasks::goals(&mut tx, ctx.project).await.map_err(internal)?;
                let mut active = Vec::new();
                let mut closed = 0;
                for g in &goals {
                    if !g.state.is_active() {
                        closed += 1;
                        continue;
                    }
                    let tasks = tasks::tasks(&mut tx, g.id).await.map_err(internal)?;
                    let brief = artifacts::for_goal(&mut tx, g.id, ArtifactKind::Brief)
                        .await
                        .map_err(internal)?;
                    let ledger = artifacts::for_goal(&mut tx, g.id, ArtifactKind::Ledger)
                        .await
                        .map_err(internal)?;
                    active.push(json!({
                        "id": g.id.to_string(),
                        "title": g.title,
                        "planner_note": g.planner_note,
                        "brief": artifact_ref(&brief),
                        "ledger": artifact_ref(&ledger),
                        "tasks": tasks.iter().map(task_json).collect::<Vec<_>>(),
                    }));
                }
                let arts = artifacts::list(&mut tx, ctx.project)
                    .await
                    .map_err(internal)?;
                Ok(json!({
                    "project": project.name,
                    "agent": project.agent_name,
                    "created_at": format_utc(project.created_at),
                    "messages": messages,
                    "now": now,
                    "queued": ctx.queued.as_ref().is_some_and(|q| q()),
                    "active_goals": active,
                    "closed_goals": closed,
                    "artifacts": arts.iter().map(|a| json!({
                        "id": a.id.to_string(),
                        "kind": kind_name(a.kind),
                        "title": a.title,
                        "rev": a.rev,
                        "goal_id": a.goal.map(|g| g.to_string()),
                    })).collect::<Vec<_>>(),
                })
                .to_string())
            }
        }
    }

    async fn list_goals(&self, args: ListGoalsArgs) -> Result<String, ToolError> {
        let mut tx = self.ctx.store.read().await.map_err(internal)?;
        let goals = tasks::goals(&mut tx, self.ctx.project)
            .await
            .map_err(internal)?;
        let mut out = Vec::new();
        for g in goals {
            if !args.include_closed && !g.state.is_active() {
                continue;
            }
            // Planner 只看得到自己的 Goal。
            if let Scope::Goal(own) = self.ctx.scope
                && own != g.id
            {
                continue;
            }
            let tasks = tasks::tasks(&mut tx, g.id).await.map_err(internal)?;
            let outcome = match &g.state {
                GoalState::Active => None,
                GoalState::Done { summary } => Some(summary.clone()),
                GoalState::Cancelled { reason } => Some(reason.clone()),
            };
            out.push(json!({
                "id": g.id.to_string(),
                "title": g.title,
                "state": goal_state_name(&g.state),
                "outcome": outcome,
                "created_at": format_utc(g.created_at),
                "tasks": {
                    "total": tasks.len(),
                    "open": tasks.iter().filter(|t| t.state.is_open()).count(),
                },
            }));
        }
        Ok(json!({ "goals": out }).to_string())
    }

    async fn read_artifact(&self, args: ReadArtifactArgs) -> Result<String, ToolError> {
        let id: ArtifactId = parse_id("artifact_id", &args.artifact_id)?;
        let mut tx = self.ctx.store.begin().await.map_err(internal)?;
        let a = match artifacts::get(&mut tx, id).await.map_err(internal)? {
            Some(a) if a.project == self.ctx.project => a,
            _ => return Err(bad(format!("這個 project 沒有 artifact {id}"))),
        };
        // Planner 讀得到自己 Goal 的與掛在 project 上的 artifact，讀不到別的 Goal 的。
        if let Scope::Goal(own) = self.ctx.scope
            && a.goal.is_some_and(|g| g != own)
        {
            return Err(bad("你只能讀自己負責的 Goal 與 project 層級的 artifact"));
        }
        let human = artifacts::read_for_agent(&mut tx, &a, &self.ctx.reader)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(|e| internal(e.into()))?;
        let mut out = format!(
            "artifact {} · {} ·「{}」· rev {}",
            a.id,
            kind_name(a.kind),
            a.title,
            a.rev
        );
        if let Some(g) = a.goal {
            out.push_str(&format!("（掛在 Goal {g}）"));
        }
        out.push_str("\n\n");
        out.push_str(&a.body);
        if let Some(diff) = human {
            out.push_str(
                "\n---\n使用者在你上次讀取之後改了這些（請以使用者的修改為準）：\n```diff\n",
            );
            out.push_str(&diff);
            out.push_str("```\n");
        }
        Ok(out)
    }

    async fn create_goal(&self, args: CreateGoalArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let title =
            tasks::check_line("title ", &args.title, tasks::MAX_TITLE_CHARS).map_err(bad)?;
        if args.brief_md.trim().is_empty() {
            return Err(bad("brief_md 不能是空的"));
        }
        if ctx.goals_created.load(Ordering::SeqCst) >= MAX_GOALS_PER_RUN {
            return Err(bad(format!("一次回覆最多開 {MAX_GOALS_PER_RUN} 個 Goal")));
        }
        let body = if args.brief_md.trim_start().starts_with("# ") {
            args.brief_md.clone()
        } else {
            format!("# {title}\n\n{}", args.brief_md.trim())
        };
        let body =
            artifacts::apply_edits("", &[Edit::ReplaceAll { body_md: body }], true).map_err(bad)?;
        let goal = GoalRow {
            id: GoalId::new(),
            project: ctx.project,
            title: title.clone(),
            state: GoalState::Active,
            planner_note: "等待 Planner…".into(),
            created_at: Timestamp::now(),
        };
        let brief = ArtifactRow {
            id: ArtifactId::new(),
            project: ctx.project,
            goal: Some(goal.id),
            kind: ArtifactKind::Brief,
            title: title.clone(),
            rev: 1,
            body,
            human_edits: Vec::new(),
        };
        let result: Result<Result<(), String>, Error> = async {
            let mut txn = ctx.hub.begin().await?;
            let active = tasks::goals(txn.conn(), ctx.project)
                .await?
                .iter()
                .filter(|g| g.state.is_active())
                .count();
            if active >= MAX_ACTIVE_GOALS {
                return Ok(Err(format!(
                    "這個 project 已經有 {active} 個進行中的 Goal，先完成或取消一些"
                )));
            }
            tasks::insert_goal(txn.conn(), &goal, &ctx.actor).await?;
            artifacts::insert(txn.conn(), &brief, &ctx.actor).await?;
            artifacts::mark_read(txn.conn(), brief.id, &ctx.reader, brief.rev).await?;
            planner::enqueue(
                txn.conn(),
                ctx.project,
                goal.id,
                &PlannerTrigger::GoalCreated {
                    by: ctx.actor.label(),
                },
            )
            .await?;
            artifacts::push_changed(&mut txn, &brief).await?;
            txn.commit_then(|| ctx.planner.kick()).await?;
            Ok(Ok(()))
        }
        .await;
        result.map_err(internal)?.map_err(bad)?;
        ctx.goals_created.fetch_add(1, Ordering::SeqCst);
        ctx.cards
            .lock()
            .unwrap()
            .push(Card::GoalCreated { goal: goal.id });
        Ok(json!({
            "goal_id": goal.id.to_string(),
            "brief": {"artifact_id": brief.id.to_string(), "rev": brief.rev},
            "note": "Goal 已建立，Planner 會在背景拆任務；使用者會在對話裡看到 Goal 卡片。",
        })
        .to_string())
    }

    async fn steer_goal(&self, args: SteerGoalArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal: GoalId = parse_id("goal_id", &args.goal_id)?;
        let reason =
            tasks::check_text("reason ", &args.reason, MAX_REASON_CHARS, true).map_err(bad)?;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let g = self.active_goal(txn.conn(), goal).await?;
        let mut rev = None;
        if !args.edits.is_empty() {
            let base_rev = args
                .base_rev
                .ok_or_else(|| bad("有 edits 時要帶 base_rev（先 read_artifact 讀 Brief）"))?;
            let brief = artifacts::for_goal(txn.conn(), g.id, ArtifactKind::Brief)
                .await
                .map_err(internal)?
                .ok_or_else(|| bad("這個 Goal 沒有 Brief"))?;
            let outcome = artifacts::agent_write(
                txn.conn(),
                &brief,
                base_rev,
                &args.edits,
                &ctx.actor,
                &ctx.reader,
            )
            .await
            .map_err(internal)?
            .map_err(bad)?;
            match outcome {
                WriteOutcome::Written(new) => {
                    artifacts::push_changed(&mut txn, &new)
                        .await
                        .map_err(internal)?;
                    rev = Some(new.rev);
                }
                WriteOutcome::Conflict {
                    current_rev,
                    changes,
                    human,
                } => return Ok(conflict_json(current_rev, changes, human)),
            }
        }
        planner::enqueue(
            txn.conn(),
            ctx.project,
            g.id,
            &PlannerTrigger::Steer {
                by: ctx.actor.label(),
                reason,
            },
        )
        .await
        .map_err(internal)?;
        txn.commit_then(|| ctx.planner.steer(g.id))
            .await
            .map_err(internal)?;
        Ok(json!({
            "steered": g.id.to_string(),
            "brief_rev": rev,
            "note": "已叫醒 Planner 重新規劃。",
        })
        .to_string())
    }

    async fn cancel_goal(&self, args: CancelGoalArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal: GoalId = parse_id("goal_id", &args.goal_id)?;
        let reason =
            tasks::check_text("reason ", &args.reason, MAX_REASON_CHARS, true).map_err(bad)?;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let g = self.active_goal(txn.conn(), goal).await?;
        let cancelled = close_goal(
            &mut txn,
            &g,
            GoalState::Cancelled { reason },
            &ctx.actor,
            "Goal 已取消",
        )
        .await
        .map_err(internal)?;
        txn.commit_then(|| ctx.planner.close_goal(g.id))
            .await
            .map_err(internal)?;
        Ok(json!({"cancelled": g.id.to_string(), "tasks_cancelled": cancelled}).to_string())
    }

    async fn write_artifact(&self, args: WriteArtifactArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let mut created = false;
        let written = match &args.artifact_id {
            Some(id) => {
                let id: ArtifactId = parse_id("artifact_id", id)?;
                let a = match artifacts::get(txn.conn(), id).await.map_err(internal)? {
                    Some(a) if a.project == ctx.project => a,
                    _ => return Err(bad(format!("這個 project 沒有 artifact {id}"))),
                };
                if !ctx.artifact_kinds.contains(&a.kind) {
                    return Err(bad(format!("你不能修改 {}", kind_name(a.kind))));
                }
                if let Scope::Goal(own) = ctx.scope
                    && a.goal != Some(own)
                {
                    return Err(bad("你只能修改自己負責的 Goal 的 artifact"));
                }
                let base_rev = args
                    .base_rev
                    .ok_or_else(|| bad("修改時要帶 base_rev（先 read_artifact）"))?;
                let outcome = artifacts::agent_write(
                    txn.conn(),
                    &a,
                    base_rev,
                    &args.edits,
                    &ctx.actor,
                    &ctx.reader,
                )
                .await
                .map_err(internal)?
                .map_err(bad)?;
                match outcome {
                    WriteOutcome::Written(new) => new,
                    WriteOutcome::Conflict {
                        current_rev,
                        changes,
                        human,
                    } => return Ok(conflict_json(current_rev, changes, human)),
                }
            }
            None => {
                let kind = args.kind.ok_or_else(|| bad("新建時要給 kind"))?;
                if !ctx.artifact_kinds.contains(&kind) {
                    return Err(bad(format!("你不能建立 {}", kind_name(kind))));
                }
                let goal = match (ctx.scope, &args.goal_id) {
                    (Scope::Goal(own), _) => Some(own),
                    (Scope::Project, Some(g)) => {
                        let g: GoalId = parse_id("goal_id", g)?;
                        Some(self.own_goal(txn.conn(), g).await?.id)
                    }
                    (Scope::Project, None) => None,
                };
                if kind == ArtifactKind::Ledger {
                    let Some(goal) = goal else {
                        return Err(bad("Ledger 一定要掛在 Goal 上"));
                    };
                    if let Some(existing) = artifacts::for_goal(txn.conn(), goal, kind)
                        .await
                        .map_err(internal)?
                    {
                        return Err(bad(format!(
                            "這個 Goal 已經有 Ledger（{}，rev {}），請用 artifact_id 修改",
                            existing.id, existing.rev
                        )));
                    }
                }
                let title = match &args.title {
                    Some(t) => {
                        tasks::check_line("title ", t, artifacts::MAX_TITLE_CHARS).map_err(bad)?
                    }
                    None if kind == ArtifactKind::Ledger => "Ledger".into(),
                    None => return Err(bad("新建時要給 title")),
                };
                let body = artifacts::apply_edits("", &args.edits, true).map_err(bad)?;
                let row = ArtifactRow {
                    id: ArtifactId::new(),
                    project: ctx.project,
                    goal,
                    kind,
                    title,
                    rev: 1,
                    body,
                    human_edits: Vec::new(),
                };
                artifacts::insert(txn.conn(), &row, &ctx.actor)
                    .await
                    .map_err(internal)?;
                artifacts::mark_read(txn.conn(), row.id, &ctx.reader, row.rev)
                    .await
                    .map_err(internal)?;
                created = true;
                row
            }
        };
        artifacts::push_changed(&mut txn, &written)
            .await
            .map_err(internal)?;
        txn.commit().await.map_err(internal)?;
        // commit 成功後才附卡片。
        if created && ctx.scope == Scope::Project {
            ctx.cards.lock().unwrap().push(Card::Artifact {
                id: written.id,
                kind: written.kind,
                title: written.title.clone(),
                note: format!("{} 已建立 · v1", kind_name(written.kind)),
            });
        }
        Ok(
            json!({"written": {"artifact_id": written.id.to_string(), "rev": written.rev}})
                .to_string(),
        )
    }

    async fn create_task(&self, args: CreateTaskArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal = self.goal_scope()?;
        let title =
            tasks::check_line("title ", &args.title, tasks::MAX_TITLE_CHARS).map_err(bad)?;
        let summary =
            tasks::check_text("summary ", &args.summary, MAX_SUMMARY_CHARS, false).map_err(bad)?;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let g = self.active_goal(txn.conn(), goal).await?;
        let existing = tasks::tasks(txn.conn(), g.id).await.map_err(internal)?;
        let open = existing.iter().filter(|t| t.state.is_open()).count();
        if open >= tasks::MAX_OPEN_TASKS_PER_GOAL {
            return Err(bad(format!(
                "一個 Goal 同時最多 {} 個未結束的任務",
                tasks::MAX_OPEN_TASKS_PER_GOAL
            )));
        }
        if existing.len() >= tasks::MAX_TASKS_PER_GOAL {
            return Err(bad(format!(
                "一個 Goal 最多 {} 個任務（含已取消的）",
                tasks::MAX_TASKS_PER_GOAL
            )));
        }
        let parent = match &args.parent_id {
            Some(p) => {
                let p: TaskId = parse_id("parent_id", p)?;
                if !existing.iter().any(|t| t.id == p) {
                    return Err(bad(format!("這個 Goal 沒有任務 {p}")));
                }
                Some(p)
            }
            None => None,
        };
        let task = TaskRow {
            id: TaskId::new(),
            goal: g.id,
            parent,
            title,
            summary,
            assignee: String::new(),
            state: TaskState::Planned,
        };
        let cause = Cause {
            by: ctx.actor.clone(),
            reason: String::new(),
        };
        let result: Result<(), Error> = async {
            tasks::insert_task(txn.conn(), &task, &cause).await?;
            tasks::push_goal(&mut txn, g.id).await?;
            txn.commit().await
        }
        .await;
        result.map_err(internal)?;
        Ok(json!({"task_id": task.id.to_string()}).to_string())
    }

    async fn cancel_task(&self, args: CancelTaskArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal = self.goal_scope()?;
        let id: TaskId = parse_id("task_id", &args.task_id)?;
        let reason =
            tasks::check_text("reason ", &args.reason, MAX_REASON_CHARS, true).map_err(bad)?;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let task = match tasks::task(txn.conn(), id).await.map_err(internal)? {
            Some(t) if t.goal == goal => t,
            _ => return Err(bad(format!("你負責的 Goal 沒有任務 {id}"))),
        };
        if !task.state.is_open() {
            return Err(bad(format!("任務「{}」已經結束", task.title)));
        }
        let result: Result<(), Error> = async {
            let cause = Cause {
                by: ctx.actor.clone(),
                reason,
            };
            let to = TaskState::Cancelled {
                by: ctx.actor.clone(),
            };
            tasks::set_task_state(txn.conn(), &task, &to, &cause).await?;
            tasks::push_goal(&mut txn, goal).await?;
            txn.commit().await
        }
        .await;
        result.map_err(internal)?;
        Ok(json!({"cancelled": id.to_string()}).to_string())
    }

    async fn post_milestone(&self, args: MilestoneArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal = self.goal_scope()?;
        let text =
            tasks::check_text("text ", &args.text, MAX_MILESTONE_CHARS, true).map_err(bad)?;
        if ctx.milestones.load(Ordering::SeqCst) >= MAX_MILESTONES_PER_RUN {
            return Err(bad(format!(
                "一輪最多貼 {MAX_MILESTONES_PER_RUN} 張里程碑；把進展合併成一句"
            )));
        }
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        self.active_goal(txn.conn(), goal).await?;
        let result: Result<(), Error> = async {
            post_card(
                &mut txn,
                ctx.project,
                Card::Milestone { goal, text },
                Some(ctx.run_id),
            )
            .await?;
            txn.commit().await
        }
        .await;
        result.map_err(internal)?;
        ctx.milestones.fetch_add(1, Ordering::SeqCst);
        Ok("已貼到使用者的對話流".into())
    }

    async fn complete_goal(&self, args: CompleteGoalArgs) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let goal = self.goal_scope()?;
        let summary =
            tasks::check_text("summary ", &args.summary, MAX_REASON_CHARS, true).map_err(bad)?;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let g = self.active_goal(txn.conn(), goal).await?;
        let result: Result<(), Error> = async {
            close_goal(
                &mut txn,
                &g,
                GoalState::Done {
                    summary: summary.clone(),
                },
                &ctx.actor,
                "Goal 已完成",
            )
            .await?;
            post_card(
                &mut txn,
                ctx.project,
                Card::Milestone {
                    goal,
                    text: format!("Goal 完成：{summary}"),
                },
                Some(ctx.run_id),
            )
            .await?;
            txn.commit().await
        }
        .await;
        result.map_err(internal)?;
        // Goal 結束了，這個 run 不該再做事：Gate 在下一輪開始前停下。
        ctx.finished.store(true, Ordering::SeqCst);
        Ok("Goal 已標成完成，這一輪到此結束".into())
    }
}

/// 結束一個 Goal：改狀態、取消還沒結束的任務、清掉排隊中的 Planner run、記 `GoalChanged`。
/// 正在跑的 Planner 由呼叫端在 commit 之後中斷（[`Planner::close_goal`]）。回傳取消了幾個任務。
pub async fn close_goal(
    txn: &mut LogTxn<'_>,
    g: &GoalRow,
    state: GoalState,
    by: &Principal,
    reason: &str,
) -> Result<usize, Error> {
    tasks::set_goal_state(txn.conn(), g.id, &state).await?;
    let cancelled = tasks::cancel_open_tasks(
        txn.conn(),
        g.id,
        &Cause {
            by: by.clone(),
            reason: reason.into(),
        },
    )
    .await?;
    planner::drop_queued(txn.conn(), g.id).await?;
    tasks::push_goal(txn, g.id).await?;
    Ok(cancelled)
}
