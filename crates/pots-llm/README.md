# pots-llm

Pots 的模型推論層：ChatGPT Plan（Sign in with ChatGPT，SIWC）授權、OpenAI Responses 串流 provider、API key provider（OpenAI 相容、Anthropic），以及所有 agent profile 共用的多輪工具迴圈。設計見 [`proposal/agent-runtime.md`](../../proposal/agent-runtime.md)。

## 模組

- `provider`：與模型無關的 `Item`、`ModelRequest`、`StreamEvent`、`LlmError` 與 `ModelProvider` trait。伺服器把每個 `Item` 原樣存進 `om_raw`。`ModelRequest::session_id` 是這次對話的穩定 ID，provider 可以用它做 session affinity（OpenCode 的 `x-opencode-session`）。
- `responses`：`POST /v1/responses` 的 provider。請求一律 `store: false`、`stream: true`，指示放 `instructions`，不送不支援的參數；只有 `response.completed` 算成功。`ResponsesProvider::new` 給 ChatGPT Plan（SIWC），工具放在 `pots` namespace；`ResponsesProvider::with_api_key` 給 OpenAI 平台的 API key，Bearer 授權、工具平整地放在頂層 `tools`。設定 `ModelRequest::reasoning` 時送 `reasoning: {"effort": ...}`（`Off` 是 `none`），沒設定時推理交給模型預設值（Responses 允許工具與推理並用）。`list_models` 同時接受 plan 的 `models[]` 與平台的 `data[].id`。
- `openai_compat`：OpenAI 相容的 `POST {base}/chat/completions` 串流 provider（OpenAI、OpenRouter、Ollama、DeepSeek 等），`Authorization: Bearer`。base URL 要含 `/v1`；`instructions` 變成開頭的 system message，同一輪的文字與工具呼叫合成一則 assistant message。`[DONE]` 或送過 `finish_reason` 才算完成。`list_models` 讀 `GET {base}/models`。設定 `ModelRequest::reasoning` 時送 `reasoning_effort`（`Off` 是 `none`），沒設定時不送；帶工具被 Chat Completions 以「工具 + reasoning」拒絕（GPT-5.4 以後的模型預設會推理）時，改成 `reasoning_effort: "none"` 重送並記住（之後帶工具的請求一律送 `none`），連 `none` 都不接受的模型回 `Unsupported` 並提示改用 Responses API。
- `anthropic`：Anthropic Messages API（`POST {base}/v1/messages`，base 已含 `/v1` 時不重複）的 provider，`x-api-key` + `anthropic-version: 2023-06-01`。相鄰同角色的 content block 合併成一則 message；只有 `message_stop` 算成功。`thinking`／`redacted_thinking` block 存成 `Item::Opaque`，下一輪原樣放回 assistant message。設定 `ModelRequest::reasoning` 時送 `thinking: {type: "adaptive"}` + `output_config.effort`（`Off` 送 `disabled`）；adaptive 被拒時改用 `enabled` + `budget_tokens`，`disabled` 被拒時不送 `thinking`，重送成功後依模型記住。`list_models` 讀 `GET {base}/v1/models`（會分頁）。
- 三個 API key provider 都支援 `.with_headers(HeaderMap)` 與 `.with_session_header(name)`：額外 header 排在認證 header 之後，同名時可以覆寫預設值；session header 只在請求有 `session_id` 且使用者沒有同名 header 時補上。HTTP 錯誤一律對應到 `LlmError`（401／403 → `InvalidCredentials`，400／404／422 → `Unsupported`，429 → `Unavailable`，額度或帳單問題 → `Other`，5xx／529 → `Unavailable`）；API key 不會出現在 `Debug` 或錯誤訊息。
- `siwc`：瀏覽器登入（PKCE、state、nonce，callback 固定 `http://127.0.0.1:<port>/auth/callback`）、第一次登入時動態註冊並沿用 issued client ID、ID token 驗證、單一 refresh owner、登出撤銷。憑證存在 `0600` 的 JSON 檔，原子寫入；host ID 另存。
- `agent`：`agent::run`，一輪一次串流；工具呼叫先經 `Hook` 把關（放行、拒絕、執行後結束本輪、改成核准後結束本輪）。
- `testing`（feature `test-util`）：腳本化的 `ScriptedProvider` 等，給其他 crate 的測試用。

## 驗證

```sh
cargo test -p pots-llm      # 單元測試，加上假的 OIDC、Responses、Chat Completions 與 Anthropic server 的整合測試
```

### 用真的帳號手動測試

自動化測試只對假的 server。用真的 ChatGPT 帳號測登入與推論：

```sh
cargo run -p pots-llm --example siwc_smoke -- ./target/siwc-smoke              # 登入 → 列模型 → 用第一個模型
cargo run -p pots-llm --example siwc_smoke -- ./target/siwc-smoke <模型 slug>  # 指定模型
cargo run -p pots-llm --example siwc_smoke -- ./target/siwc-smoke --logout     # 撤銷並清掉 token
```

會用 `open` 開瀏覽器登入，接著串流一句問候，再跑一次 `get_time` 工具往返。不會印出任何 token。資料目錄裡的 `host-id` 與 `chatgpt-credentials.json` 之後可以刪掉。
