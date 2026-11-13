//! The Pots desktop app. Everything below the UI is in `pots-agent` and
//! `pots-llm`.

/// A keystroke with the platform's primary shortcut modifier: `cmd-` on
/// macOS, `ctrl-` elsewhere. gpui-pre's `KeyBinding::new` no longer
/// resolves the `secondary-` alias and panics on it, so it is resolved here
/// at compile time instead.
macro_rules! secondary {
    ($key:literal) => {{
        #[cfg(target_os = "macos")]
        let keystroke = concat!("cmd-", $key);
        #[cfg(not(target_os = "macos"))]
        let keystroke = concat!("ctrl-", $key);
        keystroke
    }};
}

mod artifact;
mod board_editor;
mod chat;
mod checklist_editor;
mod composer;
mod document_editor;
mod render_md;
mod selection;
mod settings;
mod table_editor;
mod theme;

use gpui::{
    App, Bounds, Focusable, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, actions,
    point, prelude::*, px, size,
};
use gpui_component::Root;

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

    gpui_platform::application().run(move |cx: &mut App| {
        // Before our own bindings and before any window: it installs the
        // global `Theme` and the `Root` plugin that its components need.
        gpui_component::init(cx);
        theme::install_component_theme(cx);
        composer::bind_keys(cx);
        chat::bind_keys(cx);
        table_editor::bind_keys(cx);
        checklist_editor::bind_keys(cx);
        board_editor::bind_keys(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_window_closed(|cx, _| cx.quit()).detach();

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let mut chat = None;
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
                |window, cx| {
                    let app = cx.new(|cx| chat::ChatApp::new(mock, window, cx));
                    chat = Some(app.clone());
                    // gpui-component's widgets require `Root` as the window's
                    // root view. Its surface styles default the text color to
                    // the theme's foreground; pin gpui's own default so text
                    // we never colored explicitly keeps looking the same.
                    cx.new(|cx| Root::new(app, window, cx).text_color(gpui::black()))
                },
            )
            .unwrap();

        let chat = chat.expect("open_window builds the root view");
        // Save the artifact being edited before asking the platform to
        // quit, while the window and its views are certainly still there.
        let weak = chat.downgrade();
        cx.on_action(move |_: &Quit, cx| {
            weak.update(cx, |chat, cx| chat.save_open_artifact(None, cx)).ok();
            cx.quit();
        });
        window
            .update(cx, |_, window, cx| {
                window.focus(&chat.read(cx).composer().focus_handle(cx), cx);
            })
            .unwrap();
        cx.activate(true);
    });
}
