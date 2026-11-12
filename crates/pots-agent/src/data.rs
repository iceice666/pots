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

    /// The message's text as shown (no markdown markers), on one line.
    pub fn plain_text(&self) -> String {
        let text = markdown::fragments(&self.blocks)
            .iter()
            .map(|f| f.text.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The message's text on one line, cut to `max` characters.
    pub fn snippet(&self, max: usize) -> String {
        let text = self.plain_text();
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
    /// Archived (closed) by the user: hidden from the sidebar, still reachable
    /// from its card under the starter. A new message reopens it, as in Discord.
    #[serde(default)]
    pub archived: bool,
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

/// The kinds of artifact. Each has its own content type in
/// [`ArtifactContent`] and its own editor in the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    Table,
}

impl ArtifactKind {
    /// Every kind, in the order the app offers them.
    pub const ALL: &[ArtifactKind] = &[ArtifactKind::Table];

    /// The kind's name, as shown in the app ("Table").
    pub fn label(self) -> &'static str {
        match self {
            ArtifactKind::Table => "Table",
        }
    }
}

/// What an artifact holds. Saved apart from `workspace.json`, one file
/// per artifact (see `store`), tagged with its kind:
/// `{"kind": "table", "columns": [...], "rows": [[...], ...]}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ArtifactContent {
    Table(Table),
}

impl ArtifactContent {
    /// A new, blank artifact of `kind`.
    pub fn new(kind: ArtifactKind) -> Self {
        match kind {
            ArtifactKind::Table => ArtifactContent::Table(Table::blank(DEFAULT_TABLE_COLUMNS, DEFAULT_TABLE_ROWS)),
        }
    }

    pub fn kind(&self) -> ArtifactKind {
        match self {
            ArtifactContent::Table(_) => ArtifactKind::Table,
        }
    }
}

/// Size of a new table.
pub const DEFAULT_TABLE_COLUMNS: usize = 3;
pub const DEFAULT_TABLE_ROWS: usize = 5;

/// A table of plain-text cells under named columns. Every row has exactly
/// one cell per column (a file that doesn't is padded or cut on load).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "TableFile")]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// A table as written in its file, before its rows are made to fit the columns.
#[derive(Deserialize)]
struct TableFile {
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    rows: Vec<Vec<String>>,
}

impl From<TableFile> for Table {
    fn from(file: TableFile) -> Self {
        let mut table = Table { columns: file.columns, rows: file.rows };
        let width = table.columns.len();
        for row in &mut table.rows {
            row.resize(width, String::new());
        }
        table
    }
}

impl Table {
    /// `columns` × `rows` empty cells, the columns named "Column 1", "Column 2"…
    pub fn blank(columns: usize, rows: usize) -> Self {
        Table {
            columns: (1..=columns).map(|i| format!("Column {i}")).collect(),
            rows: vec![vec![String::new(); columns]; rows],
        }
    }

    pub fn cell(&self, row: usize, col: usize) -> &str {
        self.rows.get(row).and_then(|r| r.get(col)).map_or("", String::as_str)
    }

    /// Set a cell's text; false if there is no such cell or it already says that.
    pub fn set_cell(&mut self, row: usize, col: usize, text: &str) -> bool {
        match self.rows.get_mut(row).and_then(|r| r.get_mut(col)) {
            Some(cell) if cell != text => {
                *cell = text.to_string();
                true
            }
            _ => false,
        }
    }

    /// Insert an empty row before row `at` (at the end if past it); returns its index.
    pub fn insert_row(&mut self, at: usize) -> usize {
        let at = at.min(self.rows.len());
        self.rows.insert(at, vec![String::new(); self.columns.len()]);
        at
    }

    pub fn remove_row(&mut self, row: usize) -> bool {
        if row < self.rows.len() {
            self.rows.remove(row);
            true
        } else {
            false
        }
    }

    /// Insert an empty column named `name` before column `at` (at the end
    /// if past it); returns its index.
    pub fn insert_column(&mut self, at: usize, name: &str) -> usize {
        let at = at.min(self.columns.len());
        self.columns.insert(at, name.to_string());
        for row in &mut self.rows {
            row.insert(at, String::new());
        }
        at
    }

    /// Remove column `col` and its cells. The last column stays: a table
    /// without columns would have rows with nothing in them.
    pub fn remove_column(&mut self, col: usize) -> bool {
        if col >= self.columns.len() || self.columns.len() == 1 {
            return false;
        }
        self.columns.remove(col);
        for row in &mut self.rows {
            row.remove(col);
        }
        true
    }

    /// Rename column `col`; a blank name is refused (false).
    pub fn rename_column(&mut self, col: usize, name: &str) -> bool {
        let name = name.trim();
        match self.columns.get_mut(col) {
            Some(c) if !name.is_empty() && c != name => {
                *c = name.to_string();
                true
            }
            _ => false,
        }
    }

    /// A name for a new column that no column has yet ("Column 4").
    pub fn next_column_name(&self) -> String {
        (self.columns.len() + 1..)
            .map(|i| format!("Column {i}"))
            .find(|n| !self.columns.contains(n))
            .expect("some number is free")
    }
}

/// Something made and kept in a channel: a table now, documents and
/// whiteboards later. The metadata lives in `workspace.json`; the content
/// is in its own file (see `store`), loaded with the workspace.
#[derive(Serialize, Deserialize)]
pub struct Artifact {
    /// Stable for the artifact's life (it names its file); not an index.
    pub id: usize,
    pub title: String,
    pub kind: ArtifactKind,
    pub channel: usize,
    /// `None` when its file couldn't be read (see `load_error`): the app
    /// then won't open it, so the file is never overwritten.
    #[serde(skip)]
    pub content: Option<ArtifactContent>,
    #[serde(skip)]
    pub load_error: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    pub me: usize,
    pub users: Vec<User>,
    pub channels: Vec<Channel>,
    pub threads: Vec<Thread>,
    next_id: usize,
    /// Workspaces saved before artifacts existed have none.
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    next_artifact_id: usize,
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
            Conv::Thread(t) => {
                let thread = &mut self.threads[t];
                thread.messages.push(msg);
                thread.archived = false;
            }
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
            archived: false,
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

    /// Whether thread `t` still has a card (a question, a merge request)
    /// waiting on the owner.
    pub fn waits_on_owner(&self, t: usize) -> bool {
        self.threads[t].messages.iter().any(|m| m.card.as_ref().is_some_and(Card::is_open))
    }

    /// Threads of `channel` that aren't archived, for the sidebar.
    pub fn open_threads(&self, channel: usize) -> Vec<usize> {
        let mut ts = self.channel_threads(channel);
        ts.retain(|&t| !self.threads[t].archived);
        ts
    }

    // --- artifacts -------------------------------------------------------

    pub fn artifact(&self, id: usize) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.id == id)
    }

    pub fn artifact_mut(&mut self, id: usize) -> Option<&mut Artifact> {
        self.artifacts.iter_mut().find(|a| a.id == id)
    }

    /// Ids of `channel`'s artifacts, oldest first.
    pub fn channel_artifacts(&self, channel: usize) -> Vec<usize> {
        self.artifacts.iter().filter(|a| a.channel == channel).map(|a| a.id).collect()
    }

    /// A new, blank artifact of `kind` in `channel`, titled "Untitled
    /// table" (numbered if the channel has one by that name). Returns its id.
    pub fn add_artifact(&mut self, channel: usize, kind: ArtifactKind) -> usize {
        // Never reuse an id, even with a counter from an older file.
        let id = self.next_artifact_id.max(self.artifacts.iter().map(|a| a.id + 1).max().unwrap_or(0));
        self.next_artifact_id = id + 1;
        let base = format!("Untitled {}", kind.label().to_lowercase());
        let taken = |title: &str| self.artifacts.iter().any(|a| a.channel == channel && a.title == title);
        let title = std::iter::once(base.clone())
            .chain((2..).map(|n| format!("{base} {n}")))
            .find(|t| !taken(t))
            .expect("some number is free");
        self.artifacts.push(Artifact {
            id,
            title,
            kind,
            channel,
            content: Some(ArtifactContent::new(kind)),
            load_error: None,
        });
        id
    }

    /// Rename artifact `id`. Surrounding space is dropped; a blank title is
    /// refused. Returns the title it has now, if it exists.
    pub fn rename_artifact(&mut self, id: usize, title: &str) -> Option<String> {
        let artifact = self.artifact_mut(id)?;
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if !title.is_empty() {
            artifact.title = title;
        }
        Some(artifact.title.clone())
    }

    pub fn remove_artifact(&mut self, id: usize) -> Option<Artifact> {
        let ix = self.artifacts.iter().position(|a| a.id == id)?;
        Some(self.artifacts.remove(ix))
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
        artifacts: Vec::new(),
        next_artifact_id: 0,
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
        artifacts: Vec::new(),
        next_artifact_id: 0,
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
    fn archived_threads_leave_the_sidebar_until_someone_posts() {
        let mut ws = seed();
        let eng = ws.find_channel("engineering").unwrap();
        let t = ws.channel_threads(eng)[0];
        ws.threads[t].archived = true;
        assert!(!ws.open_threads(eng).contains(&t));
        assert!(ws.channel_threads(eng).contains(&t), "still a thread of the channel");
        ws.post(Conv::Thread(t), ws.me, "one more thing");
        assert!(!ws.threads[t].archived, "a new message reopens it");
        assert!(ws.open_threads(eng).contains(&t));
    }

    #[test]
    fn a_thread_waits_on_the_owner_while_a_card_is_unanswered() {
        let mut ws = seed();
        let eng = ws.find_channel("engineering").unwrap();
        let t = ws.channel_threads(eng)[0];
        assert!(!ws.waits_on_owner(t));
        let ix = ws.post(Conv::Thread(t), 1, "which one?");
        let choice = |answer| Card::Choice { question: "?".into(), options: vec!["a".into(), "b".into()], recommended: None, answer };
        ws.message_mut(Conv::Thread(t), ix).card = Some(choice(None));
        assert!(ws.waits_on_owner(t));
        ws.message_mut(Conv::Thread(t), ix).card = Some(choice(Some(0)));
        assert!(!ws.waits_on_owner(t));
    }

    #[test]
    fn threads_saved_before_archiving_load_as_open() {
        let thread: Thread = serde_json::from_str(r#"{"name":"t","channel":0,"starter":0,"messages":[]}"#).unwrap();
        assert!(!thread.archived);
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
    fn table_rows_and_columns_grow_and_shrink_together() {
        let mut t = Table::blank(2, 2);
        assert_eq!(t.columns, ["Column 1", "Column 2"]);
        assert!(t.set_cell(0, 1, "b"));
        assert!(!t.set_cell(0, 1, "b"), "unchanged");
        assert!(!t.set_cell(5, 0, "x"), "no such cell");

        assert_eq!(t.insert_row(0), 0);
        assert_eq!(t.insert_row(99), 3, "past the end appends");
        assert_eq!(t.rows.len(), 4);
        assert_eq!(t.cell(1, 1), "b", "moved down by the row above");
        assert!(t.remove_row(0));
        assert!(!t.remove_row(10));
        assert_eq!(t.cell(0, 1), "b");

        let name = t.next_column_name();
        assert_eq!(name, "Column 3");
        assert_eq!(t.insert_column(1, &name), 1);
        assert_eq!(t.columns, ["Column 1", "Column 3", "Column 2"]);
        assert!(t.rows.iter().all(|r| r.len() == 3));
        assert_eq!(t.cell(0, 2), "b", "shifted right with its column");
        assert_eq!(t.next_column_name(), "Column 4");

        assert!(t.rename_column(1, "  Owner "));
        assert_eq!(t.columns[1], "Owner");
        assert!(!t.rename_column(1, "   "), "blank names are refused");

        assert!(t.remove_column(2));
        assert_eq!(t.columns, ["Column 1", "Owner"]);
        assert!(t.rows.iter().all(|r| r.len() == 2));
        assert!(t.remove_column(0));
        assert!(!t.remove_column(0), "the last column stays");
        assert_eq!(t.columns, ["Owner"]);
    }

    #[test]
    fn table_files_with_ragged_rows_are_made_to_fit() {
        let json = r#"{"kind":"table","columns":["a","b"],"rows":[["1"],["1","2","3"]]}"#;
        let ArtifactContent::Table(t) = serde_json::from_str(json).unwrap();
        assert_eq!(t.rows, [vec!["1", ""], vec!["1", "2"]]);
        // And it is written back as plain columns and rows.
        let back = serde_json::to_value(ArtifactContent::Table(t)).unwrap();
        assert_eq!(back, serde_json::json!({"kind": "table", "columns": ["a", "b"], "rows": [["1", ""], ["1", "2"]]}));
    }

    #[test]
    fn artifacts_belong_to_a_channel_and_keep_their_ids() {
        let mut ws = seed();
        let (general, eng) = (ws.find_channel("general").unwrap(), ws.find_channel("engineering").unwrap());
        let a = ws.add_artifact(general, ArtifactKind::Table);
        let b = ws.add_artifact(general, ArtifactKind::Table);
        let c = ws.add_artifact(eng, ArtifactKind::Table);
        assert_eq!(ws.artifact(a).unwrap().title, "Untitled table");
        assert_eq!(ws.artifact(b).unwrap().title, "Untitled table 2");
        assert_eq!(ws.artifact(c).unwrap().title, "Untitled table", "titles are per channel");
        assert_eq!(ws.channel_artifacts(general), [a, b]);
        let Some(ArtifactContent::Table(t)) = &ws.artifact(a).unwrap().content else { panic!("a blank table") };
        assert_eq!((t.columns.len(), t.rows.len()), (DEFAULT_TABLE_COLUMNS, DEFAULT_TABLE_ROWS));

        assert_eq!(ws.rename_artifact(a, "  Q4   budget "), Some("Q4 budget".into()));
        assert_eq!(ws.rename_artifact(a, "  "), Some("Q4 budget".into()), "blank keeps the title");
        assert_eq!(ws.rename_artifact(99, "x"), None);

        assert!(ws.remove_artifact(b).is_some());
        assert!(ws.artifact(b).is_none());
        assert_eq!(ws.channel_artifacts(general), [a]);
        // A removed artifact's id (it names a file) is never given out again.
        let d = ws.add_artifact(general, ArtifactKind::Table);
        assert!(d > c);
    }

    #[test]
    fn default_thread_name_is_a_short_snippet() {
        let ws = seed();
        let general = &ws.channels[0];
        assert_eq!(default_thread_name(&general.messages[0]), "Good morning everyone 👋 Quick reminder…");
    }
}
