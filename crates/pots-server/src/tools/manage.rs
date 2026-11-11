//! 對話 agent 管理這個 project 的自動化與 artifact。
//!
//! - 自動化：`list_automations` 列出排程與 listener；`update_schedule`、`update_listener` 停用、
//!   啟用或改時間，直接執行（和你在自動化頁做的一樣，對話裡附一張卡片）；`delete_schedule`、
//!   `delete_listener` 只申請，你核准後平台才刪（[`crate::approvals::propose_removal`]）。
//!   等核准的 listener 提案不能用 `update_listener` 啟用（不能略過核准）。
//! - artifact：`artifact_history` 看版本紀錄或某一版的全文；`restore_artifact` 還原成某一版
//!   （新增一個版本，作者是 agent）；`archive_artifact` 申請封存，你核准後從列表移除。
//!   還原與封存只能動 profile 的 `artifact_kinds`（chat 是 Spec、Report）。
//!
//! 只有 project 的對話 agent 能用；範圍限這個 project。

use pots_llm::agent::ToolError;
use pots_proto::{ArtifactId, ListenerId, ScheduleId, Timestamp};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    PlatformTool, Scope, bad, format_utc, internal, kind_name, notice, object, parse_id,
    parse_time_arg,
};
use crate::Error;
use crate::approvals::{self, Action};
use crate::artifacts::{self, ArtifactRow};
use crate::automation::{self, ListenerDbState, ScheduleOwner, ScheduleRow};
use crate::profiles::ToolName;
use crate::tasks;

/// 一次 `artifact_history` 最多回幾個字（版本的 diff 依序放，放不下就停）。
const MAX_HISTORY_CHARS: usize = 20_000;
/// 每一版的 diff 最多幾個字。
const MAX_DIFF_CHARS: usize = 3_000;

/// 這個模組提供的工具的規格；不是的回 `None`。
pub(super) fn spec(t: ToolName) -> Option<(&'static str, &'static str, Value)> {
    Some(match t {
        ToolName::ListAutomations => (
            "list_automations",
            "列出這個 project 的排程與 listener：狀態（啟用、停用、等核准、超過上限）、下一次觸發、\
             過期時間與條件，以及自動化是否被暫停。問到有哪些自動化時以這裡為準。",
            object(
                json!({"include_finished": {"type": "boolean", "description": "也列出已經結束的排程"}}),
                &[],
            ),
        ),
        ToolName::UpdateSchedule => (
            "update_schedule",
            "修改這個 project 的一個排程：停用／啟用、改下一次觸發的時間（單次排程就是改時間）、\
             改週期排程的過期時間。直接生效，使用者會在對話裡看到一張卡片。要刪除用 delete_schedule。",
            object(
                json!({
                    "schedule_id": {"type": "string"},
                    "enabled": {"type": "boolean"},
                    "next_at": {"type": "string", "description": "下一次觸發的時間（RFC 3339，帶時區）"},
                    "expires_at": {"type": "string", "description": "週期排程的過期時間（RFC 3339，最多一年後）"},
                }),
                &["schedule_id"],
            ),
        ),
        ToolName::DeleteSchedule => (
            "delete_schedule",
            "申請刪除這個 project 的一個排程：使用者會看到一張核准卡片，核准後才刪除。\
             只是暫時不要觸發的話，用 update_schedule 停用就好。",
            object(
                json!({
                    "schedule_id": {"type": "string"},
                    "reason": {"type": "string", "description": "給使用者看的理由"},
                }),
                &["schedule_id", "reason"],
            ),
        ),
        ToolName::UpdateListener => (
            "update_listener",
            "停用或啟用這個 project 的一個 listener。直接生效，使用者會在對話裡看到一張卡片。\
             等使用者核准的提案不能用這裡啟用；停用它就是撤回提案。要刪除用 delete_listener。",
            object(
                json!({
                    "listener_id": {"type": "string"},
                    "enabled": {"type": "boolean"},
                }),
                &["listener_id", "enabled"],
            ),
        ),
        ToolName::DeleteListener => (
            "delete_listener",
            "申請刪除這個 project 的一個 listener：使用者會看到一張核准卡片，核准後才刪除。\
             只是暫時不要觸發的話，用 update_listener 停用就好。",
            object(
                json!({
                    "listener_id": {"type": "string"},
                    "reason": {"type": "string", "description": "給使用者看的理由"},
                }),
                &["listener_id", "reason"],
            ),
        ),
        ToolName::ArtifactHistory => (
            "artifact_history",
            "看一份 artifact 的版本紀錄：不給 rev 時列出最近的版本（由新到舊，作者、時間與和前一版的 diff）；\
             給 rev 時回傳那一版的全文。",
            object(
                json!({
                    "artifact_id": {"type": "string"},
                    "rev": {"type": "integer", "description": "要看全文的版本"},
                }),
                &["artifact_id"],
            ),
        ),
        ToolName::RestoreArtifact => (
            "restore_artifact",
            "把 artifact 還原成某一版的內容（新增一個版本，舊的版本都還在）。base_rev 要是目前的版本\
             （read_artifact 拿到的 rev），否則不寫入。",
            object(
                json!({
                    "artifact_id": {"type": "string"},
                    "rev": {"type": "integer", "description": "要還原成哪一版"},
                    "base_rev": {"type": "integer", "description": "目前的版本"},
                }),
                &["artifact_id", "rev", "base_rev"],
            ),
        ),
        ToolName::ArchiveArtifact => (
            "archive_artifact",
            "申請封存一份不再需要的 artifact：使用者會看到一張核准卡片，核准後從 artifact 列表移除\
             （資料與版本紀錄仍留著）。",
            object(
                json!({
                    "artifact_id": {"type": "string"},
                    "reason": {"type": "string", "description": "給使用者看的理由"},
                }),
                &["artifact_id", "reason"],
            ),
        ),
        _ => return None,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListAutomationsArgs {
    #[serde(default)]
    include_finished: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateScheduleArgs {
    schedule_id: String,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    next_at: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteScheduleArgs {
    schedule_id: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateListenerArgs {
    listener_id: String,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteListenerArgs {
    listener_id: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArtifactHistoryArgs {
    artifact_id: String,
    #[serde(default)]
    rev: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RestoreArtifactArgs {
    artifact_id: String,
    rev: u32,
    base_rev: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArchiveArtifactArgs {
    artifact_id: String,
    reason: String,
}

fn schedule_summary(s: &ScheduleRow) -> String {
    format!("{}：{}", s.spec.cadence(), tasks::one_line(&s.prompt, 100))
}

fn schedule_json(s: &ScheduleRow) -> Value {
    let (owner, goal) = match s.owner {
        ScheduleOwner::Chat(_) => ("chat", None),
        ScheduleOwner::Planner { goal, .. } => ("planner", Some(goal.to_string())),
        ScheduleOwner::Global => ("global", None),
    };
    json!({
        "id": s.id.to_string(),
        "owner": owner,
        "goal_id": goal,
        "cadence": s.spec.cadence(),
        "prompt": s.prompt,
        "enabled": s.enabled,
        "finished": s.finished,
        "next_at": format_utc(s.next_at),
        "expires_at": s.expires_at.map(format_utc),
    })
}

fn listener_state(l: &automation::ListenerRow) -> &'static str {
    match (l.state, l.approval) {
        (ListenerDbState::Active, _) => "active",
        (ListenerDbState::Disabled, _) => "disabled",
        (ListenerDbState::Proposed, Some(_)) => "pending_approval",
        (ListenerDbState::Proposed, None) => "proposed",
        (ListenerDbState::Throttled, _) => "throttled",
    }
}

impl PlatformTool {
    fn project_chat_only(&self, tool: &str) -> Result<(), ToolError> {
        if self.ctx.scope != Scope::Project {
            return Err(bad(format!("只有 project 的對話 agent 能用 {tool}")));
        }
        Ok(())
    }

    /// 這個 project 的排程（找不到或不是這個 project 的都當作沒有）。
    async fn own_schedule(
        &self,
        conn: &mut sqlx::SqliteConnection,
        id: &str,
    ) -> Result<ScheduleRow, ToolError> {
        let id: ScheduleId = parse_id("schedule_id", id)?;
        match automation::schedule(conn, id).await.map_err(internal)? {
            Some(s) if s.owner.project() == Some(self.ctx.project) => Ok(s),
            _ => Err(bad(format!("這個 project 沒有排程 {id}"))),
        }
    }

    async fn own_listener(
        &self,
        conn: &mut sqlx::SqliteConnection,
        id: &str,
    ) -> Result<automation::ListenerRow, ToolError> {
        let id: ListenerId = parse_id("listener_id", id)?;
        match automation::listener(conn, id).await.map_err(internal)? {
            Some(l) if l.spec.project == self.ctx.project => Ok(l),
            _ => Err(bad(format!("這個 project 沒有 listener {id}"))),
        }
    }

    /// 這個 project 的 artifact；`writable` 時還要是 profile 允許動的種類。
    async fn own_artifact(
        &self,
        conn: &mut sqlx::SqliteConnection,
        id: &str,
        writable: bool,
    ) -> Result<ArtifactRow, ToolError> {
        let id: ArtifactId = parse_id("artifact_id", id)?;
        let a = match artifacts::get(conn, id).await.map_err(internal)? {
            Some(a) if a.project == self.ctx.project => a,
            _ => return Err(bad(format!("這個 project 沒有 artifact {id}"))),
        };
        if writable && !self.ctx.artifact_kinds.contains(&a.kind) {
            return Err(bad(format!("你不能修改 {}", kind_name(a.kind))));
        }
        Ok(a)
    }

    pub(super) async fn list_automations(
        &self,
        args: ListAutomationsArgs,
    ) -> Result<String, ToolError> {
        self.project_chat_only("list_automations")?;
        let ctx = &self.ctx;
        let now = ctx.clock.now();
        let mut tx = ctx.store.read().await.map_err(internal)?;
        let schedules: Vec<Value> = automation::schedules(&mut tx)
            .await
            .map_err(internal)?
            .iter()
            .filter(|s| s.owner.project() == Some(ctx.project))
            .filter(|s| args.include_finished || !s.finished)
            .map(schedule_json)
            .collect();
        let listeners: Vec<Value> = automation::listeners(&mut tx)
            .await
            .map_err(internal)?
            .iter()
            .filter(|l| l.spec.project == ctx.project)
            .map(|l| {
                json!({
                    "id": l.id.to_string(),
                    "title": automation::listener_title(&l.spec),
                    "state": listener_state(l),
                    "approval_id": l.approval.map(|a| a.to_string()),
                    "kinds": l.spec.kinds,
                    "resource": l.spec.resource,
                    "fields": l.spec.fields,
                    "action": l.spec.action,
                    "max_per_hour": l.spec.max_per_hour,
                })
            })
            .collect();
        let pauses = automation::pauses(&mut tx).await.map_err(internal)?;
        let paused: Vec<Value> = pauses
            .iter()
            .filter(|p| p.project.is_none() || p.project == Some(ctx.project))
            .filter(|p| p.until.is_none_or(|u| u.0 > now.0))
            .map(|p| {
                json!({
                    "scope": if p.project.is_none() { "all_projects" } else { "this_project" },
                    "until": p.until.map(format_utc),
                    "by": p.by.label(),
                })
            })
            .collect();
        Ok(json!({
            "now": format_utc(now),
            "paused": paused,
            "schedules": schedules,
            "listeners": listeners,
        })
        .to_string())
    }

    pub(super) async fn update_schedule(
        &self,
        args: UpdateScheduleArgs,
    ) -> Result<String, ToolError> {
        self.project_chat_only("update_schedule")?;
        let ctx = &self.ctx;
        if args.enabled.is_none() && args.next_at.is_none() && args.expires_at.is_none() {
            return Err(bad("要給 enabled、next_at 或 expires_at 至少一個"));
        }
        let next_at = args
            .next_at
            .as_deref()
            .map(|t| parse_time_arg("next_at", t))
            .transpose()?;
        let expires_at = args
            .expires_at
            .as_deref()
            .map(|t| parse_time_arg("expires_at", t))
            .transpose()?;
        let now = ctx.clock.now();
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let s = self.own_schedule(txn.conn(), &args.schedule_id).await?;
        let result: Result<Result<ScheduleRow, String>, Error> = async {
            // 先改過期時間，改下一次時才不會被舊的過期時間擋住。
            if let Some(e) = expires_at {
                let s = automation::schedule(txn.conn(), s.id)
                    .await?
                    .unwrap_or(s.clone());
                if let Err(e) = automation::set_expiry(txn.conn(), &s, e, now).await? {
                    return Ok(Err(e));
                }
            }
            if let Some(n) = next_at {
                let s = automation::schedule(txn.conn(), s.id)
                    .await?
                    .unwrap_or(s.clone());
                if let Err(e) = automation::reschedule(txn.conn(), &s, n, now).await? {
                    return Ok(Err(e));
                }
            }
            if let Some(enabled) = args.enabled {
                automation::set_schedule_enabled(txn.conn(), s.id, enabled, now).await?;
            }
            let Some(updated) = automation::schedule(txn.conn(), s.id).await? else {
                return Ok(Err("找不到這個排程".into()));
            };
            automation::push(&mut txn, now).await?;
            Ok(Ok(updated))
        }
        .await;
        let updated = result.map_err(internal)?.map_err(bad)?;
        txn.commit().await.map_err(internal)?;
        let state = if updated.finished {
            "已結束".to_string()
        } else if updated.enabled {
            format!("下一次 {}", automation::local(updated.next_at))
        } else {
            "已停用".to_string()
        };
        ctx.cards.lock().unwrap().push(notice(
            "已修改排程",
            format!("{}（{state}）", schedule_summary(&updated)),
        ));
        Ok(json!({"schedule": schedule_json(&updated)}).to_string())
    }

    pub(super) async fn delete_schedule(
        &self,
        args: DeleteScheduleArgs,
    ) -> Result<String, ToolError> {
        self.project_chat_only("delete_schedule")?;
        let ctx = &self.ctx;
        let now = ctx.clock.now();
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let s = self.own_schedule(txn.conn(), &args.schedule_id).await?;
        let summary = schedule_summary(&s);
        let action = Action::DeleteSchedule {
            schedule: s.id,
            summary: summary.clone(),
        };
        self.propose(
            txn,
            action,
            format!("刪除排程：{summary}"),
            &args.reason,
            now,
        )
        .await
    }

    pub(super) async fn update_listener(
        &self,
        args: UpdateListenerArgs,
    ) -> Result<String, ToolError> {
        self.project_chat_only("update_listener")?;
        let ctx = &self.ctx;
        let now = ctx.clock.now();
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let l = self.own_listener(txn.conn(), &args.listener_id).await?;
        // 和你在自動化頁的「啟用」一樣：等核准的提案要在核准卡片決定。
        if args.enabled && l.state == ListenerDbState::Proposed && l.approval.is_some() {
            return Err(bad(
                "這個 listener 在等使用者核准，不能直接啟用；請使用者在核准卡片或 Inbox 決定",
            ));
        }
        let state = if args.enabled {
            ListenerDbState::Active
        } else {
            ListenerDbState::Disabled
        };
        let result: Result<(), Error> = async {
            if l.state != state {
                automation::set_listener_state(txn.conn(), l.id, state, now).await?;
            }
            // 停用等核准的提案就是撤回：那筆核准跟著撤回。
            approvals::sync_listeners(&mut txn).await?;
            automation::push(&mut txn, now).await?;
            Ok(())
        }
        .await;
        result.map_err(internal)?;
        txn.commit().await.map_err(internal)?;
        let title = automation::listener_title(&l.spec);
        ctx.cards.lock().unwrap().push(notice(
            if args.enabled {
                "已啟用 listener"
            } else {
                "已停用 listener"
            },
            title,
        ));
        Ok(json!({
            "listener_id": l.id.to_string(),
            "state": if args.enabled { "active" } else { "disabled" },
        })
        .to_string())
    }

    pub(super) async fn delete_listener(
        &self,
        args: DeleteListenerArgs,
    ) -> Result<String, ToolError> {
        self.project_chat_only("delete_listener")?;
        let ctx = &self.ctx;
        let now = ctx.clock.now();
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let l = self.own_listener(txn.conn(), &args.listener_id).await?;
        let summary = automation::listener_title(&l.spec);
        let action = Action::DeleteListener {
            listener: l.id,
            summary: summary.clone(),
        };
        self.propose(
            txn,
            action,
            format!("刪除 listener：{summary}"),
            &args.reason,
            now,
        )
        .await
    }

    pub(super) async fn artifact_history(
        &self,
        args: ArtifactHistoryArgs,
    ) -> Result<String, ToolError> {
        self.project_chat_only("artifact_history")?;
        let mut tx = self.ctx.store.read().await.map_err(internal)?;
        let a = self.own_artifact(&mut tx, &args.artifact_id, false).await?;
        let head = format!(
            "artifact {} · {} ·「{}」· 目前 rev {}",
            a.id,
            kind_name(a.kind),
            a.title,
            a.rev
        );
        if let Some(rev) = args.rev {
            let body = artifacts::revision_body(&mut tx, a.id, rev)
                .await
                .map_err(internal)?
                .ok_or_else(|| bad(format!("找不到 v{rev}（目前是 v{}）", a.rev)))?;
            return Ok(format!("{head}\n\n--- v{rev} 的全文 ---\n{body}"));
        }
        let revisions = artifacts::history(&mut tx, a.id).await.map_err(internal)?;
        let mut out = format!("{head}\n版本由新到舊；要看某一版的全文時帶 rev。\n");
        for (i, r) in revisions.iter().enumerate() {
            let diff: String = if r.diff.chars().count() > MAX_DIFF_CHARS {
                let cut: String = r.diff.chars().take(MAX_DIFF_CHARS).collect();
                format!("{cut}\n…（diff 太長，已截斷）")
            } else {
                r.diff.clone()
            };
            let entry = format!(
                "\n## v{} · {} · {}\n{}\n",
                r.rev,
                r.author,
                format_utc(r.at),
                if diff.is_empty() {
                    "（最早的一版）".to_string()
                } else {
                    format!("```diff\n{diff}```")
                }
            );
            if out.chars().count() + entry.chars().count() > MAX_HISTORY_CHARS {
                out.push_str(&format!(
                    "\n…還有 {} 個更舊的版本沒有列出。\n",
                    revisions.len() - i
                ));
                break;
            }
            out.push_str(&entry);
        }
        Ok(out)
    }

    pub(super) async fn restore_artifact(
        &self,
        args: RestoreArtifactArgs,
    ) -> Result<String, ToolError> {
        self.project_chat_only("restore_artifact")?;
        let ctx = &self.ctx;
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let a = self
            .own_artifact(txn.conn(), &args.artifact_id, true)
            .await?;
        let restored = artifacts::agent_restore(
            txn.conn(),
            &a,
            args.base_rev,
            args.rev,
            &ctx.actor,
            &ctx.reader,
        )
        .await
        .map_err(internal)?
        .map_err(bad)?;
        let Some(new) = restored else {
            return Ok(json!({
                "artifact_id": a.id.to_string(),
                "rev": a.rev,
                "note": format!("v{} 的內容和目前一樣，沒有新增版本。", args.rev),
            })
            .to_string());
        };
        artifacts::push_changed(&mut txn, &new)
            .await
            .map_err(internal)?;
        txn.commit().await.map_err(internal)?;
        ctx.cards.lock().unwrap().push(pots_proto::Card::Artifact {
            id: new.id,
            kind: new.kind,
            title: new.title.clone(),
            note: format!("已還原成 v{} 的內容 · v{}", args.rev, new.rev),
        });
        Ok(json!({"artifact_id": new.id.to_string(), "rev": new.rev}).to_string())
    }

    pub(super) async fn archive_artifact(
        &self,
        args: ArchiveArtifactArgs,
    ) -> Result<String, ToolError> {
        self.project_chat_only("archive_artifact")?;
        let ctx = &self.ctx;
        let now = ctx.clock.now();
        let mut txn = ctx.hub.begin().await.map_err(internal)?;
        let a = self
            .own_artifact(txn.conn(), &args.artifact_id, true)
            .await?;
        let action = Action::ArchiveArtifact {
            artifact: a.id,
            artifact_kind: a.kind,
            title: a.title.clone(),
        };
        self.propose(
            txn,
            action,
            format!("封存 {}：{}", kind_name(a.kind), a.title),
            &args.reason,
            now,
        )
        .await
    }

    /// 申請一筆刪除／封存的核准，commit 後把卡片附在這次回覆下面。
    async fn propose(
        &self,
        mut txn: crate::hub::LogTxn<'_>,
        action: Action,
        title: String,
        reason: &str,
        now: Timestamp,
    ) -> Result<String, ToolError> {
        let ctx = &self.ctx;
        let (id, attach) = approvals::propose_removal(&mut txn, ctx, action, title, reason, now)
            .await
            .map_err(internal)?
            .map_err(bad)?;
        txn.commit().await.map_err(internal)?;
        if attach {
            ctx.cards
                .lock()
                .unwrap()
                .push(pots_proto::Card::Approval { id });
        }
        Ok(json!({
            "approval_id": id.to_string(),
            "note": "已申請，使用者會看到一張核准卡片；核准後才會執行。",
        })
        .to_string())
    }
}
