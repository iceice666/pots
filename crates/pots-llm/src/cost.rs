//! What agent replies cost: token usage × per-model prices, which come
//! from the agent's config or the models.dev catalog.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::Usage;

const CATALOG_URL: &str = "https://models.dev/api.json";
const CATALOG_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// US dollars per million tokens.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    /// Default: 1.25 × input (Anthropic's 5-minute cache writes).
    pub cache_write: Option<f64>,
    /// Default: 0.1 × input.
    pub cache_read: Option<f64>,
}

impl Price {
    pub fn cost(&self, u: &Usage) -> f64 {
        let write = self.cache_write.unwrap_or(self.input * 1.25);
        let read = self.cache_read.unwrap_or(self.input * 0.1);
        (u.input as f64 * self.input + u.output as f64 * self.output + u.cache_write as f64 * write + u.cache_read as f64 * read)
            / 1_000_000.0
    }
}

/// Model prices from models.dev, by provider id and model id.
#[derive(Debug, Default)]
pub struct Catalog {
    providers: HashMap<String, HashMap<String, Price>>,
}

impl Catalog {
    pub fn parse(json: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Provider {
            #[serde(default)]
            models: HashMap<String, Model>,
        }
        #[derive(Deserialize)]
        struct Model {
            cost: Option<Price>,
        }
        let raw: HashMap<String, Provider> = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let providers = raw
            .into_iter()
            .map(|(id, p)| (id, p.models.into_iter().filter_map(|(m, model)| Some((m, model.cost?))).collect()))
            .collect();
        Ok(Self { providers })
    }

    /// The price of `model`, looking first under each of `preferred`
    /// provider ids in order, then under any provider that lists it.
    pub fn price(&self, preferred: &[&str], model: &str) -> Option<Price> {
        preferred
            .iter()
            .find_map(|p| self.providers.get(*p)?.get(model))
            .or_else(|| {
                // Deterministic among providers that list the same model id.
                let mut ids: Vec<&String> = self.providers.keys().collect();
                ids.sort();
                ids.into_iter().find_map(|p| self.providers[p].get(model))
            })
            .copied()
    }

    /// Whether models.dev lists provider id `provider`.
    pub fn has_provider(&self, provider: &str) -> bool {
        self.providers.contains_key(provider)
    }

    /// The priced models listed under provider id `provider`, by model id.
    pub fn models(&self, provider: &str) -> Vec<(&str, Price)> {
        let mut models: Vec<(&str, Price)> = self
            .providers
            .get(provider)
            .map(|m| m.iter().map(|(id, p)| (id.as_str(), *p)).collect())
            .unwrap_or_default();
        models.sort_by(|a, b| a.0.cmp(b.0));
        models
    }

    /// The catalog cached in `dir`, refreshed from models.dev when it's
    /// older than a day. A stale copy is used when the download fails.
    /// Blocks: call it off the UI thread.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join("models.json");
        let age = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        if age.is_some_and(|a| a < CATALOG_MAX_AGE) {
            if let Ok(text) = std::fs::read_to_string(&path) {
                return Self::parse(&text);
            }
        }
        match fetch() {
            Ok(text) => {
                let catalog = Self::parse(&text)?;
                let _ = std::fs::create_dir_all(dir);
                let _ = std::fs::write(&path, &text);
                Ok(catalog)
            }
            Err(e) => match std::fs::read_to_string(&path) {
                Ok(text) => Self::parse(&text),
                Err(_) => Err(format!("couldn't download model prices from {CATALOG_URL}: {e}")),
            },
        }
    }
}

fn fetch() -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into();
    let mut resp = agent.get(CATALOG_URL).call().map_err(|e| e.to_string())?;
    resp.body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| e.to_string())
}

/// "950", "12.3k", "1.2M".
pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3).replace(".0k", "k"),
        _ => format!("{:.1}M", n as f64 / 1e6).replace(".0M", "M"),
    }
}

/// "$0.0042", "$0.08", "$12.40".
pub fn dollars(x: f64) -> String {
    if x < 0.01 { format!("${x:.4}") } else { format!("${x:.2}") }
}

/// One line about a reply's usage, e.g. "12.3k in · 1.1k out · $0.08".
pub fn summary(u: &Usage, cost: Option<f64>) -> String {
    let mut s = format!("{} in · {} out", tokens(u.total_input()), tokens(u.output));
    if u.cache_read > 0 {
        s.push_str(&format!(" · {}% cached", u.cache_read * 100 / u.total_input().max(1)));
    }
    if let Some(c) = cost {
        s.push_str(&format!(" · {}", dollars(c)));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = r#"{
        "anthropic": {"id": "anthropic", "env": ["ANTHROPIC_API_KEY"], "models": {
            "claude-opus-5-5": {"name": "Claude Opus 5.5", "cost": {"input": 4, "output": 20, "cache_read": 0.2, "cache_write": 5}},
            "free-thing": {"name": "no cost listed"}
        }},
        "zz-reseller": {"models": {"claude-opus-5-5": {"cost": {"input": 9, "output": 99}}, "only-here": {"cost": {"input": 1, "output": 1, "reasoning": 3}}}},
        "aa-other": {"models": {"only-here": {"cost": {"input": 2, "output": 2}}}}
    }"#;

    #[test]
    fn looks_prices_up_by_preferred_provider_then_anywhere() {
        let c = Catalog::parse(CATALOG).unwrap();
        let opus = c.price(&["mock", "anthropic"], "claude-opus-5-5").unwrap();
        assert_eq!((opus.input, opus.output, opus.cache_read, opus.cache_write), (4.0, 20.0, Some(0.2), Some(5.0)));
        assert_eq!(c.price(&["zz-reseller"], "claude-opus-5-5").unwrap().input, 9.0);
        assert_eq!(c.price(&[], "only-here").unwrap().input, 2.0, "sorted: aa-other first");
        assert_eq!(c.price(&["anthropic"], "free-thing"), None);
        assert_eq!(c.price(&["anthropic"], "gpt-x"), None);
    }

    #[test]
    fn costs_count_cached_tokens_at_their_rates() {
        let opus = Catalog::parse(CATALOG).unwrap().price(&["anthropic"], "claude-opus-5-5").unwrap();
        let u = Usage { input: 1_000_000, output: 100_000, cache_write: 0, cache_read: 2_000_000 };
        assert!((opus.cost(&u) - (4.0 + 2.0 + 0.4)).abs() < 1e-9);
        let custom = Price { input: 1.0, output: 2.0, cache_write: None, cache_read: None };
        let u = Usage { input: 0, output: 0, cache_write: 1_000_000, cache_read: 1_000_000 };
        assert!((custom.cost(&u) - 1.35).abs() < 1e-9);
    }

    #[test]
    fn loads_the_cached_catalog_without_fetching() {
        let dir = std::env::temp_dir().join(format!("pots-catalog-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("models.json"), CATALOG).unwrap();
        let c = Catalog::load(&dir).unwrap();
        assert!(c.price(&["anthropic"], "claude-opus-5-5").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn formats_numbers_compactly() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(12_345), "12.3k");
        assert_eq!(tokens(2_000), "2k");
        assert_eq!(tokens(1_250_000), "1.2M");
        assert_eq!(dollars(0.08), "$0.08");
        assert_eq!(dollars(0.0042), "$0.0042");
        let u = Usage { input: 1_000, output: 200, cache_write: 0, cache_read: 3_000 };
        assert_eq!(summary(&u, Some(0.081)), "4k in · 200 out · 75% cached · $0.08");
    }
}
