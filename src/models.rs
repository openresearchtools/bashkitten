use crate::agent::ModelCost;
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
    /// Authentication status is separate from router/model availability.
    pub authentication: String,
    /// Non-secret invocation and loading parameters from the shared registry.
    pub parameters: serde_json::Value,
}

impl ModelInfo {
    pub fn full_id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

pub fn codex_models() -> Vec<ModelInfo> {
    crate::codex::catalog()
        .into_iter()
        .map(|model| ModelInfo {
            provider: "openai-codex".into(),
            id: model["id"].as_str().expect("catalog ID").into(),
            name: model["name"].as_str().expect("catalog name").into(),
            context_window: model["contextWindow"].as_u64().expect("catalog context"),
            max_tokens: model["maxTokens"].as_u64().expect("catalog maximum"),
            input: serde_json::from_value(model["input"].clone()).expect("catalog inputs"),
            reasoning: model["reasoning"] == true,
            thinking_levels: crate::codex::thinking_levels(&model),
            default_thinking: "medium".into(),
            cost: serde_json::from_value(model["cost"].clone()).expect("catalog costs"),
            available: true,
            authentication: "authenticated".into(),
            parameters: model,
        })
        .collect()
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
        authentication: "not_required".into(),
        parameters: serde_json::json!({}),
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
        model.authentication = if codex_authenticated {
            "authenticated"
        } else {
            "required"
        }
        .into();
    }
    for provider in &config.compatible_providers {
        out.extend(provider.models.iter().map(|p| {
            let mut model = from_preset(&provider.id, p, true);
            model.authentication = match &provider.auth {
                crate::config::CompatibleAuth::None => "not_required",
                crate::config::CompatibleAuth::Bearer { secret }
                | crate::config::CompatibleAuth::Header { secret, .. } => {
                    if secret.is_empty() {
                        "required"
                    } else {
                        "configured"
                    }
                }
            }
            .into();
            model.available = model.authentication != "required";
            model.parameters =
                crate::completions::preset_model(&provider.id, &provider.base_url, p, false);
            model
        }));
    }
    out.extend(config.llama.models.iter().map(|p| {
        let reported = config.llama.catalog.iter().find(|m| m["id"] == p.id);
        let mut model = from_preset(
            "llama.cpp",
            p,
            llama_available
                && reported
                    .is_some_and(|m| crate::llama::selectable(m, config.llama.router_autoload)),
        );
        model.authentication = if config.llama.api_key.is_empty() {
            "not_required"
        } else {
            "configured"
        }
        .into();
        model.parameters = crate::completions::preset_model(
            "llama.cpp",
            &format!("http://127.0.0.1:{}/v1", config.llama.port),
            p,
            true,
        );
        model.parameters["llamaOptions"] = serde_json::json!(p.llama_options);
        model.parameters["llamaModelPath"] = serde_json::json!(p.llama_model_path);
        if let Some(entry) = reported
            .filter(|m| matches!(m["status"]["value"].as_str(), Some("loaded" | "sleeping")))
            && entry["meta"]["n_ctx"].as_u64().is_some_and(|n| n > 0)
        {
            model.context_window = entry["meta"]["n_ctx"].as_u64().unwrap();
            model.parameters["contextWindow"] = serde_json::json!(model.context_window);
        }
        model
    }));
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

/// One launch/switch validation path for Web, CLI and session workers.
pub fn resolve_model(
    config: &AppConfig,
    full_id: &str,
    thinking: &str,
    codex_authenticated: bool,
    llama_available: bool,
) -> anyhow::Result<ModelInfo> {
    let model = find_model(config, full_id, codex_authenticated, llama_available)
        .filter(|model| model.available)
        .ok_or_else(|| anyhow::anyhow!("Unknown or unavailable model: {full_id}"))?;
    if !model.thinking_levels.iter().any(|level| level == thinking) {
        anyhow::bail!("Thinking level {thinking} is not supported by {full_id}");
    }
    Ok(model)
}

/// Pinned sdk.ts: restore the configured pair when no model override exists;
/// otherwise use the preset's per-model thinking setting, with Codex's native
/// clamp for its built-in catalog. Explicit thinking always goes through the
/// strict shared validation required by BashKitten.
pub fn resolve_new_session(
    config: &AppConfig,
    model: Option<&str>,
    thinking: Option<&str>,
    authenticated: bool,
    llama_available: bool,
) -> anyhow::Result<(ModelInfo, String)> {
    let full_id = model.unwrap_or(&config.default_model);
    let selected = find_model(config, full_id, authenticated, llama_available)
        .filter(|model| model.available)
        .ok_or_else(|| anyhow::anyhow!("Unknown or unavailable model: {full_id}"))?;
    let thinking = thinking.map(str::to_owned).unwrap_or_else(|| {
        if full_id == config.default_model {
            config.default_thinking.clone()
        } else if selected.provider == "openai-codex" {
            crate::codex::clamp_thinking(&selected.parameters, &config.default_thinking)
        } else {
            selected.default_thinking.clone()
        }
    });
    let selected = resolve_model(config, full_id, &thinking, authenticated, llama_available)?;
    Ok((selected, thinking))
}

#[cfg(test)]
mod tests {
    #[test]
    fn explicit_model_uses_its_declared_default_and_explicit_thinking_stays_strict() {
        use crate::config::{AppConfig, CompatibleAuth, CompatibleProvider, ModelPreset};
        let mut config = AppConfig::default();
        let preset = ModelPreset {
            id: "local".into(),
            reasoning: false,
            thinking_levels: vec!["off".into()],
            default_thinking: "off".into(),
            ..Default::default()
        };
        config.compatible_providers.push(CompatibleProvider {
            id: "fixture".into(),
            name: "Fixture".into(),
            base_url: "http://127.0.0.1:8000/v1".into(),
            auth: CompatibleAuth::None,
            models: vec![preset],
        });
        let (model, thinking) =
            super::resolve_new_session(&config, Some("fixture/local"), None, false, false).unwrap();
        assert_eq!(model.full_id(), "fixture/local");
        assert_eq!(thinking, "off");
        assert!(
            super::resolve_new_session(
                &config,
                Some("fixture/local"),
                Some("medium"),
                false,
                false
            )
            .is_err()
        );
        let (model, thinking) =
            super::resolve_new_session(&config, None, None, true, false).unwrap();
        assert_eq!(model.full_id(), config.default_model);
        assert_eq!(thinking, config.default_thinking);
    }
    fn tier(input: f64, output: f64, cache_read: f64, cache_write: f64) -> crate::agent::CostRates {
        crate::agent::CostRates {
            input,
            output,
            cache_read,
            cache_write,
        }
    }
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
            ["minimal", "low", "medium", "high", "xhigh", "max"]
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
