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
    use pots_desktop::app::{Panel, PotsApp};
    use pots_desktop::mock;
    use pots_desktop::model::{InboxAction, TaskId};

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
                pots_desktop::app::bind_keys(cx);
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
                .unwrap_or(TaskId(0));
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
    }
}
