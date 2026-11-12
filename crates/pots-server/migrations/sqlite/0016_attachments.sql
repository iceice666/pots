-- 圖片的附件（proposal/ux-and-frontend.md 的圖片輸入）。
--
-- 只有中介資料：本體落在 `<data_dir>/attachments/<sha256>`，這裡只存參照，
-- **不要**把 base64 或 bytes 寫進來（訊息流、事件日誌與 om_raw 都會跟著爆量）。
CREATE TABLE attachments (
    id         TEXT    PRIMARY KEY NOT NULL,
    project_id TEXT    NOT NULL REFERENCES projects (id),
    -- 本體的 sha256（hex）；檔名就是它，同一張圖只落地一份。
    sha256     TEXT    NOT NULL,
    -- image/png | image/jpeg | image/webp | image/gif
    mime       TEXT    NOT NULL,
    bytes      BIGINT  NOT NULL,
    -- 像素寬高；解不出來時是 NULL。
    width      INTEGER,
    height     INTEGER,
    -- 使用者原本的檔名（拖放、選檔時有；貼上的圖沒有）。
    filename   TEXT,
    created_at BIGINT  NOT NULL
);

-- 同一個 project 內同一張圖只留一列（上傳第二次回同一個 id）。
-- 不同 project 各自一列：attachments 屬於 project，不跨 project 共用 id。
CREATE UNIQUE INDEX attachments_dedup ON attachments (project_id, sha256);
