//! 以 GPUI 的離屏 renderer 把主要畫面存成 PNG，方便不開視窗檢查版面。
//!
//! ```sh
//! cargo test -p pots-desktop --test screenshots
//! ```
//!
//! 圖檔寫到 `target/tmp/screenshots/`。這不是 golden-image 比對，只是產生圖檔。

fn main() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    shots::run();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    println!("screenshots: skipped; GPUI 在這個平台沒有離屏 renderer");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod shots {
    use std::path::PathBuf;
    use std::sync::Arc;

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, HeadlessAppContext, WindowBounds,
        WindowOptions, px, size,
    };
    use markdown_editor::{EditorCommand, MarkKind};
    use pots_desktop::app::{AutomationSection, ChannelTab, GoalTab, Panel, PotsApp, SettingsTab};
    use pots_desktop::mock;
    use pots_desktop::model::{ArtifactKind, ExecKind, InboxAction, TaskId, WorkerBackendKind};

    struct Shooter {
        cx: HeadlessAppContext,
        window: AnyWindowHandle,
        view: Entity<PotsApp>,
        out: PathBuf,
    }

    impl Shooter {
        fn new() -> Self {
            Self::with(|window, cx| PotsApp::new(mock::store(), window, cx))
        }

        fn with(
            make: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::Context<PotsApp>) -> PotsApp,
        ) -> Self {
            let mut cx = HeadlessAppContext::with_platform(
                gpui_kit::platform::current_platform(true).text_system(),
                Arc::new(gpui_kit::assets::AllAssets),
                gpui_kit::platform::current_headless_renderer,
            );
            cx.update(|cx| {
                gpui_kit::init(cx);
                pots_desktop::app::init(cx);
            });
            let (window, view) = cx
                .update(|cx| {
                    gpui_kit::open_window(
                        WindowOptions {
                            window_bounds: Some(WindowBounds::Windowed(Bounds {
                                origin: Default::default(),
                                size: size(px(1440.), px(900.)),
                            })),
                            focus: false,
                            show: false,
                            ..Default::default()
                        },
                        cx,
                        |window, cx| cx.new(|cx| make(window, cx)),
                    )
                })
                .expect("open window");
            let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("screenshots");
            std::fs::create_dir_all(&out).expect("create output dir");
            Self {
                cx,
                window,
                view,
                out,
            }
        }

        fn update(
            &mut self,
            f: impl FnOnce(&mut PotsApp, &mut gpui_kit::Window, &mut gpui_kit::Context<PotsApp>),
        ) {
            let view = self.view.clone();
            self.cx
                .update_window(self.window, |_, window, cx| {
                    view.update(cx, |app, cx| f(app, window, cx));
                })
                .expect("update window");
        }

        /// 直接對視窗送事件；點擊的 listener 會自己更新 `PotsApp`，不能再包一層 `update`。
        fn act(&mut self, f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App)) {
            self.cx
                .update_window(self.window, |_, window, cx| f(window, cx))
                .expect("update window");
        }

        fn view_settings_closed(&mut self) -> bool {
            let view = self.view.clone();
            self.cx.update(|cx| view.read(cx).settings.is_none())
        }

        /// 等連線執行緒送來的事件套用完，直到 `done` 成立（最多 10 秒）。
        fn wait_until(&mut self, what: &str, done: impl Fn(&PotsApp) -> bool) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                self.cx.run_until_parked();
                let view = self.view.clone();
                if self.cx.update(|cx| done(view.read(cx))) {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "等不到：{what}");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        fn shot(&mut self, name: &str) {
            // 兩次 frame：第一次排版，第二次套用 scroll_to_bottom 等延後的捲動。
            for _ in 0..2 {
                self.cx
                    .update_window(self.window, |_, window, cx| window.render_frame(cx))
                    .expect("render frame");
                self.cx.run_until_parked();
            }
            let image = self
                .cx
                .capture_screenshot(self.window)
                .expect("offscreen rendering must be available");
            let path = self.out.join(format!("{name}.png"));
            image.save(&path).expect("save png");
            println!("{}", path.display());
        }
    }

    pub fn run() {
        mock_screens();
        live_screens();
    }

    /// 連到真正的服務（腳本化的模型）：還沒設定 provider → 在設定頁新增 OpenAI 相容 API
    /// → 測試連線 → 儲存 → 建立 project → 傳訊息 → 收到串流回覆 → agent 開 Goal、Planner
    /// 拆任務 → 在編輯器改 Brief 並儲存 → 版本紀錄 → 設定 GitHub token → Worker 設定（被拒絕、
    /// 改好後儲存）→ project 監看 repo、agent 建立排程 → 自動化頁新增 listener。
    fn live_screens() {
        use pots_desktop::connection::{Endpoint, Target};
        use pots_desktop::model::*;
        use pots_llm::testing::{FnProvider, ScriptedProvider, ScriptedTurn};
        use pots_llm::{Item, LlmError, ModelProvider, ModelRequest};

        struct Fixed(Target);
        impl Endpoint for Fixed {
            fn locate(&mut self) -> Result<Target, String> {
                Ok(self.0.clone())
            }
        }

        /// 每個 API provider 都接到同一個腳本化的模型。
        struct ScriptedApi(Arc<FnProvider>);

        #[async_trait::async_trait]
        impl pots_server::ApiProviderFactory for ScriptedApi {
            fn build(
                &self,
                _: ProviderKind,
                _: &url::Url,
                _: &str,
                _: &[pots_proto::ProviderHeader],
            ) -> Result<Arc<dyn ModelProvider>, String> {
                Ok(self.0.clone())
            }

            async fn list_models(
                &self,
                _: ProviderKind,
                _: &url::Url,
                _: &str,
                _: &[pots_proto::ProviderHeader],
            ) -> Result<Vec<ModelOption>, LlmError> {
                Ok(["gpt-4.1", "gpt-4.1-mini", "o4-mini"]
                    .into_iter()
                    .map(|m| ModelOption {
                        id: m.into(),
                        display_name: m.into(),
                    })
                    .collect())
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        // 對話 agent：一般訊息回一段串流文字；「開 Goal」時呼叫 create_goal。
        // Planner：拆兩個任務、建 Ledger、貼里程碑。
        fn fake_model(req: &ModelRequest) -> ScriptedTurn {
            let results = req
                .input
                .iter()
                .rev()
                .take_while(|i| !matches!(i, Item::User { .. }))
                .filter(|i| matches!(i, Item::ToolResult { .. }))
                .count();
            let last_user = req.input.iter().rev().find_map(|i| match i {
                Item::User { text } => Some(text.as_str()),
                _ => None,
            });
            let planner = req
                .instructions
                .as_deref()
                .is_some_and(|i| i.starts_with("你是 Pots 的 Planner"));
            if planner {
                let edited = matches!(
                    req.input.first(),
                    Some(Item::User { text }) if text.contains("使用者直接修改了 Brief")
                );
                if edited {
                    return ScriptedTurn::reply("看過新的 Brief，規劃不用改");
                }
                return match results {
                    0 => ScriptedTurn::new()
                        .tool_call(
                            "t1",
                            "create_task",
                            r#"{"title":"Google OAuth callback","summary":"新增 /auth/google/callback 並換發 session"}"#,
                        )
                        .tool_call(
                            "t2",
                            "create_task",
                            r#"{"title":"登入頁按鈕","summary":"在登入頁加上「用 Google 登入」"}"#,
                        )
                        .tool_call(
                            "t3",
                            "write_artifact",
                            r###"{"kind":"Ledger","edits":[{"op":"replace_all","body_md":"# Ledger\n\n## 任務\n\n- [ ] Google OAuth callback\n- [ ] 登入頁按鈕"}]}"###,
                        )
                        .tool_call("t4", "post_milestone", r#"{"text":"已拆成 2 個任務，等 worker 接手"}"#),
                    _ => ScriptedTurn::reply("拆成 2 個任務，Ledger 已建立"),
                };
            }
            match (last_user, results) {
                (Some(t), 0) if t.contains("排程") => ScriptedTurn::new().tool_call(
                    "s1",
                    "schedule",
                    r#"{"kind":"every","cron":"0 9 * * 1-5","prompt":"整理昨天失敗的 CI，有需要就開 Goal"}"#,
                ),
                (Some(t), 0) if t.contains("listener") => ScriptedTurn::new().tool_call(
                    "l1",
                    "create_listener",
                    r#"{"kinds":["issue.labeled"],"fields":[{"field":"label","any_of":["agent"]}],"action":"create_goal","template":"處理這個 issue"}"#,
                ),
                (Some(t), 0) if t.contains("開 Goal") => ScriptedTurn::new().tool_call(
                    "g1",
                    "create_goal",
                    r###"{"title":"Google 登入","brief_md":"## 目標\n\n使用者可以用 Google 帳號登入。\n\n## 約束\n\n- 既有的密碼登入保留\n\n## 已決定\n\n- 用 OAuth 2.0 授權碼流程"}"###,
                ),
                (_, n) if n > 0 => ScriptedTurn::reply("開好了，Planner 會在右邊拆任務。"),
                _ => ScriptedTurn::new().text_deltas([
                    "收到！",
                    "這是接上本機服務後的第一則回覆，",
                    "用的是設定頁新增的 OpenAI 相容 provider。",
                ]),
            }
        }
        let model = FnProvider::new("scripted", fake_model);
        let deps = pots_server::Deps::new(
            ScriptedProvider::new(Vec::<ScriptedTurn>::new()),
            "auto",
            Arc::new(pots_server::FakeModelAuth::signed_out()),
        )
        .with_api_providers(Arc::new(ScriptedApi(model)));
        // GitHub API 指到沒有服務的本機 port：截圖不會連到真的 GitHub。
        let mut config = pots_server::Config::desktop(dir.path());
        config.watcher.github_api = "http://127.0.0.1:9".into();
        let handle = runtime
            .block_on(pots_server::start(config, deps))
            .expect("start server");
        let target = Target {
            addr: handle.addr,
            token: handle.token.as_str().to_string(),
        };

        let mut s = Shooter::with(move |window, cx| PotsApp::new_live(Fixed(target), window, cx));
        // 截圖不送真的系統通知，記下來檢查。
        let notices = Arc::new(pots_desktop::notify::Recorder::default());
        {
            let notices = notices.clone();
            s.update(move |app, _, _| {
                app.notifier = Some(pots_desktop::notify::Notifier::new(notices));
            });
        }
        s.wait_until("快照與設定提示", |app| {
            app.current_instance().connection == ConnectionState::Connected
                && app.store.pending(None) == 1
        });
        s.shot("11-live-needs-provider");

        // Inbox 的「開啟設定」→ 新增 OpenAI 相容 API → 測試連線。
        s.update(|app, window, cx| {
            let id = pots_desktop::sync::model_login_item_id(app.instance);
            app.act(id, InboxAction::OpenSettings, window, cx);
            app.edit_provider(None, ProviderKind::OpenAiCompatible, window, cx);
            let editor = app.provider_editor.as_ref().expect("表單已開啟");
            editor
                .api_key
                .update(cx, |state, cx| state.set_value("sk-test-1234", window, cx));
            app.test_provider(cx);
        });
        s.wait_until("測試連線結果", |app| {
            app.provider_editor
                .as_ref()
                .is_some_and(|e| matches!(e.test, pots_desktop::app::ProviderTest::Ok(_)))
        });
        s.update(|app, window, cx| app.pick_model("gpt-4.1".into(), window, cx));
        s.shot("12-live-settings-editor");

        // 模型的清單是下拉選單（含搜尋列）：捲到看得見的地方、打開再截一張。
        s.update(|app, _, cx| {
            app.settings_scroll.scroll_to_bottom();
            cx.notify();
        });
        s.act(|window, cx| window.render_frame(cx));
        s.act(|window, cx| window.click("provider-model-picker", cx));
        // 下拉選單的進場動畫 150ms，跑完才截，不然只拍到全透明的一張。
        s.cx.advance_clock(std::time::Duration::from_millis(200));
        s.act(|window, cx| window.render_frame(cx));
        s.shot("12b-live-settings-model-menu");

        // 搜尋列：打字就篩選清單。
        s.act(|window, cx| window.input("mini", cx));
        s.shot("12c-live-settings-model-search");

        s.update(|app, _, cx| app.save_provider(cx));
        s.wait_until("儲存並設為使用中", |app| {
            app.current_instance()
                .providers
                .active()
                .is_some_and(|p| p.kind == ProviderKind::OpenAiCompatible)
                && app.store.pending(None) == 0
        });
        s.shot("13-live-settings");

        s.update(|app, _, cx| {
            let instance = app.instance;
            app.store.outbox.push((
                instance,
                ClientCommand::CreateProject {
                    name: "pots".into(),
                },
            ));
            cx.notify();
        });
        s.wait_until("建立 project", |app| {
            !app.current_instance().projects.is_empty()
        });
        s.update(|app, window, cx| {
            let instance = app.instance;
            let project = app.current_instance().projects[0].id;
            app.open_project(instance, project, window, cx);
            app.composer.update(cx, |state, cx| {
                state.set_value("你好，接上了嗎？", window, cx)
            });
            app.send_composer(window, cx);
        });
        s.wait_until("agent 回覆完成", |app| {
            app.current_project().is_some_and(|p| {
                p.agent_state == AgentState::Idle
                    && p.items.iter().any(|i| {
                        matches!(&i.kind, StreamItemKind::Agent(m) if m.status == MsgStatus::Done)
                    })
            })
        });
        s.shot("14-live-chat");

        // agent 開 Goal → Planner 拆任務：對話裡出現 Goal 卡片與里程碑，右欄是 Goal 面板。
        s.update(|app, window, cx| {
            app.close_settings(cx);
            app.composer
                .update(cx, |state, cx| state.set_value("開 Goal 吧", window, cx));
            app.send_composer(window, cx);
        });
        s.wait_until("Planner 拆完任務", |app| {
            app.current_project().is_some_and(|p| {
                p.goals
                    .first()
                    .is_some_and(|g| g.tasks.len() == 2 && g.planner_note.starts_with("拆成"))
                    && p.agent_state == AgentState::Idle
            })
        });
        s.update(|app, _, cx| {
            let goal = app.current_project().unwrap().goals[0].id;
            app.open_panel(
                Some(Panel::Goal(goal, pots_desktop::app::GoalTab::Tasks)),
                cx,
            );
        });
        s.shot("15-live-goal");

        // 在編輯器改 Brief 並儲存：伺服器回新的版本，區段標成你改過。
        s.update(|app, window, cx| {
            let brief = app
                .current_project()
                .and_then(|p| p.artifacts.iter().find(|a| a.kind == ArtifactKind::Brief))
                .map(|a| a.id)
                .expect("有 Brief");
            app.open_artifact(brief, window, cx);
            let editor = app.artifact_editor.as_ref().expect("編輯器已開啟");
            let section = editor
                .sections
                .iter()
                .find(|s| s.heading.as_deref() == Some("約束"))
                .map(|s| s.editor.clone())
                .expect("有「約束」");
            section.update(cx, |state, cx| {
                let _ = state.run(EditorCommand::SelectAll, cx);
                let _ = state.run(EditorCommand::ToggleMark(MarkKind::Bold), cx);
            });
        });
        s.update(|app, _, cx| app.save_artifact(cx));
        s.wait_until("Brief 存成 v2", |app| {
            app.current_project().is_some_and(|p| {
                p.artifacts.iter().any(|a| {
                    a.kind == ArtifactKind::Brief && a.rev == 2 && !a.human_edits.is_empty()
                })
            }) && app
                .artifact_editor
                .as_ref()
                .is_some_and(|e| !e.dirty && !e.saving)
        });
        s.shot("16-live-brief-saved");

        s.update(|app, _, cx| app.toggle_artifact_history(cx));
        s.wait_until("版本紀錄", |app| {
            app.artifact_editor
                .as_ref()
                .and_then(|e| e.history.as_ref())
                .is_some_and(|h| h.revisions.as_ref().is_some_and(|r| r.len() == 2))
        });
        s.shot("17-live-brief-history");

        // 設定視窗的 GitHub 類別：貼上 token、儲存，只看得到末四碼。
        s.update(|app, window, cx| {
            app.open_settings(pots_desktop::app::SettingsTab::Github, cx);
            app.edit_github(window, cx);
            let editor = app.github_editor.as_ref().expect("表單已開啟");
            editor.token.update(cx, |state, cx| {
                state.set_value("ghp_screenshot_1234", window, cx)
            });
            app.save_github(cx);
        });
        s.wait_until("GitHub token 已儲存", |app| {
            app.current_instance().github.token_hint.as_deref() == Some("…1234")
                && app.github_editor.is_none()
        });
        s.shot("18-live-github");

        // 「Worker」：不合法的容器執行檔被服務拒絕，原因留在表單；改好後儲存。
        s.update(|app, window, cx| {
            app.open_settings(pots_desktop::app::SettingsTab::Worker, cx);
            app.edit_worker(window, cx);
            app.set_worker_backend(WorkerBackendKind::ClaudeCode, cx);
            let editor = app.worker_editor.as_ref().expect("表單已開啟");
            editor
                .runtime
                .update(cx, |state, cx| state.set_value("docker run", window, cx));
            app.save_worker(cx);
        });
        s.wait_until("worker 設定被拒絕", |app| {
            app.worker_editor
                .as_ref()
                .is_some_and(|e| !e.saving && e.error.is_some())
        });
        s.shot("18a-live-worker-rejected");
        s.update(|app, window, cx| {
            let editor = app.worker_editor.as_ref().expect("表單還在");
            editor
                .runtime
                .update(cx, |state, cx| state.set_value("podman", window, cx));
            app.set_worker_parallel(4, cx);
            app.save_worker(cx);
        });
        s.wait_until("worker 設定已儲存", |app| {
            let w = &app.current_instance().worker;
            w.backend == WorkerBackendKind::ClaudeCode
                && w.runtime == "podman"
                && w.max_parallel == 4
                && w.login_command.contains("podman run")
                && app.worker_editor.is_none()
        });
        s.shot("18b-live-worker");

        // 頻道「自動化」分頁：監看 repo；agent 建立排程，對話裡出現卡片。
        s.update(|app, window, cx| {
            app.close_settings(cx);
            app.open_panel(None, cx);
            let project = app.current_project().unwrap().id;
            app.edit_repos(project, window, cx);
            let editor = app.repo_editor.as_ref().expect("repo 表單");
            editor
                .input
                .update(cx, |state, cx| state.set_value("me/pots", window, cx));
            app.save_repos(cx);
            app.composer.update(cx, |state, cx| {
                state.set_value("平日早上幫我排程整理 CI", window, cx)
            });
            app.send_composer(window, cx);
        });
        s.wait_until("repo 與排程", |app| {
            app.current_project().is_some_and(|p| {
                p.repos == ["github.com/me/pots"] && p.agent_state == AgentState::Idle
            }) && app.current_instance().schedules.len() == 1
        });
        s.update(|app, _, cx| {
            app.channel_tab = pots_desktop::app::ChannelTab::Automations;
            cx.notify();
        });
        s.shot("19-live-project-automations");

        // 自動化頁：新增「issue 加上 agent label → 開 Goal」的 listener。
        s.update(|app, window, cx| {
            app.open_automations(pots_desktop::app::AutomationSection::Listeners, cx);
            app.new_listener(window, cx);
        });
        s.shot("20-live-new-listener");
        // 外部來源的表單（RSS／探測／狀態頁／通用 webhook）。
        s.update(|app, window, cx| {
            app.new_source(window, cx);
        });
        s.shot("20a-live-new-source");
        s.update(|app, _, cx| app.cancel_source_form(cx));
        s.update(|app, _, cx| app.save_listener_form(cx));
        s.wait_until("listener 已建立", |app| {
            app.current_instance().listeners.len() == 1 && app.listener_form.is_none()
        });
        s.update(|app, _, cx| {
            let id = app.current_instance().listeners[0].id;
            app.open_panel(Some(Panel::Listener(id)), cx);
        });
        s.shot("21-live-automations");

        // agent 提案 listener：Inbox 多一筆核准並推播（你不在那個 project）；對話裡是核准卡片。
        s.update(|app, _, cx| {
            let instance = app.instance;
            let project = app.current_instance().projects[0].id;
            app.open_inbox(Some(instance), cx);
            app.store.send_to_project(
                instance,
                project,
                "幫我提案一個 listener：issue 加上 agent label 就開 Goal".into(),
                SendMode::Queue,
            );
            cx.notify();
        });
        s.wait_until("listener 提案等核准", |app| {
            app.store.inbox.iter().any(|i| {
                i.is_open()
                    && matches!(
                        i.approval.as_ref().map(|a| &a.action),
                        Some(ApprovalActionView::CreateListener { .. })
                    )
            })
        });
        assert_eq!(notices.0.lock().unwrap().len(), 1, "新的核准推播一次");
        s.shot("22-live-inbox-approval");
        s.update(|app, window, cx| {
            let instance = app.instance;
            let project = app.current_instance().projects[0].id;
            app.open_project(instance, project, window, cx);
            app.channel_tab = pots_desktop::app::ChannelTab::Chat;
        });
        s.wait_until("回覆完成", |app| {
            app.current_project()
                .is_some_and(|p| p.agent_state == AgentState::Idle)
        });
        s.shot("23-live-approval-card");

        // 拒絕：先填理由，服務撤掉提案的 listener。
        s.update(|app, window, cx| {
            let id = app
                .store
                .inbox
                .iter()
                .find(|i| i.is_open() && i.approval.is_some())
                .map(|i| i.id)
                .expect("待核准");
            app.act(id, InboxAction::Reject, window, cx);
            let d = app.reject_dialog.as_ref().expect("拒絕對話框");
            d.input.update(cx, |state, cx| {
                state.set_value("先不要自動開工", window, cx)
            });
        });
        s.shot("24-live-reject-dialog");
        s.update(|app, _, cx| app.confirm_reject(cx));
        s.wait_until("提案被拒絕", |app| {
            app.store.inbox.iter().any(|i| {
                i.approval
                    .as_ref()
                    .is_some_and(|a| a.state == ApprovalState::Rejected)
            }) && app.current_instance().listeners.len() == 1
        });

        // 設定的「通知」：開啟靜音時段。
        s.update(|app, window, cx| {
            app.open_settings(pots_desktop::app::SettingsTab::Notifications, cx);
            app.edit_notifications(window, cx);
            app.toggle_notification_flag(true, cx);
            app.save_notifications(cx);
        });
        s.wait_until("通知設定已儲存", |app| {
            app.current_instance().notifications.quiet && app.notification_editor.is_none()
        });
        s.shot("25-live-settings-notifications");

        // 「USER.md」：直接編輯、儲存。
        s.update(|app, window, cx| {
            app.open_settings(pots_desktop::app::SettingsTab::UserMd, cx);
            app.edit_user_md(window, cx);
            let e = app.user_md_editor.as_ref().expect("USER.md 表單");
            e.input.update(cx, |state, cx| {
                state.set_value(
                    "## 語言\n\n用繁體中文回答。\n\n## commit\n\n用 Conventional Commits。",
                    window,
                    cx,
                )
            });
            app.save_user_md(cx);
        });
        s.wait_until("USER.md 已儲存", |app| {
            app.current_instance().user_md.rev == 1 && app.user_md_editor.is_none()
        });
        s.shot("26-live-settings-user-md");

        handle.shutdown();
        runtime.block_on(handle.wait()).expect("shutdown");
    }

    fn mock_screens() {
        let mut s = Shooter::new();
        s.shot("01-channel-goal");

        // Goal 面板：任務依狀態分組（進度、需要你、失敗的原因、PR），Team 分頁新的在上面。
        s.update(|app, _, cx| {
            let goal = app
                .current_project()
                .and_then(|p| p.goals.first())
                .map(|g| g.id)
                .expect("auth-service 有 Goal");
            app.open_panel(Some(Panel::Goal(goal, GoalTab::Tasks)), cx);
        });
        s.shot("01a-goal-tasks");
        s.update(|app, _, cx| {
            let Some(Panel::Goal(goal, _)) = app.panel else {
                panic!("右欄應該是 Goal");
            };
            app.open_panel(Some(Panel::Goal(goal, GoalTab::Team)), cx);
        });
        s.shot("01b-goal-team");

        // web-app：需要你決定的任務（原因）與阻礙貼文。之後回到 auth-service。
        s.update(|app, window, cx| {
            let home = app.store.instances[1].id;
            let web = app.store.instances[1].projects[1].id;
            app.open_project(home, web, window, cx);
            let goal = app.current_project().unwrap().goals[0].id;
            app.open_panel(Some(Panel::Goal(goal, GoalTab::Tasks)), cx);
        });
        s.shot("01c-goal-needs-human");
        // 「重試」：任務回到「已規劃」，指令留在 outbox（假資料模式不送）。
        s.update(|app, _, cx| {
            let task = app.current_project().unwrap().goals[0].tasks[0].id;
            app.retry_task(task, cx);
            assert!(app.store.outbox.iter().any(|(_, c)| matches!(
                c,
                pots_desktop::model::ClientCommand::RetryTask { task: t } if *t == task
            )));
        });
        s.shot("01d-goal-retried");
        s.update(|app, window, cx| {
            let home = app.store.instances[1].id;
            let auth = app.store.instances[1].projects[0].id;
            app.open_project(home, auth, window, cx);
        });

        s.update(|app, _, cx| {
            let task = app
                .current_project()
                .and_then(|p| p.goals.first())
                .and_then(|g| g.tasks.first())
                .map(|t| t.id)
                .unwrap_or(TaskId::from_u64(0));
            app.open_panel(Some(Panel::Task(task)), cx);
        });
        s.shot("02-task-thread");

        s.update(|app, _, cx| app.open_inbox(None, cx));
        s.shot("03-inbox-global");

        s.update(|app, window, cx| {
            let first = app.store.inbox[0].id;
            app.act(first, InboxAction::Approve, window, cx);
            let home = app.store.instances[1].id;
            app.open_inbox(Some(home), cx);
        });
        s.shot("04-inbox-instance-after-approve");

        s.update(|app, window, cx| {
            let home = app.store.instances[1].id;
            app.select_instance(home, window, cx);
            app.open_overseer(window, cx);
        });
        s.shot("05-overseer");

        s.update(|app, window, cx| {
            let laptop = app.store.instances[0].id;
            app.select_instance(laptop, window, cx);
        });
        s.shot("06-laptop-overseer");

        s.update(|app, window, cx| {
            let laptop = app.store.instances[0].id;
            let dotfiles = app.store.instances[0].projects[0].id;
            app.open_project(laptop, dotfiles, window, cx);
        });
        s.shot("06b-laptop-dotfiles");

        // 經由真的輸入框送出：agent 忙磌時訊息會標成「排隊中」。
        s.update(|app, window, cx| {
            let home = app.store.instances[1].id;
            app.select_instance(home, window, cx);
            app.composer.update(cx, |state, cx| {
                state.set_value("順便加上 GitHub 登入，等 Google 的做完再開始。", window, cx)
            });
            app.send_composer(window, cx);
        });
        s.shot("07-queued-message");

        // 對話裡的 Spec 卡片→右欄編輯器；切到 Artifacts 分頁看列表。
        s.update(|app, window, cx| {
            let spec = app
                .current_project()
                .and_then(|p| p.artifacts.iter().find(|a| a.kind == ArtifactKind::Spec))
                .map(|a| a.id)
                .expect("auth-service 有 Spec");
            app.open_artifact(spec, window, cx);
            app.channel_tab = ChannelTab::Artifacts;
        });
        s.shot("08-artifact-editor");

        // 把「範圍」整段加粗：這段標成你改過。agent 正在寫的「測試計畫」唯讀，加粗不會生效。
        s.update(|app, _, cx| {
            let editor = app.artifact_editor.as_ref().expect("編輯器已開啟");
            let section = |name: &str| {
                editor
                    .sections
                    .iter()
                    .find(|s| s.heading.as_deref() == Some(name))
                    .map(|s| s.editor.clone())
                    .expect("區段存在")
            };
            for name in ["範圍", "測試計畫"] {
                section(name).update(cx, |state, cx| {
                    let _ = state.run(EditorCommand::SelectAll, cx);
                    let _ = state.run(EditorCommand::ToggleMark(MarkKind::Bold), cx);
                });
            }
        });
        // 修改先留在編輯器裡（未儲存），按「儲存」才寫回。
        s.update(|app, _, _| {
            let Some(Panel::Artifact(id)) = app.panel else {
                panic!("右欄應該是 artifact");
            };
            assert!(
                app.artifact_editor.as_ref().unwrap().dirty,
                "有未儲存的修改"
            );
            let a = app.store.artifact(app.instance, id).unwrap().1;
            assert!(!a.body.contains("**後端新增"), "還沒儲存");
        });
        s.shot("08b-artifact-unsaved");
        s.update(|app, _, cx| app.save_artifact(cx));
        s.update(|app, _, _| {
            let Some(Panel::Artifact(id)) = app.panel else {
                panic!("右欄應該是 artifact");
            };
            assert!(!app.artifact_editor.as_ref().unwrap().dirty, "已儲存");
            let a = app.store.artifact(app.instance, id).unwrap().1;
            assert!(a.body.contains("**後端新增"), "儲存後寫回全文");
            // 整段加粗連標題也變成「**範圍**」，所以用包含比對。
            assert!(
                a.human_edits.iter().any(|(h, _)| h.contains("範圍")),
                "標示改過的區段"
            );
            assert!(a.edited_at("測試計畫").is_none(), "agent 寫入中的區段唯讀");
        });
        s.shot("08c-artifact-saved");

        s.update(|app, _, cx| {
            app.open_automations(AutomationSection::Listeners, cx);
            let first = app.current_instance().listeners[0].id;
            app.open_panel(Some(Panel::Listener(first)), cx);
        });
        s.shot("09-automations");

        s.update(|app, window, cx| {
            let home = app.store.instances[1].id;
            let auth = app.store.instances[1].projects[0].id;
            app.open_project(home, auth, window, cx);
            app.channel_tab = ChannelTab::Automations;
            let schedule = app
                .current_instance()
                .schedules_for(Some(auth))
                .next()
                .map(|s| s.id)
                .expect("auth-service 有排程");
            app.open_panel(Some(Panel::Schedule(schedule)), cx);
        });
        s.shot("10-channel-automations");

        // 「記憶」分頁：觀察紀錄，修正其中一條。
        s.update(|app, window, cx| {
            app.channel_tab = ChannelTab::Memory;
            app.open_panel(None, cx);
            let second = app
                .current_project()
                .and_then(|p| p.memory.observations.get(1))
                .map(|o| o.id)
                .expect("auth-service 有記憶");
            app.edit_observation(second, window, cx);
        });
        s.shot("10a-channel-memory");
        s.update(|app, _, cx| app.cancel_observation_edit(cx));

        s.update(|app, _, cx| app.open_settings(SettingsTab::Providers, cx));
        s.shot("10b-settings");

        s.update(|app, _, cx| app.open_settings(SettingsTab::About, cx));
        s.shot("10c-settings-about");

        s.update(|app, window, cx| {
            app.open_settings(SettingsTab::Memory, cx);
            app.edit_memory_model(window, cx);
        });
        s.shot("10d-settings-memory");

        // 「Worker」：家用伺服器是內建 worker；改成 Claude Code、本機執行時出現登入指令與警告。
        s.update(|app, _, cx| app.open_settings(SettingsTab::Worker, cx));
        s.shot("10e-settings-worker");
        s.update(|app, window, cx| {
            app.edit_worker(window, cx);
            app.set_worker_backend(WorkerBackendKind::ClaudeCode, cx);
            app.set_worker_exec(ExecKind::Local, cx);
        });
        s.shot("10f-settings-worker-edit");
        s.update(|app, _, cx| {
            app.set_worker_exec(ExecKind::Container, cx);
            app.save_worker(cx);
        });
        s.update(|app, _, _| {
            assert!(
                app.store.outbox.iter().all(|(_, c)| !matches!(
                    c,
                    pots_desktop::model::ClientCommand::SaveWorkerSettings { .. }
                )),
                "假資料模式不送出"
            );
            assert!(!app.worker_editor.as_ref().unwrap().saving);
        });

        s.update(|app, _, cx| app.close_settings(cx));
        assert!(s.view_settings_closed(), "關閉後沒有設定視窗");
    }
}
