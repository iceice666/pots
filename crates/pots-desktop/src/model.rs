//! 客戶端整體狀態。型別定義在 [`pots_proto`]。
//!
//! 觀察的釘選／修正／刪除先改本地，服務接受後用 `Memory` 事件整份替換；被拒絕時服務也會
//! 重送一次 `Memory`，本地的修改就被蓋回去。

pub use pots_proto::*;
use serde::{Deserialize, Serialize};

/// 可以「重試」的任務：需要你決定或失敗的（`RetryTask`）。
pub fn retryable(state: &TaskStateView) -> bool {
    matches!(
        state,
        TaskStateView::NeedsHuman { .. } | TaskStateView::Failed { .. }
    )
}

// ---------------------------------------------------------------------------
// 客戶端整體狀態

/// 所有已連線實例的資料，加上合併後的 Inbox。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Store {
    pub now: Timestamp,
    pub instances: Vec<InstanceView>,
    pub inbox: Vec<InboxItem>,
    /// 尚未送出的指令；接上 WebSocket 前只是記錄，方便除錯與測試。
    pub outbox: Vec<(InstanceId, ClientCommand)>,
}

impl Store {
    pub fn instance(&self, id: InstanceId) -> Option<&InstanceView> {
        self.instances.iter().find(|i| i.id == id)
    }

    pub fn instance_mut(&mut self, id: InstanceId) -> Option<&mut InstanceView> {
        self.instances.iter_mut().find(|i| i.id == id)
    }

    pub fn project(&self, instance: InstanceId, project: ProjectId) -> Option<&ProjectView> {
        self.instance(instance)?
            .projects
            .iter()
            .find(|p| p.id == project)
    }

    pub fn project_mut(
        &mut self,
        instance: InstanceId,
        project: ProjectId,
    ) -> Option<&mut ProjectView> {
        self.instance_mut(instance)?
            .projects
            .iter_mut()
            .find(|p| p.id == project)
    }

    pub fn goal(&self, instance: InstanceId, goal: GoalId) -> Option<&GoalView> {
        self.instance(instance)?
            .projects
            .iter()
            .flat_map(|p| p.goals.iter())
            .find(|g| g.id == goal)
    }

    pub fn task(&self, instance: InstanceId, task: TaskId) -> Option<(&GoalView, &TaskView)> {
        self.instance(instance)?
            .projects
            .iter()
            .flat_map(|p| p.goals.iter())
            .find_map(|g| g.tasks.iter().find(|t| t.id == task).map(|t| (g, t)))
    }

    pub fn task_mut(&mut self, instance: InstanceId, task: TaskId) -> Option<&mut TaskView> {
        self.instance_mut(instance)?
            .projects
            .iter_mut()
            .flat_map(|p| p.goals.iter_mut())
            .flat_map(|g| g.tasks.iter_mut())
            .find(|t| t.id == task)
    }

    pub fn approval(&self, id: ApprovalId) -> Option<&InboxItem> {
        self.inbox.iter().find(|i| i.id == id)
    }

    /// Artifact 與它所屬的 project。
    pub fn artifact(
        &self,
        instance: InstanceId,
        id: ArtifactId,
    ) -> Option<(&ProjectView, &ArtifactView)> {
        self.instance(instance)?
            .projects
            .iter()
            .find_map(|p| p.artifacts.iter().find(|a| a.id == id).map(|a| (p, a)))
    }

    /// 假資料模式的「儲存」：更新全文、版本加一，並標示內容有變的 `##` 區段。
    /// 連著服務時改送 `SaveArtifact`，由服務回的 `ArtifactChanged` 更新。回傳新的版本。
    pub fn edit_artifact(
        &mut self,
        instance: InstanceId,
        id: ArtifactId,
        body: String,
    ) -> Option<u32> {
        let now = self.now;
        let a = self.instance_mut(instance).and_then(|i| {
            i.projects
                .iter_mut()
                .flat_map(|p| p.artifacts.iter_mut())
                .find(|a| a.id == id)
        })?;
        let old = split_sections(&a.body);
        for section in split_sections(&body) {
            let Some(heading) = section.heading else {
                continue;
            };
            let before = old.iter().find(|o| o.heading.as_ref() == Some(&heading));
            if before.map(|o| &o.source) == Some(&section.source) {
                continue;
            }
            match a.human_edits.iter_mut().find(|(h, _)| *h == heading) {
                Some(edit) => edit.1 = now,
                None => a.human_edits.push((heading, now)),
            }
        }
        a.body = body;
        a.rev += 1;
        let (rev, kind, goal, body) = (a.rev, a.kind, a.goal, a.body.clone());
        // Goal 面板的 Brief 分頁讀 `GoalView::brief`（live 模式由服務的 `GoalChanged` 更新）。
        if kind == ArtifactKind::Brief
            && let Some(goal) = goal
            && let Some(g) = self.instance_mut(instance).and_then(|i| {
                i.projects
                    .iter_mut()
                    .flat_map(|p| p.goals.iter_mut())
                    .find(|g| g.id == goal)
            })
        {
            g.brief = body;
        }
        Some(rev)
    }

    pub fn schedule(&self, instance: InstanceId, id: ScheduleId) -> Option<&ScheduleView> {
        self.instance(instance)?
            .schedules
            .iter()
            .find(|s| s.id == id)
    }

    pub fn listener(&self, instance: InstanceId, id: ListenerId) -> Option<&ListenerView> {
        self.instance(instance)?
            .listeners
            .iter()
            .find(|l| l.id == id)
    }

    // 自動化的修改先改本地，再送指令；連著服務時由 `Automation` 事件整份替換。

    /// 提前結束批次暫停（`resume_automations`）。`project` 為 `None` 是「所有 project」那一筆。
    pub fn resume_automations(&mut self, instance: InstanceId, project: Option<ProjectId>) {
        if let Some(i) = self.instance_mut(instance) {
            if project.is_none() {
                i.automation_pause = None;
            }
            i.pauses.retain(|p| p.project != project);
        }
        self.outbox
            .push((instance, ClientCommand::ResumeAutomations { project }));
    }

    /// 暫停排程與 listener，直到手動恢復。顯示等服務回 `Automation`。
    pub fn pause_automations(&mut self, instance: InstanceId, project: Option<ProjectId>) {
        self.outbox.push((
            instance,
            ClientCommand::PauseAutomations {
                project,
                until: None,
            },
        ));
    }

    pub fn set_schedule_enabled(&mut self, instance: InstanceId, id: ScheduleId, enabled: bool) {
        if let Some(s) = self
            .instance_mut(instance)
            .and_then(|i| i.schedules.iter_mut().find(|s| s.id == id))
        {
            s.enabled = enabled;
            self.outbox
                .push((instance, ClientCommand::SetScheduleEnabled { id, enabled }));
        }
    }

    /// 啟用或停用 listener；等待核准中的不受影響。啟用 agent 提案的 listener 就是核准它。
    pub fn set_listener_enabled(&mut self, instance: InstanceId, id: ListenerId, enabled: bool) {
        if let Some(l) = self
            .instance_mut(instance)
            .and_then(|i| i.listeners.iter_mut().find(|l| l.id == id))
            && !matches!(l.state, ListenerState::PendingApproval(_))
        {
            l.state = if enabled {
                ListenerState::Active
            } else {
                ListenerState::Disabled
            };
            self.outbox
                .push((instance, ClientCommand::SetListenerEnabled { id, enabled }));
        }
    }

    pub fn delete_schedule(&mut self, instance: InstanceId, id: ScheduleId) {
        if let Some(i) = self.instance_mut(instance) {
            i.schedules.retain(|s| s.id != id);
        }
        self.outbox
            .push((instance, ClientCommand::DeleteSchedule { id }));
    }

    pub fn delete_listener(&mut self, instance: InstanceId, id: ListenerId) {
        if let Some(i) = self.instance_mut(instance) {
            i.listeners.retain(|l| l.id != id);
        }
        self.outbox
            .push((instance, ClientCommand::DeleteListener { id }));
    }

    /// 新增 listener；服務檢查後以 `Automation` 回來（被拒絕時回 `Error`）。
    pub fn save_listener(&mut self, instance: InstanceId, spec: ListenerSpec) {
        self.outbox
            .push((instance, ClientCommand::SaveListener { id: None, spec }));
    }

    /// 設定 project 監看的 repo；服務整理寫法後以 `ProjectRepos` 回來。
    pub fn set_project_repos(
        &mut self,
        instance: InstanceId,
        project: ProjectId,
        repos: Vec<String>,
    ) {
        self.outbox
            .push((instance, ClientCommand::SetProjectRepos { project, repos }));
    }

    /// `None` 代表所有實例。
    pub fn pending(&self, instance: Option<InstanceId>) -> usize {
        self.inbox
            .iter()
            .filter(|i| i.is_open() && instance.is_none_or(|id| i.instance == id))
            .count()
    }

    /// 實例直列徽章：只算需要你決定或處理的項目，不含「找你」的訊息。
    pub fn badge_count(&self, instance: Option<InstanceId>) -> usize {
        self.inbox
            .iter()
            .filter(|i| {
                i.is_open()
                    && i.kind != InboxKind::Mention
                    && instance.is_none_or(|id| i.instance == id)
            })
            .count()
    }

    pub fn pending_for_project(&self, instance: InstanceId, project: ProjectId) -> usize {
        self.inbox
            .iter()
            .filter(|i| i.is_open() && i.instance == instance && i.project == Some(project))
            .count()
    }

    pub fn project_needs_you(&self, instance: InstanceId, project: ProjectId) -> bool {
        self.inbox.iter().any(|i| {
            i.is_open()
                && i.instance == instance
                && i.project == Some(project)
                && i.kind == InboxKind::NeedsYou
        })
    }

    /// 在 project 對話流尾端加入使用者訊息，並記下要送出的 `Send` 指令。
    pub fn send_to_project(
        &mut self,
        instance: InstanceId,
        project: ProjectId,
        text: String,
        mode: SendMode,
    ) {
        let id = ItemId::new();
        let at = self.now;
        let Some(p) = self.project_mut(instance, project) else {
            return;
        };
        let busy = p.agent_state.is_busy();
        p.items.push(StreamItemView {
            id,
            at,
            kind: StreamItemKind::Human(Msg {
                author: Author::Human,
                text: text.clone(),
                status: if busy && mode == SendMode::Queue {
                    MsgStatus::Queued
                } else {
                    MsgStatus::Done
                },
                tools: None,
                narration: Vec::new(),
                cards: vec![],
                steer: mode == SendMode::Steer,
            }),
        });
        if mode == SendMode::Steer {
            p.agent_state = AgentState::Thinking;
        }
        self.outbox.push((
            instance,
            ClientCommand::Send {
                project,
                id,
                text,
                mode,
            },
        ));
    }

    fn observation_mut(
        &mut self,
        instance: InstanceId,
        project: ProjectId,
        id: ObservationId,
    ) -> Option<&mut ObservationView> {
        self.project_mut(instance, project)?
            .memory
            .observations
            .iter_mut()
            .find(|o| o.id == id)
    }

    /// 釘選或取消釘選一條觀察。先改本地，服務回 `Memory` 時整份替換。
    pub fn pin_observation(
        &mut self,
        instance: InstanceId,
        project: ProjectId,
        id: ObservationId,
        pinned: bool,
    ) {
        let Some(o) = self.observation_mut(instance, project, id) else {
            return;
        };
        o.pinned = pinned;
        self.outbox.push((
            instance,
            ClientCommand::PinObservation {
                project,
                id,
                pinned,
            },
        ));
    }

    /// 修正一條觀察的文字。和服務一樣把連續的空白（含換行）合成一個空格。
    /// 空的或太長時不送出，回傳原因。
    pub fn edit_observation(
        &mut self,
        instance: InstanceId,
        project: ProjectId,
        id: ObservationId,
        text: String,
    ) -> Result<(), String> {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return Err("觀察不能是空的；不要這條請按「刪除」。".into());
        }
        if text.chars().count() > MAX_OBSERVATION_CHARS {
            return Err(format!("一條觀察最多 {MAX_OBSERVATION_CHARS} 個字"));
        }
        let Some(o) = self.observation_mut(instance, project, id) else {
            return Ok(());
        };
        if o.text == text {
            return Ok(());
        }
        o.text = text.clone();
        o.edited = true;
        self.outbox.push((
            instance,
            ClientCommand::EditObservation { project, id, text },
        ));
        Ok(())
    }

    pub fn delete_observation(
        &mut self,
        instance: InstanceId,
        project: ProjectId,
        id: ObservationId,
    ) {
        let Some(p) = self.project_mut(instance, project) else {
            return;
        };
        let before = p.memory.observations.len();
        p.memory.observations.retain(|o| o.id != id);
        if p.memory.observations.len() == before {
            return;
        }
        self.outbox
            .push((instance, ClientCommand::DeleteObservation { project, id }));
    }

    pub fn send_to_overseer(&mut self, instance: InstanceId, text: String) {
        let id = ItemId::new();
        let at = self.now;
        if let Some(inst) = self.instance_mut(instance) {
            inst.overseer.items.push(StreamItemView {
                id,
                at,
                kind: StreamItemKind::Human(Msg {
                    author: Author::Human,
                    text: text.clone(),
                    status: MsgStatus::Queued,
                    tools: None,
                    narration: Vec::new(),
                    cards: vec![],
                    steer: false,
                }),
            });
        }
        self.outbox
            .push((instance, ClientCommand::SendOverseer { id, text }));
    }

    pub fn send_to_task(&mut self, instance: InstanceId, task: TaskId, text: String) {
        let id = ItemId::new();
        let at = self.now;
        if let Some(t) = self.task_mut(instance, task) {
            t.thread.push(StreamItemView {
                id,
                at,
                kind: StreamItemKind::Human(Msg {
                    author: Author::Human,
                    text,
                    status: MsgStatus::Done,
                    tools: None,
                    narration: Vec::new(),
                    cards: vec![],
                    steer: false,
                }),
            });
        }
    }

    pub fn post_goal_note(&mut self, instance: InstanceId, goal: GoalId, text: String) {
        let at = self.now;
        if let Some(g) = self.instance_mut(instance).and_then(|i| {
            i.projects
                .iter_mut()
                .flat_map(|p| p.goals.iter_mut())
                .find(|g| g.id == goal)
        }) {
            // 和服務一樣由舊到新。
            g.team.push(TeamPost {
                kind: TeamPostKind::Question,
                from: "你".into(),
                at,
                body: text,
                to: None,
            });
        }
    }

    /// 核准或拒絕服務的一筆核准（可以附理由）。先標成已決定，服務回 `ApprovalChanged` 時整筆替換
    ///（被拒絕時服務也會重送目前的狀態）。
    pub fn decide(&mut self, id: ApprovalId, approve: bool, note: Option<String>) -> bool {
        let Some(item) = self
            .inbox
            .iter_mut()
            .find(|i| i.id == id && i.approval.is_some() && i.is_open())
        else {
            return false;
        };
        item.decision = if approve {
            Decision::Approved
        } else {
            Decision::Rejected
        };
        let instance = item.instance;
        let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
        self.outbox.push((
            instance,
            ClientCommand::Decide {
                approval: id,
                approve,
                note,
            },
        ));
        true
    }

    /// 處理 Inbox 項目或對話內核准卡上的按鈕。回傳是否改變了狀態。
    pub fn act(&mut self, id: ApprovalId, action: InboxAction) -> bool {
        let Some(item) = self.inbox.iter_mut().find(|i| i.id == id) else {
            return false;
        };
        if !item.is_open() {
            return false;
        }
        // 服務的核准：送 `Decide`，listener 等服務的 `Automation` 更新。
        if item.approval.is_some() {
            return match action {
                InboxAction::Approve => self.decide(id, true, None),
                InboxAction::Reject => self.decide(id, false, None),
                _ => false,
            };
        }
        // 需要你處理的任務：重試或取消，等服務的 `GoalChanged` 收掉這筆。
        if let Some(task) = item.task {
            let instance = item.instance;
            let label = match action {
                InboxAction::Retry => "已要求重試",
                InboxAction::CancelTask => "已取消任務",
                _ => return false,
            };
            item.decision = Decision::Resolved(label.into());
            if action == InboxAction::Retry {
                self.retry_task(instance, task);
            } else {
                self.cancel_task(instance, task);
            }
            return true;
        }
        // 模型登入：交給伺服器，這筆等它回報登入完成才收掉。
        if action == InboxAction::StartLogin
            && id == crate::sync::model_login_item_id(item.instance)
        {
            let instance = item.instance;
            self.outbox.push((instance, ClientCommand::StartModelLogin));
            return true;
        }
        let decision = match action {
            InboxAction::Approve => Decision::Approved,
            InboxAction::Reject => Decision::Rejected,
            InboxAction::Retry => Decision::Resolved("已重新開始自動修復".into()),
            InboxAction::CancelTask => Decision::Resolved("已取消任務".into()),
            InboxAction::StartLogin => Decision::Resolved("已開始登入流程".into()),
            _ => return false,
        };
        let instance = item.instance;
        let approve = decision == Decision::Approved;
        let is_approval = item.kind == InboxKind::Approval;
        item.decision = decision;
        // 核准的是 listener 提案：核准就生效，拒絕就撤掉。
        if is_approval && let Some(inst) = self.instance_mut(instance) {
            let pending = ListenerState::PendingApproval(id);
            if approve {
                for l in inst.listeners.iter_mut().filter(|l| l.state == pending) {
                    l.state = ListenerState::Active;
                }
            } else {
                inst.listeners.retain(|l| l.state != pending);
            }
        }
        if is_approval {
            self.outbox.push((
                instance,
                ClientCommand::Decide {
                    approval: id,
                    approve,
                    note: None,
                },
            ));
        }
        true
    }

    /// 重試需要你決定或失敗的任務：先改成「已規劃」，服務接受後由 `GoalChanged` 整筆替換
    /// （被拒絕時回 `Error`，下一次 `GoalChanged` 蓋回去）。
    pub fn retry_task(&mut self, instance: InstanceId, task: TaskId) {
        if let Some(t) = self.task_mut(instance, task)
            && retryable(&t.state)
        {
            t.state = TaskStateView::Planned;
            self.outbox
                .push((instance, ClientCommand::RetryTask { task }));
        }
    }

    pub fn cancel_task(&mut self, instance: InstanceId, task: TaskId) {
        if let Some(t) = self.task_mut(instance, task) {
            if matches!(
                t.state,
                TaskStateView::Done { .. } | TaskStateView::Cancelled
            ) {
                return;
            }
            t.state = TaskStateView::Cancelled;
            self.outbox.push((instance, ClientCommand::Cancel { task }));
        }
    }
}
