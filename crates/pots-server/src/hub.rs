//! 事件日誌與廣播（`proposal/ux-and-frontend.md#即時協定`）。
//!
//! 所有帶 `seq` 的事件都經過 [`LogTxn`]：狀態變更與事件寫在同一個 transaction，
//! commit 之後才廣播，所以「先寫日誌再廣播」，日誌順序就是廣播順序。快照與補送也在
//! 同一把鎖下讀，客戶端拿到的狀態與之後收到的事件不會重疊或漏掉。
//!
//! `Delta` 不寫日誌，但要和「重連時補送的串流全文」對齊：每個串流中的訊息在記憶體裡
//! 留一份目前的全文，`Delta` 帶著串流位置廣播，連線端據此丟掉已經包含在
//! `StreamingText` 裡的增量。`ItemUpdated` 會整份換掉內文（例如一輪的話搬進旁白），
//! 所以位置是送過的總長度，不是目前全文的長度。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pots_proto::{
    AgentState, InstanceSnapshot, ItemId, ModelAuthView, ProjectId, ProjectSnapshot, ProvidersView,
    ServerEvent, StreamItemKind, StreamScope, Timestamp,
};
use sqlx::sqlite::SqliteConnection;
use tokio::sync::{MutexGuard, broadcast};

use crate::Error;
use crate::store::{Store, event_json};

/// 快照裡每個 project 帶的最近訊息數；更早的走 HTTP 分頁。
pub const SNAPSHOT_ITEMS: u32 = 100;

/// 廣播佇列長度。連線跟不上（lag）就斷線，讓客戶端用 `Resume` 重連。
const BROADCAST_CAPACITY: usize = 4096;

/// 廣播給所有連線的訊息。JSON 先序列化好，每條連線共用。
#[derive(Clone, Debug)]
pub enum Envelope {
    Logged {
        seq: u64,
        json: Arc<str>,
        /// `ItemDone` 的 item，連線端據此清掉串流位置。
        done: Option<ItemId>,
    },
    Delta {
        item: ItemId,
        /// 套用這個增量後的串流位置（送過的總 bytes，內文被換掉也不會變小）。
        end: usize,
        json: Arc<str>,
    },
}

/// 實例層級、不在資料庫裡的即時狀態。只在持有 [`Hub`] 的鎖時讀寫。
#[derive(Debug)]
pub struct HubState {
    head: u64,
    pub model_auth: ModelAuthView,
    /// 最後一筆 `Providers` 事件的內容，快照用。
    pub providers: ProvidersView,
    /// 最後一筆 `Github` 事件的內容，快照用。
    github: pots_proto::GithubView,
    /// 最後一筆 `Worker` 事件的內容，快照用。
    worker: pots_proto::WorkerSettingsView,
    agent_states: HashMap<ProjectId, AgentState>,
}

impl HubState {
    pub fn agent_state(&self, project: ProjectId) -> AgentState {
        self.agent_states
            .get(&project)
            .copied()
            .unwrap_or(AgentState::Idle)
    }
}

struct Streaming {
    scope: StreamScope,
    text: String,
    /// 串流位置：送過的增量總長度（bytes），只增不減。
    sent: usize,
}

/// 連線開始時要送的內容。
pub struct Initial {
    pub frames: Vec<String>,
    /// 已經用 `StreamingText` 補送到哪個長度。
    pub stream_pos: HashMap<ItemId, usize>,
}

/// 實例資訊，快照用。
#[derive(Clone, Debug)]
pub struct InstanceInfo {
    pub id: pots_proto::InstanceId,
    pub name: String,
    pub kind: pots_proto::InstanceKind,
}

pub struct Hub {
    store: Store,
    instance: InstanceInfo,
    tx: broadcast::Sender<Envelope>,
    state: tokio::sync::Mutex<HubState>,
    // 鎖順序：先 `state` 再 `streaming`。
    streaming: Mutex<HashMap<ItemId, Streaming>>,
}

impl Hub {
    pub async fn new(
        store: Store,
        instance: InstanceInfo,
        model_auth: ModelAuthView,
        providers: ProvidersView,
    ) -> Result<Self, Error> {
        let head = store.event_bounds().await?.map_or(0, |(_, hi)| hi);
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Self {
            store,
            instance,
            tx,
            state: tokio::sync::Mutex::new(HubState {
                head,
                model_auth,
                providers,
                github: Default::default(),
                worker: Default::default(),
                agent_states: HashMap::new(),
            }),
            streaming: Mutex::new(HashMap::new()),
        })
    }

    pub fn instance(&self) -> &InstanceInfo {
        &self.instance
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Envelope> {
        self.tx.subscribe()
    }

    /// 開始一筆「狀態變更加事件」。持有期間其他寫入與新連線都會等待，要盡快 commit。
    pub async fn begin(&self) -> Result<LogTxn<'_>, Error> {
        let state = self.state.lock().await;
        let tx = self.store.begin().await?;
        Ok(LogTxn {
            hub: self,
            state,
            tx,
            pending: Vec::new(),
        })
    }

    /// 串流增量。訊息不在串流中（已結束）就丟掉。
    pub fn delta(&self, item: ItemId, text: &str) {
        let mut streaming = self.streaming.lock().unwrap();
        let Some(s) = streaming.get_mut(&item) else {
            return;
        };
        s.text.push_str(text);
        s.sent += text.len();
        let ev = ServerEvent::Delta {
            scope: s.scope,
            item_id: item,
            text: text.to_string(),
        };
        let Ok(json) = event_json(&ev) else { return };
        // 沒有任何連線時 send 會失敗，不要緊。
        let _ = self.tx.send(Envelope::Delta {
            item,
            end: s.sent,
            json: json.into(),
        });
    }

    /// 連線的第一步：決定送快照或補送事件，再補送串流中訊息的全文。
    /// 呼叫前要先 [`subscribe`](Self::subscribe)，之後廣播收到的事件依 `seq` 去重。
    pub async fn initial(&self, last_seq: u64) -> Result<(Initial, u64), Error> {
        let state = self.state.lock().await;
        let head = state.head;
        let bounds = self.store.event_bounds().await?;
        let retained = match bounds {
            // 日誌從 lo 開始，last_seq 之後的第一筆必須還在。
            Some((lo, _)) => last_seq + 1 >= lo,
            None => last_seq == head,
        };
        let mut frames = Vec::new();
        if last_seq == 0 || last_seq > head || !retained {
            let snapshot = ServerEvent::Snapshot {
                seq: head,
                instance: Box::new(self.snapshot(&state).await?),
            };
            frames.push(event_json(&snapshot)?);
        } else {
            for (_, json) in self.store.events_after(last_seq).await? {
                frames.push(json);
            }
        }

        let mut stream_pos = HashMap::new();
        let streaming = self.streaming.lock().unwrap();
        for (item, s) in streaming.iter() {
            let ev = ServerEvent::StreamingText {
                scope: s.scope,
                item_id: *item,
                text: s.text.clone(),
            };
            frames.push(event_json(&ev)?);
            stream_pos.insert(*item, s.sent);
        }
        Ok((Initial { frames, stream_pos }, head))
    }

    async fn snapshot(&self, state: &HubState) -> Result<InstanceSnapshot, Error> {
        let mut projects = Vec::new();
        for p in self.store.list_projects().await? {
            let (memory, goals, artifacts, repos, merge_method) = {
                let mut conn = self.store.acquire().await?;
                let memory = crate::memory::view(&mut conn, p.id).await?;
                let goals = crate::tasks::project_goals(&mut conn, p.id).await?;
                let artifacts = crate::artifacts::list(&mut conn, p.id)
                    .await?
                    .iter()
                    .map(|a| a.view())
                    .collect();
                let repos = crate::automation::project_repos(&mut conn, p.id).await?;
                let merge = crate::approvals::project_merge_method(&mut conn, p.id).await?;
                (memory, goals, artifacts, repos, merge)
            };
            projects.push(ProjectSnapshot {
                id: p.id,
                name: p.name,
                agent_name: p.agent_name,
                agent_state: state.agent_state(p.id),
                items: self.store.latest_items(p.id, SNAPSHOT_ITEMS).await?,
                memory,
                goals,
                artifacts,
                repos,
                workdir: p.workdir.clone(),
                merge_method,
            });
        }
        let (automation, approvals, notifications, user_md) = {
            let mut conn = self.store.acquire().await?;
            (
                crate::automation::view(&mut conn, Timestamp::now()).await?,
                crate::approvals::recent(&mut conn, Timestamp::now()).await?,
                crate::notifications::load(&mut conn).await?,
                crate::user_md::current(&mut conn).await?,
            )
        };
        let overseer = self.overseer(state).await?;
        Ok(InstanceSnapshot {
            id: self.instance.id,
            name: self.instance.name.clone(),
            kind: self.instance.kind,
            projects,
            model_auth: state.model_auth.clone(),
            providers: state.providers.clone(),
            automation,
            github: state.github.clone(),
            worker: state.worker.clone(),
            approvals,
            notifications,
            user_md,
            overseer,
        })
    }

    /// 總管的快照：全域對話、各 project 概況與自己的記憶。
    async fn overseer(&self, state: &HubState) -> Result<pots_proto::OverseerView, Error> {
        let project = crate::store::OVERSIGHT_PROJECT_ID;
        let mut conn = self.store.acquire().await?;
        let memory = crate::memory::view(&mut conn, project).await?;
        let mut overview = Vec::new();
        for p in self.store.list_projects().await? {
            let goals = crate::tasks::project_goals(&mut conn, p.id).await?;
            let note = goals
                .iter()
                .find(|g| matches!(g.state, pots_proto::GoalState::Active))
                .map(|g| g.planner_note.clone())
                .unwrap_or_else(|| "沒有進行中的 Goal".into());
            overview.push(pots_proto::ProjectOverview {
                project: p.name,
                state: state.agent_state(p.id),
                note,
            });
        }
        drop(conn);
        Ok(pots_proto::OverseerView {
            state: state.agent_state(project),
            items: self.store.latest_items(project, SNAPSHOT_ITEMS).await?,
            overview,
            memory: memory
                .observations
                .iter()
                .map(|o| pots_proto::Observation {
                    priority: o.priority,
                    text: o.text.clone(),
                })
                .collect(),
            memory_total: memory.observations.len() as u32,
        })
    }

    /// 寫不進日誌時的最後手段：只在記憶體裡結束串流、把 agent 狀態設回閒置，
    /// 讓之後的快照正確（已連線的客戶端要等重連才會看到）。
    pub async fn abandon(&self, project: ProjectId, item: ItemId) {
        let mut state = self.state.lock().await;
        state.agent_states.insert(project, AgentState::Idle);
        self.streaming.lock().unwrap().remove(&item);
    }

    /// 模型授權狀態有變時寫一筆 `ModelAuth`；`force` 時即使相同也寫（啟動時用）。
    pub async fn set_model_auth(&self, view: ModelAuthView, force: bool) -> Result<(), Error> {
        let mut txn = self.begin().await?;
        if force || txn.state().model_auth != view {
            txn.push(|seq| ServerEvent::ModelAuth { seq, state: view })
                .await?;
        }
        txn.commit().await
    }
}

/// 持有日誌鎖的 transaction。用 [`push`](Self::push) 加事件，[`commit`](Self::commit) 後才廣播。
pub struct LogTxn<'a> {
    hub: &'a Hub,
    state: MutexGuard<'a, HubState>,
    tx: sqlx::Transaction<'static, sqlx::Sqlite>,
    pending: Vec<(u64, String, ServerEvent)>,
}

impl LogTxn<'_> {
    pub fn conn(&mut self) -> &mut SqliteConnection {
        &mut self.tx
    }

    pub fn state(&self) -> &HubState {
        &self.state
    }

    /// 配一個序號、寫進事件日誌。`make` 收到的是這個事件的 `seq`。
    pub async fn push(&mut self, make: impl FnOnce(u64) -> ServerEvent) -> Result<(), Error> {
        let seq = self.state.head + self.pending.len() as u64 + 1;
        let ev = make(seq);
        debug_assert_eq!(ev.seq(), Some(seq));
        let json = event_json(&ev)?;
        let stored = match &ev {
            // 登入網址可能帶 id_token_hint，不落地：日誌與補送只留空網址，
            // 即時廣播與快照（記憶體）才有完整網址。
            ServerEvent::ModelAuth {
                state: ModelAuthView::Pending { .. },
                ..
            } => event_json(&ServerEvent::ModelAuth {
                seq,
                state: ModelAuthView::Pending { url: String::new() },
            })?,
            _ => json.clone(),
        };
        Store::insert_event(&mut self.tx, seq, &stored, Timestamp::now()).await?;
        self.pending.push((seq, json, ev));
        Ok(())
    }

    pub async fn commit(self) -> Result<(), Error> {
        self.commit_then(|| ()).await
    }

    /// commit 並廣播後、釋放日誌鎖前執行 `then`。用來讓後續動作（例如把訊息排進 run）
    /// 的順序和事件序號一致。
    pub async fn commit_then<T>(mut self, then: impl FnOnce() -> T) -> Result<T, Error> {
        self.tx.commit().await?;
        let mut streaming = self.hub.streaming.lock().unwrap();
        for (seq, json, ev) in self.pending {
            self.state.head = seq;
            let mut done = None;
            match &ev {
                ServerEvent::AgentState { project, state, .. } => {
                    self.state.agent_states.insert(*project, *state);
                }
                ServerEvent::ModelAuth { state, .. } => {
                    self.state.model_auth = state.clone();
                }
                ServerEvent::Providers { providers, .. } => {
                    self.state.providers = providers.clone();
                }
                ServerEvent::Worker { worker, .. } => {
                    self.state.worker = worker.clone();
                }
                ServerEvent::Github { github, .. } => {
                    self.state.github = github.clone();
                }
                ServerEvent::Item { scope, item, .. } => {
                    if let StreamItemKind::Agent(msg) = &item.kind
                        && msg.status == pots_proto::MsgStatus::Streaming
                    {
                        streaming.insert(
                            item.id,
                            Streaming {
                                scope: *scope,
                                text: msg.text.clone(),
                                sent: msg.text.len(),
                            },
                        );
                    }
                }
                // 串流中的訊息整份更新：客戶端會用它的內文取代，重連補送的全文也要跟著換；
                // 位置不變，之後的增量照樣接在後面。
                ServerEvent::ItemUpdated { item, .. } => {
                    if let StreamItemKind::Agent(msg) = &item.kind
                        && let Some(s) = streaming.get_mut(&item.id)
                    {
                        s.text.clone_from(&msg.text);
                    }
                }
                ServerEvent::ItemDone { item, .. } => {
                    streaming.remove(&item.id);
                    done = Some(item.id);
                }
                _ => {}
            }
            let _ = self.hub.tx.send(Envelope::Logged {
                seq,
                json: json.into(),
                done,
            });
        }
        drop(streaming);
        let out = then();
        drop(self.state);
        Ok(out)
    }
}
