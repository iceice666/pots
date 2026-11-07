# Observational Memory

[← 產品提案索引](product.md)

## Observational Memory 的 Rust 實作

OM 是 Pots 自己的模組，不依賴 agent 框架的記憶介面。它和 pots-llm 的 loop 只在兩個地方接觸：

- `load`：run 開始前，把「觀察紀錄 + 未觀察的原始尾巴」組成 `Vec<pots_llm::Item>`，當作 `agent::run` 的 `history`。
- 追加：loop 每送出一個 `AgentEvent::Item`（模型輸出、工具呼叫與結果），伺服器就把它序列化寫進 `om_raw`（順便記下估計的 token 數）；人類訊息在 run 開始時寫入。寫入後叫醒背景 Observer。

實作在 [`crates/pots-server/src/memory.rs`](../crates/pots-server/src/memory.rs)。

追加發生在 run 的關鍵路徑上，所以壓縮必須在背景做。

### 三層結構

| 層 | 內容 | 更新時機 | 預設閾值 |
| --- | --- | --- | --- |
| 反思（Reflections） | 合併、去重後的長期觀察 | 觀察層超過閾值時，由 Reflector 重寫成新世代 | 觀察層 40k tokens |
| 觀察（Observations） | 帶日期與優先序的條目，內容不就地修改（修正是新增一列取代舊的） | 未觀察訊息超過閾值時，由 Observer 追加 | 未觀察 30k tokens |
| 原始尾巴 | 最近的原始訊息，逐字保留 | 每輪 append | 至少保留最近 12 則 |

閾值採用 Mastra 公開的預設值作為起點，全部可設定（`Config::memory`，即 `OmConfig`）。加一個安全闥 `block_after` （預設 60k）：如果背景 Observer 跟不上，`load` 會同步觀察到不超過它為止，作為最後手段；同步觀察失敗時照樣用完整的尾巴，不擋住對話。一次送給 Observer 的原始歷史最多 `max_chunk`（預設 60k），再多就分幾次。

token 數用字元估算，不依賴 tokenizer：CJK（漢字、假名、諺文、全形標點）一字約一個 token，其他字元約四個一個，每筆項目另加四個。對各家 provider 都只是近似，閾值本來就是粗估。升級前寫入的 `om_raw` 在啟動時補算。

### 實作骨架

```rust
use pots_llm::Item;
pub(crate) struct Memory { /* hub、store、ProviderRouter、OmConfig、每個 project 一條 lane */ }
impl Memory {
    /// run 開始前呼叫；回傳值就是 agent::run 的 history（觸發訊息已經在尾巴裡）
    pub async fn load(&self, project: ProjectId) -> Result<Vec<Item>> {
        if unobserved_tokens(project) > self.cfg.block_after {
            self.process(project, Mode::Blocking).await; // Observer 跟不上時的最後手段
        }
        // 記憶區塊與尾巴在同一個讀取 transaction 裡讀，Observer 剛好 commit 也不會漏掉一段
        let (state, observations, tail) = read_snapshot(project).await?;
        // 穩定的記憶區塊放最前面，和 preamble 一起形成可快取的前綴
        let mut items: Vec<Item> = memory_block(&observations, &state.current_task)
            .map(|text| Item::User { text })
            .into_iter()
            .collect();
        items.extend(tail); // om_raw 裡未觀察的 Item，原樣還原
        Ok(items)
    }
    /// 寫進 om_raw 之後呼叫；背景 driver 自己判斷是否到閾值
    pub fn wake(&self, project: ProjectId);
}
```

未觀察尾巴的切點不能落在工具呼叫與它的結果中間；切點前的呼叫與結果一起進觀察，送給模型的歷史才不會有懸空的 `ToolCall`。實際的規則是切點只落在兩種邊界：下一則是人類訊息（新的 run；中斷的 run 留下的懸空呼叫也在這裡結束），或上一則是工具結果、所有呼叫都有結果、下一則不是工具結果（同一個 run 裡兩次回應之間）；所以 reasoning（`Item::Opaque`）也不會和它後面的輸出拆開。永久對話沒有 clear。

### Observer 與 Reflector

1. 每個 project 一個背景 tokio task，收到 wake 後計算未觀察 token 數。

2. 超過閾值時，取出「未觀察且不在最近 12 則內」的訊息，連同現有觀察一起送給 Observer 模型。

3. Observer 輸出固定格式：日期`·`優先序（高`/`中`/`低）`·`一句話，另外附上「目前在做什麼」一行。寫入 `om_observations` ，並把對應的 `om_raw` 標為 `observed` 。

4. 觀察層超過閾值時跑 Reflector，產生新的 `generation` 。舊世代保留作稽核，不刪除。被釘選的觀察一定保留。

5. 同一片訊息只觀察一次：以 `(project_id,`最後一則`seq)` 作為冪等鍵，重啟後也不會重複計費。實作上，commit 時只把這一段裡 `observed = FALSE` 的列標成已觀察，筆數不符就丟掉這次的結果；每條觀察記下 `through_seq`。同一個 project 的 Observer／Reflector（含 `load` 的同步觀察）用一把 async mutex 序列化。

6. Reflector 重寫期間觀察被改過（你修正了、或又有新的觀察）就丟掉這次的結果：`om_state.revision` 在每次變動時加一，commit 前比對。重寫後沒釘選的觀察只剩原本不到 1/20（包括一條都不留）時當作輸出不對、不寫入，除非沒釘選的本來就很少；全部都釘選時不反思。反思後記憶要再多出 `reflect_after` 的四分之一才再反思，避免壓不下來時每次都重寫。

7. 模型輸出認不得、請求失敗或逾時（預設 5 分鐘）時什麼都不標記；錯誤記在 `om_state.last_error`，顯示在「記憶」分頁，下次同一階段成功時清掉。Observer 與 Reflector 各自冷卻：從 `retry_after`（預設 60 秒）開始，連續失敗就加倍，最多 30 分鐘；Reflector 失敗不影響觀察。你在設定改了 provider 或記憶用的模型時清掉冷卻、馬上再試。所有 project 同時最多兩個 Observer／Reflector 請求。服務關閉時取消進行中的請求；服務啟動完成後叫醒每個 project，把沒整理完的補上。

8. 同步觀察（`load`）每一段各自拿 project 的鎖，最多等背景正在進行的那一段；run 被停止或改方向時馬上放棄等鎖與模型請求（寫資料庫的部分不會被中斷）。Observer 在冷卻中時不做同步觀察，直接用完整的尾巴。已經整理過、卻沒留下任何觀察時，歷史前面補一個空的記憶區塊，讓歷史一定從使用者訊息開始。

9. 送給 Observer 的對話紀錄是資料不是指示：每則以 `[時間] 誰：` 開頭（本機時區），內容的換行之後一律縮排，訊息或工具結果裡偽造的「使用者：」行不會出現在行首；指示也要求只把使用者那幾則當成使用者的決定。觀察與「目前在做什麼」放進記憶區塊前拿掉 `<memory>` 標籤。

### 整理記憶的模型

Observer 與 Reflector 用 Sonnet、Luna 或 Flash 級別的模型：便宜、快速、context 夠長。模型在設定視窗的「記憶」類別可以換，和對話 agent 的模型分開：選一個已設定的 provider，模型留空就用該 provider 設定的模型。沒有另外設定時跟對話 agent 用同一個 provider 與模型；刪掉記憶用的 provider 時改回跟對話相同。設定存在 `instance_settings` 的 `memory_provider` 與 `memory_model`，協定上是 `ProvidersView::memory` 與 `SetMemoryModel` 指令。

Observer／Reflector 的請求不帶工具，只有指示與一則使用者訊息（現有的觀察、目前在做什麼、要整理的對話紀錄）。輸出一行一條 `- YYYY-MM-DD · 高 · 一句話`，最後一行 `目前在做什麼：…`；認不得的行略過，一行都認不得就是失敗。

### 記憶面板

每個 project 頻道的「記憶」分頁列出目前這一代的觀察與「目前在做什麼」，可以：

- **釘選／取消釘選**：Reflector 重寫時釘選的原樣帶到新的一代。
- **修正**：舊列標成 `superseded`，新增一列（`source = human`、`replaces` 指向舊列）取代它，位置與釘選狀態不變。
- **刪除**：標成 `deleted`。

都是軟性的：舊內容留在資料庫作稽核，模型只看得到 `active` 的列，從下一次 run 開始生效。只能改目前這一代的觀察；你開著的那條剛好被重寫掉時，服務回「找不到這條觀察」。記憶每變一次，服務送一個 `Memory { project, memory }` 事件整份替換，快照裡每個 project 也帶著它。

### Context 組裝順序

1. preamble（幾乎不變）：USER.md + project 指示 + 角色指示

2. 記憶區塊：反思 + 觀察（只有 Observer 或 Reflector 跑完時才變）

3. 未觀察的原始尾巴（只會往後 append）

4. 即時狀態快照：附在本輪觸發訊息末尾，每輪重新產生

穩定的放前面、易變的放後面，prompt caching 的前綴只有在觀察更新時才失效。即時狀態絕不放進記憶區塊。

### USER.md：跨 project 的使用者偏好

跨 project 的偏好（commit 風格、語言、程式碼習慣、溝通方式）放在一份 USER.md，由你直接編輯。對話 agent、Planner 與內建 worker 都把它放在 preamble 最前面；外部 CLI 則以它們會讀取的使用者層級指示檔形式掛進容器。

- 快取友善。 USER.md 很少變動，放在前綴最前面，快取命中率最高。

- agent 只能提案。 對話 agent 發現值得長期記住的偏好時，呼叫 `propose_user_md_edit` 產生一張 diff 卡片，你核准後才寫入。

- 和 OM 分工。 OM 記「這個 project 裡發生了什麼」，USER.md 記「你是什麼樣的人」，兩者不重複。

- 儲存。 存在 `users.user_md` ，每次修改保留版本；長度超過約 2k tokens 時在 UI 提示整理。

### 什麼不進記憶

`memory_policy` 為 `Skip` 的項目（外部事件、「你不在期間」卡片）不會作為訊息送進 agent，因此也不會被 append。核准卡片只以一句結果進入：「使用者核准了 merge PR #42」。

