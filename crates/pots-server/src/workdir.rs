//! project 的工作資料夾（`proposal/onboarding.md`）：程式碼在哪、worker 在哪裡改。
//!
//! 一個 project 只有一個工作資料夾，和自動化分頁「監看的 repo」（外部事件來源）分開：
//! - `github.com/owner/repo`：平台維護 bare 鏡像，每個任務一個任務分支，核准後 merge PR。
//! - 本機的 git repo：平台一樣在鏡像與任務分支上做，核准後由平台把分支套用回這個資料夾。
//!
//! 驗證在建立 project 與修改設定時都跑同一份（[`normalize`]）：設定錯的資料夾比沒有設定更糟，
//! 因為 worker 會在某個地方改 code。

use std::path::{Path, PathBuf};
use std::time::Duration;

use pots_proto::WorkDir;
use tokio::process::Command;

/// 本機資料夾最多等多久（`git` 檢查）。
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// 驗證並正規化工作資料夾。`data_dir` 用來擋下「資料目錄就在這個資料夾裡」這種會把鏡像、
/// 工作目錄寫進 repo 的設定。
pub fn normalize(dir: WorkDir, data_dir: &Path) -> Result<WorkDir, String> {
    match dir {
        WorkDir::Github { repo } => {
            let repo = repo.trim();
            if repo.is_empty() {
                return Err("請填 GitHub repo（owner/repo）".into());
            }
            Ok(WorkDir::Github {
                repo: crate::watcher::event::normalize_repo(repo)?,
            })
        }
        WorkDir::Local { path } => {
            let path = path.trim();
            if path.is_empty() {
                return Err("請填本機資料夾的路徑".into());
            }
            let input = PathBuf::from(path);
            if !input.is_absolute() {
                return Err(format!(
                    "本機資料夾要寫絕對路徑（例如 /Users/me/code/app）：{path}"
                ));
            }
            let canonical = input
                .canonicalize()
                .map_err(|e| format!("找不到資料夾 {path}：{e}"))?;
            let meta =
                std::fs::metadata(&canonical).map_err(|e| format!("讀不到資料夾 {path}：{e}"))?;
            if !meta.is_dir() {
                return Err(format!("{path} 不是資料夾"));
            }
            if !is_git_repo(&canonical) {
                return Err(format!(
                    "{path} 不是 git repo：Pots 在任務分支上工作，需要一個 git repo（先 git init 或換一個資料夾）"
                ));
            }
            let data_dir = data_dir
                .canonicalize()
                .unwrap_or_else(|_| data_dir.to_path_buf());
            if canonical.starts_with(&data_dir) {
                return Err(format!(
                    "{path} 在 Pots 的資料目錄裡：請指向你自己的專案資料夾"
                ));
            }
            if data_dir.starts_with(&canonical) {
                return Err(format!(
                    "{path} 包含 Pots 的資料目錄（{}）：請指向專案自己的資料夾",
                    data_dir.display()
                ));
            }
            let path = canonical
                .to_str()
                .ok_or_else(|| format!("資料夾的路徑不是 UTF-8：{}", canonical.display()))?
                .to_string();
            Ok(WorkDir::Local { path })
        }
    }
}

fn is_git_repo(path: &Path) -> bool {
    let dot_git = path.join(".git");
    // 工作區裡的 repo 是目錄；submodule 與 `git worktree` 是檔案（gitfile）。
    dot_git.is_dir() || dot_git.is_file()
}

/// 放進 agent instructions 的一行說明：「工作資料夾是什麼、worker 實際在哪裡工作」。
pub fn describe(dir: Option<&WorkDir>) -> String {
    match dir {
        None => "還沒設定：worker 不會派工，Goal 的任務只記在任務樹上。使用者提到要在哪個 repo 或資料夾工作時，\
                 先問清楚再設定。"
            .into(),
        Some(WorkDir::Github { repo }) => format!(
            "GitHub repo {repo}：平台維護自己的鏡像，每個任務在自己的分支上改、開 PR、等 CI，核准後 merge。\
             這不是你電腦上的資料夾。自動化分頁「監看的 repo」只決定要看哪些事件，和這裡分開。"
        ),
        Some(WorkDir::Local { path }) => format!(
            "本機資料夾 {path}（git repo）：worker 在任務分支上改平台自己的副本，不是你資料夾裡的即時檔案；\
             核准後平台把分支的 commit 線性接到你的分支上（能快轉就快轉，岔開時接到後面；有衝突就不套用）。沒有 PR 與 CI；資料夾有未 commit 的修改時不能套用。\
             自動化分頁「監看的 repo」只決定要看哪些事件，和這裡分開。"
        ),
    }
}

/// 本機資料夾目前在哪個分支、有沒有未 commit 的修改（onboarding 的成功訊息與核准時的檢查用）。
pub async fn status(path: &Path) -> Result<LocalStatus, String> {
    let branch = git(path, &["symbolic-ref", "--short", "-q", "HEAD"])
        .await?
        .trim()
        .to_string();
    if branch.is_empty() {
        return Err(format!(
            "{} 現在不在任何分支上（detached HEAD）：請先切到一個分支",
            path.display()
        ));
    }
    let dirty = !git(path, &["status", "--porcelain"])
        .await?
        .trim()
        .is_empty();
    Ok(LocalStatus { branch, dirty })
}

/// 本機資料夾的現況。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalStatus {
    pub branch: String,
    /// 有未 commit 的修改：套用核准的變更前要先處理，免得蓋掉你的工作。
    pub dirty: bool,
}

/// 在**你的**資料夾執行一個讀取用的 git 指令。和平臺自己的 git 不同：這裡刻意讀你的設定
/// （身分、預設分支），但清掉會改變行為的環境變數與互動提示，逾時就殺掉。
pub async fn git(path: &Path, args: &[&str]) -> Result<String, String> {
    git_env(path, args, &[]).await
}

/// 同 [`git`]，另外帶幾個環境變數（`commit-tree` 保留原作者用）。
async fn git_env(path: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(path).args(args);
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "git 逾時".to_string())?
        .map_err(|e| format!("跑不了 git：{e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = stderr.lines().rev().take(4).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    Err(crate::tasks::truncate(&tail.join(" / "), 400))
}

/// 把 `tip` 上、`HEAD` 沒有的 commit 依序接到 `HEAD` 後面（等同 rebase），回傳新的頂端。
///
/// 只動 `.git` 的物件：每個 commit 用 `merge-tree --write-tree` 算出新的 tree、`commit-tree`
/// 建 commit（保留原作者與訊息），不碰工作區、index 與任何分支。任何一個 commit 有衝突、
/// 或分支裡有 merge commit 就整個失敗，你的資料夾維持原樣。接完之後由呼叫端 fast-forward。
/// 已經在 `HEAD` 上的變更（接上去沒有差異）會略過。
pub async fn replay_onto_head(path: &Path, tip: &str) -> Result<String, String> {
    let head = git(path, &["rev-parse", "--verify", "HEAD"])
        .await?
        .trim()
        .to_string();
    let base = git(path, &["merge-base", &head, tip])
        .await
        .map_err(|e| format!("任務分支和你的分支沒有共同的祖先：{e}"))?
        .trim()
        .to_string();
    let range = format!("{base}..{tip}");
    if !git(path, &["rev-list", "--merges", &range])
        .await?
        .trim()
        .is_empty()
    {
        return Err("任務分支裡有 merge commit，沒辦法接成線性歷史".into());
    }
    let commits = git(path, &["rev-list", "--reverse", "--topo-order", &range]).await?;
    let mut onto = head;
    for commit in commits.lines().map(str::trim).filter(|c| !c.is_empty()) {
        let parent = format!("{commit}^");
        let merge_base = format!("--merge-base={parent}");
        let tree = git(
            path,
            &[
                "merge-tree",
                "--write-tree",
                "--no-messages",
                &merge_base,
                &onto,
                commit,
            ],
        )
        .await
        .map_err(|_| {
            format!(
                "任務分支的 {} 和你的分支有衝突，沒有套用（你的資料夾沒有被改動）",
                &commit[..commit.len().min(7)]
            )
        })?;
        let tree = tree.lines().next().unwrap_or_default().trim().to_string();
        let onto_tree = git(path, &["rev-parse", &format!("{onto}^{{tree}}")])
            .await?
            .trim()
            .to_string();
        if tree == onto_tree {
            continue;
        }
        let meta = git(
            path,
            &[
                "show",
                "-s",
                "--date=raw",
                "--format=%an%x00%ae%x00%ad%x00%B",
                commit,
            ],
        )
        .await?;
        let mut parts = meta.splitn(4, '\0');
        let (name, email, date, message) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default().trim_end(),
        );
        onto = git_env(
            path,
            &["commit-tree", &tree, "-p", &onto, "-m", message],
            &[
                ("GIT_AUTHOR_NAME", name),
                ("GIT_AUTHOR_EMAIL", email),
                ("GIT_AUTHOR_DATE", date),
            ],
        )
        .await?
        .trim()
        .to_string();
    }
    Ok(onto)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
    }

    #[test]
    fn rejects_relative_and_non_repo_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let rel = normalize(
            WorkDir::Local {
                path: "code".into(),
            },
            &data,
        )
        .unwrap_err();
        assert!(rel.contains("絕對路徑"), "{rel}");
        let missing = normalize(
            WorkDir::Local {
                path: tmp.path().join("nope").display().to_string(),
            },
            &data,
        )
        .unwrap_err();
        assert!(missing.contains("找不到"), "{missing}");
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let not_repo = normalize(
            WorkDir::Local {
                path: plain.display().to_string(),
            },
            &data,
        )
        .unwrap_err();
        assert!(not_repo.contains("不是 git repo"), "{not_repo}");
    }

    #[test]
    fn accepts_a_repo_and_canonicalizes() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let repo = tmp.path().join("app");
        temp_repo(&repo);
        let dir = normalize(
            WorkDir::Local {
                path: repo.display().to_string(),
            },
            &data,
        )
        .unwrap();
        assert!(matches!(dir, WorkDir::Local { ref path } if path.ends_with("app")));
    }

    #[test]
    fn rejects_the_data_dir_and_its_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("pots-data");
        std::fs::create_dir_all(&data).unwrap();
        let inside = data.join("repo");
        temp_repo(&inside);
        let err = normalize(
            WorkDir::Local {
                path: inside.display().to_string(),
            },
            &data,
        )
        .unwrap_err();
        assert!(err.contains("Pots 的資料目錄"), "{err}");
        // 把整個家目錄當工作資料夾：資料目錄在裡面。
        temp_repo(tmp.path());
        let err = normalize(
            WorkDir::Local {
                path: tmp.path().display().to_string(),
            },
            &data,
        )
        .unwrap_err();
        assert!(err.contains("包含 Pots 的資料目錄"), "{err}");
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Me", "-c", "user.email=me@example.com"])
            .args(["-c", "commit.gpgsign=false"])
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
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit_file(dir: &Path, file: &str, body: &str, msg: &str) {
        std::fs::write(dir.join(file), body).unwrap();
        sh(dir, &["add", file]);
        sh(dir, &["commit", "-q", "-m", msg]);
    }

    /// main 和任務分支從同一個 commit 岔開。
    fn diverged(dir: &Path) -> String {
        sh(dir, &["init", "-q", "-b", "main"]);
        sh(dir, &["config", "commit.gpgsign", "false"]);
        // commit-tree 的 committer 讀你的設定；測試環境不一定有全域身分。
        sh(dir, &["config", "user.name", "Committer"]);
        sh(dir, &["config", "user.email", "c@example.com"]);
        commit_file(dir, "a.txt", "a\n", "base");
        sh(dir, &["checkout", "-q", "-b", "task"]);
        commit_file(dir, "b.txt", "b\n", "task one");
        commit_file(dir, "c.txt", "c\n", "task two");
        let tip = sh(dir, &["rev-parse", "HEAD"]);
        sh(dir, &["checkout", "-q", "main"]);
        commit_file(dir, "d.txt", "d\n", "mine");
        tip
    }

    #[tokio::test]
    async fn replays_task_commits_after_head_without_touching_the_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let tip = diverged(dir);
        let head = sh(dir, &["rev-parse", "HEAD"]);
        let new = replay_onto_head(dir, &tip).await.unwrap();
        // 只建了物件：main 與工作區都還沒動。
        assert_eq!(sh(dir, &["rev-parse", "HEAD"]), head);
        assert!(!dir.join("b.txt").exists());
        assert_eq!(
            sh(dir, &["log", "--format=%s", &format!("{head}..{new}")]),
            "task two\ntask one"
        );
        assert_eq!(sh(dir, &["rev-parse", &format!("{new}~2")]), head);
        assert_eq!(
            sh(dir, &["show", "-s", "--format=%an <%ae>", &new]),
            "Me <me@example.com>"
        );
        assert_eq!(
            sh(dir, &["show", "-s", "--format=%ad", "--date=raw", &new]),
            sh(dir, &["show", "-s", "--format=%ad", "--date=raw", &tip])
        );
        git(dir, &["merge", "--ff-only", "--quiet", &new])
            .await
            .unwrap();
        for f in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            assert!(dir.join(f).exists(), "{f}");
        }
    }

    #[tokio::test]
    async fn refuses_conflicting_task_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let tip = diverged(dir);
        commit_file(dir, "c.txt", "mine\n", "clash");
        let head = sh(dir, &["rev-parse", "HEAD"]);
        let err = replay_onto_head(dir, &tip).await.unwrap_err();
        assert!(err.contains("衝突"), "{err}");
        assert_eq!(sh(dir, &["rev-parse", "HEAD"]), head);
        assert!(sh(dir, &["status", "--porcelain"]).is_empty());
    }

    #[test]
    fn normalizes_github_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = normalize(
            WorkDir::Github {
                repo: "https://github.com/Me/App.git".into(),
            },
            tmp.path(),
        )
        .unwrap();
        assert_eq!(
            dir,
            WorkDir::Github {
                repo: "github.com/me/app".into()
            }
        );
        assert!(normalize(WorkDir::Github { repo: "  ".into() }, tmp.path()).is_err());
    }
}
