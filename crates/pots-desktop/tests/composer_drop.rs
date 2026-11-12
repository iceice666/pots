//! 主對話輸入框的拖放收圖：合成 `FileDropEvent`（Entered／Submit／Exited）走完整條
//! 事件路徑，驗證圖片進得了待送附件、被拒時有中文原因、懸停高亮會亮也會收。
//!
//! 這是 headless 測試：拖放事件是**合成**的，沒有碰過真的 OS 拖放（見 PR 的未驗證項目）。
//!
//! `cargo test -p pots-desktop --test composer_drop -j 1`；容器裡要 `-j 1`（並行連結多個
//! gpui 測試 binary 會 OOM）。
#![recursion_limit = "512"]

use gpui_kit::gpui;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, FileDropEvent, InputEvent as _, TestAppContext,
    VisualTestContext, WindowBounds, WindowOptions, point, px, size,
};
use pots_desktop::app::PotsApp;
use pots_desktop::mock;

/// 輸入框容器（拖放目標）的 debug selector。
const DROP_TARGET: &str = "composer-drop";

/// 一張 2×2 的 PNG。
fn png_bytes() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([30, 40, 50, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("寫 PNG");
    out.into_inner()
}

/// 開一個 headless 視窗並畫一帧（拖放的 hitbox 要存在才命中）。
fn open_app(cx: &mut TestAppContext) -> (VisualTestContext, Entity<PotsApp>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        pots_desktop::app::init(cx);
    });
    let (window, view): (AnyWindowHandle, Entity<PotsApp>) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds {
                        origin: Default::default(),
                        size: size(px(1280.), px(800.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| PotsApp::new(mock::store(), window, cx)),
            )
        })
        .expect("開視窗");
    let mut cx = VisualTestContext::from_window(window, cx);
    cx.update(|window, cx| {
        window.render_frame(cx);
    });
    (cx, view)
}

/// 輸入框在視窗裡的中心點。
fn composer_center(cx: &mut VisualTestContext) -> gpui_kit::Point<gpui_kit::Pixels> {
    cx.debug_bounds(DROP_TARGET)
        .unwrap_or_else(|| panic!("找不到 {DROP_TARGET} 的 bounds"))
        .center()
}

fn external(paths: Vec<std::path::PathBuf>) -> gpui_kit::ExternalPaths {
    gpui_kit::ExternalPaths(paths.into())
}

/// 一次完整的拖放：檔案進來（Entered）→ 放開（Submit）。
fn drop_files(cx: &mut VisualTestContext, paths: Vec<std::path::PathBuf>) {
    let position = composer_center(cx);
    let paths = external(paths);
    cx.update(|window, cx| {
        window.dispatch_event(
            FileDropEvent::Entered { position, paths }.to_platform_input(),
            cx,
        );
        window.dispatch_event(FileDropEvent::Submit { position }.to_platform_input(), cx);
    });
    // 讀檔在背景執行緒，等它跑完。
    cx.run_until_parked();
    cx.update(|window, cx| window.render_frame(cx));
}

fn write_png(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, png_bytes()).expect("寫檔");
    path
}

#[gpui::test]
fn dropping_image_files_adds_them_to_the_composer(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = write_png(dir.path(), "a.png");
    let b = write_png(dir.path(), "b.png");
    let (mut cx, view) = open_app(cx);
    drop_files(&mut cx, vec![a.clone(), b.clone()]);

    cx.update(|_, cx| {
        let app = view.read(cx);
        let pending = app.pending.read(cx);
        assert_eq!(pending.len(), 2, "拖進來的兩張圖都要進待送附件");
        assert!(pending.error.is_none(), "沒有被拒就不該有錯誤訊息");
        assert_eq!(pending.items()[0].filename.as_deref(), Some("a.png"));
        assert_eq!(pending.items()[1].filename.as_deref(), Some("b.png"));
        assert_eq!(pending.items()[0].mime, "image/png");
        assert!(
            pending.items()[0].is_uploaded(),
            "假資料模式沒有服務，圖片直接算上傳完成"
        );
        assert!(!app.drop_hot, "放開之後高亮要收回");
    });
}

#[gpui::test]
fn a_missing_path_and_a_non_image_file_are_rejected_in_chinese(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("不存在.png");
    let notes = dir.path().join("notes.txt");
    std::fs::write(&notes, "不是圖片").expect("寫檔");
    let (mut cx, view) = open_app(cx);
    drop_files(&mut cx, vec![missing, notes]);

    cx.update(|_, cx| {
        let app = view.read(cx);
        let pending = app.pending.read(cx);
        assert_eq!(pending.len(), 0, "不是圖片就不進待送附件");
        let error = pending.error.as_deref().unwrap_or_default();
        assert!(error.contains("讀不到"), "要說明讀不到：{error}");
        assert!(
            error.contains("不是看得懂的圖片"),
            "要說明不是圖片：{error}"
        );
        assert!(!app.drop_hot, "被拒之後高亮要收回");
    });
}

#[gpui::test]
fn a_dropped_directory_is_rejected_with_a_reason(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let folder = dir.path().join("截圖");
    std::fs::create_dir(&folder).expect("建資料夾");
    let (mut cx, view) = open_app(cx);
    drop_files(&mut cx, vec![folder]);

    cx.update(|_, cx| {
        let pending = view.read(cx).pending.read(cx);
        assert_eq!(pending.len(), 0);
        let error = pending.error.as_deref().unwrap_or_default();
        assert!(error.contains("是資料夾"), "要說明是資料夾：{error}");
    });
}

#[gpui::test]
fn dropping_a_fifth_image_is_rejected(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut cx, view) = open_app(cx);
    for i in 1..=4 {
        let path = write_png(dir.path(), &format!("{i}.png"));
        drop_files(&mut cx, vec![path]);
        cx.update(|_, cx| {
            assert_eq!(view.read(cx).pending.read(cx).len(), i);
        });
    }
    let fifth = write_png(dir.path(), "5.png");
    drop_files(&mut cx, vec![fifth]);

    cx.update(|_, cx| {
        let pending = view.read(cx).pending.read(cx);
        assert_eq!(pending.len(), 4, "第 5 張不該進來");
        let error = pending.error.as_deref().unwrap_or_default();
        assert!(error.contains("最多 4 張"), "要說清楚為什麼沒收：{error}");
    });
}

#[gpui::test]
fn the_composer_highlights_while_a_file_is_dragged_over_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_png(dir.path(), "hot.png");
    let (mut cx, view) = open_app(cx);
    let position = composer_center(&mut cx);

    // 拖進來：游標在輸入框上 ⇒ 亮。
    cx.update(|window, cx| {
        window.dispatch_event(
            FileDropEvent::Entered {
                position,
                paths: external(vec![path.clone()]),
            }
            .to_platform_input(),
            cx,
        );
    });
    cx.update(|window, cx| window.render_frame(cx));
    cx.update(|_, cx| assert!(view.read(cx).drop_hot, "游標在輸入框上要高亮"));

    // 拖到別的地方（視窗左上角）：不用放開就要收回。
    cx.update(|window, cx| {
        window.dispatch_event(
            FileDropEvent::Pending {
                position: point(px(2.), px(2.)),
            }
            .to_platform_input(),
            cx,
        );
    });
    cx.update(|window, cx| window.render_frame(cx));
    cx.update(|_, cx| assert!(!view.read(cx).drop_hot, "游標離開輸入框就收回高亮"));

    // 再回到輸入框、然後拖出視窗：`on_file_drop_exit` 收高亮。
    cx.update(|window, cx| {
        window.dispatch_event(FileDropEvent::Pending { position }.to_platform_input(), cx);
    });
    cx.update(|window, cx| window.render_frame(cx));
    cx.update(|_, cx| assert!(view.read(cx).drop_hot));
    cx.update(|window, cx| {
        window.dispatch_event(FileDropEvent::Exited.to_platform_input(), cx);
    });
    cx.update(|window, cx| window.render_frame(cx));
    cx.update(|_, cx| {
        let app = view.read(cx);
        assert!(!app.drop_hot, "拖出去要收回高亮");
        assert!(app.pending.read(cx).is_empty(), "只是拖過去不該收圖");
    });
}
