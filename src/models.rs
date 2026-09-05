use crate::agent::{CostRates, CostTier, ModelCost};
use crate::config::{AppConfig, ModelPreset};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub max_tokens: u64,
    pub input: Vec<String>,
    pub reasoning: bool,
    pub thinking_levels: Vec<String>,
    pub default_thinking: String,
    pub cost: ModelCost,
    pub available: bool,
}

impl ModelInfo {
    pub fn full_id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

fn codex_model(
    id: &str,
    name: &str,
    context: u64,
    image: bool,
    xhigh: bool,
    max: bool,
    cost: ModelCost,
) -> ModelInfo {
    let mut levels = vec!["off", "minimal", "low", "medium", "high"];
    if xhigh {
        levels.push("xhigh");
    }
    if max {
        levels.push("max");
    }
    if id == "gpt-6-astra" {
        levels.retain(|level| !matches!(*level, "off" | "minimal"));
    }
    ModelInfo {
        provider: "openai-codex".into(),
        id: id.into(),
        name: name.into(),
        context_window: context,
        max_tokens: 128_000,
        input: if image {
            vec!["text".into(), "image".into()]
        } else {
            vec!["text".into()]
        },
        reasoning: true,
        thinking_levels: levels.into_iter().map(str::to_owned).collect(),
        default_thinking: "medium".into(),
        cost,
        available: true,
    }
}

fn cost(
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    tier: Option<CostRates>,
) -> ModelCost {
    ModelCost {
        rates: CostRates {
            input,
            output,
            cache_read,
            cache_write,
        },
        tiers: tier
            .into_iter()
            .map(|rates| CostTier {
                rates,
                input_tokens_above: 272_000,
            })
            .collect(),
    }
}

fn tier(input: f64, output: f64, cache_read: f64, cache_write: f64) -> CostRates {
    CostRates {
        input,
        output,
        cache_read,
        cache_write,
    }
}

pub fn codex_models() -> Vec<ModelInfo> {
    vec![
        codex_model(
            "gpt-6-astra",
            "GPT-6 Astra",
            272_000,
            true,
            true,
            true,
            cost(10.0, 50.0, 1.0, 12.5, Some(tier(20.0, 75.0, 2.0, 25.0))),
        ),
        codex_model(
            "gpt-5.3-codex-spark",
            "GPT-5.3 Codex Spark",
            128_000,
            false,
            true,
            false,
            cost(1.75, 14.0, 0.175, 0.0, None),
        ),
        codex_model(
            "gpt-5.4",
            "GPT-5.4",
            272_000,
            true,
            true,
            false,
            cost(2.5, 15.0, 0.25, 0.0, Some(tier(5.0, 22.5, 0.5, 0.0))),
        ),
        codex_model(
            "gpt-5.4-mini",
            "GPT-5.4 mini",
            272_000,
            true,
            true,
            false,
            cost(0.75, 4.5, 0.075, 0.0, None),
        ),
        codex_model(
            "gpt-5.5",
            "GPT-5.5",
            272_000,
            true,
            true,
            false,
            cost(5.0, 30.0, 0.5, 0.0, Some(tier(10.0, 45.0, 1.0, 0.0))),
        ),
        codex_model(
            "gpt-5.6-luna",
            "GPT-5.6 Luna",
            272_000,
            true,
            true,
            true,
            cost(0.2, 1.2, 0.02, 0.25, Some(tier(0.4, 1.8, 0.04, 0.5))),
        ),
        codex_model(
            "gpt-5.6-sol",
            "GPT-5.6 Sol",
            272_000,
            true,
            true,
            true,
            cost(5.0, 30.0, 0.5, 6.25, Some(tier(10.0, 45.0, 1.0, 12.5))),
        ),
        codex_model(
            "gpt-5.6-terra",
            "GPT-5.6 Terra",
            272_000,
            true,
            true,
            true,
            cost(2.0, 12.0, 0.2, 2.5, Some(tier(4.0, 18.0, 0.4, 5.0))),
        ),
    ]
}

fn from_preset(provider: &str, p: &ModelPreset, available: bool) -> ModelInfo {
    ModelInfo {
        provider: provider.into(),
        id: p.id.clone(),
        name: if p.name.is_empty() {
            p.id.clone()
        } else {
            p.name.clone()
        },
        context_window: p.context_window,
        max_tokens: p.max_tokens,
        input: p.input.clone(),
        reasoning: p.reasoning,
        thinking_levels: p.thinking_levels.clone(),
        default_thinking: p.default_thinking.clone(),
        cost: p.cost.clone(),
        available,
    }
}

pub fn all_models(
    config: &AppConfig,
    codex_authenticated: bool,
    llama_available: bool,
) -> Vec<ModelInfo> {
    let mut out = codex_models();
    for model in &mut out {
        model.available = codex_authenticated;
    }
    for provider in &config.compatible_providers {
        out.extend(
            provider
                .models
                .iter()
                .map(|p| from_preset(&provider.id, p, true)),
        );
    }
    out.extend(
        config
            .llama
            .models
            .iter()
            .map(|p| from_preset("llama.cpp", p, llama_available)),
    );
    out
}

pub fn find_model(
    config: &AppConfig,
    full_id: &str,
    codex_authenticated: bool,
    llama_available: bool,
) -> Option<ModelInfo> {
    all_models(config, codex_authenticated, llama_available)
        .into_iter()
        .find(|m| m.full_id() == full_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn astra_catalog_matches_pinned_pi_metadata_and_costs() {
        let models = codex_models();
        assert_eq!(models.len(), 8);
        let astra = &models[0];
        assert_eq!(astra.id, "gpt-6-astra");
        assert_eq!(astra.context_window, 272_000);
        assert_eq!(astra.max_tokens, 128_000);
        assert_eq!(astra.input, ["text", "image"]);
        assert_eq!(
            astra.thinking_levels,
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(astra.cost.rates, tier(10.0, 50.0, 1.0, 12.5));
        assert_eq!(astra.cost.tiers[0].rates, tier(20.0, 75.0, 2.0, 25.0));
        assert!(
            models
                .iter()
                .all(|model| model.thinking_levels.contains(&"xhigh".into()))
        );
    }

    #[test]
    fn codex_costs_include_pi_long_context_tiers() {
        let models = codex_models();
        let model = models.iter().find(|model| model.id == "gpt-5.5").unwrap();
        assert_eq!(model.cost.rates.input, 5.0);
        assert_eq!(model.cost.rates.output, 30.0);
        assert_eq!(model.cost.tiers[0].input_tokens_above, 272_000);
        assert_eq!(model.cost.tiers[0].rates.output, 45.0);

        let luna = models
            .iter()
            .find(|model| model.id == "gpt-5.6-luna")
            .unwrap();
        assert_eq!(luna.cost.rates.cache_write, 0.25);
        assert_eq!(luna.cost.tiers[0].rates.cache_write, 0.5);
    }
}
