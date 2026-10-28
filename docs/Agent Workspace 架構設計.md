# Agent Workspace 架構設計

| 項目 | 內容 |
| --- | --- |
| 文件狀態 | 草案 |
| 最後更新 | 2026-10-27 |
| 相關文件 | `plans/README.md`（開發計畫）、`docs/Agent Workspace UI v1.html`（UI mockup） |

## 1. 概覽

本文件描述一套自架的 agent 工作空間。人類與 agent 同為頻道成員，在 Slack 式的頻道中協作；agent 可以邀請其他 agent 加入，也可以在使用者自有的機器上執行工作。系統定位為個人助理與多專案管理工具，目標是實際產出成果，而非對話娛樂。

### 1.1 核心需求

- **頻道式互動**：不使用 session 概念；對話是持續的事件串流，由 agent 自行判斷發言時機
- **Agent 互相邀請**：例如 PM agent 邀請工程師 agent 討論、邀請 worker agent 實作；受邀成員會收到交辦摘要（brief）
- **持久身分**：同一個 agent 跨頻道、跨日保有身分與記憶
- **機器操控**：由中央的 Agent Host 搭配多個 node，agent 可在 node 的 sandbox 內自由操作；另提供單機版，Agent Host 與 node 於同一台機器執行（見 3.6 節）
- **開發板**：agent 可在 sandbox 內編譯、燒錄專供 agent 使用的開發板，並讀寫其 serial（見 6.11 節）
- **自有硬體與外部推理**：系統元件部署於使用者控制的硬體，多機連線位於私有 overlay 網路內；呼叫外部 LLM provider 時會傳送 prompt 與回應，其資料保留依實際服務條款與帳號設定。零資料保留（ZDR）為可選限制，非系統硬條件
- **自建 UI**

### 1.2 設計原則

1. **身分僅來自金鑰**：裝置、node 與 host 皆以自身金鑰證明身分；不信任網路位置、來源 IP 或 reverse proxy 附加的 header
2. **人類的訊息與指令一律簽章**：node 只信任簽章與本地政策，不信任 Agent Host 轉送的任何內容
3. **以 sandbox 為邊界**：sandbox 內全自動執行；跨出邊界須經使用者以裝置金鑰簽章核准；寫入外部服務採出口白名單，目前僅有 GitHub（push、開 PR），其憑證僅存放於 Agent Host，每次寫入須經使用者裝置簽章，或落在使用者簽署的 auto mode grant 範圍內（見 6.8、6.10 節）；其他外部服務的寫入由使用者親自執行
4. **預設保持安靜**：agent 無法確定是否應發言時，不發言

### 1.3 術語與部署角色

本文件僅描述部署角色，不綁定特定硬體、作業系統發行版或既有基礎設施。文中的工具名稱（例如 Tailscale、Traefik、sops、R2）為參考實作，可替換為具備相同性質的方案；可替換時以「例如」或「參考實作」標示。

| 術語 / 代稱 | 說明 |
| --- | --- |
| 使用者 | 系統的擁有者與主要操作者；在資料模型中對應 `Role::Owner` |
| Agent Host | 執行所有 agent、hub 與 connector 的中央服務 |
| Agent Host 主機（`agent-host`） | 部署 Agent Host 的常駐 Linux 主機；不需高效能。可為專用主機，亦可與其他對外服務同機（風險見 2.3 節） |
| Hub | Agent Host 內負責連線驗證、頻道排序、扇出與 node 佇列的模組 |
| Node | 接受簽章指令、在 sandbox 內執行任務的機器 |
| `node-dev` | 以 Linux 單機版部署時的 node，為第一台 node |
| `node-linux` | 常駐的獨立 Linux node（x86_64 或 arm64），用於多機部署 |
| `node-build` | 選用的重型 build node（可為 WSL 等非全天開機的 Linux 環境） |
| `node-mac` | Apple Silicon Mac node，用於 Xcode、iOS build 與 Safari 測試 |
| 開發機 | 使用者日常開發與執行桌面 client 的機器；以單機版部署時兼任 Agent Host 與 `node-dev`，開發板可接於此機 |
| 單機版（standalone） | Agent Host（含 hub）與 node daemon 於同一台機器、以不同 OS 使用者執行的部署方式，與多機部署並存（見 3.6 節） |
| Overlay 網路 | 串接多機部署各元件的私有網路（例如 WireGuard 為基礎的 mesh VPN；參考實作為 Tailscale，下稱其網路為 tailnet） |
| 反向代理 | 選用；Agent Host 主機上負責 TLS 終止的 proxy（例如 Traefik、Caddy） |
| Bench device | 登記於 node 本地政策檔、專供 agent 使用的開發板（見 6.11 節） |

## 2. 信任邊界與威脅模型

系統內的明文位於使用者控制的硬體上（裝置、Agent Host、node）；外部 LLM provider 亦可見送出的 prompt 與回應，並可能依其服務條款與帳號設定保留資料。Client、Agent Host 與 node 之間的多機連線經由使用者的 overlay 網路，路徑上不存在可讀取內容的第三方；對外部 LLM provider 的連線不屬於此保證。

| 元件 | 位置 | 可信 | 可見資訊 |
| --- | --- | --- | --- |
| Client（Tauri、手機） | 使用者的裝置 | 是 | 明文、自身的裝置金鑰 |
| Agent Host（含 hub） | Agent Host 主機，獨立帳號 `pots` | 是 | 明文、agent 私鑰、host 金鑰、記憶、LLM API key、所有服務 token（GitHub 寫入 token 以外皆為唯讀） |
| 反向代理（選用） | Agent Host 主機 | 相鄰：終止 TLS，遭攻破即等同 Agent Host 遭攻破 | 可見 pots 流量的明文 |
| Agent Host 主機上的其他服務（若有） | 部署者既有的對外或內部服務 | 相鄰：以 root 執行的服務遭攻破，即等同 Agent Host 遭攻破 | 取決於其權限 |
| Node daemon | `node-dev`、`node-linux`、`node-build`、`node-mac` | 是（僅信任簽章） | 發送給自身的指令與任務資料 |
| Overlay 網路控制面 | 例如 Tailscale 的協調伺服器與 DERP relay | 僅負責發布節點公鑰；啟用節點加入簽章（Tailscale 為 Tailnet Lock）後無法私自加入節點 | Metadata；relay 僅見 WireGuard 密文 |
| 遠端物件儲存 | S3 相容儲存（例如 Cloudflare R2） | 否 | 僅有以 recovery key 加密的備份 |
| APNs / FCM | 推播 | 否 | 僅有內容為空的「有更新」通知 |
| LLM provider | 透過 OpenAI-compatible / Anthropic-compatible API 或 OAuth 使用的服務 | 接受的信任點；ZDR 可選 | 可見 prompt 與回應；是否保留依實際服務條款與帳號設定 |

### 2.1 不採用 E2EE 的理由

E2EE 所防範的對象是「負責轉送訊息、但不可信的中間人」。本架構中，client 與 node 直接經 overlay 網路（例如 WireGuard）連線至 Agent Host，而 Agent Host 本身即是可見明文的端點，路徑上不存在不可信的中繼，因此額外加上 MLS 等 E2EE 機制無法防範任何新的對象。與機密性無關的機制仍予保留：裝置簽章核准、靜態加密、復原金鑰與加密備份。

### 2.2 不依賴 SSO / forward-auth 或 proxy header 認證的理由

即使 Agent Host 主機上已有反向代理搭配 SSO / forward-auth（例如 Authelia、oauth2-proxy），其交付給後端的身分仍僅是 proxy 附加的 header：同機任何能連線至 loopback 的程式皆可偽造；SSO 的 session cookie 範圍亦可能涵蓋同網域的其他服務。因此 pots 的 API 不經 SSO，每條連線皆以裝置或 node 金鑰進行雙向 challenge-response；反向代理僅負責 TLS 終止與來源過濾。

### 2.3 威脅分析

**可接受的資訊外洩**

- Overlay 網路的控制面可見 metadata：機器之間的連線時間、對象與流量大小
- APNs / FCM 可見推播時間點；推播不含內容
- 遠端物件儲存的提供者可見備份的大小與時間
- LLM provider 可見 prompt 與回應，並可能依服務條款與帳號設定保留資料；僅於頻道明確要求 ZDR 時，限制使用已確認符合條件的 provider

**防範的威脅**

| 威脅 | 對策 |
| --- | --- |
| 同機程式或 overlay 網路內其他機器冒充使用者 | 即使能連線至 hub，仍須持有裝置或 node 私鑰才能通過連線驗證；不信任任何 header 或來源 IP |
| Node 或 sandbox 遭攻破後橫向移動 | Overlay 網路 ACL 限制 node 僅能連線至 Agent Host 主機上 pots 專用的 port，無法連線至同機其他服務或其他 node |
| Overlay 網路控制面遭入侵或惡意操作 | 啟用節點加入簽章（例如 Tailnet Lock），新節點須經使用者信任的機器簽章方可加入 |
| Agent 遭 prompt injection | 影響範圍限於 sandbox 內；跨出邊界須經使用者簽章；GitHub push / 開 PR 須經使用者簽章，或落在 auto mode grant 範圍內並通過 reviewer 與 `HARD_DENY` 檢查（見 6.8 節）；無法寫入其他外部服務 |
| 區網或 overlay 網路遭掃描 | Sandbox 封鎖對區網與 overlay 網路的 egress |
| 裝置遺失 | 由另一台已信任裝置撤銷其金鑰；node 上的核准公鑰清單由使用者手動移除（見 4.5 節） |

**已接受的風險：Agent Host 遭入侵等同全面失守。** 包含 Agent Host 主機的 root 權限遭取得，例如同機對外的反向代理遭攻破。若該主機另有對外公開服務，此情境的發生機率相對較高，建議以專用主機部署。損害上限由「除 GitHub 外不存在寫入憑證」保證：入侵者可讀取訊息、記憶與 connector 資料，可消耗 LLM 預算、在 sandbox 內執行指令，**並可直接使用 GitHub 寫入 token，在其涵蓋的 repo 上 push 與開 PR**（GitHub 寫入的裝置簽章由 Agent Host 驗證，Agent Host 遭入侵時無法提供保護）；但無法寄信、繳交作業，亦無法偽造使用者對 node 跨邊界動作的裝置簽章。GitHub 部分的損害由 token 的權限範圍與 repo 的 branch protection 限制（見 6.10 節）。GitHub 的 auto mode 不改變此上限：Agent Host 遭入侵時，入侵者本即可直接使用 token（見 6.8 節）。

單機版的信任邊界與上述差異見 3.6 節。

**不在防範範圍內**

- 「中繼可見內容」：路徑上不存在第三方中繼。反向代理雖可見明文，但它與 Agent Host 位於同一台主機，屬於同一信任範圍

### 2.4 與常見 agent gateway 設計的比較

常見的自架 agent gateway 多將 gateway 視為唯一的信任點。pots 是獨立產品，可沿用既有的 provider 與 agent loop 實作；沿用時仍須符合 pots 的身分、核准與 sandbox 邊界，不因程式碼重用而沿用其信任模型。兩者皆將 gateway 視為可信、皆可位於私有網路內，但授權模型不同：

| 面向 | 常見 gateway 設計 | pots |
| --- | --- | --- |
| 人類身分 | SSO session 或裝置 bearer token，經 proxy header 交給 gateway | 裝置 Ed25519 金鑰，連線時進行 challenge-response |
| 核准 | 由 gateway 轉交 node；gateway 遭攻破即可自行核准 | Node 驗證使用者的裝置簽章；Agent Host 遭攻破亦無法偽造 |
| Node 驗證 | 所有 node 共用一個 token | 每台 node 持有自己的金鑰，指令帶 nonce 與到期時間 |
| Agent 執行位置 | Agent loop 在 node 上 | Agent loop 在 Agent Host；node 僅執行經簽章的 `SandboxOp` |
| 寫入憑證 | 允許（sandbox 內的 git 憑證、不經 sandbox 的 terminal、不經 sandbox 的 shell tool） | 寫入出口白名單；目前僅 GitHub 寫入 token 存放於 Agent Host，不進入 sandbox；每次寫入須裝置簽章或落在使用者簽署的 auto mode grant 內；其他寫入由使用者執行 |
| Sandbox 網路 | 網域白名單；瀏覽器在 sandbox 外 | 於網路層封鎖區網與 overlay 網路 |

相較常見 gateway 設計，pots 須確保「核准」「Node 驗證」「寫入憑證」「Sandbox 網路」四項：即使 gateway 或 host 遭攻破，損害仍有上限。

## 3. 系統元件

系統由三個元件組成：Agent Host 主機上的 Agent Host（內含 hub）、各 node 上的 node daemon，以及使用者的 client。系統支援兩種並存的部署方式：本章所述的多機部署，以及三個元件位於同一台機器的單機版（3.6 節）。所有機器以 overlay 網路串接；client 與 node 皆主動連線至 Agent Host，node 不監聽任何 port，亦不為 pots 開放任何公開入口。

```
client ──overlay──▶ agent-host:8443（反向代理的 pots 專用 port，僅接 overlay 介面）──▶ 127.0.0.1 Agent Host / hub
node   ──overlay──▶ agent-host:8443 ──▶ 同上（網路 ACL：node 僅能連線此 port）
Agent Host ──▶ LLM provider、郵件 / 行事曆 / LMS API、APNs / FCM、遠端物件儲存（皆為主動外連）
```

`:8443` 為範例，可設定。

&#91;embedded content: 系統架構 · 不可信區與可信區\]

### 3.1 部署（Agent Host 主機）

以下為多機部署的參考做法；各項工具可依部署者既有環境替換，但須維持相同的安全性質。建議將部署設定與網路 ACL 納入版本控制。

| 項目 | 做法 |
| --- | --- |
| 行程 | 以主機的 service manager（systemd、OpenRC 等）執行，獨立帳號 `pots`，狀態存放於 `/var/lib/pots`（權限 0700） |
| 監聽 | 僅綁定 `127.0.0.1`；不綁定 `0.0.0.0` 或 overlay IP（未使用反向代理時例外，見下） |
| 入口 | 反向代理新增 pots 專用 port（例如 `:8443`），僅掛載 pots 的路由；主機防火牆僅對 overlay 介面開放該 port，不對區網或公網開放。若主機上沒有其他服務，可省略反向代理，由 hub 直接以 TLS 監聽 overlay 介面 |
| 不共用 443 的理由 | 若 443 依 Host header 同時服務多個同機服務，而網路 ACL 僅能限制至 port 層級，使用專用 port 才能讓 node 只連得到 pots |
| TLS | 例如 Let's Encrypt DNS-01 或 overlay 網路提供的憑證；pots 網域僅解析至 Agent Host 主機的 overlay IP（內部 DNS 或 overlay 網路的 DNS） |
| 認證 | 不掛載 SSO / forward-auth、不讀取任何身分 header；由 hub 自行執行金鑰 challenge-response（見 4.1 節） |
| Secret | 解密後僅供 `pots` 帳號讀取（參考實作：sops / sops-nix） |
| Overlay 網路 | ACL：使用者裝置 → `agent-host:8443`；node → `agent-host:8443`；其餘一律拒絕。啟用節點加入簽章（例如 Tailnet Lock） |

### 3.2 Agent Host

- Rust + tokio；每個 agent 為一個 actor，以 `mpsc` 作為收件匣
- 負責 turn arbiter、hop limit 與預算控管
- 記憶庫：SQLite + `sqlite-vec`，整個檔案加密為後續目標（P4）；P1 僅作測試驗證、不投入日常使用，可暫不加密，投入日常使用前須另定資料保護要求
- 持有 host 金鑰（client 釘選其指紋）與所有 agent 的身分簽章私鑰
- 強制執行 agent 的頻道成員資格、成員上限與邀請權限
- 呼叫 LLM provider（OpenAI-compatible / Anthropic-compatible API，另支援 OAuth；ZDR 為可選頻道限制）
- 多機部署時不兼任 node：Agent Host 主機不執行 sandbox（單機版以不同 OS 使用者與 node 同機，見 3.6 節）
- 所有 agent loop（含 coding 任務）皆在此執行；tool call 以簽章指令經 hub 送至 node
- Secret 以加密形式納入版本控制（參考實作：sops / sops-nix），部署時僅解密至 Agent Host。Agent Host 是系統中唯一持有服務 token 的位置；除 GitHub 寫入 token 外，所有 token 皆僅具讀取權限（見 6.9、6.10 節）

### 3.3 Hub

Hub 是 Agent Host 內的模組，與 Agent Host 位於同一行程。

| 職責 | 做法 |
| --- | --- |
| 連線驗證 | 裝置、node 與 host 之間的雙向 challenge-response；比對裝置註冊表與 node 清單 |
| 頻道排序與扇出 | 每個頻道一個 actor，負責指派 `seq`、WebSocket 扇出與歷史補拉；儲存於 SQLite |
| Node 佇列 | 每台 node 的指令佇列、presence 與指令 TTL；node 離線時指令於此排隊 |
| Blob | Git bundle 與附件暫存於本機，定期清除 |
| 推播 | 主動連線至 APNs / FCM，payload 僅含「有更新」 |

### 3.4 Nodes

| Node | 用途 | Sandbox | 注意事項 |
| --- | --- | --- | --- |
| `node-dev`（Linux 單機版） | 第一台 node：日常 coding 任務、開發板（bench device） | rootless Podman，以 `pots-node` 使用者執行 | 與 Agent Host 同機，見 3.6 節；開發板見 6.11 節 |
| `node-linux`（常駐 Linux 主機） | 多機部署：常駐任務、排程、監控（Phase 2 後期加入） | rootless Podman（x86_64 或 arm64） | 建議為無其他狀態、專供 agent 使用的主機；單板電腦建議使用 SSD 而非 SD 卡 |
| `node-mac`（Apple Silicon Mac） | Phase 4 加入：Xcode、iOS build、Safari 測試 | Linux 工作使用 Apple `container` 或 OrbStack；macOS 工作使用 Tart VM | 須設定不休眠（例如 `pmset`）；無風扇機型會降頻 |
| `node-build`（選用） | 重型 build、長時間任務 | rootless Podman | 可非全天開機，任務於 hub 的 node 佇列排隊；若為 WSL，閒置時會自動關閉 |

Node 主機的對外連線限制應同時以本機防火牆與 overlay 網路 ACL 實施，不僅依賴其中一者。

### 3.5 Client

- Tauri 桌面版，共用 Rust core（同步、本地儲存、裝置金鑰）
- 行動版透過 UniFFI 重用同一個 core；行動裝置須常駐連上 overlay 網路才能連線至 host
- 搜尋於 Agent Host 執行
- 核准介面為一級功能，須清楚呈現：哪個 agent、哪台機器、什麼指令、理由為何

### 3.6 單機版（standalone）

Agent Host（含 hub）與 node daemon 於同一台機器執行，與多機部署並存。典型使用情境為開發機（`node-dev`）：開發板接於開發機，不需分別部署 hub 與 node；亦適合只有一台機器的使用者。單機版是一個獨立的工作區（見 8.1 節），與多機部署的工作區互不共享資料。

- **同一套程式碼與協定**：node 仍僅接受 `SignedCommand`，連線仍經 challenge-response 與 host 指紋釘選；差異僅在部署
- **連線**：hub 以 Unix domain socket 監聽（或僅綁定 `127.0.0.1`），以檔案權限限制連線者；身分仍由 challenge-response 保證。不需 overlay 網路與反向代理；行動版或其他機器的 node 加入時，再依 3.1 節的方式對 overlay 網路開放入口
- **Secret**：解密至僅 `pots-host` 可讀的位置；node 不持有任何服務 token

**三個 OS 身分**

| 身分 | 執行內容 | 持有 | Linux（systemd；參考打包為 NixOS module） | macOS（Phase 4） |
| --- | --- | --- | --- | --- |
| `pots-host` | Agent Host + hub | Host 金鑰、agent 私鑰、記憶庫、LLM API key、GitHub 寫入 token | systemd service，`User=pots-host` | launchd daemon，role account `_pots-host` |
| `pots-node` | Node daemon + sandbox | Node 金鑰；不持有服務 token | systemd service + rootless Podman，`User=pots-node` | launchd daemon，role account `_pots-node`；Linux sandbox 使用 OrbStack |
| 使用者 | 桌面 client | 裝置金鑰 | 使用者帳號 | 使用者帳號（Keychain） |

- Node 本地政策檔（`owner_device_keys`、bench device 清單）由 root 擁有，`pots-node` 僅可讀取
- `pots-host` 與 `pots-node` 互相無法讀取對方的狀態目錄；兩者皆無法讀取使用者的 home
- macOS 的 Linux sandbox 不使用 Apple `container`：其尚不支援 USB passthrough，無法使用開發板。OrbStack 預設會將執行者的 home 掛載至其 Linux VM，因此須以 `_pots-node` 執行，使掛載的是該帳號而非使用者的 home

**信任邊界的差異**

- **維持不變**：`pots-host` 遭入侵時，損害等同 2.3 節的「Agent Host 遭入侵」，但仍無法讀取裝置金鑰或修改 node 政策檔，因此無法偽造 node 跨邊界動作的核准，也無法擴充 bench device
- **已接受**：該機器的 root 或使用者帳號遭取得時，等同全面失守（包括裝置金鑰）
- **Sandbox 的重要性提高**：逃逸後所在的是使用者的日常機器。逃逸者取得 `pots-node` 的權限，無法讀取使用者的 home，但可連線至同機監聽 loopback 的其他服務。以 nftables 的 `meta skuid` 限制 `pots-node` 僅能連線至 hub 與網際網路
- **與多機部署相比**：不存在「Agent Host 與對外服務同機」的風險，但 Agent Host 與使用者的日常環境（瀏覽器、各種開發工具）同機

## 4. 通訊協定

傳輸層為 overlay 網路上的 WebSocket（overlay 網路的加密加上 TLS），不採用 E2EE（理由見 2.1 節）。安全性建立在兩項機制上：**連線身分**與**作者簽章**。

### 4.1 連線與身分

**首台 Owner bootstrap（P1）**：client 產生並保留裝置私鑰，僅匯出公鑰。使用者透過主機端本機管理 CLI 手動將該公鑰登記至 Owner 的裝置註冊表，並取得 host 指紋供 client 首次連線時核對後釘選。不提供「第一個網路連線者自動成為 Owner」的流程；後續裝置的簽章加入於 P3 實作（見 4.5 節）。

每條 WebSocket 連線建立後，須先完成雙向 challenge-response；通過驗證前，hub 不處理任何訊息：

1. Hub 送出 nonce 與 host 公鑰，並以 host 金鑰簽署該 nonce
2. Client 或 node 比對已釘選的 host 指紋（首次連線時建立；指紋改變時拒絕連線並發出警告），再以自身私鑰簽署 `(nonce, host_id, 時間)`
3. Hub 比對裝置註冊表或 node 清單進行驗證；連線建立後的身分即為該金鑰，而非來源 IP 或任何 header

| 主體 | 金鑰 | 註冊位置 |
| --- | --- | --- |
| 使用者的裝置 | 裝置 Ed25519 金鑰 | Hub 的裝置註冊表（首台 Owner 由主機端 CLI 手動登記；後續由已信任裝置簽章加入，見 4.5 節） |
| Node | Node Ed25519 金鑰 | Hub 的 node 清單（部署時手動登記） |
| Agent Host | Host Ed25519 金鑰 | Client 與 node 首次連線時釘選 |
| Agent | Agent Ed25519 金鑰（身分的根） | Agent Host |

### 4.2 訊息格式

```rust
struct Message {
    v: u8,                    // 格式版本
    channel: ChannelId,
    seq: Option<u64>,         // 由 hub 的頻道 actor 指派的全序序號
    author: AuthorId,         // 人類（裝置）或 agent
    msg_id: MsgId,
    sent_at: Timestamp,       // 與 msg_id 共同防止重播
    body: Body,               // Say / MemberJoined / MemberLeft / ...
    sig: Signature,           // 作者簽章：人類使用裝置金鑰，agent 使用 agent 金鑰
}
```

- 連線主體的身分由 4.1 節的 challenge-response 保證，訊息本身不再另加連線層簽章
- 作者簽章讓人類的訊息與指令可由 node 與稽核日誌事後驗證；Phase 5 中來自其他 host 的訪客 agent 亦以此辨識
- Agent 的作者簽章無法防範 Agent Host 遭入侵（私鑰皆存放於 host），其用途為歸屬與稽核

### 4.3 排序

每個頻道由一個 actor 指派 `seq`。Hub 與 Agent Host 位於同一行程，無需分散式排序。

### 4.4 Brief

受邀的 agent 僅取得邀請者整理的 brief 與頻道摘要，不讀取完整歷史。此限制由 Agent Host 在應用層保證，屬產品設計，而非密碼學保證。

### 4.5 多裝置與備份

- **新增裝置**：由已信任的裝置掃描 QR code，簽署新裝置的公鑰，hub 將其寫入裝置註冊表
- **撤銷裝置**：由任一台已信任裝置簽署撤銷，hub 立即中斷該裝置的連線。Node 上的 `owner_device_keys` 須由使用者於 node 上手動移除（node 不從網路更新核准公鑰）
- **復原金鑰**：一組離線保存的 recovery key，用於重建裝置信任與解密備份
- **加密備份**：Agent Host 的金鑰、記憶庫與稽核日誌，以 recovery key 加密後備份至遠端物件儲存；遠端僅存在密文

## 5. Agent 模型

每個 agent 是一個長期存在的 actor：一把永不變更的簽章金鑰即為其身分，人格、記憶與權限皆附屬於此身分。

### 5.1 身分與記憶

```rust
struct AgentIdentity {
    id: AgentId,
    signing_key: Ed25519KeyPair,   // 身分的根
    profile: Profile,              // 名稱、角色、個性、system prompt
    memory: MemoryStore,
    capabilities: Vec<Capability>, // 見 6.6 節
}

struct MemoryStore {
    global: Partition,                          // agent 自身設定、使用者偏好
    per_project: HashMap<ProjectId, Partition>, // 專案之間互相隔離
}
```

- 同一個 agent 可同時參與專案 A 與 B，但在 B 的頻道中無法讀取 A 的記憶分區
- 僅個人助理（幕僚長）可跨專案存取記憶

### 5.2 Agent 的上下文（ChannelView）

系統不將完整頻道歷史放入 context，而是在每次呼叫 LLM 時組裝以下內容：

1. 最近 K 則訊息（滑動視窗）
2. 頻道的滾動摘要（定期壓縮）
3. Agent 自身的私人筆記（例如「已承諾處理 X」）
4. 必要時進行 retrieval（舊訊息、文件）

長任務的進度回報放在 thread 中，以保持主頻道整潔。

### 5.3 發言輪替（Turn-taking）

Arbiter 於 Agent Host 執行，對每個新事件依序判斷：

1. **Debounce**：等待頻道安靜 2 至 3 秒，將人類連續送出的多則訊息合併處理
2. **@mention**：被點名的 agent 必定觸發
3. **Relevance gate**：未被點名的 agent 以低成本模型或 embedding 評分，超過門檻者列為候選
4. **Floor lock**：同一時間僅授予一個候選發言權；其「輸入中」期間，其他 agent 的門檻提高
5. **防護檢查**：hop limit、預算與冷卻時間（見第 9 章）

### 5.4 邀請（Invite）

邀請以 tool call 的形式實作：

```rust
enum Action {
    Say { text: String, thread: Option<ThreadId> },
    Invite { who: InviteTarget, brief: String },
    StartTask { spec: String, requirements: TaskRequirements, thread: ThreadId },
    React { emoji: String },
    StaySilent,
}

enum InviteTarget {
    Existing(AgentId), // 邀請已有記憶的成員，例如「Alex（後端）」
    Role(Role),        // 由幕僚長挑選或新建
}
```

- 受邀的 agent 取得 brief 與頻道摘要，不讀取完整歷史
- 邀請權限屬於 capability 的一種：worker agent 預設不得再邀請 worker agent
- 每個頻道設有成員上限

### 5.5 個人助理（幕僚長）

- 使用者的預設對話對象，可跨所有專案
- 負責開設頻道、將合適的 agent 邀入合適的專案、彙整各專案進度
- 本身不操作機器，僅負責派工

### 5.6 模型路由

模型屬於設定的一部分，可在四個層級覆寫，越具體者優先：單次任務 > 頻道 > agent（角色）> 全域預設。各層級皆可依用途分別指定。

API adapter 支援 OpenAI-compatible 與 Anthropic-compatible API，端點可設定，不限於官方服務；OAuth 支援範圍維持 Codex、Claude、GitHub Copilot。可沿用既有 provider 實作，但 API 相容性、登入方式與 ZDR 資格是不同屬性，不互相推導。

| 用途 | 建議的模型等級（實際模型由設定決定） |
| --- | --- |
| 日常對話、幕僚長、摘要、relevance gate | 低成本、低延遲的 Flash 等級模型 |
| 撰寫程式碼、code review、複雜規劃 | 高能力的旗艦等級模型 |

```rust
enum Purpose { Chat, Gate, Summarize, Code, Plan, Review }

struct ModelRef { provider: ProviderId, model: String }

struct Provider {
    id: ProviderId,
    zdr: bool,                    // 該端點與帳號是否已確認符合 ZDR；未確認為 false
    adapter: Box<dyn LlmAdapter>, // 各家 API 與 tool calling 格式不同，以統一介面封裝
}

fn resolve(p: Purpose, task: &Task, ch: &Channel, agent: &Agent, global: &Models) -> Result<ModelRef> {
    let m = task.model.clone()
        .or_else(|| ch.models.get(&p).cloned())
        .or_else(|| agent.models.get(&p).cloned())
        .unwrap_or_else(|| global.get(p));
    ensure!(!ch.require_zdr || providers().get(&m.provider)?.zdr, Error::ZdrRequired);
    Ok(m)
}
```

- ZDR 非硬條件，頻道預設 `require_zdr = false`；僅於使用者明確啟用時，未確認符合 ZDR 的 provider 才會被直接拒絕，不會自動 fallback
- 每次呼叫所使用的模型皆記入稽核日誌，成本依頻道與模型分別統計
- 同一個 agent 可使用不同模型：例如工程師 agent 對話時使用 Flash，接到 `StartTask` 撰寫程式碼時切換至旗艦模型

## 6. Node 與機器操控

Agent 在 sandbox 內可自由操作；跨出 sandbox 的動作須經使用者核准，且核准必定附帶簽章。Node 不信任 Agent Host（含 hub），僅信任簽章與本地政策。

### 6.1 Node daemon

- 僅主動連線至 Agent Host 主機的 hub（多機部署為專用 port，例如 `:8443`；單機版為本機的 hub socket），不開放任何 port
- 持有自身金鑰，連線時以其進行 challenge-response，並釘選 host 指紋；僅能收到發送給自身的指令
- 上線時從 hub 的 node 佇列拉取排隊中的指令，超過 TTL 者直接捨棄
- 回報 `NodeDescriptor`：label、狀態、資源

### 6.2 以 label 排程

Agent 不指定機器名稱，僅描述需求（例如 `arch=arm64` + `cap=docker`，或 `cap=xcode`）。

```rust
struct NodeDescriptor {
    id: NodeId,
    labels: BTreeSet<Label>,  // os=macos, arch=arm64, cap=xcode, cap=docker, always_on
    status: NodeStatus,       // Online | Sleeping { since } | Offline
    resources: Resources,     // cpu、mem、是否 thermal-limited
}

fn schedule<'a>(req: &TaskRequirements, nodes: &'a [NodeDescriptor]) -> Placement<'a> {
    let mut fit = nodes.iter().filter(|n| req.labels.is_subset(&n.labels));
    match fit.clone().find(|n| n.status.is_online()) {
        Some(n) => Placement::Run(n),
        None => match fit.next() {
            Some(n) => Placement::Queue { node: n, ttl: req.ttl }, // 等待 node 喚醒
            None => Placement::Impossible,                         // agent 於頻道中說明
        },
    }
}
```

### 6.3 Sandbox 邊界

| Sandbox 內（自動執行） | 跨出邊界（須使用者裝置簽章，由 node 執行） | GitHub 寫入（須使用者裝置簽章或 auto mode grant，由 Agent Host 執行） | 其他外部寫入（由使用者執行） |
| --- | --- | --- | --- |
| 安裝套件、執行任意指令、編譯、測試 | 存取區網或 overlay 網路（路由器、儲存設備、其他機器） | `git push`（不含 force push） | deploy |
| 存取網際網路 | 存取 host 檔案系統 | 開 PR | 寄信、發送行事曆邀請、繳交作業 |
| 修改專案 worktree 內的檔案 | | | |
| 燒錄、讀寫 node 政策檔登記的 bench device（見 6.11 節） | | | |
| 銷毀並重建 | | | |

- Sandbox 內不存在任何 secret 或 API key；GitHub 寫入 token 僅存放於 Agent Host，sandbox 內無法直接執行 `git push` 或 `gh pr create`，須改用 `GitHubPublish` 操作（見 6.10 節）；其他外部服務的寫入一律由使用者以自身帳號執行
- **用後即棄**：每個任務建立新的 sandbox（或從 snapshot 開始）
- **封鎖區網與 overlay 網路 egress**：封鎖 RFC1918、link-local 與 overlay 網路的位址範圍（可設定；Tailscale 為 `100.64.0.0/10`、`fd7a:115c:a1e0::/48` 與 MagicDNS `100.100.100.100`），sandbox 不得經由 host 的 overlay 介面連線；僅放行網際網路
- **僅掛載 worktree**：不掛載 host 的其他路徑；除任務所需的 bench device 外，不傳入任何裝置
- **資源限制**：以 cgroup 限制 CPU 與記憶體，資源較少（例如單板電腦）或散熱受限的 node 尤須設定

### 6.4 Sandbox 設計

**生命週期**：一個 `StartTask` 對應一個 sandbox，流程為建立 → 多次操作 → 交付 → 銷毀。Worktree 於任務結束後保留 7 天以便後續追問，之後刪除。

**程式碼進出：以 git bundle 進出**

1. Agent Host 以唯讀 token fetch repo，打包為 `git bundle`，經 hub 的 blob 暫存交給 node
2. Node 將其解開為該任務的 worktree，掛載至 sandbox 的 `/work`
3. 任務結束時，node 將新的 commit 打包為 bundle，經 hub 送回 Agent Host
4. Agent 發出 `GitHubPublish`；使用者於 client 預覽 diff 並以裝置金鑰簽章（或符合 auto mode grant 並通過 reviewer，見 6.8 節）後，Agent Host 以 GitHub 寫入 token push 並開 PR（見 6.10 節）

Node 永遠不接觸 GitHub token；公開 repo 亦採用相同流程。

**執行模型**：Agent loop 於 Agent Host 執行，每個 tool call 為一個經簽章的 typed 操作。

```rust
enum SandboxOp {
    Exec { argv: Vec<String>, cwd: PathBuf, timeout: Duration, stdin: Option<Bytes> },
    ReadFile { path: PathBuf },
    WriteFile { path: PathBuf, content: Bytes },
    ApplyPatch { diff: String },
    Deliver,                      // 將目前的 commit 打包為 bundle，經 hub 送回 Agent Host
}
```

- 每個任務維持一條持久 exec session，減少 Agent Host 與 node 之間的往返
- stdout / stderr 以串流回報，截斷後標記為不可信內容
- 每個操作皆經 `authorize()` 檢查並寫入稽核日誌

**隔離**

| 層級 | 做法 |
| --- | --- |
| 執行身分 | rootless Podman + user namespace；容器內的 root 對應至 host 上的無權限 uid |
| 權限 | `--cap-drop=ALL`、`no-new-privileges`、預設 seccomp；不掛載 container socket、不使用 `--privileged` |
| 檔案系統 | rootfs 唯讀；僅 `/work`（worktree）與 `/tmp`（tmpfs）可寫 |
| 網路 | 獨立 netns；nftables 僅放行網際網路（封鎖清單見 6.3 節，另封鎖 multicast）；DNS 使用公共 resolver，不使用區網 DNS 或 overlay 網路的 DNS |
| 資源 | cgroup v2 限制 CPU、記憶體與 pids；磁碟配額；每個操作設 timeout，每個任務設總時間上限 |
| Secret | 環境變數保持乾淨，sandbox 內不存在任何 token |
| 裝置 | 預設不傳入任何裝置；bench device 僅以 `--device` 傳入其 tty，不傳入 `/dev/bus/usb`（見 6.11 節） |
| 強化隔離（選用） | gVisor `runsc`（支援 arm64），待實際評估逃逸風險後決定是否採用 |

**映像**：以 Nix 建置 base image（`dockerTools`），每種 toolchain 一個，可重現、可固定版本。Sandbox 內仍可自行安裝套件。

**各 node 的實作**

| Node | Runtime | 備註 |
| --- | --- | --- |
| `node-dev`（Linux 單機版） | rootless Podman（`pots-node`） | Phase 2 的第一台 node；與 Agent Host 同機（見 3.6 節）；bench device |
| `node-linux`（x86_64 / arm64） | rootless Podman | Phase 2 後期加入；資源上限依主機規格設定 |
| `node-build`（選用，可為 WSL2） | rootless Podman | 若為 WSL，nftables 設定於 WSL 內，且閒置時會自動關閉；任務於 hub 的 node 佇列排隊 |
| `node-mac` | Linux：Apple `container` 或 OrbStack（需要 bench device 時僅能使用 OrbStack）；macOS：Tart VM（每個任務從 golden image clone） | Phase 4；可以單機版部署（見 3.6 節） |

### 6.5 跨邊界動作

跨邊界動作須經使用者裝置簽章，由 node 於 sandbox 外執行：

- `LanAccess { host, port, ttl }`：暫時為該 sandbox 新增一條 nftables 放行規則，到期自動撤除
- `HostRead { paths }`：以唯讀方式額外掛載 host 路徑

**逃逸時的損害範圍**：node 的金鑰（僅能簽署回報，無法偽造使用者的核准）、同一台 node 上其他任務的 worktree，以及 node 本身可連線的網路（網路 ACL 限制其僅能連線至 `agent-host:8443`，且連線後也僅具 node 身分）。Node 上不存在任何 secret，損害有限。單機版逃逸後所在的是使用者的日常機器，其損害範圍與緩解措施見 3.6 節；另可操作該 node 的所有 bench device。

### 6.6 Capability

```rust
struct Capability {
    agent: AgentId,
    node: NodeId,
    scope: Scope,             // Sandbox { image, limits, devices } | Boundary(BoundaryAction)  // LanAccess | HostRead
                              // devices 僅能取自 node 本地政策檔的 bench device（見 6.11 節）
    approval: ApprovalPolicy, // Auto | AskHuman | Never
    expires: Option<Timestamp>,
}
```

Node 端的 `Auto` 僅用於 `Sandbox` scope；`Boundary` scope 一律為 `AskHuman` 或 `Never`，不適用 auto mode（見 6.8 節）。

### 6.7 裝置簽章核准

跨邊界操作會向使用者的裝置發送核准請求（Phase 2 為桌面 client，Phase 4 起亦支援手機）。使用者同意時，裝置以裝置金鑰對「該指令的 hash」簽章；node 驗證簽章後才執行。即使 Agent Host（含 hub）遭入侵，亦無法偽造核准。

```rust
fn authorize(cmd: &SignedCommand, policy: &Policy, approval: Option<&SignedApproval>) -> Result<()> {
    verify(&cmd.agent_pubkey, &cmd.payload, &cmd.sig)?;       // 確認由該 agent 發出
    let cap = policy.find(cmd.agent, cmd.node, &cmd.action)?;  // 確認其具備此權限
    if let ApprovalPolicy::AskHuman = cap.approval {
        let a = approval.ok_or(Error::NeedsApproval)?;
        verify(&policy.owner_device_keys, &a.cmd_hash, &a.sig)?; // 確認使用者已簽署此指令
        ensure!(a.cmd_hash == hash(&cmd.payload));
    }
    Ok(())
}
```

核准公鑰清單（`owner_device_keys`）記載於 node 本地的政策檔，僅能於 node 上手動修改，不從網路更新。多人工作區改以 `ApprovalRule` 決定核准者（見第 8 章）。

### 6.8 自動核准（auto mode，僅限 `GitHubPublish`）

Auto mode 允許 reviewer 模型在使用者預先簽署的範圍（`AutoModeGrant`）內，代替使用者核准 `GitHubPublish`。**僅適用於 `GitHubPublish`**；node 跨邊界動作（`LanAccess`、`HostRead`）一律須使用者裝置簽章。

**為何僅限 GitHub**

- `GitHubPublish` 的簽章本即由 Agent Host 驗證，Agent Host 遭入侵時無法提供保護（見 2.3、6.10 節）。因此 auto mode 不擴大入侵情境的損害，新增的風險僅為「prompt injection 騙過 reviewer」，其範圍受 grant、`HARD_DENY`、token 權限與 branch protection 限制
- Node 跨邊界動作的簽章由 node 自行驗證，即使 Agent Host 遭入侵亦無法偽造；這是本設計最強的保證，若改為由 Agent Host 上的 reviewer 核准，此保證即不成立

**Grant**

```rust
struct AutoModeGrant {
    id: GrantId,
    repos: Vec<RepoId>,          // 須為 token 授權 repo 清單的子集
    branch_prefix: String,       // 例如 "agent/"；僅允許推送至此前綴的分支
    pr: AutoPrRule,              // None | DraftOnly
    max_diff_lines: u32,         // 單次 publish 的 diff 行數上限
    max_per_day: u32,            // 每日自動核准次數上限
    expires: Timestamp,          // 建議不超過 7 天
}
// 使用者裝置以裝置金鑰簽署 hash(AutoModeGrant)，publisher 以 owner_device_keys 驗證
```

- Grant 由使用者於 client 建立並簽署；可隨時於 client 撤銷，撤銷即時生效。Kill switch 亦會暫停所有 grant
- Reviewer 無法擴大或延長 grant；grant 的檢查全部由 publisher 的程式碼執行

**`HARD_DENY`（不得自動核准）**

由 publisher 的程式碼強制檢查，與 reviewer 的判斷無關；符合任一條件時退回 `AskHuman`：

- 目標分支不符合 `branch_prefix`，或為 repo 的預設分支、受保護分支
- PR 非 draft（grant 為 `DraftOnly` 時），或 PR 的目標分支非預設分支
- Diff 超過 `max_diff_lines`，或當日自動核准次數已達 `max_per_day`
- 修改敏感路徑：`.github/`、`CODEOWNERS`，以及 repo 設定檔另行列出的路徑（例如部署設定、lockfile）
- Grant 已過期、已撤銷，或 repo 不在 grant 內

Force push 與刪除分支不屬於 `GitHubPublish` 支援的操作，無論是否經簽章皆遭拒絕（見 6.10 節）。

**Reviewer**

- 與發起任務的 agent 分開呼叫，不共享 context；模型依 5.6 節設定
- 輸入：任務的原始指示（使用者簽章的訊息）、`base..head` 的 diff、PR 內容。Diff 與 PR 內容標記為不可信內容
- 輸出僅有 `Approve` 或 `Escalate { reason }`；`Escalate` 時退回 `AskHuman`，並於核准請求中附上理由
- 判斷依據：變更是否符合任務指示、是否夾帶與任務無關的變更（例如新增網路請求、修改建置腳本、外洩資料）

**流程**

1. Agent 發出 `GitHubPublish`
2. Publisher 尋找涵蓋該 repo 的有效 grant 並檢查 `HARD_DENY`；不符合時退回 `AskHuman`
3. Reviewer 審閱；`Escalate` 時退回 `AskHuman`
4. 通過後 publisher push 並開 PR；稽核日誌記錄 grant ID 與 reviewer 的判斷
5. Client 於 thread 與「已自動核准」清單中通知使用者（不阻擋）；使用者可事後關閉 PR、刪除分支或撤銷 grant

**損害範圍**：reviewer 遭 prompt injection 誤核准時，最多為在 grant 範圍內推送 `branch_prefix` 下的分支並開 draft PR；合併仍須使用者於 GitHub 上操作，且受 branch protection 限制。

持續統計每日的使用者簽章次數、自動核准次數與退回人工的比例，作為調整 grant 條件的依據。

### 6.9 Secret 與寫入出口

- Secret 永不進入 sandbox
- 所有服務 token 以加密形式納入版本控制（參考實作：sops / sops-nix），僅解密至 Agent Host；node 上不存在任何服務 token
- **寫入出口採白名單**：未列入下表的服務，token 僅具讀取權限，寫入（deploy、寄信、邀請、繳交作業）由使用者以自身帳號執行，agent 負責準備內容，client 提供一鍵交接
- 程式碼交付以 git bundle 進出，再由 Agent Host 經使用者簽章或 auto mode grant 後 push（見 6.4、6.8、6.10 節）

| 出口 | typed 操作 | 憑證位置 | 簽章驗證者 | Auto mode | 硬性限制 |
| --- | --- | --- | --- | --- | --- |
| GitHub（6.10 節） | `GitHubPublish` | Agent Host publisher | Agent Host（入侵時失效，已接受） | 可，限 grant 範圍（6.8 節） | 不支援 force push、刪除分支；token 不含 Workflows / Administration |

**新增出口的條件**：須定義上表各欄並新增對應章節與變更紀錄。影響範圍較大的出口（例如未來的 SSH）應優先由 node 執行並由 node 驗證簽章，使 Agent Host 遭入侵時仍無法偽造；並優先採用每次核准時簽發的短效憑證（例如短效 SSH certificate 搭配 `force-command`），而非長期金鑰。
- 盡可能使用範圍小、會過期的 token（例如單一 repo 的 deploy key）
- **已知例外**：推播所用的 APNs / FCM 金鑰屬於寫入憑證，但僅能對使用者自己的 app 發送空通知；外洩的損害為通知騷擾，無法偽造核准

### 6.10 GitHub 寫入（push 與開 PR）

實際使用上，由使用者手動 push 與開 PR 的成本過高，因此 GitHub 列入寫入出口白名單（見 6.9 節），目前為唯一的寫入出口。

**Token 條件**

- 使用 fine-grained personal access token 或 GitHub App installation token，僅授權明確列出的 repo；設定到期日並定期輪替
- 權限僅限 Contents: read/write、Pull requests: read/write、Metadata: read；**不授予** Workflows、Administration、Secrets 等權限。因此 agent 無法修改 `.github/workflows/`，此類變更須由使用者自行 push
- 與其他 secret 相同，僅解密至 Agent Host，由 Agent Host 內的 publisher 模組持有；不進入 LLM context、sandbox 或 node
- Token 涵蓋的 repo 須設定 branch protection：預設分支須經 PR 合併、禁止 force push 與刪除分支

**操作**

Agent 無法在 sandbox 內執行 `git push` 或 `gh pr create`，而是發出 typed 操作；push 與開 PR 合併為一次核准（使用者裝置簽章，或 auto mode，見 6.8 節）：

```rust
struct GitHubPublish {
    repo: RepoId,               // 須位於 token 授權的 repo 清單內
    branch: String,             // 目標分支；僅允許一般 push，不支援 force push 與刪除
    base: CommitId,             // fetch 時的 commit，client 以 base..head 呈現 diff
    head: CommitId,             // node 交付之 bundle 的 head，即使用者審閱的內容
    pr: Option<PrSpec>,         // { base_branch, title, body, draft }
}
```

**流程**

1. Node 以 `Deliver` 將 commit 打包為 bundle，經 hub 送回 Agent Host
2. Agent 發出 `GitHubPublish`；hub 將核准請求連同 bundle 送至使用者的裝置，client 呈現 repo、分支、`base..head` 的 diff 與 PR 內容
3. 使用者同意時，裝置以裝置金鑰簽署 `hash(GitHubPublish)`
4. Agent Host 的 publisher 驗證簽章來自 `owner_device_keys`、bundle 的 head 與簽章內容一致，再以寫入 token push 並開 PR；結果寫入稽核日誌

符合 auto mode grant 時，步驟 2–3 由 grant、`HARD_DENY` 與 reviewer 取代，publisher 驗證的是 grant 的簽章（見 6.8 節）。

**保護範圍**：裝置簽章可防範 prompt injection 讓 agent 自行 push 或開 PR。但簽章由 Agent Host 驗證，且 token 存放於 Agent Host，因此 **Agent Host 遭入侵時此簽章無法提供保護**（見 2.3 節）；此時的損害由 token 的 repo 範圍、權限與 branch protection 限制：入侵者可推送新分支與開 PR，但無法直接修改受保護的預設分支、修改 CI workflow 或讀取 repo secrets。

**未納入 token 範圍的 repo**：無法交付；agent 於頻道中說明，由使用者決定是否將該 repo 加入 token。

### 6.11 開發板（bench device）

Bench device 是專供 agent 使用的開發板（目前以 Arduino 為主），接於 node（常見為 `node-dev`），並登記於 node 的本地政策檔。其定位為「可能燒壞、損失可接受」的設備。

```rust
struct BenchDevice {
    id: DeviceId,
    by_id: PathBuf,           // /dev/serial/by-id/...，不使用 ttyACM0 等會變動的名稱
    usb: UsbMatch,            // vendor / product / serial，用於重新枚舉後重新對應
    labels: BTreeSet<Label>,  // 例如 board=arduino-uno、board=esp32
}
```

**核准**

- Bench device 屬於 sandbox 資源（`Scope::Sandbox` 的 `devices`），任務需要時自動附加，不需裝置簽章，亦不經 reviewer
- 其信任來自 node 本地政策檔：清單僅能於 node 上手動修改，Agent Host 遭入侵亦無法新增裝置
- 未登記的裝置一律無法使用；需要時由使用者於 node 上登記

**排程**：node 於 `NodeDescriptor` 回報各 bench device 的 label（例如 `board=arduino-uno`）；任務以 `TaskRequirements` 要求，由 6.2 節的 `schedule()` 選擇 node。同一個裝置同時只附加至一個任務。

**使用方式**

- 於 sandbox 內執行 `arduino-cli`（compile、upload、monitor）；core 與燒錄工具安裝於 sandbox 內，常用 core 可預載於 Nix base image
- 裝置以 `--device` 傳入，僅暴露 tty；不傳入 `/dev/bus/usb`，以縮小 host USB 驅動的攻擊面
- 不由 node 於 sandbox 外代為燒錄：`arduino-cli upload` 依 core 的 `platform.txt` 呼叫 avrdude、bossac、esptool 等工具，若於 sandbox 外使用 sandbox 提供的 core，等同於讓 sandbox 在 host 上執行任意指令；且 DTR 重置與 1200bps touch 須直接操作 tty
- 重新枚舉：SAMD、Leonardo 等板子進入 bootloader 時會以新的裝置節點重新出現。Node 以 udev 規則依 `UsbMatch` 追蹤，並以 device cgroup 規則放行該裝置的新節點（須實測）
- 暫不支援需要 USB 大量儲存或 raw USB 的燒錄方式（例如 RP2040 的 UF2、picotool）
- 裝置的附加與移除寫入稽核日誌

**macOS**：Apple `container` 不支援 USB passthrough；OrbStack 可將 serial 裝置轉發至 Linux，但於容器內存取仍有已知問題，須於 Phase 4 實測後確認。

**已接受的風險**

- 不可逆的操作無法於 tty 層阻擋，例如 ESP32 的 eFuse 可經 serial 燒寫；板子可能變磚。AVR 的 fuse 須以 ISP 燒錄器修改，無法僅經 serial bootloader 修改
- 板子所連接的外部硬體（馬達、電源等）造成的實體影響不在保護範圍內，由使用者自行確保接線安全

## 7. Connectors（郵件、行事曆、LMS）

Connector 一律採用 MCP 介面，於 Agent Host 上執行，且**僅提供讀取型 tool**。首批 connector 以下表服務為參考實作，其他同類服務依相同原則加入；個別 connector 無法使用時（例如機構未開放 API）可停用，不影響其他功能。寫入由使用者執行：agent 產出內容（信件草稿文字、行程建議、作業檔案），client 協助開啟對應頁面。此設計可直接重用現成的 MCP server（例如 moodle-mcp），自行開發者亦採用同一套介面（Rust 可使用 `rmcp`）。Token 由 Agent Host 上的 MCP 行程持有，不進入 LLM context。

| Connector | 驗證 | 讀取 | 寫入 |
| --- | --- | --- | --- |
| Gmail | Google OAuth，`gmail.readonly` | 信件、搜尋、標籤 | 不支援：Gmail 沒有「僅能建立草稿」的 scope，草稿由使用者貼上後自行寄出 |
| Google Calendar | Google OAuth，readonly | 行程、空檔 | 不支援：agent 提供建議，由使用者自行新增 |
| LMS（例如 Moodle） | 例如 Moodle Web Services token | 課程、作業、公告、成績 | 不支援。部分 LMS 的個人 token 本身可能具備繳交作業的權限，因此 MCP 僅暴露讀取函式（此為軟體層限制，Agent Host 遭入侵時不成立） |

### 7.1 Manifest

每個 connector 宣告自身的 tool 與影響範圍，直接作為 capability 的輸入。

```rust
struct ConnectorManifest {
    id: ConnectorId,
    transport: McpTransport,   // Stdio { command } | Http { url }
    tools: Vec<ToolSpec>,
}

struct ToolSpec {
    name: String,
    effect: Effect,            // Read | Write { reversible: bool, external: bool }
    untrusted_output: bool,    // 回傳內容是否可能含 prompt injection
}
```

### 7.2 不可信內容

- 信件內文、行事曆邀請描述與 LMS 公告皆可能夾帶 prompt injection，一律標記為 `untrusted_output`
- 由低權限的 reader agent 讀取並摘要後轉交；可操作機器或寄信的 agent 不直接讀取原文

### 7.3 同步方式

系統不開放任何公開 webhook 入口（不使用公開 tunnel，例如 Tailscale Funnel），一律由 Agent Host 主動輪詢：

- **Gmail**：以 `users.history.list` 進行增量同步（保存上次的 `historyId`），例如每 5 分鐘一次；`historyId` 過期而回傳 404 時，重新執行一次完整同步
- **Calendar**：以 `syncToken` 進行增量同步，例如每 10 分鐘一次
- **LMS**：排程輪詢（例如每 30 分鐘查詢新作業與公告）
- 代價是數分鐘的延遲；若需即時性，再評估 Gmail Pub/Sub 的 pull subscription（仍為主動外連，無需公開入口）

## 8. 多 host 與多人

Client 中的每個工作區對應一個 host，個人與實驗室工作區彼此獨立。連線與簽章協定無需修改，因為一個 host 本即可註冊多位使用者的裝置；需新增的是角色、指揮權、核准者與費用歸屬。資料模型自 Phase 1 起即支援多人，多人功能則於 Phase 5 實作。

### 8.1 工作區

- 工作區由 host URL、host 公鑰指紋與一組身分構成。Client 為每個工作區分別保存裝置金鑰、本地資料庫與通知設定
- 工作區之間不共享任何資料，個人 agent 不會自動出現在其他工作區
- 加入新 host：掃描 host 發出的邀請（含 host URL 與 host 公鑰指紋），首次連線即核對指紋；之後指紋改變時拒絕連線並發出警告
- 他人的 host 位於他人的 overlay 網路：須由對方以 overlay 網路的跨網路分享功能（例如 Tailscale 機器分享）分享 host，或反之（待決，Phase 5 決定）
- 由他人管理的 host，介面固定顯示「管理員可見內容」

|  | 個人工作區 | 實驗室工作區 |
| --- | --- | --- |
| Agent Host 管理者 | 使用者本人 | 實驗室管理員 |
| 可見明文者 | 使用者、LLM provider | 所有成員、host 管理員、LLM provider |
| 費用 | 使用者的 API 帳戶 | 實驗室帳戶，可依成員設定上限 |
| 核准者 | 使用者 | 依動作而定（自動、發起人、管理員） |
| 使用者的個人記憶 | 有 | 無 |

### 8.2 資料模型（自 Phase 1 採用）

```rust
enum MemberKind { Human { user: UserId }, Agent { agent: AgentId } }

enum Role { Owner, Admin, Member, Guest }

struct Member {
    id: MemberId,
    kind: MemberKind,
    role: Role,
    devices: Vec<DevicePubKey>,  // 人類可擁有多台裝置
}

// agent 接受哪些成員的指令
struct CommandPolicy {
    agent: AgentId,
    accept_from: Vec<Principal>, // Role(Role) | Member(MemberId)
}

// 各類動作的核准者
struct ApprovalRule {
    action: ActionPattern,
    approvers: Approvers,        // Auto | Initiator | AnyOf(Vec<MemberId>) | Role(Role) | Quorum { n: u8, of: Vec<MemberId> }
}
```

- Node 的 `authorize` 不再僅比對單一組 `owner_device_keys`：先依 `ApprovalRule` 找出核准者，再驗證簽章來自其任一台裝置
- 個人工作區為僅有一條規則的特例：所有動作由 Owner 核准

### 8.3 多人情境下的 agent 行為

- 回覆時 @ 發起人；arbiter 的 relevance gate 納入「提問者是誰」
- 稽核紀錄載明來源：由哪位成員透過哪個 agent 發起
- 兩位成員下達矛盾指令時，agent 不自行選擇，而於頻道中請雙方或管理員決定
- 工作區記憶由所有成員共享，個人記憶不帶入
- 個人 agent 僅能以 Guest 身分加入他人的工作區：僅取得 brief，不得下達 node 指令，亦不得邀請成員。連線方面，使用者的 Agent Host 以 host 金鑰連線至對方的 hub，訪客 agent 掛載於其下，訊息附帶 agent 的作者簽章

### 8.4 費用

- 每個工作區擁有獨立的 provider 帳戶與預算
- 實驗室工作區可進一步細分：設定每位成員的每日上限，花費計入發起人名下

## 9. 失控防護與稽核

Agent 之間的互相對話最容易失控，因此防護機制自 Phase 1 即須具備，而非事後補上。

| 機制 | 初始值（寬鬆） | 觸發後 |
| --- | --- | --- |
| Debounce | 頻道安靜 2 秒 | 合併處理人類連續送出的訊息 |
| Hop limit | 無人類介入時，agent 之間連續 12 則 | 暫停該頻道的 agent 發言，並 @ 使用者 |
| 冷卻時間 | 同一 agent 每 10 分鐘最多 20 則 | 略過該次發言 |
| 頻道預算 | 每頻道每日 US$10 | 停止新的 LLM 呼叫，待使用者追加預算 |
| 任務預算 | 每個 coding 任務 US$5 | 暫停任務，於 thread 中詢問使用者是否繼續 |
| 全域預算 | 每日 US$30 | 暫停所有 agent |
| 成員上限 | 每頻道 8 個 agent | 拒絕邀請 |
| Kill switch | 使用者下達「全部停止」或按下停止鈕 | 取消進行中的 LLM 呼叫、暫停 node 任務 |

上述數值皆存放於設定檔，將於實際運行一至兩週後依用量調整。Relevance gate 與摘要使用 Flash 等級模型，預算主要用於旗艦模型的 coding 任務。

### 9.1 Prompt injection 防護：讀寫分離

- 讀取不可信內容（網頁、email、外部 issue）的 agent 僅具低權限：不得下達 node 指令、不得邀請成員
- 可操作機器的 agent 不直接讀取外部內容，而是透過摘要轉交
- Sandbox 內的輸出亦視為不可信內容，不會自動提升權限

### 9.2 稽核日誌

- Append-only，每筆紀錄包含前一筆的 hash（hash-chained），以簡單方式達成 tamper-evident
- 紀錄內容：agent、node、指令、核准者與結果
- 每台 node 與 Agent Host 各自寫入；node 的日誌同步回 Agent Host，再隨備份以 recovery key 加密後上傳至遠端物件儲存
- Client 可重播「誰在何時對哪台機器執行了什麼」

## 10. 開發階段規劃

開發策略是優先確立安全邊界：Phase 1 即提供多機部署的 Agent Host 參考部署，連線身分與簽章自第一天起即具備；Phase 3 補齊多裝置與復原機制。P1 的單 agent 整合測試直接使用真實 API，不以 mock 作為前置（單元測試仍可使用 mock）；M1.3 包含首個 adapter 與最小預算檢查、成本記帳、輸出 token 上限、timeout、CLI 取消／停止機制，完整 provider 與防護分別於 M1.5、M1.7 擴充，自動多 agent 互聊待 M1.6、M1.7 完成後啟用。各階段的詳細範圍與里程碑見 `plans/`。

&#91;embedded content: 開發路線 · 5 個階段、4 個關卡\]

| Phase | 主題 |
| --- | --- |
| 1 | 骨架 |
| 2 | 機器操控 |
| 3 | 多裝置與復原 |
| 4 | 多專案與個人助理 |
| 5 | 多人工作區 |

每個階段結束前皆須通過該階段關卡的完成標準。

## 11. 決策紀錄與風險

### 11.1 已決定事項

| 議題 | 決定 |
| --- | --- |
| Agent Host 離線 | 接受所有 agent 同時離線，不設備援 |
| 部署角色 | 多機部署：Agent Host 部署於 Agent Host 主機（獨立帳號 `pots`），node 為 `node-linux`（Phase 2 後期）、選用的 `node-build`、`node-mac`（Phase 4），全部以 overlay 網路串接。單機版：同一台機器兼任 Agent Host 與 `node-dev`（Phase 2 第一台 node）及桌面 client。角色與實際機器的對應由部署者決定 |
| 部署方式 | 多機部署與單機版並存；單機版分為 `pots-host`、`pots-node` 與使用者三個 OS 身分，提供 Linux（Phase 2；參考打包為 NixOS module）與 macOS（Phase 4）版本（見 3.6 節） |
| 開發板 | 登記於 node 本地政策檔的 bench device 自動附加至 sandbox，於 sandbox 內以 `arduino-cli` 燒錄與讀寫 serial；僅傳入 tty（見 6.11 節） |
| Gateway | 以 hub 模組形式內建於 Agent Host，以降低部署複雜度 |
| E2EE | 不採用：路徑上不存在不可信中繼，MLS 無法防範新的對象（見 2.1 節） |
| 認證 | 裝置 / node / host 金鑰雙向 challenge-response；不使用 SSO / forward-auth，不信任 proxy header 或來源 IP |
| 入口 | Agent Host 主機上的 pots 專用 port（經反向代理或由 hub 直接 TLS），僅對 overlay 介面開放；搭配網路 ACL 與節點加入簽章 |
| 外部通知 | 不開放公開入口；郵件、行事曆、LMS 等 connector 皆由 Agent Host 輪詢 |
| Secret 與寫入 | 以加密形式納入版本控制，僅解密至 Agent Host；寫入出口採白名單，目前僅 GitHub（見 6.9、6.10 節）；其他寫入外部服務由使用者執行；不設 vault |
| Agent Host 遭入侵 | 視同全面失守，予以接受；損害上限由「除 GitHub 外不存在寫入憑證」保證，GitHub 部分由 token 範圍與 branch protection 限制 |
| 外部服務 | 採用自訂 connector，優先實作郵件、行事曆與 LMS（參考實作：Gmail、Google Calendar、Moodle；見第 7 章） |
| 模型 | 依任務、頻道、agent 與全域設定選用；建議日常使用 Flash 等級模型，撰寫程式碼使用旗艦等級模型（見 5.6 節） |
| 產品與程式碼重用 | pots 為獨立產品，可沿用既有 provider 與 agent loop 實作，但須符合 pots 的安全邊界；採 `AGPL-3.0-only`，初期以固定 Git revision 依賴重用 GitHub [`mozufu/pirc`](https://github.com/mozufu/pirc)，來源與邊界見 `plans/phase-1-skeleton.md` §2.1 |
| Provider API 與 ZDR | 支援可設定端點的 OpenAI-compatible / Anthropic-compatible API，保留 OAuth；ZDR 非硬條件，頻道預設不要求，僅於明確啟用時強制檢查 |
| P1 使用定位 | 僅作測試驗證，不投入日常使用；資料庫可暫不加密，後續整檔加密規劃不變 |
| 首台 Owner bootstrap | 主機端本機管理 CLI 手動登記裝置公鑰，client 核對主機端提供的 host 指紋後釘選；裝置私鑰不離開 client，不接受首個網路連線者自動取得 Owner |
| P1 API 測試 | M1.3 直接使用真實 API，首個 adapter、最小預算與停止機制一併就緒；mock 非整合前置，仍可用於單元測試 |
| 協定 | 自建協定；連線身分 + 作者簽章（見第 4 章） |
| 部署 | Phase 1 即提供多機部署的 Agent Host 參考部署；部署設定建議納入版本控制 |
| 防護數值 | 採用寬鬆的初始值（見第 9 章） |
| 核准疲勞 | `GitHubPublish` 採用 auto mode（使用者簽署的 grant + reviewer + `HARD_DENY`）；node 跨邊界動作一律須裝置簽章（見 6.8 節） |
| 程式碼交付 | Git bundle 經 hub 送回 Agent Host；使用者於 client 預覽 diff 並簽章（或符合 auto mode grant）後，由 Agent Host push 並開 PR（見 6.8、6.10 節） |
| 多 host 與多人 | Client 以工作區切換 host；資料模型自 Phase 1 支援多人，功能於 Phase 5 實作（見第 8 章） |

### 11.2 待確認事項

- [ ] 若要啟用 `require_zdr`，須先確認所選 provider 端點與帳號的 ZDR 資格；此為選用限制的前提，不阻擋一般頻道使用
- [ ] LMS connector 的可行性：以 Moodle 為例，機構是否啟用 Web Services、個人能否取得 token
- [ ] 選定參考的 LMS MCP 實作（例如 moodle-mcp），決定直接採用或以 Rust 重寫
- [ ] Bench device 重新枚舉（SAMD、Leonardo 的 1200bps touch）在 rootless Podman 下能否以 udev + device cgroup 規則處理
- [ ] macOS 單機版：OrbStack 能否於 role account 下以 launchd 執行；容器內存取轉發的 serial 裝置是否穩定；secret 的解密方式（nix-darwin + sops）

### 11.3 風險

| 風險 | 影響 | 緩解措施 |
| --- | --- | --- |
| 同機服務冒充身分 | Agent Host 主機上任何程式皆可連線至 loopback；信任來自 `127.0.0.1` 的身分 header 的設計可被直接冒充，proxy 設定错誤（例如未傳送 proxy secret）亦常見 | Hub 不讀取任何身分 header，僅認可金鑰；關卡測試納入「自 loopback 直連冒充失敗」 |
| Node 橫向連線至同機服務 | 遭攻破的 node 經 Agent Host 主機的其他 port（例如 443）存取其他同機服務 | pots 使用專用 port；ACL 僅放行 node → `agent-host:8443` |
| Overlay 網路控制面 | 遭入侵時可插入惡意節點 | 節點加入簽章（例如 Tailnet Lock）；不支援此機制的 overlay 網路須自行評估風險 |
| 行動裝置無法連線至 host | 未連上 overlay 網路時無法接收內容或進行核准 | 推播仍可送達（僅為空通知）；將 overlay 網路設為常駐或 on-demand |
| 核准疲勞 | 使用者未審閱即同意 | push 與開 PR 合併為一次簽章；範圍明確的 `GitHubPublish` 由 auto mode 處理；統計每日簽章次數 |
| Reviewer 遭 prompt injection 誤核准 | 在 grant 範圍內推送 agent 分支並開 draft PR，內容可能含惡意變更 | Grant 限制 repo、分支前綴、draft、diff 大小與次數；`HARD_DENY` 由程式碼檢查；合併仍由使用者操作並受 branch protection 限制；事後通知與稽核；grant 可隨時撤銷 |
| GitHub 寫入 token 外洩 | Agent Host 遭入侵時，入侵者可在 token 涵蓋的 repo 上 push 分支與開 PR（已接受） | Fine-grained token 僅限指定 repo、到期輪替、不授予 Workflows / Administration；預設分支 branch protection；稽核日誌比對 GitHub 上的 push 紀錄 |
| Agent 互聊失控 | 成本暴增、頻道遭洗版 | Hop limit、預算與 floor lock 自 Phase 1 起具備 |
| Agent Host 單點故障 | 整個系統停擺（已接受） | 加密備份至遠端物件儲存，可於另一台機器快速重建 |
| Agent Host 與對外服務同機（若採用） | 對外服務（尤其以 root 執行的反向代理）遭攻破時，可讀取所有訊息、記憶與 connector 資料 | 建議以專用主機部署；否則以獨立帳號、僅綁定 loopback、記憶庫加密；除 GitHub 外 token 全部唯讀；核准仍須裝置簽章 |
| 單機版 sandbox 逃逸 | 逃逸者取得該機器上 `pots-node` 的權限，可嘗試連線至本機 loopback 服務、操作 bench device | `pots-node` 無法讀取使用者 home 與 `pots-host` 狀態；nftables `meta skuid` 限制其連線；node 政策檔由 root 擁有 |
| 開發板變磚 | Agent 燒寫 eFuse 或錯誤的 bootloader 設定（已接受） | 僅專供 agent 使用的板子可登記為 bench device |
| Node 休眠或離線（例如 `node-mac`、`node-build`） | 任務停滯 | Hub 的 node 佇列設定 TTL；常駐 node 設定不休眠（例如 macOS 的 `pmset`） |
| 低階硬體的儲存損壞（例如單板電腦的 SD 卡） | Agent Host 或 node 故障 | 使用 SSD；Agent Host 另有加密備份 |

### 11.4 參考資料

- [Tailscale Tailnet Lock](https://tailscale.com/docs/features/tailnet-lock)
- [Tailnet Lock white paper](https://tailscale.com/docs/concepts/tailnet-lock-whitepaper)
- [Gmail：Synchronize clients](https://developers.google.com/workspace/gmail/api/guides/sync)
- [Calendar：Synchronize resources efficiently](https://developers.google.com/workspace/calendar/api/guides/sync)

## 附錄 A：變更紀錄

| 日期 | 變更 | 理由 |
| --- | --- | --- |
| 2026-10-27 | 文件泛化：移除特定硬體、發行版與既有基礎設施的描述，改以部署角色與參考實作表示；`node-arm` 更名為 `node-linux`、`node-x86` 更名為 `node-build`；Tailscale、Traefik、sops、R2 等改為可替換的參考實作；connector 以郵件 / 行事曆 / LMS 表示；預設模型改為模型等級 | 設計與計畫不應綁定特定使用者的部署方式；安全性質維持不變 |
| 2026-10-26 | 確認 pots 採 `AGPL-3.0-only` 並新增根目錄 `LICENSE`；初期以固定 Git revision 依賴重用 pirc | 依使用者確認採用與來源相同的授權，並以固定版本維持獨立交付與可重現依賴 |
| 2026-10-26 | 確認首台 Owner 由主機端 CLI 手動登記公鑰、client 核對 host 指紋；M1.3 直接使用真實 API，提前包含首個 adapter、最小預算與停止機制 | 確立第一台裝置的信任來源，並依使用者選擇直接以真實 API 驗證整合，不先實作 mock 回覆迴圈 |
| 2026-10-26 | 確認 pots 為獨立產品，可沿用既有 provider 與 agent loop；API 支援 OpenAI-compatible / Anthropic-compatible，保留 OAuth；ZDR 改為可選頻道限制；P1 不投入日常使用，可暫不加密資料庫 | 依開工前決策調整產品定位、API 範圍與資料保護承諾，不將 ZDR 作為開工前提 |
| 2026-10-26 | Gateway 由 Cloudflare（workers-rs + Durable Objects）移入 Agent Host，成為 hub 模組；取代原「正式 gateway 維持於 Cloudflare」 | 降低部署複雜度；所有元件改位於私有 overlay 網路內 |
| 2026-10-26 | 取消 E2EE（MLS）；原「MLS leaf 粒度」決定一併取消 | Gateway 移入 Agent Host 後，路徑上不存在不可信中繼（見 2.1 節） |
| 2026-10-26 | 原訊息兩層簽章的外層（連線主體簽署 envelope）改由連線時的雙向 challenge-response 取代；作者簽章保留 | 隨 gateway 移入 Agent Host 而簡化 |
| 2026-10-26 | 原「Phase 1–2 僅以 `wrangler dev` 執行」改為 Phase 1 即部署至自架的 Agent Host 主機 | Gateway 不再位於 Cloudflare |
| 2026-10-26 | 路線圖 Phase 3 由「E2EE」改為「多裝置與復原」 | 隨 E2EE 取消調整 |
| 2026-10-26 | 取消 auto mode（reviewer 模型代為核准、`AutoModeGrant`、`HARD_DENY` 清單） | 改為 token 全部唯讀、寫入由使用者執行後，核准疲勞的前提不成立（見 6.8 節） |
| 2026-10-26 | 「系統內不存在任何寫入憑證」改為「不建議存放寫入憑證」，允許 GitHub 寫入 token 存放於 Agent Host；新增 `GitHubPublish` 與 6.10 節；移除「使用者預覽 diff 後自行 push」的交付流程 | 實際使用上非常需要 push 與開 PR，手動交接成本過高。代價：Agent Host 遭入侵時可在 token 涵蓋的 repo 上 push 與開 PR |
| 2026-10-26 | 「不建議存放寫入憑證」改為「寫入出口白名單」（6.9 節），目前僅 GitHub；新增出口的條件，未來影響範圍大的出口（例如 SSH）優先由 node 驗證簽章 | 實務上寫入憑證難以避免，改為逐一定義每個出口的憑證位置、驗證者與限制 |
| 2026-10-26 | 恢復 auto mode，僅限 `GitHubPublish`：使用者簽署的 `AutoModeGrant`、由 publisher 強制檢查的 `HARD_DENY`、reviewer 模型；node 跨邊界動作不適用（6.8 節） | `GitHubPublish` 的簽章本即由 Agent Host 驗證，auto mode 不擴大入侵情境的損害；node 跨邊界動作由 node 自行驗證簽章，維持此保證 |
| 2026-10-26 | 新增單機版（3.6 節），與多機部署並存；第一台 node 由獨立的 Linux node（現稱 `node-linux`）改為單機版 `node-dev`，獨立 node 改於 Phase 2 後期加入；開發機由「不作為 node」改為「單機版時兼任 Agent Host 與 node」 | 開發板接於開發機，需要不必分別部署 hub 與 node 的版本；另需要 macOS 版本 |
| 2026-10-26 | 新增 bench device（6.11 節）：登記於 node 本地政策檔的開發板自動附加至 sandbox，於 sandbox 內以 `arduino-cli` 燒錄與讀寫 serial | 開發板工作需反覆燒錄與讀取 serial，逐次簽章不可行；板子專供 agent 使用，損失可接受；由 node 於 sandbox 外代為燒錄反而會執行 sandbox 提供的 core |

**已知文件問題**：「系統架構」與「開發路線」兩張圖仍為 Cloudflare gateway 版本，且路線圖的 Phase 3 仍標示為 E2EE；兩張圖皆未包含單機版與 `node-dev`，待更新。
