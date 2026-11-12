-- project 的 worker 環境（proposal/agent-runtime.md#project-環境由環境-worker-建立）。
-- 每次環境 worker 套用一份驗證過的規格就新增一個版本；projects.env_rev 指向使用中的版本，
-- NULL 表示不用環境（用「Worker」設定的 image）。
CREATE TABLE project_envs (
    project_id TEXT    NOT NULL REFERENCES projects (id),
    rev        INTEGER NOT NULL,
    -- EnvSpec 的 JSON
    spec       TEXT    NOT NULL,
    -- 環境 worker 的說明
    summary    TEXT    NOT NULL,
    -- 哪個環境 Goal 建立的
    goal_id    TEXT,
    created_at BIGINT  NOT NULL,
    PRIMARY KEY (project_id, rev)
);

ALTER TABLE projects ADD COLUMN env_rev INTEGER;
