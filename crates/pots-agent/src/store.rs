//! On-disk state: the workspace (JSON), the user's config (TOML) and the
//! API keys the app saved (TOML, readable only by the user), all in the data
//! directory — `$POTS_HOME`, or `~/.pots`.
//!
//! Artifacts are listed in `workspace.json`, but each one's content is its
//! own file, `artifacts/<id>.json`. Editing a table then rewrites only that
//! table, not every message of the workspace; and saving the workspace (on
//! every message) doesn't rewrite every table, document and whiteboard.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::{self, Config};
use crate::data::{ArtifactContent, Workspace};

const KEYS_HEADER: &str = "\
# API keys for the providers in config.toml, by provider name. Pots writes
# this file from Settings → Providers. Only you can read it: keep it that way,
# and don't share or commit it.
";

pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("POTS_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".pots")
}

fn workspace_path() -> PathBuf {
    home().join("workspace.json")
}

fn artifact_path(home: &Path, id: usize) -> PathBuf {
    home.join("artifacts").join(format!("{id}.json"))
}

pub fn config_path() -> PathBuf {
    home().join("config.toml")
}

/// The saved workspace, or `None` if there is none yet. A file that exists
/// but can't be read is an error, so it is never silently replaced.
pub fn load_workspace() -> io::Result<Option<Workspace>> {
    load_workspace_from(&home())
}

fn load_workspace_from(home: &Path) -> io::Result<Option<Workspace>> {
    let text = match fs::read_to_string(home.join("workspace.json")) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut ws: Workspace = serde_json::from_str(&text).map_err(io::Error::other)?;
    ws.restore();
    for artifact in &mut ws.artifacts {
        match load_artifact(home, artifact.id) {
            // Listed but never written (the app quit in between): blank.
            Ok(None) => artifact.content = Some(ArtifactContent::new(artifact.kind)),
            Ok(Some(content)) if content.kind() == artifact.kind => artifact.content = Some(content),
            Ok(Some(content)) => {
                artifact.load_error = Some(format!(
                    "{} holds a {}, not a {}.",
                    artifact_path(home, artifact.id).display(),
                    content.kind().noun(),
                    artifact.kind.noun()
                ))
            }
            Err(e) => artifact.load_error = Some(format!("{}: {e}", artifact_path(home, artifact.id).display())),
        }
    }
    Ok(Some(ws))
}

fn load_artifact(home: &Path, id: usize) -> io::Result<Option<ArtifactContent>> {
    match fs::read_to_string(artifact_path(home, id)) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write the workspace atomically (temp file + rename). Artifact content is
/// saved apart, with `save_artifact`.
pub fn save_workspace(ws: &Workspace) -> io::Result<()> {
    let path = workspace_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(ws).map_err(io::Error::other)?)?;
    fs::rename(tmp, path)
}

/// Write artifact `id`'s content to its own file, atomically.
pub fn save_artifact(id: usize, content: &ArtifactContent) -> io::Result<()> {
    save_artifact_in(&home(), id, content)
}

fn save_artifact_in(home: &Path, id: usize, content: &ArtifactContent) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(content).map_err(io::Error::other)?;
    write_atomic(&artifact_path(home, id), &json, false)
}

/// Delete artifact `id`'s file (there may be none).
pub fn delete_artifact(id: usize) -> io::Result<()> {
    delete_artifact_in(&home(), id)
}

fn delete_artifact_in(home: &Path, id: usize) -> io::Result<()> {
    match fs::remove_file(artifact_path(home, id)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The config file's text, writing `default` there first if it's missing.
pub fn read_or_init_config(default: &str) -> io::Result<String> {
    let path = config_path();
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(&path, default)?;
            Ok(default.to_string())
        }
        Err(e) => Err(e),
    }
}

/// Replace the config file atomically (temp file + rename).
pub fn write_config(text: &str) -> io::Result<()> {
    write_atomic(&config_path(), text.as_bytes(), false)
}

pub fn keys_path() -> PathBuf {
    home().join("keys.toml")
}

/// The saved API keys (provider name → key); none if there's no keys.toml.
pub fn read_keys() -> io::Result<BTreeMap<String, String>> {
    read_keys_from(&keys_path())
}

/// Replace keys.toml, readable and writable only by the user.
pub fn write_keys(keys: &BTreeMap<String, String>) -> io::Result<()> {
    write_keys_to(&keys_path(), keys)
}

/// config.toml (created on first run), with keys.toml's keys attached.
pub fn load_config() -> Result<Config, String> {
    let text = read_or_init_config(config::DEFAULT).map_err(|e| format!("{}: {e}", config_path().display()))?;
    let mut config = Config::parse(&text).map_err(|e| format!("{}: {e}", config_path().display()))?;
    let keys = read_keys().map_err(|e| format!("{}: {e}", keys_path().display()))?;
    config.attach_keys(&keys);
    Ok(config)
}

fn read_keys_from(path: &Path) -> io::Result<BTreeMap<String, String>> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e),
    }
}

fn write_keys_to(path: &Path, keys: &BTreeMap<String, String>) -> io::Result<()> {
    let body = toml::to_string(keys).map_err(io::Error::other)?;
    write_atomic(path, format!("{KEYS_HEADER}\n{body}").as_bytes(), true)
}

/// Write `path` through a temp file and a rename. With `private`, the file
/// is 0600 from the moment it exists, so the key is never readable by others.
fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp); // a leftover would keep its old mode
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::data::{self, ArtifactContent, ArtifactKind, Conv};

    #[test]
    fn keys_round_trip_and_stay_private() {
        let dir = std::env::temp_dir().join(format!("pots-keys-test-{}", std::process::id()));
        let path = dir.join("keys.toml");
        assert!(super::read_keys_from(&path).unwrap().is_empty(), "no file: no keys");
        let keys = BTreeMap::from([("anthropic".to_string(), "sk-a".to_string()), ("my-local".to_string(), "x".to_string())]);
        super::write_keys_to(&path, &keys).unwrap();
        // Rewriting replaces the file and keeps it private.
        super::write_keys_to(&path, &keys).unwrap();
        assert_eq!(super::read_keys_from(&path).unwrap(), keys);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# API keys"), "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::write(&path, "not = [valid").unwrap();
        assert!(super::read_keys_from(&path).is_err(), "a broken file is an error, never read as empty");
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn temp_home(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pots-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn workspaces_saved_before_artifacts_still_load() {
        let home = temp_home("old-workspace");
        // As written before artifacts existed: no `artifacts`, no `next_artifact_id`.
        let old = r#"{
          "name": "Pots", "me": 0,
          "users": [{"name": "you", "color": 2845872, "online": true, "kind": "human"}],
          "channels": [{"name": "general", "topic": "", "dm": null, "unread": false,
                        "messages": [{"id": 0, "author": 0, "ts": 1700000000, "source": "hi"}]}],
          "threads": [],
          "next_id": 1
        }"#;
        std::fs::write(home.join("workspace.json"), old).unwrap();
        let mut ws = super::load_workspace_from(&home).unwrap().unwrap();
        assert!(ws.artifacts.is_empty());
        assert_eq!(ws.channels[0].messages[0].snippet(10), "hi");
        assert_eq!(ws.add_artifact(0, ArtifactKind::Table), 0);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn artifacts_round_trip_through_their_own_files() {
        let home = temp_home("artifacts");
        let mut ws = data::seed();
        let general = ws.find_channel("general").unwrap();
        let id = ws.add_artifact(general, ArtifactKind::Table);
        ws.rename_artifact(id, "Lunch spots");
        let Some(ArtifactContent::Table(table)) = &mut ws.artifact_mut(id).unwrap().content else { unreachable!() };
        table.set_cell(0, 0, "Zqx noodles 🍜");
        table.insert_column(1, "Price");
        table.set_cell(0, 1, "$$");
        table.remove_row(4);
        let content = ws.artifact(id).unwrap().content.clone().unwrap();

        std::fs::write(home.join("workspace.json"), serde_json::to_vec(&ws).unwrap()).unwrap();
        super::save_artifact_in(&home, id, &content).unwrap();
        assert!(!String::from_utf8(std::fs::read(home.join("workspace.json")).unwrap()).unwrap().contains("Zqx noodles"),
            "content stays out of workspace.json");

        let back = super::load_workspace_from(&home).unwrap().unwrap();
        let a = back.artifact(id).unwrap();
        assert_eq!((a.title.as_str(), a.kind, a.channel), ("Lunch spots", ArtifactKind::Table, general));
        assert_eq!(a.content.as_ref(), Some(&content));
        assert!(a.load_error.is_none());

        // Deleting removes the file; deleting again is fine.
        super::delete_artifact_in(&home, id).unwrap();
        super::delete_artifact_in(&home, id).unwrap();
        // Listed but without a file: it opens blank.
        let back = super::load_workspace_from(&home).unwrap().unwrap();
        assert_eq!(back.artifact(id).unwrap().content, Some(ArtifactContent::new(ArtifactKind::Table)));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_table_an_agent_made_and_changed_is_saved_like_any_other() {
        use crate::artifact_tools::{self, ADD_TABLE_ROWS, CREATE_TABLE, UPDATE_TABLE};
        use serde_json::json;
        let home = temp_home("agent-table");
        let mut ws = data::empty();
        let input = json!({"channel": "general", "title": "Reading list", "columns": ["Book", "Done"], "rows": [["Dune", "no"]]});
        let id = artifact_tools::write(&mut ws, CREATE_TABLE, &input).unwrap().id;
        artifact_tools::write(&mut ws, UPDATE_TABLE, &json!({"id": id, "title": "Books"})).unwrap();
        artifact_tools::write(&mut ws, ADD_TABLE_ROWS, &json!({"id": id, "rows": [["Emma", "yes"]]})).unwrap();
        // As the app saves it: the file, then the workspace listing it.
        let content = ws.artifact(id).unwrap().content.clone().unwrap();
        super::save_artifact_in(&home, id, &content).unwrap();
        std::fs::write(home.join("workspace.json"), serde_json::to_vec(&ws).unwrap()).unwrap();

        let file: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join(format!("artifacts/{id}.json"))).unwrap()).unwrap();
        assert_eq!(file, json!({"kind": "table", "columns": ["Book", "Done"], "rows": [["Dune", "no"], ["Emma", "yes"]]}));
        let back = super::load_workspace_from(&home).unwrap().unwrap();
        let a = back.artifact(id).unwrap();
        assert_eq!((a.title.as_str(), a.kind, a.channel), ("Books", ArtifactKind::Table, 0));
        assert_eq!(a.content.as_ref(), Some(&content));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_page_is_saved_beside_a_workspace_written_before_pages_existed() {
        use crate::artifact_tools::{self, CREATE_PAGE, UPDATE_PAGE};
        use serde_json::json;
        let home = temp_home("agent-page");
        // A workspace.json and artifact file as the app wrote them before pages.
        let mut ws = data::empty();
        let table = ws.add_artifact(0, ArtifactKind::Table);
        let content = ws.artifact(table).unwrap().content.clone().unwrap();
        super::save_artifact_in(&home, table, &content).unwrap();
        std::fs::write(home.join("workspace.json"), serde_json::to_vec(&ws).unwrap()).unwrap();
        let mut ws = super::load_workspace_from(&home).unwrap().unwrap();
        assert_eq!(ws.artifact(table).unwrap().content.as_ref(), Some(&content));

        let html = "<!doctype html><button onclick=\"this.textContent++\">0</button>";
        let id = artifact_tools::write(&mut ws, CREATE_PAGE, &json!({"channel": "general", "title": "Clicker", "html": "<p>"})).unwrap().id;
        artifact_tools::write(&mut ws, UPDATE_PAGE, &json!({"id": id, "html": html})).unwrap();
        let content = ws.artifact(id).unwrap().content.clone().unwrap();
        super::save_artifact_in(&home, id, &content).unwrap();
        std::fs::write(home.join("workspace.json"), serde_json::to_vec(&ws).unwrap()).unwrap();

        let file: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join(format!("artifacts/{id}.json"))).unwrap()).unwrap();
        assert_eq!(file, json!({"kind": "page", "html": html}));
        let back = super::load_workspace_from(&home).unwrap().unwrap();
        let a = back.artifact(id).unwrap();
        assert_eq!((a.title.as_str(), a.kind), ("Clicker", ArtifactKind::Page));
        assert_eq!(a.content.as_ref(), Some(&content));
        assert!(back.artifact(table).unwrap().load_error.is_none());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_unreadable_artifact_is_reported_not_replaced() {
        let home = temp_home("broken-artifact");
        let mut ws = data::empty();
        let id = ws.add_artifact(0, ArtifactKind::Table);
        std::fs::write(home.join("workspace.json"), serde_json::to_vec(&ws).unwrap()).unwrap();
        std::fs::create_dir_all(home.join("artifacts")).unwrap();
        std::fs::write(home.join("artifacts").join(format!("{id}.json")), "{not json").unwrap();
        let back = super::load_workspace_from(&home).unwrap().unwrap();
        let a = back.artifact(id).unwrap();
        assert!(a.content.is_none());
        assert!(a.load_error.as_deref().is_some_and(|e| e.contains(&format!("{id}.json"))), "{:?}", a.load_error);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn removing_a_message_keeps_threads_on_their_starters() {
        let mut ws = data::seed();
        let g = ws.find_channel("general").unwrap();
        let ch = Conv::Channel(g);
        let gone = ws.post(ch, 0, "placeholder");
        let starter = ws.post(ch, 1, "start a thread here");
        let t = ws.create_thread(g, starter, "t");
        let id = ws.message(Conv::Thread(t), 0).id;
        assert!(ws.remove(ch, gone).is_some());
        assert_eq!(ws.message(Conv::Thread(t), 0).id, id, "the thread still starts from the same message");
        // Messages with a thread, and thread starters, stay.
        assert!(ws.remove(ch, starter - 1).is_none());
        assert!(ws.remove(Conv::Thread(t), 0).is_none());
        let reply = ws.post(Conv::Thread(t), 1, "in thread");
        assert!(ws.remove(Conv::Thread(t), reply).is_some());
        assert_eq!(ws.len(Conv::Thread(t)), 1);
    }

    #[test]
    fn workspace_round_trips_through_json() {
        let mut ws = data::seed();
        let eng = ws.find_channel("engineering").unwrap();
        let ix = ws.post(Conv::Channel(eng), 1, "half a rep");
        ws.message_mut(Conv::Channel(eng), ix).pending = true;

        let json = serde_json::to_string(&ws).unwrap();
        let mut back: data::Workspace = serde_json::from_str(&json).unwrap();
        back.restore();

        assert_eq!(back.channels.len(), ws.channels.len());
        assert_eq!(back.threads.len(), ws.threads.len());
        let a = ws.message(Conv::Channel(eng), 0);
        let b = back.message(Conv::Channel(eng), 0);
        assert_eq!((a.id, a.ts, a.snippet(200)), (b.id, b.ts, b.snippet(200)));
        assert_eq!(b.reactions.len(), a.reactions.len());
        // A reply cut off mid-stream comes back finished, with a note.
        let cut = back.message(Conv::Channel(eng), ix);
        assert!(!cut.pending && cut.error.is_some());
        // Ids keep counting from where they were.
        let next = back.post(Conv::Channel(eng), 0, "x");
        assert_eq!(back.message(Conv::Channel(eng), next).id, ws.message(Conv::Channel(eng), ix).id + 1);
    }
}
