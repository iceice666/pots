# Artifacts

[← 產品提案索引](product.md)

## Artifacts：人與 agent 共同編輯的文件

長內容（spec、計畫、進度帳本、報告）不寫在對話裡，而是寫成 artifact：一份掛在 project 或 Goal 上的 Markdown 文件，你與 agent 都能直接編輯。agent 寫入前一定先讀你的修改，不會蓋掉。

### 種類

| 種類 | 誰建立 | 用途 |
| --- | --- | --- |
| Spec | 對話 agent | 討論功能時的產出：需求、決定、未決問題 |
| Brief | 對話 agent（`create_goal` 時） | Goal 交接給 Planner 的內容；取代 `goals` 表上的 brief 欄位 |
| Ledger | Planner | 任務帳本：拆了哪些子任務、各自結果、待辦 checklist |
| Report | Worker 或 Planner | 調查結果、多個方案的比較與建議 |
| Project Notes | 你（agent 只能提案） | 跨 Goal 的專案知識，放進所有 profile 的 preamble |

### 編輯協定

```rust
pub enum ArtifactKind { Spec, Brief, Ledger, Report, ProjectNotes }
pub enum Edit {
    ReplaceSection { heading: String, body_md: String }, // 以標題區段為單位
    AppendSection  { heading: String, body_md: String },
    ReplaceAll     { body_md: String },                  // 只在新建或你同意時使用
}
pub enum WriteResult {
    Written  { rev: u32 },
    Conflict { current_rev: u32, human_changes: String }, // 你之前改過的 diff
}
// 寫入一定帶 base_rev；期間有人改過就退回，讓 agent 先吸收再重送
pub fn write(a: &Artifact, base_rev: u32, edits: Vec<Edit>) -> WriteResult {
    if a.rev != base_rev {
        WriteResult::Conflict { current_rev: a.rev, human_changes: a.diff_since(base_rev) }
    } else {
        WriteResult::Written { rev: a.rev + 1 }
    }
}
```

- 讀先於寫。 `read_artifact` 回傳全文，加上「自這個 agent 上次讀取以來人類的修改」的 diff，讓 agent 明確知道你改了什麼。

- 以區段編輯。 agent 預設只替換或新增某個標題區段，減少和你同時編輯時的衝突。

- 你的編輯就是訊號。 你改了某個 Goal 的 brief，平台自動發出 steer 事件喚醒 Planner；你在 ledger 裡取消勾選某項，Planner 下一輪會看到。

- 版本。 每次修改寫入 `artifact_revisions` ，可以看 diff、還原到任一版。

### M2 的實作

- 寫入在 `crates/pots-server/src/artifacts.rs`，和 `ArtifactChanged` 事件在同一個 transaction。agent 的編輯是 `replace_section`、`append_section`、`replace_all`（只能在新建時用）；`body_md` 裡不能再有 `## ` 標題。`base_rev` 不是目前版本時回衝突，附上 `base_rev` 到目前的 diff 與「有沒有人類的修改」。
- `read_artifact` 依 `artifact_reads` 附上「這個 agent 上次讀取之後人類的修改」；agent 自己寫完、Planner 被叫醒時拿到的 Brief／Ledger 也算讀過。
- 你的編輯先是桌面版的草稿，按「儲存」才帶著開始編輯時的版本送出（`SaveArtifact`）。期間 agent 改過時以 `##` 區段做三方合併：你改的區段套到目前版本上，同一段兩邊改得不一樣時拒絕，草稿留在客戶端。內容有變的區段記進 `human_edits`（「你在 HH:MM 改過」）。
- 寫入都是單一 transaction，沒有「寫到一半」的狀態，所以 `agent_writing` 目前一律是空的；區段鎖定留給之後串流寫入時用。
- 版本紀錄是 `ArtifactHistory`（只回給問的連線，每一版附和前一版的 unified diff）；`RestoreArtifact` 新增一個內容等於舊版的版本。
- 你儲存或還原進行中 Goal 的 Brief 時，Planner 被 steer（正在跑的那輪中斷、重新排一個）。Project Notes 的提案流程還沒做。

### 和對話、記憶的關係

- 對話流裡只出現 `Card::Artifact` （建立、重大更新），點開在側邊面板編輯。

- artifact 內容不進 OM。OM 只記引用，例如「登入重構的 spec 在 artifact：auth-spec」，需要時用 `read_artifact` 取最新版。這和「狀態不靠記憶」是同一個原則。

- Planner 的 context 是 Brief 與 Ledger 兩份 artifact 加上任務樹快照，不讀對話。

- 桌面客戶端用 `gpui-kit-markdown-editor` 所見即所得編輯，以 Markdown 讀寫，與 artifact 的儲存格式一致（見[前端](ux-and-frontend.md#前端)）。checkbox 可以直接勾選。編輯器以 `##` 標題區段為單位：你改過的區段會標出時間；agent 正在寫入時，面板頂端顯示「agent 正在更新某段落」，那個區段暫時唯讀，其他區段照常可以編輯。

