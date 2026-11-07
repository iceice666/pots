//! 假資料：內容取自 `ui/` 的設計稿，模擬同時連著「我的筆電」（桌面版）與
//! 「家用伺服器」（多設備版）兩個實例的情境。

use crate::model::*;

const fn at(h: i64, m: i64) -> Timestamp {
    Timestamp(h * 3600 + m * 60)
}

struct Ids(u64);

impl Ids {
    fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }

    fn item(&mut self, at: Timestamp, kind: StreamItemKind) -> StreamItemView {
        StreamItemView {
            id: ItemId(self.next()),
            at,
            kind,
        }
    }
}

fn human(text: &str) -> StreamItemKind {
    StreamItemKind::Human(Msg {
        author: Author::Human,
        text: text.into(),
        status: MsgStatus::Done,
        tools: None,
        cards: vec![],
        steer: false,
    })
}

fn agent(label: &str, role: AgentRole, text: &str) -> Msg {
    Msg {
        author: Author::Agent {
            label: label.into(),
            role,
        },
        text: text.into(),
        status: MsgStatus::Done,
        tools: None,
        cards: vec![],
        steer: false,
    }
}

fn tools(text: &str, details: &[&str]) -> Option<ToolSummary> {
    Some(ToolSummary {
        text: text.into(),
        details: details.iter().map(|s| s.to_string()).collect(),
    })
}

fn day(text: &str) -> StreamItemKind {
    StreamItemKind::DayDivider(text.into())
}

pub fn store() -> Store {
    let mut ids = Ids(0);
    let laptop = InstanceId(1);
    let home = InstanceId(2);

    // ---- 家用伺服器 · auth-service ---------------------------------------
    let auth = ProjectId(ids.next());
    let oauth_goal = GoalId(ids.next());
    let pr42 = ApprovalId(ids.next());
    let spec = ArtifactId(ids.next());

    let callback_task = TaskView {
        id: TaskId(ids.next()),
        goal: oauth_goal,
        title: "後端 OAuth callback".into(),
        assignee: "worker · Claude Code".into(),
        summary: "補錯誤處理的測試".into(),
        state: TaskStateView::Running,
        branch: Some("agent/t31-oauth-callback".into()),
        elapsed_min: 12,
        cost_cents: 84,
        steps: vec![
            TaskStep {
                text: "讀了 session 模組與 3 個相關檔案".into(),
                at: Some(at(10, 24)),
            },
            TaskStep {
                text: "新增 src/auth/oauth.rs".into(),
                at: Some(at(10, 29)),
            },
            TaskStep {
                text: "cargo test auth::oauth 3 個通過".into(),
                at: Some(at(10, 33)),
            },
            TaskStep {
                text: "補 callback 的錯誤處理與測試".into(),
                at: None,
            },
        ],
        thread: vec![
            ids.item(at(10, 36), human("state 參數記得驗證，存在 session 裡。")),
            ids.item(
                at(10, 38),
                StreamItemKind::Agent(agent(
                    "worker · 後端 OAuth callback",
                    AgentRole::Worker,
                    "收到。已加上 state 驗證，並補了一個 CSRF 的測試案例。",
                )),
            ),
        ],
    };
    let button_task = TaskView {
        id: TaskId(ids.next()),
        goal: oauth_goal,
        title: "前端登入按鈕".into(),
        assignee: "worker · rig".into(),
        summary: "修改登入頁".into(),
        state: TaskStateView::Running,
        branch: Some("agent/t32-login-button".into()),
        elapsed_min: 8,
        cost_cents: 31,
        steps: vec![
            TaskStep {
                text: "讀了登入頁與按鈕元件".into(),
                at: Some(at(10, 50)),
            },
            TaskStep {
                text: "加入「用 Google 登入」按鈕".into(),
                at: None,
            },
        ],
        thread: vec![],
    };
    let integration_task = TaskView {
        id: TaskId(ids.next()),
        goal: oauth_goal,
        title: "整合測試".into(),
        assignee: "worker · Claude Code".into(),
        summary: "等待 CI · PR #44".into(),
        state: TaskStateView::Waiting {
            on: "CI · PR #44".into(),
        },
        branch: Some("agent/t33-oauth-e2e".into()),
        elapsed_min: 21,
        cost_cents: 57,
        steps: vec![
            TaskStep {
                text: "新增 OAuth 登入的端對端測試".into(),
                at: Some(at(10, 40)),
            },
            TaskStep {
                text: "開了 PR #44，等待 CI".into(),
                at: Some(at(10, 58)),
            },
        ],
        thread: vec![],
    };
    let review_task = TaskView {
        id: TaskId(ids.next()),
        goal: oauth_goal,
        title: "Spec 審查".into(),
        assignee: "reviewer subagent".into(),
        summary: "沒有阻擋意見".into(),
        state: TaskStateView::Done {
            outcome: "沒有阻擋意見".into(),
        },
        branch: None,
        elapsed_min: 3,
        cost_cents: 9,
        steps: vec![TaskStep {
            text: "對照 spec 檢查 session 與舊密碼登入的相容性".into(),
            at: Some(at(10, 23)),
        }],
        thread: vec![],
    };

    let oauth = GoalView {
        id: oauth_goal,
        title: "OAuth 登入遷移".into(),
        planner_note: "Planner · 4 個任務".into(),
        budget_used_pct: 38,
        tasks: vec![callback_task, button_task, integration_task, review_task],
        team: vec![
            TeamPost {
                kind: TeamPostKind::Decision,
                from: "Planner".into(),
                at: at(10, 58),
                body: "client id 用 GOOGLE_CLIENT_ID，並寫進 .env.example。".into(),
            },
            TeamPost {
                kind: TeamPostKind::Question,
                from: "前端登入按鈕 → Planner".into(),
                at: at(10, 55),
                body: "Google client id 要放哪個環境變數？".into(),
            },
            TeamPost {
                kind: TeamPostKind::Finding,
                from: "後端 OAuth callback".into(),
                at: at(10, 41),
                body: "auth 的測試要先啟動 redis。".into(),
            },
        ],
        brief: "把登入改成 OAuth，先支援 Google。舊的密碼登入保留三個月，期間兩種方式並存；\
                session 過期改為滑動視窗（見 Spec v3「Session 處理」）。完成條件：\
                後端 callback、前端按鈕、整合測試都合併到 main。"
            .into(),
    };

    let mut tool_reply = agent(
        "auth-service agent",
        AgentRole::Project,
        "了解。我先讀了現在的登入流程，整理成一份 spec，你看看有沒有漏掉的地方。",
    );
    tool_reply.tools = tools(
        "用了 4 個工具 · 讀了 6 個檔案",
        &[
            "read_file src/auth/login.rs",
            "read_file src/auth/session.rs",
            "search_history 「密碼登入」",
            "write_artifact Spec · OAuth 登入遷移",
        ],
    );
    tool_reply.cards.push(Card::Artifact {
        id: spec,
        kind: ArtifactKind::Spec,
        title: "OAuth 登入遷移".into(),
        note: "v3 · 你在 10:20 改過「Session 處理」".into(),
    });
    let mut goal_reply = agent(
        "auth-service agent",
        AgentRole::Project,
        "好，已經照你改過的 spec 開了 Goal，Planner 拆成 4 個任務。",
    );
    goal_reply
        .cards
        .push(Card::GoalCreated { goal: oauth_goal });
    let mut approval_reply = agent("auth-service agent", AgentRole::Project, "");
    approval_reply.cards.push(Card::Approval { id: pr42 });
    let mut streaming = agent(
        "auth-service agent",
        AgentRole::Project,
        "前端登入按鈕還在改，整合測試正在等 CI。結果出來我再告訴你",
    );
    streaming.status = MsgStatus::Streaming;

    let auth_service = ProjectView {
        id: auth,
        name: "auth-service".into(),
        agent_name: "auth-service agent".into(),
        agent_state: AgentState::WaitingCi,
        budget: Budget {
            spent_cents: 420,
            limit_cents: 2000,
        },
        unread: 0,
        items: vec![
            ids.item(at(10, 0), day("今天")),
            ids.item(
                at(10, 2),
                human("登入要改成 OAuth，先支援 Google，舊的密碼登入保留三個月。"),
            ),
            ids.item(at(10, 3), StreamItemKind::Agent(tool_reply)),
            ids.item(
                at(10, 21),
                human("session 過期那段我改了，其他沒問題，可以開工。"),
            ),
            ids.item(at(10, 22), StreamItemKind::Agent(goal_reply)),
            ids.item(
                at(10, 48),
                StreamItemKind::Event {
                    text: "PR #42 已開：後端 OAuth callback".into(),
                },
            ),
            ids.item(at(11, 5), StreamItemKind::Agent(approval_reply)),
            ids.item(at(11, 6), StreamItemKind::Agent(streaming)),
        ],
        goals: vec![oauth],
        artifact_count: 5,
        automation_count: 3,
    };

    // ---- 家用伺服器 · 其他 project ----------------------------------------
    let web = ProjectId(ids.next());
    let checkout_goal = GoalId(ids.next());
    let flaky_task = TaskId(ids.next());
    let web_app = ProjectView {
        id: web,
        name: "web-app".into(),
        agent_name: "web-app agent".into(),
        agent_state: AgentState::Planning,
        budget: Budget {
            spent_cents: 1130,
            limit_cents: 3000,
        },
        unread: 2,
        items: vec![
            ids.item(at(8, 0), day("今天")),
            ids.item(
                at(8, 7),
                StreamItemKind::Card(Card::CatchUp {
                    summary: "你不在期間：結帳頁改版的 CI 連續失敗 3 次，已停止自動修復，\
                              等你決定要再試一次還是取消。"
                        .into(),
                }),
            ),
            ids.item(
                at(9, 15),
                StreamItemKind::Event {
                    text: "總管開了 Goal：修好 checkout 的不穩定測試".into(),
                },
            ),
        ],
        goals: vec![GoalView {
            id: checkout_goal,
            title: "結帳頁改版".into(),
            planner_note: "Planner · 2 個任務".into(),
            budget_used_pct: 71,
            tasks: vec![TaskView {
                id: TaskId(flaky_task.0),
                goal: checkout_goal,
                title: "修正 e2e/checkout.spec.ts".into(),
                assignee: "worker · Claude Code".into(),
                summary: "CI 連續失敗 3 次".into(),
                state: TaskStateView::NeedsHuman {
                    reason: "CI 連續失敗 3 次，已停止自動修復".into(),
                },
                branch: Some("agent/t21-checkout".into()),
                elapsed_min: 46,
                cost_cents: 212,
                steps: vec![
                    TaskStep {
                        text: "第 1 次修復：等待元素出現".into(),
                        at: Some(at(7, 20)),
                    },
                    TaskStep {
                        text: "第 2 次修復：關閉動畫".into(),
                        at: Some(at(7, 41)),
                    },
                    TaskStep {
                        text: "第 3 次修復後仍失敗，停止".into(),
                        at: Some(at(8, 7)),
                    },
                ],
                thread: vec![],
            }],
            team: vec![],
            brief: "結帳頁改版：新版地址表單與付款步驟。".into(),
        }],
        artifact_count: 3,
        automation_count: 1,
    };

    let infra = ProjectView {
        id: ProjectId(ids.next()),
        name: "infra".into(),
        agent_name: "infra agent".into(),
        agent_state: AgentState::Idle,
        budget: Budget {
            spent_cents: 90,
            limit_cents: 1000,
        },
        unread: 0,
        items: vec![
            ids.item(at(10, 0), day("今天")),
            ids.item(
                at(10, 7),
                StreamItemKind::Agent(agent(
                    "infra agent",
                    AgentRole::Project,
                    "我提了一個新的 listener：main 分支 CI 失敗時自動開 Goal 修復，等你在 Inbox 核准。",
                )),
            ),
        ],
        goals: vec![],
        artifact_count: 4,
        automation_count: 2,
    };
    let infra_id = infra.id;

    let docs = ProjectView {
        id: ProjectId(ids.next()),
        name: "docs-site".into(),
        agent_name: "docs-site agent".into(),
        agent_state: AgentState::Idle,
        budget: Budget {
            spent_cents: 0,
            limit_cents: 500,
        },
        unread: 0,
        items: vec![],
        goals: vec![],
        artifact_count: 1,
        automation_count: 1,
    };

    let mut search_reply = agent(
        "總管",
        AgentRole::Overseer,
        "在 # infra，Goal「API 偶發 500 調查」，11/2 已完成。結論是資料庫連線池上限太小，後來調高了。完整報告在這裡：",
    );
    search_reply.tools = tools(
        "搜尋了 4 個專案的歷史",
        &[
            "search_history 「500」 · 4 個專案",
            "read_artifact Report · API 偶發 500 調查",
        ],
    );
    search_reply.cards.push(Card::Artifact {
        id: ArtifactId(ids.next()),
        kind: ArtifactKind::Report,
        title: "API 偶發 500 調查".into(),
        note: "# infra · 11/2".into(),
    });
    let mut dispatch_reply = agent(
        "總管",
        AgentRole::Overseer,
        "好，已在 # web-app 開了 Goal。之後的進度會在 web-app 頻道裡。",
    );
    dispatch_reply.cards.push(Card::RemoteGoal {
        project: "web-app".into(),
        title: "修好 checkout 的不穩定測試".into(),
        note: "由總管建立 · Planner 規劃中".into(),
    });
    let mut pause_reply = agent("總管", AgentRole::Overseer, "");
    pause_reply.cards.push(Card::Notice {
        title: "已暫停 3 個排程、2 個 listener".into(),
        body: "週一 09:00 自動恢復。暫停期間錯過的觸發不會補跑。".into(),
        actions: vec!["查看清單".into(), "立即恢復".into()],
    });
    let mut digest = agent("總管", AgentRole::Overseer, "");
    digest.cards.push(Card::Digest {
        title: "早安摘要".into(),
        rows: vec![
            (
                "# auth-service".into(),
                "OAuth 登入遷移 · 2 進行中 · 1 待你核准".into(),
            ),
            (
                "# web-app".into(),
                "結帳頁改版 · CI 卡住，需要你決定".into(),
            ),
            ("# infra".into(), "沒有進行中的 Goal".into()),
            ("# docs-site".into(), "1 個排程今晚執行".into()),
        ],
        footer: "上游 LLM provider 昨晚的事故已解決，沒有任務受影響。".into(),
    });

    let home_overseer = OverseerView {
        state: AgentState::Idle,
        items: vec![
            ids.item(at(8, 0), day("今天")),
            ids.item(at(8, 30), StreamItemKind::Agent(digest)),
            ids.item(at(9, 12), human("上週那個 500 錯誤的調查在哪個專案？")),
            ids.item(at(9, 12), StreamItemKind::Agent(search_reply)),
            ids.item(at(9, 15), human("web-app 那個 flaky test 交給它修。")),
            ids.item(at(9, 15), StreamItemKind::Agent(dispatch_reply)),
            ids.item(
                at(9, 20),
                human("週末所有排程和 listener 都先暫停，週一再開。"),
            ),
            ids.item(at(9, 20), StreamItemKind::Agent(pause_reply)),
        ],
        overview: vec![
            ProjectOverview {
                project: "auth-service".into(),
                state: AgentState::WaitingCi,
                note: "1 個 Goal · 1 件待核准".into(),
            },
            ProjectOverview {
                project: "web-app".into(),
                state: AgentState::Planning,
                note: "2 個 Goal · 1 件需要你處理".into(),
            },
            ProjectOverview {
                project: "infra".into(),
                state: AgentState::Idle,
                note: "沒有進行中的 Goal".into(),
            },
            ProjectOverview {
                project: "docs-site".into(),
                state: AgentState::Idle,
                note: "排程暫停到週一".into(),
            },
        ],
        memory: vec![
            Observation {
                priority: Priority::High,
                text: "週末不希望被排程打擾，週一早上再恢復。".into(),
            },
            Observation {
                priority: Priority::High,
                text: "web-app 的 e2e 測試常不穩定，修之前先確認是不是 flaky。".into(),
            },
            Observation {
                priority: Priority::Medium,
                text: "早安摘要只要一段，不用逐條列出 CI 結果。".into(),
            },
        ],
        memory_total: 24,
    };

    // ---- 我的筆電 ----------------------------------------------------------
    let dotfiles_id = ProjectId(ids.next());
    let pr9 = ApprovalId(ids.next());
    let shell_goal = GoalId(ids.next());
    let mut dotfiles_reply = agent(
        "dotfiles agent",
        AgentRole::Project,
        "alias 整理好了，重複的 12 個合併、過期的 PATH 設定移除。PR #9 的 CI 都過了。",
    );
    dotfiles_reply.cards.push(Card::Approval { id: pr9 });
    let dotfiles = ProjectView {
        id: dotfiles_id,
        name: "dotfiles".into(),
        agent_name: "dotfiles agent".into(),
        agent_state: AgentState::WaitingApproval,
        budget: Budget {
            spent_cents: 35,
            limit_cents: 500,
        },
        unread: 1,
        items: vec![
            ids.item(at(10, 0), day("今天")),
            ids.item(
                at(10, 12),
                human("幫我整理 shell 設定，重複的 alias 合併掉。"),
            ),
            ids.item(at(10, 42), StreamItemKind::Agent(dotfiles_reply)),
        ],
        goals: vec![GoalView {
            id: shell_goal,
            title: "整理 shell 設定".into(),
            planner_note: "Planner · 1 個任務".into(),
            budget_used_pct: 12,
            tasks: vec![TaskView {
                id: TaskId(ids.next()),
                goal: shell_goal,
                title: "合併 alias 與 PATH".into(),
                assignee: "worker · Claude Code".into(),
                summary: "PR #9 · 等你核准".into(),
                state: TaskStateView::Waiting {
                    on: "核准 · PR #9".into(),
                },
                branch: Some("agent/t3-shell-cleanup".into()),
                elapsed_min: 9,
                cost_cents: 35,
                steps: vec![TaskStep {
                    text: "開了 PR #9".into(),
                    at: Some(at(10, 40)),
                }],
                thread: vec![],
            }],
            team: vec![],
            brief: "合併重複的 alias，移除過期的 PATH 設定。".into(),
        }],
        artifact_count: 1,
        automation_count: 0,
    };
    let side_id = ProjectId(ids.next());
    let side = ProjectView {
        id: side_id,
        name: "side-project".into(),
        agent_name: "side-project agent".into(),
        agent_state: AgentState::Idle,
        budget: Budget {
            spent_cents: 0,
            limit_cents: 500,
        },
        unread: 0,
        items: vec![
            ids.item(at(6, 0), day("今天")),
            ids.item(
                at(6, 7),
                StreamItemKind::Agent(agent(
                    "side-project agent",
                    AgentRole::Project,
                    "匯出功能的 Goal 開好了，但這台電腦的 Claude Code 還沒登入，worker 沒辦法開工。",
                )),
            ),
        ],
        goals: vec![],
        artifact_count: 0,
        automation_count: 0,
    };

    let instances = vec![
        InstanceView {
            id: laptop,
            name: "我的筆電".into(),
            kind: InstanceKind::Desktop,
            host: "127.0.0.1（內嵌服務）".into(),
            connection: ConnectionState::Connected,
            last_seq: 1_204,
            projects: vec![dotfiles, side],
            overseer: OverseerView {
                state: AgentState::Idle,
                items: vec![],
                overview: vec![
                    ProjectOverview {
                        project: "dotfiles".into(),
                        state: AgentState::WaitingApproval,
                        note: "1 個 Goal · 1 件待核准".into(),
                    },
                    ProjectOverview {
                        project: "side-project".into(),
                        state: AgentState::Idle,
                        note: "Claude Code 尚未登入".into(),
                    },
                ],
                memory: vec![],
                memory_total: 0,
            },
            schedules: AutomationSummary {
                active: 0,
                paused: false,
            },
            listeners: AutomationSummary {
                active: 0,
                paused: false,
            },
        },
        InstanceView {
            id: home,
            name: "家用伺服器".into(),
            kind: InstanceKind::MultiDevice,
            host: "pots.home.arpa".into(),
            connection: ConnectionState::Connected,
            last_seq: 88_412,
            projects: vec![auth_service, web_app, infra, docs],
            overseer: home_overseer,
            schedules: AutomationSummary {
                active: 3,
                paused: false,
            },
            listeners: AutomationSummary {
                active: 2,
                paused: false,
            },
        },
    ];

    let inbox = vec![
        InboxItem {
            id: pr42,
            instance: home,
            project: Some(auth),
            context: "OAuth 登入遷移".into(),
            kind: InboxKind::Approval,
            title: "Merge PR #42 到 main".into(),
            detail: "後端 OAuth callback · CI 12 / 12 通過 · +182 −40".into(),
            facts: vec![
                "後端 OAuth callback · +182 −40 · 6 個檔案".into(),
                "CI 12 / 12 通過 · reviewer subagent 沒有阻擋意見".into(),
                "核准後由平台執行 merge，內容與核准時一致才會執行".into(),
            ],
            actions: vec![
                InboxAction::Approve,
                InboxAction::Reject,
                InboxAction::ViewDiff,
            ],
            at: at(11, 5),
            expiry: Some("72 小時內有效".into()),
            decision: Decision::Pending,
        },
        InboxItem {
            id: pr9,
            instance: laptop,
            project: Some(dotfiles_id),
            context: "整理 shell 設定".into(),
            kind: InboxKind::Approval,
            title: "Merge PR #9 到 main".into(),
            detail: "合併重複的 alias，移除過期的 PATH 設定 · CI 4 / 4 通過 · +36 −58".into(),
            facts: vec![
                "合併重複的 alias，移除過期的 PATH 設定 · +36 −58".into(),
                "CI 4 / 4 通過".into(),
            ],
            actions: vec![
                InboxAction::Approve,
                InboxAction::Reject,
                InboxAction::ViewDiff,
            ],
            at: at(10, 42),
            expiry: Some("72 小時內有效".into()),
            decision: Decision::Pending,
        },
        InboxItem {
            id: ApprovalId(ids.next()),
            instance: home,
            project: Some(infra_id),
            context: "新的 listener".into(),
            kind: InboxKind::Approval,
            title: "main 分支 CI 失敗時，自動開 Goal 修復".into(),
            detail: String::new(),
            facts: vec![
                "來源：GitHub".into(),
                "信任等級：Owned".into(),
                "每次上限 $2".into(),
                "每小時最多 2 次".into(),
            ],
            actions: vec![
                InboxAction::Approve,
                InboxAction::Reject,
                InboxAction::EditConditions,
            ],
            at: at(10, 7),
            expiry: None,
            decision: Decision::Pending,
        },
        InboxItem {
            id: ApprovalId(ids.next()),
            instance: home,
            project: Some(web),
            context: "結帳頁改版".into(),
            kind: InboxKind::NeedsYou,
            title: "CI 連續失敗 3 次，已停止自動修復".into(),
            detail: "agent 判斷可能是不穩定的測試：e2e/checkout.spec.ts，3 次失敗的位置都不同。"
                .into(),
            facts: vec![],
            actions: vec![
                InboxAction::Retry,
                InboxAction::OpenThread,
                InboxAction::CancelTask,
            ],
            at: at(8, 7),
            expiry: None,
            decision: Decision::Pending,
        },
        InboxItem {
            id: ApprovalId(ids.next()),
            instance: laptop,
            project: Some(side_id),
            context: "匯出功能".into(),
            kind: InboxKind::NeedsYou,
            title: "這台電腦的 Claude Code 尚未登入，worker 無法開工".into(),
            detail: "每個實例各自登入，家用伺服器的登入不會帶到這裡。請在這台電腦上用官方 CLI 完成登入。"
                .into(),
            facts: vec![],
            actions: vec![InboxAction::StartLogin, InboxAction::OpenThread],
            at: at(6, 7),
            expiry: None,
            decision: Decision::Pending,
        },
        InboxItem {
            id: ApprovalId(ids.next()),
            instance: home,
            project: None,
            context: String::new(),
            kind: InboxKind::Mention,
            title: "早安摘要".into(),
            detail: "2 個 Goal 進行中，1 件需要你處理。上游 LLM provider 昨晚的事故已解決，沒有任務受影響。"
                .into(),
            facts: vec![],
            actions: vec![InboxAction::OpenOverseer],
            at: at(8, 30),
            expiry: None,
            decision: Decision::Pending,
        },
    ];

    Store {
        now: at(11, 7),
        instances,
        inbox,
        next_id: ids.0,
        outbox: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_cards_resolve() {
        let store = store();
        for inst in &store.instances {
            for p in &inst.projects {
                for item in &p.items {
                    let cards = match &item.kind {
                        StreamItemKind::Agent(m) | StreamItemKind::Human(m) => m.cards.as_slice(),
                        StreamItemKind::Card(c) => std::slice::from_ref(c),
                        _ => &[],
                    };
                    for card in cards {
                        match card {
                            Card::Approval { id } => {
                                let a = store.approval(*id).expect("approval exists");
                                assert_eq!(a.instance, inst.id);
                                assert_eq!(a.project, Some(p.id));
                            }
                            Card::GoalCreated { goal } => {
                                assert!(store.goal(inst.id, *goal).is_some());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn approving_updates_counts_and_outbox() {
        let mut store = store();
        assert_eq!(store.pending(None), 6);
        let home = InstanceId(2);
        let first = store.inbox[0].id;
        assert!(store.act(first, InboxAction::Approve));
        assert!(
            !store.act(first, InboxAction::Reject),
            "decided items stay decided"
        );
        assert_eq!(store.pending(None), 5);
        assert_eq!(store.pending(Some(home)), 3);
        assert!(matches!(
            store.outbox.last(),
            Some((i, ClientCommand::Decide { approve: true, .. })) if *i == home
        ));
    }

    #[test]
    fn queued_while_busy() {
        let mut store = store();
        let home = InstanceId(2);
        let auth = store.instances[1].projects[0].id;
        store.send_to_project(home, auth, "加上 GitHub 登入".into(), SendMode::Queue);
        let p = store.project(home, auth).unwrap();
        let StreamItemKind::Human(m) = &p.items.last().unwrap().kind else {
            panic!("expected human message");
        };
        assert_eq!(m.status, MsgStatus::Queued);
    }
}
