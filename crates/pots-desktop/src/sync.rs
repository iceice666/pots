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
    /// provider 設定有變（`Providers` 事件或快照）。
    pub providers_changed: bool,
    /// `TestProvider` 的結果：請求編號與可用模型或錯誤。
    pub provider_test: Option<(u64, Result<Vec<ModelOption>, String>)>,
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
        providers: ProvidersView::default(),
        model_auth: ModelAuthView::SignedOut,
    }
}

/// 「設定 provider／登入 ChatGPT」在 Inbox 裡的那一筆用固定 ID：直接沿用實例的 UUID。
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
                view.providers = snap.providers;
                view.model_auth = snap.model_auth;
                out.providers_changed = true;
                if new_id != instance {
                    for item in store.inbox.iter_mut().filter(|i| i.instance == instance) {
                        item.instance = new_id;
                    }
                    let old_login = model_login_item_id(instance);
                    store.inbox.retain(|i| i.id != old_login);
                    out.rekeyed = Some((instance, new_id));
                    instance = new_id;
                }
                refresh_setup_prompt(store, instance);
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
                if let Some(view) = store.instance_mut(instance) {
                    view.model_auth = state;
                }
                refresh_setup_prompt(store, instance);
            }
            ServerEvent::Providers { providers, .. } => {
                if let Some(view) = store.instance_mut(instance) {
                    view.providers = providers;
                }
                out.providers_changed = true;
                refresh_setup_prompt(store, instance);
            }
            ServerEvent::ProviderTest { request, result } => {
                out.provider_test = Some((request, result));
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
}

/// 還不能和 agent 對話時，在 Inbox 放一筆「需要你處理」：沒有使用中的 provider 就請你去
/// 設定；用 ChatGPT 但還沒登入就請你登入。可以對話後收掉。
pub fn refresh_setup_prompt(store: &mut Store, instance: InstanceId) {
    let Some(view) = store.instance(instance) else {
        return;
    };
    let id = model_login_item_id(instance);
    let active = view.providers.active().map(|p| p.kind);
    let (title, detail, actions): (&str, String, Vec<InboxAction>) = match (active, &view.model_auth)
    {
        (None, _) => (
            "設定模型 provider 才能和 agent 對話",
            "到「設定」新增 OpenAI 或 Anthropic 相容的 API，或改用 ChatGPT 方案登入。".into(),
            vec![InboxAction::OpenSettings],
        ),
        (Some(ProviderKind::ChatGpt), ModelAuthView::SignedOut) => (
            "登入 ChatGPT 才能和 agent 對話",
            "使用中的 provider 是 ChatGPT 方案。按「開始登入」會開瀏覽器，用 ChatGPT 帳號授權這個實例；也可以到「設定」改用 API。".into(),
            vec![InboxAction::StartLogin, InboxAction::OpenSettings],
        ),
        // 網址只經由 `LoginUrl` 回給按下登入的那條連線，這裡不開瀏覽器。
        (Some(ProviderKind::ChatGpt), ModelAuthView::Pending { .. }) => (
            "在瀏覽器完成 ChatGPT 登入",
            "已開啟瀏覽器；完成授權後這筆會自動收掉。瀏覽器沒有開啟的話，再按一次「開始登入」。".into(),
            vec![InboxAction::StartLogin],
        ),
        (Some(ProviderKind::ChatGpt), ModelAuthView::Error { message }) => (
            "ChatGPT 授權需要重新登入",
            message.clone(),
            vec![InboxAction::StartLogin, InboxAction::OpenSettings],
        ),
        // 使用中的 API provider 還缺東西，送出訊息一定會失敗。
        (Some(kind), _) if incomplete(view.providers.active(), kind).is_some() => (
            "完成模型 provider 設定",
            incomplete(view.providers.active(), kind)
                .unwrap_or_default()
                .to_string(),
            vec![InboxAction::OpenSettings],
        ),
        (Some(_), _) => {
            if let Some(item) = store.inbox.iter_mut().find(|i| i.id == id && i.is_open()) {
                item.decision = Decision::Resolved("模型 provider 已就緒".into());
            }
            return;
        }
    };
    let item = InboxItem {
        id,
        instance,
        project: None,
        context: String::new(),
        kind: InboxKind::NeedsYou,
        title: title.into(),
        detail,
        facts: vec![],
        actions,
        at: store.now,
        expiry: None,
        decision: Decision::Pending,
    };
    match store.inbox.iter_mut().find(|i| i.id == id) {
        Some(existing) => *existing = item,
        None => store.inbox.insert(0, item),
    }
}

/// 使用中的 API provider 缺了什麼（沒選模型、Anthropic 沒有 key）；齊全時是 `None`。
fn incomplete(provider: Option<&ProviderView>, kind: ProviderKind) -> Option<&'static str> {
    let provider = provider?;
    if kind == ProviderKind::ChatGpt {
        return None;
    }
    if provider.model.trim().is_empty() {
        return Some("使用中的 provider 還沒選模型。到「設定」編輯它，用「測試連線」挑一個模型。");
    }
    if kind == ProviderKind::AnthropicCompatible && provider.key_hint.is_none() {
        return Some("使用中的 Anthropic 相容 API 沒有 API key。到「設定」編輯它並填入 key。");
    }
    None
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
                providers: chatgpt_active(),
            },
        }
    }

    fn chatgpt_active() -> ProvidersView {
        ProvidersView {
            active: Some(ProviderId::CHATGPT),
            items: vec![ProviderView {
                id: ProviderId::CHATGPT,
                kind: ProviderKind::ChatGpt,
                name: "ChatGPT".into(),
                base_url: String::new(),
                model: String::new(),
                key_hint: None,
            }],
        }
    }

    #[test]
    fn setup_prompt_follows_active_provider() {
        let inst = InstanceId::from_u64(1);
        let mut store = empty_store(inst);
        let sync = Sync::new(0);
        let mut snap = snapshot(inst, ProjectId::from_u64(2), ModelAuthView::SignedOut);
        if let ServerEvent::Snapshot { instance, .. } = &mut snap {
            instance.providers.active = None;
        }
        sync.apply(&mut store, inst, snap);
        let item = store.approval(model_login_item_id(inst)).unwrap();
        assert_eq!(item.actions, vec![InboxAction::OpenSettings]);

        // 新增 API provider 並設為使用中：不用登入 ChatGPT，提示收掉。
        let mut providers = chatgpt_active();
        let api = ProviderId::from_u64(5);
        providers.items.push(ProviderView {
            id: api,
            kind: ProviderKind::OpenAiCompatible,
            name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4.1".into(),
            key_hint: Some("…abcd".into()),
        });
        providers.active = Some(api);
        sync.apply(
            &mut store,
            inst,
            ServerEvent::Providers {
                seq: 4,
                providers: providers.clone(),
            },
        );
        assert_eq!(store.pending(Some(inst)), 0);

        // 沒選模型：提示補上。
        let mut no_model = providers.clone();
        no_model.items[1].model.clear();
        sync.apply(
            &mut store,
            inst,
            ServerEvent::Providers {
                seq: 40,
                providers: no_model,
            },
        );
        let item = store.approval(model_login_item_id(inst)).unwrap();
        assert!(item.is_open());
        assert_eq!(item.actions, vec![InboxAction::OpenSettings]);
        sync.apply(
            &mut store,
            inst,
            ServerEvent::Providers {
                seq: 41,
                providers: providers.clone(),
            },
        );
        assert_eq!(store.pending(Some(inst)), 0);
        assert_eq!(store.instance(inst).unwrap().providers.active, Some(api));

        // 改回 ChatGPT，但還沒登入：要求登入。
        providers.active = Some(ProviderId::CHATGPT);
        sync.apply(
            &mut store,
            inst,
            ServerEvent::Providers { seq: 5, providers },
        );
        let item = store.approval(model_login_item_id(inst)).unwrap();
        assert!(item.is_open());
        assert_eq!(item.actions[0], InboxAction::StartLogin);

        let out = sync.apply(
            &mut store,
            inst,
            ServerEvent::ProviderTest {
                request: 7,
                result: Err("連不上".into()),
            },
        );
        assert_eq!(out.provider_test, Some((7, Err("連不上".into()))));
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
