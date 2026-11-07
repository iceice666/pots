-- M0 骨架。只用 SQLite 與 Postgres 都支援的寫法（proposal/deployment.md#讓移交便宜的規則）：
-- UUID 存 TEXT，時間存 INTEGER（unix 秒），JSON 存 TEXT，不用陣列。

-- 本實例的永久身分，只有一列。
CREATE TABLE instance (
    id         TEXT    PRIMARY KEY NOT NULL,
    name       TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE projects (
    id           TEXT    PRIMARY KEY NOT NULL,
    user_id      TEXT    NOT NULL,
    name         TEXT    NOT NULL,
    instructions TEXT    NOT NULL DEFAULT '',
    agent_name   TEXT    NOT NULL,
    -- active | moving | moved
    residency    TEXT    NOT NULL DEFAULT 'active',
    created_at   INTEGER NOT NULL
);

CREATE INDEX projects_user ON projects (user_id, created_at);

-- UI 顯示的完整對話流。`body` 是 pots_proto::StreamItemKind 的 JSON。
-- 人類訊息可能由客戶端產生 ID，不保證時間有序，所以排序與分頁用 `ord`。
CREATE TABLE stream_items (
    id         TEXT    PRIMARY KEY NOT NULL,
    project_id TEXT    NOT NULL REFERENCES projects (id),
    ord        INTEGER NOT NULL,
    -- pots_proto::Author 的 JSON；卡片與事件為 NULL。
    author     TEXT,
    -- human | agent | card | event | divider
    kind       TEXT    NOT NULL,
    body       TEXT    NOT NULL,
    -- 人類訊息：pending（還沒進 om_raw）| done；agent 訊息：streaming | done | failed
    status     TEXT    NOT NULL,
    run_id     TEXT,
    created_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX stream_items_order ON stream_items (project_id, ord);
CREATE INDEX stream_items_status ON stream_items (status);

-- 帶序號的即時事件日誌；`body` 是 pots_proto::ServerEvent 的 JSON。
CREATE TABLE events (
    seq  INTEGER PRIMARY KEY AUTOINCREMENT,
    body TEXT    NOT NULL,
    at   INTEGER NOT NULL
);

CREATE TABLE agent_runs (
    id          TEXT    PRIMARY KEY NOT NULL,
    project_id  TEXT    NOT NULL REFERENCES projects (id),
    -- profile 名稱，例如 chat
    agent       TEXT    NOT NULL,
    trigger     TEXT    NOT NULL,
    -- running | done | failed | cancelled
    state       TEXT    NOT NULL,
    error       TEXT,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE INDEX agent_runs_state ON agent_runs (state);

-- agent 的原始歷史；`item` 是 pots_llm::Item 的 JSON，依 seq 排序原樣放回模型請求。
CREATE TABLE om_raw (
    project_id TEXT    NOT NULL REFERENCES projects (id),
    seq        INTEGER NOT NULL,
    run_id     TEXT,
    item       TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, seq)
);
