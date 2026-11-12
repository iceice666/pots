//! 頻道右欄的成員列表（像 Discord／Slack 的成員欄），從現有資料推導，不另外存。
//!
//! 成員：你、project 對話 agent、每個進行中 Goal 的 Planner（快速修改不經 Planner，沒有）、
//! 每個進行中／等待中／需要你的任務各一個 worker（已規劃的還沒有 worker，結束的不顯示）。
//! 依「工作中」「等待中」「待命」分組，空的組不列出。

use crate::model::*;

/// 成員列表的分組，依這個順序顯示。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemberGroup {
    /// 對話 agent 正在跑這一輪，或 worker 正在寫程式。
    Working,
    /// 等你核准、等 CI 等外部事件，或需要你決定。
    Waiting,
    /// 閒置的 agent、Planner 與你。
    Standby,
}

impl MemberGroup {
    pub const ALL: [Self; 3] = [Self::Working, Self::Waiting, Self::Standby];

    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "工作中",
            Self::Waiting => "等待中",
            Self::Standby => "待命",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberKind {
    Human,
    /// project 對話 agent（角色 Chat）。
    Agent,
    Planner,
    Worker,
}

impl MemberKind {
    /// 名稱旁的角色標籤，和對話裡的 AGENT chip 同一種樣式；你沒有。
    pub fn tag(self) -> Option<&'static str> {
        match self {
            Self::Human => None,
            Self::Agent => Some("AGENT"),
            Self::Planner => Some("PLANNER"),
            Self::Worker => Some("WORKER"),
        }
    }
}

/// 點了成員打開什麼。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberTarget {
    /// 設定視窗的「角色」類別。
    Roles,
    /// 那個 Goal 的面板。
    Goal(GoalId),
    /// 任務 thread。
    Task(TaskId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub kind: MemberKind,
    pub name: String,
    pub group: MemberGroup,
    /// 第二行：狀態或進度（等待中的是原因）。
    pub status: String,
    /// 第三行：「模型 · 思考 高」；你沒有。
    pub model: Option<String>,
    pub target: Option<MemberTarget>,
}

impl Member {
    /// 頭像上的字：名稱的第一個字（英文轉大寫）。
    pub fn initial(&self) -> String {
        self.name
            .chars()
            .find(|c| !c.is_whitespace())
            .map(|c| c.to_uppercase().collect())
            .unwrap_or_default()
    }
}

/// 一組成員，標題是「工作中 — 2」。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberSection {
    pub group: MemberGroup,
    pub members: Vec<Member>,
}

impl MemberSection {
    pub fn title(&self) -> String {
        format!("{} — {}", self.group.label(), self.members.len())
    }
}

/// 從 project、provider 設定與 worker 設定推導成員列表。
pub fn members(
    project: &ProjectView,
    providers: &ProvidersView,
    worker: &WorkerSettingsView,
) -> Vec<MemberSection> {
    let mut all = Vec::new();

    // 對話 agent。
    let (group, status) = match project.agent_state {
        state if state.is_busy() => (MemberGroup::Working, state.label().to_string()),
        AgentState::WaitingApproval => (MemberGroup::Waiting, "等你核准".to_string()),
        state => (MemberGroup::Standby, state.label().to_string()),
    };
    all.push(Member {
        kind: MemberKind::Agent,
        name: project.agent_name.clone(),
        group,
        status,
        model: Some(role_model(providers, ModelRole::Chat)),
        target: Some(MemberTarget::Roles),
    });

    for goal in project.goals.iter().filter(|g| g.state.is_active()) {
        // 快速修改與環境分析沒有 Planner。
        if !goal.quick && !goal.env {
            all.push(Member {
                kind: MemberKind::Planner,
                name: format!("Planner · {}", goal.title),
                group: MemberGroup::Standby,
                status: planner_status(goal),
                model: Some(role_model(providers, ModelRole::Planner)),
                target: Some(MemberTarget::Goal(goal.id)),
            });
        }
        for task in &goal.tasks {
            let Some((group, status)) = worker_status(task) else {
                continue;
            };
            all.push(Member {
                kind: MemberKind::Worker,
                name: task.title.clone(),
                group,
                status,
                model: Some(worker_model(&task.assignee, providers, worker)),
                target: Some(MemberTarget::Task(task.id)),
            });
        }
    }

    all.push(Member {
        kind: MemberKind::Human,
        name: "你".into(),
        group: MemberGroup::Standby,
        status: "在線".into(),
        model: None,
        target: None,
    });

    // 同一組裡維持上面的順序：agent、Planner／worker（依 Goal 與任務順序）、你。
    MemberGroup::ALL
        .into_iter()
        .filter_map(|group| {
            let members: Vec<Member> = all.iter().filter(|m| m.group == group).cloned().collect();
            (!members.is_empty()).then_some(MemberSection { group, members })
        })
        .collect()
}

fn trimmed_or(text: &str, fallback: impl FnOnce() -> String) -> String {
    let text = text.trim();
    if text.is_empty() {
        fallback()
    } else {
        text.to_string()
    }
}

/// Planner 的最後一句話；沒有時是任務完成數。
fn planner_status(goal: &GoalView) -> String {
    trimmed_or(&goal.planner_note, || {
        if goal.tasks.is_empty() {
            "還沒拆任務".into()
        } else {
            format!(
                "{}/{} 個任務完成",
                goal.count(TaskBucket::Done),
                goal.tasks.len()
            )
        }
    })
}

/// 有 worker 的任務：分組與第二行。已規劃（還沒有 worker）與結束的任務是 `None`。
fn worker_status(task: &TaskView) -> Option<(MemberGroup, String)> {
    match &task.state {
        TaskStateView::Running => Some((
            MemberGroup::Working,
            trimmed_or(&task.progress, || {
                trimmed_or(&task.summary, || "worker 開始處理".into())
            }),
        )),
        TaskStateView::Waiting { on } => Some((MemberGroup::Waiting, format!("等待 {on}"))),
        TaskStateView::NeedsHuman { reason } => Some((
            MemberGroup::Waiting,
            format!(
                "需要你：{}",
                trimmed_or(reason, || "worker 停下來了".into())
            ),
        )),
        TaskStateView::Planned
        | TaskStateView::Done { .. }
        | TaskStateView::Failed { .. }
        | TaskStateView::Cancelled => None,
    }
}

/// 「模型 · 思考 高」；thinking 是模型預設時只有模型。
fn with_thinking(model: String, thinking: ThinkingLevel) -> String {
    if thinking == ThinkingLevel::Default {
        model
    } else {
        format!("{model} · 思考 {}", thinking.label())
    }
}

/// 角色實際用的模型與 thinking。
pub fn role_model(providers: &ProvidersView, role: ModelRole) -> String {
    let model = match providers.resolve_role(role) {
        None => "還沒有可用的 provider".to_string(),
        Some((_, model)) if !model.is_empty() => model,
        Some((p, _)) if p.kind == ProviderKind::ChatGpt => "ChatGPT · 自動".to_string(),
        Some((p, _)) => format!("{} · 未設定模型", p.name),
    };
    with_thinking(model, providers.role(role).thinking)
}

/// 執行任務的是哪一種 worker。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Engine {
    ClaudeCode,
    Role(ModelRole),
}

/// 優先看任務的執行者（服務寫的是「worker · Claude Code」「worker · 內建」，subagent 是
/// 「reviewer subagent」等）；看不出來時依目前的 worker 設定。
fn engine(assignee: &str, worker: &WorkerSettingsView) -> Engine {
    let a = assignee.to_lowercase();
    if a.contains("claude code") {
        Engine::ClaudeCode
    } else if a.contains("reviewer") {
        Engine::Role(ModelRole::Reviewer)
    } else if a.contains("explorer") {
        Engine::Role(ModelRole::Explorer)
    } else if a.contains("內建") || worker.backend == WorkerBackendKind::Builtin {
        Engine::Role(ModelRole::Worker)
    } else {
        Engine::ClaudeCode
    }
}

fn worker_model(assignee: &str, providers: &ProvidersView, worker: &WorkerSettingsView) -> String {
    match engine(assignee, worker) {
        Engine::ClaudeCode => {
            let model = worker.claude_model.trim();
            format!(
                "Claude Code · {}",
                if model.is_empty() { "預設" } else { model }
            )
        }
        Engine::Role(role) => role_model(providers, role),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC: ProviderId = ProviderId::from_u64(2);

    fn providers() -> ProvidersView {
        ProvidersView {
            active: Some(ProviderId::CHATGPT),
            items: vec![
                ProviderView {
                    id: ProviderId::CHATGPT,
                    kind: ProviderKind::ChatGpt,
                    name: "ChatGPT".into(),
                    base_url: String::new(),
                    model: String::new(),
                    key_hint: None,
                    headers: Vec::new(),
                },
                ProviderView {
                    id: ANTHROPIC,
                    kind: ProviderKind::AnthropicCompatible,
                    name: "Anthropic".into(),
                    base_url: "https://api.anthropic.com".into(),
                    model: "claude-sonnet-4-5".into(),
                    key_hint: Some("…9f2c".into()),
                    headers: Vec::new(),
                },
            ],
            memory: MemoryModelView::default(),
            roles: vec![
                RoleModelView {
                    role: ModelRole::Planner,
                    provider: Some(ANTHROPIC),
                    model: "claude-opus-4-1".into(),
                    thinking: ThinkingLevel::High,
                },
                RoleModelView {
                    role: ModelRole::Reviewer,
                    provider: Some(ANTHROPIC),
                    model: String::new(),
                    thinking: ThinkingLevel::Off,
                },
            ],
        }
    }

    fn worker(backend: WorkerBackendKind, claude_model: &str) -> WorkerSettingsView {
        WorkerSettingsView {
            backend,
            claude_model: claude_model.into(),
            ..Default::default()
        }
    }

    fn task(n: u64, title: &str, assignee: &str, state: TaskStateView) -> TaskView {
        TaskView {
            id: TaskId::from_u64(n),
            goal: GoalId::from_u64(1),
            title: title.into(),
            assignee: assignee.into(),
            summary: String::new(),
            state,
            branch: None,
            elapsed_min: 0,
            cost_cents: 0,
            steps: Vec::new(),
            thread: Vec::new(),
            pr: None,
            pr_url: None,
            progress: String::new(),
        }
    }

    fn goal(n: u64, title: &str, state: GoalState, tasks: Vec<TaskView>) -> GoalView {
        GoalView {
            id: GoalId::from_u64(n),
            title: title.into(),
            state,
            planner_note: String::new(),
            budget_used_pct: 0,
            tasks,
            team: Vec::new(),
            activity: Vec::new(),
            brief: String::new(),
            quick: false,
            env: false,
        }
    }

    fn project(agent_state: AgentState, goals: Vec<GoalView>) -> ProjectView {
        ProjectView {
            id: ProjectId::from_u64(1),
            name: "auth-service".into(),
            agent_name: "auth-service 負責人".into(),
            agent_state,
            budget: Budget {
                spent_cents: 0,
                limit_cents: 0,
            },
            unread: 0,
            items: Vec::new(),
            goals,
            artifacts: Vec::new(),
            memory: MemoryView::default(),
            repos: Vec::new(),
            workdir: None,
            merge_method: MergeMethod::default(),
            env: ProjectEnvView::default(),
        }
    }

    fn names(sections: &[MemberSection]) -> Vec<(MemberGroup, Vec<&str>)> {
        sections
            .iter()
            .map(|s| (s.group, s.members.iter().map(|m| m.name.as_str()).collect()))
            .collect()
    }

    fn find<'a>(sections: &'a [MemberSection], name: &str) -> &'a Member {
        sections
            .iter()
            .flat_map(|s| &s.members)
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("找不到成員 {name}"))
    }

    #[test]
    fn groups_agent_planner_workers_and_you() {
        let mut running = task(
            1,
            "後端 OAuth callback",
            "worker · 內建",
            TaskStateView::Running,
        );
        running.progress = "寫好 callback，正在跑測試".into();
        let mut active = goal(
            1,
            "OAuth 登入遷移",
            GoalState::Active,
            vec![
                running,
                task(
                    2,
                    "整合測試",
                    "worker · 內建",
                    TaskStateView::Waiting { on: "CI".into() },
                ),
                task(
                    3,
                    "前端登入按鈕",
                    "worker · 內建",
                    TaskStateView::NeedsHuman {
                        reason: "要選按鈕樣式".into(),
                    },
                ),
                task(4, "文件", "尚未指派", TaskStateView::Planned),
                task(
                    5,
                    "Spec 審查",
                    "reviewer subagent",
                    TaskStateView::Done {
                        outcome: "通過".into(),
                    },
                ),
                task(
                    6,
                    "舊 API",
                    "worker · 內建",
                    TaskStateView::Failed {
                        attempts: 2,
                        error: "x".into(),
                    },
                ),
                task(7, "取消的", "worker · 內建", TaskStateView::Cancelled),
            ],
        );
        active.planner_note = "等整合測試跑完再合併".into();
        let finished = goal(
            2,
            "舊的 Goal",
            GoalState::Done {
                summary: "好了".into(),
            },
            vec![task(8, "舊任務", "worker · 內建", TaskStateView::Running)],
        );
        let p = project(AgentState::Thinking, vec![active, finished]);

        let sections = members(&p, &providers(), &worker(WorkerBackendKind::Builtin, ""));
        assert_eq!(
            names(&sections),
            vec![
                (
                    MemberGroup::Working,
                    vec!["auth-service 負責人", "後端 OAuth callback"]
                ),
                (MemberGroup::Waiting, vec!["整合測試", "前端登入按鈕"]),
                (MemberGroup::Standby, vec!["Planner · OAuth 登入遷移", "你"]),
            ]
        );
        assert_eq!(sections[0].title(), "工作中 — 2");

        let agent = find(&sections, "auth-service 負責人");
        assert_eq!(agent.kind, MemberKind::Agent);
        assert_eq!(agent.status, "思考中");
        assert_eq!(agent.model.as_deref(), Some("ChatGPT · 自動"));
        assert_eq!(agent.target, Some(MemberTarget::Roles));
        assert_eq!(agent.initial(), "A");

        let w = find(&sections, "後端 OAuth callback");
        assert_eq!(w.status, "寫好 callback，正在跑測試");
        assert_eq!(w.target, Some(MemberTarget::Task(TaskId::from_u64(1))));
        assert_eq!(w.kind.tag(), Some("WORKER"));
        assert_eq!(find(&sections, "整合測試").status, "等待 CI");
        assert_eq!(
            find(&sections, "前端登入按鈕").status,
            "需要你：要選按鈕樣式"
        );

        let planner = find(&sections, "Planner · OAuth 登入遷移");
        assert_eq!(planner.status, "等整合測試跑完再合併");
        assert_eq!(planner.model.as_deref(), Some("claude-opus-4-1 · 思考 高"));
        assert_eq!(
            planner.target,
            Some(MemberTarget::Goal(GoalId::from_u64(1)))
        );

        let you = find(&sections, "你");
        assert_eq!(you.kind.tag(), None);
        assert_eq!(you.model, None);
        assert_eq!(you.target, None);
    }

    #[test]
    fn agent_waiting_for_approval_or_idle() {
        let empty = WorkerSettingsView::default();
        let waiting = members(
            &project(AgentState::WaitingApproval, Vec::new()),
            &providers(),
            &empty,
        );
        assert_eq!(waiting[0].group, MemberGroup::Waiting);
        assert_eq!(waiting[0].members[0].status, "等你核准");

        let idle = members(&project(AgentState::Idle, Vec::new()), &providers(), &empty);
        assert_eq!(
            names(&idle),
            vec![(MemberGroup::Standby, vec!["auth-service 負責人", "你"])]
        );
        assert_eq!(idle[0].members[0].status, "閒置");
    }

    #[test]
    fn worker_model_follows_assignee_then_backend() {
        let g = goal(
            1,
            "G",
            GoalState::Active,
            vec![
                task(1, "cc", "worker · Claude Code", TaskStateView::Running),
                task(2, "builtin", "worker · 內建", TaskStateView::Running),
                task(3, "unknown", "worker", TaskStateView::Running),
                task(4, "review", "reviewer subagent", TaskStateView::Running),
            ],
        );
        let p = project(AgentState::Idle, vec![g]);

        let claude = members(&p, &providers(), &worker(WorkerBackendKind::ClaudeCode, ""));
        assert_eq!(
            find(&claude, "cc").model.as_deref(),
            Some("Claude Code · 預設")
        );
        assert_eq!(
            find(&claude, "builtin").model.as_deref(),
            Some("ChatGPT · 自動")
        );
        assert_eq!(
            find(&claude, "unknown").model.as_deref(),
            Some("Claude Code · 預設")
        );
        // Reviewer 用 Anthropic 設定的模型，thinking 關閉。
        assert_eq!(
            find(&claude, "review").model.as_deref(),
            Some("claude-sonnet-4-5 · 思考 關閉")
        );

        let builtin = members(
            &p,
            &providers(),
            &worker(WorkerBackendKind::Builtin, "opus"),
        );
        assert_eq!(
            find(&builtin, "cc").model.as_deref(),
            Some("Claude Code · opus")
        );
        assert_eq!(
            find(&builtin, "unknown").model.as_deref(),
            Some("ChatGPT · 自動")
        );
    }

    #[test]
    fn quick_fix_has_no_planner_and_missing_provider_is_explained() {
        let mut quick = goal(
            1,
            "改 alias",
            GoalState::Active,
            vec![task(
                1,
                "合併 alias",
                "worker · 內建",
                TaskStateView::Planned,
            )],
        );
        quick.quick = true;
        let mut none = providers();
        none.active = None;
        let sections = members(
            &project(AgentState::Idle, vec![quick]),
            &none,
            &WorkerSettingsView::default(),
        );
        assert_eq!(
            names(&sections),
            vec![(MemberGroup::Standby, vec!["auth-service 負責人", "你"])]
        );
        assert_eq!(
            sections[0].members[0].model.as_deref(),
            Some("還沒有可用的 provider")
        );
    }

    #[test]
    fn env_goal_has_no_planner() {
        let mut env = goal(
            1,
            "分析環境",
            GoalState::Active,
            vec![task(1, "建立環境", "env", TaskStateView::Planned)],
        );
        env.env = true;
        let sections = members(
            &project(AgentState::Idle, vec![env]),
            &providers(),
            &WorkerSettingsView::default(),
        );
        assert!(
            sections
                .iter()
                .flat_map(|s| &s.members)
                .all(|m| m.kind != MemberKind::Planner)
        );
    }

    #[test]
    fn planner_without_note_shows_progress() {
        let g = goal(
            1,
            "G",
            GoalState::Active,
            vec![
                task(
                    1,
                    "a",
                    "worker",
                    TaskStateView::Done {
                        outcome: String::new(),
                    },
                ),
                task(2, "b", "worker", TaskStateView::Planned),
            ],
        );
        let sections = members(
            &project(AgentState::Idle, vec![g]),
            &providers(),
            &WorkerSettingsView::default(),
        );
        assert_eq!(find(&sections, "Planner · G").status, "1/2 個任務完成");
    }
}
