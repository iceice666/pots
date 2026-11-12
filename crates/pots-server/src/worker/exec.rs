//! worker 的執行環境（`proposal/security-and-operations.md` 的 Worker 沙箱）。
//!
//! - **容器**：每個 worker run 一個 Docker／Podman 容器（`sleep infinity` 常駐，指令用
//!   `exec` 進去跑），只掛：任務的工作目錄（`/workspace/<任務>`）、這次 run 的設定（`/run/pots`，
//!   唯讀，例如 MCP 設定）、輸出目錄（`/run/pots-out`，例如交回平台的 bundle）；Claude Code 後端
//!   另外掛任務自己的 home（`/home/pots`，跨 run 保留）與共用的 Claude Code 設定目錄
//!   （`/claude-config`，`CLAUDE_CONFIG_DIR`：官方登入與 session 在這裡）。home 一個任務一份，
//!   所以 worker 寫進 `~/.gitconfig`、shell rc 的東西影響不到其他任務；內建 worker 讀不到 Claude
//!   的登入。拿掉所有 capability、`no-new-privileges`、限制行程數，不掛 Docker socket。run 結束就
//!   `rm -f`，所以「重建容器後續接」是每次喚醒的常態。M4 還沒有對外網路的白名單（容器可以連外網）。
//!   指令的逾時在容器裡以 `timeout` 執行（只殺 `docker exec` 的 client 停不了容器裡的行程）。
//! - **本機**：直接在這台電腦上、任務的工作目錄裡執行，沒有沙箱；Claude Code 用你自己的登入。
//!   指令只拿到少數環境變數（不繼承服務的 API key 等），逾時或取消時殺掉整個行程群組。
//!
//! 工作目錄的路徑在容器裡固定是 `/workspace/<任務>`，所以 Claude Code 的 session（依 cwd 分目錄）
//! 換了容器也找得到，不同任務的 session 與自動記憶也不會混在同一個目錄。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures::future::BoxFuture;
use pots_proto::ExecKind;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// 容器裡的工作目錄（下面一層是任務 ID）、home、Claude Code 設定與 run 設定的位置。
pub const CONTAINER_WORKDIR: &str = "/workspace";
pub const CONTAINER_HOME: &str = "/home/pots";
pub const CONTAINER_CLAUDE_CONFIG: &str = "/claude-config";
pub const CONTAINER_RUN_DIR: &str = "/run/pots";
pub const CONTAINER_OUT_DIR: &str = "/run/pots-out";
/// 容器裡連回平台用的主機名稱（Docker Desktop、OrbStack 內建）。服務只綁 loopback，所以 Linux
/// 主機上的容器模式要等多設備版。
pub const CONTAINER_HOST_ALIAS: &str = "host.docker.internal";
/// 啟動容器最多等多久（第一次可能要拉 image）。
const START_TIMEOUT: Duration = Duration::from_secs(600);
/// 一次指令輸出最多保留多少位元組（超過的截掉）。
pub const MAX_OUTPUT: usize = 64 * 1024;

/// 啟動環境所需的設定。
#[derive(Clone, Debug)]
pub struct ExecSpec {
    pub kind: ExecKind,
    /// `docker` 或 `podman`。
    pub runtime: String,
    pub image: String,
    /// Claude Code 後端（容器）：共用的設定目錄與這個任務自己的 home。內建 worker 是 `None`。
    pub claude: Option<ClaudeMounts>,
    /// 容器的標籤：啟動時清掉這個實例之前（其他次啟動）留下的容器。
    pub instance: String,
    pub boot: String,
    /// project 環境的環境變數（容器以 `-e` 帶入；本機加在每個指令上）。
    pub env: Vec<(String, String)>,
    /// project 環境的快取：主機目錄掛到容器裡的路徑（本機不掛）。
    pub mounts: Vec<(PathBuf, String)>,
}

#[derive(Clone, Debug)]
pub struct ClaudeMounts {
    /// 官方登入與 session（`CLAUDE_CONFIG_DIR`），所有任務共用。
    pub config: PathBuf,
    /// 這個任務的 home。
    pub home: PathBuf,
}

/// 一次指令的結果。
#[derive(Clone, Debug, Default)]
pub struct ExecOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub truncated: bool,
}

impl ExecOutput {
    pub fn ok(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }
}

/// 一次 run 的執行環境。
pub struct Env {
    kind: ExecKind,
    runtime: String,
    /// 容器名稱（本機是 `None`）。
    container: Option<String>,
    /// 任務的工作目錄（主機路徑）。
    task_dir: PathBuf,
    /// 指令看到的工作目錄。
    workdir: String,
    /// 這次 run 的設定目錄（主機路徑）。
    run_dir: PathBuf,
    /// 這次 run 的輸出目錄（主機路徑；執行環境可寫）。
    out_dir: PathBuf,
    /// project 環境的環境變數（本機執行時加在每個指令上；容器在啟動時就帶了）。
    extra_env: Vec<(String, String)>,
}

/// 本機執行時留給指令的環境變數。
const LOCAL_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TMPDIR",
    "TZ",
];

/// 一律不轉交的環境變數。
const NEVER_ENV: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

/// 只留 [`LOCAL_ENV`]（與 `extra_prefixes` 開頭的）環境變數。
fn restrict_env(cmd: &mut Command, extra_prefixes: &[&str]) {
    cmd.env_clear();
    for (k, v) in std::env::vars_os() {
        let Some(name) = k.to_str() else { continue };
        // API key 會讓 Claude Code 改用 API 計費（不是你的訂閱登入）：不轉交。
        if NEVER_ENV.contains(&name) {
            continue;
        }
        if LOCAL_ENV.contains(&name) || extra_prefixes.iter().any(|p| name.starts_with(p)) {
            cmd.env(&k, v);
        }
    }
}

/// 子行程結束前被丟掉（例如 run 被取消）時殺掉整個行程群組。
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.0.is_some() {
            kill_group(self.0, libc_sigkill());
        }
    }
}

fn truncate_bytes(b: &[u8]) -> (String, bool) {
    if b.len() <= MAX_OUTPUT {
        (String::from_utf8_lossy(b).into_owned(), false)
    } else {
        // 保留頭尾：錯誤通常在最後。
        let head = &b[..MAX_OUTPUT / 4];
        let tail = &b[b.len() - MAX_OUTPUT * 3 / 4..];
        (
            format!(
                "{}\n…（中間省略 {} 位元組）…\n{}",
                String::from_utf8_lossy(head),
                b.len() - head.len() - tail.len(),
                String::from_utf8_lossy(tail)
            ),
            true,
        )
    }
}

/// 殺掉整個行程群組（本機執行時指令自己開的子行程也一起）。
#[cfg(unix)]
pub fn kill_group(pid: Option<u32>, signal: i32) {
    if let Some(pid) = pid {
        // SAFETY: killpg 只送訊號，pid 來自我們自己開的子行程（它自己是群組 leader）。
        unsafe {
            libc::killpg(pid as libc::pid_t, signal);
        }
    }
}

#[cfg(not(unix))]
pub fn kill_group(_pid: Option<u32>, _signal: i32) {}

pub(crate) async fn output_with_timeout(
    mut cmd: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
) -> std::io::Result<ExecOutput> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn()?;
    let mut guard = GroupGuard(child.id());
    if let Some(bytes) = stdin
        && let Some(mut w) = child.stdin.take()
    {
        // 另一個 task 寫：指令不讀 stdin、或先寫滿 stdout 時不會卡住。寫入失敗不算錯。
        tokio::spawn(async move {
            let _ = w.write_all(&bytes).await;
        });
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(out) => {
            guard.0 = None;
            let out = out?;
            let (stdout, t1) = truncate_bytes(&out.stdout);
            let (stderr, t2) = truncate_bytes(&out.stderr);
            Ok(ExecOutput {
                code: out.status.code(),
                stdout,
                stderr,
                timed_out: false,
                truncated: t1 || t2,
            })
        }
        Err(_) => {
            // guard 被丟掉時殺掉整個行程群組。
            drop(guard);
            Ok(ExecOutput {
                code: None,
                timed_out: true,
                ..Default::default()
            })
        }
    }
}

#[cfg(unix)]
fn libc_sigkill() -> i32 {
    libc::SIGKILL
}

#[cfg(not(unix))]
fn libc_sigkill() -> i32 {
    9
}

fn container_name(run: &str) -> String {
    format!("pots-run-{run}")
}

/// 容器裡任務的工作目錄。
pub fn container_workdir(task: &str) -> String {
    format!("{CONTAINER_WORKDIR}/{task}")
}

impl Env {
    /// 準備環境：容器模式啟動一個容器。`run` 是 run 的 ID（容器名稱用）。
    pub async fn start(
        spec: &ExecSpec,
        task_dir: &Path,
        run_dir: &Path,
        run: &str,
        task: &str,
    ) -> Result<Self, String> {
        let out_dir = run_dir.with_extension("out");
        for d in [run_dir, out_dir.as_path()] {
            std::fs::create_dir_all(d).map_err(|e| format!("無法建立 run 目錄：{e}"))?;
        }
        let mut env = Self {
            kind: spec.kind,
            runtime: spec.runtime.clone(),
            container: None,
            task_dir: task_dir.to_path_buf(),
            workdir: match spec.kind {
                ExecKind::Container => container_workdir(task),
                ExecKind::Local => task_dir.display().to_string(),
            },
            run_dir: run_dir.to_path_buf(),
            out_dir,
            extra_env: spec.env.clone(),
        };
        if spec.kind == ExecKind::Local {
            return Ok(env);
        }
        for (host, _) in &spec.mounts {
            std::fs::create_dir_all(host).map_err(|e| format!("無法建立快取目錄：{e}"))?;
        }
        let name = container_name(run);
        let mut cmd = Command::new(&spec.runtime);
        cmd.args(["run", "-d", "--rm", "--name", &name])
            .arg(format!("--label=pots.instance={}", spec.instance))
            .arg(format!("--label=pots.run={run}"))
            .arg(format!("--label=pots.boot={}", spec.boot))
            .args([
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--pids-limit=4096",
            ])
            .arg(format!("--add-host={CONTAINER_HOST_ALIAS}:host-gateway"))
            .arg("-v")
            .arg(format!("{}:{}", task_dir.display(), env.workdir))
            .arg("-v")
            .arg(format!("{}:{CONTAINER_RUN_DIR}:ro", run_dir.display()))
            .arg("-v")
            .arg(format!("{}:{CONTAINER_OUT_DIR}", env.out_dir.display()));
        if let Some(c) = &spec.claude {
            for d in [&c.config, &c.home] {
                std::fs::create_dir_all(d)
                    .map_err(|e| format!("無法建立 Claude Code 的目錄：{e}"))?;
            }
            cmd.arg("-v")
                .arg(format!("{}:{CONTAINER_CLAUDE_CONFIG}", c.config.display()))
                .arg("-v")
                .arg(format!("{}:{CONTAINER_HOME}", c.home.display()))
                .args([
                    "-e",
                    &format!("CLAUDE_CONFIG_DIR={CONTAINER_CLAUDE_CONFIG}"),
                ]);
        }
        for (host, path) in &spec.mounts {
            cmd.arg("-v").arg(format!("{}:{path}", host.display()));
        }
        for (k, v) in &spec.env {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        cmd.args(["-e", &format!("HOME={CONTAINER_HOME}")])
            .args(["-w", &env.workdir]);
        #[cfg(unix)]
        {
            // 讓工作目錄裡新建的檔案屬於你（image 的 entrypoint 會補上 passwd 項目）。
            // SAFETY: getuid／getgid 沒有前置條件。
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            cmd.arg(format!("--user={uid}:{gid}"));
        }
        cmd.arg("--").arg(&spec.image).args(["sleep", "infinity"]);
        let out = output_with_timeout(cmd, None, START_TIMEOUT)
            .await
            .map_err(|e| format!("無法執行 {}：{e}（有安裝並啟動嗎？）", spec.runtime))?;
        if !out.ok() {
            let why = if out.timed_out {
                "逾時".to_string()
            } else {
                crate::tasks::truncate(out.stderr.trim(), 400)
            };
            return Err(format!("無法啟動 worker 容器（{}）：{why}", spec.image));
        }
        env.container = Some(name);
        Ok(env)
    }

    pub fn kind(&self) -> ExecKind {
        self.kind
    }

    /// 這次 run 的設定目錄（主機路徑，寫檔用）。
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// 指令看到的 run 設定目錄。
    pub fn run_dir_seen(&self) -> String {
        match self.kind {
            ExecKind::Container => CONTAINER_RUN_DIR.into(),
            ExecKind::Local => self.run_dir.display().to_string(),
        }
    }

    /// 這次 run 的輸出目錄（主機路徑，讀執行環境交回的檔案用）。
    pub fn out_dir(&self) -> &Path {
        &self.out_dir
    }

    /// 執行環境看到的輸出目錄。
    pub fn out_dir_seen(&self) -> String {
        match self.kind {
            ExecKind::Container => CONTAINER_OUT_DIR.into(),
            ExecKind::Local => self.out_dir.display().to_string(),
        }
    }

    /// 執行程式的指令（Claude Code 用）：本機是直接執行（只帶少數環境變數，加上 Claude Code 自己的
    /// `ANTHROPIC_*`、`CLAUDE_*`，但不帶 API key），容器是 `exec -i` 進去。
    pub fn command(&self, program: &str, env: &[(&str, String)]) -> Command {
        self.command_with(program, env, &["ANTHROPIC_", "CLAUDE_"])
    }

    fn command_with(&self, program: &str, env: &[(&str, String)], prefixes: &[&str]) -> Command {
        match &self.container {
            Some(name) => {
                let mut cmd = Command::new(&self.runtime);
                cmd.args(["exec", "-i", "-w", &self.workdir]);
                for (k, v) in env {
                    cmd.arg("-e").arg(format!("{k}={v}"));
                }
                cmd.arg(name).arg(program);
                cmd
            }
            None => {
                let mut cmd = Command::new(program);
                cmd.current_dir(&self.task_dir);
                restrict_env(&mut cmd, prefixes);
                for (k, v) in &self.extra_env {
                    cmd.env(k, v);
                }
                for (k, v) in env {
                    cmd.env(k, v);
                }
                cmd
            }
        }
    }

    /// 用 `sh -c` 在工作目錄執行一段指令。
    pub async fn exec(
        &self,
        script: &str,
        stdin: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<ExecOutput, String> {
        let cmd = match self.container {
            // 容器裡的行程要在容器裡殺：只殺 `docker exec` 的 client 停不了它。
            Some(_) => {
                // 先 TERM、5 秒後 KILL；逾時的結束代碼是 124（KILL 時 137）。
                let mut c = self.command_with("timeout", &[], &[]);
                c.args([
                    "-k",
                    "5",
                    &timeout.as_secs().max(1).to_string(),
                    "sh",
                    "-c",
                    script,
                ]);
                c
            }
            None => {
                let mut c = self.command_with("sh", &[], &[]);
                c.args(["-c", script]);
                c
            }
        };
        // 容器裡的 `timeout` 先到；主機這邊多等一點再殺 client。
        let host_timeout = match self.container {
            Some(_) => timeout + Duration::from_secs(10),
            None => timeout,
        };
        let started = std::time::Instant::now();
        let mut out = output_with_timeout(cmd, stdin, host_timeout)
            .await
            .map_err(|e| format!("無法執行指令：{e}"))?;
        if self.container.is_some()
            && (out.code == Some(124) || (out.code == Some(137) && started.elapsed() >= timeout))
        {
            out.timed_out = true;
            out.code = None;
        }
        Ok(out)
    }

    /// 行程不肯結束時的最後手段：直接移除容器（裡面所有行程一起結束）。本機沒有。
    pub fn hard_stop(&self) -> Option<Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>> {
        let name = self.container.clone()?;
        let runtime = self.runtime.clone();
        Some(Box::new(move || {
            Box::pin(async move {
                remove_container(&runtime, &name).await;
            })
        }))
    }

    /// 結束這次 run：移除容器、run 設定目錄與輸出目錄。
    pub async fn stop(self) {
        if let Some(name) = &self.container {
            remove_container(&self.runtime, name).await;
        }
        let _ = std::fs::remove_dir_all(&self.run_dir);
        let _ = std::fs::remove_dir_all(&self.out_dir);
    }
}

async fn remove_container(runtime: &str, name: &str) {
    let mut cmd = Command::new(runtime);
    cmd.args(["rm", "-f", name]);
    match output_with_timeout(cmd, None, Duration::from_secs(60)).await {
        Ok(o) if o.ok() => {}
        Ok(o) => {
            tracing::warn!(container = name, stderr = %crate::tasks::truncate(&o.stderr, 300), "無法移除 worker 容器")
        }
        Err(e) => tracing::warn!(container = name, error = %e, "無法移除 worker 容器"),
    }
}

/// 啟動時清掉這個實例之前（其他次啟動）留下的 worker 容器（當機時沒有移除的）；這次啟動開的不動。
/// 找不到 runtime 時略過。
pub async fn cleanup_stale(runtime: &str, instance: &str, boot: &str) {
    let mut cmd = Command::new(runtime);
    cmd.args(["ps", "-a", "--filter"])
        .arg(format!("label=pots.instance={instance}"))
        .args(["--format", "{{.ID}} {{.Label \"pots.boot\"}}"]);
    let Ok(out) = output_with_timeout(cmd, None, Duration::from_secs(20)).await else {
        return;
    };
    if !out.ok() {
        return;
    }
    for line in out.stdout.lines() {
        let mut parts = line.split_whitespace();
        let (Some(id), b) = (parts.next(), parts.next()) else {
            continue;
        };
        if b != Some(boot) {
            remove_container(runtime, id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_exec_runs_in_task_dir_with_timeout_and_stdin() {
        let tmp = tempfile::tempdir().unwrap();
        let task = tmp.path().join("task");
        std::fs::create_dir_all(&task).unwrap();
        let spec = ExecSpec {
            kind: ExecKind::Local,
            runtime: "docker".into(),
            image: String::new(),
            claude: None,
            instance: "i".into(),
            boot: "b".into(),
            env: vec![("POTS_ENV_X".into(), "from-env".into())],
            mounts: Vec::new(),
        };
        let env = Env::start(&spec, &task, &tmp.path().join("run"), "r1", "t1")
            .await
            .unwrap();
        let out = env
            .exec(
                "pwd; cat > f.txt; echo err >&2; exit 3",
                Some(b"hello".to_vec()),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert!(out.stdout.trim().ends_with("task"), "{}", out.stdout);
        assert_eq!(out.stderr.trim(), "err");
        assert_eq!(
            std::fs::read_to_string(task.join("f.txt")).unwrap(),
            "hello"
        );

        let started = std::time::Instant::now();
        let out = env
            .exec("sleep 30 & sleep 30", None, Duration::from_millis(300))
            .await
            .unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
        // 服務的環境變數（這裡是 cargo 設的）不會交給指令。
        let out = env
            .exec(
                "echo \"[$CARGO_MANIFEST_DIR]\"",
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "[]");
        // project 環境的環境變數會帶給指令。
        let out = env
            .exec("echo \"[$POTS_ENV_X]\"", None, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "[from-env]");
        assert!(env.hard_stop().is_none());
        let run_dir = env.run_dir().to_path_buf();
        assert!(run_dir.exists());
        env.stop().await;
        assert!(!run_dir.exists());
    }

    #[test]
    fn output_is_truncated_keeping_tail() {
        let big = vec![b'x'; MAX_OUTPUT * 2];
        let (s, t) = truncate_bytes(&big);
        assert!(t);
        assert!(s.len() < MAX_OUTPUT + 200);
        assert!(s.contains("中間省略"));
    }
}
