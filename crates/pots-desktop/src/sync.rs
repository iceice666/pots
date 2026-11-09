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
    /// 這個 project 的記憶整份換了（`Memory` 事件）。
    pub memory_changed: Option<ProjectId>,
    /// 這份 artifact 新增或有變（`ArtifactChanged`）。
    pub artifact_changed: Option<ArtifactId>,
    /// 快照整份換掉了 artifact。
    pub artifacts_reset: bool,
    /// `ArtifactHistory` 的結果（時間已換成本地）。
    pub artifact_history: Option<(ArtifactId, Vec<ArtifactRevisionView>)>,
    /// GitHub 設定有變（`Github` 事件或快照）。
    pub github_changed: bool,
    /// `TestGithub` 的結果：請求編號與帳號或錯誤。
    pub github_test: Option<(u64, Result<String, String>)>,
    /// 排程、listener 或暫停有變（`Automation` 事件或快照）。
    pub automation_changed: bool,
    /// 套用後這個實例有幾個 listener。
    pub listener_count: usize,
    /// worker 設定有變（`Worker` 事件或快照）。
    pub worker_changed: bool,
    /// `Why` 的結果（「為什麼」面板）。
    pub why: Option<WhyView>,
    /// 這個 project 監看的 repo 換了（`ProjectRepos`）。
    pub repos_changed: Option<ProjectId>,
    /// 要不要彈桌面通知的事（新的核准、需要你處理的任務、Goal 完成）。快照不產生通知。
    pub notices: Vec<Notice>,
    /// 通知設定有變（`Notifications` 事件或快照）。
    pub notifications_changed: bool,
    /// USER.md 有變（`UserMd` 事件或快照）。
    pub user_md_changed: bool,
}

/// 一則可能要推播的事。點通知（或之後回到視窗）時開 `item`，沒有就開 project。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub instance: InstanceId,
    pub project: Option<ProjectId>,
    pub item: Option<ApprovalId>,
    pub title: String,
    pub body: String,
}

/// 「需要你處理」的任務在 Inbox 裡用任務的 UUID 當 ID。
pub fn task_item_id(task: TaskId) -> ApprovalId {
    ApprovalId(task.0)
}

/// 交給 Planner 決定的（worker 的 escalate、快速修改升級）不算需要你處理。
fn needs_you(state: &TaskStateView) -> Option<&str> {
    match state {
        TaskStateView::NeedsHuman { reason }
            if !reason.starts_with("worker 需要決定：") && !reason.starts_with("升級：") =>
        {
            Some(reason)
        }
        _ => None,
    }
}

/// 核准的狀態 → Inbox 與卡片上的決定。
fn approval_decision(a: &ApprovalView) -> Decision {
    let result = a.result.clone().unwrap_or_default();
    match a.state {
        ApprovalState::Pending => Decision::Pending,
        ApprovalState::Executing => Decision::Approved,
        ApprovalState::Rejected => Decision::Rejected,
        ApprovalState::Executed if result.is_empty() => Decision::Resolved("已核准".into()),
        ApprovalState::Executed => Decision::Resolved(format!("已核准 · {result}")),
        ApprovalState::Failed => Decision::Resolved(format!("核准了，但沒有執行：{result}")),
        ApprovalState::Expired => Decision::Resolved("已逾時，沒有決定".into()),
        ApprovalState::Cancelled => Decision::Resolved("已撤回，不再需要".into()),
    }
}

/// 核准卡片與 Inbox 上列出的細節。
fn approval_facts(a: &ApprovalView) -> Vec<String> {
    let mut facts = match &a.action {
        ApprovalActionView::MergePr {
            pr,
            sha,
            method,
            branch,
            base,
            ..
        } => vec![
            format!("PR {pr}"),
            format!("{branch} → {base}"),
            format!(
                "head {} · 用 {} merge",
                &sha[..sha.len().min(7)],
                method.label()
            ),
        ],
        ApprovalActionView::DeleteBranch { repo, branch } => {
            vec![repo.clone(), format!("刪除分支 {branch}")]
        }
        ApprovalActionView::CreateListener { summary, .. } => vec![summary.clone()],
        ApprovalActionView::EditUserMd { base_rev, .. } => {
            vec![format!("修改 USER.md（目前 v{base_rev}）")]
        }
        ApprovalActionView::EditProjectNotes { base_rev, .. } => {
            vec![format!("修改 Project Notes（目前 v{base_rev}）")]
        }
        ApprovalActionView::IncreaseBudget { tokens, .. } => {
            vec![format!("追加 {tokens} tokens")]
        }
        ApprovalActionView::ResumeProject { .. } => {
            vec!["預算熔斷：核准後恢復這個 project 的 run".to_string()]
        }
    };
    facts.push(format!("{}申請 · 動作 {}", a.requested_by, a.action_hash));
    if let Some(note) = &a.note {
        facts.push(format!("你的理由：{note}"));
    }
    facts
}

/// 已經換成本地時間的 `Timestamp` 顯示成 `11/08 09:00`。
pub fn month_day_time(at: Timestamp) -> String {
    chrono::DateTime::from_timestamp(at.0, 0)
        .map(|t| t.format("%m/%d %H:%M").to_string())
        .unwrap_or_default()
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
        pauses: vec![],
        sources: vec![],
        github: GithubView::default(),
        worker: WorkerSettingsView::default(),
        notifications: NotificationSettings::default(),
        user_md: UserMdView::default(),
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

    fn localize_goal(&self, mut goal: GoalView) -> GoalView {
        for t in &mut goal.tasks {
            for s in &mut t.steps {
                s.at = s.at.map(|at| self.local(at));
            }
            t.thread = std::mem::take(&mut t.thread)
                .into_iter()
                .map(|i| self.localize(i))
                .collect();
        }
        for post in &mut goal.team {
            post.at = self.local(post.at);
        }
        goal
    }

    fn localize_artifact(&self, mut artifact: ArtifactView) -> ArtifactView {
        for (_, at) in &mut artifact.human_edits {
            *at = self.local(*at);
        }
        artifact
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
            goals: p.goals.into_iter().map(|g| self.localize_goal(g)).collect(),
            artifacts: p
                .artifacts
                .into_iter()
                .map(|a| self.localize_artifact(a))
                .collect(),
            memory: p.memory,
            repos: p.repos,
            merge_method: p.merge_method,
        }
    }

    /// 服務的核准 → Inbox 項目。
    fn approval_item(&self, store: &Store, instance: InstanceId, a: ApprovalView) -> InboxItem {
        let goal = a
            .goal
            .and_then(|g| store.goal(instance, g))
            .map(|g| g.title.clone())
            .unwrap_or_default();
        let mut actions = vec![InboxAction::Approve, InboxAction::Reject];
        if matches!(&a.action, ApprovalActionView::MergePr { url: Some(_), .. }) {
            actions.push(InboxAction::ViewDiff);
        }
        actions.push(InboxAction::OpenThread);
        let left = a.expires_at.0 - Timestamp::now().0;
        let expiry = (a.is_pending() && left > 0).then(|| {
            if left >= 3600 {
                format!("{} 小時內有效", (left + 3599) / 3600)
            } else {
                format!("{} 分鐘內有效", ((left + 59) / 60).max(1))
            }
        });
        InboxItem {
            id: a.id,
            instance,
            project: Some(a.project),
            context: goal,
            kind: InboxKind::Approval,
            title: a.title.clone(),
            detail: a.reason.clone(),
            facts: approval_facts(&a),
            actions,
            at: self.local(a.created_at),
            expiry,
            decision: approval_decision(&a),
            task: a.task,
            approval: Some(a),
        }
    }

    /// 核准新增或有變：換掉 Inbox 那一筆。回傳是不是新的、要推播的核准。
    fn upsert_approval(&self, store: &mut Store, instance: InstanceId, a: ApprovalView) -> bool {
        let fresh = a.is_pending() && a.notify;
        let item = self.approval_item(store, instance, a);
        match store.inbox.iter_mut().find(|i| i.id == item.id) {
            Some(existing) => {
                *existing = item;
                false
            }
            None => {
                store.inbox.push(item);
                fresh
            }
        }
    }

    /// 依 Goal 的任務更新「需要你處理」的項目。回傳新出現的那些（推播用）。
    fn sync_task_items(
        &self,
        store: &mut Store,
        instance: InstanceId,
        project: ProjectId,
        goal: &GoalView,
    ) -> Vec<(ApprovalId, String, String)> {
        let mut fresh = Vec::new();
        for t in &goal.tasks {
            let id = task_item_id(t.id);
            let existing = store.inbox.iter_mut().find(|i| i.id == id);
            match (needs_you(&t.state), existing) {
                (Some(reason), Some(item)) => {
                    if !item.is_open() {
                        item.at = store.now;
                        fresh.push((id, t.title.clone(), reason.to_string()));
                    }
                    item.detail = reason.to_string();
                    item.decision = Decision::Pending;
                    item.context = goal.title.clone();
                }
                (Some(reason), None) => {
                    store.inbox.push(InboxItem {
                        id,
                        instance,
                        project: Some(project),
                        context: goal.title.clone(),
                        kind: InboxKind::NeedsYou,
                        title: format!("任務需要你處理：{}", t.title),
                        detail: reason.to_string(),
                        facts: vec![],
                        actions: vec![
                            InboxAction::Retry,
                            InboxAction::CancelTask,
                            InboxAction::OpenThread,
                        ],
                        at: store.now,
                        expiry: None,
                        decision: Decision::Pending,
                        task: Some(t.id),
                        approval: None,
                    });
                    fresh.push((id, t.title.clone(), reason.to_string()));
                }
                (None, Some(item)) if item.is_open() => {
                    item.decision =
                        Decision::Resolved(format!("已處理：{}", t.state.bucket().label()));
                }
                (None, _) => {}
            }
        }
        fresh
    }

    /// 自動化頁的資料整份換掉。排程與 listener 的顯示文字服務已經用本地時間排好；
    /// `next_at`、`expires_at` 留著 UTC，只拿來送指令。暫停與來源的時間換成本地。
    fn apply_automation(&self, view: &mut InstanceView, automation: AutomationView) {
        view.schedules = automation.schedules;
        view.listeners = automation.listeners;
        view.pauses = automation
            .pauses
            .into_iter()
            .map(|mut p| {
                p.until = p.until.map(|u| self.local(u));
                p.at = self.local(p.at);
                p
            })
            .collect();
        view.sources = automation
            .sources
            .into_iter()
            .map(|mut s| {
                s.last_ok = s.last_ok.map(|t| self.local(t));
                if let SourceHealth::Error { since, .. } = &mut s.health {
                    *since = self.local(*since);
                }
                s
            })
            .collect();
        view.automation_pause =
            view.pauses
                .iter()
                .find(|p| p.project.is_none())
                .map(|p| AutomationPause {
                    until: p
                        .until
                        .map_or_else(|| "手動恢復為止".to_string(), month_day_time),
                    until_short: p
                        .until
                        .map_or_else(|| "手動恢復".to_string(), |u| u.hh_mm()),
                    by: format!("由{}在 {} 暫停", p.by, month_day_time(p.at)),
                });
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
                view.github = snap.github;
                view.worker = snap.worker;
                view.notifications = snap.notifications;
                view.user_md = snap.user_md;
                view.overseer = snap.overseer;
                view.overseer.items = view
                    .overseer
                    .items
                    .drain(..)
                    .map(|item| self.localize(item))
                    .collect();
                self.apply_automation(view, snap.automation);
                out.notifications_changed = true;
                out.user_md_changed = true;
                out.providers_changed = true;
                out.github_changed = true;
                out.worker_changed = true;
                out.automation_changed = true;
                out.artifacts_reset = true;
                if new_id != instance {
                    for item in store.inbox.iter_mut().filter(|i| i.instance == instance) {
                        item.instance = new_id;
                    }
                    let old_login = model_login_item_id(instance);
                    store.inbox.retain(|i| i.id != old_login);
                    out.rekeyed = Some((instance, new_id));
                    instance = new_id;
                }
                // 核准與「需要你處理」的任務整份換掉（本機的登入提示留著）。
                let login = model_login_item_id(instance);
                store
                    .inbox
                    .retain(|i| i.instance != instance || i.id == login);
                for a in snap.approvals {
                    self.upsert_approval(store, instance, a);
                }
                let goals: Vec<(ProjectId, GoalView)> = store
                    .instance(instance)
                    .map(|v| {
                        v.projects
                            .iter()
                            .flat_map(|p| p.goals.iter().map(|g| (p.id, g.clone())))
                            .collect()
                    })
                    .unwrap_or_default();
                for (p, g) in &goals {
                    self.sync_task_items(store, instance, *p, g);
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
                } else if scope == StreamScope::Overseer {
                    if matches!(item.kind, StreamItemKind::Agent(_))
                        && let Some(view) = store.instance_mut(instance)
                    {
                        for i in &mut view.overseer.items {
                            if let StreamItemKind::Human(m) = &mut i.kind
                                && m.status == MsgStatus::Queued
                            {
                                m.status = MsgStatus::Done;
                            }
                        }
                    }
                    if let Some(view) = store.instance_mut(instance) {
                        upsert(&mut view.overseer.items, item, true);
                    }
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
                } else if scope == StreamScope::Overseer
                    && let Some(view) = store.instance_mut(instance)
                {
                    upsert(&mut view.overseer.items, item, false);
                }
            }
            ServerEvent::AgentState { project, state, .. } => {
                let known = store
                    .instance(instance)
                    .is_some_and(|v| v.projects.iter().any(|p| p.id == project));
                if !known && let Some(view) = store.instance_mut(instance) {
                    view.overseer.state = state;
                }
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
            ServerEvent::Memory {
                project, memory, ..
            } => {
                if let Some(p) = store.project_mut(instance, project) {
                    p.memory = memory;
                }
                out.memory_changed = Some(project);
            }
            ServerEvent::GoalChanged { project, goal, .. } => {
                let goal = self.localize_goal(goal);
                let was_active = store
                    .goal(instance, goal.id)
                    .is_some_and(|g| g.state.is_active());
                for (id, title, reason) in self.sync_task_items(store, instance, project, &goal) {
                    out.notices.push(Notice {
                        instance,
                        project: Some(project),
                        item: Some(id),
                        title: format!("需要你處理：{title}"),
                        body: reason,
                    });
                }
                if was_active && let GoalState::Done { summary } = &goal.state {
                    out.notices.push(Notice {
                        instance,
                        project: Some(project),
                        item: None,
                        title: format!("Goal 完成：{}", goal.title),
                        body: summary.clone(),
                    });
                }
                if let Some(p) = store.project_mut(instance, project) {
                    match p.goals.iter_mut().find(|g| g.id == goal.id) {
                        Some(g) => *g = goal,
                        None => p.goals.push(goal),
                    }
                }
            }
            ServerEvent::GoalArchived { project, goal, .. } => {
                if let Some(p) = store.project_mut(instance, project) {
                    p.goals.retain(|g| g.id != goal);
                }
            }
            ServerEvent::ApprovalChanged { approval, .. } => {
                let a = *approval;
                let (project, title, reason, id) =
                    (a.project, a.title.clone(), a.reason.clone(), a.id);
                if self.upsert_approval(store, instance, a) {
                    out.notices.push(Notice {
                        instance,
                        project: Some(project),
                        item: Some(id),
                        title: format!("等你核准：{title}"),
                        body: reason,
                    });
                }
            }
            ServerEvent::Notifications { settings, .. } => {
                if let Some(view) = store.instance_mut(instance) {
                    view.notifications = settings;
                }
                out.notifications_changed = true;
            }
            ServerEvent::UserMd { user_md, .. } => {
                if let Some(view) = store.instance_mut(instance) {
                    view.user_md = user_md;
                }
                out.user_md_changed = true;
            }
            ServerEvent::ProjectMergeMethod {
                project, method, ..
            } => {
                if let Some(p) = store.project_mut(instance, project) {
                    p.merge_method = method;
                }
            }
            ServerEvent::ArtifactChanged {
                project, artifact, ..
            } => {
                let artifact = self.localize_artifact(artifact);
                out.artifact_changed = Some(artifact.id);
                if let Some(p) = store.project_mut(instance, project) {
                    match p.artifacts.iter_mut().find(|a| a.id == artifact.id) {
                        Some(a) => *a = artifact,
                        None => p.artifacts.push(artifact),
                    }
                }
            }
            ServerEvent::ArtifactHistory {
                artifact,
                mut revisions,
            } => {
                for r in &mut revisions {
                    r.at = self.local(r.at);
                }
                out.artifact_history = Some((artifact, revisions));
            }
            ServerEvent::Automation { automation, .. } => {
                if let Some(view) = store.instance_mut(instance) {
                    self.apply_automation(view, automation);
                    out.listener_count = view.listeners.len();
                }
                out.automation_changed = true;
            }
            ServerEvent::Github { github, .. } => {
                if let Some(view) = store.instance_mut(instance) {
                    view.github = github;
                }
                out.github_changed = true;
            }
            ServerEvent::Worker { worker, .. } => {
                if let Some(view) = store.instance_mut(instance) {
                    view.worker = worker;
                }
                out.worker_changed = true;
            }
            ServerEvent::ProjectRepos { project, repos, .. } => {
                if let Some(p) = store.project_mut(instance, project) {
                    p.repos = repos;
                }
                out.repos_changed = Some(project);
            }
            ServerEvent::GithubTest { request, result } => {
                out.github_test = Some((request, result));
            }
            ServerEvent::Why { why } => out.why = why,
            // 成本報表與單價表是設定視窗的面板，桌面版下一輪才顯示；先不影響連線狀態。
            ServerEvent::CostReport { .. } | ServerEvent::Pricing { .. } => {}
            ServerEvent::Error { message } => out.toast = Some(message),
            // 任務的變化隨 `GoalChanged` 整個 Goal 送來。
            ServerEvent::TaskChanged { .. } => {}
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
        task: None,
        approval: None,
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
    let items = match scope {
        StreamScope::Project(p) => &mut store.project_mut(instance, p)?.items,
        StreamScope::Overseer => &mut store.instance_mut(instance)?.overseer.items,
        StreamScope::Task(_) => return None,
    };
    items
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
            instance: Box::new(InstanceSnapshot {
                id,
                name: "本機".into(),
                kind: InstanceKind::Desktop,
                projects: vec![ProjectSnapshot {
                    id: project,
                    name: "demo".into(),
                    agent_name: "Pots".into(),
                    agent_state: AgentState::Idle,
                    items: vec![],
                    memory: MemoryView::default(),
                    goals: vec![],
                    artifacts: vec![],
                    repos: vec![],
                    merge_method: Default::default(),
                }],
                model_auth: auth,
                providers: chatgpt_active(),
                automation: Default::default(),
                github: Default::default(),
                worker: Default::default(),
                approvals: vec![],
                notifications: Default::default(),
                user_md: Default::default(),
                overseer: Default::default(),
            }),
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
            memory: MemoryModelView::default(),
        }
    }

    #[test]
    fn memory_event_replaces_project_memory() {
        let inst = InstanceId::from_u64(1);
        let project = ProjectId::from_u64(2);
        let mut store = empty_store(inst);
        let sync = Sync::new(0);
        sync.apply(
            &mut store,
            inst,
            snapshot(inst, project, ModelAuthView::SignedOut),
        );
        let memory = MemoryView {
            generation: 1,
            current_task: "寫測試".into(),
            observations: vec![ObservationView {
                id: ObservationId::from_u64(9),
                date: "2026-11-07".into(),
                priority: Priority::High,
                text: "用 SQLite".into(),
                pinned: true,
                edited: false,
            }],
            tokens: 40,
            error: None,
        };
        let out = sync.apply(
            &mut store,
            inst,
            ServerEvent::Memory {
                seq: 4,
                project,
                memory: memory.clone(),
            },
        );
        assert_eq!(out.memory_changed, Some(project));
        assert_eq!(store.project(inst, project).unwrap().memory, memory);
        assert_eq!(store.instance(inst).unwrap().last_seq, 4);
    }

    #[test]
    fn worker_settings_follow_snapshot_and_events() {
        let inst = InstanceId::from_u64(1);
        let project = ProjectId::from_u64(2);
        let mut store = empty_store(inst);
        let sync = Sync::new(0);
        let mut snap = snapshot(inst, project, ModelAuthView::SignedOut);
        let ServerEvent::Snapshot { instance, .. } = &mut snap else {
            unreachable!()
        };
        instance.worker.runtime = "docker".into();
        let out = sync.apply(&mut store, inst, snap);
        assert!(out.worker_changed);
        assert_eq!(store.instance(inst).unwrap().worker.runtime, "docker");

        let worker = WorkerSettingsView {
            backend: WorkerBackendKind::ClaudeCode,
            exec: ExecKind::Local,
            runtime: "podman".into(),
            image: "pots-worker:latest".into(),
            claude_model: String::new(),
            login_command: "claude".into(),
            max_parallel: 4,
        };
        let out = sync.apply(
            &mut store,
            inst,
            ServerEvent::Worker {
                seq: 5,
                worker: worker.clone(),
            },
        );
        assert!(out.worker_changed);
        assert_eq!(store.instance(inst).unwrap().worker, worker);
        assert_eq!(store.instance(inst).unwrap().last_seq, 5);
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

    fn approval(id: u64, project: ProjectId, state: ApprovalState) -> ApprovalView {
        ApprovalView {
            id: ApprovalId::from_u64(id),
            project,
            goal: None,
            task: None,
            title: "merge PR #40：Add hello".into(),
            reason: "CI 通過".into(),
            requested_by: "worker".into(),
            action: ApprovalActionView::MergePr {
                pr: "github.com/me/app#40".into(),
                url: Some("https://github.com/me/app/pull/40".into()),
                sha: "a".repeat(40),
                method: MergeMethod::Squash,
                branch: "pots/x".into(),
                base: "main".into(),
            },
            action_hash: "0123456789ab".into(),
            state,
            note: None,
            result: None,
            notify: true,
            created_at: Timestamp::now(),
            expires_at: Timestamp(Timestamp::now().0 + 72 * 3600),
            decided_at: None,
        }
    }

    fn goal(id: GoalId, state: GoalState, task: TaskId, task_state: TaskStateView) -> GoalView {
        GoalView {
            id,
            title: "加 hello".into(),
            state,
            planner_note: String::new(),
            budget_used_pct: 0,
            tasks: vec![TaskView {
                id: task,
                goal: id,
                title: "Add hello".into(),
                assignee: "worker".into(),
                summary: String::new(),
                state: task_state,
                branch: None,
                elapsed_min: 0,
                cost_cents: 0,
                steps: vec![],
                thread: vec![],
                pr: None,
                pr_url: None,
                progress: String::new(),
            }],
            team: vec![],
            brief: String::new(),
            quick: false,
        }
    }

    #[test]
    fn approvals_and_tasks_become_inbox_items_and_notices() {
        let inst = InstanceId::from_u64(1);
        let project = ProjectId::from_u64(2);
        let mut store = empty_store(inst);
        let sync = Sync::new(0);
        let mut snap = snapshot(inst, project, ModelAuthView::SignedIn { account: None });
        if let ServerEvent::Snapshot { instance, .. } = &mut snap {
            instance.approvals = vec![approval(7, project, ApprovalState::Executed)];
        }
        let out = sync.apply(&mut store, inst, snap);
        assert!(out.notices.is_empty(), "快照不推播");
        assert!(!store.approval(ApprovalId::from_u64(7)).unwrap().is_open());

        // 新的核准：Inbox 多一筆、要推播；核准後送 `Decide`。
        let a = approval(8, project, ApprovalState::Pending);
        let out = sync.apply(
            &mut store,
            inst,
            ServerEvent::ApprovalChanged {
                seq: 4,
                approval: Box::new(a.clone()),
            },
        );
        assert_eq!(out.notices.len(), 1);
        assert_eq!(out.notices[0].item, Some(a.id));
        let item = store.approval(a.id).unwrap();
        assert!(item.is_open());
        assert_eq!(item.kind, InboxKind::Approval);
        assert_eq!(
            item.actions,
            vec![
                InboxAction::Approve,
                InboxAction::Reject,
                InboxAction::ViewDiff,
                InboxAction::OpenThread
            ]
        );
        assert_eq!(item.expiry.as_deref(), Some("72 小時內有效"));
        assert!(item.facts.iter().any(|f| f.contains("用 squash merge")));
        assert!(store.act(a.id, InboxAction::Approve));
        assert!(matches!(
            store.outbox.last(),
            Some((_, ClientCommand::Decide { approve: true, .. }))
        ));
        let mut done = a.clone();
        done.state = ApprovalState::Executed;
        done.result = Some("用 squash merge 了 PR #40".into());
        let out = sync.apply(
            &mut store,
            inst,
            ServerEvent::ApprovalChanged {
                seq: 5,
                approval: Box::new(done),
            },
        );
        assert!(out.notices.is_empty());
        assert_eq!(
            store.approval(a.id).unwrap().decision,
            Decision::Resolved("已核准 · 用 squash merge 了 PR #40".into())
        );
        assert!(!store.decide(a.id, false, None), "已決定的不再送");

        // 需要你處理的任務：出現在 Inbox 並推播；重試後收掉。交給 Planner 的不算。
        let (g, t) = (GoalId::from_u64(3), TaskId::from_u64(4));
        let needs = TaskStateView::NeedsHuman {
            reason: "CI 連續失敗 3 次".into(),
        };
        let out = sync.apply(
            &mut store,
            inst,
            ServerEvent::GoalChanged {
                seq: 6,
                project,
                goal: goal(g, GoalState::Active, t, needs.clone()),
            },
        );
        assert_eq!(out.notices.len(), 1);
        let item = store.approval(task_item_id(t)).unwrap();
        assert_eq!(item.kind, InboxKind::NeedsYou);
        assert_eq!(item.task, Some(t));
        assert!(store.project_needs_you(inst, project));
        assert!(store.act(task_item_id(t), InboxAction::Retry));
        assert!(matches!(
            store.outbox.last(),
            Some((_, ClientCommand::RetryTask { task })) if *task == t
        ));
        let escalated = TaskStateView::NeedsHuman {
            reason: "worker 需要決定：用哪個套件？".into(),
        };
        let out = sync.apply(
            &mut store,
            inst,
            ServerEvent::GoalChanged {
                seq: 7,
                project,
                goal: goal(g, GoalState::Active, t, escalated),
            },
        );
        assert!(out.notices.is_empty(), "交給 Planner 的不推播");
        assert!(!store.project_needs_you(inst, project));

        // Goal 完成：推播一次。
        let done = GoalState::Done {
            summary: "hello 加好了".into(),
        };
        let out = sync.apply(
            &mut store,
            inst,
            ServerEvent::GoalChanged {
                seq: 8,
                project,
                goal: goal(
                    g,
                    done.clone(),
                    t,
                    TaskStateView::Done {
                        outcome: "好了".into(),
                    },
                ),
            },
        );
        assert_eq!(out.notices.len(), 1);
        assert!(out.notices[0].title.starts_with("Goal 完成"));
        let out = sync.apply(
            &mut store,
            inst,
            ServerEvent::GoalChanged {
                seq: 9,
                project,
                goal: goal(
                    g,
                    done,
                    t,
                    TaskStateView::Done {
                        outcome: "好了".into(),
                    },
                ),
            },
        );
        assert!(out.notices.is_empty());

        // 通知設定、USER.md 與 merge 方式。
        let out = sync.apply(
            &mut store,
            inst,
            ServerEvent::UserMd {
                seq: 10,
                user_md: UserMdView {
                    rev: 2,
                    body: "x".into(),
                },
            },
        );
        assert!(out.user_md_changed);
        assert_eq!(store.instance(inst).unwrap().user_md.rev, 2);
        sync.apply(
            &mut store,
            inst,
            ServerEvent::ProjectMergeMethod {
                seq: 11,
                project,
                method: MergeMethod::Rebase,
            },
        );
        assert_eq!(
            store.project(inst, project).unwrap().merge_method,
            MergeMethod::Rebase
        );
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
