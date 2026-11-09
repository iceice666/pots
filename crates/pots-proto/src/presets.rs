//! 有內建模型目錄的 provider（目前是 OpenCode Zen／Go）。
//!
//! 資料來自 pi-ai 1.4.0 的模型目錄，只留下 Pots 支援的 chat API：
//! `anthropic-messages`、`openai-completions` 與 `openai-responses`。同一個 provider
//! 的不同模型會走不同 API，所以 [`ProviderPreset::model`] 的 `api` 是路由的依據。
//!
//! `data/provider-presets.json` 是產生出來的檔案；更新 pi-ai 之後重跑
//! `proposal/agent-runtime.md#opencode-與額外-header` 記載的步驟。

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use super::{ProviderHeader, ProviderKind};

/// 內建目錄的模型走哪一種 wire API。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PresetApi {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
}

impl PresetApi {
    /// 對應的 [`ProviderKind`]，給需要一個人看的標籤或成本分類時用。
    pub fn provider_kind(self) -> ProviderKind {
        match self {
            Self::AnthropicMessages => ProviderKind::AnthropicCompatible,
            Self::OpenAiCompletions => ProviderKind::OpenAiCompatible,
            Self::OpenAiResponses => ProviderKind::OpenAiResponses,
        }
    }
}

/// 內建目錄的一筆模型。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    pub api: PresetApi,
}

/// 一個有內建目錄的 provider。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderPreset {
    pub kind: ProviderKind,
    pub label: String,
    /// 目錄的 API 根目錄（OpenAI 的 `/v1` 由 provider 依模型補上）。
    pub base_url: String,
    /// models.dev 的 provider 鍵；成本報表用。
    pub pricing_provider: String,
    /// 建立時預填的 header。
    pub headers: Vec<ProviderHeader>,
    /// 只包含 Pots 支援的 API，依 pi-ai 的目錄順序。
    pub models: Vec<CatalogModel>,
}

impl ProviderPreset {
    pub fn model(&self, id: &str) -> Option<&CatalogModel> {
        self.models.iter().find(|model| model.id == id)
    }

    pub fn contains_model(&self, id: &str) -> bool {
        self.model(id).is_some()
    }
}

#[derive(Deserialize)]
struct PresetFile {
    presets: Vec<PresetEntry>,
}

#[derive(Deserialize)]
struct PresetEntry {
    kind: ProviderKind,
    label: String,
    base_url: String,
    pricing_provider: String,
    #[serde(default)]
    headers: Vec<ProviderHeader>,
    models: Vec<CatalogModel>,
}

impl From<PresetEntry> for ProviderPreset {
    fn from(entry: PresetEntry) -> Self {
        Self {
            kind: entry.kind,
            label: entry.label,
            base_url: entry.base_url,
            pricing_provider: entry.pricing_provider,
            headers: entry.headers,
            models: entry.models,
        }
    }
}

fn catalog() -> &'static Vec<ProviderPreset> {
    static CATALOG: OnceLock<Vec<ProviderPreset>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let file: PresetFile = serde_json::from_str(include_str!("../data/provider-presets.json"))
            .expect("provider-presets.json 格式錯誤");
        file.presets.into_iter().map(Into::into).collect()
    })
}

/// 所有有內建目錄的 provider。
pub fn provider_presets() -> &'static [ProviderPreset] {
    catalog()
}

/// `kind` 的內建目錄；不是 preset 時是 `None`。
pub fn preset_for(kind: ProviderKind) -> Option<&'static ProviderPreset> {
    catalog().iter().find(|preset| preset.kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_both_opencode_providers() {
        let zen = preset_for(ProviderKind::OpenCodeZen).expect("OpenCode Zen");
        let go = preset_for(ProviderKind::OpenCodeGo).expect("OpenCode Go");
        assert_eq!(zen.base_url, "https://opencode.ai/inference");
        assert_eq!(go.base_url, "https://opencode.ai/inference/go");
        assert_eq!(zen.pricing_provider, "opencode");
        assert_eq!(go.pricing_provider, "opencode-go");
        assert!(zen.models.len() >= go.models.len());
        assert!(preset_for(ProviderKind::OpenAiCompatible).is_none());
    }

    #[test]
    fn models_cover_all_supported_apis() {
        for preset in provider_presets() {
            for api in [
                PresetApi::AnthropicMessages,
                PresetApi::OpenAiCompletions,
                PresetApi::OpenAiResponses,
            ] {
                assert!(
                    preset.models.iter().any(|model| model.api == api),
                    "{} 缺少 {api:?} 模型",
                    preset.label
                );
            }
            for model in &preset.models {
                assert!(!model.id.is_empty());
                assert!(!model.name.is_empty());
                assert_eq!(preset.model(&model.id), Some(model));
            }
        }
    }

    #[test]
    fn header_debug_hides_value() {
        let header = super::super::ProviderHeader {
            name: "x-secret".into(),
            value: "sk-do-not-log".into(),
        };
        let debug = format!("{header:?}");
        assert!(debug.contains("x-secret"));
        assert!(!debug.contains("sk-do-not-log"));
    }

    #[test]
    fn kinds_roundtrip_through_serde() {
        for kind in [ProviderKind::OpenCodeZen, ProviderKind::OpenCodeGo] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{kind:?}\""));
            assert_eq!(serde_json::from_str::<ProviderKind>(&json).unwrap(), kind);
        }
    }

    #[test]
    fn preset_base_url_is_default() {
        assert_eq!(
            ProviderKind::OpenCodeGo.default_base_url(),
            "https://opencode.ai/inference/go"
        );
    }
}
