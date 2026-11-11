# OpenShell CLI help 原始輸出（v0.1.8）

這裡的每個檔案都是 `openshell` **v0.1.8**（tag `v0.1.8`，commit
`f3d976590d3eb1379690a2abaf0f0e26c21d487d`）aarch64 CLI 的原始 stdout+stderr，
未經任何編輯。

- 下載來源、SHA256、官方 checksum 對照、授權與環境限制：見 [`../UPSTREAM.md`](../UPSTREAM.md)。
- 檔案名稱 = 命令（空白換成 `-`）再加上 `--help.txt`，例如 `openshell sandbox exec --help`
  對應 `sandbox-exec--help.txt`。
- `version.txt` 是 `openshell --version`；`RUN-*.txt` 是實際執行（非 help）的輸出，
  每檔開頭兩行記錄被執行的命令與抓取時間，結尾有 `[exit=N]`。
- 全域 flags 沒有獨立的 help 命令，列在各命令的 `FLAGS` 區段（頂層 `help.txt` 有完整一份）。

重新產生這些檔案的方法寫在 `../UPSTREAM.md` 的「重現整份證據」。

這些檔案只證明「CLI 介面文字長這樣」。本環境沒有 Docker/Podman/gateway，
任何 sandbox 生命週期行為都**沒有**被驗證；未取得項目清單見 `../UPSTREAM.md`。
