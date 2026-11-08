-- M6a：階層預算、熔斷、trace 與「你不在期間」摘要
-- （proposal/security-and-operations.md#預算、#可觀測性）。
-- 寫法同前面的 migration：TEXT ID、BIGINT 時間、JSON 存 TEXT。

-- 總管對話與 project 共用同一套對話流、記憶與 run 儲存，以 kind 分辨；
-- project 列表與快照只帶 kind = 'project' 的列。
ALTER TABLE projects ADD COLUMN kind TEXT NOT NULL DEFAULT 'project';
-- 熔斷：一小時內用掉多少 token 就暫停這個 project 的所有 run（NULL 用預設值）。
ALTER TABLE projects ADD COLUMN hourly_budget_tokens BIGINT;
-- 熔斷中：新的 run 不開始，既有 run 在下一輪前停下；核准恢復後清掉。
ALTER TABLE projects ADD COLUMN breaker_until BIGINT;
-- 上次真的看到對話的時間（「你不在期間」摘要卡片的判斷）。
ALTER TABLE projects ADD COLUMN last_read_at BIGINT;
-- 預算追加被拒絕過：不再重複申請，Goal 停在原地等你處理。
ALTER TABLE goals ADD COLUMN budget_denied BIGINT NOT NULL DEFAULT 0;

-- 每一次 run 的用量（provider 回報的 Usage；缺少資料就維持 0 並另外標示）。
-- 熔斷與成本報表都用這幾欄算，`started_at` 是 run 的開始時間。
ALTER TABLE agent_runs ADD COLUMN input_tokens BIGINT NOT NULL DEFAULT 0;
ALTER TABLE agent_runs ADD COLUMN output_tokens BIGINT NOT NULL DEFAULT 0;
-- 這個 run 為什麼停下：budget | breaker | NULL（正常結束或工具要求）。
ALTER TABLE agent_runs ADD COLUMN stop_reason TEXT;
-- 「為什麼」面板：用的模型與 run 開始時的記憶版本（沒有記憶的 profile 是 NULL）。
ALTER TABLE agent_runs ADD COLUMN model TEXT;
ALTER TABLE agent_runs ADD COLUMN memory_revision BIGINT;
CREATE INDEX agent_runs_project_started ON agent_runs (project_id, started_at);

-- 每次模型呼叫與工具呼叫的 trace；「為什麼」面板依 run_id 列出。
-- detail 是遮蔽過的 JSON（不含憑證與完整秘密），只增不改。
CREATE TABLE run_trace (
    run_id TEXT   NOT NULL,
    seq    BIGINT NOT NULL,
    -- model | tool
    kind   TEXT   NOT NULL,
    name   TEXT   NOT NULL,
    detail TEXT   NOT NULL,
    ok     BIGINT NOT NULL DEFAULT 1,
    -- 模型輪的 token 總和（熔斷的滾動一小時用它算，工具輪是 0）。
    tokens BIGINT NOT NULL DEFAULT 0,
    at     BIGINT NOT NULL,
    PRIMARY KEY (run_id, seq)
);
CREATE INDEX run_trace_at ON run_trace (at, kind);

-- 這個 migration 之前建立的 Goal 沒有預算；補上預設值，舊的 Goal 也會受上限約束。
UPDATE goals SET budget_tokens = 400000 WHERE budget_tokens IS NULL;
