-- project 的工作資料夾（proposal/onboarding.md）：程式碼在哪、worker 在哪裡改。
-- 一個 project 只有一個；自動化分頁「監看的 repo」是外部事件來源，和這裡分開。
-- NULL 表示還沒設定（onboarding 沒做完），任務只記在任務樹上、不派工。
ALTER TABLE projects ADD COLUMN workdir_kind TEXT;
ALTER TABLE projects ADD COLUMN workdir TEXT;

-- 任務是從哪個分支開出來的（本機資料夾套用時要確認你的資料夾還在那個分支）。
ALTER TABLE tasks ADD COLUMN base_branch TEXT;
