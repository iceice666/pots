//! On-disk state: the workspace (JSON), the user's config (TOML) and the
//! API keys the app saved (TOML, readable only by the user), all in the data
//! directory — `$POTS_HOME`, or `~/.pots`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::{self, Config};
use crate::data::Workspace;

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

pub fn config_path() -> PathBuf {
    home().join("config.toml")
}

/// The saved workspace, or `None` if there is none yet. A file that exists
/// but can't be read is an error, so it is never silently replaced.
pub fn load_workspace() -> io::Result<Option<Workspace>> {
    let text = match fs::read_to_string(workspace_path()) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut ws: Workspace = serde_json::from_str(&text).map_err(io::Error::other)?;
    ws.restore();
    Ok(Some(ws))
}

/// Write the workspace atomically (temp file + rename).
pub fn save_workspace(ws: &Workspace) -> io::Result<()> {
    let path = workspace_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(ws).map_err(io::Error::other)?)?;
    fs::rename(tmp, path)
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
    let tmp = path.with_extension("toml.tmp");
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

    use crate::data::{self, Conv};

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
