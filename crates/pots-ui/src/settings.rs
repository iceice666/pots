//! The settings dialog. Two pages: the LLM providers, kept in config.toml
//! and their keys in keys.toml, and which provider and model each agent
//! and the worker use (see `pots_agent::providers`).

use std::sync::Arc;

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyDownEvent, Subscription, Window, div,
    prelude::*, px,
};

use pots_agent::config::{Api, Config, Endpoint};
use pots_agent::agent::catalog_ids;
use pots_agent::providers::{self, AgentChoice, ProviderFields};
use pots_agent::store;
use pots_llm::cost::{Catalog, Price};

use crate::composer::{Composer, ComposerEvent};
use crate::theme;

pub enum SettingsEvent {
    /// config.toml changed: agents should take up its providers and models.
    Changed(Config),
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Providers,
    Agents,
}

/// An entry on the Agents page.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Which {
    Agent(String),
    Worker,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Selected {
    Provider(String),
    New,
}

pub struct Settings {
    focus: FocusHandle,
    page: Page,
    providers: Vec<ProviderFields>,
    /// Agents' providers and models, as config.toml has them.
    agents: Vec<AgentChoice>,
    /// `[worker]`'s, if config.toml has one.
    worker: Option<AgentChoice>,
    /// Why config.toml couldn't be read. Nothing can be edited then.
    problem: Option<String>,
    selected: Option<Selected>,
    /// The selected provider as saved, to tell whether it has been edited.
    original: ProviderFields,
    api: Api,
    name: Entity<Composer>,
    base_url: Entity<Composer>,
    api_key: Entity<Composer>,
    api_key_env: Entity<Composer>,
    /// The agent being edited, as saved.
    agent: Option<AgentChoice>,
    /// The provider picked for it.
    agent_provider: String,
    model: Entity<Composer>,
    /// models.dev, for suggesting model names and showing their prices.
    catalog: Option<Arc<Catalog>>,
    /// Why the last Save or Delete didn't go through.
    error: Option<String>,
    /// What the last Save or Delete did.
    notice: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SettingsEvent> for Settings {}

impl Focusable for Settings {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Settings {
    pub fn new(catalog: Option<Arc<Catalog>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(Composer::compact);
        let model = cx.new(Composer::compact);
        let base_url = cx.new(Composer::compact);
        let api_key = cx.new(Composer::secret);
        let api_key_env = cx.new(Composer::compact);
        let mut subscriptions = Vec::new();
        // Enter moves down the fields, then saves.
        for (field, next) in [(&name, Some(&base_url)), (&base_url, Some(&api_key)), (&api_key, Some(&api_key_env)), (&api_key_env, None)] {
            let next = next.cloned();
            subscriptions.push(cx.subscribe_in(field, window, move |this, _, event: &ComposerEvent, window, cx| {
                if let ComposerEvent::Submit(_) = event {
                    match &next {
                        Some(next) => window.focus(&next.read(cx).focus_handle(cx)),
                        None => this.save(window, cx),
                    }
                }
            }));
            // Hints and the Save button follow what's typed.
            subscriptions.push(cx.observe(field, |_, _, cx| cx.notify()));
        }
        subscriptions.push(cx.subscribe_in(&model, window, |this, _, event: &ComposerEvent, window, cx| {
            if let ComposerEvent::Submit(_) = event {
                this.save_agent(window, cx)
            }
        }));
        subscriptions.push(cx.observe(&model, |_, _, cx| cx.notify()));
        let mut this = Self {
            focus: cx.focus_handle(),
            page: Page::Providers,
            providers: Vec::new(),
            agents: Vec::new(),
            worker: None,
            problem: None,
            selected: None,
            original: ProviderFields::new(Api::Anthropic),
            api: Api::Anthropic,
            name,
            base_url,
            api_key,
            api_key_env,
            agent: None,
            agent_provider: String::new(),
            model,
            catalog,
            error: None,
            notice: None,
            _subscriptions: subscriptions,
        };
        this.reload(None, window, cx);
        this
    }

    /// Re-read config.toml and select provider `select` (or the first one).
    /// The agent form is reset to what's saved, on the same agent if it's
    /// still there.
    fn reload(&mut self, select: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        match store::load_config() {
            Ok(config) => {
                self.problem = None;
                self.providers = config.providers.iter().map(|(n, pc)| ProviderFields::from_config(n, pc)).collect();
                self.agents = config
                    .agents
                    .iter()
                    .map(|a| AgentChoice {
                        name: a.name.clone(),
                        provider: a.provider.clone(),
                        model: a.model.clone(),
                        worker: false,
                    })
                    .collect();
                self.worker = config.worker.as_ref().map(|w| AgentChoice::worker(&w.provider, &w.model));
            }
            Err(e) => {
                self.problem = Some(format!("Can't read the settings: {e}"));
                self.providers.clear();
                self.agents.clear();
                self.worker = None;
            }
        }
        let select = select
            .filter(|n| self.providers.iter().any(|p| &p.name == n))
            .or_else(|| self.providers.first().map(|p| p.name.clone()));
        self.select_agent(self.which(), cx);
        match select {
            Some(name) => self.select(Selected::Provider(name), window, cx),
            None => {
                self.selected = None;
                window.focus(&self.focus);
                cx.notify();
            }
        }
    }

    fn select(&mut self, selected: Selected, window: &mut Window, cx: &mut Context<Self>) {
        let fields = match &selected {
            Selected::Provider(name) => self.providers.iter().find(|p| &p.name == name).cloned(),
            Selected::New => None,
        };
        let new = fields.is_none();
        let fields = fields.unwrap_or_else(|| ProviderFields::new(Api::Anthropic));
        for (field, text, placeholder) in [
            (&self.name, &fields.name, "e.g. anthropic"),
            (&self.base_url, &fields.base_url, fields.api.default_base_url()),
            (&self.api_key, &fields.api_key, "Paste a key to keep it in keys.toml"),
            (&self.api_key_env, &fields.api_key_env, "e.g. ANTHROPIC_API_KEY"),
        ] {
            field.update(cx, |f, cx| {
                f.set_text(text.clone(), cx);
                f.set_placeholder(placeholder, cx);
            });
        }
        self.api = fields.api;
        self.original = fields;
        self.selected = Some(selected);
        self.error = None;
        if new {
            window.focus(&self.name.read(cx).focus_handle(cx));
        } else {
            window.focus(&self.focus);
        }
        cx.notify();
    }

    fn fields(&self, cx: &gpui::App) -> ProviderFields {
        let text = |f: &Entity<Composer>| f.read(cx).text().trim().to_string();
        ProviderFields {
            name: text(&self.name),
            api: self.api,
            endpoint: self.original.endpoint,
            base_url: text(&self.base_url),
            api_key: text(&self.api_key),
            api_key_env: text(&self.api_key_env),
            key_in_config: false,
        }
    }

    fn edited(&self, cx: &gpui::App) -> bool {
        self.selected == Some(Selected::New) || self.fields(cx) != self.original
    }

    fn set_api(&mut self, api: Api, cx: &mut Context<Self>) {
        if api == self.api {
            return;
        }
        // Follow the switch if the URL was still the other API's default.
        let url = self.base_url.read(cx).text().trim().to_string();
        if url.is_empty() || url == self.api.default_base_url() {
            self.base_url.update(cx, |f, cx| f.set_text(api.default_base_url(), cx));
        }
        self.base_url.update(cx, |f, cx| f.set_placeholder(api.default_base_url(), cx));
        self.api = api;
        cx.notify();
    }

    /// Write the edited provider to config.toml.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.selected.clone() else { return };
        if !self.edited(cx) {
            return;
        }
        let fields = self.fields(cx);
        let old = match &selected {
            Selected::Provider(name) => Some(name.as_str()),
            Selected::New => None,
        };
        let verb = if old.is_some() { "Saved" } else { "Added" };
        match providers::save_to_disk(old, &fields) {
            Ok(config) => {
                cx.emit(SettingsEvent::Changed(config));
                self.notice = Some(format!("{verb} {}.", fields.name));
                self.reload(Some(fields.name), window, cx);
            }
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Selected::Provider(name)) = self.selected.clone() else { return };
        match providers::delete_from_disk(&name) {
            Ok(config) => {
                cx.emit(SettingsEvent::Changed(config));
                self.notice = Some(format!("Deleted {name}."));
                self.reload(None, window, cx);
            }
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(SettingsEvent::Close);
    }

    fn users(&self, provider: &str) -> Vec<&str> {
        self.agents.iter().chain(&self.worker).filter(|a| a.provider == provider).map(|a| a.name.as_str()).collect()
    }

    fn set_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        if page == self.page {
            return;
        }
        self.page = page;
        self.error = None;
        self.notice = None;
        match page {
            Page::Agents if self.agent.is_some() => window.focus(&self.model.read(cx).focus_handle(cx)),
            _ => window.focus(&self.focus),
        }
        cx.notify();
    }

    // --- agents ----------------------------------------------------------

    /// What the Agents page is editing.
    fn which(&self) -> Option<Which> {
        self.agent.as_ref().map(|a| match a.worker {
            true => Which::Worker,
            false => Which::Agent(a.name.clone()),
        })
    }

    /// Edit `which` (or the first agent), as saved. A worker that isn't set
    /// up yet starts out empty.
    fn select_agent(&mut self, which: Option<Which>, cx: &mut Context<Self>) {
        let agent = match which {
            Some(Which::Worker) => Some(self.worker.clone().unwrap_or_else(|| AgentChoice::worker("", ""))),
            Some(Which::Agent(n)) => self.agents.iter().find(|a| a.name == n).cloned(),
            None => None,
        }
        .or_else(|| self.agents.first().cloned());
        let (provider, model) = agent.as_ref().map_or_else(Default::default, |a| (a.provider.clone(), a.model.clone()));
        let placeholder = match agent.as_ref().is_some_and(|a| a.worker) {
            true => "e.g. deepseek/deepseek-v4.1-flash-1017",
            false => "e.g. anthropic/claude-opus-5.5",
        };
        self.model.update(cx, |f, cx| {
            f.set_text(model, cx);
            f.set_placeholder(placeholder, cx);
        });
        self.agent_provider = provider;
        self.agent = agent;
        self.error = None;
        cx.notify();
    }

    /// The agent form as edited.
    fn agent_choice(&self, cx: &gpui::App) -> Option<AgentChoice> {
        let agent = self.agent.as_ref()?;
        Some(AgentChoice {
            name: agent.name.clone(),
            provider: self.agent_provider.clone(),
            model: self.model.read(cx).text().trim().to_string(),
            worker: agent.worker,
        })
    }

    fn agent_edited(&self, cx: &gpui::App) -> bool {
        self.agent_choice(cx).is_some_and(|c| Some(&c) != self.agent.as_ref())
    }

    fn save_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(choice) = self.agent_choice(cx).filter(|_| self.agent_edited(cx)) else { return };
        match providers::save_agent_to_disk(&choice) {
            Ok(config) => {
                cx.emit(SettingsEvent::Changed(config));
                self.notice = Some(match (choice.worker, self.worker.is_some()) {
                    (true, false) => format!("Saved. Agents can ask {} ({}) from their next reply.", choice.name, choice.model),
                    (true, true) => format!("Saved. {} uses {} from the next ask.", choice.name, choice.model),
                    (false, _) => format!("Saved. {} uses {} from its next reply.", choice.name, choice.model),
                });
                self.reload(None, window, cx);
                window.focus(&self.model.read(cx).focus_handle(cx));
            }
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    /// Take `[worker]` out of config.toml.
    fn remove_worker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match providers::remove_worker_from_disk() {
            Ok(config) => {
                cx.emit(SettingsEvent::Changed(config));
                self.notice = Some("Removed the worker. Agents can't ask it from their next reply.".into());
                self.reload(None, window, cx);
            }
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    /// Where the picked provider's models are listed on models.dev, best first.
    fn agent_catalog_ids(&self) -> Vec<String> {
        match self.providers.iter().find(|p| p.name == self.agent_provider) {
            Some(p) => catalog_ids(&p.name, p.api, &p.base_url),
            None => Vec::new(),
        }
    }

    /// The models models.dev lists for the picked provider, if it knows it.
    fn listed_models(&self) -> Option<Vec<(&str, Price)>> {
        let catalog = self.catalog.as_ref()?;
        let ids = self.agent_catalog_ids();
        let id = ids.iter().find(|id| catalog.has_provider(id))?;
        Some(catalog.models(id))
    }

    /// Models on models.dev for the picked provider that match what's typed,
    /// with `.` and `-` matching each other ("claude-opus-5-5" finds
    /// "anthropic/claude-opus-5.5").
    fn suggestions(&self, cx: &gpui::App) -> Vec<(String, Price)> {
        let typed = loose(self.model.read(cx).text().trim());
        let Some(models) = self.listed_models().filter(|_| !typed.is_empty()) else { return Vec::new() };
        let exact = self.model.read(cx).text().trim().to_string();
        let mut found: Vec<(bool, &str, Price)> = models
            .into_iter()
            .filter(|(id, _)| *id != exact && loose(id).contains(&typed))
            .map(|(id, price)| {
                let l = loose(id);
                let short = l.rsplit('/').next().unwrap_or(&l);
                (!(l.starts_with(&typed) || short.starts_with(&typed)), id, price)
            })
            .collect();
        found.sort_by(|a, b| (a.0, a.1.len(), a.1).cmp(&(b.0, b.1.len(), b.1)));
        found.into_iter().take(8).map(|(_, id, price)| (id.to_string(), price)).collect()
    }

    // --- rendering -------------------------------------------------------

    fn render_nav(&self, cx: &Context<Self>) -> impl IntoElement {
        let item = |page: Page, text: &'static str| {
            div()
                .id(text)
                .px_2()
                .h(px(30.))
                .flex()
                .items_center()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(14.))
                .map(|d| match self.page == page {
                    true => d.bg(theme::sidebar_selected()).text_color(gpui::white()).font_weight(FontWeight::SEMIBOLD),
                    false => d.hover(|s| s.bg(theme::reaction_bg())),
                })
                .child(text)
                .on_click(cx.listener(move |this, _, window, cx| this.set_page(page, window, cx)))
        };
        div()
            .w(px(180.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .bg(theme::hover())
            .border_r_1()
            .border_color(theme::border())
            .child(div().px_2().pt_2().pb_3().text_size(px(18.)).font_weight(FontWeight::BOLD).child("Settings"))
            .child(item(Page::Providers, "Providers"))
            .child(item(Page::Agents, "Agents"))
            .child(div().flex_1())
            .child(
                div()
                    .id("open-config")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(theme::link())
                    .hover(|s| s.bg(theme::reaction_bg()))
                    .child("Open config.toml")
                    .on_click(|_, _, cx| cx.open_with_system(&store::config_path())),
            )
    }

    fn render_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let row = |id: usize, selected: bool| {
            div()
                .id(("provider", id))
                .px_3()
                .py(px(6.))
                .rounded_md()
                .cursor_pointer()
                .map(|d| match selected {
                    true => d.bg(theme::mention_bg()),
                    false => d.hover(|s| s.bg(theme::hover())),
                })
        };
        div()
            .id("provider-list")
            .w(px(210.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme::border())
            .children(self.providers.iter().enumerate().map(|(i, p)| {
                let selected = self.selected == Some(Selected::Provider(p.name.clone()));
                let (status, ok) = key_status(&p.api_key, &p.api_key_env, p.key_in_config);
                let name = p.name.clone();
                row(i, selected)
                    .child(div().text_size(px(14.)).font_weight(FontWeight::BOLD).child(p.name.clone()))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .text_size(px(12.))
                            .text_color(theme::muted())
                            .child(api_label(p.api))
                            .child("·")
                            .child(
                                div()
                                    .min_w_0()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .when(!ok, |d| d.text_color(theme::error()))
                                    .child(status.short),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.notice = None;
                        this.select(Selected::Provider(name.clone()), window, cx)
                    }))
            }))
            .when(self.selected == Some(Selected::New), |d| {
                d.child(row(usize::MAX, true).child(div().text_size(px(14.)).font_weight(FontWeight::BOLD).child("New provider")))
            })
            .when(self.problem.is_none(), |d| {
                d.child(
                    div()
                        .id("add-provider")
                        .px_3()
                        .py(px(6.))
                        .rounded_md()
                        .cursor_pointer()
                        .text_size(px(14.))
                        .text_color(theme::link())
                        .hover(|s| s.bg(theme::hover()))
                        .child("+ Add provider")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.notice = None;
                            this.select(Selected::New, window, cx)
                        })),
                )
            })
    }

    fn render_form(&self, cx: &Context<Self>) -> impl IntoElement {
        let label = |text: &'static str| {
            div().text_size(px(12.)).font_weight(FontWeight::BOLD).text_color(theme::muted()).child(text)
        };
        let hint = |text: String| div().text_size(px(12.)).text_color(theme::muted()).child(text);
        let field = |name: &'static str| div().flex().flex_col().gap_1().child(label(name));

        let form = div().id("provider-form").flex_1().min_w_0().flex().flex_col().gap_4().p_5().overflow_y_scroll();
        if let Some(problem) = &self.problem {
            return form.child(div().text_size(px(13.)).text_color(theme::error()).child(problem.clone()));
        }
        let Some(selected) = &self.selected else {
            return form.child(hint("No providers yet. Add one to tell agents where to send their requests.".into()));
        };

        let fields = self.fields(cx);
        let base = if fields.base_url.is_empty() { fields.api.default_base_url() } else { &fields.base_url };
        let base = base.trim_end_matches('/');
        let requests_go = match (fields.api, fields.endpoint) {
            (Api::ClaudeCode, _) => String::new(),
            (Api::Anthropic, _) => format!("Requests go to {base}/v1/messages"),
            (Api::Openai, Endpoint::Responses) => format!("Requests go to {base}/responses (endpoint = \"responses\" in config.toml)"),
            (Api::Openai, Endpoint::Chat) => format!("Requests go to {base}/chat/completions (endpoint = \"chat\" in config.toml)"),
            (Api::Openai, Endpoint::Auto) => {
                format!("Requests go to {base}/responses, or {base}/chat/completions if the server doesn't have it")
            }
        };
        let (mut status, ok) = key_status(&fields.api_key, &fields.api_key_env, self.original.key_in_config);
        let claude_code = self.api == Api::ClaudeCode;
        if claude_code && fields.api_key.is_empty() && fields.api_key_env.is_empty() {
            status.long = "No token: agents without a workspace use this computer's Claude Code login; agents with \
                           one can't run."
                .into();
        }
        let used_by = match selected {
            Selected::Provider(name) => {
                let users = self.users(name);
                match users.as_slice() {
                    [] => "No agent uses it yet: pick it for one under Agents.".to_string(),
                    users => format!("Used by {}.", users.join(", ")),
                }
            }
            Selected::New => "Once it's added, pick it for an agent under Agents.".to_string(),
        };
        let api_button = |api: Api| {
            let on = self.api == api;
            div()
                .id(api.as_str())
                .flex_1()
                .px_3()
                .py(px(7.))
                .rounded_md()
                .border_1()
                .cursor_pointer()
                .text_size(px(13.))
                .text_center()
                .map(|d| match on {
                    true => d.border_color(theme::reaction_mine_border()).bg(theme::reaction_mine_bg()).font_weight(FontWeight::BOLD),
                    false => d.border_color(theme::border()).hover(|s| s.bg(theme::hover())),
                })
                .child(match api {
                    Api::Anthropic => "Anthropic Messages",
                    Api::Openai => "OpenAI-compatible",
                    Api::ClaudeCode => "Claude Code",
                })
                .on_click(cx.listener(move |this, _, _, cx| this.set_api(api, cx)))
        };

        form.child(field("NAME").child(self.name.clone()).child(hint(
            "What agents' provider = \"…\" in config.toml refers to. Renaming updates them.".into(),
        )))
        .child(
            field("API")
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(api_button(Api::Anthropic))
                        .child(api_button(Api::Openai))
                        .child(api_button(Api::ClaudeCode)),
                )
                .child(hint(match claude_code {
                    true => "Runs your own Claude Code CLI (claude -p), on your Claude subscription. Agents with a \
                             workspace run it in their container."
                        .into(),
                    false => "Any server compatible with it works too: proxies, gateways, local models.".into(),
                })),
        )
        .when(!claude_code, |d| d.child(field("BASE URL").child(self.base_url.clone()).child(hint(requests_go))))
        .child(field(if claude_code { "TOKEN" } else { "API KEY" }).child(self.api_key.clone()).when(claude_code, |d| {
            d.child(hint(
                "Paste what `claude setup-token` prints. Agents with a workspace need it: their container can't \
                 see this computer's login."
                    .into(),
            ))
        }).when(self.original.key_in_config, |d| {
            d.child(div().text_size(px(12.)).text_color(theme::error()).child(
                "This key is written in config.toml as plain text. Save to move it to keys.toml, which only you can read.",
            ))
        }))
        .child(field("OR READ THE KEY FROM").child(self.api_key_env.clone()).child(
            div().text_size(px(12.)).text_color(if ok { theme::muted() } else { theme::error() }).child(status.long),
        ))
        .child(hint(used_by))
    }

    fn render_agent_list(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("agent-list")
            .w(px(210.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme::border())
            .children(self.agents.iter().enumerate().map(|(i, a)| {
                let which = Which::Agent(a.name.clone());
                self.render_agent_item(("agent", i), a.name.clone(), format!("{} · {}", a.provider, a.model), which, cx)
            }))
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .text_size(px(12.))
                    .text_color(theme::muted())
                    .child("Add or remove agents in config.toml, then restart Pots."),
            )
            .child(div().mt_3().mx_3().border_t_1().border_color(theme::border()))
            .child(
                div()
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .text_size(px(12.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::muted())
                    .child("WORKER"),
            )
            .child({
                let detail = match &self.worker {
                    Some(w) => format!("{} · {}", w.provider, w.model),
                    None => "Not set up".to_string(),
                };
                self.render_agent_item(("worker", 0), pots_agent::worker::NAME.to_string(), detail, Which::Worker, cx)
            })
    }

    fn render_agent_item(
        &self,
        id: (&'static str, usize),
        title: String,
        detail: String,
        which: Which,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let selected = self.which().as_ref() == Some(&which);
        div()
            .id(id)
            .px_3()
            .py(px(6.))
            .rounded_md()
            .cursor_pointer()
            .map(|d| match selected {
                true => d.bg(theme::mention_bg()),
                false => d.hover(|s| s.bg(theme::hover())),
            })
            .child(div().text_size(px(14.)).font_weight(FontWeight::BOLD).child(title))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::muted())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(detail),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.notice = None;
                this.select_agent(Some(which.clone()), cx);
                window.focus(&this.model.read(cx).focus_handle(cx));
            }))
    }

    fn render_agent_form(&self, cx: &Context<Self>) -> impl IntoElement {
        let label = |text: &'static str| {
            div().text_size(px(12.)).font_weight(FontWeight::BOLD).text_color(theme::muted()).child(text)
        };
        let hint = |text: String| div().text_size(px(12.)).text_color(theme::muted()).child(text);
        let field = |name: &'static str| div().flex().flex_col().gap_1().child(label(name));

        let form = div().id("agent-form").flex_1().min_w_0().flex().flex_col().gap_4().p_5().overflow_y_scroll();
        if let Some(problem) = &self.problem {
            return form.child(div().text_size(px(13.)).text_color(theme::error()).child(problem.clone()));
        }
        let Some(editing) = &self.agent else {
            return form.child(hint("No agents yet. Add one as [[agents]] in config.toml, then restart Pots.".into()));
        };
        let worker_intro = editing.worker.then(|| {
            let setup = match self.worker {
                Some(_) => "",
                None => " It isn't set up yet: pick a provider and a model, then Save.",
            };
            hint(format!(
                "The cheaper model PM, Engineer and Reviewer hand look-ups to with ask_worker: reading code, \
                 summarizing a repository, running a command to see what it prints. It isn't in the chat: each ask \
                 starts fresh in a throwaway read-only checkout, and what it costs is added to the reply that \
                 asked.{setup}"
            ))
        });

        let provider_button = |i: usize, p: &ProviderFields| {
            let on = self.agent_provider == p.name;
            let name = p.name.clone();
            div()
                .id(("agent-provider", i))
                .px_3()
                .py(px(7.))
                .rounded_md()
                .border_1()
                .cursor_pointer()
                .text_size(px(13.))
                .map(|d| match on {
                    true => d.border_color(theme::reaction_mine_border()).bg(theme::reaction_mine_bg()).font_weight(FontWeight::BOLD),
                    false => d.border_color(theme::border()).hover(|s| s.bg(theme::hover())),
                })
                .child(p.name.clone())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.agent_provider = name.clone();
                    cx.notify();
                }))
        };
        let provider_hint = match self.providers.iter().find(|p| p.name == self.agent_provider) {
            Some(p) => {
                let (status, ok) = key_status(&p.api_key, &p.api_key_env, p.key_in_config);
                let text = match p.api {
                    Api::ClaudeCode => format!("Claude Code, on your subscription · {}", status.short),
                    _ => format!("{} API at {} · {}", api_label(p.api), p.base_url, status.short),
                };
                div().text_size(px(12.)).text_color(if ok { theme::muted() } else { theme::error() }).child(text)
            }
            None => hint(match editing.worker {
                true => "Pick where the worker sends its requests.".into(),
                false => "Pick where this agent sends its requests.".into(),
            }),
        };

        let typed = self.model.read(cx).text().trim().to_string();
        let model_ok = typed.is_empty() || self.listed_models().is_none_or(|m| m.iter().any(|(id, _)| *id == typed));
        let model_hint = match (self.listed_models(), typed.is_empty()) {
            (Some(_), true) => "The model name this provider expects. Type to see the ones it lists on models.dev.".to_string(),
            (None, true) => "The model name this provider expects.".to_string(),
            (Some(models), false) => match models.iter().find(|(id, _)| *id == typed) {
                Some((_, price)) => format!("On models.dev: {}.", price_label(price)),
                None => format!(
                    "{} doesn't list this model on models.dev: check the name{}.",
                    self.agent_provider,
                    if self.suggestions(cx).is_empty() { "" } else { ", or pick one below" }
                ),
            },
            // A provider models.dev doesn't know (a local server, a proxy):
            // the model may still be priced under another provider.
            (None, false) => match self.catalog.as_ref().and_then(|c| c.price(&[], &typed)) {
                Some(price) => format!("On models.dev: {}.", price_label(&price)),
                None => "Replies won't show what they cost unless the agent has price = { … } in config.toml.".to_string(),
            },
        };
        let suggestions = self.suggestions(cx);

        form.children(worker_intro)
        .child(
            field("PROVIDER")
                .child(div().flex().flex_wrap().gap_2().children(self.providers.iter().enumerate().map(|(i, p)| provider_button(i, p))))
                .child(provider_hint),
        )
        .child(field("MODEL").child(self.model.clone()).child(
            div().text_size(px(12.)).text_color(if model_ok { theme::muted() } else { theme::error() }).child(model_hint),
        ).when(!suggestions.is_empty(), |d| {
            d.child(div().flex().flex_col().children(suggestions.into_iter().enumerate().map(|(i, (id, price))| {
                let pick = id.clone();
                div()
                    .id(("model-suggestion", i))
                    .flex()
                    .justify_between()
                    .gap_3()
                    .px_2()
                    .py(px(4.))
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .hover(|s| s.bg(theme::hover()))
                    .child(div().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(id))
                    .child(div().flex_none().text_size(px(12.)).text_color(theme::muted()).child(price_label(&price)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.model.update(cx, |f, cx| f.set_text(pick.clone(), cx));
                        window.focus(&this.model.read(cx).focus_handle(cx));
                    }))
            })))
        }))
        .child(hint(match editing.worker {
            true => "Saving takes effect from the next ask; one already running finishes as it started.".into(),
            false => "Saving takes effect from the agent's next reply; one already running finishes as it started.".into(),
        }))
    }

    fn render_footer(&self, cx: &Context<Self>) -> impl IntoElement {
        if self.page == Page::Agents {
            return self.render_agent_footer(cx).into_any_element();
        }
        self.render_provider_footer(cx).into_any_element()
    }

    fn render_agent_footer(&self, cx: &Context<Self>) -> impl IntoElement {
        let edited = self.agent_edited(cx);
        let button = |id: &'static str, text: &'static str| {
            div().id(id).px_4().py(px(6.)).rounded_md().text_size(px(14.)).font_weight(FontWeight::BOLD).child(text)
        };
        footer_bar()
            .child(status_line(&self.error, &self.notice))
            .when(edited, |d| {
                d.child(
                    button("revert-agent", "Revert")
                        .cursor_pointer()
                        .border_1()
                        .border_color(theme::border())
                        .hover(|s| s.bg(theme::hover()))
                        .on_click(cx.listener(|this, _, _, cx| this.select_agent(this.which(), cx))),
                )
            })
            .when(self.which() == Some(Which::Worker) && self.worker.is_some() && !edited, |d| {
                d.child(
                    button("remove-worker", "Remove")
                        .cursor_pointer()
                        .border_1()
                        .border_color(theme::border())
                        .text_color(theme::error())
                        .hover(|s| s.border_color(theme::error()))
                        .on_click(cx.listener(|this, _, window, cx| this.remove_worker(window, cx))),
                )
            })
            .when(self.agent.is_some(), |d| {
                d.child(button("save-agent", "Save").map(|b| match edited {
                    true => b
                        .cursor_pointer()
                        .bg(theme::send())
                        .text_color(gpui::white())
                        .hover(|s| s.bg(theme::send_hover()))
                        .on_click(cx.listener(|this, _, window, cx| this.save_agent(window, cx))),
                    false => b.bg(theme::composer_toolbar()).text_color(theme::faint()),
                }))
            })
    }

    fn render_provider_footer(&self, cx: &Context<Self>) -> impl IntoElement {
        let edited = self.selected.is_some() && self.edited(cx);
        let existing = matches!(self.selected, Some(Selected::Provider(_)));
        let button = |id: &'static str, text: &'static str| {
            div().id(id).px_4().py(px(6.)).rounded_md().text_size(px(14.)).font_weight(FontWeight::BOLD).child(text)
        };
        footer_bar()
            .when(existing, |d| {
                d.child(
                    button("delete-provider", "Delete")
                        .cursor_pointer()
                        .text_color(theme::error())
                        .hover(|s| s.bg(theme::hover()))
                        .on_click(cx.listener(|this, _, window, cx| this.delete(window, cx))),
                )
            })
            .child(status_line(&self.error, &self.notice))
            .when(edited, |d| {
                d.child(
                    button("revert-provider", "Revert")
                        .cursor_pointer()
                        .border_1()
                        .border_color(theme::border())
                        .hover(|s| s.bg(theme::hover()))
                        .on_click(cx.listener(|this, _, window, cx| match this.selected.clone() {
                            Some(Selected::Provider(name)) => this.select(Selected::Provider(name), window, cx),
                            _ => this.reload(None, window, cx),
                        })),
                )
            })
            .when(self.selected.is_some(), |d| {
                d.child(
                    button("save-provider", if existing { "Save" } else { "Add" }).map(|b| match edited {
                        true => b
                            .cursor_pointer()
                            .bg(theme::send())
                            .text_color(gpui::white())
                            .hover(|s| s.bg(theme::send_hover()))
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                        false => b.bg(theme::composer_toolbar()).text_color(theme::faint()),
                    }),
                )
            })
    }
}

impl Render for Settings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, subtitle) = match self.page {
            Page::Providers => (
                "Providers",
                format!("Where agents send their requests. Saved to {}, keys to keys.toml beside it.", config_path()),
            ),
            Page::Agents => (
                "Agents",
                format!("Which provider and model each agent and the worker use. Saved to {}.", config_path()),
            ),
        };
        div()
            .id("settings")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.4))
            .occlude()
            .child(
                div()
                    .track_focus(&self.focus)
                    .w(px(860.))
                    .h(px(560.))
                    .max_w_full()
                    .max_h_full()
                    .flex()
                    .rounded_lg()
                    .overflow_hidden()
                    .bg(theme::bg())
                    .shadow_lg()
                    .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                        if e.keystroke.key == "escape" {
                            this.close(cx);
                        }
                    }))
                    .child(self.render_nav(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_start()
                                    .justify_between()
                                    .gap_4()
                                    .pl_5()
                                    .pr_3()
                                    .pt_4()
                                    .pb_3()
                                    .border_b_1()
                                    .border_color(theme::border())
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(div().text_size(px(20.)).font_weight(FontWeight::BOLD).child(title))
                                            .child(
                                                div()
                                                    .text_size(px(13.))
                                                    .text_color(theme::muted())
                                                    .whitespace_nowrap()
                                                    .overflow_hidden()
                                                    .text_ellipsis()
                                                    .child(subtitle),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id("close-settings")
                                            .flex_none()
                                            .size(px(28.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_md()
                                            .cursor_pointer()
                                            .text_color(theme::muted())
                                            .hover(|s| s.bg(theme::reaction_bg()).text_color(theme::text()))
                                            .child("✕")
                                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .flex()
                                    .map(|d| match self.page {
                                        Page::Providers => d.child(self.render_list(cx)).child(self.render_form(cx)),
                                        Page::Agents => d.child(self.render_agent_list(cx)).child(self.render_agent_form(cx)),
                                    }),
                            )
                            .child(self.render_footer(cx)),
                    ),
            )
    }
}

/// config.toml's path, with the home directory as `~`.
fn config_path() -> String {
    let path = store::config_path();
    match std::env::var_os("HOME").and_then(|h| path.strip_prefix(h).ok().map(|p| p.to_path_buf())) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn footer_bar() -> gpui::Div {
    div().flex_none().flex().items_center().gap_3().px_5().py_3().border_t_1().border_color(theme::border())
}

/// Why the last save failed, or what it did.
fn status_line(error: &Option<String>, notice: &Option<String>) -> impl IntoElement {
    div().flex_1().min_w_0().text_size(px(13.)).map(|d| match (error, notice) {
        (Some(e), _) => d.text_color(theme::error()).child(e.clone()),
        (None, Some(n)) => d.text_color(theme::checkbox_on()).child(format!("✓ {n}")),
        (None, None) => d,
    })
}

/// `s` lowercased, with `.` read as `-`, for matching model names.
fn loose(s: &str) -> String {
    s.to_lowercase().replace('.', "-")
}

/// "$5 in · $25 out per M tokens".
fn price_label(p: &Price) -> String {
    let dollars = |x: f64| {
        let s = format!("{x:.2}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        format!("${s}")
    };
    format!("{} in · {} out per M tokens", dollars(p.input), dollars(p.output))
}

fn api_label(api: Api) -> &'static str {
    match api {
        Api::Anthropic => "Anthropic",
        Api::Openai => "OpenAI",
        Api::ClaudeCode => "Claude Code",
    }
}

struct KeyStatus {
    /// For the provider list.
    short: String,
    /// Under the key fields.
    long: String,
}

/// Where a provider's key comes from, and whether it is there and private.
/// `in_config`: the saved key is still written in config.toml.
fn key_status(key: &str, env: &str, in_config: bool) -> (KeyStatus, bool) {
    let (key, env) = (key.trim(), env.trim());
    if !key.is_empty() && in_config {
        let long = match env.is_empty() {
            true => "The key above is used.".to_string(),
            false => format!("The key above is used, not ${env}."),
        };
        return (KeyStatus { short: "key in config".into(), long }, false);
    }
    if !key.is_empty() {
        let long = match env.is_empty() {
            true => "The key above is used. It's kept in keys.toml, which only you can read.".to_string(),
            false => format!("The key above is used, not ${env}. It's kept in keys.toml, which only you can read."),
        };
        return (KeyStatus { short: "key saved".into(), long }, true);
    }
    if env.is_empty() {
        let long = "No key: requests go without one, which is fine for a local server.".to_string();
        return (KeyStatus { short: "no key".into(), long }, true);
    }
    if std::env::var(env).is_ok_and(|v| !v.is_empty()) {
        return (KeyStatus { short: "key from env".into(), long: format!("Using ${env} from Pots' environment.") }, true);
    }
    let long = format!(
        "${env} isn't set where Pots runs (apps opened from the Dock don't see your shell's variables). Paste the key above instead."
    );
    (KeyStatus { short: "key missing".into(), long }, false)
}
