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
             核准後平台把分支 fast-forward 套用進去。沒有 PR 與 CI；資料夾有未 commit 的修改時不能套用。\
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
