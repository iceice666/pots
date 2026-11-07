//! 客戶端指令（[`ClientCommand`]）的處理。結果經由事件日誌廣播；
//! 被拒絕或失敗時只回給發出指令的連線一個 `Error`。

use pots_proto::{
    AgentState, Author, ClientCommand, ItemId, Msg, MsgStatus, ProjectId, ProjectSnapshot,
    SendMode, ServerEvent, StreamItemKind, StreamItemView, StreamScope, Timestamp,
};
use tokio::sync::mpsc;

use crate::App;
use crate::runs::Trigger;
use crate::store::{ItemStatus, ProjectRow, Store};

const MAX_PROJECT_NAME: usize = 100;

pub(crate) async fn handle(app: &App, text: &str, reply: &mpsc::Sender<ServerEvent>) {
    let result = match serde_json::from_str::<ClientCommand>(text) {
        Ok(cmd) => run(app, cmd).await,
        Err(_) => Err("無法解析指令".to_string()),
    };
    if let Err(message) = result {
        let _ = reply.send(ServerEvent::Error { message }).await;
    }
}

async fn run(app: &App, cmd: ClientCommand) -> Result<(), String> {
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
        ClientCommand::Decide { .. } | ClientCommand::Cancel { .. } => Err("尚未支援".into()),
        ClientCommand::StartModelLogin => app
            .model_auth
            .start_login()
            .await
            .map(drop)
            .map_err(|e| format!("無法開始登入：{e}")),
        ClientCommand::CancelModelLogin => {
            app.model_auth.cancel_login().await;
            Ok(())
        }
        ClientCommand::ModelLogout => app
            .model_auth
            .logout()
            .await
            .map_err(|e| format!("無法登出：{e}")),
    }
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
        };
        txn.push(|seq| ServerEvent::ProjectAdded { seq, project })
            .await?;
        txn.commit().await
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

    let inserted: Result<Result<bool, String>, crate::Error> = async {
        let mut txn = app.hub.begin().await?;
        match Store::item_project(txn.conn(), id).await? {
            // 同一則訊息重送（例如重連後）：已經處理過，不再觸發。
            Some(p) if p == project => return Ok(Ok(false)),
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
        txn.commit().await?;
        Ok(Ok(true))
    }
    .await;
    if inserted.map_err(internal)?? {
        app.runs.enqueue(project, Trigger { item: id, text }, steer);
    }
    Ok(())
}
