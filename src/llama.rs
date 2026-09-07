//! Native port of pinned Pi extensions/llama/client.ts. Router management is
//! distinct from the OpenAI completions inference endpoint.
use crate::config::{GpuLayers, LlamaConfig};
use crate::tools::CancellationToken;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Installation {
    pub package: String,
    pub backend: String,
}

/// Explicit local-settings addition. See docs/settings-local-models.md.
/// These entries are applied only to the independently managed llama-server.
pub fn launch_environment(config: &LlamaConfig) -> Result<BTreeMap<String, String>> {
    for (key, value) in &config.gpu_environment {
        if !matches!(
            key.as_str(),
            "CUDA_VISIBLE_DEVICES" | "GGML_VK_VISIBLE_DEVICES"
        ) {
            bail!("Unsupported GPU visibility variable: {key}");
        }
        if value.chars().any(char::is_control) {
            bail!("GPU visibility values must not contain control characters");
        }
        if key == "GGML_VK_VISIBLE_DEVICES" {
            if value
                .split([',', ' '])
                .filter(|v| !v.is_empty())
                .any(|v| v.parse::<u32>().is_err())
            {
                bail!(
                    "GGML_VK_VISIBLE_DEVICES requires nonnegative indices separated by commas or spaces"
                );
            }
        } else if !value.is_empty()
            && value.split(',').any(|v| {
                let v = v.trim();
                v.is_empty()
                    || !v
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/'))
            })
        {
            bail!(
                "CUDA_VISIBLE_DEVICES requires comma-separated GPU indices, UUIDs or MIG identifiers"
            );
        }
    }
    Ok(config.gpu_environment.clone())
}

pub fn validate_local_settings(config: &LlamaConfig) -> Result<()> {
    if config.port == 0 {
        bail!("Router port must be between 1 and 65535");
    }
    launch_environment(config)?;
    for device in &config.gpu_devices {
        if device.is_empty()
            || device.len() > 128
            || device
                .chars()
                .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.' | '/')))
        {
            bail!("Invalid llama.cpp device selector");
        }
    }
    if config.fit_target_mib == 0 {
        bail!("Fit target margin must be greater than zero");
    }
    if config.fit_context == 0
        || (config.context_size > 0 && config.fit_context > config.context_size)
    {
        bail!("Fit minimum context must be positive and no larger than the router context size");
    }
    for path in &config.model_search_dirs {
        if !path.is_absolute()
            || path
                .as_os_str()
                .to_string_lossy()
                .chars()
                .any(char::is_control)
        {
            bail!("Custom model folders must be absolute paths without control characters");
        }
    }
    for preset in &config.models {
        if preset.id.is_empty() || preset.id.contains(['\n', '\r', '[', ']']) {
            bail!("Invalid llama.cpp preset name");
        }
        if !preset.llama_model_path.as_os_str().is_empty() {
            validate_local_model_path(&preset.llama_model_path)?;
            for line in preset.llama_options.lines() {
                let key = line.split('=').next().unwrap_or_default().trim();
                if matches!(
                    key,
                    "model"
                        | "m"
                        | "LLAMA_ARG_MODEL"
                        | "hf-repo"
                        | "hf"
                        | "hfr"
                        | "LLAMA_ARG_HF_REPO"
                        | "hf-file"
                        | "hff"
                        | "LLAMA_ARG_HF_FILE"
                        | "model-url"
                        | "mu"
                        | "LLAMA_ARG_MODEL_URL"
                        | "docker-repo"
                        | "dr"
                        | "LLAMA_ARG_DOCKER_REPO"
                ) {
                    bail!(
                        "Remove model-source overrides from the preset when a local GGUF file is selected"
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_local_model_path(path: &Path) -> Result<()> {
    let value = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Local model path must be valid UTF-8"))?;
    if !path.is_absolute() || !is_model_file(path) {
        bail!("Select an absolute GGUF model path");
    }
    // The installed native INI grammar has no quoting or escaping syntax.
    if value.chars().any(char::is_control)
        || value.contains([';', '#'])
        || value.trim_end() != value
    {
        bail!(
            "Local model path cannot contain control characters, ;, #, or trailing whitespace in a llama.cpp preset"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSource {
    pub kind: String,
    pub label: String,
    pub path: PathBuf,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalModel {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub source: String,
    pub source_path: PathBuf,
    pub split_count: u32,
    pub missing_parts: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
}

fn model_sources(
    config: &LlamaConfig,
    home: &Path,
    env: impl Fn(&str) -> Option<PathBuf>,
) -> Vec<LocalSource> {
    let cache = env("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"));
    let hf_cache = env("HF_HUB_CACHE")
        .or_else(|| env("HUGGINGFACE_HUB_CACHE"))
        .unwrap_or_else(|| {
            env("HF_HOME")
                .unwrap_or_else(|| cache.join("huggingface"))
                .join("hub")
        });
    let mut candidates = vec![
        (
            "router",
            "Router models directory",
            config.models_dir.clone(),
        ),
        (
            "llama-cache",
            "llama.cpp cache",
            env("LLAMA_CACHE").unwrap_or_else(|| cache.join("llama.cpp")),
        ),
        ("huggingface", "Hugging Face cache", hf_cache),
        (
            "lmstudio",
            "LM Studio models",
            home.join(".lmstudio/models"),
        ),
        (
            "lmstudio-legacy",
            "LM Studio legacy models",
            home.join(".cache/lm-studio/models"),
        ),
    ];
    candidates.extend(
        config
            .model_search_dirs
            .iter()
            .cloned()
            .map(|p| ("custom", "Custom model folder", p)),
    );
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter_map(|(kind, label, path)| {
            if !seen.insert(std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone())) {
                return None;
            }
            let (exists, error) = match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_dir() => (true, None),
                Ok(_) => (false, Some("Not a directory".into())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (false, None),
                Err(error) => (false, Some(error.to_string())),
            };
            Some(LocalSource {
                kind: kind.into(),
                label: label.into(),
                error,
                path,
                exists,
            })
        })
        .collect()
}

fn is_model_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    name.ends_with(".gguf")
        && !name.contains("mmproj")
        && !["mtp-", "dspark-", "dflash-"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

fn split_model(path: &Path) -> Option<(String, u32, u32)> {
    let name = path.file_name()?.to_str()?;
    let without_ext = name
        .strip_suffix(".gguf")
        .or_else(|| name.strip_suffix(".GGUF"))?;
    let (before, total) = without_ext.rsplit_once("-of-")?;
    let (prefix, index) = before.rsplit_once('-')?;
    if index.len() != 5
        || total.len() != 5
        || !index
            .bytes()
            .chain(total.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let (index, total) = (index.parse().ok()?, total.parse().ok()?);
    if index == 0 || total == 0 || index > total {
        return None;
    }
    Some((prefix.into(), index, total))
}

fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| (m.dev(), m.ino()))
}

/// Read local filenames and metadata only. No provider/Hugging Face requests.
pub fn discover_local_models(config: &LlamaConfig) -> (Vec<LocalSource>, Vec<LocalModel>) {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let sources = model_sources(config, &home, |key| {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    });
    scan_local_sources(config, sources)
}

fn scan_local_sources(
    config: &LlamaConfig,
    mut sources: Vec<LocalSource>,
) -> (Vec<LocalSource>, Vec<LocalModel>) {
    use sha2::{Digest, Sha256};
    let mut models: Vec<LocalModel> = Vec::new();
    let mut identities = BTreeMap::<(u64, u64), usize>::new();
    for source in &mut sources {
        if !source.exists {
            continue;
        }
        let mut files = BTreeSet::new();
        let mut directories = std::collections::VecDeque::from([source.path.clone()]);
        let mut visited = HashSet::new();
        while let Some(directory) = directories.pop_front() {
            let identity = std::fs::canonicalize(&directory).unwrap_or_else(|_| directory.clone());
            if !visited.insert(identity) {
                continue;
            }
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    source
                        .error
                        .get_or_insert_with(|| format!("{}: {error}", directory.display()));
                    continue;
                }
            };
            let mut entry_paths = Vec::new();
            for entry in entries {
                match entry {
                    Ok(entry) => entry_paths.push(entry.path()),
                    Err(error) => {
                        source.error.get_or_insert_with(|| error.to_string());
                    }
                }
            }
            entry_paths.sort();
            for path in entry_paths {
                match std::fs::metadata(&path) {
                    Ok(metadata) if metadata.is_dir() => directories.push_back(path),
                    Ok(metadata) if metadata.is_file() && is_model_file(&path) => {
                        files.insert(path);
                    }
                    _ => {}
                }
            }
        }
        let mut groups = BTreeSet::new();
        for path in files {
            let (first, parts) = if let Some((prefix, _, total)) = split_model(&path) {
                let extension = path.extension().unwrap().to_string_lossy();
                let parent = path.parent().unwrap();
                let parts = (1..=total)
                    .map(|i| parent.join(format!("{prefix}-{i:05}-of-{total:05}.{extension}")))
                    .collect::<Vec<_>>();
                (parts[0].clone(), parts)
            } else {
                (path.clone(), vec![path.clone()])
            };
            if !groups.insert(first.clone()) {
                continue;
            }
            let Some(key) = parts.iter().find_map(|p| file_identity(p)) else {
                continue;
            };
            let missing_parts = parts
                .iter()
                .filter(|p| file_identity(p).is_none())
                .cloned()
                .collect::<Vec<_>>();
            // Prefer a complete snapshot if two HF revisions share a first shard.
            let old = identities.get(&key).copied();
            if old.is_some_and(|i| models[i].missing_parts.len() <= missing_parts.len()) {
                continue;
            }
            let size_bytes = parts
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum();
            let canonical = std::fs::canonicalize(&first).unwrap_or_else(|_| first.clone());
            let digest = hex::encode(Sha256::digest(canonical.as_os_str().as_encoded_bytes()));
            let name = first
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let prefix: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                        c
                    } else {
                        '-'
                    }
                })
                .take(64)
                .collect();
            let preset_id = config
                .models
                .iter()
                .find(|p| file_identity(&p.llama_model_path) == Some(key))
                .map(|p| p.id.clone());
            let model = LocalModel {
                id: format!("local-{prefix}-{}", &digest[..12]),
                name,
                path: first,
                size_bytes,
                source: source.kind.clone(),
                source_path: source.path.clone(),
                split_count: parts.len() as u32,
                missing_parts,
                preset_id,
            };
            if let Some(i) = old {
                models[i] = model;
            } else {
                identities.insert(key, models.len());
                models.push(model);
            }
        }
    }
    models.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    (sources, models)
}

pub fn validate_local_preset(
    config: &LlamaConfig,
    preset: &crate::config::ModelPreset,
) -> Result<()> {
    validate_local_model_path(&preset.llama_model_path)?;
    let (_, discovered) = discover_local_models(config);
    let model = discovered
        .iter()
        .find(|m| {
            m.path == preset.llama_model_path
                || file_identity(&m.path)
                    .is_some_and(|key| Some(key) == file_identity(&preset.llama_model_path))
        })
        .ok_or_else(|| anyhow::anyhow!("Select a GGUF file from a configured model folder"))?;
    if !model.missing_parts.is_empty() {
        bail!(
            "The selected split GGUF is incomplete; {} parts are missing",
            model.missing_parts.len()
        );
    }
    if let Some((prefix, index, total)) = split_model(&preset.llama_model_path) {
        if index != 1 {
            bail!("Select the first shard of a split GGUF model");
        }
        let parent = preset.llama_model_path.parent().unwrap();
        let extension = preset
            .llama_model_path
            .extension()
            .unwrap()
            .to_string_lossy();
        let missing = (1..=total)
            .filter(|i| {
                file_identity(&parent.join(format!("{prefix}-{i:05}-of-{total:05}.{extension}")))
                    .is_none()
            })
            .count();
        if missing > 0 {
            bail!("The selected split GGUF is incomplete; {missing} parts are missing");
        }
    }
    Ok(())
}

pub fn detect_installation() -> Option<Installation> {
    if !std::path::Path::new("/usr/bin/llama-server").is_file() {
        return None;
    }
    let output = std::process::Command::new("dpkg-query")
        .args([
            "-W",
            "-f=${binary:Package}\t${Status}\n",
            "llama-cpp-cuda",
            "llama-cpp",
        ])
        .output()
        .ok()?;
    installed_package(&String::from_utf8_lossy(&output.stdout))
}

fn installed_package(output: &str) -> Option<Installation> {
    for (package, backend) in [("llama-cpp-cuda", "CUDA"), ("llama-cpp", "CPU/Vulkan")] {
        if output.lines().any(|line| {
            line.split_once('\t').is_some_and(|(name, status)| {
                name.split(':').next() == Some(package) && status == "install ok installed"
            })
        }) {
            return Some(Installation {
                package: package.into(),
                backend: backend.into(),
            });
        }
    }
    None
}

/// A device advertised by the installed llama.cpp build. IDs are the exact
/// values accepted by `--device`; the display fields are parsed only for a
/// friendly settings picker.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LlamaDevice {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_memory_mib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_memory_mib: Option<u64>,
}

const DEVICE_OUTPUT_LIMIT: usize = 64 * 1024;
const DEVICE_LINE_LIMIT: usize = 512;
const DEVICE_COUNT_LIMIT: usize = 64;
const DEVICE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Parse `llama-server --list-devices` output. The format is intentionally
/// line based because it is a human-facing diagnostic, while the selector ID
/// before the first colon is stable and is what `--device` consumes.
pub fn parse_devices(output: &str) -> Vec<LlamaDevice> {
    output
        .lines()
        .take(DEVICE_COUNT_LIMIT * 2)
        .filter_map(|line| {
            if line.len() > DEVICE_LINE_LIMIT {
                return None;
            }
            let line = line.trim();
            let (id, details) = line.split_once(':')?;
            let id = id.trim();
            if id.is_empty()
                || id.len() > 128
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
                || (!id.chars().any(|c| c.is_ascii_digit()) && id != "CPU")
            {
                return None;
            }
            let (name, memory) = details
                .rsplit_once('(')
                .map_or((details.trim(), ""), |(name, memory)| {
                    (name.trim(), memory.trim_end_matches(')').trim())
                });
            if name.is_empty() {
                return None;
            }
            let mut numbers = memory.split(',').filter_map(|part| {
                let mut words = part.split_whitespace();
                let value = words.next()?.parse::<u64>().ok()?;
                (words.next()? == "MiB").then_some(value)
            });
            Some(LlamaDevice {
                id: id.into(),
                name: name.into(),
                total_memory_mib: numbers.next(),
                free_memory_mib: numbers.next(),
            })
        })
        .take(DEVICE_COUNT_LIMIT)
        .collect()
}

/// Discover devices through the installed binary with the configured CUDA /
/// Vulkan visibility environment. No hardware probing or network request is
/// performed by BashKitten itself.
pub fn list_devices(config: &LlamaConfig) -> Result<Vec<LlamaDevice>> {
    validate_local_settings(config)?;
    let mut child = std::process::Command::new("/usr/bin/llama-server")
        .arg("--list-devices")
        .envs(launch_environment(config)?)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("run llama-server --list-devices")?;
    let deadline = Instant::now() + DEVICE_PROBE_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "llama-server --list-devices exceeded {} seconds",
                DEVICE_PROBE_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child
        .wait_with_output()
        .context("read llama-server --list-devices")?;
    let mut bytes = output.stdout;
    bytes.extend_from_slice(&output.stderr);
    if bytes.len() > DEVICE_OUTPUT_LIMIT {
        bail!(
            "llama-server device listing exceeded {} bytes",
            DEVICE_OUTPUT_LIMIT
        );
    }
    let text = String::from_utf8_lossy(&bytes);
    if !output.status.success() {
        bail!("llama-server --list-devices failed: {}", text.trim());
    }
    Ok(parse_devices(&text))
}

fn server_help() -> String {
    let output = std::process::Command::new("/usr/bin/llama-server")
        .arg("--help")
        .output();
    output
        .map(|output| {
            let mut bytes = output.stdout;
            bytes.extend_from_slice(&output.stderr);
            String::from_utf8_lossy(&bytes).into_owned()
        })
        .unwrap_or_default()
}

pub fn fit_supported() -> bool {
    server_help().contains("--fit")
}

pub fn predict_supported() -> bool {
    server_help().contains("--n-predict") || server_help().contains("--predict")
}

/// llama.cpp's native fitter rejects any explicit `n_gpu_layers`, including
/// the otherwise convenient `-ngl 999` Auto setting. Leave that parameter
/// unset only for the Auto/all + fit combination; explicit CPU/count choices
/// are intentionally represented with fit disabled.
fn fit_active(config: &LlamaConfig) -> bool {
    config.fit && matches!(config.gpu_layers, GpuLayers::Auto)
}

/// Debian launcher difference: router only, owned by its existing systemd unit.
/// Never return secret-bearing arguments to the browser.
pub fn launch_arguments(
    config: &LlamaConfig,
    reveal_key: bool,
    flash_supported: bool,
) -> Result<Vec<String>> {
    validate_local_settings(config)?;
    for arg in &config.extra_arguments {
        let flag = arg.split('=').next().unwrap_or(arg);
        if matches!(
            flag,
            "-m" | "--model"
                | "-hf"
                | "-hfr"
                | "--hf-repo"
                | "-hff"
                | "--hf-file"
                | "-mu"
                | "--model-url"
                | "-dr"
                | "--docker-repo"
        ) || flag.ends_with("-default")
            || flag.starts_with("--fim-")
        {
            bail!("llama.cpp must run in router mode; {flag} selects a single model");
        }
    }
    let layers = match config.gpu_layers {
        GpuLayers::Auto => 999,
        GpuLayers::Cpu => 0,
        GpuLayers::Count(n) => n,
    };
    let mut args = vec![
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        config.port.to_string(),
        "--models-dir".into(),
        config.models_dir.to_string_lossy().into_owned(),
        "--jinja".into(),
        "--batch-size".into(),
        config.batch_size.to_string(),
        "--parallel".into(),
        config.parallel_slots.to_string(),
        // Prevent the router's default maximum from silently evicting other models.
        "--models-max".into(),
        "0".into(),
        if config.autoload {
            "--models-autoload"
        } else {
            "--no-models-autoload"
        }
        .into(),
        if config.mmap { "--mmap" } else { "--no-mmap" }.into(),
    ];
    if config.context_size > 0 {
        // llama.cpp documents zero as "loaded from model". Leaving this
        // unset is required for native model context and --fit adjustment.
        let jinja = args.iter().position(|arg| arg == "--jinja").unwrap() + 1;
        args.splice(
            jinja..jinja,
            ["--ctx-size".into(), config.context_size.to_string()],
        );
    }
    if !fit_supported() || !fit_active(config) {
        args.extend(["-ngl".into(), layers.to_string()]);
    }
    if !config.gpu_devices.is_empty() {
        args.extend(["--device".into(), config.gpu_devices.join(",")]);
    }
    if fit_supported() {
        let active = fit_active(config);
        args.extend(["--fit".into(), if active { "on" } else { "off" }.into()]);
        if active {
            args.extend([
                "--fit-target".into(),
                config.fit_target_mib.to_string(),
                "--fit-ctx".into(),
                config.fit_context.to_string(),
            ]);
        }
    }
    if config.max_new_tokens > 0 && predict_supported() {
        args.extend(["--n-predict".into(), config.max_new_tokens.to_string()]);
    }
    if config.cpu_threads > 0 {
        args.extend(["--threads".into(), config.cpu_threads.to_string()]);
    }
    if flash_supported {
        args.extend([
            "--flash-attn".into(),
            if config.flash_attention { "on" } else { "off" }.into(),
        ]);
    }
    if config.mlock {
        args.push("--mlock".into());
    }
    if !config.api_key.is_empty() {
        args.extend([
            "--api-key".into(),
            if reveal_key {
                config.api_key.clone()
            } else {
                "<stored API key>".into()
            },
        ]);
    }
    args.extend(config.extra_arguments.clone());
    if !reveal_key {
        args = redact_extra_arguments(&args);
    }
    Ok(args)
}

// Advanced launch options may contain credentials too. The browser receives
// opaque placeholders; saves restore only matching existing credential slots.
const STORED_SECRET: &str = "<stored secret>";
fn secret_slots(args: &[String]) -> Vec<(usize, String, String, String)> {
    let mut slots = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let (flag, inline) = args[index]
            .split_once('=')
            .map_or((args[index].as_str(), None), |(a, b)| (a, Some(b)));
        if matches!(flag, "--api-key" | "--hf-token" | "-hft") {
            if let Some(value) = inline {
                slots.push((index, flag.into(), format!("{flag}="), value.into()));
            } else if let Some(value) = args.get(index + 1) {
                slots.push((index + 1, flag.into(), String::new(), value.clone()));
                index += 1;
            }
        }
        index += 1;
    }
    slots
}
pub fn redact_extra_arguments(args: &[String]) -> Vec<String> {
    let mut redacted = args.to_vec();
    for (index, _, prefix, _) in secret_slots(args) {
        redacted[index] = format!("{prefix}{STORED_SECRET}");
    }
    redacted
}
pub fn restore_extra_arguments(args: &mut [String], stored: &[String]) -> Result<()> {
    let mut previous =
        std::collections::HashMap::<String, std::collections::VecDeque<String>>::new();
    for (_, flag, _, value) in secret_slots(stored) {
        previous.entry(flag).or_default().push_back(value);
    }
    for (index, flag, prefix, value) in secret_slots(args) {
        let old = previous
            .get_mut(&flag)
            .and_then(|values| values.pop_front());
        if value == STORED_SECRET {
            let Some(old) = old else {
                bail!(
                    "Stored credential for {flag} no longer exists; enter a replacement or remove the option"
                );
            };
            args[index] = format!("{prefix}{old}");
        }
    }
    Ok(())
}

pub fn flash_attention_supported() -> bool {
    server_help().contains("--flash-attn")
}

/// Native llama.cpp INI, with its documented global < model < CLI precedence.
/// Once presets are configured, model defaults move to [*] so a per-model value
/// can override them. Router-only switches remain direct command arguments.
pub fn preset_contents(config: &LlamaConfig, flash_supported: bool) -> Result<String> {
    validate_local_settings(config)?;
    let layers = match config.gpu_layers {
        GpuLayers::Auto => 999,
        GpuLayers::Cpu => 0,
        GpuLayers::Count(n) => n,
    };
    let mut text = format!(
        "version = 1\n\n[*]\njinja = true\nbatch-size = {}\nparallel = {}\nmmap = {}\nmlock = {}\n",
        config.batch_size, config.parallel_slots, config.mmap, config.mlock
    );
    if !fit_supported() || !fit_active(config) {
        text.insert_str(
            "version = 1\n\n[*]\njinja = true\n".len(),
            &format!("ngl = {layers}\n"),
        );
    }
    if config.context_size > 0 {
        text.insert_str(
            "version = 1\n\n[*]\njinja = true\n".len(),
            &format!("ctx-size = {}\n", config.context_size),
        );
    }
    if !config.gpu_devices.is_empty() {
        text.push_str(&format!("device = {}\n", config.gpu_devices.join(",")));
    }
    if fit_supported() {
        let active = fit_active(config);
        text.push_str(&format!("fit = {}\n", if active { "on" } else { "off" },));
        if active {
            text.push_str(&format!(
                "fit-target = {}\nfit-ctx = {}\n",
                config.fit_target_mib, config.fit_context
            ));
        }
    }
    if config.max_new_tokens > 0 && predict_supported() {
        text.push_str(&format!("n-predict = {}\n", config.max_new_tokens));
    }
    if config.cpu_threads > 0 {
        text.push_str(&format!("threads = {}\n", config.cpu_threads));
    }
    if flash_supported {
        text.push_str(&format!(
            "flash-attn = {}\n",
            if config.flash_attention { "on" } else { "off" }
        ));
    }
    for model in &config.models {
        if model.id.is_empty() || model.id.contains(['\n', '\r', '[', ']']) {
            bail!("Invalid llama.cpp preset name");
        }
        for line in model.llama_options.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                bail!(
                    "Model overrides contain INI entries only; the model ID supplies the section name"
                );
            }
            let key = line.split('=').next().unwrap_or_default().trim();
            if matches!(
                key,
                "api-key" | "hf-token" | "hft" | "LLAMA_API_KEY" | "HF_TOKEN"
            ) {
                bail!(
                    "Keep credentials in the dedicated provider settings or HF_TOKEN environment variable"
                );
            }
        }
        text.push_str(&format!(
            "\n[{}]\nctx-size = {}\n",
            model.id, model.context_window
        ));
        if let Some(value) = model.llama_max_new_tokens
            && value > 0
            && predict_supported()
        {
            text.push_str(&format!("n-predict = {value}\n"));
        }
        if fit_supported() {
            if let Some(value) = model.llama_fit {
                text.push_str(&format!(
                    "fit = {}\n",
                    if value && fit_active(config) {
                        "on"
                    } else {
                        "off"
                    }
                ));
            }
            if fit_active(config) {
                if let Some(value) = model.llama_fit_target_mib {
                    text.push_str(&format!("fit-target = {value}\n"));
                }
                if let Some(value) = model.llama_fit_context {
                    text.push_str(&format!("fit-ctx = {value}\n"));
                }
            }
        }
        if !model.llama_model_path.as_os_str().is_empty() {
            text.push_str(&format!("model = {}\n", model.llama_model_path.display()));
        }
        text.push_str(&model.llama_options);
        text.push('\n');
    }
    Ok(text)
}

pub fn managed_launch_arguments(
    config: &LlamaConfig,
    paths: &crate::paths::AppPaths,
    reveal_key: bool,
    flash_supported: bool,
) -> Result<Vec<String>> {
    let args = launch_arguments(config, reveal_key, flash_supported)?;
    if config.models.is_empty() {
        return Ok(args);
    }
    // Only remove the generated defaults; explicit advanced CLI arguments retain
    // native llama.cpp's highest precedence and are shown as supplied.
    let generated_len = args.len() - config.extra_arguments.len();
    let mut result = Vec::new();
    let mut index = 0;
    while index < generated_len {
        let key = args[index].as_str();
        if matches!(
            key,
            "--ctx-size"
                | "-ngl"
                | "--batch-size"
                | "--parallel"
                | "--threads"
                | "--flash-attn"
                | "--device"
                | "--fit"
                | "--fit-target"
                | "--fit-ctx"
                | "--n-predict"
        ) {
            index += 2;
        } else if matches!(key, "--jinja" | "--mmap" | "--no-mmap" | "--mlock") {
            index += 1;
        } else {
            result.push(args[index].clone());
            index += 1;
        }
    }
    result.extend([
        "--models-preset".into(),
        paths
            .config
            .join("llama-models.ini")
            .to_string_lossy()
            .into_owned(),
    ]);
    result.extend_from_slice(&args[generated_len..]);
    Ok(result)
}

pub fn normalize_server_url(value: &str) -> Result<String> {
    let mut url = url::Url::parse(value.trim()).map_err(|_| anyhow::anyhow!("Invalid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Server URL must use http or https");
    }
    url.set_fragment(None);
    url.set_query(None);
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/v1").unwrap_or(path).to_owned();
    url.set_path(if path.is_empty() { "/" } else { &path });
    Ok(url.to_string().trim_end_matches('/').into())
}

pub fn format_bytes(bytes: f64) -> String {
    if bytes < 1024.0 {
        return format!("{bytes} B");
    }
    let mut value = bytes / 1024.0;
    let mut unit = "KiB";
    for next in ["MiB", "GiB", "TiB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!(
        "{} {unit}",
        crate::usage::fixed(value, if value >= 10.0 { 1 } else { 2 })
    )
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Progress {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn message_progress(message: &str) -> Progress {
    Progress {
        message: message.into(),
        ratio: None,
        detail: None,
    }
}

pub fn load_progress(data: &Value) -> Option<Progress> {
    let progress = data.get("progress")?.as_object()?;
    let stage = progress
        .get("current")
        .and_then(Value::as_str)
        .or_else(|| progress.get("stage").and_then(Value::as_str));
    let stages: Vec<_> = progress
        .get("stages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut ratio = progress
        .get("value")
        .and_then(Value::as_f64)
        .map(|n| n.clamp(0.0, 1.0));
    if let Some(index) = stage
        .filter(|s| !s.is_empty())
        .and_then(|s| stages.iter().position(|v| *v == s))
    {
        ratio = Some((index as f64 + ratio.unwrap_or(0.0)) / stages.len() as f64);
    }
    Some(Progress {
        message: stage
            .filter(|s| !s.is_empty())
            .map(|s| format!("Loading {}", s.replace('_', " ")))
            .unwrap_or_else(|| "Loading model".into()),
        ratio,
        detail: None,
    })
}

pub fn download_progress(data: &Value) -> Option<Progress> {
    let files = data
        .get("progress")
        .filter(|p| p.is_object() || p.is_array())
        .unwrap_or(data);
    let values: Vec<_> = match files {
        Value::Object(v) => v.values().collect(),
        Value::Array(v) => v.iter().collect(),
        _ => return None,
    };
    let (mut done, mut total) = (0.0, 0.0);
    for entry in values {
        if let (Some(d), Some(t)) = (entry["done"].as_f64(), entry["total"].as_f64()) {
            done += d;
            total += t;
        }
    }
    if total <= 0.0 {
        return None;
    }
    Some(Progress {
        message: "Downloading model".into(),
        ratio: Some(done / total),
        detail: Some(format!("{} / {}", format_bytes(done), format_bytes(total))),
    })
}

pub fn error_message(payload: &Value, fallback: &str) -> String {
    payload["error"]["message"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .into()
}

pub fn validate_catalog(payload: Value) -> Result<Vec<Value>> {
    let Some(data) = payload.get("data").and_then(Value::as_array) else {
        bail!("llama.cpp returned an invalid model catalog");
    };
    if !data
        .iter()
        .all(|m| m["id"].is_string() && m["status"]["value"].is_string())
    {
        bail!("Server is not running in llama.cpp router mode");
    }
    Ok(data.clone())
}

pub fn selectable(model: &Value, autoload: bool) -> bool {
    matches!(
        model["status"]["value"].as_str(),
        Some("loaded" | "sleeping")
    ) || (autoload
        && model["status"]["value"] == "unloaded"
        && model["status"]["failed"] != true
        && model["source"] == "preset")
}

pub fn model_preset(model: &Value) -> crate::config::ModelPreset {
    let context = model["meta"]
        .get("n_ctx")
        .filter(|n| !n.is_null())
        .or_else(|| model["meta"].get("n_ctx_train"))
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(128_000);
    let id = model["id"].as_str().unwrap_or_default().to_string();
    let input = if model["architecture"]["input_modalities"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v == "image"))
    {
        vec!["text".into(), "image".into()]
    } else {
        vec!["text".into()]
    };
    crate::config::ModelPreset {
        id: id.clone(),
        name: id,
        context_window: context,
        max_tokens: context,
        input,
        ..Default::default()
    }
}

#[derive(Clone)]
pub struct Client {
    pub server_url: String,
    api_key: String,
    http: reqwest::Client,
}

pub type OnProgress = Arc<dyn Fn(Progress) + Send + Sync>;

impl Client {
    pub fn new(url: &str, api_key: &str) -> Result<Self> {
        Ok(Self {
            server_url: normalize_server_url(url)?,
            api_key: api_key.into(),
            http: reqwest::Client::builder().no_proxy().build()?,
        })
    }
    pub fn configured(config: &LlamaConfig) -> Result<Self> {
        Self::new(
            &format!("http://127.0.0.1:{}", config.port),
            &config.api_key,
        )
    }
    pub(crate) fn matches_config(&self, config: &LlamaConfig) -> bool {
        self.server_url == format!("http://127.0.0.1:{}", config.port)
            && self.api_key == config.api_key
    }
    fn redact(&self, message: String) -> String {
        if self.api_key.is_empty() {
            message
        } else {
            message.replace(&self.api_key, "[redacted]")
        }
    }
    async fn request(
        &self,
        path: &str,
        body: Option<Value>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        // Allowed network category: explicitly configured model-provider router.
        let mut request = self.http.request(
            if body.is_some() {
                reqwest::Method::POST
            } else {
                reqwest::Method::GET
            },
            format!("{}{path}", self.server_url),
        );
        if let Some(body) = body {
            request = request.json(&body);
        }
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let operation = async {
            let response = request
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("fetch failed"))?;
            let status = response.status();
            let payload = response.json::<Value>().await.unwrap_or(Value::Null);
            if !status.is_success() {
                bail!(
                    "{}",
                    self.redact(error_message(
                        &payload,
                        &format!("llama.cpp returned HTTP {}", status.as_u16())
                    ))
                );
            }
            Ok(payload)
        };
        tokio::select! {
            biased;
            _=cancel.cancelled()=>bail!("This operation was aborted"),
            result=tokio::time::timeout(Duration::from_secs(15),operation)=>result.map_err(|_|anyhow::anyhow!("The operation was aborted due to timeout"))?,
        }
    }
    pub async fn list(&self, reload: bool, cancel: &CancellationToken) -> Result<Vec<Value>> {
        validate_catalog(
            self.request(
                if reload {
                    "/models?reload=1"
                } else {
                    "/models"
                },
                None,
                cancel,
            )
            .await?,
        )
    }
    pub async fn props(&self, cancel: &CancellationToken) -> Result<Value> {
        let payload = self.request("/props", None, cancel).await?;
        Ok(payload["models_autoload"]
            .as_bool()
            .map(|v| json!({"models_autoload":v}))
            .unwrap_or_else(|| json!({})))
    }
    pub async fn load(&self, model: &str, cancel: &CancellationToken) -> Result<()> {
        self.request("/models/load", Some(json!({"model":model})), cancel)
            .await?;
        Ok(())
    }
    pub async fn unload(&self, model: &str, cancel: &CancellationToken) -> Result<()> {
        self.request("/models/unload", Some(json!({"model":model})), cancel)
            .await?;
        Ok(())
    }
    pub async fn download(&self, model: &str, cancel: &CancellationToken) -> Result<()> {
        self.request("/models", Some(json!({"model":model})), cancel)
            .await?;
        Ok(())
    }
    pub async fn unload_and_wait(&self, model: &str, cancel: &CancellationToken) -> Result<()> {
        self.unload(model, cancel).await?;
        loop {
            if self
                .list(false, cancel)
                .await?
                .iter()
                .find(|m| m["id"] == model)
                .is_none_or(|m| m["status"]["value"] == "unloaded")
            {
                return Ok(());
            }
            sleep(100, cancel).await?;
        }
    }
    pub async fn watch(
        &self,
        on_event: Arc<dyn Fn(Value) + Send + Sync>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // Allowed network category: configured router's model progress stream.
        let mut request = self.http.get(format!("{}/models/sse", self.server_url));
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let response = tokio::select! {biased;_=cancel.cancelled()=>bail!("This operation was aborted"),r=request.send()=>r.map_err(|_|anyhow::anyhow!("fetch failed"))?};
        if !response.status().is_success() {
            bail!("llama.cpp SSE returned HTTP {}", response.status().as_u16());
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        let mut buffer = String::new();
        loop {
            let chunk = tokio::select! {biased;_=cancel.cancelled()=>bail!("This operation was aborted"),r=stream.next()=>r};
            let Some(chunk) = chunk else {
                return Ok(());
            };
            bytes.extend(chunk.map_err(|_| anyhow::anyhow!("terminated"))?);
            // TextDecoder(stream:true): retain incomplete UTF-8 across chunks.
            let valid = match std::str::from_utf8(&bytes) {
                Ok(_) => bytes.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => bytes.len(),
            };
            buffer.push_str(&String::from_utf8_lossy(&bytes[..valid]).replace("\r\n", "\n"));
            bytes.drain(..valid);
            while let Some(boundary) = buffer.find("\n\n") {
                let frame = buffer[..boundary].to_string();
                buffer.drain(..boundary + 2);
                let data = frame
                    .lines()
                    .filter_map(|s| s.strip_prefix("data:").map(str::trim_start))
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Ok(event) = serde_json::from_str::<Value>(&data)
                    && event["model"].is_string()
                    && event["event"].is_string()
                {
                    on_event(event);
                }
            }
        }
    }
    pub async fn load_and_wait(
        &self,
        model: &str,
        progress: OnProgress,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let state = Arc::new(Mutex::new((false, None::<String>)));
        let watched = state.clone();
        let target = model.to_string();
        let output = progress.clone();
        let watcher = self.spawn_watch(
            Arc::new(move |event| {
                if event["model"] != target
                    || !matches!(
                        event["event"].as_str(),
                        Some("model_status" | "status_change")
                    )
                {
                    return;
                }
                {
                    let mut state = watched.lock().unwrap();
                    if event["data"]["status"] == "loaded" {
                        state.0 = true;
                    }
                    if event["data"]["status"] == "unloaded" {
                        state.1 = Some("Model failed to load".into());
                    }
                }
                if let Some(p) = load_progress(&event["data"]) {
                    output(p);
                }
            }),
            cancel.clone(),
        );
        let _watcher = watcher;
        self.load(model, cancel).await?;
        progress(message_progress("Loading model"));
        loop {
            let entry = self
                .list(false, cancel)
                .await?
                .into_iter()
                .find(|m| m["id"] == model);
            if entry
                .as_ref()
                .is_some_and(|m| m["status"]["value"] == "loaded")
            {
                return Ok(entry.unwrap());
            }
            let (loaded, error) = state.lock().unwrap().clone();
            if loaded && entry.is_none() {
                return Ok(json!({"id":model,"status":{"value":"loaded"}}));
            }
            if error.is_some()
                || entry
                    .as_ref()
                    .is_some_and(|m| m["status"]["failed"] == true)
            {
                if let Some(code) = entry.as_ref().and_then(|m| m["status"].get("exit_code")) {
                    bail!("Model exited with code {code}");
                }
                bail!("{}", error.as_deref().unwrap_or("Model failed to load"));
            }
            sleep(250, cancel).await?;
        }
    }
    pub async fn download_and_wait(
        &self,
        model: &str,
        progress: OnProgress,
        cancel: &CancellationToken,
    ) -> Result<Vec<Value>> {
        let state = Arc::new(Mutex::new((false, None::<String>, false)));
        let watched = state.clone();
        let target = model.to_string();
        let output = progress.clone();
        let client = self.clone();
        let _watcher = self.spawn_watch(
            Arc::new(move |event| {
                if event["model"] != target {
                    return;
                }
                {
                    let mut state = watched.lock().unwrap();
                    if event["event"] == "download_finished" {
                        state.0 = true;
                    }
                    if event["event"] == "download_failed" {
                        state.1 =
                            Some(client.redact(error_message(&event["data"], "Download failed")));
                    }
                    if event["event"] == "download_progress" {
                        state.2 = true;
                    }
                }
                if event["event"] == "download_progress"
                    && let Some(p) = download_progress(&event["data"])
                {
                    output(p);
                }
            }),
            cancel.clone(),
        );
        self.download(model, cancel).await?;
        progress(message_progress("Downloading model"));
        let mut polls = 0;
        loop {
            if let Some(error) = state.lock().unwrap().1.clone() {
                bail!("{error}");
            }
            let models = self.list(false, cancel).await?;
            polls += 1;
            let entry = models.iter().find(|m| m["id"] == model);
            if entry.is_some_and(|m| m["status"]["value"] == "downloading") {
                state.lock().unwrap().2 = true;
                if let Some(p) = download_progress(&entry.unwrap()["status"]["progress"]) {
                    progress(p);
                }
            } else {
                let (finished, _, saw) = state.lock().unwrap().clone();
                if finished || (entry.is_some() && (saw || polls >= 2)) {
                    return self.list(true, cancel).await;
                }
            }
            sleep(500, cancel).await?;
        }
    }
    fn spawn_watch(
        &self,
        callback: Arc<dyn Fn(Value) + Send + Sync>,
        cancel: CancellationToken,
    ) -> AbortWatcher {
        let client = self.clone();
        AbortWatcher(tokio::spawn(async move {
            let _ = client.watch(callback, &cancel).await;
        }))
    }
}

struct AbortWatcher(tokio::task::JoinHandle<()>);
impl Drop for AbortWatcher {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn sleep(ms: u64, cancel: &CancellationToken) -> Result<()> {
    tokio::select! {biased;_=cancel.cancelled()=>bail!("This operation was aborted"),_=tokio::time::sleep(Duration::from_millis(ms))=>Ok(())}
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(path: PathBuf) -> LocalSource {
        LocalSource {
            kind: "custom".into(),
            label: "Fixture models".into(),
            exists: path.is_dir(),
            path,
            error: None,
        }
    }
    #[test]
    fn local_cache_sources_respect_hugging_face_environment_precedence() {
        let root = tempfile::tempdir().unwrap();
        let config = LlamaConfig {
            models_dir: root.path().join("router"),
            model_search_dirs: vec![root.path().join("custom")],
            ..Default::default()
        };
        let lookup = |values: &[(&str, &str)]| {
            model_sources(&config, root.path(), |key| {
                values
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| PathBuf::from(v))
            })
        };
        let find = |sources: Vec<LocalSource>, kind: &str| {
            sources.into_iter().find(|s| s.kind == kind).unwrap().path
        };
        assert_eq!(
            find(lookup(&[]), "huggingface"),
            root.path().join(".cache/huggingface/hub")
        );
        assert_eq!(
            find(lookup(&[("XDG_CACHE_HOME", "/xdg")]), "huggingface"),
            PathBuf::from("/xdg/huggingface/hub")
        );
        assert_eq!(
            find(
                lookup(&[("HF_HOME", "/hf"), ("XDG_CACHE_HOME", "/xdg")]),
                "huggingface"
            ),
            PathBuf::from("/hf/hub")
        );
        assert_eq!(
            find(
                lookup(&[
                    ("HF_HOME", "/hf"),
                    ("HUGGINGFACE_HUB_CACHE", "/legacy"),
                    ("HF_HUB_CACHE", "/hub")
                ]),
                "huggingface"
            ),
            PathBuf::from("/hub")
        );
        assert_eq!(
            find(lookup(&[("LLAMA_CACHE", "/llama")]), "llama-cache"),
            PathBuf::from("/llama")
        );
        assert_eq!(
            find(lookup(&[]), "lmstudio"),
            root.path().join(".lmstudio/models")
        );
        assert_eq!(
            find(lookup(&[]), "lmstudio-legacy"),
            root.path().join(".cache/lm-studio/models")
        );
    }
    #[test]
    fn local_discovery_preserves_snapshot_shards_deduplicates_and_avoids_symlink_cycles() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let snapshot = root.path().join("models--fixture/snapshots/revision");
        let blobs = root.path().join("models--fixture/blobs");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::create_dir_all(&blobs).unwrap();
        for index in 1..=2 {
            let blob = blobs.join(format!("blob{index}"));
            std::fs::write(&blob, vec![0; index * 4]).unwrap();
            symlink(
                &blob,
                snapshot.join(format!("fixture-Q4-0000{index}-of-00002.gguf")),
            )
            .unwrap();
        }
        let first = snapshot.join("fixture-Q4-00001-of-00002.gguf");
        symlink(root.path(), snapshot.join("cycle")).unwrap();
        std::fs::write(snapshot.join("mmproj-fixture.gguf"), [0; 4]).unwrap();
        std::fs::write(snapshot.join("fixture-mmproj.gguf"), [0; 4]).unwrap();
        std::fs::write(snapshot.join("mtp-fixture.gguf"), [0; 4]).unwrap();
        std::fs::write(snapshot.join("incomplete-00002-of-00003.gguf"), [0; 2]).unwrap();
        std::fs::write(snapshot.join("download.gguf.incomplete"), [0; 2]).unwrap();
        let config = LlamaConfig {
            models: vec![crate::config::ModelPreset {
                id: "saved-model".into(),
                llama_model_path: first.clone(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let (_, models) = scan_local_sources(
            &config,
            vec![
                source(root.path().into()),
                source(snapshot.clone()),
                source(root.path().join("missing")),
            ],
        );
        assert_eq!(models.len(), 2);
        let complete = models.iter().find(|m| m.missing_parts.is_empty()).unwrap();
        assert_eq!(complete.path, first);
        assert_eq!(complete.split_count, 2);
        assert_eq!(complete.size_bytes, 12);
        assert_eq!(complete.preset_id.as_deref(), Some("saved-model"));
        let incomplete = models.iter().find(|m| !m.missing_parts.is_empty()).unwrap();
        assert_eq!(incomplete.missing_parts.len(), 2);
        assert!(incomplete.path.ends_with("incomplete-00001-of-00003.gguf"));
        assert_eq!(std::fs::metadata(blobs.join("blob1")).unwrap().len(), 4);
    }
    #[test]
    fn local_presets_keep_native_model_path_and_reject_incomplete_or_conflicting_sources() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("local model.gguf");
        std::fs::write(&path, [0; 8]).unwrap();
        let preset = crate::config::ModelPreset {
            id: "local-model".into(),
            llama_model_path: path.clone(),
            ..Default::default()
        };
        let mut config = LlamaConfig {
            models_dir: root.path().into(),
            models: vec![preset.clone()],
            ..Default::default()
        };
        validate_local_preset(&config, &preset).unwrap();
        let ini = preset_contents(&config, true).unwrap();
        assert!(ini.contains(&format!("model = {}\n", path.display())));
        assert!(
            managed_launch_arguments(
                &config,
                &crate::paths::AppPaths {
                    config: root.path().into(),
                    data: root.path().into(),
                    runtime: root.path().into()
                },
                true,
                true
            )
            .unwrap()
            .iter()
            .all(|v| v != "--model")
        );
        config.models[0].llama_options = "m = /another.gguf".into();
        assert!(
            preset_contents(&config, true)
                .unwrap_err()
                .to_string()
                .contains("model-source")
        );
        config.models[0].llama_options.clear();
        config.models[0].llama_model_path = root.path().join("model#comment.gguf");
        assert!(preset_contents(&config, true).is_err());
        let incomplete = root.path().join("split-00001-of-00002.gguf");
        std::fs::write(&incomplete, [0; 8]).unwrap();
        let preset = crate::config::ModelPreset {
            llama_model_path: incomplete,
            ..preset
        };
        assert!(
            validate_local_preset(&config, &preset)
                .unwrap_err()
                .to_string()
                .contains("incomplete")
        );
    }
    #[test]
    fn gpu_visibility_is_explicit_validated_and_applied_as_child_environment() {
        let mut config = LlamaConfig::default();
        assert!(launch_environment(&config).unwrap().is_empty());
        config.gpu_environment.insert(
            "CUDA_VISIBLE_DEVICES".into(),
            "GPU-1234,MIG-GPU-5678/1/2".into(),
        );
        config
            .gpu_environment
            .insert("GGML_VK_VISIBLE_DEVICES".into(), "1,0".into());
        let output = std::process::Command::new("/usr/bin/env")
            .env_clear()
            .envs(launch_environment(&config).unwrap())
            .output()
            .unwrap();
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.contains("CUDA_VISIBLE_DEVICES=GPU-1234,MIG-GPU-5678/1/2\n"));
        assert!(output.contains("GGML_VK_VISIBLE_DEVICES=1,0\n"));
        config
            .gpu_environment
            .insert("GGML_VK_VISIBLE_DEVICES".into(), "".into());
        assert_eq!(
            launch_environment(&config).unwrap()["GGML_VK_VISIBLE_DEVICES"],
            ""
        );
        config
            .gpu_environment
            .insert("LD_PRELOAD".into(), "/foreign.so".into());
        assert!(
            launch_environment(&config)
                .unwrap_err()
                .to_string()
                .contains("Unsupported")
        );
        config.gpu_environment.remove("LD_PRELOAD");
        config
            .gpu_environment
            .insert("GGML_VK_VISIBLE_DEVICES".into(), "-1".into());
        assert!(launch_environment(&config).is_err());
        config
            .gpu_environment
            .insert("GGML_VK_VISIBLE_DEVICES".into(), "0\n1".into());
        assert!(launch_environment(&config).is_err());
        config.model_search_dirs = vec![PathBuf::from("relative")];
        assert!(validate_local_settings(&config).is_err());
    }
    #[test]
    fn debian_detection_requires_installed_status_and_cuda_wins() {
        assert!(installed_package("llama-cpp\tunknown ok not-installed\n").is_none());
        assert_eq!(
            installed_package(
                "llama-cpp\tinstall ok installed\nllama-cpp-cuda:amd64\tinstall ok installed\n"
            )
            .unwrap()
            .backend,
            "CUDA"
        );
    }
    #[test]
    fn launcher_preserves_router_mode_and_hides_keys() {
        let mut config = LlamaConfig {
            api_key: "fixture-secret".into(),
            autoload: false,
            ..Default::default()
        };
        let args = launch_arguments(&config, false, true).unwrap();
        assert!(!args.iter().any(|a| a.contains("fixture-secret")));
        assert!(args.windows(2).any(|a| a == ["-ngl", "999"]));
        assert!(args.windows(2).any(|a| a == ["--models-max", "0"]));
        assert!(args.contains(&"--no-models-autoload".into()));
        config.extra_arguments = vec!["--model=x.gguf".into()];
        assert!(launch_arguments(&config, true, true).is_err());
    }
    #[test]
    fn device_listing_parser_keeps_friendly_metadata_and_ignores_headers() {
        let devices = parse_devices(
            "Available devices:\n  CUDA0: NVIDIA GeForce RTX 5090 (23983 MiB, 23463 MiB free)\n  Vulkan1: Intel Arc (8192 MiB, 4000 MiB free)\n  malformed: no memory\n",
        );
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].id, "CUDA0");
        assert_eq!(devices[0].name, "NVIDIA GeForce RTX 5090");
        assert_eq!(devices[0].total_memory_mib, Some(23983));
        assert_eq!(devices[0].free_memory_mib, Some(23463));
        assert_eq!(devices[1].id, "Vulkan1");
    }
    #[test]
    fn selected_devices_and_installed_fit_controls_are_emitted() {
        let config = LlamaConfig {
            gpu_devices: vec!["CUDA0".into(), "Vulkan1".into()],
            fit: true,
            fit_target_mib: 512,
            fit_context: 2048,
            max_new_tokens: 1024,
            ..Default::default()
        };
        let args = launch_arguments(&config, true, false).unwrap();
        assert!(args.windows(2).any(|v| v == ["--device", "CUDA0,Vulkan1"]));
        if fit_supported() {
            assert!(args.windows(2).any(|v| v == ["--fit", "on"]));
            assert!(args.windows(2).any(|v| v == ["--fit-target", "512"]));
            assert!(args.windows(2).any(|v| v == ["--fit-ctx", "2048"]));
        }
        if predict_supported() {
            assert!(args.windows(2).any(|v| v == ["--n-predict", "1024"]));
        }
    }
    #[test]
    fn zero_router_context_leaves_ctx_size_unset_for_native_fit() {
        let config = LlamaConfig {
            context_size: 0,
            fit_context: 4096,
            ..Default::default()
        };
        let args = launch_arguments(&config, true, false).unwrap();
        assert!(!args.iter().any(|arg| arg == "--ctx-size"));
        if fit_supported() {
            assert!(args.windows(2).any(|v| v == ["--fit", "on"]));
            assert!(!args.iter().any(|arg| arg == "-ngl"));
        }
        let ini = preset_contents(&config, false).unwrap();
        assert!(!ini.contains("ctx-size = 0"));
        if fit_supported() {
            assert!(ini.contains("fit = on\n"));
            assert!(!ini.contains("ngl = 999\n"));
        }
    }
    #[test]
    fn preset_can_override_fit_context_and_max_new_tokens() {
        let config = LlamaConfig {
            models: vec![crate::config::ModelPreset {
                id: "fit-model".into(),
                context_window: 8192,
                llama_max_new_tokens: Some(1024),
                llama_fit: Some(false),
                llama_fit_target_mib: Some(256),
                llama_fit_context: Some(4096),
                ..Default::default()
            }],
            ..Default::default()
        };
        let ini = preset_contents(&config, false).unwrap();
        assert!(ini.contains("[fit-model]\nctx-size = 8192\n"));
        if fit_supported() {
            assert!(ini.contains("fit = off\nfit-target = 256\nfit-ctx = 4096\n"));
        }
        if predict_supported() {
            assert!(ini.contains("n-predict = 1024\n"));
        }
    }
}
