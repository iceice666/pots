//! Workspace model, plus the empty and demo (seed) workspaces.
//!
//! The whole workspace is (de)serialized by `store`. Messages keep their
//! markdown source; the parsed `blocks` are rebuilt on load.

use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};

use pots_llm::Usage;
use crate::markdown::{self, Block};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserKind {
    #[default]
    Human,
    /// An integration that posts on its own (shown with an APP badge).
    Bot,
    /// An LLM-backed teammate configured in `config.toml`.
    Agent,
}

#[derive(Serialize, Deserialize)]
pub struct User {
    pub name: String,
    pub color: u32,
    pub online: bool,
    #[serde(default)]
    pub kind: UserKind,
}

impl User {
    fn new(name: &str, color: u32, online: bool, kind: UserKind) -> Self {
        Self { name: name.to_string(), color, online, kind }
    }

    /// Whether `@mention` refers to this user: the full name or its first
    /// word, ignoring case (`@Mara` and `@mara` both mean "Mara Chen").
    pub fn answers_to(&self, mention: &str) -> bool {
        self.name.eq_ignore_ascii_case(mention)
            || self.name.split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case(mention))
    }
}

#[derive(Serialize, Deserialize)]
pub struct Reaction {
    pub emoji: String,
    pub count: u32,
    pub mine: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Message {
    pub id: usize,
    pub author: usize,
    /// Unix time in seconds.
    pub ts: i64,
    /// Markdown source; `blocks` is parsed from it.
    pub source: String,
    #[serde(skip)]
    pub blocks: Vec<Block>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    /// The thread started from this message, if any.
    #[serde(default)]
    pub thread: Option<usize>,
    /// An agent is still writing this message.
    #[serde(default)]
    pub pending: bool,
    /// Why an agent's reply failed or stopped short.
    #[serde(default)]
    pub error: Option<String>,
    /// A remark from the app about the message (e.g. why nobody answered it).
    #[serde(default)]
    pub note: Option<String>,
    /// Something an agent asks the owner to act on.
    #[serde(default)]
    pub card: Option<Card>,
    /// Tokens an agent's reply used, and what that cost if the price is known.
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub cost: Option<f64>,
    /// Tokens the reply's worker asks used; what they cost is in `cost`.
    #[serde(default)]
    pub worker_usage: Option<Usage>,
    /// What an agent is doing right now ("Reading #engineering…"). Not saved.
    #[serde(skip)]
    pub activity: Option<String>,
}

impl Message {
    fn new(id: usize, author: usize, ts: i64, source: &str) -> Self {
        Self {
            id,
            author,
            ts,
            source: source.to_string(),
            blocks: markdown::parse(source),
            reactions: Vec::new(),
            thread: None,
            pending: false,
            error: None,
            note: None,
            card: None,
            usage: None,
            cost: None,
            worker_usage: None,
            activity: None,
        }
    }

    pub fn set_source(&mut self, source: String) {
        self.blocks = markdown::parse(&source);
        self.source = source;
    }

    pub fn push_source(&mut self, text: &str) {
        let mut source = std::mem::take(&mut self.source);
        source.push_str(text);
        self.set_source(source);
    }

    /// `@mentions` in the message (outside code).
    pub fn mentions(&self) -> Vec<String> {
        markdown::mentions(&self.blocks)
    }

    /// The message's text on one line, cut to `max` characters.
    pub fn snippet(&self, max: usize) -> String {
        let text = markdown::fragments(&self.blocks)
            .iter()
            .map(|f| f.text.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().count() <= max {
            text
        } else {
            let cut: String = text.chars().take(max + 1).collect();
            // Break at the last word boundary, unless that throws most of it away.
            let cut = match cut.rfind(' ') {
                Some(i) if i >= cut.len() / 2 => &cut[..i],
                _ => cut.char_indices().nth(max).map_or(&cut[..], |(i, _)| &cut[..i]),
            };
            format!("{}…", cut.trim_end())
        }
    }
}

/// An agent's request for the owner, shown under its message with buttons.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Card {
    /// Pick one of a few options.
    Choice {
        question: String,
        options: Vec<String>,
        recommended: Option<usize>,
        answer: Option<usize>,
    },
    /// Accept a task branch into the project repository.
    Merge {
        branch: String,
        /// The clone holding the branch.
        source: String,
        summary: String,
        commits: Vec<String>,
        /// The repository the branch is accepted into (the one the task
        /// cloned). Older cards lack it: their channel's repo is used.
        #[serde(default)]
        repo: Option<String>,
        /// Other work it conflicts with when it was requested, in markdown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conflicts: Option<String>,
        /// What happened when the owner answered.
        outcome: Option<MergeOutcome>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeOutcome {
    Approved(String),
    Rejected,
    Failed(String),
}

impl Card {
    pub fn is_open(&self) -> bool {
        match self {
            Card::Choice { answer, .. } => answer.is_none(),
            Card::Merge { outcome, .. } => outcome.is_none() || matches!(outcome, Some(MergeOutcome::Failed(_))),
        }
    }
}

/// A Discord-style thread: a side conversation started from one channel
/// message (the *starter*), which stays in the channel.
#[derive(Serialize, Deserialize)]
pub struct Thread {
    pub name: String,
    pub channel: usize,
    /// Index of the starter message in the parent channel.
    pub starter: usize,
    pub messages: Vec<Message>,
}

/// A message list: a channel, or a thread. In a thread, item 0 is the
/// starter message and items 1.. are the thread's own messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Conv {
    Channel(usize),
    Thread(usize),
}

#[derive(Serialize, Deserialize)]
pub struct Channel {
    pub name: String,
    pub topic: String,
    /// `Some(user)` for a direct message conversation.
    pub dm: Option<usize>,
    /// The git repository work asked for here happens in, as written in
    /// `[[channels]]` of config.toml (`~` not yet expanded). Set on load.
    #[serde(default)]
    pub repo: Option<String>,
    pub unread: bool,
    pub messages: Vec<Message>,
}

#[derive(Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    pub me: usize,
    pub users: Vec<User>,
    pub channels: Vec<Channel>,
    pub threads: Vec<Thread>,
    next_id: usize,
}

impl Workspace {
    /// Rebuild what isn't saved, after loading. A reply that was still being
    /// written when the app quit can't be resumed, so it's marked as cut off.
    pub fn restore(&mut self) {
        let messages = self
            .channels
            .iter_mut()
            .flat_map(|c| c.messages.iter_mut())
            .chain(self.threads.iter_mut().flat_map(|t| t.messages.iter_mut()));
        for msg in messages {
            msg.blocks = markdown::parse(&msg.source);
            if msg.pending {
                msg.pending = false;
                msg.error.get_or_insert_with(|| "Interrupted: the app quit before this reply finished.".into());
            }
        }
    }

    /// Where message `id` is: its conversation and index there. A thread
    /// starter is reported in its channel.
    pub fn locate(&self, id: usize) -> Option<(Conv, usize)> {
        for (c, channel) in self.channels.iter().enumerate() {
            if let Some(ix) = channel.messages.iter().position(|m| m.id == id) {
                return Some((Conv::Channel(c), ix));
            }
        }
        for (t, thread) in self.threads.iter().enumerate() {
            if let Some(ix) = thread.messages.iter().position(|m| m.id == id) {
                return Some((Conv::Thread(t), ix + 1));
            }
        }
        None
    }

    pub fn message_by_id_mut(&mut self, id: usize) -> Option<&mut Message> {
        let (conv, ix) = self.locate(id)?;
        Some(self.message_mut(conv, ix))
    }

    pub fn find_channel(&self, name: &str) -> Option<usize> {
        let name = name.trim_start_matches('#');
        self.channels.iter().position(|c| c.dm.is_none() && c.name.eq_ignore_ascii_case(name))
    }

    /// `name` made into a channel name the way Slack does it (`#` dropped,
    /// lowercase, spaces as `-`), or why it can't be one. `except` is the
    /// channel being renamed, which may keep its name.
    pub fn channel_name(&self, name: &str, except: Option<usize>) -> Result<String, String> {
        let name = name.trim().trim_start_matches('#').trim().to_lowercase();
        let name = name.split_whitespace().collect::<Vec<_>>().join("-");
        if name.is_empty() {
            return Err("Give the channel a name.".into());
        }
        if !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
            return Err("Use letters, digits, - and _ only.".into());
        }
        match self.find_channel(&name) {
            Some(c) if Some(c) != except => Err(format!("#{name} already exists.")),
            _ => Ok(name),
        }
    }

    /// A new, empty channel named `name`.
    pub fn add_channel(&mut self, name: &str) -> usize {
        self.channels.push(Channel {
            name: name.to_string(),
            topic: String::new(),
            dm: None,
            repo: None,
            unread: false,
            messages: Vec::new(),
        });
        self.channels.len() - 1
    }

    /// The direct-message channel with `user`, created if missing.
    pub fn ensure_dm(&mut self, user: usize) -> usize {
        if let Some(c) = self.channels.iter().position(|c| c.dm == Some(user)) {
            return c;
        }
        self.channels.push(Channel {
            name: self.users[user].name.clone(),
            topic: String::new(),
            dm: Some(user),
            repo: None,
            unread: false,
            messages: Vec::new(),
        });
        self.channels.len() - 1
    }

    pub fn len(&self, conv: Conv) -> usize {
        match conv {
            Conv::Channel(c) => self.channels[c].messages.len(),
            Conv::Thread(t) => 1 + self.threads[t].messages.len(),
        }
    }

    pub fn message(&self, conv: Conv, ix: usize) -> &Message {
        match conv {
            Conv::Channel(c) => &self.channels[c].messages[ix],
            Conv::Thread(t) => {
                let thread = &self.threads[t];
                match ix {
                    0 => &self.channels[thread.channel].messages[thread.starter],
                    _ => &thread.messages[ix - 1],
                }
            }
        }
    }

    pub fn message_mut(&mut self, conv: Conv, ix: usize) -> &mut Message {
        match conv {
            Conv::Channel(c) => &mut self.channels[c].messages[ix],
            Conv::Thread(t) => {
                let thread = &mut self.threads[t];
                match ix {
                    0 => &mut self.channels[thread.channel].messages[thread.starter],
                    _ => &mut thread.messages[ix - 1],
                }
            }
        }
    }

    /// All messages of `conv`, in order.
    pub fn messages(&self, conv: Conv) -> Vec<&Message> {
        (0..self.len(conv)).map(|ix| self.message(conv, ix)).collect()
    }

    /// Append a message, timestamped now; returns its index within `conv`.
    pub fn post(&mut self, conv: Conv, author: usize, source: &str) -> usize {
        let len = self.len(conv);
        // Never before the previous message, even if the clock went back.
        let ts = match len {
            0 => now(),
            _ => now().max(self.message(conv, len - 1).ts),
        };
        let msg = Message::new(self.next_id, author, ts, source);
        self.next_id += 1;
        match conv {
            Conv::Channel(c) => self.channels[c].messages.push(msg),
            Conv::Thread(t) => self.threads[t].messages.push(msg),
        }
        len
    }

    /// Take message `ix` out of `conv` (a reply that ended up saying
    /// nothing). A message with a thread, or a thread's starter, stays.
    pub fn remove(&mut self, conv: Conv, ix: usize) -> Option<Message> {
        match conv {
            Conv::Thread(_) if ix == 0 => None,
            Conv::Thread(t) => Some(self.threads[t].messages.remove(ix - 1)),
            Conv::Channel(c) => {
                if self.channels[c].messages[ix].thread.is_some() {
                    return None;
                }
                let msg = self.channels[c].messages.remove(ix);
                for thread in self.threads.iter_mut().filter(|t| t.channel == c && t.starter > ix) {
                    thread.starter -= 1;
                }
                Some(msg)
            }
        }
    }

    /// Start a thread from message `starter` of `channel`. Returns the
    /// existing thread if that message already has one.
    pub fn create_thread(&mut self, channel: usize, starter: usize, name: &str) -> usize {
        if let Some(t) = self.channels[channel].messages[starter].thread {
            return t;
        }
        let t = self.threads.len();
        self.threads.push(Thread {
            name: name.to_string(),
            channel,
            starter,
            messages: Vec::new(),
        });
        self.channels[channel].messages[starter].thread = Some(t);
        t
    }

    /// Start a thread straight from `channel`, with no message to start it
    /// from: `author` posts its name in the channel as the starter, the way
    /// Discord announces a new thread. Returns (starter index, thread).
    pub fn new_thread(&mut self, channel: usize, author: usize, name: &str) -> (usize, usize) {
        let starter = self.post(Conv::Channel(channel), author, &format!("**{name}**"));
        (starter, self.create_thread(channel, starter, name))
    }

    /// Threads of `channel`, most recently active first (like Discord's sidebar).
    pub fn channel_threads(&self, channel: usize) -> Vec<usize> {
        let mut ts: Vec<usize> = (0..self.threads.len())
            .filter(|&t| self.threads[t].channel == channel)
            .collect();
        ts.sort_by_key(|&t| std::cmp::Reverse(self.message(Conv::Thread(t), self.len(Conv::Thread(t)) - 1).ts));
        ts
    }
}

/// Default thread name for a starter message, as Discord pre-fills it.
pub fn default_thread_name(msg: &Message) -> String {
    let name = msg.snippet(40);
    if name.is_empty() { "New Thread".to_string() } else { name }
}

pub fn now() -> i64 {
    Local::now().timestamp()
}

/// Local wall-clock time of `ts`, like "9:05 AM".
pub fn format_time(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0).single() {
        Some(t) => t.format("%-I:%M %p").to_string(),
        None => String::new(),
    }
}

/// Unix time of `minutes` past midnight yesterday: seed content is always
/// in the past, so anything posted now sorts after it.
fn seed_time(minutes: u32) -> i64 {
    let day = Local::now().date_naive().pred_opt().unwrap();
    let time = day.and_hms_opt(minutes / 60, minutes % 60, 0).unwrap();
    Local.from_local_datetime(&time).earliest().map_or(0, |t| t.timestamp())
}

/// A fresh workspace: just you and an empty #general (agents post there,
/// and the app needs a channel to show).
pub fn empty() -> Workspace {
    let mut ws = Workspace {
        name: "Pots".to_string(),
        me: 0,
        users: vec![User::new("you", 0x2b6cb0, true, UserKind::Human)],
        channels: Vec::new(),
        threads: Vec::new(),
        next_id: 0,
    };
    ws.channels.push(Channel {
        name: "general".to_string(),
        topic: String::new(),
        dm: None,
        repo: None,
        unread: false,
        messages: Vec::new(),
    });
    ws
}

/// Demo content (people, channels, messages), shown only with `pots --mock`.
pub fn seed() -> Workspace {
    let users = vec![
        User::new("you", 0x2b6cb0, true, UserKind::Human),
        User::new("Mara Chen", 0xd53f8c, true, UserKind::Human),
        User::new("Leo Okafor", 0x38a169, true, UserKind::Human),
        User::new("Priya Nair", 0xdd6b20, false, UserKind::Human),
        User::new("deploy-bot", 0x6b46c1, true, UserKind::Bot),
    ];

    let mut ws = Workspace {
        name: "Pots".to_string(),
        me: 0,
        users,
        channels: Vec::new(),
        threads: Vec::new(),
        next_id: 0,
    };

    let add = |ws: &mut Workspace, name: &str, topic: &str, dm: Option<usize>, unread: bool, msgs: &[(usize, u32, &str, &[(&str, u32, bool)])]| {
        let mut messages = Vec::new();
        for (author, minutes, src, reactions) in msgs {
            let mut msg = Message::new(ws.next_id, *author, seed_time(*minutes), src);
            msg.reactions = reactions
                .iter()
                .map(|(e, c, mine)| Reaction {
                    emoji: markdown::emoji(e).to_string(),
                    count: *c,
                    mine: *mine,
                })
                .collect();
            messages.push(msg);
            ws.next_id += 1;
        }
        ws.channels.push(Channel {
            name: name.to_string(),
            topic: topic.to_string(),
            dm,
            repo: None,
            unread,
            messages,
        });
    };

    add(&mut ws, "general", "Company-wide announcements and work-based matters", None, false, &[
        (1, 9 * 60 + 2, "Good morning everyone :wave:\nQuick reminder that **all-hands** is at *2:00 PM* today.", &[("wave", 4, false)]),
        (2, 9 * 60 + 3, "Thanks @Mara! Will the recording be posted in #announcements?", &[]),
        (1, 9 * 60 + 5, "Yep — it'll be up by end of day. Agenda is in the doc: [All-hands Q4](https://example.com/all-hands)", &[("+1", 3, true)]),
        (3, 9 * 60 + 41, "## Office update :sparkles:\n\nThe new coffee machine has arrived :coffee:\n\n- Espresso, flat white, and ~~decaf~~ *hot chocolate*\n- Please **descale it on Fridays**\n- Questions → ask @Leo", &[("coffee", 7, true), ("tada", 2, false)]),
        (2, 9 * 60 + 44, "> Please descale it on Fridays\n\nI volunteer as tribute :joy:", &[("joy", 5, false)]),
    ]);

    add(&mut ws, "engineering", "Ship it :rocket: · Release train leaves Thursdays", None, true, &[
        (4, 10 * 60 + 12, "**Release `v2.14.0` deployed to production** :white_check_mark:\n\n| Service | Version | Status | p95 latency |\n|:--|:--:|:--|--:|\n| api-gateway | `2.14.0` | ✅ healthy | 112 ms |\n| auth | `2.14.0` | ✅ healthy | 38 ms |\n| search | `2.13.4` | ⚠️ *degraded* | 870 ms |\n| billing | `2.14.0` | ✅ healthy | 64 ms |", &[("rocket", 3, false), ("eyes", 1, true)]),
        (2, 10 * 60 + 14, "Search is still on `2.13.4` — looks like the index migration is stuck. I'm on it :eyes:", &[]),
        (2, 10 * 60 + 15, "Repro:\n\n```rust\nlet index = Index::open(path)?;\nindex.migrate(Schema::V7)\n    .with_batch_size(10_000)\n    .run()?; // hangs at batch 42\n```", &[]),
        (3, 10 * 60 + 31, "Probably the batch size. We hit the same thing last quarter:\n\n> Migrations over 5k docs/batch exceed the lock timeout on the old shards.\n\nTry `--batch-size 2500` and it should go through.", &[("pray", 2, false)]),
        (2, 10 * 60 + 52, "That fixed it :tada: Benchmarks before/after:\n\n| Batch size | Docs/sec | Lock waits | Result |\n|--:|--:|--:|:--|\n| 10,000 | 0 | 312 | **stuck** |\n| 5,000 | 4,120 | 18 | slow |\n| 2,500 | 9,870 | 0 | ✅ **done** |\n\nFollow-ups:\n1. Make `2500` the default\n2. Add an alert on lock waits > 50\n3. Write a postmortem", &[("100", 4, true), ("fire", 2, false)]),
        (1, 11 * 60 + 3, "Nice work! Tracking the follow-ups here:\n\n- [x] Lower default batch size\n- [x] Deploy `search 2.14.0`\n- [ ] Lock-wait alert\n  - needs a new metric in the exporter\n- [ ] Postmortem — owner @Leo", &[]),
    ]);

    add(&mut ws, "design", "Pixels, prototypes & critique", None, true, &[
        (3, 13 * 60 + 20, "### Type scale proposal\n\n| Token | Size | Weight | Line height |\n|:--|--:|:--:|--:|\n| `display` | 32 px | Bold | 40 px |\n| `title` | 22 px | Bold | 28 px |\n| `body` | 15 px | Regular | 22 px |\n| `caption` | 12 px | Medium | 16 px |\n\nThoughts? I'd like to lock this by **Friday**.", &[("eyes", 2, false)]),
        (1, 13 * 60 + 26, "Love it. Maybe bump `caption` to *13 px* for accessibility?", &[("+1", 2, false)]),
    ]);

    add(&mut ws, "random", "Non-work banter and water cooler conversation", None, false, &[
        (2, 12 * 60 + 1, "Lunch spot ranking, *scientifically determined*:\n\n1. Ramen place on 5th :fire:\n2. The taco truck\n3. ~~Salad bar~~ (closed, RIP)", &[("100", 3, false)]),
        (3, 12 * 60 + 4, "Strong disagree on #2 but I respect the methodology :thinking_face:", &[]),
    ]);

    add(&mut ws, "Mara Chen", "", Some(1), false, &[
        (1, 15 * 60 + 2, "Hey! Do you have a minute to look at the onboarding copy? Draft is at https://example.com/onboarding-draft", &[]),
    ]);
    add(&mut ws, "Leo Okafor", "", Some(2), false, &[
        (2, 16 * 60 + 40, "Pushed the fix — can you review when you get a chance? :pray:", &[]),
    ]);
    add(&mut ws, "deploy-bot", "", Some(4), false, &[
        (4, 8 * 60, "Nightly build **#1284** passed in `14m 02s` :white_check_mark:", &[]),
    ]);

    // A thread already running off the stuck-migration report in #engineering.
    let eng = ws.channels.iter().position(|c| c.name == "engineering").unwrap();
    let t = ws.create_thread(eng, 1, "search index migration");
    for (author, minutes, src) in [
        (3, 10 * 60 + 18, "Which shard is it stuck on? `search-07` was flaky last week too."),
        (2, 10 * 60 + 20, "`search-04` and `search-07` — both on the **old storage nodes** :thinking_face:"),
        (1, 10 * 60 + 47, "Lowering the batch size worked, details in the channel :tada:"),
    ] {
        let msg = Message::new(ws.next_id, author, seed_time(minutes), src);
        ws.next_id += 1;
        ws.threads[t].messages.push(msg);
    }

    ws
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_starts_from_a_message_and_shows_it_first() {
        let mut ws = seed();
        let general = ws.channels.iter().position(|c| c.name == "general").unwrap();
        let t = ws.create_thread(general, 3, "coffee machine");
        assert_eq!(ws.channels[general].messages[3].thread, Some(t));
        // Starting again from the same message reuses the thread.
        assert_eq!(ws.create_thread(general, 3, "other"), t);

        assert_eq!(ws.len(Conv::Thread(t)), 1);
        assert_eq!(ws.message(Conv::Thread(t), 0).id, ws.channels[general].messages[3].id);

        let channel_len = ws.len(Conv::Channel(general));
        assert_eq!(ws.post(Conv::Thread(t), ws.me, "who descales it?"), 1);
        assert_eq!(ws.len(Conv::Thread(t)), 2);
        // Replies stay out of the channel.
        assert_eq!(ws.len(Conv::Channel(general)), channel_len);
        let reply = ws.message(Conv::Thread(t), 1);
        assert_eq!(reply.snippet(80), "who descales it?");
        assert!(reply.ts >= ws.channels[general].messages[3].ts);
    }

    #[test]
    fn new_thread_needs_no_message_to_start_from() {
        let mut ws = seed();
        let general = 0;
        let before = ws.len(Conv::Channel(general));
        let (starter, t) = ws.new_thread(general, ws.me, "lunch plans");
        // Its name is announced in the channel, and that is its starter.
        assert_eq!(starter, before);
        assert_eq!(ws.len(Conv::Channel(general)), before + 1);
        assert_eq!(ws.channels[general].messages[starter].thread, Some(t));
        assert_eq!(ws.channels[general].messages[starter].author, ws.me);
        assert_eq!(ws.threads[t].name, "lunch plans");
        assert_eq!(ws.message(Conv::Thread(t), 0).source, "**lunch plans**");
        assert_eq!(ws.len(Conv::Thread(t)), 1);
    }

    #[test]
    fn threads_sort_by_latest_activity() {
        let mut ws = seed();
        let eng = ws.channels.iter().position(|c| c.name == "engineering").unwrap();
        let seeded = ws.channel_threads(eng);
        assert_eq!(seeded.len(), 1);
        let t = ws.create_thread(eng, 5, "follow-ups");
        ws.post(Conv::Thread(t), ws.me, "on it");
        assert_eq!(ws.channel_threads(eng), vec![t, seeded[0]]);
    }

    #[test]
    fn channel_names_are_normalized_and_unique() {
        let ws = seed();
        assert_eq!(ws.channel_name(" #Data Science ", None), Ok("data-science".into()));
        assert!(ws.channel_name("  ", None).is_err());
        assert!(ws.channel_name("a/b", None).unwrap_err().contains("letters"));
        assert_eq!(ws.channel_name("General", None), Err("#general already exists.".into()));
        let general = ws.find_channel("general");
        assert_eq!(ws.channel_name("General", general), Ok("general".into()), "renaming to itself");
    }

    #[test]
    fn default_thread_name_is_a_short_snippet() {
        let ws = seed();
        let general = &ws.channels[0];
        assert_eq!(default_thread_name(&general.messages[0]), "Good morning everyone 👋 Quick reminder…");
    }
}
