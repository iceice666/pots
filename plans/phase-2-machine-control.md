# Phase 2：機器操控

## 1. 目標

Agent 透過 `StartTask` 在 Linux 單機版 `node-dev` 的 sandbox 內執行指令並回報、燒錄並讀寫 bench device，產出的 commit 以 git bundle 送回 Agent Host，經使用者裝置簽章（或符合 auto mode grant）後由 Agent Host push 並開 PR。
跨出 sandbox 的動作（區網、host 檔案系統）須經使用者的裝置簽章。Node 僅信任簽章與本地政策，且不持有任何服務 token；GitHub 寫入 token 僅存放於 Agent Host（見架構文件 6.10 節）。

Sandbox 的完整設計見 `docs/Agent Workspace 架構設計.md` 6.4 節「Sandbox 設計」；單機版見 3.6 節，bench device 見 6.11 節。

## 2. 範圍

**納入範圍**

- `SignedCommand` / `SignedApproval`：簽章涵蓋 `node_id`、`task_id`、`nonce`、`expires_at` 與 typed action；node 記錄已使用的 nonce
- Hub 的 node 佇列：指令佇列、TTL、presence、`NodeDescriptor`（SQLite）
- Linux 單機版（參考打包：NixOS module；另提供通用 systemd unit 與帳號 / 權限設定說明）：Agent Host、hub 與 node daemon 於同一台機器以不同 OS 使用者執行
  - `pots-host`（Agent Host + hub）與 `pots-node`（node daemon + rootless Podman）兩個 systemd service；桌面 client 以使用者帳號執行並持有裝置金鑰
  - Hub 以 Unix domain socket 監聽，以檔案權限限制連線者；不需 overlay 網路與反向代理
  - Node 政策檔由 root 擁有；兩個服務帳號互相無法讀取對方狀態，亦無法讀取使用者 home
  - nftables `meta skuid` 限制 `pots-node` 僅能連線至 hub 與網際網路
  - Secret 解密至僅 `pots-host` 可讀的位置（例如 sops-nix）
- Node daemon，**首先部署於 `node-dev`**（單機版），P2 後期加入 `node-linux`（多機部署；須支援 x86_64 與 arm64）：主動連線至 hub（單機版為本機 socket，多機部署為 `agent-host:8443`）、以 node 金鑰進行 challenge-response 並釘選 host 指紋、上線時拉取佇列、捨棄過期指令、回報 label 與資源
- Label 排程 `schedule()`；結果為 `Placement::Impossible` 時，agent 於頻道中說明
- Sandbox driver（rootless Podman）：
  - 一個 `StartTask` 對應一個 sandbox，用後即棄；worktree 保留 7 天
  - `--cap-drop=ALL`、`no-new-privileges`、seccomp、rootfs 唯讀，僅 `/work` 與 `/tmp` 可寫
  - 獨立 netns + nftables：僅放行網際網路；封鎖 RFC1918、link-local、multicast 與 overlay 網路位址範圍（可設定；Tailscale 為 `100.64.0.0/10`、`fd7a:115c:a1e0::/48`、`100.100.100.100`）；DNS 使用公共 resolver
  - cgroup v2 限制 CPU / 記憶體 / pids、磁碟配額、每個操作的 timeout、每個任務的總時間上限
  - 以 Nix `dockerTools` 建置 base image
- 執行模型：agent loop 於 Agent Host 執行；tool call 為經簽章的 `SandboxOp`（`Exec` / `ReadFile` / `WriteFile` / `ApplyPatch` / `Deliver`）；每個任務一條持久 exec session；輸出以串流回報、截斷並標記為不可信
- 程式碼進出：Agent Host fetch → `git bundle` → hub blob → node；交付時 node 打包 bundle → hub → Agent Host
- GitHub 寫入：Agent Host 的 publisher 模組持有 fine-grained GitHub token（僅限指定 repo、Contents 與 Pull requests 讀寫、不含 Workflows / Administration、設到期日）；agent 發出 `GitHubPublish { repo, branch, base, head, pr }` → client 預覽 `base..head` diff 與 PR 內容 → 裝置簽署 hash → publisher 驗證簽章與 bundle head 後 push 並開 PR；不支援 force push 與刪除分支
- `GitHubPublish` auto mode（見架構文件 6.8 節）：client 建立、簽署與撤銷 `AutoModeGrant`（repo、分支前綴、draft、diff 上限、每日次數、到期）；publisher 以程式碼檢查 grant 與 `HARD_DENY`；reviewer 模型僅輸出 `Approve` / `Escalate`，`Escalate` 或不符合 grant 時退回人工核准；自動核准結果寫入稽核並通知使用者；kill switch 暫停所有 grant
- 跨邊界動作（typed）：`LanAccess { host, port, ttl }`、`HostRead { paths }`，預設為 `AskHuman`
- Bench device（見架構文件 6.11 節）：
  - Node 本地政策檔登記 `BenchDevice`（`/dev/serial/by-id` 路徑、USB vendor / product / serial、label）；`NodeDescriptor` 回報 `board=...` label
  - 任務要求該 label 時自動附加，以 `--device` 僅傳入 tty；同一裝置同時只附加至一個任務
  - 以 udev 規則與 device cgroup 規則處理燒錄時的重新枚舉（DTR 重置、1200bps touch）
  - Nix base image 預載 `arduino-cli` 與常用 core（AVR、SAMD、ESP32）
  - 裝置的附加與移除寫入稽核日誌
- `policy` crate：`Capability`、`authorize()`、本地政策檔（`owner_device_keys` 僅能於 node 上手動修改）
- 桌面 client：核准畫面（呈現 agent、node、動作與理由，以裝置金鑰簽署 `cmd_hash`）與交付核准畫面（repo、分支、diff 預覽與 PR 內容，以裝置金鑰簽署 `GitHubPublish`）
- `StartTask` + thread 進度回報 + 任務預算
- `audit` crate：hash-chained 稽核日誌，node 與 Agent Host 各自寫入，node 的日誌同步回 Agent Host（加密遠端備份於 P3 實作）
- Overlay 網路 ACL（多機部署，`node-linux` 加入時）：node 僅能連線至 `agent-host:8443`，無法連線至 Agent Host 主機的其他 port、其他 node 或其他服務

**不納入範圍**：`node-mac` 與 macOS 單機版（P4）；gVisor（之後評估）；行動版；connector；需要 USB 大量儲存或 raw USB 的燒錄方式（例如 RP2040 UF2）。`node-build` 可於 P2 後期視進度加入。

## 3. 已決定事項

- Coding loop 於 Agent Host 執行，tool call 經 hub 送至 node（LLM key 不離開 Agent Host）
- 不實作通用 credential broker；寫入出口採白名單，目前僅 Agent Host 上的 GitHub 寫入 token，僅供 `GitHubPublish` 使用，每次須裝置簽章或符合 auto mode grant
- Auto mode 僅適用於 `GitHubPublish`；`LanAccess`、`HostRead` 一律須裝置簽章，由 node 驗證
- 第一台 node 為 Linux 單機版 `node-dev`；單機版為獨立工作區，可與多機部署的工作區並存
- Bench device 不需簽章，其信任來自 node 本地政策檔；燒錄於 sandbox 內執行，不由 node 於 sandbox 外代為燒錄
- 移除「client 預覽 diff 後由使用者自行 push」的交付流程；未納入 token 範圍的 repo 無法交付，agent 於頻道中說明

## 4. 里程碑

- [ ] M2.1 `SignedCommand` / `SignedApproval` + `policy` crate + 單元測試（重播、錯誤 node、過期、竄改 payload）
- [ ] M2.2 Hub node 佇列 + TTL + presence
- [ ] M2.3 Node daemon + Linux 單機版部署（`node-dev`：NixOS module 與通用 systemd 設定、`pots-host` / `pots-node` 分離、hub socket、node 金鑰登記、`meta skuid` 限制）
- [ ] M2.4 Sandbox driver：rootless Podman、權限收斂、唯讀 rootfs、cgroup、Nix base image
- [ ] M2.5 Sandbox 網路：netns + nftables 封鎖清單 + 公共 DNS
- [ ] M2.6 `SandboxOp` 執行模型 + 持久 exec session + 輸出串流
- [ ] M2.7 Label 排程 + `StartTask` + thread 回報 + 任務預算
- [ ] M2.8 Git bundle 進出 + `GitHubPublish`（client diff 預覽與簽章、Agent Host publisher push 與開 PR）
- [ ] M2.9 跨邊界動作（`LanAccess`、`HostRead`）+ 桌面核准 UI + 裝置金鑰簽章
- [ ] M2.10 `audit` crate，node 與 Agent Host 寫入
- [ ] M2.11 `GitHubPublish` auto mode：`AutoModeGrant` 簽署與撤銷 UI、publisher 的 grant 與 `HARD_DENY` 檢查、reviewer、事後通知與稽核（依賴 M2.8、M2.10）
- [ ] M2.12 Bench device：政策檔登記、label 回報、`--device` 附加、重新枚舉處理、`arduino-cli` base image（依賴 M2.4、M2.7）
- [ ] M2.13 `node-linux`（多機部署）：node 金鑰登記、overlay 網路 ACL、x86_64 / arm64 base image（P2 後期）

## 5. 關卡

**路線圖關卡**

- Agent 在 `node-dev`（單機版）的 sandbox 內執行指令並回報
- Agent 能對 bench device 編譯、燒錄並讀取 serial 輸出
- Node 與 sandbox 上不存在任何寫入憑證；未經簽章的跨邊界動作遭 node 拒絕；未經簽章且不在 auto mode grant 範圍內的 GitHub push / 開 PR 遭 Agent Host 拒絕

**補充驗收測試**

- 重播舊核准、將 A node 的核准送至 B node、竄改 payload 後沿用核准，皆遭拒絕
- 單機版：`pots-host` 無法讀取裝置金鑰、修改 node 政策檔或讀取 `pots-node` 狀態；`pots-node` 與 sandbox 無法讀取使用者 home 與 `pots-host` 狀態
- 單機版：`pots-node` 連線至同機其他 loopback 服務失敗；hub 未對任何 TCP port 或區網開放
- Bench device：未登記的 serial 裝置及 `/dev/bus/usb` 在 sandbox 內不可見；未要求該 label 的任務看不到任何裝置；同一裝置無法同時附加至兩個任務
- Bench device：Uno（DTR 重置）、SAMD 或 Leonardo（1200bps touch、重新枚舉）、ESP32 皆能於 sandbox 內完成 upload 並讀取 serial
- 多機部署：自 node（sandbox 外）連線至其他 node 或 Agent Host 主機的其他 port 皆失敗；僅 `agent-host:8443` 可連線
- 以 A node 的金鑰連線至 hub，無法收到 B node 的指令
- Sandbox 內連線至私有位址（例如 192.168.x.x）、區網上的儲存設備或路由器皆失敗，網際網路可用
- Sandbox 內連線至任何 overlay 網路節點（overlay IP、內部 DNS 名稱，例如 Tailscale 的 `100.x` 與 MagicDNS）皆失敗
- `LanAccess` 核准後僅開放指定的 host:port，TTL 到期後連線失敗
- Sandbox 內掃描不到任何 secret（環境變數、檔案）；容器內的 root 無法寫入 rootfs，亦無法載入 kernel module
- 超出記憶體 / pids / 時間上限的任務遭終止，node 維持正常運作
- 竄改稽核日誌任一筆紀錄後，hash chain 驗證失敗
- Sandbox 內執行 `git push` 或 `gh pr create` 皆因無憑證而失敗
- `GitHubPublish`：未簽章、簽章後竄改分支 / PR 內容、bundle head 與簽章不符、重播舊簽章、目標 repo 不在授權清單內，皆遭拒絕
- 以 GitHub token 推送修改 `.github/workflows/` 的 commit、force push、直接推送至受保護的預設分支，皆遭 GitHub 拒絕
- Auto mode：符合 grant 且 reviewer 通過的 publish 不需人工簽章即完成，並留下含 grant ID 的稽核紀錄
- Auto mode：分支不符前綴、非 draft PR、diff 超過上限、超過每日次數、修改 `.github/` 等敏感路徑、grant 過期或已撤銷、grant 簽章遭竄改，皆退回人工核准，即使 reviewer 回覆 `Approve` 亦然
- Auto mode：`LanAccess`、`HostRead` 無法經任何 grant 自動核准
- Auto mode：以含 prompt injection 的 diff（例如註解要求 reviewer 核准）測試，結果仍受 grant 與 `HARD_DENY` 限制
