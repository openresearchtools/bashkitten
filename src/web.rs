use crate::auth;
use crate::config::AppConfig;
use crate::models;
use crate::paths::{AppPaths, set_private_file};
use crate::session::{self, ControlRequest, Delivery, NewSession, QueueAction};
use anyhow::{Context, Result};
use async_stream::stream;
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
struct QueueMutation {
    id: String,
    action: QueueAction,
    content: Option<String>,
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
        .route("/api/sessions/{id}/status", get(session_status))
        .route("/api/sessions/{id}/queue", post(mutate_queue))
        .route("/api/sessions/{id}/stop", post(stop_session))
        .route("/api/sessions/{id}/model", post(change_model))
        .route("/api/folders", get(list_folders).post(create_folder))
        .route("/api/settings", get(get_settings).post(save_settings))
        .route("/api/provider/import-pi", post(import_pi_auth))
        .route("/api/llama/restart", post(restart_llama))
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
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
        let path = dir.join(format!("{}-{name}", uuid::Uuid::new_v4()));
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

async fn import_pi_auth(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let source = home.join(".pi/agent/auth.json");
    let bytes = fs::read(&source).with_context(|| format!("read {}", source.display()))?;
    let parsed: Value = serde_json::from_slice(&bytes).context("parse Pi credential file")?;
    if parsed.get("openai-codex").is_none() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Pi has no OpenAI subscription credential".into(),
        ));
    }
    fs::write(state.paths.provider_auth_file(), bytes).context("write provider credential file")?;
    set_private_file(&state.paths.provider_auth_file())?;
    Ok(Json(json!({"ok": true})))
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

#[allow(dead_code)]
const LEGACY_INDEX_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>BashKitten</title><style>
:root{color-scheme:dark;--bg:#12100f;--panel:#1b1816;--line:#332d29;--text:#f3eee8;--muted:#a99d94;--accent:#e69654;--danger:#d75b5b}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:15px system-ui,sans-serif}button,input,select,textarea{font:inherit;color:inherit;background:#211d1a;border:1px solid var(--line);border-radius:7px;padding:.65rem}button{cursor:pointer}button.primary{background:var(--accent);color:#1a1008;border:0;font-weight:700}.hidden{display:none!important}#auth{max-width:390px;margin:12vh auto;padding:2rem;background:var(--panel);border:1px solid var(--line);border-radius:14px}#auth input{display:block;width:100%;margin:.7rem 0}#app{height:100vh;display:grid;grid-template-columns:280px 1fr}aside{background:var(--panel);border-right:1px solid var(--line);display:flex;flex-direction:column;min-width:0}header{height:58px;border-bottom:1px solid var(--line);display:flex;align-items:center;gap:.6rem;padding:.7rem}header strong{flex:1}.sessions{overflow:auto;flex:1}.session{padding:.8rem 1rem;border-bottom:1px solid var(--line);cursor:pointer}.session:hover,.session.active{background:#29231f}.session small{display:block;color:var(--muted);margin-top:.25rem;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}.dot{color:#64c278}main{min-width:0;display:grid;grid-template-rows:58px 1fr auto}#chat{overflow:auto;padding:1.2rem max(1rem,calc((100% - 850px)/2));display:flex;flex-direction:column;gap:.8rem}.msg{border:1px solid var(--line);border-radius:10px;padding:.85rem;white-space:pre-wrap;overflow-wrap:anywhere}.msg.user{background:#28221e;margin-left:12%}.msg.assistant{background:#191715;margin-right:6%}.msg.tool{font-family:ui-monospace,monospace;color:#d8c8ba}.meta{font-size:.8rem;color:var(--muted)}#composer{border-top:1px solid var(--line);padding:.8rem max(1rem,calc((100% - 850px)/2));display:grid;grid-template-columns:1fr auto;gap:.6rem}#composer textarea{resize:vertical;min-height:66px}.toolbar{grid-column:1/-1;display:flex;gap:.5rem;align-items:center}.toolbar select{max-width:260px}dialog{width:min(680px,92vw);background:var(--panel);color:var(--text);border:1px solid var(--line);border-radius:12px}dialog label{display:block;margin:.8rem 0}dialog input,dialog select{width:100%}.row{display:flex;gap:.5rem}.row>*{flex:1}.usage{color:var(--muted);font-size:.85rem}@media(max-width:720px){#app{grid-template-columns:1fr}aside{display:none}.msg.user{margin-left:4%}}
</style></head><body>
<section id="auth" class="hidden"><h1>🐈 BashKitten</h1><p id="authHint"></p><form id="authForm"><input id="username" autocomplete="username" placeholder="Username" required><input id="password" type="password" autocomplete="current-password" placeholder="Password (8+ characters)" required minlength="8"><button class="primary" type="submit">Continue</button><p id="authError" class="meta"></p></form></section>
<section id="app" class="hidden"><aside><header><strong>🐈 BashKitten</strong><button id="newBtn">＋</button><button id="settingsBtn">⚙</button></header><div id="sessions" class="sessions"></div></aside><main><header><strong id="title">Select a session</strong><span id="usage" class="usage"></span><button id="stopBtn" class="hidden">Stop</button></header><div id="chat"></div><form id="composer"><textarea id="prompt" placeholder="Message the agent…"></textarea><button class="primary">Send</button><div class="toolbar"><select id="delivery"><option value="queue">Queue</option><option value="steer">Steer current turn</option></select><input id="files" type="file" accept="image/*" multiple><select id="model"></select><select id="thinking"></select><button id="applyModel" type="button">Apply model</button></div></form></main></section>
<dialog id="newDialog"><form id="newForm"><h2>New session</h2><label>Working directory<input id="newCwd" required></label><label>Model<select id="newModel"></select></label><label>Thinking<select id="newThinking"></select></label><label>First message<textarea id="newPrompt" required></textarea></label><label>Images<input id="newFiles" type="file" accept="image/*" multiple></label><div class="row"><button type="button" data-close>Cancel</button><button class="primary">Start</button></div></form></dialog>
<dialog id="settingsDialog"><form id="settingsForm"><h2>Settings</h2><label>Web port<input id="webPort" type="number" min="1024" max="65535"></label><label>Default working directory<input id="defaultCwd"></label><label>Default model<select id="defaultModel"></select></label><label>Default thinking<select id="defaultThinking"></select></label><label><input id="llamaEnabled" type="checkbox"> Enable managed llama.cpp router</label><div class="row"><label>llama.cpp port<input id="llamaPort" type="number"></label><label>Context size<input id="llamaContext" type="number"></label></div><div class="row"><button id="importPi" type="button">Import Pi login</button><button id="restartLlama" type="button">Restart llama.cpp</button></div><div class="row"><button type="button" data-close>Cancel</button><button class="primary">Save</button></div><p class="meta">Port changes take effect when the Web service restarts. No telemetry or update checks are performed.</p></form></dialog>
<script>
let csrf='',hasUser=false,models=[],current=null,source=null,config=null;const $=s=>document.querySelector(s);async function api(url,opt={}){opt.headers=opt.headers||{};if(opt.method&&opt.method!=='GET')opt.headers['x-bashkitten-csrf']=csrf;let r=await fetch(url,opt);let j=await r.json().catch(()=>({}));if(!r.ok)throw Error(j.error||r.statusText);return j}function authScreen(signup){hasUser=!signup;$('#app').classList.add('hidden');$('#auth').classList.remove('hidden');$('#authHint').textContent=signup?'Create the one local Web UI account.':'Sign in to the local Web UI.'}async function boot(){let b=await api('/api/bootstrap');if(!b.hasUser)return authScreen(true);if(!b.authenticated)return authScreen(false);csrf=b.csrf;$('#auth').classList.add('hidden');$('#app').classList.remove('hidden');await Promise.all([loadModels(),loadSessions(),loadSettings()])}$('#authForm').onsubmit=async e=>{e.preventDefault();try{let r=await api(hasUser?'/api/login':'/api/signup',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({username:$('#username').value,password:$('#password').value})});csrf=r.csrf;await boot()}catch(x){$('#authError').textContent=x.message}};
function fillSelect(el,list,chosen){el.innerHTML=list.map(m=>`<option value="${escapeHtml(m.provider+'/'+m.id)}">${escapeHtml(m.name)} · ${escapeHtml(m.provider)}</option>`).join('');if(chosen)el.value=chosen;setThinking(el)}function setThinking(modelEl,target){let m=models.find(x=>x.provider+'/'+x.id===modelEl.value),t=target||($(modelEl=== $('#newModel')?'#newThinking':modelEl===$('#defaultModel')?'#defaultThinking':'#thinking'));t.innerHTML=(m?.thinking_levels||['off']).map(x=>`<option>${escapeHtml(x)}</option>`).join('');if(m?.default_thinking)t.value=m.default_thinking}async function loadModels(){let x=await api('/api/models');models=x.models.filter(m=>m.available);fillSelect($('#model'),models,x.defaultModel);fillSelect($('#newModel'),models,x.defaultModel);fillSelect($('#defaultModel'),models,x.defaultModel);setThinking($('#newModel'));setThinking($('#defaultModel'));$('#newThinking').value=x.defaultThinking;$('#defaultThinking').value=x.defaultThinking}for(let id of ['model','newModel','defaultModel'])$('#'+id).onchange=e=>setThinking(e.target);
async function loadSessions(){let x=await api('/api/sessions?limit=100');$('#sessions').innerHTML=x.sessions.map(s=>`<div class="session" data-id="${s.id}"><b>${escapeHtml(s.title)}</b><small>${s.running?'<span class="dot">● running</span> · ':''}${escapeHtml(s.model||'')} · ${escapeHtml(s.cwd||'')}</small></div>`).join('');document.querySelectorAll('.session').forEach(e=>e.onclick=()=>openSession(x.sessions.find(s=>s.id===e.dataset.id)));return x.sessions}async function openSession(s){current=s;if(source)source.close();document.querySelectorAll('.session').forEach(e=>e.classList.toggle('active',e.dataset.id===s.id));$('#title').textContent=s.title;$('#stopBtn').classList.toggle('hidden',!s.running);if(s.model){$('#model').value=s.model;setThinking($('#model'));$('#thinking').value=s.thinking||'off'}$('#chat').innerHTML='';let h=await api(`/api/sessions/${s.id}/segments/${s.current_segment}`);h.entries.forEach(render);$('#chat').scrollTop=$('#chat').scrollHeight;if(s.running){source=new EventSource(`/api/sessions/${s.id}/events`);source.onmessage=e=>{try{render(JSON.parse(e.data))}catch{}}}}function render(e){if(e.type==='assistant_delta'){let d=$('#liveAssistant');if(!d){d=document.createElement('div');d.id='liveAssistant';d.className='msg assistant';$('#chat').append(d)}d.textContent+=e.delta;$('#chat').scrollTop=$('#chat').scrollHeight;return}if(e.type==='message')$('#liveAssistant')?.remove();if(e.type==='session'||(!e.type&&e.ok)||['agent_start','agent_end','turn_start','turn_end','status','thinking_delta','tool_start'].includes(e.type))return;let role=e.role||e.message?.role||e.type||'event',content=e.error??e.message?.errorMessage??e.content??e.message?.content??e.message??e;if(Array.isArray(content))content=content.map(x=>x.text||x.content||x.thinking||`[${x.type}]`).join('\n');if(typeof content!=='string')content=JSON.stringify(content,null,2);let d=document.createElement('div');d.className='msg '+(role.includes('tool')?'tool':role);d.textContent=content;$('#chat').append(d);$('#chat').scrollTop=$('#chat').scrollHeight;let usage=e.usage||e.message?.usage;if(usage)$('#usage').textContent=`${usage.totalTokens||0} tokens`}function escapeHtml(s){return String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]))}
$('#composer').onsubmit=async e=>{e.preventDefault();if(!current)return;let f=new FormData();f.append('content',$('#prompt').value);f.append('delivery',$('#delivery').value);for(let x of $('#files').files)f.append('file',x);$('#prompt').value='';try{await api(`/api/sessions/${current.id}/messages`,{method:'POST',body:f});let sessions=await loadSessions(),active=sessions.find(s=>s.id===current.id);if(active)await openSession(active)}catch(x){alert(x.message)}};$('#applyModel').onclick=async()=>{if(!current)return;try{await api(`/api/sessions/${current.id}/model`,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({model:$('#model').value,thinking:$('#thinking').value})});await loadSessions()}catch(x){alert(x.message)}};$('#newBtn').onclick=()=>$('#newDialog').showModal();document.querySelectorAll('[data-close]').forEach(x=>x.onclick=()=>x.closest('dialog').close());$('#newForm').onsubmit=async e=>{e.preventDefault();let f=new FormData();f.append('prompt',$('#newPrompt').value);f.append('cwd',$('#newCwd').value);f.append('model',$('#newModel').value);f.append('thinking',$('#newThinking').value);for(let x of $('#newFiles').files)f.append('file',x);try{let r=await api('/api/sessions',{method:'POST',body:f});$('#newDialog').close();await loadSessions();document.querySelector(`.session[data-id="${r.id}"]`)?.click()}catch(x){alert(x.message)}};$('#stopBtn').onclick=async()=>{if(current){await api(`/api/sessions/${current.id}/stop`,{method:'POST'});await loadSessions()}};
async function loadSettings(){let x=await api('/api/settings');config=x.config;$('#webPort').value=config.web_port;$('#defaultCwd').value=config.default_cwd;$('#llamaEnabled').checked=config.llama.enabled;$('#llamaPort').value=config.llama.port;$('#llamaContext').value=config.llama.context_size;$('#newCwd').value=config.default_cwd}$('#settingsBtn').onclick=()=>$('#settingsDialog').showModal();$('#settingsForm').onsubmit=async e=>{e.preventDefault();config.web_port=Number($('#webPort').value);config.default_cwd=$('#defaultCwd').value;config.default_model=$('#defaultModel').value;config.default_thinking=$('#defaultThinking').value;config.llama.enabled=$('#llamaEnabled').checked;config.llama.port=Number($('#llamaPort').value);config.llama.context_size=Number($('#llamaContext').value);await api('/api/settings',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(config)});$('#settingsDialog').close()};$('#importPi').onclick=async()=>{try{await api('/api/provider/import-pi',{method:'POST'});await loadModels();alert('Pi OpenAI login imported.')}catch(x){alert(x.message)}};$('#restartLlama').onclick=async()=>{try{await api('/api/llama/restart',{method:'POST'});alert('llama.cpp restarted.')}catch(x){alert(x.message)}};boot();setInterval(()=>{if(csrf)loadSessions().catch(()=>{})},5000);
</script></body></html>"#;
