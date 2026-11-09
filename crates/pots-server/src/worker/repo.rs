//! 任務的程式碼：repo 鏡像、每個任務一個工作目錄、push 與開 PR。
//!
//! - **來源**：[`RepoSource`] 是之後接 GitLab 與沒有 remote 的本機 repo 的介面；M4 只有 GitHub。
//! - **鏡像**：平台用 GitHub token 在 `data_dir/repos/<host>/<owner>/<name>.git` 維護一份 bare
//!   鏡像（`refs/remotes/origin/*`）。token 只在平台這邊的 git 指令以環境變數帶入
//!   （`http.extraHeader`），不寫進設定檔、不出現在參數列，也不交給 worker 或容器。
//! - **工作目錄**：每個任務一個獨立的 git repo（`data_dir/worktrees/<task>`），從鏡像取出基準分支後
//!   開任務分支（先在暫存目錄做好再改名，中斷時不會留下做到一半的目錄）。用獨立 repo 而不是
//!   `git worktree`：worktree 的 `.git` 指向鏡像的絕對路徑，掛進容器就失效，而且容器可以改到其他
//!   任務的 ref。工作目錄在 run 之間保留（等待、重建容器後續接）。
//! - **不信任工作目錄**：worker（與容器）可以改工作目錄裡的 `.git`（設定、hooks、alternates、
//!   指向別處的 gitfile 或符號連結的 ref），所以平台在主機上**完全不對工作目錄執行 git**——連
//!   `git fetch file://` 也不行：那會讓主機上的 git 依 worker 的 alternates／符號連結讀到其他 repo
//!   的物件。要拿 worker 的 commit 時，由執行環境（容器）裡的 git 打一個 bundle 寫到這次 run 的
//!   輸出目錄，平台以不跟隨符號連結的方式讀出、複製到自己的目錄，再在鏡像裡以 `transfer.fsckObjects`
//!   檢查後 fetch（[`Repos::collect`]）；之後的比較、push 都在鏡像裡做。
//! - 平台的 git 清空環境變數（只留 `PATH`）、隔離使用者的全域與系統設定，也不會停下來問密碼。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use pots_proto::TaskId;
use tokio::process::Command;

use crate::watcher::event::repo_path;

/// 平台在執行環境裡跑的 git：不讀 home 的全域設定、不跑 hooks 與 fsmonitor（worker 可以在 home 或
/// `.git` 放這些，影響平台替它做的 commit 與 bundle）。
pub const SAFE_GIT: &str = "GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 git -c core.hooksPath=/dev/null -c core.fsmonitor=false";

/// 一次 git 指令（fetch、push）最多等多久。
const GIT_TIMEOUT: Duration = Duration::from_secs(300);
/// 平台在任務 repo 裡 commit 時用的身分。
pub const COMMIT_NAME: &str = "Pots worker";
pub const COMMIT_EMAIL: &str = "pots-worker@users.noreply.github.com";

/// 程式碼從哪裡來、要推到哪裡去。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoSource {
    /// `github.com/owner/repo`。
    Github { repo: String },
    /// 本機的 git repo（絕對路徑）；沒有 remote、沒有 PR 與 CI，核准後由平台套用到資料夾。
    Local { path: String },
}

impl RepoSource {
    /// 依 project 設定的工作資料夾（`github.com/owner/repo` 或 `local:/abs/path`）判斷來源；
    /// 之後的 GitLab 在這裡分流。
    pub fn parse(repo: &str) -> Result<Self, String> {
        if let Some(path) = repo.trim().strip_prefix("local:") {
            let path = path.trim();
            if !std::path::Path::new(path).is_absolute() {
                return Err(format!("本機工作資料夾要是絕對路徑：{path}"));
            }
            return Ok(Self::Local {
                path: path.to_string(),
            });
        }
        let repo = crate::watcher::event::normalize_repo(repo)?;
        Ok(Self::Github { repo })
    }

    /// 任務與事件記的 key：`github.com/owner/repo` 或 `local:/abs/path`。
    pub fn key(&self) -> String {
        match self {
            Self::Github { repo } => repo.clone(),
            Self::Local { path } => format!("local:{path}"),
        }
    }

    /// 本機資料夾：沒有 remote，不能 push、沒有 PR 與 CI。
    pub fn local_path(&self) -> Option<&str> {
        match self {
            Self::Local { path } => Some(path),
            Self::Github { .. } => None,
        }
    }
}

/// 一個任務的工作位置。
#[derive(Clone, Debug)]
pub struct Checkout {
    pub dir: PathBuf,
    pub branch: String,
    /// 基準分支（repo 的預設分支）。
    pub base: String,
}

/// 推上去的結果。
#[derive(Clone, Debug)]
pub struct Pushed {
    pub sha: String,
    pub files: u32,
    pub insertions: u32,
    pub deletions: u32,
    /// 改到的檔案。
    pub paths: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("git：{0}")]
    Git(String),
    #[error("檔案系統：{0}")]
    Io(#[from] std::io::Error),
}

/// git 與 GitHub 的存取。
pub struct Repos {
    data_dir: PathBuf,
    /// 測試用：git 的 base URL（例如 `file:///tmp/remotes`）；`None` 時依 GitHub API 推算。
    git_base: Option<String>,
    /// 同一個鏡像同時只有一個 git 指令在改它。
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// 平台這邊的 git：隔離使用者設定、不問密碼。`auth` 是要帶的 `Authorization` 標頭值。
pub(super) fn git(dir: Option<&Path>, auth: Option<&str>) -> Command {
    let mut cmd = Command::new("git");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    // 不繼承服務的環境（例如 GIT_TRACE_CURL 會把標頭印到 stderr、進到給 agent 的錯誤訊息）。
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false")
        .env("GIT_TRACE_REDACT", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match auth {
        Some(value) => {
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", format!("Authorization: {value}"));
        }
        None => {
            cmd.env("GIT_CONFIG_COUNT", "0");
        }
    }
    cmd
}

/// 執行 git，回傳 stdout。失敗時錯誤訊息帶 stderr 的最後幾行（不含標頭：token 只在環境變數裡）。
async fn run(mut cmd: Command) -> Result<String, RepoError> {
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| RepoError::Git("逾時".into()))??;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = stderr.lines().rev().take(4).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    Err(RepoError::Git(crate::tasks::truncate(
        &tail.join(" / "),
        500,
    )))
}

/// 任務分支名稱：`pots/<標題的英數字>-<任務 ID 後 8 碼>`。
pub fn branch_name(task: TaskId, title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 30 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let id = task.to_string();
    let short = &id[id.len() - 8..];
    if slug.is_empty() {
        format!("pots/task-{short}")
    } else {
        format!("pots/{slug}-{short}")
    }
}

fn task_ref(task: TaskId) -> String {
    format!("refs/pots/tasks/{task}")
}

impl Repos {
    pub fn new(data_dir: &Path, git_base: Option<String>) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            git_base,
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    fn mirror_dir(&self, source: &RepoSource) -> PathBuf {
        match source {
            RepoSource::Github { repo } => self.data_dir.join("repos").join(format!("{repo}.git")),
            // 本機路徑不能直接當目錄名（有 `/` 與長度限制）：用在資料目錄裡的穩定雜湊。
            RepoSource::Local { path } => self
                .data_dir
                .join("repos")
                .join("local")
                .join(format!("{}.git", path_key(path))),
        }
    }

    pub fn task_dir(&self, task: TaskId) -> PathBuf {
        self.data_dir.join("worktrees").join(task.to_string())
    }

    /// git 的 remote。GitHub 是 URL（`api` 是 GitHub API base：`https://api.github.com` →
    /// `https://github.com`，GitHub Enterprise 的 `https://host/api/v3` → `https://host`）；
    /// 本機資料夾就是那個絕對路徑（git 的 local transport）。
    pub fn remote_url(&self, source: &RepoSource, api: &str) -> String {
        match source {
            RepoSource::Local { path } => path.clone(),
            RepoSource::Github { repo } => {
                let path = repo_path(repo);
                let base = match &self.git_base {
                    Some(b) => b.trim_end_matches('/').to_string(),
                    None => {
                        let api = api.trim_end_matches('/');
                        if api == crate::config::GITHUB_API {
                            "https://github.com".to_string()
                        } else {
                            api.trim_end_matches("/api/v3").to_string()
                        }
                    }
                };
                format!("{base}/{path}.git")
            }
        }
    }

    /// `http.extraHeader` 的值（GitHub 的 git over HTTPS 接受 Basic `x-access-token:<token>`）。
    fn auth_header(token: &str) -> String {
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        format!("Basic {basic}")
    }

    /// 確定鏡像存在並更新。回傳 (鏡像路徑, 預設分支)。
    pub async fn sync(
        &self,
        source: &RepoSource,
        url: &str,
        token: Option<&str>,
    ) -> Result<(PathBuf, String), RepoError> {
        let lock = self.lock(&source.key());
        let _guard = lock.lock().await;
        let mirror = self.mirror_dir(source);
        let auth = token.map(Self::auth_header);
        if !mirror.join("HEAD").exists() {
            std::fs::create_dir_all(&mirror)?;
            run({
                let mut c = git(Some(&mirror), None);
                c.args(["init", "--quiet", "--bare"]);
                c
            })
            .await?;
            for args in [
                vec!["remote", "add", "origin", url],
                vec![
                    "config",
                    "remote.origin.fetch",
                    "+refs/heads/*:refs/remotes/origin/*",
                ],
                vec!["config", "user.name", COMMIT_NAME],
                vec!["config", "user.email", COMMIT_EMAIL],
            ] {
                let mut c = git(Some(&mirror), None);
                c.args(args);
                run(c).await?;
            }
        } else {
            // token 或 base URL 可能換過。
            let mut c = git(Some(&mirror), None);
            c.args(["remote", "set-url", "origin", url]);
            run(c).await?;
        }
        let mut c = git(Some(&mirror), auth.as_deref());
        c.args(["fetch", "--quiet", "--prune", "--no-tags", "origin"]);
        run(c).await?;
        let mut c = git(Some(&mirror), auth.as_deref());
        c.args(["ls-remote", "--symref", "origin", "HEAD"]);
        let base = match run(c).await {
            Ok(out) => out
                .lines()
                .find_map(|l| l.strip_prefix("ref: refs/heads/"))
                .and_then(|l| l.split_whitespace().next().map(str::to_string)),
            // 本機資料夾的 HEAD 可能是還沒有 commit 的分支：`ls-remote` 不會回它。
            Err(_) if source.local_path().is_some() => None,
            Err(e) => return Err(e),
        };
        let base = match base {
            Some(b) => b,
            None => {
                let path = source.local_path().ok_or_else(|| {
                    RepoError::Git("找不到 repo 的預設分支（是空的 repo 嗎？）".into())
                })?;
                let branch = crate::workdir::git(
                    Path::new(path),
                    &["symbolic-ref", "--short", "-q", "HEAD"],
                )
                .await
                .map_err(RepoError::Git)?;
                let branch = branch.trim().to_string();
                if branch.is_empty() {
                    return Err(RepoError::Git(format!(
                        "{path} 不在任何分支上（detached HEAD）：請先切到一個分支"
                    )));
                }
                branch
            }
        };
        let mut c = git(Some(&mirror), None);
        c.args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{base}"),
        ]);
        if run(c).await.is_err() {
            return Err(RepoError::Git(format!(
                "找不到分支 {base} 的 commit（是空的 repo 嗎？先 commit 一次再試）"
            )));
        }
        Ok((mirror, base))
    }

    /// 任務的工作目錄：已經有就沿用（之前的 run 改到一半的東西都在），沒有就從鏡像的基準分支
    /// 開一個新的任務分支。
    pub async fn checkout(
        &self,
        source: &RepoSource,
        mirror: &Path,
        base: &str,
        task: TaskId,
        branch: &str,
    ) -> Result<Checkout, RepoError> {
        let dir = self.task_dir(task);
        if dir.join(".git").exists() {
            return Ok(Checkout {
                dir,
                branch: branch.to_string(),
                base: base.to_string(),
            });
        }
        let lock = self.lock(&source.key());
        let _guard = lock.lock().await;
        // 在暫存目錄做好再改名：中斷（取消、關閉）時不會留下做到一半、之後又被沿用的目錄。
        let tmp = dir.with_extension("partial");
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)?;
        }
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(&tmp)?;
        let mirror_url = format!("file://{}", mirror.display());
        let base_ref = format!("refs/remotes/origin/{base}");
        let steps: Vec<Vec<String>> = vec![
            vec!["init".into(), "--quiet".into()],
            vec![
                "fetch".into(),
                "--quiet".into(),
                "--no-tags".into(),
                mirror_url,
                format!("+{base_ref}:{base_ref}"),
            ],
            vec![
                "checkout".into(),
                "--quiet".into(),
                "-b".into(),
                branch.into(),
                base_ref.clone(),
            ],
            vec!["config".into(), "user.name".into(), COMMIT_NAME.into()],
            vec!["config".into(), "user.email".into(), COMMIT_EMAIL.into()],
        ];
        for args in steps {
            // 這時的目錄是平台剛建立的，還沒交給 worker。
            let mut c = git(Some(&tmp), None);
            c.args(&args);
            if let Err(e) = run(c).await {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e);
            }
        }
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::rename(&tmp, &dir)?;
        Ok(Checkout {
            dir,
            branch: branch.to_string(),
            base: base.to_string(),
        })
    }

    /// 在執行環境裡打 bundle 用的指令（寫到 `out`，執行環境看到的路徑）。只包含基準分支之後的 commit。
    pub fn bundle_script(checkout: &Checkout, out: &str) -> String {
        format!(
            "{SAFE_GIT} bundle create --quiet {} refs/heads/{} ^refs/remotes/origin/{}",
            super::code::shell_quote(out),
            super::code::shell_quote(&checkout.branch),
            super::code::shell_quote(&checkout.base),
        )
    }

    /// 把 worker 在執行環境裡打好的 bundle（[`Repos::bundle_script`]，主機路徑 `bundle`）取回鏡像，
    /// 回傳 commit 與相對基準分支的改動量。bundle 以不跟隨符號連結的方式讀出、複製到平台自己的
    /// 目錄，fetch 時檢查物件。
    pub async fn collect(
        &self,
        source: &RepoSource,
        mirror: &Path,
        bundle: &Path,
        checkout: &Checkout,
        task: TaskId,
    ) -> Result<Pushed, RepoError> {
        let copy = self.data_dir.join("tmp").join(format!("{task}.bundle"));
        {
            let (from, to) = (bundle.to_path_buf(), copy.clone());
            tokio::task::spawn_blocking(move || copy_regular_file(&from, &to, MAX_BUNDLE_BYTES))
                .await
                .map_err(|e| RepoError::Git(format!("複製 bundle 失敗：{e}")))??;
        }
        let lock = self.lock(&source.key());
        let _guard = lock.lock().await;
        let reference = task_ref(task);
        let mut c = git(Some(mirror), None);
        c.args([
            "-c",
            "transfer.fsckObjects=true",
            "fetch",
            "--quiet",
            "--no-tags",
        ])
        .arg(&copy)
        .arg(format!("+refs/heads/{}:{reference}", checkout.branch));
        let fetched = run(c).await;
        let _ = std::fs::remove_file(&copy);
        fetched?;
        let mut c = git(Some(mirror), None);
        c.args(["rev-parse", &reference]);
        let sha = run(c).await?.trim().to_string();
        let base = format!("refs/remotes/origin/{}", checkout.base);
        let mut c = git(Some(mirror), None);
        c.args(["diff", "--shortstat", &format!("{base}...{reference}")]);
        let stat = run(c).await?;
        let (files, insertions, deletions) = parse_shortstat(&stat);
        let mut c = git(Some(mirror), None);
        // `-z`：檔名不加引號、不跳脫；`--no-renames`：搬走的檔案舊新路徑都列出來。
        c.args([
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            &format!("{base}...{reference}"),
        ]);
        let paths = run(c)
            .await?
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        Ok(Pushed {
            sha,
            files,
            insertions,
            deletions,
            paths,
        })
    }

    /// 鏡像的位置（核准後把本機資料夾的分支套用過來時用）。
    pub fn mirror(&self, source: &RepoSource) -> PathBuf {
        self.mirror_dir(source)
    }

    /// 任務分支在鏡像裡的 commit；還沒有任務分支時是錯誤。
    pub async fn task_sha(&self, mirror: &Path, task: TaskId) -> Result<String, RepoError> {
        let mut c = git(Some(mirror), None);
        c.args(["rev-parse", "--verify", "--quiet", &task_ref(task)]);
        Ok(run(c).await?.trim().to_string())
    }

    /// 基準分支之後有沒有 commit。
    pub async fn ahead(
        &self,
        mirror: &Path,
        checkout: &Checkout,
        task: TaskId,
    ) -> Result<u32, RepoError> {
        let mut c = git(Some(mirror), None);
        c.args([
            "rev-list",
            "--count",
            &format!("refs/remotes/origin/{}..{}", checkout.base, task_ref(task)),
        ]);
        Ok(run(c).await?.trim().parse().unwrap_or(0))
    }

    /// 把任務分支推到 remote（強制更新：只會是平台開的 `pots/` 分支）。本機資料夾沒有 remote：
    /// 分支留在鏡像裡，核准後由平台套用（[`crate::approvals`]）。
    pub async fn push(
        &self,
        source: &RepoSource,
        mirror: &Path,
        url: &str,
        token: Option<&str>,
        checkout: &Checkout,
        task: TaskId,
    ) -> Result<(), RepoError> {
        if source.local_path().is_some() {
            return Err(RepoError::Git("本機資料夾沒有 remote，不用 push".into()));
        }
        let lock = self.lock(&source.key());
        let _guard = lock.lock().await;
        let auth = token.map(Self::auth_header);
        let mut c = git(Some(mirror), auth.as_deref());
        c.args(["push", "--quiet", "--force", url]).arg(format!(
            "{}:refs/heads/{}",
            task_ref(task),
            checkout.branch
        ));
        run(c).await.map(|_| ())
    }
}

/// 本機資料夾在資料目錄裡的穩定名字（同一個路徑永遠是同一個鏡像）。
fn path_key(path: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(path.as_bytes());
    hex::encode(&h.finalize()[..16])
}

/// bundle 最大多大。
const MAX_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

/// 從不信任的目錄複製一個一般檔案：最後一段不跟隨符號連結、必須是一般檔案、有大小上限。
fn copy_regular_file(from: &Path, to: &Path, max: u64) -> Result<(), RepoError> {
    use std::io::Read;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = opts
        .open(from)
        .map_err(|e| RepoError::Git(format!("讀不到 worker 的 bundle：{e}")))?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(RepoError::Git("worker 的 bundle 不是一般檔案".into()));
    }
    if meta.len() > max {
        return Err(RepoError::Git("worker 的 bundle 太大".into()));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = std::fs::File::create(to)?;
    let copied = std::io::copy(&mut file.take(max + 1), &mut out)?;
    if copied > max {
        let _ = std::fs::remove_file(to);
        return Err(RepoError::Git("worker 的 bundle 太大".into()));
    }
    Ok(())
}

/// ` 3 files changed, 10 insertions(+), 2 deletions(-)` → (3, 10, 2)。
fn parse_shortstat(s: &str) -> (u32, u32, u32) {
    let mut out = (0, 0, 0);
    for part in s.split(',') {
        let part = part.trim();
        let n: u32 = part
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        if part.contains("file") {
            out.0 = n;
        } else if part.contains("insertion") {
            out.1 = n;
        } else if part.contains("deletion") {
            out.2 = n;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names() {
        let t: TaskId = "01900000-0000-7000-8000-00000000abcd".parse().unwrap();
        assert_eq!(
            branch_name(t, "Fix login bug!"),
            "pots/fix-login-bug-0000abcd"
        );
        assert_eq!(branch_name(t, "修好登入"), "pots/task-0000abcd");
    }

    #[test]
    fn shortstat() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 10 insertions(+), 2 deletions(-)\n"),
            (3, 10, 2)
        );
        assert_eq!(parse_shortstat(" 1 file changed, 1 deletion(-)"), (1, 0, 1));
        assert_eq!(parse_shortstat(""), (0, 0, 0));
    }

    #[test]
    fn remote_urls() {
        let r = Repos::new(Path::new("/tmp/x"), None);
        let s = RepoSource::parse("github.com/me/app").unwrap();
        assert_eq!(
            r.remote_url(&s, crate::config::GITHUB_API),
            "https://github.com/me/app.git"
        );
        assert_eq!(
            r.remote_url(&s, "https://ghe.example.com/api/v3"),
            "https://ghe.example.com/me/app.git"
        );
        let r = Repos::new(Path::new("/tmp/x"), Some("file:///tmp/remotes/".into()));
        assert_eq!(r.remote_url(&s, ""), "file:///tmp/remotes/me/app.git");
    }

    /// 鏡像、任務目錄、取回與 push，對一個本機的 bare repo 當 remote。
    #[tokio::test]
    async fn mirror_checkout_collect_push() {
        let tmp = tempfile::tempdir().unwrap();
        let remotes = tmp.path().join("remotes");
        let remote = remotes.join("me/app.git");
        std::fs::create_dir_all(&remote).unwrap();
        let sh = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        sh(&remote, &["init", "--quiet", "--bare", "-b", "main"]);
        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        sh(&seed, &["init", "--quiet", "-b", "main"]);
        std::fs::write(seed.join("README.md"), "hi\n").unwrap();
        sh(&seed, &["add", "."]);
        sh(
            &seed,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--quiet",
                "-m",
                "init",
            ],
        );
        sh(
            &seed,
            &["push", "--quiet", remote.to_str().unwrap(), "main"],
        );

        let data = tmp.path().join("data");
        let repos = Repos::new(&data, Some(format!("file://{}", remotes.display())));
        let source = RepoSource::parse("me/app").unwrap();
        let url = repos.remote_url(&source, "");
        let (mirror, base) = repos.sync(&source, &url, None).await.unwrap();
        assert_eq!(base, "main");
        let task = TaskId::new();
        let branch = branch_name(task, "Add feature");
        let co = repos
            .checkout(&source, &mirror, &base, task, &branch)
            .await
            .unwrap();
        assert!(co.dir.join("README.md").exists());
        assert!(!co.dir.with_extension("partial").exists());

        std::fs::write(co.dir.join("new.txt"), "a\nb\n").unwrap();
        sh(&co.dir, &["add", "."]);
        sh(&co.dir, &["commit", "--quiet", "-m", "add new"]);
        // worker 在執行環境裡打 bundle（這裡直接跑同一段指令）。
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let bundle = out.join("task.bundle");
        let script = Repos::bundle_script(&co, bundle.to_str().unwrap());
        let ok = std::process::Command::new("sh")
            .args(["-c", &script])
            .current_dir(&co.dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(ok.success());
        let p = repos
            .collect(&source, &mirror, &bundle, &co, task)
            .await
            .unwrap();
        assert_eq!((p.files, p.insertions, p.deletions), (1, 2, 0));
        assert_eq!(p.paths, ["new.txt"]);

        // 符號連結的 bundle 不讀（例如指向主機上的其他檔案）。
        let link = out.join("link.bundle");
        std::os::unix::fs::symlink(&bundle, &link).unwrap();
        assert!(
            repos
                .collect(&source, &mirror, &link, &co, task)
                .await
                .is_err()
        );
        assert_eq!(repos.ahead(&mirror, &co, task).await.unwrap(), 1);
        repos
            .push(&source, &mirror, &url, None, &co, task)
            .await
            .unwrap();
        let heads = sh(&remote, &["branch", "--list"]);
        assert!(heads.contains(&branch), "{heads}");
        assert_eq!(sh(&remote, &["rev-parse", &branch]).trim(), p.sha);

        // 同一個任務再取一次沿用目錄。
        let again = repos
            .checkout(&source, &mirror, &base, task, &branch)
            .await
            .unwrap();
        assert!(again.dir.join("new.txt").exists());
    }
}
