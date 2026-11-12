//! 假資料：模擬同時連著「我的筆電」（桌面版）與「家用伺服器」（多設備版）兩個實例的情境。

use crate::model::*;

const fn at(h: i64, m: i64) -> Timestamp {
    Timestamp(h * 3600 + m * 60)
}

/// 內建的 ChatGPT（使用中），可以再加一個 API provider。
fn mock_providers(extra: Option<ProviderView>) -> ProvidersView {
    let mut items = vec![ProviderView {
        id: ProviderId::CHATGPT,
        kind: ProviderKind::ChatGpt,
        name: "ChatGPT".into(),
        base_url: String::new(),
        model: String::new(),
        key_hint: None,
        headers: Vec::new(),
    }];
    items.extend(extra);
    ProvidersView {
        active: Some(ProviderId::CHATGPT),
        items,
        memory: MemoryModelView::default(),
        roles: Vec::new(),
    }
}

/// worker 設定：容器裡跑 Claude Code（筆電）或內建 worker（家用伺服器）。
fn mock_worker(backend: WorkerBackendKind, data_dir: &str) -> WorkerSettingsView {
    let home = format!("'{data_dir}/claude-home'");
    WorkerSettingsView {
        backend,
        exec: ExecKind::Container,
        runtime: "docker".into(),
        image: "pots-worker:latest".into(),
        claude_model: String::new(),
        login_command: format!(
            "mkdir -p {home} && docker run -it --rm --user 501:20 -v {home}:/home/pots \
             -e HOME=/home/pots pots-worker:latest claude"
        ),
        max_parallel: 3,
    }
}

/// auth-service 的環境：兩個版本，使用中的是第 2 版。
fn auth_env() -> ProjectEnvView {
    let v1 = EnvSpec {
        base_image: String::new(),
        install: "apt-get update && apt-get install -y pkg-config libssl-dev".into(),
        prepare: "cargo fetch".into(),
        verify: "cargo test --workspace --no-run".into(),
        env: vec![("CARGO_TERM_COLOR".into(), "never".into())],
        caches: vec!["/home/pots/.cargo/registry".into()],
    };
    let v2 = EnvSpec {
        install: "apt-get update && apt-get install -y pkg-config libssl-dev postgresql-client\n\
                  rustup component add clippy rustfmt"
            .into(),
        prepare: "cargo fetch\nsqlx database setup --source migrations".into(),
        env: vec![
            ("CARGO_TERM_COLOR".into(), "never".into()),
            (
                "DATABASE_URL".into(),
                "postgres://pots@localhost/auth_test".into(),
            ),
        ],
        caches: vec![
            "/home/pots/.cargo/registry".into(),
            "/workspace/target".into(),
        ],
        ..v1.clone()
    };
    ProjectEnvView {
        current: Some(2),
        revs: vec![
            EnvRevView {
                rev: 2,
                spec: v2,
                summary:
                    "Rust workspace（edition 2024）。整合測試要 Postgres：裝了 postgresql-client，\
                          prepare 跑 migration；target 放進快取，第二次 run 的建置快很多。"
                        .into(),
                created_at: at(9, 12),
            },
            EnvRevView {
                rev: 1,
                spec: v1,
                summary: "Rust workspace，openssl 需要 libssl-dev；cargo fetch 預先抓 crate。"
                    .into(),
                created_at: at(8, 30),
            },
        ],
        setup_goal: None,
    }
}

/// auth-service 對話 agent 的記憶。
fn auth_memory(ids: &mut Ids) -> MemoryView {
    let mut obs =
        |date: &str, priority: Priority, text: &str, pinned: bool, edited: bool| ObservationView {
            id: ObservationId::from_u64(ids.next()),
            date: date.into(),
            priority,
            text: text.into(),
            pinned,
            edited,
        };
    MemoryView {
        generation: 1,
        current_task: "等整合測試的 CI 結果，之後串上前端登入按鈕。".into(),
        observations: vec![
            obs(
                "2026-10-28",
                Priority::High,
                "登入改成 OAuth，先支援 Google；舊的密碼登入保留三個月後移除。",
                true,
                false,
            ),
            obs(
                "2026-10-30",
                Priority::High,
                "session 存在 Redis，不用 JWT；有效期 14 天。",
                false,
                true,
            ),
            obs(
                "2026-11-02",
                Priority::Medium,
                "callback 路徑定為 /auth/google/callback，staging 與 production 各一組 client ID。",
                false,
                false,
            ),
            obs(
                "2026-11-05",
                Priority::Medium,
                "後端 OAuth 流程已合併（PR #38），整合測試還在跑。",
                false,
                false,
            ),
            obs(
                "2026-11-06",
                Priority::Low,
                "整合測試用 staging 的測試帳號，不碰正式的 Google 專案。",
                false,
                false,
            ),
        ],
        tokens: 412,
        error: None,
    }
}

struct Ids(u64);

impl Ids {
    fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }

    fn item(&mut self, at: Timestamp, kind: StreamItemKind) -> StreamItemView {
        StreamItemView {
            id: ItemId::from_u64(self.next()),
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
        narration: Vec::new(),
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
        narration: Vec::new(),
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

fn doc(
    id: ArtifactId,
    kind: ArtifactKind,
    title: &str,
    note: &str,
    rev: u32,
    goal: Option<GoalId>,
    body: &str,
) -> ArtifactView {
    ArtifactView {
        id,
        kind,
        title: title.into(),
        note: note.into(),
        rev,
        goal,
        body: body.into(),
        human_edits: vec![],
        agent_writing: None,
        proposals: 0,
    }
}

fn trigger(title: &str, meta: &str, status: TriggerStatus) -> TriggerRecord {
    TriggerRecord {
        title: title.into(),
        meta: meta.into(),
        status,
    }
}

const OAUTH_SPEC: &str = "\
# OAuth 登入遷移

## 目標

以 Google OAuth 取代密碼登入。舊的密碼登入保留三個月後移除。

## 範圍

- 後端新增 OAuth callback 與 token 交換
- 前端登入頁加上「使用 Google 登入」
- 既有帳號以 email 綁定 Google 身分

## Session 處理

OAuth 登入後沿用現有的 session 機制，過期時間改為 14 天。登出時同時撤銷 Google token。

## 測試計畫

- [x] callback 成功與失敗路徑
- [x] state 參數驗證（CSRF）
- [ ] 舊的密碼登入仍可使用
- [ ] session 過期後重新導向登入頁

## 未決問題

- 是否同時支援 GitHub 登入？目前不在範圍內。
";

const OAUTH_BRIEF: &str = "\
# OAuth 登入遷移

## 目標

把登入改成 OAuth，先支援 Google。舊的密碼登入保留三個月，期間兩種方式並存。

## 完成條件

- 後端 callback、前端按鈕、整合測試都合併到 main
- session 過期時間改為 14 天（見 Spec「Session 處理」）

## 限制

- 不改動既有的密碼登入流程
- 不做 GitHub 登入（Spec 的未決問題）
";

const OAUTH_LEDGER: &str = "\
# OAuth 登入遷移 · Ledger

## 任務

- [ ] 後端 OAuth callback（worker · Claude Code，進行中）
- [ ] 前端登入按鈕（worker · rig，進行中）
- [ ] 整合測試（等待 CI · PR #44）
- [x] Spec 審查（reviewer subagent，沒有阻擋意見）

## 決定

- client id 用 `GOOGLE_CLIENT_ID`，並寫進 `.env.example`。
- auth 的測試要先啟動 redis。

## 花費

目前 $1.81，預算已用 38%。
";

const OAUTH_REVIEW: &str = "\
# Spec 審查

## 結論

沒有阻擋意見，可以開工。

## 檢查項目

- [x] session 機制與舊的密碼登入可以並存
- [x] callback 的 state 參數有防 CSRF
- [x] 登出時撤銷 Google token

## 建議

- 整合測試加上 session 過期後重新導向登入頁的案例。
";

const AUTH_NOTES: &str = "\
# Project Notes

## 開發環境

- 跑測試前先 `docker compose up redis`。
- 本機的 OAuth redirect 是 `http://localhost:8080/auth/callback`。

## 慣例

- 錯誤訊息用中文，log 用英文。
- 每個 PR 只做一件事。
";

const INFRA_500_REPORT: &str = "\
# API 偶發 500 調查

## 結論

資料庫連線池上限太小，尖峰時 request 等不到連線而逾時。已把上限從 10 調到 40。

## 證據

- 500 都發生在每天 09:00 與 14:00 的流量尖峰
- 錯誤 log 是 `pool timed out while waiting for an open connection`
- 調高上限後觀察三天沒有再發生

## 後續

- [ ] 替連線池使用率加上告警
";

const INFRA_NOTES: &str = "\
# Project Notes

## 部署

- 正式環境只從 main 部署，由 CI 觸發。
- 資料庫 migration 要先在 staging 跑過。
";

pub fn store() -> Store {
    let mut ids = Ids(0);
    let laptop = InstanceId::from_u64(1);
    let home = InstanceId::from_u64(2);

    // ---- 家用伺服器 · auth-service ---------------------------------------
    let auth = ProjectId::from_u64(ids.next());
    let oauth_goal = GoalId::from_u64(ids.next());
    let pr42 = ApprovalId::from_u64(ids.next());
    let spec = ArtifactId::from_u64(ids.next());
    let infra_report = ArtifactId::from_u64(ids.next());
    let infra_listener_approval = ApprovalId::from_u64(ids.next());

    let callback_task = TaskView {
        id: TaskId::from_u64(ids.next()),
        goal: oauth_goal,
        title: "後端 OAuth callback".into(),
        assignee: "worker · Claude Code".into(),
        summary: "補錯誤處理的測試".into(),
        state: TaskStateView::Running,
        branch: Some("agent/t31-oauth-callback".into()),
        elapsed_min: 12,
        started_at: None,
        finished_at: None,
        spent_tokens: 0,
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
        pr: None,
        pr_url: None,
        progress: "補 callback 的錯誤處理與測試".into(),
    };
    let button_task = TaskView {
        id: TaskId::from_u64(ids.next()),
        goal: oauth_goal,
        title: "前端登入按鈕".into(),
        assignee: "worker · 內建".into(),
        summary: "修改登入頁".into(),
        state: TaskStateView::Running,
        branch: Some("agent/t32-login-button".into()),
        elapsed_min: 8,
        started_at: None,
        finished_at: None,
        spent_tokens: 0,
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
        pr: None,
        pr_url: None,
        progress: "在登入頁加上「用 Google 登入」按鈕，正在跑元件測試".into(),
    };
    let integration_task = TaskView {
        id: TaskId::from_u64(ids.next()),
        goal: oauth_goal,
        title: "整合測試".into(),
        assignee: "worker · Claude Code".into(),
        summary: "等待 CI · PR #44".into(),
        state: TaskStateView::Waiting {
            on: "CI · PR #44".into(),
        },
        branch: Some("agent/t33-oauth-e2e".into()),
        elapsed_min: 21,
        started_at: None,
        finished_at: None,
        spent_tokens: 0,
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
        pr: Some("github.com/acme/auth-service#44".into()),
        pr_url: Some("https://github.com/acme/auth-service/pull/44".into()),
        progress: "開了 PR #44，等待 CI".into(),
    };
    let review_task = TaskView {
        id: TaskId::from_u64(ids.next()),
        goal: oauth_goal,
        title: "Spec 審查".into(),
        assignee: "reviewer subagent".into(),
        summary: "沒有阻擋意見".into(),
        state: TaskStateView::Done {
            outcome: "沒有阻擋意見".into(),
        },
        branch: None,
        elapsed_min: 3,
        started_at: None,
        finished_at: None,
        spent_tokens: 0,
        cost_cents: 9,
        steps: vec![TaskStep {
            text: "對照 spec 檢查 session 與舊密碼登入的相容性".into(),
            at: Some(at(10, 23)),
        }],
        thread: vec![],
        pr: None,
        pr_url: None,
        progress: String::new(),
    };
    let docs_task = TaskView {
        id: TaskId::from_u64(ids.next()),
        goal: oauth_goal,
        title: "更新登入 API 文件".into(),
        assignee: "worker · 內建".into(),
        summary: "在 docs/auth.md 補上 OAuth 的流程".into(),
        state: TaskStateView::Failed {
            attempts: 2,
            error: "docs 建置失敗：找不到 docs/SUMMARY.md".into(),
        },
        branch: Some("agent/t34-auth-docs".into()),
        elapsed_min: 6,
        started_at: None,
        finished_at: None,
        spent_tokens: 0,
        cost_cents: 14,
        steps: vec![TaskStep {
            text: "第 2 次建置仍失敗，停止".into(),
            at: Some(at(10, 47)),
        }],
        thread: vec![],
        pr: None,
        pr_url: None,
        progress: "第 2 次建置仍失敗".into(),
    };

    let oauth = GoalView {
        id: oauth_goal,
        title: "OAuth 登入遷移".into(),
        state: GoalState::Active,
        planner_note: "Planner · 5 個任務".into(),
        budget_used_pct: 38,
        tasks: vec![
            callback_task,
            button_task,
            integration_task,
            review_task,
            docs_task,
        ],
        // 和服務一樣由舊到新。
        team: vec![
            TeamPost {
                kind: TeamPostKind::Finding,
                from: "worker · 後端 OAuth callback".into(),
                at: at(10, 41),
                body: "auth 的測試要先啟動 redis。".into(),
                to: None,
            },
            TeamPost {
                kind: TeamPostKind::Handoff,
                from: "worker · 後端 OAuth callback".into(),
                at: at(10, 44),
                body: "callback 的路徑是 /auth/google/callback，整合測試可以直接打這個。".into(),
                to: Some("整合測試".into()),
            },
            TeamPost {
                kind: TeamPostKind::Question,
                from: "worker · 前端登入按鈕".into(),
                at: at(10, 55),
                body: "Google client id 要放哪個環境變數？".into(),
                to: None,
            },
            TeamPost {
                kind: TeamPostKind::Decision,
                from: "Planner".into(),
                at: at(10, 58),
                body: "client id 用 GOOGLE_CLIENT_ID，並寫進 .env.example。".into(),
                to: None,
            },
        ],
        // 和服務一樣由舊到新。
        activity: vec![
            GoalActivity {
                at: at(10, 12),
                text: "你開始了這個 Goal，交給 Planner".into(),
            },
            GoalActivity {
                at: at(10, 20),
                text: "規劃完成：拆成後端 callback、前端按鈕、整合測試、review 與文件 5 個任務，\
                       callback 先做，其他等它的路徑定下來。"
                    .into(),
            },
            GoalActivity {
                at: at(11, 2),
                text:
                    "callback 已開 PR；整合測試第 2 次建置仍失敗，已請 worker 先修 redis 啟動順序。"
                        .into(),
            },
        ],
        brief: "把登入改成 OAuth，先支援 Google。舊的密碼登入保留三個月，期間兩種方式並存；\
                session 過期時間改為 14 天（見 Spec「Session 處理」）。完成條件：\
                後端 callback、前端按鈕、整合測試都合併到 main。"
            .into(),
        quick: false,
        env: false,
    };

    let mut tool_reply = agent(
        "auth-service 負責人",
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
        "auth-service 負責人",
        AgentRole::Project,
        "好，已經照你改過的 spec 開了 Goal，Planner 拆成 4 個任務。",
    );
    goal_reply
        .cards
        .push(Card::GoalCreated { goal: oauth_goal });
    let mut approval_reply = agent("auth-service 負責人", AgentRole::Project, "");
    approval_reply.cards.push(Card::Approval { id: pr42 });
    let mut streaming = agent(
        "auth-service 負責人",
        AgentRole::Project,
        "前端登入按鈕還在改，整合測試正在等 CI。結果出來我再告訴你",
    );
    streaming.status = MsgStatus::Streaming;

    let auth_service = ProjectView {
        memory: auth_memory(&mut ids),
        id: auth,
        name: "auth-service".into(),
        agent_name: "auth-service 負責人".into(),
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
        artifacts: vec![
            ArtifactView {
                human_edits: vec![("Session 處理".into(), at(10, 20))],
                agent_writing: Some("測試計畫".into()),
                ..doc(
                    spec,
                    ArtifactKind::Spec,
                    "OAuth 登入遷移",
                    "v4",
                    4,
                    None,
                    OAUTH_SPEC,
                )
            },
            doc(
                ArtifactId::from_u64(ids.next()),
                ArtifactKind::Brief,
                "OAuth 登入遷移",
                "Goal 交接 · v1",
                1,
                Some(oauth_goal),
                OAUTH_BRIEF,
            ),
            doc(
                ArtifactId::from_u64(ids.next()),
                ArtifactKind::Ledger,
                "OAuth 登入遷移",
                "Planner 維護 · 4 個任務",
                7,
                Some(oauth_goal),
                OAUTH_LEDGER,
            ),
            doc(
                ArtifactId::from_u64(ids.next()),
                ArtifactKind::Report,
                "Spec 審查",
                "reviewer subagent · 沒有阻擋意見",
                1,
                Some(oauth_goal),
                OAUTH_REVIEW,
            ),
            ArtifactView {
                proposals: 1,
                ..doc(
                    ArtifactId::from_u64(ids.next()),
                    ArtifactKind::ProjectNotes,
                    "Project Notes",
                    "1 個待核准的提案",
                    3,
                    None,
                    AUTH_NOTES,
                )
            },
        ],
        repos: vec![],
        workdir: Some(WorkDir::Github {
            repo: "github.com/acme/auth-service".into(),
        }),
        merge_method: MergeMethod::Squash,
        env: auth_env(),
    };

    // ---- 家用伺服器 · 其他 project ----------------------------------------
    let web = ProjectId::from_u64(ids.next());
    let checkout_goal = GoalId::from_u64(ids.next());
    let flaky_task = TaskId::from_u64(ids.next());
    let web_app = ProjectView {
        memory: MemoryView::default(),
        id: web,
        name: "web-app".into(),
        agent_name: "web-app 負責人".into(),
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
            state: GoalState::Active,
            planner_note: "Planner · 2 個任務".into(),
            budget_used_pct: 71,
            tasks: vec![TaskView {
                id: flaky_task,
                goal: checkout_goal,
                title: "修正 e2e/checkout.spec.ts".into(),
                assignee: "worker · Claude Code".into(),
                summary: "CI 連續失敗 3 次".into(),
                state: TaskStateView::NeedsHuman {
                    reason: "CI 連續失敗 3 次，已停止自動修復".into(),
                },
                branch: Some("agent/t21-checkout".into()),
                elapsed_min: 46,
                started_at: None,
                finished_at: None,
                spent_tokens: 0,
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
                pr: Some("github.com/acme/web-app#57".into()),
                pr_url: Some("https://github.com/acme/web-app/pull/57".into()),
                progress: "第 3 次修復後 CI 仍失敗".into(),
            }],
            team: vec![TeamPost {
                kind: TeamPostKind::Blocker,
                from: "worker · 修正 e2e/checkout.spec.ts".into(),
                at: at(8, 7),
                body: "付款步驟的動畫在 CI 上時間不固定，試了 3 種等法都不穩，需要決定要不要改測試策略。"
                    .into(),
                to: None,
            }],
            activity: Vec::new(),
            brief: "結帳頁改版：新版地址表單與付款步驟。".into(),
            quick: false,
            env: false,
        }],
        artifacts: vec![
            doc(
                ArtifactId::from_u64(ids.next()),
                ArtifactKind::Spec,
                "結帳頁改版",
                "v2",
                2,
                None,
                "# 結帳頁改版\n\n## 目標\n\n地址表單與付款拆成兩步，手機上不用橫向捲動。\n\n\
                 ## 範圍\n\n- 新版地址表單\n- 付款步驟改成獨立頁面\n",
            ),
            doc(
                ArtifactId::from_u64(ids.next()),
                ArtifactKind::Brief,
                "結帳頁改版",
                "Goal 交接 · v1",
                1,
                Some(checkout_goal),
                "# 結帳頁改版\n\n## 完成條件\n\n- e2e 測試全部通過\n- 合併到 main\n",
            ),
            doc(
                ArtifactId::from_u64(ids.next()),
                ArtifactKind::Ledger,
                "結帳頁改版",
                "Planner 維護 · CI 卡住",
                5,
                Some(checkout_goal),
                "# 結帳頁改版 · Ledger\n\n## 任務\n\n\
                 - [ ] 修正 e2e/checkout.spec.ts（CI 連續失敗 3 次，等你決定）\n",
            ),
        ],
        repos: vec![],
        workdir: None,
        merge_method: MergeMethod::Squash,
        env: ProjectEnvView::default(),
    };

    let infra = ProjectView {
        memory: MemoryView::default(),
        id: ProjectId::from_u64(ids.next()),
        name: "infra".into(),
        agent_name: "infra 負責人".into(),
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
                    "infra 負責人",
                    AgentRole::Project,
                    "我提了一個新的 listener：main 分支 CI 失敗時自動開 Goal 修復，等你在 Inbox 核准。",
                )),
            ),
        ],
        goals: vec![],
        artifacts: vec![
            doc(
                infra_report,
                ArtifactKind::Report,
                "API 偶發 500 調查",
                "11/2 · Goal 已完成",
                2,
                None,
                INFRA_500_REPORT,
            ),
            doc(
                ArtifactId::from_u64(ids.next()),
                ArtifactKind::ProjectNotes,
                "Project Notes",
                "v2",
                2,
                None,
                INFRA_NOTES,
            ),
        ],
        repos: vec![],
        workdir: None,
        merge_method: MergeMethod::Squash,
        env: ProjectEnvView::default(),
    };
    let infra_id = infra.id;

    let docs = ProjectView {
        memory: MemoryView::default(),
        id: ProjectId::from_u64(ids.next()),
        name: "docs-site".into(),
        agent_name: "docs-site 負責人".into(),
        agent_state: AgentState::Idle,
        budget: Budget {
            spent_cents: 0,
            limit_cents: 500,
        },
        unread: 0,
        items: vec![],
        goals: vec![],
        artifacts: vec![doc(
            ArtifactId::from_u64(ids.next()),
            ArtifactKind::ProjectNotes,
            "Project Notes",
            "v1",
            1,
            None,
            "# Project Notes\n\n## 建置\n\n- 文件站用 `mdbook build`，輸出在 `book/`。\n",
        )],
        repos: vec![],
        workdir: None,
        merge_method: MergeMethod::Squash,
        env: ProjectEnvView::default(),
    };
    let docs_id = docs.id;
    let auth_id = auth;

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
        id: infra_report,
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
        actions: vec![
            NoticeAction::ViewAutomations,
            NoticeAction::ResumeAutomations,
        ],
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
    let dotfiles_id = ProjectId::from_u64(ids.next());
    let pr9 = ApprovalId::from_u64(ids.next());
    let shell_goal = GoalId::from_u64(ids.next());
    let mut dotfiles_reply = agent(
        "dotfiles 負責人",
        AgentRole::Project,
        "alias 整理好了，重複的 12 個合併、過期的 PATH 設定移除。PR #9 的 CI 都過了。",
    );
    dotfiles_reply.cards.push(Card::Approval { id: pr9 });
    let dotfiles = ProjectView {
        memory: MemoryView::default(),
        id: dotfiles_id,
        name: "dotfiles".into(),
        agent_name: "dotfiles 負責人".into(),
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
            state: GoalState::Active,
            planner_note: "直接交給 worker · 1 個任務".into(),
            budget_used_pct: 12,
            tasks: vec![TaskView {
                id: TaskId::from_u64(ids.next()),
                goal: shell_goal,
                title: "合併 alias 與 PATH".into(),
                assignee: "worker · Claude Code".into(),
                summary: "PR #9 · 等你核准".into(),
                state: TaskStateView::Waiting {
                    on: "核准 · PR #9".into(),
                },
                branch: Some("agent/t3-shell-cleanup".into()),
                elapsed_min: 9,
                started_at: None,
                finished_at: None,
                spent_tokens: 0,
                cost_cents: 35,
                steps: vec![TaskStep {
                    text: "開了 PR #9".into(),
                    at: Some(at(10, 40)),
                }],
                thread: vec![],
                pr: Some("github.com/me/dotfiles#9".into()),
                pr_url: Some("https://github.com/me/dotfiles/pull/9".into()),
                progress: "開了 PR #9，CI 都過了".into(),
            }],
            team: vec![],
            activity: Vec::new(),
            brief: "合併重複的 alias，移除過期的 PATH 設定。".into(),
            quick: true,
            env: false,
        }],
        artifacts: vec![doc(
            ArtifactId::from_u64(ids.next()),
            ArtifactKind::Brief,
            "整理 shell 設定",
            "Goal 交接 · v1",
            1,
            Some(shell_goal),
            "# 整理 shell 設定\n\n## 目標\n\n合併重複的 alias，移除過期的 PATH 設定。\n",
        )],
        repos: vec![],
        workdir: None,
        merge_method: MergeMethod::Squash,
        env: ProjectEnvView::default(),
    };
    let side_id = ProjectId::from_u64(ids.next());
    let side = ProjectView {
        memory: MemoryView::default(),
        id: side_id,
        name: "side-project".into(),
        agent_name: "side-project 負責人".into(),
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
                    "side-project 負責人",
                    AgentRole::Project,
                    "匯出功能的 Goal 開好了，但這台電腦的 Claude Code 還沒登入，worker 沒辦法開工。",
                )),
            ),
        ],
        goals: vec![],
        artifacts: vec![],
        repos: vec![],
        workdir: None,
        merge_method: MergeMethod::Squash,
        env: ProjectEnvView::default(),
    };

    // ---- 家用伺服器 · 自動化 -------------------------------------------------
    let home_schedules = vec![
        ScheduleView {
            id: ScheduleId::from_u64(ids.next()),
            project: Some(infra_id),
            prompt: "整理前一天失敗的 CI".into(),
            note: "30 天後過期".into(),
            creator: ScheduleCreator::Chat,
            cadence: "每天 08:00".into(),
            next: "明天 08:00".into(),
            next_after_pause: Some("週一 08:00".into()),
            enabled: true,
            history: vec![
                trigger(
                    "11/6 08:00 → 2 個失敗已整理",
                    "花費 $0.12",
                    TriggerStatus::Done,
                ),
                trigger("11/5 08:00 → 沒有失敗", "花費 $0.03", TriggerStatus::Done),
            ],
            next_at: None,
            expires_at: None,
            goal: None,
            finished: false,
        },
        ScheduleView {
            id: ScheduleId::from_u64(ids.next()),
            project: Some(auth_id),
            prompt: "提醒移除 legacy_login feature flag".into(),
            note: "單次".into(),
            creator: ScheduleCreator::Planner,
            cadence: "單次".into(),
            next: "11/10 10:00".into(),
            next_after_pause: None,
            enabled: true,
            history: vec![],
            next_at: None,
            expires_at: None,
            goal: None,
            finished: false,
        },
        ScheduleView {
            id: ScheduleId::from_u64(ids.next()),
            project: Some(docs_id),
            prompt: "檢查文件站建置與壞掉的連結".into(),
            note: "30 天後過期".into(),
            creator: ScheduleCreator::Chat,
            cadence: "每晚 22:00".into(),
            next: "今晚 22:00".into(),
            next_after_pause: Some("週一 22:00".into()),
            enabled: true,
            history: vec![trigger(
                "11/6 22:00 → 找到 1 個壞掉的連結",
                "花費 $0.08",
                TriggerStatus::Done,
            )],
            next_at: None,
            expires_at: None,
            goal: None,
            finished: false,
        },
    ];
    let home_listeners = vec![
        ListenerView {
            id: ListenerId::from_u64(ids.next()),
            project: Some(auth_id),
            condition: "issue 加上 agent label".into(),
            action: "開 Goal".into(),
            source: "GitHub".into(),
            source_detail: "個人帳號".into(),
            trust: TrustLevel::Members,
            resource: "github.com/me/auth-service".into(),
            event: "Issue 被加上 label".into(),
            matcher: "label = agent".into(),
            action_detail: "開 Goal，Brief 用以下模板：".into(),
            template: Some(
                "依照 Issue 的描述實作，先在 Brief 補上驗收條件；需求不清楚時在 Team 貼文發問，不要猜。"
                    .into(),
            ),
            budget_per_trigger_cents: Some(200),
            max_per_hour: Some(2),
            triggers_this_week: 2,
            state: ListenerState::Active,
            history: vec![
                trigger(
                    "#118 → 支援 refresh token",
                    "11/5 · 花費 $1.40",
                    TriggerStatus::Done,
                ),
                trigger(
                    "#112 → 錯誤訊息改成中文",
                    "11/3 · 目前 $0.60",
                    TriggerStatus::Running,
                ),
            ],
            spec: None,
        },
        ListenerView {
            id: ListenerId::from_u64(ids.next()),
            project: None,
            condition: "上游 LLM 狀態頁出現重大事故".into(),
            action: "通知對話".into(),
            source: "狀態頁".into(),
            source_detail: "LLM provider 公開狀態頁".into(),
            trust: TrustLevel::Public,
            resource: "service:openai/*".into(),
            event: "Incident 開啟".into(),
            matcher: "severity ≥ major".into(),
            action_detail:
                "只在各 project 的對話裡告訴對話 agent，由它檢查哪些任務受影響。Public 來源預設只能通知。"
                    .into(),
            template: None,
            budget_per_trigger_cents: None,
            max_per_hour: Some(1),
            triggers_this_week: 1,
            state: ListenerState::Active,
            history: vec![trigger(
                "API 錯誤率升高 → 通知 4 個專案",
                "11/6 23:10 · 沒有任務受影響",
                TriggerStatus::Notified,
            )],
            spec: None,
        },
        ListenerView {
            id: ListenerId::from_u64(ids.next()),
            project: Some(infra_id),
            condition: "main 分支 CI 失敗".into(),
            action: "開 Goal 修復".into(),
            source: "GitHub".into(),
            source_detail: "個人帳號".into(),
            trust: TrustLevel::Owned,
            resource: "github.com/me/infra".into(),
            event: "CI 執行結束".into(),
            matcher: "branch = main、conclusion = failure".into(),
            action_detail: "開 Goal「修好 main」，Brief 用以下模板：".into(),
            template: Some(
                "讀 CI log 找出失敗原因並修好；同一個 commit 修兩次還失敗就停下來問我。".into(),
            ),
            budget_per_trigger_cents: Some(200),
            max_per_hour: Some(2),
            triggers_this_week: 0,
            state: ListenerState::PendingApproval(infra_listener_approval),
            history: vec![],
            spec: None,
        },
    ];

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
            schedules: vec![],
            listeners: vec![],
            automation_pause: None,
            providers: mock_providers(None),
            model_auth: ModelAuthView::SignedIn {
                account: Some("you@example.com".into()),
            },
            pauses: vec![],
            custom_kinds: vec![],
            sources: vec![],
            github: GithubView::default(),
            worker: mock_worker(
                WorkerBackendKind::ClaudeCode,
                "/Users/you/Library/Application Support/pots",
            ),
            notifications: NotificationSettings::default(),
            user_md: UserMdView::default(),
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
            schedules: home_schedules,
            listeners: home_listeners,
            // 09:20 你請總管把週末的自動化全部暫停（見總管對話）。
            automation_pause: Some(AutomationPause {
                until: "週一 09:00".into(),
                until_short: "週一".into(),
                by: "由總管在 09:20 依你的要求暫停".into(),
            }),
            // Planner 用 Anthropic 的大模型、思考高；Explorer 用便宜的模型、不思考；
            // 對話 agent 跟使用中的 ChatGPT 相同，只把思考調成中。
            providers: ProvidersView {
                roles: vec![
                    RoleModelView {
                        role: ModelRole::Chat,
                        provider: None,
                        model: String::new(),
                        thinking: ThinkingLevel::Medium,
                    },
                    RoleModelView {
                        role: ModelRole::Planner,
                        provider: Some(ProviderId::from_u64(2)),
                        model: "claude-opus-4-1".into(),
                        thinking: ThinkingLevel::High,
                    },
                    RoleModelView {
                        role: ModelRole::Explorer,
                        provider: Some(ProviderId::from_u64(2)),
                        model: "claude-haiku-4-5".into(),
                        thinking: ThinkingLevel::Off,
                    },
                ],
                ..mock_providers(Some(ProviderView {
                    id: ProviderId::from_u64(2),
                    kind: ProviderKind::AnthropicCompatible,
                    name: "Anthropic".into(),
                    base_url: "https://api.anthropic.com".into(),
                    model: "claude-sonnet-4-5".into(),
                    key_hint: Some("…9f2c".into()),
                    headers: Vec::new(),
                }))
            },
            model_auth: ModelAuthView::SignedIn {
                account: Some("you@example.com".into()),
            },
            pauses: vec![],
            custom_kinds: vec![],
            sources: vec![],
            github: GithubView::default(),
            worker: mock_worker(WorkerBackendKind::Builtin, "/var/lib/pots"),
            notifications: NotificationSettings::default(),
            user_md: UserMdView::default(),
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
            task: None,
            approval: None,
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
            task: None,
            approval: None,
        },
        InboxItem {
            id: infra_listener_approval,
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
            task: None,
            approval: None,
        },
        InboxItem {
            id: ApprovalId::from_u64(ids.next()),
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
            task: None,
            approval: None,
        },
        InboxItem {
            id: ApprovalId::from_u64(ids.next()),
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
            task: None,
            approval: None,
        },
        InboxItem {
            id: ApprovalId::from_u64(ids.next()),
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
            task: None,
            approval: None,
        },
    ];

    Store {
        now: at(11, 7),
        instances,
        inbox,
        outbox: vec![],
    }
}

/// 用量頁的假報表：每個 project 的對話、各 Goal 的 Planner 與 worker，加上記憶整理。
/// 用量跟著時間範圍（`secs`）放大，切換範圍時看得出差別。
pub fn cost_report(inst: &InstanceView, secs: i64) -> CostReport {
    let days = (secs / 86_400).max(1) as u64;
    // 一天之後的用量遞減，不是單純乘上天數。
    let scale = |base: u64| base * (1 + (days - 1) * 3 / 5);
    let mut rows = Vec::new();
    let row = |project: &ProjectView,
               goal: Option<&GoalView>,
               agent: &str,
               billing: Billing,
               model: &str,
               runs: u64,
               input: u64,
               output: u64| {
        let (provider, runtime, category) = match (billing, agent) {
            (_, "observer" | "reflector") => ("ChatGPT", "builtin", CostCategory::Om),
            (Billing::Cli, _) => ("Claude Code", "claude_code", CostCategory::Work),
            (Billing::Api, _) => ("OpenAI", "builtin", CostCategory::Work),
            _ => ("ChatGPT", "builtin", CostCategory::Work),
        };
        let tokens = input + output;
        // API 有單價；訂閱用量不算金額；CLI 用 Claude Code 回報的費用。
        let estimated = (billing == Billing::Api)
            .then_some((input as f64 * 1.25 + output as f64 * 10.0) / 1_000_000.0);
        let reported = (billing == Billing::Cli)
            .then_some((input as f64 * 3.0 + output as f64 * 15.0) / 1_000_000.0);
        CostRow {
            project: project.id,
            project_name: project.name.clone(),
            goal: goal.map(|g| g.id),
            goal_title: goal.map(|g| g.title.clone()),
            agent: agent.into(),
            category,
            provider: provider.into(),
            billing,
            runtime: runtime.into(),
            model: model.into(),
            runs,
            input_tokens: input,
            output_tokens: output,
            duration_secs: runs * 95,
            reported_cost_usd: reported,
            estimated_cost_usd: estimated,
            priced_tokens: if billing == Billing::Api { tokens } else { 0 },
            unpriced_tokens: if billing == Billing::Subscription {
                tokens
            } else {
                0
            },
        }
    };
    for (i, p) in inst.projects.iter().enumerate() {
        let k = i as u64 + 1;
        rows.push(row(
            p,
            None,
            "chat",
            Billing::Subscription,
            "gpt-5.1",
            scale(12 / k + 2),
            scale(380_000 / k),
            scale(42_000 / k),
        ));
        rows.push(row(
            p,
            None,
            "observer",
            Billing::Subscription,
            "gpt-5.1-mini",
            scale(4),
            scale(96_000 / k),
            scale(8_000 / k),
        ));
        for (j, g) in p.goals.iter().enumerate() {
            let m = (j as u64 + 1) * k;
            rows.push(row(
                p,
                Some(g),
                "planner",
                Billing::Api,
                "gpt-5.1",
                scale(3),
                scale(260_000 / m),
                scale(31_000 / m),
            ));
            rows.push(row(
                p,
                Some(g),
                "worker",
                Billing::Cli,
                "claude-sonnet-4-5",
                scale(2),
                scale(1_450_000 / m),
                scale(88_000 / m),
            ));
        }
    }
    let mut totals = CostTotals::default();
    for r in &rows {
        let tokens = r.input_tokens + r.output_tokens;
        totals.runs += r.runs;
        totals.input_tokens += r.input_tokens;
        totals.output_tokens += r.output_tokens;
        totals.duration_secs += r.duration_secs;
        totals.reported_cost_usd += r.reported_cost_usd.unwrap_or(0.);
        totals.estimated_cost_usd += r.estimated_cost_usd.unwrap_or(0.);
        totals.priced_tokens += r.priced_tokens;
        totals.unpriced_tokens += r.unpriced_tokens;
        match r.billing {
            Billing::Subscription | Billing::Unknown => totals.subscription_tokens += tokens,
            Billing::Api => totals.api_tokens += tokens,
            Billing::Cli => totals.cli_tokens += tokens,
        }
        match r.category {
            CostCategory::Om => totals.om_tokens += tokens,
            CostCategory::Work => totals.work_tokens += tokens,
        }
    }
    let now = Timestamp::now();
    CostReport {
        since: Timestamp(now.0 - secs),
        until: now,
        generated_at: now,
        rows,
        totals,
        pricing: PricingInfo {
            source: "models.dev".into(),
            models: 412,
            updated_at: Some(Timestamp(now.0 - 3 * 86_400)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_only_needs_human_or_failed_tasks() {
        let mut store = store();
        let tasks: Vec<_> = store
            .instances
            .iter()
            .flat_map(|i| {
                i.projects
                    .iter()
                    .flat_map(|p| p.goals.iter())
                    .flat_map(|g| g.tasks.iter())
                    .map(move |t| (i.id, t.id, t.state.clone()))
            })
            .collect();
        store.outbox.clear();
        for (inst, task, _) in &tasks {
            store.retry_task(*inst, *task);
        }
        let retried: Vec<_> = tasks
            .iter()
            .filter(|(_, _, state)| retryable(state))
            .map(|(_, t, _)| *t)
            .collect();
        assert_eq!(retried.len(), 2, "假資料有一個需要你、一個失敗");
        let sent: Vec<_> = store
            .outbox
            .iter()
            .map(|(_, c)| match c {
                ClientCommand::RetryTask { task } => *task,
                other => panic!("不該送 {other:?}"),
            })
            .collect();
        assert_eq!(sent, retried);
        for (inst, task, _) in tasks.iter().filter(|(_, t, _)| retried.contains(t)) {
            assert_eq!(
                store.task(*inst, *task).unwrap().1.state,
                TaskStateView::Planned
            );
        }
    }

    #[test]
    fn draft_goals_start_or_discard_once() {
        let mut store = store();
        let (inst, goal) = store
            .instances
            .iter()
            .find_map(|i| {
                i.projects
                    .iter()
                    .flat_map(|p| p.goals.iter())
                    .find(|g| g.state.is_active())
                    .map(|g| (i.id, g.id))
            })
            .expect("假資料有進行中的 Goal");
        store.outbox.clear();
        // 不是草稿：不送。
        assert!(!store.start_goal(inst, goal));
        assert!(!store.discard_goal(inst, goal));
        assert!(store.outbox.is_empty());

        store.goal_mut(inst, goal).unwrap().state = GoalState::Draft;
        assert!(store.start_goal(inst, goal));
        assert_eq!(store.goal(inst, goal).unwrap().state, GoalState::Active);
        assert!(!store.start_goal(inst, goal), "開始之後不再送");

        store.goal_mut(inst, goal).unwrap().state = GoalState::Draft;
        assert!(store.discard_goal(inst, goal));
        assert!(matches!(
            store.goal(inst, goal).unwrap().state,
            GoalState::Cancelled { .. }
        ));
        let sent: Vec<_> = store.outbox.iter().map(|(_, c)| c.clone()).collect();
        assert!(matches!(
            sent.as_slice(),
            [
                ClientCommand::StartGoal { goal: a },
                ClientCommand::DiscardGoal { goal: b },
            ] if *a == goal && *b == goal
        ));
    }

    #[test]
    fn observation_edits_update_locally_and_queue_commands() {
        let mut store = store();
        let (inst, project) = store
            .instances
            .iter()
            .find_map(|i| {
                i.projects
                    .iter()
                    .find(|p| !p.memory.observations.is_empty())
                    .map(|p| (i.id, p.id))
            })
            .expect("有一個 project 有記憶");
        let ids: Vec<_> = store.project(inst, project).unwrap().memory.observations[..3]
            .iter()
            .map(|o| o.id)
            .collect();
        store.outbox.clear();

        store.pin_observation(inst, project, ids[1], true);
        store
            .edit_observation(inst, project, ids[2], "  改過的\n  ".into())
            .unwrap();
        store
            .edit_observation(inst, project, ids[2], "改過的".into())
            .unwrap();
        assert!(
            store
                .edit_observation(inst, project, ids[2], " ".into())
                .is_err()
        );
        assert!(
            store
                .edit_observation(
                    inst,
                    project,
                    ids[2],
                    "長".repeat(MAX_OBSERVATION_CHARS + 1)
                )
                .is_err()
        );
        store.delete_observation(inst, project, ids[0]);
        store.delete_observation(inst, project, ids[0]);

        let memory = &store.project(inst, project).unwrap().memory;
        assert!(!memory.observations.iter().any(|o| o.id == ids[0]));
        assert!(memory.observations[0].pinned);
        assert_eq!(memory.observations[1].text, "改過的");
        assert!(memory.observations[1].edited);
        let kinds: Vec<_> = store
            .outbox
            .iter()
            .map(|(_, c)| match c {
                ClientCommand::PinObservation { pinned: true, .. } => "pin",
                ClientCommand::EditObservation { text, .. } if text == "改過的" => "edit",
                ClientCommand::DeleteObservation { .. } => "delete",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["pin", "edit", "delete"]);
    }

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
        let home = InstanceId::from_u64(2);
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
    fn artifact_cards_resolve() {
        let store = store();
        for inst in &store.instances {
            let items = inst
                .projects
                .iter()
                .flat_map(|p| p.items.iter())
                .chain(inst.overseer.items.iter());
            for item in items {
                let (StreamItemKind::Agent(m) | StreamItemKind::Human(m)) = &item.kind else {
                    continue;
                };
                for card in &m.cards {
                    if let Card::Artifact { id, kind, .. } = card {
                        let (_, a) = store.artifact(inst.id, *id).expect("artifact exists");
                        assert_eq!(a.kind, *kind);
                    }
                }
            }
        }
    }

    #[test]
    fn listener_follows_its_approval() {
        let home = InstanceId::from_u64(2);
        let pending = |store: &Store| {
            store
                .instance(home)
                .unwrap()
                .listeners
                .iter()
                .find_map(|l| match l.state {
                    ListenerState::PendingApproval(a) => Some((l.id, a)),
                    _ => None,
                })
                .expect("mock has a pending listener")
        };

        let mut store = store();
        let (listener, approval) = pending(&store);
        assert!(store.act(approval, InboxAction::Approve));
        assert_eq!(
            store.listener(home, listener).unwrap().state,
            ListenerState::Active
        );

        let mut store = super::store();
        let (listener, approval) = pending(&store);
        assert!(store.act(approval, InboxAction::Reject));
        assert!(store.listener(home, listener).is_none());
    }

    #[test]
    fn queued_while_busy() {
        let mut store = store();
        let home = InstanceId::from_u64(2);
        let auth = store.instances[1].projects[0].id;
        store.send_to_project(home, auth, "加上 GitHub 登入".into(), SendMode::Queue);
        let p = store.project(home, auth).unwrap();
        let StreamItemKind::Human(m) = &p.items.last().unwrap().kind else {
            panic!("expected human message");
        };
        assert_eq!(m.status, MsgStatus::Queued);
    }
}
