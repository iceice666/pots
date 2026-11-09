-- 模型 provider 的額外 HTTP headers（proposal/agent-runtime.md#opencode-與額外-header）。
-- JSON 陣列，例如 [{"name":"x-opencode-client","value":"pots"}]。值不是秘密：
-- API key 還是在 data_dir/auth/providers/<id>.json；這裡只放路由或識別用的 header。
ALTER TABLE model_connections ADD COLUMN headers TEXT NOT NULL DEFAULT '[]';
