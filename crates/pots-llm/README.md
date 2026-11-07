# pots-llm

Pots 的模型推論層：ChatGPT Plan（Sign in with ChatGPT，SIWC）授權、OpenAI Responses 串流 provider，以及所有 agent profile 共用的多輪工具迴圈。設計見 [`proposal/agent-runtime.md`](../../proposal/agent-runtime.md)。

## 模組

- `provider`：與模型無關的 `Item`、`ModelRequest`、`StreamEvent`、`LlmError` 與 `ModelProvider` trait。伺服器把每個 `Item` 原樣存進 `om_raw`。
- `responses`：`POST /v1/responses` 的 provider。請求一律 `store: false`、`stream: true`，指示放 `instructions`，工具放在 `pots` namespace，不送 SIWC 不支援的參數；只有 `response.completed` 算成功。`list_models` 讀 `GET /v1/models`。
- `siwc`：瀏覽器登入（PKCE、state、nonce，callback 固定 `http://127.0.0.1:<port>/auth/callback`）、第一次登入時動態註冊並沿用 issued client ID、ID token 驗證、單一 refresh owner、登出撤銷。憑證存在 `0600` 的 JSON 檔，原子寫入；host ID 另存。
- `agent`：`agent::run`，一輪一次串流；工具呼叫先經 `Hook` 把關（放行、拒絕、執行後結束本輪、改成核准後結束本輪）。
- `testing`（feature `test-util`）：腳本化的 `ScriptedProvider` 等，給其他 crate 的測試用。

## 驗證

```sh
cargo test -p pots-llm      # 單元測試，加上假的 OIDC 與 Responses server 的整合測試
```

### 用真的帳號手動測試

自動化測試只對假的 server。用真的 ChatGPT 帳號測登入與推論：

```sh
cargo run -p pots-llm --example siwc_smoke -- ./target/siwc-smoke              # 登入 → 列模型 → 用第一個模型
cargo run -p pots-llm --example siwc_smoke -- ./target/siwc-smoke <模型 slug>  # 指定模型
cargo run -p pots-llm --example siwc_smoke -- ./target/siwc-smoke --logout     # 撤銷並清掉 token
```

會用 `open` 開瀏覽器登入，接著串流一句問候，再跑一次 `get_time` 工具往返。不會印出任何 token。資料目錄裡的 `host-id` 與 `chatgpt-credentials.json` 之後可以刪掉。
