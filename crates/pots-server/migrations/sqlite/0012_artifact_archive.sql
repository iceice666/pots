-- 封存的 artifact（對話 agent 申請、你核准）：從 project 列表與快照消失，資料與版本紀錄仍留著。
-- NULL 表示沒有封存。
ALTER TABLE artifacts ADD COLUMN archived_at BIGINT;
