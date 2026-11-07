# Observational Memory

[← 產品提案索引](product.md)

## Observational Memory 的 Rust 實作

OM 是 Pots 自己的模組，不依賴 agent 框架的記憶介面。它和 pots-llm 的 loop 只在兩個地方接觸：

- `load`：run 開始前，把「觀察紀錄 + 未觀察的原始尾巴」組成 `Vec<pots_llm::Item>`，當作 `agent::run` 的 `history`。
- 追加：loop 每送出一個 `AgentEvent::Item`（模型輸出、工具呼叫與結果），伺服器就把它序列化寫進 `om_raw`；人類訊息在 run 開始時寫入。M0 已經這樣持久化每個 `Item`，OM 只要在寫入後叫醒背景 Observer。

追加發生在 run 的關鍵路徑上，所以壓縮必須在背景做。

### 三層結構

| 層 | 內容 | 更新時機 | 預設閾值 |
| --- | --- | --- | --- |
| 反思（Reflections） | 合併、去重後的長期觀察 | 觀察層超過閾值時，由 Reflector 重寫成新世代 | 觀察層 40k tokens |
| 觀察（Observations） | 帶日期與優先序的條目，只增不改 | 未觀察訊息超過閾值時，由 Observer 追加 | 未觀察 30k tokens |
| 原始尾巴 | 最近的原始訊息，逐字保留 | 每輪 append | 至少保留最近 12 則 |

閾值採用 Mastra 公開的預設值作為起點，全部可設定。加一個安全闥 `block_after` （例如 60k）：如果背景 Observer 跟不上，`load` 會同步觀察一次作為最後手段。

### 實作骨架

```rust
use pots_llm::Item;
pub struct ObservationalMemory {
    db: Pool,
    wake: tokio::sync::mpsc::Sender<ConvId>,
    cfg: OmConfig,
}
impl ObservationalMemory {
    /// run 開始前呼叫；回傳值就是 agent::run 的 history（觸發訊息由呼叫端接在最後）
    pub async fn load(&self, conv: &ConvId) -> Result<Vec<Item>> {
        let mut snap = self.snapshot(conv).await?;
        if snap.unobserved_tokens > self.cfg.block_after {
            self.observe_now(conv).await?; // Observer 跟不上時的最後手段
            snap = self.snapshot(conv).await?;
        }
        // 穩定的記憶區塊放最前面，和 preamble 一起形成可快取的前綴
        let mut items = vec![Item::User { text: snap.render_memory_block() }];
        items.extend(snap.unobserved_raw); // om_raw 裡未觀察的 Item，原樣還原
        Ok(items)
    }
    /// 每個 AgentEvent::Item 寫進 om_raw 之後呼叫
    pub async fn append(&self, tx: &mut Tx, conv: &ConvId, run: RunId, item: &Item) -> Result<()> {
        insert_raw(tx, conv, run, serde_json::to_string(item)?).await?;
        let _ = self.wake.try_send(conv.clone()); // Observer 自己判斷是否到閾值
        Ok(())
    }
}
```

未觀察尾巴的切點不能落在工具呼叫與它的結果中間；切點前的呼叫與結果一起進觀察，送給模型的歷史才不會有懸空的 `ToolCall`。永久對話沒有 clear。

### Observer 與 Reflector

1. 每個 project 一個背景 tokio task，收到 wake 後計算未觀察 token 數。

2. 超過閾值時，取出「未觀察且不在最近 12 則內」的訊息，連同現有觀察一起送給 Observer 模型。

3. Observer 輸出固定格式：日期`·`優先序（高`/`中`/`低）`·`一句話，另外附上「目前在做什麼」一行。寫入 `om_observations` ，並把對應的 `om_raw` 標為 `observed` 。

4. 觀察層超過閾值時跑 Reflector，產生新的 `generation` 。舊世代保留作稽核，不刪除。被釘選的觀察一定保留。

5. 同一片訊息只觀察一次：以 `(project_id,`最後一則`seq)` 作為冪等鍵，重啟後也不會重複計費。

Observer 與 Reflector 用 Sonnet、Luna 或 Flash 級別的模型：便宜、快速、context 夠長。模型在設定檔中可以換，和對話 agent 的模型分開。

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

