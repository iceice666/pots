# provider-presets.json

`OpenCode Zen`／`OpenCode Go` 的內建模型目錄。資料來自 pi-ai 1.4.0 的
`dist/providers/data/opencode.json` 與 `opencode-go.json`（models.dev 快照），
只留下 Pots 支援的三種 chat API：

- `anthropic-messages`
- `openai-completions`
- `openai-responses`

Google Generative AI 與 classifier 模型沒有列入。`api` 是 Pots 選擇推論實作的依據；
`base_url` 是目錄的根目錄，OpenAI 系列的 `/v1` 由 `PresetProvider` 補上。

## 重新產生

在 pi-ai 的 `dist/providers/data/` 目錄執行（需要 Python 3）：

```python
import json

SUPPORTED = ["anthropic-messages", "openai-completions", "openai-responses"]
presets = [
    ("OpenCodeZen", "OpenCode Zen", "opencode.json",
     "https://opencode.ai/inference", "opencode"),
    ("OpenCodeGo", "OpenCode Go", "opencode-go.json",
     "https://opencode.ai/inference/go", "opencode-go"),
]
out = {"source": "...", "presets": []}
for kind, label, filename, base_url, pricing in presets:
    data = json.load(open(filename))
    models, seen = [], set()
    for api in SUPPORTED:
        for value in data.get(api, {}).values():
            if value["id"] in seen:
                continue
            seen.add(value["id"])
            models.append({"id": value["id"], "name": value.get("name") or value["id"], "api": api})
    out["presets"].append({
        "kind": kind,
        "label": label,
        "base_url": base_url,
        "pricing_provider": pricing,
        "headers": [{"name": "x-opencode-client", "value": "pots"}],
        "source": filename,
        "models": models,
    })
print(json.dumps(out, ensure_ascii=False, indent=2))
```

把輸出存回這個檔案，並更新 `source` 的 pi-ai 版本與快照日期。`pots-proto` 的
`presets.rs` 會用 `include_str!` 內嵌它，`cargo test -p pots-proto` 會檢查兩個 preset
都至少有一個模型在每種支援的 API 上。
