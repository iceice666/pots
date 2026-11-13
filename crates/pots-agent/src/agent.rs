//! LLM-backed teammates: which messages they answer, what they see, and
//! the tools they can use. The reply loop itself is in [`crate::reply`].

use serde_json::{Value, json};

use std::path::PathBuf;

use crate::config::{
    AgentConfig, ChannelConfig, Config, ProjectConfig, SandboxConfig, TeamConfig, WorkerConfig, WorkspaceMode, expand_home,
};
use crate::data::{Card, Conv, Decision, MergeOutcome, Retired, UserKind, Workspace};
use crate::reply::Reply;
use pots_llm::{Api, Part, Provider, Request, Role, ToolSpec, Turn};
use crate::sandbox;
use crate::store;
use crate::worker;

const DEFAULT_HISTORY: usize = 40;
const DEFAULT_MAX_STEPS: u32 = 8;
/// Coding takes many more tool calls per reply than chatting.
const DEFAULT_MAX_STEPS_WORKSPACE: u32 = 60;
const PALETTE: [u32; 5] = [0x0f766e, 0x7c3aed, 0xb45309, 0x0369a1, 0xbe185d];

pub struct Agent {
    pub user: usize,
    pub config: AgentConfig,
    pub provider: Provider,
    /// Set when the provider's key variable isn't in the environment.
    pub missing_key: Option<String>,
}

impl Agent {
    pub fn max_steps(&self) -> u32 {
        let default = if self.config.workspace.is_some() { DEFAULT_MAX_STEPS_WORKSPACE } else { DEFAULT_MAX_STEPS };
        self.config.max_steps.unwrap_or(default).max(1)
    }
}

/// `[worker]`: the model `ask_worker` runs on.
pub struct Worker {
    pub config: WorkerConfig,
    pub provider: Provider,
    pub missing_key: Option<String>,
}

impl Worker {
    fn new(config: &Config) -> Option<Self> {
        let wc = config.worker.clone()?;
        let pc = &config.providers[&wc.provider];
        Some(Self { provider: Provider::new(pc), missing_key: missing_key(&wc.provider, pc), config: wc })
    }
}

#[derive(Default)]
pub struct Agents {
    pub list: Vec<Agent>,
    /// Why no agents could be loaded (a broken config file).
    pub problem: Option<String>,
    pub project: Option<ProjectConfig>,
    pub sandbox: SandboxConfig,
    pub team: TeamConfig,
    /// `[[channels]]`: what they set is reapplied on every load.
    pub channels: Vec<ChannelConfig>,
    /// Model prices from models.dev, once downloaded.
    pub catalog: Option<std::sync::Arc<pots_llm::cost::Catalog>>,
    pub worker: Option<Worker>,
}

/// Why provider `name` has no key, if it's meant to come from the
/// environment and isn't there.
fn missing_key(name: &str, pc: &crate::config::ProviderConfig) -> Option<String> {
    match (&pc.api_key, &pc.api_key_env) {
        (None, Some(var)) if pc.key().is_none() => Some(format!(
            "No API key: set the `{var}` environment variable, or add a key to {name} in Settings → Providers."
        )),
        _ => None,
    }
}

/// The models.dev provider ids a provider's models are likely listed
/// under, best first: its name in config.toml, the name in its base URL's
/// host (openrouter.ai, api.anthropic.com), then its API's name.
pub fn catalog_ids(name: &str, api: crate::config::Api, base_url: &str) -> Vec<String> {
    // Claude Code takes Anthropic's model names.
    if api == Api::ClaudeCode {
        return vec!["anthropic".to_string()];
    }
    let host = base_url.split("://").nth(1).unwrap_or(base_url).split(['/', ':']).next().unwrap_or("");
    let host = host.strip_prefix("api.").unwrap_or(host);
    let site = host.split('.').next().unwrap_or("");
    let mut ids = Vec::new();
    for id in [name.to_lowercase(), site.to_lowercase(), api.as_str().to_string()] {
        if !id.is_empty() && !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// Who answers a message.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Triggered {
    pub agents: Vec<usize>,
    /// Reviewers to start once the task's writer has finished its reply:
    /// what they'd review is still being written.
    pub waiting: Vec<usize>,
    /// Agents were @mentioned but have used up their turns in a row.
    pub paused: bool,
}

impl Agents {
    /// Read `config.toml` (creating the default one on first run) and make
    /// sure every configured agent has a user and a DM channel.
    pub fn load(ws: &mut Workspace) -> Self {
        match store::load_config() {
            Ok(config) => Self::from_config(ws, &config),
            Err(e) => {
                for u in ws.users.iter_mut().filter(|u| u.kind == UserKind::Agent) {
                    u.online = false;
                }
                Self { problem: Some(e), ..Self::default() }
            }
        }
    }

    pub fn from_config(ws: &mut Workspace, config: &Config) -> Self {
        // Agents dropped from the config keep their history, shown offline.
        for u in ws.users.iter_mut().filter(|u| u.kind == UserKind::Agent) {
            u.online = false;
        }
        let mut list = Vec::new();
        for (i, ac) in config.agents.iter().enumerate() {
            let user = match ws.users.iter().position(|u| u.kind == UserKind::Agent && u.name == ac.name) {
                Some(u) => u,
                None => {
                    ws.users.push(crate::data::User {
                        name: ac.name.clone(),
                        color: PALETTE[i % PALETTE.len()],
                        online: true,
                        kind: UserKind::Agent,
                    });
                    ws.users.len() - 1
                }
            };
            ws.users[user].online = true;
            if let Some(color) = ac.color() {
                ws.users[user].color = color;
            }
            ws.ensure_dm(user);
            let pc = &config.providers[&ac.provider];
            let missing_key = missing_key(&ac.provider, pc);
            list.push(Agent { user, config: ac.clone(), provider: Provider::new(pc), missing_key });
        }
        // Channels in the config exist, and what the config says about them
        // (repo, topic) wins over what was set in the app. Anything it leaves
        // out, including channels it doesn't list, stays as the app has it.
        for cc in &config.channels {
            let c = ws.find_channel(&cc.name).unwrap_or_else(|| ws.add_channel(&cc.name));
            if let Some(repo) = &cc.repo {
                ws.channels[c].repo = Some(repo.clone());
            }
            if let Some(topic) = &cc.topic {
                ws.channels[c].topic = topic.clone();
            }
        }
        Self {
            list,
            problem: None,
            project: config.project.clone(),
            sandbox: config.sandbox.clone(),
            team: config.team.clone(),
            channels: config.channels.clone(),
            catalog: None,
            worker: Worker::new(config),
        }
    }

    /// Take up what Settings changed in `config`: providers, and each
    /// agent's provider and model, matching agents by name. Replies already
    /// running finish as they started; the next one uses the new settings.
    /// The agent list itself stays as it is, so replies in flight and queued
    /// agents keep pointing at the right one; agents config.toml gained or
    /// lost wait for the next launch.
    pub fn apply_settings(&mut self, config: &Config) {
        for agent in &mut self.list {
            let Some(ac) = config.agents.iter().find(|ac| ac.name == agent.config.name) else { continue };
            let pc = &config.providers[&ac.provider];
            agent.config.provider = ac.provider.clone();
            agent.config.model = ac.model.clone();
            agent.provider = Provider::new(pc);
            agent.missing_key = missing_key(&ac.provider, pc);
        }
        self.worker = Worker::new(config);
    }

    /// Agents that should answer message `ix` of `conv`.
    ///
    /// A person's message reaches the agent whose DM it is, the agents it
    /// @mentions, and otherwise, in a thread, the agent that spoke last
    /// there. An agent's message only reaches agents it @mentions, and only
    /// until `max_agent_turns` agent messages in a row. An agent already
    /// writing a reply in `conv` isn't started again.
    pub fn triggered(&self, ws: &Workspace, conv: Conv, ix: usize) -> Triggered {
        let msg = ws.message(conv, ix);
        let author = ws.users[msg.author].kind;
        if author == UserKind::Bot {
            return Triggered::default();
        }
        let by_user = |user: usize| self.list.iter().position(|a| a.user == user);
        let mentions = msg.mentions();
        let mut agents: Vec<usize> = (0..self.list.len())
            .filter(|&a| mentions.iter().any(|m| ws.users[self.list[a].user].answers_to(m)))
            .collect();
        if author == UserKind::Human {
            if let Conv::Channel(c) = conv {
                agents.extend(ws.channels[c].dm.and_then(by_user));
            }
            if agents.is_empty() && matches!(conv, Conv::Thread(_)) {
                let last = (0..ix).rev().map(|i| ws.message(conv, i).author).find(|&u| ws.users[u].kind == UserKind::Agent);
                agents.extend(last.and_then(by_user));
            }
        }
        agents.retain(|&a| self.list[a].user != msg.author);
        let mut seen = Vec::new();
        agents.retain(|a| !seen.contains(a) && {
            seen.push(*a);
            true
        });
        agents.retain(|&a| !self.busy(a, ws, conv));

        if author == UserKind::Agent && !agents.is_empty() {
            let run = (0..=ix).rev().take_while(|&i| ws.users[ws.message(conv, i).author].kind == UserKind::Agent).count();
            if run >= self.team.max_agent_turns {
                return Triggered { paused: true, ..Triggered::default() };
            }
        }
        // A writer already replying, or one asked by this very message.
        let is_writer = |a: usize| self.list[a].config.workspace == Some(WorkspaceMode::Write);
        let wait = self.writer_busy(ws, conv) || agents.iter().any(|&a| is_writer(a));
        let (waiting, agents) =
            agents.into_iter().partition(|&a| wait && self.list[a].config.workspace == Some(WorkspaceMode::Review));
        Triggered { agents, waiting, paused: false }
    }

    /// The agents that coordinate the task in `conv` (no workspace, like
    /// PM): the one that started its thread with start_task, or else all
    /// of them.
    pub fn coordinators(&self, ws: &Workspace, conv: Conv) -> Vec<usize> {
        let coordinates = |a: &usize| self.list[*a].config.workspace.is_none();
        if let Conv::Thread(t) = conv {
            let starter = ws.message(Conv::Thread(t), 0).author;
            if let Some(a) = self.list.iter().position(|a| a.user == starter).filter(coordinates) {
                return vec![a];
            }
        }
        (0..self.list.len()).filter(coordinates).collect()
    }

    /// Whether agent `a` is writing a reply in `conv`.
    pub fn busy(&self, a: usize, ws: &Workspace, conv: Conv) -> bool {
        (0..ws.len(conv)).any(|i| {
            let m = ws.message(conv, i);
            m.pending && m.author == self.list[a].user
        })
    }

    /// Whether an agent that writes code for `conv`'s task is replying there.
    pub fn writer_busy(&self, ws: &Workspace, conv: Conv) -> bool {
        (0..self.list.len()).any(|a| self.list[a].config.workspace == Some(WorkspaceMode::Write) && self.busy(a, ws, conv))
    }

    /// Agent `a`'s model price: from its config, or from models.dev (see
    /// [`catalog_ids`]).
    pub fn price(&self, a: usize) -> Option<pots_llm::cost::Price> {
        let agent = &self.list[a];
        self.price_of(&agent.config.provider, &agent.provider, &agent.config.model, agent.config.price)
    }

    /// The worker's model price, like [`price`](Self::price).
    pub fn worker_price(&self) -> Option<pots_llm::cost::Price> {
        let w = self.worker.as_ref()?;
        self.price_of(&w.config.provider, &w.provider, &w.config.model, w.config.price)
    }

    fn price_of(
        &self,
        name: &str,
        provider: &Provider,
        model: &str,
        configured: Option<pots_llm::cost::Price>,
    ) -> Option<pots_llm::cost::Price> {
        // On the owner's subscription: tokens cost nothing extra.
        if provider.api == Api::ClaudeCode {
            return None;
        }
        configured.or_else(|| {
            let ids = catalog_ids(name, provider.api, &provider.base_url);
            let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
            self.catalog.as_ref()?.price(&ids, model)
        })
    }

    /// The repository work in `conv` happens in: its channel's (a thread's
    /// is its parent channel's), or else `[project]`'s.
    pub fn repo(&self, ws: &Workspace, conv: Conv) -> Option<PathBuf> {
        let c = match conv {
            Conv::Channel(c) => c,
            Conv::Thread(t) => ws.threads[t].channel,
        };
        ws.channels[c].repo.as_deref().map(expand_home).or_else(|| self.project.as_ref().map(ProjectConfig::repo_path))
    }

    /// The `[[channels]]` entry for channel `c`, if config.toml has one.
    pub fn channel_config(&self, ws: &Workspace, c: usize) -> Option<&ChannelConfig> {
        self.channels.iter().find(|cc| cc.name.eq_ignore_ascii_case(&ws.channels[c].name))
    }

    /// The workspace agent `a` works in when replying in `conv`.
    pub fn workspace(&self, a: usize, ws: &Workspace, conv: Conv) -> Option<sandbox::Env> {
        let agent = &self.list[a];
        let mode = agent.config.workspace?;
        let repo = self.repo(ws, conv)?;
        Some(sandbox::Env::new(
            mode,
            sandbox::find_docker(self.sandbox.docker.as_deref()),
            agent.config.image.clone().unwrap_or_else(|| self.sandbox.image.clone()),
            repo,
            &store::home().join("tasks"),
            &task_key(ws, conv),
            &agent.config.name,
        ))
    }

    /// Agent `a`'s reply in `conv`, written into its placeholder message at
    /// index `upto`, ready to [`run`](Reply::run).
    pub fn reply(&self, a: usize, ws: &Workspace, conv: Conv, upto: usize) -> Reply {
        let agent = &self.list[a];
        Reply {
            provider: agent.provider.clone(),
            max_steps: agent.max_steps(),
            request: self.request(a, ws, conv, upto),
            session: self.workspace(a, ws, conv).map(sandbox::Session::new),
            worker: self.worker_job(a, ws, conv),
            conflicts: Some(self.conflict_check(ws)),
        }
    }

    /// Every task that may have a writer's branch, by repository, for
    /// `check_merge_conflicts` and merge requests' conflict warnings. Tasks nobody has worked on yet are skipped
    /// when the check runs (it's the one that looks at the disk).
    pub fn conflict_check(&self, ws: &Workspace) -> sandbox::ConflictCheck {
        let tasks = store::home().join("tasks");
        let mut repos: Vec<(PathBuf, Vec<String>, Vec<sandbox::TaskBranch>)> = Vec::new();
        let convs = (0..ws.threads.len()).map(Conv::Thread).chain((0..ws.channels.len()).map(Conv::Channel));
        for conv in convs {
            let Some(repo) = self.repo(ws, conv) else { continue };
            let key = task_key(ws, conv);
            let channel = match conv {
                Conv::Channel(c) => c,
                Conv::Thread(t) => ws.threads[t].channel,
            };
            let label = match conv {
                Conv::Thread(t) => format!("thread “{}” in #{}", ws.threads[t].name, ws.channels[channel].name),
                Conv::Channel(c) => match ws.channels[c].dm {
                    Some(u) => format!("DM with {}", display_name(ws, u)),
                    None => format!("#{} (outside threads)", ws.channels[c].name),
                },
            };
            let task = sandbox::TaskBranch {
                label,
                clone: tasks.join(&key).join("repo"),
                branch: format!("pots/{key}"),
                status: merge_status(ws, conv),
            };
            let at = match repos.iter().position(|(r, _, _)| *r == repo) {
                Some(at) => at,
                None => {
                    repos.push((repo, Vec::new(), Vec::new()));
                    repos.len() - 1
                }
            };
            let (_, channels, list) = &mut repos[at];
            let name = ws.channels[channel].name.to_lowercase();
            if ws.channels[channel].dm.is_none() && !channels.contains(&name) {
                channels.push(name);
            }
            list.push(task);
        }
        sandbox::ConflictCheck { scratch: tasks, repos }
    }

    /// What agent `a`'s `ask_worker` calls in `conv` run, if there's a worker.
    pub fn worker_job(&self, a: usize, ws: &Workspace, conv: Conv) -> Option<worker::Job> {
        let w = self.worker.as_ref()?;
        let caller = &self.list[a].config.name;
        // One checkout per asking agent, so two agents' asks never share one.
        let name = format!("{}-{caller}", worker::NAME);
        let env = |repo: PathBuf, task: &str| {
            sandbox::Env::new(
                WorkspaceMode::Scratch,
                sandbox::find_docker(self.sandbox.docker.as_deref()),
                w.config.image.clone().unwrap_or_else(|| self.sandbox.image.clone()),
                repo,
                &store::home().join("tasks"),
                task,
                &name,
            )
        };
        let channels = (0..ws.channels.len())
            .filter(|&c| ws.channels[c].dm.is_none())
            .filter_map(|c| {
                let repo = ws.channels[c].repo.as_deref().map(expand_home)?;
                Some((ws.channels[c].name.to_lowercase(), env(repo, &task_key(ws, Conv::Channel(c)))))
            })
            .collect();
        let prompt = match w.config.system_prompt.trim() {
            "" => worker::DEFAULT_PROMPT,
            p => p,
        };
        let system = format!(
            "{prompt}\n\n# Who you work for\nThis task comes from {caller}, a teammate in the \"{wsname}\" team chat. \
             Today is {today}.",
            wsname = ws.name,
            today = chrono::Local::now().format("%A, %B %-d, %Y"),
        );
        Some(worker::Job {
            provider: w.provider.clone(),
            max_steps: w.config.max_steps.unwrap_or(worker::DEFAULT_MAX_STEPS).max(1),
            request: Request {
                model: w.config.model.clone(),
                system,
                turns: Vec::new(),
                tools: worker::chat_tools(),
                max_tokens: w.config.max_tokens,
                extra: w.config.extra_body(),
            },
            claude_code: w.provider.api == Api::ClaudeCode,
            env: self.repo(ws, conv).map(|repo| env(repo, &task_key(ws, conv))),
            channels,
            problem: w.missing_key.clone(),
            spent: Default::default(),
        })
    }

    /// The first request for agent `a` replying in `conv`, seeing the
    /// messages before index `upto` (its own placeholder).
    pub fn request(&self, a: usize, ws: &Workspace, conv: Conv, upto: usize) -> Request {
        let agent = &self.list[a];
        let limit = agent.config.history.unwrap_or(DEFAULT_HISTORY).max(1);
        let mut system = agent.config.system_prompt.trim().to_string();
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str(&context(ws, conv, agent.user));
        let mut tools = tools::specs();
        // Agents that don't write or review code coordinate: they hand out
        // tasks and keep parallel ones from getting in each other's way,
        // and keep the decision log.
        if agent.config.workspace.is_none() {
            tools.push(tools::start_task_spec());
            tools.push(tools::post_in_thread_spec());
            tools.push(sandbox::check_merge_conflicts_spec());
            tools.push(tools::record_decision_spec());
            system.push_str(COORDINATING);
            system.push_str(RECORDING);
        }
        system.push_str(&decisions(ws));
        // Workers in a task thread say when it ends without a merge request.
        if agent.config.workspace.is_some() && matches!(conv, Conv::Thread(_)) {
            tools.push(tools::task_done_spec());
            system.push_str(FINISHING);
        }
        if self.worker.is_some() {
            tools.push(worker::spec());
            system.push_str(
                "\n\n# Your worker\n\
                 Hand look-ups that only gather information to ask_worker: it runs on a cheaper model, so keep \
                 your own steps for judgement and for changes.",
            );
        }
        if let Some(env) = self.workspace(a, ws, conv) {
            system.push_str("\n\n");
            // Claude Code works in the container with tools of its own.
            match agent.provider.api {
                Api::ClaudeCode => {
                    system.push_str(&env.describe_claude_code());
                    tools.extend(sandbox::specs(env.mode).into_iter().filter(|t| !sandbox::is_tool(t.name)));
                }
                _ => {
                    system.push_str(&env.describe());
                    tools.extend(sandbox::specs(env.mode));
                }
            }
        }
        Request {
            model: agent.config.model.clone(),
            system,
            turns: history(ws, conv, agent.user, upto, limit),
            tools,
            max_tokens: agent.config.max_tokens,
            extra: agent.config.extra_body(),
        }
    }
}

/// Add `text` to `request` as the latest thing said to the agent.
pub fn add_note(request: &mut Request, text: &str) {
    match request.turns.last_mut().filter(|t| t.role == Role::User).and_then(|t| t.parts.last_mut()) {
        Some(Part::Text(last)) => {
            last.push_str("\n\n");
            last.push_str(text);
        }
        _ => request.turns.push(Turn::user(text)),
    }
}

/// How a task is named to agents: “thread “A” in #backend”, or its channel.
pub fn task_place(ws: &Workspace, conv: Conv) -> String {
    match conv {
        Conv::Thread(t) => format!("thread “{}” in #{}", ws.threads[t].name, ws.channels[ws.threads[t].channel].name),
        Conv::Channel(c) => format!("#{}", ws.channels[c].name),
    }
}

/// Where coordinators report on the task in `conv`: #general, or else the
/// task's own channel.
pub fn report_conv(ws: &Workspace, conv: Conv) -> Conv {
    let channel = match conv {
        Conv::Thread(t) => ws.threads[t].channel,
        Conv::Channel(c) => c,
    };
    let general = ws.find_channel("general").filter(|&c| ws.channels[c].dm.is_none());
    Conv::Channel(general.unwrap_or(channel))
}

/// The commits a note lists at most.
const NOTE_COMMITS: usize = 10;

/// What a coordinator is told when `requester` asks the owner to merge
/// `branch` from `place`: report it, and coordinate any `conflicts`.
pub fn merge_request_note(
    requester: &str,
    branch: &str,
    place: &str,
    summary: &str,
    commits: &[String],
    conflicts: Option<&str>,
) -> String {
    let mut note = format!(
        "Pots (the app, not the owner): {requester} just asked the owner to merge `{branch}` from {place}. \
         Their summary: {}\nCommits:\n",
        summary.trim()
    );
    for c in commits.iter().take(NOTE_COMMITS) {
        note.push_str(&format!("- {c}\n"));
    }
    if commits.len() > NOTE_COMMITS {
        note.push_str(&format!("- …and {} more\n", commits.len() - NOTE_COMMITS));
    }
    match conflicts {
        Some(conflicts) => note.push_str(&format!(
            "Checked against the other tasks, it conflicts with other work:\n{conflicts}\n\
             Coordinate it: tell the owner here, briefly, which task is ready, what conflicts with what, and \
             recommend a merge order with ask_owner. Run check_merge_conflicts first if you need the full \
             picture. Don't start new tasks for it unasked."
        )),
        None => note.push_str(
            "Report it to the owner here, in a line or two: which task is ready for them to accept, and where \
             the merge request is. If other tasks are still open, say briefly where they stand. Don't start new \
             tasks for it unasked.",
        ),
    }
    note
}

/// What a coordinator is told when the owner accepted `branch` from `place`.
pub fn merge_approved_note(branch: &str, place: &str) -> String {
    format!(
        "Pots (the app, not the owner): the owner accepted the merge request for `{branch}` from {place}. The \
         branch is now in the project repo under the same name; merging it into the default branch is up to \
         the owner. Report it to the owner here, in a line or two, with what is still open. If an open task \
         touches the same code, say which one should wait for this branch or rebase once it's merged. Don't \
         start new tasks for it unasked."
    )
}

/// What a coordinator is told when `who` calls task_done in `place`.
pub fn task_done_note(who: &str, place: &str, done: &tools::Done) -> String {
    let what = match done.status {
        tools::DoneStatus::Done => "finished the task without a merge request",
        tools::DoneStatus::Stuck => "is stuck on the task and can't go on",
    };
    let ask = match done.status {
        tools::DoneStatus::Done => "Report it to the owner here, in a line or two: what came out of it, and where.",
        tools::DoneStatus::Stuck => {
            "Tell the owner here, briefly, what is in the way and what would unblock it. If it needs their \
             decision, use ask_owner."
        }
    };
    format!(
        "Pots (the app, not the owner): {who} {what}, in {place}. Their summary: {}\n{ask} If other tasks are \
         still open, say briefly where they stand. Don't start new tasks for it unasked.",
        done.summary
    )
}

/// What a coordinator is told when `asker` is waiting on the owner's
/// answer to a choice card in `place`.
pub fn owner_question_note(asker: &str, place: &str, question: &str, options: &[String]) -> String {
    format!(
        "Pots (the app, not the owner): {asker} asked the owner a question in {place} and is waiting for the \
         answer: “{}” Options: {}.\nTell the owner here, in a line, that the task is waiting on them and where \
         to answer. Don't answer it for them, and don't ask it again with ask_owner.",
        question.trim(),
        options.join(" / ")
    )
}

/// The most decisions shown to agents: the most recent ones in force.
const MAX_DECISIONS_SHOWN: usize = 80;

/// The decision log as agents see it: the decisions still in force, oldest
/// first. Dates are spelled out, since agents see no dates on messages.
pub fn decisions(ws: &Workspace) -> String {
    let active: Vec<(usize, &Decision)> = ws.decisions.iter().enumerate().filter(|(_, d)| d.retired.is_none()).collect();
    let mut out = String::from(
        "\n\n# Decisions so far\n\
         The owner's decisions that hold across conversations, from the team's decision log. Follow them. When \
         a request goes against one, say which, and ask before acting against it.\n",
    );
    if active.is_empty() {
        out.push_str("(none recorded yet)");
        return out;
    }
    let skipped = active.len().saturating_sub(MAX_DECISIONS_SHOWN);
    if skipped > 0 {
        out.push_str(&format!("({skipped} older decisions not shown.)\n"));
    }
    for (id, d) in &active[skipped..] {
        let date = chrono::TimeZone::timestamp_opt(&chrono::Local, d.ts, 0)
            .single()
            .map(|t| t.format("%b %-d, %Y").to_string())
            .unwrap_or_default();
        let place = match d.place.as_str() {
            "" => String::new(),
            p => format!(", {p}"),
        };
        out.push_str(&format!("- [{id}] ({date}{place}) {}\n", d.text));
    }
    out.truncate(out.trim_end().len());
    out
}

/// Where a decision made in `conv` was made, for the log.
fn decision_place(ws: &Workspace, conv: Conv) -> String {
    match conv {
        Conv::Channel(c) if ws.channels[c].dm.is_some() => "direct messages".to_string(),
        conv => task_place(ws, conv),
    }
}

/// What coordinators are told about keeping the decision log.
const RECORDING: &str = "\n\n# The decision log\n\
Teammates only see the conversation they are in, plus the decision log below. So when the owner settles \
something meant to hold beyond this conversation (a choice, a constraint, a convention, a date, a priority), \
call record_decision right away with one self-contained line. If it changes an earlier decision, pass that \
decision's id in `replaces`. Don't record tasks or status, things already in the log, or your own suggestions \
the owner hasn't agreed to.\n\
The owner often settles things in follow-ups that don't @mention you, so they never reached you. Each time you \
reply, look over the conversation you can see for decisions missing from the log, and record those too.";

/// What agents with a workspace are told about ending a task.
const FINISHING: &str = "\n\n# Ending a task\n\
When this thread's task ends without a merge request (you answered the question, found there is nothing to \
change, or can't go on), call task_done with a short summary, so PM can tell the owner. Don't call it after \
request_merge or ask_owner: those reach PM already.";

/// What coordinators are told about tasks running side by side.
const COORDINATING: &str = "\n\n# Tasks side by side\n\
Each task works on a branch of its own, started from the default branch, and nobody sees the other tasks' \
work until the owner merges it. Two tasks changing the same code therefore conflict when the second one \
is merged. Keep that from surprising the owner:\n\
- Before starting a task, think about which code it touches. If an open task in the same repository may \
touch the same code, run check_merge_conflicts and read that task's thread. Then fold the new work into \
the open task (post_in_thread to its engineer), or start it once the open one is merged, or start it \
anyway, saying in its brief what the open task changes and telling the open task with post_in_thread.\n\
- When the owner asks about merging, or a merge request comes up while other tasks are open, run \
check_merge_conflicts. If branches conflict, tell the owner which ones and in which files, and recommend a \
merge order with ask_owner.\n\
- Once the first of two conflicting branches is in the default branch, use post_in_thread in the other \
task's thread to @mention its engineer: rebase onto `origin/HEAD` (refreshed from the project every time \
they're asked), resolve the conflicts, re-run the tests, and ask for review again. Don't @mention them \
anywhere else: outside the task's thread they'd work on another checkout.";

/// Where the merge request of the task in `conv` stands, if it has one.
fn merge_status(ws: &Workspace, conv: Conv) -> Option<String> {
    let card = (0..ws.len(conv)).rev().find_map(|i| match &ws.message(conv, i).card {
        Some(Card::Merge { outcome, .. }) => Some(outcome.clone()),
        _ => None,
    })?;
    Some(match card {
        None => "merge requested, waiting for the owner".to_string(),
        Some(MergeOutcome::Approved(_)) => "approved: the branch is in the project repo".to_string(),
        Some(MergeOutcome::Rejected) => "merge rejected".to_string(),
        Some(MergeOutcome::Failed(_)) => "merge request failed".to_string(),
    })
}

/// The task a conversation is, for naming its workspace: one per thread
/// (or channel / DM, for work asked for outside a thread).
pub fn task_key(ws: &Workspace, conv: Conv) -> String {
    let (prefix, name) = match conv {
        Conv::Thread(t) => (format!("t{t}"), ws.threads[t].name.as_str()),
        Conv::Channel(c) => match ws.channels[c].dm {
            Some(_) => ("dm".to_string(), ws.channels[c].name.as_str()),
            None => ("c".to_string(), ws.channels[c].name.as_str()),
        },
    };
    match sandbox::slug(name) {
        s if s.is_empty() => prefix,
        s => format!("{prefix}-{s}"),
    }
}

/// How a user is named to agents. The local user is literally called "you"
/// in the UI, which would read as the agent itself.
pub fn display_name(ws: &Workspace, user: usize) -> String {
    let name = &ws.users[user].name;
    if user == ws.me && name.eq_ignore_ascii_case("you") { "Owner".to_string() } else { name.clone() }
}

/// What the agent is told about where it is.
fn context(ws: &Workspace, conv: Conv, me: usize) -> String {
    let members = (0..ws.users.len())
        .filter(|&u| u != me)
        .map(|u| {
            let name = display_name(ws, u);
            match (u == ws.me, ws.users[u].kind) {
                (true, _) => format!("{name} (the project owner)"),
                (_, UserKind::Agent) => format!("{name} (AI agent)"),
                (_, UserKind::Bot) => format!("{name} (bot)"),
                _ => name,
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let place = match conv {
        Conv::Channel(c) => match ws.channels[c].dm {
            Some(_) => "your direct messages with the owner".to_string(),
            None => {
                let ch = &ws.channels[c];
                match ch.topic.trim() {
                    "" => format!("the #{} channel", ch.name),
                    topic => format!("the #{} channel (topic: {topic})", ch.name),
                }
            }
        },
        Conv::Thread(t) => {
            let th = &ws.threads[t];
            format!("the thread \"{}\" in #{}; its first message is the one it was started from", th.name, ws.channels[th.channel].name)
        }
    };
    format!(
        "# Where you are\n\
         You are {me_name}, a teammate in the \"{wsname}\" team chat. Other members: {members}.\n\
         This conversation is {place}.\n\
         Messages from others are shown as \"Name: text\". Write only your reply, with no name prefix; markdown is rendered. \
         @mention people by first name.\n\
         Today is {today}.",
        me_name = ws.users[me].name,
        wsname = ws.name,
        today = chrono::Local::now().format("%A, %B %-d, %Y"),
    )
}

/// The last `limit` messages of `conv` before `upto`, as alternating turns:
/// the agent's own messages are its turns, everyone else's are user turns.
pub fn history(ws: &Workspace, conv: Conv, me: usize, upto: usize, limit: usize) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for ix in upto.saturating_sub(limit)..upto {
        let msg = ws.message(conv, ix);
        let text = msg.source.trim();
        if msg.pending || text.is_empty() {
            continue;
        }
        let (role, text) = if msg.author == me {
            (Role::Assistant, text.to_string())
        } else {
            (Role::User, format!("{}: {text}", display_name(ws, msg.author)))
        };
        match turns.last_mut() {
            Some(last) if last.role == role => {
                if let Some(Part::Text(t)) = last.parts.last_mut() {
                    t.push_str("\n\n");
                    t.push_str(&text);
                }
            }
            _ => turns.push(Turn { role, parts: vec![Part::Text(text)] }),
        }
    }
    // Both APIs want the conversation to open with a user turn.
    while turns.first().is_some_and(|t| t.role == Role::Assistant) {
        turns.remove(0);
    }
    turns
}

pub mod tools {
    use super::*;

    /// The tool an agent calls to post nothing.
    pub const PASS: &str = "pass";
    /// The tool that opens a task thread in a channel; it changes the
    /// workspace, so the host runs it with [`start_task`], not [`run`].
    pub const START_TASK: &str = "start_task";
    pub const MAX_TITLE: usize = 60;

    pub fn start_task_spec() -> ToolSpec {
        ToolSpec {
            name: START_TASK,
            description: "Start a task: open a new thread in a topic channel and post the brief there, @mentioning \
                          who should do it. Work in a thread happens in its channel's repository (list_channels \
                          shows which), on a branch of its own, so pick the channel the work belongs to. One call \
                          per task. The brief is all the assignee sees, so make it self-contained: the goal, \
                          what done looks like, and any decisions already made.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "channel": {"type": "string", "description": "Channel name, without the #."},
                    "title": {"type": "string", "description": "Thread name: a few words saying what the task is."},
                    "brief": {"type": "string", "description": "The task, in markdown. Don't @mention the assignee here; `assignee` does that."},
                    "assignee": {"type": "string", "description": "Name of the teammate to @mention, e.g. Engineer."}
                },
                "required": ["channel", "title", "brief"],
                "additionalProperties": false
            }),
        }
    }

    /// Where a `start_task` call put the task.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Started {
        pub channel: usize,
        /// The thread's starter message in the channel.
        pub starter: usize,
        pub thread: usize,
        /// The brief's index in the thread: post-processing (who answers) starts there.
        pub brief: usize,
        /// The tool result for the agent.
        pub result: String,
    }

    /// Run `start_task` for `author`: a starter message in the channel with
    /// a thread on it, and the brief as the thread's first reply.
    pub fn start_task(ws: &mut Workspace, author: usize, input: &Value) -> Result<Started, String> {
        let name = input["channel"].as_str().ok_or("`channel` is required.")?;
        let channel = ws.find_channel(name).ok_or_else(|| {
            let names = ws.channels.iter().filter(|c| c.dm.is_none()).map(|c| format!("#{}", c.name)).collect::<Vec<_>>();
            format!("No channel named {name}. Channels: {}", names.join(", "))
        })?;
        let title = input["title"].as_str().map(str::trim).filter(|t| !t.is_empty()).ok_or("`title` is required.")?;
        let title: String = match title.chars().count() > MAX_TITLE {
            true => format!("{}…", title.chars().take(MAX_TITLE).collect::<String>().trim_end()),
            false => title.to_string(),
        };
        let brief = input["brief"].as_str().map(str::trim).filter(|b| !b.is_empty()).ok_or("`brief` is required.")?;
        let assignee = match input["assignee"].as_str().map(|a| a.trim().trim_start_matches('@')).filter(|a| !a.is_empty()) {
            Some(a) => {
                let u = (0..ws.users.len()).find(|&u| u != author && ws.users[u].answers_to(a)).ok_or_else(|| {
                    let names = (0..ws.users.len()).filter(|&u| u != author && u != ws.me).map(|u| ws.users[u].name.clone());
                    format!("No teammate named {a}. Teammates: {}", names.collect::<Vec<_>>().join(", "))
                })?;
                Some((a.to_string(), u))
            }
            None => None,
        };
        let text = match &assignee {
            Some((a, _)) => format!("@{a} {brief}"),
            None => brief.to_string(),
        };
        let (starter, thread) = ws.new_thread(channel, author, &title);
        let brief = ws.post(Conv::Thread(thread), author, &text);
        let channel_name = &ws.channels[channel].name;
        let result = match assignee {
            Some((_, u)) => format!(
                "Started thread “{title}” in #{channel_name}; {} has the brief there. Follow it with read_channel.",
                ws.users[u].name
            ),
            None => format!("Started thread “{title}” in #{channel_name}. Nobody was @mentioned, so nobody has started on it."),
        };
        Ok(Started { channel, starter, thread, brief, result })
    }

    /// The tool that ends a task without a merge request; the host runs it
    /// (it wakes the task's coordinator), not [`run`].
    pub const TASK_DONE: &str = "task_done";

    pub fn task_done_spec() -> ToolSpec {
        ToolSpec {
            name: TASK_DONE,
            description: "Say this thread's task has ended without a merge request: you answered the question, \
                          found there is nothing to change, or can't go on. PM is told and reports it to the \
                          owner. Not after request_merge or ask_owner, which reach PM already. Your reply here \
                          goes on as usual.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["done", "stuck"], "description": "done: finished; stuck: can't go on without help."},
                    "summary": {"type": "string", "description": "What came out of it, or what is in the way: a few sentences, in markdown."}
                },
                "required": ["status", "summary"],
                "additionalProperties": false
            }),
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum DoneStatus {
        Done,
        Stuck,
    }

    /// A `task_done` call.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Done {
        pub status: DoneStatus,
        pub summary: String,
    }

    pub fn task_done(input: &Value) -> Result<Done, String> {
        let status = match input["status"].as_str().map(str::trim) {
            Some("done") => DoneStatus::Done,
            Some("stuck") => DoneStatus::Stuck,
            _ => return Err("`status` must be \"done\" or \"stuck\".".into()),
        };
        let summary = input["summary"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("`summary` is required.")?;
        Ok(Done { status, summary: summary.to_string() })
    }

    /// The tool that adds to the workspace's decision log; the host runs it
    /// with [`record_decision`].
    pub const RECORD_DECISION: &str = "record_decision";
    /// The longest decision recorded, in characters.
    pub const MAX_DECISION: usize = 300;

    pub fn record_decision_spec() -> ToolSpec {
        ToolSpec {
            name: RECORD_DECISION,
            description: "Add a decision the owner made to the team's decision log, which every teammate sees in \
                          every conversation from now on. Record choices, constraints, conventions, dates and \
                          priorities meant to hold beyond this conversation, as soon as the owner settles them. \
                          Not tasks, status, or your own suggestions the owner hasn't agreed to. When it changes \
                          an earlier decision, pass that decision's id in `replaces`; to withdraw one with nothing \
                          in its place, pass only `replaces`. Your reply goes on as usual.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "decision": {"type": "string", "description": "One self-contained line that makes sense without this conversation: what was decided, and why if it matters. In the owner's language."},
                    "replaces": {"type": "integer", "minimum": 0, "description": "Id of the decision this one changes or withdraws."}
                },
                "additionalProperties": false
            }),
        }
    }

    /// What a `record_decision` call did.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Recorded {
        /// The new decision's id, unless one was only withdrawn.
        pub id: Option<usize>,
        /// A line for the owner, under the agent's reply.
        pub shown: String,
        /// The tool result for the agent.
        pub result: String,
    }

    /// Run `record_decision` for `author`, decided in `conv`.
    pub fn record_decision(ws: &mut Workspace, author: usize, conv: Option<Conv>, input: &Value) -> Result<Recorded, String> {
        let text = input["decision"].as_str().map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "));
        let text = text.filter(|t| !t.is_empty());
        let replaces = match &input["replaces"] {
            Value::Null => None,
            v => {
                let id = v.as_u64().ok_or("`replaces` must be a decision id.")? as usize;
                match ws.decisions.get(id) {
                    None => return Err(format!("There is no decision {id}.")),
                    Some(d) if d.retired.is_some() => return Err(format!("Decision {id} no longer holds already.")),
                    Some(_) => Some(id),
                }
            }
        };
        let Some(text) = text else {
            let id = replaces.ok_or("Pass `decision`, `replaces`, or both.")?;
            ws.decisions[id].retired = Some(Retired::Withdrawn);
            return Ok(Recorded {
                id: None,
                shown: format!("Withdrew decision {id}: {}", ws.decisions[id].text),
                result: format!("Withdrew decision {id}."),
            });
        };
        if text.chars().count() > MAX_DECISION {
            return Err(format!("Keep a decision to one line of at most {MAX_DECISION} characters."));
        }
        let id = ws.decisions.len();
        ws.decisions.push(Decision {
            text: text.clone(),
            author,
            ts: crate::data::now(),
            place: conv.map(|c| decision_place(ws, c)).unwrap_or_default(),
            retired: None,
        });
        if let Some(old) = replaces {
            ws.decisions[old].retired = Some(Retired::ReplacedBy(id));
        }
        let (shown, result) = match replaces {
            Some(old) => (format!("Decision {id} (replaces {old}): {text}"), format!("Recorded decision {id}; decision {old} no longer holds.")),
            None => (format!("Decision {id}: {text}"), format!("Recorded decision {id}.")),
        };
        Ok(Recorded { id: Some(id), shown, result })
    }

    /// The tool that posts into an existing task thread; like
    /// [`START_TASK`], the host runs it with [`post_in_thread`].
    pub const POST_IN_THREAD: &str = "post_in_thread";

    pub fn post_in_thread_spec() -> ToolSpec {
        ToolSpec {
            name: POST_IN_THREAD,
            description: "Post a message in an existing thread, as yourself, to pass something on to the people \
                          working there: @mention who should act on it (e.g. @Engineer), and they pick it up in \
                          that thread's task, on its branch. Use it to coordinate tasks, e.g. to have an engineer \
                          rebase after another task was merged. Your reply here goes on as usual.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "channel": {"type": "string", "description": "The thread's channel name, without the #."},
                    "thread": {"type": "string", "description": "The thread's name."},
                    "text": {"type": "string", "description": "The message, in markdown."}
                },
                "required": ["channel", "thread", "text"],
                "additionalProperties": false
            }),
        }
    }

    /// Where a `post_in_thread` call put its message.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Posted {
        pub thread: usize,
        /// The message's index in the thread.
        pub ix: usize,
        /// The tool result for the agent.
        pub result: String,
    }

    /// Run `post_in_thread` for `author`.
    pub fn post_in_thread(ws: &mut Workspace, author: usize, input: &Value) -> Result<Posted, String> {
        let c = channel(ws, input)?;
        let name = input["thread"].as_str().ok_or("`thread` is required.")?;
        let thread = find_thread(ws, c, name)?;
        let text = input["text"].as_str().map(str::trim).filter(|t| !t.is_empty()).ok_or("`text` is required.")?;
        let ix = ws.post(Conv::Thread(thread), author, text);
        let result = format!(
            "Posted in thread “{}” in #{}. Whoever you @mentioned picks it up there; follow it with read_channel.",
            ws.threads[thread].name, ws.channels[c].name
        );
        Ok(Posted { thread, ix, result })
    }

    /// The channel `input["channel"]` names.
    pub(crate) fn channel(ws: &Workspace, input: &Value) -> Result<usize, String> {
        let name = input["channel"].as_str().ok_or("`channel` is required.")?;
        ws.find_channel(name).ok_or_else(|| {
            let names = ws.channels.iter().filter(|c| c.dm.is_none()).map(|c| format!("#{}", c.name)).collect::<Vec<_>>();
            format!("No channel named {name}. Channels: {}", names.join(", "))
        })
    }

    /// Channel `c`'s thread called `name`.
    fn find_thread(ws: &Workspace, c: usize, name: &str) -> Result<usize, String> {
        ws.channel_threads(c)
            .into_iter()
            .find(|&t| ws.threads[t].name.eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| format!("#{} has no thread named “{name}”.", ws.channels[c].name))
    }

    pub fn specs() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: PASS,
                description: "Post nothing this time. Call it, instead of replying, when a message reached you but \
                              you have nothing useful to add yet: the work you'd look at isn't ready, someone else \
                              is already on it, or it wasn't really for you. Your reply ends and nothing you wrote \
                              is posted; you'll be asked again when someone @mentions you. Don't use it when the \
                              owner asks you something directly.",
                input_schema: json!({
                    "type": "object",
                    "properties": {"reason": {"type": "string", "description": "A few words on why, for the log."}},
                    "additionalProperties": false
                }),
            },
            ToolSpec {
                name: "list_channels",
                description: "List the workspace's channels with their topics, message counts and threads.",
                input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            },
            ToolSpec {
                name: "read_channel",
                description: "Read the most recent messages of a channel, or of one of its threads. \
                              Use it to catch up before summarizing or answering about other conversations.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "channel": {"type": "string", "description": "Channel name, without the #."},
                        "thread": {"type": "string", "description": "Optional thread name in that channel, to read the thread instead."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "How many recent messages (default 30)."}
                    },
                    "required": ["channel"],
                    "additionalProperties": false
                }),
            },
            ToolSpec {
                name: "ask_owner",
                description: "Ask the project owner to decide, with 2-4 short options shown as buttons under your \
                              message. Explain the trade-offs in your message text. After calling it, stop: your \
                              reply ends there, and their choice arrives as a new message. Use it for decisions \
                              that are theirs (scope, priorities, trade-offs), not for things you can find out.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "question": {"type": "string", "description": "One short line."},
                        "options": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 4, "description": "A few words each."},
                        "recommended": {"type": "integer", "minimum": 0, "description": "Index of the option you recommend."}
                    },
                    "required": ["question", "options"],
                    "additionalProperties": false
                }),
            },
        ]
        .into_iter()
        .chain(crate::artifact_tools::specs())
        .collect()
    }

    /// The card an `ask_owner` call asks for.
    pub fn choice_card(input: &Value) -> Result<Card, String> {
        let question = input["question"].as_str().map(str::trim).filter(|q| !q.is_empty()).ok_or("`question` is required.")?;
        let options: Vec<String> = input["options"]
            .as_array()
            .ok_or("`options` is required.")?
            .iter()
            .filter_map(|o| o.as_str().map(|s| s.trim().to_string()))
            .filter(|o| !o.is_empty())
            .collect();
        if !(2..=4).contains(&options.len()) {
            return Err("Give 2 to 4 options.".into());
        }
        let recommended = input["recommended"].as_u64().map(|i| i as usize).filter(|&i| i < options.len());
        Ok(Card::Choice { question: question.to_string(), options, recommended, answer: None })
    }

    /// What the UI shows while a tool runs.
    pub fn activity(name: &str, input: &Value) -> String {
        if let Some(text) = crate::artifact_tools::activity(name, input) {
            return text;
        }
        match (name, input["channel"].as_str()) {
            ("read_channel", Some(c)) => match input["thread"].as_str() {
                Some(t) => format!("Reading thread “{t}” in #{c}…"),
                None => format!("Reading #{}…", c.trim_start_matches('#')),
            },
            ("list_channels", _) => "Looking at channels…".to_string(),
            ("start_task", Some(c)) => format!("Starting a task in #{}…", c.trim_start_matches('#')),
            ("ask_owner", _) => "Asking you…".to_string(),
            ("request_merge", _) => "Preparing a merge request…".to_string(),
            ("post_in_thread", Some(c)) => match input["thread"].as_str() {
                Some(t) => format!("Posting in thread “{t}” in #{}…", c.trim_start_matches('#')),
                None => "Posting in a thread…".to_string(),
            },
            (sandbox::CHECK_MERGE_CONFLICTS, _) => "Checking how the task branches merge…".to_string(),
            _ => format!("Using {name}…"),
        }
    }

    /// Run a tool; the result text, and whether it failed.
    pub fn run(ws: &Workspace, name: &str, input: &Value) -> (String, bool) {
        let result = match name {
            "list_channels" => Ok(list_channels(ws)),
            "read_channel" => read_channel(ws, input),
            crate::artifact_tools::LIST_ARTIFACTS | crate::artifact_tools::READ_ARTIFACT => {
                crate::artifact_tools::read(ws, name, input)
            }
            _ => Err(format!("There is no tool named {name}.")),
        };
        match result {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        }
    }

    fn list_channels(ws: &Workspace) -> String {
        let mut out = String::new();
        for (c, ch) in ws.channels.iter().enumerate().filter(|(_, c)| c.dm.is_none()) {
            out.push_str(&format!("#{} ({} messages)", ch.name, ch.messages.len()));
            if let Some(repo) = &ch.repo {
                out.push_str(&format!(" [repo {repo}]"));
            }
            if !ch.topic.is_empty() {
                out.push_str(&format!(" — {}", ch.topic));
            }
            out.push('\n');
            for t in ws.channel_threads(c) {
                out.push_str(&format!("  thread “{}” ({} replies)\n", ws.threads[t].name, ws.threads[t].messages.len()));
            }
            for id in ws.channel_artifacts(c) {
                let a = ws.artifact(id).expect("listed");
                out.push_str(&format!("  {} {id} “{}”\n", a.kind.label().to_lowercase(), a.title));
            }
        }
        out
    }

    fn read_channel(ws: &Workspace, input: &Value) -> Result<String, String> {
        let c = channel(ws, input)?;
        let limit = input["limit"].as_u64().unwrap_or(30).clamp(1, 100) as usize;
        let conv = match input["thread"].as_str() {
            Some(thread) => Conv::Thread(find_thread(ws, c, thread)?),
            None => Conv::Channel(c),
        };
        let len = ws.len(conv);
        let mut out = String::new();
        if len > limit {
            out.push_str(&format!("({} earlier messages not shown)\n", len - limit));
        }
        for ix in len.saturating_sub(limit)..len {
            let msg = ws.message(conv, ix);
            let when = chrono::TimeZone::timestamp_opt(&chrono::Local, msg.ts, 0)
                .single()
                .map(|t| t.format("%a %-I:%M %p").to_string())
                .unwrap_or_default();
            let text = match msg.pending {
                true => "(still writing this reply; it isn't finished yet)",
                false => msg.source.trim(),
            };
            out.push_str(&format!("[{when}] {}: {text}\n", display_name(ws, msg.author)));
            if let (Conv::Channel(_), Some(t)) = (conv, msg.thread) {
                out.push_str(&format!("  ↳ thread “{}” ({} replies)\n", ws.threads[t].name, ws.threads[t].messages.len()));
            }
        }
        if len == 0 {
            out.push_str("(no messages)\n");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data;

    const CONFIG: &str = "[providers.p]\napi = \"openai\"\nbase_url = \"http://localhost:1/v1\"\n\
                          [[agents]]\nname = \"PM\"\nprovider = \"p\"\nmodel = \"m\"\nsystem_prompt = \"Be useful.\"\n";

    fn setup() -> (Workspace, Agents) {
        let mut ws = data::seed();
        let agents = Agents::from_config(&mut ws, &Config::parse(CONFIG).unwrap());
        (ws, agents)
    }

    #[test]
    fn catalog_ids_come_from_name_host_and_api() {
        use crate::config::Api;
        assert_eq!(catalog_ids("OpenRouter", Api::Openai, "https://openrouter.ai/api/v1"), ["openrouter", "openai"]);
        assert_eq!(catalog_ids("work", Api::Anthropic, "https://api.anthropic.com"), ["work", "anthropic"]);
        assert_eq!(catalog_ids("local", Api::Openai, "http://localhost:11434/v1"), ["local", "localhost", "openai"]);
    }

    #[test]
    fn settings_changes_reach_agents_live() {
        let (_, mut agents) = setup();
        let changed = CONFIG.replace("model = \"m\"", "model = \"other\"").replace("localhost:1", "localhost:2");
        agents.apply_settings(&Config::parse(&changed).unwrap());
        assert_eq!(agents.list[0].config.model, "other");
        assert_eq!(agents.list[0].provider.base_url, "http://localhost:2/v1");
    }

    #[test]
    fn config_agents_become_users_with_a_dm() {
        let (mut ws, agents) = setup();
        let user = agents.list[0].user;
        assert_eq!(ws.users[user].kind, UserKind::Agent);
        assert!(ws.channels.iter().any(|c| c.dm == Some(user)));
        // Loading again reuses the same user and DM.
        let (users, channels) = (ws.users.len(), ws.channels.len());
        Agents::from_config(&mut ws, &Config::parse(CONFIG).unwrap());
        assert_eq!((ws.users.len(), ws.channels.len()), (users, channels));
    }

    const TEAM: &str = "[providers.p]\napi = \"openai\"\nbase_url = \"http://localhost:1/v1\"\n\
                        [project]\nrepo = \"/src/app\"\n[team]\nmax_agent_turns = 3\n\
                        [[agents]]\nname = \"PM\"\nprovider = \"p\"\nmodel = \"m\"\n\
                        [[agents]]\nname = \"Engineer\"\nprovider = \"p\"\nmodel = \"m\"\nworkspace = \"write\"\n\
                        [[agents]]\nname = \"Reviewer\"\nprovider = \"p\"\nmodel = \"m\"\nworkspace = \"review\"\n";

    fn team() -> (Workspace, Agents, [usize; 3]) {
        let mut ws = data::seed();
        let agents = Agents::from_config(&mut ws, &Config::parse(TEAM).unwrap());
        let users = [agents.list[0].user, agents.list[1].user, agents.list[2].user];
        (ws, agents, users)
    }

    fn who(t: Triggered) -> Vec<usize> {
        assert!(!t.paused);
        t.agents
    }

    #[test]
    fn people_reach_dms_mentions_and_the_last_agent_in_a_thread() {
        let (mut ws, agents, [pm, eng, _]) = team();
        let dm = Conv::Channel(ws.ensure_dm(pm));
        let general = Conv::Channel(ws.find_channel("general").unwrap());

        let ix = ws.post(dm, ws.me, "status?");
        assert_eq!(who(agents.triggered(&ws, dm, ix)), vec![0]);

        let ix = ws.post(general, ws.me, "no mention here");
        assert!(who(agents.triggered(&ws, general, ix)).is_empty());
        let ix = ws.post(general, ws.me, "@pm and @Engineer, can you look?");
        assert_eq!(who(agents.triggered(&ws, general, ix)), vec![0, 1]);
        let ix = ws.post(general, ws.me, "`@PM` in code doesn't count");
        assert!(who(agents.triggered(&ws, general, ix)).is_empty());

        // In a thread, an unaddressed reply goes to the agent that spoke last.
        let Conv::Channel(g) = general else { unreachable!() };
        let t = Conv::Thread(ws.create_thread(g, 0, "plan"));
        ws.post(t, pm, "Planning.");
        ws.post(t, eng, "Building.");
        let ix = ws.post(t, ws.me, "go on");
        assert_eq!(who(agents.triggered(&ws, t, ix)), vec![1]);
        let ix = ws.post(t, ws.me, "@PM what's next?");
        assert_eq!(who(agents.triggered(&ws, t, ix)), vec![0]);
    }

    #[test]
    fn agents_reach_only_who_they_mention_until_the_turn_limit() {
        let (mut ws, agents, [pm, eng, rev]) = team();
        let general = Conv::Channel(ws.find_channel("general").unwrap());
        let Conv::Channel(g) = general else { unreachable!() };
        let t = Conv::Thread(ws.create_thread(g, 0, "task"));

        ws.post(t, ws.me, "@Engineer build it");
        let ix = ws.post(t, eng, "Done, no mention.");
        assert!(who(agents.triggered(&ws, t, ix)).is_empty());
        let ix = ws.post(t, eng, "@Engineer @Reviewer please review");
        assert_eq!(who(agents.triggered(&ws, t, ix)), vec![2], "never itself");
        // Third agent message in a row: the limit (3) is reached.
        let ix = ws.post(t, rev, "@Engineer fix the test");
        assert_eq!(agents.triggered(&ws, t, ix), Triggered { paused: true, ..Triggered::default() });
        // A person speaking resets the count.
        ws.post(t, ws.me, "carry on");
        let ix = ws.post(t, rev, "@Engineer fix the test");
        assert_eq!(who(agents.triggered(&ws, t, ix)), vec![1]);
        // Nobody else reaches a bot-authored message; PM untouched.
        let _ = pm;
    }

    #[test]
    fn an_agent_already_replying_is_not_started_again() {
        let (mut ws, agents, [_, eng, _]) = team();
        let dm = Conv::Channel(ws.ensure_dm(eng));
        ws.post(dm, ws.me, "build it");
        let pending = ws.post(dm, eng, "");
        ws.message_mut(dm, pending).pending = true;
        let ix = ws.post(dm, ws.me, "also, add tests");
        assert!(who(agents.triggered(&ws, dm, ix)).is_empty());
    }

    #[test]
    fn reviewers_wait_while_the_writer_is_still_replying() {
        let (mut ws, agents, [_, eng, _]) = team();
        let g = ws.find_channel("general").unwrap();
        let t = Conv::Thread(ws.create_thread(g, 0, "task"));
        let pending = ws.post(t, eng, "");
        ws.message_mut(t, pending).pending = true;
        assert!(agents.writer_busy(&ws, t));

        // Asked at once, the reviewer waits; PM (no workspace) doesn't.
        let ix = ws.post(t, ws.me, "@Engineer @Reviewer @PM build it, review it, track it");
        let got = agents.triggered(&ws, t, ix);
        assert_eq!((got.agents, got.waiting), (vec![0], vec![2]));

        ws.message_mut(t, pending).pending = false;
        assert!(!agents.writer_busy(&ws, t));
        let ix = ws.post(t, ws.me, "@Reviewer now?");
        assert_eq!(who(agents.triggered(&ws, t, ix)), vec![2]);

        // Asked together with an idle writer, the reviewer still goes second.
        let ix = ws.post(t, ws.me, "@Reviewer @Engineer build and review");
        let got = agents.triggered(&ws, t, ix);
        assert_eq!((got.agents, got.waiting), (vec![1], vec![2]));
    }

    const CHANNELS: &str = "[providers.p]\napi = \"openai\"\nbase_url = \"http://localhost:1/v1\"\n\
                            [project]\nrepo = \"/src/app\"\n\
                            [[channels]]\nname = \"backend\"\ntopic = \"API\"\nrepo = \"/src/api\"\n\
                            [[channels]]\nname = \"engineering\"\n\
                            [[agents]]\nname = \"PM\"\nprovider = \"p\"\nmodel = \"m\"\n\
                            [[agents]]\nname = \"Engineer\"\nprovider = \"p\"\nmodel = \"m\"\nworkspace = \"write\"\n";

    #[test]
    fn channels_from_the_config_carry_their_repo() {
        let mut ws = data::seed();
        let topic = ws.channels[ws.find_channel("engineering").unwrap()].topic.clone();
        let agents = Agents::from_config(&mut ws, &Config::parse(CHANNELS).unwrap());
        let backend = ws.find_channel("backend").expect("created");
        assert_eq!((ws.channels[backend].repo.as_deref(), ws.channels[backend].topic.as_str()), (Some("/src/api"), "API"));
        let eng = ws.find_channel("engineering").unwrap();
        assert_eq!(ws.channels[eng].topic, topic, "no topic in the config: kept");

        // A thread works where its channel does; elsewhere, [project].
        let starter = ws.post(Conv::Channel(backend), ws.me, "login");
        let t = Conv::Thread(ws.create_thread(backend, starter, "Login"));
        assert_eq!(agents.repo(&ws, t), Some(PathBuf::from("/src/api")));
        assert_eq!(agents.workspace(1, &ws, t).unwrap().repo, PathBuf::from("/src/api"));
        assert_eq!(agents.repo(&ws, Conv::Channel(eng)), Some(PathBuf::from("/src/app")));
        let dm = Conv::Channel(ws.ensure_dm(agents.list[1].user));
        assert_eq!(agents.repo(&ws, dm), Some(PathBuf::from("/src/app")));
        let (out, _) = tools::run(&ws, "list_channels", &json!({}));
        assert!(out.contains("#backend (1 messages) [repo /src/api] — API"), "{out}");

        // Loading again doesn't duplicate. What the config doesn't say, the
        // app keeps: a repo set in the app, or one from a dropped entry.
        ws.channels[eng].repo = Some("/src/web".into());
        let channels = ws.channels.len();
        Agents::from_config(&mut ws, &Config::parse(&CHANNELS.replace("name = \"backend\"", "name = \"api\"")).unwrap());
        assert_eq!(ws.channels.len(), channels + 1);
        assert_eq!(ws.channels[backend].repo.as_deref(), Some("/src/api"));
        assert_eq!(ws.channels[eng].repo.as_deref(), Some("/src/web"));
        assert_eq!(agents.repo(&ws, Conv::Channel(eng)), Some(PathBuf::from("/src/web")));
    }

    #[test]
    fn start_task_opens_a_thread_and_reaches_the_assignee() {
        let (mut ws, agents, [pm, eng, _]) = team();
        let eng_channel = ws.find_channel("engineering").unwrap();
        let before = ws.channels[eng_channel].messages.len();
        let input = json!({"channel": "#Engineering", "title": " Login page ", "brief": "Add a login page.", "assignee": "@engineer"});
        let started = tools::start_task(&mut ws, pm, &input).unwrap();
        let _ = &agents;
        assert_eq!((started.channel, started.starter, started.brief), (eng_channel, before, 1));
        assert_eq!(ws.threads[started.thread].name, "Login page");
        let thread = Conv::Thread(started.thread);
        assert_eq!(ws.message(thread, 0).source, "**Login page**");
        assert_eq!(ws.message(thread, 1).source, "@engineer Add a login page.");
        assert!(started.result.contains("Engineer has the brief"), "{}", started.result);
        assert_eq!(who(agents.triggered(&ws, thread, started.brief)), vec![1]);
        let _ = eng;

        // Without an assignee nobody is triggered; long titles are cut.
        let long = "x".repeat(100);
        let started = tools::start_task(&mut ws, pm, &json!({"channel": "general", "title": long, "brief": "b"})).unwrap();
        assert!(who(agents.triggered(&ws, Conv::Thread(started.thread), started.brief)).is_empty());
        assert_eq!(ws.threads[started.thread].name.chars().count(), tools::MAX_TITLE + 1);

        // Bad calls change nothing.
        let (threads, before) = (ws.threads.len(), ws.channels[eng_channel].messages.len());
        let bad = [
            (json!({"channel": "nope", "title": "t", "brief": "b"}), "#engineering"),
            (json!({"channel": "engineering", "title": " ", "brief": "b"}), "`title`"),
            (json!({"channel": "engineering", "title": "t"}), "`brief`"),
            (json!({"channel": "engineering", "title": "t", "brief": "b", "assignee": "Nobody"}), "Teammates: "),
            (json!({"channel": "engineering", "title": "t", "brief": "b", "assignee": "PM"}), "No teammate named PM"),
        ];
        for (input, says) in bad {
            let e = tools::start_task(&mut ws, pm, &input).unwrap_err();
            assert!(e.contains(says), "{e}");
        }
        assert_eq!((ws.threads.len(), ws.channels[eng_channel].messages.len()), (threads, before));
    }

    #[test]
    fn a_note_from_the_app_comes_last() {
        let mut request = Request { model: "m".into(), system: String::new(), turns: vec![], tools: vec![], max_tokens: None, extra: None };
        add_note(&mut request, "first");
        assert_eq!(request.turns, vec![Turn::user("first")]);
        add_note(&mut request, "second");
        assert_eq!(request.turns, vec![Turn::user("first\n\nsecond")]);
        request.turns.push(Turn::assistant("ok"));
        add_note(&mut request, "third");
        assert_eq!(request.turns, vec![Turn::user("first\n\nsecond"), Turn::assistant("ok"), Turn::user("third")]);

    }

    #[test]
    fn merge_notes_ask_for_a_report_and_coordinate_conflicts() {
        let commits: Vec<String> = (0..12).map(|i| format!("abc{i} change {i}")).collect();
        let place = "thread “A” in #backend";
        let note = merge_request_note("Reviewer", "pots/t1-a", place, " Adds A. ", &commits[..1], None);
        assert!(note.starts_with("Pots (the app, not the owner): Reviewer just asked the owner to merge `pots/t1-a`"));
        assert!(note.contains("Their summary: Adds A.\nCommits:\n- abc0 change 0\nReport it to the owner here"), "{note}");
        assert!(!note.contains("ask_owner"));

        let note = merge_request_note("Reviewer", "pots/t1-a", place, "s", &commits, Some("- Conflicts with x."));
        assert!(note.contains("- abc9 change 9\n- …and 2 more\n"), "{note}");
        assert!(note.contains("- Conflicts with x.\nCoordinate it"));
        assert!(!note.contains("Report it to the owner here"));

        let done = |input| tools::task_done(&input);
        let ok = done(json!({"status": "done", "summary": " Found 3 callers. "})).unwrap();
        assert_eq!(ok.summary, "Found 3 callers.");
        let note = task_done_note("Engineer", place, &ok);
        assert!(note.contains("Engineer finished the task without a merge request, in thread “A” in #backend"), "{note}");
        assert!(note.contains("Their summary: Found 3 callers.\nReport it"));
        let stuck = done(json!({"status": "stuck", "summary": "No docker."})).unwrap();
        assert!(task_done_note("Engineer", place, &stuck).contains("is stuck on the task"));
        assert!(done(json!({"status": "maybe", "summary": "x"})).unwrap_err().contains("`status`"));
        assert!(done(json!({"status": "done", "summary": " "})).unwrap_err().contains("`summary`"));

        let note = owner_question_note("Engineer", place, " Which DB? ", &["Postgres".into(), "SQLite".into()]);
        assert!(note.contains("waiting for the answer: “Which DB?” Options: Postgres / SQLite."), "{note}");

        let note = merge_approved_note("pots/t1-a", place);
        assert!(note.contains("accepted the merge request for `pots/t1-a` from thread “A” in #backend"));
        assert!(note.contains("with what is still open"));
    }

    #[test]
    fn task_notes_go_to_the_coordinator_that_started_it_in_general() {
        let mut ws = data::seed();
        let two = format!("{TEAM}[[agents]]\nname = \"PM2\"\nprovider = \"p\"\nmodel = \"m\"\n");
        let agents = Agents::from_config(&mut ws, &Config::parse(&two).unwrap());
        let (pm, eng) = (agents.list[0].user, agents.list[1].user);
        let eng_channel = ws.find_channel("engineering").unwrap();
        let general = ws.find_channel("general").unwrap();
        let input = json!({"channel": "engineering", "title": "Login", "brief": "b", "assignee": "Engineer"});
        let started = tools::start_task(&mut ws, pm, &input).unwrap();
        let thread = Conv::Thread(started.thread);
        assert_eq!(task_place(&ws, thread), "thread “Login” in #engineering");
        assert_eq!(report_conv(&ws, thread), Conv::Channel(general));
        assert_eq!(agents.coordinators(&ws, thread), [0], "only the one that started it");

        // Threads someone else started reach every coordinator.
        let (_, t) = ws.new_thread(eng_channel, eng, "by hand");
        assert_eq!(agents.coordinators(&ws, Conv::Thread(t)), [0, 3]);

        // Without #general, the report goes to the task's channel.
        ws.channels[general].name = "lobby".into();
        assert_eq!(report_conv(&ws, thread), Conv::Channel(eng_channel));
        assert_eq!(task_place(&ws, Conv::Channel(eng_channel)), "#engineering");
    }

    #[test]
    fn post_in_thread_reaches_whoever_it_mentions_there() {
        let (mut ws, agents, [pm, eng, _]) = team();
        let g = ws.find_channel("general").unwrap();
        let t = ws.create_thread(g, 0, "Login page");
        let input = json!({"channel": "#general", "thread": "login PAGE", "text": "@Engineer rebase onto origin/HEAD, please."});
        let posted = tools::post_in_thread(&mut ws, pm, &input).unwrap();
        assert_eq!(posted.thread, t);
        let conv = Conv::Thread(t);
        assert_eq!(ws.message(conv, posted.ix).author, pm);
        assert_eq!(agents.triggered(&ws, conv, posted.ix).agents, [agents.list.iter().position(|a| a.user == eng).unwrap()]);

        let missing = tools::post_in_thread(&mut ws, pm, &json!({"channel": "general", "thread": "Nope", "text": "x"}));
        assert!(missing.unwrap_err().contains("no thread named “Nope”"));
        let empty = tools::post_in_thread(&mut ws, pm, &json!({"channel": "general", "thread": "Login page", "text": " "}));
        assert!(empty.unwrap_err().contains("`text` is required"));
    }

    #[test]
    fn read_channel_marks_replies_still_being_written() {
        let (mut ws, _, [_, eng, _]) = team();
        let general = Conv::Channel(ws.find_channel("general").unwrap());
        let ix = ws.post(general, eng, "");
        ws.message_mut(general, ix).pending = true;
        let (out, _) = tools::run(&ws, "read_channel", &json!({"channel": "general", "limit": 1}));
        assert!(out.contains("Engineer: (still writing this reply"), "{out}");
    }

    #[test]
    fn workspace_agents_get_tools_and_a_task_per_thread() {
        let (mut ws, agents, _) = team();
        let g = ws.find_channel("general").unwrap();
        let t = ws.create_thread(g, 0, "Login page");
        let conv = Conv::Thread(t);
        let ix = ws.post(conv, ws.me, "@Engineer go");
        let names = |a: usize| agents.request(a, &ws, conv, ix + 1).tools.iter().map(|t| t.name).collect::<Vec<_>>();
        let chat = ["pass", "list_channels", "read_channel", "ask_owner", "list_artifacts", "read_artifact", "create_table", "update_table", "add_table_rows", "create_document", "update_document", "create_checklist", "update_checklist", "add_checklist_items", "check_items", "create_board", "update_board", "add_board_cards", "move_board_card", "create_whiteboard", "add_whiteboard_items", "update_whiteboard_items", "remove_whiteboard_items", "create_page", "update_page"];
        assert_eq!(
            names(0),
            [&chat[..], &["start_task", "post_in_thread", "check_merge_conflicts", "record_decision"]].concat(),
            "only coordinators start tasks"
        );
        assert!(agents.request(0, &ws, conv, ix + 1).system.contains("# Tasks side by side"));
        assert!(!agents.request(1, &ws, conv, ix + 1).system.contains("# Tasks side by side"));

        // The coordinator's check knows every task's branch, by repository.
        let check = agents.reply(0, &ws, conv, ix + 1).conflicts.expect("coordinators check conflicts");
        let (_, channels, branches) = check.repos.iter().find(|(_, _, b)| b.iter().any(|b| b.label.contains("Login page"))).unwrap();
        assert!(channels.contains(&"general".to_string()));
        let login = branches.iter().find(|b| b.label == "thread “Login page” in #general").unwrap();
        assert_eq!(login.branch, format!("pots/t{t}-login-page"));
        assert!(login.clone.ends_with(format!("tasks/t{t}-login-page/repo")));
        assert!(agents.reply(1, &ws, conv, ix + 1).conflicts.is_some(), "merge requests check too");
        let workspace = |rest: &[&'static str]| [&chat[..], &["task_done"], rest].concat();
        assert_eq!(names(1), workspace(&["request_merge", "bash", "read_file", "write_file", "edit_file"]));
        assert_eq!(names(2), workspace(&["request_merge", "bash", "read_file"]));
        assert!(agents.request(1, &ws, conv, ix + 1).system.contains("# Ending a task"));
        let top = Conv::Channel(g);
        let top_names = agents.request(1, &ws, top, ws.len(top)).tools.iter().map(|t| t.name).collect::<Vec<_>>();
        assert!(!top_names.contains(&"task_done"), "only in a task thread");

        let env = agents.workspace(1, &ws, conv).unwrap();
        assert_eq!(env.branch, format!("pots/t{t}-login-page"));
        assert_eq!(env.image, "rust:latest");
        assert!(agents.request(1, &ws, conv, ix + 1).system.contains("# Your workspace"));
        assert!(agents.workspace(0, &ws, conv).is_none());
        assert_eq!(agents.list[1].max_steps(), DEFAULT_MAX_STEPS_WORKSPACE);
    }

    #[test]
    fn history_alternates_roles_and_names_people() {
        let (mut ws, agents) = setup();
        let pm = agents.list[0].user;
        let dm = Conv::Channel(ws.ensure_dm(pm));
        ws.post(dm, pm, "Hi! I'm PM."); // leading agent turn is dropped
        ws.post(dm, ws.me, "first");
        ws.post(dm, ws.me, "second");
        ws.post(dm, pm, "ok");
        ws.post(dm, ws.me, "third");
        let placeholder = ws.post(dm, pm, "");
        ws.message_mut(dm, placeholder).pending = true;

        let req = agents.request(0, &ws, dm, placeholder);
        assert_eq!(
            req.turns,
            vec![Turn::user("Owner: first\n\nOwner: second"), Turn::assistant("ok"), Turn::user("Owner: third")]
        );
        assert!(req.system.starts_with("Be useful.\n\n# Where you are"));
        assert!(req.system.contains("your direct messages with the owner"));
        assert_eq!(req.tools.len(), 29);
    }

    #[test]
    fn ask_owner_makes_a_choice_card() {
        let card = tools::choice_card(&json!({"question": " Which DB? ", "options": ["SQLite", " Postgres "], "recommended": 1})).unwrap();
        assert_eq!(
            card,
            Card::Choice { question: "Which DB?".into(), options: vec!["SQLite".into(), "Postgres".into()], recommended: Some(1), answer: None }
        );
        assert!(tools::choice_card(&json!({"question": "q", "options": ["only one"]})).is_err());
        assert!(tools::choice_card(&json!({"options": ["a", "b"]})).is_err());
        // A recommendation out of range is dropped, not an error.
        let Card::Choice { recommended, .. } = tools::choice_card(&json!({"question": "q", "options": ["a", "b"], "recommended": 7})).unwrap() else {
            panic!()
        };
        assert_eq!(recommended, None);
    }

    #[test]
    fn read_channel_reads_channels_and_threads() {
        let ws = data::seed();
        let (out, err) = tools::run(&ws, "read_channel", &json!({"channel": "#engineering", "limit": 2}));
        assert!(!err);
        assert!(out.starts_with("(4 earlier messages not shown)\n"), "{out}");
        assert!(out.contains("Mara Chen: Nice work!"), "{out}");

        let (out, err) = tools::run(&ws, "read_channel", &json!({"channel": "engineering", "thread": "Search Index Migration"}));
        assert!(!err, "{out}");
        assert!(out.contains("Leo Okafor: `search-04`"), "{out}");

        let (out, err) = tools::run(&ws, "read_channel", &json!({"channel": "nope"}));
        assert!(err && out.contains("#general"), "{out}");

        let (out, _) = tools::run(&ws, "list_channels", &json!({}));
        assert!(out.contains("#engineering (6 messages)") && out.contains("thread “search index migration”"), "{out}");
    }

    #[test]
    fn decisions_are_recorded_replaced_and_withdrawn() {
        let (mut ws, _, [pm, _, _]) = team();
        let general = Conv::Channel(ws.find_channel("general").unwrap());
        let record = |ws: &mut Workspace, input: Value| tools::record_decision(ws, pm, Some(general), &input);

        let first = record(&mut ws, json!({"decision": "  Rate limit:\n 100 requests/min  "})).unwrap();
        assert_eq!(first.id, Some(0));
        assert_eq!(first.shown, "Decision 0: Rate limit: 100 requests/min");
        assert_eq!(ws.decisions[0].place, "#general");
        assert_eq!(ws.decisions[0].author, pm);

        let second = record(&mut ws, json!({"decision": "Rate limit: 60 requests/min", "replaces": 0})).unwrap();
        assert_eq!(second.id, Some(1));
        assert!(second.result.contains("decision 0 no longer holds"), "{}", second.result);
        assert_eq!(ws.decisions[0].retired, Some(Retired::ReplacedBy(1)));

        assert!(record(&mut ws, json!({"decision": "x", "replaces": 0})).unwrap_err().contains("no longer holds"));
        assert!(record(&mut ws, json!({"replaces": 9})).unwrap_err().contains("no decision 9"));
        assert!(record(&mut ws, json!({"decision": "  "})).is_err());
        assert!(record(&mut ws, json!({"decision": "x".repeat(tools::MAX_DECISION + 1)})).is_err());

        let withdrawn = record(&mut ws, json!({"replaces": 1})).unwrap();
        assert_eq!(withdrawn.id, None);
        assert_eq!(ws.decisions[1].retired, Some(Retired::Withdrawn));
        assert_eq!(ws.decisions.len(), 2, "nothing is ever removed");

        let dm = Conv::Channel(ws.ensure_dm(pm));
        tools::record_decision(&mut ws, pm, Some(dm), &json!({"decision": "Ship on Fridays"})).unwrap();
        assert_eq!(ws.decisions[2].place, "direct messages");
    }

    #[test]
    fn every_agent_sees_the_decisions_in_force_and_coordinators_keep_them() {
        let (mut ws, agents, [pm, _, _]) = team();
        let general = Conv::Channel(ws.find_channel("general").unwrap());
        let ix = ws.post(general, ws.me, "@PM @Engineer hi");
        let request = |ws: &Workspace, a: usize| agents.request(a, ws, general, ix + 1);
        assert!(request(&ws, 0).system.contains("# Decisions so far\n"));
        assert!(request(&ws, 0).system.ends_with("(none recorded yet)"));

        for input in [json!({"decision": "Use SQLite"}), json!({"decision": "Freeze auth until v0.5"}), json!({"decision": "Use Postgres", "replaces": 0})] {
            tools::record_decision(&mut ws, pm, Some(general), &input).unwrap();
        }
        let today = chrono::Local::now().format("%b %-d, %Y").to_string();
        for a in [0, 1] {
            let system = request(&ws, a).system;
            assert!(system.contains(&format!("- [1] ({today}, #general) Freeze auth until v0.5\n- [2] ({today}, #general) Use Postgres")), "{system}");
            assert!(!system.contains("Use SQLite"), "replaced decisions are left out");
        }
        let names = |a: usize| request(&ws, a).tools.iter().map(|t| t.name).collect::<Vec<_>>();
        assert!(names(0).contains(&tools::RECORD_DECISION));
        assert!(request(&ws, 0).system.contains("# The decision log"));
        assert!(!names(1).contains(&tools::RECORD_DECISION), "only coordinators record");
        assert!(!request(&ws, 1).system.contains("# The decision log"));
    }
}
