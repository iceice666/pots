//! 客戶端指令（[`ClientCommand`]）的處理。結果經由事件日誌廣播；
//! 被拒絕或失敗時只回給發出指令的連線一個 `Error`。

use pots_proto::{
    AgentState, ArtifactId, ArtifactKind, Author, ClientCommand, ItemId, Msg, MsgStatus, ProjectId,
    ProjectSnapshot, SendMode, ServerEvent, StreamItemKind, StreamItemView, StreamScope, TaskId,
    Timestamp,
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
    run_logged(app, cmd).await.map(|()| None)
}

async fn run_logged(app: &App, cmd: ClientCommand) -> Result<(), String> {
    match cmd {
        ClientCommand::Resume { .. } => Err("這條連線已經 Resume 過".into()),
        ClientCommand::CreateProject { name } => create_project(app, name).await,
        ClientCommand::Send {
            project,
            id,
            text,
            mode,
        } => send(app, project, id, text, mode).await,
        ClientCommand::Stop { project } => {
            app.runs.stop(project);
            Ok(())
        }
        ClientCommand::Decide { .. } => Err("尚未支援".into()),
        ClientCommand::Cancel { task } => cancel_task(app, task).await,
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
                    // 啟用 agent 提案的 listener 就是核准它（M5 之前）。
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
        };
        txn.push(|seq| ServerEvent::ProjectAdded { seq, project })
            .await?;
        txn.commit().await
    }
    .await;
    result.map_err(internal)
}

/// 這個 project 屬於本機使用者（`Store::project` 依 user 過濾）。
async fn owns(app: &App, project: ProjectId) -> Result<bool, crate::Error> {
    Ok(app.store.project(project).await?.is_some())
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

/// 你取消一個任務。不叫醒 Planner（M2 的 Planner 只在 Goal 建立與 steer 時醒來）。
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
        tasks::push_goal(&mut txn, task.goal).await?;
        txn.commit().await?;
        Ok(Ok(()))
    }
    .await;
    result.map_err(internal)?
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
