# NVIDIA OpenShell 上游證據（CLI 實機輸出）

本文件只記錄**本次真的取得、真的執行過**的上游事實，供 sandbox 遷移決策使用。
調查基準時間：2026-11-11T10:00:31Z（UTC）。沒有取得的項目一律列在
[本次沒取得的項目](#本次沒取得的項目)，不做推測補齊。

本文件不含任何 adapter 設計或 API 對照；那些要等 gateway 可執行後再談。

---

## 1. 官方身份證據

| 項目 | 證據 | 來源 |
| --- | --- | --- |
| 上游 repo | `github.com/NVIDIA/OpenShell`，`full_name=NVIDIA/OpenShell`，owner 為 GitHub Organization `NVIDIA`，`fork=false`、`archived=false` | `GET https://api.github.com/repos/NVIDIA/OpenShell`（2026-11-11） |
| 專案描述 | "OpenShell is the safe, private runtime for autonomous AI agents." | 同上 |
| repo 建立 / 最後 push | `created_at=2026-02-24T22:52:22Z`、`pushed_at=2026-11-11T07:45:41Z` | 同上 |
| NVIDIA 官方文件 | `https://docs.nvidia.com/openshell/latest/` → 307 導向 `https://docs.nvidia.com/openshell/about/overview`，標題 `Overview of OpenShell \| NVIDIA OpenShell`，頁面 navbar 反向連回 `https://github.com/NVIDIA/OpenShell`；文件版本選擇器列出 `Latest (v0.1.8)` | 直接抓取該 URL（HTTP 200） |
| 官方安裝文件 | `https://docs.nvidia.com/openshell/about/installation`（HTTP 200），標題「Install OpenShell」，版本標示 `Latest (v0.1.8)` | 直接抓取 |
| CLI 自身交叉引用 | `openshell --help` 的 LEARN MORE／各子命令 help 文字直接描述 gateway、gRPC exec、policy、provider 等本專案概念 | `cli-help/help.txt`、`cli-help/sandbox-exec--help.txt` |

反向連結的意義：`docs.nvidia.com`（NVIDIA 官方文件網域）上的 OpenShell 文件頁面連回
`github.com/NVIDIA/OpenShell`，且該 GitHub repo 的 owner 是 NVIDIA 組織。這是我方認定
「NVIDIA OpenShell」即此專案的依據；除此之外沒有做進一步的身份推論。

補充（來自上游 README）：`NVIDIA/OpenShell` 的 README 註明專案正在移往
[`ai-openshell`](https://github.com/ai-openshell) 組織並交由 CNCF 治理，遷移完成前
仍以 `NVIDIA/OpenShell` 為程式碼、issue 與 PR 的正式位置。後續升級版本時要重新確認這點。

## 2. 釘選版本

| 項目 | 值 |
| --- | --- |
| Release tag | `v0.1.8`（`https://github.com/NVIDIA/OpenShell/releases/tag/v0.1.8`） |
| Release 名稱 / 發佈時間 | `OpenShell v0.1.8` / `2026-11-10T18:14:42Z` |
| Tag 解析後的 commit | `f3d976590d3eb1379690a2abaf0f0e26c21d487d` |
| Commit 訊息與作者 | `fix(tmachine): restore package-installed Podman qualification (#4289)`，author `Evan Lezar <elezar@nvidia.com>`，2026-11-10T15:06:53Z |
| Release 產生者 | `github-actions[bot]`（由上游 CI 發佈） |

要重現同一個 CLI 行為，請用完整 commit `f3d976590d3eb1379690a2abaf0f0e26c21d487d`
或 tag `v0.1.8`；不要用 `main`。

## 3. 授權

- `license.spdx_id = Apache-2.0`（GitHub API `repos/NVIDIA/OpenShell`）。
- 上游 `LICENSE`（`https://raw.githubusercontent.com/NVIDIA/OpenShell/v0.1.8/LICENSE`）：
  `Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.` + Apache License 2.0，
  全文副本見 `artifacts/UPSTREAM-LICENSE-Apache-2.0.txt`。
- 上游 README 亦宣告 Apache 2.0。

## 4. 下載來源與 SHA256

下載指令（本次實際執行，架構 `aarch64`，與執行環境 `uname -m` 一致）：

```bash
B=https://github.com/NVIDIA/OpenShell/releases/download/v0.1.8
curl -sSL -o openshell-aarch64-unknown-linux-musl.tar.gz \
  $B/openshell-aarch64-unknown-linux-musl.tar.gz
curl -sSL -o openshell-checksums-sha256.txt \
  $B/openshell-checksums-sha256.txt
grep ' openshell-aarch64-unknown-linux-musl.tar.gz$' \
  openshell-checksums-sha256.txt > expected.one
sha256sum -c expected.one        # => OK
tar xzf openshell-aarch64-unknown-linux-musl.tar.gz   # 內含單一檔案 openshell
```

| 產物 | SHA256 | 官方 checksum 對照 |
| --- | --- | --- |
| `openshell-aarch64-unknown-linux-musl.tar.gz` | `9a2e8f522ccfb3fbd1ed1985a51191a60e083f3404903a56bd4813710a2c7080` | OK（來源：同 release 的 `openshell-checksums-sha256.txt` asset） |
| `openshell-x86_64-unknown-linux-musl.tar.gz`（僅為交叉確認而額外下載） | `87f5c41d396dcd36b4c30a7b5e6f41643a656cb31a82b0edbb00319c9a719c2d` | OK（同上） |
| 解開後的 aarch64 `openshell` ELF | `be5a53e4622db67a56b870840c3fdd747f6dc21717bd4d43fdec8745c5e8e9cc` | 上游未提供單檔 checksum |
| 解開後的 x86_64 `openshell` ELF | `4d3950f4ec5e973ff05c98edcfbdb6cc500a5184b55808e77610eb6af99f76db` | 上游未提供單檔 checksum |

官方 checksum 清單原文（`openshell-checksums-sha256.txt`）已存於
`artifacts/openshell-checksums-sha256.txt`；驗證輸出見 `artifacts/sha256-verify.txt`。

`--version` 實測輸出：`openshell 0.1.8`（`cli-help/version.txt`）。

> 注意：本次環境 `uname -m = aarch64`，但**兩個架構的 binary 都執行成功**
> （x86_64 ELF：`e_machine`=62；aarch64 ELF：`e_machine`=183，兩者都印出 `openshell 0.1.8`）。
> 容器內的 `/proc/sys/fs/binfmt_misc` 是空的，看不到實際的模擬機制，因此無法斷定是
> Rosetta 或其他 binfmt handler 在運作——這點沒有進一步追查，也不影響本文件的結論：
> 兩架構下 32 份 help 輸出**逐位元相同**（比對指令見 `artifacts/environment.txt`）。

## 5. 安裝 / 部署前提（來自官方文件與 CLI 自述）

以下全部出自上游文件與 CLI 文字，未經我方實測（除註明者）：

1. **CLI 不是自足的**：CLI 必須連上一個 gateway。無 gateway 時 `openshell status` 回
   `No gateway configured.`；`openshell sandbox list` / `sandbox create` 直接失敗
   （`Error: × No active gateway.`，exit 1）。→ 已實測，見 `cli-help/RUN-*.txt`。
2. **Gateway 註冊**：`openshell gateway add <endpoint>`，可加 `--local`
   （例：`openshell gateway add http://127.0.0.1:8080 --local`）；已完成 TLS 的本地安裝預設端點是
   `https://127.0.0.1:17670`（官方安裝文件的 systemd user service 說明）。
3. **本機 runtime 需求**（官方 installation 頁）：Docker Desktop 或 Docker Engine **28.0+**；
   或 Podman 5.x + cgroups v2 + 作用中的 user socket；或 MicroVM（macOS Hypervisor.framework /
   Linux KVM）。Windows MXC 為開發中、非 production-qualified，且 release 通路不提供原生 Windows binary。
4. **Linux 套件前提**：deb/rpm 需 glibc 2.28+；gateway 以 systemd user service 執行並讀
   `~/.config/openshell/gateway.toml`。
5. **執行期架構**：CLI ↔ gateway 之間是 gateway 連線（`sandbox exec` help 明寫 "using the gRPC
   exec endpoint"）；sandbox 需先 create，再 exec（`sandbox-exec--help.txt`）。
6. **本環境不滿足上述前提**：容器內沒有 `docker`、沒有 `podman`、沒有 `systemctl`，
   `openshell doctor check` exit 1 並回：
   `Docker ............. Error: × failed to execute docker info ╰─▶ No such file or directory (os error 2)`。
   因此**無法**起 gateway、無法做 create→exec→delete smoke。原文見
   `cli-help/RUN-doctor-check.txt`、`artifacts/environment.txt`。

## 6. CLI 介面證據（`cli-help/`）

所有檔案都是 v0.1.8 aarch64 binary 的**原始 stdout+stderr**，未經編輯；每檔開頭兩行記錄
被執行的命令與抓取時間。檔名規則：把命令中的空白換成 `-` 再加上 `--help.txt`，例如
`openshell sandbox exec --help` → `sandbox-exec--help.txt`。

- 頂層：`help.txt`、`version.txt`
- 頂層 15 個命令的 help：`sandbox`、`service`、`forward`、`logs`、`policy`、`settings`、
  `provider`、`profile`、`gateway`、`status`、`whoami`、`doctor`、`term`、`completions`、`ssh-proxy`
- `sandbox` 全部 15 個子命令的 help：`create`、`get`、`list`、`delete`、`stop`、`start`、
  `suspend`、`resume`、`exec`、`connect`、`upload`、`download`、`ssh-config`、`provider`、`template`
- 額外：`gateway-add`、`doctor-check`
- 實跑輸出（非 help）：`RUN-status.txt`、`RUN-doctor.txt`、`RUN-doctor-check.txt`、
  `RUN-gateway-list.txt`、`RUN-sandbox-list.txt`、`RUN-sandbox-create-attempt.txt`

**全域 flags 沒有獨立的 help 命令**。`-g/--gateway`、`--gateway-endpoint`、`--gateway-insecure`、
`--workspace`、`-v/--verbose`、`--color`、`-h/--help`、`-V/--version` 都列在各命令的 `FLAGS` 區段
（頂層 `help.txt` 有完整一份），因此用各命令的 help 檔作為全域 flag 證據。

`--help` 中直接可見的環境變數：`OPENSHELL_GATEWAY`、`OPENSHELL_GATEWAY_ENDPOINT`、
`OPENSHELL_GATEWAY_INSECURE`、`OPENSHELL_WORKSPACE`、`OPENSHELL_COLOR`，
以及 `sandbox create --policy` 敘述提到的 `OPENSHELL_SANDBOX_POLICY`。
（`OPENSHELL_TELEMETRY_ENABLED` 出自上游 README，不是 CLI help。）

與舊 docker 實作最相關的介面對應（**只列 help 真的寫到的東西**，不代表行為已驗證）：

| 需求 | v0.1.8 介面 | 證據檔 |
| --- | --- | --- |
| 建立 | `sandbox create`（`--name`、`--from`、`--template`、`--cpu`、`--memory`、`--env`、`--env-from`、`--upload`、`--policy`、`--provider`、`--restart-policy`、`--detach`、`-- <COMMAND>...`） | `sandbox-create--help.txt` |
| 執行 / 串流 | `sandbox exec <SANDBOX> -- <COMMAND>...`（`--workdir`、`--timeout`、`--tty/--no-tty`、`--no-login-shell`、`--env`；help 明寫 output 即時串流、CLI 以遠端 exit code 結束） | `sandbox-exec--help.txt` |
| 互動 shell | `sandbox connect`（exec help 指向它處理互動 session） | `sandbox-connect--help.txt` |
| 停止 / 終止 | `sandbox stop`（保留 workspace）、`sandbox suspend`/`resume`（保留 memory）、`sandbox delete` | `sandbox-stop--help.txt`、`sandbox-suspend--help.txt`、`sandbox-delete--help.txt` |
| 列出 | `sandbox list`、`sandbox get` | `sandbox-list--help.txt`、`sandbox-get--help.txt` |
| 檔案進出 | `sandbox upload`（`LOCAL[:SANDBOX]`，gitignore 過濾）、`sandbox download` | `sandbox-upload--help.txt`、`sandbox-download--help.txt` |
| 日誌 / 埠 / 服務 | `logs`、`forward`、`service`、`ssh-config` | `logs--help.txt`、`forward--help.txt`、`service--help.txt`、`sandbox-ssh-config--help.txt` |
| 政策 | `policy`、`sandbox create --policy`、`--approval-mode` | `policy--help.txt`、`sandbox-create--help.txt` |
| 憑證 | `provider`、`sandbox create --provider` | `provider--help.txt`、`sandbox-create--help.txt` |

注意：上表是「help 有這個選項」層級的事實，**沒有**任何一項經過 gateway 實測。

## 本次沒取得的項目

以下全部**未取得**，不得當作已知事實使用：

1. 沒有建立過任何 gateway。`openshell gateway list` 回 `No gateways found.`，
   所以 gateway 註冊、TLS、token/OIDC 登入、`gateway info` 的實際行為都未驗證。
2. 沒有任何 `sandbox create` / `exec` / `stop` / `delete` / `list` / `upload` / `download` 的真實
   成功案例。`sandbox create` 在無 gateway 下即失敗，錯誤原文見 `cli-help/RUN-sandbox-create-attempt.txt`。
3. 未驗證 stdin/stdout/stderr 串流語意、非 TTY 下的行為、exit code 傳遞、逾時（`--timeout`）與
   取消時遠端子孫程序是否終止。
4. 未驗證檔案上傳/下載的 size 上限、symlink/`O_NOFOLLOW` 邊界、gitignore 過濾的實際效果。
5. 未驗證策略（policy）拒絕/允許的實際判定、prover、`--approval-mode auto` 行為。
6. 未驗證 workspace 持久化、`sandbox suspend/resume` 的 memory 保留、跨 run 的 session/home 掛載。
7. 未驗證資源限制（`--cpu`/`--memory`/`--gpu`）、capability/`no-new-privileges`/pids-limit 等
   隔離性質在 OpenShell 下的等價物。
8. 未取得 Rust/Go/Python SDK 的實際 API，也未讀 SDK 文件；只有 CLI help。
9. 沒有跑任何 repo 測試（本 PR 是純文件），也沒有跑 `cargo` 相關檢查。
10. 沒有實測 gateway 部署（systemd user service、Helm/Kubernetes、snap、Homebrew 任一形式）。
11. 沒有測上游 `install.sh`（本任務刻意只下載 release tarball 並驗 checksum，避免執行網路安裝腳本）。
12. 未確認上游 README 提到的 `ai-openshell` 組織／CNCF 遷移對未來版本的影響。

要在有 Docker/Podman 的環境補做最小 smoke，可重現步驟（尚未執行）：

```bash
# 1) 安裝 CLI（或直接用已驗證的 tarball）
B=https://github.com/NVIDIA/OpenShell/releases/download/v0.1.8
curl -sSL -o os.tgz $B/openshell-aarch64-unknown-linux-musl.tar.gz   # x86_64 換檔名
tar xzf os.tgz
# 2) 起 gateway（依官方文件：install.sh 會一併安裝並啟動本地 gateway）
curl -LsSf https://raw.githubusercontent.com/NVIDIA/OpenShell/main/install.sh | OPENSHELL_VERSION=v0.1.8 sh
./openshell doctor check && ./openshell status
# 3) 最小生命週期
./openshell sandbox create --name pots-smoke --no-tty -- true
./openshell sandbox exec pots-smoke -- /bin/sh -c 'echo hi; exit 7'; echo "exit=$?"  # 期望 7
./openshell sandbox list
./openshell sandbox delete pots-smoke
```

## 重現整份證據

```bash
mkdir -p /tmp/oshell && cd /tmp/oshell
B=https://github.com/NVIDIA/OpenShell/releases/download/v0.1.8
curl -sSL -o openshell-aarch64-unknown-linux-musl.tar.gz $B/openshell-aarch64-unknown-linux-musl.tar.gz
curl -sSL -o openshell-checksums-sha256.txt $B/openshell-checksums-sha256.txt
grep ' openshell-aarch64-unknown-linux-musl.tar.gz$' openshell-checksums-sha256.txt > exp.one
sha256sum -c exp.one
mkdir -p arm && tar xzf openshell-aarch64-unknown-linux-musl.tar.gz -C arm
./arm/openshell --version
./arm/openshell --help
for c in sandbox service forward logs policy settings provider profile gateway status whoami doctor term completions ssh-proxy; do
  ./arm/openshell $c --help > "$c--help.txt" 2>&1
done
for c in create get list delete stop start suspend resume exec connect upload download ssh-config provider template; do
  ./arm/openshell sandbox $c --help > "sandbox-$c--help.txt" 2>&1
done
```
