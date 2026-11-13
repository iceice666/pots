//! The agent reply loop: stream the model's answer, run the tools it asks
//! for, and go again until it's done.
//!
//! It knows nothing about the UI. Whoever runs it passes a [`Host`], which
//! shows progress and owns the workspace that chat tools read.

use futures::StreamExt;
use serde_json::Value;

use pots_llm::{Api, Event, Part, Provider, Request, Role, Stop, Turn, Usage};

use crate::agent::tools;
use crate::artifact_tools;
use crate::data::Card;
use crate::sandbox;

/// Where a reply shows up. Each method returns `Err` once nobody is
/// listening any more (say, the app closed); the reply then ends with it.
pub trait Host {
    /// More reply text to append, and/or what the agent is doing now.
    fn progress(&mut self, text: &str, activity: Option<String>) -> Result<(), String>;
    /// Tokens used by the reply so far, after each model call.
    fn usage(&mut self, total: Usage) -> Result<(), String>;
    /// A card for the owner (a choice, a merge request) under the reply.
    fn card(&mut self, card: Card) -> Result<(), String>;
    /// Run a chat tool ([`tools::run`]) against the workspace: the result
    /// text, and whether it failed.
    fn run_tool(&mut self, name: &str, input: &Value) -> Result<(String, bool), String>;
    /// Tokens the reply's [worker](crate::worker) asks have used so far,
    /// priced at the worker's model rather than the agent's.
    fn worker_usage(&mut self, _total: Usage) -> Result<(), String> {
        Ok(())
    }
}

/// How a reply ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It said what it had to say.
    Replied,
    /// The agent chose to post nothing (the `pass` tool): drop its message.
    Passed,
}

/// One agent reply, ready to run.
pub struct Reply {
    pub provider: Provider,
    /// Model calls allowed before giving up.
    pub max_steps: u32,
    pub request: Request,
    /// The agent's container workspace, if it has one.
    pub session: Option<sandbox::Session>,
    /// What `ask_worker` runs, if a worker is configured.
    pub worker: Option<crate::worker::Job>,
    /// The tasks `check_merge_conflicts` (offered to agents that
    /// coordinate) and merge requests' conflict warnings look at.
    pub conflicts: Option<sandbox::ConflictCheck>,
}

impl Reply {
    /// Run the reply to its end. `Err` is what to show the owner instead of
    /// (or after) the text that came so far.
    pub async fn run(self, host: &mut impl Host) -> Result<Outcome, String> {
        if self.provider.api == Api::ClaudeCode {
            return crate::claude_code::run(self, host).await;
        }
        let Reply { provider, max_steps, request: mut req, mut session, mut worker, conflicts } = self;
        let mut usage = Usage::default();
        for step in 0..max_steps {
            let mut events = pots_llm::stream(&provider, &req);
            // Text from a later step starts a new paragraph.
            let mut separate = step > 0;
            let (turn, stop) = loop {
                let Some(first) = events.next().await else {
                    return Err("The connection closed unexpectedly.".into());
                };
                // Take everything that has arrived, and update the host once.
                let mut batch = vec![first];
                while let Ok(e) = events.try_recv() {
                    batch.push(e);
                }
                let (mut text, mut activity, mut done) = (String::new(), None, None);
                for event in batch {
                    match event {
                        Event::Text(t) => text.push_str(&t),
                        Event::ToolCall(name) => activity = Some(tools::activity(&name, &Value::Null)),
                        Event::Done(turn, stop, used) => {
                            usage.add(used);
                            done = Some((turn, stop));
                        }
                        Event::Error(e) => {
                            host.progress(&text, None)?;
                            return Err(e);
                        }
                    }
                }
                if separate && !text.is_empty() {
                    separate = false;
                    text.insert_str(0, "\n\n");
                }
                host.progress(&text, activity)?;
                if let Some(done) = done {
                    host.usage(usage)?;
                    break done;
                }
            };

            let calls: Vec<(String, String, Value)> =
                turn.tool_calls().map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone())).collect();
            req.turns.push(turn);
            match stop {
                Stop::MaxTokens => return Err("Cut off: the reply hit the max_tokens limit.".into()),
                Stop::Refusal => return Err("The model declined to answer.".into()),
                _ => {}
            }
            if calls.is_empty() {
                return Ok(Outcome::Replied);
            }
            if calls.iter().any(|(_, name, _)| name == tools::PASS) {
                return Ok(Outcome::Passed);
            }
            if step + 1 == max_steps {
                return Err(format!("Stopped after {max_steps} model calls (max_steps in config.toml)."));
            }
            // A card for the owner ends the reply: their answer is a new message.
            let mut asked = false;
            let mut results = Vec::new();
            for (call, name, input) in calls {
                let activity = sandbox::activity(&name, &input).unwrap_or_else(|| tools::activity(&name, &input));
                host.progress("", Some(activity))?;
                if name == crate::worker::ASK_WORKER {
                    let (content, is_error) = match worker.as_mut() {
                        Some(job) => job.ask(&input, host).await?,
                        None => ("There is no worker: add a [worker] to config.toml.".to_string(), true),
                    };
                    results.push(Part::ToolResult { id: call, content, is_error });
                    continue;
                }
                if name == artifact_tools::USE_ARTIFACT_TOOLS
                    && let Some(loaded) = artifact_tools::load(&mut req.tools, &input)
                {
                    let (content, is_error) = match loaded {
                        Ok(text) => (text, false),
                        Err(e) => (e, true),
                    };
                    results.push(Part::ToolResult { id: call, content, is_error });
                    continue;
                }
                artifact_tools::load_for(&mut req.tools, &name);
                if name == sandbox::CHECK_MERGE_CONFLICTS {
                    let (content, is_error) = check_conflicts(conflicts.clone(), input).await?;
                    results.push(Part::ToolResult { id: call, content, is_error });
                    continue;
                }
                let card = match name.as_str() {
                    "ask_owner" => Some(tools::choice_card(&input)),
                    "request_merge" => Some(match session.take() {
                        Some(mut s) => {
                            let (input, others) = (input.clone(), conflicts.clone());
                            let (card, s) = blocking(move || (s.merge_card(&input, others.as_ref()), s)).await?;
                            session = Some(s);
                            card
                        }
                        None => Err("You have no workspace, so there is no branch to merge.".into()),
                    }),
                    _ => None,
                };
                let (content, is_error) = match card {
                    Some(Ok(card)) => {
                        asked = true;
                        let shown = shown(&card);
                        host.card(card)?;
                        (shown, false)
                    }
                    Some(Err(e)) => (e, true),
                    None => match session.take() {
                        // Workspace tools run in the container, off the caller's thread.
                        Some(mut s) if sandbox::is_tool(&name) => {
                            let (result, s) = blocking(move || (s.run(&name, &input), s)).await?;
                            session = Some(s);
                            result
                        }
                        s => {
                            session = s;
                            host.run_tool(&name, &input)?
                        }
                    },
                };
                results.push(Part::ToolResult { id: call, content, is_error });
            }
            req.turns.push(Turn { role: Role::User, parts: results });
            if asked {
                return Ok(Outcome::Replied);
            }
        }
        Ok(Outcome::Replied)
    }
}

/// The tool result for a card shown to the owner.
pub fn shown(card: &Card) -> String {
    match card {
        Card::Merge { conflicts: Some(c), .. } => format!(
            "Shown to the owner, with a warning that the branch conflicts with other work:\n{c}\n\
             Their answer will arrive as a new message."
        ),
        _ => "Shown to the owner. Their answer will arrive as a new message.".to_string(),
    }
}

/// Run a `check_merge_conflicts` call off the caller's thread.
pub async fn check_conflicts(check: Option<sandbox::ConflictCheck>, input: Value) -> Result<(String, bool), String> {
    match check {
        Some(check) => blocking(move || check.run(&input)).await,
        None => Ok(("Only teammates who coordinate work can check merge conflicts.".to_string(), true)),
    }
}

/// Run `f` on a thread of its own (it may block for minutes) and wait for it.
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("agent-tool".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(|e| e.to_string())?;
    rx.await.map_err(|_| "The tool crashed.".to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use pots_llm::Endpoint;
    use pots_mock::{MockServer, Script};
    use serde_json::json;

    use super::*;
    use crate::data::{self, Workspace};

    /// A host that records what it was shown and runs chat tools on `ws`.
    pub(crate) struct Recorder {
        pub ws: Workspace,
        pub text: String,
        pub activities: Vec<String>,
        pub usage: Usage,
        pub worker_usage: Usage,
        pub cards: Vec<Card>,
    }

    impl Recorder {
        pub fn new() -> Self {
            Self { ws: data::seed(), text: String::new(), activities: vec![], usage: Usage::default(),
                   worker_usage: Usage::default(), cards: vec![] }
        }
    }

    impl Host for Recorder {
        fn progress(&mut self, text: &str, activity: Option<String>) -> Result<(), String> {
            self.text.push_str(text);
            self.activities.extend(activity);
            Ok(())
        }
        fn usage(&mut self, total: Usage) -> Result<(), String> {
            self.usage = total;
            Ok(())
        }
        fn card(&mut self, card: Card) -> Result<(), String> {
            self.cards.push(card);
            Ok(())
        }
        fn worker_usage(&mut self, total: Usage) -> Result<(), String> {
            self.worker_usage = total;
            Ok(())
        }
        fn run_tool(&mut self, name: &str, input: &Value) -> Result<(String, bool), String> {
            // As the app does: start_task changes the workspace, as user 1.
            Ok(match name {
                tools::START_TASK => match tools::start_task(&mut self.ws, 1, input) {
                    Ok(started) => (started.result, false),
                    Err(e) => (e, true),
                },
                tools::POST_IN_THREAD => match tools::post_in_thread(&mut self.ws, 1, input) {
                    Ok(posted) => (posted.result, false),
                    Err(e) => (e, true),
                },
                _ => tools::run(&self.ws, name, input),
            })
        }
    }

    fn reply(server: &MockServer, api: Api, max_steps: u32) -> Reply {
        let base_url = match api {
            Api::Anthropic => server.url(),
            Api::Openai => format!("{}/v1", server.url()),
            Api::ClaudeCode => unreachable!("tested in claude_code"),
        };
        let provider = Provider {
            api,
            base_url,
            key: None,
            headers: BTreeMap::new(),
            prompt_cache: true,
            stream_usage: true,
            endpoint: Endpoint::Auto,
            command: None,
        };
        let request = Request {
            model: "mock".into(),
            system: String::new(),
            turns: vec![Turn::user("Owner: what's going on?")],
            tools: tools::specs(),
            max_tokens: None,
            extra: None,
        };
        Reply { provider, max_steps, request, session: None, worker: None, conflicts: None }
    }

    /// Every wire format: Anthropic, OpenAI Responses (found by detection)
    /// and OpenAI Chat Completions.
    const WIRES: [(Api, Endpoint); 3] =
        [(Api::Anthropic, Endpoint::Auto), (Api::Openai, Endpoint::Auto), (Api::Openai, Endpoint::Chat)];

    fn reply_over(server: &MockServer, (api, endpoint): (Api, Endpoint), max_steps: u32) -> Reply {
        let mut r = reply(server, api, max_steps);
        r.provider.endpoint = endpoint;
        r
    }

    #[test]
    fn runs_a_tool_and_answers_with_its_result() {
        for api in WIRES {
            let server = MockServer::start(Script::new().tool("list_channels", json!({})).text("All quiet."));
            let mut host = Recorder::new();
            futures::executor::block_on(reply_over(&server, api, 8).run(&mut host)).unwrap();

            assert_eq!(host.text, "\n\nAll quiet.", "{api:?}: a later step starts a new paragraph");
            assert!(host.activities.contains(&"Looking at channels…".to_string()), "{api:?}: {:?}", host.activities);
            assert!(host.usage.output > 0, "{api:?}: usage is reported");
            let requests = server.requests();
            assert_eq!(requests.len(), 2, "{api:?}: one call, then one with the tool result");
            assert!(requests[1].body.to_string().contains("#general"), "{api:?}: the tool result went back to the model");
            let path = if api == (Api::Openai, Endpoint::Auto) { "/v1/responses" } else { requests[0].path.as_str() };
            assert!(requests.iter().all(|r| r.path == path), "{api:?}: {:?}", requests.iter().map(|r| &r.path).collect::<Vec<_>>());
        }
    }

    #[test]
    fn start_task_reaches_the_host_and_its_result_the_model() {
        let input = json!({"channel": "engineering", "title": "Login page", "brief": "Add one.", "assignee": "Leo"});
        let server = MockServer::start(Script::new().tool(tools::START_TASK, input).text("Started it."));
        let mut host = Recorder::new();
        let threads = host.ws.threads.len();
        let mut r = reply(&server, Api::Anthropic, 8);
        r.request.tools.push(tools::start_task_spec());
        futures::executor::block_on(r.run(&mut host)).unwrap();

        assert_eq!(host.ws.threads.len(), threads + 1);
        let t = host.ws.threads.last().unwrap();
        assert_eq!(t.name, "Login page");
        assert_eq!(t.messages[0].source, "@Leo Add one.");
        assert!(host.activities.contains(&"Starting a task in #engineering…".to_string()), "{:?}", host.activities);
        let requests = server.requests();
        assert!(requests[0].body.to_string().contains("\"start_task\""), "the tool was offered");
        assert!(requests[1].body.to_string().contains("Leo Okafor has the brief"), "the result went back to the model");
        assert_eq!(host.text, "\n\nStarted it.");
    }

    /// The names of the tools a recorded request offered, in any wire format.
    fn offered(request: &pots_mock::Recorded) -> Vec<String> {
        let tools = request.body["tools"].as_array().cloned().unwrap_or_default();
        tools.iter().filter_map(|t| t["name"].as_str().or(t["function"]["name"].as_str()).map(String::from)).collect()
    }

    #[test]
    fn artifact_writing_tools_come_when_asked_for() {
        for api in WIRES {
            let script = Script::new()
                .tool(artifact_tools::USE_ARTIFACT_TOOLS, json!({"kinds": ["table", "checklist"]}))
                .tool(artifact_tools::CREATE_TABLE, json!({"channel": "general", "title": "T", "columns": ["A"]}))
                .text("Done.");
            let server = MockServer::start(script);
            let mut host = Recorder::new();
            futures::executor::block_on(reply_over(&server, api, 8).run(&mut host)).unwrap();

            let requests = server.requests();
            assert_eq!(requests.len(), 3, "{api:?}");
            let first = offered(&requests[0]);
            assert!(first.iter().any(|t| t == artifact_tools::USE_ARTIFACT_TOOLS), "{api:?}: {first:?}");
            assert!(!first.iter().any(|t| artifact_tools::is_write(t)), "{api:?}: no writing tools yet: {first:?}");
            let second = offered(&requests[1]);
            assert_eq!(second[..first.len()], first[..], "{api:?}: added after what was there");
            for name in [artifact_tools::CREATE_TABLE, artifact_tools::ADD_TABLE_ROWS, artifact_tools::CHECK_ITEMS] {
                assert!(second.iter().any(|t| t == name), "{api:?}: {name} in {second:?}");
            }
            assert!(!second.iter().any(|t| t == artifact_tools::CREATE_BOARD), "{api:?}: only what was asked for");
            assert!(requests[1].body.to_string().contains("You can use the table tools now"), "{api:?}");
            assert_eq!(offered(&requests[2]), second, "{api:?}: loading is once");
            assert!(host.activities.contains(&"Getting table and checklist tools…".to_string()), "{:?}", host.activities);
        }
    }

    #[test]
    fn a_writing_tool_called_before_asking_brings_its_kind() {
        let script = Script::new()
            .tool(artifact_tools::CREATE_BOARD, json!({"channel": "general", "title": "B"}))
            .tool(artifact_tools::USE_ARTIFACT_TOOLS, json!({"kinds": ["table", "poster"]}))
            .text("Done.");
        let server = MockServer::start(script);
        let mut host = Recorder::new();
        futures::executor::block_on(reply(&server, Api::Anthropic, 8).run(&mut host)).unwrap();

        let requests = server.requests();
        let second = offered(&requests[1]);
        assert!(second.iter().any(|t| t == artifact_tools::MOVE_BOARD_CARD), "{second:?}");
        assert!(!second.iter().any(|t| t == artifact_tools::CREATE_TABLE), "{second:?}");
        let third = requests[2].body.to_string();
        assert!(third.contains("There is no artifact kind “poster”"), "{third}");
        let third = offered(&requests[2]);
        assert!(!third.iter().any(|t| t == artifact_tools::CREATE_TABLE), "a failed call adds nothing: {third:?}");
    }

    #[test]
    fn a_reply_without_the_loader_gets_no_writing_tools() {
        // The worker's tools: reading ones only.
        let script = Script::new().tool(artifact_tools::CREATE_TABLE, json!({"channel": "general", "title": "T"})).text("ok");
        let server = MockServer::start(script);
        let mut r = reply(&server, Api::Anthropic, 8);
        r.request.tools = crate::worker::chat_tools();
        futures::executor::block_on(r.run(&mut Recorder::new())).unwrap();
        let second = offered(&server.requests()[1]);
        assert!(!second.iter().any(|t| artifact_tools::is_write(t)), "{second:?}");
    }

    /// A reply that can ask a worker, which talks to the same mock server.
    fn with_worker(server: &MockServer, problem: Option<&str>) -> Reply {
        let mut r = reply(server, Api::Anthropic, 8);
        r.request.tools.push(crate::worker::spec());
        r.worker = Some(crate::worker::Job {
            provider: r.provider.clone(),
            max_steps: 8,
            request: Request {
                model: "cheap".into(),
                system: "You are Worker.".into(),
                turns: vec![],
                tools: crate::worker::chat_tools(),
                max_tokens: None,
                extra: None,
            },
            claude_code: false,
            env: None,
            channels: vec![],
            problem: problem.map(str::to_string),
            spent: Usage::default(),
        });
        r
    }

    #[test]
    fn the_worker_reports_back_without_acting() {
        let server = MockServer::start(
            Script::new()
                .tool(crate::worker::ASK_WORKER, json!({"task": "Which channels are there?"}))
                // The worker: reading is fine, acting isn't.
                .tool("list_channels", json!({}))
                .tool(tools::START_TASK, json!({"channel": "engineering", "title": "x", "brief": "y"}))
                .text("Channels: general, engineering.")
                // The asking agent again, with the report.
                .text("There are two channels."),
        );
        let mut host = Recorder::new();
        let threads = host.ws.threads.len();
        futures::executor::block_on(with_worker(&server, None).run(&mut host)).unwrap();

        assert_eq!(host.text, "\n\nThere are two channels.", "the worker's text isn't posted");
        assert!(host.activities.contains(&"Worker: Looking at channels…".to_string()), "{:?}", host.activities);
        assert_eq!(host.ws.threads.len(), threads, "the worker can't start tasks");
        assert!(host.worker_usage.output > 0, "the worker's tokens are reported apart");
        let bodies: Vec<String> = server.requests().iter().map(|r| r.body.to_string()).collect();
        assert_eq!(bodies.len(), 5);
        assert!(bodies[1].contains("\"cheap\"") && bodies[1].contains("Which channels are there?"), "{}", bodies[1]);
        assert!(!bodies[1].contains("what's going on"), "the worker doesn't see the conversation");
        assert!(!bodies[1].contains("\"start_task\"") && !bodies[1].contains("\"ask_owner\""), "{}", bodies[1]);
        assert!(bodies[3].contains("There is no tool named start_task"), "{}", bodies[3]);
        assert!(bodies[4].contains("Channels: general, engineering."), "the report goes back as the tool result");
    }

    #[test]
    fn a_worker_that_cant_run_says_why() {
        let server = MockServer::start(
            Script::new().tool(crate::worker::ASK_WORKER, json!({"task": "Look."})).text("I'll look myself."),
        );
        let mut host = Recorder::new();
        futures::executor::block_on(with_worker(&server, Some("No API key")).run(&mut host)).unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "no worker request was made");
        assert!(requests[1].body.to_string().contains("The worker can't run: No API key"));
    }

    #[test]
    fn a_card_ends_the_reply() {
        let server = MockServer::start(
            Script::new().tool("ask_owner", json!({"question": "Ship it?", "options": ["Yes", "Not yet"]})).text("unreached"),
        );
        let mut host = Recorder::new();
        futures::executor::block_on(reply(&server, Api::Anthropic, 8).run(&mut host)).unwrap();
        assert!(matches!(&host.cards[..], [Card::Choice { question, .. }] if question == "Ship it?"), "{:?}", host.cards);
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn passing_posts_nothing() {
        for api in WIRES {
            let server = MockServer::start(Script::new().tool(tools::PASS, json!({"reason": "not ready"})).text("unreached"));
            let mut host = Recorder::new();
            let outcome = futures::executor::block_on(reply_over(&server, api, 8).run(&mut host)).unwrap();
            assert_eq!(outcome, Outcome::Passed, "{api:?}");
            assert_eq!(server.requests().len(), 1, "{api:?}: nothing runs after a pass");
        }
    }

    #[test]
    fn stops_after_max_steps() {
        let server = MockServer::start(Script::new().tool("list_channels", json!({})).tool("list_channels", json!({})));
        let mut host = Recorder::new();
        let err = futures::executor::block_on(reply(&server, Api::Openai, 2).run(&mut host)).unwrap_err();
        assert!(err.contains("after 2 model calls"), "{err}");
    }

    #[test]
    fn reports_api_errors() {
        let server = MockServer::start(Script::new().error(529, "Overloaded"));
        let mut host = Recorder::new();
        let err = futures::executor::block_on(reply(&server, Api::Anthropic, 8).run(&mut host)).unwrap_err();
        assert_eq!(err, "HTTP 529: Overloaded");
    }
}
