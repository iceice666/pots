//! 一個實例的 WebSocket 連線（`proposal/deployment.md#客戶端多連線`）。
//!
//! 連線跑在自己的執行緒與 tokio runtime 上，和 GPUI 之間只靠 channel：UI 送
//! [`ClientCommand`]，連線把 [`ConnEvent`] 送回來。斷線後自動重連，帶 `Resume
//! { last_seq }` 讓伺服器補送錯過的事件；斷線期間送出的指令先排著，連上後依序送出。
//! 已送出但還沒看到伺服器回送的 `Send` 也會在重連後再送一次（`Send` 帶客戶端產生的
//! ID，伺服器端是冪等的）。

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest as _};

use crate::model::{ClientCommand, ConnectionState, ItemId, ServerEvent};

/// 連線位置與存取 token。
#[derive(Clone)]
pub struct Target {
    pub addr: SocketAddr,
    pub token: String,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

/// 找到要連的服務；桌面版的實作必要時會先啟動內嵌服務。會在連線執行緒上呼叫，可以阻塞。
pub trait Endpoint: Send + 'static {
    fn locate(&mut self) -> Result<Target, String>;
}

#[derive(Debug)]
pub enum ConnEvent {
    State(ConnectionState),
    Server(Box<ServerEvent>),
    /// 找不到或啟動不了服務等需要讓人知道的錯誤。
    Problem(String),
}

/// UI 端持有的連線把手。丟掉它連線執行緒就會結束。
#[derive(Clone)]
pub struct Connection {
    commands: mpsc::UnboundedSender<ClientCommand>,
}

impl Connection {
    pub fn send(&self, command: ClientCommand) {
        // 連線執行緒結束時送不出去，沒有別的事可做。
        let _ = self.commands.send(command);
    }
}

/// 開始連線。回傳的 receiver 交給 GPUI 的 foreground task 讀。
pub fn spawn(
    endpoint: impl Endpoint,
) -> (
    Connection,
    futures::channel::mpsc::UnboundedReceiver<ConnEvent>,
) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = futures::channel::mpsc::unbounded();
    std::thread::Builder::new()
        .name("pots-connection".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ev_tx.unbounded_send(ConnEvent::Problem(format!("無法啟動連線：{e}")));
                    return;
                }
            };
            runtime.block_on(run(endpoint, cmd_rx, ev_tx));
        })
        .expect("無法建立連線執行緒");
    (Connection { commands: cmd_tx }, ev_rx)
}

type Events = futures::channel::mpsc::UnboundedSender<ConnEvent>;

/// 跨越多次連線保留的狀態。
#[derive(Default)]
struct State {
    last_seq: u64,
    /// 斷線期間 UI 送來、還沒送出的指令。
    pending: Vec<ClientCommand>,
    /// 已送出、還沒看到伺服器回送同 ID 訊息的 `Send`。連線在伺服器處理前斷掉時，
    /// 指令可能已經寫出去卻沒被收到；重連後再送一次（伺服器依 ID 去重）。
    unacked: Vec<ClientCommand>,
}

impl State {
    fn sent(&mut self, cmd: &ClientCommand) {
        if matches!(cmd, ClientCommand::Send { .. }) {
            self.unacked.push(cmd.clone());
        }
    }

    /// 收到事件時，把已經出現在伺服器端的訊息確認掉。
    fn ack(&mut self, event: &ServerEvent) {
        if self.unacked.is_empty() {
            return;
        }
        let seen: Vec<ItemId> = match event {
            ServerEvent::Item { item, .. } | ServerEvent::ItemUpdated { item, .. } => {
                vec![item.id]
            }
            ServerEvent::Snapshot { instance, .. } => instance
                .projects
                .iter()
                .flat_map(|p| p.items.iter().map(|i| i.id))
                .collect(),
            _ => return,
        };
        self.unacked
            .retain(|cmd| !matches!(cmd, ClientCommand::Send { id, .. } if seen.contains(id)));
    }

    /// 重連後要送的指令：先補上次沒被確認的訊息，再送斷線期間排著的。
    fn take_replay(&mut self) -> Vec<ClientCommand> {
        let mut replay = std::mem::take(&mut self.unacked);
        replay.append(&mut self.pending);
        replay
    }
}

async fn run(
    endpoint: impl Endpoint,
    mut commands: mpsc::UnboundedReceiver<ClientCommand>,
    events: Events,
) {
    let endpoint = std::sync::Arc::new(std::sync::Mutex::new(endpoint));
    let mut state = State::default();
    let mut backoff = Duration::from_millis(300);

    loop {
        let locate = {
            let endpoint = endpoint.clone();
            tokio::task::spawn_blocking(move || match endpoint.lock() {
                Ok(mut e) => e.locate(),
                Err(_) => Err("連線狀態損毀".into()),
            })
        };
        let result = match locate.await {
            Ok(Ok(target)) => session(&target, &mut state, &mut commands, &events).await,
            Ok(Err(problem)) => Err(SessionEnd::Failed(problem)),
            Err(e) => Err(SessionEnd::Failed(e.to_string())),
        };
        match result {
            Ok(()) => return,
            Err(SessionEnd::Closed) => backoff = Duration::from_millis(300),
            Err(SessionEnd::Failed(problem)) => {
                tracing::warn!(%problem, "連線失敗");
                if events.unbounded_send(ConnEvent::Problem(problem)).is_err() {
                    return;
                }
            }
        }
        if events
            .unbounded_send(ConnEvent::State(ConnectionState::Reconnecting))
            .is_err()
        {
            return;
        }
        // 等待重連的期間也收指令，免得 UI 關掉時卡住。
        let sleep = tokio::time::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = &mut sleep => break,
                cmd = commands.recv() => match cmd {
                    Some(cmd) => state.pending.push(cmd),
                    None => return,
                },
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

enum SessionEnd {
    /// 連上過之後斷線。
    Closed,
    Failed(String),
}

fn encode(cmd: &ClientCommand) -> Result<Message, SessionEnd> {
    serde_json::to_string(cmd)
        .map(|s| Message::Text(s.into()))
        .map_err(|e| SessionEnd::Failed(e.to_string()))
}

/// 一次連線：連上、Resume、轉送事件與指令，直到斷線。`Ok` 表示 UI 已經關閉。
async fn session(
    target: &Target,
    state: &mut State,
    commands: &mut mpsc::UnboundedReceiver<ClientCommand>,
    events: &Events,
) -> Result<(), SessionEnd> {
    let mut request = format!("ws://{}/ws", target.addr)
        .into_client_request()
        .map_err(|e| SessionEnd::Failed(e.to_string()))?;
    let auth = format!("Bearer {}", target.token)
        .parse()
        .map_err(|_| SessionEnd::Failed("token 格式不正確".into()))?;
    request
        .headers_mut()
        .insert(tungstenite::http::header::AUTHORIZATION, auth);
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| SessionEnd::Failed(format!("連不上服務：{e}")))?;
    let (mut sink, mut stream) = ws.split();

    sink.send(encode(&ClientCommand::Resume {
        last_seq: state.last_seq,
    })?)
    .await
    .map_err(|_| SessionEnd::Closed)?;
    if events
        .unbounded_send(ConnEvent::State(ConnectionState::Connected))
        .is_err()
    {
        return Ok(());
    }
    let replay = state.take_replay();
    for (i, cmd) in replay.iter().enumerate() {
        if sink.send(encode(cmd)?).await.is_err() {
            state.pending.extend(replay.into_iter().skip(i));
            return Err(SessionEnd::Closed);
        }
        state.sent(cmd);
    }

    loop {
        tokio::select! {
            msg = stream.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Err(SessionEnd::Closed),
                    Some(Ok(_)) => continue,
                };
                let event: ServerEvent = match serde_json::from_str(&text) {
                    Ok(event) => event,
                    Err(e) => {
                        tracing::warn!(error = %e, "看不懂的伺服器事件");
                        if events
                            .unbounded_send(ConnEvent::Problem(
                                "收到看不懂的事件：app 和服務的版本可能不同，請重新開啟 app。".into(),
                            ))
                            .is_err()
                        {
                            return Ok(());
                        }
                        continue;
                    }
                };
                state.ack(&event);
                if let Some(seq) = event.seq() {
                    // 重播與即時事件可能重疊：已經看過的序號直接略過（快照除外）。
                    if seq <= state.last_seq && !matches!(event, ServerEvent::Snapshot { .. }) {
                        continue;
                    }
                    state.last_seq = seq;
                }
                if events.unbounded_send(ConnEvent::Server(Box::new(event))).is_err() {
                    return Ok(());
                }
            }
            cmd = commands.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                if sink.send(encode(&cmd)?).await.is_err() {
                    state.pending.push(cmd);
                    return Err(SessionEnd::Closed);
                }
                state.sent(&cmd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn send(id: ItemId) -> ClientCommand {
        ClientCommand::Send {
            project: ProjectId::from_u64(1),
            id,
            text: "hi".into(),
            mode: SendMode::Queue,
        }
    }

    #[test]
    fn unacked_sends_are_replayed_until_echoed() {
        let (a, b) = (ItemId::from_u64(10), ItemId::from_u64(11));
        let mut state = State::default();
        state.sent(&send(a));
        state.sent(&send(b));
        state.sent(&ClientCommand::Stop {
            project: ProjectId::from_u64(1),
        });
        assert_eq!(state.unacked.len(), 2, "只追蹤 Send");

        let echo = StreamItemView {
            id: a,
            at: Timestamp(0),
            kind: StreamItemKind::Event {
                text: String::new(),
            },
        };
        state.ack(&ServerEvent::Item {
            seq: 1,
            scope: StreamScope::Project(ProjectId::from_u64(1)),
            item: echo.clone(),
        });
        assert_eq!(state.unacked.len(), 1);

        // 斷線：沒確認的 b 排在斷線期間的指令前面重送。
        state.pending.push(ClientCommand::StartModelLogin);
        let replay = state.take_replay();
        assert!(matches!(&replay[0], ClientCommand::Send { id, .. } if *id == b));
        assert!(matches!(replay[1], ClientCommand::StartModelLogin));
        for cmd in &replay {
            state.sent(cmd);
        }

        state.ack(&ServerEvent::Snapshot {
            seq: 2,
            instance: Box::new(InstanceSnapshot {
                id: InstanceId::from_u64(1),
                name: String::new(),
                kind: InstanceKind::Desktop,
                projects: vec![ProjectSnapshot {
                    id: ProjectId::from_u64(1),
                    name: String::new(),
                    agent_name: String::new(),
                    agent_state: AgentState::Idle,
                    items: vec![StreamItemView { id: b, ..echo }],
                    memory: MemoryView::default(),
                    goals: vec![],
                    artifacts: vec![],
                    repos: vec![],
                    workdir: None,
                    merge_method: Default::default(),
                }],
                model_auth: ModelAuthView::SignedOut,
                providers: ProvidersView::default(),
                automation: Default::default(),
                github: Default::default(),
                worker: Default::default(),
                approvals: vec![],
                notifications: Default::default(),
                user_md: Default::default(),
                overseer: Default::default(),
            }),
        });
        assert!(state.unacked.is_empty());
    }
}
