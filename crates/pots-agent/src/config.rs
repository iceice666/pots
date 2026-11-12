//! `config.toml`: LLM providers and the agents that use them.

use std::collections::BTreeMap;

use serde::Deserialize;

pub use pots_llm::{Api, Endpoint, ProviderConfig};

/// Written to the data directory on first run.
pub const DEFAULT: &str = r##"# Pots configuration.
#
# Providers speak one of two wire formats:
#   api = "anthropic"  POST {base_url}/v1/messages        (Anthropic Messages API)
#   api = "openai"     POST {base_url}/responses          (OpenAI Responses API)
#                   or POST {base_url}/chat/completions   (OpenAI Chat Completions)
# Any server compatible with either works (proxies, gateways, local models).
# For api = "openai", Pots tries /responses first and falls back to
# /chat/completions if the server doesn't have it; set endpoint = "responses"
# or endpoint = "chat" to skip the check.
# The key comes from `api_key`, then keys.toml next to this file (where
# Settings → Providers saves keys; only you can read it), then the
# environment variable `api_key_env`.

# OpenRouter: one key for models from many labs (https://openrouter.ai/keys).
# Model names are "lab/model", e.g. "anthropic/claude-opus-5.5".
[providers.openrouter]
api = "openai"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
endpoint = "chat"   # its Chat Completions API; its Responses API is stateless-only

[providers.anthropic]
api = "anthropic"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"

[providers.openai]
api = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

# Claude Code: your own `claude` CLI, on your Claude subscription instead of
# an API key. Agents without a workspace run it on this computer, logged in
# as you are. Agents with one run it in their container (installed there on
# first use), which can't see your login: run `claude setup-token` and paste
# the token into this provider in Settings → Providers. It uses its own
# tools (Bash, Read, Edit, …) in the container; max_steps caps its turns.
# Models: "claude-opus-5-5", "sonnet", …
# [providers.claude]
# api = "claude-code"
# command = "~/.local/bin/claude"   # only if Pots doesn't find it

# Example: a local OpenAI-compatible server.
# [providers.local]
# api = "openai"
# base_url = "http://localhost:11434/v1"

# Example: the fake model from `cargo run -p pots-mock`, for trying things
# out without a key. Point an agent's `provider` at it.
# [providers.mock]
# api = "anthropic"
# base_url = "http://127.0.0.1:4000"

# The git repository engineer/reviewer agents work on by default, and how
# their containers run. Each task (thread) gets its own clone under
# ~/.pots/tasks/, mounted at /workspace in a container of `image`.
# [project]
# repo = "~/code/my-project"
#
# Topic channels can each work on a repository of their own: tasks in a
# channel (its threads) clone the channel's `repo`; channels without one use
# [project]'s. Keep #general for talking to PM. Add channels in the app (the
# + next to Channels), or here: channels listed here are created if missing,
# and the repo/topic given here win over what's set in the app.
# [[channels]]
# name = "backend"
# topic = "API server"
# repo = "~/code/api"
#
# [sandbox]
# image = "rust:latest"   # needs git; add your build tools
# docker = "docker"       # path to the docker (or compatible) CLI
#
# [team]
# max_agent_turns = 6     # agent messages in a row before they pause for you

# Agents show up as teammates. Talk to one in its DM, @mention it in a
# channel, or reply in a thread to whoever spoke last. Agents can hand work
# to each other by @mentioning, and agents without a workspace (like PM) can
# open a task thread in a channel with the start_task tool. `name` is what
# you @mention.
#
# Optional per agent:
#   color = "#0f766e"
#   max_tokens = 16000      # Anthropic default 16000; OpenAI/OpenRouter: unset unless given
#   max_steps = 8           # model calls per reply (tool use loops)
#   history = 40            # recent messages it sees
#   workspace = "write"     # its own container + clone to change code in,
#                           # or "review": a fresh checkout of the work under review
#   image = "rust:latest"   # container image, overriding [sandbox]
#   price = { input = 3.0, output = 15.0 }  # $/million tokens, for cost display;
#                           # otherwise looked up on models.dev
#   [agents.extra_body]     # merged into every request body as-is,
#   output_config = { effort = "high" }   # e.g. Anthropic effort

[[agents]]
name = "PM"
provider = "openrouter"
model = "anthropic/claude-opus-5.5"
color = "#0f766e"
system_prompt = """
You are PM, the project manager on this team. The person you work with owns
the project: they make the decisions and set priorities; you do the legwork
so they don't have to.

- Turn what they decide into concrete next steps with owners.
- When something needs their decision, use the ask_owner tool: 2-4 short
  options and the one you recommend, with the why in your message. Don't
  decide for them.
- Catch up before you summarize: read the relevant channels with your tools
  instead of guessing what happened.
- Write like a teammate in chat: short, plain, no headings for a one-line
  answer. Use markdown lists or tables only when they make it clearer.
- Hand work that changes a repository out with start_task: one thread per
  task, in the channel whose repository it touches (list_channels shows
  each channel's repo), assigned to Engineer, with a self-contained brief.
  Then tell the owner, where they asked, which threads you started.
- Questions about what a repository has or how it works aren't tasks:
  look them up with ask_worker and answer yourself. If you have no worker,
  start a task whose brief says it is read-only: no new, edited or deleted
  files and no commits, with the answer in the thread.
- Reply in the language they write to you in.
"""

# Engineer and reviewer: work in their channel's repo (set in the app), or [project]'s.
#
# [[agents]]
# name = "Engineer"
# provider = "openrouter"
# model = "anthropic/claude-opus-5.5"
# workspace = "write"
# system_prompt = """
# You are Engineer. You work on tasks in your workspace. First decide what
# the task asks for:
# - A change: read the code, make focused changes, build and run the tests,
#   and commit with clear messages. When it's ready, @mention Reviewer in
#   the thread with a short summary of what changed and how you tested it.
# - Only information (a question, an investigation, an inventory): answer
#   in the thread. Don't create, edit or delete files, and don't commit.
# Limits in the brief ("don't modify anything", "read-only") cover every
# file, new ones included, and win over the rest of these instructions.
# Hand long look-ups (reading through code, running a command to see what it
# prints) to ask_worker. If the task is unclear or needs a decision, use
# ask_owner instead of guessing. Reply in the language of the task.
# """
#
# [[agents]]
# name = "Reviewer"
# provider = "openrouter"
# model = "anthropic/claude-opus-5.5"
# workspace = "review"
# system_prompt = """
# You are Reviewer. Review the engineer's branch: read the diff, build it,
# run the tests, and look for bugs, missing tests and unclear code. Reply
# with a verdict (approve / changes needed) and concrete findings. If changes
# are needed, @mention Engineer with them. If it's good, call request_merge
# so the owner can accept the branch. Don't fix the code yourself.
# Reply in the language of the thread.
# """
#
## Worker: a cheap model every agent can hand look-ups to with ask_worker
## (reading through code, summarizing a repository, running a command to
## see what it prints). It isn't a teammate in the chat: each ask starts
## fresh in a throwaway checkout of the task's committed work (or the
## project's default branch), and its report goes back to whoever asked.
## What it costs is added to that agent's reply. Set it up, change it or
## remove it under Settings → Agents too. Optional: system_prompt,
## max_steps (default 30), max_tokens, image, price, extra_body.
#
# [worker]
# provider = "openrouter"
# model = "deepseek/deepseek-v4.1-flash-1017"
"##;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    pub project: Option<ProjectConfig>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub team: TeamConfig,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    pub worker: Option<WorkerConfig>,
}

/// `[worker]`: the model agents hand look-ups to with `ask_worker`. It
/// isn't a chat member; each ask is a fresh, read-only run.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    pub provider: String,
    pub model: String,
    /// Empty for the built-in one.
    #[serde(default)]
    pub system_prompt: String,
    pub max_tokens: Option<u32>,
    pub max_steps: Option<u32>,
    pub image: Option<String>,
    /// $ per million tokens; otherwise looked up on models.dev.
    pub price: Option<pots_llm::cost::Price>,
    pub extra_body: Option<toml::Table>,
}

impl WorkerConfig {
    pub fn extra_body(&self) -> Option<serde_json::Value> {
        self.extra_body.as_ref().and_then(|t| serde_json::to_value(t).ok())
    }
}

/// A topic channel and the repository its tasks work on.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub name: String,
    pub topic: Option<String>,
    /// Overrides `[project] repo` for work asked for in this channel and its threads.
    pub repo: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// The git repository agents with a workspace clone. `~` is expanded.
    pub repo: String,
}

impl ProjectConfig {
    pub fn repo_path(&self) -> std::path::PathBuf {
        expand_home(&self.repo)
    }
}

/// `path` with a leading `~/` replaced by the home directory.
pub fn expand_home(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map_or_else(|| path.into(), |h| std::path::PathBuf::from(h).join(rest)),
        None => path.into(),
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// The docker CLI; found on PATH or in the usual install places if unset.
    pub docker: Option<String>,
    #[serde(default = "default_image")]
    pub image: String,
}

fn default_image() -> String {
    "rust:latest".to_string()
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self { docker: None, image: default_image() }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamConfig {
    /// Agent messages in a row (no person in between) after which agents
    /// stop @mentioning each other into replying.
    #[serde(default = "default_max_agent_turns")]
    pub max_agent_turns: usize,
}

fn default_max_agent_turns() -> usize {
    6
}

impl Default for TeamConfig {
    fn default() -> Self {
        Self { max_agent_turns: default_max_agent_turns() }
    }
}

/// What an agent's container is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceMode {
    /// Its own clone, on the task's branch, to change and commit.
    Write,
    /// A throwaway checkout of the task's branch, refreshed each reply.
    Review,
    /// The worker's: like `Review`, but with no work under review it looks
    /// at the project's default branch instead. Not set in config.toml.
    #[serde(skip)]
    Scratch,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub system_prompt: String,
    pub color: Option<String>,
    pub max_tokens: Option<u32>,
    pub max_steps: Option<u32>,
    pub history: Option<usize>,
    pub workspace: Option<WorkspaceMode>,
    pub image: Option<String>,
    /// $ per million tokens; known Claude models are priced already.
    pub price: Option<pots_llm::cost::Price>,
    pub extra_body: Option<toml::Table>,
}

impl AgentConfig {
    pub fn color(&self) -> Option<u32> {
        let hex = self.color.as_deref()?.trim_start_matches('#');
        u32::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 6)
    }

    pub fn extra_body(&self) -> Option<serde_json::Value> {
        self.extra_body.as_ref().and_then(|t| serde_json::to_value(t).ok())
    }
}

impl Config {
    /// Give providers their keys from keys.toml (provider name → key).
    pub fn attach_keys(&mut self, keys: &BTreeMap<String, String>) {
        for (name, pc) in &mut self.providers {
            pc.stored_key = keys.get(name).cloned();
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        for agent in &config.agents {
            if !config.providers.contains_key(&agent.provider) {
                return Err(format!("agent {:?} uses unknown provider {:?}", agent.name, agent.provider));
            }
            if agent.name.is_empty()
                || !agent.name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            {
                return Err(format!(
                    "agent name {:?} must be one word (letters, digits, _ or -) so it can be @mentioned",
                    agent.name
                ));
            }
        }
        if let Some(worker) = &config.worker
            && !config.providers.contains_key(&worker.provider)
        {
            return Err(format!("[worker] uses unknown provider {:?}", worker.provider));
        }
        for (i, channel) in config.channels.iter().enumerate() {
            if channel.name.is_empty() || !channel.name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                return Err(format!("channel name {:?} must be one word (letters, digits, _ or -)", channel.name));
            }
            if config.channels[..i].iter().any(|c| c.name.eq_ignore_ascii_case(&channel.name)) {
                return Err(format!("channel {:?} is listed twice", channel.name));
            }
            if channel.repo.as_deref().is_some_and(|r| r.trim().is_empty()) {
                return Err(format!("channel {:?} has an empty repo", channel.name));
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let config = Config::parse(DEFAULT).unwrap();
        assert_eq!(config.agents.len(), 1);
        let pm = &config.agents[0];
        assert_eq!(pm.name, "PM");
        assert_eq!(pm.provider, "openrouter");
        let openrouter = &config.providers[&pm.provider];
        assert_eq!((openrouter.api, openrouter.endpoint), (Api::Openai, Endpoint::Chat));
        assert_eq!(config.providers["openai"].endpoint, Endpoint::Auto);
        assert_eq!(pm.color(), Some(0x0f766e));
    }

    #[test]
    fn rejects_unknown_provider_and_unmentionable_names() {
        let base = "[providers.p]\napi = \"openai\"\nbase_url = \"http://x\"\n";
        let bad = format!("{base}[[agents]]\nname = \"A\"\nprovider = \"q\"\nmodel = \"m\"\n");
        assert!(Config::parse(&bad).unwrap_err().contains("unknown provider"));
        let bad = format!("{base}[[agents]]\nname = \"Two Words\"\nprovider = \"p\"\nmodel = \"m\"\n");
        assert!(Config::parse(&bad).unwrap_err().contains("one word"));
    }

    #[test]
    fn default_engineer_and_reviewer_parse_once_uncommented() {
        // Uncomment everything after the marker, as the comment tells people to.
        let (head, team) = DEFAULT.split_once("# Engineer and reviewer").unwrap();
        let team: String = team
            .lines()
            .skip(1)
            .map(|l| l.strip_prefix("# ").or(l.strip_prefix("#")).unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("{head}\n{team}\n");
        let config = Config::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        let names: Vec<_> = config.agents.iter().map(|a| (a.name.as_str(), a.workspace)).collect();
        assert_eq!(
            names,
            vec![("PM", None), ("Engineer", Some(WorkspaceMode::Write)), ("Reviewer", Some(WorkspaceMode::Review))]
        );
        assert!(config.project.is_none());
        assert_eq!(config.team.max_agent_turns, 6);
        let worker = config.worker.expect("the [worker] example is in the uncommented part");
        assert_eq!(worker.provider, "openrouter");
        assert!(worker.system_prompt.is_empty());
    }

    #[test]
    fn worker_needs_a_known_provider() {
        let base = "[providers.p]\napi = \"openai\"\nbase_url = \"http://x\"\n";
        let ok = format!("{base}[worker]\nprovider = \"p\"\nmodel = \"cheap\"\nmax_steps = 12\n");
        assert_eq!(Config::parse(&ok).unwrap().worker.unwrap().max_steps, Some(12));
        let bad = format!("{base}[worker]\nprovider = \"q\"\nmodel = \"cheap\"\n");
        assert!(Config::parse(&bad).unwrap_err().contains("[worker] uses unknown provider"));
    }

    #[test]
    fn workspace_agents_dont_need_a_project() {
        // Channels set up in the app can each have a repo of their own, so
        // config.toml alone can't tell whether workspace agents have one.
        let text = "[providers.p]\napi = \"openai\"\nbase_url = \"http://x\"\n\
                    [[agents]]\nname = \"E\"\nprovider = \"p\"\nmodel = \"m\"\nworkspace = \"write\"\n";
        assert!(Config::parse(text).unwrap().project.is_none());
    }

    #[test]
    fn channels_parse() {
        let base = "[providers.p]\napi = \"openai\"\nbase_url = \"http://x\"\n\
                    [[agents]]\nname = \"E\"\nprovider = \"p\"\nmodel = \"m\"\nworkspace = \"write\"\n";
        let text = format!("{base}[[channels]]\nname = \"backend\"\nrepo = \"~/code/api\"\n[[channels]]\nname = \"docs\"\n");
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.channels.len(), 2);
        assert!(expand_home(config.channels[0].repo.as_deref().unwrap()).ends_with("code/api"));
        assert_eq!(config.channels[1].repo, None);

        let twice = format!("{base}[[channels]]\nname = \"a\"\nrepo = \"/r\"\n[[channels]]\nname = \"A\"\n");
        assert!(Config::parse(&twice).unwrap_err().contains("listed twice"));
        let spaced = format!("{base}[[channels]]\nname = \"my channel\"\nrepo = \"/r\"\n");
        assert!(Config::parse(&spaced).unwrap_err().contains("one word"));
    }

    #[test]
    fn claude_code_needs_no_base_url() {
        let text = "[providers.claude]\napi = \"claude-code\"\ncommand = \"~/bin/claude\"\n\
                    [[agents]]\nname = \"E\"\nprovider = \"claude\"\nmodel = \"opus\"\nworkspace = \"write\"\n";
        let config = Config::parse(text).unwrap();
        let claude = &config.providers["claude"];
        assert_eq!((claude.api, claude.base_url.as_str()), (Api::ClaudeCode, ""));
        assert_eq!(claude.command.as_deref(), Some("~/bin/claude"));
    }

    #[test]
    fn extra_body_becomes_json() {
        let text = "[providers.p]\napi = \"anthropic\"\nbase_url = \"http://x\"\n\
                    [[agents]]\nname = \"A\"\nprovider = \"p\"\nmodel = \"m\"\n\
                    [agents.extra_body]\noutput_config = { effort = \"high\" }\n";
        let config = Config::parse(text).unwrap();
        assert_eq!(
            config.agents[0].extra_body(),
            Some(serde_json::json!({"output_config": {"effort": "high"}}))
        );
    }
}
