//! 客戶端指令（[`ClientCommand`]）的處理。結果經由事件日誌廣播；
//! 被拒絕或失敗時只回給發出指令的連線一個 `Error`。

use pots_proto::{
    AgentState, ArtifactId, ArtifactKind, Author, Card, ClientCommand, ItemId, Msg, MsgStatus,
    ProjectId, ProjectSnapshot, SendMode, ServerEvent, StreamItemKind, StreamItemView, StreamScope,
    TaskId, Timestamp, TraceStepView, WhyView,
};
use tokio::sync::mpsc;

use crate::App;
use crate::artifacts::{self, ArtifactRow};
use crate::automation::{self, ListenerDbState};
use crate::planner::{self, PlannerTrigger};
use crate::runs::Trigger;
use crate::store::{ItemStatus, ProjectRow, Store};
use crate::tasks::{self, Cause, Principal, TaskState};

const MAX_PROJECT_NAME: usize = 100;

pub(crate) async fn handle(app: &App, text: &str, reply: &mpsc::Sender<ServerEvent>) {
    let result = match serde_json::from_str::<ClientCommand>(text) {
        Ok(ClientCommand::TestProvider { request, draft }) => {
            // 要連網（最多十幾秒），另開 task，不擋住這條連線之後的指令。
            let providers = app.providers.clone();
            let reply = reply.clone();
            tokio::spawn(async move {
                let ev = crate::providers::test(&providers, request, draft).await;
                let _ = reply.send(ev).await;
            });
            return;
        }
        Ok(ClientCommand::RefreshPricing) => {
            // 抓 models.dev 要幾秒，另開 task，不擋住這條連線之後的指令。
            let store = app.store.clone();
            let reply = reply.clone();
            tokio::spawn(async move {
                let ev = match crate::cost::refresh_pricing(&store).await {
                    Ok(info) => ServerEvent::Pricing { info },
                    Err(message) => ServerEvent::Error { message },
                };
                let _ = reply.send(ev).await;
            });
            return;
        }
        Ok(ClientCommand::TestGithub { request, draft }) => {
            let watcher = app.watcher.clone();
            let hub = app.hub.clone();
            let reply = reply.clone();
            tokio::spawn(async move {
                let ev = crate::watcher::test_github(&watcher, &hub, request, draft).await;
                let _ = reply.send(ev).await;
            });
            return;
        }
        Ok(cmd) => run(app, cmd).await,
        Err(_) => Err("無法解析指令".to_string()),
    };
    let direct = match result {
        Ok(Some(ev)) => ev,
        Ok(None) => return,
        Err(message) => ServerEvent::Error { message },
    };
    let _ = reply.send(direct).await;
}

/// 執行指令。`Ok(Some(ev))` 是只回給這條連線、不寫日誌的事件。
async fn run(app: &App, cmd: ClientCommand) -> Result<Option<ServerEvent>, String> {
    if let ClientCommand::StartModelLogin = cmd {
        // 登入已經在進行中時，授權控制會回同一個網址而不改狀態，不會有 `ModelAuth`
        // 事件；所以網址直接回給按下按鈕的連線。
        return app
            .model_auth
            .start_login()
            .await
            .map(|url| Some(ServerEvent::LoginUrl { url }))
            .map_err(|e| format!("無法開始登入：{e}"));
    }
    if let ClientCommand::ArtifactHistory { artifact } = cmd {
        return artifact_history(app, artifact).await.map(Some);
    }
    if let ClientCommand::Why { item } = cmd {
        return why(app, item).await.map(Some);
    }
    if let ClientCommand::CostReport {
        project,
        since,
        until,
    } = cmd
    {
        return cost_report(app, project, since, until).await.map(Some);
    }
    if let ClientCommand::Pricing = cmd {
        return pricing(app).await.map(Some);
    }
    if let ClientCommand::SetPrice { price } = cmd {
        return set_price(app, price).await.map(Some);
    }
    if let ClientCommand::DeletePrice { provider, model } = cmd {
        return delete_price(app, provider, model).await.map(Some);
    }
    run_logged(app, cmd).await.map(|()| None)
}

async fn run_logged(app: &App, cmd: ClientCommand) -> Result<(), String> {
    match cmd {
        ClientCommand::Resume { .. } => Err("這條連線已經 Resume 過".into()),
        ClientCommand::Why { .. } => Err("這個指令要直接回覆".into()),
        ClientCommand::CostReport { .. }
        | ClientCommand::Pricing
        | ClientCommand::SetPrice { .. }
        | ClientCommand::DeletePrice { .. }
        | ClientCommand::RefreshPricing => Err("這個指令要直接回覆".into()),
        ClientCommand::CreateProject { name } => create_project(app, name).await,
        ClientCommand::Send {
            project,
            id,
            text,
            mode,
        } => send(app, project, id, text, mode).await,
        ClientCommand::Stop { project } => {
            if project == crate::store::OVERSIGHT_PROJECT_ID {
                return Err("總管的對話請用 StopOverseer".into());
            }
            app.runs.stop(project);
            Ok(())
        }
        ClientCommand::SendOverseer { id, text } => send_overseer(app, id, text).await,
        ClientCommand::StopOverseer => {
            app.overseer_runs.stop(crate::store::OVERSIGHT_PROJECT_ID);
            Ok(())
        }
        ClientCommand::MarkRead { project } => mark_read(app, project).await,
        ClientCommand::Decide {
            approval,
            approve,
            note,
        } => crate::approvals::decide(app, approval, approve, note).await,
        ClientCommand::Cancel { task } => cancel_task(app, task).await,
        ClientCommand::RetryTask { task } => retry_task(app, task).await,
        ClientCommand::SaveArtifact {
            artifact,
            base_rev,
            body,
        } => save_artifact(app, artifact, Edit::Save { base_rev, body }).await,
        ClientCommand::RestoreArtifact { artifact, rev } => {
            save_artifact(app, artifact, Edit::Restore { rev }).await
        }
        ClientCommand::ArtifactHistory { .. } => unreachable!("在 run 處理"),
        ClientCommand::StartModelLogin => unreachable!("在 run 處理"),
        ClientCommand::CancelModelLogin => {
            app.model_auth.cancel_login().await;
            Ok(())
        }
        ClientCommand::ModelLogout => app
            .model_auth
            .logout()
            .await
            .map_err(|e| format!("無法登出：{e}")),
        ClientCommand::SaveProvider { draft } => {
            retry_memory(app, crate::providers::save(app, draft).await).await
        }
        ClientCommand::DeleteProvider { id } => {
            retry_memory(app, crate::providers::delete(app, id).await).await
        }
        ClientCommand::SetActiveProvider { id } => {
            retry_memory(app, crate::providers::set_active(app, id).await).await
        }
        ClientCommand::TestProvider { .. } => unreachable!("在 handle 處理"),
        ClientCommand::SetMemoryModel { provider, model } => {
            retry_memory(
                app,
                crate::providers::set_memory(app, provider, model).await,
            )
            .await
        }
        ClientCommand::PinObservation {
            project,
            id,
            pinned,
        } => resync_on_error(app, project, app.memory.pin(project, id, pinned).await).await,
        ClientCommand::EditObservation { project, id, text } => {
            resync_on_error(app, project, app.memory.edit(project, id, text).await).await
        }
        ClientCommand::DeleteObservation { project, id } => {
            resync_on_error(app, project, app.memory.delete(project, id).await).await
        }
        ClientCommand::SaveGithub { draft } => {
            crate::watcher::save_github(&app.watcher, &app.hub, draft).await
        }
        ClientCommand::TestGithub { .. } => unreachable!("在 handle 處理"),
        ClientCommand::SaveSource { draft } => save_source(app, draft).await,
        ClientCommand::DeleteSource { id } => delete_source(app, &id).await,
        ClientCommand::SetSourceEnabled { id, enabled } => {
            automation_edit(app, |conn, now| {
                Box::pin(async move {
                    sqlx::query(
                        "UPDATE sources SET enabled = ?, updated_at = ? WHERE id = ? AND kind != 'github'",
                    )
                    .bind(i64::from(enabled))
                    .bind(now.0)
                    .bind(&id)
                    .execute(conn)
                    .await?;
                    Ok(Ok(()))
                })
            })
            .await
        }
        ClientCommand::SetProjectRepos { project, repos } => {
            set_project_repos(app, project, repos).await
        }
        ClientCommand::SaveListener { id, spec } => save_listener(app, id, spec).await,
        ClientCommand::SetListenerEnabled { id, enabled } => {
            automation_edit(app, |conn, now| {
                Box::pin(async move {
                    let Some(l) = automation::listener(conn, id).await? else {
                        return Ok(Err("找不到這個 listener".to_string()));
                    };
                    // agent 的提案在等核准：在核准卡片決定（停用就是撤回提案）。M5 之前的提案沒有
                    // 核准，啟用就是核准它。
                    if enabled && l.state == ListenerDbState::Proposed && l.approval.is_some() {
                        return Ok(Err(
                            "這個 listener 在等你核准：請在 Inbox 或核准卡片決定".to_string()
                        ));
                    }
                    let state = if enabled {
                        ListenerDbState::Active
                    } else {
                        ListenerDbState::Disabled
                    };
                    if l.state != state {
                        automation::set_listener_state(conn, id, state, now).await?;
                    }
                    Ok(Ok(()))
                })
            })
            .await
        }
        ClientCommand::DeleteListener { id } => {
            automation_edit(app, |conn, _| {
                Box::pin(async move {
                    Ok(if automation::delete_listener(conn, id).await? {
                        Ok(())
                    } else {
                        Err("找不到這個 listener".to_string())
                    })
                })
            })
            .await
        }
        ClientCommand::SetScheduleEnabled { id, enabled } => {
            automation_edit(app, |conn, now| {
                Box::pin(async move {
                    if automation::schedule(conn, id).await?.is_none() {
                        return Ok(Err("找不到這個排程".to_string()));
                    }
                    automation::set_schedule_enabled(conn, id, enabled, now).await?;
                    Ok(Ok(()))
                })
            })
            .await
        }
        ClientCommand::RescheduleSchedule { id, next_at } => {
            automation_edit(app, |conn, now| {
                Box::pin(async move {
                    let Some(s) = automation::schedule(conn, id).await? else {
                        return Ok(Err("找不到這個排程".to_string()));
                    };
                    automation::reschedule(conn, &s, next_at, now).await
                })
            })
            .await
        }
        ClientCommand::SetScheduleExpiry { id, expires_at } => {
            automation_edit(app, |conn, now| {
                Box::pin(async move {
                    let Some(s) = automation::schedule(conn, id).await? else {
                        return Ok(Err("找不到這個排程".to_string()));
                    };
                    automation::set_expiry(conn, &s, expires_at, now).await
                })
            })
            .await
        }
        ClientCommand::DeleteSchedule { id } => {
            automation_edit(app, |conn, _| {
                Box::pin(async move {
                    Ok(if automation::delete_schedule(conn, id).await? {
                        Ok(())
                    } else {
                        Err("找不到這個排程".to_string())
                    })
                })
            })
            .await
        }
        ClientCommand::PauseAutomations { project, until } => {
            if let Some(p) = project
                && !owns(app, p).await.map_err(internal)?
            {
                return Err("找不到這個 project".into());
            }
            automation_edit(app, |conn, now| {
                Box::pin(async move {
                    if until.is_some_and(|u| u.0 <= now.0) {
                        return Ok(Err("恢復時間已經過了".to_string()));
                    }
                    automation::pause(conn, project, until, &Principal::Human, now).await?;
                    Ok(Ok(()))
                })
            })
            .await
        }
        ClientCommand::SaveWorkerSettings { draft } => save_worker_settings(app, draft).await,
        ClientCommand::SaveNotifications { settings } => {
            let settings = crate::notifications::check(&settings)?;
            let result: Result<(), crate::Error> = async {
                let mut txn = app.hub.begin().await?;
                crate::notifications::save(&mut txn, &settings).await?;
                txn.commit().await
            }
            .await;
            result.map_err(internal)
        }
        ClientCommand::SaveUserMd { base_rev, body } => {
            let result: Result<Result<(), String>, crate::Error> = async {
                let mut txn = app.hub.begin().await?;
                match crate::user_md::save(&mut txn, base_rev, &body, &Principal::Human).await? {
                    Ok(_) => {
                        txn.commit().await?;
                        Ok(Ok(()))
                    }
                    Err(e) => Ok(Err(e)),
                }
            }
            .await;
            result.map_err(internal)?
        }
        ClientCommand::SetMergeMethod { project, method } => {
            if !owns(app, project).await.map_err(internal)? {
                return Err("找不到這個 project".into());
            }
            let result: Result<(), crate::Error> = async {
                let mut txn = app.hub.begin().await?;
                crate::approvals::set_project_merge_method(txn.conn(), project, method).await?;
                txn.push(|seq| ServerEvent::ProjectMergeMethod {
                    seq,
                    project,
                    method,
                })
                .await?;
                txn.commit().await
            }
            .await;
            result.map_err(internal)
        }
        ClientCommand::ResumeAutomations { project } => {
            automation_edit(app, |conn, _| {
                Box::pin(async move {
                    Ok(if automation::resume(conn, project).await? {
                        Ok(())
                    } else {
                        Err("沒有暫停中的自動化".to_string())
                    })
                })
            })
            .await
        }
    }
}

type EditFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Result<(), String>, crate::Error>> + Send + 'a>,
>;

/// 改自動化資料：在同一個 transaction 裡記一筆 `Automation`，完成後讓 sweeper 馬上掃一次
/// （例如改了排程時間、恢復了暫停）。
async fn automation_edit(
    app: &App,
    edit: impl for<'c> FnOnce(&'c mut sqlx::SqliteConnection, Timestamp) -> EditFuture<'c>,
) -> Result<(), String> {
    let now = app.watcher.clock().now();
    let mut txn = app.hub.begin().await.map_err(internal)?;
    match edit(txn.conn(), now).await.map_err(internal)? {
        Ok(()) => {}
        Err(e) => return Err(e),
    }
    // 被刪掉、被你直接改掉或停用的提案：它的核准撤回。
    crate::approvals::sync_listeners(&mut txn)
        .await
        .map_err(internal)?;
    automation::push(&mut txn, now).await.map_err(internal)?;
    txn.commit().await.map_err(internal)?;
    app.watcher.sweep_now();
    Ok(())
}

async fn set_project_repos(
    app: &App,
    project: ProjectId,
    repos: Vec<String>,
) -> Result<(), String> {
    if !owns(app, project).await.map_err(internal)? {
        return Err("找不到這個 project".into());
    }
    let mut clean: Vec<String> = Vec::new();
    for r in repos.iter().filter(|r| !r.trim().is_empty()) {
        let r = crate::watcher::event::normalize_repo(r)?;
        if !clean.contains(&r) {
            clean.push(r);
        }
    }
    if clean.len() > automation::MAX_REPOS_PER_PROJECT {
        return Err(format!(
            "每個 project 最多監看 {} 個 repo",
            automation::MAX_REPOS_PER_PROJECT
        ));
    }
    let now = app.watcher.clock().now();
    let result: Result<(), crate::Error> = async {
        let mut txn = app.hub.begin().await?;
        automation::set_project_repos(txn.conn(), project, &clean, now).await?;
        // 不再監看的 repo 丟掉輪詢進度：之後再加回來時從當時開始，不把中間的事件當新的。
        let watched = automation::all_repos(txn.conn()).await?;
        crate::watcher::prune_cursor(txn.conn(), &watched).await?;
        let repos = clean.clone();
        txn.push(|seq| ServerEvent::ProjectRepos {
            seq,
            project,
            repos,
        })
        .await?;
        // listener 的來源說明會變。
        automation::push(&mut txn, now).await?;
        txn.commit().await
    }
    .await;
    result.map_err(internal)?;
    app.watcher.poll_now();
    Ok(())
}

async fn save_listener(
    app: &App,
    id: Option<pots_proto::ListenerId>,
    spec: pots_proto::ListenerSpec,
) -> Result<(), String> {
    if !owns(app, spec.project).await.map_err(internal)? {
        return Err("找不到這個 project".into());
    }
    let spec = automation::check_spec(spec)?;
    automation_edit(app, |conn, now| {
        Box::pin(async move {
            if let Some(id) = id
                && automation::listener(conn, id).await?.is_none()
            {
                return Ok(Err("找不到這個 listener".to_string()));
            }
            // 你建立或修改的 listener 直接生效。
            Ok(automation::save_listener(
                conn,
                id,
                &spec,
                ListenerDbState::Active,
                &Principal::Human,
                now,
            )
            .await?
            .map(|_| ()))
        })
    })
    .await
}

/// provider 或記憶用的模型改了：整理記憶不用等冷卻結束，馬上再試。
async fn retry_memory(app: &App, result: Result<(), String>) -> Result<(), String> {
    if result.is_ok() {
        app.memory.retry_all().await;
    }
    result
}

/// 觀察的修改被拒絕時，重送一次目前的記憶，讓客戶端丟掉它先改的內容。
async fn resync_on_error(
    app: &App,
    project: ProjectId,
    result: Result<(), String>,
) -> Result<(), String> {
    if result.is_err() && app.store.project(project).await.ok().flatten().is_some() {
        app.memory.resync(project).await;
    }
    result
}

fn internal(e: crate::Error) -> String {
    tracing::error!(error = %e, "指令執行失敗");
    "伺服器內部錯誤".into()
}

async fn create_project(app: &App, name: String) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("project 名稱不能是空的".into());
    }
    if name.chars().count() > MAX_PROJECT_NAME {
        return Err(format!("project 名稱最多 {MAX_PROJECT_NAME} 個字"));
    }
    let row = ProjectRow {
        id: ProjectId::new(),
        name,
        instructions: String::new(),
        agent_name: app.agent_name.clone(),
        created_at: Timestamp::now(),
    };
    let result: Result<(), crate::Error> = async {
        let mut txn = app.hub.begin().await?;
        Store::insert_project(txn.conn(), &row).await?;
        let project = ProjectSnapshot {
            id: row.id,
            name: row.name.clone(),
            agent_name: row.agent_name.clone(),
            agent_state: AgentState::Idle,
            items: Vec::new(),
            memory: Default::default(),
            goals: Vec::new(),
            artifacts: Vec::new(),
            repos: Vec::new(),
            merge_method: Default::default(),
        };
        txn.push(|seq| ServerEvent::ProjectAdded { seq, project })
            .await?;
        crate::user_md::ensure_project_notes(&mut txn, row.id).await?;
        txn.commit().await
    }
    .await;
    result.map_err(internal)
}

/// 這個 project 屬於本機使用者（`Store::project` 依 user 過濾）。
async fn owns(app: &App, project: ProjectId) -> Result<bool, crate::Error> {
    // 總管的內部 project 不是使用者操作的 project；一般指令不接受它。
    Ok(app.store.project(project).await?.is_some()
        && app.store.project_kind(project).await?.as_deref() == Some("project"))
}

/// 你對 artifact 的修改。
enum Edit {
    Save { base_rev: u32, body: String },
    Restore { rev: u32 },
}

/// 儲存或還原 artifact。改了進行中 Goal 的 Brief 時叫醒 Planner（你的編輯就是 steer）。
async fn save_artifact(app: &App, id: ArtifactId, edit: Edit) -> Result<(), String> {
    let result: Result<Result<(), String>, crate::Error> = async {
        let mut txn = app.hub.begin().await?;
        let Some(row) = artifacts::get(txn.conn(), id).await? else {
            return Ok(Err("找不到這份 artifact".into()));
        };
        if !owns(app, row.project).await? {
            return Ok(Err("找不到這份 artifact".into()));
        }
        let saved = match &edit {
            Edit::Save { base_rev, body } => {
                artifacts::human_save(txn.conn(), &row, *base_rev, body).await?
            }
            Edit::Restore { rev } => artifacts::restore(txn.conn(), &row, *rev).await?,
        };
        let new: ArtifactRow = match saved {
            Err(e) => return Ok(Err(e)),
            // 沒有變化：重送目前的內容，讓客戶端收掉「未儲存」。
            Ok(None) => {
                artifacts::push_changed(&mut txn, &row).await?;
                txn.commit().await?;
                return Ok(Ok(()));
            }
            Ok(Some(new)) => new,
        };
        artifacts::push_changed(&mut txn, &new).await?;
        let steer = match (new.kind, new.goal) {
            (ArtifactKind::Brief, Some(goal))
                if tasks::goal(txn.conn(), goal)
                    .await?
                    .is_some_and(|g| g.state.is_active()) =>
            {
                planner::enqueue(
                    txn.conn(),
                    new.project,
                    goal,
                    &PlannerTrigger::BriefEdited { rev: new.rev },
                )
                .await?;
                Some(goal)
            }
            _ => None,
        };
        txn.commit_then(|| {
            if let Some(goal) = steer {
                app.planner.steer(goal);
            }
        })
        .await?;
        Ok(Ok(()))
    }
    .await;
    result.map_err(internal)?
}

/// 「你不在期間」的門檻：距上次看到超過兩小時才插入摘要卡片。
const CATCH_UP_SECS: i64 = 2 * 3600;

/// 你看到這個 project 的對話了。離開超過兩小時而且期間有里程碑時，先插一張摘要卡片。
async fn mark_read(app: &App, project: ProjectId) -> Result<(), String> {
    if project == crate::store::OVERSIGHT_PROJECT_ID {
        return Err("總管沒有已讀狀態".into());
    }
    if app
        .store
        .project(project)
        .await
        .map_err(internal)?
        .is_none()
    {
        return Err("找不到這個 project".into());
    }
    let now = app.watcher.clock().now();
    let last = app
        .store
        .project_last_read(project)
        .await
        .map_err(internal)?;
    if let Some(last) = last
        && now.0 - last.0 >= CATCH_UP_SECS
    {
        let result: Result<(), crate::Error> = async {
            let mut conn = app.store.acquire().await?;
            let milestones = crate::budget::milestones_since(&mut conn, project, last).await?;
            drop(conn);
            if milestones.any() {
                let mut txn = app.hub.begin().await?;
                crate::tools::post_card(
                    &mut txn,
                    project,
                    Card::CatchUp {
                        summary: milestones.summary(),
                    },
                    None,
                )
                .await?;
                txn.commit().await?;
            }
            Ok(())
        }
        .await;
        result.map_err(internal)?;
    }
    app.store
        .mark_project_read(project, now)
        .await
        .map_err(internal)?;
    Ok(())
}

/// 對總管說話。
async fn send_overseer(app: &App, id: ItemId, text: String) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("訊息是空的".into());
    }
    let project = crate::store::OVERSIGHT_PROJECT_ID;
    let item = StreamItemView {
        id,
        at: Timestamp::now(),
        kind: StreamItemKind::Human(Msg {
            author: Author::Human,
            text: text.clone(),
            status: MsgStatus::Done,
            tools: None,
            cards: Vec::new(),
            steer: false,
        }),
    };
    let inserted: Result<Result<(), String>, crate::Error> = async {
        let mut txn = app.hub.begin().await?;
        match Store::item_project(txn.conn(), id).await? {
            Some(p) if p == project => return Ok(Ok(())),
            Some(_) => return Ok(Err("訊息 ID 重複".into())),
            None => {}
        }
        Store::insert_item(txn.conn(), project, &item, ItemStatus::Pending, None).await?;
        txn.push(|seq| ServerEvent::Item {
            seq,
            scope: StreamScope::Overseer,
            item,
        })
        .await?;
        txn.commit_then(|| {
            app.overseer_runs
                .enqueue(project, Trigger { item: id, text }, false)
        })
        .await?;
        Ok(Ok(()))
    }
    .await;
    inserted.map_err(internal)?
}

/// 成本報表；只回給問的連線。
async fn cost_report(
    app: &App,
    project: Option<ProjectId>,
    since: Option<Timestamp>,
    until: Option<Timestamp>,
) -> Result<ServerEvent, String> {
    let now = Timestamp::now();
    let until = until.unwrap_or(now);
    let since = since.unwrap_or(Timestamp(until.0 - crate::cost::DEFAULT_REPORT_SECS));
    if since.0 >= until.0 {
        return Err("報表的開始時間要早於結束時間".into());
    }
    if until.0.saturating_sub(since.0) > 366 * 24 * 3600 {
        return Err("報表區間最多一年".into());
    }
    if let Some(project) = project
        && app
            .store
            .project(project)
            .await
            .map_err(internal)?
            .is_none()
    {
        return Err("找不到這個 project".into());
    }
    let report = crate::cost::report(&app.store, project, since, until)
        .await
        .map_err(internal)?;
    Ok(ServerEvent::CostReport { report })
}

async fn pricing(app: &App) -> Result<ServerEvent, String> {
    let info = crate::cost::pricing_info(&app.store)
        .await
        .map_err(internal)?;
    Ok(ServerEvent::Pricing { info })
}

async fn set_price(app: &App, price: pots_proto::ModelPrice) -> Result<ServerEvent, String> {
    crate::cost::set_price(&app.store, &price).await?;
    pricing(app).await
}

async fn delete_price(app: &App, provider: String, model: String) -> Result<ServerEvent, String> {
    crate::cost::delete_price(&app.store, &provider, &model).await?;
    pricing(app).await
}

/// 一則 agent 訊息為什麼這樣回答：觸發來源、當下的記憶版本、模型與工具順序。
async fn why(app: &App, item: ItemId) -> Result<ServerEvent, String> {
    let result: Result<Option<ServerEvent>, crate::Error> = async {
        let mut conn = app.store.acquire().await?;
        let row = sqlx::query("SELECT run_id, project_id FROM stream_items WHERE id = ?")
            .bind(item.to_string())
            .fetch_optional(&mut *conn)
            .await?;
        let Some(row) = row else { return Ok(None) };
        use sqlx::Row;
        let project: ProjectId = crate::store::parse_id(row.try_get("project_id")?)?;
        if app.store.project(project).await?.is_none() {
            return Ok(None);
        }
        let run: Option<String> = row.try_get("run_id")?;
        let Some(run) = run else {
            return Ok(Some(ServerEvent::Why { why: None }));
        };
        let run: uuid::Uuid = crate::store::parse_id(run)?;
        let row = sqlx::query(
            "SELECT agent, model, trigger, memory_revision, started_at, finished_at, stop_reason, \
             input_tokens, output_tokens FROM agent_runs WHERE id = ?",
        )
        .bind(run.to_string())
        .fetch_optional(&mut *conn)
        .await?;
        let Some(row) = row else {
            return Ok(Some(ServerEvent::Why { why: None }));
        };
        let steps = app
            .store
            .trace(run)
            .await?
            .into_iter()
            .map(|t| TraceStepView {
                kind: t.kind,
                name: t.name,
                at: t.at,
                ok: t.ok,
                detail: t.detail.to_string(),
            })
            .collect();
        Ok(Some(ServerEvent::Why {
            why: Some(WhyView {
                item,
                agent: row.try_get("agent")?,
                model: row.try_get("model")?,
                trigger: trigger_summary(&row.try_get::<String, _>("trigger")?),
                memory_revision: row.try_get("memory_revision")?,
                started_at: Timestamp(row.try_get("started_at")?),
                finished_at: row.try_get::<Option<i64>, _>("finished_at")?.map(Timestamp),
                stop_reason: row.try_get("stop_reason")?,
                input_tokens: row.try_get::<i64, _>("input_tokens")?.max(0) as u64,
                output_tokens: row.try_get::<i64, _>("output_tokens")?.max(0) as u64,
                steps,
            }),
        }))
    }
    .await;
    result
        .map_err(internal)?
        .ok_or_else(|| "找不到這則訊息".into())
}

/// 把 `agent_runs.trigger` 的 JSON 轉成一句人看得懂的話。
fn trigger_summary(trigger: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(trigger).unwrap_or(serde_json::Value::Null);
    let pick = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .unwrap_or_default()
    };
    match value.get("kind").and_then(|k| k.as_str()) {
        Some("messages") => {
            let text = pick(&value, "text");
            if text.is_empty() {
                "使用者訊息".into()
            } else {
                format!("使用者訊息：{}", tasks::one_line(&text, 200))
            }
        }
        Some("scheduled") => {
            let prompt = pick(&value, "prompt");
            format!("排程觸發：{}", tasks::one_line(&prompt, 200))
        }
        Some("subagent") => "subagent".into(),
        Some(other) => other.to_string(),
        None if value.get("triggers").is_some() => {
            let kinds = value
                .get("triggers")
                .and_then(|t| t.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|t| t.get("kind").and_then(|k| k.as_str()))
                        .collect::<Vec<_>>()
                        .join("、")
                })
                .unwrap_or_default();
            if kinds.is_empty() {
                "Planner 被叫醒".into()
            } else {
                format!("Planner 觸發：{kinds}")
            }
        }
        None => tasks::one_line(trigger, 200),
    }
}

async fn artifact_history(app: &App, id: ArtifactId) -> Result<ServerEvent, String> {
    let result: Result<Option<ServerEvent>, crate::Error> = async {
        let mut conn = app.store.acquire().await?;
        match artifacts::get(&mut conn, id).await? {
            Some(a) if owns(app, a.project).await? => {}
            _ => return Ok(None),
        }
        let revisions = artifacts::history(&mut conn, id).await?;
        Ok(Some(ServerEvent::ArtifactHistory {
            artifact: id,
            revisions,
        }))
    }
    .await;
    result
        .map_err(internal)?
        .ok_or_else(|| "找不到這份 artifact".into())
}

/// 你取消一個任務：正在跑的 worker 被中斷。不叫醒 Planner（你自己決定的）。
async fn cancel_task(app: &App, id: TaskId) -> Result<(), String> {
    let result: Result<Result<(), String>, crate::Error> = async {
        let mut txn = app.hub.begin().await?;
        let Some(task) = tasks::task(txn.conn(), id).await? else {
            return Ok(Err("找不到這個任務".into()));
        };
        let project = tasks::goal(txn.conn(), task.goal).await?.map(|g| g.project);
        match project {
            Some(p) if owns(app, p).await? => {}
            _ => return Ok(Err("找不到這個任務".into())),
        }
        if !task.state.is_open() {
            // 已經結束：重送目前的 Goal，讓客戶端蓋掉它先改的狀態。
            tasks::push_goal(&mut txn, task.goal).await?;
            txn.commit().await?;
            return Ok(Err("這個任務已經結束".into()));
        }
        tasks::set_task_state(
            txn.conn(),
            &task,
            &TaskState::Cancelled {
                by: Principal::Human,
            },
            &Cause {
                by: Principal::Human,
                reason: String::new(),
            },
        )
        .await?;
        // 快速修改只有這一個任務、沒有 Planner：任務取消就是 Goal 取消。
        let goal = tasks::goal(txn.conn(), task.goal).await?;
        if let Some(g) = goal.filter(|g| g.state.is_active())
            && tasks::goal_kind(txn.conn(), g.id).await? == "quick"
        {
            crate::tools::close_goal(
                &mut txn,
                &g,
                pots_proto::GoalState::Cancelled {
                    reason: "你取消了這個快速修改".into(),
                },
                &Principal::Human,
                "快速修改已取消",
            )
            .await?;
        }
        tasks::push_goal(&mut txn, task.goal).await?;
        let worker = app.worker.clone();
        txn.commit_then(|| worker.interrupt(id)).await?;
        Ok(Ok(()))
    }
    .await;
    result.map_err(internal)?
}

/// 你重試一個需要決定（或失敗）的任務：回到佇列，worker 從工作目錄的現況接著做。
async fn retry_task(app: &App, id: TaskId) -> Result<(), String> {
    let result: Result<Result<(), String>, crate::Error> = async {
        let mut txn = app.hub.begin().await?;
        let Some(task) = tasks::task(txn.conn(), id).await? else {
            return Ok(Err("找不到這個任務".into()));
        };
        let Some(goal) = tasks::goal(txn.conn(), task.goal).await? else {
            return Ok(Err("找不到這個任務".into()));
        };
        if !owns(app, goal.project).await? {
            return Ok(Err("找不到這個任務".into()));
        }
        let rejected = if !goal.state.is_active() {
            Some(format!("Goal「{}」已經{}", goal.title, goal.state.label()))
        } else if !matches!(
            task.state,
            TaskState::NeedsHuman { .. } | TaskState::Failed { .. }
        ) {
            Some("只有需要決定或失敗的任務可以重試".to_string())
        } else if task.repo.is_none() {
            Some("這個任務沒有 repo，不會派給 worker".to_string())
        } else {
            None
        };
        if let Some(why) = rejected {
            // 重送目前的 Goal，讓客戶端蓋掉它先改的狀態。
            tasks::push_goal(&mut txn, goal.id).await?;
            txn.commit().await?;
            return Ok(Err(why));
        }
        tasks::set_task_state(
            txn.conn(),
            &task,
            &TaskState::Planned,
            &Cause {
                by: Principal::Human,
                reason: "你要求重試".into(),
            },
        )
        .await?;
        // 帶上之前停在哪裡（例如核准逾時：重新申請就好，不用重做）。
        let was = match &task.state {
            TaskState::NeedsHuman { reason } | TaskState::Failed { error: reason } => {
                format!("使用者要求重試；之前停在：{}", tasks::one_line(reason, 500))
            }
            _ => "使用者要求重試".into(),
        };
        crate::worker::enqueue(
            txn.conn(),
            goal.project,
            goal.id,
            task.id,
            &crate::worker::WorkerTrigger::Retry { reason: was },
        )
        .await?;
        tasks::push_goal(&mut txn, goal.id).await?;
        let worker = app.worker.clone();
        txn.commit_then(|| worker.kick()).await?;
        Ok(Ok(()))
    }
    .await;
    result.map_err(internal)?
}

/// 儲存 worker 設定（只影響之後開始的 run）。
async fn save_worker_settings(
    app: &App,
    draft: pots_proto::WorkerSettingsDraft,
) -> Result<(), String> {
    let draft = crate::worker::check_settings(&draft)?;
    let result: Result<(), crate::Error> = async {
        let mut txn = app.hub.begin().await?;
        crate::worker::save_settings(&mut txn, &draft, app.worker.data_dir()).await?;
        let worker = app.worker.clone();
        txn.commit_then(|| worker.kick()).await
    }
    .await;
    result.map_err(internal)
}

async fn send(
    app: &App,
    project: ProjectId,
    id: ItemId,
    text: String,
    mode: SendMode,
) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("訊息是空的".into());
    }
    if project == crate::store::OVERSIGHT_PROJECT_ID {
        return Err("總管的對話請用 SendOverseer".into());
    }
    if app
        .store
        .project(project)
        .await
        .map_err(internal)?
        .is_none()
    {
        return Err("找不到這個 project".into());
    }
    let steer = mode == SendMode::Steer;
    let item = StreamItemView {
        id,
        at: Timestamp::now(),
        kind: StreamItemKind::Human(Msg {
            author: Author::Human,
            text: text.clone(),
            status: MsgStatus::Done,
            tools: None,
            cards: Vec::new(),
            steer,
        }),
    };

    let inserted: Result<Result<(), String>, crate::Error> = async {
        let mut txn = app.hub.begin().await?;
        match Store::item_project(txn.conn(), id).await? {
            // 同一則訊息重送（例如重連後）：已經處理過，不再觸發。
            Some(p) if p == project => return Ok(Ok(())),
            Some(_) => return Ok(Err("訊息 ID 重複".into())),
            None => {}
        }
        Store::insert_item(txn.conn(), project, &item, ItemStatus::Pending, None).await?;
        txn.push(|seq| ServerEvent::Item {
            seq,
            scope: StreamScope::Project(project),
            item,
        })
        .await?;
        // 在日誌鎖內排進 run，佇列順序就和事件序號一致。
        txn.commit_then(|| app.runs.enqueue(project, Trigger { item: id, text }, steer))
            .await?;
        Ok(Ok(()))
    }
    .await;
    inserted.map_err(internal)?
}

/// 新增或修改一個來源；成功後馬上輪詢一次（新來源的進度是空的）。
async fn save_source(app: &App, draft: pots_proto::SourceDraft) -> Result<(), String> {
    let dir = app.watcher.data_dir().to_path_buf();
    let result = automation_edit(app, |conn, now| {
        Box::pin(async move {
            match crate::watcher::source::save(conn, &dir, &draft, now).await {
                Ok(()) => Ok(Ok(())),
                Err(e) => Ok(Err(e)),
            }
        })
    })
    .await;
    if result.is_ok() {
        app.watcher.poll_now();
    }
    result
}

async fn delete_source(app: &App, id: &str) -> Result<(), String> {
    let path = app.watcher.data_dir().to_path_buf();
    let id = id.to_string();
    automation_edit(app, |conn, _| {
        Box::pin(async move {
            Ok(crate::watcher::source::delete(conn, &path, &id)
                .await
                .map_err(internal))
        })
    })
    .await
}
