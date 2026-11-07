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
    use pots_desktop::app::{AutomationSection, ChannelTab, Panel, PotsApp};
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
                        |window, cx| cx.new(|cx| PotsApp::new(mock::store(), window, cx)),
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
    }
}
