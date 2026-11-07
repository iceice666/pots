-- M1 Observational Memory（proposal/memory.md）。寫法同 0001：TEXT ID、BIGINT 時間，
-- 布林用 BOOLEAN（SQLite 存 0／1，Postgres 原生支援）。

-- 每筆原始歷史的估計 token 數（NULL：升級前寫入的，啟動時補算）與是否已經整理進觀察。
-- 已觀察的永遠是每個 project 最舊的一段（seq 的前綴）。
ALTER TABLE om_raw ADD COLUMN tokens BIGINT;
ALTER TABLE om_raw ADD COLUMN observed BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX om_raw_unobserved ON om_raw (project_id, observed, seq);

-- Observer 與 Reflector 產出的觀察。內容不就地改：你修正時舊列標成 superseded、新增一列
-- 指向它（`replaces`）；刪除標成 deleted。只有 `pinned`、`state` 與 `updated_at` 會就地更新。Reflector 每次重寫產生新的 `generation`，
-- 舊世代保留作稽核。送給模型的是 `om_state.generation` 那一代、`state = 'active'` 的列。
CREATE TABLE om_observations (
    id          TEXT    PRIMARY KEY NOT NULL,
    project_id  TEXT    NOT NULL REFERENCES projects (id),
    generation  BIGINT  NOT NULL,
    -- 世代內的順序；修正後的列沿用被取代那列的 seq。
    seq         BIGINT  NOT NULL,
    -- 對話發生的日期，YYYY-MM-DD。
    date        TEXT    NOT NULL,
    -- high | medium | low
    priority    TEXT    NOT NULL,
    text        TEXT    NOT NULL,
    pinned      BOOLEAN NOT NULL DEFAULT FALSE,
    -- observer | reflector | human
    source      TEXT    NOT NULL,
    -- active | superseded | deleted
    state       TEXT    NOT NULL,
    replaces    TEXT,
    -- Observer 產生的列：這一批觀察到的最後一則 om_raw 的 seq。
    through_seq BIGINT,
    created_at  BIGINT  NOT NULL,
    updated_at  BIGINT  NOT NULL
);

CREATE INDEX om_observations_current ON om_observations (project_id, generation, state, seq);

-- instance_settings 新增兩個 key（見 0002）：
-- - memory_provider：整理記憶用的 provider ID；空字串或沒有這列表示跟對話 agent 相同。
-- - memory_model：整理記憶用的模型；空字串表示用該 provider 設定的模型。

-- 每個 project 的記憶狀態。沒有這列等於第 0 代、沒有觀察。
CREATE TABLE om_state (
    project_id       TEXT   PRIMARY KEY NOT NULL REFERENCES projects (id),
    generation       BIGINT NOT NULL,
    current_task     TEXT   NOT NULL DEFAULT '',
    -- 觀察內容每變一次（Observer、Reflector、你的修正）加一；Reflector commit 前比對，
    -- 重寫期間內容被改過就丟掉這次的結果。
    revision         BIGINT NOT NULL DEFAULT 0,
    -- 上次反思後記憶的 token 數；記憶要比它多出一段才再反思，避免反思不下去時一直重跑。
    reflected_tokens BIGINT NOT NULL DEFAULT 0,
    last_error       TEXT,
    updated_at       BIGINT NOT NULL
);
