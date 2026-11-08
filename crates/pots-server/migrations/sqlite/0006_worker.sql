-- M4 coding worker（proposal/agent-runtime.md#worker-adapter內建-worker-與外部-coding-cli）。
-- 寫法同 0001：TEXT ID、BIGINT 時間、JSON 存 TEXT。

-- 任務的工作位置與結果。repo 是 `github.com/owner/repo`；branch 是平台推上去的分支；
-- pr 是 `github.com/owner/repo#42`。progress 是 worker 最後一句進度（狀態面板）。
ALTER TABLE tasks ADD COLUMN repo TEXT;
ALTER TABLE tasks ADD COLUMN branch TEXT;
ALTER TABLE tasks ADD COLUMN pr TEXT;
ALTER TABLE tasks ADD COLUMN pr_url TEXT;
ALTER TABLE tasks ADD COLUMN progress TEXT NOT NULL DEFAULT '';
-- 第一次開始執行與結束的時間（耗時）。
ALTER TABLE tasks ADD COLUMN started_at BIGINT;
ALTER TABLE tasks ADD COLUMN finished_at BIGINT;

-- subagent 的 run 掛在開它的 run 下面（不建任務）。
ALTER TABLE agent_runs ADD COLUMN parent_run_id TEXT;

-- 外部 runtime（Claude Code）的續接：每個任務一筆。external_session_id 是 CLI 的 session ID，
-- 在啟動 CLI 之前就寫入；state_ref 指向官方 session 儲存所在的 home（不是憑證匯入欄位），
-- worktree_ref 是任務的工作目錄。
CREATE TABLE worker_sessions (
    id                  TEXT   PRIMARY KEY NOT NULL,
    user_id             TEXT   NOT NULL,
    task_id             TEXT   NOT NULL REFERENCES tasks (id),
    run_id              TEXT,
    -- builtin | claude_code
    backend             TEXT   NOT NULL,
    external_session_id TEXT,
    state_ref           TEXT,
    worktree_ref        TEXT,
    cli_version         TEXT,
    created_at          BIGINT NOT NULL,
    updated_at          BIGINT NOT NULL
);

CREATE UNIQUE INDEX worker_sessions_task ON worker_sessions (task_id, backend);

-- Team 貼文（proposal/agent-collaboration.md#資訊分享管道）：Goal 內的短訊，只增不改。
-- kind：finding | question | decision | blocker | handoff。author 是 Principal 的 JSON；
-- from_task 是 worker 發文時的任務，to_task 是 question／handoff 的對象。
CREATE TABLE team_posts (
    id         TEXT   PRIMARY KEY NOT NULL,
    goal_id    TEXT   NOT NULL REFERENCES goals (id),
    seq        BIGINT NOT NULL,
    kind       TEXT   NOT NULL,
    author     TEXT   NOT NULL,
    from_task  TEXT,
    to_task    TEXT,
    body       TEXT   NOT NULL,
    at         BIGINT NOT NULL
);

CREATE UNIQUE INDEX team_posts_goal_seq ON team_posts (goal_id, seq);

-- 每個成員讀到哪一則；reader 例如 `planner:<goal>`、`worker:<task>`。
CREATE TABLE team_reads (
    goal_id  TEXT   NOT NULL REFERENCES goals (id),
    reader   TEXT   NOT NULL,
    last_seq BIGINT NOT NULL,
    PRIMARY KEY (goal_id, reader)
);
