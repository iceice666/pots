-- 模型 provider 設定（API provider；內建的 ChatGPT 方案不是一列）。
-- 寫法同 0001：TEXT ID、BIGINT 時間。API key 不進資料庫，存在
-- `data_dir/auth/providers/<id>.json`（0600），`secret_ref` 是相對於資料目錄的路徑。
CREATE TABLE model_connections (
    id         TEXT   PRIMARY KEY NOT NULL,
    user_id    TEXT   NOT NULL,
    -- pots_proto::ProviderKind 的變體名稱，例如 OpenAiCompatible
    kind       TEXT   NOT NULL,
    name       TEXT   NOT NULL,
    base_url   TEXT   NOT NULL,
    model      TEXT   NOT NULL DEFAULT '',
    secret_ref TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE INDEX model_connections_user ON model_connections (user_id, created_at);

-- 實例層級的設定（key/value）。目前有：
-- - active_provider：使用中的 provider ID；空字串表示刻意清空（沒有這列表示還沒設定過）。
-- - chatgpt_model：ChatGPT 方案指定的模型；空字串表示自動選。
CREATE TABLE instance_settings (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
