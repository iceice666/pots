//! 訊息附圖在 headless 視窗裡真的畫得出來：縮圖列不會 panic、假資料模式（沒有連線
//! 目標）不會去抓圖、大圖 viewer 開得起來也關得掉。
//!
//! 這一層補的是「render 路徑接得上」；外觀（縮圖大小、載入中的灰塊、大圖排版）需要 GPU，
//! 沒有覆蓋（見 PR 的未驗證項目）。
//!
//! ```sh
//! cargo test -p pots-desktop --test message_images -j 1
//! ```
#![recursion_limit = "512"]

use gpui_kit::gpui;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, TestAppContext, VisualTestContext, WindowBounds,
    WindowOptions, px, size,
};
use pots_desktop::app::PotsApp;
use pots_desktop::mock;
use pots_desktop::model::{AttachmentView, SendMode};

fn attachment(id: u64) -> AttachmentView {
    AttachmentView {
        id: pots_desktop::model::AttachmentId::from_u64(id),
        mime: "image/png".into(),
        bytes: 128,
        width: Some(2),
        height: Some(2),
        filename: Some("shot.png".into()),
    }
}

/// 開一個 headless 視窗，並在目前畫面那個 project 的對話裡塞一則帶圖片的訊息。
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
        let app = view.read(cx);
        let instance = app.instance;
        let project = app.current_project().expect("mock 有 project").id;
        view.update(cx, |app, _| {
            app.store.send_to_project(
                instance,
                project,
                "看這張圖".into(),
                SendMode::Queue,
                vec![attachment(1), attachment(2)],
            );
        });
        window.draw(cx).clear(cx);
    });
    (cx, view)
}

#[gpui::test]
fn a_message_with_images_renders_without_fetching_in_mock_mode(cx: &mut TestAppContext) {
    let (mut cx, view) = open_app(cx);
    cx.update(|window, cx| {
        // 再畫一帧：假資料模式沒有連線目標，縮圖列只畫佔位。
        window.draw(cx).clear(cx);
        let app = view.read(cx);
        assert!(
            app.attachment_images
                .state(&pots_desktop::model::AttachmentId::from_u64(1))
                .is_none(),
            "沒有連線目標時不該排取圖、也不該有狀態"
        );
    });
}

#[gpui::test]
fn the_image_viewer_opens_and_closes(cx: &mut TestAppContext) {
    let (mut cx, view) = open_app(cx);
    cx.update(|window, cx| {
        view.update(cx, |app, cx| {
            app.open_image_viewer(attachment(1), cx);
        });
        assert!(view.read(cx).image_viewer.is_some());
        // 大圖還沒在快取裡：viewer 畫的是提示文字，不是空白。
        window.draw(cx).clear(cx);

        view.update(cx, |app, cx| app.close_image_viewer(cx));
        assert!(view.read(cx).image_viewer.is_none());
        window.draw(cx).clear(cx);
    });
}

#[gpui::test]
fn a_failed_thumbnail_can_be_retried(cx: &mut TestAppContext) {
    let (mut cx, view) = open_app(cx);
    cx.update(|window, cx| {
        let id = pots_desktop::model::AttachmentId::from_u64(1);
        view.update(cx, |app, cx| {
            app.attachment_images
                .finish(id, "image/png", Err("找不到這個附件".into()));
            cx.notify();
        });
        window.draw(cx).clear(cx);
        assert!(
            matches!(
                view.read(cx).attachment_images.state(&id).cloned(),
                Some(pots_desktop::images::ImageState::Failed(_))
            ),
            "失敗的縮圖要留在失敗狀態，等使用者重試"
        );

        // 點「無法載入」的縮圖：清掉失敗狀態，下一帧重排（這裡沒有 fetcher，所以回到
        // 「沒有狀態」的佔位，而不是一直卡在失敗）。
        view.update(cx, |app, cx| app.retry_attachment_image(id, cx));
        assert!(view.read(cx).attachment_images.state(&id).is_none());
        window.draw(cx).clear(cx);
    });
}
