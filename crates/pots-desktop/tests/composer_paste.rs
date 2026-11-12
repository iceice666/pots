//! 主對話輸入框的貼圖：真的走 `Textarea::on_paste`（合成 ctrl-v／cmd-v），
//! 驗證剪貼簿裡的圖片進得了待送附件、而且不會被當成文字塞進輸入框。
//!
//! 這是 headless 測試：剪貼簿是 GPUI `TestPlatform` 的記憶體版本，**沒有**碰過真的
//! 系統剪貼簿（見 PR 的未驗證項目）。
//!
//! `cargo test -p pots-desktop --test composer_paste`；容器裡要 `-j 1`（並行連結多個
//! gpui 測試 binary 會 OOM）。
#![recursion_limit = "512"]

// `gpui_kit` 把底層的 `gpui` re-export 成 `gpui`，`#[gpui::test]` 走這裡。
use gpui_kit::gpui;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, ClipboardItem, Entity, Image as GpuiImage, ImageFormat,
    TestAppContext, VisualTestContext, WindowBounds, WindowOptions, px, size,
};
use pots_desktop::app::PotsApp;
use pots_desktop::mock;

/// 剪貼簿圖片用的貼上鍵：macOS 是 cmd-v，其他平台是 ctrl-v。
fn paste_keystroke() -> &'static str {
    if cfg!(target_os = "macos") {
        "cmd-v"
    } else {
        "ctrl-v"
    }
}

/// 一張 2×2 的 PNG。
fn png_bytes() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("寫 PNG");
    out.into_inner()
}

fn clipboard_image() -> ClipboardItem {
    ClipboardItem::new_image(&GpuiImage::from_bytes(ImageFormat::Png, png_bytes()))
}

/// 開一個 headless 視窗，聚焦輸入框並畫一帧（貼上的 action 由聚焦的節點處理）。
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
        let composer = view.read(cx).composer.clone();
        composer.update(cx, |state, cx| state.focus(window, cx));
        window.draw(cx).clear(cx);
    });
    (cx, view)
}

#[gpui::test]
fn pasting_an_image_adds_it_to_the_composer_without_inserting_text(cx: &mut TestAppContext) {
    let (mut cx, view) = open_app(cx);
    cx.write_to_clipboard(clipboard_image());
    cx.simulate_keystrokes(paste_keystroke());

    cx.update(|_, cx| {
        let app = view.read(cx);
        let pending = app.pending.read(cx);
        assert_eq!(pending.len(), 1, "貼上的圖應該進待送附件");
        let image = &pending.items()[0];
        assert_eq!(image.mime, "image/png");
        assert!(
            image.is_uploaded(),
            "假資料模式沒有服務，圖片直接算上傳完成"
        );
        // 輸入框裡沒有被塞進任何文字（圖片不會變成路徑或 base64）。
        assert_eq!(app.composer.read(cx).value().as_ref(), "");
    });
}

#[gpui::test]
fn pasting_text_still_inserts_it(cx: &mut TestAppContext) {
    let (mut cx, view) = open_app(cx);
    cx.write_to_clipboard(ClipboardItem::new_string("貼上的字".into()));
    cx.simulate_keystrokes(paste_keystroke());

    cx.update(|_, cx| {
        let app = view.read(cx);
        assert_eq!(app.pending.read(cx).len(), 0, "文字不該變成附件");
        assert_eq!(app.composer.read(cx).value().as_ref(), "貼上的字");
    });
}

#[gpui::test]
fn pasting_a_second_image_appends_and_the_fifth_is_rejected(cx: &mut TestAppContext) {
    let (mut cx, view) = open_app(cx);
    for expected in 1..=4 {
        cx.write_to_clipboard(clipboard_image());
        cx.simulate_keystrokes(paste_keystroke());
        cx.update(|_, cx| {
            let app = view.read(cx);
            assert_eq!(app.pending.read(cx).len(), expected);
            assert!(app.pending.read(cx).error.is_none());
        });
    }
    cx.write_to_clipboard(clipboard_image());
    cx.simulate_keystrokes(paste_keystroke());
    cx.update(|_, cx| {
        let app = view.read(cx);
        let pending = app.pending.read(cx);
        assert_eq!(pending.len(), 4, "第 5 張不該進來");
        assert!(
            pending
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("最多 4 張"),
            "要說清楚為什麼沒收：{:?}",
            pending.error
        );
        // 被拒的貼上仍然算「有處理」，輸入框不會冒出文字。
        assert_eq!(app.composer.read(cx).value().as_ref(), "");
    });
}

#[gpui::test]
fn a_non_image_file_on_the_clipboard_is_rejected_with_a_reason(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, "不是圖片").expect("寫檔");
    let paths = gpui_kit::ExternalPaths(vec![path.clone()].into());
    cx.write_to_clipboard(ClipboardItem {
        entries: vec![gpui_kit::ClipboardEntry::ExternalPaths(paths)],
    });
    // 複製檔案再貼上的形狀：hook 收到 ExternalPaths，讀檔後走同一條驗證。
    let (mut cx, view) = open_app(cx);
    cx.simulate_keystrokes(paste_keystroke());
    cx.update(|_, cx| {
        let app = view.read(cx);
        let pending = app.pending.read(cx);
        assert_eq!(pending.len(), 0, "不是圖片就不收");
        assert!(
            pending
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("不是看得懂的圖片"),
            "要說明為什麼沒收：{:?}",
            pending.error
        );
        // 全部被拒 ⇒ hook 回 false ⇒ 走原本的文字貼上（檔案路徑），這是 brief 指定的行為。
        assert_eq!(
            app.composer.read(cx).value().as_ref(),
            path.to_string_lossy()
        );
    });
}

#[gpui::test]
fn the_thumbnail_strip_renders_for_pending_images(cx: &mut TestAppContext) {
    let (mut cx, view) = open_app(cx);
    cx.write_to_clipboard(clipboard_image());
    cx.simulate_keystrokes(paste_keystroke());
    // 縮圖列畫得出來：`img(ImageSource::Image(..))` 與移除鈕都在同一帧裡排版，
    // 沒有圖片解碼失敗或版面 panic（元素本身沒有 `.test_support()`，查不到 id）。
    for _ in 0..2 {
        cx.update(|window, cx| window.render_frame(cx));
    }
    cx.update(|_, cx| assert_eq!(view.read(cx).pending.read(cx).len(), 1));
}
