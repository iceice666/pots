# Phase 1：骨架

## 1. 目標

以 Agent Host 內建的 hub 為基礎，建立「人類 + 多個 agent」的持續頻道，並提供多機部署的 Agent Host 參考部署；金鑰連線驗證與失控防護自本階段起即具備。
本階段的 agent 僅能發言、邀請、回應表情與保持沉默，尚不能操作機器。
P1 僅作測試驗證，不投入日常使用；資料庫可暫不加密，後續整檔加密規劃不變。整合測試直接使用真實 API，不以 mock 回覆迴圈作為前置；mock 仍可用於單元測試。

## 2. 範圍

**納入範圍**

- `proto`：`Message`、`MemberId` / `ChannelId`、Ed25519 簽章；訊息含 `msg_id` 與 `sent_at` 以防重播
- 連線驗證：裝置與 host 之間的雙向 challenge-response；client 首次連線時核對由主機端取得的 host 指紋並釘選，之後指紋改變即拒絕連線
- 首台 Owner bootstrap：裝置私鑰在 client 產生並保留，僅匯出公鑰；由主機端本機管理 CLI 手動登記公鑰至 Owner 的裝置註冊表，並輸出 host 指紋供 client 核對；不允許首個網路連線者自動成為 Owner。後續裝置加入流程於 P3 實作
- 作者簽章：人類的訊息以裝置金鑰簽署，agent 的訊息以 agent 金鑰簽署
- Hub（Agent Host 內的模組）：連線驗證、每個頻道一個 actor 指派 `seq`、WebSocket 扇出、歷史補拉；以 SQLite 儲存。不讀取任何身分 header，不依賴來源 IP
- Agent Host 參考部署（見 README 第 3 節）：以主機的 service manager 執行（提供 NixOS module 與 systemd unit，其他平台可提供範例）、帳號 `pots`、僅綁定 `127.0.0.1`；反向代理的專用 port（例如 `:8443`），或由 hub 直接以 TLS 監聽 overlay 介面；主機防火牆僅對 overlay 介面開放該 port；pots 網域僅解析至 overlay 位址；secret 僅供 `pots` 帳號讀取
- Overlay 網路 ACL 範例（建議納入版本控制）與節點加入簽章（例如 Tailnet Lock）
- Agent Host：tokio，每個 agent 一個 actor（以 `mpsc` 作為收件匣）；可沿用既有 provider 與 agent loop 實作，但 pots 為獨立產品，沿用時仍須符合本計畫的安全邊界
- Turn arbiter：debounce、@mention、relevance gate、floor lock
- `Action`：`Say`、`Invite`（含 brief）、`React`、`StaySilent`
- 失控防護：hop limit、冷卻時間、頻道 / 全域預算、成員上限、kill switch；數值存放於設定檔
- 模型路由：`Purpose` × 四層覆寫；ZDR 非硬條件，頻道預設 `require_zdr = false`，僅於明確啟用時拒絕不符合者、不 fallback；成本依頻道 / 模型記帳
- 最小記憶：ChannelView = 最近 K 則 + 滾動摘要 + 私人筆記；僅使用 global 分區（專案分區於 P4 實作）
- 依架構文件建立 `Member` / `Role` / `MemberKind` 資料模型，僅實作 Owner
- Tauri client：**工作區切換**（多組 host URL + host 指紋 + 身分的本地設定）、頻道、thread、@ 提及、邀請 agent、預算顯示、停止鈕

**不納入範圍**：node、sandbox、稽核日誌（P2）；多裝置與備份（P3）；行動版、幕僚長與專案記憶（P4）；connector；多人功能、工作區邀請流程（P5）。

### 2.1 既有實作重用來源

重用來源為 GitHub [`mozufu/pirc`](https://github.com/mozufu/pirc)，Git URL 為 `https://github.com/mozufu/pirc.git`；先前本機唯讀檢查的 HEAD 為 `e34586b23be9e26619d3df62ca87853944b4328c`，尚未驗證該 revision 可由遠端取得。已決定 pots 採用 `AGPL-3.0-only`（見根目錄 `LICENSE`），初期以固定 Git revision 依賴重用 pirc，不使用浮動 branch 或本機絕對 path；選定的完整 revision 於接入時驗證並寫入 manifest / lockfile。建議優先重用以下邊界，不整包引入 pirc 的產品層：

| 來源路徑（相對於 pirc） | 可重用內容 | pots 仍須負責 |
| --- | --- | --- |
| `crates/pirc-llm-types` | 模型、訊息、tool 與串流 DTO | 映射至 pots 自身的簽章訊息與頻道資料模型，不將 provider DTO 當作授權依據 |
| `crates/pirc-llm` | HTTP / SSE、OpenAI / Anthropic 相容 wire API、OAuth 登入與 refresh 機制；以 `Transport::run_request`、`RunRequest`、`RunOutcome` 為執行邊界 | 端點與 route 選擇、注入 HTTP / TLS client、憑證儲存與 refresh 協調、取消訊號、預算與日誌遮罩；各 wire 的 base URL 拼接規則須分別確認 |
| `crates/pirc-agent-loop` | runtime-neutral `Loop` 與 `Step::Model` / `Step::Tools` 推進、串流 partial、取消與過期 callback fencing | Tokio actor、arbiter、ChannelView、持久化、工具驗證與簽章派送、預算、重試與實際 I/O 取消；ticket 並非 durable ID，也不保證重啟後 exactly-once |

- `crates/pirc-providers` 包含 pirc 的設定、catalog、憑證儲存、路由與 TLS 政策，且依賴 `pirc-config` / `pirc-protocol`；建議作為整合參考，而非首選整包依賴。
- `crates/pirc-agent/src/run_loop.rs` 可作 host adapter 參考，但不是完整 loop 的獨立抽離；不可照搬其對無效 tool call 的寬鬆處理來繞過 pots 的驗證。
- 三個候選 crate 尚未發布，manifest 繼承 pirc workspace 的 package / dependencies / lints；來源標示 `AGPL-3.0-only`，workspace 宣告 Rust 1.97。pots 採相同授權並以固定 Git revision 依賴；接入時須驗證所選 revision 可由 Git URL 取得、workspace 繼承可正常解析，以及所選 crate 的實際最低工具鏈需求。
- 本次僅檢查來源，未搬入程式碼、執行測試或呼叫真實 API；pirc 的測試紀錄不等於 pots 整合已驗證。

## 3. 里程碑

- [ ] M1.1 Repo 骨架、`proto` crate、訊息編解碼 + 簽章 + handshake 測試
- [ ] M1.2 Hub：首台 Owner bootstrap CLI + 連線驗證 + 頻道 seq / 扇出 / 歷史補拉
- [ ] M1.3 Agent Host 接上 hub，接入首個真實 API adapter，完成單一 agent 回覆迴圈；首次真實呼叫前具備最小預算檢查與成本記帳、輸出 token 上限、timeout 與可由測試 CLI 觸發的取消／停止機制
- [ ] M1.4 Agent Host 參考部署：service 定義、反向代理專用 port（或 hub 直接 TLS）、主機防火牆、DNS、overlay 網路 ACL 與節點加入簽章
- [ ] M1.5 擴充 LLM adapter（API：OpenAI-compatible、Anthropic-compatible，可設定端點；OAuth：Codex、Claude、GitHub Copilot；可沿用既有 provider 實作）+ 完整模型路由 + 成本記帳
- [ ] M1.6 Arbiter（debounce、mention、relevance gate、floor lock）
- [ ] M1.7 完整失控防護 + kill switch（延伸 M1.3 的最小預算與停止機制）
- [ ] M1.8 `Invite` + brief + 成員上限
- [ ] M1.9 ChannelView 與滾動摘要
- [ ] M1.10 Tauri client MVP（含工作區切換、停止鈕）

**執行依賴**：M1.3 即直接使用真實 API，不等待 M1.5 全部 adapter 完成；M1.3 的最小預算與停止機制不得延至 M1.7 才補。自動多 agent 互聊須待 M1.6 與 M1.7 完成後才啟用。

## 4. 關卡

**路線圖關卡**

- 使用者發送訊息後，agent 回覆並邀請另一個 agent 加入
- Hop limit 能阻止 agent 互聊

**補充驗收測試**

- 兩個互相 @ 的 agent 在 12 hop 內停止並 @ 使用者；預算歸零後停止呼叫；kill switch 在 2 秒內取消進行中的呼叫
- 首台 Owner 的公鑰可由主機端 CLI 登記並登入；未登記公鑰無法取得 Owner 身分；首次連線的 host 指紋與主機端提供值不符時，client 拒絕連線
- 真實 API 可完成單一 agent 回覆；最小預算耗盡或停止後不再發起新呼叫，timeout 與主動取消可中斷本地等待／串流（不保證 provider 停止計費）
- 偽造簽章與重播舊訊息皆遭拒絕
- 在 Agent Host 主機上自 loopback 直連 hub，無論附帶任何 header，皆無法通過驗證
- 自區網、公網或 Agent Host 主機上其他服務的 port（例如 `:443`）皆無法連線至 pots；僅 overlay 網路上的 `agent-host:8443` 可連線
- Host 金鑰改變時，client 拒絕連線並發出警告
- Overlay 網路的節點加入簽章已啟用（例如 `tailscale lock status`）；未經簽章的新節點無法連線至任何機器

## 5. 待決事項

- pirc 接入細節：從 `https://github.com/mozufu/pirc.git` 選定並驗證可取得的完整 revision，以及實際最低工具鏈需求；來源、授權與固定 Git revision 依賴方式已決定
- SQLCipher 與 `sqlite-vec` 能否共存（須於 P4 前確認；P1 不投入日常使用，可暫不加密）；投入日常使用前另須明確定義資料保護要求
- Relevance gate 的成本控制：先以 embedding 篩選，僅於灰色區間使用 Flash 等級模型
