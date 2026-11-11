# Onboarding 與工作資料夾

[← 產品提案索引](product.md)

## 為什麼

新增 project 只問一個名字，但 agent 要在哪裡寫程式，從來沒有人告訴它。缺口有三層：

- 建立時只收名稱。左欄按 Enter 就建立 project，沒有機會指定 repo 或資料夾。
- 「工作資料夾」藏在自動化分頁。那裡設定的其實是「監看的 repo」，語意是外部事件來源，可以有多個，也可以完全沒有；它不是 worker 的工作位置。
- 對話 agent 的 instructions 只有 project 名稱、Project 指示與 Project Notes。它不知道程式碼在哪，也不會主動問；使用者建立專案後，無從告訴 agent 該在哪個資料夾工作。

結果是任務可以規劃、可以看到，卻沒有地方派工；或者使用者把「監看的 repo」誤當成工作資料夾，改了自動化設定，worker 仍然不動。Onboarding 要解決的是「建立一個 project 時，順便把工作位置說清楚」，而不是再增加一個藏在設定裡的全新概念。

## 工作資料夾與監看來源分開

| | 工作資料夾（`WorkDir`） | 監看的 repo |
| --- | --- | --- |
| 數量 | 每個 project 最多一個；未設定時 worker 不派工 | 零到多個 |
| 語意 | 程式碼在哪、worker 在哪裡改 | 哪些外部事件要收進來 |
| 設定位置 | 新增專案精靈、頻道提示／自動化分頁、對話 agent 的 `set_workdir` | 自動化分頁 |
| 影響 | 派工、任務分支、核准與套用 | connector、subscription、listener、喚醒 |
| 協定 | `WorkDir`、`ProjectWorkdir` | 既有的 `SetProjectRepos`、`ProjectRepos` |

同一個 GitHub repo 可以同時是工作資料夾與監看來源，但兩者是兩筆獨立的設定。把監看的 repo 移除不會讓 worker 停下來，反之亦然。

## 兩種工作資料夾

```rust
pub enum WorkDir {
    Github { repo: String }, // github.com/owner/repo
    Local  { path: String }, // 本機 git repo 的絕對路徑
}
```

`WorkDir::key()` 是任務與事件記下來的字串：GitHub 是 `github.com/owner/repo`，本機是 `local:/abs/path`。

| | GitHub repo | 本機資料夾 |
| --- | --- | --- |
| 來源 | `github.com/owner/repo` | 絕對路徑，必須是 git repo |
| 平台鏡像 | `repos/github.com/owner/repo.git` | `repos/local/<sha256 前 16 bytes>.git`，URL 就是該絕對路徑（git local transport） |
| worker 的副本 | 每個任務一個獨立 repo，任務分支 `pots/<標題>-<任務 ID 後 8 碼>` | 相同；不是你資料夾裡的即時檔案 |
| 完成方式 | push 任務分支、開 PR、等 CI | `open_pr` 只 commit、打 bundle、記下變更（`tasks.pr = local:/path`），不 push、不開 PR、不等 CI |
| 核准 | `MergePr`，merge 後可能再接一筆刪除分支 | `ApplyChanges { path, branch, base, sha }`，核准後線性套用進你的資料夾（能快轉就快轉，岔開時把任務 commit 接到後面） |
| 後續 | CI 失敗自動修、合併後刪分支 | 沒有 CI、沒有 PR、沒有刪分支的後續核准 |

### 本機資料夾的 worker 生命週期

1. 平台從你的資料夾同步到自己的 bare 鏡像，替任務建立工作目錄與任務分支。worker 只改平台副本。
2. worker 完成後呼叫 `open_pr`。平台在執行環境裡 commit、打 bundle，取回鏡像後記下任務分支與 `local:/path`；這一步不碰你的工作區。
3. worker 呼叫 `request_approval`，平台建立 `Action::ApplyChanges { path, branch, base, sha }`。核准綁定任務分支與它開出來的分支，以及當時的 sha。
4. 你核准後，平台先確認 project 的工作資料夾還是同一個路徑、任務分支還是核准的 sha、你的資料夾還停在 `base` 分支而且沒有未 commit 的修改。
5. 平台在你的資料夾執行 `git fetch <mirror> +refs/pots/tasks/<task>:refs/pots/apply/<task>`，再 `git merge --ff-only`，最後刪掉暫時 ref。只動 `.git` 的 fetch 先完成，失敗時工作區不變。
6. 你的分支上有任務分支沒有的 commit（例如另一個平行任務先套用了）時，平台把任務分支在共同祖先之後的 commit 依序接到你的分支後面（等同 rebase）：每個 commit 用 `git merge-tree --write-tree --merge-base=<parent>` 算出 tree、`git commit-tree` 建 commit，保留原作者、時間與訊息，committer 是你；已經在你分支上的變更會略過。這一步只寫 `.git` 的物件，不動工作區、index 與分支，接完才 `merge --ff-only` 到新的頂端。套用後的 sha 和核准的不同，結果訊息會列出兩者。
7. 任一個 commit 有衝突、任務分支有 merge commit，或 fast-forward 失敗，就整個不套用並回報原因；你的資料夾維持原樣。平台不會 merge commit、squash 或自動解衝突，也不會刪掉你的分支。需要 git 2.40 以上（`merge-tree --merge-base`）。

## 驗證規則

建立 project 前、`SetProjectWorkdir`、agent 的 `set_workdir` 走同一份 `crates/pots-server/src/workdir.rs`：

| 種類 | 規則 |
| --- | --- |
| GitHub | 以 `normalize_repo` 正規化；接受 `owner/repo`、`github.com/owner/repo` 或 GitHub URL，存成 `github.com/owner/repo` |
| 本機 | 必須是絕對路徑、存在、是目錄、有 `.git`（目錄或 gitfile）；不能位於 Pots 資料目錄內，也不能包含資料目錄；canonicalize 後才存 |
| 時機 | 建立 project 前先驗證，失敗不留半個 project；修改設定與 agent 設定也先驗證 |
| 本機補充 | 對話 agent 的 `set_workdir` 會確認不在 detached HEAD，有未 commit 的修改時警告；同步鏡像時 detached HEAD 也會被拒絕，核准套用前一定檢查未 commit 的修改 |

資料目錄的檢查是為了避免把鏡像、工作目錄寫進被監看的 repo，也避免把整個家目錄當成工作資料夾。

## Onboarding 三步

1. **新增專案精靈。** 名稱之外選 GitHub repo 或本機資料夾，也可以「稍後再設定」。`CreateProject { name, workdir }` 在寫入前驗證；錯誤直接顯示在對話框，不建立空 project。
2. **未設定提示。** 沒有工作資料夾的 project 在頻道顯示提示橫幅，按「設定」打開同一個對話框補上；自動化分頁也能改。這段期間 Goal 與任務仍可建立，但任務只記在任務樹上、不派工；Planner 的 `create_task` 與對話都會說明原因。
3. **對話 agent 的 `set_workdir`。** 使用者說「用這個 repo」或「在這個資料夾工作」時，對話 agent 以 `kind = github|local` 設定；和人類指令共用驗證。成功後寫入 `projects.workdir_kind`／`workdir`、送出 `ProjectWorkdir`，並在回覆與 instructions 說明 worker 實際在哪工作。

對話、Planner 與 worker 的 instructions 都有「工作資料夾」段落：對話與 Planner 知道程式碼在哪、沒有設定時要先問；worker 知道自己改的是平台副本，以及本機資料夾沒有 PR 與 CI。`create_task` 的舊 `repo` 參數保留，但只能是同一個工作資料夾。

## 協定 v12

```rust
// 快照的 ProjectView／ProjectSnapshot 多了 workdir；事件與指令如下。
ProjectWorkdir { seq: u64, project: ProjectId, workdir: Option<WorkDir> }
CreateProject { name: String, #[serde(default)] workdir: Option<WorkDir> }
SetProjectWorkdir { project: ProjectId, workdir: Option<WorkDir> }
ApprovalActionView::ApplyChanges { path: String, branch: String, base: String, sha: String }
```

- `ProjectWorkdir` 帶 `seq`，寫進事件日誌，重連時照常補送。
- `CreateProject` 的 `workdir` 是 `Option`，舊客戶端不帶也能建立；驗證失敗只回 `Error`。
- `SetProjectWorkdir` 可以設定或清除；清除後回到「任務只記在任務樹上」。
- `SetProjectRepos` 保留，只影響監看與事件，不再決定 worker 在哪工作。
- 本機資料夾的核准在 `ApprovalActionView` 是 `ApplyChanges`，不是 `MergePr`。

## 既有 project

M9 之前的 project 沒有 `WorkDir`：任務只記在任務樹上、不派工，頻道提示與自動化分頁的「設定」可以補上。原本「監看的 repo」同時被當成工作位置，M9 起不再自動派工——寧可停下來要求設定，也不要讓 worker 在使用者沒有確認過的 repo 上改 code。

## 已知限制

| 限制 | 影響 |
| --- | --- |
| 本機只支援線性套用 | 不 squash、不建 merge commit、不自動解衝突；有衝突時要自己處理。接上去的 commit 不跑 hooks，會照 `commit.gpgSign` 簽章 |
| 套用要求同一個基準分支 | 你的資料夾要還停在任務開出來的分支（核准卡片上看得到 `branch → base`），切走了就先切回來 |
| 本機 repo 不能是 detached HEAD | 設定與同步時會拒絕，請先切到分支 |
| 本機沒有 CI | CI 失敗自動修不適用；worker 直接申請套用 |
| 本機沒有沙箱 | 沿用目前的執行環境設定，worker 仍需要 git |
| 工作資料夾可以換掉 | 舊任務的核准會被拒絕，要求重新 `open_pr` |
| 平台副本不是即時檔案 | 你資料夾裡的修改不會自動出現在任務中；核准前也不要手動改同一個分支 |
| 套用會在你的 repo 跑 git | `merge --ff-only` 可能觸發你 repo 的 hooks（`commit-tree` 不跑 hooks）；目前不隔離你的 git 設定 |

## 待確認

- 精靈與提示的文案、錯誤訊息是否足夠清楚；目前允許「稍後再設定」，頻道提示與自動化分頁都能補，需實際走一次 onboarding 驗證。
- 多設備版的「本機資料夾」是伺服器主機上的路徑，不是手機或桌面客戶端的路徑；遠端實例要怎麼呈現與授權。
- 有衝突而套用失敗後的引導：直接顯示衝突的 commit，或提供「讓 worker 在最新的分支上重做」的按鈕。
- 本機資料夾的 hooks 與 git 設定要不要隔離；目前沿用使用者自己的環境。
