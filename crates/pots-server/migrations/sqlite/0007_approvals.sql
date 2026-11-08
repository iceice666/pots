-- M5 核准與通知（proposal/approvals-and-notifications.md）。
-- 寫法同 0001：TEXT ID、BIGINT 時間、JSON 存 TEXT。

-- 有副作用的動作要你核准。`action` 是動作的 JSON，`action_hash` 是它的 SHA-256：建立後兩者都不再
-- 改變，平台只執行 hash 與核准時一致的動作。狀態的變化另記在只增不改的 `approval_events`。
CREATE TABLE approvals (
    id           TEXT   PRIMARY KEY NOT NULL,
    user_id      TEXT   NOT NULL,
    project_id   TEXT   NOT NULL REFERENCES projects (id),
    goal_id      TEXT,
    -- 在等這筆核准的任務（merge PR）；不擋任務的核准是 NULL
    task_id      TEXT,
    action       TEXT   NOT NULL,
    action_hash  TEXT   NOT NULL,
    title        TEXT   NOT NULL,
    -- agent 給的理由
    reason       TEXT   NOT NULL DEFAULT '',
    -- 誰申請的（Principal 的 JSON）
    requested_by TEXT   NOT NULL,
    -- 新的核准要不要推播（merge 後自動開的刪分支不推）
    notify       BIGINT NOT NULL DEFAULT 1,
    -- pending | executing | executed | rejected | expired | failed | cancelled
    state        TEXT   NOT NULL,
    -- 你決定時附的理由
    note         TEXT,
    -- 執行結果或失敗原因
    result       TEXT,
    -- 核准者（Principal 的 JSON；v1 只有本機使用者）
    decided_by   TEXT,
    decided_at   BIGINT,
    created_at   BIGINT NOT NULL,
    expires_at   BIGINT NOT NULL,
    updated_at   BIGINT NOT NULL
);

CREATE INDEX approvals_state ON approvals (state, expires_at);
CREATE INDEX approvals_goal ON approvals (goal_id, state);
CREATE INDEX approvals_recent ON approvals (updated_at);

-- 核准狀態變化的只增日誌（稽核）。`by_principal` 是 Principal 的 JSON。
CREATE TABLE approval_events (
    approval_id  TEXT   NOT NULL REFERENCES approvals (id),
    seq          BIGINT NOT NULL,
    state        TEXT   NOT NULL,
    by_principal TEXT   NOT NULL,
    detail       TEXT,
    at           BIGINT NOT NULL,
    PRIMARY KEY (approval_id, seq)
);

-- agent 提案的 listener 等哪一筆核准（`state = 'pending'`）。
ALTER TABLE listeners ADD COLUMN approval_id TEXT;

-- 平台 merge PR 的預設方式：merge | squash | rebase。
ALTER TABLE projects ADD COLUMN merge_method TEXT NOT NULL DEFAULT 'squash';

-- 連續幾次 CI 失敗（自動修的次數上限）；CI 通過時歸零。
ALTER TABLE tasks ADD COLUMN ci_failures BIGINT NOT NULL DEFAULT 0;

-- USER.md：跨 project 的使用者偏好，每次修改一版（author 是 Principal 的 JSON）。
CREATE TABLE user_md_revisions (
    user_id TEXT   NOT NULL,
    rev     BIGINT NOT NULL,
    body    TEXT   NOT NULL,
    author  TEXT   NOT NULL,
    at      BIGINT NOT NULL,
    PRIMARY KEY (user_id, rev)
);
