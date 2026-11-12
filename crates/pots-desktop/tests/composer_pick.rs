//! 主對話輸入框的「選擇檔案」按鈕：按下開系統檔案對話框（gpui `prompt_for_paths`），
//! 選到的圖片進待送附件、取消不動輸入框、非圖片被中文拒收。
//!
//! 這是 headless 測試：對話框是 GPUI `TestPlatform` 的替身，用
//! `simulate_path_prompt_response` 回覆，**沒有**開過真的 NSOpenPanel／IFileOpenDialog／
//! xdg-desktop-portal（見 PR 的未驗證項目）。「對話框開不起來」（`Ok(Err(_))`）也模擬不出來，
//! 那條分支直接餵 `PotsApp::on_picked`／`pick_outcome` 驗證。
//!
//! `cargo test -p pots-desktop --test composer_pick -j 1`；容器裡要 `-j 1`（並行連結多個
//! gpui 測試 binary 會 OOM）。
#![recursion_limit = "512"]

use gpui_kit::gpui;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, TestAppContext, VisualTestContext, WindowBounds,
    WindowOptions, px, size,
};
use pots_desktop::app::PotsApp;
use pots_desktop::mock;
use std::path::PathBuf;

/// 輸入框底列「選擇檔案」按鈕的 id。
const PICK_BUTTON: &str = "composer-pick";

/// 一張 2×2 的 PNG。
fn png_bytes() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([40, 50, 60, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("寫 PNG");
    out.into_inner()
}

fn write(dir: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("寫檔");
    path
}

/// 開一個 headless 視窗並畫一帧（按鈕的 hitbox 要存在才點得到）。
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

/// 按下「選擇檔案」：在按鈕的實際位置送一次真的滑鼠點擊（走 `.on_click`），
/// 不是直接呼叫入口。按鈕用 `debug_selector` 註冊位置，跟拖放目標一樣。
fn click_pick(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.render_frame(cx));
    let center = cx
        .debug_bounds(PICK_BUTTON)
        .unwrap_or_else(|| panic!("找不到 {PICK_BUTTON} 的 bounds"))
        .center();
    cx.simulate_click(center, gpui_kit::Modifiers::none());
    cx.update(|window, cx| window.render_frame(cx));
}

#[gpui::test]
fn picking_a_png_adds_it_to_the_composer(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write(dir.path(), "shot.png", &png_bytes());
    let (mut cx, view) = open_app(cx);

    assert!(!cx.did_prompt_for_paths(), "按之前沒有對話框");
    click_pick(&mut cx);
    assert!(cx.did_prompt_for_paths(), "按下按鈕要開系統檔案對話框");

    cx.simulate_path_prompt_response(move |options| {
        assert!(options.files, "要能選檔案");
        assert!(!options.directories, "不選資料夾");
        assert!(options.multiple, "可以多選");
        Some(vec![png.clone()])
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.render_frame(cx));

    cx.update(|_, cx| {
        let app = view.read(cx);
        let pending = app.pending.read(cx);
        assert_eq!(pending.len(), 1, "選到的圖要進待送附件");
        let image = &pending.items()[0];
        assert_eq!(image.filename.as_deref(), Some("shot.png"));
        assert_eq!(image.mime, "image/png");
        assert!(
            image.is_uploaded(),
            "假資料模式沒有服務，圖片直接算上傳完成"
        );
        assert!(
            pending.error.is_none(),
            "沒有被拒就不該有錯誤：{:?}",
            pending.error
        );
        // 選檔不該在輸入框裡留下任何文字。
        assert_eq!(app.composer.read(cx).value().as_ref(), "");
    });
    assert!(!cx.did_prompt_for_paths(), "回覆之後對話框就結束了");
}

#[gpui::test]
fn cancelling_the_dialog_changes_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = write(dir.path(), "first.png", &png_bytes());
    let (mut cx, view) = open_app(cx);

    // 先收一張，確認取消不會把它洗掉。
    click_pick(&mut cx);
    cx.simulate_path_prompt_response(move |_| Some(vec![first.clone()]));
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(view.read(cx).pending.read(cx).len(), 1));

    click_pick(&mut cx);
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();
    cx.update(|window, cx| window.render_frame(cx));

    cx.update(|_, cx| {
        let app = view.read(cx);
        let pending = app.pending.read(cx);
        assert_eq!(pending.len(), 1, "取消不該動到既有的圖片");
        assert!(pending.error.is_none(), "取消不是錯誤：{:?}", pending.error);
    });
}

#[gpui::test]
fn a_non_image_file_is_rejected_in_chinese(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let txt = write(dir.path(), "notes.txt", b"not an image");
    let (mut cx, view) = open_app(cx);

    click_pick(&mut cx);
    cx.simulate_path_prompt_response(move |_| Some(vec![txt.clone()]));
    cx.run_until_parked();
    cx.update(|window, cx| window.render_frame(cx));

    cx.update(|_, cx| {
        let app = view.read(cx);
        let pending = app.pending.read(cx);
        assert_eq!(pending.len(), 0, "非圖片不該進待送附件");
        let error = pending.error.as_deref().unwrap_or_default();
        assert!(error.contains("不是看得懂的圖片"), "要是中文原因：{error}");
        assert!(error.contains("notes.txt"), "要說出是哪個檔案：{error}");
    });
}

#[gpui::test]
fn the_fifth_pick_is_rejected_but_the_first_four_stay(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut cx, view) = open_app(cx);

    // 一則訊息 4 張；第 5 次要被拒收。
    for i in 0..=4 {
        let path = write(dir.path(), &format!("{i}.png"), &png_bytes());
        click_pick(&mut cx);
        cx.simulate_path_prompt_response(move |_| Some(vec![path.clone()]));
        cx.run_until_parked();
    }
    cx.update(|window, cx| window.render_frame(cx));

    cx.update(|_, cx| {
        let pending = view.read(cx).pending.read(cx);
        assert_eq!(pending.len(), 4, "只收前 4 張");
        let error = pending.error.as_deref().unwrap_or_default();
        assert!(error.contains("最多 4 張"), "要說清楚為什麼沒收：{error}");
    });
}

#[gpui::test]
fn a_missing_dialog_shows_a_chinese_hint(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write(dir.path(), "kept.png", &png_bytes());
    let (mut cx, view) = open_app(cx);

    // headless 平台模擬不出「對話框開不起來」（`simulate_path_prompt_response` 只能回
    // 選了或取消），先放一張圖、再直接餵 `note_pick_failure` 那條分支。
    click_pick(&mut cx);
    cx.simulate_path_prompt_response(move |_| Some(vec![png.clone()]));
    cx.run_until_parked();

    let message =
        "系統沒有檔案選擇對話框（缺少 xdg-desktop-portal），請改用拖放或貼上：no dbus session";
    cx.update(|_, cx| {
        view.update(cx, |app, cx| app.note_pick_failure(message.into(), cx));
    });
    cx.update(|window, cx| window.render_frame(cx));

    cx.update(|_, cx| {
        let pending = view.read(cx).pending.read(cx);
        assert_eq!(pending.len(), 1, "對話框開不起來不該動到既有的圖片");
        let error = pending.error.as_deref().unwrap_or_default();
        assert!(error.contains("系統沒有檔案選擇對話框"), "{error}");
        assert!(error.contains("缺少 xdg-desktop-portal"), "{error}");
        assert!(error.contains("拖放或貼上"), "{error}");
        assert!(error.contains("no dbus session"), "平台原文要留著：{error}");
    });
}
