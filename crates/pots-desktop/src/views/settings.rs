//! 設定視窗：左下角「設定」打開，蓋在主畫面上；左邊是類別，右邊是內容。
//!
//! 「模型 Provider」類別：
//! 內建的 ChatGPT 方案在第一張，可以登入、登出、指定模型；另外可以新增 OpenAI 或
//! Anthropic 相容的 API。對話 agent 用「使用中」的那一個。API key 只送給服務、存在
//! 實例的資料目錄，畫面上只看得到末四碼。
//!
//! 新增分兩步：先在清單下面的「新增 provider」選種類（每一格說明適合接哪些服務），
//! 那一區再換成表單。表單依填寫的順序排，「測試連線」與它列出的模型跟模型欄放在一起；
//! 內建目錄的 provider 直接從清單挑模型；額外 headers 收在「進階設定」。
//!
//! 「記憶」類別：整理記憶（Observer／Reflector）用的 provider 與模型，預設跟對話 agent 相同。
//!
//! 「角色」在 `roles.rs`，「GitHub」在 `automation_forms.rs`，「Worker」在 `worker_settings.rs`，「通知」與「USER.md」在
//! `preferences.rs`。

use gpui_kit::assets::IconName;
use gpui_kit::component::input::Input;
use gpui_kit::component::select::Select;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
};

use super::widgets::{icon, link_button, primary_button, secondary_button, tag};
use crate::app::{
    MemoryModelEditor, PROVIDER_SHORTCUTS, PotsApp, ProviderEditor, ProviderTest, SettingsTab,
    saves_as_active,
};
use crate::model::*;
use crate::theme;

pub(super) fn field(label: &str, hint: Option<&str>, input: AnyElement) -> Div {
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

pub(super) fn detail_row(label: &str, value: impl Into<SharedString>) -> Div {
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

pub(super) fn card() -> Div {
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

/// 表單裡的一行結果（測試成功、錯誤）：圖示加文字，長訊息會換行。
fn status_line(glyph: IconName, color: Hsla, text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_start()
        .gap(px(6.))
        .text_size(px(13.))
        .text_color(color)
        .child(div().pt(px(3.)).child(icon(glyph, 14., color)))
        .child(div().flex_1().min_w_0().child(text.into()))
}

/// 可以新增的種類與各自的元素 ID，最通用的排前面。
const ADDABLE_KINDS: [(ProviderKind, &str); 5] = [
    (ProviderKind::OpenAiCompatible, "add-openai"),
    (ProviderKind::OpenAiResponses, "add-openai-responses"),
    (ProviderKind::AnthropicCompatible, "add-anthropic"),
    (ProviderKind::OpenCodeZen, "add-opencode-zen"),
    (ProviderKind::OpenCodeGo, "add-opencode-go"),
];

/// 「新增 provider」裡每一種的一行說明：適合接哪些服務。
fn kind_blurb(kind: ProviderKind) -> String {
    if let Some(preset) = kind.preset() {
        return format!(
            "內建 {} 個模型的目錄，填 API key 就能挑模型",
            preset.models.len()
        );
    }
    match kind {
        ProviderKind::OpenAiCompatible => "OpenAI、OpenRouter、Ollama 等多數服務",
        ProviderKind::OpenAiResponses => "OpenAI 平台的 Responses API，工具與推理可以並用",
        ProviderKind::AnthropicCompatible => "Claude 的 Messages API，或相容的服務",
        _ => "",
    }
    .to_string()
}

fn auth_status(auth: &ModelAuthView) -> (String, Hsla) {
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
    /// 設定視窗：蓋在主畫面上，左邊是類別，右邊是內容。點背景、右上角或按 Esc 關閉。
    pub fn render_settings_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tab = self.settings?;
        let inst = self.current_instance();

        let mut nav = div()
            .flex_none()
            .w(px(220.))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(12.))
            .bg(theme::subtle_bg())
            .border_r_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .px(px(10.))
                    .pt(px(6.))
                    .pb(px(12.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .child("設定"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(format!("{} · {}", inst.name, inst.kind.label())),
                    ),
            );
        for t in SettingsTab::ALL {
            let selected = t == tab;
            let glyph = match t {
                SettingsTab::Providers => IconName::Sparkles,
                SettingsTab::Roles => IconName::UserCog,
                SettingsTab::Memory => IconName::Brain,
                SettingsTab::Github => IconName::GitPullRequest,
                SettingsTab::Worker => IconName::Bot,
                SettingsTab::Notifications => IconName::Bell,
                SettingsTab::UserMd => IconName::User,
                SettingsTab::About => IconName::Info,
            };
            nav = nav.child(
                div()
                    .id(SharedString::from(format!("settings-tab-{t:?}")))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(7.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(14.))
                    .when_else(
                        selected,
                        |d| d.bg(theme::white()).font_weight(FontWeight::SEMIBOLD),
                        |d| {
                            d.text_color(theme::text_muted())
                                .hover(|s| s.bg(theme::white()))
                        },
                    )
                    .child(icon(
                        glyph,
                        15.,
                        if selected {
                            theme::text()
                        } else {
                            theme::text_muted()
                        },
                    ))
                    .child(t.label())
                    .on_click(cx.listener(move |this, _, _, cx| this.open_settings(t, cx))),
            );
        }

        let body = match tab {
            SettingsTab::Providers => self.render_providers_section(cx).into_any_element(),
            SettingsTab::Roles => self.render_roles_section(cx).into_any_element(),
            SettingsTab::Memory => self.render_memory_section(cx).into_any_element(),
            SettingsTab::Github => self.render_github_section(cx).into_any_element(),
            SettingsTab::Worker => self.render_worker_section(cx).into_any_element(),
            SettingsTab::Notifications => self.render_notifications_section(cx).into_any_element(),
            SettingsTab::UserMd => self.render_user_md_section(cx).into_any_element(),
            SettingsTab::About => self.render_about_section().into_any_element(),
        };
        let content = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(28.))
                    .pt(px(20.))
                    .pb(px(12.))
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::BOLD)
                            .child(tab.label()),
                    )
                    .child(
                        div()
                            .id("settings-close")
                            .p(px(6.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::subtle_bg()))
                            .child(icon(IconName::X, 16., theme::text_muted()))
                            .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx))),
                    ),
            )
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_h_0()
                    .track_scroll(&self.settings_scroll)
                    .overflow_y_scroll()
                    .px(px(28.))
                    .pb(px(24.))
                    .child(body),
            );

        let dialog = div()
            .id("settings-dialog")
            .w(px(980.))
            .max_w(gpui_kit::relative(0.92))
            .h(px(680.))
            .max_h(gpui_kit::relative(0.9))
            .flex()
            .rounded(px(12.))
            .overflow_hidden()
            .bg(theme::white())
            .shadow_lg()
            .child(nav)
            .child(content)
            // 視窗裡的點擊不往外傳到背景，免得關掉。
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .on_click(|_, _, cx| cx.stop_propagation());

        Some(
            div()
                .id("settings-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::hsla(0., 0., 0., 0.45))
                .occlude()
                .child(dialog)
                .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
                .into_any_element(),
        )
    }

    fn render_providers_section(&self, cx: &mut Context<Self>) -> Div {
        let inst = self.current_instance();
        let providers = &inst.providers;

        let mut list = div().flex().flex_col().gap(px(12.)).child(
            div()
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .child(
                    "對話 agent 用「使用中」的 provider 推論。API key 只存在這個實例的資料目錄，設定好後不會再顯示。",
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
            _ => list = list.child(self.render_provider_kind_picker(cx)),
        }

        list
    }

    fn render_memory_section(&self, cx: &mut Context<Self>) -> Div {
        let inst = self.current_instance();
        let providers = &inst.providers;
        let describe = |id: Option<ProviderId>, model: &str| -> String {
            let Some(p) = id.and_then(|id| providers.get(id)) else {
                return "還沒有使用中的 provider".into();
            };
            let model = if !model.is_empty() {
                model.to_string()
            } else if !p.model.is_empty() {
                p.model.clone()
            } else if p.kind == ProviderKind::ChatGpt {
                "自動".into()
            } else {
                "未設定模型".into()
            };
            format!("{} · {model}", p.name)
        };
        let current = &providers.memory;
        let (label, detail) = match current.provider {
            None => (
                "跟對話 agent 相同".to_string(),
                format!("目前：{}", describe(providers.active, "")),
            ),
            Some(id) => (describe(Some(id), &current.model), String::new()),
        };

        let mut model_card = card()
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::BOLD)
                    .child("整理記憶的模型"),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme::text_muted())
                    .child(
                        "Observer 與 Reflector 在背景把對話整理成觀察。建議用便宜、快速、context 夠長的模型，\
                         和對話 agent 分開設定。",
                    ),
            );
        match &self.memory_model_editor {
            None => {
                model_card = model_card
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(detail_row("模型", label))
                            .when(!detail.is_empty(), |d| d.child(detail_row("", detail))),
                    )
                    .child(
                        div().child(
                            link_button("memory-model-edit", "變更")
                                .pl(px(0.))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.edit_memory_model(window, cx)
                                })),
                        ),
                    );
            }
            Some(editor) => {
                model_card = model_card.child(self.render_memory_model_editor(editor, cx));
            }
        }

        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(model_card)
            .child(
                card()
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::BOLD)
                            .child("什麼時候整理（預設值）"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .text_size(px(13.))
                            .text_color(theme::text_muted())
                            .child("還沒整理的對話超過約 3 萬 token 時，Observer 把較早的部分整理成觀察；最近 12 則一直保留原文。")
                            .child("觀察超過約 4 萬 token 時，Reflector 把它們合併、去重；釘選的觀察一定保留。")
                            .child("每個 project 的觀察在頻道的「記憶」分頁，可以釘選、修正或刪除。"),
                    ),
            )
    }

    fn render_memory_model_editor(
        &self,
        editor: &MemoryModelEditor,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let providers = &self.current_instance().providers;
        let option = |key: String,
                      label: String,
                      value: Option<ProviderId>,
                      cx: &mut Context<Self>| {
            let selected = editor.provider == value;
            div()
                .id(SharedString::from(key))
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .py(px(8.))
                .rounded(px(8.))
                .border_1()
                .cursor_pointer()
                .text_size(px(13.))
                .when_else(
                    selected,
                    |d| d.border_color(theme::accent()).bg(theme::accent_soft()),
                    |d| {
                        d.border_color(theme::border())
                            .bg(theme::white())
                            .hover(|s| s.bg(theme::subtle_bg()))
                    },
                )
                .child(icon(
                    if selected {
                        IconName::CircleCheck
                    } else {
                        IconName::Square
                    },
                    14.,
                    if selected {
                        theme::accent()
                    } else {
                        theme::text_muted()
                    },
                ))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.pick_memory_provider(value, cx)))
        };

        let mut options = div().flex().flex_col().gap(px(6.)).child(option(
            "memory-provider-same".into(),
            "跟對話 agent 相同".into(),
            None,
            cx,
        ));
        for p in &providers.items {
            let label = match p.kind {
                ProviderKind::ChatGpt => p.kind.label().to_string(),
                kind => format!("{}（{}）", p.name, kind.label()),
            };
            options = options.child(option(
                format!("memory-provider-{}", p.id),
                label,
                Some(p.id),
                cx,
            ));
        }

        let mut form = div().flex().flex_col().gap(px(12.)).child(field(
            "Provider",
            None,
            options.into_any_element(),
        ));
        if editor.provider.is_some() {
            form = form.child(field(
                "模型",
                Some("例如 gpt-4.1-mini、claude-haiku-4-5；留空用這個 provider 設定的模型"),
                Input::new(&editor.model).into_any_element(),
            ));
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
                        "memory-model-save",
                        if editor.saving {
                            "儲存中…"
                        } else {
                            "儲存"
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.save_memory_model(cx))),
                )
                .child(
                    link_button("memory-model-cancel", "取消")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_memory_model_edit(cx))),
                ),
        )
    }

    fn render_about_section(&self) -> Div {
        let inst = self.current_instance();
        let (status, color) = match inst.connection {
            ConnectionState::Connected => ("已連線", theme::ok()),
            ConnectionState::Reconnecting => ("重新連線中", theme::warn_text()),
            ConnectionState::Offline => ("離線", theme::danger()),
        };
        let mode = if self.live.is_some() {
            "連著實例的服務"
        } else {
            "假資料（--mock）"
        };
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                card()
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::BOLD)
                            .child("Pots"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(detail_row("版本", env!("CARGO_PKG_VERSION")))
                            .child(detail_row("模式", mode)),
                    ),
            )
            .child(
                card()
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::BOLD)
                            .child("這個實例"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(detail_row("名稱", inst.name.clone()))
                            .child(detail_row("形態", inst.kind.label()))
                            .child(detail_row("位置", inst.host.clone()))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(12.))
                                    .text_size(px(13.))
                                    .child(
                                        div()
                                            .w(px(72.))
                                            .flex_none()
                                            .text_color(theme::text_muted())
                                            .child("連線"),
                                    )
                                    .child(div().text_color(color).child(status)),
                            )
                            .child(detail_row("實例 ID", inst.id.to_string())),
                    ),
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
            if !p.headers.is_empty() {
                let names = p
                    .headers
                    .iter()
                    .map(|h| h.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                details = details.child(detail_row("Headers", names));
            }
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

    /// 新增 provider 的第一步：選種類。每一格說明它適合接哪些服務。
    fn render_provider_kind_picker(&self, cx: &mut Context<Self>) -> Div {
        let mut grid = div().grid().grid_cols(2).gap(px(8.));
        for (kind, id) in ADDABLE_KINDS {
            grid = grid.child(
                div()
                    .id(id)
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(12.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::white())
                    .cursor_pointer()
                    .hover(|s| s.border_color(theme::accent()).bg(theme::accent_soft()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(kind.label()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::text_muted())
                                    .child(kind_blurb(kind)),
                            ),
                    )
                    .child(icon(IconName::Plus, 16., theme::text_muted()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.edit_provider(None, kind, window, cx)
                    })),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .pt(px(8.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("新增 provider"),
            )
            .child(grid)
    }

    /// 表單依填寫的順序排：名稱 → 連到哪裡（Base URL、API key）→ 模型（旁邊就是「測試連線」
    /// 與它列出的清單）→ 很少用到的進階設定。
    fn render_provider_editor(
        &self,
        editor: &ProviderEditor,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let kind = editor.kind;
        let api = kind != ProviderKind::ChatGpt;
        let title = match (editor.id, kind) {
            (_, ProviderKind::ChatGpt) => "ChatGPT 方案的模型".to_string(),
            (None, _) => format!("新增 {}", kind.label()),
            (Some(_), _) => format!("編輯 {}", kind.label()),
        };

        let mut form = card().bg(theme::subtle_bg()).child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::BOLD)
                .child(title),
        );
        if api {
            form = form.child(field(
                "名稱",
                None,
                Input::new(&editor.name).into_any_element(),
            ));
            // 內建目錄的 base URL 很少要改，收在進階設定。
            if kind.preset().is_none() {
                form = form.child(self.render_base_url_field(editor, cx));
            }
            form = form.child(field(
                "API key",
                None,
                Input::new(&editor.api_key).mask_toggle().into_any_element(),
            ));
        }
        form = form.child(self.render_model_field(editor, cx));
        if api {
            form = form.child(self.render_provider_advanced(editor, cx));
        }
        if let Some(error) = &editor.error {
            form = form.child(status_line(
                IconName::CircleAlert,
                theme::danger(),
                error.clone(),
            ));
        }

        // 還沒有使用中的 provider 時，服務會把新增的這個設為使用中。
        let activates =
            api && editor.id.is_none() && saves_as_active(None, &self.current_instance().providers);
        let save = if editor.saving {
            "儲存中…"
        } else if activates {
            "儲存並設為使用中"
        } else {
            "儲存"
        };
        form.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    primary_button("provider-save", save)
                        .on_click(cx.listener(|this, _, _, cx| this.save_provider(cx))),
                )
                .child(
                    link_button("provider-cancel", "取消")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_provider_edit(cx))),
                ),
        )
    }

    /// Base URL；新增 OpenAI 相容 API 時附上常見服務的快速填入。
    fn render_base_url_field(&self, editor: &ProviderEditor, cx: &mut Context<Self>) -> Div {
        let kind = editor.kind;
        let shortcuts = editor.id.is_none() && kind == ProviderKind::OpenAiCompatible;
        let hint = match kind {
            ProviderKind::OpenAiCompatible if shortcuts => {
                "到 /v1 為止；其他相容的服務直接貼上它的 base URL"
            }
            ProviderKind::OpenAiCompatible => {
                "到 /v1 為止，例如 https://api.openai.com/v1、https://openrouter.ai/api/v1、http://localhost:11434/v1"
            }
            ProviderKind::OpenAiResponses => {
                "到 /v1 為止，例如 https://api.openai.com/v1；Pots 會呼叫 /responses，工具與推理可以並用"
            }
            ProviderKind::OpenCodeZen | ProviderKind::OpenCodeGo => {
                "OpenCode 的 API 根目錄；Pots 會依模型自動選 Anthropic／OpenAI 路徑"
            }
            _ => "例如 https://api.anthropic.com；Pots 會呼叫 /v1/messages",
        };
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(Input::new(&editor.base_url));
        if shortcuts {
            body = body.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child("快速填入"),
                    )
                    .children(PROVIDER_SHORTCUTS.iter().enumerate().map(|(i, shortcut)| {
                        div()
                            .id(SharedString::from(format!("provider-shortcut-{i}")))
                            .px(px(10.))
                            .py(px(2.))
                            .rounded_full()
                            .border_1()
                            .border_color(theme::border_strong())
                            .bg(theme::white())
                            .text_size(px(12.))
                            .cursor_pointer()
                            .hover(|s| s.border_color(theme::accent()).bg(theme::accent_soft()))
                            .child(if shortcut.local {
                                format!("{}（本機）", shortcut.name)
                            } else {
                                shortcut.name.to_string()
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.apply_provider_shortcut(i, window, cx)
                            }))
                    })),
            );
        }
        field("Base URL", Some(hint), body.into_any_element())
    }

    /// 模型：內建目錄的直接從清單挑；其他的填模型 ID，或「測試連線」後從服務回報的清單挑。
    /// 測試的按鈕、結果與清單都跟模型欄放在一起。
    fn render_model_field(&self, editor: &ProviderEditor, cx: &mut Context<Self>) -> Div {
        let kind = editor.kind;
        let picker = |no_match: &'static str| {
            Select::new(&editor.model_picker)
                .id("provider-model-picker")
                .placeholder("從清單選一個模型")
                .accessibility_label("模型")
                .search_placeholder("搜尋模型 ID 或名稱")
                .menu_max_h(px(280.))
                .empty(move |_, _| {
                    div()
                        .px(px(10.))
                        .py(px(6.))
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .child(no_match)
                })
        };
        if let Some(preset) = kind.preset() {
            let hint = format!(
                "{} 的內建清單，{} 個模型，可以搜尋",
                preset.label,
                preset.models.len()
            );
            return field(
                "模型",
                Some(&hint),
                picker("沒有符合的模型").into_any_element(),
            );
        }
        if kind == ProviderKind::ChatGpt {
            return field("模型", None, Input::new(&editor.model).into_any_element());
        }

        let running = matches!(editor.test, ProviderTest::Running(_));
        let body = div().flex().flex_col().gap(px(8.)).child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(div().flex_1().min_w_0().child(Input::new(&editor.model)))
                .child(
                    secondary_button(
                        "provider-test",
                        if running {
                            "測試中…"
                        } else {
                            "測試連線"
                        },
                    )
                    // 和旁邊的輸入框一樣高。
                    .py(px(4.))
                    .on_click(cx.listener(|this, _, _, cx| this.test_provider(cx))),
                ),
        );
        let body = match &editor.test {
            ProviderTest::Idle => body.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child("不知道模型 ID？按「測試連線」，從服務回報的清單挑。"),
            ),
            ProviderTest::Running(_) => body.child(
                div()
                    .text_size(px(13.))
                    .text_color(theme::text_muted())
                    .child("正在向服務要模型清單…"),
            ),
            ProviderTest::Err(message) => body.child(status_line(
                IconName::CircleAlert,
                theme::danger(),
                message.clone(),
            )),
            ProviderTest::Ok(models) if models.is_empty() => body.child(status_line(
                IconName::CircleCheck,
                theme::ok(),
                "連線成功，但服務沒有回報模型清單，直接填模型 ID。",
            )),
            ProviderTest::Ok(models) => body
                .child(status_line(
                    IconName::CircleCheck,
                    theme::ok(),
                    format!(
                        "連線成功，{} 個模型：從清單挑（可以搜尋），或直接填模型 ID",
                        models.len()
                    ),
                ))
                .child(picker("沒有符合的模型，可以直接在上面填 ID")),
        };
        field("模型", None, body.into_any_element())
    }

    /// 很少用到的設定預設收著：額外 headers；內建目錄的 provider 還有 Base URL。
    fn render_provider_advanced(&self, editor: &ProviderEditor, cx: &mut Context<Self>) -> Div {
        let preset = editor.kind.preset().is_some();
        let mut summary = if preset {
            "Base URL、額外 headers".to_string()
        } else {
            "額外 headers".to_string()
        };
        if !editor.headers.is_empty() {
            summary.push_str(&format!("（{}）", editor.headers.len()));
        }
        let toggle = div()
            .id("provider-advanced")
            .flex()
            .items_center()
            .gap(px(6.))
            .cursor_pointer()
            .text_size(px(13.))
            .text_color(theme::text_muted())
            .hover(|s| s.text_color(theme::text()))
            .child(icon(
                if editor.advanced_open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
                14.,
                theme::text_muted(),
            ))
            .child(div().font_weight(FontWeight::SEMIBOLD).child("進階設定"))
            .child(summary)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_provider_advanced(cx)));
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(div().flex().child(toggle));
        if !editor.advanced_open {
            return section;
        }
        if preset {
            section = section.child(self.render_base_url_field(editor, cx));
        }

        let mut headers = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("額外 headers"),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child("每次請求都會送；值不是秘密，API key 請填在上面。"),
            );
        for (i, row) in editor.headers.iter().enumerate() {
            let index = i;
            headers =
                headers.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(div().flex_1().child(Input::new(&row.name)))
                        .child(div().flex_1().child(Input::new(&row.value)))
                        .child(
                            link_button(
                                SharedString::from(format!("provider-header-remove-{i}")),
                                "移除",
                            )
                            .text_color(theme::danger())
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.remove_provider_header(index, cx),
                            )),
                        ),
                );
        }
        headers =
            headers.child(div().flex().child(
                secondary_button("provider-header-add", "新增 header").on_click(
                    cx.listener(|this, _, window, cx| this.add_provider_header(window, cx)),
                ),
            ));
        section.child(headers)
    }
}
