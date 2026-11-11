-- 每個 project 有自己的負責人（對話 agent）。之前所有 project 都用全域的「Pots」，
-- 改成和新 project 相同的預設名字（`store::default_agent_name`）；總管的內部 project 不動。
UPDATE projects SET agent_name = TRIM(name) || ' 負責人'
WHERE kind = 'project' AND agent_name = 'Pots';
