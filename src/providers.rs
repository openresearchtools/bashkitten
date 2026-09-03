//! Native provider transports.
//!
//! This module deliberately has no provider catalog, update checker, telemetry, or
//! background network activity. Network calls happen only when a caller invokes a
//! request against its configured endpoint, refreshes OpenAI OAuth, or asks for
//! llama.cpp model discovery.

use crate::config::{CompatibleAuth, CompatibleProvider, LlamaConfig, ModelPreset};
use crate::paths::{AppPaths, ensure_private_dir, set_private_file};
use anyhow::{Context, Result, anyhow, bail};
use async_stream::try_stream;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use fs2::FileExt;
use futures_util::{Stream, StreamExt};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::time::Duration;

pub const OPENAI_CODEX_PROVIDER_ID: &str = "openai-codex";
pub const OPENAI_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub const OPENAI_CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_CODEX_ACCOUNT_CLAIM: &str = "https://api.openai.com/auth";
const OAUTH_REFRESH_MARGIN_MS: i64 = 5 * 60 * 1_000;
const OAUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent>> + Send>>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// OpenAI Codex maps Pi's `minimal` setting to `low`.
    fn codex_effort(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Minimal => Some("low"),
            level => Some(level.as_str()),
        }
    }

    /// Pi-compatible shared-output reasoning budgets. `xhigh` and `max` clamp
    /// to the `high` budget unless a provider accepts named effort directly.
    pub fn token_budget(self, output_ceiling: u64) -> Option<u64> {
        let requested = match self {
            Self::Off => return None,
            Self::Minimal => 1_024,
            Self::Low => 2_048,
            Self::Medium => 8_192,
            Self::High | Self::Xhigh | Self::Max => 16_384,
        };
        Some(requested.min(output_ceiling.saturating_sub(1_024)))
    }
}

impl FromStr for ThinkingLevel {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => bail!("unsupported thinking level"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Url {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Base64 {
        media_type: String,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

impl ImageSource {
    pub fn from_bytes(media_type: impl Into<String>, bytes: &[u8]) -> Self {
        Self::Base64 {
            media_type: media_type.into(),
            data: STANDARD.encode(bytes),
            detail: None,
        }
    }

    fn data_url(&self) -> String {
        match self {
            Self::Url { url, .. } => url.clone(),
            Self::Base64 {
                media_type, data, ..
            } => format!("data:{media_type};base64,{data}"),
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Self::Url { detail, .. } | Self::Base64 { detail, .. } => detail.as_deref(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    /// `encrypted_content` must be persisted and replayed unchanged for
    /// stateless Codex `store:false` conversations.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        arguments: Value,
    },
    ToolResult {
        tool_call_id: String,
        output: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderMessage {
    pub role: MessageRole,
    #[serde(default)]
    pub content: Vec<ContentPart>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub strict: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub model: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub messages: Vec<ProviderMessage>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub thinking: ThinkingLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// Provider/model-specific keys. They are applied last, except that
    /// `model`, `stream`, and Codex's mandatory `store:false` are enforced.
    #[serde(default)]
    pub request_parameters: Map<String, Value>,
    /// Per-request headers. Values are never included in `Debug` output.
    #[serde(skip)]
    pub headers: BTreeMap<String, String>,
}

impl ProviderRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ProviderMessage>) -> Self {
        Self {
            model: model.into(),
            system_prompt: String::new(),
            messages,
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            max_tokens: None,
            temperature: None,
            session_id: None,
            tool_choice: None,
            request_parameters: Map::new(),
            headers: BTreeMap::new(),
        }
    }
}

impl fmt::Debug for ProviderRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRequest")
            .field("model", &self.model)
            .field("messages", &self.messages.len())
            .field("tools", &self.tools.len())
            .field("thinking", &self.thinking)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("session_id", &self.session_id)
            .field("request_parameters", &self.request_parameters.keys())
            .field("headers", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct NormalizedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    ToolUse,
    Length,
    ContentFilter,
    Error,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    Start {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
    },
    TextDelta {
        delta: String,
    },
    ThinkingDelta {
        delta: String,
    },
    ThinkingDone {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    ToolCallStart {
        index: u64,
        id: String,
        name: String,
    },
    ToolCallDelta {
        index: u64,
        arguments_delta: String,
    },
    ToolCallDone {
        index: u64,
        id: String,
        name: String,
        arguments: String,
    },
    Usage {
        usage: NormalizedUsage,
    },
    Done {
        reason: StopReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_reason: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThinkingFormat {
    OpenAi,
    Qwen,
    ChatTemplate,
    DeepSeek,
    OpenRouter,
    String,
    LlamaCpp,
}

#[derive(Clone, Debug)]
pub struct OpenAiCompatibility {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_strict_tools: bool,
    pub max_tokens_field: MaxTokensField,
    pub thinking_format: ThinkingFormat,
}

impl Default for OpenAiCompatibility {
    fn default() -> Self {
        Self {
            supports_store: true,
            supports_developer_role: true,
            supports_reasoning_effort: true,
            supports_usage_in_streaming: true,
            supports_strict_tools: true,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            thinking_format: ThinkingFormat::OpenAi,
        }
    }
}

impl OpenAiCompatibility {
    pub fn from_preset(preset: &ModelPreset) -> Self {
        Self {
            supports_developer_role: preset.supports_developer_role,
            supports_reasoning_effort: preset.supports_reasoning_effort,
            ..Self::default()
        }
    }

    pub fn llama_cpp(preset: Option<&ModelPreset>) -> Self {
        let mut value = preset.map(Self::from_preset).unwrap_or_default();
        value.supports_store = false;
        value.supports_developer_role = false;
        value.supports_strict_tools = false;
        value.max_tokens_field = MaxTokensField::MaxTokens;
        value.thinking_format = ThinkingFormat::LlamaCpp;
        value
    }
}

#[derive(Clone, Default)]
pub enum EndpointAuth {
    #[default]
    None,
    Bearer(String),
    Header {
        name: String,
        value: String,
    },
}

impl fmt::Debug for EndpointAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::Bearer(_) => formatter.write_str("Bearer(<redacted>)"),
            Self::Header { name, .. } => formatter
                .debug_struct("Header")
                .field("name", name)
                .field("value", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Clone)]
pub struct OpenAiCompatibleEndpoint {
    pub base_url: String,
    pub auth: EndpointAuth,
    pub headers: BTreeMap<String, String>,
    pub compatibility: OpenAiCompatibility,
}

impl fmt::Debug for OpenAiCompatibleEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleEndpoint")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("headers", &"<redacted>")
            .field("compatibility", &self.compatibility)
            .finish()
    }
}

impl OpenAiCompatibleEndpoint {
    pub fn from_config(provider: &CompatibleProvider, model: &ModelPreset) -> Self {
        let auth = match &provider.auth {
            CompatibleAuth::None => EndpointAuth::None,
            CompatibleAuth::Bearer { secret } => EndpointAuth::Bearer(secret.clone()),
            CompatibleAuth::Header { name, secret } => EndpointAuth::Header {
                name: name.clone(),
                value: secret.clone(),
            },
        };
        Self {
            base_url: provider.base_url.clone(),
            auth,
            headers: BTreeMap::new(),
            compatibility: OpenAiCompatibility::from_preset(model),
        }
    }

    pub fn from_llama_config(config: &LlamaConfig, model: Option<&ModelPreset>) -> Self {
        Self {
            base_url: format!("http://127.0.0.1:{}/v1", config.port),
            auth: if config.api_key.is_empty() {
                EndpointAuth::None
            } else {
                EndpointAuth::Bearer(config.api_key.clone())
            },
            headers: BTreeMap::new(),
            compatibility: OpenAiCompatibility::llama_cpp(model),
        }
    }
}

#[derive(Clone)]
pub struct CodexEndpoint {
    pub base_url: String,
    pub auth_store: ProviderAuthStore,
    pub headers: BTreeMap<String, String>,
}

impl CodexEndpoint {
    pub fn for_paths(paths: &AppPaths) -> Self {
        Self {
            base_url: OPENAI_CODEX_BASE_URL.to_owned(),
            auth_store: ProviderAuthStore::for_paths(paths),
            headers: BTreeMap::new(),
        }
    }
}

impl fmt::Debug for CodexEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexEndpoint")
            .field("base_url", &self.base_url)
            .field("auth_store", &self.auth_store)
            .field("headers", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum ProviderEndpoint {
    OpenAiCompatible(OpenAiCompatibleEndpoint),
    OpenAiCodex(CodexEndpoint),
    LlamaCpp(OpenAiCompatibleEndpoint),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderCredential {
    #[serde(rename = "api_key")]
    ApiKey {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<Value>,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
    #[serde(rename = "oauth")]
    OAuth {
        access: String,
        refresh: String,
        expires: i64,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
}

impl fmt::Debug for ProviderCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey { env, extra, .. } => formatter
                .debug_struct("ApiKey")
                .field("key", &"<redacted>")
                .field("env", &env.as_ref().map(|_| "<redacted>"))
                .field("extra_keys", &extra.keys())
                .finish(),
            Self::OAuth { expires, extra, .. } => formatter
                .debug_struct("OAuth")
                .field("access", &"<redacted>")
                .field("refresh", &"<redacted>")
                .field("expires", expires)
                .field("extra_keys", &extra.keys())
                .finish(),
        }
    }
}

pub type ProviderAuthFile = BTreeMap<String, ProviderCredential>;

#[derive(Clone)]
pub struct ProviderAuthStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl ProviderAuthStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_name = format!(
            "{}.lock",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("provider-auth.json")
        );
        let lock_path = path.with_file_name(lock_name);
        Self { path, lock_path }
    }

    pub fn for_paths(paths: &AppPaths) -> Self {
        Self::new(paths.provider_auth_file())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<ProviderAuthFile> {
        load_auth_file(&self.path)
    }

    pub fn credential(&self, provider: &str) -> Result<Option<ProviderCredential>> {
        Ok(self.load()?.remove(provider))
    }

    async fn codex_access(&self, client: &Client) -> Result<CodexAccess> {
        let current = self
            .credential(OPENAI_CODEX_PROVIDER_ID)?
            .context("OpenAI Codex is not authenticated")?;
        let ProviderCredential::OAuth {
            access, expires, ..
        } = current
        else {
            bail!("OpenAI Codex requires an OAuth credential");
        };

        if !oauth_needs_refresh(expires) {
            return CodexAccess::from_token(access);
        }

        let lock_path = self.lock_path.clone();
        let lock = tokio::task::spawn_blocking(move || acquire_auth_lock(&lock_path))
            .await
            .context("join provider credential lock task")??;

        // Double-check after taking the cross-process lock. Another worker may
        // already have refreshed and atomically replaced the file.
        let mut auth = self.load()?;
        let credential = auth
            .get(OPENAI_CODEX_PROVIDER_ID)
            .context("OpenAI Codex credential disappeared during refresh")?
            .clone();
        let ProviderCredential::OAuth {
            access,
            refresh,
            expires,
            extra,
        } = credential
        else {
            bail!("OpenAI Codex requires an OAuth credential");
        };

        if !oauth_needs_refresh(expires) {
            FileExt::unlock(&lock)?;
            return CodexAccess::from_token(access);
        }

        let refreshed = refresh_codex_token(client, &refresh).await?;
        let access = refreshed.access_token.clone();
        auth.insert(
            OPENAI_CODEX_PROVIDER_ID.to_owned(),
            ProviderCredential::OAuth {
                access: refreshed.access_token,
                refresh: refreshed.refresh_token,
                expires: refreshed.expires,
                extra,
            },
        );
        crate::config::atomic_private_json(&self.path, &auth)?;
        FileExt::unlock(&lock)?;
        CodexAccess::from_token(access)
    }
}

impl fmt::Debug for ProviderAuthStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAuthStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

struct CodexAccess {
    access_token: String,
    account_id: String,
}

impl CodexAccess {
    fn from_token(access_token: String) -> Result<Self> {
        let account_id = account_id_from_jwt(&access_token)?;
        Ok(Self {
            access_token,
            account_id,
        })
    }
}

impl fmt::Debug for CodexAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexAccess")
            .field("access_token", &"<redacted>")
            .field("account_id", &self.account_id)
            .finish()
    }
}

#[derive(Deserialize)]
struct RefreshTokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

struct RefreshedToken {
    access_token: String,
    refresh_token: String,
    expires: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Clone)]
pub struct ProviderClient {
    http: Client,
}

impl ProviderClient {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .context("build provider HTTP client")?;
        Ok(Self { http })
    }

    pub fn with_client(http: Client) -> Self {
        Self { http }
    }

    pub async fn stream(
        &self,
        endpoint: &ProviderEndpoint,
        request: ProviderRequest,
    ) -> Result<ProviderStream> {
        match endpoint {
            ProviderEndpoint::OpenAiCompatible(endpoint) | ProviderEndpoint::LlamaCpp(endpoint) => {
                self.stream_openai_compatible(endpoint, request).await
            }
            ProviderEndpoint::OpenAiCodex(endpoint) => {
                self.stream_openai_codex(endpoint, request).await
            }
        }
    }

    pub async fn stream_openai_compatible(
        &self,
        endpoint: &OpenAiCompatibleEndpoint,
        request: ProviderRequest,
    ) -> Result<ProviderStream> {
        let url = endpoint_url(&endpoint.base_url, "chat/completions")?;
        let body = build_chat_completions_body(&request, &endpoint.compatibility)?;
        let headers = compatible_headers(endpoint, &request)?;
        let response = self
            .http
            .post(url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .context("send OpenAI-compatible request")?;
        ensure_success(response, parse_chat_completions_stream).await
    }

    pub async fn stream_openai_codex(
        &self,
        endpoint: &CodexEndpoint,
        request: ProviderRequest,
    ) -> Result<ProviderStream> {
        let auth = endpoint.auth_store.codex_access(&self.http).await?;
        let url = endpoint_url(&endpoint.base_url, "codex/responses")?;
        let body = build_codex_responses_body(&request)?;
        let headers = codex_headers(endpoint, &request, &auth)?;
        let response = self
            .http
            .post(url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .context("send OpenAI Codex request")?;
        ensure_success(response, parse_codex_responses_stream).await
    }

    /// Discover models only from this configured endpoint. No public model
    /// catalog or package service is contacted.
    pub async fn discover_llama_models(
        &self,
        endpoint: &OpenAiCompatibleEndpoint,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = llama_models_url(&endpoint.base_url)?;
        let headers = compatible_headers_without_request(endpoint)?;
        let response = self
            .http
            .get(url)
            .headers(headers)
            .send()
            .await
            .context("query llama.cpp models")?;
        if !response.status().is_success() {
            bail!(
                "llama.cpp model discovery failed with HTTP {}",
                response.status()
            );
        }
        let value: Value = response
            .json()
            .await
            .context("decode llama.cpp /v1/models response")?;
        parse_discovered_models(value)
    }
}

impl Default for ProviderClient {
    fn default() -> Self {
        Self::new().expect("build default provider HTTP client")
    }
}

fn load_auth_file(path: &Path) -> Result<ProviderAuthFile> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let bytes = fs::read(path)
        .with_context(|| format!("read provider credentials at {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse provider credentials at {}", path.display()))
}

fn acquire_auth_lock(path: &Path) -> Result<std::fs::File> {
    let parent = path.parent().context("credential lock has no parent")?;
    ensure_private_dir(parent)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open credential lock {}", path.display()))?;
    set_private_file(path)?;
    file.lock_exclusive()
        .with_context(|| format!("lock provider credentials through {}", path.display()))?;
    Ok(file)
}

fn oauth_needs_refresh(expires: i64) -> bool {
    let now = chrono::Utc::now().timestamp_millis();
    expires <= now.saturating_add(OAUTH_REFRESH_MARGIN_MS)
}

async fn refresh_codex_token(client: &Client, refresh_token: &str) -> Result<RefreshedToken> {
    let response = client
        .post(OPENAI_CODEX_TOKEN_URL)
        .timeout(OAUTH_REFRESH_TIMEOUT)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", OPENAI_CODEX_CLIENT_ID),
        ])
        .send()
        .await
        .context("refresh OpenAI Codex OAuth credential")?;
    if !response.status().is_success() {
        bail!(
            "OpenAI Codex OAuth refresh failed with HTTP {}",
            response.status()
        );
    }
    let token: RefreshTokenResponse = response
        .json()
        .await
        .context("decode OpenAI Codex OAuth refresh response")?;
    if token.access_token.is_empty() || token.refresh_token.is_empty() || token.expires_in <= 0 {
        bail!("OpenAI Codex OAuth refresh response is incomplete");
    }
    let expires = chrono::Utc::now()
        .timestamp_millis()
        .checked_add(token.expires_in.saturating_mul(1_000))
        .context("OpenAI Codex OAuth expiry overflow")?;
    Ok(RefreshedToken {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires,
    })
}

fn account_id_from_jwt(token: &str) -> Result<String> {
    let payload = token
        .split('.')
        .nth(1)
        .filter(|part| !part.is_empty())
        .context("OpenAI Codex access token is not a JWT")?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .context("decode OpenAI Codex JWT payload")?;
    let claims: Value =
        serde_json::from_slice(&decoded).context("parse OpenAI Codex JWT payload")?;
    claims
        .get(OPENAI_CODEX_ACCOUNT_CLAIM)
        .and_then(|claim| claim.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .context("OpenAI Codex access token has no account ID")
}

fn endpoint_url(base_url: &str, endpoint: &str) -> Result<String> {
    let mut base = base_url.trim_end_matches('/').to_owned();
    let endpoint = endpoint.trim_matches('/');
    if base.ends_with(&format!("/{endpoint}")) {
        return Ok(base);
    }
    if endpoint == "models" && base.ends_with("/v1") {
        base.push_str("/models");
    } else {
        base.push('/');
        base.push_str(endpoint);
    }
    let url = url::Url::parse(&base).context("configured provider base URL is invalid")?;
    if url.scheme() != "http" && url.scheme() != "https" {
        bail!("provider URL must use http or https");
    }
    Ok(url.into())
}

fn llama_models_url(base_url: &str) -> Result<String> {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") || base.ends_with("/v1/models") {
        endpoint_url(base, "models")
    } else {
        endpoint_url(&format!("{base}/v1"), "models")
    }
}

fn insert_headers(target: &mut HeaderMap, source: &BTreeMap<String, String>) -> Result<()> {
    for (name, value) in source {
        let name =
            HeaderName::from_bytes(name.as_bytes()).context("invalid provider header name")?;
        let mut value = HeaderValue::from_str(value).context("invalid provider header value")?;
        value.set_sensitive(true);
        target.insert(name, value);
    }
    Ok(())
}

fn compatible_headers_without_request(endpoint: &OpenAiCompatibleEndpoint) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("bashkitten/", env!("CARGO_PKG_VERSION"))),
    );
    insert_headers(&mut headers, &endpoint.headers)?;
    match &endpoint.auth {
        EndpointAuth::None => {}
        EndpointAuth::Bearer(secret) => {
            let mut value = HeaderValue::from_str(&format!("Bearer {secret}"))
                .context("invalid bearer credential")?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        EndpointAuth::Header { name, value } => {
            let name = HeaderName::from_bytes(name.as_bytes())
                .context("invalid authentication header name")?;
            let mut value =
                HeaderValue::from_str(value).context("invalid authentication header value")?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
    }
    Ok(headers)
}

fn compatible_headers(
    endpoint: &OpenAiCompatibleEndpoint,
    request: &ProviderRequest,
) -> Result<HeaderMap> {
    let mut headers = compatible_headers_without_request(endpoint)?;
    insert_headers(&mut headers, &request.headers)?;
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    // Endpoint authentication wins over per-request headers.
    match &endpoint.auth {
        EndpointAuth::None => {}
        EndpointAuth::Bearer(secret) => {
            let mut value = HeaderValue::from_str(&format!("Bearer {secret}"))
                .context("invalid bearer credential")?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        EndpointAuth::Header { name, value } => {
            let name = HeaderName::from_bytes(name.as_bytes())
                .context("invalid authentication header name")?;
            let mut value =
                HeaderValue::from_str(value).context("invalid authentication header value")?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
    }
    Ok(headers)
}

fn codex_headers(
    endpoint: &CodexEndpoint,
    request: &ProviderRequest,
    auth: &CodexAccess,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    insert_headers(&mut headers, &endpoint.headers)?;
    insert_headers(&mut headers, &request.headers)?;
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("bashkitten/", env!("CARGO_PKG_VERSION"))),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        "openai-beta",
        HeaderValue::from_static("responses=experimental"),
    );
    headers.insert("originator", HeaderValue::from_static("pi"));
    let account_id =
        HeaderValue::from_str(&auth.account_id).context("invalid OpenAI Codex account ID")?;
    headers.insert("chatgpt-account-id", account_id);
    let mut authorization = HeaderValue::from_str(&format!("Bearer {}", auth.access_token))
        .context("invalid OpenAI Codex access token")?;
    authorization.set_sensitive(true);
    headers.insert(AUTHORIZATION, authorization);
    if let Some(session_id) = request.session_id.as_deref() {
        let value = HeaderValue::from_str(session_id).context("invalid provider session ID")?;
        headers.insert("session-id", value.clone());
        headers.insert("x-client-request-id", value);
    }
    Ok(headers)
}

async fn ensure_success(
    response: Response,
    parser: fn(Response) -> ProviderStream,
) -> Result<ProviderStream> {
    if response.status().is_success() {
        return Ok(parser(response));
    }
    let status = response.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        bail!("provider usage or rate limit reached (HTTP 429)");
    }
    // Do not include arbitrary response bodies: a broken proxy can reflect
    // authorization material into them.
    bail!("provider request failed with HTTP {status}")
}

fn build_chat_completions_body(
    request: &ProviderRequest,
    compatibility: &OpenAiCompatibility,
) -> Result<Value> {
    let mut body = Map::new();
    body.insert("model".into(), Value::String(request.model.clone()));
    body.insert(
        "messages".into(),
        Value::Array(convert_chat_messages(request, compatibility)?),
    );
    body.insert("stream".into(), Value::Bool(true));

    if compatibility.supports_usage_in_streaming {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if compatibility.supports_store {
        body.insert("store".into(), Value::Bool(false));
    }
    if let Some(max_tokens) = request.max_tokens {
        let field = match compatibility.max_tokens_field {
            MaxTokensField::MaxTokens => "max_tokens",
            MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
        };
        body.insert(field.into(), Value::from(max_tokens));
    }
    if let Some(temperature) = request.temperature {
        let temperature = serde_json::Number::from_f64(temperature)
            .context("temperature must be a finite number")?;
        body.insert("temperature".into(), Value::Number(temperature));
    }
    if !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| chat_tool(tool, compatibility.supports_strict_tools))
                    .collect(),
            ),
        );
    } else if request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|part| {
            matches!(
                part,
                ContentPart::ToolCall { .. } | ContentPart::ToolResult { .. }
            )
        })
    {
        // Several OpenAI-compatible proxies reject tool history unless `tools`
        // exists, even when no tools remain active.
        body.insert("tools".into(), Value::Array(Vec::new()));
    }
    if let Some(tool_choice) = &request.tool_choice {
        body.insert("tool_choice".into(), tool_choice.clone());
    }
    apply_compatible_thinking(&mut body, request, compatibility);

    for (key, value) in &request.request_parameters {
        body.insert(key.clone(), value.clone());
    }
    // Caller parameters may customize provider behavior, but cannot turn a
    // streaming request into a stored or non-streaming operation.
    body.insert("model".into(), Value::String(request.model.clone()));
    body.insert("stream".into(), Value::Bool(true));
    if compatibility.supports_store {
        body.insert("store".into(), Value::Bool(false));
    }
    Ok(Value::Object(body))
}

fn apply_compatible_thinking(
    body: &mut Map<String, Value>,
    request: &ProviderRequest,
    compatibility: &OpenAiCompatibility,
) {
    let enabled = request.thinking != ThinkingLevel::Off;
    match compatibility.thinking_format {
        ThinkingFormat::OpenAi => {
            if enabled && compatibility.supports_reasoning_effort {
                body.insert(
                    "reasoning_effort".into(),
                    Value::String(request.thinking.as_str().into()),
                );
            }
        }
        ThinkingFormat::Qwen => {
            body.insert("enable_thinking".into(), Value::Bool(enabled));
            if enabled && compatibility.supports_reasoning_effort {
                body.insert(
                    "reasoning_effort".into(),
                    Value::String(request.thinking.as_str().into()),
                );
            }
        }
        ThinkingFormat::ChatTemplate => {
            body.insert(
                "chat_template_kwargs".into(),
                json!({ "enable_thinking": enabled, "preserve_thinking": true }),
            );
        }
        ThinkingFormat::DeepSeek => {
            body.insert(
                "thinking".into(),
                json!({ "type": if enabled { "enabled" } else { "disabled" } }),
            );
            if enabled && compatibility.supports_reasoning_effort {
                body.insert(
                    "reasoning_effort".into(),
                    Value::String(request.thinking.as_str().into()),
                );
            }
        }
        ThinkingFormat::OpenRouter => {
            body.insert(
                "reasoning".into(),
                json!({ "effort": if enabled { request.thinking.as_str() } else { "none" } }),
            );
        }
        ThinkingFormat::String => {
            body.insert(
                "thinking".into(),
                Value::String(if enabled {
                    request.thinking.as_str().into()
                } else {
                    "none".into()
                }),
            );
        }
        ThinkingFormat::LlamaCpp => {
            body.insert(
                "chat_template_kwargs".into(),
                json!({ "enable_thinking": enabled, "preserve_thinking": true }),
            );
            if let Some(budget) = request
                .thinking
                .token_budget(request.max_tokens.unwrap_or(16_384))
                && budget > 0
            {
                body.insert("thinking_budget_tokens".into(), Value::from(budget));
            }
        }
    }
}

fn convert_chat_messages(
    request: &ProviderRequest,
    compatibility: &OpenAiCompatibility,
) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    if !request.system_prompt.is_empty() {
        let role =
            if compatibility.supports_developer_role && request.thinking != ThinkingLevel::Off {
                "developer"
            } else {
                "system"
            };
        messages.push(json!({ "role": role, "content": request.system_prompt }));
    }

    for message in &request.messages {
        let tool_results: Vec<_> = message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::ToolResult {
                    tool_call_id,
                    output,
                    ..
                } => Some((tool_call_id, output)),
                _ => None,
            })
            .collect();
        if !tool_results.is_empty() {
            for (tool_call_id, output) in tool_results {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": output,
                }));
            }
            continue;
        }

        let mut text = String::new();
        let mut reasoning = String::new();
        let mut rich_content = Vec::new();
        let mut tool_calls = Vec::new();
        let mut has_image = false;
        for part in &message.content {
            match part {
                ContentPart::Text { text: value } => {
                    text.push_str(value);
                    rich_content.push(json!({ "type": "text", "text": value }));
                }
                ContentPart::Image { source } => {
                    has_image = true;
                    let mut image = Map::new();
                    image.insert("url".into(), Value::String(source.data_url()));
                    if let Some(detail) = source.detail() {
                        image.insert("detail".into(), Value::String(detail.into()));
                    }
                    rich_content.push(json!({ "type": "image_url", "image_url": image }));
                }
                ContentPart::Thinking { text: value, .. } => reasoning.push_str(value),
                ContentPart::ToolCall {
                    id,
                    name,
                    arguments,
                } => tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments_to_string(arguments),
                    }
                })),
                ContentPart::ToolResult { .. } => {}
            }
        }

        let mut object = Map::new();
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::Developer if compatibility.supports_developer_role => "developer",
            MessageRole::Developer => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        object.insert("role".into(), Value::String(role.into()));
        if has_image {
            object.insert("content".into(), Value::Array(rich_content));
        } else if text.is_empty() && !tool_calls.is_empty() {
            object.insert("content".into(), Value::Null);
        } else {
            object.insert("content".into(), Value::String(text));
        }
        if !reasoning.is_empty() {
            object.insert("reasoning_content".into(), Value::String(reasoning));
        }
        if !tool_calls.is_empty() {
            object.insert("tool_calls".into(), Value::Array(tool_calls));
        }
        messages.push(Value::Object(object));
    }
    Ok(messages)
}

fn chat_tool(tool: &ToolDefinition, supports_strict: bool) -> Value {
    let mut function = Map::new();
    function.insert("name".into(), Value::String(tool.name.clone()));
    function.insert(
        "description".into(),
        Value::String(tool.description.clone()),
    );
    function.insert("parameters".into(), tool.parameters.clone());
    if tool.strict && supports_strict {
        function.insert("strict".into(), Value::Bool(true));
    }
    json!({ "type": "function", "function": function })
}

fn build_codex_responses_body(request: &ProviderRequest) -> Result<Value> {
    let mut body = Map::new();
    body.insert("model".into(), Value::String(request.model.clone()));
    body.insert("store".into(), Value::Bool(false));
    body.insert("stream".into(), Value::Bool(true));
    body.insert(
        "instructions".into(),
        Value::String(codex_instructions(request)),
    );
    body.insert("input".into(), Value::Array(convert_codex_input(request)?));
    body.insert("text".into(), json!({ "verbosity": "low" }));
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    body.insert(
        "tool_choice".into(),
        request.tool_choice.clone().unwrap_or_else(|| json!("auto")),
    );
    body.insert("parallel_tool_calls".into(), Value::Bool(true));
    if let Some(session_id) = request.session_id.as_deref() {
        body.insert("prompt_cache_key".into(), Value::String(session_id.into()));
    }
    if let Some(temperature) = request.temperature {
        let temperature = serde_json::Number::from_f64(temperature)
            .context("temperature must be a finite number")?;
        body.insert("temperature".into(), Value::Number(temperature));
    }
    if !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                            "strict": if tool.strict { Value::Bool(true) } else { Value::Null },
                        })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(effort) = request.thinking.codex_effort() {
        body.insert(
            "reasoning".into(),
            json!({ "effort": effort, "summary": "auto" }),
        );
    }

    for (key, value) in &request.request_parameters {
        body.insert(key.clone(), value.clone());
    }
    body.insert("model".into(), Value::String(request.model.clone()));
    body.insert("stream".into(), Value::Bool(true));
    body.insert("store".into(), Value::Bool(false));
    Ok(Value::Object(body))
}

fn codex_instructions(request: &ProviderRequest) -> String {
    let mut instructions = Vec::new();
    if !request.system_prompt.trim().is_empty() {
        instructions.push(request.system_prompt.trim().to_owned());
    }
    for message in &request.messages {
        if matches!(message.role, MessageRole::System | MessageRole::Developer) {
            let text = message
                .content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if !text.trim().is_empty() {
                instructions.push(text);
            }
        }
    }
    if instructions.is_empty() {
        "You are a helpful assistant.".into()
    } else {
        instructions.join("\n\n")
    }
}

fn convert_codex_input(request: &ProviderRequest) -> Result<Vec<Value>> {
    let mut input = Vec::new();
    for message in &request.messages {
        if matches!(message.role, MessageRole::System | MessageRole::Developer) {
            continue;
        }
        let role = if message.role == MessageRole::Assistant {
            "assistant"
        } else {
            "user"
        };
        let mut content = Vec::new();
        for part in &message.content {
            match part {
                ContentPart::Text { text } => {
                    let kind = if message.role == MessageRole::Assistant {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    content.push(json!({ "type": kind, "text": text }));
                }
                ContentPart::Image { source } => {
                    let mut image = Map::new();
                    image.insert("type".into(), Value::String("input_image".into()));
                    image.insert("image_url".into(), Value::String(source.data_url()));
                    if let Some(detail) = source.detail() {
                        image.insert("detail".into(), Value::String(detail.into()));
                    }
                    content.push(Value::Object(image));
                }
                ContentPart::Thinking {
                    text,
                    id,
                    encrypted_content,
                } => {
                    flush_codex_message(&mut input, role, &mut content);
                    if encrypted_content.is_some() || !text.is_empty() {
                        let mut reasoning = Map::new();
                        reasoning.insert("type".into(), Value::String("reasoning".into()));
                        if let Some(id) = id {
                            reasoning.insert("id".into(), Value::String(id.clone()));
                        }
                        if !text.is_empty() {
                            reasoning.insert(
                                "summary".into(),
                                json!([{ "type": "summary_text", "text": text }]),
                            );
                        }
                        if let Some(encrypted_content) = encrypted_content {
                            reasoning.insert(
                                "encrypted_content".into(),
                                Value::String(encrypted_content.clone()),
                            );
                        }
                        input.push(Value::Object(reasoning));
                    }
                }
                ContentPart::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    flush_codex_message(&mut input, role, &mut content);
                    input.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": arguments_to_string(arguments),
                    }));
                }
                ContentPart::ToolResult {
                    tool_call_id,
                    output,
                    ..
                } => {
                    flush_codex_message(&mut input, role, &mut content);
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_call_id,
                        "output": output,
                    }));
                }
            }
        }
        flush_codex_message(&mut input, role, &mut content);
    }
    Ok(input)
}

fn flush_codex_message(input: &mut Vec<Value>, role: &str, content: &mut Vec<Value>) {
    if !content.is_empty() {
        input.push(json!({
            "type": "message",
            "role": role,
            "content": std::mem::take(content),
        }));
    }
}

fn arguments_to_string(arguments: &Value) -> String {
    arguments
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into()))
}

#[derive(Default)]
struct ToolAccumulator {
    id: String,
    name: String,
    arguments: String,
    started: bool,
    done: bool,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct SseFrame {
    event: Option<String>,
    data: String,
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some((boundary, separator_len)) = find_sse_boundary(&self.buffer) {
            let block = self.buffer[..boundary].to_vec();
            self.buffer.drain(..boundary + separator_len);
            if let Some(frame) = parse_sse_frame(&block)? {
                frames.push(frame);
            }
        }
        Ok(frames)
    }

    fn finish(&mut self) -> Result<Option<SseFrame>> {
        if self.buffer.is_empty() {
            return Ok(None);
        }
        let block = std::mem::take(&mut self.buffer);
        parse_sse_frame(&block)
    }
}

fn find_sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(left), Some(right)) if left <= right => Some((left, 2)),
        (Some(_), Some(right)) => Some((right, 4)),
        (Some(left), None) => Some((left, 2)),
        (None, Some(right)) => Some((right, 4)),
        (None, None) => None,
    }
}

fn parse_sse_frame(block: &[u8]) -> Result<Option<SseFrame>> {
    let block = std::str::from_utf8(block).context("provider SSE stream is not UTF-8")?;
    let mut event = None;
    let mut data = Vec::new();
    for line in block.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(':') || line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    if data.is_empty() {
        Ok(None)
    } else {
        Ok(Some(SseFrame {
            event,
            data: data.join("\n"),
        }))
    }
}

#[derive(Default)]
struct ChatStreamState {
    started: bool,
    done: bool,
    raw_stop_reason: Option<String>,
    tools: HashMap<u64, ToolAccumulator>,
}

#[allow(clippy::collapsible_if)]
fn parse_chat_completions_stream(response: Response) -> ProviderStream {
    Box::pin(try_stream! {
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut state = ChatStreamState::default();
        'stream: while let Some(chunk) = bytes.next().await {
            let chunk = chunk.context("read OpenAI-compatible stream")?;
            for frame in decoder.push(&chunk)? {
                let (events, terminal) = chat_frame_events(frame, &mut state)?;
                for event in events {
                    yield event;
                }
                if terminal {
                    break 'stream;
                }
            }
        }
        if !state.done {
            if let Some(frame) = decoder.finish()? {
                let (events, _) = chat_frame_events(frame, &mut state)?;
                for event in events {
                    yield event;
                }
            }
        }
        if !state.done {
            for event in finish_chat_stream(&mut state) {
                yield event;
            }
        }
    })
}

fn chat_frame_events(
    frame: SseFrame,
    state: &mut ChatStreamState,
) -> Result<(Vec<ProviderEvent>, bool)> {
    if frame.data.trim() == "[DONE]" {
        return Ok((finish_chat_stream(state), true));
    }
    let value: Value =
        serde_json::from_str(&frame.data).context("decode OpenAI-compatible SSE event")?;
    if value.get("error").is_some() {
        bail!(
            "OpenAI-compatible stream error: {}",
            stream_error_label(&value)
        );
    }

    let mut events = Vec::new();
    if !state.started {
        state.started = true;
        events.push(ProviderEvent::Start {
            response_id: value.get("id").and_then(Value::as_str).map(str::to_owned),
        });
    }
    if let Some(usage) = value.get("usage") {
        events.push(ProviderEvent::Usage {
            usage: normalize_usage(usage),
        });
    }

    if let Some(choice) = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    {
        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(value_text)
                && !content.is_empty()
            {
                events.push(ProviderEvent::TextDelta { delta: content });
            }
            for field in ["reasoning_content", "reasoning", "reasoning_text"] {
                if let Some(reasoning) = delta.get(field).and_then(value_text) {
                    if !reasoning.is_empty() {
                        events.push(ProviderEvent::ThinkingDelta { delta: reasoning });
                    }
                    break;
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    append_chat_tool_delta(call, state, &mut events);
                }
            }
            if let Some(function) = delta.get("function_call") {
                let legacy = json!({ "index": 0, "function": function });
                append_chat_tool_delta(&legacy, state, &mut events);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            state.raw_stop_reason = Some(reason.to_owned());
        }
    }
    Ok((events, false))
}

fn append_chat_tool_delta(
    call: &Value,
    state: &mut ChatStreamState,
    events: &mut Vec<ProviderEvent>,
) {
    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
    let tool = state.tools.entry(index).or_default();
    if let Some(id) = call.get("id").and_then(Value::as_str) {
        append_stream_fragment(&mut tool.id, id);
    }
    let function = call.get("function").unwrap_or(call);
    if let Some(name) = function.get("name").and_then(Value::as_str) {
        append_stream_fragment(&mut tool.name, name);
    }
    if !tool.started && (!tool.id.is_empty() || !tool.name.is_empty()) {
        tool.started = true;
        events.push(ProviderEvent::ToolCallStart {
            index,
            id: tool.id.clone(),
            name: tool.name.clone(),
        });
    }
    if let Some(arguments) = function.get("arguments").and_then(Value::as_str)
        && !arguments.is_empty()
    {
        tool.arguments.push_str(arguments);
        events.push(ProviderEvent::ToolCallDelta {
            index,
            arguments_delta: arguments.to_owned(),
        });
    }
}

fn append_stream_fragment(target: &mut String, fragment: &str) {
    if fragment.is_empty() || target.ends_with(fragment) {
        return;
    }
    if fragment.starts_with(target.as_str()) {
        target.clear();
    }
    target.push_str(fragment);
}

fn finish_chat_stream(state: &mut ChatStreamState) -> Vec<ProviderEvent> {
    if state.done {
        return Vec::new();
    }
    state.done = true;
    let mut events = Vec::new();
    let mut indexes: Vec<_> = state.tools.keys().copied().collect();
    indexes.sort_unstable();
    for index in indexes {
        let Some(tool) = state.tools.get_mut(&index) else {
            continue;
        };
        if tool.done {
            continue;
        }
        if !tool.started {
            events.push(ProviderEvent::ToolCallStart {
                index,
                id: tool.id.clone(),
                name: tool.name.clone(),
            });
        }
        tool.done = true;
        events.push(ProviderEvent::ToolCallDone {
            index,
            id: tool.id.clone(),
            name: tool.name.clone(),
            arguments: tool.arguments.clone(),
        });
    }
    let mut reason = map_stop_reason(state.raw_stop_reason.as_deref());
    if !state.tools.is_empty() && reason == StopReason::Stop {
        reason = StopReason::ToolUse;
    }
    events.push(ProviderEvent::Done {
        reason,
        raw_reason: state.raw_stop_reason.clone(),
    });
    events
}

#[derive(Default)]
struct CodexStreamState {
    started: bool,
    done: bool,
    tools: HashMap<u64, ToolAccumulator>,
}

#[allow(clippy::collapsible_if)]
fn parse_codex_responses_stream(response: Response) -> ProviderStream {
    Box::pin(try_stream! {
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut state = CodexStreamState::default();
        'stream: while let Some(chunk) = bytes.next().await {
            let chunk = chunk.context("read OpenAI Codex stream")?;
            for frame in decoder.push(&chunk)? {
                let (events, terminal) = codex_frame_events(frame, &mut state)?;
                for event in events {
                    yield event;
                }
                if terminal {
                    break 'stream;
                }
            }
        }
        if !state.done {
            if let Some(frame) = decoder.finish()? {
                let (events, _) = codex_frame_events(frame, &mut state)?;
                for event in events {
                    yield event;
                }
            }
        }
        if !state.done {
            Err(anyhow!("OpenAI Codex stream ended before a terminal response event"))?;
        }
    })
}

fn codex_frame_events(
    frame: SseFrame,
    state: &mut CodexStreamState,
) -> Result<(Vec<ProviderEvent>, bool)> {
    if frame.data.trim() == "[DONE]" {
        if state.done {
            return Ok((Vec::new(), true));
        }
        bail!("OpenAI Codex stream ended before a terminal response event");
    }
    let mut value: Value =
        serde_json::from_str(&frame.data).context("decode OpenAI Codex SSE event")?;
    if value.get("type").is_none()
        && let Some(event) = frame.event
    {
        value["type"] = Value::String(event);
    }
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "error" || event_type == "response.failed" {
        bail!("OpenAI Codex stream error: {}", stream_error_label(&value));
    }

    let mut events = Vec::new();
    if !state.started {
        state.started = true;
        let response_id = if event_type == "response.created" {
            value.pointer("/response/id")
        } else {
            value.get("response_id")
        };
        events.push(ProviderEvent::Start {
            response_id: response_id.and_then(Value::as_str).map(str::to_owned),
        });
    }

    match event_type {
        "response.output_text.delta" | "response.refusal.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                events.push(ProviderEvent::TextDelta {
                    delta: delta.into(),
                });
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                events.push(ProviderEvent::ThinkingDelta {
                    delta: delta.into(),
                });
            }
        }
        "response.reasoning_summary_part.done" => {
            events.push(ProviderEvent::ThinkingDelta {
                delta: "\n\n".into(),
            });
        }
        "response.output_item.added" => {
            let index = value
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if let Some(item) = value.get("item") {
                codex_tool_start(index, item, state, &mut events);
            }
        }
        "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
            let index = value
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let tool = state.tools.entry(index).or_default();
            tool.arguments.push_str(delta);
            if !delta.is_empty() {
                events.push(ProviderEvent::ToolCallDelta {
                    index,
                    arguments_delta: delta.into(),
                });
            }
        }
        "response.function_call_arguments.done" => {
            let index = value
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
                let tool = state.tools.entry(index).or_default();
                if arguments.starts_with(&tool.arguments) {
                    let delta = &arguments[tool.arguments.len()..];
                    if !delta.is_empty() {
                        events.push(ProviderEvent::ToolCallDelta {
                            index,
                            arguments_delta: delta.into(),
                        });
                    }
                }
                tool.arguments = arguments.into();
            }
        }
        "response.custom_tool_call_input.done" => {
            let index = value
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if let Some(arguments) = value.get("input").and_then(Value::as_str) {
                state.tools.entry(index).or_default().arguments = arguments.into();
            }
        }
        "response.output_item.done" => {
            let index = value
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if let Some(item) = value.get("item") {
                match item.get("type").and_then(Value::as_str) {
                    Some("reasoning") => events.push(ProviderEvent::ThinkingDone {
                        id: item.get("id").and_then(Value::as_str).map(str::to_owned),
                        encrypted_content: item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    }),
                    Some("function_call") | Some("custom_tool_call") => {
                        codex_tool_done(index, item, state, &mut events);
                    }
                    _ => {}
                }
            }
        }
        "response.done" | "response.completed" | "response.incomplete" => {
            let response = value.get("response").unwrap_or(&value);
            if let Some(usage) = response.get("usage") {
                events.push(ProviderEvent::Usage {
                    usage: normalize_usage(usage),
                });
            }
            for event in finish_codex_tools(state) {
                events.push(event);
            }
            let status = response.get("status").and_then(Value::as_str).unwrap_or(
                if event_type == "response.incomplete" {
                    "incomplete"
                } else {
                    "completed"
                },
            );
            let incomplete_reason = response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str);
            let raw_reason = incomplete_reason
                .map(|reason| format!("{status}.{reason}"))
                .unwrap_or_else(|| status.into());
            let mut reason = map_codex_stop_reason(status, incomplete_reason);
            if !state.tools.is_empty() && reason == StopReason::Stop {
                reason = StopReason::ToolUse;
            }
            events.push(ProviderEvent::Done {
                reason,
                raw_reason: Some(raw_reason),
            });
            state.done = true;
            return Ok((events, true));
        }
        _ => {}
    }
    Ok((events, false))
}

fn codex_tool_start(
    index: u64,
    item: &Value,
    state: &mut CodexStreamState,
    events: &mut Vec<ProviderEvent>,
) {
    if !matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call") | Some("custom_tool_call")
    ) {
        return;
    }
    let tool = state.tools.entry(index).or_default();
    tool.id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .into();
    tool.name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .into();
    tool.arguments = item
        .get("arguments")
        .or_else(|| item.get("input"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .into();
    if !tool.started {
        tool.started = true;
        events.push(ProviderEvent::ToolCallStart {
            index,
            id: tool.id.clone(),
            name: tool.name.clone(),
        });
    }
}

fn codex_tool_done(
    index: u64,
    item: &Value,
    state: &mut CodexStreamState,
    events: &mut Vec<ProviderEvent>,
) {
    codex_tool_start(index, item, state, events);
    let tool = state.tools.entry(index).or_default();
    if let Some(arguments) = item
        .get("arguments")
        .or_else(|| item.get("input"))
        .and_then(Value::as_str)
    {
        tool.arguments = arguments.into();
    }
    if !tool.done {
        tool.done = true;
        events.push(ProviderEvent::ToolCallDone {
            index,
            id: tool.id.clone(),
            name: tool.name.clone(),
            arguments: tool.arguments.clone(),
        });
    }
}

fn finish_codex_tools(state: &mut CodexStreamState) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    let mut indexes: Vec<_> = state.tools.keys().copied().collect();
    indexes.sort_unstable();
    for index in indexes {
        let Some(tool) = state.tools.get_mut(&index) else {
            continue;
        };
        if !tool.done {
            tool.done = true;
            events.push(ProviderEvent::ToolCallDone {
                index,
                id: tool.id.clone(),
                name: tool.name.clone(),
                arguments: tool.arguments.clone(),
            });
        }
    }
    events
}

fn normalize_usage(usage: &Value) -> NormalizedUsage {
    let prompt = first_u64(usage, &["input_tokens", "prompt_tokens"]);
    let output = first_u64(usage, &["output_tokens", "completion_tokens"]);
    let cache_read = first_nested_u64(
        usage,
        &[
            ("input_tokens_details", "cached_tokens"),
            ("prompt_tokens_details", "cached_tokens"),
        ],
    );
    let cache_write = first_nested_u64(
        usage,
        &[
            ("input_tokens_details", "cache_write_tokens"),
            ("prompt_tokens_details", "cache_write_tokens"),
        ],
    );
    let reasoning = first_nested_u64(
        usage,
        &[
            ("output_tokens_details", "reasoning_tokens"),
            ("completion_tokens_details", "reasoning_tokens"),
        ],
    );
    NormalizedUsage {
        input_tokens: prompt
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: reasoning,
        total_tokens: first_u64(usage, &["total_tokens"]).max(prompt.saturating_add(output)),
    }
}

fn first_u64(value: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
        .unwrap_or(0)
}

fn first_nested_u64(value: &Value, paths: &[(&str, &str)]) -> u64 {
    paths
        .iter()
        .find_map(|(parent, key)| value.get(*parent)?.get(*key)?.as_u64())
        .unwrap_or(0)
}

fn value_text(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str() {
        return Some(value.into());
    }
    value.as_array().map(|parts| {
        parts
            .iter()
            .filter_map(|part| {
                part.as_str()
                    .or_else(|| part.get("text").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("")
    })
}

fn map_stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        None | Some("stop") => StopReason::Stop,
        Some("tool_calls" | "function_call") => StopReason::ToolUse,
        Some("length" | "max_tokens") => StopReason::Length,
        Some("content_filter") => StopReason::ContentFilter,
        Some("error") => StopReason::Error,
        Some(_) => StopReason::Unknown,
    }
}

fn map_codex_stop_reason(status: &str, incomplete_reason: Option<&str>) -> StopReason {
    match (status, incomplete_reason) {
        ("completed", _) | ("done", _) => StopReason::Stop,
        ("incomplete", Some("max_output_tokens" | "max_tokens")) => StopReason::Length,
        ("incomplete", Some("content_filter")) => StopReason::ContentFilter,
        ("failed" | "cancelled", _) => StopReason::Error,
        ("incomplete", _) => StopReason::Unknown,
        _ => StopReason::Unknown,
    }
}

fn stream_error_label(value: &Value) -> String {
    let error = value.get("error").unwrap_or(value);
    error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("unspecified")
        .chars()
        .take(128)
        .collect()
}

fn parse_discovered_models(value: Value) -> Result<Vec<DiscoveredModel>> {
    let items = if let Some(items) = value.get("data").and_then(Value::as_array) {
        items.clone()
    } else if let Some(items) = value.as_array() {
        items.clone()
    } else {
        bail!("llama.cpp /v1/models response has no model list");
    };
    let mut models = Vec::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned();
        let context_window = [
            "/meta/n_ctx",
            "/meta/n_ctx_train",
            "/meta/context_length",
            "/n_ctx",
            "/n_ctx_train",
            "/context_length",
        ]
        .iter()
        .find_map(|pointer| item.pointer(pointer).and_then(Value::as_u64));
        models.push(DiscoveredModel {
            id: id.to_owned(),
            name,
            context_window,
            raw: item,
        });
    }
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_auth_schema_round_trips_without_debugging_secrets() {
        let auth: ProviderAuthFile = serde_json::from_value(json!({
            "openai-codex": {
                "type": "oauth",
                "access": "access-secret",
                "refresh": "refresh-secret",
                "expires": 123456789,
                "accountId": "account"
            },
            "example": {
                "type": "api_key",
                "key": "api-secret"
            }
        }))
        .unwrap();
        let debug = format!("{:?}", auth.get("openai-codex").unwrap());
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
        let encoded = serde_json::to_value(auth).unwrap();
        assert_eq!(encoded["openai-codex"]["accountId"], "account");
        assert_eq!(encoded["example"]["type"], "api_key");
    }

    #[test]
    fn extracts_codex_account_id_from_urlsafe_jwt() {
        let claims = json!({
            OPENAI_CODEX_ACCOUNT_CLAIM: { "chatgpt_account_id": "acct-test" }
        });
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let token = format!("header.{payload}.signature");
        assert_eq!(account_id_from_jwt(&token).unwrap(), "acct-test");
    }

    #[test]
    fn sse_decoder_handles_crlf_multiline_and_split_utf8() {
        let bytes = "event: update\r\ndata: {\"text\":\"🐈\"}\r\ndata: tail\r\n\r\n".as_bytes();
        let split = bytes
            .windows(4)
            .position(|window| window == "🐈".as_bytes())
            .unwrap()
            + 2;
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(&bytes[..split]).unwrap().is_empty());
        let frames = decoder.push(&bytes[split..]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("update"));
        assert_eq!(frames[0].data, "{\"text\":\"🐈\"}\ntail");
    }

    #[test]
    fn normalizes_openai_usage_without_double_counting_cache() {
        let usage = normalize_usage(&json!({
            "prompt_tokens": 120,
            "completion_tokens": 35,
            "total_tokens": 155,
            "prompt_tokens_details": { "cached_tokens": 20, "cache_write_tokens": 10 },
            "completion_tokens_details": { "reasoning_tokens": 12 }
        }));
        assert_eq!(usage.input_tokens, 90);
        assert_eq!(usage.output_tokens, 35);
        assert_eq!(usage.cache_read_tokens, 20);
        assert_eq!(usage.cache_write_tokens, 10);
        assert_eq!(usage.reasoning_tokens, 12);
        assert_eq!(usage.total_tokens, 155);
    }

    #[test]
    fn chat_stream_accumulates_tool_arguments() {
        let mut state = ChatStreamState::default();
        let first = SseFrame {
            event: None,
            data: json!({
                "id": "response-1",
                "choices": [{
                    "delta": {
                        "reasoning_content": "think",
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": { "name": "read", "arguments": "{\"path\":" }
                        }]
                    },
                    "finish_reason": null
                }]
            })
            .to_string(),
        };
        let (events, terminal) = chat_frame_events(first, &mut state).unwrap();
        assert!(!terminal);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ThinkingDelta { .. }))
        );
        assert!(events.iter().any(
            |event| matches!(event, ProviderEvent::ToolCallStart { name, .. } if name == "read")
        ));

        let second = SseFrame {
            event: None,
            data: json!({
                "choices": [{
                    "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "\"a\"}" } }] },
                    "finish_reason": "tool_calls"
                }]
            })
            .to_string(),
        };
        chat_frame_events(second, &mut state).unwrap();
        let (events, terminal) = chat_frame_events(
            SseFrame {
                event: None,
                data: "[DONE]".into(),
            },
            &mut state,
        )
        .unwrap();
        assert!(terminal);
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderEvent::ToolCallDone { arguments, .. } if arguments == "{\"path\":\"a\"}"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderEvent::Done {
                reason: StopReason::ToolUse,
                ..
            }
        )));
    }

    #[test]
    fn codex_body_enforces_stateless_stream_and_preserves_item_order() {
        let mut request = ProviderRequest::new(
            "gpt-5.5",
            vec![ProviderMessage {
                role: MessageRole::Assistant,
                content: vec![
                    ContentPart::Text {
                        text: "before".into(),
                    },
                    ContentPart::ToolCall {
                        id: "call-1".into(),
                        name: "read".into(),
                        arguments: json!({ "path": "a" }),
                    },
                    ContentPart::Text {
                        text: "after".into(),
                    },
                ],
            }],
        );
        request.thinking = ThinkingLevel::Minimal;
        request
            .request_parameters
            .insert("store".into(), Value::Bool(true));
        request
            .request_parameters
            .insert("stream".into(), Value::Bool(false));
        let body = build_codex_responses_body(&request).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "message");
    }

    #[test]
    fn llama_uses_v1_discovery_and_pi_thinking_budget() {
        assert_eq!(
            llama_models_url("http://127.0.0.1:8080").unwrap(),
            "http://127.0.0.1:8080/v1/models"
        );
        assert_eq!(ThinkingLevel::Medium.token_budget(4_096), Some(3_072));
        let models = parse_discovered_models(json!({
            "data": [
                { "id": "z-model", "meta": { "n_ctx": 32768 } },
                { "id": "a-model", "context_length": 8192 }
            ]
        }))
        .unwrap();
        assert_eq!(models[0].id, "a-model");
        assert_eq!(models[1].context_window, Some(32_768));
    }
}
