use gpui_kit::{App, AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};
use pots_desktop::{app, mock};

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            app::bind_keys(cx);

            let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                window.set_window_title("Pots");
                cx.new(|cx| app::PotsApp::new(mock::store(), window, cx))
            })
            .expect("無法開啟視窗");
            cx.activate(true);
        });
}
