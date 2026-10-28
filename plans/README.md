# Agent Workspace 開發計畫

| 項目 | 內容 |
| --- | --- |
| 文件狀態 | 草案 |
| 最後更新 | 2026-10-27 |
| 依據 | `docs/Agent Workspace 架構設計.md` 及其「開發路線 · 5 個階段、4 個關卡」圖 |

本計畫依循架構文件的設計原則：gateway 以 hub 模組形式內建於 Agent Host，多機部署的所有元件皆位於同一個私有 overlay 網路內（單機版則位於同一台機器），不採用 E2EE；身分僅來自金鑰，跨邊界核准一律須經使用者的裝置簽章。

本計畫僅描述**部署角色**，不綁定特定硬體、作業系統發行版或既有基礎設施。文中的工具名稱（例如 Tailscale、Traefik、sops、R2）為參考實作，可替換為具備相同性質的方案；可替換時以「例如」標示。術語與角色代稱沿用架構文件 1.3 節。

## 1. 階段總覽

| Phase | 主題 | 內容 | 關卡 |
| --- | --- | --- | --- |
| 1 | 骨架 | Agent Host + hub、金鑰連線驗證、作者簽章、tokio actors、arbiter、hop limit、預算、Tauri client（工作區切換）、**多機部署的 Agent Host 參考部署** | 使用者發送訊息後，agent 回覆並邀請另一個 agent 加入；hop limit 能阻止 agent 互聊 |
| 2 | 機器操控 | Linux 單機版（`node-dev`）、node daemon、label 排程、hub 的 node 佇列、用後即棄 sandbox、封鎖區網、bench device（Arduino）、git bundle 交付、capability、裝置簽章核准、`GitHubPublish` auto mode、hash-chained 稽核；後期加入獨立 Linux node（`node-linux`，多機部署） | Agent 在 `node-dev` 的 sandbox 執行指令並回報，並能燒錄 bench device；Node 與 sandbox 上不存在寫入憑證，未經簽章的跨邊界動作，以及未經簽章且不在 auto mode grant 範圍內的 GitHub push / 開 PR 遭拒 |
| 3 | 多裝置與復原 | 多裝置註冊與撤銷、recovery key、遠端加密備份 | 遠端儲存上僅有密文；新裝置能以 recovery key 復原 |
| 4 | 多專案與個人助理 | 專案記憶分區、幕僚長、macOS node（`node-mac`）、行動版、Connectors（唯讀、輪詢） | 幕僚長能處理郵件、行事曆與 LMS 的日常工作；其寫入皆由使用者執行 |
| 5 | 多人工作區 | 角色、CommandPolicy、ApprovalRule、工作區邀請與指紋、訪客 agent、成員預算 | 路線圖未列，見 Phase 5 文件的建議完成標準 |

各 phase 文件的「關卡」分為 **路線圖關卡**（路線圖所定）與 **補充驗收測試**（設計討論中補充的安全測試）。標示「待決」者為尚未定案的設計問題。

## 2. Repo 結構（建議）

```
crates/
  proto/    Message、ID、簽章、連線 handshake、SignedCommand、Approval
  core/     client 共用 core：同步、本地儲存、裝置金鑰
  policy/   Capability、authorize()、（P5）ApprovalRule
  audit/    hash-chained append-only log
agent-host/ tokio actors、arbiter、model router、memory、hub（連線驗證、頻道 seq、扇出、node 佇列、blob、推播）
node/       node daemon、sandbox driver
client/     Tauri 桌面版；（P4）UniFFI 行動版
nix/        NixOS module（多機部署、單機版）、base image；（P4）nix-darwin module（macOS 單機版）
deploy/     （選用）其他平台的服務定義與範例設定（systemd、OpenRC、反向代理範例）
```

`proto` 無需編譯為 wasm。Nix 為首個參考打包方式，但程式本身不得依賴 Nix 才能執行。

## 3. 部署角色與網路

本計畫使用以下角色代稱；實際對應的機器由部署者決定，同一台機器可兼任多個角色（單機版即為一例）。

| 角色 | 需求 | 職責 | 啟用階段 |
| --- | --- | --- | --- |
| Agent Host 主機（`agent-host`） | 常駐的 Linux 主機；不需高效能，可為小型主機或單板電腦 | **Agent Host + hub**（獨立帳號 `pots`，僅綁定 loopback，經反向代理的專用 port 僅對 overlay 網路開放） | P1 |
| 開發機 | 使用者日常使用的 Linux 機器 | 桌面 client（裝置金鑰）；**Linux 單機版：Agent Host + 第一台 node `node-dev`**（`pots-host` / `pots-node` 分離，可接 bench device） | P1（client）、P2（單機版） |
| `node-linux` | 常駐 Linux 主機，可為任意架構；建議無其他狀態，專供 agent 使用 | Node（rootless Podman），用於多機部署 | P2 後期 |
| `node-build`（選用） | 效能較高、可非全天開機的 Linux 環境（含 WSL） | 重型 build node，僅於上線時接收任務（任務於 hub 的 node 佇列等待） | P2 後期 |
| `node-mac` | Apple Silicon Mac，常開 | Node：Linux sandbox（例如 Apple `container` / OrbStack）+ macOS / Xcode（例如 Tart VM） | P4 |
| 行動裝置 | — | 行動 client（核准）；須能常駐連上 overlay 網路 | P4 |
| 遠端物件儲存 | S3 相容儲存（例如 Cloudflare R2） | 僅存放以 recovery key 加密的備份 | P3 |

- **部署模式**：多機部署與單機版並存。單機版的 hub 僅以本機 socket 監聽，不需 overlay 網路；三個 OS 身分與信任邊界見架構文件 3.6 節。以下各項適用於多機部署
- **連線**：多機部署的所有機器以私有 overlay 網路串接（參考實作：Tailscale）。Sandbox egress 除 RFC1918 / link-local 外，亦須封鎖 overlay 網路的位址範圍（見 Phase 2）
- **入口**：hub 僅綁定 loopback，經反向代理（例如 Traefik、Caddy）的**專用 port**（下文以 `:8443` 為例，可設定）對外，該 port 僅掛載 pots 的路由；主機防火牆僅對 overlay 介面開放此 port。若主機上的反向代理同時以 443 服務其他網站，不得共用 443，因其依 Host header 分流，而網路 ACL 僅能限制至 port 層級。若 Agent Host 主機上沒有其他服務，可由 hub 直接以 TLS 監聽 overlay 介面
- **網路 ACL**（建議納入版本控制）：使用者裝置 → `agent-host:8443`；node → `agent-host:8443`；node 之間、node → 其他服務一律拒絕。Overlay 網路須支援「新節點加入須經簽章」（Tailscale 為 Tailnet Lock），並予以啟用
- **認證不依賴 proxy 或 SSO**：hub 僅認可金鑰 challenge-response，不讀取任何身分 header，即使主機上已有 forward-auth / SSO 服務亦不使用。理由：同機任何程式皆可連線至 loopback 並偽造 header
- **Agent Host 與其他對外服務同機的取捨**（部署者自行評估）：同機對外服務（尤其以 root 執行、終止 TLS 的反向代理）遭攻破時，可讀取所有訊息、記憶與 connector 資料。緩解措施：獨立帳號、僅綁定 loopback、記憶庫加密、不於該主機執行 sandbox；核准仍須裝置簽章。若可行，建議以專用主機部署 Agent Host

## 4. Secret 與寫入原則

- 所有 secret 以加密形式納入版本控制（參考實作：sops / sops-nix），部署時僅解密至 Agent Host，且僅供 `pots` 帳號讀取；不設 vault
- **寫入出口採白名單**：未列入白名單的服務，token 僅具讀取權限；connector 僅提供讀取型 tool。新增出口須定義 typed 操作、憑證位置、簽章驗證者、是否適用 auto mode 與硬性限制（見架構文件 6.9 節）
- **目前唯一的出口：GitHub**（push、開 PR）。寫入 token 僅存放於 Agent Host，不進入 sandbox 或 node；使用 fine-grained token，僅限指定 repo、設到期日，權限僅 Contents 與 Pull requests 讀寫（不含 Workflows / Administration）；相關 repo 須設定 branch protection。Agent 以 `GitHubPublish` 操作請求 push 與開 PR，每次須使用者裝置簽章，或落在使用者簽署的 auto mode grant 範圍內（見架構文件 6.8、6.10 節）
- **未來的出口**（例如 SSH）：影響範圍較大者優先由 node 執行並驗證簽章，並採用每次核准時簽發的短效憑證
- **其他外部服務的寫入一律由使用者執行**（寄信、行事曆、繳交作業、deploy）。Agent 負責準備內容，client 提供一鍵交接
- **Agent Host 遭入侵視同全面失守**（已接受）。損害上限由「除 GitHub 外不存在寫入憑證」保證：入侵者可讀取資料、消耗 LLM 預算、於 sandbox 內執行指令，並可在 GitHub token 涵蓋的 repo 上 push 與開 PR（GitHub 寫入的簽章由 Agent Host 驗證，此時無法提供保護），但無法寫入其他外部服務
- Prompt injection 仍由 Agent Host 的程式碼（而非 LLM）阻擋：對 node 的跨邊界動作（區網、host 檔案系統）一律須使用者的裝置簽章；GitHub push / 開 PR 須裝置簽章，或經 auto mode 處理，此時 grant 範圍與 `HARD_DENY` 由 publisher 的程式碼檢查，reviewer 僅能在其範圍內核准
- **已知例外**：部分 LMS（例如 Moodle）的個人 token 本身可能具備繳交作業的權限，僅能靠 MCP 不暴露寫入函式（軟體層限制）；APNs / FCM 金鑰僅能對使用者的 app 發送空通知

## 5. 跨階段決策

| # | 議題 | 狀態 | 內容 | 階段 |
| --- | --- | --- | --- | --- |
| 1 | Gateway 位置 | 已決定 | Hub 為 Agent Host 內的模組，P1 即提供多機部署的參考部署 | P1 |
| 2 | 核准防重播 | 已決定 | `SignedCommand` 簽章須涵蓋 `node_id`、`nonce`、`expires_at`；node 記錄已使用的 nonce | P2 |
| 3 | 跨邊界動作的表示方式 | 已決定 | 以 typed enum 表示（`LanAccess`、`HostRead`），政策僅比對 enum，不比對 shell 字串 | P2 |
| 4 | E2EE 與訊息簽章 | 已決定 | 不採用 E2EE；連線主體身分由雙向 challenge-response 保證；作者簽章保留，供 node、稽核與 P5 訪客 agent 驗證 | P1 |
| 5 | Coding loop 位置 | 已決定 | 於 Agent Host 執行，tool call 以簽章的 `SandboxOp` 經 hub 送至 node | P2 |
| 6 | Secret 管理 | 已決定 | 不設 vault；寫入出口採白名單，目前僅 GitHub；其他寫入由使用者執行（見第 4 節） | 全階段 |
| 7 | 自動核准（auto mode） | 已決定 | 僅限 `GitHubPublish`：使用者簽署的 `AutoModeGrant`、publisher 強制檢查的 `HARD_DENY`、reviewer 模型；node 跨邊界動作不適用；統計簽章與自動核准次數 | P2 |
| 8 | 程式碼交付 | 已決定 | Git bundle 進（Agent Host fetch）、git bundle 出（經 hub 送回 Agent Host）；使用者於 client 預覽 diff 並簽章 `GitHubPublish`（或符合 auto mode grant）後，由 Agent Host push 並開 PR | P2 |
| 9 | 第一台 node | 已決定 | Linux 單機版的 `node-dev` 為 P2 第一台 node；`node-linux` 於 P2 後期加入（多機部署）；`node-mac` 於 P4 加入 | P2 |
| 10 | gVisor | 延後評估 | P2 先採用 rootless Podman，實際評估逃逸風險後再決定是否加入 | P2 之後 |
| 11 | 公開入口 | 待決（建議採用） | 不開放任何公開入口：郵件 / 行事曆以增量同步輪詢，不使用 webhook 或公開 tunnel；行動裝置一律經 overlay 網路連線至 host | P4 |
| 12 | 既有實作重用 | 已決定 | pots 為獨立產品，可沿用既有 provider 與 agent loop 實作；來源為 GitHub [`mozufu/pirc`](https://github.com/mozufu/pirc)（`https://github.com/mozufu/pirc.git`），候選 crate 與整合邊界見 Phase 1 §2.1；沿用時仍須符合 pots 的身分、核准與 sandbox 邊界；pots 採 `AGPL-3.0-only`，初期以固定 Git revision 依賴重用 | 全階段 |
| 13 | 單機版 | 已決定 | 與多機部署並存，為獨立工作區；同一套程式碼與協定；`pots-host`、`pots-node`、使用者三個 OS 身分；Linux 於 P2（參考打包：NixOS module）、macOS 於 P4 | P2、P4 |
| 14 | 開發板 | 已決定 | 登記於 node 本地政策檔的 bench device 自動附加至 sandbox；於 sandbox 內以 `arduino-cli` 燒錄與讀寫 serial；僅傳入 tty；板子變磚為已接受風險 | P2 |
| 15 | Provider API 與 ZDR | 已決定 | API 支援 OpenAI-compatible 與 Anthropic-compatible 端點；ZDR 非硬條件，頻道預設不要求，僅於明確啟用 `require_zdr` 時強制檢查；OAuth 登入方式保留 | P1 起 |
| 16 | P1 使用定位 | 已決定 | P1 僅作測試驗證，不投入日常使用；資料庫可暫不加密，後續整檔加密規劃不變 | P1 |
| 17 | 首台 Owner bootstrap | 已決定 | 由主機端本機管理 CLI 手動登記首台裝置公鑰，client 核對主機端提供的 host 指紋後釘選；裝置私鑰不離開 client，不允許首個網路連線者自動成為 Owner | P1 |
| 18 | P1 API 測試方式 | 已決定 | M1.3 直接以真實 API 測試，不以 mock 作為整合前置；首個 adapter、最小預算與停止機制於 M1.3 就緒，完整 provider 與防護分別於 M1.5、M1.7 擴充 | P1 |
