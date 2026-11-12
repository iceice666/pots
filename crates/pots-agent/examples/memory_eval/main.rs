//! Memory eval: does an agent remember decisions made long ago, or in
//! another thread?
//!
//! `scenario.txt` is three weeks of a team chat with decisions planted at
//! different distances from its end, and probes that ask about them. Each
//! probe is asked once per *cell* (a memory strategy × whether the agent may
//! use its chat tools), on a fresh copy of the workspace, through the same
//! reply loop the app uses, so `claude-code` providers work too.
//!
//! The decision log is measured in two halves. *Capture* replays the
//! scenario's decision points in order and lets the agent answer each one
//! with `record_decision` available: does it record what the owner decided,
//! and only that? *Recall* (`--memory log`) then asks the probes with the
//! log it captured; `--memory oracle` uses a hand-written log instead, as
//! the upper bound.
//!
//! ```text
//! cargo run -p pots-agent --example memory_eval -- list
//! cargo run -p pots-agent --example memory_eval -- dump DIR
//! cargo run -p pots-agent --example memory_eval -- capture [options]
//! cargo run -p pots-agent --example memory_eval -- run [options]
//! ```
//!
//! Nothing is saved to the workspace; `run` and `capture` only read
//! `config.toml` and `keys.toml` from the data directory (`$POTS_HOME` or
//! `~/.pots`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{Duration, Local, NaiveDate, TimeZone};
use serde_json::Value;

use pots_agent::agent::{self, Agents, tools};
use pots_agent::config::Config;
use pots_agent::data::{self, Card, Conv, Decision, Retired, User, UserKind, Workspace};
use pots_agent::reply::{Host, Outcome, Reply};
use pots_agent::store;
use pots_llm::{Part, Request, Usage};

const SCENARIO: &str = include_str!("scenario.txt");

const USAGE: &str = "\
usage: memory_eval list
       memory_eval dump DIR
       memory_eval capture [--agent NAME] [--capture app|forced] [--out DIR]
       memory_eval run [--agent NAME] [--memory 40,all] [--tools on,off]
                       [--probe ID,...] [--runs N] [--log FILE]
                       [--capture app|forced] [--out DIR]

  list      the probes, how far back each answer is, and what can reach it
  dump      write the scenario to DIR/workspace.json (plus your config.toml),
            to try it in the app: POTS_HOME=DIR cargo run
  capture   replay the scenario and see what the agent records with
            record_decision, and whether the final log covers each decision
  run       ask every probe in every cell and grade the answers

  --agent   who answers (default PM; agents with a workspace aren't supported)
  --memory  memory strategies (default 40,all):
              N       the last N messages
              all     the whole conversation
              none    only the question: the control; with --tools off it
                      should get every probe but the near one wrong
              log     the last 40 messages, plus the decision log `capture`
                      recorded (runs capture first, unless --log is given)
              oracle  the last 40 messages, plus a hand-written decision log
  --tools   on: the agent may use list_channels / read_channel; off: it may not
            (default on,off)
  --probe   only these probes
  --runs    times each cell is asked (default 1; answers vary between runs)
  --log     the log.json of an earlier capture, for --memory log
  --capture app: answer every message that wakes the agent in the app (default);
            forced: answer each marked point, whether it would wake it or not
  --out     where transcripts go (default target/memory-eval/<time>)";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("dump") => match args.get(1) {
            Some(dir) => dump(Path::new(dir)),
            None => Err("dump needs a directory".into()),
        },
        Some("capture") => Options::parse(&args[1..]).and_then(|o| capture_only(&o)),
        Some("run") => Options::parse(&args[1..]).and_then(run),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return;
        }
        _ => Err("expected list, dump, capture or run".into()),
    };
    if let Err(e) = result {
        eprintln!("memory_eval: {e}\n\n{USAGE}");
        std::process::exit(2);
    }
}

// --- The scenario -----------------------------------------------------------

#[derive(Debug)]
enum Item {
    Channel(String),
    Thread { channel: String, name: String, by: String },
    Clock { day: i64, minutes: u32 },
    Message { author: String, text: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    Near,
    Mid,
    Far,
    Cross,
}

impl Tag {
    fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "near" => Tag::Near,
            "mid" => Tag::Mid,
            "far" => Tag::Far,
            "cross" => Tag::Cross,
            _ => return Err(format!("unknown tag {s:?}")),
        })
    }

    fn name(self) -> &'static str {
        match self {
            Tag::Near => "near",
            Tag::Mid => "mid",
            Tag::Far => "far",
            Tag::Cross => "cross",
        }
    }

    /// The tag a fact `distance` messages before the question has.
    fn of(distance: usize) -> Self {
        match distance {
            ..WINDOW => Tag::Near,
            WINDOW..READ_CHANNEL_MAX => Tag::Mid,
            _ => Tag::Far,
        }
    }
}

/// The default `history` window.
const WINDOW: usize = 40;
/// The most messages `read_channel` shows.
const READ_CHANNEL_MAX: usize = 100;

#[derive(Clone, Debug)]
enum Place {
    /// At the end of a channel.
    Channel(String),
    /// In a new thread of a channel.
    NewThread(String),
}

#[derive(Clone, Debug)]
struct Probe {
    id: String,
    place: Place,
    tag: Tag,
    /// Text of the message that holds the answer.
    fact: String,
    question: String,
    /// All groups must match; any alternative in a group will do.
    expect: Vec<Vec<String>>,
}

/// A message the agent is asked to answer during capture.
#[derive(Clone, Debug)]
struct Point {
    id: String,
    /// Index of the message in [`Scenario::items`].
    item: usize,
    kind: PointKind,
}

#[derive(Clone, Debug)]
enum PointKind {
    /// The owner (or the team) settles something: it should be recorded.
    Decision {
        /// The point whose decision this one changes.
        replaces: Option<String>,
        /// How a careful agent would record it, for the oracle log.
        oracle: String,
        /// Words a log entry recording it would contain (any of them).
        key: Vec<String>,
    },
    /// Nothing to record.
    Status,
}

struct Scenario {
    items: Vec<Item>,
    points: Vec<Point>,
    probes: Vec<Probe>,
}

/// `{date:+N}`: today + N days.
fn expand(text: &str, today: NaiveDate, answer: bool) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{date:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "{date:".len()..];
        let Some(end) = after.find('}') else { break };
        let days: i64 = after[..end].trim_start_matches('+').parse().unwrap_or(0);
        let d = today + Duration::days(days);
        if answer {
            let alts = [
                d.format("%-m/%-d").to_string(),
                d.format("%-m月%-d").to_string(),
                d.format("%-m 月 %-d").to_string(),
                d.format("%Y-%m-%d").to_string(),
                d.format("%b %-d").to_string(),
                d.format("%B %-d").to_string(),
            ];
            out.push_str(&alts.join("|"));
        } else {
            out.push_str(&d.format("%-m/%-d").to_string());
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn parse(text: &str, today: NaiveDate) -> Result<Scenario, String> {
    let mut items = Vec::new();
    let mut points: Vec<Point> = Vec::new();
    let mut probes: Vec<Probe> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let err = |e: &str| format!("scenario.txt:{}: {e}", n + 1);
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(cont) = line.strip_prefix("  ") {
            match items.last_mut() {
                Some(Item::Message { text, .. }) if probes.is_empty() => {
                    text.push('\n');
                    text.push_str(&expand(cont.trim_start(), today, false));
                }
                _ => return Err(err("a continuation line must follow a message")),
            }
        } else if let Some(rest) = line.strip_prefix("!! ") {
            if !matches!(items.last(), Some(Item::Message { .. })) || !probes.is_empty() {
                return Err(err("a point must follow a message"));
            }
            let item = items.len() - 1;
            let (head, oracle) = match rest.split_once(": ") {
                Some((head, oracle)) => (head, Some(expand(oracle.trim(), today, false))),
                None => (rest, None),
            };
            let usage = "!! status ID  or  !! decision ID key=WORD[|WORD] [replaces ID]: how to record it";
            let mut words = head.split_whitespace();
            let (Some(what), Some(id)) = (words.next(), words.next()) else { return Err(err(usage)) };
            let (mut key, mut replaces) = (Vec::new(), None);
            while let Some(w) = words.next() {
                match w.strip_prefix("key=") {
                    Some(k) => key = expand(k, today, true).split('|').map(|w| w.replace('~', " ")).collect(),
                    None if w == "replaces" => {
                        let old = words.next().ok_or_else(|| err(usage))?;
                        if !points.iter().any(|p| p.id == old) {
                            return Err(err(&format!("no earlier point {old}")));
                        }
                        replaces = Some(old.to_string());
                    }
                    None => return Err(err(usage)),
                }
            }
            let kind = match (what, oracle) {
                ("status", None) if key.is_empty() && replaces.is_none() => PointKind::Status,
                ("decision", Some(oracle)) if !key.is_empty() => PointKind::Decision { replaces, oracle, key },
                _ => return Err(err(usage)),
            };
            points.push(Point { id: id.into(), item, kind });
        } else if let Some(name) = line.strip_prefix("== channel ") {
            items.push(Item::Channel(name.trim().to_string()));
        } else if let Some(rest) = line.strip_prefix("== thread ") {
            let (channel, rest) = rest.split_once(' ').ok_or_else(|| err("== thread CHANNEL \"NAME\" by AUTHOR"))?;
            let rest = rest.trim().strip_prefix('"').ok_or_else(|| err("the thread name must be quoted"))?;
            let (name, rest) = rest.split_once('"').ok_or_else(|| err("unclosed thread name"))?;
            let by = rest.trim().strip_prefix("by ").ok_or_else(|| err("missing `by AUTHOR`"))?;
            items.push(Item::Thread { channel: channel.into(), name: name.into(), by: by.trim().into() });
        } else if let Some(rest) = line.strip_prefix("-- day ") {
            let (day, time) = rest.trim().split_once(' ').ok_or_else(|| err("-- day -N HH:MM"))?;
            let (h, m) = time.split_once(':').ok_or_else(|| err("time is HH:MM"))?;
            let num = |s: &str| s.parse::<u32>().map_err(|_| err("bad time"));
            items.push(Item::Clock {
                day: day.parse().map_err(|_| err("bad day"))?,
                minutes: num(h)? * 60 + num(m)?,
            });
        } else if let Some(rest) = line.strip_prefix("?? ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            let [id, place, tag] = parts[..] else { return Err(err("?? ID @PLACE TAG")) };
            let place = match place.strip_prefix("@thread:") {
                Some(c) => Place::NewThread(c.into()),
                None => Place::Channel(place.strip_prefix('@').ok_or_else(|| err("place starts with @"))?.into()),
            };
            probes.push(Probe {
                id: id.into(),
                place,
                tag: Tag::parse(tag).map_err(|e| err(&e))?,
                fact: String::new(),
                question: String::new(),
                expect: Vec::new(),
            });
        } else if let Some((key, value)) = line.split_once(": ").filter(|_| !probes.is_empty()) {
            let probe = probes.last_mut().unwrap();
            match key {
                "F" => probe.fact = value.trim().into(),
                "Q" => probe.question = value.trim().into(),
                "A" => probe.expect.push(expand(value.trim(), today, true).split('|').map(str::to_string).collect()),
                _ => return Err(err("expected F:, Q: or A:")),
            }
        } else if let Some((author, text)) = line.split_once(": ") {
            items.push(Item::Message { author: author.into(), text: expand(text.trim(), today, false) });
        } else {
            return Err(err("can't read this line"));
        }
    }
    for p in &probes {
        if p.fact.is_empty() || p.question.is_empty() || p.expect.is_empty() {
            return Err(format!("probe {} needs F:, Q: and A:", p.id));
        }
    }
    Ok(Scenario { items, points, probes })
}

fn scenario() -> Scenario {
    parse(SCENARIO, Local::now().date_naive()).unwrap_or_else(|e| panic!("{e}"))
}

/// Unix time of `minutes` past midnight, `day` days from today.
fn at(day: i64, minutes: u32) -> i64 {
    let date = Local::now().date_naive() + Duration::days(day);
    let time = date.and_hms_opt(minutes / 60 % 24, minutes % 60, 0).unwrap();
    Local.from_local_datetime(&time).earliest().map_or(0, |t| t.timestamp())
}

/// The workspace after the scenario's first `n` items, and where its last
/// message went. Authors other than "Owner" are agents, so the config's
/// agents of the same names take them over.
fn build_until(sc: &Scenario, n: usize) -> (Workspace, Option<(Conv, usize)>) {
    let mut ws = data::empty();
    let user = |ws: &mut Workspace, name: &str| -> usize {
        if name == "Owner" {
            return ws.me;
        }
        match ws.users.iter().position(|u| u.kind == UserKind::Agent && u.name == name) {
            Some(u) => u,
            None => {
                ws.users.push(User { name: name.into(), color: 0x0f766e, online: true, kind: UserKind::Agent });
                ws.users.len() - 1
            }
        }
    };
    let mut conv = Conv::Channel(0);
    let mut clock = (0i64, 0u32);
    let mut last = None;
    let stamp = |ws: &mut Workspace, conv: Conv, ix: usize, clock: &mut (i64, u32)| {
        ws.message_mut(conv, ix).ts = at(clock.0, clock.1);
        clock.1 += 2;
    };
    for item in &sc.items[..n] {
        match item {
            Item::Channel(name) => {
                let c = ws.find_channel(name).unwrap_or_else(|| ws.add_channel(name));
                conv = Conv::Channel(c);
            }
            Item::Thread { channel, name, by } => {
                let c = ws.find_channel(channel).unwrap_or_else(|| ws.add_channel(channel));
                let author = user(&mut ws, by);
                let (starter, t) = ws.new_thread(c, author, name);
                stamp(&mut ws, Conv::Channel(c), starter, &mut clock);
                conv = Conv::Thread(t);
            }
            Item::Clock { day, minutes } => clock = (*day, *minutes),
            Item::Message { author, text } => {
                let author = user(&mut ws, author);
                let ix = ws.post(conv, author, text);
                stamp(&mut ws, conv, ix, &mut clock);
                last = Some((conv, ix));
            }
        }
    }
    (ws, last)
}

fn build(sc: &Scenario) -> Workspace {
    build_until(sc, sc.items.len()).0
}

/// Post the probe's question as the owner: where, and the index the reply
/// would get (what the agent sees ends before it).
fn ask(ws: &mut Workspace, probe: &Probe) -> (Conv, usize) {
    let conv = match &probe.place {
        Place::Channel(name) => Conv::Channel(ws.find_channel(name).expect("probe channel exists")),
        Place::NewThread(name) => {
            let c = ws.find_channel(name).expect("probe channel exists");
            let (_, t) = ws.new_thread(c, ws.me, "快速問題");
            Conv::Thread(t)
        }
    };
    let ix = ws.post(conv, ws.me, &probe.question);
    (conv, ix + 1)
}

/// Where the probe's fact is: its conversation and index.
fn locate_fact(ws: &Workspace, fact: &str) -> Vec<(Conv, usize)> {
    let convs = (0..ws.channels.len()).map(Conv::Channel).chain((0..ws.threads.len()).map(Conv::Thread));
    convs
        .flat_map(|conv| (0..ws.len(conv)).filter(move |&ix| ws.message(conv, ix).source.contains(fact)).map(move |ix| (conv, ix)))
        .collect()
}

fn conv_name(ws: &Workspace, conv: Conv) -> String {
    match conv {
        Conv::Channel(c) => format!("#{}", ws.channels[c].name),
        Conv::Thread(t) => format!("#{} › {}", ws.channels[ws.threads[t].channel].name, ws.threads[t].name),
    }
}

/// How many messages before the question in `#channel` the fact is, if it
/// is in that channel.
fn distance(sc: &Scenario, probe: &Probe) -> (String, Option<usize>) {
    let mut ws = build(sc);
    let (conv, upto) = ask(&mut ws, probe);
    let question = upto - 1;
    let found = locate_fact(&ws, &probe.fact);
    let Some(&(fact_conv, fact_ix)) = found.first() else { return ("(not found)".into(), None) };
    let channel = match probe.place {
        Place::Channel(_) => Some(conv),
        Place::NewThread(_) => None,
    };
    let d = match (channel, fact_conv) {
        (Some(c), f) if c == f => Some(question - fact_ix),
        (None, Conv::Channel(c)) => {
            // Asked in a new thread: how far back the channel's own history is.
            Some(ws.len(Conv::Channel(c)) - fact_ix)
        }
        _ => None,
    };
    (conv_name(&ws, fact_conv), d)
}

/// The decision log a careful agent would have kept: the scenario's own
/// wording of each decision point.
fn oracle_log(sc: &Scenario) -> Vec<Decision> {
    let mut log: Vec<Decision> = Vec::new();
    let mut ids: BTreeMap<&str, usize> = BTreeMap::new();
    for p in &sc.points {
        let PointKind::Decision { replaces, oracle, .. } = &p.kind else { continue };
        let (ws, Some((conv, ix))) = build_until(sc, p.item + 1) else { unreachable!("points follow messages") };
        let pm = ws.users.iter().position(|u| u.name == "PM").unwrap_or(ws.me);
        let id = log.len();
        log.push(Decision {
            text: oracle.clone(),
            author: pm,
            ts: ws.message(conv, ix).ts,
            place: agent::task_place(&ws, conv),
            retired: None,
        });
        if let Some(old) = replaces {
            log[ids[old.as_str()]].retired = Some(Retired::ReplacedBy(id));
        }
        ids.insert(&p.id, id);
    }
    log
}

fn list() -> Result<(), String> {
    let sc = scenario();
    println!("{:<20} {:<6} {:<26} {:>9}  reachable by", "probe", "tag", "fact in", "msgs back");
    for p in &sc.probes {
        let (place, d) = distance(&sc, p);
        let reach = match (p.tag, d) {
            (Tag::Cross, Some(d)) if d < READ_CHANNEL_MAX => "read_channel",
            (Tag::Cross, Some(_)) => "nothing",
            (Tag::Cross, None) => "read_channel (thread)",
            (_, Some(d)) => match Tag::of(d) {
                Tag::Near => "window",
                Tag::Mid => "window=all, read_channel",
                _ => "window=all",
            },
            (_, None) => "?",
        };
        let d = d.map_or("-".into(), |d| d.to_string());
        println!("{:<20} {:<6} {:<26} {:>9}  {reach}", p.id, p.tag.name(), place, d);
    }
    let decisions = sc.points.iter().filter(|p| matches!(p.kind, PointKind::Decision { .. })).count();
    println!("\n{} capture points: {decisions} decisions, {} without one.", sc.points.len(), sc.points.len() - decisions);
    Ok(())
}

fn dump(dir: &Path) -> Result<(), String> {
    if dir.join("workspace.json").exists() {
        return Err(format!("{} already has a workspace.json; pick an empty directory", dir.display()));
    }
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let ws = build(&scenario());
    let json = serde_json::to_vec_pretty(&ws).map_err(|e| e.to_string())?;
    fs::write(dir.join("workspace.json"), json).map_err(|e| e.to_string())?;
    let config = store::config_path();
    match fs::copy(&config, dir.join("config.toml")) {
        Ok(_) => println!("Copied {}.", config.display()),
        Err(e) => println!("No config copied ({}: {e}); the app will write a default one.", config.display()),
    }
    println!(
        "Wrote {}/workspace.json. Try it in the app:\n\n  POTS_HOME={} cargo run\n\n\
         Then ask the probe questions (`memory_eval list`); #general needs @PM. Providers with API keys \
         also need keys.toml in that directory.",
        dir.display(),
        dir.display()
    );
    Ok(())
}

// --- Running replies --------------------------------------------------------

/// Runs a reply with no app around it: chat look-ups and the decision log
/// work on the reply's own copy of the workspace; anything else that would
/// change it is refused.
struct EvalHost {
    ws: Workspace,
    /// Who is replying, where, and when (decisions get this time).
    author: usize,
    conv: Conv,
    ts: i64,
    text: String,
    calls: Vec<String>,
    cards: Vec<Card>,
    usage: Usage,
}

impl EvalHost {
    fn new(ws: Workspace, author: usize, conv: Conv, ts: i64) -> Self {
        Self { ws, author, conv, ts, text: String::new(), calls: Vec::new(), cards: Vec::new(), usage: Usage::default() }
    }

    /// The reply text, with any choice card it ended with.
    fn answer(&self) -> String {
        let mut answer = self.text.trim().to_string();
        for card in &self.cards {
            if let Card::Choice { question, options, .. } = card {
                answer.push_str(&format!("\n\n[ask_owner] {question} — {}", options.join(" / ")));
            }
        }
        answer
    }
}

impl Host for EvalHost {
    fn progress(&mut self, text: &str, _activity: Option<String>) -> Result<(), String> {
        self.text.push_str(text);
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
    fn run_tool(&mut self, name: &str, input: &Value) -> Result<(String, bool), String> {
        self.calls.push(format!("{name} {input}"));
        Ok(match name {
            "list_channels" | "read_channel" => tools::run(&self.ws, name, input),
            tools::RECORD_DECISION => match tools::record_decision(&mut self.ws, self.author, Some(self.conv), input) {
                Ok(recorded) => {
                    if let Some(id) = recorded.id {
                        self.ws.decisions[id].ts = self.ts;
                    }
                    (recorded.result, false)
                }
                Err(e) => (e, true),
            },
            _ => (format!("{name} isn't available right now; answer with what you know."), true),
        })
    }
}

/// The agent's reply in `conv`, seeing what comes before `upto`.
fn reply(agents: &Agents, a: usize, request: Request, host: &mut EvalHost) -> (Result<Outcome, String>, f64) {
    let reply = Reply {
        provider: agents.list[a].provider.clone(),
        max_steps: agents.list[a].max_steps(),
        request,
        session: None,
        worker: None,
        conflicts: None,
    };
    let started = Instant::now();
    let outcome = futures::executor::block_on(reply.run(host));
    (outcome, started.elapsed().as_secs_f64())
}

/// `agent` in `ws`, if it can be evaluated.
fn find_agent(agents: &Agents, agent: &str) -> Result<usize, String> {
    let a = agents
        .list
        .iter()
        .position(|x| x.config.name.eq_ignore_ascii_case(agent))
        .ok_or_else(|| format!("no agent named {agent} in config.toml"))?;
    if agents.list[a].config.workspace.is_some() {
        return Err(format!("{agent} has a workspace; only agents without one can be evaluated"));
    }
    Ok(a)
}

// --- Capture ----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureMode {
    /// Answer the messages that wake the agent in the app, as the app would.
    App,
    /// Answer every marked point, whether or not it would wake the agent.
    Forced,
}

struct CaptureRow {
    /// The message's index in [`Scenario::items`].
    item: usize,
    /// The marked point at this message, if any.
    point: Option<Point>,
    /// In the app, the message would wake the agent (it's in its DM, or
    /// @mentions it, or the agent spoke last in the thread). Forced capture
    /// asks it to answer either way.
    reached: bool,
    /// At a marked point: it recorded the decision (or, for status, nothing).
    ok: Option<bool>,
    /// Decisions it recorded, as "[id] text".
    recorded: Vec<String>,
    /// Ids it retired.
    retired: Vec<usize>,
    calls: Vec<String>,
    answer: String,
    error: Option<String>,
    secs: f64,
}

impl CaptureRow {
    fn id(&self) -> String {
        self.point.as_ref().map_or_else(|| format!("m{}", self.item), |p| p.id.clone())
    }

    fn mark(&self) -> &'static str {
        match (&self.error, self.ok) {
            (Some(_), _) => "!",
            (None, Some(true)) => "✓",
            (None, Some(false)) => "✗",
            (None, None) => "·",
        }
    }
}

/// Whether message `item` would wake `agent` in the app.
fn wakes(sc: &Scenario, config: &Config, agent: &str, item: usize) -> Result<bool, String> {
    let (mut ws, Some((conv, ix))) = build_until(sc, item + 1) else { return Ok(false) };
    let agents = Agents::from_config(&mut ws, config);
    let a = find_agent(&agents, agent)?;
    Ok(agents.triggered(&ws, conv, ix).agents.contains(&a))
}

/// Have the agent answer messages of the scenario in order, carrying the
/// decision log forward.
fn capture(
    sc: &Scenario,
    config: &Config,
    agent: &str,
    mode: CaptureMode,
    progress: bool,
) -> Result<(Vec<Decision>, Vec<CaptureRow>), String> {
    let mut targets: Vec<(usize, Option<&Point>, bool)> = Vec::new();
    match mode {
        CaptureMode::Forced => {
            for p in &sc.points {
                targets.push((p.item, Some(p), wakes(sc, config, agent, p.item)?));
            }
        }
        CaptureMode::App => {
            for (i, item) in sc.items.iter().enumerate() {
                if matches!(item, Item::Message { .. }) && wakes(sc, config, agent, i)? {
                    targets.push((i, sc.points.iter().find(|p| p.item == i), true));
                }
            }
        }
    }
    let mut log: Vec<Decision> = Vec::new();
    let mut rows = Vec::new();
    // Which decisions each point recorded, for checking `replaces`.
    let mut by_point: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (n, &(item, point, reached)) in targets.iter().enumerate() {
        let (mut ws, Some((conv, ix))) = build_until(sc, item + 1) else { unreachable!("targets are messages") };
        let agents = Agents::from_config(&mut ws, config);
        let a = find_agent(&agents, agent)?;
        ws.decisions = log.clone();
        let request = agents.request(a, &ws, conv, ix + 1);
        let label = point.map_or_else(|| format!("m{item}"), |p| p.id.clone());
        if progress {
            eprint!("[capture {}/{}] {label} … ", n + 1, targets.len());
        }
        let ts = ws.message(conv, ix).ts;
        let mut host = EvalHost::new(ws, agents.list[a].user, conv, ts);
        let (outcome, secs) = reply(&agents, a, request, &mut host);
        let before = log.len();
        let retired: Vec<usize> =
            (0..before).filter(|&i| log[i].retired.is_none() && host.ws.decisions[i].retired.is_some()).collect();
        let recorded: Vec<String> =
            (before..host.ws.decisions.len()).map(|i| format!("[{i}] {}", host.ws.decisions[i].text)).collect();
        let ok = point.map(|p| match &p.kind {
            PointKind::Status => recorded.is_empty() && retired.is_empty(),
            PointKind::Decision { replaces: None, .. } => !recorded.is_empty(),
            // It must retire what the earlier point recorded, or what has
            // replaced that since (say, a reworded copy).
            PointKind::Decision { replaces: Some(old), .. } => {
                let mut lineage = by_point.get(old.as_str()).cloned().unwrap_or_default();
                let mut i = 0;
                while i < lineage.len() {
                    if let Some(Retired::ReplacedBy(next)) = log.get(lineage[i]).and_then(|d| d.retired.clone()) {
                        lineage.push(next);
                    }
                    i += 1;
                }
                !recorded.is_empty() && retired.iter().any(|id| lineage.contains(id))
            }
        });
        if let Some(p) = point {
            by_point.insert(&p.id, (before..host.ws.decisions.len()).collect());
        }
        let error = outcome.err();
        let (calls, answer) = (host.calls.clone(), host.answer());
        let row = CaptureRow { item, point: point.cloned(), reached, ok, recorded, retired, calls, answer, error, secs };
        if progress {
            eprintln!("{} ({secs:.0}s){}", row.mark(), if row.recorded.is_empty() { "" } else { " recorded" });
        }
        rows.push(row);
        log = host.ws.decisions;
    }
    Ok((log, rows))
}

/// Each decision still meant to hold at the end, and whether an entry in
/// force in `log` records it.
fn coverage<'a>(sc: &'a Scenario, log: &[Decision]) -> Vec<(&'a Point, bool)> {
    let replaced: Vec<&str> = sc
        .points
        .iter()
        .filter_map(|p| match &p.kind {
            PointKind::Decision { replaces: Some(old), .. } => Some(old.as_str()),
            _ => None,
        })
        .collect();
    let active: Vec<String> = log.iter().filter(|d| d.retired.is_none()).map(|d| d.text.to_lowercase()).collect();
    sc.points
        .iter()
        .filter(|p| !replaced.contains(&p.id.as_str()))
        .filter_map(|p| match &p.kind {
            PointKind::Decision { key, .. } => {
                let found = active.iter().any(|t| key.iter().any(|k| t.contains(&k.to_lowercase())));
                Some((p, found))
            }
            PointKind::Status => None,
        })
        .collect()
}

fn capture_report(sc: &Scenario, mode: CaptureMode, rows: &[CaptureRow], log: &[Decision], out: &Path) -> Result<(), String> {
    let dir = out.join("capture");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    println!("\n{:<14} {:<10} {:<8} {:<3} recorded", "message", "point", "reached", "");
    for r in rows {
        let (kind, expected) = match r.point.as_ref().map(|p| &p.kind) {
            None => ("—", "nothing in particular".to_string()),
            Some(PointKind::Status) => ("status", "nothing".to_string()),
            Some(PointKind::Decision { replaces: None, oracle, .. }) => ("decision", oracle.clone()),
            Some(PointKind::Decision { replaces: Some(old), oracle, .. }) => ("replaces", format!("{oracle} (replaces {old})")),
        };
        let mut what = r.recorded.join(" / ");
        if !r.retired.is_empty() {
            what.push_str(&format!(" (retired {:?})", r.retired));
        }
        let reached = if r.reached { "yes" } else { "no" };
        println!("{:<14} {:<10} {:<8} {:<3} {}", r.id(), kind, reached, r.mark(), if what.is_empty() { "—" } else { &what });
        let message = match &sc.items[r.item] {
            Item::Message { author, text } => format!("{author}: {text}"),
            _ => String::new(),
        };
        let mut s = format!(
            "# capture {} ({kind})\n\n**Result:** {}  \n**Reaches the agent in the app:** {reached}  \n\
             **Expected:** {expected}  \n**Time:** {:.1}s\n\n**Message:** {message}\n\n**Recorded:**\n",
            r.id(),
            r.mark(),
            r.secs
        );
        for d in &r.recorded {
            s.push_str(&format!("- {d}\n"));
        }
        if !r.retired.is_empty() {
            s.push_str(&format!("\n**Retired:** {:?}\n", r.retired));
        }
        if !r.calls.is_empty() {
            s.push_str("\n**Tool calls:**\n");
            for c in &r.calls {
                s.push_str(&format!("- `{c}`\n"));
            }
        }
        s.push_str(&format!("\n**Reply:**\n\n{}\n", r.answer));
        if let Some(e) = &r.error {
            s.push_str(&format!("\n**Error:** {e}\n"));
        }
        fs::write(dir.join(format!("{}.md", r.id())), s).map_err(|e| e.to_string())?;
    }
    let marked = |status: bool| rows.iter().filter(move |r| matches!(r.point.as_ref().map(|p| &p.kind), Some(PointKind::Status)) == status && r.point.is_some());
    let (decided, quiet) = (marked(false).filter(|r| r.ok == Some(true)).count(), marked(true).filter(|r| r.ok == Some(true)).count());
    match mode {
        CaptureMode::Forced => println!(
            "\nForced: recorded {decided}/{} decisions when asked; kept quiet on {quiet}/{} status messages. \
             {} decision points don't wake the agent in the app.",
            marked(false).count(),
            marked(true).count(),
            marked(false).filter(|r| !r.reached).count()
        ),
        CaptureMode::App => println!(
            "\nApp: {} replies, one per message that wakes the agent. Recorded at {decided}/{} decision points it was \
             woken by; kept quiet on {quiet}/{} status messages.",
            rows.len(),
            marked(false).count(),
            marked(true).count()
        ),
    }
    let cov = coverage(sc, log);
    let missing: Vec<&str> = cov.iter().filter(|(_, found)| !found).map(|(p, _)| p.id.as_str()).collect();
    println!(
        "Final log covers {}/{} decisions in force{}. {} entries, {} in force.",
        cov.len() - missing.len(),
        cov.len(),
        if missing.is_empty() { String::new() } else { format!(" (missing: {})", missing.join(", ")) },
        log.len(),
        log.iter().filter(|d| d.retired.is_none()).count()
    );
    let json = serde_json::to_vec_pretty(log).map_err(|e| e.to_string())?;
    fs::write(out.join("log.json"), json).map_err(|e| e.to_string())?;
    println!("Log: {}/log.json; transcripts: {}", out.display(), dir.display());
    Ok(())
}

// --- Recall -----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogSource {
    /// What `capture` recorded.
    Captured,
    /// The scenario's own wording.
    Oracle,
}

/// How an agent remembers what came before the question. New strategies
/// (a rolling summary, pinned starters) go here.
#[derive(Clone, Copy, Debug)]
enum Memory {
    /// The last N messages, verbatim (the app's `history`).
    Window(usize),
    /// The last N messages, plus a decision log.
    Log(usize, LogSource),
}

impl Memory {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "all" => Ok(Memory::Window(usize::MAX)),
            "none" => Ok(Memory::Window(1)),
            "log" => Ok(Memory::Log(WINDOW, LogSource::Captured)),
            "oracle" => Ok(Memory::Log(WINDOW, LogSource::Oracle)),
            n => match n.parse() {
                Ok(n) if n > 0 => Ok(Memory::Window(n)),
                _ => Err(format!("unknown memory strategy {n:?}")),
            },
        }
    }

    fn label(self) -> String {
        match self {
            Memory::Window(usize::MAX) => "all".into(),
            Memory::Window(1) => "none".into(),
            Memory::Window(n) => format!("w{n}"),
            Memory::Log(_, LogSource::Captured) => "log".into(),
            Memory::Log(_, LogSource::Oracle) => "oracle".into(),
        }
    }

    fn window(self) -> usize {
        match self {
            Memory::Window(n) | Memory::Log(n, _) => n,
        }
    }
}

/// The tools that let an agent look things up in the chat itself.
const RECALL_TOOLS: [&str; 3] = ["list_channels", "read_channel", "ask_worker"];

#[derive(Clone, Copy, Debug)]
struct Cell {
    memory: Memory,
    tools: bool,
}

impl Cell {
    fn label(self) -> String {
        format!("{}/{}", self.memory.label(), if self.tools { "tools" } else { "bare" })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grade {
    Correct,
    /// Says it doesn't know or can't find it.
    Unsure,
    Wrong,
    /// The reply failed.
    Error,
}

impl Grade {
    fn mark(self) -> &'static str {
        match self {
            Grade::Correct => "✓",
            Grade::Unsure => "?",
            Grade::Wrong => "✗",
            Grade::Error => "!",
        }
    }
}

const UNSURE: [&str; 24] = [
    "不確定", "不清楚", "不知道", "沒有紀錄", "沒有記錄", "找不到", "沒看到", "看不到", "無法確認", "記不得", "不記得",
    "查不到", "沒查到", "答不出", "沒辦法給", "不想猜", "不想亂猜", "不想用猜", "不想憑印象", "用猜的",
    "not sure", "don't know", "no record", "can't find",
];

/// Whether the agent looked something up in the chat (rather than trying a
/// tool this eval refuses).
fn looked_up(calls: &[String]) -> bool {
    calls.iter().any(|c| RECALL_TOOLS.iter().any(|t| c.starts_with(&format!("{t} "))))
}

fn grade(answer: &str, expect: &[Vec<String>]) -> Grade {
    let answer = answer.to_lowercase();
    let has = |alt: &String| answer.contains(&alt.to_lowercase());
    if expect.iter().all(|group| group.iter().any(has)) {
        Grade::Correct
    } else if UNSURE.iter().any(|u| answer.contains(u)) {
        Grade::Unsure
    } else {
        Grade::Wrong
    }
}

struct RunResult {
    grade: Grade,
    /// The fact's message was in what the model was sent.
    in_context: bool,
    /// The answer is in a decision the model was shown.
    in_log: bool,
    calls: Vec<String>,
    answer: String,
    error: Option<String>,
    usage: Usage,
    secs: f64,
}

fn request_text(req: &Request) -> String {
    let mut out = String::new();
    for turn in &req.turns {
        for part in &turn.parts {
            if let Part::Text(t) = part {
                out.push('\n');
                out.push_str(t);
            }
        }
    }
    out
}

fn run_one(sc: &Scenario, config: &Config, agent: &str, probe: &Probe, cell: Cell, log: &[Decision]) -> Result<RunResult, String> {
    let mut ws = build(sc);
    let mut agents = Agents::from_config(&mut ws, config);
    let a = find_agent(&agents, agent)?;
    if let Memory::Log(..) = cell.memory {
        ws.decisions = log.to_vec();
    }
    let (conv, upto) = ask(&mut ws, probe);
    agents.list[a].config.history = Some(cell.memory.window());
    let mut request = agents.request(a, &ws, conv, upto);
    if !cell.tools {
        request.tools.retain(|t| !RECALL_TOOLS.contains(&t.name));
    }
    let in_context = request_text(&request).contains(&probe.fact);
    let shown = agent::decisions(&ws);
    let in_log = !ws.decisions.is_empty() && grade(&shown, &probe.expect) == Grade::Correct;
    let author = agents.list[a].user;
    let mut host = EvalHost::new(ws, author, conv, data::now());
    let (outcome, secs) = reply(&agents, a, request, &mut host);
    let answer = host.answer();
    let (grade, error) = match outcome {
        Ok(Outcome::Replied) => (grade(&answer, &probe.expect), None),
        Ok(Outcome::Passed) => (Grade::Error, Some("the agent passed".into())),
        Err(e) => (Grade::Error, Some(e)),
    };
    Ok(RunResult { grade, in_context, in_log, calls: host.calls, answer, error, usage: host.usage, secs })
}

struct Options {
    agent: String,
    memories: Vec<Memory>,
    tools: Vec<bool>,
    probes: Option<Vec<String>>,
    runs: usize,
    log: Option<PathBuf>,
    capture: CaptureMode,
    out: Option<PathBuf>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut o = Options {
            agent: "PM".into(),
            memories: vec![Memory::Window(WINDOW), Memory::Window(usize::MAX)],
            tools: vec![true, false],
            probes: None,
            runs: 1,
            log: None,
            capture: CaptureMode::App,
            out: None,
        };
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let mut value = || it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
            let list = |v: String| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
            match flag.as_str() {
                "--agent" => o.agent = value()?,
                "--memory" => o.memories = list(value()?).iter().map(|s| Memory::parse(s)).collect::<Result<_, _>>()?,
                "--tools" => {
                    o.tools = list(value()?)
                        .iter()
                        .map(|s| match s.as_str() {
                            "on" => Ok(true),
                            "off" => Ok(false),
                            s => Err(format!("--tools takes on and/or off, not {s:?}")),
                        })
                        .collect::<Result<_, _>>()?
                }
                "--probe" => o.probes = Some(list(value()?)),
                "--runs" => o.runs = value()?.parse().map_err(|_| "--runs needs a number")?,
                "--log" => o.log = Some(PathBuf::from(value()?)),
                "--capture" => {
                    o.capture = match value()?.as_str() {
                        "app" => CaptureMode::App,
                        "forced" => CaptureMode::Forced,
                        s => return Err(format!("--capture takes app or forced, not {s:?}")),
                    }
                }
                "--out" => o.out = Some(PathBuf::from(value()?)),
                other => return Err(format!("unknown option {other:?}")),
            }
        }
        Ok(o)
    }

    fn out_dir(&self) -> Result<PathBuf, String> {
        let out = self.out.clone().unwrap_or_else(|| {
            PathBuf::from("target/memory-eval").join(Local::now().format("%Y%m%d-%H%M%S").to_string())
        });
        fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        Ok(out)
    }
}

fn load_config() -> Result<Config, String> {
    let path = store::config_path();
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut config = Config::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let keys: BTreeMap<String, String> = store::read_keys().map_err(|e| e.to_string())?;
    config.attach_keys(&keys);
    Ok(config)
}

fn describe_agent(config: &Config, agent: &str) -> Result<String, String> {
    let ac = config
        .agents
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(agent))
        .ok_or_else(|| format!("no agent named {agent} in config.toml"))?;
    Ok(format!("{} ({} / {})", ac.name, ac.provider, ac.model))
}

fn capture_only(o: &Options) -> Result<(), String> {
    let sc = scenario();
    let config = load_config()?;
    let out = o.out_dir()?;
    eprintln!("Capture ({:?}) with {}, one reply at a time.", o.capture, describe_agent(&config, &o.agent)?);
    let (log, rows) = capture(&sc, &config, &o.agent, o.capture, true)?;
    capture_report(&sc, o.capture, &rows, &log, &out)
}

fn run(o: Options) -> Result<(), String> {
    let sc = scenario();
    let config = load_config()?;
    let probes: Vec<&Probe> = match &o.probes {
        None => sc.probes.iter().collect(),
        Some(ids) => {
            let picked: Vec<&Probe> = sc.probes.iter().filter(|p| ids.contains(&p.id)).collect();
            if picked.len() != ids.len() {
                let known: Vec<&str> = sc.probes.iter().map(|p| p.id.as_str()).collect();
                return Err(format!("unknown probe in {ids:?}; probes: {}", known.join(", ")));
            }
            picked
        }
    };
    let cells: Vec<Cell> =
        o.memories.iter().flat_map(|&memory| o.tools.iter().map(move |&tools| Cell { memory, tools })).collect();
    let who = describe_agent(&config, &o.agent)?;
    let out = o.out_dir()?;

    // The captured log, if a cell needs it.
    let needs_log = o.memories.iter().any(|m| matches!(m, Memory::Log(_, LogSource::Captured)));
    let captured: Vec<Decision> = match (&o.log, needs_log) {
        (_, false) => Vec::new(),
        (Some(path), true) => {
            let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?
        }
        (None, true) => {
            eprintln!("Capture ({:?}) with {who} first, for --memory log.", o.capture);
            let (log, rows) = capture(&sc, &config, &o.agent, o.capture, true)?;
            capture_report(&sc, o.capture, &rows, &log, &out)?;
            log
        }
    };
    let oracle = oracle_log(&sc);

    let total = probes.len() * cells.len() * o.runs;
    eprintln!("\n{total} replies from {who}, one at a time. Transcripts: {}", out.display());

    let mut tsv =
        String::from("probe\ttag\tcell\trun\tgrade\tin_context\tin_log\ttool_calls\tsecs\tinput_tokens\toutput_tokens\n");
    // probe → cell → marks over runs
    let mut table: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut totals: Vec<[usize; 4]> = vec![[0; 4]; cells.len()];
    let mut done = 0;
    for p in &probes {
        let mut row = Vec::new();
        for (ci, &cell) in cells.iter().enumerate() {
            let log = match cell.memory {
                Memory::Log(_, LogSource::Captured) => &captured[..],
                Memory::Log(_, LogSource::Oracle) => &oracle[..],
                Memory::Window(_) => &[][..],
            };
            let mut marks = String::new();
            for n in 1..=o.runs {
                done += 1;
                eprint!("[{done}/{total}] {} {} #{n} … ", p.id, cell.label());
                let r = run_one(&sc, &config, &o.agent, p, cell, log)?;
                eprintln!("{} ({:.0}s)", r.grade.mark(), r.secs);
                totals[ci][r.grade as usize] += 1;
                marks.push_str(r.grade.mark());
                if looked_up(&r.calls) {
                    marks.push('*');
                }
                tsv.push_str(&format!(
                    "{}\t{}\t{}\t{n}\t{:?}\t{}\t{}\t{}\t{:.1}\t{}\t{}\n",
                    p.id,
                    p.tag.name(),
                    cell.label(),
                    r.grade,
                    r.in_context,
                    r.in_log,
                    r.calls.len(),
                    r.secs,
                    r.usage.total_input(),
                    r.usage.output
                ));
                let file = out.join(format!("{}__{}__{n}.md", p.id, cell.label().replace('/', "-")));
                fs::write(&file, transcript(p, cell, &r)).map_err(|e| format!("{}: {e}", file.display()))?;
            }
            row.push(marks);
        }
        table.push((p.id.clone(), p.tag.name().into(), row));
    }
    fs::write(out.join("results.tsv"), tsv).map_err(|e| e.to_string())?;

    // The summary table.
    let width = cells.iter().map(|c| c.label().len()).max().unwrap_or(0).max(o.runs * 2 + 1);
    print!("\n{:<20} {:<6}", "probe", "tag");
    for c in &cells {
        print!(" {:<width$}", c.label());
    }
    println!();
    for (id, tag, row) in &table {
        print!("{id:<20} {tag:<6}");
        for marks in row {
            print!(" {:<width$}", marks, width = width + marks.len() - marks.chars().count());
        }
        println!();
    }
    print!("{:<27}", "correct");
    for t in &totals {
        let n: usize = t.iter().sum();
        print!(" {:<width$}", format!("{}/{n}", t[Grade::Correct as usize]));
    }
    println!(
        "\n\n✓ correct  ? says it doesn't know  ✗ wrong  ! failed  * used chat tools\n\
         Transcripts and results.tsv: {}",
        out.display()
    );
    Ok(())
}

fn transcript(p: &Probe, cell: Cell, r: &RunResult) -> String {
    let mut s = format!(
        "# {} ({}) — {}\n\n**Grade:** {:?}  \n**Fact in context:** {}  \n**Answer in decision log:** {}  \n\
         **Time:** {:.1}s, {} in / {} out tokens\n\n**Expected:** {}\n\n**Question:** {}\n\n",
        p.id,
        p.tag.name(),
        cell.label(),
        r.grade,
        r.in_context,
        r.in_log,
        r.secs,
        r.usage.total_input(),
        r.usage.output,
        p.expect.iter().map(|g| g.join(" | ")).collect::<Vec<_>>().join("  AND  "),
        p.question
    );
    if !r.calls.is_empty() {
        s.push_str("**Tool calls:**\n");
        for c in &r.calls {
            s.push_str(&format!("- `{c}`\n"));
        }
        s.push('\n');
    }
    s.push_str(&format!("**Answer:**\n\n{}\n", r.answer));
    if let Some(e) = &r.error {
        s.push_str(&format!("\n**Error:** {e}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use pots_mock::{MockServer, Script};
    use serde_json::json;

    #[test]
    fn tags_match_how_far_back_each_fact_is() {
        let sc = scenario();
        assert!(sc.probes.len() >= 10);
        for p in &sc.probes {
            let ws = build(&sc);
            let found = locate_fact(&ws, &p.fact);
            assert_eq!(found.len(), 1, "{}: fact {:?} must be in exactly one message, found {found:?}", p.id, p.fact);
            let (_, d) = distance(&sc, p);
            match p.tag {
                Tag::Cross => assert!(matches!(p.place, Place::NewThread(_)), "{}: cross probes go in a new thread", p.id),
                tag => {
                    let d = d.unwrap_or_else(|| panic!("{}: fact isn't in the probe's channel", p.id));
                    assert_eq!(Tag::of(d), tag, "{}: the fact is {d} messages back", p.id);
                }
            }
        }
    }

    #[test]
    fn answers_dont_leak_into_what_the_agent_can_see_anyway() {
        let sc = scenario();
        for p in &sc.probes {
            let mut ws = build(&sc);
            let (conv, upto) = ask(&mut ws, p);
            let question = upto - 1;
            let leaked = |from: usize, to: usize, conv: Conv| {
                (from..to).find(|&ix| {
                    let text = ws.message(conv, ix).source.to_lowercase();
                    p.expect[0].iter().any(|alt| text.contains(&alt.to_lowercase()))
                })
            };
            let found = locate_fact(&ws, &p.fact)[0];
            match p.tag {
                // Nothing closer than the fact gives the answer away.
                Tag::Mid | Tag::Far => {
                    let reach = if p.tag == Tag::Mid { WINDOW } else { READ_CHANNEL_MAX };
                    let hit = leaked(question.saturating_sub(reach - 1), question, conv);
                    assert_eq!(hit, None, "{}: the answer shows up within {reach} messages of the question", p.id);
                }
                // The answer is only in the conversation the fact is in.
                Tag::Cross => {
                    for c in 0..ws.channels.len() {
                        let conv = Conv::Channel(c);
                        if conv != found.0 {
                            assert_eq!(leaked(0, ws.len(conv), conv), None, "{}: answer leaks into #{}", p.id, ws.channels[c].name);
                        }
                    }
                }
                Tag::Near => {}
            }
        }
    }

    #[test]
    fn the_oracle_log_answers_every_probe_an_owner_decided() {
        let sc = scenario();
        let log = oracle_log(&sc);
        let replaced: Vec<&Decision> = log.iter().filter(|d| d.retired.is_some()).collect();
        assert_eq!(replaced.len(), 1, "the rate limit changed once");
        assert!(replaced[0].text.contains("100"));
        let mut ws = build(&sc);
        ws.decisions = log;
        let shown = agent::decisions(&ws);
        assert!(!shown.contains("100 次"), "replaced decisions aren't shown: {shown}");
        for p in &sc.probes {
            // The engineer set the page size while implementing; nobody decided it.
            let expected = if p.id == "page-size" { Grade::Wrong } else { Grade::Correct };
            assert_eq!(grade(&shown, &p.expect), expected, "{}: {shown}", p.id);
        }
        let statuses = sc.points.iter().filter(|p| matches!(p.kind, PointKind::Status)).count();
        assert!(statuses >= 2, "capture also checks that status talk isn't recorded");
    }

    #[test]
    fn dates_expand_for_messages_and_answers() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 12).unwrap();
        assert_eq!(expand("on {date:+8}.", today, false), "on 11/20.");
        let alts = expand("{date:+8}", today, true);
        assert!(alts.split('|').any(|a| a == "11月20"));
        assert!(alts.split('|').any(|a| a == "2026-11-20"));
    }

    #[test]
    fn grades_correct_unsure_and_wrong() {
        let expect = vec![vec!["凍結".to_string(), "freeze".to_string()], vec!["v0.3".to_string()]];
        assert_eq!(grade("auth 在 v0.3 前凍結，先不要", &expect), Grade::Correct);
        assert_eq!(grade("Auth is under a FREEZE until v0.3", &expect), Grade::Correct);
        assert_eq!(grade("我不確定之前有沒有說過", &expect), Grade::Unsure);
        assert_eq!(grade("可以，我開任務", &expect), Grade::Wrong);
    }

    fn mock_config(server: &MockServer) -> Config {
        Config::parse(&format!(
            "[providers.mock]\napi = \"anthropic\"\nbase_url = \"{}\"\n\n\
             [[agents]]\nname = \"PM\"\nprovider = \"mock\"\nmodel = \"mock\"\n\n\
             [[agents]]\nname = \"Engineer\"\nprovider = \"mock\"\nmodel = \"mock\"\nworkspace = \"write\"\n",
            server.url()
        ))
        .unwrap()
    }

    #[test]
    fn a_probe_runs_through_the_reply_loop() {
        let sc = scenario();
        let probe = sc.probes.iter().find(|p| p.id == "sqlite-why").unwrap();
        let server = MockServer::start(
            Script::new()
                .tool("read_channel", json!({"channel": "general", "limit": 5}))
                .text("因為整個資料庫是一個檔案，可以放在 iCloud 同步。")
                .text("我不確定。")
                .text("iCloud"),
        );
        let config = mock_config(&server);

        let window = Cell { memory: Memory::Window(WINDOW), tools: true };
        let r = run_one(&sc, &config, "PM", probe, window, &[]).unwrap();
        assert_eq!(r.grade, Grade::Correct, "{:?}", r.error);
        assert!(!r.in_context, "a far fact is outside the default window");
        assert!(!r.in_log);
        assert_eq!(r.calls.len(), 1);
        assert!(looked_up(&r.calls));

        let bare_all = Cell { memory: Memory::Window(usize::MAX), tools: false };
        let r = run_one(&sc, &config, "PM", probe, bare_all, &[]).unwrap();
        assert_eq!(r.grade, Grade::Unsure);
        assert!(r.in_context, "the whole history is sent");
        let requests = server.requests();
        let tools = requests.last().unwrap().body["tools"].to_string();
        assert!(!tools.contains("read_channel"), "bare cells have no chat look-ups: {tools}");
        assert!(requests[0].body["tools"].to_string().contains("read_channel"));

        let oracle = Cell { memory: Memory::Log(WINDOW, LogSource::Oracle), tools: false };
        let r = run_one(&sc, &config, "PM", probe, oracle, &oracle_log(&sc)).unwrap();
        assert!(r.in_log && !r.in_context);
        assert!(server.requests().last().unwrap().body["system"].to_string().contains("iCloud Drive"));

        let err = run_one(&sc, &config, "Engineer", probe, window, &[]).err().unwrap();
        assert!(err.contains("workspace"));
    }

    #[test]
    fn forced_capture_replays_points_and_carries_the_log_forward() {
        let sc = scenario();
        let first = &sc.points[0];
        assert!(matches!(first.kind, PointKind::Decision { replaces: None, .. }));
        // The first point records a decision; the server echoes the rest.
        let server = MockServer::start(
            Script::new().tool(tools::RECORD_DECISION, json!({"decision": "資料庫用 SQLite，因為能放 iCloud 同步"})).text("記下了。"),
        );
        let config = mock_config(&server);
        let (log, rows) = capture(&sc, &config, "PM", CaptureMode::Forced, false).unwrap();
        assert_eq!(rows.len(), sc.points.len());
        assert!(rows[0].ok == Some(true) && rows[0].recorded == ["[0] 資料庫用 SQLite，因為能放 iCloud 同步"], "{:?}", rows[0].recorded);
        assert_eq!(log.len(), 1);
        let (ws, Some((conv, ix))) = build_until(&sc, first.item + 1) else { panic!() };
        assert_eq!(log[0].ts, ws.message(conv, ix).ts, "dated when it was decided");
        assert_eq!(log[0].place, "#general");
        for r in &rows[1..] {
            let status = matches!(r.point.as_ref().unwrap().kind, PointKind::Status);
            assert_eq!(r.ok, Some(status), "{}", r.id());
        }
        // Owner follow-ups without an @mention don't wake PM in the app.
        assert!(rows.iter().any(|r| !r.reached), "some decisions are stated where PM isn't woken");
        // Later points see what was recorded before.
        let later = server.requests().last().unwrap().body["system"].to_string();
        assert!(later.contains("資料庫用 SQLite，因為能放 iCloud 同步"), "{later}");
        // Only the first decision is in the log.
        let cov = coverage(&sc, &log);
        assert_eq!(cov.iter().filter(|(_, found)| *found).map(|(p, _)| p.id.as_str()).collect::<Vec<_>>(), ["sqlite"]);
    }

    #[test]
    fn app_capture_answers_only_what_wakes_the_agent() {
        let sc = scenario();
        let server = MockServer::start(Script::new());
        let config = mock_config(&server);
        let (log, rows) = capture(&sc, &config, "PM", CaptureMode::App, false).unwrap();
        assert!(log.is_empty());
        assert!(rows.len() > 10 && rows.iter().all(|r| r.reached && r.error.is_none()));
        assert_eq!(server.requests().len(), rows.len(), "one request per reply");
        // Every reply answers a message that @mentions PM (no DMs or PM-led
        // threads with owner messages in this scenario).
        for r in &rows {
            let Item::Message { text, .. } = &sc.items[r.item] else { panic!() };
            assert!(text.contains("@PM"), "{}: {text}", r.id());
        }
        // Marked points PM never sees aren't answered.
        let answered: Vec<usize> = rows.iter().map(|r| r.item).collect();
        assert!(sc.points.iter().any(|p| !answered.contains(&p.item)));
    }

    #[test]
    fn coverage_counts_decisions_in_force_by_their_key_words() {
        let sc = scenario();
        let cov = coverage(&sc, &oracle_log(&sc));
        assert!(cov.iter().all(|(_, found)| *found), "{:?}", cov.iter().map(|(p, f)| (&p.id, f)).collect::<Vec<_>>());
        assert!(!cov.iter().any(|(p, _)| p.id == "rate-100"), "replaced decisions aren't counted");
        assert_eq!(cov.len(), 11);
        assert!(coverage(&sc, &[]).iter().all(|(_, found)| !found));
    }
}
