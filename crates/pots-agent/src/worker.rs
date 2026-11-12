//! The worker: a cheaper model agents hand look-ups to with `ask_worker`.
//!
//! It isn't a member of the chat. Each ask is a reply of its own, run to
//! its end inside the asking agent's reply: a fresh conversation holding
//! only the task, read-only tools, and a throwaway checkout. Its final text
//! goes back to the asking agent as the tool's result; what it did along
//! the way shows as that agent's activity, and what it cost is added to
//! that agent's reply.

use std::future::Future;
use std::pin::Pin;

use pots_llm::{Provider, Request, Turn, Usage};
use serde_json::{Value, json};

use crate::data::Card;
use crate::reply::{Host, Outcome, Reply};
use crate::sandbox;
use pots_llm::ToolSpec;

/// The tool agents call to ask the worker.
pub const ASK_WORKER: &str = "ask_worker";
/// What the worker is called in activities and in its prompt.
pub const NAME: &str = "Worker";
pub const DEFAULT_MAX_STEPS: u32 = 30;
/// Chat tools the worker may use; the rest (start_task, ask_owner, …) act.
const CHAT_TOOLS: [&str; 2] = ["list_channels", "read_channel"];

pub const DEFAULT_PROMPT: &str = "You are Worker, a helper that teammates hand look-ups to. Do the task you're \
given and reply with what it asks for: findings with file paths and line numbers, the commands you ran and what \
they printed. Be complete but brief: your reply is all the teammate sees, and they can't ask you follow-up \
questions. Look things up instead of guessing, and say plainly what you couldn't find out. Don't try to change \
the project. Reply in the language of the task.";

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: ASK_WORKER,
        description: "Hand a look-up to Worker, a cheaper model, and get its report back as this tool's result. \
                      Use it for work that only gathers information: reading through code, finding where something \
                      is defined or used, summarizing a repository, running a command or the tests to see what \
                      they print, catching up on channels. It starts fresh every time and can't see this \
                      conversation, so make the task self-contained and say what you want back. It works in a \
                      throwaway checkout of this task's latest commit (or the project's default branch): work that \
                      isn't committed isn't there. It can't change the project or talk to anyone.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "What to find out and what to report back, in markdown."},
                "channel": {"type": "string", "description": "Look at this channel's repository instead of this conversation's (name without the #)."}
            },
            "required": ["task"],
            "additionalProperties": false
        }),
    }
}

/// The worker tools a reply's agent can use: everything an ask needs,
/// ready before the reply starts.
#[derive(Clone)]
pub struct Job {
    pub provider: Provider,
    pub max_steps: u32,
    /// The worker's request, with no turns and no workspace description
    /// yet: each ask adds its task and where it works.
    pub request: Request,
    /// Whether the worker runs through Claude Code (with its own tools).
    pub claude_code: bool,
    /// Where it works by default: the asking conversation's repository.
    pub env: Option<sandbox::Env>,
    /// Where it works when asked about a channel, by lowercase name.
    pub channels: Vec<(String, sandbox::Env)>,
    /// Why the worker can't run (say, its provider has no key).
    pub problem: Option<String>,
    /// Tokens this reply's asks have used so far.
    pub spent: Usage,
}

impl Job {
    /// Run one ask to its end: the tool result text, and whether it failed.
    /// `Err` only when `host` is gone, which ends the asking reply too.
    pub fn ask<'a>(
        &'a mut self,
        input: &Value,
        host: &'a mut dyn Host,
    ) -> Pin<Box<dyn Future<Output = Result<(String, bool), String>> + 'a>> {
        let task = input["task"].as_str().map(str::trim).unwrap_or_default().to_string();
        let channel = input["channel"].as_str().map(|c| c.trim().trim_start_matches('#').to_lowercase());
        Box::pin(async move {
            if task.is_empty() {
                return Ok(("`task` is required.".to_string(), true));
            }
            if let Some(problem) = &self.problem {
                return Ok((format!("The worker can't run: {problem}"), true));
            }
            let env = match channel.filter(|c| !c.is_empty()) {
                None => self.env.clone(),
                Some(c) => match self.channels.iter().find(|(name, _)| *name == c) {
                    Some((_, env)) => Some(env.clone()),
                    None => {
                        let names: Vec<String> = self.channels.iter().map(|(n, _)| format!("#{n}")).collect();
                        return Ok((format!("#{c} has no repository. Channels with one: {}", names.join(", ")), true));
                    }
                },
            };
            let mut request = self.request.clone();
            if let Some(env) = &env {
                let describe = if self.claude_code { env.describe_claude_code() } else { env.describe() };
                request.system.push_str("\n\n");
                request.system.push_str(&describe);
                let workspace = sandbox::specs(env.mode).into_iter();
                request.tools.extend(workspace.filter(|t| !self.claude_code || !sandbox::is_tool(t.name)));
            }
            request.turns = vec![Turn::user(task)];
            let reply = Reply {
                provider: self.provider.clone(),
                max_steps: self.max_steps,
                request,
                session: env.map(sandbox::Session::new),
                worker: None,
                conflicts: None,
            };
            let mut worker = WorkerHost { parent: host, before: self.spent, used: Usage::default(), text: String::new(), gone: None };
            let outcome = reply.run(&mut worker).await;
            self.spent.add(worker.used);
            if let Some(gone) = worker.gone {
                return Err(gone);
            }
            let text = sandbox::truncate(worker.text.trim());
            Ok(match outcome {
                Ok(Outcome::Replied) if !text.is_empty() => (text, false),
                Ok(Outcome::Replied) => ("The worker finished without saying anything.".to_string(), true),
                Ok(Outcome::Passed) => ("The worker gave up without an answer.".to_string(), true),
                Err(e) if text.is_empty() => (format!("The worker stopped: {e}"), true),
                Err(e) => (format!("{text}\n\n(The worker stopped before finishing: {e})"), true),
            })
        })
    }
}

/// The asking reply's host, as the worker's reply sees it: its text is
/// kept for the tool result, its activity shows on the asking reply.
struct WorkerHost<'a> {
    parent: &'a mut dyn Host,
    /// The reply's worker usage before this ask.
    before: Usage,
    /// This ask's usage so far.
    used: Usage,
    text: String,
    /// Why the asking reply's host stopped listening, if it did.
    gone: Option<String>,
}

impl WorkerHost<'_> {
    fn check(&mut self, result: Result<(), String>) -> Result<(), String> {
        if let Err(e) = &result {
            self.gone = Some(e.clone());
        }
        result
    }
}

impl Host for WorkerHost<'_> {
    fn progress(&mut self, text: &str, activity: Option<String>) -> Result<(), String> {
        self.text.push_str(text);
        match activity {
            Some(a) => {
                let shown = self.parent.progress("", Some(format!("{NAME}: {a}")));
                self.check(shown)
            }
            None => Ok(()),
        }
    }

    fn usage(&mut self, total: Usage) -> Result<(), String> {
        self.used = total;
        let mut all = self.before;
        all.add(total);
        let shown = self.parent.worker_usage(all);
        self.check(shown)
    }

    /// The worker isn't offered card tools; a card it asks for anyway is dropped.
    fn card(&mut self, _card: Card) -> Result<(), String> {
        Ok(())
    }

    fn run_tool(&mut self, name: &str, input: &Value) -> Result<(String, bool), String> {
        if !CHAT_TOOLS.contains(&name) {
            return Ok((format!("There is no tool named {name}."), true));
        }
        let result = self.parent.run_tool(name, input);
        if let Err(e) = &result {
            self.gone = Some(e.clone());
        }
        result
    }

    fn worker_usage(&mut self, _total: Usage) -> Result<(), String> {
        Ok(())
    }
}

/// The chat tools the worker is offered.
pub fn chat_tools() -> Vec<ToolSpec> {
    crate::agent::tools::specs().into_iter().filter(|t| CHAT_TOOLS.contains(&t.name)).collect()
}
