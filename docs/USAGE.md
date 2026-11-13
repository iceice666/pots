# Pots 使用說明

Pots 是一個像 Slack / Discord 的桌面聊天 app，隊友是 AI agent。你是專案的擁有者（Owner）：負責做決定、排優先順序；PM 幫你規劃、分派工作，Engineer 寫 code，Reviewer 審 code。

## 啟動

```bash
cargo run --release            # 啟動 app（使用你的工作區）
cargo run --release -- --mock  # 只看示範資料，什麼都不會存
```

第一次啟動時會建立資料目錄 `~/.pots`（可以用環境變數 `POTS_HOME` 改位置），裡面有：

| 檔案 | 內容 |
| --- | --- |
| `config.toml` | provider、agent、專案 repo、頻道、sandbox 設定 |
| `keys.toml` | 在 Settings 裡填的 API key（只有你自己可讀，不要分享或 commit） |
| `workspace.json` | 頻道、訊息、thread |
| `tasks/` | 每個任務的 git clone（Engineer / Reviewer 在這裡工作） |

### Linux 需要的系統套件

agent 做的互動式頁面（Page artifact）是用 WebKitGTK 顯示的，所以在 Linux 上 build 和執行都需要它，就算從來沒打開過頁面也一樣：

```bash
# Debian / Ubuntu（build 用；只執行的話裝不帶 -dev 的 libwebkit2gtk-4.1-0、libgtk-3-0 即可）
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev \
  libxkbcommon-x11-dev libfontconfig-dev pkg-config
```

用 `nix develop` 的話，`flake.nix` 已經包含這些。

- 少了這些函式庫，app 會直接**啟動不了**（執行檔動態連結 webkit2gtk、gtk3、libsoup、javascriptcore）。
- 頁面只能在 **X11 或 XWayland** 下顯示。在純 Wayland 視窗裡，頁面的位置會顯示錯誤訊息，app 其他部分照常運作。

### 沒有 API key 想先試玩

```bash
cargo run -p pots-mock
```

它會在 `127.0.0.1:4000` 開一個假的 LLM server，並印出要貼進 `config.toml` 的設定。在聊天裡打 `!tool NAME {json}` 可以讓它呼叫工具，`!error 529 msg` 則讓請求失敗，方便測試。

## 設定

點左上角的工作區名稱打開選單：
- **Settings…**（⌘,）：
  - **Providers**：管理 LLM provider 和 API key。key 存在 `keys.toml`，改完立即生效。
  - **Agents**：選每個 agent 用哪個 provider、哪個模型。輸入模型名稱時會列出 models.dev 上這個 provider 有的模型和價格（`.` 和 `-` 視為相同，打 `claude-opus-5-5` 也找得到 `anthropic/claude-opus-5.5`）；名稱不在清單上會標紅。存檔後從該 agent 的下一則回覆開始生效，不用重開。
- **Open config.toml**：用系統編輯器打開設定檔。新增或移除 agent 要**重開 app** 才會生效。

### Provider

每個 provider 走以下其中一種 API 格式：
- `api = "anthropic"`：Anthropic Messages API
- `api = "openai"`：OpenAI Responses 或 Chat Completions。預設會先試 `/responses`，失敗再改用 `/chat/completions`；也可以用 `endpoint = "responses"` / `"chat"` 指定。

相容的 proxy、gateway、本機模型都可以用。API key 的讀取順序：`api_key` → `keys.toml` → 環境變數 `api_key_env`。

### 用 Claude 訂閱（Claude Code）

`api = "claude-code"` 不走 API key，而是讓 agent 透過你自己的 Claude Code CLI（`claude -p`）回覆，用的是你的 Claude 訂閱額度。

```toml
[providers.claude]
api = "claude-code"
# command = "~/.local/bin/claude"   # 找不到 claude 時才需要設
```

然後在 Settings → Agents 把 agent 的 provider 換成 `claude`，模型填 `claude-opus-5-5`、`sonnet` 這類 Claude Code 認得的名稱。

- **沒有 workspace 的 agent（例如 PM）**：在這台電腦上執行 `claude`，用你目前的登入（先在終端機跑一次 `claude` 登入）。只開放 Pots 的聊天工具，不載入你自己的 Claude Code 設定、hook 和 MCP server。
- **有 workspace 的 agent（Engineer、Reviewer）**：在它的容器裡執行 `claude`，用 Claude Code 自己的 Bash、Read、Edit 等工具。容器沒有 `claude` 的話，第一次會用官方安裝腳本裝進去，想省時間可以直接做進 image。容器看不到這台電腦的登入，所以要在終端機跑 `claude setup-token`，再把印出來的 token 貼到這個 provider 的 Token 欄位（存在 `keys.toml`）。
- Pots 的工具（`ask_owner`、`pass`、`start_task`、`request_merge` 等）透過一個只活在這次回覆期間的本機 MCP server 提供給 Claude Code，在 Claude Code 裡的名稱是 `mcp__pots__<名稱>`。
- Pots 會把 `ANTHROPIC_API_KEY` 這類變數從 `claude` 的環境裡拿掉，避免改走 API 計費；訊息下方只顯示 token 數，不顯示金額。
- `max_steps` 是 Claude Code 的 `--max-turns`。按 Stop 會連容器裡的 `claude` 一起停掉。
- 依照 Anthropic 的條款，訂閱登入只能用在你自己的電腦、自己使用；不要把 token 或這種設定給別人用。

### Agent

預設只有 PM。要讓 agent 改 code，把 Engineer 和 Reviewer 取消註解，並告訴它們要改哪個 repo：可以在 app 裡替每個頻道設定 repo（見下方「頻道」），或設定 `[project]` 當作沒設 repo 的頻道和 DM 的預設值（不是必填）：

```toml
[project]
repo = "~/code/my-project"   # agent 預設要 clone 的 git repo

[sandbox]
image = "rust:latest"        # 容器 image，要有 git 和你的建置工具
# docker = "docker"          # docker（或相容工具）的 CLI 路徑
```

每個 agent 的常用欄位：

| 欄位 | 說明 |
| --- | --- |
| `name` | @mention 時用的名字 |
| `provider` / `model` | 用哪個 provider 和模型（也可以在 Settings → Agents 改）。OpenRouter 的模型名稱是 `lab/model`，例如 `anthropic/claude-opus-5.5` |
| `system_prompt` | 角色指示 |
| `workspace` | `"write"`：有自己的 clone 可以改 code、commit；`"review"`：每次被叫到時重新 checkout 要審的 branch；不填：沒有 workspace，只能協調（PM 就是這種，會多 `start_task`、`post_in_thread` 和 `check_merge_conflicts` 三個工具）。有 workspace 的 agent 在任務 thread 裡會多一個 `task_done`（見「PM 的回報」） |
| `history` | 能看到最近幾則訊息，預設 40 |
| `max_steps` | 每次回覆最多呼叫模型幾次（工具迴圈） |
| `max_tokens`、`color`、`image`、`price`、`extra_body` | 見 `config.toml` 裡的註解 |

有 workspace 的 agent 需要 Docker。

### Worker

Worker 是給 PM、Engineer、Reviewer 共用的跑腿，適合放便宜的模型，讓這三個 agent 放比較強的模型。它不是聊天室成員，不能 @，也不會發訊息：agent 用 `ask_worker` 工具交代一件查資料的事（讀 code、找東西在哪、盤點 repo、跑指令看輸出、讀頻道），Worker 做完把報告當成工具結果交回去。

```toml
[worker]
provider = "openrouter"
model = "deepseek/deepseek-v4.1-flash-1017"
# 選填：system_prompt、max_steps（預設 30）、max_tokens、image、price、extra_body
```

也可以在 Settings → Agents 最下面的 **Worker** 設定、更換 provider 和模型，或移除它，不用重開 Pots。

- **每次都是全新的**：看不到聊天內容，只看到 agent 交代的那件事。
- **拋棄式、唯讀**：在任務 branch 最新 commit 的獨立 checkout 裡工作（`tasks/<任務>/scratch-worker-<agent>`）；還沒有任務 branch 時用專案的預設 branch。沒 commit 的東西它看不到。它寫的檔案、做的 commit 下次被叫時就清掉，不會進到任務 branch，也不能發 merge request。它只有 `bash`、`read_file`、`list_channels`、`read_channel`，不能開任務、問你或 @ 人。
- **費用**：Worker 用的 token 用 Worker 模型的價格計算，加在叫它的那則回覆上，例如 `6.3k in · 1.2k out · Worker 30k in · 2k out · $0.01`。
- 沒有 workspace 的 PM 也能叫 Worker 看 repo：預設看目前頻道的 repo，也可以指定 `channel`，改看別的頻道的 repo。

```toml
[team]
max_agent_turns = 6   # agent 連續發幾則訊息後暫停，等你回覆
```

## 頻道、DM 和 thread

### 誰會回你

| 你在哪裡發訊息 | 誰會回 |
| --- | --- |
| 跟某個 agent 的 DM | 那個 agent，不用 @ |
| 頻道（頂層） | **只有被 @ 的 agent**。沒有 @ 就沒人回 |
| thread | 被 @ 的 agent；沒有 @ 的話，由這個 thread 裡上一個發言的 agent 回 |

agent 之間也會互相 @ 來交接工作（例如 Engineer 做完會 @Reviewer）。連續 `max_agent_turns` 則都是 agent 的訊息後，它們會暫停，等你回一句話才繼續。

寫在 `` `@PM` `` 這種 code 裡的 @ 不算 mention。

### 頻道

- 點側欄 **Channels** 旁邊的 **+** 新增頻道，點頻道標題旁的 **⚙** 修改。每個頻道可以設定名稱、主題和 **repo**。
- 頻道裡（包括它的 thread）的工作會 clone 這個頻道的 repo；沒設定的話用 `[project]` 的 repo；兩者都沒有的話，Engineer 在這裡沒有 workspace。
- 也可以在 `config.toml` 用 `[[channels]]` 定義頻道。啟動時會自動建立，而且這裡寫的 topic / repo 會蓋過 app 裡的設定。

```toml
[[channels]]
name = "backend"
topic = "API server"
repo = "~/code/api"
```

### Thread

開 thread 有兩種方式，都是在右側面板取名、送出第一則訊息後才真正開出來：

- **直接開：** 點頻道標題列右邊的 🧵（Create Thread）。不用先發頻道訊息；送出後頻道裡會出現一則以 thread 名稱為內容的起頭訊息（沒取名就叫 New Thread）。
- **從訊息開：** 滑鼠移到頻道訊息上，點 🧵，thread 就掛在那則訊息底下。

PM 用 `start_task` 分派工作時也會自動開 thread。

每個 thread 是一個獨立任務：
- 有自己的 git clone（`~/.pots/tasks/t<編號>-<名稱>`）和 branch（`pots/t<編號>-<名稱>`），不會跟其他任務互相干擾。
- agent 只看到這個 thread 自己最近的訊息，上下文不會被頻道裡的其他對話擠掉。

**結案（Archive）：** 用不到的 thread 可以封存，讓它從左側欄消失。滑鼠移到左側欄的 thread 上點 ✓，或在 thread 面板標題列點 **Archive**。封存只是藏起來：訊息、clone 和 branch 都還在，頻道裡起頭訊息下面的 thread 卡片會標上 Archived，點進去按 **Unarchive** 就拿回來。有人（包括 agent）在封存的 thread 裡發新訊息時，它會自動回到側欄。

Engineer 用 `task_done` 回報任務完成（status 是 done），或你按 **Approve** 收下任務的 merge request 時，Pots 會自動封存那個 thread，結果由 PM 到 #general 跟你回報。卡住（stuck）、還有等你回答的選擇卡或 merge request、或是 `task_done` 時沒有 PM 可以回報，thread 會留在側欄。

### DM

- 不用 @，講什麼那個 agent 都會回。
- **其他 agent 看不到 DM**：PM 的 `read_channel` / `list_channels` 會跳過 DM，所以在 DM 裡講定的事，PM 不會知道。
- 在 DM 叫 Engineer 改 code 時，用的是 `[project]` 的 repo，而且整個 DM 從頭到尾共用同一份 clone。

## 建議的用法

- **日常：在 #general @PM。** 談規劃、優先順序、要做什麼。PM 會先讀相關頻道掌握狀況，再用 `start_task` 把工作開成對應頻道裡的 thread，指派給 Engineer。
- **問 repo 裡有什麼：直接問 PM。** 有設定 `[worker]` 的話，PM 會叫 Worker 去查再回答你，不會開任務給 Engineer。
- **深入討論一件事：開 thread。**
  - 跟某個 repo 的 code 有關 → 在那個頻道（例如 #backend）開。
  - 偏規劃、跨 repo、還沒落到 code → 在 #general 開，談定了再請 PM 開到對應的頻道。
- **DM：** 私下問的小問題、一次性的事，或還不想讓團隊知道 / 動手的事。結論要讓 PM 知道的話，記得到 #general 講一聲。
- 不要在頻道頂層叫 Engineer 寫 code：同一個頻道頂層的所有工作會共用一份 clone（`c-<頻道名>`）。請開 thread。

## 一個任務的流程

1. 你在 #general 跟 @PM 討論，決定要做什麼。
2. PM 在對應頻道開 thread，貼上任務說明並 @Engineer。
3. Engineer 在自己的容器裡改 code、跑測試、commit，然後 @Reviewer。
4. Reviewer checkout 那個 branch 來審：
   - 需要修改 → @Engineer，回到第 3 步。
   - 沒問題 → 呼叫 `request_merge`，thread 裡出現 🔀 **Merge request** 卡片，列出這個 branch 的 commit。同時 Pots 會叫 PM 到 #general 跟你回報哪個任務可以收了（見下方「PM 的回報」）。
5. 你按 **Approve**：Pots 先把 branch 以同名 branch（`pots/...`）fetch 到專案 repo，接著在下面這些條件**全部**成立時，直接把它 merge 進你 checkout 的預設 branch（等同在 repo 裡跑 `git merge --no-edit pots/...`，可以 fast-forward 就 fast-forward，不行就用 git 預設訊息建 merge commit）：
   - 專案 repo 有 working tree（不是 bare repo），而且 checkout 在預設 branch 上（也就是任務 clone 時的 `origin/HEAD`），不是別的 branch 或 detached HEAD；
   - 已追蹤的檔案沒有未 commit 的改動（untracked 檔案不算），也沒有進行到一半的 merge、rebase、cherry-pick 或 revert；
   - `git merge-tree` 試合的結果沒有衝突。

   任何一條不成立，Pots 就只 fetch、不動你的 checkout，卡片上會寫出沒 merge 的原因，要不要 merge 由你自己在 repo 裡處理。merge 用的是你 repo 的 git 設定（作者、hook）；如果 git 在途中失敗，Pots 會 `git merge --abort`，讓 checkout 回到原狀。PM 也會在 #general 回報有沒有 merge，順便說還有哪些任務在跑。任務到這裡就結束了：thread 會自動封存，thread 裡的 agent 不會再回話。按 **Reject** 會通知 Reviewer。

想在接受前自己看看，可以直接從任務的 clone 抓：

```bash
git fetch ~/.pots/tasks/<任務>/repo pots/<任務>
```

### 同時進行的任務和 merge 衝突

每個任務的 branch 都從預設 branch 開出來，彼此看不到對方的改動；兩個任務改到同一段 code，第二個 merge 時就會衝突。PM 有兩個工具來協調：`check_merge_conflicts` 查衝突，`post_in_thread` 到別的任務 thread 發訊息、@ 那裡的 Engineer（被 @ 的 agent 會在那個 thread 的任務、branch 上接手）。

- `check_merge_conflicts` 在主機上試做 merge，不會動到你的 repo 和任務的 clone。對每個有 commit 的任務，列出改了哪些檔案、能不能乾淨 merge 進**現在的**預設 branch；對每兩個任務，列出一起 merge 會不會衝突、衝突在哪些檔案。已經在預設 branch 裡的任務會標成沒有東西要 merge。只看得到已經 commit 的東西。
- PM 開新任務前，如果同一個 repo 有可能改到同一段 code 的進行中任務，會先查一次：可能把新工作併進那個任務、等它 merge 完再開，或照開，在新任務的說明裡寫清楚舊任務改了什麼，並用 `post_in_thread` 告訴舊任務。
- 你問 PM 要怎麼 merge，或有 merge request 時還有其他任務在跑，PM 會查衝突、用 `ask_owner` 建議 merge 順序。
- 第一個 branch merge 進預設 branch 之後（Approve 時 Pots 自動 merge，或你自己 merge），PM 會用 `post_in_thread` 到另一個任務的 thread @Engineer，請它 rebase 到 `origin/HEAD`、解衝突、重跑測試、再送審。

Engineer 每次被叫到時，Pots 會在主機上替它的 clone 跑一次 `git fetch origin`（容器裡連不到你的 repo），所以 `origin/HEAD` 是你 repo 現在的預設 branch，你 Approve 過的任務 branch 也會以 `origin/pots/...` 出現。Reviewer 的 `base` 也跟著變成最新的預設 branch，所以 Engineer rebase 之後，Reviewer 看到的還是只有這個任務的改動。

送 merge request 時，Pots 會自動對其他任務查一次衝突。有衝突的話，🔀 **Merge request** 卡片上會出現 ⚠ 警告，列出跟誰、在哪些檔案衝突（是送出那一刻的狀況），送出的 Reviewer 也會收到同樣的內容。

### PM 的回報

PM 只有被叫到才會講話，所以任務走到下面這些點時，Pots 會叫 PM 在 #general（沒有 #general 就在任務的頻道）回報。PM 這則回覆下方會註明是 Pots 叫它的。

- **有人送 merge request**：PM 說哪個任務可以收了、卡片在哪，還有其他進行中任務的狀況。有衝突的話，改成說明哪裡衝突，並用 `ask_owner` 建議 merge 順序。
- **你按 Approve**：PM 回報這個 branch 已經進專案 repo、Pots 有沒有把它 merge 進預設 branch（沒有的話是為什麼），說還有哪些任務在跑；如果有任務改到同一段 code，會提醒哪個要等或要 rebase。
- **Engineer 或 Reviewer 在 thread 裡用 `ask_owner` 問你**：PM 提醒你有任務在等你決定、要去哪裡回答。PM 不會替你回答，也不會再問一次。
- **任務沒送 merge request 就結束了**：例如唯讀的查詢答完了、查完發現不用改，或做不下去（缺工具、環境壞了）。這時 Engineer 或 Reviewer 會呼叫 `task_done`，附上狀態（`done` 完成，`stuck` 卡住）和摘要，PM 再依此回報；卡住的話會說是什麼擋住、怎樣能解開。

叫的是用 `start_task` 開這個 thread 的那個 PM；thread 是別人開的話，就叫所有沒有 workspace 的 agent。PM 當時正在 #general 回覆的話，會等它回完再叫。

`task_done` 只有在任務 thread 裡、有 workspace 的 agent 才有。「結束時要呼叫它」的說明是 Pots 自動加在它們的系統提示後面的，舊的 `config.toml` 不用改。

這些點以外，PM 不會自動知道 thread 裡的進度：例如 Reviewer 要求修改、Engineer 還在改，PM 都不會回報。想知道的話在 #general @PM，它會去讀那些 thread。

PM 不會自動知道你在自己的 repo merge 了什麼：merge 完想讓它請其他任務 rebase，在 #general @PM 一聲。

### agent 問你問題

agent 需要你做決定時（範圍、優先順序、取捨），會用 `ask_owner` 在訊息下方放 2–4 個選項按鈕，推薦的那個前面會有 ★。點按鈕就等於用那個選項 @ 回覆它。

## 其他

- 側欄的 agent 旁會顯示它目前在做什麼，或到目前為止花了多少錢（價格取自 config 的 `price`，或從 models.dev 查）。每則回覆下方也有 token 用量和費用。
- 快捷鍵：Enter 送出、Shift+Enter 換行、⌘, 開設定、⌘Q 離開；在訊息列表裡用 ⌘A / ⌘C 全選和複製，Esc 取消選取。
