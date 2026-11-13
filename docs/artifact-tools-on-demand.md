# 設計：artifact 工具按需載入

狀態：第一階段已實作（`artifact_tools::offered` / `load` / `load_for` / `load_all`）

## 現況與量測

每個 agent 回覆的第一個請求由 `Agents::request`（`crates/pots-agent/src/agent.rs`）組出，工具清單在這時就決定好，整個回覆的多步 loop（`Reply::run`）裡不再變動。篩選只看角色與設定，沒有按需載入。

用 `ToolSpec` 序列化成 JSON 量出的大小（約 4 bytes ≈ 1 token）：

| 群組 | 工具數 | bytes | ≈ tokens | 誰拿到 |
| --- | ---: | ---: | ---: | --- |
| 聊天（`pass`、`list_channels`、`read_channel`、`ask_owner`） | 4 | 2,077 | 520 | 所有 agent |
| artifact（`artifact_tools::specs()`） | 19 | 14,502 | 3,630 | 所有 agent |
| 協調（`start_task`、`post_in_thread`、`check_merge_conflicts`、`record_decision`） | 4 | 3,277 | 820 | 沒有 workspace 的 agent |
| `task_done` + `ask_worker` | 2 | 1,623 | 400 | 視情況 |
| sandbox（write 模式） | 5 | 2,100 | 525 | 有 workspace 的 agent |

協調型 agent 每次送約 5K tokens 的工具定義，其中 **artifact 工具佔約 73%**。whiteboard 一類就有 4 個工具、約 1.2K tokens。

### 這件事值多少

- **錢：不多。** Anthropic 路徑有 `cache_control`，工具定義在快取前綴最前面，重複送的成本大多是 cache read。一般經驗是工具定義超過約 10K tokens，延後載入才明顯划算；我們只有約 5K。
- **context 與注意力：有感。** 大部分對話用不到 board、whiteboard，卻每一步都要讀 17 個寫入工具的 schema。工具越少，選錯工具、亂用參數的機會越小。
- **代價：**
  1. 要寫 artifact 時多一次往返（可以靠平行呼叫抵掉大半，見下文）。
  2. 一次回覆中途載入後，該回覆剩下的請求要重寫一次快取前綴（tools → system → history）。
  3. 實作與測試的複雜度。

結論：值得做，但重點是讓 agent 專注，不是省錢。做得很小即可。

## 設計（第一階段，與 provider 無關）

### 1. 永遠載入的部分

`list_artifacts`、`read_artifact` 保留在 `tools::specs()`。worker 的 `CHAT_TOOLS` 本來就只用這兩個，而且 agent 也是靠它們得知頻道裡有哪些 kind。

### 2. 17 個寫入工具換成一個載入工具

新增 `use_artifact_tools`：

```json
{
  "name": "use_artifact_tools",
  "input_schema": {
    "type": "object",
    "properties": {
      "kinds": {
        "type": "array", "minItems": 1,
        "items": {"type": "string", "enum": ["table", "document", "checklist", "board", "whiteboard"]}
      }
    },
    "required": ["kinds"],
    "additionalProperties": false
  }
}
```

- 用陣列，一次可以載好幾種 kind。
- description 要用一句話說明每種 kind 適合放什麼（目前這些資訊散在各個 `create_*` 的描述裡），並明說：「要改一個 artifact 時，和 `read_artifact` 在同一步一起呼叫」。這樣最常見的「先讀再改」流程不會多一次往返。
- 每種 kind 回傳一行，列出現在可用的工具名，例如「You can use the table tools now: create_table, update_table, add_table_rows.」。
- 只要有一個 kind 名稱不對，就整個失敗、一個都不加。

`artifact_tools` 裡的函式：

- `specs()`：仍是全部 19 個 artifact 工具，`ALL` 不變。
- `offered()`：agent 一開始拿到的：兩個讀取工具加 `use_artifact_tools`。`agent::tools::specs()` 改接這個。
- `kind_of(name) -> Option<ArtifactKind>`：寫入工具屬於哪個 kind。

### 3. 在 `Reply::run` 裡處理

跟 `ASK_WORKER`、`CHECK_MERGE_CONFLICTS` 一樣，在 reply loop 本身處理，不經過 host：

- 把該 kind 的 specs **附加到 `req.tools` 尾端**，已經在的就跳過。`req` 本來就是 `mut`。
- `activity` 加一個標籤，例如「Getting table tools…」。

**呼叫了尚未載入的寫入工具時**：工具名會出現在別的描述裡，模型可能直接呼叫。目前 `chat.rs` 的 `artifact_tools::is_tool` 判斷仍會把它跑起來。規則定為：**自動載入該 kind，照常執行這次呼叫**。這樣下一個請求就帶著它的定義，不用依賴各家 API 怎麼處理「歷史裡有 `tool_use`，`tools` 卻沒有定義」。

### 4. 範圍是一次回覆

載入只在這次回覆內有效，下一則回覆從 `Agents::request` 的基本清單重新開始。

這樣做是安全的：`history()` 只產生純文字的 turn，跨回覆的歷史裡沒有任何 `tool_use` block，不會有指向未載入工具的呼叫。

快取行為：

- 載入後，該回覆剩下的請求要重寫一次前綴。
- 下一則回覆的基本請求和之前的完全相同，會重新命中先前的快取（在 TTL 內）。

### 5. Claude Code 路徑不變

`claude_code::run` 用 `request.tools` 建 MCP server：

- `initialize` 回 `listChanged: false`；
- `tools/call` 會拒絕清單外的名字。

所以回覆中途載入在這條路徑上行不通。`claude_code::run` 建 MCP server 前呼叫 `artifact_tools::load_all`，拿掉 `use_artifact_tools`、補上全部寫入工具。放在這裡而不是 `Agents::request`，任何走 Claude Code 的請求都會經過。MCP 工具太多時，Claude Code 會不會自己延後載入，還沒確認。

只有拿到 `use_artifact_tools` 的請求才會按需載入。worker 只有讀取工具，直接呼叫寫入工具也不會被加進清單。

### 6. 測試

- 改：`agent.rs` 的 `history_alternates_roles_and_names_people`（工具數 27 → 11）、`workspace_agents_get_tools_and_a_task_per_thread`（名稱清單）；`artifact_tools.rs` 的 `every_agent_gets_the_tools_and_the_ui_says_what_they_do`。
- 新增 `reply.rs`（三種 wire format 都跑）：
  - `artifact_writing_tools_come_when_asked_for`：載入 table、checklist 後，下一個請求在原清單尾端多出這兩類，沒有 board；
  - `a_writing_tool_called_before_asking_brings_its_kind`：直接呼叫 `create_board` 會帶入 board 工具；kind 名稱有錯時一個都不加；
  - `a_reply_without_the_loader_gets_no_writing_tools`：worker 不會被加寫入工具。
- 新增 `claude_code.rs` 的 `claude_code_gets_every_artifact_tool_up_front`：MCP `tools/list` 有全部寫入工具、沒有 `use_artifact_tools`。
- `chat.rs` 的 UI 測試直接呼叫工具，不經 reply loop，不受影響。

## 之後可以做的

- **合併同一 kind 內的工具**（第一階段之後，或取代它）：例如 whiteboard 的 add / update / remove 合成一個 `edit_whiteboard{add, update, remove}`，`update_*` 和 `add_*` 也可以合併。這能縮小 payload，而且沒有額外往返或快取代價，可以和按需載入並用。
- **Anthropic 原生的延後載入**：
  - 在 `tools` 裡一開始就宣告全部工具並設 `defer_loading: true`；
  - 載入時在 `messages` 附加一則 `role: "system"` 訊息，帶 `tool_addition`（`tool_reference` 指名），beta header 為 `mid-conversation-tool-changes-2026-07-01`。
  - 這樣前綴不變，快取不會失效。
  - 需要 `pots-llm` 支援對話中途的 system 訊息和 beta header，只適用 Anthropic API，所以放在第一階段之後。
- **不採用伺服器端 tool search**（regex / BM25）：只有五種固定 kind，用 enum 比搜尋精準，總量也低於划算的門檻。
- OpenAI Responses / Chat Completions 有沒有對應機制，這份設計沒有查證。第一階段只改我們送出的 `tools` 陣列，三種 wire format 都適用。
