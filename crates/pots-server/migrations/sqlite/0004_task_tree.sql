-- M2 任務樹與 artifact（proposal/architecture.md#資料表、proposal/artifacts.md）。
-- 寫法同 0001：TEXT ID、BIGINT 時間、JSON 存 TEXT。

-- 人類意圖的根節點。Brief 是掛在 Goal 上的 artifact，不放在這張表。
CREATE TABLE goals (
    id            TEXT   PRIMARY KEY NOT NULL,
    project_id    TEXT   NOT NULL REFERENCES projects (id),
    user_id       TEXT   NOT NULL,
    title         TEXT   NOT NULL,
    -- feature | quick（quick 在 M4 的快速通道才會出現）
    kind          TEXT   NOT NULL,
    -- active | done | cancelled
    state         TEXT   NOT NULL,
    -- done 的摘要或 cancelled 的理由
    outcome       TEXT,
    -- 誰建立的（Principal 的 JSON）
    created_by    TEXT   NOT NULL,
    -- 狀態面板上 Planner 的最後一句話（規劃中、完成說明或失敗原因）
    planner_note  TEXT   NOT NULL DEFAULT '',
    -- M6 才有預算；NULL 表示不限
    budget_tokens BIGINT,
    spent_tokens  BIGINT NOT NULL DEFAULT 0,
    created_at    BIGINT NOT NULL,
    updated_at    BIGINT NOT NULL
);

CREATE INDEX goals_project ON goals (project_id, created_at);

-- 任務樹節點。`state` 是伺服器 TaskState 的 JSON。
CREATE TABLE tasks (
    id           TEXT   PRIMARY KEY NOT NULL,
    goal_id      TEXT   NOT NULL REFERENCES goals (id),
    parent_id    TEXT   REFERENCES tasks (id),
    title        TEXT   NOT NULL,
    summary      TEXT   NOT NULL DEFAULT '',
    -- 指派給哪個 profile（M4 之前沒有 worker，一律是空字串）
    assignee     TEXT   NOT NULL DEFAULT '',
    state        TEXT   NOT NULL,
    attempts     BIGINT NOT NULL DEFAULT 0,
    spent_tokens BIGINT NOT NULL DEFAULT 0,
    created_at   BIGINT NOT NULL,
    updated_at   BIGINT NOT NULL
);

CREATE INDEX tasks_goal ON tasks (goal_id, created_at);

-- 任務狀態變化的只增日誌。`from_state` 為 NULL 表示建立；`cause` 是 {by: Principal, reason} 的 JSON。
CREATE TABLE task_events (
    task_id    TEXT   NOT NULL REFERENCES tasks (id),
    seq        BIGINT NOT NULL,
    from_state TEXT,
    to_state   TEXT   NOT NULL,
    cause      TEXT   NOT NULL,
    at         BIGINT NOT NULL,
    PRIMARY KEY (task_id, seq)
);

-- agent_runs 變成持久化佇列：Planner 的 run 先以 queued 寫入，runner 取走時改成 running
-- 並持有 lease（定期心跳）；lease 過期或服務重啟時回到 queued，attempts 加一。
-- chat 的 run 仍由記憶體裡的 lane 排程，只在開始時寫一列 running。
-- state：queued | running | done | failed | cancelled。queued 的 started_at 是排入時間。
ALTER TABLE agent_runs ADD COLUMN goal_id TEXT;
ALTER TABLE agent_runs ADD COLUMN task_id TEXT;
ALTER TABLE agent_runs ADD COLUMN lease_until BIGINT;
ALTER TABLE agent_runs ADD COLUMN attempts BIGINT NOT NULL DEFAULT 0;

CREATE INDEX agent_runs_queue ON agent_runs (agent, state, started_at);
CREATE INDEX agent_runs_goal ON agent_runs (goal_id, state);

-- 人與 agent 共同編輯的 Markdown 文件。
CREATE TABLE artifacts (
    id          TEXT   PRIMARY KEY NOT NULL,
    project_id  TEXT   NOT NULL REFERENCES projects (id),
    goal_id     TEXT   REFERENCES goals (id),
    -- spec | brief | ledger | report | project_notes
    kind        TEXT   NOT NULL,
    title       TEXT   NOT NULL,
    rev         BIGINT NOT NULL,
    body_md     TEXT   NOT NULL,
    -- 你改過的區段：[[標題, unix 秒], ...]
    human_edits TEXT   NOT NULL DEFAULT '[]',
    created_at  BIGINT NOT NULL,
    updated_at  BIGINT NOT NULL
);

CREATE INDEX artifacts_project ON artifacts (project_id, created_at);
-- 每個 Goal 最多一份 Brief、一份 Ledger。
CREATE UNIQUE INDEX artifacts_goal_kind ON artifacts (goal_id, kind)
    WHERE goal_id IS NOT NULL AND kind IN ('brief', 'ledger');

-- 每次修改的全文與作者（Principal 的 JSON），用於 diff、還原與「人類的修改」。
CREATE TABLE artifact_revisions (
    artifact_id TEXT   NOT NULL REFERENCES artifacts (id),
    rev         BIGINT NOT NULL,
    author      TEXT   NOT NULL,
    body_md     TEXT   NOT NULL,
    at          BIGINT NOT NULL,
    PRIMARY KEY (artifact_id, rev)
);

-- 每個 agent 上次讀到哪一版，`read_artifact` 據此附上「之後人類的修改」。
-- reader 例如 `chat:<project>`、`planner:<goal>`。
CREATE TABLE artifact_reads (
    artifact_id TEXT   NOT NULL REFERENCES artifacts (id),
    reader      TEXT   NOT NULL,
    rev         BIGINT NOT NULL,
    PRIMARY KEY (artifact_id, reader)
);
