# Phase 5：多人工作區

## 1. 目標

支援實驗室等多人工作區：角色、指揮權、多位核准者、工作區邀請與 host 指紋驗證、訪客 agent，以及依成員分攤的預算。
Client 的工作區切換已於 P1 完成，資料模型亦已於 P1 預留，本階段僅需開放功能。

## 2. 範圍

- 於 Agent Host 強制執行 `Role`（Owner / Admin / Member / Guest）
- `CommandPolicy`：設定 agent 接受哪些成員的指令
- `ApprovalRule`：Auto / Initiator / AnyOf / Role / Quorum；`authorize()` 先找出核准者，再驗證簽章來自其任一台裝置
- 工作區邀請：host URL + host 公鑰指紋（指紋釘選已於 P1 實作，本階段新增邀請流程）；首次連線時核對，之後指紋改變即拒絕連線並發出警告
- 網路可達性（待決）：他人的 host 位於他人的 overlay 網路。候選方案為 overlay 網路的跨網路分享（僅分享 host 的 pots port；例如 Tailscale 機器分享），或由實驗室自行開放公開入口；無論採用何者，身分仍僅來自金鑰
- 由他人管理的 host，UI 常駐顯示「管理員可見內容」
- 多人 arbiter：relevance gate 納入「提問者是誰」、回覆時 @ 發起人、矛盾指令交由人類決定
- 稽核紀錄「哪位成員透過哪個 agent 發起」
- 訪客 agent：個人 agent 以 Guest 身分加入，僅取得 brief，不得下達 node 指令、不得邀請成員（使用者的 host 以 host 金鑰連線至對方的 hub，訪客 agent 掛載於其下，訊息附帶 agent 作者簽章）
- 費用：工作區帳戶、成員每日上限、花費計入發起人名下

## 3. 里程碑

- [ ] M5.1 工作區邀請 + host 指紋釘選
- [ ] M5.2 Role 與 CommandPolicy
- [ ] M5.3 ApprovalRule + quorum 納入 `policy`
- [ ] M5.4 多人 arbiter 行為與稽核來源
- [ ] M5.5 依成員分攤預算
- [ ] M5.6 訪客 agent

## 4. 完成標準（路線圖未列，建議）

- UI mockup 的 `#experiments` 情境：林同學透過 Mira 發起、超出成員上限 → 管理員核准 → node 執行
- Member 無法指揮僅接受管理員指令的 agent
- Host 指紋改變時，client 拒絕連線
- 個人記憶不會出現在實驗室工作區的任何 prompt 中（以 prompt log 驗證）
