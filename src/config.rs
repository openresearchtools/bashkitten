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
    pub default_cwd: PathBuf,
    pub default_model: String,
    pub default_thinking: String,
    pub compatible_providers: Vec<CompatibleProvider>,
    pub llama: LlamaConfig,
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
    pub request_parameters: Value,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LlamaConfig {
    pub enabled: bool,
    pub models_dir: PathBuf,
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
            default_cwd: home.clone(),
            default_model: "openai-codex/gpt-5.5".into(),
            default_thinking: "medium".into(),
            compatible_providers: Vec::new(),
            llama: LlamaConfig {
                enabled: false,
                models_dir: home.join("models"),
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
            request_parameters: Value::Object(Default::default()),
            supports_developer_role: false,
            supports_reasoning_effort: false,
        }
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
        let data = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_slice(&data).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self, paths: &AppPaths) -> Result<()> {
        atomic_private_json(&paths.config_file(), self)
    }
}

pub fn atomic_private_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("configuration file has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    set_private_file(&temp)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    set_private_file(path)?;
    if let Ok(dir) = OpenOptions::new().read(true).open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}
