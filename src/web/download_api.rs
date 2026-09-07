//! SimpleHF downloads are native Web operations; no downloader process or symlinks.
use super::*;
use crate::download::{Control, ControlState, Event as DownloadEvent, Manifest};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Clone, Default)]
pub struct Downloads(Arc<Mutex<BTreeMap<String, Job>>>);
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    manifest: Manifest,
    status: String,
    files: Vec<Value>,
    error: Option<String>,
}
struct Job {
    record: Record,
    control: Option<Control>,
    samples: BTreeMap<usize, (std::time::Instant, u64)>,
}
impl Downloads {
    pub fn load(paths: &AppPaths) -> Result<Self> {
        let manager = Self::default();
        let directory = paths.data.join("downloads");
        if !directory.exists() {
            return Ok(manager);
        }
        ensure_private_dir(&directory)?;
        for item in fs::read_dir(directory)? {
            let path = item?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            set_private_file(&path)?;
            let mut record: Record = serde_json::from_slice(&fs::read(path)?)?;
            if matches!(
                record.status.as_str(),
                "downloading" | "paused" | "cancelling" | "queued"
            ) {
                record.status = "interrupted".into();
            }
            manager.0.lock().unwrap().insert(
                record.id.clone(),
                Job {
                    record,
                    control: None,
                    samples: BTreeMap::new(),
                },
            );
        }
        Ok(manager)
    }
    fn snapshot(&self) -> Value {
        json!(
            self.0
                .lock()
                .unwrap()
                .values()
                .rev()
                .map(|job| &job.record)
                .collect::<Vec<_>>()
        )
    }
}
#[derive(Default, Serialize, Deserialize)]
struct Settings {
    destination: Option<PathBuf>,
    token: Option<String>,
}
fn settings(paths: &AppPaths) -> Result<Settings> {
    let file = paths.config.join("download-settings.json");
    if !file.exists() {
        return Ok(Settings::default());
    }
    set_private_file(&file)?;
    Ok(serde_json::from_slice(&fs::read(file)?)?)
}
async fn token(paths: &AppPaths, entered: Option<String>) -> Result<Option<String>> {
    if let Some(token) = entered.filter(|t| !t.trim().is_empty()) {
        return Ok(Some(token.trim().into()));
    }
    if let Some(token) = settings(paths)?.token {
        return Ok(Some(token));
    }
    Ok(crate::huggingface::find_token().await)
}
fn persist(paths: &AppPaths, record: &Record) -> Result<()> {
    let directory = paths.data.join("downloads");
    ensure_private_dir(&directory)?;
    crate::config::atomic_private_json(&directory.join(format!("{}.json", record.id)), record)
}
pub fn routes() -> Router<WebState> {
    Router::new()
        .route("/api/downloads", get(list).post(start))
        .route(
            "/api/downloads/settings",
            get(get_settings).post(save_settings),
        )
        .route("/api/downloads/search", post(search))
        .route("/api/downloads/repository", post(repository))
        .route("/api/downloads/control", post(control))
}
async fn list(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    Ok(Json(json!({"jobs":state.downloads.snapshot()})))
}
async fn get_settings(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    let saved = settings(&state.paths)?;
    Ok(Json(
        json!({"destination":saved.destination.unwrap_or_else(||state.paths.data.join("models")),"hasToken":saved.token.is_some()}),
    ))
}
#[derive(Deserialize)]
struct SettingsRequest {
    destination: PathBuf,
    token: Option<String>,
    #[serde(default)]
    clear_token: bool,
}
async fn save_settings(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<SettingsRequest>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    if !body.destination.is_absolute() {
        return Err(anyhow::anyhow!("Download folder must be absolute").into());
    }
    let _guard = state.downloads.0.lock().unwrap();
    let mut saved = settings(&state.paths)?;
    saved.destination = Some(body.destination);
    if body.clear_token {
        saved.token = None;
    } else if let Some(token) = body.token.filter(|t| !t.trim().is_empty()) {
        HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .map_err(|_| anyhow::anyhow!("Invalid Hugging Face token"))?;
        saved.token = Some(token.trim().into());
    }
    crate::config::atomic_private_json(&state.paths.config.join("download-settings.json"), &saved)?;
    Ok(Json(json!({"ok":true,"hasToken":saved.token.is_some()})))
}
#[derive(Deserialize)]
struct Lookup {
    #[serde(default)]
    query: String,
    #[serde(default)]
    id: String,
    token: Option<String>,
}
async fn search(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<Lookup>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let client = crate::huggingface::Client::new(token(&state.paths, body.token).await?);
    Ok(Json(
        json!({"models":client.search(&body.query,&Default::default()).await?}),
    ))
}
async fn repository(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<Lookup>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let client = crate::huggingface::Client::new(token(&state.paths, body.token).await?);
    Ok(Json(
        client.repository(&body.id, &Default::default()).await?,
    ))
}
#[derive(Deserialize)]
struct Start {
    #[serde(flatten)]
    manifest: Manifest,
    token: Option<String>,
}
async fn start(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<Start>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    crate::download::validate_manifest(&body.manifest).map_err(anyhow::Error::msg)?;
    let secret = token(&state.paths, body.token).await?;
    let id = uuid::Uuid::now_v7().to_string();
    let files = body
        .manifest
        .files
        .iter()
        .map(|file| json!({"path":file.path,"total":file.size,"downloaded":0,"status":"queued"}))
        .collect();
    let record = Record {
        id: id.clone(),
        manifest: body.manifest,
        status: "queued".into(),
        files,
        error: None,
    };
    {
        let mut jobs = state.downloads.0.lock().unwrap();
        if jobs.values().any(|job| {
            (job.control.is_some() || job.record.status == "queued")
                && job.record.manifest.destination == record.manifest.destination
                && job.record.manifest.repo_id == record.manifest.repo_id
        }) {
            return Err(anyhow::anyhow!(
                "This repository already has an active download in that folder"
            )
            .into());
        }
        persist(&state.paths, &record)?;
        jobs.insert(
            id.clone(),
            Job {
                record,
                control: None,
                samples: BTreeMap::new(),
            },
        );
    }
    if let Err(error) = launch(&state, &id, secret) {
        if let Some(job) = state.downloads.0.lock().unwrap().get_mut(&id) {
            job.record.status = "failed".into();
            job.record.error = Some(error.to_string());
            persist(&state.paths, &job.record)?;
        }
        return Err(error.into());
    }
    Ok(Json(json!({"id":id,"jobs":state.downloads.snapshot()})))
}
fn launch(state: &WebState, id: &str, secret: Option<String>) -> Result<()> {
    let progress = state.downloads.clone();
    let progress_id = id.to_owned();
    let ctl = Control::new(Arc::new(move |event| {
        if let DownloadEvent::File {
            index,
            status,
            downloaded,
            total,
            error,
        } = event
        {
            let mut jobs = progress.0.lock().unwrap();
            if let Some(job) = jobs.get_mut(&progress_id) {
                if let Some(file) = job.record.files.get_mut(index) {
                    file["status"] = json!(status);
                    file["downloaded"] = json!(downloaded);
                    file["total"] = json!(total);
                    file["error"] = json!(error);
                }
                let sample = job
                    .samples
                    .entry(index)
                    .or_insert_with(|| (std::time::Instant::now(), downloaded));
                let elapsed = sample.0.elapsed().as_secs_f64();
                if elapsed >= 0.2 {
                    let speed = downloaded.saturating_sub(sample.1) as f64 / elapsed;
                    if let Some(file) = job.record.files.get_mut(index) {
                        file["bytesPerSecond"] = json!(speed);
                    }
                    *sample = (std::time::Instant::now(), downloaded);
                }
            }
        }
    }));
    let manifest = {
        let jobs = state.downloads.0.lock().unwrap();
        let job = jobs.get(id).context("Download no longer exists")?;
        job.record.manifest.clone()
    };
    // Explicit chosen download folder is included in the ordinary local-model scan.
    {
        let mut config = state.config.write().unwrap();
        if config.llama.models_dir != manifest.destination
            && !config
                .llama
                .model_search_dirs
                .contains(&manifest.destination)
        {
            let mut next = config.clone();
            next.llama
                .model_search_dirs
                .push(manifest.destination.clone());
            next.save(&state.paths)?;
            *config = next;
        }
    }
    let manifest = {
        let mut jobs = state.downloads.0.lock().unwrap();
        anyhow::ensure!(
            !jobs.iter().any(|(other_id, job)| other_id != id
                && (job.control.is_some() || job.record.status == "queued")
                && job.record.manifest.destination == manifest.destination
                && job.record.manifest.repo_id == manifest.repo_id),
            "This repository already has an active download in that folder"
        );
        let job = jobs.get_mut(id).context("Download no longer exists")?;
        anyhow::ensure!(job.control.is_none(), "Download is already running");
        job.samples.clear();
        job.record.status = "downloading".into();
        job.record.error = None;
        persist(&state.paths, &job.record)?;
        job.control = Some(ctl.clone());
        job.record.manifest.clone()
    };
    let state = state.clone();
    let id = id.to_owned();
    tokio::spawn(async move {
        let result = crate::download::run(manifest, secret, ctl).await;
        let mut jobs = state.downloads.0.lock().unwrap();
        if let Some(job) = jobs.get_mut(&id) {
            job.control = None;
            job.record.status = match &result {
                Ok(()) => "complete",
                Err(e) if e == "Download cancelled" => "cancelled",
                Err(_) => "failed",
            }
            .into();
            job.record.error = result.err();
            if let Err(error) = persist(&state.paths, &job.record) {
                job.record.error = Some(error.to_string());
                job.record.status = "failed".into();
            }
        }
    });
    Ok(())
}
#[derive(Deserialize)]
struct Action {
    id: String,
    action: String,
    token: Option<String>,
}
async fn control(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<Action>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let secret = token(&state.paths, body.token).await?;
    let restart = {
        let mut jobs = state.downloads.0.lock().unwrap();
        let job = jobs
            .get_mut(&body.id)
            .context("Download no longer exists")?;
        match body.action.as_str() {
            "pause" => {
                let ctl = job.control.as_ref().context("Download is not running")?;
                ctl.set(ControlState::Paused);
                job.record.status = "paused".into();
                false
            }
            "resume" => {
                if job.record.status == "cancelling" {
                    return Err(
                        anyhow::anyhow!("Wait for cancellation to finish before resuming").into(),
                    );
                }
                if let Some(ctl) = &job.control {
                    ctl.set(ControlState::Running);
                    job.record.status = "downloading".into();
                    false
                } else {
                    true
                }
            }
            "cancel" => {
                if let Some(ctl) = &job.control {
                    ctl.set(ControlState::Cancelled);
                    job.record.status = "cancelling".into();
                } else {
                    job.record.status = "cancelled".into();
                }
                false
            }
            _ => return Err(anyhow::anyhow!("Unknown download action").into()),
        }
    };
    if restart {
        launch(&state, &body.id, secret)?;
    }
    Ok(Json(json!({"jobs":state.downloads.snapshot()})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn saved_token_is_private_and_download_controls_require_login_origin_and_csrf() {
        use std::os::unix::fs::PermissionsExt;
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
        let app = super::super::router(paths.clone(), config).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = reqwest::Client::new();
        let origin = format!("http://127.0.0.1:{port}");
        let cookie = format!("{COOKIE}={}", login.token);
        assert_eq!(
            http.get(format!("{origin}/api/downloads/settings"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let body = json!({"destination":root.path().join("models"),"token":"private-hf-token"});
        let post = || {
            http.post(format!("{origin}/api/downloads/settings"))
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, &origin)
                .header("x-bashkitten-csrf", &login.csrf)
                .json(&body)
        };
        assert_eq!(
            post()
                .header(header::ORIGIN, "http://foreign.example")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert!(post().send().await.unwrap().status().is_success());
        let output = http
            .get(format!("{origin}/api/downloads/settings"))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!output.contains("private-hf-token"));
        assert!(output.contains("true"));
        let file = paths.config.join("download-settings.json");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            settings(&paths).unwrap().token.as_deref(),
            Some("private-hf-token")
        );
        let interrupted = Record {
            id: "fixture-job".into(),
            manifest: Manifest {
                repo_id: "org/model".into(),
                revision: "main".into(),
                destination: root.path().join("models"),
                connections: 2,
                files: vec![crate::download::ManifestFile {
                    path: "model.gguf".into(),
                    size: Some(42),
                }],
            },
            status: "downloading".into(),
            files: vec![],
            error: None,
        };
        persist(&paths, &interrupted).unwrap();
        let restored = Downloads::load(&paths).unwrap().snapshot();
        assert_eq!(restored[0]["status"], "interrupted");
        assert!(!restored.to_string().contains("private-hf-token"));
        server.abort();
    }
}
