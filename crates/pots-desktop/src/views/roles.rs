//! 設定視窗的「角色」類別：對話 agent、總管、Planner、內建 worker 與 subagent 各自用的
//! provider、模型與 thinking 強度。沒有另外設定的角色跟使用中的 provider 相同。
//!
//! 「編輯」打開和「記憶」一樣的表單：provider、模型（選了特定 provider 才能填；內建目錄或
//! 服務列出的模型可以在下拉選單搜尋）、thinking 的分段按鈕。「儲存」送 `SetRoleModel`，
//! 服務回 `Providers` 時收起表單，被拒絕時原因留在表單裡。

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Div, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px, relative,
};

use super::settings::{card, field};
use super::widgets::*;
use crate::app::{ModelChoice, PotsApp, ProviderTest, RoleModelEditor, fill_model_picker};
use crate::model::*;
use crate::theme;

const THINKING_NOTE: &str = "thinking 會依 provider 轉換：OpenAI 是 reasoning effort，Anthropic 是 \
     thinking。不支援的模型可能會拒絕請求，這時改回「模型預設」。";

const WORKER_NOTE: &str = "Claude Code worker 用「Worker」設定裡的模型，這裡只影響內建 worker。";

/// 角色在做什麼，列在名稱下面。
fn role_hint(role: ModelRole) -> &'static str {
    match role {
        ModelRole::Chat => "每個 project 頻道裡和你對話、開 Goal",
        ModelRole::Global => "跨 project 的總管對話",
        ModelRole::Planner => "把 Goal 拆成任務、追進度、整理 Ledger",
        ModelRole::Worker => "在任務分支上寫程式、跑測試、開 PR",
        ModelRole::Explorer => "唯讀查程式碼，替其他角色找資料",
        ModelRole::Reviewer => "唯讀審查 Spec 與程式碼",
    }
}

/// provider 的模型欄留空時顯示什麼。
fn provider_model(p: &ProviderView, model: &str) -> String {
    if !model.is_empty() {
        model.to_string()
    } else if p.kind == ProviderKind::ChatGpt {
        "自動".into()
    } else {
        "未設定模型".into()
    }
}

fn provider_option_label(p: &ProviderView) -> String {
    match p.kind {
        ProviderKind::ChatGpt => p.kind.label().to_string(),
        kind => format!("{}（{}）", p.name, kind.label()),
    }
}

fn muted_text(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(13.))
        .text_color(theme::text_muted())
        .line_height(relative(1.5))
        .child(text.into())
}

impl PotsApp {
    /// 開啟一個角色的表單，帶入目前的設定（同時只有一個角色在編輯）。
    pub fn edit_role_model(
        &mut self,
        role: ModelRole,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.current_instance().providers.role(role);
        let model = cx.new(|cx| {
            let mut state =
                InputState::new(window, cx).placeholder("留空用這個 provider 設定的模型");
            state.set_value(current.model.clone(), window, cx);
            state
        });
        let model_picker = cx.new(|cx| {
            SelectState::new(SearchableVec::new(Vec::new()), None, window, cx).searchable(true)
        });
        let subscriptions = vec![
            // 模型欄改了，下拉選單的選取也跟著對齊。
            cx.subscribe_in(&model, window, |this, _, ev: &InputEvent, window, cx| {
                if !matches!(ev, InputEvent::Change) {
                    return;
                }
                let Some(editor) = &this.role_model_editor else {
                    return;
                };
                let model = editor.model.read(cx).value().trim().to_string();
                let picker = editor.model_picker.clone();
                picker.update(cx, |select, cx| {
                    select.set_selected_value(&model, window, cx)
                });
            }),
            // 下拉選單選了模型就填回模型欄。
            cx.subscribe_in(
                &model_picker,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<ModelChoice>>, window, cx| {
                    let SelectEvent::Confirm(Some(model)) = ev else {
                        return;
                    };
                    if let Some(editor) = &this.role_model_editor {
                        let model = model.clone();
                        editor
                            .model
                            .update(cx, |s, cx| s.set_value(model.clone(), window, cx));
                        // `set_value` 不發 `InputEvent`，下拉選單的選取要自己對齊。
                        editor.model_picker.update(cx, |select, cx| {
                            select.set_selected_value(&model, window, cx)
                        });
                    }
                    cx.notify();
                },
            ),
        ];
        self.role_model_editor = Some(RoleModelEditor {
            role,
            provider: current.provider,
            model,
            model_picker,
            models: ProviderTest::Idle,
            thinking: current.thinking,
            saving: false,
            error: None,
            _subscriptions: subscriptions,
        });
        self.load_role_models(window, cx);
        cx.notify();
    }

    /// 表單裡選一個 provider；`None` 是跟使用中的 provider 相同。換了 provider 時模型欄清空。
    pub fn pick_role_provider(
        &mut self,
        provider: Option<ProviderId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = &mut self.role_model_editor else {
            return;
        };
        if editor.provider != provider {
            editor.provider = provider;
            editor.model.update(cx, |s, cx| s.set_value("", window, cx));
            self.load_role_models(window, cx);
        }
        if let Some(editor) = &mut self.role_model_editor {
            editor.error = None;
        }
        cx.notify();
    }

    /// 選了特定 provider 時列出它的模型：內建目錄直接有；其他 provider 用已存的設定（含 key）
    /// 向服務問一次（`TestProvider`）。假資料模式沒有服務，只能直接填模型 ID。
    fn load_role_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let provider = self
            .role_model_editor
            .as_ref()
            .and_then(|e| e.provider)
            .and_then(|id| self.current_instance().providers.get(id).cloned());
        let live = self.live.is_some();
        let instance = self.instance;
        let Some(editor) = &mut self.role_model_editor else {
            return;
        };
        fill_model_picker(&editor.model_picker, &[], "", window, cx);
        editor.models = ProviderTest::Idle;
        let Some(p) = provider else {
            return;
        };
        if let Some(preset) = p.kind.preset() {
            let models: Vec<ModelOption> = preset
                .models
                .iter()
                .map(|m| ModelOption {
                    id: m.id.clone(),
                    display_name: m.name.clone(),
                })
                .collect();
            let current = editor.model.read(cx).value().trim().to_string();
            fill_model_picker(&editor.model_picker, &models, &current, window, cx);
            editor.models = ProviderTest::Ok(models);
        } else if live {
            self.next_test_request += 1;
            let request = self.next_test_request;
            editor.models = ProviderTest::Running(request);
            let draft = ProviderDraft {
                id: Some(p.id),
                kind: p.kind,
                name: p.name,
                base_url: p.base_url,
                model: p.model,
                api_key: None,
                headers: p.headers,
            };
            self.store
                .outbox
                .push((instance, ClientCommand::TestProvider { request, draft }));
        }
    }

    pub fn set_role_thinking(&mut self, thinking: ThinkingLevel, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.role_model_editor {
            editor.thinking = thinking;
            editor.error = None;
        }
        cx.notify();
    }

    pub fn save_role_model(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.role_model_editor else {
            return;
        };
        let (role, provider, thinking) = (editor.role, editor.provider, editor.thinking);
        let model = if provider.is_some() {
            editor.model.read(cx).value().trim().to_string()
        } else {
            String::new()
        };
        let sent = self.settings_command(
            ClientCommand::SetRoleModel {
                role,
                provider,
                model,
                thinking,
            },
            "儲存角色模型",
            cx,
        );
        if let Some(editor) = &mut self.role_model_editor {
            editor.saving = sent;
            editor.error = None;
        }
    }

    pub fn cancel_role_model_edit(&mut self, cx: &mut Context<Self>) {
        self.role_model_editor = None;
        cx.notify();
    }

    pub(super) fn render_roles_section(&self, cx: &mut Context<Self>) -> Div {
        let providers = &self.current_instance().providers;
        let mut list = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(muted_text(
                "每個角色可以用不同的 provider、模型與 thinking 強度，例如 Planner 用比較強的模型、\
                 Explorer 用便宜快速的模型。沒有另外設定的角色跟使用中的 provider 相同。",
            ))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(8.))
                    .bg(theme::subtle_bg())
                    .border_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .pt(px(2.))
                            .child(icon(IconName::Info, 14., theme::text_muted())),
                    )
                    .child(muted_text(THINKING_NOTE).flex_1().min_w_0()),
            );
        for role in ModelRole::ALL {
            let editing = self.role_model_editor.as_ref().filter(|e| e.role == role);
            list = list.child(self.render_role_card(providers, role, editing, cx));
        }
        list
    }

    fn render_role_card(
        &self,
        providers: &ProvidersView,
        role: ModelRole,
        editor: Option<&RoleModelEditor>,
        cx: &mut Context<Self>,
    ) -> Div {
        let config = providers.role(role);
        let (model, model_color) = match (config.provider, providers.resolve_role(role)) {
            (_, Some((p, model))) => {
                let model = provider_model(p, &model);
                (
                    match config.provider {
                        None => format!("跟使用中的 provider 相同（目前：{} · {model}）", p.name),
                        Some(_) => format!("{} · {model}", p.name),
                    },
                    theme::text(),
                )
            }
            (None, None) => ("還沒有使用中的 provider".to_string(), theme::text_muted()),
            (Some(_), None) => (
                "設定的 provider 已經刪除，請重新選擇".to_string(),
                theme::danger(),
            ),
        };
        let thinking = if config.thinking == ThinkingLevel::Default {
            tag(
                format!("思考 {}", config.thinking.label()),
                theme::chip_text(),
                theme::chip_bg(),
            )
        } else {
            tag(
                format!("思考 {}", config.thinking.label()),
                theme::accent_text(),
                theme::accent_soft(),
            )
        };

        let title = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::BOLD)
                    .child(role.label()),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(role_hint(role)),
            )
            .when(editor.is_none(), |d| {
                d.child(
                    link_button(
                        SharedString::from(format!("role-edit-{}", role.profile())),
                        "編輯",
                    )
                    .ml_auto()
                    .py(px(0.))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.edit_role_model(role, window, cx)
                    })),
                )
            });

        let summary = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(13.))
            .child(div().min_w_0().text_color(model_color).child(model))
            .child(thinking);

        card()
            .gap(px(8.))
            .px(px(18.))
            .py(px(14.))
            .when(editor.is_some(), |d| d.border_color(theme::accent()))
            .child(title)
            .child(summary)
            .when(role == ModelRole::Worker, |d| {
                d.child(muted_text(WORKER_NOTE).text_size(px(12.)))
            })
            .when_some(editor, |d, editor| {
                d.child(self.render_role_editor(providers, editor, cx))
            })
    }

    fn render_role_editor(
        &self,
        providers: &ProvidersView,
        editor: &RoleModelEditor,
        cx: &mut Context<Self>,
    ) -> Div {
        let option =
            |key: String, label: String, value: Option<ProviderId>, cx: &mut Context<Self>| {
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.pick_role_provider(value, window, cx)
                    }))
            };

        let same = match providers.active() {
            Some(p) => format!(
                "跟使用中的 provider 相同（{} · {}）",
                p.name,
                provider_model(p, &p.model)
            ),
            None => "跟使用中的 provider 相同".to_string(),
        };
        let mut options = div().flex().flex_col().gap(px(6.)).child(option(
            "role-provider-same".into(),
            same,
            None,
            cx,
        ));
        for p in &providers.items {
            options = options.child(option(
                format!("role-provider-{}", p.id),
                provider_option_label(p),
                Some(p.id),
                cx,
            ));
        }

        let mut form = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .mt(px(4.))
            .pt(px(14.))
            .border_t_1()
            .border_color(theme::border())
            .child(field("Provider", None, options.into_any_element()));

        if let Some(p) = editor.provider.and_then(|id| providers.get(id)) {
            let hint = format!(
                "留空用這個 provider 設定的模型（{}）",
                provider_model(p, &p.model)
            );
            let mut model = div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(Input::new(&editor.model));
            match &editor.models {
                ProviderTest::Idle => {}
                ProviderTest::Running(_) => {
                    model = model.child(muted_text("正在列出模型…"));
                }
                ProviderTest::Err(message) => {
                    model = model.child(muted_text(format!(
                        "列不出模型（{message}），直接填模型 ID。"
                    )));
                }
                ProviderTest::Ok(models) if models.is_empty() => {
                    model = model.child(muted_text("這個 provider 沒有回報模型，直接填模型 ID。"));
                }
                ProviderTest::Ok(models) => {
                    model = model.child(
                        Select::new(&editor.model_picker)
                            .id("role-model-picker")
                            .placeholder(format!("從 {} 個模型裡選一個", models.len()))
                            .accessibility_label("模型")
                            .search_placeholder("搜尋模型 ID 或名稱")
                            .menu_max_h(px(280.))
                            .empty(|_, _| {
                                div()
                                    .px(px(10.))
                                    .py(px(6.))
                                    .text_size(px(13.))
                                    .text_color(theme::text_muted())
                                    .child("沒有符合的模型，可以直接在上面填 ID")
                            }),
                    );
                }
            }
            form = form.child(field("模型", Some(&hint), model.into_any_element()));
        }

        let thinking = ThinkingLevel::ALL
            .into_iter()
            .fold(segment_group(), |group, level| {
                group.child(
                    segment(
                        SharedString::from(format!("role-thinking-{level:?}")),
                        level.label(),
                        editor.thinking == level,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.set_role_thinking(level, cx))),
                )
            });
        form = form.child(field(
            "Thinking",
            Some("「模型預設」不送參數；不支援 thinking 的模型可能會拒絕其他選項"),
            div().flex().child(thinking).into_any_element(),
        ));

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
                        "role-model-save",
                        if editor.saving {
                            "儲存中…"
                        } else {
                            "儲存"
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.save_role_model(cx))),
                )
                .child(
                    link_button("role-model-cancel", "取消")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_role_model_edit(cx))),
                ),
        )
    }
}
