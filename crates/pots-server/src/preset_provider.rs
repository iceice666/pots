//! 內建模型目錄的 provider（OpenCode Zen／Go）。
//!
//! 同一個 provider 的模型可能走不同的 wire API，所以依 [`ProviderPreset::model`] 的
//! `api` 把請求交給 [Anthropic Messages](pots_llm::anthropic)、
//! [Chat Completions](pots_llm::openai_compat) 或
//! [Responses](pots_llm::responses) 實作。額外 header 與 OpenCode 的
//! `x-opencode-session` 由這些實作在每次請求時加上。

use std::collections::HashMap;
use std::sync::Arc;

use pots_llm::anthropic::AnthropicProvider;
use pots_llm::openai_compat::OpenAiCompatProvider;
use pots_llm::provider::{EventStream, LlmError, ModelProvider, ModelRequest};
use pots_llm::reqwest::header::HeaderMap;
use pots_llm::responses::{ModelInfo, ResponsesProvider};
use pots_proto::{PresetApi, ProviderPreset};
use tokio_util::sync::CancellationToken;
use url::Url;

/// OpenCode 的 per-conversation 路由 header；值來自 [`ModelRequest::session_id`]。
const SESSION_HEADER: &str = "x-opencode-session";

/// 依模型目錄選 API 的 provider。
pub(crate) struct PresetProvider {
    preset: &'static ProviderPreset,
    inner: HashMap<PresetApi, Arc<dyn ModelProvider>>,
}

impl PresetProvider {
    /// 建立每個 API 的實作。`base_url` 是目錄的根目錄（不含 OpenAI 的 `/v1`）。
    pub fn new(
        preset: &'static ProviderPreset,
        base_url: Url,
        api_key: String,
        http: pots_llm::reqwest::Client,
        headers: HeaderMap,
    ) -> Self {
        let mut inner = HashMap::new();
        for api in [
            PresetApi::AnthropicMessages,
            PresetApi::OpenAiCompletions,
            PresetApi::OpenAiResponses,
        ] {
            if !preset.models.iter().any(|model| model.api == api) {
                continue;
            }
            let base = base_for_api(&base_url, api);
            let provider: Arc<dyn ModelProvider> = match api {
                PresetApi::AnthropicMessages => Arc::new(
                    AnthropicProvider::new(base, api_key.clone(), http.clone())
                        .with_headers(headers.clone())
                        .with_session_header(SESSION_HEADER),
                ),
                PresetApi::OpenAiCompletions => Arc::new(
                    OpenAiCompatProvider::new(base, api_key.clone(), http.clone())
                        .with_headers(headers.clone())
                        .with_session_header(SESSION_HEADER),
                ),
                PresetApi::OpenAiResponses => Arc::new(
                    ResponsesProvider::with_api_key(base, api_key.clone(), http.clone())
                        .with_headers(headers.clone())
                        .with_session_header(SESSION_HEADER),
                ),
            };
            inner.insert(api, provider);
        }
        Self { preset, inner }
    }

    /// 內建目錄的模型；不連網。
    pub fn models(&self) -> Vec<ModelInfo> {
        self.preset
            .models
            .iter()
            .map(|model| ModelInfo {
                slug: model.id.clone(),
                display_name: model.name.clone(),
            })
            .collect()
    }

    /// 測試用：這個模型會被交給哪個內建實作。
    #[cfg(test)]
    fn inner_name(&self, model: &str) -> Result<&str, LlmError> {
        Ok(self.provider_for(model)?.name())
    }

    fn provider_for(&self, model: &str) -> Result<&Arc<dyn ModelProvider>, LlmError> {
        let entry = self.preset.model(model).ok_or_else(|| {
            LlmError::Unsupported(format!(
                "「{model}」不在 {} 的內建模型清單；請在設定按「測試連線」挑一個",
                self.preset.label
            ))
        })?;
        self.inner.get(&entry.api).ok_or_else(|| {
            LlmError::Unsupported(format!(
                "{} 沒有 {} 的實作",
                self.preset.label,
                entry.api.provider_kind().label()
            ))
        })
    }
}

/// Anthropic Messages 的 base 就是目錄根目錄；OpenAI 系列要補 `/v1`。
fn base_for_api(base: &Url, api: PresetApi) -> Url {
    match api {
        PresetApi::AnthropicMessages => base.clone(),
        PresetApi::OpenAiCompletions | PresetApi::OpenAiResponses => {
            let mut url = base.clone();
            let path = url.path().trim_end_matches('/').to_string();
            if path.ends_with("/v1") {
                url.set_path(&path);
            } else {
                url.set_path(&format!("{path}/v1"));
            }
            url
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for PresetProvider {
    fn name(&self) -> &str {
        &self.preset.pricing_provider
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        let provider = self.provider_for(&request.model)?;
        provider.stream(request, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pots_proto::{ProviderKind, provider_presets};

    fn preset(kind: ProviderKind) -> &'static ProviderPreset {
        provider_presets()
            .iter()
            .find(|preset| preset.kind == kind)
            .unwrap()
    }

    fn provider(kind: ProviderKind) -> PresetProvider {
        PresetProvider::new(
            preset(kind),
            Url::parse(&preset(kind).base_url).unwrap(),
            "sk-test".into(),
            pots_llm::reqwest::Client::new(),
            HeaderMap::new(),
        )
    }

    #[test]
    fn openai_bases_get_v1_and_anthropic_does_not() {
        let base = Url::parse("https://opencode.ai/inference/go").unwrap();
        assert_eq!(
            base_for_api(&base, PresetApi::AnthropicMessages).as_str(),
            "https://opencode.ai/inference/go"
        );
        assert_eq!(
            base_for_api(&base, PresetApi::OpenAiCompletions).as_str(),
            "https://opencode.ai/inference/go/v1"
        );
        assert_eq!(
            base_for_api(&base, PresetApi::OpenAiResponses).as_str(),
            "https://opencode.ai/inference/go/v1"
        );
        let already = Url::parse("https://example.com/v1/").unwrap();
        assert_eq!(
            base_for_api(&already, PresetApi::OpenAiCompletions).as_str(),
            "https://example.com/v1"
        );
    }

    #[test]
    fn models_route_to_the_right_api() {
        let provider = provider(ProviderKind::OpenCodeGo);
        assert_eq!(
            provider.inner_name("claude-haiku-5-5").unwrap(),
            "anthropic-compatible"
        );
        assert_eq!(
            provider.inner_name("deepseek-v4-pro").unwrap(),
            "openai-compatible"
        );
        assert_eq!(
            provider.inner_name("gpt-5.6-luna").unwrap(),
            "openai-responses"
        );
    }

    #[test]
    fn catalog_is_exposed_as_models() {
        let provider = provider(ProviderKind::OpenCodeGo);
        let models = provider.models();
        assert_eq!(models.len(), preset(ProviderKind::OpenCodeGo).models.len());
        assert!(models.iter().any(|model| model.slug == "claude-haiku-5-5"));
    }

    #[tokio::test]
    async fn unknown_model_is_rejected_before_network() {
        let provider = provider(ProviderKind::OpenCodeGo);
        let error = match provider
            .stream(
                ModelRequest {
                    model: "not-in-the-catalog".into(),
                    instructions: None,
                    input: Vec::new(),
                    tools: Vec::new(),
                    session_id: None,
                },
                CancellationToken::new(),
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("不認得的模型應該在連網前被拒絕"),
        };
        assert!(
            matches!(error, LlmError::Unsupported(message) if message.contains("內建模型清單"))
        );
    }
}
