# 參考資料

[← 產品提案索引](product.md)

## 參考 UI

- 專案內的 [`ui/`](../ui/)：靜態 UI 設計稿，作為畫面配置與視覺呈現的參考，不代表功能已實作。
- [畫面總覽](../ui/index.html)與[預覽說明](../ui/README.md)。各畫面對應見[使用體驗與前端：參考 UI](ux-and-frontend.md#參考-ui)。

## 前端

- [GPUI（Zed）](https://github.com/zed-industries/zed/tree/main/crates/gpui)

- [GPUI Kit](https://github.com/longbridge/gpui-kit)

- [gpui-kit-markdown-editor（docs.rs）](https://docs.rs/crate/gpui-kit-markdown-editor/latest)

- [gpui-mobile](https://github.com/itsbalamurali/gpui-mobile)：GPUI 的 iOS／Android 平台層（實驗中）

- 專案內的 [`prototypes/gpui-editor/`](../prototypes/gpui-editor/)：Artifact 編輯器與中文輸入法測試原型

## 帳號與模型整合

下列官方文件核對日期為 **2026-11-07**。SIWC 仍為 preview，資格、限制、token 與端點協定可能改變，實作與發佈前須重新核對；這些來源支援設計方向，不代表 Pots 的登入、token refresh、Responses 串流或自架匯入已完成端到端驗證。

- [OpenAI：Token sharing for open-source apps](https://developers.openai.com/siwc/token-sharing-open-source)：開源應用的官方 SIWC token-sharing 路徑；Pots 的 ChatGPT Plan provider 由 rig 直接呼叫 Responses API，不是 Codex runtime 整合。
- [OpenAI：Sign in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in)：官方瀏覽器登入、loopback callback 與授權流程。
- [OpenAI：Preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations)：preview 適用資格與使用限制；不可把訂閱授權視為一般 API key 的無條件替代品。
- [OpenAI：Self-hosted VMs](https://developers.openai.com/siwc/token-sharing-open-source/self-hosted-vms)：本機瀏覽器授權、透過 SSH 安全匯入自架 VM，以及 VM 自己的 Host ID。
- [Claude Code：Legal and compliance](https://code.claude.com/docs/en/legal-and-compliance)：官方 CLI 與訂閱授權的使用邊界；Pots 不抽取 Claude 訂閱 token 供自建 provider 使用。
- [Claude Code：Run Claude Code programmatically](https://code.claude.com/docs/en/headless)：官方 CLI 的程式化執行介面，供 runtime adapter 參考；登入與 session storage 仍由官方 CLI 管理。

## 託管電腦（macOS）

下列為第三方整理，核對日期為 **2026-11-07**；macOS 權限行為會隨版本改變，實作前須以 Apple 官方文件與實機測試為準。

- [Der Flounder：Sequoia 15.1 的 `forceBypassScreenCaptureAlert`](https://derflounder.wordpress.com/2024/10/31/managing-user-notifications-for-apps-which-request-screen-access-on-macos-sequoia-15-1/)：螢幕錄製權限的定期重新授權提示，以及描述檔的關閉方式。
- [Breeze RMM：macOS Permissions](https://docs.breezermm.com/agents/macos-permissions/)：輔助使用可以用 MDM PPPC 授權，螢幕錄製只能由使用者手動核准。
- [Crabfleet：Screen-Recording Indicator & ScreenCaptureKit](https://docs.crabfleet.ai/screen-recording-indicator.html)：錄製指示燈不能隱藏、定期重新授權提示，以及內建 Screen Sharing 的差異。
- [macrun：Auto-login on a headless Mac](https://macrun.dev/blog/auto-login-on-a-headless-mac)：FileVault 與自動登入衝突、無人值守登入常見的失敗原因。
- [macrun：Always-on Mac for AI agents](https://macrun.dev/guides/always-on-mac-for-ai-agents)：不睡眠、自動登入、程序監管與人類接手。
- [SideMini：Mac mini without a monitor](https://sidemini.com/blog/use-mac-mini-without-monitor/)：沒有顯示器時的畫面問題，以及假螢幕與虛擬顯示器。

## 參考資料

- [rig-core Tool trait（docs.rs, 0.37）](https://docs.rs/rig-core/0.37.0/src/rig_core/tool/mod.rs.html)

- [rig-core ConversationMemory（docs.rs）](https://docs.rs/rig-core/latest/src/rig_core/memory.rs.html)

- [rig-core AgentBuilder（docs.rs）](https://docs.rs/rig-core/latest/rig_core/agent/struct.AgentBuilder.html)

- [rig-memory（docs.rs）](https://docs.rs/crate/rig-memory/latest)

- [Mastra: Announcing Observational Memory](https://mastra.ai/blog/observational-memory)

- [Mastra: Observational Memory docs](https://mastra.ai/docs/memory/observational-memory)

Artifact、自我排程、listener 與總管 agent 參考了 Amp 的設計：

- [Amp: Spaces](https://ampcode.com/news/spaces)

- [Amp: Right on Schedule](https://ampcode.com/news/schedule)

- [Amp: Meet Puck](https://ampcode.com/news/meet-puck)

- [Amp 2026.11.02 Artifacts（Havoptic）](https://www.havoptic.com/r/amp-artifacts)

- [Amp 2026.10.12 GitHub listeners（Havoptic）](https://www.havoptic.com/r/amp-github-listeners)
