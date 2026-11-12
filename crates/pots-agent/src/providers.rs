//! Editing providers, and which provider and model each agent (and the
//! worker) uses, from the app. Settings go to config.toml, where only what's being edited
//! changes (comments, other tables, formatting and fields the app doesn't
//! show stay as they are); provider keys go to keys.toml, which only the
//! user can read.

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::config::{self, Api, Config, Endpoint, ProviderConfig};
use crate::store;
use crate::worker;

/// A provider as the settings screen edits it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderFields {
    pub name: String,
    pub api: Api,
    /// For `api = "openai"`: set in config.toml only, shown but not edited here.
    pub endpoint: Endpoint,
    pub base_url: String,
    /// The saved key; empty for none.
    pub api_key: String,
    /// The environment variable the key comes from otherwise; empty for none.
    pub api_key_env: String,
    /// The key is written in config.toml itself, in plain text. Saving the
    /// provider moves it to keys.toml.
    pub key_in_config: bool,
}

impl ProviderFields {
    pub fn new(api: Api) -> Self {
        Self {
            name: String::new(),
            api,
            endpoint: Endpoint::Auto,
            base_url: api.default_base_url().to_string(),
            api_key: String::new(),
            api_key_env: String::new(),
            key_in_config: false,
        }
    }

    pub fn from_config(name: &str, pc: &ProviderConfig) -> Self {
        Self {
            name: name.to_string(),
            api: pc.api,
            endpoint: pc.endpoint,
            base_url: pc.base_url.clone(),
            api_key: pc.api_key.clone().or_else(|| pc.stored_key.clone()).unwrap_or_default(),
            api_key_env: pc.api_key_env.clone().unwrap_or_default(),
            key_in_config: pc.api_key.is_some(),
        }
    }
}

/// Save provider `old` (or a new one, for `None`) as `fields`: its settings
/// to config.toml, its key to keys.toml. Returns the config as now saved.
pub fn save_to_disk(old: Option<&str>, fields: &ProviderFields) -> Result<Config, String> {
    let text = read_config()?;
    let new_text = save(&text, old, fields)?;
    let name = fields.name.trim();
    let mut keys = read_keys()?;
    let before = keys.clone();
    match fields.api_key.trim() {
        "" => keys.remove(name),
        key => keys.insert(name.to_string(), key.to_string()),
    };
    // A renamed provider's old key stays until config.toml has the new name,
    // so a failed write never leaves it without one.
    if keys != before {
        write_keys(&keys)?;
    }
    write_config(&new_text)?;
    if let Some(old) = old.filter(|&o| o != name)
        && keys.remove(old).is_some()
    {
        write_keys(&keys)?;
    }
    store::load_config()
}

/// Delete provider `name` from config.toml, and its key from keys.toml.
pub fn delete_from_disk(name: &str) -> Result<Config, String> {
    let new_text = remove(&read_config()?, name)?;
    write_config(&new_text)?;
    let mut keys = read_keys()?;
    if keys.remove(name).is_some() {
        write_keys(&keys)?;
    }
    store::load_config()
}

fn read_config() -> Result<String, String> {
    store::read_or_init_config(config::DEFAULT).map_err(|e| format!("Couldn't read {}: {e}", store::config_path().display()))
}

fn write_config(text: &str) -> Result<(), String> {
    store::write_config(text).map_err(|e| format!("Couldn't write {}: {e}", store::config_path().display()))
}

fn read_keys() -> Result<std::collections::BTreeMap<String, String>, String> {
    store::read_keys().map_err(|e| format!("Couldn't read {}: {e}", store::keys_path().display()))
}

fn write_keys(keys: &std::collections::BTreeMap<String, String>) -> Result<(), String> {
    store::write_keys(keys).map_err(|e| format!("Couldn't write {}: {e}", store::keys_path().display()))
}

/// Config `text` with provider `old` (or a new one, for `None`) set to
/// `fields`, minus its key, which belongs in keys.toml: a key written in
/// config.toml is taken out. A renamed provider's agents are pointed at the
/// new name. The result is checked to still be a valid config.
pub fn save(text: &str, old: Option<&str>, fields: &ProviderFields) -> Result<String, String> {
    let name = fields.name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        return Err("The name must be one word: letters, digits, _ or -.".into());
    }
    let base_url = fields.base_url.trim();
    // Claude Code is a program on this machine, not a URL.
    let claude_code = fields.api == Api::ClaudeCode;
    if !claude_code && !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err("The base URL must start with http:// or https://.".into());
    }
    let env = fields.api_key_env.trim();
    if !env.is_empty()
        && (env.starts_with(|c: char| c.is_ascii_digit()) || !env.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    {
        return Err("The environment variable name can only have letters, digits and _.".into());
    }

    let mut doc = parse(text)?;
    let providers = doc
        .entry("providers")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_mut()
        .ok_or("`providers` in config.toml isn't a table: edit it by hand.")?;
    if old != Some(name) && providers.contains_key(name) {
        return Err(format!("There's already a provider named {name}."));
    }
    let mut table = match old {
        Some(old) => match providers.remove(old) {
            Some(Item::Table(t)) => t,
            Some(_) => return Err(format!("[providers.{old}] isn't written as a table: edit it in config.toml.")),
            None => return Err(format!("config.toml has no provider {old} any more. Was it changed outside the app?")),
        },
        None => {
            let mut t = Table::new();
            t.decor_mut().set_prefix("\n");
            t
        }
    };
    set(&mut table, "api", fields.api.as_str());
    match claude_code {
        true => drop(table.remove("base_url")),
        false => set(&mut table, "base_url", base_url),
    }
    table.remove("api_key");
    set_or_remove(&mut table, "api_key_env", env);
    providers.insert(name, Item::Table(table));

    if let Some(old) = old.filter(|&o| o != name)
        && let Some(agents) = doc.get_mut("agents").and_then(Item::as_array_of_tables_mut)
    {
        for agent in agents.iter_mut() {
            if agent.get("provider").and_then(Item::as_str) == Some(old) {
                set(agent, "provider", name);
            }
        }
    }
    if let Some(old) = old.filter(|&o| o != name)
        && let Some(worker) = doc.get_mut("worker").and_then(Item::as_table_mut)
        && worker.get("provider").and_then(Item::as_str) == Some(old)
    {
        set(worker, "provider", name);
    }
    checked(doc)
}

/// Config `text` without provider `name`. Refused while agents use it.
pub fn remove(text: &str, name: &str) -> Result<String, String> {
    let config = Config::parse(text)?;
    let mut users: Vec<&str> = config.agents.iter().filter(|a| a.provider == name).map(|a| a.name.as_str()).collect();
    if config.worker.as_ref().is_some_and(|w| w.provider == name) {
        users.push(worker::NAME);
    }
    if !users.is_empty() {
        return Err(format!("{} uses {name}: switch it to another provider under Agents first.", users.join(", ")));
    }
    let mut doc = parse(text)?;
    let Some(Item::Table(gone)) = doc.get_mut("providers").and_then(Item::as_table_mut).and_then(|p| p.remove(name)) else {
        return Err(format!("config.toml has no provider {name}."));
    };
    let prefix = gone.decor().prefix().and_then(|p| p.as_str()).unwrap_or("").to_string();
    keep_comments(&mut doc, gone.position(), &prefix);
    checked(doc)
}

/// Comments that were above a removed table's header (on the first one,
/// the file's own) stay, above whatever came after it.
fn keep_comments(doc: &mut DocumentMut, pos: Option<isize>, prefix: &str) {
    let Some(pos) = pos.filter(|_| !prefix.trim().is_empty()) else { return };
    let prefix = prefix.trim_end_matches([' ', '\t']).to_string();
    let next = next_position(doc.as_table(), pos);
    match next.and_then(|n| table_at(doc.as_table_mut(), n)) {
        Some(t) => {
            let rest = t.decor().prefix().and_then(|p| p.as_str()).unwrap_or("").to_string();
            let joined = format!("{}\n\n{}", prefix.trim_end(), rest.trim_start_matches('\n'));
            t.decor_mut().set_prefix(joined);
        }
        None => {
            let trailing = doc.trailing().as_str().unwrap_or("").to_string();
            doc.set_trailing(format!("{}\n{trailing}", prefix.trim_end()));
        }
    }
}

/// An agent's provider and model, as the settings screen edits them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentChoice {
    pub name: String,
    pub provider: String,
    pub model: String,
    /// It's `[worker]`, not one of the `[[agents]]`.
    pub worker: bool,
}

impl AgentChoice {
    /// The `[worker]` choice; empty fields for a worker not set up yet.
    pub fn worker(provider: &str, model: &str) -> Self {
        Self { name: worker::NAME.to_string(), provider: provider.to_string(), model: model.to_string(), worker: true }
    }
}

/// Save which provider and model agent `choice.name` uses to config.toml.
/// Returns the config as now saved.
pub fn save_agent_to_disk(choice: &AgentChoice) -> Result<Config, String> {
    let new_text = save_agent(&read_config()?, choice)?;
    write_config(&new_text)?;
    store::load_config()
}

/// Config `text` with agent `choice.name`'s provider and model set. The
/// result is checked to still be a valid config.
pub fn save_agent(text: &str, choice: &AgentChoice) -> Result<String, String> {
    let model = choice.model.trim();
    if choice.provider.is_empty() {
        return Err("Pick a provider.".into());
    }
    if model.is_empty() {
        return Err("Enter a model name.".into());
    }
    let mut doc = parse(text)?;
    let providers = doc.get("providers").and_then(Item::as_table_like);
    if !providers.is_some_and(|p| p.contains_key(&choice.provider)) {
        return Err(format!("config.toml has no provider {} any more. Was it changed outside the app?", choice.provider));
    }
    if choice.worker {
        // A new [worker] goes at the very end, after the file's closing
        // comments (which toml_edit keeps apart), with a line on what it is.
        if !doc.contains_key("worker") {
            let trailing = doc.trailing().as_str().unwrap_or("").to_string();
            doc.set_trailing("");
            let mut table = Table::new();
            table.decor_mut().set_prefix(format!("{trailing}{WORKER_COMMENT}"));
            doc.insert("worker", Item::Table(table));
        }
        let worker = doc.get_mut("worker").and_then(Item::as_table_mut).ok_or("[worker] in config.toml isn't a table.")?;
        set(worker, "provider", &choice.provider);
        set(worker, "model", model);
        return checked(doc);
    }
    let agent = doc
        .get_mut("agents")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|agents| agents.iter_mut().find(|a| a.get("name").and_then(Item::as_str) == Some(choice.name.as_str())))
        .ok_or_else(|| format!("config.toml has no [[agents]] entry named {}. Was it changed outside the app?", choice.name))?;
    set(agent, "provider", &choice.provider);
    set(agent, "model", model);
    checked(doc)
}

/// The line a `[worker]` set up from the app gets above its header.
const WORKER_COMMENT: &str = "\n# The cheaper model agents hand look-ups to (ask_worker).\n";

/// Take `[worker]` out of config.toml: agents lose `ask_worker`.
pub fn remove_worker_from_disk() -> Result<Config, String> {
    let new_text = remove_worker(&read_config()?)?;
    write_config(&new_text)?;
    store::load_config()
}

/// Config `text` without `[worker]`.
pub fn remove_worker(text: &str) -> Result<String, String> {
    let mut doc = parse(text)?;
    let Some(Item::Table(gone)) = doc.remove("worker") else {
        return Err("config.toml has no [worker].".into());
    };
    // Other comments above it stay; the line the app added for it goes.
    let prefix = gone.decor().prefix().and_then(|p| p.as_str()).unwrap_or("");
    let prefix = prefix.strip_suffix(WORKER_COMMENT).unwrap_or(prefix).to_string();
    keep_comments(&mut doc, gone.position(), &prefix);
    checked(doc)
}

fn parse(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>().map_err(|e| format!("config.toml isn't valid TOML: {e}"))
}

fn checked(doc: DocumentMut) -> Result<String, String> {
    let text = doc.to_string();
    Config::parse(&text)?;
    Ok(text)
}

/// Set string `key`, keeping its comments and leaving it untouched if it
/// already says that.
fn set(table: &mut Table, key: &str, s: &str) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(v) if v.as_str() == Some(s) => {}
        Some(v) => {
            let decor = v.decor().clone();
            *v = Value::from(s);
            *v.decor_mut() = decor;
        }
        None => {
            table.insert(key, toml_edit::value(s));
        }
    }
}

fn set_or_remove(table: &mut Table, key: &str, s: &str) {
    if s.is_empty() {
        table.remove(key);
    } else {
        set(table, key, s);
    }
}

/// The smallest table position after `after`, anywhere in the document.
fn next_position(table: &Table, after: isize) -> Option<isize> {
    let mut best: Option<isize> = None;
    let mut consider = |t: &Table| {
        if let Some(p) = t.position().filter(|&p| p > after && !t.is_implicit()) {
            best = Some(best.map_or(p, |b| b.min(p)));
        }
        next_position(t, after)
    };
    let mut found = Vec::new();
    for (_, item) in table.iter() {
        match item {
            Item::Table(t) => found.push(consider(t)),
            Item::ArrayOfTables(a) => found.extend(a.iter().map(&mut consider)),
            _ => {}
        }
    }
    found.into_iter().flatten().chain(best).min()
}

/// The table at document position `pos`.
fn table_at(table: &mut Table, pos: isize) -> Option<&mut Table> {
    for (_, item) in table.iter_mut() {
        let tables: Vec<&mut Table> = match item {
            Item::Table(t) => vec![t],
            Item::ArrayOfTables(a) => a.iter_mut().collect(),
            _ => continue,
        };
        for t in tables {
            if t.position() == Some(pos) && !t.is_implicit() {
                return Some(t);
            }
            if let Some(found) = table_at(t, pos) {
                return Some(found);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT;

    #[test]
    fn claude_code_is_saved_without_a_url() {
        let text = save(DEFAULT, None, &fields("claude", Api::ClaudeCode, "")).unwrap();
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.providers["claude"].api, Api::ClaudeCode);
        assert!(!text.split("[providers.claude]").nth(1).unwrap().contains("base_url"), "{text}");
    }

    fn fields(name: &str, api: Api, base_url: &str) -> ProviderFields {
        ProviderFields { name: name.into(), base_url: base_url.into(), ..ProviderFields::new(api) }
    }

    #[test]
    fn adds_a_provider_after_the_others_keeping_comments() {
        let text = save(DEFAULT, None, &fields("local", Api::Openai, "http://localhost:11434/v1")).unwrap();
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.providers["local"].base_url, "http://localhost:11434/v1");
        assert!(config.providers["local"].api_key.is_none());
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("no {s:?} in\n{text}"));
        assert!(at("[providers.openai]") < at("[providers.local]"));
        assert!(at("[providers.local]") < at("# Example: a local"));
        assert!(text.starts_with("# Pots configuration."));
        assert!(text.contains("# Engineer and reviewer"), "comments elsewhere survive");
    }

    #[test]
    fn edits_and_renames_a_provider_and_its_agents() {
        let mut f = ProviderFields::from_config("openrouter", &Config::parse(DEFAULT).unwrap().providers["openrouter"]);
        f.name = "router".into();
        f.api_key = "sk-test".into();
        f.api_key_env = String::new();
        let text = save(DEFAULT, Some("openrouter"), &f).unwrap();
        let config = Config::parse(&text).unwrap();
        assert!(!config.providers.contains_key("openrouter"));
        let p = &config.providers["router"];
        assert_eq!((p.api_key.as_deref(), p.api_key_env.as_deref()), (None, None), "the key goes to keys.toml");
        assert_eq!(p.endpoint, Endpoint::Chat, "fields the form doesn't edit stay");
        assert!(!text.contains("sk-test"));
        assert_eq!(config.agents[0].provider, "router");
        assert!(text.starts_with("# Pots configuration."), "the header comment stays at the top");
        assert!(text.find("[providers.router]").unwrap() < text.find("[providers.anthropic]").unwrap(), "same place");
    }

    #[test]
    fn takes_a_plain_text_key_out_of_config() {
        let text = "[providers.p]\napi = \"openai\"\nbase_url = \"http://x\"\napi_key = \"sk-old\"\n";
        let pc = &Config::parse(text).unwrap().providers["p"];
        let f = ProviderFields::from_config("p", pc);
        assert!(f.key_in_config);
        assert_eq!(f.api_key, "sk-old");
        let out = save(text, Some("p"), &f).unwrap();
        assert!(!out.contains("sk-old"), "{out}");
    }

    #[test]
    fn keeps_fields_it_does_not_edit() {
        let text = "[providers.p]\napi = \"openai\" # the wire format\nbase_url = \"http://x\"\nheaders = { a = \"b\" }\n";
        let mut f = fields("p", Api::Openai, "http://x");
        f.api_key_env = "P_KEY".into();
        let out = save(text, Some("p"), &f).unwrap();
        assert!(out.contains("api = \"openai\" # the wire format"), "{out}");
        assert!(out.contains("headers = { a = \"b\" }"), "{out}");
        assert!(out.contains("api_key_env = \"P_KEY\""), "{out}");
    }

    #[test]
    fn checks_what_it_is_given() {
        let err = |f: &ProviderFields, old| save(DEFAULT, old, f).unwrap_err();
        assert!(err(&fields("two words", Api::Openai, "http://x"), None).contains("one word"));
        assert!(err(&fields("openai", Api::Openai, "http://x"), None).contains("already"));
        assert!(err(&fields("x", Api::Openai, "localhost:1"), None).contains("http"));
        let mut f = fields("x", Api::Openai, "http://x");
        f.api_key_env = "MY-KEY".into();
        assert!(err(&f, None).contains("environment variable"));
        assert!(err(&fields("x", Api::Openai, "http://x"), Some("gone")).contains("no provider"));
    }

    #[test]
    fn adding_then_removing_gives_back_the_same_file() {
        let added = save(DEFAULT, None, &fields("local", Api::Openai, "http://localhost:1/v1")).unwrap();
        assert_eq!(remove(&added, "local").unwrap(), DEFAULT);
    }

    #[test]
    fn sets_an_agents_provider_and_model_keeping_the_rest() {
        let choice =
            AgentChoice { name: "PM".into(), provider: "anthropic".into(), model: " claude-opus-5-5 ".into(), worker: false };
        let text = save_agent(DEFAULT, &choice).unwrap();
        let pm = &Config::parse(&text).unwrap().agents[0];
        assert_eq!((pm.provider.as_str(), pm.model.as_str()), ("anthropic", "claude-opus-5-5"));
        assert!(pm.system_prompt.starts_with("You are PM"));
        assert_eq!(text.replace("provider = \"anthropic\"\nmodel = \"claude-opus-5-5\"", ""),
                   DEFAULT.replace("provider = \"openrouter\"\nmodel = \"anthropic/claude-opus-5.5\"", ""),
                   "nothing else changes");

        let err = |c: AgentChoice| save_agent(DEFAULT, &c).unwrap_err();
        assert!(err(AgentChoice { model: " ".into(), ..choice.clone() }).contains("model"));
        assert!(err(AgentChoice { provider: "nope".into(), ..choice.clone() }).contains("no provider"));
        assert!(err(AgentChoice { name: "Engineer".into(), ..choice }).contains("no [[agents]]"));
    }

    #[test]
    fn sets_up_changes_and_removes_the_worker() {
        let text = save_agent(DEFAULT, &AgentChoice::worker("openrouter", "deepseek/flash")).unwrap();
        let worker = Config::parse(&text).unwrap().worker.unwrap();
        assert_eq!((worker.provider.as_str(), worker.model.as_str()), ("openrouter", "deepseek/flash"));
        assert!(text.starts_with(DEFAULT), "the rest of the file stays as it was");
        assert!(text.contains("# The cheaper model agents hand look-ups to (ask_worker).\n[worker]"), "{text}");

        // Hand-written fields stay; only provider and model change.
        let tuned = text.replace("model = \"deepseek/flash\"", "model = \"deepseek/flash\"\nmax_steps = 12");
        let changed = save_agent(&tuned, &AgentChoice::worker("anthropic", "claude-haiku-5-5")).unwrap();
        let worker = Config::parse(&changed).unwrap().worker.unwrap();
        assert_eq!((worker.provider.as_str(), worker.max_steps), ("anthropic", Some(12)));

        // Renaming its provider follows it; deleting that provider is refused.
        let renamed = save(&changed, Some("anthropic"), &fields("claude", Api::Anthropic, "https://api.anthropic.com")).unwrap();
        assert_eq!(Config::parse(&renamed).unwrap().worker.unwrap().provider, "claude");
        assert!(remove(&renamed, "claude").unwrap_err().contains("Worker uses claude"));

        // Setting it up and removing it again gives back the same file.
        assert_eq!(remove_worker(&text).unwrap(), DEFAULT);

        let gone = remove_worker(&changed).unwrap();
        assert!(Config::parse(&gone).unwrap().worker.is_none());
        assert!(remove_worker(&gone).unwrap_err().contains("no [worker]"));
        assert!(save_agent(DEFAULT, &AgentChoice::worker("", "m")).unwrap_err().contains("Pick a provider"));
    }

    #[test]
    fn removes_unused_providers_only() {
        assert!(remove(DEFAULT, "openrouter").unwrap_err().contains("PM uses openrouter"));
        let text = remove(DEFAULT, "openai").unwrap();
        assert!(!Config::parse(&text).unwrap().providers.contains_key("openai"));
        assert!(text.contains("# Example: a local"));

        // Removing the first table keeps the file's header comment.
        let text = save(DEFAULT, Some("openrouter"), &{
            let mut f = fields("openrouter", Api::Openai, "https://openrouter.ai/api/v1");
            f.api_key_env = "OPENROUTER_API_KEY".into();
            f
        })
        .unwrap();
        let moved = text.replace("provider = \"openrouter\"", "provider = \"anthropic\"");
        let out = remove(&moved, "openrouter").unwrap();
        assert!(out.starts_with("# Pots configuration."), "{out}");
        assert!(out.contains("`api_key_env`.\n"), "{out}");
        assert!(!Config::parse(&out).unwrap().providers.contains_key("openrouter"));
        assert!(out.contains("[providers.anthropic]"));
    }
}
