//! Artifact：人與 agent 共同編輯的 Markdown 文件（`proposal/artifacts.md`）。
//!
//! - agent 寫入一定帶 `base_rev`；期間有人改過就回 [`WriteOutcome::Conflict`]，附上這段期間的
//!   diff，讓 agent 先吸收再重送。agent 以 `## ` 標題區段為單位編輯（[`Edit`]）。
//! - 你的儲存也帶 `base_rev`；期間 agent 改過時以區段合併（[`merge_human`]），同一段兩邊都改過
//!   才拒絕。
//! - 每次修改寫一列 `artifact_revisions`（全文與作者），用於 diff、還原與「人類的修改」。
//! - `artifact_reads` 記每個 agent 上次讀到哪一版，`read_artifact` 據此附上之後人類的修改。

use pots_proto::{
    ArtifactId, ArtifactKind, ArtifactRevisionView, ArtifactView, GoalId, ProjectId, ServerEvent,
    Timestamp, join_sections, split_sections,
};
use serde::Deserialize;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnection, SqliteRow};

use crate::Error;
use crate::hub::LogTxn;
use crate::store::{parse_id, to_json};
use crate::tasks::{self, Principal};

/// 一份 artifact 最多幾個字。
pub const MAX_BODY_CHARS: usize = 100_000;
/// 標題（artifact 與區段）最多幾個字。
pub const MAX_TITLE_CHARS: usize = 120;
/// 版本紀錄最多列出最近幾版。
pub const MAX_HISTORY: u32 = 50;
/// 一次 diff 最多算多久；超過時 `similar` 退回較粗的結果（仍是正確的 diff，只是不最小）。
/// diff 常在持有日誌鎖時計算，不能讓刻意構造的內容卡住整個服務。
const DIFF_DEADLINE: std::time::Duration = std::time::Duration::from_millis(200);

pub fn kind_str(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Spec => "spec",
        ArtifactKind::Brief => "brief",
        ArtifactKind::Ledger => "ledger",
        ArtifactKind::Report => "report",
        ArtifactKind::ProjectNotes => "project_notes",
    }
}

fn parse_kind(s: &str) -> Result<ArtifactKind, Error> {
    Ok(match s {
        "spec" => ArtifactKind::Spec,
        "brief" => ArtifactKind::Brief,
        "ledger" => ArtifactKind::Ledger,
        "report" => ArtifactKind::Report,
        "project_notes" => ArtifactKind::ProjectNotes,
        _ => return Err(Error::Corrupt(format!("artifact 種類不對：{s}"))),
    })
}

/// `artifacts` 的一列。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactRow {
    pub id: ArtifactId,
    pub project: ProjectId,
    pub goal: Option<GoalId>,
    pub kind: ArtifactKind,
    pub title: String,
    pub rev: u32,
    pub body: String,
    pub human_edits: Vec<(String, Timestamp)>,
}

impl ArtifactRow {
    pub fn view(&self) -> ArtifactView {
        let what = match self.kind {
            ArtifactKind::Spec => "規格",
            ArtifactKind::Brief => "Goal 交接",
            ArtifactKind::Ledger => "任務帳本",
            ArtifactKind::Report => "報告",
            ArtifactKind::ProjectNotes => "專案知識",
        };
        ArtifactView {
            id: self.id,
            kind: self.kind,
            title: self.title.clone(),
            note: format!("{what} · v{}", self.rev),
            rev: self.rev,
            goal: self.goal,
            body: self.body.clone(),
            human_edits: self.human_edits.clone(),
            // 寫入在一個 transaction 裡完成，沒有「寫到一半」的狀態。
            agent_writing: None,
            proposals: 0,
        }
    }
}

macro_rules! artifact_columns {
    () => {
        "id, project_id, goal_id, kind, title, rev, body_md, human_edits"
    };
}

fn from_row(row: &SqliteRow) -> Result<ArtifactRow, Error> {
    let goal: Option<String> = row.try_get("goal_id")?;
    let kind: String = row.try_get("kind")?;
    let rev: i64 = row.try_get("rev")?;
    let edits: String = row.try_get("human_edits")?;
    Ok(ArtifactRow {
        id: parse_id(row.try_get("id")?)?,
        project: parse_id(row.try_get("project_id")?)?,
        goal: goal.map(parse_id).transpose()?,
        kind: parse_kind(&kind)?,
        title: row.try_get("title")?,
        rev: rev as u32,
        body: row.try_get("body_md")?,
        human_edits: serde_json::from_str(&edits)?,
    })
}

pub async fn get(
    conn: &mut SqliteConnection,
    id: ArtifactId,
) -> Result<Option<ArtifactRow>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        artifact_columns!(),
        " FROM artifacts WHERE id = ?"
    ))
    .bind(id.to_string())
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(from_row).transpose()
}

/// Goal 的 Brief 或 Ledger。
pub async fn for_goal(
    conn: &mut SqliteConnection,
    goal: GoalId,
    kind: ArtifactKind,
) -> Result<Option<ArtifactRow>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        artifact_columns!(),
        " FROM artifacts WHERE goal_id = ? AND kind = ? ORDER BY created_at, id LIMIT 1"
    ))
    .bind(goal.to_string())
    .bind(kind_str(kind))
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(from_row).transpose()
}

/// project 的所有 artifact，依建立時間。
pub async fn list(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<Vec<ArtifactRow>, Error> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        artifact_columns!(),
        " FROM artifacts WHERE project_id = ? ORDER BY created_at, id"
    ))
    .bind(project.to_string())
    .fetch_all(conn)
    .await?;
    rows.iter().map(from_row).collect()
}

/// 新增一份 artifact（第 1 版）。
pub async fn insert(
    conn: &mut SqliteConnection,
    row: &ArtifactRow,
    author: &Principal,
) -> Result<(), Error> {
    let now = Timestamp::now().0;
    sqlx::query(
        "INSERT INTO artifacts \
         (id, project_id, goal_id, kind, title, rev, body_md, human_edits, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(row.id.to_string())
    .bind(row.project.to_string())
    .bind(row.goal.map(|g| g.to_string()))
    .bind(kind_str(row.kind))
    .bind(&row.title)
    .bind(i64::from(row.rev))
    .bind(&row.body)
    .bind(to_json(&row.human_edits)?)
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    insert_revision(conn, row.id, row.rev, author, &row.body).await
}

async fn insert_revision(
    conn: &mut SqliteConnection,
    id: ArtifactId,
    rev: u32,
    author: &Principal,
    body: &str,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO artifact_revisions (artifact_id, rev, author, body_md, at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(i64::from(rev))
    .bind(to_json(author)?)
    .bind(body)
    .bind(Timestamp::now().0)
    .execute(conn)
    .await?;
    Ok(())
}

/// 寫入新的一版：`row.rev` 之後加一。回傳新的列。
async fn commit_revision(
    conn: &mut SqliteConnection,
    row: &ArtifactRow,
    body: String,
    human_edits: Vec<(String, Timestamp)>,
    author: &Principal,
) -> Result<ArtifactRow, Error> {
    let rev = row.rev + 1;
    let done = sqlx::query(
        "UPDATE artifacts SET rev = ?, body_md = ?, human_edits = ?, updated_at = ? \
         WHERE id = ? AND rev = ?",
    )
    .bind(i64::from(rev))
    .bind(&body)
    .bind(to_json(&human_edits)?)
    .bind(Timestamp::now().0)
    .bind(row.id.to_string())
    .bind(i64::from(row.rev))
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() != 1 {
        // 呼叫端在同一個 transaction 裡剛讀過這列，不該發生。
        return Err(Error::Corrupt(format!("artifact {} 的版本不一致", row.id)));
    }
    insert_revision(conn, row.id, rev, author, &body).await?;
    Ok(ArtifactRow {
        rev,
        body,
        human_edits,
        ..row.clone()
    })
}

/// 某一版的全文。
pub async fn revision_body(
    conn: &mut SqliteConnection,
    id: ArtifactId,
    rev: u32,
) -> Result<Option<String>, Error> {
    Ok(sqlx::query_scalar(
        "SELECT body_md FROM artifact_revisions WHERE artifact_id = ? AND rev = ?",
    )
    .bind(id.to_string())
    .bind(i64::from(rev))
    .fetch_optional(conn)
    .await?)
}

/// 最近 [`MAX_HISTORY`] 個版本，由新到舊，每一版附上和前一版的 diff（不帶全文，避免訊息過大）。
pub async fn history(
    conn: &mut SqliteConnection,
    id: ArtifactId,
) -> Result<Vec<ArtifactRevisionView>, Error> {
    // 多拿一版，最舊的那一版才有前一版可以比較。
    let mut rows = sqlx::query(
        "SELECT rev, author, body_md, at FROM artifact_revisions WHERE artifact_id = ? \
         ORDER BY rev DESC LIMIT ?",
    )
    .bind(id.to_string())
    .bind(i64::from(MAX_HISTORY + 1))
    .fetch_all(conn)
    .await?;
    rows.reverse();
    let skip_first = rows.len() > MAX_HISTORY as usize;
    let mut out: Vec<ArtifactRevisionView> = Vec::with_capacity(rows.len());
    let mut prev: Option<(u32, String)> = None;
    for row in &rows {
        let rev: i64 = row.try_get("rev")?;
        let rev = rev as u32;
        let author: String = row.try_get("author")?;
        let author: Principal = serde_json::from_str(&author)?;
        let body: String = row.try_get("body_md")?;
        let d = match &prev {
            Some((p, old)) => diff(old, &body, &format!("v{p}"), &format!("v{rev}")),
            None => String::new(),
        };
        out.push(ArtifactRevisionView {
            rev,
            author: author.label(),
            at: Timestamp(row.try_get("at")?),
            diff: d,
        });
        prev = Some((rev, body));
    }
    if skip_first {
        out.remove(0);
    }
    out.reverse();
    Ok(out)
}

/// `after` 之後（不含）到 `through`（含）之間有沒有你的修改。
async fn human_changed_between(
    conn: &mut SqliteConnection,
    id: ArtifactId,
    after: u32,
    through: u32,
) -> Result<bool, Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM artifact_revisions \
         WHERE artifact_id = ? AND rev > ? AND rev <= ? AND author = ?",
    )
    .bind(id.to_string())
    .bind(i64::from(after))
    .bind(i64::from(through))
    .bind(to_json(&Principal::Human)?)
    .fetch_one(conn)
    .await?;
    Ok(n > 0)
}

pub async fn last_read(
    conn: &mut SqliteConnection,
    id: ArtifactId,
    reader: &str,
) -> Result<Option<u32>, Error> {
    let rev: Option<i64> =
        sqlx::query_scalar("SELECT rev FROM artifact_reads WHERE artifact_id = ? AND reader = ?")
            .bind(id.to_string())
            .bind(reader)
            .fetch_optional(conn)
            .await?;
    Ok(rev.map(|r| r as u32))
}

pub async fn mark_read(
    conn: &mut SqliteConnection,
    id: ArtifactId,
    reader: &str,
    rev: u32,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO artifact_reads (artifact_id, reader, rev) VALUES (?, ?, ?) \
         ON CONFLICT (artifact_id, reader) DO UPDATE SET rev = excluded.rev",
    )
    .bind(id.to_string())
    .bind(reader)
    .bind(i64::from(rev))
    .execute(conn)
    .await?;
    Ok(())
}

/// agent 讀取：全文，加上自它上次讀取之後你的修改（diff）。之後記下它讀到這一版。
pub async fn read_for_agent(
    conn: &mut SqliteConnection,
    row: &ArtifactRow,
    reader: &str,
) -> Result<Option<String>, Error> {
    let human = match last_read(conn, row.id, reader).await? {
        Some(last)
            if last < row.rev && human_changed_between(conn, row.id, last, row.rev).await? =>
        {
            let old = revision_body(conn, row.id, last).await?.unwrap_or_default();
            Some(diff(
                &old,
                &row.body,
                &format!("v{last}"),
                &format!("v{}", row.rev),
            ))
        }
        _ => None,
    };
    mark_read(conn, row.id, reader, row.rev).await?;
    Ok(human)
}

/// 把 artifact 的變化記成事件；Brief 還會更新它所屬 Goal 的檢視（`GoalView::brief`）。
pub async fn push_changed(txn: &mut LogTxn<'_>, row: &ArtifactRow) -> Result<(), Error> {
    let project = row.project;
    let artifact = row.view();
    txn.push(|seq| ServerEvent::ArtifactChanged {
        seq,
        project,
        artifact,
    })
    .await?;
    if row.kind == ArtifactKind::Brief
        && let Some(goal) = row.goal
    {
        tasks::push_goal(txn, goal).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 編輯協定

/// agent 對 artifact 的一個編輯。`body_md` 是區段內文，不含 `## ` 標題行。
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Edit {
    ReplaceSection {
        heading: String,
        body_md: String,
    },
    AppendSection {
        heading: String,
        body_md: String,
    },
    /// 整份取代；只在新建時可以用。
    ReplaceAll {
        body_md: String,
    },
}

/// agent 寫入的結果。
#[derive(Debug)]
pub enum WriteOutcome {
    Written(ArtifactRow),
    /// `base_rev` 不是目前的版本。`changes` 是 `base_rev` 到目前版本的 diff。
    Conflict {
        current_rev: u32,
        changes: String,
        human: bool,
    },
}

fn section_source(heading: &str, body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        format!("## {heading}")
    } else {
        format!("## {heading}\n\n{body}")
    }
}

fn check_heading(heading: &str) -> Result<String, String> {
    let h = heading.trim();
    if h.is_empty() || h.contains('\n') {
        return Err("區段標題要是非空的單行文字".into());
    }
    if h.chars().count() > MAX_TITLE_CHARS {
        return Err(format!("區段標題最多 {MAX_TITLE_CHARS} 個字"));
    }
    Ok(h.to_string())
}

/// 依序套用編輯。`allow_replace_all` 只有新建時為真。
pub fn apply_edits(body: &str, edits: &[Edit], allow_replace_all: bool) -> Result<String, String> {
    if edits.is_empty() {
        return Err("edits 不能是空的".into());
    }
    let mut sections: Vec<(Option<String>, String)> = split_sections(body)
        .into_iter()
        .map(|s| (s.heading, s.source))
        .collect();
    for edit in edits {
        match edit {
            Edit::ReplaceSection { heading, body_md }
            | Edit::AppendSection { heading, body_md } => {
                let h = check_heading(heading)?;
                if split_sections(body_md).iter().any(|s| s.heading.is_some()) {
                    return Err(format!(
                        "「{h}」的 body_md 不能再包含 `## ` 標題；新的區段請另外用 append_section"
                    ));
                }
                let pos = sections.iter().position(|(s, _)| s.as_deref() == Some(&h));
                match (edit, pos) {
                    (Edit::ReplaceSection { .. }, Some(i)) => {
                        sections[i].1 = section_source(&h, body_md)
                    }
                    (Edit::ReplaceSection { .. }, None) => {
                        return Err(format!("找不到「{h}」區段；新增區段請用 append_section"));
                    }
                    (_, Some(_)) => {
                        return Err(format!("已經有「{h}」區段；修改請用 replace_section"));
                    }
                    (_, None) => sections.push((Some(h.clone()), section_source(&h, body_md))),
                }
            }
            Edit::ReplaceAll { body_md } => {
                if !allow_replace_all {
                    return Err(
                        "replace_all 只能在新建時使用；修改既有的 artifact 請用 replace_section 或 append_section"
                            .into(),
                    );
                }
                sections = split_sections(body_md)
                    .into_iter()
                    .map(|s| (s.heading, s.source))
                    .collect();
            }
        }
    }
    let out = join_sections(sections.iter().map(|(_, s)| s.as_str()));
    if out.chars().count() > MAX_BODY_CHARS {
        return Err(format!("artifact 最多 {MAX_BODY_CHARS} 個字"));
    }
    Ok(out)
}

/// agent 寫入既有的 artifact。編輯不合法時回 `Ok(Err(說明))`。
pub async fn agent_write(
    conn: &mut SqliteConnection,
    row: &ArtifactRow,
    base_rev: u32,
    edits: &[Edit],
    author: &Principal,
    reader: &str,
) -> Result<Result<WriteOutcome, String>, Error> {
    if base_rev != row.rev {
        let old = revision_body(conn, row.id, base_rev).await?;
        let Some(old) = old.filter(|_| base_rev < row.rev) else {
            return Ok(Err(format!(
                "base_rev {base_rev} 不存在；目前是 v{}，請先 read_artifact",
                row.rev
            )));
        };
        let human = human_changed_between(conn, row.id, base_rev, row.rev).await?;
        return Ok(Ok(WriteOutcome::Conflict {
            current_rev: row.rev,
            changes: diff(
                &old,
                &row.body,
                &format!("v{base_rev}"),
                &format!("v{}", row.rev),
            ),
            human,
        }));
    }
    let body = match apply_edits(&row.body, edits, false) {
        Ok(b) => b,
        Err(e) => return Ok(Err(e)),
    };
    if body == row.body {
        return Ok(Err("這些編輯沒有改變內容".into()));
    }
    let human_edits = prune_edits(&row.human_edits, &body);
    let new = commit_revision(conn, row, body, human_edits, author).await?;
    // agent 剛寫完，知道目前的內容。
    mark_read(conn, new.id, reader, new.rev).await?;
    Ok(Ok(WriteOutcome::Written(new)))
}

/// 你儲存修改。期間 agent 改過時以區段合併；兩邊改了同一段回 `Ok(Err(說明))`。
pub async fn human_save(
    conn: &mut SqliteConnection,
    row: &ArtifactRow,
    base_rev: u32,
    body: &str,
) -> Result<Result<Option<ArtifactRow>, String>, Error> {
    if body.chars().count() > MAX_BODY_CHARS {
        return Ok(Err(format!("artifact 最多 {MAX_BODY_CHARS} 個字")));
    }
    let merged = if base_rev == row.rev {
        normalize(body)
    } else {
        let base = match revision_body(conn, row.id, base_rev).await? {
            Some(b) if base_rev < row.rev => b,
            _ => return Ok(Err(format!("找不到你開始編輯的版本 v{base_rev}"))),
        };
        match merge_human(&base, &row.body, body) {
            Ok(m) => m,
            Err(conflicts) => {
                return Ok(Err(format!(
                    "你編輯期間 artifact 已更新到 v{}，而且這些區段兩邊都改了：{}。你的修改還沒儲存",
                    row.rev,
                    conflicts.join("、")
                )));
            }
        }
    };
    if merged == row.body {
        return Ok(Ok(None));
    }
    let now = Timestamp::now();
    let mut human_edits = prune_edits(&row.human_edits, &merged);
    for h in edited_sections(&row.body, &merged) {
        human_edits.retain(|(x, _)| *x != h);
        human_edits.push((h, now));
    }
    let new = commit_revision(conn, row, merged, human_edits, &Principal::Human).await?;
    Ok(Ok(Some(new)))
}

/// 還原成某一版的內容（新增一個版本）。
pub async fn restore(
    conn: &mut SqliteConnection,
    row: &ArtifactRow,
    rev: u32,
) -> Result<Result<Option<ArtifactRow>, String>, Error> {
    let Some(body) = revision_body(conn, row.id, rev).await? else {
        return Ok(Err(format!("找不到 v{rev}")));
    };
    human_save(conn, row, row.rev, &body).await
}

/// 與 `join_sections` 一致的正規化，避免只差空白就多一個版本。
fn normalize(body: &str) -> String {
    join_sections(split_sections(body).iter().map(|s| s.source.as_str()))
}

/// 只留還存在的區段的「你改過」標記。
fn prune_edits(edits: &[(String, Timestamp)], body: &str) -> Vec<(String, Timestamp)> {
    let headings: Vec<String> = split_sections(body)
        .into_iter()
        .filter_map(|s| s.heading)
        .collect();
    edits
        .iter()
        .filter(|(h, _)| headings.contains(h))
        .cloned()
        .collect()
}

/// `new` 裡內容和 `old` 不同（或新增）的 `## ` 區段標題。
fn edited_sections(old: &str, new: &str) -> Vec<String> {
    let old = split_sections(old);
    split_sections(new)
        .into_iter()
        .filter_map(|s| {
            let h = s.heading.clone()?;
            let before = old.iter().find(|o| o.heading.as_ref() == Some(&h));
            (before.map(|o| &o.source) != Some(&s.source)).then_some(h)
        })
        .collect()
}

type Keyed = Vec<(Option<String>, String)>;

/// 依標題切段；有重複標題時回 `None`（無法以標題對應）。
fn keyed(md: &str) -> Option<Keyed> {
    let out: Keyed = split_sections(md)
        .into_iter()
        .map(|s| (s.heading, s.source))
        .collect();
    let mut seen = std::collections::HashSet::new();
    out.iter()
        .all(|(h, _)| seen.insert(h.clone()))
        .then_some(out)
}

fn label(key: &Option<String>) -> String {
    match key {
        Some(h) => format!("「{h}」"),
        None => "開頭（第一個 ## 之前）".into(),
    }
}

/// 三方合併：`base` 是你開始編輯時的版本，`current` 是目前的版本（agent 期間改過），
/// `mine` 是你的草稿。你改過的區段套用到 `current` 上；同一段兩邊改得不一樣時回傳衝突的區段。
pub fn merge_human(base: &str, current: &str, mine: &str) -> Result<String, Vec<String>> {
    let (Some(b), Some(c), Some(m)) = (keyed(base), keyed(current), keyed(mine)) else {
        return Err(vec!["（文件裡有重複的標題，無法自動合併）".into()]);
    };
    let get =
        |v: &Keyed, k: &Option<String>| v.iter().find(|(h, _)| h == k).map(|(_, s)| s.clone());
    let mut keys: Vec<Option<String>> = Vec::new();
    for (k, _) in b.iter().chain(&m).chain(&c) {
        if !keys.contains(k) {
            keys.push(k.clone());
        }
    }
    let mine_changed: Vec<&Option<String>> =
        keys.iter().filter(|k| get(&b, k) != get(&m, k)).collect();
    let conflicts: Vec<String> = mine_changed
        .iter()
        .filter(|k| get(&b, k) != get(&c, k) && get(&c, k) != get(&m, k))
        .map(|k| label(k))
        .collect();
    if !conflicts.is_empty() {
        return Err(conflicts);
    }
    let mut out = c.clone();
    for k in mine_changed {
        let pos = out.iter().position(|(h, _)| h == k);
        match (get(&m, k), pos) {
            (Some(src), Some(i)) => out[i].1 = src,
            (Some(src), None) => {
                // 插在你的草稿裡它前一段的後面；開頭段放最前面；找不到就放最後。
                let at = if k.is_none() {
                    0
                } else {
                    let mi = m.iter().position(|(h, _)| h == k).unwrap_or(0);
                    m[..mi]
                        .iter()
                        .rev()
                        .find_map(|(h, _)| out.iter().position(|(x, _)| x == h).map(|i| i + 1))
                        .unwrap_or(out.len())
                };
                out.insert(at, (k.clone(), src));
            }
            (None, Some(i)) => {
                out.remove(i);
            }
            (None, None) => {}
        }
    }
    Ok(join_sections(out.iter().map(|(_, s)| s.as_str())))
}

/// 逐行的 unified diff。
pub fn diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    similar::TextDiff::configure()
        .deadline(std::time::Instant::now() + DIFF_DEADLINE)
        .diff_lines(old, new)
        .unified_diff()
        .context_radius(2)
        .header(old_label, new_label)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# 標題\n\n前言\n\n## 目標\n\n舊目標\n\n## 範圍\n\n- 一\n";

    #[test]
    fn edits_replace_and_append_sections() {
        let out = apply_edits(
            DOC,
            &[
                Edit::ReplaceSection {
                    heading: "目標".into(),
                    body_md: "新目標".into(),
                },
                Edit::AppendSection {
                    heading: "風險".into(),
                    body_md: "- 沒有".into(),
                },
            ],
            false,
        )
        .unwrap();
        assert_eq!(
            out,
            "# 標題\n\n前言\n\n## 目標\n\n新目標\n\n## 範圍\n\n- 一\n\n## 風險\n\n- 沒有\n"
        );
    }

    #[test]
    fn edits_are_validated() {
        let replace_missing = Edit::ReplaceSection {
            heading: "不存在".into(),
            body_md: "x".into(),
        };
        assert!(
            apply_edits(DOC, &[replace_missing], false)
                .unwrap_err()
                .contains("找不到")
        );
        let append_existing = Edit::AppendSection {
            heading: "目標".into(),
            body_md: "x".into(),
        };
        assert!(
            apply_edits(DOC, &[append_existing], false)
                .unwrap_err()
                .contains("已經有")
        );
        let nested = Edit::ReplaceSection {
            heading: "目標".into(),
            body_md: "x\n\n## 偷塞的標題\n\ny".into(),
        };
        assert!(apply_edits(DOC, &[nested], false).is_err());
        let all = Edit::ReplaceAll {
            body_md: "# 新".into(),
        };
        assert!(apply_edits(DOC, std::slice::from_ref(&all), false).is_err());
        assert_eq!(apply_edits("", &[all], true).unwrap(), "# 新\n");
        assert!(apply_edits(DOC, &[], false).is_err());
    }

    #[test]
    fn merge_keeps_both_sides_when_sections_differ() {
        let current = DOC.replace("- 一", "- 一\n- 二（agent）");
        let mine = DOC.replace("舊目標", "我的目標");
        let merged = merge_human(DOC, &current, &mine).unwrap();
        assert!(merged.contains("我的目標"));
        assert!(merged.contains("- 二（agent）"));
    }

    #[test]
    fn merge_rejects_same_section() {
        let current = DOC.replace("舊目標", "agent 的目標");
        let mine = DOC.replace("舊目標", "我的目標");
        let err = merge_human(DOC, &current, &mine).unwrap_err();
        assert_eq!(err, ["「目標」"]);
        // 兩邊改成一樣不算衝突。
        assert!(merge_human(DOC, &current, &current).is_ok());
    }

    #[test]
    fn merge_inserts_new_sections_in_place() {
        let current = format!("{DOC}\n## 風險\n\n- agent 加的\n");
        let mine = DOC.replace("## 範圍", "## 新段\n\n我加的\n\n## 範圍");
        let merged = merge_human(DOC, &current, &mine).unwrap();
        let headings: Vec<_> = split_sections(&merged)
            .into_iter()
            .filter_map(|s| s.heading)
            .collect();
        assert_eq!(headings, ["目標", "新段", "範圍", "風險"]);
        // 你刪掉的區段也會從合併結果拿掉。
        let removed = DOC.replace("## 範圍\n\n- 一\n", "");
        let merged = merge_human(DOC, &current, &removed).unwrap();
        assert!(!merged.contains("## 範圍"));
        assert!(merged.contains("## 風險"));
    }

    #[test]
    fn edited_sections_lists_changed_headings() {
        let new = DOC.replace("舊目標", "新目標") + "\n## 新增\n\nx\n";
        assert_eq!(edited_sections(DOC, &new), ["目標", "新增"]);
    }

    #[test]
    fn diff_is_unified() {
        let d = diff("a\nb\n", "a\nc\n", "v1", "v2");
        assert!(d.contains("--- v1"));
        assert!(d.contains("-b"));
        assert!(d.contains("+c"));
    }
}
