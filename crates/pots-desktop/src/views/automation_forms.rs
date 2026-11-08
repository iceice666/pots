//! M3 的表單：設定視窗的「GitHub」類別、右欄的「新增 listener」、頻道「自動化」分頁的監看
//! repo，以及暫停與來源狀態。
//!
//! 送出的指令都經 `Store::outbox`；服務接受後以 `Github`、`Automation`、`ProjectRepos` 事件
//! 回來時收起表單，被拒絕時（`Error`）把原因留在表單裡。

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Div, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, px, relative,
};

use super::panel::{panel_frame, white_box};
use super::settings::{card, detail_row, field};
use super::widgets::*;
use crate::app::{
    GithubEditor, GithubTest, ListenerActionKind, ListenerForm, Panel, PotsApp, RepoEditor,
};
use crate::model::*;
use crate::sync::{Applied, month_day_time};
use crate::theme;

fn input(
    value: String,
    placeholder: &str,
    masked: bool,
    window: &mut Window,
    cx: &mut Context<PotsApp>,
) -> Entity<InputState> {
    let placeholder = placeholder.to_string();
    cx.new(|cx| {
        let mut state = InputState::new(window, cx)
            .placeholder(placeholder)
            .masked(masked);
        state.set_value(value, window, cx);
        state
    })
}

fn value(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_string()
}

/// 逗號或空白分開的清單（repo）。
fn list(text: &str) -> Vec<String> {
    text.split([',', '，', ' ', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// 逗號分開的清單（label、作者等可能有空白，例如「good first issue」）。
fn comma_list(text: &str) -> Vec<String> {
    text.split([',', '，', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// 這些事件有沒有這個欄位（沒有的欄位當條件永遠不會符合）。
fn kinds_have_field(kinds: &[&str], field: &str) -> bool {
    kinds.iter().any(|k| match field {
        "label" => *k == "issue.labeled",
        "branch" | "conclusion" => *k == "ci.finished",
        "author" => *k != "ci.finished",
        _ => false,
    })
}

fn error_note(text: &str) -> Div {
    div()
        .px(px(12.))
        .py(px(8.))
        .rounded(px(8.))
        .bg(theme::danger_soft())
        .text_color(theme::danger())
        .text_size(px(13.))
        .child(text.to_string())
}

fn chip(id: SharedString, label: &str, selected: bool) -> gpui_kit::Stateful<Div> {
    div()
        .id(id)
        .px(px(10.))
        .py(px(4.))
        .rounded(px(999.))
        .border_1()
        .text_size(px(13.))
        .cursor_pointer()
        .when_else(
            selected,
            |d| {
                d.border_color(theme::accent())
                    .bg(theme::accent_soft())
                    .text_color(theme::accent())
                    .font_weight(FontWeight::SEMIBOLD)
            },
            |d| {
                d.border_color(theme::border())
                    .hover(|s| s.bg(theme::subtle_bg()))
            },
        )
        .child(label.to_string())
}

/// 表單狀態的借用，讓 app 在持有連線（`live`）時也能更新它們。
pub(crate) struct Forms<'a> {
    pub github: &'a mut Option<GithubEditor>,
    pub listener: &'a mut Option<ListenerForm>,
    pub repo: &'a mut Option<RepoEditor>,
    pub panel: &'a mut Option<Panel>,
}

impl Forms<'_> {
    /// 斷線時等不到回覆了。
    pub(crate) fn on_disconnected(&mut self) {
        if let Some(e) = self.github {
            if matches!(e.test, GithubTest::Running(_)) {
                e.test = GithubTest::Idle;
            }
            if e.saving {
                e.saving = false;
                e.error = Some("連線中斷，請再儲存一次".into());
            }
        }
        if let Some(f) = self.listener
            && f.saving
        {
            f.saving = false;
            f.error = Some("連線中斷，請再儲存一次".into());
        }
        if let Some(r) = self.repo
            && r.saving
        {
            r.saving = false;
            r.error = Some("連線中斷，請再儲存一次".into());
        }
    }

    /// 服務拒絕了正在儲存的表單：原因放進表單，回傳 `None`；不是表單的錯誤原樣回傳。
    pub(crate) fn route_error(&mut self, toast: String) -> Option<String> {
        if let Some(e) = self.github
            && e.saving
        {
            e.saving = false;
            e.error = Some(toast);
            return None;
        }
        if let Some(f) = self.listener
            && f.saving
        {
            f.saving = false;
            f.error = Some(toast);
            return None;
        }
        if let Some(r) = self.repo
            && r.saving
        {
            r.saving = false;
            r.error = Some(toast);
            return None;
        }
        Some(toast)
    }

    /// 服務回了 GitHub 設定、自動化或 repo：收起儲存中的表單、填上測試結果。
    pub(crate) fn applied(&mut self, applied: &Applied) {
        if applied.github_changed && self.github.as_ref().is_some_and(|e| e.saving) {
            *self.github = None;
        }
        if let Some((request, result)) = &applied.github_test
            && let Some(e) = self.github
            && e.test == GithubTest::Running(*request)
        {
            e.test = match result {
                Ok(login) => GithubTest::Ok(login.clone()),
                Err(message) => GithubTest::Err(message.clone()),
            };
        }
        // 新的 listener 出現了才算建立成功（其他原因的 `Automation` 不收起表單）。
        if applied.automation_changed
            && self
                .listener
                .as_ref()
                .is_some_and(|f| f.saving && applied.listener_count > f.listeners_before)
        {
            *self.listener = None;
            if *self.panel == Some(Panel::NewListener) {
                *self.panel = None;
            }
        }
        if let Some(r) = &self.repo
            && r.saving
            && applied.repos_changed == Some(r.project)
        {
            *self.repo = None;
        }
    }
}

impl PotsApp {
    // ---- 事件 ---------------------------------------------------------------

    // ---- GitHub 設定 ----------------------------------------------------------

    pub fn edit_github(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let gh = self.current_instance().github.clone();
        let token_placeholder = if gh.token_hint.is_some() {
            "留空沿用已存的 token"
        } else {
            "貼上 fine-grained personal access token"
        };
        self.github_editor = Some(GithubEditor {
            token: input(String::new(), token_placeholder, true, window, cx),
            api_base: input(
                gh.api_base.clone(),
                "https://api.github.com",
                false,
                window,
                cx,
            ),
            webhook_secret: input(
                String::new(),
                if gh.webhook_secret_set {
                    "留空沿用已存的 secret"
                } else {
                    "有公開網址時才需要"
                },
                true,
                window,
                cx,
            ),
            test: GithubTest::Idle,
            saving: false,
            error: None,
        });
        cx.notify();
    }

    pub fn cancel_github(&mut self, cx: &mut Context<Self>) {
        self.github_editor = None;
        cx.notify();
    }

    fn github_draft(&self, cx: &App) -> Option<GithubDraft> {
        let e = self.github_editor.as_ref()?;
        let gh = &self.current_instance().github;
        let token = value(&e.token, cx);
        let secret = value(&e.webhook_secret, cx);
        let base = value(&e.api_base, cx);
        Some(GithubDraft {
            token: (!token.is_empty()).then_some(token),
            // 沒改就不送，免得服務以為換了 API 而要求重新輸入 token。
            api_base: (base != gh.api_base).then_some(base),
            webhook_secret: (!secret.is_empty()).then_some(secret),
        })
    }

    pub fn save_github(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.github_draft(cx) else {
            return;
        };
        let sent =
            self.settings_command(ClientCommand::SaveGithub { draft }, "儲存 GitHub 設定", cx);
        if let Some(e) = &mut self.github_editor {
            e.saving = sent;
            e.error = None;
        }
    }

    pub fn test_github(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.github_draft(cx) else {
            return;
        };
        self.next_test_request += 1;
        let request = self.next_test_request;
        let sent =
            self.settings_command(ClientCommand::TestGithub { request, draft }, "測試連線", cx);
        if let Some(e) = &mut self.github_editor {
            e.test = if sent {
                GithubTest::Running(request)
            } else {
                GithubTest::Err("沒有連到服務，不能測試".into())
            };
        }
    }

    /// 清掉 webhook secret（不再接受 webhook）。
    pub fn clear_github_webhook(&mut self, cx: &mut Context<Self>) {
        let draft = GithubDraft {
            webhook_secret: Some(String::new()),
            ..Default::default()
        };
        self.settings_command(
            ClientCommand::SaveGithub { draft },
            "清除 webhook secret",
            cx,
        );
    }

    pub(crate) fn render_github_section(&self, cx: &mut Context<Self>) -> Div {
        let inst = self.current_instance();
        let gh = &inst.github;
        let source = inst.sources.iter().find(|s| s.kind == "GitHub");
        let (status, color) = match (&gh.token_hint, source.map(|s| &s.health)) {
            (None, _) => ("還沒設定 token".to_string(), theme::text_muted()),
            (Some(_), Some(SourceHealth::Error { message, .. })) => {
                (message.clone(), theme::danger())
            }
            (Some(_), Some(SourceHealth::Ok)) => (
                match &gh.login {
                    Some(l) => format!("連線正常（{l}）"),
                    None => "連線正常".into(),
                },
                theme::ok(),
            ),
            (Some(_), _) => ("還沒輪詢過".to_string(), theme::text_muted()),
        };
        let mut out = div().flex().flex_col().gap(px(12.)).child(
            div()
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .line_height(relative(1.5))
                .child(
                    "用 fine-grained personal access token 讀取 project 監看的 repo（Contents、Issues、\
                     Pull requests、Checks 的唯讀權限）。這台電腦每分鐘輪詢一次；有公開網址時也可以在 \
                     repo 設定 webhook 打 /webhooks/github。token 只存在這個實例的資料目錄，畫面只顯示末四碼。",
                ),
        );
        let summary = card()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("GitHub"))
                    .child(div().text_size(px(13.)).text_color(color).child(status)),
            )
            .child(detail_row(
                "Token",
                gh.token_hint.clone().unwrap_or_else(|| "（未設定）".into()),
            ))
            .child(detail_row("API", gh.api_base.clone()))
            .child(detail_row(
                "Webhook",
                if gh.webhook_secret_set {
                    "已設定 secret"
                } else {
                    "未設定（只用輪詢）"
                },
            ))
            .when_some(source.and_then(|s| s.last_ok), |d, at| {
                d.child(detail_row("上次成功", month_day_time(at)))
            })
            .when(self.github_editor.is_none(), |d| {
                d.child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            secondary_button(
                                "github-edit",
                                if gh.token_hint.is_some() {
                                    "修改"
                                } else {
                                    "設定 token"
                                },
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.edit_github(window, cx)),
                            ),
                        )
                        .when(gh.webhook_secret_set, |d| {
                            d.child(
                                link_button("github-clear-webhook", "清除 webhook secret")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.clear_github_webhook(cx)),
                                    ),
                            )
                        }),
                )
            });
        out = out.child(summary);
        if let Some(e) = &self.github_editor {
            let test = match &e.test {
                GithubTest::Idle => None,
                GithubTest::Running(_) => Some(("測試中…".to_string(), theme::text_muted())),
                GithubTest::Ok(login) => Some((format!("可以連線，帳號 {login}"), theme::ok())),
                GithubTest::Err(m) => Some((m.clone(), theme::danger())),
            };
            out = out.child(
                card()
                    .child(field(
                        "Token",
                        Some("Settings → Developer settings → Fine-grained tokens"),
                        Input::new(&e.token).into_any_element(),
                    ))
                    .child(field(
                        "API base URL",
                        Some("GitHub Enterprise 是 https://<host>/api/v3；改了要重新輸入 token"),
                        Input::new(&e.api_base).into_any_element(),
                    ))
                    .child(field(
                        "Webhook secret",
                        Some("repo 的 webhook 設定裡填同一個 secret，Content type 選 application/json"),
                        Input::new(&e.webhook_secret).into_any_element(),
                    ))
                    .when_some(e.error.as_deref(), |d, m| d.child(error_note(m)))
                    .when_some(test, |d, (t, c)| {
                        d.child(div().text_size(px(13.)).text_color(c).child(t))
                    })
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                primary_button("github-save", if e.saving { "儲存中…" } else { "儲存" })
                                    .on_click(cx.listener(|this, _, _, cx| this.save_github(cx))),
                            )
                            .child(
                                secondary_button("github-test", "測試連線")
                                    .on_click(cx.listener(|this, _, _, cx| this.test_github(cx))),
                            )
                            .child(
                                link_button("github-cancel", "取消")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_github(cx))),
                            ),
                    ),
            );
        }
        out
    }

    // ---- 新增 listener ---------------------------------------------------------

    pub fn new_listener(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project = self
            .automation_filter
            .or_else(|| self.current_project().map(|p| p.id))
            .or_else(|| self.current_instance().projects.first().map(|p| p.id));
        self.listener_form = Some(ListenerForm {
            project,
            kinds: vec!["issue.labeled"],
            action: ListenerActionKind::CreateGoal,
            label: input("agent".into(), "例如 agent", false, window, cx),
            author: input(String::new(), "GitHub 帳號，逗號分開", false, window, cx),
            branch: input(String::new(), "例如 main", false, window, cx),
            conclusion: input(String::new(), "success 或 failure", false, window, cx),
            template: input(
                "依照 issue 的描述修好問題，附上測試。".into(),
                "Goal brief 的模板，事件內容會自動附上",
                false,
                window,
                cx,
            ),
            max_per_hour: input("5".into(), "1 到 60", false, window, cx),
            saving: false,
            listeners_before: 0,
            error: None,
        });
        self.panel = Some(Panel::NewListener);
        cx.notify();
    }

    pub fn cancel_listener_form(&mut self, cx: &mut Context<Self>) {
        self.listener_form = None;
        if self.panel == Some(Panel::NewListener) {
            self.panel = None;
        }
        cx.notify();
    }

    pub fn toggle_listener_kind(&mut self, kind: &'static str, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.listener_form {
            if let Some(i) = f.kinds.iter().position(|k| *k == kind) {
                f.kinds.remove(i);
            } else {
                f.kinds.push(kind);
            }
        }
        cx.notify();
    }

    pub fn set_listener_action(&mut self, action: ListenerActionKind, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.listener_form {
            f.action = action;
        }
        cx.notify();
    }

    pub fn set_listener_project(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.listener_form {
            f.project = Some(project);
        }
        cx.notify();
    }

    fn listener_spec(&self, cx: &App) -> Result<ListenerSpec, String> {
        let f = self.listener_form.as_ref().ok_or("沒有表單")?;
        let project = f.project.ok_or("先選一個 project")?;
        if f.kinds.is_empty() {
            return Err("至少選一種事件".into());
        }
        let mut fields = Vec::new();
        for (name, input) in [
            ("label", &f.label),
            ("author", &f.author),
            ("branch", &f.branch),
            ("conclusion", &f.conclusion),
        ] {
            let values = comma_list(&value(input, cx));
            // 選的事件沒有這個欄位：不當條件（否則永遠不會觸發）。
            if !values.is_empty() && kinds_have_field(&f.kinds, name) {
                fields.push(FieldMatch {
                    field: name.into(),
                    any_of: values,
                });
            }
        }
        let template = value(&f.template, cx);
        let action = match f.action {
            ListenerActionKind::CreateGoal => ListenerActionSpec::CreateGoal { template },
            ListenerActionKind::QuickFix => ListenerActionSpec::QuickFix { template },
            ListenerActionKind::NotifyChat => ListenerActionSpec::NotifyChat,
        };
        let max_per_hour = value(&f.max_per_hour, cx)
            .parse()
            .map_err(|_| "每小時上限要是數字".to_string())?;
        Ok(ListenerSpec {
            project,
            kinds: f.kinds.iter().map(|k| k.to_string()).collect(),
            resource: None,
            fields,
            action,
            max_per_hour,
            budget_per_trigger_cents: None,
        })
    }

    pub fn save_listener_form(&mut self, cx: &mut Context<Self>) {
        let spec = self.listener_spec(cx);
        let Some(f) = &mut self.listener_form else {
            return;
        };
        let spec = match spec {
            Ok(s) => s,
            Err(e) => {
                f.error = Some(e);
                cx.notify();
                return;
            }
        };
        f.error = None;
        if self.live.is_none() {
            self.notify_unimplemented("新增 listener", cx);
            return;
        }
        let instance = self.instance;
        let before = self.current_instance().listeners.len();
        self.store.save_listener(instance, spec);
        if let Some(f) = &mut self.listener_form {
            f.saving = true;
            f.listeners_before = before;
        }
        cx.notify();
    }

    pub(crate) fn render_listener_form(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let f = self.listener_form.as_ref()?;
        let inst = self.current_instance();
        let projects = div()
            .flex()
            .flex_wrap()
            .gap(px(6.))
            .children(inst.projects.iter().map(|p| {
                let id = p.id;
                chip(
                    SharedString::from(format!("listener-project-{}", p.id.0)),
                    &p.name,
                    f.project == Some(id),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_listener_project(id, cx)))
            }));
        let kinds = div()
            .flex()
            .flex_wrap()
            .gap(px(6.))
            .children(EVENT_KINDS.iter().map(|(k, label)| {
                let k: &'static str = k;
                chip(
                    SharedString::from(format!("listener-kind-{k}")),
                    label,
                    f.kinds.contains(&k),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_listener_kind(k, cx)))
            }));
        let actions = div().flex().gap(px(6.)).children(
            [
                (ListenerActionKind::CreateGoal, "開 Goal"),
                (ListenerActionKind::QuickFix, "快速修正"),
                (ListenerActionKind::NotifyChat, "通知對話"),
            ]
            .into_iter()
            .map(|(a, label)| {
                chip(
                    SharedString::from(format!("listener-action-{a:?}")),
                    label,
                    f.action == a,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_listener_action(a, cx)))
            }),
        );
        let repos = f
            .project
            .and_then(|p| inst.projects.iter().find(|x| x.id == p))
            .map(|p| p.repos.clone())
            .unwrap_or_default();
        let body = div()
            .id("listener-form-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(20.))
            .pb(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .when(repos.is_empty(), |d| {
                d.child(
                    div()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(8.))
                        .bg(theme::warn_soft())
                        .text_color(theme::warn_text())
                        .text_size(px(13.))
                        .child("這個 project 還沒有監看的 repo：到頻道的「自動化」分頁加上 repo，listener 才收得到事件。"),
                )
            })
            .child(white_box().gap(px(8.)).child(section_label("Project")).child(projects))
            .child(
                white_box()
                    .gap(px(8.))
                    .child(section_label("事件"))
                    .child(kinds)
                    .child(field("label", Some("issue 加上其中一個 label 才算"), Input::new(&f.label).into_any_element()))
                    .child(field(
                        "author",
                        Some("限定作者；任何人都能觸發的事件（issue 開啟、留言、review）要自動開工必須填"),
                        Input::new(&f.author).into_any_element(),
                    ))
                    .child(field("branch", Some("CI 看哪些分支；留空是預設分支"), Input::new(&f.branch).into_any_element()))
                    .child(field("conclusion", None, Input::new(&f.conclusion).into_any_element())),
            )
            .child(
                white_box()
                    .gap(px(8.))
                    .child(section_label("動作"))
                    .child(actions)
                    .when(f.action != ListenerActionKind::NotifyChat, |d| {
                        d.child(field("Brief 模板", None, Input::new(&f.template).into_any_element()))
                    })
                    .child(field("每小時最多", Some("超過就暫停這個 listener 並通知你"), Input::new(&f.max_per_hour).into_any_element())),
            )
            .when_some(f.error.as_deref(), |d, m| d.child(error_note(m)));
        Some(
            panel_frame()
                .child(
                    div()
                        .flex()
                        .items_start()
                        .px(px(20.))
                        .pt(px(16.))
                        .pb(px(12.))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(17.))
                                .font_weight(FontWeight::BOLD)
                                .child("新增 listener"),
                        )
                        .child(self.close_button(cx)),
                )
                .child(body)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(20.))
                        .py(px(16.))
                        .border_t_1()
                        .border_color(theme::border())
                        .child(
                            primary_button(
                                "listener-save",
                                if f.saving { "儲存中…" } else { "建立" },
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.save_listener_form(cx))),
                        )
                        .child(
                            link_button("listener-cancel", "取消").on_click(
                                cx.listener(|this, _, _, cx| this.cancel_listener_form(cx)),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }

    // ---- 監看的 repo ------------------------------------------------------------

    pub fn edit_repos(&mut self, project: ProjectId, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .store
            .project(self.instance, project)
            .map(|p| {
                p.repos
                    .iter()
                    .map(|r| r.strip_prefix("github.com/").unwrap_or(r).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let state = input(
            current,
            "owner/repo，逗號分開；Enter 儲存",
            false,
            window,
            cx,
        );
        state.update(cx, |s, cx| s.focus(window, cx));
        let sub = cx.subscribe(&state, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.save_repos(cx);
            }
        });
        self.repo_editor = Some(RepoEditor::new(project, state, sub));
        cx.notify();
    }

    pub fn cancel_repos(&mut self, cx: &mut Context<Self>) {
        self.repo_editor = None;
        cx.notify();
    }

    pub fn save_repos(&mut self, cx: &mut Context<Self>) {
        let Some(r) = &self.repo_editor else {
            return;
        };
        let project = r.project;
        let repos = list(&value(&r.input, cx));
        if self.live.is_none() {
            self.notify_unimplemented("設定 repo", cx);
            return;
        }
        let instance = self.instance;
        self.store.set_project_repos(instance, project, repos);
        if let Some(r) = &mut self.repo_editor {
            r.saving = true;
            r.error = None;
        }
        cx.notify();
    }

    /// 頻道「自動化」分頁最上面：監看的 repo 與這個 project 的暫停。
    pub(crate) fn render_project_watch(&self, p: &ProjectView, cx: &mut Context<Self>) -> Div {
        let project = p.id;
        let inst = self.current_instance();
        let paused = inst.project_paused(project).is_some();
        let all_paused = inst.pauses.iter().any(|p| p.project.is_none());
        let editing = self.repo_editor.as_ref().filter(|r| r.project == project);
        let mut b = white_box().gap(px(8.)).child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(section_label("監看的 repo").flex_1())
                .child(if all_paused {
                    // 所有 project 都暫停了：在自動化頁的橫幅恢復。
                    div().into_any_element()
                } else if paused {
                    small_button(secondary_button("project-resume", "恢復自動化"))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.resume_project(project, cx)),
                        )
                        .into_any_element()
                } else {
                    small_button(secondary_button("project-pause", "暫停這個 project"))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.pause_project(project, cx)),
                        )
                        .into_any_element()
                }),
        );
        if all_paused {
            b = b.child(
                div()
                    .text_size(px(13.))
                    .text_color(theme::warn_text())
                    .child("所有 project 的自動化已暫停；到左欄「排程」的橫幅恢復。"),
            );
        } else if paused {
            b = b.child(
                div()
                    .text_size(px(13.))
                    .text_color(theme::warn_text())
                    .child("這個 project 的排程與 listener 已暫停，期間錯過的觸發不會補跑。"),
            );
        }
        match editing {
            Some(r) => {
                b = b
                    .child(Input::new(&r.input))
                    .when_some(r.error.as_deref(), |d, m| d.child(error_note(m)))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                small_button(primary_button(
                                    "repos-save",
                                    if r.saving { "儲存中…" } else { "儲存" },
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.save_repos(cx))),
                            )
                            .child(
                                link_button("repos-cancel", "取消")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_repos(cx))),
                            ),
                    );
            }
            None => {
                b = b.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .flex_wrap()
                        .children(p.repos.iter().map(|r| {
                            tag(r.clone(), theme::text(), theme::subtle_bg())
                        }))
                        .when(p.repos.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(theme::text_muted())
                                    .child("還沒有。加上 repo 後，agent 可以等它的 CI 與 PR，listener 也收得到它的事件。"),
                            )
                        })
                        .child(
                            link_button("repos-edit", "編輯")
                                .text_size(px(13.))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit_repos(project, window, cx)
                                })),
                        ),
                );
            }
        }
        b
    }

    // ---- 暫停與來源 ------------------------------------------------------------

    pub fn pause_project(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        if self.live.is_none() {
            self.notify_unimplemented("暫停這個 project", cx);
            return;
        }
        self.store.pause_automations(self.instance, Some(project));
        cx.notify();
    }

    pub fn resume_project(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        if self.live.is_none() {
            self.notify_unimplemented("恢復這個 project", cx);
            return;
        }
        self.store.resume_automations(self.instance, Some(project));
        cx.notify();
    }

    pub fn pause_all_automations(&mut self, cx: &mut Context<Self>) {
        if self.live.is_none() {
            self.notify_unimplemented("暫停全部", cx);
            return;
        }
        self.store.pause_automations(self.instance, None);
        cx.notify();
    }

    /// 自動化頁的「外部來源」區塊。
    pub(crate) fn render_sources(&self, cx: &mut Context<Self>) -> Option<Div> {
        let inst = self.current_instance();
        if inst.sources.is_empty() {
            return None;
        }
        let rows = inst.sources.iter().map(|s| {
            let (text, color) = match &s.health {
                SourceHealth::Unknown => ("還沒收過事件".to_string(), theme::text_muted()),
                SourceHealth::Ok => (
                    s.last_ok.map_or("正常".to_string(), |t| {
                        format!("正常 · {}", month_day_time(t))
                    }),
                    theme::ok(),
                ),
                SourceHealth::Error { message, since } => (
                    format!("異常（{} 起）：{message}", month_day_time(*since)),
                    theme::danger(),
                ),
            };
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(16.))
                .py(px(12.))
                .border_t_1()
                .border_color(theme::border())
                .text_size(px(14.))
                .child(
                    div()
                        .flex_none()
                        .w(px(120.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(s.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(140.))
                        .text_color(theme::text_muted())
                        .child(s.ingest.clone()),
                )
                .child(div().flex_1().min_w_0().text_color(color).child(text))
        });
        let no_token = inst.github.token_hint.is_none();
        Some(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .pb(px(10.))
                        .child(div().text_size(px(16.)).font_weight(FontWeight::BOLD).child("外部來源"))
                        .child(
                            link_button("sources-settings", "到設定修改")
                                .text_size(px(13.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.open_settings(crate::app::SettingsTab::Github, cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(theme::border())
                        .bg(theme::white())
                        .overflow_hidden()
                        .when(no_token, |d| {
                            d.child(
                                div()
                                    .px(px(16.))
                                    .py(px(12.))
                                    .text_size(px(13.))
                                    .text_color(theme::text_muted())
                                    .child("還沒設定 GitHub token；到「設定 → GitHub」貼上之後才會開始輪詢。"),
                            )
                        })
                        .children(rows),
                ),
        )
    }
}

fn small_button(button: gpui_kit::Stateful<Div>) -> gpui_kit::Stateful<Div> {
    button.text_size(px(13.)).px(px(12.)).py(px(5.))
}
