use crate::agent::ModelCost;
use crate::paths::{AppPaths, set_private_file};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub web_port: u16,
    pub web_restart_on_failure: bool,
    pub start_at_login: bool,
    pub theme: UiTheme,
    pub default_cwd: PathBuf,
    pub default_model: String,
    pub default_thinking: String,
    pub http_idle_timeout_ms: u64,
    pub transport: crate::codex_websocket::Transport,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub compaction: crate::agent::CompactionSettings,
    pub retry: crate::recovery::RetryPolicy,
    pub compatible_providers: Vec<CompatibleProvider>,
    pub llama: LlamaConfig,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiTheme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompatibleProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub auth: CompatibleAuth,
    #[serde(default)]
    pub models: Vec<ModelPreset>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibleAuth {
    #[default]
    None,
    Bearer {
        secret: String,
    },
    Header {
        name: String,
        secret: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPreset {
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub max_tokens: u64,
    pub input: Vec<String>,
    pub reasoning: bool,
    pub thinking_levels: Vec<String>,
    pub default_thinking: String,
    pub cost: ModelCost,
    pub request_parameters: Value,
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    /// Pinned OpenAICompletionsCompat field names and optional overrides.
    pub compatibility: Value,
    pub thinking_level_map: Value,
    pub thinking_budgets: Value,
    /// Native llama.cpp INI entries for this model's section.
    pub llama_options: String,
    /// User-selected local GGUF. Kept as its logical path for split cache files.
    pub llama_model_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LlamaConfig {
    pub enabled: bool,
    pub models_dir: PathBuf,
    pub model_search_dirs: Vec<PathBuf>,
    pub gpu_environment: std::collections::BTreeMap<String, String>,
    pub port: u16,
    pub api_key: String,
    pub context_size: u64,
    pub gpu_layers: GpuLayers,
    pub cpu_threads: u16,
    pub batch_size: u32,
    pub parallel_slots: u16,
    pub flash_attention: bool,
    pub mmap: bool,
    pub mlock: bool,
    pub autoload: bool,
    pub extra_arguments: Vec<String>,
    pub models: Vec<ModelPreset>,
    pub catalog: Vec<Value>,
    pub router_autoload: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuLayers {
    Auto,
    Cpu,
    Count(u32),
}

impl Default for AppConfig {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        Self {
            web_port: 3939,
            web_restart_on_failure: true,
            start_at_login: false,
            theme: UiTheme::System,
            default_cwd: home.clone(),
            default_model: "openai-codex/gpt-5.5".into(),
            default_thinking: "medium".into(),
            http_idle_timeout_ms: 300_000,
            transport: Default::default(),
            websocket_connect_timeout_ms: None,
            compaction: crate::agent::CompactionSettings::default(),
            retry: crate::recovery::RetryPolicy::default(),
            compatible_providers: Vec::new(),
            llama: LlamaConfig {
                enabled: false,
                models_dir: home.join("models"),
                model_search_dirs: Vec::new(),
                gpu_environment: Default::default(),
                port: 8080,
                api_key: String::new(),
                context_size: 32768,
                gpu_layers: GpuLayers::Auto,
                cpu_threads: 0,
                batch_size: 2048,
                parallel_slots: 1,
                flash_attention: true,
                mmap: true,
                mlock: false,
                autoload: false,
                extra_arguments: Vec::new(),
                models: Vec::new(),
                catalog: Vec::new(),
                router_autoload: false,
            },
        }
    }
}

impl Default for ModelPreset {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            context_window: 128_000,
            max_tokens: 16_384,
            input: vec!["text".into()],
            reasoning: false,
            thinking_levels: vec!["off".into()],
            default_thinking: "off".into(),
            cost: ModelCost::default(),
            request_parameters: Value::Object(Default::default()),
            supports_developer_role: None,
            supports_reasoning_effort: None,
            compatibility: serde_json::json!({}),
            thinking_level_map: serde_json::json!({}),
            thinking_budgets: serde_json::json!({}),
            llama_options: String::new(),
            llama_model_path: PathBuf::new(),
        }
    }
}

impl ModelPreset {
    pub fn validate(&self) -> Result<()> {
        use anyhow::bail;
        const LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
        if self.id.is_empty() || self.context_window == 0 || self.max_tokens == 0 {
            bail!("A model preset requires a model ID, context window and maximum output tokens");
        }
        if self.thinking_levels.is_empty()
            || self
                .thinking_levels
                .iter()
                .any(|level| !LEVELS.contains(&level.as_str()))
            || !self.thinking_levels.contains(&self.default_thinking)
        {
            bail!(
                "Model {} must declare supported thinking levels and a default from that list",
                self.id
            );
        }
        if !self.reasoning && self.thinking_levels.iter().any(|v| v != "off") {
            bail!(
                "Model {} must support reasoning to accept thinking levels",
                self.id
            );
        }
        if self.input.is_empty()
            || self
                .input
                .iter()
                .any(|v| !matches!(v.as_str(), "text" | "image"))
        {
            bail!(
                "Model {} input capabilities must be text and/or image",
                self.id
            );
        }
        for (name, value) in [
            ("Request parameters", &self.request_parameters),
            ("Compatibility", &self.compatibility),
            ("Thinking level mapping", &self.thinking_level_map),
            ("Thinking budgets", &self.thinking_budgets),
        ] {
            if !value.is_object() {
                bail!("{name} for model {} must be a JSON object", self.id);
            }
        }
        for (level, value) in self.thinking_level_map.as_object().unwrap() {
            if !LEVELS.contains(&level.as_str()) || !(value.is_null() || value.is_string()) {
                bail!("Invalid thinking level mapping for model {}", self.id);
            }
        }
        for (level, value) in self.thinking_budgets.as_object().unwrap() {
            if !matches!(level.as_str(), "minimal" | "low" | "medium" | "high")
                || !value.is_number()
            {
                bail!("Invalid thinking budget for model {}", self.id);
            }
        }
        Ok(())
    }
}

impl Default for LlamaConfig {
    fn default() -> Self {
        AppConfig::default().llama
    }
}

impl AppConfig {
    pub fn load(paths: &AppPaths) -> Result<Self> {
        let path = paths.config_file();
        if !path.exists() {
            let config = Self::default();
            config.save(paths)?;
            return Ok(config);
        }
        set_private_file(&path)?;
        let data = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_slice(&data).with_context(|| format!("parse {}", path.display()))
    }

    pub fn validate_models(&self) -> Result<()> {
        use anyhow::bail;
        crate::llama::validate_local_settings(&self.llama)?;
        let mut providers = std::collections::HashSet::new();
        for provider in &self.compatible_providers {
            if provider.id.is_empty()
                || provider.id.contains('/')
                || matches!(provider.id.as_str(), "openai-codex" | "llama.cpp")
                || !providers.insert(&provider.id)
            {
                bail!(
                    "Compatible providers require unique IDs distinct from the built-in provider modes"
                );
            }
            let url = reqwest::Url::parse(&provider.base_url)
                .context("Provider base URL must be an absolute HTTP or HTTPS URL")?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                bail!(
                    "Provider base URL must be HTTP or HTTPS; enter credentials in the authentication field"
                );
            }
            let mut models = std::collections::HashSet::new();
            for model in &provider.models {
                model.validate()?;
                if !models.insert(&model.id) {
                    bail!(
                        "Duplicate model ID {} in provider {}",
                        model.id,
                        provider.id
                    );
                }
            }
        }
        let mut models = std::collections::HashSet::new();
        for model in &self.llama.models {
            model.validate()?;
            if !models.insert(&model.id) {
                bail!("Duplicate llama.cpp model ID {}", model.id);
            }
        }
        let default = crate::models::find_model(self, &self.default_model, false, false)
            .with_context(|| format!("Unknown default model: {}", self.default_model))?;
        if !default.thinking_levels.contains(&self.default_thinking) {
            bail!(
                "Default thinking level {} is not supported by {}",
                self.default_thinking,
                self.default_model
            );
        }
        Ok(())
    }
    pub fn save(&self, paths: &AppPaths) -> Result<()> {
        self.validate_models()?;
        if !self.llama.models.is_empty() {
            let contents = crate::llama::preset_contents(
                &self.llama,
                crate::llama::flash_attention_supported(),
            )?;
            atomic_private_bytes(&paths.config.join("llama-models.ini"), contents.as_bytes())?;
        }
        atomic_private_json(&paths.config_file(), self)
    }
}

pub fn atomic_private_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    atomic_private_bytes(path, &data)
}

pub fn atomic_private_json_create<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    publish_private_bytes(path, &data, false)
}

pub fn atomic_private_bytes(path: &Path, data: &[u8]) -> Result<()> {
    publish_private_bytes(path, data, true)
}

fn publish_private_bytes(path: &Path, data: &[u8], replace: bool) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().context("configuration file has no parent")?;
    crate::paths::ensure_private_dir(parent)?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(data)?;
        file.sync_all()?;
        if replace {
            fs::rename(&temp, path)?;
        } else {
            // Same-directory hard-link publication fails atomically if the name
            // already exists, including a dangling symlink. No partial identity
            // can be observed and no concurrently created identity is replaced.
            fs::hard_link(&temp, path)?;
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result?;
    OpenOptions::new().read(true).open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn private_publication_is_atomic_and_create_never_replaces_an_identity() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("identity.json");
        super::atomic_private_json_create(&path, &serde_json::json!({"first":true})).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(
            super::atomic_private_json_create(&path, &serde_json::json!({"second":true})).is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let dangling = root.path().join("dangling.json");
        symlink(root.path().join("missing"), &dangling).unwrap();
        assert!(super::atomic_private_json_create(&dangling, &serde_json::json!({})).is_err());
        assert!(std::fs::symlink_metadata(&dangling).unwrap().is_symlink());
        assert!(!root.path().join("missing").exists());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {
            for number in 0..8 {
                let path = &path;
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    super::atomic_private_json(
                        path,
                        &serde_json::json!({"writer":number,"payload":"x".repeat(16_384)}),
                    )
                    .unwrap();
                });
            }
        });
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["payload"].as_str().unwrap().len(), 16_384);
        assert!(value["writer"].as_u64().unwrap() < 8);
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }

    use super::*;
    #[test]
    fn default_pair_uses_registered_models_and_both_api_url_schemes() {
        let mut config = AppConfig::default();
        // Authentication is checked at launch; a logged-out account can keep a
        // valid default and still save unrelated application settings.
        config.validate_models().unwrap();
        config.compatible_providers.push(CompatibleProvider {
            id: "vllm".into(),
            name: "Local vLLM".into(),
            base_url: "http://127.0.0.1:8000/v1".into(),
            auth: CompatibleAuth::None,
            models: vec![ModelPreset {
                id: "local-model".into(),
                ..Default::default()
            }],
        });
        config.default_model = "vllm/local-model".into();
        config.default_thinking = "off".into();
        for base in [
            "http://127.0.0.1:8000/v1",
            "http://inference.lan:8000/v1",
            "https://inference.example/v1",
        ] {
            config.compatible_providers[0].base_url = base.into();
            config.validate_models().unwrap();
        }
        config.default_thinking = "high".into();
        assert!(
            config
                .validate_models()
                .unwrap_err()
                .to_string()
                .contains("Default thinking level")
        );
        config.default_thinking = "off".into();
        config.compatible_providers[0].models.clear();
        assert!(
            config
                .validate_models()
                .unwrap_err()
                .to_string()
                .contains("Unknown default model")
        );
    }
}
