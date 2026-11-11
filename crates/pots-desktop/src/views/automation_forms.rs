//! M3 的表單：設定視窗的「GitHub」類別、右欄的「新增 listener」、頻道「自動化」分頁的監看
//! repo，以及暫停與來源狀態。[`Forms`] 也管 M4 設定視窗「Worker」類別的表單
//! （畫面在 `worker_settings.rs`）。
//!
//! 送出的指令都經 `Store::outbox`；服務接受後以 `Github`、`Worker`、`Automation`、
//! `ProjectRepos`、`ProjectAgentName` 事件回來時收起表單，被拒絕時（`Error`）把原因留在表單裡。

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Div, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, px, relative,
};
use pots_proto::{SourceDraft, SourceKind, TrustLevel, WebhookMapping};

use super::panel::{panel_frame, white_box};
use super::settings::{card, detail_row, field};
use super::widgets::*;
use crate::app::{
    GithubEditor, GithubTest, ListenerActionKind, ListenerForm, NotificationEditor, Panel, PotsApp,
    ProjectField, RepoEditor, SourceEditor, UserMdEditor, WorkerEditor,
};
use crate::model::*;
use crate::sync::{Applied, month_day_time};
use crate::theme;

pub(super) fn input(
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

pub(super) fn value(input: &Entity<InputState>, cx: &App) -> String {
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
fn kinds_have_field(kinds: &[String], field: &str) -> bool {
    kinds.iter().any(|k| match field {
        "label" => k == "issue.labeled",
        "branch" | "conclusion" => k == "ci.finished",
        "author" => k != "ci.finished",
        _ => false,
    })
}

pub(super) fn error_note(text: &str) -> Div {
    div()
        .px(px(12.))
        .py(px(8.))
        .rounded(px(8.))
        .bg(theme::danger_soft())
        .text_color(theme::danger())
        .text_size(px(13.))
        .child(text.to_string())
}

pub(super) fn chip(id: SharedString, label: &str, selected: bool) -> gpui_kit::Stateful<Div> {
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
    pub worker: &'a mut Option<WorkerEditor>,
    pub listener: &'a mut Option<ListenerForm>,
    pub source: &'a mut Option<SourceEditor>,
    pub repo: &'a mut Option<RepoEditor>,
    pub notifications: &'a mut Option<NotificationEditor>,
    pub user_md: &'a mut Option<UserMdEditor>,
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
        if let Some(w) = self.worker
            && w.saving
        {
            w.saving = false;
            w.error = Some("連線中斷，請再儲存一次".into());
        }
        if let Some(n) = self.notifications
            && n.saving
        {
            n.saving = false;
            n.error = Some("連線中斷，請再儲存一次".into());
        }
        if let Some(u) = self.user_md
            && u.saving
        {
            u.saving = false;
            u.error = Some("連線中斷，請再儲存一次".into());
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
        if let Some(s) = self.source
            && s.saving
        {
            s.saving = false;
            s.error = Some("連線中斷，請再儲存一次".into());
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
        if let Some(w) = self.worker
            && w.saving
        {
            w.saving = false;
            w.error = Some(toast);
            return None;
        }
        if let Some(n) = self.notifications
            && n.saving
        {
            n.saving = false;
            n.error = Some(toast);
            return None;
        }
        if let Some(u) = self.user_md
            && u.saving
        {
            u.saving = false;
            u.error = Some(toast);
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
        if let Some(s) = self.source
            && s.saving
        {
            s.saving = false;
            s.error = Some(toast);
            return None;
        }
        Some(toast)
    }

    /// 服務回了 GitHub 設定、worker 設定、自動化或 repo：收起儲存中的表單、填上測試結果。
    pub(crate) fn applied(&mut self, applied: &Applied) {
        if applied.worker_changed && self.worker.as_ref().is_some_and(|e| e.saving) {
            *self.worker = None;
        }
        if applied.github_changed && self.github.as_ref().is_some_and(|e| e.saving) {
            *self.github = None;
        }
        if applied.notifications_changed && self.notifications.as_ref().is_some_and(|e| e.saving) {
            *self.notifications = None;
        }
        if applied.user_md_changed && self.user_md.as_ref().is_some_and(|e| e.saving) {
            *self.user_md = None;
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
            && match r.field {
                ProjectField::Repos => applied.repos_changed == Some(r.project),
                ProjectField::AgentName => applied.agent_name_changed == Some(r.project),
            }
        {
            *self.repo = None;
        }
        // 來源清單有變（儲存成功）就收起表單。
        if applied.automation_changed && self.source.as_ref().is_some_and(|e| e.saving) {
            *self.source = None;
            if *self.panel == Some(Panel::NewSource) {
                *self.panel = None;
            }
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
            kinds: vec!["issue.labeled".to_string()],
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
        self.set_panel(Some(Panel::NewListener));
        cx.notify();
    }

    pub fn cancel_listener_form(&mut self, cx: &mut Context<Self>) {
        self.listener_form = None;
        if self.panel == Some(Panel::NewListener) {
            self.panel = None;
        }
        cx.notify();
    }

    pub fn toggle_listener_kind(&mut self, kind: String, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.listener_form {
            if let Some(i) = f.kinds.iter().position(|k| k == &kind) {
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
        let kinds = div().flex().flex_wrap().gap(px(6.)).children(
            EVENT_KINDS
                .iter()
                .map(|(k, label)| ((*k).to_string(), (*label).to_string()))
                .chain(
                    inst.custom_kinds
                        .iter()
                        .filter(|k| !EVENT_KINDS.iter().any(|(n, _)| n == k))
                        .map(|k| (k.clone(), k.clone())),
                )
                .map(|(k, label)| {
                    let id = format!("listener-kind-{k}");
                    let value = k.clone();
                    chip(SharedString::from(id), &label, f.kinds.contains(&k)).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_listener_kind(value.clone(), cx)
                        }),
                    )
                }),
        );
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

    // ---- 外部來源 -------------------------------------------------------------

    pub fn new_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_source_form(None, window, cx);
    }

    pub fn edit_source(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let source = self
            .current_instance()
            .sources
            .iter()
            .find(|s| s.id == id)
            .cloned();
        self.open_source_form(source.as_ref(), window, cx);
    }

    fn open_source_form(
        &mut self,
        editing: Option<&SourceView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = editing
            .and_then(|s| SourceKind::parse(&s.kind_id))
            .unwrap_or(SourceKind::Rss);
        let mapping = editing.map(|s| s.mapping.clone()).unwrap_or_default();
        let kept = editing.is_some();
        self.source_editor = Some(SourceEditor {
            editing: editing.map(|s| s.id.clone()),
            kind,
            trust: editing.map(|s| s.trust).unwrap_or(TrustLevel::Public),
            id: input(
                editing.map(|s| s.id.clone()).unwrap_or_default(),
                "例如 rss:notes",
                false,
                window,
                cx,
            ),
            name: input(
                editing.map(|s| s.name.clone()).unwrap_or_default(),
                "畫面上顯示的名稱",
                false,
                window,
                cx,
            ),
            url: input(
                editing.and_then(|s| s.url.clone()).unwrap_or_default(),
                "https://example.com/feed.xml",
                false,
                window,
                cx,
            ),
            interval: input(
                String::new(),
                &format!("留空是 {} 秒", kind.default_interval_secs()),
                false,
                window,
                cx,
            ),
            secret: input(
                String::new(),
                if kept {
                    "留空沿用已存的 secret"
                } else {
                    "至少 16 個字元；對方用它算簽章"
                },
                true,
                window,
                cx,
            ),
            map_kind: input(mapping.kind.clone(), "/type", false, window, cx),
            map_resource: input(mapping.resource.clone(), "/id", false, window, cx),
            map_summary: input(mapping.summary.clone(), "/title", false, window, cx),
            map_dedupe: input(
                mapping.dedupe.clone(),
                "/id（留空用整個 body）",
                false,
                window,
                cx,
            ),
            map_body: input(
                mapping.body.clone(),
                "/body（沒有就留空）",
                false,
                window,
                cx,
            ),
            enabled: editing.map(|s| s.enabled).unwrap_or(true),
            saving: false,
            error: None,
        });
        self.set_panel(Some(Panel::NewSource));
        cx.notify();
    }

    pub fn cancel_source_form(&mut self, cx: &mut Context<Self>) {
        self.source_editor = None;
        if self.panel == Some(Panel::NewSource) {
            self.panel = None;
        }
        cx.notify();
    }

    pub fn set_source_kind(&mut self, kind: SourceKind, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.source_editor {
            f.kind = kind;
            if kind == SourceKind::Webhook {
                f.trust = TrustLevel::Members;
            }
        }
        cx.notify();
    }

    pub fn set_source_trust(&mut self, trust: TrustLevel, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.source_editor {
            f.trust = trust;
        }
        cx.notify();
    }

    pub fn toggle_source_enabled(&mut self, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.source_editor {
            f.enabled = !f.enabled;
        }
        cx.notify();
    }

    fn source_draft(&self, cx: &App) -> Result<SourceDraft, String> {
        let f = self.source_editor.as_ref().ok_or("沒有表單")?;
        let id = value(&f.id, cx).to_ascii_lowercase();
        if id.is_empty() {
            return Err("要填來源 id（例如 rss:notes）".into());
        }
        if f.editing.as_ref().is_some_and(|old| old != &id) {
            return Err("來源 id 不能改；要換 id 請刪除後重新建立".into());
        }
        let name = value(&f.name, cx);
        if name.is_empty() {
            return Err("要填名稱".into());
        }
        let interval = match value(&f.interval, cx) {
            v if v.is_empty() => None,
            v => Some(
                v.parse::<u32>()
                    .map_err(|_| "輪詢間隔要是秒數".to_string())?,
            ),
        };
        let url = value(&f.url, cx);
        if f.kind != SourceKind::Webhook && url.is_empty() {
            return Err("要填 URL".into());
        }
        let secret = if f.kind == SourceKind::Webhook {
            let typed = value(&f.secret, cx);
            if typed.is_empty() && f.editing.is_none() {
                return Err("通用 webhook 需要一個 secret".into());
            }
            (!typed.is_empty()).then_some(typed)
        } else {
            None
        };
        Ok(SourceDraft {
            id,
            kind: f.kind,
            name,
            url: (f.kind != SourceKind::Webhook).then_some(url),
            interval_secs: interval,
            trust: f.trust,
            secret,
            mapping: WebhookMapping {
                kind: value(&f.map_kind, cx),
                resource: value(&f.map_resource, cx),
                summary: value(&f.map_summary, cx),
                dedupe: value(&f.map_dedupe, cx),
                body: value(&f.map_body, cx),
            },
            enabled: f.enabled,
        })
    }

    pub fn save_source_form(&mut self, cx: &mut Context<Self>) {
        let draft = self.source_draft(cx);
        let Some(f) = &mut self.source_editor else {
            return;
        };
        let draft = match draft {
            Ok(d) => d,
            Err(e) => {
                f.error = Some(e);
                cx.notify();
                return;
            }
        };
        f.error = None;
        let sent = self.settings_command(ClientCommand::SaveSource { draft }, "儲存來源", cx);
        if let Some(f) = &mut self.source_editor {
            f.saving = sent;
        }
        cx.notify();
    }

    pub fn delete_source(&mut self, id: String, cx: &mut Context<Self>) {
        self.settings_command(ClientCommand::DeleteSource { id }, "刪除來源", cx);
    }

    pub fn toggle_source(&mut self, id: String, enabled: bool, cx: &mut Context<Self>) {
        self.settings_command(
            ClientCommand::SetSourceEnabled { id, enabled },
            "切換來源",
            cx,
        );
    }

    pub(crate) fn render_source_form(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let f = self.source_editor.as_ref()?;
        let kinds = div()
            .flex()
            .flex_wrap()
            .gap(px(6.))
            .children(SourceKind::ALL.iter().map(|k| {
                let k = *k;
                chip(
                    SharedString::from(format!("source-kind-{}", k.as_str())),
                    k.label(),
                    f.kind == k,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_source_kind(k, cx)))
            }));
        let trusts = div().flex().flex_wrap().gap(px(6.)).children(
            [
                (TrustLevel::Owned, "Owned（自己的系統）"),
                (TrustLevel::Members, "Members（可信的服務）"),
                (TrustLevel::Public, "Public（任何人都能寫）"),
            ]
            .into_iter()
            .map(|(t, label)| {
                chip(
                    SharedString::from(format!("source-trust-{t:?}")),
                    label,
                    f.trust == t,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_source_trust(t, cx)))
            }),
        );
        let enabled = chip(
            SharedString::from("source-enabled"),
            if f.enabled { "啟用" } else { "停用" },
            f.enabled,
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_source_enabled(cx)));
        let url_fields = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(field(
                "URL",
                Some(match f.kind {
                    SourceKind::Rss => "RSS 或 Atom 的網址",
                    SourceKind::Probe => "定時 GET 這個網址，2xx／3xx 算正常",
                    _ => "statuspage.io 的 summary.json，或 RSS／Atom 形式的狀態頁",
                }),
                Input::new(&f.url).into_any_element(),
            ))
            .child(field(
                "輪詢間隔",
                Some("秒，30 到 3600；留空用預設值"),
                Input::new(&f.interval).into_any_element(),
            ));
        let webhook_fields = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(field(
                "Secret",
                Some("對方用 HMAC-SHA256 算 `X-Pots-Signature`"),
                Input::new(&f.secret).into_any_element(),
            ))
            .child(div().flex().gap(px(8.)).child(div().flex_1().child(field(
                "種類",
                Some("JSON pointer"),
                Input::new(&f.map_kind).into_any_element(),
            ))))
            .child(field(
                "資源鍵",
                Some("同一個資源的事件會回到同一個 Goal"),
                Input::new(&f.map_resource).into_any_element(),
            ))
            .child(field(
                "摘要",
                Some("平台產生、agent 可信的部分"),
                Input::new(&f.map_summary).into_any_element(),
            ))
            .child(field(
                "去重",
                Some("同一個值只算一次；留空用整個 body 的雜湊"),
                Input::new(&f.map_dedupe).into_any_element(),
            ))
            .child(field(
                "內文",
                Some("他人寫的內容，送進 agent 時會標成不可信"),
                Input::new(&f.map_body).into_any_element(),
            ))
            .when_some(f.editing.as_ref(), |d, id| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(format!("POST /webhooks/source/{id}")),
                )
            });
        let body = div()
            .id("source-form-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(20.))
            .pb(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                white_box()
                    .gap(px(8.))
                    .child(section_label("種類"))
                    .child(kinds),
            )
            .child(
                white_box()
                    .gap(px(8.))
                    .child(section_label("名稱"))
                    .child(field(
                        "ID",
                        Some("小寫英文、數字與 `-`、`_`、`.`、`:`；建立後不能改"),
                        Input::new(&f.id).into_any_element(),
                    ))
                    .child(field(
                        "顯示名稱",
                        None,
                        Input::new(&f.name).into_any_element(),
                    )),
            )
            .child(
                white_box()
                    .gap(px(8.))
                    .child(section_label(if f.kind == SourceKind::Webhook {
                        "Webhook"
                    } else {
                        "輪詢"
                    }))
                    .when(f.kind == SourceKind::Webhook, |d| d.child(webhook_fields))
                    .when(f.kind != SourceKind::Webhook, |d| d.child(url_fields)),
            )
            .child(
                white_box()
                    .gap(px(8.))
                    .child(section_label("信任等級"))
                    .child(trusts)
                    .child(enabled),
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
                                .child(if f.editing.is_some() {
                                    "修改外部來源"
                                } else {
                                    "新增外部來源"
                                }),
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
                                "source-save",
                                if f.saving { "儲存中…" } else { "儲存" },
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.save_source_form(cx))),
                        )
                        .child(
                            link_button("source-cancel", "取消").on_click(
                                cx.listener(|this, _, _, cx| this.cancel_source_form(cx)),
                            ),
                        )
                        .into_any_element(),
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
        self.repo_editor = Some(RepoEditor::new(project, ProjectField::Repos, state, sub));
        cx.notify();
    }

    pub fn edit_agent_name(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .store
            .project(self.instance, project)
            .map(|p| p.agent_name.clone())
            .unwrap_or_default();
        let state = input(current, "負責人的名字；Enter 儲存", false, window, cx);
        state.update(cx, |s, cx| s.focus(window, cx));
        let sub = cx.subscribe(&state, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.save_repos(cx);
            }
        });
        self.repo_editor = Some(RepoEditor::new(
            project,
            ProjectField::AgentName,
            state,
            sub,
        ));
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
        let field = r.field;
        let text = value(&r.input, cx);
        if self.live.is_none() {
            self.notify_unimplemented(
                match field {
                    ProjectField::Repos => "設定 repo",
                    ProjectField::AgentName => "幫負責人改名",
                },
                cx,
            );
            return;
        }
        let instance = self.instance;
        match field {
            ProjectField::Repos => self.store.set_project_repos(instance, project, list(&text)),
            ProjectField::AgentName => self.store.rename_agent(instance, project, text),
        }
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
        let editing = self
            .repo_editor
            .as_ref()
            .filter(|r| r.project == project && r.field == ProjectField::Repos);
        let renaming = self
            .repo_editor
            .as_ref()
            .filter(|r| r.project == project && r.field == ProjectField::AgentName);
        let local = matches!(p.workdir, Some(WorkDir::Local { .. }));
        let mut b = white_box()
            .gap(px(8.))
            // 這個 project 的負責人（對話 agent）。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .flex_wrap()
                    .child(section_label("負責人").flex_1())
                    .when(renaming.is_none(), |d| {
                        d.child(tag(p.agent_name.clone(), theme::text(), theme::subtle_bg()))
                            .child(
                                link_button("agent-rename", "改名")
                                    .text_size(px(13.))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.edit_agent_name(project, window, cx)
                                    })),
                            )
                    }),
            )
            .when_some(renaming, |b, r| {
                b.child(Input::new(&r.input))
                    .when_some(r.error.as_deref(), |d, m| d.child(error_note(m)))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                small_button(primary_button(
                                    "agent-rename-save",
                                    if r.saving { "儲存中…" } else { "儲存" },
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.save_repos(cx))),
                            )
                            .child(
                                link_button("agent-rename-cancel", "取消")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_repos(cx))),
                            ),
                    )
            })
            .child(div().h(px(1.)).bg(theme::border()))
            // 工作資料夾是「worker 在哪裡改」，和下面的「監看的 repo」（事件來源）分開。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .flex_wrap()
                    .child(section_label("工作資料夾").flex_1())
                    .child(match &p.workdir {
                        Some(dir) => tag(
                            dir.label(),
                            theme::text(),
                            theme::subtle_bg(),
                        )
                        .into_any_element(),
                        None => div()
                            .text_size(px(13.))
                            .text_color(theme::warn_text())
                            .child("還沒有設定：worker 不會派工")
                            .into_any_element(),
                    })
                    .child(
                        link_button("workdir-edit", "設定")
                            .text_size(px(13.))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_workdir(project, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(if local {
                        "本機資料夾：worker 在任務分支上改，核准後平台把分支的 commit 線性接到你的分支上；沒有 PR 與 CI。"
                    } else {
                        "worker 在任務分支上改、開 PR、等 CI，核准後 merge。"
                    }),
            )
            .child(div().h(px(1.)).bg(theme::border()))
            .child(
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
                                    .w_full()
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
        // 你核准 merge 後平台用的方式（agent 申請時可以另外指定，核准卡片上看得到）。
        // 本機資料夾一律線性套用，沒有 merge 方式可選。
        if local {
            return b;
        }
        b.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .flex_wrap()
                .pt(px(4.))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .child("核准後 merge 的方式"),
                )
                .children(MergeMethod::ALL.map(|m| {
                    chip(
                        SharedString::from(format!("merge-method-{}", m.as_str())),
                        m.label(),
                        p.merge_method == m,
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_merge_method(project, m, cx)),
                    )
                })),
        )
    }

    pub fn set_merge_method(
        &mut self,
        project: ProjectId,
        method: MergeMethod,
        cx: &mut Context<Self>,
    ) {
        let instance = self.instance;
        if let Some(p) = self.store.project_mut(instance, project) {
            p.merge_method = method;
        }
        self.store
            .outbox
            .push((instance, ClientCommand::SetMergeMethod { project, method }));
        cx.notify();
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

    /// 自動化頁的「外部來源」區塊：健康狀態、編輯與啟停。
    pub(crate) fn render_sources(&self, cx: &mut Context<Self>) -> Option<Div> {
        let inst = self.current_instance();
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
            let github = s.kind_id == "github";
            let id = s.id.clone();
            let detail = match (github, &s.url) {
                (true, _) => String::new(),
                (false, _) if s.kind_id == "webhook" => {
                    format!("POST /webhooks/source/{}", s.id)
                }
                (false, Some(url)) => url.clone(),
                (false, None) => String::new(),
            };
            let actions = div().flex().flex_none().items_center().gap(px(6.));
            let actions = if github {
                actions.child(
                    link_button("sources-settings", "到設定修改")
                        .text_size(px(13.))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_settings(crate::app::SettingsTab::Github, cx)
                        })),
                )
            } else {
                let edit = id.clone();
                let toggle = id.clone();
                let enabled = s.enabled;
                let remove = id.clone();
                actions
                    .child(
                        link_button(SharedString::from(format!("source-edit-{id}")), "編輯")
                            .text_size(px(13.))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_source(edit.clone(), window, cx)
                            })),
                    )
                    .child(
                        link_button(
                            SharedString::from(format!("source-toggle-{id}")),
                            if enabled { "停用" } else { "啟用" },
                        )
                        .text_size(px(13.))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_source(toggle.clone(), !enabled, cx)
                        })),
                    )
                    .child(
                        link_button(SharedString::from(format!("source-delete-{id}")), "刪除")
                            .text_size(px(13.))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.delete_source(remove.clone(), cx)
                            })),
                    )
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
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .text_color(color)
                        .child(text)
                        .when(!detail.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::text_muted())
                                    .child(detail),
                            )
                        }),
                )
                .child(actions)
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
                            link_button("source-new", "新增來源")
                                .text_size(px(13.))
                                .on_click(cx.listener(|this, _, window, cx| this.new_source(window, cx))),
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
