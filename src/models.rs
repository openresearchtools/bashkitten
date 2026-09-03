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
) -> ModelInfo {
    let mut levels = vec!["off", "minimal", "low", "medium", "high"];
    if xhigh {
        levels.push("xhigh");
    }
    if max {
        levels.push("max");
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
        available: true,
    }
}

pub fn codex_models() -> Vec<ModelInfo> {
    vec![
        codex_model(
            "gpt-5.3-codex-spark",
            "GPT-5.3 Codex Spark",
            128_000,
            false,
            false,
            false,
        ),
        codex_model("gpt-5.4", "GPT-5.4", 272_000, true, true, false),
        codex_model("gpt-5.4-mini", "GPT-5.4 mini", 272_000, true, false, false),
        codex_model("gpt-5.5", "GPT-5.5", 272_000, true, true, false),
        codex_model("gpt-5.6-luna", "GPT-5.6 Luna", 272_000, true, true, true),
        codex_model("gpt-5.6-sol", "GPT-5.6 Sol", 272_000, true, true, true),
        codex_model("gpt-5.6-terra", "GPT-5.6 Terra", 272_000, true, true, true),
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
