//! USER.md 與 Project Notes：放進 agent preamble 的長期偏好與專案知識
//!（`proposal/memory.md#usermd跨-project-的使用者偏好`、`proposal/artifacts.md`）。
//!
//! - USER.md 屬於實例的使用者，你在設定裡直接編輯；每次修改一版（`user_md_revisions`）。
//! - Project Notes 是每個 project 一份的 artifact（`project_notes`），你在 Artifacts 分頁直接編輯。
//! - agent 對兩者都只能提案（`propose_user_md_edit`、`propose_project_note`），你核准後才由平台寫入
//!   （`crate::approvals`）。
//! - 對話 agent、Planner 與 worker 都把 USER.md 放在 instructions 的最前面，Project Notes 接在
//!   project 指示後面（[`compose`]）。

use pots_proto::{ArtifactId, ArtifactKind, ProjectId, ServerEvent, Timestamp, UserMdView};
use sqlx::sqlite::SqliteConnection;

use crate::Error;
use crate::artifacts::{self, ArtifactRow};
use crate::hub::LogTxn;
use crate::store::{LOCAL_USER_ID, to_json};
use crate::tasks::Principal;

/// 新的 Project Notes 的內容。
const NOTES_TEMPLATE: &str = "# Project Notes\n\n跨 Goal 的專案知識：怎麼跑測試、踩過的坑、慣例。agent 只能提案修改，你核准後才寫入。\n";

/// 目前的 USER.md（還沒有時 rev 是 0）。
pub async fn current(conn: &mut SqliteConnection) -> Result<UserMdView, Error> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT rev, body FROM user_md_revisions WHERE user_id = ? ORDER BY rev DESC LIMIT 1",
    )
    .bind(LOCAL_USER_ID.to_string())
    .fetch_optional(conn)
    .await?;
    Ok(row
        .map(|(rev, body)| UserMdView {
            rev: rev as u32,
            body,
        })
        .unwrap_or_default())
}

/// 某一版的內容（0 是空的）。
pub async fn revision(conn: &mut SqliteConnection, rev: u32) -> Result<Option<String>, Error> {
    if rev == 0 {
        return Ok(Some(String::new()));
    }
    Ok(
        sqlx::query_scalar("SELECT body FROM user_md_revisions WHERE user_id = ? AND rev = ?")
            .bind(LOCAL_USER_ID.to_string())
            .bind(i64::from(rev))
            .fetch_optional(conn)
            .await?,
    )
}

/// 整理內容：去掉頭尾空白、統一換行，檢查長度。
pub fn check(body: &str) -> Result<String, String> {
    let body = body.replace("\r\n", "\n");
    let body = body.trim();
    if body.chars().count() > pots_proto::MAX_USER_MD_CHARS {
        return Err(format!(
            "USER.md 最多 {} 個字",
            pots_proto::MAX_USER_MD_CHARS
        ));
    }
    Ok(if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    })
}

/// 寫入新的一版（`base_rev` 不是目前的版本時拒絕）並記一筆 `UserMd`。內容沒變時不寫新版本，
/// 但仍重送目前的內容（讓客戶端收掉「未儲存」）。
pub async fn save(
    txn: &mut LogTxn<'_>,
    base_rev: u32,
    body: &str,
    author: &Principal,
) -> Result<Result<UserMdView, String>, Error> {
    let body = match check(body) {
        Ok(b) => b,
        Err(e) => return Ok(Err(e)),
    };
    let now = current(txn.conn()).await?;
    if now.rev != base_rev {
        return Ok(Err(format!(
            "USER.md 在你開始編輯之後被改過（現在是 v{}），請重新載入再改",
            now.rev
        )));
    }
    let view = if now.body == body {
        now
    } else {
        let rev = now.rev + 1;
        sqlx::query(
            "INSERT INTO user_md_revisions (user_id, rev, body, author, at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(LOCAL_USER_ID.to_string())
        .bind(i64::from(rev))
        .bind(&body)
        .bind(to_json(author)?)
        .bind(Timestamp::now().0)
        .execute(txn.conn())
        .await?;
        UserMdView { rev, body }
    };
    let user_md = view.clone();
    txn.push(|seq| ServerEvent::UserMd { seq, user_md }).await?;
    Ok(Ok(view))
}

/// project 的 Project Notes（掛在 project 上、最早的那一份）。
pub async fn project_notes(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<Option<ArtifactRow>, Error> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM artifacts WHERE project_id = ? AND goal_id IS NULL AND kind = 'project_notes' \
         ORDER BY created_at, id LIMIT 1",
    )
    .bind(project.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    match id {
        Some(id) => artifacts::get(conn, crate::store::parse_id::<ArtifactId>(id)?).await,
        None => Ok(None),
    }
}

/// 確保 project 有一份 Project Notes（你才能直接編輯）；新建時記一筆 `ArtifactChanged`。
pub async fn ensure_project_notes(
    txn: &mut LogTxn<'_>,
    project: ProjectId,
) -> Result<ArtifactRow, Error> {
    if let Some(row) = project_notes(txn.conn(), project).await? {
        return Ok(row);
    }
    let row = ArtifactRow {
        id: ArtifactId::new(),
        project,
        goal: None,
        kind: ArtifactKind::ProjectNotes,
        title: "Project Notes".into(),
        rev: 1,
        body: NOTES_TEMPLATE.into(),
        human_edits: Vec::new(),
    };
    artifacts::insert(txn.conn(), &row, &Principal::System).await?;
    artifacts::push_changed(txn, &row).await?;
    Ok(row)
}

/// 放進 instructions 的使用者偏好與專案知識：`(最前面, 接在 project 指示後面)`。沒有內容時是空字串。
pub async fn compose(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<(String, String), Error> {
    let user = current(conn).await?;
    let head = if user.body.trim().is_empty() {
        String::new()
    } else {
        format!(
            "# 使用者偏好（USER.md，rev {}；使用者自己寫的或核准過的）\n\n{}\n\n",
            user.rev,
            user.body.trim()
        )
    };
    let notes = project_notes(conn, project).await?;
    let tail = match notes {
        Some(n) if n.body.trim() != NOTES_TEMPLATE.trim() && !n.body.trim().is_empty() => format!(
            "\n\n# 專案知識（Project Notes，rev {}；使用者寫的或核准過的）\n\n{}",
            n.rev,
            n.body.trim()
        ),
        _ => String::new(),
    };
    Ok((head, tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_are_normalized() {
        assert_eq!(check("  a\r\nb  ").unwrap(), "a\nb\n");
        assert_eq!(check("   ").unwrap(), "");
        assert!(check(&"x".repeat(pots_proto::MAX_USER_MD_CHARS + 1)).is_err());
    }
}
