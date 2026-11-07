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
    use pots_desktop::app::{AutomationSection, ChannelTab, Panel, PotsApp, SettingsTab};
    use pots_desktop::mock;
    use pots_desktop::model::{ArtifactKind, InboxAction, TaskId};

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
    /// → 測試連線 → 儲存 → 建立 project → 傳訊息 → 收到串流回覆。
    fn live_screens() {
        use pots_desktop::connection::{Endpoint, Target};
        use pots_desktop::model::*;
        use pots_llm::testing::{ScriptedProvider, ScriptedTurn};
        use pots_llm::{LlmError, ModelProvider};

        struct Fixed(Target);
        impl Endpoint for Fixed {
            fn locate(&mut self) -> Result<Target, String> {
                Ok(self.0.clone())
            }
        }

        /// 每個 API provider 都接到同一個腳本化的模型。
        struct ScriptedApi(Arc<ScriptedProvider>);

        #[async_trait::async_trait]
        impl pots_server::ApiProviderFactory for ScriptedApi {
            fn build(
                &self,
                _: ProviderKind,
                _: &url::Url,
                _: &str,
            ) -> Result<Arc<dyn ModelProvider>, String> {
                Ok(self.0.clone())
            }

            async fn list_models(
                &self,
                _: ProviderKind,
                _: &url::Url,
                _: &str,
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
        let model = ScriptedProvider::new([ScriptedTurn::new().text_deltas([
            "收到！",
            "這是接上本機服務後的第一則回覆，",
            "用的是設定頁新增的 OpenAI 相容 provider。",
        ])]);
        let deps = pots_server::Deps::new(
            ScriptedProvider::new(Vec::<ScriptedTurn>::new()),
            "auto",
            Arc::new(pots_server::FakeModelAuth::signed_out()),
        )
        .with_api_providers(Arc::new(ScriptedApi(model)));
        let handle = runtime
            .block_on(pots_server::start(
                pots_server::Config::desktop(dir.path()),
                deps,
            ))
            .expect("start server");
        let target = Target {
            addr: handle.addr,
            token: handle.token.as_str().to_string(),
        };

        let mut s = Shooter::with(move |window, cx| PotsApp::new_live(Fixed(target), window, cx));
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

        handle.shutdown();
        runtime.block_on(handle.wait()).expect("shutdown");
    }

    fn mock_screens() {
        let mut s = Shooter::new();
        s.shot("01-channel-goal");

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
        s.update(|app, _, _| {
            let Some(Panel::Artifact(id)) = app.panel else {
                panic!("右欄應該是 artifact");
            };
            let a = app.store.artifact(app.instance, id).unwrap().1;
            assert!(a.body.contains("**後端新增"), "編輯寫回全文");
            // 整段加粗連標題也變成「**範圍**」，所以用包含比對。
            assert!(
                a.human_edits.iter().any(|(h, _)| h.contains("範圍")),
                "標示改過的區段"
            );
            assert!(a.edited_at("測試計畫").is_none(), "agent 寫入中的區段唯讀");
        });
        s.shot("08b-artifact-edited");

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

        s.update(|app, _, cx| app.open_settings(SettingsTab::Providers, cx));
        s.shot("10b-settings");

        s.update(|app, _, cx| app.open_settings(SettingsTab::About, cx));
        s.shot("10c-settings-about");

        s.update(|app, _, cx| app.close_settings(cx));
        assert!(s.view_settings_closed(), "關閉後沒有設定視窗");
    }
}
