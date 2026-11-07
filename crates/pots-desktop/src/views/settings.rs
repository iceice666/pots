//! 設定：目前實例的模型 provider。左下角「設定」進入，佔中欄。
//!
//! 內建的 ChatGPT 方案在第一張，可以登入、登出、指定模型；另外可以新增 OpenAI 或
//! Anthropic 相容的 API。對話 agent 用「使用中」的那一個。API key 只送給服務、存在
//! 實例的資料目錄，畫面上只看得到末四碼。

use gpui_kit::assets::IconName;
use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
};

use super::widgets::{icon, link_button, primary_button, secondary_button, tag};
use crate::app::{PotsApp, ProviderEditor, ProviderTest};
use crate::model::*;
use crate::theme;

/// 「測試連線」最多列出幾個模型，太多時只顯示前面的。
const MAX_MODEL_CHIPS: usize = 40;

fn field(label: &str, hint: Option<&str>, input: AnyElement) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(label.to_string()),
        )
        .child(input)
        .children(hint.map(|h| {
            div()
                .text_size(px(12.))
                .text_color(theme::text_muted())
                .child(h.to_string())
        }))
}

fn detail_row(label: &str, value: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .gap(px(12.))
        .text_size(px(13.))
        .child(
            div()
                .w(px(72.))
                .flex_none()
                .text_color(theme::text_muted())
                .child(label.to_string()),
        )
        .child(div().child(value.into()))
}

fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .p(px(18.))
        .rounded(px(10.))
        .border_1()
        .border_color(theme::border())
        .bg(theme::white())
}

fn auth_status(auth: &ModelAuthView) -> (String, gpui_kit::Hsla) {
    match auth {
        ModelAuthView::SignedOut => ("尚未登入".into(), theme::text_muted()),
        ModelAuthView::Pending { .. } => ("等你在瀏覽器完成登入".into(), theme::warn_text()),
        ModelAuthView::SignedIn { account } => (
            match account {
                Some(a) => format!("已登入 {a}"),
                None => "已登入".into(),
            },
            theme::ok(),
        ),
        ModelAuthView::Error { message } => (message.clone(), theme::danger()),
    }
}

impl PotsApp {
    pub fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let inst = self.current_instance();
        let providers = &inst.providers;

        let header = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(32.))
            .pt(px(20.))
            .pb(px(16.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::BOLD)
                    .child(format!("設定 · {}", inst.name)),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(theme::text_muted())
                    .child("這些設定屬於這個實例；連到其他實例時各自設定。"),
            );

        let mut list = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .max_w(px(760.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .child("模型 Provider"),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme::text_muted())
                            .child(
                                "對話 agent 用「使用中」的 provider 推論。API key 只存在這個實例的資料目錄，設定好後不會再顯示。",
                            ),
                    ),
            );

        if providers.active.is_none() {
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(8.))
                    .bg(theme::warn_soft())
                    .text_color(theme::warn_text())
                    .text_size(px(13.))
                    .child(icon(IconName::CircleAlert, 16., theme::warn_text()))
                    .child("還沒有使用中的 provider：登入 ChatGPT，或新增一個 API provider。"),
            );
        }

        for p in &providers.items {
            list = list.child(self.render_provider_card(p, providers.active == Some(p.id), cx));
            if let Some(editor) = &self.provider_editor
                && editor.id == Some(p.id)
            {
                list = list.child(self.render_provider_editor(editor, cx));
            }
        }

        match &self.provider_editor {
            Some(editor) if editor.id.is_none() => {
                list = list.child(self.render_provider_editor(editor, cx));
            }
            _ => {
                list = list.child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .pt(px(4.))
                        .child(
                            secondary_button("add-openai", "新增 OpenAI 相容 API").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.edit_provider(
                                        None,
                                        ProviderKind::OpenAiCompatible,
                                        window,
                                        cx,
                                    )
                                }),
                            ),
                        )
                        .child(
                            secondary_button("add-anthropic", "新增 Anthropic 相容 API").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.edit_provider(
                                        None,
                                        ProviderKind::AnthropicCompatible,
                                        window,
                                        cx,
                                    )
                                }),
                            ),
                        ),
                );
            }
        }

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(32.))
                    .py(px(20.))
                    .child(list),
            )
    }

    fn render_provider_card(
        &self,
        p: &ProviderView,
        active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = p.id;
        let kind = p.kind;
        let editing = self
            .provider_editor
            .as_ref()
            .is_some_and(|e| e.id == Some(id));

        let title = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::BOLD)
                    .child(p.name.clone()),
            )
            .child(tag(kind.label(), theme::chip_text(), theme::chip_bg()))
            .when(active, |d| {
                d.child(tag("使用中", theme::accent_text(), theme::accent_soft()))
            });

        let model = if p.model.is_empty() {
            match kind {
                ProviderKind::ChatGpt => "自動（帳號清單的第一個）".to_string(),
                _ => "未設定".to_string(),
            }
        } else {
            p.model.clone()
        };
        let mut details = div().flex().flex_col().gap(px(4.));
        if kind == ProviderKind::ChatGpt {
            let (status, color) = auth_status(&self.current_instance().model_auth);
            details = details.child(
                div()
                    .flex()
                    .gap(px(12.))
                    .text_size(px(13.))
                    .child(
                        div()
                            .w(px(72.))
                            .flex_none()
                            .text_color(theme::text_muted())
                            .child("帳號"),
                    )
                    .child(div().text_color(color).child(status)),
            );
        } else {
            details = details.child(detail_row("Base URL", p.base_url.clone()));
            details = details.child(detail_row(
                "API key",
                p.key_hint.clone().unwrap_or_else(|| "未設定".into()),
            ));
        }
        details = details.child(detail_row("模型", model));

        let key = |name: &str| SharedString::from(format!("provider-{name}-{id}"));
        let mut buttons: Vec<gpui_kit::Stateful<Div>> = Vec::new();
        if !active {
            buttons.push(
                primary_button(key("activate"), "設為使用中")
                    .on_click(cx.listener(move |this, _, _, cx| this.set_active_provider(id, cx))),
            );
        }
        if kind == ProviderKind::ChatGpt {
            let signed_in = matches!(
                self.current_instance().model_auth,
                ModelAuthView::SignedIn { .. }
            );
            buttons.push(if signed_in {
                link_button(key("logout"), "登出")
                    .on_click(cx.listener(|this, _, _, cx| this.chatgpt_logout(cx)))
            } else {
                link_button(key("login"), "登入")
                    .on_click(cx.listener(|this, _, _, cx| this.chatgpt_login(cx)))
            });
        }
        if !editing {
            buttons.push(link_button(key("edit"), "編輯").on_click(cx.listener(
                move |this, _, window, cx| this.edit_provider(Some(id), kind, window, cx),
            )));
        }
        if kind != ProviderKind::ChatGpt {
            buttons.push(
                link_button(key("delete"), "刪除")
                    .text_color(theme::danger())
                    .on_click(cx.listener(move |this, _, _, cx| this.delete_provider(id, cx))),
            );
        }
        // 沒有「設為使用中」時第一個是文字連結：拿掉左邊留白，和卡片內容對齊。
        let actions = div().flex().items_center().gap(px(4.)).children(
            buttons
                .into_iter()
                .enumerate()
                .map(|(i, b)| if i == 0 && active { b.pl(px(0.)) } else { b }),
        );

        card()
            .when(active, |d| d.border_color(theme::accent()))
            .child(title)
            .child(details)
            .child(actions)
    }

    fn render_provider_editor(
        &self,
        editor: &ProviderEditor,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let kind = editor.kind;
        let title = match (editor.id, kind) {
            (_, ProviderKind::ChatGpt) => "ChatGPT 方案的模型".to_string(),
            (None, _) => format!("新增 {}", kind.label()),
            (Some(_), _) => format!("編輯 {}", kind.label()),
        };
        let input = |state| Input::new(state).into_any_element();

        let mut form = card().bg(theme::subtle_bg()).child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::BOLD)
                .child(title),
        );
        if kind != ProviderKind::ChatGpt {
            let base_hint = match kind {
                ProviderKind::OpenAiCompatible => {
                    "到 /v1 為止，例如 https://api.openai.com/v1、https://openrouter.ai/api/v1、http://localhost:11434/v1"
                }
                _ => "例如 https://api.anthropic.com；Pots 會呼叫 /v1/messages",
            };
            form = form
                .child(field("名稱", None, input(&editor.name)))
                .child(field("Base URL", Some(base_hint), input(&editor.base_url)))
                .child(field("API key", None, input(&editor.api_key)));
        }
        form = form.child(field("模型", None, input(&editor.model)));

        match &editor.test {
            ProviderTest::Idle => {}
            ProviderTest::Running(_) => {
                form = form.child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .child("測試中…"),
                );
            }
            ProviderTest::Err(message) => {
                form = form.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(13.))
                        .text_color(theme::danger())
                        .child(icon(IconName::CircleAlert, 14., theme::danger()))
                        .child(message.clone()),
                );
            }
            ProviderTest::Ok(models) => {
                let mut chips = div().flex().flex_wrap().gap(px(6.));
                for (i, m) in models.iter().take(MAX_MODEL_CHIPS).enumerate() {
                    let model = m.id.clone();
                    chips = chips.child(
                        div()
                            .id(SharedString::from(format!("model-chip-{i}")))
                            .cursor_pointer()
                            .px(px(8.))
                            .py(px(3.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(theme::border_strong())
                            .bg(theme::white())
                            .text_size(px(12.))
                            .hover(|s| s.bg(theme::accent_soft()))
                            .child(m.display_name.clone())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.pick_model(model.clone(), window, cx)
                            })),
                    );
                }
                let more = models.len().saturating_sub(MAX_MODEL_CHIPS);
                form = form.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(px(13.))
                                .text_color(theme::ok())
                                .child(icon(IconName::CircleCheck, 14., theme::ok()))
                                .child(format!("連線成功，{} 個模型，點一個填入", models.len())),
                        )
                        .child(chips)
                        .when(more > 0, |d| {
                            d.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::text_muted())
                                    .child(format!("還有 {more} 個沒列出，直接輸入模型 ID")),
                            )
                        }),
                );
            }
        }

        if let Some(error) = &editor.error {
            form = form.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(13.))
                    .text_color(theme::danger())
                    .child(icon(IconName::CircleAlert, 14., theme::danger()))
                    .child(error.clone()),
            );
        }

        form.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    primary_button(
                        "provider-save",
                        if editor.saving {
                            "儲存中…"
                        } else {
                            "儲存"
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.save_provider(cx))),
                )
                .when(kind != ProviderKind::ChatGpt, |d| {
                    d.child(
                        secondary_button("provider-test", "測試連線")
                            .on_click(cx.listener(|this, _, _, cx| this.test_provider(cx))),
                    )
                })
                .child(
                    link_button("provider-cancel", "取消")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_provider_edit(cx))),
                ),
        )
    }
}
