//! Native provider transports.
//!
//! This module deliberately has no provider catalog, update checker, telemetry, or
//! background network activity. Network calls happen only when a caller invokes a
//! request against its configured endpoint, refreshes OpenAI OAuth, or asks for
//! llama.cpp model discovery.

use crate::config::{CompatibleAuth, CompatibleProvider, LlamaConfig, ModelPreset};
use crate::lossless_json::JsString;
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
use reqwest::{Client, Response};
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
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
        output: Value,
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
    /// Current model capability, including after a model switch. Historical
    /// image blocks stay in JSONL but are omitted from non-vision requests.
    #[serde(default = "supports_images_by_default")]
    pub supports_images: bool,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub messages: Vec<ProviderMessage>,
    /// Complete Pi logical messages before provider-specific conversion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_messages: Option<Vec<Value>>,
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
    #[serde(default)]
    pub disable_cache: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_idle_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_connect_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<crate::codex_websocket::Transport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_verbosity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// Pi compatible-provider sampling parameters, applied last. The dedicated
    /// Codex builder ignores these, as pinned Pi does.
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
            supports_images: true,
            system_prompt: String::new(),
            messages,
            logical_messages: None,
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            max_tokens: None,
            temperature: None,
            session_id: None,
            disable_cache: false,
            timeout_ms: None,
            http_idle_timeout_ms: None,
            websocket_connect_timeout_ms: None,
            transport: None,
            service_tier: None,
            text_verbosity: None,
            reasoning_summary: None,
            max_retries: None,
            max_retry_delay_ms: None,
            tool_choice: None,
            request_parameters: Map::new(),
            headers: BTreeMap::new(),
        }
    }
}

fn supports_images_by_default() -> bool {
    true
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
    #[serde(default)]
    pub reasoning_present: bool,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Aborted,
    ToolUse,
    Length,
    ContentFilter,
    Error,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    AbortMessage {
        message: String,
    },
    Diagnostic {
        diagnostic: Value,
    },
    BlockContent {
        index: u64,
        block: Value,
    },
    ResponseInfo {
        response_id: Option<String>,
        end_turn: Option<bool>,
        cost_multiplier: Option<f64>,
    },
    Failure {
        message: String,
    },
    Start {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
    },
    Metadata {
        response_id: Option<String>,
        response_model: Option<String>,
    },
    TextDelta {
        index: u64,
        delta: JsString,
    },
    TextDone {
        index: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<JsString>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    ThinkingDelta {
        index: u64,
        delta: JsString,
    },
    ThinkingDone {
        index: u64,
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
        arguments_delta: JsString,
    },
    ToolCallDone {
        index: u64,
        id: String,
        name: String,
        arguments: JsString,
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
    pub pi_model: Value,
}

impl fmt::Debug for OpenAiCompatibleEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleEndpoint")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("headers", &"<redacted>")
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
            pi_model: crate::completions::preset_model(
                &provider.id,
                &provider.base_url,
                model,
                false,
            ),
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
            pi_model: crate::completions::preset_model(
                "llama.cpp",
                &format!("http://127.0.0.1:{}/v1", config.port),
                &model.cloned().unwrap_or_default(),
                true,
            ),
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

    /// Login/logout and refresh share one cross-process lock. In particular,
    /// logout must wait out refresh before removing its newly written token.
    pub async fn set_codex(&self, credential: Option<ProviderCredential>) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let lock = acquire_auth_lock(&store.lock_path)?;
            let mut auth = store.load()?;
            if let Some(credential) = credential {
                auth.insert(OPENAI_CODEX_PROVIDER_ID.into(), credential);
            } else {
                auth.remove(OPENAI_CODEX_PROVIDER_ID);
            }
            crate::config::atomic_private_json(&store.path, &auth)?;
            FileExt::unlock(&lock)?;
            Ok(())
        })
        .await
        .context("join credential update")?
    }

    async fn codex_access(&self, client: &Client) -> Result<CodexAccess> {
        self.codex_access_at(client, OPENAI_CODEX_TOKEN_URL).await
    }

    async fn codex_access_at(&self, client: &Client, token_url: &str) -> Result<CodexAccess> {
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
            ..
        } = credential
        else {
            bail!("OpenAI Codex requires an OAuth credential");
        };

        if !oauth_needs_refresh(expires) {
            FileExt::unlock(&lock)?;
            return CodexAccess::from_token(access);
        }

        let refreshed = refresh_codex_token(client, token_url, &refresh)
            .await
            .map_err(|error| anyhow!("OAuth refresh failed for openai-codex: {error}"))?;
        let ProviderCredential::OAuth { access, .. } = &refreshed else {
            unreachable!()
        };
        let access = access.clone();
        auth.insert(OPENAI_CODEX_PROVIDER_ID.to_owned(), refreshed);
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
            .no_proxy()
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
        let body = crate::completions::provider_body(&request, &endpoint.pi_model);
        let headers = compatible_headers(endpoint, &request)?;
        let mut secrets = match &endpoint.auth {
            EndpointAuth::None => vec![],
            EndpointAuth::Bearer(secret) => vec![secret.clone()],
            EndpointAuth::Header { value, .. } => vec![value.clone()],
        };
        for (name, value) in &request.headers {
            if name.eq_ignore_ascii_case("authorization") {
                secrets.push(value.strip_prefix("Bearer ").unwrap_or(value).into());
            }
        }
        let response = crate::provider_http::send_compatible(
            &self.http, url, headers, body, &request, &secrets,
        )
        .await?;
        Ok(parse_chat_completions_stream(
            response,
            endpoint.pi_model.clone(),
            request.http_idle_timeout_ms,
        ))
    }

    pub async fn stream_openai_codex(
        &self,
        endpoint: &CodexEndpoint,
        request: ProviderRequest,
    ) -> Result<ProviderStream> {
        let auth = endpoint.auth_store.codex_access(&self.http).await?;
        let url = crate::codex_http::resolve_url(&endpoint.base_url);
        let body = build_codex_responses_body(&request)?;
        let headers = codex_headers(endpoint, &request, &auth)?;
        let secret = auth.access_token.clone();
        let stream = crate::codex_websocket::stream(
            self.http.clone(),
            url,
            headers,
            body,
            request,
            auth.account_id,
            auth.access_token,
        )?;
        Ok(Box::pin(stream.map(move |event| match event {
            Ok(ProviderEvent::Failure { message }) => Ok(ProviderEvent::Failure {
                message: crate::provider_http::safe_error_body(
                    &message,
                    std::slice::from_ref(&secret),
                ),
            }),
            Err(error) => Err(crate::json_error::redact(
                error,
                std::slice::from_ref(&secret),
            )),
            event => event,
        })))
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
    set_private_file(path)?;
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

async fn refresh_codex_token(
    client: &Client,
    endpoint: &str,
    refresh_token: &str,
) -> Result<ProviderCredential> {
    // Allowed network: selected OpenAI subscription credential refresh.
    let response = client
        .post(endpoint)
        .timeout(OAUTH_REFRESH_TIMEOUT)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", OPENAI_CODEX_CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "OpenAI Codex token refresh error: {}",
                if error.is_timeout() {
                    "The operation was aborted due to timeout"
                } else {
                    "fetch failed"
                }
            )
        })?;
    crate::oauth::read_token_response(response, "refresh", &[refresh_token.to_owned()]).await
}

pub(crate) fn account_id_from_jwt(token: &str) -> Result<String> {
    if token.split('.').count() != 3 {
        bail!("OpenAI Codex access token is not a JWT");
    }
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

fn compatible_headers(
    endpoint: &OpenAiCompatibleEndpoint,
    request: &ProviderRequest,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_str(&pi_user_agent())?);
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
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
    for (name, value) in endpoint.pi_model["headers"]
        .as_object()
        .into_iter()
        .flatten()
    {
        if let Some(value) = value.as_str() {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes())?,
                HeaderValue::from_str(value)?,
            );
        }
    }
    insert_headers(&mut headers, &endpoint.headers)?;
    let compat = crate::completions::compatibility(&endpoint.pi_model);
    if !request.disable_cache
        && compat["sendSessionAffinityHeaders"] == true
        && let Some(session) = request.session_id.as_ref().filter(|v| !v.is_empty())
    {
        if compat["sessionAffinityFormat"] == "openrouter" {
            headers.insert("x-session-id", HeaderValue::from_str(session)?);
        } else {
            if compat["sessionAffinityFormat"] == "openai" {
                headers.insert("session_id", HeaderValue::from_str(session)?);
            }
            headers.insert("x-client-request-id", HeaderValue::from_str(session)?);
            headers.insert("x-session-affinity", HeaderValue::from_str(session)?);
        }
    }
    insert_headers(&mut headers, &request.headers)?;
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
        HeaderValue::from_str(&pi_user_agent()).context("invalid Pi-compatible user agent")?,
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
    if !request.disable_cache
        && let Some(session_id) = request.session_id.as_deref().filter(|v| !v.is_empty())
    {
        let clamped: String = session_id.chars().take(64).collect();
        let value = HeaderValue::from_str(&clamped).context("invalid provider session ID")?;
        headers.insert("session-id", value.clone());
        headers.insert("x-client-request-id", value);
    }
    Ok(headers)
}

fn pi_user_agent() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    let release = fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|_| "unknown".into());
    format!("pi ({platform} {release}; {arch})")
}

fn build_codex_responses_body(request: &ProviderRequest) -> Result<Value> {
    crate::codex::provider_body(request)
}

pub(crate) fn pi_short_hash(value: &str) -> String {
    let mut h1 = 0xdead_beefu32;
    let mut h2 = 0x41c6_ce57u32;
    for code_unit in value.encode_utf16() {
        h1 = (h1 ^ u32::from(code_unit)).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ u32::from(code_unit)).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", base36(h2), base36(h1))
}

fn base36(mut value: u32) -> String {
    if value == 0 {
        return "0".into();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut output = Vec::new();
    while value > 0 {
        output.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    output.reverse();
    String::from_utf8(output).expect("base36 is ASCII")
}

#[derive(Debug, Default)]
struct SseFrame {
    #[allow(dead_code)]
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
struct ChatTool {
    id: String,
    name: String,
    arguments: JsString,
    stream_index: Option<String>,
    custom: bool,
    custom_started: bool,
    custom_input: String,
}
struct ChatStreamState {
    done: bool,
    raw_stop_reason: Option<String>,
    tools: BTreeMap<u64, ChatTool>,
    tools_by_index: HashMap<String, u64>,
    tools_by_id: HashMap<String, u64>,
    next_content_index: u64,
    text_index: Option<u64>,
    thinking_index: Option<u64>,
    reasoning_details: Vec<Value>,
    model: Value,
    requires_finish: bool,
}
impl Default for ChatStreamState {
    fn default() -> Self {
        Self::new(json!({}))
    }
}
impl ChatStreamState {
    fn new(model: Value) -> Self {
        let requires_finish =
            crate::completions::compatibility(&model)["supportsFinishReason"] == true;
        Self {
            done: false,
            raw_stop_reason: None,
            tools: BTreeMap::new(),
            tools_by_index: HashMap::new(),
            tools_by_id: HashMap::new(),
            next_content_index: 0,
            text_index: None,
            thinking_index: None,
            reasoning_details: Vec::new(),
            model,
            requires_finish,
        }
    }
    fn thinking(&mut self, signature: &str, events: &mut Vec<ProviderEvent>) -> u64 {
        if let Some(index) = self.thinking_index {
            return index;
        }
        let index = self.next_content_index;
        self.next_content_index += 1;
        self.thinking_index = Some(index);
        events.push(ProviderEvent::ThinkingDone {
            index,
            id: None,
            encrypted_content: Some(signature.into()),
        });
        index
    }
    fn error(&self) -> Option<String> {
        match self.raw_stop_reason.as_deref() {
            None if self.requires_finish => Some("Stream ended without finish_reason".into()),
            Some("stop" | "end" | "length" | "tool_calls" | "function_call") | None => None,
            Some(reason) => Some(format!("Provider finish_reason: {reason}")),
        }
    }
}

#[allow(clippy::collapsible_if)]
async fn next_http_chunk<S>(
    bytes: &mut S,
    idle_timeout_ms: Option<u64>,
) -> Result<Option<std::result::Result<bytes::Bytes, reqwest::Error>>>
where
    S: futures_util::Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    if let Some(ms) = idle_timeout_ms.filter(|ms| *ms > 0) {
        tokio::time::timeout(std::time::Duration::from_millis(ms), bytes.next())
            .await
            .map_err(|_| anyhow!("terminated"))
    } else {
        Ok(bytes.next().await)
    }
}

fn parse_chat_completions_stream(
    response: Response,
    model: Value,
    idle_timeout_ms: Option<u64>,
) -> ProviderStream {
    Box::pin(try_stream! {
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut state = ChatStreamState::new(model);
        yield ProviderEvent::Start{response_id:None};
        'stream: while let Some(chunk) = next_http_chunk(&mut bytes, idle_timeout_ms).await? {
            let chunk = chunk.context("read OpenAI-compatible stream")?;
            for frame in decoder.push(&chunk)? {
                let (events, terminal) = chat_frame_events(frame, &mut state)?;
                for event in events { yield event; }
                if terminal {break 'stream;}
            }
        }
        if !state.done {
            let residual = decoder.finish()?;
            if let Some(frame) = residual {
                let (events, _) = chat_frame_events(frame, &mut state)?;
                for event in events { yield event; }
            }
        }
        for event in finish_chat_stream(&mut state) { yield event; }
        if let Some(error)=state.error(){Err(anyhow!(error))?;}
    })
}

fn chat_frame_events(
    frame: SseFrame,
    state: &mut ChatStreamState,
) -> Result<(Vec<ProviderEvent>, bool)> {
    if frame.data.trim() == "[DONE]" {
        return Ok((finish_chat_stream(state), true));
    }
    let value: Value = crate::lossless_json::from_str(&frame.data)
        .map_err(|_| crate::json_error::malformed("", &frame.data))?;
    if let Some(error) = value.get("error") {
        let message = error["message"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if error.as_object().is_some_and(|v| !v.is_empty()) {
                    error.to_string()
                } else {
                    "(no status code or body)".into()
                }
            });
        bail!("{}", crate::provider_http::safe_error_body(&message, &[]));
    }
    let mut events = Vec::new();
    if !value.is_object() {
        return Ok((events, false));
    }
    events.push(ProviderEvent::Metadata {
        response_id: value["id"].as_str().map(str::to_owned),
        response_model: value["model"]
            .as_str()
            .filter(|v| !v.is_empty() && Some(*v) != state.model["id"].as_str())
            .map(str::to_owned),
    });
    if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
        events.push(ProviderEvent::Usage {
            usage: normalize_chat_usage(usage),
        });
    }
    let Some(choice) = value["choices"].as_array().and_then(|v| v.first()) else {
        return Ok((events, false));
    };
    if value["usage"].is_null()
        && let Some(usage) = choice.get("usage").filter(|v| !v.is_null())
    {
        events.push(ProviderEvent::Usage {
            usage: normalize_chat_usage(usage),
        });
    }
    if let Some(reason) = choice["finish_reason"].as_str().filter(|v| !v.is_empty()) {
        state.raw_stop_reason = Some(reason.into());
    }
    let delta = &choice["delta"];
    if let Some(content) = JsString::from_value(&delta["content"]).filter(|v| !v.is_empty()) {
        let index = *state.text_index.get_or_insert_with(|| {
            let index = state.next_content_index;
            state.next_content_index += 1;
            index
        });
        events.push(ProviderEvent::TextDelta {
            index,
            delta: content,
        });
    }
    for field in ["reasoning_content", "reasoning", "reasoning_text"] {
        if let Some(reasoning) = JsString::from_value(&delta[field]).filter(|v| !v.is_empty()) {
            let signature = if state.model["provider"] == "opencode-go" && field == "reasoning" {
                "reasoning_content"
            } else {
                field
            };
            let index = state.thinking(signature, &mut events);
            events.push(ProviderEvent::ThinkingDelta {
                index,
                delta: reasoning,
            });
            break;
        }
    }
    for call in delta["tool_calls"].as_array().into_iter().flatten() {
        append_chat_tool_delta(call, state, &mut events);
    }
    for detail in delta["reasoning_details"].as_array().into_iter().flatten() {
        if crate::completions::valid_reasoning_detail(detail) {
            let index = state.thinking("", &mut events);
            append_reasoning_detail(&mut state.reasoning_details, detail);
            // Keep replay metadata on the in-memory block even if cancellation
            // interrupts before finalization; do not expose it as thinking text.
            events.push(ProviderEvent::ThinkingDone {
                index,
                id: None,
                encrypted_content: Some(crate::lossless_json::to_string(&state.reasoning_details)?),
            });
        }
    }
    Ok((events, false))
}
fn append_reasoning_detail(details: &mut Vec<Value>, detail: &Value) {
    let kind = detail["type"].as_str().unwrap_or_default();
    if let Some(last) = details.last_mut()
        && last["type"] == kind
        && matches!(kind, "reasoning.text" | "reasoning.summary")
    {
        let field = if kind == "reasoning.text" {
            "text"
        } else {
            "summary"
        };
        last[field] = json!(format!(
            "{}{}",
            last[field].as_str().unwrap_or_default(),
            detail[field].as_str().unwrap_or_default()
        ));
        if kind == "reasoning.text"
            && last["signature"].as_str().is_none_or(str::is_empty)
            && let Some(v) = detail.get("signature")
        {
            last["signature"] = v.clone();
        }
        for field in ["id", "format", "index"] {
            if (last[field].is_null() || (field == "format" && last[field] == ""))
                && let Some(v) = detail.get(field)
            {
                last[field] = v.clone();
            }
        }
    } else {
        details.push(detail.clone());
    }
}
fn append_chat_tool_delta(
    call: &Value,
    state: &mut ChatStreamState,
    events: &mut Vec<ProviderEvent>,
) {
    let stream_index = call
        .get("index")
        .filter(|v| v.is_number())
        .map(Value::to_string);
    let id = call["id"].as_str().unwrap_or_default();
    let name = call["function"]["name"]
        .as_str()
        .or_else(|| call["custom"]["name"].as_str())
        .unwrap_or_default();
    let existing = stream_index
        .as_ref()
        .and_then(|index| state.tools_by_index.get(index))
        .or_else(|| {
            (!id.is_empty())
                .then(|| state.tools_by_id.get(id))
                .flatten()
        })
        .copied();
    let custom = call["custom"].is_object() && !call["function"].is_object();
    let index = existing.unwrap_or_else(|| {
        let index = state.next_content_index;
        state.next_content_index += 1;
        state.tools.insert(
            index,
            ChatTool {
                id: id.into(),
                name: name.into(),
                stream_index: stream_index.clone(),
                custom,
                ..Default::default()
            },
        );
        if let Some(stream_index) = &stream_index {
            state.tools_by_index.insert(stream_index.clone(), index);
        }
        events.push(ProviderEvent::ToolCallStart {
            index,
            id: id.into(),
            name: name.into(),
        });
        index
    });
    let tool = state.tools.get_mut(&index).expect("created tool block");
    if tool.stream_index.is_none()
        && let Some(stream_index) = stream_index
    {
        state.tools_by_index.insert(stream_index.clone(), index);
        tool.stream_index = Some(stream_index);
    }
    if !id.is_empty() {
        state.tools_by_id.insert(id.into(), index);
        if tool.id.is_empty() {
            tool.id = id.into();
        }
    }
    if tool.name.is_empty() {
        tool.name = name.into();
    }
    if custom && !tool.custom {
        tool.custom = true;
        tool.arguments = JsString::default();
    }
    let mut fragment = JsString::default();
    if let Some(args) =
        JsString::from_value(&call["function"]["arguments"]).filter(|v| !v.is_empty())
    {
        fragment = args.clone();
        tool.arguments.push_js(&args);
    } else if let Some(input) = call["custom"]["input"].as_str().filter(|v| !v.is_empty()) {
        if !tool.custom_started {
            fragment.push_str("{\"input\":\"");
            tool.custom_started = true;
        }
        let escaped = serde_json::to_string(input).unwrap();
        fragment.push_str(&escaped[1..escaped.len() - 1]);
        tool.custom_input.push_str(input);
        tool.arguments.push_js(&fragment);
    }
    events.push(ProviderEvent::ToolCallDelta {
        index,
        arguments_delta: fragment,
    });
    // Identity can arrive after start. Update the complete in-memory call,
    // including Pi's current partial JSON, without adding a visible tool start.
    events.push(ProviderEvent::ToolCallDone {
        index,
        id: tool.id.clone(),
        name: tool.name.clone(),
        arguments: if tool.custom {
            json!({"input":tool.custom_input}).to_string().into()
        } else {
            tool.arguments.clone()
        },
    });
}
fn finish_chat_stream(state: &mut ChatStreamState) -> Vec<ProviderEvent> {
    if state.done {
        return Vec::new();
    }
    state.done = true;
    let mut events = Vec::new();
    for (&index, tool) in &mut state.tools {
        if tool.custom {
            let fragment = if tool.custom_started {
                "\"}"
            } else {
                "{\"input\":\"\"}"
            };
            events.push(ProviderEvent::ToolCallDelta {
                index,
                arguments_delta: fragment.into(),
            });
            tool.arguments = json!({"input":tool.custom_input}).to_string().into();
        }
        events.push(ProviderEvent::ToolCallDone {
            index,
            id: tool.id.clone(),
            name: tool.name.clone(),
            arguments: tool.arguments.clone(),
        });
    }
    let reason = match state.raw_stop_reason.as_deref() {
        Some("stop" | "end") => StopReason::Stop,
        Some("length") => StopReason::Length,
        Some("tool_calls" | "function_call") => StopReason::ToolUse,
        None if !state.requires_finish => {
            if state.tools.is_empty() {
                StopReason::Stop
            } else {
                StopReason::ToolUse
            }
        }
        _ => StopReason::Error,
    };
    events.push(ProviderEvent::Done {
        reason,
        raw_reason: state.raw_stop_reason.clone(),
    });
    events
}
fn normalize_chat_usage(raw: &Value) -> NormalizedUsage {
    let prompt = raw["prompt_tokens"].as_u64().unwrap_or(0);
    let cache_read = raw["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| raw["prompt_cache_hit_tokens"].as_u64())
        .or_else(|| raw["cached_tokens"].as_u64())
        .unwrap_or(0);
    let cache_write = raw["prompt_tokens_details"]["cache_write_tokens"]
        .as_u64()
        .unwrap_or(0);
    let input = prompt
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    let output = raw["completion_tokens"].as_u64().unwrap_or(0);
    NormalizedUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: raw["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap_or(0),
        reasoning_present: true,
        total_tokens: input + output + cache_read + cache_write,
    }
}

pub(crate) fn parse_codex_responses_stream(
    response: Response,
    model: String,
    service_tier: Option<String>,
    idle_timeout_ms: Option<u64>,
) -> ProviderStream {
    Box::pin(try_stream! {
        yield ProviderEvent::AbortMessage {message:"This operation was aborted".into()};
        yield ProviderEvent::Start {response_id:None};
        let mut bytes=response.bytes_stream();
        let mut decoder=crate::codex_stream::Decoder::default();
        let mut state=crate::codex_stream::State::new(model).with_service_tier(service_tier);
        loop {
            let chunk=next_http_chunk(&mut bytes,idle_timeout_ms).await?.transpose().context("read OpenAI Codex stream")?;
            let eof=chunk.is_none();
            for value in decoder.feed(chunk.as_deref().unwrap_or_default(),eof) {
                let value=value?;
                for event in state.push(&value)? {yield event;}
                if state.done {return;}
            }
            if eof {break;}
        }
        Err(anyhow!("OpenAI Responses stream ended before a terminal response event"))?;
    })
}

#[cfg(test)]
type CodexStreamState = crate::codex_stream::State;
#[cfg(test)]
fn codex_frame_events(
    frame: SseFrame,
    state: &mut CodexStreamState,
) -> Result<(Vec<ProviderEvent>, bool)> {
    let events = state.push(&serde_json::from_str::<Value>(&frame.data)?)?;
    Ok((events, state.done))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_images_follow_all_results_and_respect_current_model_capability() {
        let image = json!({"type":"input_image","detail":"auto","image_url":"data:image/png;base64,fixture"});
        let mut request = ProviderRequest::new(
            "fixture",
            vec![
                ProviderMessage {
                    role: MessageRole::Tool,
                    content: vec![ContentPart::ToolResult {
                        tool_call_id: "first".into(),
                        output: json!([{"type":"input_text","text":"read image"},image.clone()]),
                        is_error: false,
                    }],
                },
                ProviderMessage {
                    role: MessageRole::Tool,
                    content: vec![ContentPart::ToolResult {
                        tool_call_id: "second".into(),
                        output: json!("second result"),
                        is_error: false,
                    }],
                },
            ],
        );
        let body = crate::completions::provider_body(
            &request,
            &json!({"id":"fixture","provider":"fixture","api":"openai-completions"}),
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["content"], "read image");
        assert_eq!(messages[1]["tool_call_id"], "second");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            messages[2]["content"][0]["text"],
            "Attached image(s) from tool result:"
        );
        assert_eq!(
            messages[2]["content"][1]["image_url"]["url"],
            image["image_url"]
        );
        request.supports_images = false;
        let body = crate::completions::provider_body(
            &request,
            &json!({"id":"fixture","provider":"fixture","api":"openai-completions"}),
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(!serde_json::to_string(&messages).unwrap().contains("base64"));
        let body = crate::codex::provider_body(&request).unwrap();
        let codex = body["input"].as_array().unwrap();
        assert_eq!(
            codex[0]["output"],
            "read image\n(tool image omitted: model does not support images)"
        );
        assert!(!serde_json::to_string(&codex).unwrap().contains("base64"));
    }

    #[test]
    fn responses_parser_and_worker_keep_multiple_output_items() {
        let items = [
            json!({"type":"reasoning","id":"rs_first","summary":[{"type":"summary_text","text":"first thought"}],"encrypted_content":"first-signature"}),
            json!({"type":"message","id":"msg_commentary","phase":"commentary","role":"assistant","content":[{"type":"output_text","text":"Checking the file","annotations":[]}]}),
            json!({"type":"reasoning","id":"rs_second","summary":[{"type":"summary_text","text":"second thought"}],"encrypted_content":"second-signature"}),
            json!({"type":"message","id":"msg_final","phase":"final_answer","role":"assistant","content":[{"type":"output_text","text":"Finished","annotations":[]}]}),
        ];
        let mut state = CodexStreamState::default();
        let mut response = crate::response::ResponseAssembly::default();
        for (index, item) in items.iter().enumerate() {
            if item["type"] == "reasoning" {
                let frame = json!({"type":"response.reasoning_summary_text.delta","output_index":index,"delta":item["summary"][0]["text"]});
                let (events, _) = codex_frame_events(
                    SseFrame {
                        event: None,
                        data: frame.to_string(),
                    },
                    &mut state,
                )
                .unwrap();
                for event in events {
                    response.push(event);
                }
            }
            let frame =
                json!({"type":"response.output_item.done","output_index":index,"item":item});
            let (events, _) = codex_frame_events(
                SseFrame {
                    event: None,
                    data: frame.to_string(),
                },
                &mut state,
            )
            .unwrap();
            for event in events {
                response.push(event);
            }
        }
        let message = serde_json::to_value(response.message(
            "openai-codex",
            "fixture",
            &crate::agent::ModelCost::default(),
        ))
        .unwrap();
        assert_eq!(message["content"].as_array().unwrap().len(), 4);
        for index in [0, 2] {
            let signature: Value = serde_json::from_str(
                message["content"][index]["thinkingSignature"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(signature, items[index]);
        }
        assert_eq!(message["content"][1]["text"], "Checking the file");
        assert_eq!(message["content"][3]["text"], "Finished");
        for index in [1, 3] {
            let signature: Value =
                serde_json::from_str(message["content"][index]["textSignature"].as_str().unwrap())
                    .unwrap();
            assert_eq!(signature["id"], items[index]["id"]);
            assert_eq!(signature["phase"], items[index]["phase"]);
        }
    }

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
            ProviderEvent::ToolCallDone { arguments, .. } if arguments.as_str() == "{\"path\":\"a\"}"
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
        let reasoning = json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [],
            "encrypted_content": "secret",
        });
        let mut request = ProviderRequest::new(
            "gpt-5.5",
            vec![
                ProviderMessage {
                    role: MessageRole::Assistant,
                    content: vec![
                        ContentPart::Thinking {
                            text: String::new(),
                            id: Some("rs_1".into()),
                            encrypted_content: Some(reasoning.to_string()),
                        },
                        ContentPart::Text {
                            text: "before".into(),
                            text_signature: Some(
                                json!({"v":1,"id":"msg_1","phase":"commentary"}).to_string(),
                            ),
                        },
                        ContentPart::ToolCall {
                            id: "call_1|fc_1".into(),
                            name: "read".into(),
                            arguments: json!({ "path": "a" }),
                        },
                    ],
                },
                ProviderMessage {
                    role: MessageRole::Tool,
                    content: vec![ContentPart::ToolResult {
                        tool_call_id: "call_1|fc_1".into(),
                        output: json!([
                            {"type":"input_text","text":"image"},
                            {"type":"input_image","detail":"auto","image_url":"data:image/png;base64,AA=="}
                        ]),
                        is_error: false,
                    }],
                },
            ],
        );
        request.thinking = ThinkingLevel::Minimal;
        request.session_id = Some("session-cache-key".into());
        request
            .request_parameters
            .insert("store".into(), Value::Bool(true));
        request
            .request_parameters
            .insert("stream".into(), Value::Bool(false));
        let body = build_codex_responses_body(&request).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["prompt_cache_key"], "session-cache-key");
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["input"][0], reasoning);
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(body["input"][1]["id"], "msg_1");
        assert_eq!(body["input"][1]["phase"], "commentary");
        assert_eq!(body["input"][2]["type"], "function_call");
        assert_eq!(body["input"][2]["id"], "fc_1");
        assert_eq!(body["input"][2]["call_id"], "call_1");
        assert_eq!(body["input"][3]["type"], "function_call_output");
        assert_eq!(body["input"][3]["call_id"], "call_1");
        assert_eq!(body["input"][3]["output"][1]["type"], "input_image");
    }
}

#[cfg(test)]
#[path = "provider_auth_tests.rs"]
mod provider_auth_tests;
