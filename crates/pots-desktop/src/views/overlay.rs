//! 浮在主畫面上的元素：底部提示。

use gpui_kit::assets::IconName;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px, relative,
};

use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder as _;

use super::text::agent_text;
use super::widgets::icon;
use crate::app::{PotsApp, WorkdirKind};
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
        // 訊息上的工具摘要搬進這裡：點 chip 展開每一次工具呼叫。
        if let Some(tools) = self.why_tools.as_ref() {
            let item = why.item;
            let expanded = self.expanded_tools.contains(&item);
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(super::widgets::section_label("已呼叫的工具"))
                    .child(
                        div()
                            .id(SharedString::from(format!("why-tools-{}", item.0)))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .self_start()
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(theme::border())
                            .bg(theme::subtle_bg())
                            .text_size(px(13.))
                            .text_color(theme::text_muted())
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::track()))
                            .child(icon(IconName::Wrench, 13., theme::text_muted()))
                            .child(tools.text.clone())
                            .child(icon(
                                if expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                },
                                13.,
                                theme::text_muted(),
                            ))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.toggle_tools(item, cx)),
                            ),
                    )
                    .when(expanded, |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .pl(px(12.))
                                .border_l_2()
                                .border_color(theme::border())
                                .text_size(px(12.))
                                .text_color(theme::text_muted())
                                .children(
                                    tools.details.iter().map(|line| div().child(line.clone())),
                                ),
                        )
                    }),
            );
        }
        if why.steps.is_empty() {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child("這次 run 沒有留下 trace。"),
            );
        }
        for (i, step) in why.steps.iter().enumerate() {
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
                            .child(match step.kind.as_str() {
                                "model" => format!("模型 · {}", step.name),
                                "text" => "模型說".to_string(),
                                _ => format!("工具 · {}", step.name),
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
                    .child(if step.kind == "text" {
                        // 模型在工具呼叫之間說的話：照一般內文顯示成 Markdown，不是 JSON。
                        div().child(
                            agent_text(
                                SharedString::from(format!("why-step-text-{i}")),
                                step.detail.clone(),
                            )
                            .text_size(px(13.))
                            .line_height(relative(1.5)),
                        )
                    } else {
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(step.detail.clone())
                    }),
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

    /// 新增專案／設定工作資料夾：onboarding 的第二步就是「程式碼在哪」。
    pub fn render_workdir_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.workdir_dialog.as_ref()?;
        let editing = d.project.is_some();
        let local = d.kind == WorkdirKind::Local;
        let hint = if local {
            "worker 在任務分支上改平台自己的副本，不是你資料夾裡的即時檔案；核准後平台把分支 \
             fast-forward 套用進去。沒有 PR 與 CI，資料夾有未 commit 的修改時不能套用。"
        } else {
            "worker 在平台維護的鏡像上開任務分支、開 PR、等 CI，核准後 merge。這不是你電腦上的資料夾。"
        };
        let kind_tab = |kind: WorkdirKind, id: &'static str, cx: &mut Context<Self>| {
            super::widgets::tab(id, kind.label(), d.kind == kind).on_click(
                cx.listener(move |this, _, window, cx| this.set_workdir_kind(kind, window, cx)),
            )
        };
        let mut body = div().flex().flex_col().gap(px(10.)).child(
            div()
                .text_size(px(16.))
                .font_weight(FontWeight::BOLD)
                .child(if editing {
                    "工作資料夾"
                } else {
                    "新增專案"
                }),
        );
        if !editing {
            body = body
                .child(super::widgets::section_label("名稱"))
                .child(Input::new(&d.name));
        }
        body = body
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(super::widgets::section_label("工作資料夾（程式碼在哪）"))
                    .child(kind_tab(WorkdirKind::Github, "workdir-kind-github", cx))
                    .child(kind_tab(WorkdirKind::Local, "workdir-kind-local", cx)),
            )
            .child(Input::new(&d.target))
            .child(
                div()
                    .text_size(px(12.))
                    .line_height(relative(1.5))
                    .text_color(theme::text_muted())
                    .child(hint),
            );
        if !editing {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child("留白表示稍後再設定；沒有工作資料夾時 worker 不會派工。"),
            );
        }
        if let Some(error) = d.error.as_deref() {
            body = body.child(
                div()
                    .text_size(px(13.))
                    .text_color(theme::danger())
                    .child(error.to_string()),
            );
        }
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(
                    super::widgets::primary_button(
                        "workdir-submit",
                        if d.pending {
                            "儲存中…"
                        } else if editing {
                            "儲存"
                        } else {
                            "建立"
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.submit_workdir_dialog(cx))),
                )
                .when(!editing, |d| {
                    d.child(
                        super::widgets::link_button("workdir-skip", "稍後再設定").on_click(
                            cx.listener(|this, _, window, cx| this.skip_workdir(window, cx)),
                        ),
                    )
                })
                .child(
                    super::widgets::link_button("workdir-cancel", "取消")
                        .on_click(cx.listener(|this, _, _, cx| this.close_workdir_dialog(cx))),
                ),
        );
        let dialog = div()
            .id("workdir-dialog")
            .w(px(560.))
            .max_w(relative(0.9))
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(20.))
            .rounded(px(12.))
            .bg(theme::white())
            .shadow_lg()
            .child(body)
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .on_click(|_, _, cx| cx.stop_propagation());
        Some(
            div()
                .id("workdir-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::hsla(0., 0., 0., 0.45))
                .occlude()
                .child(dialog)
                .on_click(cx.listener(|this, _, _, cx| this.close_workdir_dialog(cx)))
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
