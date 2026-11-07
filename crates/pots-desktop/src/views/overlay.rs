//! 浮在主畫面上的元素：底部提示。

use gpui_kit::assets::IconName;
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

use super::widgets::icon;
use crate::app::PotsApp;
use crate::theme;

impl PotsApp {
    pub fn render_toast(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let text = self.toast.clone()?;
        Some(
            div()
                .absolute()
                .bottom(px(24.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("toast")
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .px(px(14.))
                        .py(px(10.))
                        .rounded(px(10.))
                        .bg(theme::text())
                        .text_color(theme::white())
                        .text_size(px(13.))
                        .shadow_lg()
                        .cursor_pointer()
                        .child(icon(IconName::Info, 15., theme::white()))
                        .child(text)
                        .child(icon(IconName::X, 13., theme::text_dim_on_dark()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toast = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }
}
