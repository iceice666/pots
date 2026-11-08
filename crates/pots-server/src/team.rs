//! Team 貼文（`proposal/agent-collaboration.md#資訊分享管道`）：Goal 內 Planner 與 worker 的短訊，
//! 有型別、只增不改。
//!
//! - 投遞：成員每次 run 開始時收到「上次之後的新貼文」（[`unread`]，讀過的記在 `team_reads`）。
//! - 喚醒：沒有指定對象的 Question 與 Blocker 叫醒 Planner（[`insert`] 回傳）；Handoff 與 Planner
//!   指定任務的 Decision 叫醒那個任務（由 `post_to_team` 依任務狀態決定）；Finding 不叫醒任何人。
//! - 防迴圈：同一個 Goal 每小時最多 [`MAX_POSTS_PER_HOUR`] 則。
//!
//! team 貼文不進 OM；你在 Goal 面板的 Team 分頁看得到。

use pots_proto::{GoalId, TaskId, TeamPost, TeamPostKind, Timestamp};
use sqlx::Row;
use sqlx::sqlite::SqliteConnection;
use uuid::Uuid;

use crate::Error;
use crate::store::{parse_id, to_json};
use crate::tasks::{self, Principal};

/// 一則貼文最多幾個字。
pub const MAX_POST_CHARS: usize = 2000;
/// 同一個 Goal 每小時最多幾則。
pub const MAX_POSTS_PER_HOUR: i64 = 30;
/// Goal 面板最多顯示幾則（最新的）。
const MAX_VIEW_POSTS: i64 = 200;
/// 一次投遞最多幾則（最新的）。
const MAX_DELIVERED: usize = 30;

pub fn kind_str(kind: TeamPostKind) -> &'static str {
    match kind {
        TeamPostKind::Finding => "finding",
        TeamPostKind::Question => "question",
        TeamPostKind::Decision => "decision",
        TeamPostKind::Blocker => "blocker",
        TeamPostKind::Handoff => "handoff",
    }
}

pub fn parse_kind(s: &str) -> Option<TeamPostKind> {
    Some(match s {
        "finding" => TeamPostKind::Finding,
        "question" => TeamPostKind::Question,
        "decision" => TeamPostKind::Decision,
        "blocker" => TeamPostKind::Blocker,
        "handoff" => TeamPostKind::Handoff,
        _ => return None,
    })
}

#[derive(Clone, Debug)]
pub struct PostRow {
    pub seq: i64,
    pub kind: TeamPostKind,
    pub author: Principal,
    pub from_task: Option<TaskId>,
    pub to_task: Option<TaskId>,
    pub body: String,
    pub at: Timestamp,
}

/// 貼文之後要不要叫醒 Planner（呼叫端在 commit 之後通知它）。叫醒任務由呼叫端決定。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wakes {
    pub planner: bool,
}

fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<PostRow, Error> {
    let kind: String = row.try_get("kind")?;
    let author: String = row.try_get("author")?;
    let from: Option<String> = row.try_get("from_task")?;
    let to: Option<String> = row.try_get("to_task")?;
    Ok(PostRow {
        seq: row.try_get("seq")?,
        kind: parse_kind(&kind).ok_or_else(|| Error::Corrupt(format!("貼文種類不對：{kind}")))?,
        author: serde_json::from_str(&author)?,
        from_task: from.map(parse_id).transpose()?,
        to_task: to.map(parse_id).transpose()?,
        body: row.try_get("body")?,
        at: Timestamp(row.try_get("at")?),
    })
}

/// 寫一則貼文。超過速率上限時回說明。呼叫端負責檢查誰能發哪一種、`to_task` 屬於同一個 Goal，
/// 並在同一個 transaction 裡依回傳的 [`Wakes`] 排 run（[`crate::planner::enqueue`]、
/// [`crate::worker::enqueue`]），最後記 `GoalChanged`。
#[allow(clippy::too_many_arguments)]
pub async fn insert(
    conn: &mut SqliteConnection,
    goal: GoalId,
    kind: TeamPostKind,
    author: &Principal,
    from_task: Option<TaskId>,
    to_task: Option<TaskId>,
    body: &str,
    now: Timestamp,
) -> Result<Result<(i64, Wakes), String>, Error> {
    let recent: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM team_posts WHERE goal_id = ? AND at > ?")
            .bind(goal.to_string())
            .bind(now.0 - 3600)
            .fetch_one(&mut *conn)
            .await?;
    if recent >= MAX_POSTS_PER_HOUR {
        return Ok(Err(format!(
            "這個 Goal 一小時內已經有 {recent} 則 team 貼文，先整理成一則再發"
        )));
    }
    let seq: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) + 1 FROM team_posts WHERE goal_id = ?")
            .bind(goal.to_string())
            .fetch_one(&mut *conn)
            .await?;
    sqlx::query(
        "INSERT INTO team_posts (id, goal_id, seq, kind, author, from_task, to_task, body, at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(goal.to_string())
    .bind(seq)
    .bind(kind_str(kind))
    .bind(to_json(author)?)
    .bind(from_task.map(|t| t.to_string()))
    .bind(to_task.map(|t| t.to_string()))
    .bind(body)
    .bind(now.0)
    .execute(conn)
    .await?;
    let wakes = Wakes {
        planner: matches!(kind, TeamPostKind::Question if to_task.is_none())
            || kind == TeamPostKind::Blocker,
    };
    Ok(Ok((seq, wakes)))
}

async fn rows(
    conn: &mut SqliteConnection,
    goal: GoalId,
    after: i64,
) -> Result<Vec<PostRow>, Error> {
    let rows = sqlx::query(
        "SELECT seq, kind, author, from_task, to_task, body, at FROM team_posts \
         WHERE goal_id = ? AND seq > ? ORDER BY seq",
    )
    .bind(goal.to_string())
    .bind(after)
    .fetch_all(conn)
    .await?;
    rows.iter().map(from_row).collect()
}

/// `reader` 上次讀過之後的新貼文（不含自己發的），並記成已讀。最多 [`MAX_DELIVERED`] 則（最新的）。
pub async fn unread(
    conn: &mut SqliteConnection,
    goal: GoalId,
    reader: &str,
    me: &Principal,
    my_task: Option<TaskId>,
) -> Result<Vec<PostRow>, Error> {
    let last: i64 =
        sqlx::query_scalar("SELECT last_seq FROM team_reads WHERE goal_id = ? AND reader = ?")
            .bind(goal.to_string())
            .bind(reader)
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or(0);
    let all = rows(conn, goal, last).await?;
    let Some(newest) = all.last().map(|p| p.seq) else {
        return Ok(Vec::new());
    };
    sqlx::query(
        "INSERT INTO team_reads (goal_id, reader, last_seq) VALUES (?, ?, ?) \
         ON CONFLICT (goal_id, reader) DO UPDATE SET last_seq = excluded.last_seq",
    )
    .bind(goal.to_string())
    .bind(reader)
    .bind(newest)
    .execute(&mut *conn)
    .await?;
    let mut out: Vec<PostRow> = all
        .into_iter()
        .filter(|p| !(p.author == *me && (my_task.is_none() || p.from_task == my_task)))
        .collect();
    if out.len() > MAX_DELIVERED {
        out.drain(..out.len() - MAX_DELIVERED);
    }
    Ok(out)
}

/// 整個 Goal 的貼文（`read_team` 用），最新的 `limit` 則。
pub async fn recent(
    conn: &mut SqliteConnection,
    goal: GoalId,
    limit: usize,
) -> Result<Vec<PostRow>, Error> {
    let mut all = rows(conn, goal, 0).await?;
    if all.len() > limit {
        all.drain(..all.len() - limit);
    }
    Ok(all)
}

/// 給 agent 看的貼文清單。任務以標題與 ID 表示。
pub async fn render(conn: &mut SqliteConnection, posts: &[PostRow]) -> Result<String, Error> {
    let mut out = String::new();
    for p in posts {
        let from = match p.from_task {
            Some(t) => match tasks::task(conn, t).await? {
                Some(t) => format!(
                    "worker（任務「{}」，{}）",
                    tasks::one_line(&t.title, 80),
                    t.id
                ),
                None => p.author.label(),
            },
            None => p.author.label(),
        };
        let to = match p.to_task {
            Some(t) => match tasks::task(conn, t).await? {
                Some(t) => format!(" → 任務「{}」（{}）", tasks::one_line(&t.title, 80), t.id),
                None => String::new(),
            },
            None => String::new(),
        };
        out.push_str(&format!(
            "- [{}] {from}{to}：{}\n",
            p.kind.label(),
            tasks::one_line(&p.body, MAX_POST_CHARS)
        ));
    }
    Ok(out)
}

/// Goal 面板的 Team 分頁。
pub async fn views(conn: &mut SqliteConnection, goal: GoalId) -> Result<Vec<TeamPost>, Error> {
    let rows = sqlx::query(
        "SELECT seq, kind, author, from_task, to_task, body, at FROM team_posts \
         WHERE goal_id = ? ORDER BY seq DESC LIMIT ?",
    )
    .bind(goal.to_string())
    .bind(MAX_VIEW_POSTS)
    .fetch_all(&mut *conn)
    .await?;
    let mut posts: Vec<PostRow> = rows.iter().map(from_row).collect::<Result<_, _>>()?;
    posts.reverse();
    let mut out = Vec::with_capacity(posts.len());
    for p in posts {
        let from_title = match p.from_task {
            Some(t) => tasks::task(conn, t).await?.map(|t| t.title),
            None => None,
        };
        let from = match from_title {
            Some(t) => format!("worker · {t}"),
            None => p.author.label(),
        };
        let to = match p.to_task {
            Some(t) => tasks::task(conn, t).await?.map(|t| t.title),
            None => None,
        };
        out.push(TeamPost {
            kind: p.kind,
            from,
            at: p.at,
            body: p.body,
            to,
        });
    }
    Ok(out)
}
