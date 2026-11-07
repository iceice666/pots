//! 把伺服器推送的 [`ServerEvent`] 套用到客戶端的 [`Store`]。
//!
//! 這裡只處理資料，不碰 UI：回傳的 [`Applied`] 告訴 app 要不要換實例 ID、顯示提示或
//! 開瀏覽器。伺服器的時間是 UTC 的 Unix 秒；`Store` 裡的 [`Timestamp`] 只拿來顯示、
//! 不做時區換算（見 `Timestamp::hh_mm`），所以進來時先加上本地時區的位移。

use crate::model::*;

/// 套用一個事件後，app 需要做的事。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// 收到 `Snapshot` 時，本地暫用的實例 ID 換成伺服器的。
    pub rekeyed: Option<(InstanceId, InstanceId)>,
    pub toast: Option<String>,
    /// 伺服器給了登入網址，交給系統瀏覽器開啟。
    pub login_url: Option<String>,
    /// 新增的 project（例如自己剛建立的），app 可以切過去。
    pub project_added: Option<ProjectId>,
}

/// 本地時區相對 UTC 的秒數。
pub fn local_offset_secs() -> i64 {
    i64::from(chrono::Local::now().offset().local_minus_utc())
}

/// 連線中、還沒收到快照時顯示的實例。
pub fn placeholder_instance(id: InstanceId, name: &str) -> InstanceView {
    InstanceView {
        id,
        name: name.into(),
        kind: InstanceKind::Desktop,
        host: "連線中…".into(),
        connection: ConnectionState::Reconnecting,
        last_seq: 0,
        projects: vec![],
        overseer: OverseerView {
            state: AgentState::Idle,
            items: vec![],
            overview: vec![],
            memory: vec![],
            memory_total: 0,
        },
        schedules: vec![],
        listeners: vec![],
        automation_pause: None,
    }
}

/// 模型登入狀態在 Inbox 裡的那一筆用固定 ID：直接沿用實例的 UUID。
pub fn model_login_item_id(instance: InstanceId) -> ApprovalId {
    ApprovalId(instance.0)
}

pub struct Sync {
    /// 本地時區位移（秒）。
    pub offset: i64,
}

impl Sync {
    pub fn new(offset: i64) -> Self {
        Self { offset }
    }

    fn local(&self, at: Timestamp) -> Timestamp {
        Timestamp(at.0 + self.offset)
    }

    fn localize(&self, mut item: StreamItemView) -> StreamItemView {
        item.at = self.local(item.at);
        item
    }

    fn project_view(&self, p: ProjectSnapshot) -> ProjectView {
        ProjectView {
            id: p.id,
            name: p.name,
            agent_name: p.agent_name,
            agent_state: p.agent_state,
            budget: Budget {
                spent_cents: 0,
                limit_cents: 0,
            },
            unread: 0,
            items: p.items.into_iter().map(|i| self.localize(i)).collect(),
            goals: vec![],
            artifacts: vec![],
        }
    }

    /// 套用一個事件。`instance` 是這條連線在 `Store` 裡目前的實例 ID。
    pub fn apply(&self, store: &mut Store, instance: InstanceId, event: ServerEvent) -> Applied {
        let mut out = Applied::default();
        let seq = event.seq();
        let mut instance = instance;
        match event {
            ServerEvent::Snapshot { instance: snap, .. } => {
                let new_id = snap.id;
                let projects = snap
                    .projects
                    .into_iter()
                    .map(|p| self.project_view(p))
                    .collect();
                let view = match store.instance_mut(instance) {
                    Some(view) => view,
                    None => {
                        store.instances.push(placeholder_instance(instance, ""));
                        store.instances.last_mut().expect("剛加入")
                    }
                };
                view.id = new_id;
                view.name = snap.name;
                view.kind = snap.kind;
                view.host = "這台電腦".into();
                view.connection = ConnectionState::Connected;
                view.projects = projects;
                if new_id != instance {
                    for item in store.inbox.iter_mut().filter(|i| i.instance == instance) {
                        item.instance = new_id;
                    }
                    let old_login = model_login_item_id(instance);
                    store.inbox.retain(|i| i.id != old_login);
                    out.rekeyed = Some((instance, new_id));
                    instance = new_id;
                }
                self.set_model_auth(store, instance, &snap.model_auth);
            }
            ServerEvent::ProjectAdded { project, .. } => {
                if let Some(view) = store.instance_mut(instance)
                    && !view.projects.iter().any(|p| p.id == project.id)
                {
                    out.project_added = Some(project.id);
                    view.projects.push(self.project_view(project));
                }
            }
            ServerEvent::Item { scope, item, .. } => {
                let item = self.localize(item);
                if let StreamScope::Project(p) = scope
                    && let Some(project) = store.project_mut(instance, p)
                {
                    // 新的 agent 訊息開始時，排隊中的訊息已經被這一輪接手。
                    if matches!(item.kind, StreamItemKind::Agent(_)) {
                        for i in &mut project.items {
                            if let StreamItemKind::Human(m) = &mut i.kind
                                && m.status == MsgStatus::Queued
                            {
                                m.status = MsgStatus::Done;
                            }
                        }
                    }
                    upsert(&mut project.items, item, true);
                }
            }
            ServerEvent::Delta {
                scope,
                item_id,
                text,
            } => {
                if let Some(msg) = find_msg(store, instance, scope, item_id) {
                    msg.text.push_str(&text);
                }
            }
            ServerEvent::StreamingText {
                scope,
                item_id,
                text,
            } => {
                if let Some(msg) = find_msg(store, instance, scope, item_id) {
                    msg.text = text;
                }
            }
            ServerEvent::ItemUpdated { scope, item, .. }
            | ServerEvent::ItemDone { scope, item, .. } => {
                let item = self.localize(item);
                if let StreamScope::Project(p) = scope
                    && let Some(project) = store.project_mut(instance, p)
                {
                    upsert(&mut project.items, item, false);
                }
            }
            ServerEvent::AgentState { project, state, .. } => {
                if let Some(p) = store.project_mut(instance, project) {
                    p.agent_state = state;
                    // 沒在跑了：排隊中的訊息已經被處理或被「停止」清掉。
                    if !state.is_busy() {
                        for i in &mut p.items {
                            if let StreamItemKind::Human(m) = &mut i.kind
                                && m.status == MsgStatus::Queued
                            {
                                m.status = MsgStatus::Done;
                            }
                        }
                    }
                }
            }
            ServerEvent::LoginUrl { url } => {
                if !url.is_empty() {
                    out.login_url = Some(url);
                }
            }
            ServerEvent::ModelAuth { state, .. } => {
                self.set_model_auth(store, instance, &state);
            }
            ServerEvent::Error { message } => out.toast = Some(message),
            // M0 還沒有任務樹與核准。
            ServerEvent::TaskChanged { .. } | ServerEvent::Inbox { .. } => {}
        }
        if let Some(seq) = seq
            && let Some(view) = store.instance_mut(instance)
        {
            view.last_seq = view.last_seq.max(seq);
        }
        out
    }

    /// 模型沒登入或授權失效時，在 Inbox 放一筆「需要你處理」；登入後收掉。
    fn set_model_auth(&self, store: &mut Store, instance: InstanceId, state: &ModelAuthView) {
        let id = model_login_item_id(instance);
        let (title, detail) = match state {
            ModelAuthView::SignedIn { .. } => {
                if let Some(item) = store.inbox.iter_mut().find(|i| i.id == id) {
                    item.decision = Decision::Resolved("已登入 ChatGPT".into());
                }
                return;
            }
            ModelAuthView::SignedOut => (
                "登入 ChatGPT 才能和 agent 對話",
                "Pots 用你的 ChatGPT 方案額度推論。按「開始登入」會開瀏覽器，用 ChatGPT 帳號授權這個實例。",
            ),
            // 網址只經由 `LoginUrl` 回給按下登入的那條連線，這裡不開瀏覽器。
            ModelAuthView::Pending { .. } => (
                "在瀏覽器完成 ChatGPT 登入",
                "已開啟瀏覽器；完成授權後這筆會自動收掉。瀏覽器沒有開啟的話，再按一次「開始登入」。",
            ),
            ModelAuthView::Error { message } => ("ChatGPT 授權需要重新登入", message.as_str()),
        };
        let item = InboxItem {
            id,
            instance,
            project: None,
            context: String::new(),
            kind: InboxKind::NeedsYou,
            title: title.into(),
            detail: detail.into(),
            facts: vec![],
            actions: vec![InboxAction::StartLogin],
            at: store.now,
            expiry: None,
            decision: Decision::Pending,
        };
        match store.inbox.iter_mut().find(|i| i.id == id) {
            Some(existing) => *existing = item,
            None => store.inbox.insert(0, item),
        }
    }
}

/// 有同 ID 就替換，沒有就加到尾端。`keep_queued` 時保留客戶端標的「排隊中」。
fn upsert(items: &mut Vec<StreamItemView>, item: StreamItemView, keep_queued: bool) {
    match items.iter_mut().find(|i| i.id == item.id) {
        Some(existing) => {
            let queued =
                matches!(&existing.kind, StreamItemKind::Human(m) if m.status == MsgStatus::Queued);
            *existing = item;
            if keep_queued
                && queued
                && let StreamItemKind::Human(m) = &mut existing.kind
            {
                m.status = MsgStatus::Queued;
            }
        }
        None => items.push(item),
    }
}

fn find_msg(
    store: &mut Store,
    instance: InstanceId,
    scope: StreamScope,
    item: ItemId,
) -> Option<&mut Msg> {
    let StreamScope::Project(p) = scope else {
        return None;
    };
    store
        .project_mut(instance, p)?
        .items
        .iter_mut()
        .find(|i| i.id == item)
        .and_then(|i| match &mut i.kind {
            StreamItemKind::Agent(m) | StreamItemKind::Human(m) => Some(m),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_store(instance: InstanceId) -> Store {
        Store {
            now: Timestamp(0),
            instances: vec![placeholder_instance(instance, "本機")],
            inbox: vec![],
            outbox: vec![],
        }
    }

    fn agent_item(id: ItemId, text: &str, status: MsgStatus) -> StreamItemView {
        StreamItemView {
            id,
            at: Timestamp(100),
            kind: StreamItemKind::Agent(Msg {
                author: Author::Agent {
                    label: "Pots".into(),
                    role: AgentRole::Project,
                },
                text: text.into(),
                status,
                tools: None,
                cards: vec![],
                steer: false,
            }),
        }
    }

    fn snapshot(id: InstanceId, project: ProjectId, auth: ModelAuthView) -> ServerEvent {
        ServerEvent::Snapshot {
            seq: 3,
            instance: InstanceSnapshot {
                id,
                name: "本機".into(),
                kind: InstanceKind::Desktop,
                projects: vec![ProjectSnapshot {
                    id: project,
                    name: "demo".into(),
                    agent_name: "Pots".into(),
                    agent_state: AgentState::Idle,
                    items: vec![],
                }],
                model_auth: auth,
            },
        }
    }

    fn text_of(store: &Store, instance: InstanceId, project: ProjectId, item: ItemId) -> String {
        store
            .project(instance, project)
            .unwrap()
            .items
            .iter()
            .find(|i| i.id == item)
            .and_then(|i| match &i.kind {
                StreamItemKind::Agent(m) => Some(m.text.clone()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn snapshot_rekeys_and_shows_login() {
        let local = InstanceId::from_u64(1);
        let server = InstanceId::from_u64(9);
        let project = ProjectId::from_u64(2);
        let mut store = empty_store(local);
        let sync = Sync::new(0);

        let out = sync.apply(
            &mut store,
            local,
            snapshot(server, project, ModelAuthView::SignedOut),
        );
        assert_eq!(out.rekeyed, Some((local, server)));
        let inst = store.instance(server).unwrap();
        assert_eq!(inst.connection, ConnectionState::Connected);
        assert_eq!(inst.last_seq, 3);
        assert_eq!(inst.projects.len(), 1);
        assert_eq!(store.pending(Some(server)), 1);

        let out = sync.apply(
            &mut store,
            server,
            ServerEvent::ModelAuth {
                seq: 4,
                state: ModelAuthView::Pending { url: "u".into() },
            },
        );
        assert_eq!(out.login_url, None, "廣播的狀態不開瀏覽器");
        let out = sync.apply(
            &mut store,
            server,
            ServerEvent::LoginUrl { url: "u".into() },
        );
        assert_eq!(out.login_url.as_deref(), Some("u"));
        let out = sync.apply(
            &mut store,
            server,
            ServerEvent::LoginUrl { url: String::new() },
        );
        assert_eq!(out.login_url, None);
        sync.apply(
            &mut store,
            server,
            ServerEvent::ModelAuth {
                seq: 5,
                state: ModelAuthView::SignedIn { account: None },
            },
        );
        assert_eq!(store.pending(Some(server)), 0);
    }

    #[test]
    fn streaming_deltas_and_done() {
        let inst = InstanceId::from_u64(1);
        let project = ProjectId::from_u64(2);
        let item = ItemId::from_u64(3);
        let mut store = empty_store(inst);
        let sync = Sync::new(3600);
        sync.apply(
            &mut store,
            inst,
            snapshot(inst, project, ModelAuthView::SignedOut),
        );
        let scope = StreamScope::Project(project);

        sync.apply(
            &mut store,
            inst,
            ServerEvent::Item {
                seq: 4,
                scope,
                item: agent_item(item, "", MsgStatus::Streaming),
            },
        );
        for t in ["你", "好"] {
            sync.apply(
                &mut store,
                inst,
                ServerEvent::Delta {
                    scope,
                    item_id: item,
                    text: t.into(),
                },
            );
        }
        assert_eq!(text_of(&store, inst, project, item), "你好");

        // 重連時補送全文：取代而不是接在後面。
        sync.apply(
            &mut store,
            inst,
            ServerEvent::StreamingText {
                scope,
                item_id: item,
                text: "你好嗎".into(),
            },
        );
        assert_eq!(text_of(&store, inst, project, item), "你好嗎");

        sync.apply(
            &mut store,
            inst,
            ServerEvent::ItemDone {
                seq: 5,
                scope,
                item: agent_item(item, "你好嗎？", MsgStatus::Done),
            },
        );
        let p = store.project(inst, project).unwrap();
        assert_eq!(p.items.len(), 1);
        assert_eq!(p.items[0].at, Timestamp(100 + 3600));
        assert_eq!(text_of(&store, inst, project, item), "你好嗎？");
        assert_eq!(store.instance(inst).unwrap().last_seq, 5);
    }

    #[test]
    fn echo_replaces_local_message_and_keeps_queue_mark() {
        let inst = InstanceId::from_u64(1);
        let project = ProjectId::from_u64(2);
        let mut store = empty_store(inst);
        let sync = Sync::new(0);
        sync.apply(
            &mut store,
            inst,
            snapshot(inst, project, ModelAuthView::SignedOut),
        );
        store.project_mut(inst, project).unwrap().agent_state = AgentState::Thinking;
        store.send_to_project(inst, project, "嗨".into(), SendMode::Queue);
        let (_, ClientCommand::Send { id, .. }) = store.outbox[0].clone() else {
            panic!("應該送出 Send");
        };

        let mut echo = store.project(inst, project).unwrap().items[0].clone();
        if let StreamItemKind::Human(m) = &mut echo.kind {
            m.status = MsgStatus::Done;
        }
        let scope = StreamScope::Project(project);
        sync.apply(
            &mut store,
            inst,
            ServerEvent::Item {
                seq: 4,
                scope,
                item: echo,
            },
        );
        let items = &store.project(inst, project).unwrap().items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
        assert!(
            matches!(&items[0].kind, StreamItemKind::Human(m) if m.status == MsgStatus::Queued)
        );

        // 下一輪開始，排隊中的訊息被接手。
        sync.apply(
            &mut store,
            inst,
            ServerEvent::Item {
                seq: 5,
                scope,
                item: agent_item(ItemId::from_u64(7), "", MsgStatus::Streaming),
            },
        );
        let items = &store.project(inst, project).unwrap().items;
        assert!(matches!(&items[0].kind, StreamItemKind::Human(m) if m.status == MsgStatus::Done));

        // 「停止」清掉排隊：回到閒置時也不再標排隊中。
        store.project_mut(inst, project).unwrap().agent_state = AgentState::Thinking;
        store.send_to_project(inst, project, "再一則".into(), SendMode::Queue);
        sync.apply(
            &mut store,
            inst,
            ServerEvent::AgentState {
                seq: 6,
                project,
                state: AgentState::Idle,
            },
        );
        let items = &store.project(inst, project).unwrap().items;
        assert!(items.iter().all(
            |i| !matches!(&i.kind, StreamItemKind::Human(m) if m.status == MsgStatus::Queued)
        ));
    }
}
