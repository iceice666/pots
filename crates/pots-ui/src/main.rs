//! The Pots desktop app. Everything below the UI is in `pots-agent` and
//! `pots-llm`.

mod chat;
mod composer;
mod render_md;
mod selection;
mod settings;
mod theme;

use gpui::{
    App, Application, Bounds, Focusable, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions,
    actions, point, prelude::*, px, size,
};

actions!(pots, [Quit]);

const USAGE: &str = "\
usage: pots [--mock]

  --mock   show demo data instead of your workspace (nothing is saved)";

fn main() {
    let mut mock = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--mock" => mock = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => {
                eprintln!("pots: unknown argument {other:?}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    Application::new().run(move |cx: &mut App| {
        composer::bind_keys(cx);
        chat::bind_keys(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx| cx.quit()).detach();

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Pots".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(12.), px(12.))),
                    }),
                    window_min_size: Some(size(px(720.), px(480.))),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| chat::ChatApp::new(mock, window, cx)),
            )
            .unwrap();

        window
            .update(cx, |app, window, cx| {
                window.focus(&app.composer().focus_handle(cx));
            })
            .unwrap();
        cx.activate(true);
    });
}
