//! 程式碼的唯讀存取（`proposal/agent-collaboration.md#唯讀基礎工具` 的程式碼類）。
//!
//! - worker（與它開的 subagent）讀自己的工作目錄：指令一律在執行環境（容器）裡跑，
//!   平台不在主機上讀不信任的工作目錄（符號連結、`.git/config` 都可能被改過）。
//! - 其他 profile 的 subagent 讀 repo 預設分支的鏡像：在平台自己的 bare 鏡像上用
//!   `git show`、`git grep`，讀的是 commit 裡的內容。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::exec::Env;

/// 一次讀取最多回傳幾行、幾個字。
pub const MAX_LINES: usize = 2000;
pub const MAX_CHARS: usize = 60_000;
/// 清單與搜尋最多幾行。
const MAX_LIST: usize = 500;
const READ_TIMEOUT: Duration = Duration::from_secs(60);

#[async_trait::async_trait]
pub trait CodeAccess: Send + Sync {
    /// 給 agent 看的位置說明，例如「任務的工作目錄」或「github.com/o/r 的 main」。
    fn describe(&self) -> String;
    async fn list_files(&self, path: &str) -> Result<String, String>;
    async fn read_file(&self, path: &str) -> Result<String, String>;
    async fn grep(&self, pattern: &str, path: &str) -> Result<String, String>;
    async fn git_log(&self, path: &str, n: usize) -> Result<String, String>;
    /// 工作目錄才有：未 commit 的修改與相對基準分支的 commit。
    async fn git_diff(&self) -> Option<Result<String, String>>;
}

/// 單引號包起來給 `sh` 用。
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 相對路徑，不能跳出工作目錄（不能是絕對路徑、不能有 `..`）。空字串是根目錄。
pub fn check_path(path: &str) -> Result<String, String> {
    let p = path.trim().trim_start_matches("./");
    if p.starts_with('/') || p.starts_with('~') {
        return Err(format!("路徑要相對於 repo 根目錄：{path}"));
    }
    if p.split('/').any(|c| c == "..") {
        return Err(format!("路徑不能有 ..：{path}"));
    }
    if p.contains('\0') || p.contains('\n') {
        return Err("路徑不合法".into());
    }
    Ok(p.trim_end_matches('/').to_string())
}

/// 只留前 `max` 行，超過時附註。
fn cap_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        text.to_string()
    } else {
        format!(
            "{}\n…（共 {} 行，只列前 {max} 行；請縮小範圍）",
            lines[..max].join("\n"),
            lines.len()
        )
    }
}

/// 檔案內容：太長時截斷並說明。
pub fn cap_file(path: &str, text: &str) -> String {
    let total = text.lines().count();
    let mut out = String::new();
    let mut chars = 0;
    for (i, line) in text.lines().enumerate() {
        if i >= MAX_LINES || chars + line.len() > MAX_CHARS {
            out.push_str(&format!(
                "…（{path} 共 {total} 行，只顯示前 {i} 行；需要後面的部分請用 grep 找行號）\n"
            ));
            return out;
        }
        out.push_str(line);
        out.push('\n');
        chars += line.len() + 1;
    }
    out
}

/// 任務的工作目錄（在執行環境裡讀）。
pub struct Worktree {
    pub env: Arc<Env>,
    /// 基準分支（`git_diff` 比較用）。
    pub base: String,
}

impl Worktree {
    async fn sh(&self, script: &str) -> Result<String, String> {
        let out = self.env.exec(script, None, READ_TIMEOUT).await?;
        if out.timed_out {
            return Err("指令逾時".into());
        }
        if out.code != Some(0) && out.stdout.trim().is_empty() {
            return Err(crate::tasks::truncate(out.stderr.trim(), 1000));
        }
        Ok(out.stdout)
    }
}

#[async_trait::async_trait]
impl CodeAccess for Worktree {
    fn describe(&self) -> String {
        "任務的工作目錄（任務分支）".into()
    }

    async fn list_files(&self, path: &str) -> Result<String, String> {
        let p = check_path(path)?;
        let target = if p.is_empty() {
            ".".into()
        } else {
            shell_quote(&p)
        };
        let out = self
            .sh(&format!(
                "git ls-files --cached --others --exclude-standard -- {target} | head -n {}",
                MAX_LIST + 1
            ))
            .await?;
        Ok(cap_lines(&out, MAX_LIST))
    }

    async fn read_file(&self, path: &str) -> Result<String, String> {
        let p = check_path(path)?;
        if p.is_empty() {
            return Err("要給檔案路徑".into());
        }
        // 只讀前面一段：執行環境的輸出有上限，超過時會被截成頭尾兩段。
        let out = self
            .env
            .exec(
                &format!("head -c {MAX_CHARS} -- {}", shell_quote(&p)),
                None,
                READ_TIMEOUT,
            )
            .await?;
        if out.code != Some(0) {
            return Err(format!("讀不到 {p}：{}", out.stderr.trim()));
        }
        let mut text = cap_file(&p, &out.stdout);
        if out.stdout.len() >= MAX_CHARS {
            text.push_str(&format!(
                "…（{p} 超過 {MAX_CHARS} 位元組，只顯示前面；需要後面的部分請用 grep 找行號）\n"
            ));
        }
        Ok(text)
    }

    async fn grep(&self, pattern: &str, path: &str) -> Result<String, String> {
        let p = check_path(path)?;
        let target = if p.is_empty() {
            ".".into()
        } else {
            shell_quote(&p)
        };
        let out = self
            .env
            .exec(
                &format!(
                    "r=$(git grep --untracked -n -I -E -e {} -- {target}); c=$?; \
                     [ -n \"$r\" ] && printf '%s\\n' \"$r\" | head -n {}; exit $c",
                    shell_quote(pattern),
                    MAX_LIST + 1
                ),
                None,
                READ_TIMEOUT,
            )
            .await?;
        match out.code {
            Some(0) => Ok(cap_lines(&out.stdout, MAX_LIST)),
            Some(1) => Ok("（沒有符合的結果）".into()),
            _ => Err(crate::tasks::truncate(out.stderr.trim(), 1000)),
        }
    }

    async fn git_log(&self, path: &str, n: usize) -> Result<String, String> {
        let p = check_path(path)?;
        let target = if p.is_empty() {
            String::new()
        } else {
            format!("-- {}", shell_quote(&p))
        };
        self.sh(&format!(
            "git log --no-color --oneline -n {} {target}",
            n.clamp(1, 100)
        ))
        .await
    }

    async fn git_diff(&self) -> Option<Result<String, String>> {
        let base = shell_quote(&format!("refs/remotes/origin/{}", self.base));
        Some(
            self.sh(&format!(
                "echo '## git status'; git status --short; \
                 echo; echo '## 相對基準分支的 commit'; git log --no-color --oneline {base}..HEAD; \
                 echo; echo '## diff（含未 commit 的修改）'; git diff --no-color {base}"
            ))
            .await
            .map(|s| cap_file("diff", &s)),
        )
    }
}

/// repo 預設分支的唯讀鏡像（平台的 bare 鏡像）。
pub struct Mirror {
    pub repo: String,
    pub dir: PathBuf,
    pub branch: String,
}

impl Mirror {
    fn rev(&self) -> String {
        format!("refs/remotes/origin/{}", self.branch)
    }

    async fn git(&self, args: &[&str]) -> Result<(Option<i32>, String, String), String> {
        let mut cmd = super::repo::git(Some(&self.dir), None);
        cmd.args(args);
        let out = tokio::time::timeout(READ_TIMEOUT, cmd.output())
            .await
            .map_err(|_| "指令逾時".to_string())?
            .map_err(|e| format!("無法執行 git：{e}"))?;
        Ok((
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}

#[async_trait::async_trait]
impl CodeAccess for Mirror {
    fn describe(&self) -> String {
        format!("{} 的 {} 分支（唯讀）", self.repo, self.branch)
    }

    async fn list_files(&self, path: &str) -> Result<String, String> {
        let p = check_path(path)?;
        let rev = self.rev();
        let mut args = vec!["ls-tree", "-r", "--name-only", &rev];
        if !p.is_empty() {
            args.push("--");
            args.push(&p);
        }
        let (code, out, err) = self.git(&args).await?;
        if code != Some(0) {
            return Err(crate::tasks::truncate(err.trim(), 1000));
        }
        Ok(cap_lines(&out, MAX_LIST))
    }

    async fn read_file(&self, path: &str) -> Result<String, String> {
        let p = check_path(path)?;
        if p.is_empty() {
            return Err("要給檔案路徑".into());
        }
        let spec = format!("{}:{p}", self.rev());
        let (code, out, err) = self.git(&["show", &spec]).await?;
        if code != Some(0) {
            return Err(format!("讀不到 {p}：{}", err.trim()));
        }
        Ok(cap_file(&p, &out))
    }

    async fn grep(&self, pattern: &str, path: &str) -> Result<String, String> {
        let p = check_path(path)?;
        let rev = self.rev();
        let mut args = vec!["grep", "-n", "-I", "-E", "-e", pattern, &rev];
        if !p.is_empty() {
            args.push("--");
            args.push(&p);
        }
        let (code, out, err) = self.git(&args).await?;
        match code {
            // 結果每行前面會帶 `<rev>:`，拿掉。
            Some(0) => Ok(cap_lines(
                &out.lines()
                    .map(|l| l.strip_prefix(&format!("{rev}:")).unwrap_or(l))
                    .collect::<Vec<_>>()
                    .join("\n"),
                MAX_LIST,
            )),
            Some(1) => Ok("（沒有符合的結果）".into()),
            _ => Err(crate::tasks::truncate(err.trim(), 1000)),
        }
    }

    async fn git_log(&self, path: &str, n: usize) -> Result<String, String> {
        let p = check_path(path)?;
        let rev = self.rev();
        let count = n.clamp(1, 100).to_string();
        let mut args = vec!["log", "--no-color", "--oneline", "-n", &count, &rev];
        if !p.is_empty() {
            args.push("--");
            args.push(&p);
        }
        let (code, out, err) = self.git(&args).await?;
        if code != Some(0) {
            return Err(crate::tasks::truncate(err.trim(), 1000));
        }
        Ok(out)
    }

    async fn git_diff(&self) -> Option<Result<String, String>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside() {
        assert_eq!(check_path("./src/lib.rs").unwrap(), "src/lib.rs");
        assert_eq!(check_path("").unwrap(), "");
        assert_eq!(check_path("src/").unwrap(), "src");
        assert!(check_path("/etc/passwd").is_err());
        assert!(check_path("a/../../b").is_err());
        assert!(check_path("~/x").is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn file_cap() {
        let text = (0..3000).map(|i| format!("{i}\n")).collect::<String>();
        let out = cap_file("x", &text);
        assert!(out.contains("共 3000 行"));
        assert_eq!(out.lines().count(), MAX_LINES + 1);
    }
}
