//! 浮在主畫面上的元素：底部提示。

use gpui_kit::assets::IconName;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px, relative,
};

use super::widgets::icon;
use crate::app::PotsApp;
use crate::theme;

impl PotsApp {
    /// 「為什麼」面板：一則 agent 訊息的觸發來源、記憶版本、模型與工具順序。
    pub fn render_why_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let why = self.why.as_ref()?;
        let mut body = div().flex().flex_col().gap(px(10.)).child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .text_size(px(12.))
                .text_color(theme::text_muted())
                .child(format!(
                    "{} · {} · 觸發：{}",
                    why.agent,
                    why.model.clone().unwrap_or_else(|| "未知模型".into()),
                    why.trigger
                ))
                .child(format!(
                    "記憶版本：{} · 用量：{} in / {} out tokens{}",
                    why.memory_revision
                        .map(|r| format!("v{r}"))
                        .unwrap_or_else(|| "無記憶".into()),
                    why.input_tokens,
                    why.output_tokens,
                    why.stop_reason
                        .as_deref()
                        .map(|r| format!(" · 停下原因：{r}"))
                        .unwrap_or_default(),
                )),
        );
        if why.steps.is_empty() {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child("這次 run 沒有留下 trace。"),
            );
        }
        for step in &why.steps {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p(px(8.))
                    .rounded(px(6.))
                    .bg(theme::subtle_bg())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if step.kind == "model" {
                                format!("模型 · {}", step.name)
                            } else {
                                format!("工具 · {}", step.name)
                            })
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::text_muted())
                                    .child(if step.ok {
                                        String::new()
                                    } else {
                                        "（失敗）".into()
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(step.detail.clone()),
                    ),
            );
        }
        let dialog = div()
            .id("why-dialog")
            .w(px(640.))
            .max_w(relative(0.9))
            .max_h(relative(0.8))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(20.))
            .rounded(px(12.))
            .bg(theme::white())
            .shadow_lg()
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::BOLD)
                    .child("這一則為什麼這樣回答"),
            )
            .child(body)
            .child(
                super::widgets::link_button("why-close", "關閉")
                    .on_click(cx.listener(|this, _, _, cx| this.close_why(cx))),
            )
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .on_click(|_, _, cx| cx.stop_propagation());
        Some(
            div()
                .id("why-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::hsla(0., 0., 0., 0.35))
                .on_click(cx.listener(|this, _, _, cx| this.close_why(cx)))
                .child(dialog)
                .into_any_element(),
        )
    }

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
