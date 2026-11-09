-- M6b：封存 Goal、成本報表與單價表（proposal/security-and-operations.md#可觀測性）。
-- 寫法同前面的 migration：TEXT ID、BIGINT 時間、JSON 存 TEXT。

-- 封存的 Goal 不再出現在 project 列表與快照，但成本報表仍算得到它。
ALTER TABLE goals ADD COLUMN archived_at BIGINT;

-- 成本報表的分組維度。provider 是顯示名稱（ChatGPT、OpenAI…），billing 是
-- subscription | api | cli | unknown，pricing_provider 是 models.dev 的 provider 鍵，
-- runtime 是 builtin | claude_code。舊的 run 這幾欄是 NULL。
ALTER TABLE agent_runs ADD COLUMN provider TEXT;
ALTER TABLE agent_runs ADD COLUMN billing TEXT;
ALTER TABLE agent_runs ADD COLUMN pricing_provider TEXT;
ALTER TABLE agent_runs ADD COLUMN runtime TEXT;
-- CLI 回報的費用估計（不是帳單）；只有 Claude Code 之類會回報的 runtime 才有值。
ALTER TABLE agent_runs ADD COLUMN cost_usd REAL;

CREATE INDEX agent_runs_started ON agent_runs (started_at);
CREATE INDEX agent_runs_agent_started ON agent_runs (agent, started_at);

-- 單價表：每 100 萬 token 的美元單價。provider 是 models.dev 的 provider 鍵，
-- `*` 表示所有 provider 的後備值；`source` 記來源（models.dev 或 manual）。
CREATE TABLE model_prices (
    provider            TEXT   NOT NULL,
    model               TEXT   NOT NULL,
    input_per_mtok      REAL   NOT NULL,
    output_per_mtok     REAL   NOT NULL,
    cache_read_per_mtok REAL,
    source              TEXT   NOT NULL,
    updated_at          BIGINT NOT NULL,
    PRIMARY KEY (provider, model)
);
