//! Agent workspaces: a git clone per task on the host, mounted at
//! `/workspace` in a long-lived container where the agent's tools run.
//!
//! Everything here blocks (git, docker); call it off the UI thread.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::WorkspaceMode;
use crate::data::Card;
use pots_llm::ToolSpec;

const MAX_OUTPUT: usize = 20_000;
const DEFAULT_TIMEOUT: u64 = 120;
const MAX_TIMEOUT: u64 = 1800;
const DEFAULT_READ_LINES: usize = 500;
/// Where the official installer puts `claude`, for shells in the container.
const CLAUDE_PATH: &str = "export PATH=\"$HOME/.local/bin:$PATH\"";

/// Where one agent works on one task.
#[derive(Clone, Debug)]
pub struct Env {
    pub mode: WorkspaceMode,
    pub docker: String,
    pub image: String,
    /// The project repository on the host.
    pub repo: PathBuf,
    /// `~/.pots/tasks/<task>`: the writer's clone and reviewers' checkouts.
    pub task_dir: PathBuf,
    /// The task's branch, e.g. `pots/t3-login-page`.
    pub branch: String,
    pub agent: String,
    pub container: String,
    /// A scratch checkout of the project's default branch, never of the
    /// task's branch: for questions asked in a channel, which are about the
    /// project as it is, not about whatever an engineer once left there.
    pub default_branch: bool,
}

impl Env {
    pub fn new(mode: WorkspaceMode, docker: String, image: String, repo: PathBuf, tasks: &Path, task: &str, agent: &str) -> Self {
        let agent_slug = slug(agent);
        Self {
            mode,
            docker,
            image,
            repo,
            task_dir: tasks.join(task),
            branch: format!("pots/{task}"),
            container: format!("pots-agent-{task}-{agent_slug}"),
            agent: agent.to_string(),
            default_branch: false,
        }
    }

    /// See [`default_branch`](Self::default_branch).
    pub fn on_default_branch(mut self, yes: bool) -> Self {
        self.default_branch = yes;
        self
    }

    /// The writer's clone (shared by the task).
    pub fn writer_dir(&self) -> PathBuf {
        self.task_dir.join("repo")
    }

    /// This agent's checkout, mounted at /workspace.
    pub fn host_dir(&self) -> PathBuf {
        match self.mode {
            WorkspaceMode::Write => self.writer_dir(),
            WorkspaceMode::Review => self.task_dir.join(format!("review-{}", slug(&self.agent))),
            WorkspaceMode::Scratch => self.task_dir.join(format!("scratch-{}", slug(&self.agent))),
        }
    }

    /// What the agent is told about its workspace.
    pub fn describe(&self) -> String {
        self.describe_with(
            "the bash, read_file, write_file and edit_file tools",
            "bash and read_file",
        )
    }

    /// [`describe`](Self::describe), for an agent running Claude Code in the
    /// container with its own built-in tools.
    pub fn describe_claude_code(&self) -> String {
        self.describe_with("your Bash, Read, Edit, Write, Glob and Grep tools", "Bash, Read, Glob and Grep")
    }

    fn describe_with(&self, write_tools: &str, review_tools: &str) -> String {
        match self.mode {
            WorkspaceMode::Write => format!(
                "# Your workspace\n\
                 You have a Linux container (image `{image}`). /workspace is your own git clone of the project \
                 (`{repo}` on the owner's machine), on branch `{branch}`; use {write_tools} there. \
                 When the task asks you to change the project, commit your work with clear messages before you \
                 report back: reviewers only see commits. When it only asks for information (a question, an \
                 investigation, an inventory), leave the clone as it is: no new, edited or deleted files and no \
                 commits; the answer goes in your reply, not in a file. Limits in the task always win over this. \
                 The project's default branch is `origin/HEAD`; if that doesn't exist, the project had no commits \
                 yet and everything you commit is new. The `origin/*` refs are refreshed from the project every \
                 time you're asked (you can't fetch from the container yourself), so `origin/HEAD` is the default \
                 branch as it is now, and task branches the owner approved show up as `origin/pots/...`: rebase \
                 onto those when you're asked to catch up with other work. The owner can fetch your work with \
                 `git fetch {dir} {branch}`.",
                image = self.image,
                repo = self.repo.display(),
                branch = self.branch,
                dir = self.writer_dir().display(),
            ),
            WorkspaceMode::Review => format!(
                "# Your workspace\n\
                 You have a Linux container (image `{image}`). /workspace is a fresh checkout of the work under \
                 review: the latest commit of branch `{branch}` in the engineer's clone, refreshed every time you \
                 are asked (or the project's default branch if no engineer has started). The local branch `base` is \
                 the project's default branch, so `git log base..HEAD` and `git diff base...HEAD` (three dots) show \
                 the work. If the project \
                 had no commits when the work started, `base` is an empty commit that shares no history with the \
                 work: use `git diff base HEAD` (two dots) there. Use {review_tools} to inspect, build and test. \
                 Anything you change there is thrown away.",
                image = self.image,
                branch = self.branch,
            ),
            WorkspaceMode::Scratch if self.default_branch => format!(
                "# Your workspace\n\
                 You have a Linux container (image `{image}`). /workspace is a throwaway checkout of the project's \
                 default branch as it is now, refreshed every time you're asked. Work that hasn't been committed \
                 there isn't in it. Use {review_tools} to look around, build and run things. Anything you change \
                 there is thrown away and nobody sees it: your final answer is all that comes back.",
                image = self.image,
            ),
            WorkspaceMode::Scratch => format!(
                "# Your workspace\n\
                 You have a Linux container (image `{image}`). /workspace is a throwaway checkout of the project: \
                 the latest commit of the task's branch `{branch}` if it has any, or else the project's default \
                 branch. Work that hasn't been committed isn't there. Use {review_tools} to look around, build and \
                 run things. Anything you change there is thrown away and nobody sees it: your final answer is \
                 all that comes back.",
                image = self.image,
                branch = self.branch,
            ),
        }
    }
}

/// Whether `path` (with `~/` expanded) is a git repository agents can
/// clone: `Err` says what's wrong with it.
pub fn check_repo(path: &str) -> Result<PathBuf, String> {
    let dir = crate::config::expand_home(path.trim());
    if !dir.is_dir() {
        return Err(format!("{} isn't a folder.", dir.display()));
    }
    // A working tree has .git; a bare repository has HEAD and objects/.
    let bare = dir.join("HEAD").is_file() && dir.join("objects").is_dir();
    if !dir.join(".git").exists() && !bare {
        return Err(format!("{} isn't a git repository (run `git init` there first).", dir.display()));
    }
    Ok(dir)
}

/// A short, container-name-safe version of `s` (ASCII letters, digits, `-`).
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    out.chars().take(32).collect::<String>().trim_end_matches('-').to_string()
}

/// The docker CLI: the configured one, or the first found on PATH or in
/// the usual install places (GUI apps don't get the shell's PATH).
pub fn find_docker(configured: Option<&str>) -> String {
    if let Some(c) = configured {
        return c.to_string();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join("docker"))
        .chain(
            [
                format!("{home}/.orbstack/bin/docker"),
                "/usr/local/bin/docker".to_string(),
                "/opt/homebrew/bin/docker".to_string(),
                "/Applications/Docker.app/Contents/Resources/bin/docker".to_string(),
            ]
            .map(PathBuf::from),
        )
        .find(|p| p.is_file())
        .map_or_else(|| "docker".to_string(), |p| p.display().to_string())
}

/// An agent's workspace for the length of one reply: set up on first use.
pub struct Session {
    pub env: Env,
    ready: bool,
}

impl Session {
    pub fn new(env: Env) -> Self {
        Self { env, ready: false }
    }

    /// Run a workspace tool. Returns the result text and whether it failed.
    pub fn run(&mut self, name: &str, input: &Value) -> (String, bool) {
        if !self.ready {
            if let Err(e) = self.prepare() {
                return (format!("Couldn't set up the workspace: {e}"), true);
            }
            self.ready = true;
        }
        let result = match (name, self.env.mode) {
            ("bash", _) => self.bash(input),
            ("read_file", _) => self.read_file(input),
            ("write_file", WorkspaceMode::Write) => self.write_file(input),
            ("edit_file", WorkspaceMode::Write) => self.edit_file(input),
            _ => Err(format!("There is no tool named {name}.")),
        };
        match result {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        }
    }

    /// The card for a `request_merge` call: the task branch's commits since
    /// it started, in the writer's clone, and what it conflicts with if
    /// `others` (the other tasks) are given.
    pub fn merge_card(&mut self, input: &Value, others: Option<&ConflictCheck>) -> Result<Card, String> {
        let summary = input["summary"].as_str().unwrap_or_default().trim().to_string();
        let writer = self.env.writer_dir();
        if !writer.join(".git").exists() {
            return Err("Nothing to merge: no engineer has worked on this task yet.".into());
        }
        if !resolves(&writer, &self.env.branch) {
            return Err(format!("Nothing to merge: `{}` has no commits yet. Commit the work first.", self.env.branch));
        }
        // A project that had no commits has no `origin/HEAD`: the whole branch is new.
        let range = match resolves(&writer, "origin/HEAD") {
            true => format!("origin/HEAD..{}", self.env.branch),
            false => self.env.branch.clone(),
        };
        let log = git(&writer, &["log", "--format=%h %s", &range])?;
        let commits: Vec<String> = log.lines().map(str::to_string).collect();
        if commits.is_empty() {
            return Err(format!("Nothing to merge: `{}` has no commits yet. Commit the work first.", self.env.branch));
        }
        Ok(Card::Merge {
            branch: self.env.branch.clone(),
            source: writer.display().to_string(),
            summary,
            commits,
            repo: Some(self.env.repo.display().to_string()),
            conflicts: others.and_then(|c| c.warning(&self.env.repo, &self.env.branch)),
            outcome: None,
        })
    }

    // --- Claude Code -------------------------------------------------------

    /// Set the workspace up, and install Claude Code in the container if
    /// the image doesn't have it (into `~/.local/bin`, with the official
    /// installer).
    pub fn prepare_claude(&mut self) -> Result<(), String> {
        if !self.ready {
            self.prepare()?;
            self.ready = true;
        }
        let script = format!("{CLAUDE_PATH}\ncommand -v claude >/dev/null || curl -fsSL https://claude.ai/install.sh | bash");
        let (code, out) = self.exec_shell(&script, None, 600)?;
        check(code, &out, "installing Claude Code in the container (bake it into the image to skip this)")
    }

    /// `claude <args>` in the container, in /workspace. Each of `env` is
    /// passed through from the docker process's environment, so values
    /// (tokens) never show up in a command line.
    pub fn claude_command(&self, env: &[&str], args: &[String]) -> Command {
        let mut cmd = Command::new(&self.env.docker);
        cmd.args(["exec", "--interactive", "--workdir", "/workspace"]);
        for name in env {
            cmd.args(["--env", name]);
        }
        let script = format!("{CLAUDE_PATH}\nexec claude \"$@\"");
        cmd.args([self.env.container.as_str(), "sh", "-c", &script, "claude"]).args(args);
        cmd
    }

    /// Stop every process in the container whose command line has `tag`
    /// (killing `docker exec` doesn't stop what it started).
    pub fn kill_tagged(&self, tag: &str) {
        // The tag is split in two so this script's own command line doesn't match.
        let (a, b) = tag.split_at(tag.len() / 2);
        let script = format!(
            "a='{a}'; b='{b}'; for p in /proc/[0-9]*; do \
             grep -qF -- \"$a$b\" \"$p/cmdline\" 2>/dev/null && kill \"${{p#/proc/}}\"; done; true"
        );
        let _ = self.exec(&["sh", "-c", &script], None, 30);
    }

    // --- setup -------------------------------------------------------------

    fn prepare(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.env.task_dir).map_err(|e| e.to_string())?;
        match self.env.mode {
            WorkspaceMode::Write => self.prepare_writer()?,
            // A scratch checkout is a review checkout nobody reviews.
            WorkspaceMode::Review | WorkspaceMode::Scratch => self.prepare_reviewer()?,
        }
        self.ensure_container()
    }

    fn prepare_writer(&self) -> Result<(), String> {
        let dir = self.env.writer_dir();
        if dir.join(".git").exists() {
            // The container can't reach the project: bring its branches in
            // here, so the engineer can catch up with the default branch and
            // approved task branches. Best effort: a moved project doesn't
            // stop work on the task.
            let _ = git(&dir, &["fetch", "--quiet", "--prune", "origin"]);
            // `fetch` leaves `origin/HEAD` where the clone found it: follow
            // the project's default branch if it moved (a rename, a rewrite).
            let _ = git(&dir, &["remote", "set-head", "origin", "--auto"]);
            // Reviewers take the clone's own default branch as `base`: keep
            // it at the project's, so a rebased branch shows only its work.
            if let Ok(default) = git(&dir, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
                let default = default.trim();
                let local = default.trim_start_matches("origin/");
                let _ = git(&dir, &["branch", "--quiet", "--force", local, default]);
            }
            return Ok(());
        }
        clone(&self.env.repo, &dir)?;
        git(&dir, &["checkout", "--quiet", "-B", &self.env.branch])?;
        self.set_identity(&dir)
    }

    fn prepare_reviewer(&self) -> Result<(), String> {
        let dir = self.env.host_dir();
        let writer = self.env.writer_dir();
        // The writer's clone is only refreshed when the engineer is asked, so
        // a checkout of the default branch comes from the project itself.
        let has_writer = !self.env.default_branch && writer.join(".git").exists();
        let source = if has_writer { &writer } else { &self.env.repo };
        if !dir.join(".git").exists() {
            clone(source, &dir)?;
            self.set_identity(&dir)?;
        }
        git(&dir, &["remote", "set-url", "origin", &source.display().to_string()])?;
        git(&dir, &["fetch", "--quiet", "--prune", "origin"])?;
        if !has_writer {
            // `origin/HEAD` is set once, at clone time (and from the writer's
            // clone, if that was the source then): point it at the project's
            // default branch as it is now. A project with no commits has none.
            let _ = git(&dir, &["remote", "set-head", "origin", "--auto"]);
        }
        let (head, base) = if has_writer {
            // The writer's default branch is where the task started.
            let default = git(&writer, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
                .map(|r| r.trim().trim_start_matches("origin/").to_string())
                .unwrap_or_else(|_| "main".to_string());
            (format!("origin/{}", self.env.branch), format!("origin/{default}"))
        } else {
            ("origin/HEAD".to_string(), "origin/HEAD".to_string())
        };
        if !resolves(&dir, &head) {
            return Err(match (has_writer, self.env.mode) {
                (_, WorkspaceMode::Scratch) => "There's nothing to look at yet: the project has no commits.".to_string(),
                (true, _) => format!("Nothing to review yet: the engineer hasn't committed anything on `{}`.", self.env.branch),
                (false, _) => "Nothing to review yet: the project has no commits and no engineer has started.".to_string(),
            });
        }
        git(&dir, &["checkout", "--quiet", "--force", "-B", "review", &head])?;
        // A project that had no commits has no base branch: start from nothing.
        let base = match resolves(&dir, &base) {
            true => base,
            false => empty_commit(&dir)?,
        };
        git(&dir, &["branch", "--quiet", "--force", "base", &base])?;
        // Untracked files go; ignored ones (build caches) stay.
        git(&dir, &["clean", "-fdq"])?;
        Ok(())
    }

    fn set_identity(&self, dir: &Path) -> Result<(), String> {
        git(dir, &["config", "user.name", &format!("{} (pots agent)", self.env.agent)])?;
        git(dir, &["config", "user.email", &format!("{}@agents.pots.local", slug(&self.env.agent))])?;
        Ok(())
    }

    fn ensure_container(&self) -> Result<(), String> {
        let env = &self.env;
        let state = self.docker(&["inspect", "--format", "{{.State.Running}}", &env.container], None, Duration::from_secs(30));
        match state {
            Ok((0, out)) if String::from_utf8_lossy(&out).trim() == "true" => return Ok(()),
            Ok((0, _)) => {
                let (code, out) = self.docker(&["start", &env.container], None, Duration::from_secs(60))?;
                return check(code, &out, "docker start");
            }
            _ => {}
        }
        let mount = format!("{}:/workspace", env.host_dir().display());
        let (code, out) = self.docker(
            &[
                "run", "--detach", "--name", &env.container, "--label", "pots.agent=1",
                "--volume", &mount, "--workdir", "/workspace",
                "--entrypoint", "tail", &env.image, "-f", "/dev/null",
            ],
            None,
            // A first run may pull the image.
            Duration::from_secs(900),
        )?;
        check(code, &out, "docker run")?;
        // The clone belongs to the host user; let git in the container use it.
        let _ = self.exec_shell("git config --global --add safe.directory '*'", None, 30);
        Ok(())
    }

    // --- tools -------------------------------------------------------------

    fn bash(&self, input: &Value) -> Result<String, String> {
        let command = input["command"].as_str().ok_or("`command` is required.")?;
        let timeout = input["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT).clamp(1, MAX_TIMEOUT);
        let (code, out) = self.exec_shell(command, None, timeout)?;
        let out = truncate(&String::from_utf8_lossy(&out));
        Ok(match code {
            137 => format!("Killed: the command ran longer than {timeout}s (or ran out of memory).\n{out}"),
            code => format!("exit code {code}\n{out}"),
        })
    }

    fn read_file(&self, input: &Value) -> Result<String, String> {
        let path = input["path"].as_str().ok_or("`path` is required.")?;
        let text = self.cat(path)?;
        let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = input["limit"].as_u64().map_or(DEFAULT_READ_LINES, |l| l.max(1) as usize);
        Ok(number_lines(&text, offset, limit))
    }

    fn write_file(&self, input: &Value) -> Result<String, String> {
        let path = input["path"].as_str().ok_or("`path` is required.")?;
        let content = input["content"].as_str().ok_or("`content` is required.")?;
        self.put(path, content)?;
        Ok(format!("Wrote {path} ({} lines).", content.lines().count()))
    }

    fn edit_file(&self, input: &Value) -> Result<String, String> {
        let path = input["path"].as_str().ok_or("`path` is required.")?;
        let old = input["old_string"].as_str().ok_or("`old_string` is required.")?;
        let new = input["new_string"].as_str().ok_or("`new_string` is required.")?;
        let all = input["replace_all"].as_bool().unwrap_or(false);
        let text = self.cat(path)?;
        let (edited, n) = apply_edit(&text, old, new, all)?;
        self.put(path, &edited)?;
        Ok(format!("Edited {path} ({n} replacement{}).", if n == 1 { "" } else { "s" }))
    }

    fn cat(&self, path: &str) -> Result<String, String> {
        let (code, out) = self.exec(&["cat", "--", path], None, 60)?;
        if code != 0 {
            return Err(format!("Couldn't read {path}: {}", String::from_utf8_lossy(&out).trim()));
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn put(&self, path: &str, content: &str) -> Result<(), String> {
        let script = "mkdir -p \"$(dirname \"$1\")\" && cat > \"$1\"";
        let (code, out) = self.exec(&["sh", "-c", script, "sh", path], Some(content.as_bytes()), 60)?;
        check(code, &out, &format!("writing {path}"))
    }

    // --- processes ---------------------------------------------------------

    /// Run a shell command in the container, stderr merged into stdout,
    /// killed after `timeout` seconds.
    fn exec_shell(&self, command: &str, stdin: Option<&[u8]>, timeout: u64) -> Result<(i32, Vec<u8>), String> {
        let script = format!("exec 2>&1\n{command}");
        let secs = timeout.to_string();
        self.exec(&["timeout", "-s", "KILL", &secs, "sh", "-c", &script], stdin, timeout)
    }

    fn exec(&self, argv: &[&str], stdin: Option<&[u8]>, timeout: u64) -> Result<(i32, Vec<u8>), String> {
        let mut args = vec!["exec", "--interactive", "--workdir", "/workspace", self.env.container.as_str()];
        args.extend_from_slice(argv);
        // The in-container `timeout` does the killing; this is a backstop.
        self.docker(&args, stdin, Duration::from_secs(timeout + 30))
    }

    fn docker(&self, args: &[&str], stdin: Option<&[u8]>, timeout: Duration) -> Result<(i32, Vec<u8>), String> {
        run(Command::new(&self.env.docker).args(args), stdin, timeout)
            .map_err(|e| format!("{} {}: {e}", self.env.docker, args.first().unwrap_or(&"")))
    }
}

fn check(code: i32, out: &[u8], what: &str) -> Result<(), String> {
    match code {
        0 => Ok(()),
        _ => Err(format!("{what} failed ({code}): {}", String::from_utf8_lossy(out).trim())),
    }
}

fn clone(from: &Path, to: &Path) -> Result<(), String> {
    let parent = to.parent().ok_or("bad path")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let (from, to) = (from.display().to_string(), to.display().to_string());
    git(parent, &["clone", "--quiet", &from, &to]).map(drop)
}

/// Whether `rev` names a commit in the repository at `dir`.
fn resolves(dir: &Path, rev: &str) -> bool {
    git(dir, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")]).is_ok()
}

/// A commit of the empty tree with no parents, to diff a project that had
/// no commits against.
fn empty_commit(dir: &Path) -> Result<String, String> {
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let commit = git(dir, &["commit-tree", EMPTY_TREE, "-m", "Empty project (no commits when the task started)"])?;
    Ok(commit.trim().to_string())
}

/// Run git on the host; stdout on success.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let (code, out) = run(Command::new("git").arg("-C").arg(dir).args(args), None, Duration::from_secs(300))
        .map_err(|e| format!("git: {e}"))?;
    let text = String::from_utf8_lossy(&out).into_owned();
    match code {
        0 => Ok(text),
        _ => Err(format!("git {} failed: {}", args.join(" "), text.trim())),
    }
}

/// Run a process with optional stdin, collecting stdout and stderr together
/// and killing it after `timeout`.
fn run(cmd: &mut Command, stdin: Option<&[u8]>, timeout: Duration) -> std::io::Result<(i32, Vec<u8>)> {
    let mut child = cmd
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let data = data.to_vec();
        thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(50));
    };
    let mut bytes = out.join().unwrap_or_default();
    bytes.extend(err.join().unwrap_or_default());
    Ok((status.code().unwrap_or(137), bytes))
}

/// Keep the start and the end of long output; the end usually matters most.
pub fn truncate(s: &str) -> String {
    let n = s.chars().count();
    if n <= MAX_OUTPUT {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX_OUTPUT * 2 / 5).collect();
    let tail: String = s.chars().skip(n - MAX_OUTPUT * 3 / 5).collect();
    format!("{head}\n[… {} characters omitted …]\n{tail}", n - MAX_OUTPUT)
}

/// `cat -n`-style lines `offset..offset+limit` (1-based) of `text`.
pub fn number_lines(text: &str, offset: usize, limit: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return "(empty file)".to_string();
    }
    if offset > total {
        return format!("(the file has only {total} lines)");
    }
    let start = offset - 1;
    let end = (start + limit).min(total);
    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{:>6}\t{line}\n", start + i + 1));
    }
    if start > 0 || end < total {
        out.push_str(&format!("(lines {}-{end} of {total})\n", start + 1));
    }
    truncate(&out)
}

/// Replace `old` with `new` in `text`: exactly one occurrence, or all of
/// them with `all`. Returns the new text and how many were replaced.
pub fn apply_edit(text: &str, old: &str, new: &str, all: bool) -> Result<(String, usize), String> {
    if old.is_empty() {
        return Err("`old_string` is empty; use write_file to create a file.".into());
    }
    match text.matches(old).count() {
        0 => Err("`old_string` wasn't found. Read the file again and copy the text exactly, including whitespace.".into()),
        n if n > 1 && !all => Err(format!(
            "`old_string` appears {n} times. Include more surrounding lines to pick one, or set replace_all."
        )),
        n => Ok((text.replace(old, new), n)),
    }
}

/// Bring an approved task branch into the project repository as a branch
/// of the same name (the owner's checkout and other branches are left
/// alone). Returns the new tip.
pub fn accept_branch(repo: &Path, source: &str, branch: &str) -> Result<String, String> {
    git(repo, &["fetch", "--quiet", source, &format!("+{branch}:{branch}")])?;
    Ok(git(repo, &["rev-parse", "--short", branch])?.trim().to_string())
}

// --- merge conflicts between tasks -------------------------------------------

/// The tool coordinators call to see how open task branches would merge.
pub const CHECK_MERGE_CONFLICTS: &str = "check_merge_conflicts";

pub fn check_merge_conflicts_spec() -> ToolSpec {
    ToolSpec {
        name: CHECK_MERGE_CONFLICTS,
        description: "See how the open task branches of a repository would merge, without touching anything: for \
                      each task with commits, the files it changes and whether it still merges cleanly into the \
                      project's default branch as it is now, and for each pair of tasks, whether merging both \
                      would conflict (and in which files) or only touch the same files. It sees committed work \
                      only. Use it before starting a \
                      task that may touch the same code as open ones, and when the owner asks about merging.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string", "description": "Only this channel's repository (name without the #). Default: every repository with tasks."}
            },
            "additionalProperties": false
        }),
    }
}

/// A task whose branch the conflict check looks at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskBranch {
    /// How to name it to the agent: where it's discussed.
    pub label: String,
    /// The writer's clone holding the branch.
    pub clone: PathBuf,
    pub branch: String,
    /// Where its merge request stands, if it has one.
    pub status: Option<String>,
}

/// Everything a `check_merge_conflicts` call needs, gathered when the
/// reply starts so the git work can run off the UI thread.
#[derive(Clone, Debug, Default)]
pub struct ConflictCheck {
    /// Where throwaway check repositories go (`~/.pots/tasks`).
    pub scratch: PathBuf,
    /// Each repository with tasks: its channels (lowercase names), and its tasks.
    pub repos: Vec<(PathBuf, Vec<String>, Vec<TaskBranch>)>,
}

impl ConflictCheck {
    /// Run the check: the report, and whether it failed. Blocks on git.
    pub fn run(&self, input: &Value) -> (String, bool) {
        let channel = input["channel"].as_str().map(|c| c.trim().trim_start_matches('#').to_lowercase()).filter(|c| !c.is_empty());
        let repos: Vec<_> = match &channel {
            Some(c) => self.repos.iter().filter(|(_, channels, _)| channels.contains(c)).collect(),
            None => self.repos.iter().collect(),
        };
        if repos.is_empty() {
            return match channel {
                Some(c) => (format!("No tasks with a workspace in #{c}'s repository."), false),
                None => ("No tasks with a workspace yet.".to_string(), false),
            };
        }
        let mut out = String::new();
        for (repo, _, tasks) in repos {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("## {}\n", repo.display()));
            match check_conflicts(repo, tasks, &self.scratch) {
                Ok(report) => out.push_str(&report.render()),
                Err(e) => out.push_str(&format!("Couldn't check: {e}\n")),
            }
        }
        (truncate(&out), false)
    }

    /// What stands in the way of merging `branch` into `repo` (see
    /// [`Report::warning`]). Blocks on git; a check that fails says nothing.
    pub fn warning(&self, repo: &Path, branch: &str) -> Option<String> {
        let (repo, _, tasks) = self.repos.iter().find(|(r, _, _)| r == repo)?;
        check_conflicts(repo, tasks, &self.scratch).ok()?.warning(branch)
    }
}

/// How the tasks of one repository would merge.
#[derive(Debug, Default)]
pub struct Report {
    pub tasks: Vec<Checked>,
    /// Pairs of `tasks` (by index) that conflict or change the same files.
    pub pairs: Vec<Pair>,
}

/// One task's branch against the project's default branch.
#[derive(Debug)]
pub struct Checked {
    pub task: TaskBranch,
    pub state: State,
}

#[derive(Debug, PartialEq, Eq)]
pub enum State {
    /// The clone has no such branch.
    NoCommits,
    /// Nothing that isn't in the default branch `into` already.
    NothingToMerge { into: String },
    /// No common history with `into` (a clone of an older, rewritten history):
    /// there's nothing to diff against, so it's left out of the checks.
    Unrelated { into: String },
    /// Work to merge into `into`: the files it changes, and the ones that
    /// conflict with `into` as it is now, if any.
    Open { into: String, files: Vec<String>, conflicts: Option<Vec<String>> },
}

#[derive(Debug)]
pub struct Pair {
    pub a: usize,
    pub b: usize,
    /// The files merging both would conflict in, if it would.
    pub conflicts: Option<Vec<String>>,
    pub shared: Vec<String>,
}

impl Report {
    /// The report as the agent reads it.
    pub fn render(&self) -> String {
        if self.tasks.is_empty() {
            return "No task has committed work yet.\n".to_string();
        }
        let mut out = "Tasks:\n".to_string();
        let mut open = 0;
        for Checked { task, state } in &self.tasks {
            let (label, branch) = (&task.label, &task.branch);
            let status = task.status.as_deref().map(|s| format!(" ({s})")).unwrap_or_default();
            out.push_str(&match state {
                State::NoCommits => format!("- {label}{status}: `{branch}` has no commits yet.\n"),
                State::NothingToMerge { into } => format!(
                    "- {label}{status}: nothing to merge, `{branch}` has no commits that aren't in {into} (none yet, or already merged).\n"
                ),
                State::Unrelated { into } => format!(
                    "- {label}{status}: `{branch}` shares no history with {into} (its clone is from an older history), so it can't be checked.\n"
                ),
                State::Open { into, files, conflicts } => {
                    open += 1;
                    let verdict = match conflicts {
                        None => format!("merges cleanly into {into}"),
                        Some(c) => format!("**conflicts with {into} as it is now** in {}", list(c)),
                    };
                    format!("- {label}{status}: `{branch}`, changes {}; {verdict}.\n", list(files))
                }
            });
        }
        if open > 1 {
            out.push_str("\nBetween tasks:\n");
            if self.pairs.is_empty() {
                out.push_str("- No two tasks touch the same files.\n");
            }
            for pair in &self.pairs {
                let both = format!("{} and {}", self.tasks[pair.a].task.label, self.tasks[pair.b].task.label);
                out.push_str(&match &pair.conflicts {
                    Some(c) => format!(
                        "- **{both} conflict** in {}: whichever merges second has to be rebased onto the first.\n",
                        list(c)
                    ),
                    None => format!("- {both} both change {}, but merge cleanly.\n", list(&pair.shared)),
                });
            }
        }
        out
    }

    /// What stands in the way of merging `branch`, in markdown: conflicts
    /// with the default branch or with other open tasks. `None` if nothing.
    pub fn warning(&self, branch: &str) -> Option<String> {
        let me = self.tasks.iter().position(|c| c.task.branch == branch)?;
        let mut lines = Vec::new();
        match &self.tasks[me].state {
            State::Open { into, conflicts: Some(c), .. } => {
                lines.push(format!("- Conflicts with {into} as it is now, in {}.", list(c)));
            }
            State::Unrelated { into } => lines.push(format!("- Shares no history with {into}: it can't be merged as it is.")),
            _ => {}
        }
        for pair in &self.pairs {
            let other = match (pair.a == me, pair.b == me) {
                (true, _) => pair.b,
                (_, true) => pair.a,
                _ => continue,
            };
            if let Some(c) = &pair.conflicts {
                let other = &self.tasks[other].task;
                lines.push(format!("- Conflicts with {} (`{}`), in {}.", other.label, other.branch, list(c)));
            }
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

/// Check how `tasks` (branches in their own clones of `repo`) would merge
/// into `repo`'s default branch and with each other. Nothing in `repo` or
/// the clones changes: the merges are tried in a throwaway bare repository
/// under `scratch` that borrows their objects. Tasks nobody has worked on
/// (no clone) are left out.
pub fn check_conflicts(repo: &Path, tasks: &[TaskBranch], scratch: &Path) -> Result<Report, String> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = scratch.join(format!(".conflict-check-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let result = check_in(repo, tasks, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn check_in(repo: &Path, tasks: &[TaskBranch], dir: &Path) -> Result<Report, String> {
    let mut report = Report::default();
    let mut objects = vec![objects_dir(repo)?];
    // Each task with a branch: its tip, and the default branch it started from.
    let mut tips = Vec::new();
    for task in tasks {
        if !task.clone.join(".git").exists() {
            continue;
        }
        let Ok(tip) = git(&task.clone, &["rev-parse", "--verify", "--quiet", &format!("{}^{{commit}}", task.branch)]) else {
            report.tasks.push(Checked { task: task.clone(), state: State::NoCommits });
            continue;
        };
        let default = git(&task.clone, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
            .map(|r| r.trim().trim_start_matches("origin/").to_string())
            .ok();
        objects.push(objects_dir(&task.clone)?);
        tips.push((task, tip.trim().to_string(), default));
    }
    if tips.is_empty() {
        return Ok(report);
    }

    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    git(dir, &["init", "--quiet", "--bare"])?;
    let alternates = objects.iter().map(|o| o.display().to_string()).collect::<Vec<_>>().join("\n") + "\n";
    std::fs::write(dir.join("objects/info/alternates"), alternates).map_err(|e| e.to_string())?;

    // The default branch as it is in the project now (not when the task started).
    let base_of = |default: &Option<String>| {
        let names = default.iter().map(|d| format!("refs/heads/{d}")).chain(["HEAD".to_string()]);
        names
            .filter_map(|name| git(repo, &["rev-parse", "--verify", "--quiet", &format!("{name}^{{commit}}")]).ok())
            .map(|sha| sha.trim().to_string())
            .next()
    };
    // Open tasks: index in the report, tip, files.
    let mut open = Vec::new();
    for (task, tip, default) in tips {
        let into = default.clone().unwrap_or_else(|| "the default branch".to_string());
        let state = match base_of(&default) {
            // The project still has no commits: everything is new.
            None => State::Open { into, files: lines_of(&git(dir, &["ls-tree", "-r", "--name-only", &tip])?), conflicts: None },
            Some(base) if git(dir, &["merge-base", "--is-ancestor", &tip, &base]).is_ok() => State::NothingToMerge { into },
            // `base...tip` needs a merge base; without one, git refuses the diff.
            Some(base) if !shares_history(dir, &base, &tip)? => State::Unrelated { into },
            Some(base) => State::Open {
                into,
                files: lines_of(&git(dir, &["diff", "--name-only", &format!("{base}...{tip}")])?),
                conflicts: merge_tree(dir, &base, &tip)?,
            },
        };
        if let State::Open { files, .. } = &state {
            open.push((report.tasks.len(), tip, files.clone()));
        }
        report.tasks.push(Checked { task: task.clone(), state });
    }

    for (i, (a, a_tip, a_files)) in open.iter().enumerate() {
        for (b, b_tip, b_files) in &open[i + 1..] {
            let shared: Vec<String> = a_files.iter().filter(|f| b_files.contains(f)).cloned().collect();
            let conflicts = merge_tree(dir, a_tip, b_tip)?;
            if conflicts.is_some() || !shared.is_empty() {
                report.pairs.push(Pair { a: *a, b: *b, conflicts, shared });
            }
        }
    }
    Ok(report)
}

/// Whether `a` and `b` have a common ancestor in `dir`.
fn shares_history(dir: &Path, a: &str, b: &str) -> Result<bool, String> {
    let (code, out) = run(Command::new("git").arg("-C").arg(dir).args(["merge-base", a, b]), None, Duration::from_secs(300))
        .map_err(|e| format!("git: {e}"))?;
    match code {
        0 => Ok(true),
        // No merge base; anything else is a real failure.
        1 => Ok(false),
        _ => Err(format!("git merge-base {a} {b} failed: {}", String::from_utf8_lossy(&out).trim())),
    }
}

/// Merge `a` and `b` in `dir` without a work tree: `None` if it's clean,
/// or the conflicting files.
fn merge_tree(dir: &Path, a: &str, b: &str) -> Result<Option<Vec<String>>, String> {
    let args = ["merge-tree", "--write-tree", "--name-only", "--no-messages", "--allow-unrelated-histories", a, b];
    let (code, out) = run(Command::new("git").arg("-C").arg(dir).args(args), None, Duration::from_secs(300))
        .map_err(|e| format!("git: {e}"))?;
    let text = String::from_utf8_lossy(&out);
    match code {
        0 => Ok(None),
        // The merged tree's id, then the conflicting files.
        1 => Ok(Some(lines_of(&text).into_iter().skip(1).collect())),
        _ => Err(format!("git merge-tree failed (it needs git 2.38 or newer): {}", text.trim())),
    }
}

/// Where the repository at `dir` keeps its objects.
fn objects_dir(dir: &Path) -> Result<PathBuf, String> {
    let common = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    Ok(PathBuf::from(common.trim()).join("objects"))
}

fn lines_of(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

/// `a`, `b` and 3 more files.
fn list(files: &[String]) -> String {
    const SHOWN: usize = 8;
    if files.is_empty() {
        return "no files".to_string();
    }
    let shown = files.iter().take(SHOWN).map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ");
    match files.len() > SHOWN {
        true => format!("{shown} and {} more", files.len() - SHOWN),
        false => shown,
    }
}

pub fn specs(mode: WorkspaceMode) -> Vec<ToolSpec> {
    let mut specs = vec![
        ToolSpec {
            name: "request_merge",
            description: "Ask the owner to accept this task's branch into the project repository. It shows them the \
                          branch's commits with Approve / Reject buttons. Call it when the work is committed and \
                          ready (reviewed, if there's a reviewer). After calling it, stop: their answer arrives as a \
                          new message.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string", "description": "What the branch does and how it was tested, in a few lines."}
                },
                "required": ["summary"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "bash",
            description: "Run a shell command in your container, in /workspace. stdout and stderr are combined; \
                          long output is shortened in the middle. Use it to explore (ls, rg, git), build and test.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT, "description": "Seconds before the command is killed (default 120)."}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "read_file",
            description: "Read a text file in your workspace, with line numbers.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative to /workspace, or absolute."},
                    "offset": {"type": "integer", "minimum": 1, "description": "First line to read (default 1)."},
                    "limit": {"type": "integer", "minimum": 1, "description": "How many lines (default 500)."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
    ];
    if mode == WorkspaceMode::Write {
        specs.push(ToolSpec {
            name: "write_file",
            description: "Create or overwrite a file in your workspace with the given content.",
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        });
        specs.push(ToolSpec {
            name: "edit_file",
            description: "Replace text in a file. `old_string` must match exactly (including whitespace) and be \
                          unique in the file, unless replace_all is set.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        });
    }
    // A scratch checkout has no branch of its own to merge.
    if mode == WorkspaceMode::Scratch {
        specs.retain(|s| s.name != "request_merge");
    }
    specs
}

pub fn is_tool(name: &str) -> bool {
    matches!(name, "bash" | "read_file" | "write_file" | "edit_file")
}

/// What the UI shows while a workspace tool runs.
pub fn activity(name: &str, input: &Value) -> Option<String> {
    let path = input["path"].as_str().unwrap_or("a file");
    Some(match name {
        "bash" => match input["command"].as_str() {
            Some(cmd) => {
                let line = cmd.lines().next().unwrap_or_default();
                let short: String = line.chars().take(60).collect();
                let more = if short.len() < cmd.len() { "…" } else { "" };
                format!("Running `{short}{more}`")
            }
            None => "Running a command…".to_string(),
        },
        "read_file" => format!("Reading {path}…"),
        "write_file" => format!("Writing {path}…"),
        "edit_file" => format!("Editing {path}…"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_repo_wants_a_git_repository() {
        let dir = std::env::temp_dir().join(format!("pots-check-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.display().to_string();
        assert!(check_repo(&format!("{path}/missing")).unwrap_err().contains("isn't a folder"));
        assert!(check_repo(&path).unwrap_err().contains("git init"));
        std::fs::create_dir(dir.join(".git")).unwrap();
        assert_eq!(check_repo(&format!(" {path} ")), Ok(dir.clone()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edits_need_a_unique_match_unless_replacing_all() {
        let text = "a = 1\nb = 1\n";
        assert_eq!(apply_edit(text, "a = 1", "a = 2", false).unwrap(), ("a = 2\nb = 1\n".into(), 1));
        assert!(apply_edit(text, "= 1", "= 2", false).unwrap_err().contains("2 times"));
        assert_eq!(apply_edit(text, "= 1", "= 2", true).unwrap(), ("a = 2\nb = 2\n".into(), 2));
        assert!(apply_edit(text, "c", "d", false).unwrap_err().contains("wasn't found"));
        assert!(apply_edit(text, "", "d", false).is_err());
    }

    #[test]
    fn numbers_a_window_of_lines() {
        let text = "one\ntwo\nthree\nfour\n";
        assert_eq!(number_lines(text, 1, 10), "     1\tone\n     2\ttwo\n     3\tthree\n     4\tfour\n");
        assert_eq!(number_lines(text, 2, 2), "     2\ttwo\n     3\tthree\n(lines 2-3 of 4)\n");
        assert_eq!(number_lines(text, 9, 2), "(the file has only 4 lines)");
        assert_eq!(number_lines("", 1, 2), "(empty file)");
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let s: String = (0..30_000).map(|i| if i % 100 == 99 { '\n' } else { 'x' }).collect::<String>() + "END";
        let t = truncate(&s);
        assert!(t.len() < s.len() && t.ends_with("END") && t.contains("characters omitted"));
    }

    #[test]
    fn names_are_slugged_for_containers_and_branches() {
        assert_eq!(slug("Fix login page!"), "fix-login-page");
        assert_eq!(slug("修 bug"), "bug");
        assert_eq!(slug("修"), "");
        let env = Env::new(WorkspaceMode::Review, "docker".into(), "img".into(), "/r".into(), Path::new("/tasks"), "t3-login", "Reviewer");
        assert_eq!(env.container, "pots-agent-t3-login-reviewer");
        assert_eq!(env.branch, "pots/t3-login");
        assert_eq!(env.host_dir(), PathBuf::from("/tasks/t3-login/review-reviewer"));
        assert_eq!(env.writer_dir(), PathBuf::from("/tasks/t3-login/repo"));
    }

    #[test]
    fn writer_clone_and_reviewer_checkout_follow_the_branch() {
        let tmp = std::env::temp_dir().join(format!("pots-sandbox-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]).unwrap();
        git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "--allow-empty", "-m", "start"]).unwrap();
        let tasks = tmp.join("tasks");
        let env = |mode, agent| Env::new(mode, "docker".into(), "img".into(), repo.clone(), &tasks, "t1", agent);

        // A reviewer before any engineer reviews the default branch.
        let reviewer = Session::new(env(WorkspaceMode::Review, "Reviewer"));
        reviewer.prepare_reviewer().unwrap();

        let writer = Session::new(env(WorkspaceMode::Write, "Engineer"));
        writer.prepare_writer().unwrap();
        let wdir = writer.env.writer_dir();
        assert_eq!(git(&wdir, &["branch", "--show-current"]).unwrap().trim(), "pots/t1");
        std::fs::write(wdir.join("a.txt"), "hi").unwrap();
        git(&wdir, &["add", "a.txt"]).unwrap();
        git(&wdir, &["commit", "--quiet", "-m", "add a"]).unwrap();
        assert_eq!(git(&wdir, &["log", "-1", "--format=%an"]).unwrap().trim(), "Engineer (pots agent)");

        // A merge request lists the branch's commits; accepting it creates the
        // branch in the project repo.
        let mut w = Session::new(writer.env.clone());
        let Card::Merge { commits, source, branch, .. } = w.merge_card(&json!({"summary": "s"}), None).unwrap() else { panic!() };
        assert_eq!(commits.len(), 1);
        assert!(commits[0].ends_with(" add a"));
        let tip = accept_branch(&repo, &source, &branch).unwrap();
        assert_eq!(git(&repo, &["rev-parse", "--short", "pots/t1"]).unwrap().trim(), tip);
        assert_eq!(git(&repo, &["branch", "--show-current"]).unwrap().trim(), "main", "the owner's checkout is untouched");

        // The reviewer's next refresh switches to the engineer's branch.
        std::fs::write(reviewer.env.host_dir().join("scratch"), "x").unwrap();
        reviewer.prepare_reviewer().unwrap();
        let rdir = reviewer.env.host_dir();
        assert_eq!(git(&rdir, &["log", "--format=%s", "base..HEAD"]).unwrap().trim(), "add a");
        assert!(!rdir.join("scratch").exists(), "untracked files are cleaned");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_conflict_check_tries_merges_without_touching_anything() {
        let tmp = std::env::temp_dir().join(format!("pots-sandbox-conflicts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        let commit = |dir: &Path, file: &str, text: &str| {
            std::fs::write(dir.join(file), text).unwrap();
            git(dir, &["add", file]).unwrap();
            git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", file]).unwrap();
        };
        git(&repo, &["init", "--quiet", "-b", "main"]).unwrap();
        commit(&repo, "a.txt", "one\n");
        commit(&repo, "b.txt", "two\n");
        let tasks = tmp.join("tasks");
        // Each task: an engineer's clone with one commit.
        let task = |key: &str, edit: Option<(&str, &str)>| {
            let env = Env::new(WorkspaceMode::Write, "docker".into(), "img".into(), repo.clone(), &tasks, key, "Engineer");
            Session::new(env.clone()).prepare_writer().unwrap();
            if let Some((file, text)) = edit {
                commit(&env.writer_dir(), file, text);
            }
            TaskBranch { label: key.to_string(), clone: env.writer_dir(), branch: env.branch, status: None }
        };
        let a1 = task("t1-a1", Some(("a.txt", "one, says t1\n")));
        let a2 = task("t2-a2", Some(("a.txt", "one, says t2\n")));
        let c = task("t3-c", Some(("c.txt", "new\n")));
        let b = task("t4-b", Some(("b.txt", "two, says t4\n")));
        let merged = task("t5-merged", Some(("b.txt", "two, merged\n")));
        let idle = task("t6-idle", None);
        let never = TaskBranch { label: "t7".into(), clone: tasks.join("t7/repo"), branch: "pots/t7".into(), status: None };
        // The owner merges t5 into main after the others started.
        git(&repo, &["fetch", "--quiet", &merged.clone.display().to_string(), "pots/t5-merged"]).unwrap();
        git(&repo, &["merge", "--quiet", "--ff-only", "FETCH_HEAD"]).unwrap();
        let main = git(&repo, &["rev-parse", "main"]).unwrap();

        let all = [a1.clone(), a2, c, b, merged, idle, never];
        let checked = check_conflicts(&repo, &all, &tasks).unwrap();
        let report = checked.render();
        let line = |label: &str| report.lines().find(|l| l.starts_with(&format!("- {label}"))).unwrap_or_default().to_string();
        assert!(line("t1-a1").contains("merges cleanly into main"), "{report}");
        assert!(line("t3-c").contains("changes `c.txt`; merges cleanly"), "{report}");
        assert!(line("t4-b").contains("conflicts with main as it is now** in `b.txt`"), "{report}");
        assert!(line("t5-merged").contains("nothing to merge"), "{report}");
        assert!(line("t6-idle").contains("nothing to merge"), "{report}");
        assert!(!report.contains("t7"), "a task nobody worked on is left out: {report}");
        assert!(report.contains("**t1-a1 and t2-a2 conflict** in `a.txt`"), "{report}");
        assert!(!report.contains("t1-a1 and t3-c"), "disjoint tasks aren't a pair to mention: {report}");
        // What a merge card warns about, per branch.
        assert_eq!(checked.warning("pots/t1-a1").unwrap(), "- Conflicts with t2-a2 (`pots/t2-a2`), in `a.txt`.");
        assert_eq!(checked.warning("pots/t4-b").unwrap(), "- Conflicts with main as it is now, in `b.txt`.");
        assert_eq!(checked.warning("pots/t3-c"), None);
        assert_eq!(checked.warning("pots/t5-merged"), None);

        // Nothing changed: not the project, not the clones, no scratch left.
        assert_eq!(git(&repo, &["rev-parse", "main"]).unwrap(), main);
        assert_eq!(git(&repo, &["status", "--porcelain"]).unwrap(), "");
        assert_eq!(git(&a1.clone, &["status", "--porcelain"]).unwrap(), "");
        assert!(std::fs::read_dir(&tasks).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with('.')));

        // The next time the engineer is asked, their clone sees the new main,
        // and once they rebase, the reviewer sees only their work.
        let env = |mode, agent| Env::new(mode, "docker".into(), "img".into(), repo.clone(), &tasks, "t3-c", agent);
        Session::new(env(WorkspaceMode::Write, "Engineer")).prepare_writer().unwrap();
        let clone = tasks.join("t3-c/repo");
        assert_eq!(git(&clone, &["rev-parse", "origin/main"]).unwrap(), main);
        git(&clone, &["-c", "user.name=t", "-c", "user.email=t@t", "rebase", "--quiet", "origin/main"]).unwrap();
        let reviewer = Session::new(env(WorkspaceMode::Review, "Reviewer"));
        reviewer.prepare_reviewer().unwrap();
        assert_eq!(git(&reviewer.env.host_dir(), &["log", "--format=%s", "base..HEAD"]).unwrap().trim(), "c.txt");

        let check = ConflictCheck { scratch: tasks.clone(), repos: vec![(repo.clone(), vec!["backend".into()], all.to_vec())] };
        assert!(check.run(&json!({"channel": "#Backend"})).0.contains("t1-a1"));
        assert!(check.run(&json!({"channel": "frontend"})).0.contains("No tasks"));

        // A merge request carries the warning; without the other tasks, none.
        let env = Env::new(WorkspaceMode::Review, "docker".into(), "img".into(), repo.clone(), &tasks, "t1-a1", "Reviewer");
        let card = |others| match Session::new(env.clone()).merge_card(&json!({"summary": "s"}), others).unwrap() {
            Card::Merge { conflicts, .. } => conflicts,
            _ => panic!(),
        };
        assert_eq!(card(Some(&check)).unwrap(), "- Conflicts with t2-a2 (`pots/t2-a2`), in `a.txt`.");
        assert_eq!(card(None), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_clone_of_an_older_history_does_not_stop_the_conflict_check() {
        let tmp = std::env::temp_dir().join(format!("pots-sandbox-unrelated-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        let commit = |dir: &Path, file: &str, text: &str| {
            std::fs::write(dir.join(file), text).unwrap();
            git(dir, &["add", file]).unwrap();
            git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", file]).unwrap();
        };
        git(&repo, &["init", "--quiet", "-b", "main"]).unwrap();
        commit(&repo, "a.txt", "one\n");
        let tasks = tmp.join("tasks");

        // A task cloned before the project's history was rewritten: its own root.
        let old = tasks.join("t0/repo");
        std::fs::create_dir_all(&old).unwrap();
        git(&old, &["init", "--quiet", "-b", "pots/t0"]).unwrap();
        commit(&old, "a.txt", "from before\n");
        let t0 = TaskBranch { label: "t0".into(), clone: old, branch: "pots/t0".into(), status: None };

        // A current task, checked after it.
        let env = Env::new(WorkspaceMode::Write, "docker".into(), "img".into(), repo.clone(), &tasks, "t1", "Engineer");
        Session::new(env.clone()).prepare_writer().unwrap();
        commit(&env.writer_dir(), "b.txt", "new\n");
        let t1 = TaskBranch { label: "t1".into(), clone: env.writer_dir(), branch: env.branch, status: None };

        let checked = check_conflicts(&repo, &[t0, t1], &tasks).unwrap();
        assert_eq!(checked.tasks[0].state, State::Unrelated { into: "the default branch".into() });
        let report = checked.render();
        assert!(report.contains("- t0: `pots/t0` shares no history with the default branch"), "{report}");
        assert!(report.contains("- t1: `pots/t1`, changes `b.txt`; merges cleanly into main"), "{report}");
        assert_eq!(checked.warning("pots/t0").unwrap(), "- Shares no history with the default branch: it can't be merged as it is.");
        assert_eq!(checked.warning("pots/t1"), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_scratch_checkout_is_thrown_away_and_never_reaches_the_task() {
        let tmp = std::env::temp_dir().join(format!("pots-sandbox-scratch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]).unwrap();
        git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "--allow-empty", "-m", "start"]).unwrap();
        let tasks = tmp.join("tasks");
        let env = |mode, agent| Env::new(mode, "docker".into(), "img".into(), repo.clone(), &tasks, "t1", agent);

        // Before any engineer: the project's default branch.
        let scratch = Session::new(env(WorkspaceMode::Scratch, "Worker-PM"));
        scratch.prepare_reviewer().unwrap();
        let sdir = scratch.env.host_dir();
        assert!(sdir.ends_with("t1/scratch-worker-pm"), "{}", sdir.display());
        assert_eq!(git(&sdir, &["log", "-1", "--format=%s"]).unwrap().trim(), "start");

        let writer = Session::new(env(WorkspaceMode::Write, "Engineer"));
        writer.prepare_writer().unwrap();
        let wdir = writer.env.writer_dir();
        std::fs::write(wdir.join("a.txt"), "hi").unwrap();
        git(&wdir, &["add", "a.txt"]).unwrap();
        git(&wdir, &["commit", "--quiet", "-m", "add a"]).unwrap();

        // Whatever the worker writes or commits is gone on its next ask, which
        // sees the task's latest commit; the engineer's branch never gets it.
        std::fs::write(sdir.join("INVENTORY.md"), "notes").unwrap();
        git(&sdir, &["add", "INVENTORY.md"]).unwrap();
        git(&sdir, &["commit", "--quiet", "-m", "inventory"]).unwrap();
        scratch.prepare_reviewer().unwrap();
        assert_eq!(git(&sdir, &["log", "-1", "--format=%s"]).unwrap().trim(), "add a");
        assert!(!sdir.join("INVENTORY.md").exists());
        assert!(!git(&wdir, &["log", "--format=%s", "--all"]).unwrap().contains("inventory"));

        let names: Vec<_> = specs(WorkspaceMode::Scratch).iter().map(|s| s.name).collect();
        assert_eq!(names, ["bash", "read_file"], "no merge requests and no file edits");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_channel_scratch_follows_the_project_not_an_old_channel_clone() {
        let tmp = std::env::temp_dir().join(format!("pots-sandbox-channel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        let commit = |dir: &Path, msg: &str| {
            git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "--allow-empty", "-m", msg]).unwrap();
        };
        let last = |dir: &Path| git(dir, &["log", "-1", "--format=%s"]).unwrap().trim().to_string();
        git(&repo, &["init", "--quiet", "-b", "old"]).unwrap();
        commit(&repo, "old start");
        let tasks = tmp.join("tasks");
        let env = |mode, agent| Env::new(mode, "docker".into(), "img".into(), repo.clone(), &tasks, "c-general", agent);

        // An engineer once worked in the channel itself, outside any thread,
        // and a checkout taken from that clone shows their commit.
        let writer = Session::new(env(WorkspaceMode::Write, "Engineer"));
        writer.prepare_writer().unwrap();
        let wdir = writer.env.writer_dir();
        commit(&wdir, "inventory");
        let stale = Session::new(env(WorkspaceMode::Scratch, "Worker-PM"));
        stale.prepare_reviewer().unwrap();
        assert_eq!(last(&stale.env.host_dir()), "inventory");

        // The owner starts a new history on `main` and drops the old branch.
        git(&repo, &["checkout", "--quiet", "--orphan", "main"]).unwrap();
        commit(&repo, "new start");
        git(&repo, &["branch", "--quiet", "-D", "old"]).unwrap();

        // Asked in the channel, the same checkout now shows the project's
        // default branch as it is, though the channel's clone never moved.
        let scratch = Session::new(env(WorkspaceMode::Scratch, "Worker-PM").on_default_branch(true));
        assert_eq!(scratch.env.host_dir(), stale.env.host_dir());
        scratch.prepare_reviewer().unwrap();
        let sdir = scratch.env.host_dir();
        assert_eq!(last(&sdir), "new start");
        assert_eq!(git(&sdir, &["symbolic-ref", "refs/remotes/origin/HEAD"]).unwrap().trim(), "refs/remotes/origin/main");
        assert!(scratch.env.describe().contains("default branch as it is now"));
        assert_eq!(last(&wdir), "inventory", "the channel's clone is left alone");

        // The engineer's clone follows the new default branch next time too.
        writer.prepare_writer().unwrap();
        assert_eq!(git(&wdir, &["symbolic-ref", "refs/remotes/origin/HEAD"]).unwrap().trim(), "refs/remotes/origin/main");
        assert_eq!(git(&wdir, &["log", "-1", "--format=%s", "main"]).unwrap().trim(), "new start");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_writer_is_told_that_questions_leave_the_clone_alone() {
        let env = Env::new(WorkspaceMode::Write, "docker".into(), "img".into(), "/r".into(), Path::new("/tasks"), "t1", "Engineer");
        for text in [env.describe(), env.describe_claude_code()] {
            assert!(text.contains("When the task asks you to change the project, commit"), "{text}");
            assert!(text.contains("no new, edited or deleted files and no commits"), "{text}");
        }
    }

    #[test]
    fn an_empty_project_can_be_worked_on_reviewed_and_merged() {
        let tmp = std::env::temp_dir().join(format!("pots-sandbox-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]).unwrap();
        let tasks = tmp.join("tasks");
        let env = |mode, agent| Env::new(mode, "docker".into(), "img".into(), repo.clone(), &tasks, "t1", agent);
        let reviewer = Session::new(env(WorkspaceMode::Review, "Reviewer"));

        // Nothing anywhere yet: the reviewer is told so plainly.
        let err = reviewer.prepare_reviewer().unwrap_err();
        assert!(err.starts_with("Nothing to review yet"), "{err}");

        let writer = Session::new(env(WorkspaceMode::Write, "Engineer"));
        writer.prepare_writer().unwrap();
        let wdir = writer.env.writer_dir();
        let err = reviewer.prepare_reviewer().unwrap_err();
        assert!(err.contains("hasn't committed anything on `pots/t1`"), "{err}");
        let mut w = Session::new(writer.env.clone());
        assert!(w.merge_card(&json!({}), None).unwrap_err().contains("no commits yet"));

        for (file, msg) in [("a.txt", "add a"), ("b.txt", "add b")] {
            std::fs::write(wdir.join(file), "hi").unwrap();
            git(&wdir, &["add", file]).unwrap();
            git(&wdir, &["commit", "--quiet", "-m", msg]).unwrap();
        }

        // The reviewer sees every commit against an empty base.
        reviewer.prepare_reviewer().unwrap();
        let rdir = reviewer.env.host_dir();
        assert_eq!(git(&rdir, &["log", "--format=%s", "base..HEAD"]).unwrap().trim(), "add b\nadd a");
        let diff = git(&rdir, &["diff", "--name-only", "base", "HEAD"]).unwrap();
        assert_eq!(diff.trim(), "a.txt\nb.txt");
        reviewer.prepare_reviewer().unwrap(); // refreshing again works too

        // Merging lists the whole branch and lands it in the empty project.
        let Card::Merge { commits, source, branch, .. } = w.merge_card(&json!({}), None).unwrap() else { panic!() };
        assert_eq!(commits.len(), 2);
        let tip = accept_branch(&repo, &source, &branch).unwrap();
        assert_eq!(git(&repo, &["rev-parse", "--short", "pots/t1"]).unwrap().trim(), tip);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Real containers: `cargo test -- --ignored docker` (needs docker and
    /// the `rust:latest` image).
    #[test]
    #[ignore]
    fn docker_engineer_commits_and_reviewer_sees_it() {
        let tmp = std::env::temp_dir().join(format!("pots-docker-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]).unwrap();
        std::fs::write(repo.join("README.md"), "# demo\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "init"]).unwrap();
        let task = format!("test{}", std::process::id());
        let env = |mode, agent| {
            Env::new(mode, find_docker(None), "rust:latest".into(), repo.clone(), &tmp.join("tasks"), &task, agent)
        };
        let mut eng = Session::new(env(WorkspaceMode::Write, "Engineer"));
        let mut rev = Session::new(env(WorkspaceMode::Review, "Reviewer"));

        let ok = |(text, err): (String, bool)| {
            assert!(!err, "{text}");
            text
        };
        ok(eng.run("write_file", &json!({"path": "src/add.rs", "content": "pub fn add(a: i32, b: i32) -> i32 { a - b }\n"})));
        ok(eng.run("edit_file", &json!({"path": "src/add.rs", "old_string": "a - b", "new_string": "a + b"})));
        assert!(ok(eng.run("read_file", &json!({"path": "src/add.rs"}))).contains("a + b"));
        let out = ok(eng.run("bash", &json!({"command": "git add -A && git commit -qm 'Add add()' && git log --format=%s -1"})));
        assert!(out.contains("exit code 0") && out.contains("Add add()"), "{out}");
        // Files written in the container are on the host, committed on the branch.
        let host = eng.env.writer_dir();
        assert_eq!(git(&host, &["log", "-1", "--format=%s"]).unwrap().trim(), "Add add()");
        assert!(std::fs::read_to_string(host.join("src/add.rs")).unwrap().contains("a + b"));

        // The reviewer sees the commit, can build it, and can't edit.
        let out = ok(rev.run("bash", &json!({"command": "git log --format=%s base..HEAD && rustc --crate-type=lib src/add.rs -o /tmp/a.rlib && echo BUILD_OK"})));
        assert!(out.contains("Add add()") && out.contains("BUILD_OK"), "{out}");
        assert!(rev.run("write_file", &json!({"path": "x", "content": ""})).1);

        // Failures and timeouts come back as results, not errors.
        assert!(ok(eng.run("bash", &json!({"command": "exit 3"}))).starts_with("exit code 3"));
        assert!(ok(eng.run("bash", &json!({"command": "sleep 5", "timeout": 1}))).starts_with("Killed"));
        assert!(eng.run("read_file", &json!({"path": "missing.rs"})).1);

        for s in [&eng, &rev] {
            let _ = s.docker(&["rm", "--force", &s.env.container], None, Duration::from_secs(60));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
