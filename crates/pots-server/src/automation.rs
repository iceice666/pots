//! 自我排程、listener 與批次暫停的資料（`proposal/events-and-automation.md#自我排程`、
//! `#listener事件自動開工`）。觸發的時機與動作在 [`crate::watcher`]。
//!
//! 自動化頁的資料（[`AutomationView`]）任一項有變就以 `ServerEvent::Automation` 整筆替換
//! （[`push`]），和 Goal 的做法一樣。

use std::collections::HashSet;

use chrono::TimeZone as _;
use croner::Cron;
use croner::parser::{CronParser, Seconds};
use pots_proto::{
    ApprovalId, AutomationView, FieldMatch, GoalId, ListenerActionSpec, ListenerId, ListenerSpec,
    ListenerState, ListenerView, PauseView, ProjectId, ScheduleCreator, ScheduleId, ScheduleView,
    ServerEvent, Timestamp, TriggerRecord, TriggerStatus, TrustLevel,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnection, SqliteRow};
use uuid::Uuid;

use crate::Error;
use crate::hub::LogTxn;
use crate::store::{parse_id, to_json};
use crate::tasks::{self, Principal};
use crate::watcher::event::{ExternalEvent, glob_match, kind_trust};

/// 週期排程最短間隔。
pub const MIN_INTERVAL_SECS: i64 = 10 * 60;
/// 每個 project 最多幾個還有效的排程；全域排程另計。
pub const MAX_SCHEDULES_PER_PROJECT: usize = 10;
pub const MAX_GLOBAL_SCHEDULES: usize = 10;
/// 週期排程預設幾天後過期。
pub const DEFAULT_EXPIRY_SECS: i64 = 30 * 86_400;
/// 每個 project 最多幾個 listener。
pub const MAX_LISTENERS_PER_PROJECT: usize = 20;
pub const DEFAULT_MAX_PER_HOUR: u32 = 5;
const MAX_PROMPT_CHARS: usize = 2000;
const MAX_TEMPLATE_CHARS: usize = 4000;
/// 自動化頁每列帶幾筆觸發紀錄。
const HISTORY: i64 = 10;

// ---------------------------------------------------------------------------
// 排程

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleSpec {
    Once {
        at: Timestamp,
    },
    /// cron 用本機時區，五個欄位（分 時 日 月 週）。
    Every {
        cron: String,
        #[serde(default)]
        until: Option<Timestamp>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleOwner {
    /// 在 project 對話裡醒來，帶 OM 記憶。
    Chat(ProjectId),
    /// 在 Goal 下醒來。
    Planner { project: ProjectId, goal: GoalId },
    /// 總管 agent（M6）。
    Global,
}

impl ScheduleOwner {
    pub fn project(self) -> Option<ProjectId> {
        match self {
            Self::Chat(p) | Self::Planner { project: p, .. } => Some(p),
            Self::Global => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScheduleRow {
    pub id: ScheduleId,
    pub owner: ScheduleOwner,
    pub spec: ScheduleSpec,
    pub prompt: String,
    pub next_at: Timestamp,
    pub expires_at: Option<Timestamp>,
    pub enabled: bool,
    pub finished: bool,
}

fn cron_parser() -> CronParser {
    CronParser::builder().seconds(Seconds::Disallowed).build()
}

fn parse_cron(expr: &str) -> Result<Cron, String> {
    cron_parser()
        .parse(expr.trim())
        .map_err(|e| format!("cron 寫法不對（要五個欄位：分 時 日 月 週）：{e}"))
}

/// `after` 之後（不含）的下一次觸發。
pub fn cron_next(cron: &Cron, after: Timestamp) -> Option<Timestamp> {
    let dt = chrono::Local.timestamp_opt(after.0, 0).single()?;
    cron.find_next_occurrence(&dt, false)
        .ok()
        .map(|t| Timestamp(t.timestamp()))
}

impl ScheduleSpec {
    /// 檢查並算出第一次觸發與過期時間。
    pub fn first(&self, now: Timestamp) -> Result<(Timestamp, Option<Timestamp>), String> {
        match self {
            Self::Once { at } => {
                if at.0 <= now.0 {
                    return Err("時間已經過了".into());
                }
                if at.0 > now.0 + 366 * 86_400 {
                    return Err("最多排到一年後".into());
                }
                Ok((*at, None))
            }
            Self::Every { cron, until } => {
                let parsed = parse_cron(cron)?;
                let first = cron_next(&parsed, now).ok_or("這個 cron 不會再觸發")?;
                // 檢查接下來幾次的間隔（例如 `*/5` 或 `0,5 * * * *` 都不行）。
                let mut prev = first;
                for _ in 0..24 {
                    let Some(next) = cron_next(&parsed, prev) else {
                        break;
                    };
                    if next.0 - prev.0 < MIN_INTERVAL_SECS {
                        return Err(format!("週期排程最短間隔 {} 分鐘", MIN_INTERVAL_SECS / 60));
                    }
                    prev = next;
                }
                let expiry = now.0 + DEFAULT_EXPIRY_SECS;
                let expires = match until {
                    Some(u) if u.0 <= first.0 => return Err("until 早於第一次觸發".into()),
                    Some(u) => u.0.min(expiry),
                    None => expiry,
                };
                Ok((first, Some(Timestamp(expires))))
            }
        }
    }

    /// 觸發後的下一次；`None` 表示結束。
    pub fn after(&self, now: Timestamp, expires: Option<Timestamp>) -> Option<Timestamp> {
        match self {
            Self::Once { .. } => None,
            Self::Every { cron, .. } => {
                let next = cron_next(&parse_cron(cron).ok()?, now)?;
                expires.is_none_or(|e| next.0 <= e.0).then_some(next)
            }
        }
    }

    pub fn cadence(&self) -> String {
        match self {
            Self::Once { at } => format!("單次 {}", local(*at)),
            Self::Every { cron, .. } => describe_cron(cron),
        }
    }
}

/// 常見的 cron 寫成白話；其他的直接顯示 cron。
fn describe_cron(cron: &str) -> String {
    let f: Vec<&str> = cron.split_whitespace().collect();
    if let [m, h, "*", "*", dow] = f.as_slice()
        && let (Ok(m), Ok(h)) = (m.parse::<u32>(), h.parse::<u32>())
    {
        let day = match *dow {
            "*" => "每天".to_string(),
            "1-5" | "MON-FRI" | "mon-fri" => "平日".to_string(),
            d => match d.parse::<usize>() {
                Ok(n) if n <= 7 => format!(
                    "每週{}",
                    ["日", "一", "二", "三", "四", "五", "六", "日"][n]
                ),
                _ => return format!("cron {cron}"),
            },
        };
        return format!("{day} {h:02}:{m:02}");
    }
    if let [m, "*", "*", "*", "*"] = f.as_slice()
        && let Some(step) = m.strip_prefix("*/")
    {
        return format!("每 {step} 分鐘");
    }
    format!("cron {cron}")
}

/// 本機時間，例如 `11/08 09:00`。
pub fn local(t: Timestamp) -> String {
    match chrono::Local.timestamp_opt(t.0, 0).single() {
        Some(dt) => dt.format("%m/%d %H:%M").to_string(),
        None => crate::runs::format_utc(t),
    }
}

fn owner_columns(owner: ScheduleOwner) -> (&'static str, Option<String>, Option<String>) {
    match owner {
        ScheduleOwner::Chat(p) => ("chat", Some(p.to_string()), None),
        ScheduleOwner::Planner { project, goal } => {
            ("planner", Some(project.to_string()), Some(goal.to_string()))
        }
        ScheduleOwner::Global => ("global", None, None),
    }
}

fn schedule_from_row(row: &SqliteRow) -> Result<ScheduleRow, Error> {
    let kind: String = row.try_get("owner_kind")?;
    let project: Option<String> = row.try_get("project_id")?;
    let goal: Option<String> = row.try_get("goal_id")?;
    let owner = match (kind.as_str(), project, goal) {
        ("chat", Some(p), _) => ScheduleOwner::Chat(parse_id(p)?),
        ("planner", Some(p), Some(g)) => ScheduleOwner::Planner {
            project: parse_id(p)?,
            goal: parse_id(g)?,
        },
        ("global", _, _) => ScheduleOwner::Global,
        _ => return Err(Error::Corrupt(format!("排程的 owner 不對：{kind}"))),
    };
    let spec: String = row.try_get("spec")?;
    let expires: Option<i64> = row.try_get("expires_at")?;
    Ok(ScheduleRow {
        id: parse_id(row.try_get("id")?)?,
        owner,
        spec: serde_json::from_str(&spec)?,
        prompt: row.try_get("prompt")?,
        next_at: Timestamp(row.try_get("next_at")?),
        expires_at: expires.map(Timestamp),
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        finished: row.try_get::<i64, _>("finished")? != 0,
    })
}

macro_rules! schedule_columns {
    () => {
        "id, owner_kind, project_id, goal_id, spec, prompt, next_at, expires_at, enabled, finished"
    };
}

pub fn check_prompt(prompt: &str) -> Result<String, String> {
    tasks::check_text("prompt ", prompt, MAX_PROMPT_CHARS, true)
}

/// 新增排程（檢查數量上限）。
pub async fn insert_schedule(
    conn: &mut SqliteConnection,
    owner: ScheduleOwner,
    spec: ScheduleSpec,
    prompt: &str,
    by: &Principal,
    now: Timestamp,
) -> Result<Result<ScheduleRow, String>, Error> {
    let prompt = match check_prompt(prompt) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    let (next_at, expires_at) = match spec.first(now) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let live = schedules(conn)
        .await?
        .into_iter()
        .filter(|s| !s.finished && s.owner.project() == owner.project())
        .count();
    let max = if owner == ScheduleOwner::Global {
        MAX_GLOBAL_SCHEDULES
    } else {
        MAX_SCHEDULES_PER_PROJECT
    };
    if live >= max {
        return Ok(Err(format!("最多 {max} 個排程，先刪掉不需要的")));
    }
    let row = ScheduleRow {
        id: ScheduleId::new(),
        owner,
        spec,
        prompt,
        next_at,
        expires_at,
        enabled: true,
        finished: false,
    };
    let (kind, project, goal) = owner_columns(owner);
    sqlx::query(
        "INSERT INTO schedules (id, owner_kind, project_id, goal_id, spec, prompt, created_by, next_at, \
         expires_at, enabled, finished, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 0, ?, ?)",
    )
    .bind(row.id.to_string())
    .bind(kind)
    .bind(project)
    .bind(goal)
    .bind(to_json(&row.spec)?)
    .bind(&row.prompt)
    .bind(to_json(by)?)
    .bind(row.next_at.0)
    .bind(row.expires_at.map(|t| t.0))
    .bind(now.0)
    .bind(now.0)
    .execute(conn)
    .await?;
    Ok(Ok(row))
}

pub async fn schedules(conn: &mut SqliteConnection) -> Result<Vec<ScheduleRow>, Error> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        schedule_columns!(),
        " FROM schedules ORDER BY created_at, id"
    ))
    .fetch_all(conn)
    .await?;
    rows.iter().map(schedule_from_row).collect()
}

pub async fn schedule(
    conn: &mut SqliteConnection,
    id: ScheduleId,
) -> Result<Option<ScheduleRow>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        schedule_columns!(),
        " FROM schedules WHERE id = ?"
    ))
    .bind(id.to_string())
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(schedule_from_row).transpose()
}

/// 時間到了、還有效的排程。
pub async fn due_schedules(
    conn: &mut SqliteConnection,
    now: Timestamp,
) -> Result<Vec<ScheduleRow>, Error> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        schedule_columns!(),
        " FROM schedules WHERE enabled = 1 AND finished = 0 AND next_at <= ? ORDER BY next_at, id"
    ))
    .bind(now.0)
    .fetch_all(conn)
    .await?;
    rows.iter().map(schedule_from_row).collect()
}

/// 觸發（或略過）後更新下一次；`next` 為 `None` 時標成結束。
pub async fn advance_schedule(
    conn: &mut SqliteConnection,
    id: ScheduleId,
    next: Option<Timestamp>,
    now: Timestamp,
) -> Result<(), Error> {
    match next {
        Some(n) => sqlx::query("UPDATE schedules SET next_at = ?, updated_at = ? WHERE id = ?")
            .bind(n.0)
            .bind(now.0)
            .bind(id.to_string()),
        None => sqlx::query("UPDATE schedules SET finished = 1, updated_at = ? WHERE id = ?")
            .bind(now.0)
            .bind(id.to_string()),
    }
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn set_schedule_enabled(
    conn: &mut SqliteConnection,
    id: ScheduleId,
    enabled: bool,
    now: Timestamp,
) -> Result<(), Error> {
    sqlx::query("UPDATE schedules SET enabled = ?, updated_at = ? WHERE id = ?")
        .bind(i64::from(enabled))
        .bind(now.0)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// 改下一次觸發的時間；單次排程同時改它的 `at`。結束的排程重新生效。
pub async fn reschedule(
    conn: &mut SqliteConnection,
    s: &ScheduleRow,
    next_at: Timestamp,
    now: Timestamp,
) -> Result<Result<(), String>, Error> {
    if next_at.0 <= now.0 {
        return Ok(Err("時間已經過了".into()));
    }
    let spec = match &s.spec {
        ScheduleSpec::Once { .. } => ScheduleSpec::Once { at: next_at },
        every @ ScheduleSpec::Every { .. } => {
            if s.expires_at.is_some_and(|e| next_at.0 > e.0) {
                return Ok(Err("晚於過期時間；先延長過期時間".into()));
            }
            every.clone()
        }
    };
    sqlx::query(
        "UPDATE schedules SET spec = ?, next_at = ?, finished = 0, updated_at = ? WHERE id = ?",
    )
    .bind(to_json(&spec)?)
    .bind(next_at.0)
    .bind(now.0)
    .bind(s.id.to_string())
    .execute(conn)
    .await?;
    Ok(Ok(()))
}

/// 延長（或縮短）週期排程的過期時間。
pub async fn set_expiry(
    conn: &mut SqliteConnection,
    s: &ScheduleRow,
    expires_at: Timestamp,
    now: Timestamp,
) -> Result<Result<(), String>, Error> {
    let ScheduleSpec::Every { cron, .. } = &s.spec else {
        return Ok(Err("單次排程沒有過期時間".into()));
    };
    if expires_at.0 <= now.0 {
        return Ok(Err("過期時間已經過了".into()));
    }
    if expires_at.0 > now.0 + 366 * 86_400 {
        return Ok(Err("最多延長到一年後".into()));
    }
    // 過期而結束的排程延長後重新生效，從下一次正常時間開始。
    let next = if s.finished {
        cron_next(&parse_cron(cron).map_err(Error::Corrupt)?, now)
            .filter(|n| n.0 <= expires_at.0)
            .unwrap_or(s.next_at)
    } else {
        s.next_at
    };
    let finished = s.finished && next == s.next_at;
    sqlx::query(
        "UPDATE schedules SET expires_at = ?, next_at = ?, finished = ?, updated_at = ? WHERE id = ?",
    )
    .bind(expires_at.0)
    .bind(next.0)
    .bind(i64::from(finished))
    .bind(now.0)
    .bind(s.id.to_string())
    .execute(conn)
    .await?;
    Ok(Ok(()))
}

pub async fn delete_schedule(conn: &mut SqliteConnection, id: ScheduleId) -> Result<bool, Error> {
    let done = sqlx::query("DELETE FROM schedules WHERE id = ?")
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Goal 結束了：它的排程不再觸發。回傳結束了幾個。
pub async fn finish_goal_schedules(
    conn: &mut SqliteConnection,
    goal: GoalId,
    now: Timestamp,
) -> Result<u64, Error> {
    let done = sqlx::query(
        "UPDATE schedules SET finished = 1, updated_at = ? WHERE goal_id = ? AND finished = 0",
    )
    .bind(now.0)
    .bind(goal.to_string())
    .execute(conn)
    .await?;
    Ok(done.rows_affected())
}

// ---------------------------------------------------------------------------
// Listener

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerDbState {
    Active,
    Disabled,
    /// agent 提案，等你核准（M5 之前的提案等你啟用）。
    Proposed,
    /// 超過每小時上限而暫停。
    Throttled,
}

impl ListenerDbState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
            Self::Proposed => "pending",
            Self::Throttled => "throttled",
        }
    }

    fn parse(s: &str) -> Result<Self, Error> {
        Ok(match s {
            "active" => Self::Active,
            "disabled" => Self::Disabled,
            "pending" => Self::Proposed,
            "throttled" => Self::Throttled,
            _ => return Err(Error::Corrupt(format!("listener 狀態不對：{s}"))),
        })
    }

    /// `approval` 是等待中的核准（agent 的提案）；M5 之前的提案沒有，以「啟用」代替核准。
    fn view(self, approval: Option<ApprovalId>) -> ListenerState {
        match (self, approval) {
            (Self::Active, _) => ListenerState::Active,
            (Self::Disabled, _) => ListenerState::Disabled,
            (Self::Proposed, Some(a)) => ListenerState::PendingApproval(a),
            (Self::Proposed, None) => ListenerState::Proposed,
            (Self::Throttled, _) => ListenerState::Throttled,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FilterJson {
    kinds: Vec<String>,
    resource: Option<String>,
    fields: Vec<FieldMatch>,
}

#[derive(Clone, Debug)]
pub struct ListenerRow {
    pub id: ListenerId,
    pub spec: ListenerSpec,
    pub state: ListenerDbState,
    /// agent 提案時等的核准。
    pub approval: Option<ApprovalId>,
    /// 最後一次修改或改狀態（重新啟用後每小時上限從這裡重新算）。
    pub updated_at: Timestamp,
}

/// 可能是 Public 的事件種類。
fn has_public_kinds(spec: &ListenerSpec) -> bool {
    spec.kinds
        .iter()
        .any(|k| kind_trust(k) == TrustLevel::Public)
}

/// 有沒有限定作者。Public 事件要自動開工只認這種白名單：repo、state 之類的欄位誰都能符合。
pub fn has_author_whitelist(spec: &ListenerSpec) -> bool {
    spec.fields.iter().any(|f| f.field == "author")
}

/// 檢查並整理 listener 的設定；不合法時回說明。
pub fn check_spec(mut spec: ListenerSpec) -> Result<ListenerSpec, String> {
    let mut seen = HashSet::new();
    spec.kinds.retain(|k| seen.insert(k.clone()));
    if spec.kinds.is_empty() {
        return Err("至少要選一種事件".into());
    }
    for k in &spec.kinds {
        if !pots_proto::EVENT_KINDS.iter().any(|(n, _)| n == k) {
            return Err(format!("不認得的事件種類：{k}"));
        }
    }
    if let Some(r) = &spec.resource {
        let r = r.trim();
        if r.is_empty() {
            spec.resource = None;
        } else if r.len() > 300 {
            return Err("resource 太長".into());
        } else {
            spec.resource = Some(r.to_string());
        }
    }
    let mut fields = Vec::new();
    for f in spec.fields {
        let field = f.field.trim().to_ascii_lowercase();
        if !matches!(
            field.as_str(),
            "label" | "author" | "branch" | "conclusion" | "state" | "repo"
        ) {
            return Err(format!("不認得的欄位：{}", f.field));
        }
        // 分支名稱保留大小寫（要拿去查 API）；比對時一律不分大小寫。
        let any_of: Vec<String> = f
            .any_of
            .iter()
            .map(|v| {
                let v = v.trim();
                if field == "branch" {
                    v.to_string()
                } else {
                    v.to_ascii_lowercase()
                }
            })
            .filter(|v| !v.is_empty())
            .collect();
        if any_of.is_empty() || any_of.len() > 50 {
            return Err(format!("欄位 {field} 要有 1 到 50 個值"));
        }
        fields.push(FieldMatch { field, any_of });
    }
    spec.fields = fields;
    if spec.max_per_hour == 0 || spec.max_per_hour > 60 {
        return Err("每小時上限要在 1 到 60 之間".into());
    }
    match &mut spec.action {
        ListenerActionSpec::CreateGoal { template } | ListenerActionSpec::QuickFix { template } => {
            *template = tasks::check_text("模板 ", template, MAX_TEMPLATE_CHARS, true)?;
            // Public 來源（郵件、公開 issue）要自動開工，必須限定作者。
            if has_public_kinds(&spec) && !has_author_whitelist(&spec) {
                return Err(
                    "可能來自任何人的事件（例如 issue 開啟、留言、review）要自動開工，必須加上作者\
                     （author）白名單；不然只能選「通知對話」"
                        .into(),
                );
            }
        }
        ListenerActionSpec::NotifyChat => {}
    }
    Ok(spec)
}

/// 事件是否符合 listener 的條件（不含 project 的 repo 範圍）。
///
/// CI 只比對輪詢確認過「是這個分支的 head」的事件（帶 `branch` 欄位）：webhook 回報的
/// check suite 可能來自 fork 的 PR，只用來喚醒在等那個 sha 的任務。
pub fn matches(spec: &ListenerSpec, ev: &ExternalEvent) -> bool {
    if !spec.kinds.iter().any(|k| k == ev.kind.name()) {
        return false;
    }
    if ev.kind.name() == "ci.finished" && !ev.fields.contains_key("branch") {
        return false;
    }
    if let Some(r) = &spec.resource
        && !glob_match(r, &ev.resource)
    {
        return false;
    }
    spec.fields.iter().all(|f| {
        ev.fields
            .get(&f.field)
            .is_some_and(|v| f.any_of.iter().any(|a| a.eq_ignore_ascii_case(v)))
    })
}

fn listener_from_row(row: &SqliteRow) -> Result<ListenerRow, Error> {
    let filter: String = row.try_get("filter")?;
    let filter: FilterJson = serde_json::from_str(&filter)?;
    let action: String = row.try_get("action")?;
    let budget: Option<i64> = row.try_get("budget_per_trigger")?;
    let state: String = row.try_get("state")?;
    Ok(ListenerRow {
        id: parse_id(row.try_get("id")?)?,
        spec: ListenerSpec {
            project: parse_id(row.try_get("project_id")?)?,
            kinds: filter.kinds,
            resource: filter.resource,
            fields: filter.fields,
            action: serde_json::from_str(&action)?,
            max_per_hour: row.try_get::<i64, _>("max_per_hour")? as u32,
            budget_per_trigger_cents: budget.map(|b| b as u32),
        },
        state: ListenerDbState::parse(&state)?,
        approval: row
            .try_get::<Option<String>, _>("approval_id")?
            .map(parse_id)
            .transpose()?,
        updated_at: Timestamp(row.try_get("updated_at")?),
    })
}

macro_rules! listener_columns {
    () => {
        "id, project_id, filter, action, budget_per_trigger, max_per_hour, state, approval_id, updated_at"
    };
}

pub async fn listeners(conn: &mut SqliteConnection) -> Result<Vec<ListenerRow>, Error> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        listener_columns!(),
        " FROM listeners ORDER BY created_at, id"
    ))
    .fetch_all(conn)
    .await?;
    rows.iter().map(listener_from_row).collect()
}

pub async fn listener(
    conn: &mut SqliteConnection,
    id: ListenerId,
) -> Result<Option<ListenerRow>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        listener_columns!(),
        " FROM listeners WHERE id = ?"
    ))
    .bind(id.to_string())
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(listener_from_row).transpose()
}

/// 新增或修改 listener。`spec` 要先經過 [`check_spec`]。
pub async fn save_listener(
    conn: &mut SqliteConnection,
    id: Option<ListenerId>,
    spec: &ListenerSpec,
    state: ListenerDbState,
    by: &Principal,
    now: Timestamp,
) -> Result<Result<ListenerId, String>, Error> {
    let filter = to_json(&FilterJson {
        kinds: spec.kinds.clone(),
        resource: spec.resource.clone(),
        fields: spec.fields.clone(),
    })?;
    let action = to_json(&spec.action)?;
    if let Some(id) = id {
        let done = sqlx::query(
            "UPDATE listeners SET project_id = ?, filter = ?, action = ?, budget_per_trigger = ?, \
             max_per_hour = ?, state = ?, updated_at = ? WHERE id = ?",
        )
        .bind(spec.project.to_string())
        .bind(filter)
        .bind(action)
        .bind(spec.budget_per_trigger_cents.map(i64::from))
        .bind(i64::from(spec.max_per_hour))
        .bind(state.as_str())
        .bind(now.0)
        .bind(id.to_string())
        .execute(conn)
        .await?;
        if done.rows_affected() == 0 {
            return Ok(Err("找不到這個 listener".into()));
        }
        return Ok(Ok(id));
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM listeners WHERE project_id = ?")
        .bind(spec.project.to_string())
        .fetch_one(&mut *conn)
        .await?;
    if count as usize >= MAX_LISTENERS_PER_PROJECT {
        return Ok(Err(format!(
            "每個 project 最多 {MAX_LISTENERS_PER_PROJECT} 個 listener"
        )));
    }
    let id = ListenerId::new();
    sqlx::query(
        "INSERT INTO listeners (id, project_id, filter, action, budget_per_trigger, max_per_hour, state, \
         created_by, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(spec.project.to_string())
    .bind(filter)
    .bind(action)
    .bind(spec.budget_per_trigger_cents.map(i64::from))
    .bind(i64::from(spec.max_per_hour))
    .bind(state.as_str())
    .bind(to_json(by)?)
    .bind(now.0)
    .bind(now.0)
    .execute(conn)
    .await?;
    Ok(Ok(id))
}

pub async fn set_listener_state(
    conn: &mut SqliteConnection,
    id: ListenerId,
    state: ListenerDbState,
    now: Timestamp,
) -> Result<(), Error> {
    sqlx::query("UPDATE listeners SET state = ?, updated_at = ? WHERE id = ?")
        .bind(state.as_str())
        .bind(now.0)
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// 記下（或清掉）agent 提案的 listener 在等哪一筆核准。
pub async fn set_listener_approval(
    conn: &mut SqliteConnection,
    id: ListenerId,
    approval: Option<ApprovalId>,
) -> Result<(), Error> {
    sqlx::query("UPDATE listeners SET approval_id = ? WHERE id = ?")
        .bind(approval.map(|a| a.to_string()))
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn delete_listener(conn: &mut SqliteConnection, id: ListenerId) -> Result<bool, Error> {
    let done = sqlx::query("DELETE FROM listeners WHERE id = ?")
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// 批次暫停

#[derive(Clone, Debug)]
pub struct PauseRow {
    /// `None` 是所有 project。
    pub project: Option<ProjectId>,
    pub until: Option<Timestamp>,
    pub by: Principal,
    pub at: Timestamp,
}

fn scope_key(project: Option<ProjectId>) -> String {
    project.map_or_else(|| "*".to_string(), |p| p.to_string())
}

pub async fn pause(
    conn: &mut SqliteConnection,
    project: Option<ProjectId>,
    until: Option<Timestamp>,
    by: &Principal,
    now: Timestamp,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO automation_pauses (scope, until, paused_by, at) VALUES (?, ?, ?, ?) \
         ON CONFLICT (scope) DO UPDATE SET until = excluded.until, paused_by = excluded.paused_by, at = excluded.at",
    )
    .bind(scope_key(project))
    .bind(until.map(|u| u.0))
    .bind(to_json(by)?)
    .bind(now.0)
    .execute(conn)
    .await?;
    Ok(())
}

/// 恢復；回傳有沒有暫停可以恢復。
pub async fn resume(
    conn: &mut SqliteConnection,
    project: Option<ProjectId>,
) -> Result<bool, Error> {
    let done = sqlx::query("DELETE FROM automation_pauses WHERE scope = ?")
        .bind(scope_key(project))
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

pub async fn pauses(conn: &mut SqliteConnection) -> Result<Vec<PauseRow>, Error> {
    let rows = sqlx::query("SELECT scope, until, paused_by, at FROM automation_pauses ORDER BY at")
        .fetch_all(conn)
        .await?;
    let mut out = Vec::new();
    for row in &rows {
        let scope: String = row.try_get("scope")?;
        let until: Option<i64> = row.try_get("until")?;
        let by: String = row.try_get("paused_by")?;
        out.push(PauseRow {
            project: if scope == "*" {
                None
            } else {
                Some(parse_id(scope)?)
            },
            until: until.map(Timestamp),
            by: serde_json::from_str(&by)?,
            at: Timestamp(row.try_get("at")?),
        });
    }
    Ok(out)
}

/// 刪掉已經到期的暫停；回傳刪了幾個。
pub async fn expire_pauses(conn: &mut SqliteConnection, now: Timestamp) -> Result<u64, Error> {
    let done = sqlx::query("DELETE FROM automation_pauses WHERE until IS NOT NULL AND until <= ?")
        .bind(now.0)
        .execute(conn)
        .await?;
    Ok(done.rows_affected())
}

/// 在 `t` 這個時間點是否被暫停（包括已經到期的暫停）：暫停期間該觸發的排程不補跑。
pub fn was_paused_at(pauses: &[PauseRow], project: Option<ProjectId>, t: Timestamp) -> bool {
    pauses.iter().any(|p| {
        (p.project.is_none() || p.project == project)
            && p.at.0 <= t.0
            && p.until.is_none_or(|u| u.0 > t.0)
    })
}

/// 這個 project（`None` 是全域排程）現在是否被暫停。
pub fn is_paused(pauses: &[PauseRow], project: Option<ProjectId>, now: Timestamp) -> bool {
    pauses.iter().any(|p| {
        (p.project.is_none() || p.project == project) && p.until.is_none_or(|u| u.0 > now.0)
    })
}

// ---------------------------------------------------------------------------
// 觸發紀錄

#[derive(Clone, Debug, Default)]
pub struct TriggerRow {
    pub schedule: Option<ScheduleId>,
    pub listener: Option<ListenerId>,
    pub project: Option<ProjectId>,
    pub goal: Option<GoalId>,
    pub event: Option<Uuid>,
    pub title: String,
    /// done | failed | notified
    pub status: &'static str,
    pub error: Option<String>,
}

pub async fn record_trigger(
    conn: &mut SqliteConnection,
    t: &TriggerRow,
    now: Timestamp,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO automation_triggers (id, schedule_id, listener_id, project_id, goal_id, event_id, \
         title, status, error, at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(t.schedule.map(|s| s.to_string()))
    .bind(t.listener.map(|l| l.to_string()))
    .bind(t.project.map(|p| p.to_string()))
    .bind(t.goal.map(|g| g.to_string()))
    .bind(t.event.map(|e| e.to_string()))
    .bind(tasks::truncate(&t.title, 200))
    .bind(t.status)
    .bind(&t.error)
    .bind(now.0)
    .execute(conn)
    .await?;
    Ok(())
}

/// listener 在 `since` 之後觸發了幾次（每小時上限用）。
pub async fn listener_triggers_since(
    conn: &mut SqliteConnection,
    id: ListenerId,
    since: Timestamp,
) -> Result<u32, Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM automation_triggers WHERE listener_id = ? AND at > ? AND status != 'skipped'",
    )
    .bind(id.to_string())
    .bind(since.0)
    .fetch_one(conn)
    .await?;
    Ok(n as u32)
}

/// 排程或 listener 最近的觸發紀錄。
enum HistoryOf {
    Schedule(ScheduleId),
    Listener(ListenerId),
}

async fn history(conn: &mut SqliteConnection, of: HistoryOf) -> Result<Vec<TriggerRecord>, Error> {
    let (sql, id) = match of {
        HistoryOf::Schedule(id) => (
            "SELECT title, status, error, at FROM automation_triggers WHERE schedule_id = ? \
             ORDER BY at DESC, id DESC LIMIT ?",
            id.to_string(),
        ),
        HistoryOf::Listener(id) => (
            "SELECT title, status, error, at FROM automation_triggers WHERE listener_id = ? \
             ORDER BY at DESC, id DESC LIMIT ?",
            id.to_string(),
        ),
    };
    let rows = sqlx::query(sql)
        .bind(id)
        .bind(HISTORY)
        .fetch_all(conn)
        .await?;
    let mut out = Vec::new();
    for row in &rows {
        let status: String = row.try_get("status")?;
        let error: Option<String> = row.try_get("error")?;
        let at = Timestamp(row.try_get("at")?);
        let mut meta = local(at);
        if let Some(e) = error {
            meta.push_str(" · ");
            meta.push_str(&e);
        }
        out.push(TriggerRecord {
            title: row.try_get("title")?,
            meta,
            status: match status.as_str() {
                "done" => TriggerStatus::Done,
                "notified" => TriggerStatus::Notified,
                _ => TriggerStatus::Failed,
            },
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 檢視

fn principal_creator(p: &Principal, owner: ScheduleOwner) -> ScheduleCreator {
    match (owner, p) {
        (ScheduleOwner::Global, _) => ScheduleCreator::Overseer,
        (ScheduleOwner::Planner { .. }, _) => ScheduleCreator::Planner,
        (_, Principal::Agent(name)) if name == "planner" => ScheduleCreator::Planner,
        _ => ScheduleCreator::Chat,
    }
}

async fn schedule_view(
    conn: &mut SqliteConnection,
    s: &ScheduleRow,
    pauses: &[PauseRow],
    now: Timestamp,
) -> Result<ScheduleView, Error> {
    let by: String = sqlx::query_scalar("SELECT created_by FROM schedules WHERE id = ?")
        .bind(s.id.to_string())
        .fetch_one(&mut *conn)
        .await?;
    let by: Principal = serde_json::from_str(&by)?;
    let note = match (&s.spec, s.finished, s.expires_at) {
        (ScheduleSpec::Once { .. }, true, _) => "單次 · 已觸發".to_string(),
        (ScheduleSpec::Once { .. }, false, _) => "單次".to_string(),
        (_, true, _) => "已過期".to_string(),
        (_, false, Some(e)) => format!("{} 過期", local(e)),
        (_, false, None) => String::new(),
    };
    let paused = is_paused(pauses, s.owner.project(), now);
    let next_after_pause = if paused && s.enabled && !s.finished {
        // 暫停到期後的第一次觸發；手動恢復的暫停不知道什麼時候恢復。
        pauses
            .iter()
            .filter(|p| p.project.is_none() || p.project == s.owner.project())
            .filter_map(|p| p.until)
            .max()
            .map(|u| match s.spec.after(u, s.expires_at) {
                Some(n) => local(n),
                None => "（暫停期間會略過）".into(),
            })
            .or(Some("恢復後的下一次".into()))
    } else {
        None
    };
    Ok(ScheduleView {
        id: s.id,
        project: s.owner.project(),
        prompt: s.prompt.clone(),
        note,
        creator: principal_creator(&by, s.owner),
        cadence: s.spec.cadence(),
        next: if s.finished {
            "—".into()
        } else if !s.enabled {
            "已停用".into()
        } else {
            local(s.next_at)
        },
        next_after_pause,
        enabled: s.enabled,
        history: history(conn, HistoryOf::Schedule(s.id)).await?,
        next_at: (!s.finished).then_some(s.next_at),
        expires_at: s.expires_at,
        goal: match s.owner {
            ScheduleOwner::Planner { goal, .. } => Some(goal),
            _ => None,
        },
        finished: s.finished,
    })
}

fn kinds_label(kinds: &[String]) -> String {
    kinds
        .iter()
        .map(|k| {
            pots_proto::EVENT_KINDS
                .iter()
                .find(|(n, _)| n == k)
                .map_or(k.as_str(), |(_, label)| label)
        })
        .collect::<Vec<_>>()
        .join("、")
}

/// 例如「issue 加上 label（label = agent）→ 開 Goal」。
pub fn listener_title(spec: &ListenerSpec) -> String {
    let mut condition = kinds_label(&spec.kinds);
    if !spec.fields.is_empty() {
        condition.push_str(&format!("（{}）", fields_label(&spec.fields)));
    }
    format!("{condition} → {}", spec.action.label())
}

fn fields_label(fields: &[FieldMatch]) -> String {
    fields
        .iter()
        .map(|f| format!("{} = {}", f.field, f.any_of.join(" 或 ")))
        .collect::<Vec<_>>()
        .join("，")
}

async fn listener_view(
    conn: &mut SqliteConnection,
    l: &ListenerRow,
    now: Timestamp,
) -> Result<ListenerView, Error> {
    let spec = &l.spec;
    let repos = project_repos(conn, spec.project).await?;
    let trust = spec
        .kinds
        .iter()
        .map(|k| kind_trust(k))
        .max_by_key(|t| match t {
            TrustLevel::Owned => 0,
            TrustLevel::Members => 1,
            TrustLevel::Public => 2,
        })
        .unwrap_or(TrustLevel::Owned);
    let mut condition = kinds_label(&spec.kinds);
    if !spec.fields.is_empty() {
        condition.push_str(&format!("（{}）", fields_label(&spec.fields)));
    }
    let week: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM automation_triggers WHERE listener_id = ? AND at > ?",
    )
    .bind(l.id.to_string())
    .bind(now.0 - 7 * 86_400)
    .fetch_one(&mut *conn)
    .await?;
    let (action_detail, template) = match &spec.action {
        ListenerActionSpec::CreateGoal { template } => (
            "開一個 Goal，Brief 用下面的模板加上事件內容".to_string(),
            Some(template.clone()),
        ),
        ListenerActionSpec::QuickFix { template } => (
            "快速修正（直接派 worker）".to_string(),
            Some(template.clone()),
        ),
        ListenerActionSpec::NotifyChat => ("在 project 對話裡告訴對話 agent".to_string(), None),
    };
    Ok(ListenerView {
        id: l.id,
        project: Some(spec.project),
        condition,
        action: spec.action.label().to_string(),
        source: "GitHub".into(),
        source_detail: if repos.is_empty() {
            "這個 project 還沒有設定 repo".into()
        } else {
            repos.join("、")
        },
        trust,
        resource: spec
            .resource
            .clone()
            .unwrap_or_else(|| "project 監看的所有 repo".into()),
        event: spec.kinds.join(", "),
        matcher: if spec.fields.is_empty() {
            "（無）".into()
        } else {
            fields_label(&spec.fields)
        },
        action_detail,
        template,
        budget_per_trigger_cents: spec.budget_per_trigger_cents,
        max_per_hour: Some(spec.max_per_hour),
        triggers_this_week: week as u32,
        state: l.state.view(l.approval),
        history: history(conn, HistoryOf::Listener(l.id)).await?,
        spec: Some(spec.clone()),
    })
}

/// 自動化頁的全部資料。
pub async fn view(conn: &mut SqliteConnection, now: Timestamp) -> Result<AutomationView, Error> {
    let pause_rows = pauses(conn).await?;
    let mut schedules_out = Vec::new();
    for s in schedules(conn).await? {
        schedules_out.push(schedule_view(conn, &s, &pause_rows, now).await?);
    }
    let mut listeners_out = Vec::new();
    for l in listeners(conn).await? {
        listeners_out.push(listener_view(conn, &l, now).await?);
    }
    Ok(AutomationView {
        schedules: schedules_out,
        listeners: listeners_out,
        pauses: pause_rows
            .iter()
            .map(|p| PauseView {
                project: p.project,
                until: p.until,
                by: p.by.label(),
                at: p.at,
            })
            .collect(),
        sources: crate::watcher::source_views(conn).await?,
    })
}

/// 記一筆 `Automation`（整筆替換）。
pub async fn push(txn: &mut LogTxn<'_>, now: Timestamp) -> Result<(), Error> {
    let automation = view(txn.conn(), now).await?;
    txn.push(|seq| ServerEvent::Automation { seq, automation })
        .await
}

// ---------------------------------------------------------------------------
// project 監看的 repo

pub const MAX_REPOS_PER_PROJECT: usize = 20;

pub async fn project_repos(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<Vec<String>, Error> {
    Ok(sqlx::query_scalar(
        "SELECT repo FROM project_repos WHERE project_id = ? ORDER BY created_at, repo",
    )
    .bind(project.to_string())
    .fetch_all(conn)
    .await?)
}

/// 所有 project 監看的 repo（去重）。
pub async fn all_repos(conn: &mut SqliteConnection) -> Result<Vec<String>, Error> {
    Ok(
        sqlx::query_scalar("SELECT DISTINCT repo FROM project_repos ORDER BY repo")
            .fetch_all(conn)
            .await?,
    )
}

/// 監看這個 repo 的 project。
pub async fn projects_watching(
    conn: &mut SqliteConnection,
    repo: &str,
) -> Result<Vec<ProjectId>, Error> {
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT project_id FROM project_repos WHERE repo = ?")
            .bind(repo)
            .fetch_all(&mut *conn)
            .await?;
    ids.into_iter().map(parse_id).collect()
}

pub async fn set_project_repos(
    conn: &mut SqliteConnection,
    project: ProjectId,
    repos: &[String],
    now: Timestamp,
) -> Result<(), Error> {
    sqlx::query("DELETE FROM project_repos WHERE project_id = ?")
        .bind(project.to_string())
        .execute(&mut *conn)
        .await?;
    for (i, repo) in repos.iter().enumerate() {
        sqlx::query("INSERT INTO project_repos (project_id, repo, created_at) VALUES (?, ?, ?)")
            .bind(project.to_string())
            .bind(repo)
            .bind(now.0 + i as i64)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher::event::{EventKind, Untrusted};

    #[test]
    fn schedule_specs() {
        let now = Timestamp(1_800_000_000);
        assert!(ScheduleSpec::Once { at: now }.first(now).is_err());
        let (first, expires) = ScheduleSpec::Once {
            at: Timestamp(now.0 + 60),
        }
        .first(now)
        .unwrap();
        assert_eq!((first.0, expires), (now.0 + 60, None));

        let every = |cron: &str| ScheduleSpec::Every {
            cron: cron.into(),
            until: None,
        };
        assert!(every("*/5 * * * *").first(now).is_err());
        assert!(every("0,5 * * * *").first(now).is_err());
        assert!(every("* * * * * *").first(now).is_err(), "不接受秒");
        assert!(every("nope").first(now).is_err());
        let (first, expires) = every("*/10 * * * *").first(now).unwrap();
        assert!(first.0 > now.0 && first.0 <= now.0 + 600);
        assert_eq!(expires, Some(Timestamp(now.0 + DEFAULT_EXPIRY_SECS)));
        // 觸發之後算下一次；超過過期時間就結束。
        let spec = every("*/10 * * * *");
        let next = spec.after(first, expires).unwrap();
        assert_eq!(next.0 - first.0, 600);
        assert_eq!(spec.after(first, Some(first)), None);
        assert_eq!(ScheduleSpec::Once { at: now }.after(now, None), None);

        assert_eq!(describe_cron("0 8 * * *"), "每天 08:00");
        assert_eq!(describe_cron("30 9 * * 1-5"), "平日 09:30");
        assert_eq!(describe_cron("0 9 * * 1"), "每週一 09:00");
        assert_eq!(describe_cron("*/10 * * * *"), "每 10 分鐘");
        assert_eq!(describe_cron("0 0 1 * *"), "cron 0 0 1 * *");
    }

    fn spec(kinds: &[&str], fields: Vec<FieldMatch>, action: ListenerActionSpec) -> ListenerSpec {
        ListenerSpec {
            project: ProjectId::from_u64(1),
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
            resource: None,
            fields,
            action,
            max_per_hour: 5,
            budget_per_trigger_cents: None,
        }
    }

    #[test]
    fn listener_specs_follow_trust_rules() {
        let goal = ListenerActionSpec::CreateGoal {
            template: "修好它".into(),
        };
        // label 只有成員能加：可以直接開工。
        check_spec(spec(&["issue.labeled"], vec![], goal.clone())).unwrap();
        // 任何人都能開 issue：要作者白名單；repo 這種誰都符合的欄位不算。
        assert!(check_spec(spec(&["issue.opened"], vec![], goal.clone())).is_err());
        let repo = vec![FieldMatch {
            field: "repo".into(),
            any_of: vec!["github.com/me/app".into()],
        }];
        assert!(check_spec(spec(&["issue.opened"], repo, goal.clone())).is_err());
        let whitelist = vec![FieldMatch {
            field: "Author".into(),
            any_of: vec![" Me ".into()],
        }];
        let ok = check_spec(spec(&["issue.opened"], whitelist, goal.clone())).unwrap();
        assert_eq!(ok.fields[0].field, "author");
        assert_eq!(ok.fields[0].any_of, ["me"]);
        // 只通知不用白名單。
        check_spec(spec(
            &["issue.opened"],
            vec![],
            ListenerActionSpec::NotifyChat,
        ))
        .unwrap();
        assert!(check_spec(spec(&[], vec![], ListenerActionSpec::NotifyChat)).is_err());
        assert!(check_spec(spec(&["nope"], vec![], ListenerActionSpec::NotifyChat)).is_err());
        let mut s = spec(&["ci.finished"], vec![], ListenerActionSpec::NotifyChat);
        s.max_per_hour = 0;
        assert!(check_spec(s).is_err());
    }

    #[test]
    fn listener_matching() {
        let s = check_spec(spec(
            &["ci.finished"],
            vec![
                FieldMatch {
                    field: "conclusion".into(),
                    any_of: vec!["failure".into()],
                },
                FieldMatch {
                    field: "branch".into(),
                    any_of: vec!["main".into()],
                },
            ],
            ListenerActionSpec::NotifyChat,
        ))
        .unwrap();
        let mut ev = ExternalEvent {
            source: "github:personal".into(),
            kind: EventKind::CiFinished {
                sha: "a".into(),
                conclusion: "failure".into(),
            },
            resource: "commit:github.com/me/app@a".into(),
            dedupe_key: "k".into(),
            occurred_at: Timestamp(0),
            trust: TrustLevel::Owned,
            summary: String::new(),
            body: Untrusted::default(),
            links: vec![],
            backfill: false,
            fields: [
                ("conclusion".to_string(), "failure".to_string()),
                ("branch".to_string(), "main".to_string()),
            ]
            .into(),
        };
        assert!(matches(&s, &ev));
        ev.fields.insert("branch".into(), "dev".into());
        assert!(!matches(&s, &ev));
        // 沒有確認過分支的 CI（webhook、可能來自 fork）不給 listener。
        ev.fields.remove("branch");
        let any_branch = check_spec(spec(
            &["ci.finished"],
            vec![],
            ListenerActionSpec::NotifyChat,
        ))
        .unwrap();
        assert!(!matches(&any_branch, &ev));
    }

    #[test]
    fn pause_scopes() {
        let now = Timestamp(100);
        let p = ProjectId::from_u64(1);
        let q = ProjectId::from_u64(2);
        let row = |project, until: Option<i64>| PauseRow {
            project,
            until: until.map(Timestamp),
            by: Principal::Human,
            at: Timestamp(0),
        };
        assert!(!is_paused(&[], Some(p), now));
        assert!(is_paused(&[row(Some(p), None)], Some(p), now));
        assert!(!is_paused(&[row(Some(p), None)], Some(q), now));
        assert!(!is_paused(&[row(Some(p), None)], None, now));
        assert!(is_paused(&[row(None, Some(200))], Some(q), now));
        assert!(is_paused(&[row(None, None)], None, now));
        assert!(!is_paused(&[row(None, Some(100))], Some(q), now));
    }
}
