//! project 的 worker 環境（`proposal/agent-runtime.md#project-環境由環境-worker-建立`）。
//!
//! 環境 worker 分析 repo 後寫一份 [`EnvSpec`]，用 `try_env` 實際建置與驗證，通過的才能用
//! `apply_env` 套用成新版本（`project_envs`，`projects.env_rev` 指向使用中的版本）。一般 worker 的
//! run 用使用中版本的衍生 image、環境變數與快取，開始前先跑 `prepare`。
//!
//! 衍生 image 以 `base_image` 與 `install` 的 hash 命名，hash 一樣就沿用；建置在資料目錄的
//! `env-builds/<hash>/` 寫 Dockerfile 與安裝腳本後 `docker build`。安裝腳本在 build 裡以 root
//! 執行、看不到 repo。快取是資料目錄的 `worker-caches/<project>/<路徑的 hash>`，以 bind mount
//! 掛進容器（容器以你的 uid 執行）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pots_proto::{EnvRevView, EnvSpec, GoalId, ProjectEnvView, ProjectId, ServerEvent, Timestamp};
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::sqlite::SqliteConnection;
use tokio::process::Command;

use crate::Error;
use crate::hub::LogTxn;
use crate::store::parse_id;
use crate::worker::exec;

/// 腳本最長幾個位元組。
pub const MAX_SCRIPT: usize = 16 * 1024;
/// 環境變數與快取最多幾個。
pub const MAX_ENV: usize = 40;
pub const MAX_CACHES: usize = 10;
/// 建 image 最多等多久。
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// 每次 run 的 `prepare` 最多跑多久（不算進 worker 的執行時間上限）。
pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// `try_env` 的 `verify` 最多跑多久。
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// 工具回傳的輸出最多保留多少字元（保留結尾：錯誤通常在最後）。
pub const OUTPUT_TAIL: usize = 6000;

/// 容器裡不能拿來當快取的位置（平台自己掛的、或系統目錄）。
const RESERVED_PATHS: &[&str] = &[
    exec::CONTAINER_WORKDIR,
    exec::CONTAINER_RUN_DIR,
    exec::CONTAINER_OUT_DIR,
    exec::CONTAINER_CLAUDE_CONFIG,
    "/proc",
    "/sys",
    "/dev",
    "/etc",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
];

/// 不能由環境覆寫的環境變數（平台自己設的、或會改變登入方式的）。
const RESERVED_ENV: &[&str] = &["HOME", "CLAUDE_CONFIG_DIR", "USER", "LOGNAME"];
const RESERVED_ENV_PREFIXES: &[&str] = &["ANTHROPIC_", "POTS_", "CLAUDE_"];

/// 檢查並正規化規格；不合法時回說明。
pub fn check_spec(spec: &EnvSpec) -> Result<EnvSpec, String> {
    let base = spec.base_image.trim();
    if !base.is_empty()
        && (base.len() > 200
            || base.starts_with('-')
            || !base
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:@+".contains(c)))
    {
        return Err("base_image 不是合法的 image 名稱".into());
    }
    for (name, s) in [
        ("install", &spec.install),
        ("prepare", &spec.prepare),
        ("verify", &spec.verify),
    ] {
        if s.len() > MAX_SCRIPT {
            return Err(format!("{name} 太長（最多 {MAX_SCRIPT} 位元組）"));
        }
        if s.contains('\0') {
            return Err(format!("{name} 不能有 NUL 字元"));
        }
    }
    if spec.env.len() > MAX_ENV {
        return Err(format!("環境變數最多 {MAX_ENV} 個"));
    }
    let mut env = Vec::new();
    for (k, v) in &spec.env {
        let k = k.trim();
        let valid = !k.is_empty()
            && k.len() <= 100
            && !k.starts_with(|c: char| c.is_ascii_digit())
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(format!("環境變數名稱「{k}」不合法"));
        }
        if RESERVED_ENV.contains(&k) || RESERVED_ENV_PREFIXES.iter().any(|p| k.starts_with(p)) {
            return Err(format!("環境變數 {k} 由平台設定，不能覆寫"));
        }
        if v.len() > 2000 || v.contains(['\0', '\n', '\r']) {
            return Err(format!("環境變數 {k} 的值不合法（最多 2000 字、不能換行）"));
        }
        if env.iter().any(|(n, _): &(String, String)| n == k) {
            return Err(format!("環境變數 {k} 重複"));
        }
        env.push((k.to_string(), v.clone()));
    }
    if spec.caches.len() > MAX_CACHES {
        return Err(format!("快取最多 {MAX_CACHES} 個"));
    }
    let mut caches: Vec<String> = Vec::new();
    for c in &spec.caches {
        let path = normalize_path(c.trim())
            .ok_or_else(|| format!("快取路徑「{}」要是不含 .. 的絕對路徑", c.trim()))?;
        if path == "/"
            || RESERVED_PATHS.iter().any(|r| {
                path == *r
                    || path.starts_with(&format!("{r}/"))
                    || r.starts_with(&format!("{path}/"))
            })
        {
            return Err(format!("快取路徑 {path} 是平台或系統使用的位置"));
        }
        if caches.iter().any(|p| {
            p == &path || p.starts_with(&format!("{path}/")) || path.starts_with(&format!("{p}/"))
        }) {
            return Err(format!("快取路徑 {path} 和其他快取重疊"));
        }
        caches.push(path);
    }
    Ok(EnvSpec {
        base_image: base.to_string(),
        install: spec.install.trim().to_string(),
        prepare: spec.prepare.trim().to_string(),
        verify: spec.verify.trim().to_string(),
        env,
        caches,
    })
}

/// 去掉多餘的 `/` 與 `.`；不是絕對路徑或含 `..` 時回 `None`。
fn normalize_path(p: &str) -> Option<String> {
    if !p.starts_with('/') || p.len() > 300 || p.contains(['\0', ':', ',', '\n']) {
        return None;
    }
    let mut parts = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => return None,
            s => parts.push(s),
        }
    }
    Some(format!("/{}", parts.join("/")))
}

fn sha256_hex(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
    }
    hex::encode(h.finalize())
}

/// 整份規格的 hash（`apply_env` 比對 `try_env` 驗證過的是不是同一份）。
pub fn spec_hash(spec: &EnvSpec) -> String {
    let json = serde_json::to_string(spec).unwrap_or_default();
    sha256_hex(&[&json])
}

/// 實際用的基底 image：規格沒寫時用「Worker」設定的。
pub fn base_of<'a>(spec: &'a EnvSpec, default_image: &'a str) -> &'a str {
    if spec.base_image.is_empty() {
        default_image
    } else {
        &spec.base_image
    }
}

/// 衍生 image 的名稱：沒有安裝腳本時就是基底 image。
pub fn image_tag(project: ProjectId, spec: &EnvSpec, default_image: &str) -> String {
    let base = base_of(spec, default_image);
    if spec.install.is_empty() {
        return base.to_string();
    }
    let hash = sha256_hex(&[base, &spec.install]);
    let simple = project.0.simple().to_string();
    format!("pots-env-{}:{}", &simple[simple.len() - 12..], &hash[..16])
}

/// 建衍生 image 的 Dockerfile。安裝腳本以 root 執行（基底 image 的 entrypoint 不變）。
pub fn dockerfile(base: &str) -> String {
    format!(
        "# Pots 的 project 環境（環境 worker 產生）。\n\
         FROM {base}\n\
         USER root\n\
         COPY install.sh /tmp/pots-install.sh\n\
         RUN sh -eu /tmp/pots-install.sh && rm -f /tmp/pots-install.sh\n"
    )
}

/// 快取在主機上的目錄。
pub fn cache_dir(data_dir: &Path, project: ProjectId, path: &str) -> PathBuf {
    data_dir
        .join("worker-caches")
        .join(project.to_string())
        .join(&sha256_hex(&[path])[..16])
}

/// 這份規格要掛的快取（主機目錄、容器路徑）。
pub fn mounts(data_dir: &Path, project: ProjectId, spec: &EnvSpec) -> Vec<(PathBuf, String)> {
    spec.caches
        .iter()
        .map(|c| (cache_dir(data_dir, project, c), c.clone()))
        .collect()
}

/// 只留結尾 `max` 個字元。
pub fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let skip = n - max;
    format!(
        "…（前面省略 {skip} 字）\n{}",
        s.chars().skip(skip).collect::<String>()
    )
}

fn build_lock(tag: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(tag.to_string())
        .or_default()
        .clone()
}

/// image 建置的結果。
#[derive(Debug)]
pub struct Built {
    pub tag: String,
    /// 這次有實際建置（沒有的話是沿用已有的）。
    pub built: bool,
    /// 建置的輸出（結尾一段）。
    pub log: String,
}

/// 確保衍生 image 存在：沒有安裝腳本時直接用基底；已建好就沿用；否則建置。同一個 image 一次只建一個。
pub async fn ensure_image(
    runtime: &str,
    data_dir: &Path,
    project: ProjectId,
    spec: &EnvSpec,
    default_image: &str,
    instance: &str,
) -> Result<Built, String> {
    let tag = image_tag(project, spec, default_image);
    if spec.install.is_empty() {
        return Ok(Built {
            tag,
            built: false,
            log: String::new(),
        });
    }
    let lock = build_lock(&tag);
    let _guard = lock.lock().await;
    let mut inspect = Command::new(runtime);
    inspect.args(["image", "inspect", "--format", "{{.Id}}", &tag]);
    if let Ok(out) = exec::output_with_timeout(inspect, None, Duration::from_secs(30)).await
        && out.ok()
    {
        return Ok(Built {
            tag,
            built: false,
            log: String::new(),
        });
    }
    let base = base_of(spec, default_image);
    let ctx = data_dir
        .join("env-builds")
        .join(&sha256_hex(&[base, &spec.install])[..16]);
    std::fs::create_dir_all(&ctx).map_err(|e| format!("無法建立建置目錄：{e}"))?;
    std::fs::write(ctx.join("Dockerfile"), dockerfile(base))
        .and_then(|()| std::fs::write(ctx.join("install.sh"), format!("{}\n", spec.install)))
        .map_err(|e| format!("無法寫入建置檔：{e}"))?;
    let mut cmd = Command::new(runtime);
    cmd.args(["build", "--progress=plain", "-t", &tag])
        .arg(format!("--label=pots.instance={instance}"))
        .arg(format!("--label=pots.project={project}"))
        .arg(&ctx);
    let out = exec::output_with_timeout(cmd, None, BUILD_TIMEOUT)
        .await
        .map_err(|e| format!("無法執行 {runtime}：{e}（有安裝並啟動嗎？）"))?;
    let log = tail(&format!("{}{}", out.stdout, out.stderr), OUTPUT_TAIL);
    if out.timed_out {
        return Err(format!(
            "建置環境 image 超過 {} 分鐘\n{log}",
            BUILD_TIMEOUT.as_secs() / 60
        ));
    }
    if !out.ok() {
        return Err(format!(
            "建置環境 image 失敗（結束代碼 {:?}）\n{log}",
            out.code
        ));
    }
    Ok(Built {
        tag,
        built: true,
        log,
    })
}

// ---------------------------------------------------------------------------
// 資料庫

/// 使用中的版本與它的規格。
pub async fn current(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<Option<(u32, EnvSpec)>, Error> {
    let row = sqlx::query(
        "SELECT e.rev, e.spec FROM projects p JOIN project_envs e \
         ON e.project_id = p.id AND e.rev = p.env_rev WHERE p.id = ?",
    )
    .bind(project.to_string())
    .fetch_optional(conn)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let rev: i64 = row.try_get("rev")?;
    let spec: EnvSpec = serde_json::from_str(&row.try_get::<String, _>("spec")?)?;
    Ok(Some((rev as u32, spec)))
}

/// 正在分析環境的 Goal。
pub async fn setup_goal(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<Option<GoalId>, Error> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM goals WHERE project_id = ? AND kind = 'env' AND state = 'active' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(project.to_string())
    .fetch_optional(conn)
    .await?;
    id.map(parse_id).transpose()
}

/// 給客戶端的檢視。
pub async fn view(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<ProjectEnvView, Error> {
    let current: Option<i64> = sqlx::query_scalar("SELECT env_rev FROM projects WHERE id = ?")
        .bind(project.to_string())
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
    let rows = sqlx::query(
        "SELECT rev, spec, summary, created_at FROM project_envs WHERE project_id = ? \
         ORDER BY rev DESC",
    )
    .bind(project.to_string())
    .fetch_all(&mut *conn)
    .await?;
    let mut revs = Vec::new();
    for r in rows {
        revs.push(EnvRevView {
            rev: r.try_get::<i64, _>("rev")? as u32,
            spec: serde_json::from_str(&r.try_get::<String, _>("spec")?)?,
            summary: r.try_get("summary")?,
            created_at: Timestamp(r.try_get("created_at")?),
        });
    }
    Ok(ProjectEnvView {
        current: current.map(|r| r as u32),
        revs,
        setup_goal: setup_goal(conn, project).await?,
    })
}

/// 記一筆 `ProjectEnv` 事件。
pub async fn push(txn: &mut LogTxn<'_>, project: ProjectId) -> Result<(), Error> {
    let env = view(txn.conn(), project).await?;
    txn.push(|seq| ServerEvent::ProjectEnv { seq, project, env })
        .await
}

/// 新增一個版本並設為使用中，回傳版本號。
pub async fn insert_rev(
    conn: &mut SqliteConnection,
    project: ProjectId,
    spec: &EnvSpec,
    summary: &str,
    goal: Option<GoalId>,
) -> Result<u32, Error> {
    let rev: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(rev), 0) + 1 FROM project_envs WHERE project_id = ?",
    )
    .bind(project.to_string())
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO project_envs (project_id, rev, spec, summary, goal_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(project.to_string())
    .bind(rev)
    .bind(serde_json::to_string(spec)?)
    .bind(summary)
    .bind(goal.map(|g| g.to_string()))
    .bind(Timestamp::now().0)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE projects SET env_rev = ? WHERE id = ?")
        .bind(rev)
        .bind(project.to_string())
        .execute(conn)
        .await?;
    Ok(rev as u32)
}

/// 換使用中的版本（`None` 是不用環境）；版本不存在時回 `false`。
pub async fn set_current(
    conn: &mut SqliteConnection,
    project: ProjectId,
    rev: Option<u32>,
) -> Result<bool, Error> {
    if let Some(rev) = rev {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM project_envs WHERE project_id = ? AND rev = ?",
        )
        .bind(project.to_string())
        .bind(rev as i64)
        .fetch_one(&mut *conn)
        .await?;
        if n == 0 {
            return Ok(false);
        }
    }
    sqlx::query("UPDATE projects SET env_rev = ? WHERE id = ?")
        .bind(rev.map(|r| r as i64))
        .bind(project.to_string())
        .execute(conn)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> EnvSpec {
        EnvSpec {
            base_image: String::new(),
            install: "apt-get update".into(),
            prepare: "cargo fetch".into(),
            verify: "cargo check".into(),
            env: vec![("CARGO_TARGET_DIR".into(), "/cache/target".into())],
            caches: vec!["/cache/target/".into(), "/opt/cargo/registry".into()],
        }
    }

    #[test]
    fn specs_are_checked_and_normalized() {
        let s = check_spec(&spec()).unwrap();
        assert_eq!(s.caches, ["/cache/target", "/opt/cargo/registry"]);

        for bad_cache in [
            "relative",
            "/a/../b",
            "/",
            "/workspace/x",
            "/run",
            "/etc/foo",
            "/a:b",
        ] {
            let mut s = spec();
            s.caches = vec![bad_cache.into()];
            assert!(check_spec(&s).is_err(), "{bad_cache}");
        }
        let mut s = spec();
        s.caches = vec!["/a".into(), "/a/b".into()];
        assert!(check_spec(&s).is_err(), "重疊");

        for bad_env in ["HOME", "ANTHROPIC_API_KEY", "1X", "A-B", "", "POTS_X"] {
            let mut s = spec();
            s.env = vec![(bad_env.into(), "v".into())];
            assert!(check_spec(&s).is_err(), "{bad_env}");
        }
        let mut s = spec();
        s.env = vec![("A".into(), "x\ny".into())];
        assert!(check_spec(&s).is_err());
        let mut s = spec();
        s.base_image = "--privileged".into();
        assert!(check_spec(&s).is_err());
        let mut s = spec();
        s.install = "x".repeat(MAX_SCRIPT + 1);
        assert!(check_spec(&s).is_err());
    }

    #[test]
    fn image_tags_follow_base_and_install() {
        let p = ProjectId::from_u64(0xabc);
        let s = spec();
        let a = image_tag(p, &s, "pots-worker:latest");
        assert!(a.starts_with("pots-env-000000000abc:"), "{a}");
        assert_eq!(a, image_tag(p, &s, "pots-worker:latest"));
        // prepare 改了不用重建。
        let mut t = s.clone();
        t.prepare = "true".into();
        assert_eq!(a, image_tag(p, &t, "pots-worker:latest"));
        // 基底或安裝腳本改了要重建。
        assert_ne!(a, image_tag(p, &s, "other:1"));
        t.install = "apt-get install -y git".into();
        assert_ne!(a, image_tag(p, &t, "pots-worker:latest"));
        // 沒有安裝腳本：直接用基底。
        t.install.clear();
        assert_eq!(image_tag(p, &t, "pots-worker:latest"), "pots-worker:latest");
        assert!(dockerfile("pots-worker:latest").starts_with("# Pots"));
    }

    #[test]
    fn spec_hash_covers_everything() {
        let s = spec();
        let mut t = s.clone();
        t.verify = "cargo test".into();
        assert_ne!(spec_hash(&s), spec_hash(&t));
        assert_eq!(spec_hash(&s), spec_hash(&s.clone()));
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("abc", 5), "abc");
        let t = tail("0123456789", 3);
        assert!(t.ends_with("789") && t.contains("省略 7"), "{t}");
    }
}
