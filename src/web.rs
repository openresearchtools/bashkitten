use crate::auth;
use crate::config::AppConfig;
use crate::models;
use crate::paths::{AppPaths, ensure_private_dir, set_private_file};
use crate::session::{self, ControlRequest, Delivery, NewSession, QueueAction};
use anyhow::{Context, Result};
use async_stream::stream;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive};
use axum::response::{Html, IntoResponse, Response, Sse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const COOKIE: &str = "bashkitten_session";
mod download_api;
mod llama_api;

#[derive(Clone)]
pub struct WebState {
    pub paths: AppPaths,
    pub config: Arc<RwLock<AppConfig>>,
    bound_port: u16,
    csrf_tokens: Arc<RwLock<std::collections::BTreeMap<String, String>>>,
    restart_pending: Arc<std::sync::atomic::AtomicBool>,
    restart_error: Arc<RwLock<Option<String>>>,
    pub oauth: crate::oauth::LoginManager,
    pub llama_operation: llama_api::Operations,
    pub downloads: download_api::Downloads,
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"ok": false, "error": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self(StatusCode::BAD_REQUEST, error.to_string())
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct Page {
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ModelChange {
    model: String,
    thinking: String,
}

#[derive(Deserialize)]
struct FolderChange {
    cwd: PathBuf,
}

#[derive(Deserialize)]
struct OAuthStart {
    method: String,
}

#[derive(Deserialize)]
struct OAuthCode {
    input: String,
}

#[derive(Deserialize)]
struct QueueMutation {
    id: String,
    action: QueueAction,
    content: Option<String>,
    #[serde(default)]
    edit_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForkRequest {
    entry_id: String,
}

#[derive(Default, Deserialize)]
struct AttachmentQuery {
    download: Option<bool>,
}

#[derive(Deserialize)]
struct FolderQuery {
    path: Option<PathBuf>,
    #[serde(default)]
    nearest: bool,
}

#[derive(Deserialize)]
struct CreateFolder {
    parent: PathBuf,
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    has_user: bool,
    authenticated: bool,
    csrf: Option<String>,
    version: &'static str,
}

pub fn router(paths: AppPaths, config: AppConfig) -> Result<Router> {
    router_with_restart_error(paths, config, None)
}

fn router_with_restart_error(
    paths: AppPaths,
    config: AppConfig,
    restart_error: Option<String>,
) -> Result<Router> {
    let csrf_tokens = Arc::new(RwLock::new(auth::prepare_csrf_tokens(&paths)?));
    let downloads = download_api::Downloads::load(&paths)?;
    let state = WebState {
        paths,
        csrf_tokens,
        bound_port: config.web_port,
        restart_pending: Default::default(),
        restart_error: Arc::new(RwLock::new(restart_error)),
        config: Arc::new(RwLock::new(config)),
        oauth: crate::oauth::LoginManager::default(),
        llama_operation: Default::default(),
        downloads,
    };
    Ok(Router::new()
        .route("/", get(index))
        .route("/api/bootstrap", get(bootstrap))
        .route("/api/signup", post(signup))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/models", get(list_models))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/session-groups",
            get(preview_project_sessions).delete(delete_project_sessions),
        )
        .route("/api/sessions/{id}/segments/{segment}", get(read_segment))
        .route(
            "/api/sessions/{id}/segments/current",
            get(read_current_segment),
        )
        .route("/api/sessions/{id}/events", get(events))
        .route("/api/sessions/{id}/resume", post(resume_session))
        .route("/api/sessions/{id}/messages", post(send_message))
        .route("/api/sessions/{id}/fork", post(fork_session))
        .route(
            "/api/sessions/{id}",
            delete(delete_session).patch(rename_session),
        )
        .route(
            "/api/sessions/{id}/attachments/{upload}/{name}",
            get(download_attachment),
        )
        .route("/api/sessions/{id}/status", get(session_status))
        .route("/api/sessions/{id}/queue", post(mutate_queue))
        .route("/api/sessions/{id}/stop", post(stop_session))
        .route("/api/sessions/{id}/compact", post(compact_session))
        .route("/api/sessions/{id}/goal", post(update_session_goal))
        .route("/api/sessions/{id}/model", post(change_model))
        .route("/api/sessions/{id}/cwd", post(change_cwd))
        .route("/api/folders", get(list_folders).post(create_folder))
        .route("/api/settings", get(get_settings).post(save_settings))
        .route(
            "/api/provider/login",
            get(provider_login_status).post(provider_login),
        )
        .route("/api/provider/login/code", post(provider_login_code))
        .route("/api/provider/login/cancel", post(provider_login_cancel))
        .route("/api/provider/logout", post(provider_logout))
        .route("/api/llama/restart", post(restart_llama))
        .merge(llama_api::routes())
        .merge(download_api::routes())
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(axum::middleware::from_fn(
            |request: axum::extract::Request, next: axum::middleware::Next| async move {
                let mut response = next.run(request).await;
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response
            },
        ))
        .with_state(state))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == COOKIE).then_some(value)
        })
}

fn authenticated(state: &WebState, headers: &HeaderMap) -> ApiResult<(String, auth::LoginSession)> {
    let token = cookie_token(headers)
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "Authentication required".into()))?;
    let login = auth::validate(&state.paths, token)
        .map_err(|_| ApiError(StatusCode::UNAUTHORIZED, "Authentication required".into()))?;
    Ok((token.to_owned(), login))
}

fn configured_origin(state: &WebState) -> String {
    format!("http://127.0.0.1:{}", state.bound_port)
}

fn require_origin(state: &WebState, headers: &HeaderMap) -> ApiResult<()> {
    let supplied = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if headers.get_all(header::ORIGIN).iter().count() != 1 || supplied != configured_origin(state) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "Invalid request origin".into(),
        ));
    }
    Ok(())
}

fn require_mutation(state: &WebState, headers: &HeaderMap) -> ApiResult<String> {
    require_origin(state, headers)?;
    let (token, login) = authenticated(state, headers)?;
    let csrf = headers
        .get("x-bashkitten-csrf")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !auth::validate_csrf(&login, csrf) {
        return Err(ApiError(StatusCode::FORBIDDEN, "Invalid CSRF token".into()));
    }
    Ok(token)
}

fn login_response(state: &WebState, login: auth::NewLogin) -> ApiResult<Response> {
    let session = auth::validate(&state.paths, &login.token)?;
    state
        .csrf_tokens
        .write()
        .expect("CSRF token lock")
        .insert(session.token_hash, login.csrf.clone());
    let cookie = format!(
        "{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        login.token,
        login.expires_at - chrono::Utc::now().timestamp()
    );
    let mut response = Json(json!({"ok": true, "csrf": login.csrf})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("cookie"),
    );
    Ok(response)
}

async fn bootstrap(State(state): State<WebState>, headers: HeaderMap) -> Json<Bootstrap> {
    let has_user = auth::has_user(&state.paths);
    let csrf = cookie_token(&headers)
        .and_then(|token| auth::validate(&state.paths, token).ok())
        .and_then(|session| {
            state
                .csrf_tokens
                .read()
                .expect("CSRF token lock")
                .get(&session.token_hash)
                .cloned()
        });
    Json(Bootstrap {
        has_user,
        authenticated: csrf.is_some(),
        csrf,
        version: crate::VERSION,
    })
}

async fn signup(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> ApiResult<Response> {
    require_origin(&state, &headers)?;
    login_response(
        &state,
        auth::signup(&state.paths, &body.username, &body.password)?,
    )
}

async fn login(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> ApiResult<Response> {
    require_origin(&state, &headers)?;
    login_response(
        &state,
        auth::login(&state.paths, &body.username, &body.password)?,
    )
}

async fn logout(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Response> {
    let token = require_mutation(&state, &headers)?;
    let session = auth::validate(&state.paths, &token)?;
    auth::logout(&state.paths, &token)?;
    state
        .csrf_tokens
        .write()
        .expect("CSRF token lock")
        .remove(&session.token_hash);
    let mut response = Json(json!({"ok": true})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "bashkitten_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        ),
    );
    Ok(response)
}

fn codex_authenticated(paths: &AppPaths) -> bool {
    crate::paths::set_private_file(&paths.provider_auth_file())
        .ok()
        .and_then(|_| fs::read(paths.provider_auth_file()).ok())
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        .and_then(|v| v.get("openai-codex").cloned())
        .is_some()
}

fn llama_available() -> bool {
    crate::llama::detect_installation().is_some()
}

async fn list_models(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    let config = state.config.read().expect("config lock").clone();
    Ok(Json(
        json!({"models": models::all_models(&config, codex_authenticated(&state.paths), llama_available()), "defaultModel": config.default_model, "defaultThinking": config.default_thinking}),
    ))
}

async fn list_sessions(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    let sessions = session::list(&state.paths)?;
    let offset = page.offset.unwrap_or(0).min(sessions.len());
    let limit = page.limit.unwrap_or(50).clamp(1, 200);
    let end = (offset + limit).min(sessions.len());
    Ok(Json(
        json!({"sessions": &sessions[offset..end], "nextOffset": (end < sessions.len()).then_some(end)}),
    ))
}

#[derive(Default)]
struct MessageForm {
    prompt: String,
    cwd: String,
    model: String,
    thinking: String,
    parent: Option<String>,
    delivery: Option<String>,
    files: Vec<(String, Vec<u8>)>,
    retained_attachments: Vec<PathBuf>,
    goal: bool,
}

async fn parse_form(mut multipart: Multipart) -> ApiResult<MessageForm> {
    let mut form = MessageForm::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        let filename = field.file_name().map(safe_filename);
        if name == "file" {
            let bytes = field
                .bytes()
                .await
                .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
            form.files.push((
                filename.unwrap_or_else(|| "attachment".into()),
                bytes.to_vec(),
            ));
            continue;
        }
        let value = field
            .text()
            .await
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
        match name.as_str() {
            "prompt" | "content" => form.prompt = value,
            "cwd" => form.cwd = value,
            "model" => form.model = value,
            "thinking" => form.thinking = value,
            "parent" if !value.is_empty() => form.parent = Some(value),
            "delivery" => form.delivery = Some(value),
            "goal" => form.goal = value == "true",
            "retained_attachment" => form.retained_attachments.push(PathBuf::from(value)),
            _ => {}
        }
    }
    Ok(form)
}

fn safe_filename(name: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("attachment");
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "attachment".into()
    } else {
        clean
    }
}

fn save_attachments(
    paths: &AppPaths,
    id: &str,
    files: Vec<(String, Vec<u8>)>,
) -> Result<Vec<PathBuf>> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = paths.session_dir(id).join("attachments");
    let mut saved = Vec::new();
    for (name, bytes) in files {
        let upload_dir = dir.join(uuid::Uuid::new_v4().to_string());
        ensure_private_dir(&upload_dir)?;
        let path = upload_dir.join(name);
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        set_private_file(&path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        saved.push(path);
    }
    Ok(saved)
}

fn retained_attachments(
    paths: &AppPaths,
    id: &str,
    requested: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    if requested.is_empty() {
        return Ok(Vec::new());
    }
    session::validate_id(id)?;
    let root = fs::canonicalize(paths.session_dir(id).join("attachments"))?;
    crate::paths::set_private_dir(&paths.session_dir(id))?;
    crate::paths::set_private_dir(&root)?;
    requested
        .into_iter()
        .map(|requested| {
            let path = fs::canonicalize(&requested)
                .context("Retained attachment is no longer available")?;
            anyhow::ensure!(
                path.starts_with(&root) && path.is_file(),
                "Retained attachment is outside this session"
            );
            crate::paths::set_private_dir(path.parent().context("Attachment parent is missing")?)?;
            crate::paths::set_private_file(&path)?;
            Ok(path)
        })
        .collect()
}

async fn create_session(
    State(state): State<WebState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let mut form = parse_form(multipart).await?;
    let config = state.config.read().expect("config lock").clone();
    if form.cwd.is_empty() {
        form.cwd = config.default_cwd.to_string_lossy().into_owned();
    }
    let (model_info, thinking) = models::resolve_new_session(
        &config,
        (!form.model.is_empty()).then_some(form.model.as_str()),
        (!form.thinking.is_empty()).then_some(form.thinking.as_str()),
        codex_authenticated(&state.paths),
        llama_available(),
    )?;
    let request = NewSession {
        cwd: PathBuf::from(form.cwd),
        model: model_info.full_id(),
        thinking,
        model_parameters: model_info.parameters,
        prompt: form.prompt.clone(),
        attachments: Vec::new(),
        parent: form.parent,
    };
    let id = session::create(&state.paths, &request)?;
    // The Web client subscribes to the session event stream before it sends the
    // first message. This keeps the very first reasoning/tool/text event live
    // instead of racing the HTTP response that created the session.
    session::start_worker(&state.paths, &id)?;
    Ok(Json(json!({"ok": true, "id": id})))
}

fn logical_json(value: Value) -> Json<crate::lossless_json::Wire<Value>> {
    Json(crate::lossless_json::wire(value))
}

async fn read_segment(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath((id, segment)): AxumPath<(String, u32)>,
) -> ApiResult<Json<crate::lossless_json::Wire<Value>>> {
    authenticated(&state, &headers)?;
    Ok(logical_json(
        json!({"entries": session::read_segment(&state.paths, &id, segment)?}),
    ))
}

async fn read_current_segment(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<crate::lossless_json::Wire<Value>>> {
    authenticated(&state, &headers)?;
    session::validate_id(&id)?;
    let (segment, _) = session::current_segment(&state.paths.session_dir(&id))?;
    Ok(logical_json(json!({
        "segment": segment,
        "entries": session::read_segment(&state.paths, &id, segment)?
    })))
}

async fn send_message(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    multipart: Multipart,
) -> ApiResult<Json<crate::lossless_json::Wire<Value>>> {
    require_mutation(&state, &headers)?;
    let form = parse_form(multipart).await?;
    let socket = session::control_socket(&state.paths, &id)?;
    if !session::socket_is_live(&socket) {
        session::start_worker(&state.paths, &id)?;
    }
    let _guard = session::lock_session(&state.paths, &id)?;
    let mut attachments = retained_attachments(&state.paths, &id, form.retained_attachments)?;
    attachments.extend(save_attachments(&state.paths, &id, form.files)?);
    let delivery = if form.delivery.as_deref() == Some("steer") {
        Delivery::Steer
    } else {
        Delivery::Queue
    };
    let request = if form.goal {
        ControlRequest::GoalMessage {
            content: form.prompt,
            attachments,
        }
    } else {
        ControlRequest::Send {
            delivery,
            content: form.prompt,
            attachments,
            source_session: None,
        }
    };
    let reply = session::send(&state.paths, &id, &request)?;
    Ok(logical_json(serde_json::to_value(reply).expect("reply")))
}

async fn fork_session(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<ForkRequest>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let paths = state.paths.clone();
    let entry_id = body.entry_id;
    let fork_id = tokio::task::spawn_blocking(move || {
        let mut last_error = None;
        for attempt in 0..20 {
            match session::fork_at(&paths, &id, &entry_id) {
                Ok(id) => return Ok(id),
                Err(error)
                    if attempt < 19
                        && error
                            .to_string()
                            .contains("not yet available in session history") =>
                {
                    last_error = Some(error);
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.context("Fork message is not available")?)
    })
    .await
    .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))??;
    Ok(Json(json!({"ok": true, "id": fork_id})))
}

async fn download_attachment(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath((id, upload, name)): AxumPath<(String, String, String)>,
    Query(query): Query<AttachmentQuery>,
) -> ApiResult<Response> {
    authenticated(&state, &headers)?;
    session::validate_id(&id)?;
    uuid::Uuid::parse_str(&upload)
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "Invalid attachment ID".into()))?;
    if name != safe_filename(&name) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid attachment filename".into(),
        ));
    }

    let attachment_root = fs::canonicalize(state.paths.session_dir(&id).join("attachments"))
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "Attachment not found".into()))?;
    crate::paths::set_private_dir(&state.paths.session_dir(&id))?;
    crate::paths::set_private_dir(&attachment_root)?;
    let path = attachment_root.join(&upload).join(&name);
    let path = fs::canonicalize(path)
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "Attachment not found".into()))?;
    if !path.starts_with(&attachment_root) || !path.is_file() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "Attachment not found".into(),
        ));
    }

    crate::paths::set_private_dir(path.parent().context("Attachment parent is missing")?)?;
    crate::paths::set_private_file(&path)?;
    let bytes = fs::read(&path).with_context(|| format!("read attachment {}", path.display()))?;
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.essence_str()).expect("valid MIME type"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    if query.download.unwrap_or(false) {
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
                .expect("sanitized filename"),
        );
    }
    Ok(response)
}

async fn session_status(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<crate::lossless_json::Wire<Value>>> {
    authenticated(&state, &headers)?;
    let socket = session::control_socket(&state.paths, &id)?;
    if !session::socket_is_live(&socket) {
        let directory = state.paths.session_dir(&id);
        let header = session::read_header(&directory)?;
        let (model, thinking) = session::effective_model(&directory, &header);
        return Ok(logical_json(json!({
            "ok": true,
            "message": "offline",
            "data": {
                "busy": false,
                "compacting": false,
                "steering": 0,
                "queued": 0,
                "steeringMessages": [],
                "queuedMessages": [],
                "modelSelection": {"model": model, "thinking": thinking},
                "goal": crate::worker::saved_goal(&state.paths,&id)?,
                "usage": crate::worker::saved_usage(&state.paths, &id, &state.config.read().expect("config lock"))?
            }
        })));
    }
    let reply = session::send(&state.paths, &id, &ControlRequest::Status)?;
    Ok(logical_json(serde_json::to_value(reply).expect("reply")))
}

async fn update_session_goal(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(action): Json<crate::goal::Action>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let socket = session::control_socket(&state.paths, &id)?;
    if !session::socket_is_live(&socket) {
        session::start_worker(&state.paths, &id)?;
    }
    let reply = session::send(&state.paths, &id, &ControlRequest::Goal { action })?;
    Ok(Json(serde_json::to_value(reply).expect("control reply")))
}

#[derive(Deserialize)]
struct CompactBody {
    #[serde(default)]
    instructions: Option<String>,
}

async fn compact_session(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<CompactBody>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let socket = session::control_socket(&state.paths, &id)?;
    if !session::socket_is_live(&socket) {
        session::start_worker(&state.paths, &id)?;
    }
    let reply = session::send(
        &state.paths,
        &id,
        &ControlRequest::Compact {
            custom_instructions: body.instructions,
        },
    )?;
    Ok(Json(serde_json::to_value(reply).expect("reply")))
}

async fn mutate_queue(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<QueueMutation>,
) -> ApiResult<Json<crate::lossless_json::Wire<Value>>> {
    require_mutation(&state, &headers)?;
    let socket = session::control_socket(&state.paths, &id)?;
    if !session::socket_is_live(&socket) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Agent session is no longer running".into(),
        ));
    }
    let reply = session::send(
        &state.paths,
        &id,
        &ControlRequest::QueueAction {
            id: body.id,
            action: body.action,
            content: body.content,
            edit_token: body.edit_token,
        },
    )?;
    if !reply.ok {
        return Err(ApiError(StatusCode::CONFLICT, reply.message));
    }
    Ok(logical_json(serde_json::to_value(reply).expect("reply")))
}

async fn list_folders(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<FolderQuery>,
) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    let requested = query.path.unwrap_or_else(|| {
        state
            .config
            .read()
            .expect("config lock")
            .default_cwd
            .clone()
    });
    Ok(Json(folder_listing(&requested, query.nearest)?))
}

fn folder_listing(requested: &Path, nearest: bool) -> Result<Value> {
    if !requested.is_absolute() {
        anyhow::bail!("Folder path must be absolute");
    }
    let mut candidate = requested;
    let (current, entries) = loop {
        let opened = fs::canonicalize(candidate)
            .and_then(|path| fs::read_dir(&path).map(|entries| (path, entries)));
        match opened {
            Ok(value) => break value,
            Err(_) if nearest && candidate.parent().is_some() => {
                candidate = candidate.parent().unwrap();
            }
            Err(error) => {
                return Err(error).with_context(|| format!("open folder {}", requested.display()));
            }
        }
    };
    let mut folders = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| {
            json!({
                "name": entry.file_name().to_string_lossy(),
                "path": entry.path()
            })
        })
        .collect::<Vec<_>>();
    folders.sort_by_key(|entry| {
        entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase()
    });
    folders.truncate(500);
    Ok(json!({
        "path": current,
        "parent": current.parent(),
        "folders": folders
    }))
}

async fn create_folder(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<CreateFolder>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let name = body.name.trim();
    let mut components = Path::new(name).components();
    if name.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Folder name must be one ordinary path component".into(),
        ));
    }
    let parent = fs::canonicalize(&body.parent)
        .with_context(|| format!("open folder {}", body.parent.display()))?;
    if !parent.is_dir() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Parent is not a folder".into(),
        ));
    }
    let created = parent.join(name);
    fs::create_dir(&created).with_context(|| format!("create folder {}", created.display()))?;
    let created = fs::canonicalize(created).context("resolve newly created folder")?;
    Ok(Json(json!({"ok": true, "path": created})))
}

#[derive(Deserialize)]
struct RenameSession {
    name: String,
}
#[derive(Deserialize)]
struct DeleteSession {
    confirm: bool,
}
#[derive(Deserialize)]
struct ProjectQuery {
    cwd: PathBuf,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteProject {
    cwd: PathBuf,
    session_ids: Vec<String>,
    confirm: bool,
}
async fn preview_project_sessions(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(body): Query<ProjectQuery>,
) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    let ids =
        tokio::task::spawn_blocking(move || session::project_sessions(&state.paths, &body.cwd))
            .await
            .context("list project chats")??;
    Ok(Json(json!({"sessionIds":ids})))
}
async fn delete_project_sessions(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<DeleteProject>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    if !body.confirm {
        return Err(anyhow::anyhow!("Confirm project chat deletion first").into());
    }
    let ids = body.session_ids.clone();
    tokio::task::spawn_blocking(move || {
        session::delete_project(&state.paths, &body.cwd, &body.session_ids)
    })
    .await
    .context("wait for project chat deletion")??;
    Ok(Json(json!({"ok":true,"deletedIds":ids})))
}
async fn rename_session(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<RenameSession>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let title = tokio::task::spawn_blocking(move || session::rename(&state.paths, &id, &body.name))
        .await
        .context("wait for chat rename")??;
    Ok(Json(json!({"ok":true,"title":title})))
}
async fn delete_session(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<DeleteSession>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    if !body.confirm {
        return Err(anyhow::anyhow!("Confirm chat deletion first").into());
    }
    tokio::task::spawn_blocking(move || session::delete(&state.paths, &id))
        .await
        .context("wait for chat deletion")??;
    Ok(Json(json!({"ok":true})))
}

async fn stop_session(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<crate::lossless_json::Wire<Value>>> {
    require_mutation(&state, &headers)?;
    let pending = tokio::task::spawn_blocking(move || session::stop_worker(&state.paths, &id))
        .await
        .context("wait for session cancellation")??;
    Ok(logical_json(json!({"ok": true, "data": pending})))
}

async fn change_model(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<ModelChange>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let config = state.config.read().expect("config lock").clone();
    models::resolve_model(
        &config,
        &body.model,
        &body.thinking,
        codex_authenticated(&state.paths),
        llama_available(),
    )?;
    let socket = session::control_socket(&state.paths, &id)?;
    if !session::socket_is_live(&socket) {
        session::start_worker(&state.paths, &id)?;
    }
    let reply = session::send(
        &state.paths,
        &id,
        &ControlRequest::ChangeModel {
            model: body.model,
            thinking: body.thinking,
        },
    )?;
    Ok(Json(serde_json::to_value(reply).expect("reply")))
}

async fn change_cwd(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<FolderChange>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let cwd = session::validate_cwd(&body.cwd)?;
    session::validate_id(&id)?;
    session::read_header(&state.paths.session_dir(&id))?;
    let socket = session::control_socket(&state.paths, &id)?;
    if !session::socket_is_live(&socket) {
        session::start_worker(&state.paths, &id)?;
    }
    let reply = session::send(&state.paths, &id, &ControlRequest::ChangeCwd { cwd })?;
    Ok(Json(serde_json::to_value(reply).expect("reply")))
}

async fn events(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Response> {
    let (token, _) = authenticated(&state, &headers)?;
    let socket = session::control_socket(&state.paths, &id)?;
    let address = session::socket_address(&socket)?;
    let output = stream! {
        match UnixStream::connect(address.as_ref()).await {
            Ok(mut connection) => {
                let request = serde_json::to_vec(&ControlRequest::Subscribe).unwrap_or_default();
                if connection.write_all(&request).await.is_ok() && connection.write_all(b"\n").await.is_ok() {
                    let mut lines = BufReader::new(connection).lines();
                    let mut authentication_check = tokio::time::interval(Duration::from_secs(1));
                    loop {
                        let next = tokio::select! {
                            value = lines.next_line() => Some(value),
                            _ = authentication_check.tick() => None,
                        };
                        if auth::validate(&state.paths, &token).is_err() {
                            yield Ok(Event::default().event("offline").data("Authentication required"));
                            break;
                        }
                        match next {
                            Some(Ok(Some(line))) => yield Ok(Event::default().data(line)),
                            Some(_) => {yield Ok(Event::default().event("offline").data("Agent session finished"));break;}
                            None => {},
                        }
                    }
                }
            }
            Err(error) => yield Ok(Event::default().event("offline").data(json!({"message": error.to_string()}).to_string())),
        }
    };
    let output: Pin<Box<dyn Stream<Item = std::result::Result<Event, Infallible>> + Send>> =
        Box::pin(output);
    Ok(Sse::new(output)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}

async fn resume_session(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let socket = session::control_socket(&state.paths, &id)?;
    if !session::socket_is_live(&socket) {
        session::start_worker(&state.paths, &id)?;
    }
    Ok(Json(json!({"ok": true})))
}

// Cgroups identify candidate units; MainPID ensures we never restart a shell,
// controller, desktop application or another process that launched this server.
fn own_web_service() -> Option<String> {
    let cgroups = fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in cgroups.lines() {
        let path = line.splitn(3, ':').nth(2)?;
        for unit in path
            .split('/')
            .rev()
            .filter(|part| part.ends_with(".service"))
        {
            if unit.starts_with('-') {
                continue;
            }
            let output = std::process::Command::new("systemctl")
                .args(["--user", "show", "--property=MainPID", "--value", unit])
                .output()
                .ok()?;
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<u32>()
                    .ok()
                    == Some(std::process::id())
            {
                return Some(unit.to_owned());
            }
        }
    }
    None
}

async fn get_settings(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    let mut config = serde_json::to_value(state.config.read().expect("config lock").clone())
        .expect("serialize config");
    config.as_object_mut().unwrap().remove("web_port_fallback");
    config["llama"].as_object_mut().unwrap().remove("api_key");
    let extra: Vec<String> = serde_json::from_value(config["llama"]["extra_arguments"].clone())
        .expect("configured argument list");
    config["llama"]["extra_arguments"] = json!(crate::llama::redact_extra_arguments(&extra));
    for provider in config["compatible_providers"].as_array_mut().unwrap() {
        for auth in ["bearer", "header"] {
            if let Some(value) = provider["auth"]
                .get_mut(auth)
                .and_then(Value::as_object_mut)
            {
                value.remove("secret");
            }
        }
    }
    Ok(Json(
        json!({"config": config, "llamaInstalled": llama_available(), "codexAuthenticated": codex_authenticated(&state.paths), "restartError": *state.restart_error.read().expect("restart error lock")}),
    ))
}

async fn save_settings(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(mut config): Json<Value>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let mut current = state.config.write().expect("config lock");
    if state
        .restart_pending
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Web UI is restarting; retry after reconnecting".into(),
        ));
    }
    // A missing credential means preserve it. An explicitly supplied credential
    // replaces it (an empty string clears it); stored values never round-trip
    // through browser code. Match compatible credentials by provider ID and mode.
    let stored = serde_json::to_value(&*current).expect("serialize config");
    if let Some(llama) = config.get_mut("llama").and_then(Value::as_object_mut) {
        llama
            .entry("api_key")
            .or_insert_with(|| stored["llama"]["api_key"].clone());
    }
    if let Some(providers) = config
        .get_mut("compatible_providers")
        .and_then(Value::as_array_mut)
    {
        for provider in providers {
            let previous = stored["compatible_providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|old| old["id"] == provider["id"]);
            for mode in ["bearer", "header"] {
                if let Some(auth) = provider
                    .get_mut("auth")
                    .and_then(|auth| auth.get_mut(mode))
                    .and_then(Value::as_object_mut)
                {
                    auth.entry("secret").or_insert_with(|| {
                        previous
                            .and_then(|old| old["auth"][mode].get("secret"))
                            .cloned()
                            .unwrap_or_else(|| json!(""))
                    });
                }
            }
        }
    }
    let mut config: AppConfig = serde_json::from_value(config).map_err(anyhow::Error::from)?;
    crate::llama::restore_extra_arguments(
        &mut config.llama.extra_arguments,
        &current.llama.extra_arguments,
    )?;
    // Catalog updates belong to the router path and may race a settings form.
    // Never overwrite fresh metadata with the form's older snapshot.
    if config.llama.port == current.llama.port {
        config.llama.catalog = current.llama.catalog.clone();
        config.llama.router_autoload = current.llama.router_autoload;
    } else {
        config.llama.catalog.clear();
        config.llama.router_autoload = false;
    }
    config
        .validate_models()
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    crate::llama::launch_arguments(&config.llama, false, true)?;
    if config.web_port == 0 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Web port must be between 1 and 65535".into(),
        ));
    }
    let restart = if config.web_port != state.bound_port {
        let probe = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, config.web_port))
            .map_err(|_| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("Web port {} is unavailable", config.web_port),
            )
        })?;
        drop(probe);
        let unit = own_web_service().ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "Port changes require the Web UI to run as its own systemd user service".into(),
            )
        })?;
        Some((unit, format!("http://127.0.0.1:{}", config.web_port)))
    } else {
        None
    };
    config.web_port_fallback = restart.as_ref().map(|_| state.bound_port);
    config.save(&state.paths)?;
    *current = config;
    *state.restart_error.write().expect("restart error lock") = None;
    if let Some((unit, url)) = restart {
        state
            .restart_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let restart_state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let result = tokio::process::Command::new("systemctl")
                .args(["--user", "--no-block", "restart", &unit])
                .status()
                .await;
            if !result.is_ok_and(|status| status.success()) {
                recover_web_restart(&restart_state);
            }
        });
        return Ok(Json(
            json!({"ok":true,"restartRequired":true,"restartUrl":url}),
        ));
    }
    Ok(Json(json!({"ok": true, "restartRequired": false})))
}

fn recover_web_restart(state: &WebState) {
    let mut current = state.config.write().expect("config lock");
    let mut restored = current.clone();
    // Other explicitly saved settings and concurrent router catalog updates
    // remain valid; only the unapplied listener port needs restoration.
    restored.web_port = state.bound_port;
    restored.web_port_fallback = None;
    let message = match restored.save(&state.paths) {
        Ok(()) => {
            *current = restored;
            "Could not restart Web UI; the previous port has been restored".to_owned()
        }
        Err(error) => format!("Could not restart Web UI or restore its previous port: {error}"),
    };
    *state.restart_error.write().expect("restart error lock") = Some(message);
    state
        .restart_pending
        .store(false, std::sync::atomic::Ordering::SeqCst);
}

async fn provider_login_status(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let (_, login) = authenticated(&state, &headers)?;
    Ok(Json(
        json!({"authenticated":codex_authenticated(&state.paths),"login":state.oauth.status(&login.token_hash).await}),
    ))
}

async fn provider_login(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<OAuthStart>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let (_, login) = authenticated(&state, &headers)?;
    let status = state
        .oauth
        .start(
            login.token_hash,
            &body.method,
            crate::providers::ProviderAuthStore::for_paths(&state.paths),
        )
        .await?;
    let browser_opened = if body.method == "browser" {
        status
            .url
            .as_deref()
            .map(|url| crate::oauth::open_browser(url).is_ok())
    } else {
        None
    };
    Ok(Json(
        json!({"ok":true,"login":status,"browserOpened":browser_opened}),
    ))
}

async fn provider_login_code(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<OAuthCode>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let (_, login) = authenticated(&state, &headers)?;
    state.oauth.submit(&login.token_hash, body.input).await?;
    Ok(Json(json!({"ok":true})))
}

async fn provider_login_cancel(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let (_, login) = authenticated(&state, &headers)?;
    state.oauth.cancel(&login.token_hash).await?;
    Ok(Json(json!({"ok":true})))
}

async fn provider_logout(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    state
        .oauth
        .logout(crate::providers::ProviderAuthStore::for_paths(&state.paths))
        .await?;
    Ok(Json(json!({"ok":true})))
}

async fn restart_llama(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let status = std::process::Command::new("systemctl")
        .args(["--user", "restart", "bashkitten-llama.service"])
        .status()
        .context("run systemctl")?;
    if !status.success() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Could not restart llama.cpp service".into(),
        ));
    }
    Ok(Json(json!({"ok": true})))
}

async fn bind_web_listener(
    paths: &AppPaths,
    mut config: AppConfig,
) -> Result<(tokio::net::TcpListener, AppConfig, Option<String>)> {
    let address = format!("127.0.0.1:{}", config.web_port);
    let (listener, failure) = match tokio::net::TcpListener::bind(&address).await {
        Ok(listener) => (listener, None),
        Err(error) => {
            let Some(previous) = config
                .web_port_fallback
                .filter(|port| *port != config.web_port)
            else {
                return Err(error).with_context(|| format!("bind {address}"));
            };
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, previous))
                .await
                .with_context(|| {
                    format!("bind {address} failed ({error}); could not restore port {previous}")
                })?;
            config.web_port = previous;
            (
                listener,
                Some(format!(
                    "Could not bind {address}; the previous port {previous} has been restored"
                )),
            )
        }
    };
    if config.web_port_fallback.take().is_some() {
        config.save(paths)?;
    }
    Ok((listener, config, failure))
}

pub async fn serve(paths: AppPaths, config: AppConfig) -> Result<()> {
    let (listener, config, failure) = bind_web_listener(&paths, config).await?;
    axum::serve(listener, router_with_restart_error(paths, config, failure)?).await?;
    Ok(())
}

const INDEX_HTML: &str = include_str!("web_ui.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn chat_management_requires_confirmation_login_origin_and_csrf() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = AppConfig {
            web_port: port,
            ..Default::default()
        };
        config.save(&paths).unwrap();
        let login = auth::signup(&paths, "fixture", "fixture-password").unwrap();
        let id = session::create(
            &paths,
            &NewSession {
                cwd: root.path().into(),
                model: "p/m".into(),
                thinking: "off".into(),
                model_parameters: json!({}),
                prompt: "Rename me".into(),
                attachments: vec![],
                parent: None,
            },
        )
        .unwrap();
        let app = router(paths.clone(), config).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = reqwest::Client::new();
        let origin = format!("http://127.0.0.1:{port}");
        let url = format!("{origin}/api/sessions/{id}");
        let cookie = format!("{COOKIE}={}", login.token);
        for method in [reqwest::Method::DELETE, reqwest::Method::PATCH] {
            let body = if method == reqwest::Method::DELETE {
                json!({"confirm":true})
            } else {
                json!({"name":"Renamed"})
            };
            assert_eq!(
                http.request(method.clone(), &url)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                http.request(method.clone(), &url)
                    .header(header::COOKIE, &cookie)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                http.request(method, &url)
                    .header(header::COOKIE, &cookie)
                    .header(header::ORIGIN, "http://other.invalid")
                    .header("x-bashkitten-csrf", &login.csrf)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        let authorized = |method| {
            http.request(method, &url)
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, &origin)
                .header("x-bashkitten-csrf", &login.csrf)
        };
        assert!(
            authorized(reqwest::Method::PATCH)
                .json(&json!({"name":"Renamed 🐈"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert_eq!(
            fs::read_to_string(paths.session_dir(&id).join("title")).unwrap(),
            "Renamed 🐈\n"
        );
        assert_eq!(
            authorized(reqwest::Method::DELETE)
                .json(&json!({"confirm":false}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert!(paths.session_dir(&id).exists());
        assert!(
            authorized(reqwest::Method::DELETE)
                .json(&json!({"confirm":true}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert!(!paths.session_dir(&id).exists());
        assert!(root.path().exists());
        // Project preview must include chats beyond the sidebar's 100-row page.
        let mut project_ids = Vec::new();
        for _ in 0..103 {
            project_ids.push(
                session::create(
                    &paths,
                    &NewSession {
                        cwd: root.path().into(),
                        model: "p/m".into(),
                        thinking: "off".into(),
                        model_parameters: json!({}),
                        prompt: "Project fixture".into(),
                        attachments: vec![],
                        parent: None,
                    },
                )
                .unwrap(),
            );
        }
        project_ids.sort();
        let group_url = format!("{origin}/api/session-groups");
        assert_eq!(
            http.get(&group_url)
                .query(&[("cwd", root.path())])
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let preview: Value = http
            .get(&group_url)
            .query(&[("cwd", root.path())])
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(preview["sessionIds"], json!(project_ids));
        let body = json!({"cwd":root.path(),"sessionIds":project_ids,"confirm":true});
        for request in [
            http.delete(&group_url),
            http.delete(&group_url)
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, &origin),
            http.delete(&group_url)
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, "http://other.invalid")
                .header("x-bashkitten-csrf", &login.csrf),
        ] {
            assert_eq!(
                request.json(&body).send().await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
        let authorized_group = || {
            http.delete(&group_url)
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, &origin)
                .header("x-bashkitten-csrf", &login.csrf)
        };
        let mut unconfirmed = body.clone();
        unconfirmed["confirm"] = json!(false);
        assert_eq!(
            authorized_group()
                .json(&unconfirmed)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let mut stale = body.clone();
        stale["sessionIds"] = json!([project_ids[0]]);
        assert_eq!(
            authorized_group()
                .json(&stale)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert!(project_ids.iter().all(|id| paths.session_dir(id).is_dir()));
        let deleted: Value = authorized_group()
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(deleted["deletedIds"], json!(project_ids));
        assert!(project_ids.iter().all(|id| !paths.session_dir(id).exists()));
        assert!(root.path().is_dir());
        server.abort();
    }

    #[test]
    fn recovered_attachments_stay_inside_the_original_session() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        ensure_private_dir(&paths.session_dir(&id).join("attachments")).unwrap();
        let saved = save_attachments(
            &paths,
            &id,
            vec![("original.txt".into(), b"unchanged attachment".to_vec())],
        )
        .unwrap();
        fs::set_permissions(&saved[0], fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            retained_attachments(&paths, &id, saved.clone()).unwrap(),
            saved
        );
        assert_eq!(fs::read(&saved[0]).unwrap(), b"unchanged attachment");
        assert_eq!(
            fs::metadata(&saved[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let outside = root.path().join("not-an-attachment");
        fs::write(&outside, "private unrelated file").unwrap();
        assert!(retained_attachments(&paths, &id, vec![outside.clone()]).is_err());
        let link = saved[0].parent().unwrap().join("escape.txt");
        symlink(&outside, &link).unwrap();
        assert!(retained_attachments(&paths, &id, vec![link]).is_err());
        let other = uuid::Uuid::now_v7().to_string();
        ensure_private_dir(&paths.session_dir(&other).join("attachments")).unwrap();
        assert!(retained_attachments(&paths, &other, saved).is_err());
    }

    #[tokio::test]
    async fn offline_stream_is_read_only_and_resume_requires_origin_and_csrf() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = AppConfig {
            web_port: port,
            ..Default::default()
        };
        config.save(&paths).unwrap();
        let login = auth::signup(&paths, "test", "fixture-password").unwrap();
        let id = session::create(
            &paths,
            &NewSession {
                cwd: root.path().to_owned(),
                model: config.default_model.clone(),
                thinking: config.default_thinking.clone(),
                model_parameters: Value::Null,
                prompt: "offline history".into(),
                attachments: vec![],
                parent: None,
            },
        )
        .unwrap();
        let history = fs::read(paths.session_dir(&id).join("000001.jsonl")).unwrap();
        let app = router(paths.clone(), config).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let origin = format!("http://127.0.0.1:{port}");
        let cookie = format!("{COOKIE}={}", login.token);
        let url = format!("{origin}/api/sessions/{id}");
        assert_eq!(
            client
                .get(format!("{url}/events"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let stream = client
            .get(format!("{url}/events"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(stream.status(), StatusCode::OK);
        let body = tokio::time::timeout(Duration::from_secs(2), stream.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("event: offline"), "{body}");
        for (supplied_origin, csrf) in [
            (&origin[..], "wrong"),
            ("http://attacker.invalid", &login.csrf),
        ] {
            assert_eq!(
                client
                    .post(format!("{url}/resume"))
                    .header("cookie", &cookie)
                    .header("origin", supplied_origin)
                    .header("x-bashkitten-csrf", csrf)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            client
                .get(format!("{url}/resume"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert!(!paths.session_dir(&id).join("control.sock").exists());
        assert_eq!(
            fs::read(paths.session_dir(&id).join("000001.jsonl")).unwrap(),
            history
        );
        server.abort();
    }

    #[tokio::test]
    async fn logout_revokes_an_already_open_event_stream() {
        use futures_util::StreamExt;
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = http.local_addr().unwrap().port();
        let config = AppConfig {
            web_port: port,
            ..Default::default()
        };
        let login = auth::signup(&paths, "test", "fixture-password").unwrap();
        let id = session::create(
            &paths,
            &NewSession {
                cwd: root.path().to_owned(),
                model: config.default_model.clone(),
                thinking: config.default_thinking.clone(),
                model_parameters: Value::Null,
                prompt: "stream".into(),
                attachments: vec![],
                parent: None,
            },
        )
        .unwrap();
        let address =
            session::socket_address(&session::control_socket(&paths, &id).unwrap()).unwrap();
        let control = tokio::net::UnixListener::bind(address.as_ref()).unwrap();
        let (send_after_logout, after_logout) = tokio::sync::oneshot::channel::<()>();
        let fake_worker = tokio::spawn(async move {
            let (mut connection, _) = control.accept().await.unwrap();
            use tokio::io::AsyncReadExt;
            let mut byte = [0];
            while connection.read_exact(&mut byte).await.is_ok() && byte[0] != b'\n' {}
            connection
                .write_all(b"{\"ok\":true,\"message\":\"subscribed\",\"data\":{}}\n")
                .await
                .unwrap();
            let _ = after_logout.await;
            let _ = connection
                .write_all(b"{\"type\":\"assistant_delta\",\"delta\":\"private-after-logout\"}\n")
                .await;
        });
        let app = router(paths.clone(), config).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(http, app).await.unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let origin = format!("http://127.0.0.1:{port}");
        let cookie = format!("{COOKIE}={}", login.token);
        let mut body = client
            .get(format!("{origin}/api/sessions/{id}/events"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .bytes_stream();
        let first = body.next().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&first).contains("subscribed"));
        let response = client
            .post(format!("{origin}/api/logout"))
            .header("cookie", &cookie)
            .header("origin", &origin)
            .header("x-bashkitten-csrf", &login.csrf)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        send_after_logout.send(()).unwrap();
        let rest = tokio::time::timeout(Duration::from_secs(2), async move {
            let mut rest = Vec::new();
            while let Some(chunk) = body.next().await {
                rest.extend_from_slice(&chunk.unwrap());
            }
            String::from_utf8(rest).unwrap()
        })
        .await
        .unwrap();
        assert!(rest.contains("Authentication required"), "{rest}");
        assert!(!rest.contains("private-after-logout"));
        fake_worker.await.unwrap();
        server.abort();
    }

    #[test]
    fn failed_web_restart_restores_port_and_preserves_other_saved_settings() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let config = AppConfig {
            web_port: 4888,
            theme: crate::config::UiTheme::Light,
            ..Default::default()
        };
        config.save(&paths).unwrap();
        let state = WebState {
            paths: paths.clone(),
            config: Arc::new(RwLock::new(config)),
            bound_port: 3939,
            restart_pending: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            restart_error: Default::default(),
            csrf_tokens: Default::default(),
            oauth: Default::default(),
            llama_operation: Default::default(),
            downloads: Default::default(),
        };
        recover_web_restart(&state);
        assert_eq!(AppConfig::load(&paths).unwrap().web_port, 3939);
        assert!(matches!(
            AppConfig::load(&paths).unwrap().theme,
            crate::config::UiTheme::Light
        ));
        assert_eq!(state.config.read().unwrap().web_port, 3939);
        assert!(
            !state
                .restart_pending
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(
            state
                .restart_error
                .read()
                .unwrap()
                .as_deref()
                .unwrap()
                .contains("restored")
        );
    }

    #[tokio::test]
    async fn new_web_process_recovers_a_port_taken_after_restart_preflight() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let stolen = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let previous = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let old_port = previous.local_addr().unwrap().port();
        drop(previous);
        let config = AppConfig {
            web_port: stolen.local_addr().unwrap().port(),
            web_port_fallback: Some(old_port),
            ..Default::default()
        };
        config.save(&paths).unwrap();
        let (listener, restored, failure) = bind_web_listener(&paths, config).await.unwrap();
        assert_eq!(listener.local_addr().unwrap().port(), old_port);
        assert_eq!(restored.web_port, old_port);
        assert!(failure.unwrap().contains("restored"));
        assert_eq!(AppConfig::load(&paths).unwrap().web_port, old_port);
        assert!(AppConfig::load(&paths).unwrap().web_port_fallback.is_none());
    }

    #[tokio::test]
    async fn bootstrap_is_read_only_and_open_tabs_work_across_web_restarts() {
        async fn start(paths: &AppPaths) -> (String, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let app = router(
                paths.clone(),
                AppConfig {
                    web_port: port,
                    ..Default::default()
                },
            )
            .unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            (format!("http://127.0.0.1:{port}"), server)
        }
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let (origin, server) = start(&paths).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let signup = client
            .post(format!("{origin}/api/signup"))
            .header("origin", &origin)
            .json(&json!({"username":"test","password":"fixture-password"}))
            .send()
            .await
            .unwrap();
        assert_eq!(signup.status(), StatusCode::OK);
        let cookie = signup.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let login: Value = signup.json().await.unwrap();
        let before = fs::read(paths.web_auth_file()).unwrap();
        let tabs = futures_util::future::join_all((0..8).map(|_| async {
            client
                .get(format!("{origin}/api/bootstrap"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }))
        .await;
        assert!(
            tabs.iter()
                .all(|tab| tab["authenticated"] == true && tab["csrf"] == login["csrf"])
        );
        assert_eq!(fs::read(paths.web_auth_file()).unwrap(), before);
        server.abort();
        server.await.unwrap_err();
        let (origin, restarted) = start(&paths).await;
        let before = fs::read(paths.web_auth_file()).unwrap();
        let bootstrap: Value = client
            .get(format!("{origin}/api/bootstrap"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(bootstrap["authenticated"], true);
        assert_ne!(bootstrap["csrf"], login["csrf"]);
        assert_eq!(fs::read(paths.web_auth_file()).unwrap(), before);
        for (name, token) in [("old-tab", &login["csrf"]), ("new-tab", &bootstrap["csrf"])] {
            let response = client
                .post(format!("{origin}/api/folders"))
                .header("cookie", &cookie)
                .header("origin", &origin)
                .header("x-bashkitten-csrf", token.as_str().unwrap())
                .json(&json!({"parent":root.path(), "name":name}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(root.path().join(name).is_dir());
        }
        for (name, request_origin, token) in [
            ("wrong-token", origin.as_str(), "wrong"),
            (
                "wrong-origin",
                "http://127.0.0.1:1",
                login["csrf"].as_str().unwrap(),
            ),
        ] {
            let response = client
                .post(format!("{origin}/api/folders"))
                .header("cookie", &cookie)
                .header("origin", request_origin)
                .header("x-bashkitten-csrf", token)
                .json(&json!({"parent":root.path(), "name":name}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(!root.path().join(name).exists());
        }
        restarted.abort();
    }

    #[test]
    fn shared_folder_picker_opens_nearest_parent_but_explicit_paths_stay_strict() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("existing child");
        fs::create_dir(&child).unwrap();
        fs::write(root.path().join("ordinary-file"), "fixture").unwrap();
        let missing = child.join("missing/deeper");
        let listing = folder_listing(&missing, true).unwrap();
        assert_eq!(listing["path"], child.to_str().unwrap());
        assert_eq!(listing["parent"], root.path().to_str().unwrap());
        assert!(!missing.exists());
        assert!(folder_listing(&missing, false).is_err());
        assert!(folder_listing(Path::new("relative/path"), true).is_err());
        let parent = folder_listing(root.path(), false).unwrap();
        assert_eq!(parent["folders"].as_array().unwrap().len(), 1);
        assert_eq!(parent["folders"][0]["name"], "existing child");
    }

    #[tokio::test]
    async fn history_opens_latest_segment_and_sidebar_paginates_without_parsing_messages() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: temp.path().join("config"),
            data: temp.path().join("data"),
            runtime: temp.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let mut ids = Vec::new();
        for index in 0..3 {
            let id = session::create(
                &paths,
                &NewSession {
                    cwd: temp.path().to_owned(),
                    model: "fixture/model".into(),
                    thinking: "off".into(),
                    model_parameters: json!({}),
                    prompt: format!("Page {index}"),
                    attachments: vec![],
                    parent: None,
                },
            )
            .unwrap();
            ids.push(id);
        }
        let dir = paths.session_dir(&ids[0]);
        let header = session::read_header(&dir).unwrap();
        session::rotate_compaction(&paths, &header, &[]).unwrap();
        // Listing is header/title/stat-only, even when a message body is damaged.
        // Opening that specific damaged history still reports the read failure.
        let damaged = paths.session_dir(&ids[1]).join("000001.jsonl");
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(damaged)
            .unwrap()
            .write_all(b"invalid history\n")
            .unwrap();
        let login = auth::signup(&paths, "fixture", "fixture-password").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = router(
            paths,
            AppConfig {
                web_port: port,
                ..Default::default()
            },
        )
        .unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let origin = format!("http://127.0.0.1:{port}");
        let cookie = format!("{COOKIE}={}", login.token);
        let endpoint = format!("{origin}/api/sessions/{}/segments/current", ids[0]);
        assert_eq!(
            client.get(&endpoint).send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let current: Value = client
            .get(endpoint)
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(current["segment"], 2);
        assert_eq!(current["entries"].as_array().unwrap().len(), 1);
        let mut listed = Vec::new();
        for offset in [0, 2] {
            let page: Value = client
                .get(format!("{origin}/api/sessions?limit=2&offset={offset}"))
                .header(header::COOKIE, &cookie)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(
                page["nextOffset"],
                if offset == 0 { json!(2) } else { Value::Null }
            );
            listed.extend(
                page["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["id"].as_str().unwrap().to_owned()),
            );
        }
        listed.sort();
        ids.sort();
        assert_eq!(listed, ids);
        server.abort();
    }

    #[tokio::test]
    async fn settings_never_return_credentials_and_saving_preserves_or_replaces_them() {
        use crate::config::{CompatibleAuth, CompatibleProvider};
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut config = AppConfig {
            web_port: port,
            ..Default::default()
        };
        config.llama.api_key = "fixture-router-secret".into();
        config.llama.extra_arguments = vec![
            "--hf-token".into(),
            "fixture-hf-secret".into(),
            "--api-key=fixture-advanced-secret".into(),
            "--verbose".into(),
        ];
        config.compatible_providers.push(CompatibleProvider {
            id: "local".into(),
            name: "Local".into(),
            base_url: "http://127.0.0.1:9876/v1".into(),
            auth: CompatibleAuth::Bearer {
                secret: "fixture-provider-secret".into(),
            },
            models: vec![],
        });
        config.save(&paths).unwrap();
        let login = auth::signup(&paths, "tester", "test-password-123").unwrap();
        let app = router(paths.clone(), config).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let origin = format!("http://127.0.0.1:{port}");
        let cookie = format!("{COOKIE}={}", login.token);
        let settings = client
            .get(format!("{origin}/api/settings"))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            !settings.contains("fixture-router-secret")
                && !settings.contains("fixture-provider-secret")
                && !settings.contains("fixture-hf-secret")
                && !settings.contains("fixture-advanced-secret")
        );
        let mut config = serde_json::from_str::<Value>(&settings).unwrap()["config"].clone();
        for replace in [false, true] {
            if replace {
                config["llama"]["api_key"] = json!("");
                config["compatible_providers"][0]["auth"]["bearer"]["secret"] =
                    json!("replacement-secret");
                config["llama"]["extra_arguments"] = json!([
                    "--verbose",
                    "--hf-token",
                    "replacement-hf-secret",
                    "--api-key=<stored secret>"
                ]);
            }
            let response = client
                .post(format!("{origin}/api/settings"))
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, &origin)
                .header("x-bashkitten-csrf", &login.csrf)
                .json(&config)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let saved = AppConfig::load(&paths).unwrap();
            assert!(saved.llama.extra_arguments.iter().any(|v| v
                == if replace {
                    "replacement-hf-secret"
                } else {
                    "fixture-hf-secret"
                }));
            assert!(
                saved
                    .llama
                    .extra_arguments
                    .iter()
                    .any(|v| v == "--api-key=fixture-advanced-secret")
            );
            assert!(
                !saved
                    .llama
                    .extra_arguments
                    .iter()
                    .any(|v| v.contains("<stored secret>"))
            );
            assert_eq!(
                saved.llama.api_key,
                if replace { "" } else { "fixture-router-secret" }
            );
            let CompatibleAuth::Bearer { secret } = &saved.compatible_providers[0].auth else {
                panic!("missing credential");
            };
            assert_eq!(
                secret,
                if replace {
                    "replacement-secret"
                } else {
                    "fixture-provider-secret"
                }
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn subscription_routes_require_auth_origin_csrf_and_logout_removes_only_provider_login() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: temp.path().join("c"),
            data: temp.path().join("d"),
            runtime: temp.path().join("r"),
        };
        paths.ensure().unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = AppConfig {
            web_port: port,
            ..Default::default()
        };
        let login = auth::signup(&paths, "test", "test-password-123").unwrap();
        let store = crate::providers::ProviderAuthStore::for_paths(&paths);
        store.set_codex(Some(serde_json::from_value(json!({"type":"oauth","access":"test-access","refresh":"test-refresh","expires":0})).unwrap())).await.unwrap();
        let app = router(paths.clone(), config).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let origin = format!("http://127.0.0.1:{port}");
        let cookie = format!("{COOKIE}={}", login.token);
        let status = client
            .get(format!("{origin}/api/provider/login"))
            .send()
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::UNAUTHORIZED);
        for endpoint in [
            "/api/provider/login",
            "/api/provider/login/code",
            "/api/provider/login/cancel",
            "/api/provider/logout",
        ] {
            let blocked = client
                .post(format!("{origin}{endpoint}"))
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, &origin)
                .json(&json!({"method":"browser","input":"code"}))
                .send()
                .await
                .unwrap();
            assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
        }
        let wrong_origin = client
            .post(format!("{origin}/api/provider/logout"))
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, "https://example.com")
            .header("x-bashkitten-csrf", &login.csrf)
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_origin.status(), StatusCode::FORBIDDEN);
        let status = client
            .get(format!("{origin}/api/provider/login"))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(status.headers()[header::CACHE_CONTROL], "no-store");
        let body = status.text().await.unwrap();
        assert!(!body.contains("test-access") && !body.contains("test-refresh"));
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["authenticated"],
            true
        );
        let removed = client
            .post(format!("{origin}/api/provider/import-pi"))
            .send()
            .await
            .unwrap();
        assert_eq!(removed.status(), StatusCode::NOT_FOUND);
        let response = client
            .post(format!("{origin}/api/provider/logout"))
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, &origin)
            .header("x-bashkitten-csrf", &login.csrf)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(store.credential("openai-codex").unwrap().is_none());
        assert!(auth::validate(&paths, &login.token).is_ok());
        server.abort();
    }
}
