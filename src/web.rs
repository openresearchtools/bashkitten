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
use axum::routing::{get, post};
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

#[derive(Clone)]
pub struct WebState {
    pub paths: AppPaths,
    pub config: Arc<RwLock<AppConfig>>,
    pub oauth: crate::oauth::LoginManager,
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

pub fn router(paths: AppPaths, config: AppConfig) -> Router {
    let state = WebState {
        paths,
        config: Arc::new(RwLock::new(config)),
        oauth: crate::oauth::LoginManager::default(),
    };
    Router::new()
        .route("/", get(index))
        .route("/api/bootstrap", get(bootstrap))
        .route("/api/signup", post(signup))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/models", get(list_models))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}/segments/{segment}", get(read_segment))
        .route("/api/sessions/{id}/events", get(events))
        .route("/api/sessions/{id}/messages", post(send_message))
        .route("/api/sessions/{id}/fork", post(fork_session))
        .route(
            "/api/sessions/{id}/attachments/{upload}/{name}",
            get(download_attachment),
        )
        .route("/api/sessions/{id}/status", get(session_status))
        .route("/api/sessions/{id}/queue", post(mutate_queue))
        .route("/api/sessions/{id}/stop", post(stop_session))
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
        .with_state(state)
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
    format!(
        "http://127.0.0.1:{}",
        state.config.read().expect("config lock").web_port
    )
}

fn require_origin(state: &WebState, headers: &HeaderMap) -> ApiResult<()> {
    let supplied = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if supplied != configured_origin(state) {
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

fn login_response(login: auth::NewLogin) -> Response {
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
    response
}

async fn bootstrap(State(state): State<WebState>, headers: HeaderMap) -> Json<Bootstrap> {
    let has_user = auth::has_user(&state.paths);
    let csrf = cookie_token(&headers)
        .and_then(|token| {
            auth::validate(&state.paths, token)
                .ok()
                .map(|_| token.to_owned())
        })
        .and_then(|token| auth::rotate_csrf(&state.paths, &token).ok());
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
    Ok(login_response(auth::signup(
        &state.paths,
        &body.username,
        &body.password,
    )?))
}

async fn login(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> ApiResult<Response> {
    require_origin(&state, &headers)?;
    Ok(login_response(auth::login(
        &state.paths,
        &body.username,
        &body.password,
    )?))
}

async fn logout(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Response> {
    let token = require_mutation(&state, &headers)?;
    auth::logout(&state.paths, &token)?;
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
    fs::read(paths.provider_auth_file())
        .ok()
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        .and_then(|v| v.get("openai-codex").cloned())
        .is_some()
}

fn llama_available() -> bool {
    std::path::Path::new("/usr/bin/llama-server").is_file()
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
    let dir = paths.session_dir(id).join("attachments");
    let mut saved = Vec::new();
    for (name, bytes) in files {
        let upload_dir = dir.join(uuid::Uuid::new_v4().to_string());
        ensure_private_dir(&upload_dir)?;
        let path = upload_dir.join(name);
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        set_private_file(&path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        saved.push(path);
    }
    Ok(saved)
}

async fn create_session(
    State(state): State<WebState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let mut form = parse_form(multipart).await?;
    let config = state.config.read().expect("config lock").clone();
    if form.model.is_empty() {
        form.model = config.default_model.clone();
    }
    if form.thinking.is_empty() {
        form.thinking = config.default_thinking.clone();
    }
    if form.cwd.is_empty() {
        form.cwd = config.default_cwd.to_string_lossy().into_owned();
    }
    let model = models::find_model(
        &config,
        &form.model,
        codex_authenticated(&state.paths),
        llama_available(),
    )
    .ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "Unknown or unavailable model".into(),
        )
    })?;
    if !model.thinking_levels.iter().any(|v| v == &form.thinking) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Thinking level is not supported by this model".into(),
        ));
    }
    let request = NewSession {
        cwd: PathBuf::from(form.cwd),
        model: form.model,
        thinking: form.thinking,
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

async fn read_segment(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath((id, segment)): AxumPath<(String, u32)>,
) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    Ok(Json(
        json!({"entries": session::read_segment(&state.paths, &id, segment)?}),
    ))
}

async fn send_message(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    multipart: Multipart,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let form = parse_form(multipart).await?;
    let socket = session::control_socket(&state.paths, &id)?;
    if !session::socket_is_live(&socket) {
        session::start_worker(&state.paths, &id)?;
    }
    let attachments = save_attachments(&state.paths, &id, form.files)?;
    let delivery = if form.delivery.as_deref() == Some("steer") {
        Delivery::Steer
    } else {
        Delivery::Queue
    };
    let reply = session::send(
        &state.paths,
        &id,
        &ControlRequest::Send {
            delivery,
            content: form.prompt,
            attachments,
            source_session: None,
        },
    )?;
    Ok(Json(serde_json::to_value(reply).expect("reply")))
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
    let path = attachment_root.join(&upload).join(&name);
    let path = fs::canonicalize(path)
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "Attachment not found".into()))?;
    if !path.starts_with(&attachment_root) || !path.is_file() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "Attachment not found".into(),
        ));
    }

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
) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    let socket = session::control_socket(&state.paths, &id)?;
    if !session::socket_is_live(&socket) {
        return Ok(Json(json!({
            "ok": true,
            "message": "offline",
            "data": {
                "busy": false,
                "steering": 0,
                "queued": 0,
                "steeringMessages": [],
                "queuedMessages": []
            }
        })));
    }
    let reply = session::send(&state.paths, &id, &ControlRequest::Status)?;
    Ok(Json(serde_json::to_value(reply).expect("reply")))
}

async fn mutate_queue(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<QueueMutation>,
) -> ApiResult<Json<Value>> {
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
        },
    )?;
    Ok(Json(serde_json::to_value(reply).expect("reply")))
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
    let current = fs::canonicalize(&requested)
        .with_context(|| format!("open folder {}", requested.display()))?;
    if !current.is_dir() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("Not a folder: {}", current.display()),
        ));
    }
    let mut folders = fs::read_dir(&current)
        .with_context(|| format!("read folder {}", current.display()))?
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
    Ok(Json(json!({
        "path": current,
        "parent": current.parent(),
        "folders": folders
    })))
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

async fn stop_session(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    session::stop_worker(&id)?;
    Ok(Json(json!({"ok": true})))
}

async fn change_model(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<ModelChange>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let config = state.config.read().expect("config lock").clone();
    let model = models::find_model(
        &config,
        &body.model,
        codex_authenticated(&state.paths),
        llama_available(),
    )
    .ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "Unknown or unavailable model".into(),
        )
    })?;
    if !model.thinking_levels.iter().any(|v| v == &body.thinking) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Thinking level is not supported by this model".into(),
        ));
    }
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
    authenticated(&state, &headers)?;
    let socket = session::control_socket(&state.paths, &id)?;
    if !session::socket_is_live(&socket) {
        session::start_worker(&state.paths, &id)?;
    }
    let output = stream! {
        match UnixStream::connect(socket).await {
            Ok(mut connection) => {
                let request = serde_json::to_vec(&ControlRequest::Subscribe).unwrap_or_default();
                if connection.write_all(&request).await.is_ok() && connection.write_all(b"\n").await.is_ok() {
                    let mut lines = BufReader::new(connection).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        yield Ok(Event::default().data(line));
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

async fn get_settings(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    Ok(Json(
        json!({"config": state.config.read().expect("config lock").clone(), "llamaInstalled": llama_available(), "codexAuthenticated": codex_authenticated(&state.paths)}),
    ))
}

async fn save_settings(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(config): Json<AppConfig>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    config.save(&state.paths)?;
    *state.config.write().expect("config lock") = config;
    Ok(Json(json!({"ok": true, "restartRequired": true})))
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
    Ok(Json(json!({"ok":true,"login":status})))
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

pub async fn serve(paths: AppPaths, config: AppConfig) -> Result<()> {
    let address = format!("127.0.0.1:{}", config.web_port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("bind {address}"))?;
    axum::serve(listener, router(paths, config)).await?;
    Ok(())
}

const INDEX_HTML: &str = include_str!("web_ui.html");

#[cfg(test)]
mod tests {
    use super::*;

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
        let app = router(paths.clone(), config);
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
