# GPUI 所見即所得編輯器測試

用來評估 [gpui-kit-markdown-editor](https://docs.rs/crate/gpui-kit-markdown-editor/latest) 的中文輸入法與編輯體驗。修改只留在記憶體中，不會寫回檔案。

**結果：** 已在 macOS 實測中文輸入法可用，採用為桌面客戶端的 Artifact 編輯器（見 [`proposal/roadmap-and-decisions.md`](../../proposal/roadmap-and-decisions.md#已決定事項)）。Linux、Windows 尚未實測。

## 執行

GPUI 需要比 Rust 1.95 更新的編譯器（已用 1.98 測試）。若 `cargo --version` 太舊，改用 rustup 的 stable：

```sh
cd prototypes/gpui-editor
cargo run                      # 載入內建測試文件 sample.md
cargo run -- path/to/file.md   # 載入其他 Markdown（唯讀，不會儲存）

# 系統 cargo 太舊時
PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" cargo run
```

Linux 需要 GPUI 的系統套件（Wayland/X11、xkbcommon、Vulkan 等），以及中文字型（例如 Noto Sans CJK）。

## 測試重點

測試步驟寫在 `sample.md` 的「中文輸入法測試」清單裡，可以邊測邊勾選。下方狀態列會顯示是否正在組字。

上游的完整手動測試表：`docs/macos-test-checklist.md`（在 crate 原始碼內）。
