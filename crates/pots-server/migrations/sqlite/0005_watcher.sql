-- M3 Watcher 與等待、自我排程與 listener（proposal/events-and-automation.md）。
-- 寫法同 0001：TEXT ID、BIGINT 時間（unix 秒）、JSON 存 TEXT。

-- 排程與 listener 送給對話 agent 的系統觸發：對話流裡顯示的是一則 Event，給 agent 的文字存在這裡，
-- run 開始時（或當機重啟時）才寫進 om_raw，和人類訊息一樣。
ALTER TABLE stream_items ADD COLUMN trigger_text TEXT;

-- project 監看的 repo，例如 `github.com/owner/repo`。
CREATE TABLE project_repos (
    project_id TEXT   NOT NULL REFERENCES projects (id),
    repo       TEXT   NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (project_id, repo)
);

CREATE INDEX project_repos_repo ON project_repos (repo);

-- 外部來源。M3 只有 GitHub（`github:personal`）。秘密（token、webhook secret）不放這裡，
-- 存在資料目錄的 `auth/sources/<id>.json`。
CREATE TABLE sources (
    id         TEXT    PRIMARY KEY NOT NULL,
    -- github
    kind       TEXT    NOT NULL,
    name       TEXT    NOT NULL,
    -- 非秘密設定（例如 API base URL）的 JSON
    config     TEXT    NOT NULL,
    -- owned | members | public：這個來源的預設信任等級（個別事件可能更低）
    trust      TEXT    NOT NULL,
    -- 接收方式，依偏好排序的 JSON 陣列：["push", "poll"]
    ingest     TEXT    NOT NULL,
    -- 輪詢進度的 JSON（每個 repo 的 ETag 與已看過的事件）
    cursor     TEXT    NOT NULL DEFAULT '{}',
    -- 健康狀態的 JSON
    health     TEXT    NOT NULL DEFAULT '{}',
    enabled    BIGINT  NOT NULL DEFAULT 1,
    created_at BIGINT  NOT NULL,
    updated_at BIGINT  NOT NULL
);

-- 正規化後的外部事件。`dedupe_key` 讓 webhook 與輪詢收到同一個事件時只留一筆。
CREATE TABLE external_events (
    id            TEXT   PRIMARY KEY NOT NULL,
    source_id     TEXT   NOT NULL,
    dedupe_key    TEXT   NOT NULL,
    -- 例如 change.merged、ci.finished
    kind          TEXT   NOT NULL,
    resource      TEXT   NOT NULL,
    trust         TEXT   NOT NULL,
    -- ExternalEvent 的 JSON
    payload       TEXT   NOT NULL,
    occurred_at   BIGINT NOT NULL,
    received_at   BIGINT NOT NULL,
    -- 這個事件叫醒或送去的對象：{"tasks": [...], "goals": [...], "listeners": [...]}
    matched       TEXT   NOT NULL DEFAULT '{}'
);

CREATE UNIQUE INDEX external_events_dedupe ON external_events (dedupe_key);
CREATE INDEX external_events_resource ON external_events (resource, received_at);

-- webhook 的 delivery ID（`X-GitHub-Delivery`）：同一次投遞重送時直接略過。
CREATE TABLE source_deliveries (
    source_id   TEXT   NOT NULL,
    delivery_id TEXT   NOT NULL,
    at          BIGINT NOT NULL,
    PRIMARY KEY (source_id, delivery_id)
);

-- 任務或 Goal 訂閱的外部資源，例如 `commit:github.com/o/r@<sha>`、`change:github.com/o/r#42`。
-- 任務的訂閱在 wait_for 時寫入；Goal 的訂閱是 listener 開 Goal 時寫入，之後同一個資源的事件
-- 都送回這個 Goal。
CREATE TABLE subscriptions (
    id           TEXT   PRIMARY KEY NOT NULL,
    project_id   TEXT   NOT NULL REFERENCES projects (id),
    task_id      TEXT   REFERENCES tasks (id),
    goal_id      TEXT   REFERENCES goals (id),
    resource_key TEXT   NOT NULL,
    created_at   BIGINT NOT NULL
);

CREATE INDEX subscriptions_resource ON subscriptions (resource_key);
CREATE UNIQUE INDEX subscriptions_task ON subscriptions (task_id, resource_key) WHERE task_id IS NOT NULL;
CREATE UNIQUE INDEX subscriptions_goal ON subscriptions (goal_id, resource_key) WHERE goal_id IS NOT NULL;

-- agent 自己設定的單次或週期喚醒。
CREATE TABLE schedules (
    id           TEXT   PRIMARY KEY NOT NULL,
    -- chat | planner | global
    owner_kind   TEXT   NOT NULL,
    -- global 為 NULL
    project_id   TEXT   REFERENCES projects (id),
    -- planner 才有
    goal_id      TEXT   REFERENCES goals (id),
    -- ScheduleSpec 的 JSON
    spec         TEXT   NOT NULL,
    prompt       TEXT   NOT NULL,
    -- 誰建立的（Principal 的 JSON）
    created_by   TEXT   NOT NULL,
    -- 下一次觸發；停用或已結束時仍保留最後的值
    next_at      BIGINT NOT NULL,
    -- 週期排程預設 30 天後過期；單次排程為 NULL
    expires_at   BIGINT,
    enabled      BIGINT NOT NULL DEFAULT 1,
    -- 單次排程觸發後、或已過期時設定，之後不再觸發
    finished     BIGINT NOT NULL DEFAULT 0,
    created_at   BIGINT NOT NULL,
    updated_at   BIGINT NOT NULL
);

CREATE INDEX schedules_due ON schedules (enabled, finished, next_at);

-- 符合條件的外部事件自動開工的規則。
CREATE TABLE listeners (
    id                 TEXT   PRIMARY KEY NOT NULL,
    project_id         TEXT   NOT NULL REFERENCES projects (id),
    -- EventFilter 的 JSON
    filter             TEXT   NOT NULL,
    -- ListenerAction 的 JSON
    action             TEXT   NOT NULL,
    -- 每次觸發的預算（M6 才執行）
    budget_per_trigger BIGINT,
    max_per_hour       BIGINT NOT NULL,
    -- active | disabled | pending（等你核准，M5 之前以「啟用」代替核准）| throttled（超過每小時上限而暫停）
    state              TEXT   NOT NULL,
    created_by         TEXT   NOT NULL,
    created_at         BIGINT NOT NULL,
    updated_at         BIGINT NOT NULL
);

CREATE INDEX listeners_project ON listeners (project_id);

-- 排程與 listener 每次觸發的紀錄（自動化頁的歷史、每小時上限）。
CREATE TABLE automation_triggers (
    id          TEXT   PRIMARY KEY NOT NULL,
    schedule_id TEXT,
    listener_id TEXT,
    project_id  TEXT,
    goal_id     TEXT,
    event_id    TEXT,
    title       TEXT   NOT NULL,
    -- done | failed | notified | skipped（暫停期間略過、超過上限；不算進每小時上限）
    status      TEXT   NOT NULL,
    error       TEXT,
    at          BIGINT NOT NULL
);

CREATE INDEX automation_triggers_schedule ON automation_triggers (schedule_id, at);
CREATE INDEX automation_triggers_listener ON automation_triggers (listener_id, at);

-- 批次暫停：`scope` 是 project ID，或 `*` 表示所有 project（含全域排程）。
CREATE TABLE automation_pauses (
    scope      TEXT   PRIMARY KEY NOT NULL,
    -- 自動恢復的時間；NULL 表示要手動恢復
    until      BIGINT,
    -- 誰暫停的（Principal 的 JSON）
    paused_by  TEXT   NOT NULL,
    at         BIGINT NOT NULL
);
