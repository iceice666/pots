-- Goal 的動態（proposal/ux-and-frontend.md）：Planner 的里程碑、開工、Planner 設的排程等
-- 進展只記在 Goal 底下，在 Goal 面板的「動態」分頁看；Goal 結束、需要使用者處理的才同時貼到對話流。
CREATE TABLE goal_activity (
    goal_id TEXT   NOT NULL REFERENCES goals (id),
    seq     BIGINT NOT NULL,
    text    TEXT   NOT NULL,
    at      BIGINT NOT NULL,
    PRIMARY KEY (goal_id, seq)
);

-- 之前的里程碑都貼在對話流：搬一份過來，舊 Goal 的「動態」才不是空的（對話流裡的卡片保留）。
INSERT INTO goal_activity (goal_id, seq, text, at)
SELECT goal_id,
       ROW_NUMBER() OVER (PARTITION BY goal_id ORDER BY ord),
       text,
       created_at
FROM (
    SELECT json_extract(body, '$.Card.Milestone.goal') AS goal_id,
           json_extract(body, '$.Card.Milestone.text') AS text,
           ord,
           created_at
    FROM stream_items
    WHERE kind = 'card' AND json_extract(body, '$.Card.Milestone.goal') IS NOT NULL
)
WHERE goal_id IN (SELECT id FROM goals);
