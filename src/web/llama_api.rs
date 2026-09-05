//! Authenticated Web plumbing for the pinned router workflow. It owns progress
//! only; systemd owns the independently running llama-server process.
use super::*;
use crate::llama::{Client, Progress};
use crate::tools::CancellationToken;
use std::sync::Mutex;

#[derive(Clone, Default)]
pub struct Operations(Arc<Mutex<Option<Operation>>>);
struct Operation {
    id: String,
    model: String,
    action: String,
    cancel: CancellationToken,
    phase: String,
    progress: Option<Progress>,
    error: Option<String>,
}
impl Operations {
    fn status(&self) -> Value {
        self.0.lock().unwrap().as_ref().map(|op|json!({"id":op.id,"model":op.model,"action":op.action,"phase":op.phase,"progress":op.progress,"error":op.error})).unwrap_or(Value::Null)
    }
}

pub fn routes() -> Router<WebState> {
    Router::new()
        .route("/api/llama/status", get(status))
        .route("/api/llama/refresh", post(refresh))
        .route("/api/llama/operation", get(operation_status).post(start))
        .route("/api/llama/cancel", post(cancel))
        .route("/api/llama/preset", post(save_preset))
        .route("/api/llama/hf/search", post(search))
        .route("/api/llama/hf/details", post(details))
}

async fn operation_status(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    Ok(Json(json!({"operation":state.llama_operation.status()})))
}

async fn status(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    authenticated(&state, &headers)?;
    let config = state.config.read().unwrap().llama.clone();
    let scan_config = config.clone();
    let (local_sources, local_models) =
        tokio::task::spawn_blocking(move || crate::llama::discover_local_models(&scan_config))
            .await
            .map_err(anyhow::Error::from)?;
    let installation = tokio::task::spawn_blocking(crate::llama::detect_installation)
        .await
        .map_err(anyhow::Error::from)?;
    let flash = if installation.is_some() {
        tokio::task::spawn_blocking(crate::llama::flash_attention_supported)
            .await
            .map_err(anyhow::Error::from)?
    } else {
        false
    };
    let running = tokio::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "bashkitten-llama.service"])
        .status()
        .await
        .is_ok_and(|s| s.success());
    Ok(Json(
        json!({"installation":installation,"running":running,"flashAttentionSupported":flash,"arguments":crate::llama::managed_launch_arguments(&config,&state.paths,false,flash)?,"presetIni":if config.models.is_empty(){String::new()}else{crate::llama::preset_contents(&config,flash)?},"catalog":public_catalog(config.catalog.clone()),"operation":state.llama_operation.status(),"localSources":local_sources,"localModels":local_models,"environment":crate::llama::launch_environment(&config)?}),
    ))
}

// Pi persists converted model metadata, not launch arguments that might contain
// credentials. Keep only fields needed by the catalog/registry and Web controls.
fn public_catalog(models: Vec<Value>) -> Vec<Value> {
    models
        .into_iter()
        .map(|model| {
            let mut clean = json!({"id":model["id"],"status":{"value":model["status"]["value"]},"preset":crate::llama::model_preset(&model)});
            for key in ["failed", "exit_code", "progress"] {
                if let Some(value) = model["status"].get(key) {
                    clean["status"][key] = value.clone();
                }
            }
            for key in ["architecture", "meta", "source", "aliases"] {
                if let Some(value) = model.get(key) {
                    clean[key] = value.clone();
                }
            }
            clean
        })
        .collect()
}

async fn sync_catalog(
    state: &WebState,
    client: &Client,
    models: Option<Vec<Value>>,
) -> Result<Vec<Value>> {
    let cancel = CancellationToken::default();
    let models = match models {
        Some(m) => m,
        None => client.list(false, &cancel).await?,
    };
    let autoload = if models
        .iter()
        .any(|m| m["status"]["value"] == "unloaded" && m["source"] == "preset")
    {
        client
            .props(&cancel)
            .await
            .ok()
            .is_some_and(|v| v["models_autoload"] == true)
    } else {
        false
    };
    let models = public_catalog(models);
    let mut config = state.config.write().unwrap();
    if client.server_url != format!("http://127.0.0.1:{}", config.llama.port) {
        anyhow::bail!("Router settings changed during the operation");
    }
    config.llama.catalog = models.clone();
    config.llama.router_autoload = autoload;
    config.save(&state.paths)?;
    Ok(models)
}

async fn refresh(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let client = Client::configured(&state.config.read().unwrap().llama)?;
    let models = client.list(true, &CancellationToken::default()).await?;
    Ok(Json(
        json!({"catalog":sync_catalog(&state,&client,Some(models)).await?}),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Start {
    action: String,
    model: String,
    #[serde(default = "retain_default")]
    retain_others: bool,
}
fn retain_default() -> bool {
    true
}

async fn start(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<Start>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    if !matches!(body.action.as_str(), "load" | "unload" | "download")
        || body.model.trim().is_empty()
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid model operation".into(),
        ));
    }
    let client = Client::configured(&state.config.read().unwrap().llama)?;
    let id = uuid::Uuid::new_v4().to_string();
    let cancellation = CancellationToken::default();
    {
        let mut current = state.llama_operation.0.lock().unwrap();
        if current
            .as_ref()
            .is_some_and(|o| matches!(o.phase.as_str(), "running" | "cancelling"))
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "A model operation is already running".into(),
            ));
        }
        *current = Some(Operation {
            id: id.clone(),
            model: body.model.clone(),
            action: body.action.clone(),
            cancel: cancellation.clone(),
            phase: "running".into(),
            progress: None,
            error: None,
        });
    }
    let response = json!({"operation":state.llama_operation.status()});
    tokio::spawn(async move {
        let update_state = state.llama_operation.clone();
        let update_id = id.clone();
        let progress: crate::llama::OnProgress = Arc::new(move |progress| {
            if let Some(op) = update_state
                .0
                .lock()
                .unwrap()
                .as_mut()
                .filter(|op| op.id == update_id)
            {
                op.progress = Some(progress);
            }
        });
        let result = async {
            let mut replaced = Vec::new();
            if body.action == "load" && !body.retain_others {
                replaced = client
                    .list(false, &cancellation)
                    .await?
                    .into_iter()
                    .filter(|m| {
                        m["id"] != body.model
                            && matches!(m["status"]["value"].as_str(), Some("loaded" | "sleeping"))
                    })
                    .collect();
                for model in &replaced {
                    client
                        .unload_and_wait(
                            model["id"].as_str().unwrap(),
                            &CancellationToken::default(),
                        )
                        .await?;
                }
            }
            let result = match body.action.as_str() {
                "load" => client
                    .load_and_wait(&body.model, progress.clone(), &cancellation)
                    .await
                    .map(|_| ()),
                "unload" => client.unload_and_wait(&body.model, &cancellation).await,
                _ => client
                    .download_and_wait(&body.model, progress.clone(), &cancellation)
                    .await
                    .map(|_| ()),
            };
            if cancellation.is_cancelled() && body.action != "unload" {
                let _ = client
                    .unload(&body.model, &CancellationToken::default())
                    .await;
            }
            if result.is_err() && !replaced.is_empty() {
                progress(Progress {
                    message: "Restoring previously loaded models".into(),
                    ratio: None,
                    detail: None,
                });
                for model in replaced {
                    if client
                        .load_and_wait(
                            model["id"].as_str().unwrap(),
                            Arc::new(|_| {}),
                            &CancellationToken::default(),
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            result?;
            sync_catalog(&state, &client, None).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if result.is_err() {
            let _ = sync_catalog(&state, &client, None).await;
        }
        if let Some(op) = state
            .llama_operation
            .0
            .lock()
            .unwrap()
            .as_mut()
            .filter(|o| o.id == id)
        {
            op.phase = if cancellation.is_cancelled() {
                "cancelled"
            } else if result.is_ok() {
                "complete"
            } else {
                "failed"
            }
            .into();
            op.error = result.err().map(|e| e.to_string());
        }
    });
    Ok(Json(response))
}

async fn cancel(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    if let Some(op) = state
        .llama_operation
        .0
        .lock()
        .unwrap()
        .as_mut()
        .filter(|op| op.phase == "running")
    {
        op.phase = "cancelling".into();
        op.cancel.cancel();
    }
    Ok(Json(json!({"operation":state.llama_operation.status()})))
}

async fn save_preset(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(preset): Json<crate::config::ModelPreset>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    preset
        .validate()
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    let selected_local = !preset.llama_model_path.as_os_str().is_empty();
    let scanned_config = state.config.read().unwrap().llama.clone();
    if selected_local {
        let scan_config = scanned_config.clone();
        let scan_preset = preset.clone();
        tokio::task::spawn_blocking(move || {
            crate::llama::validate_local_preset(&scan_config, &scan_preset)
        })
        .await
        .map_err(anyhow::Error::from)?
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    }
    let mut current = state.config.write().unwrap();
    let mut config = current.clone();
    if selected_local
        && (config.llama.model_search_dirs != scanned_config.model_search_dirs
            || config.llama.models_dir != scanned_config.models_dir)
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Model folders changed while saving; select the local model again".into(),
        ));
    }
    if !selected_local
        && !config
            .llama
            .catalog
            .iter()
            .any(|model| model["id"] == preset.id)
        && !config
            .llama
            .models
            .iter()
            .any(|model| model.id == preset.id)
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Refresh the router catalog before saving this model".into(),
        ));
    }
    if let Some(old) = config.llama.models.iter_mut().find(|m| m.id == preset.id) {
        *old = preset;
    } else {
        config.llama.models.push(preset);
    }
    config
        .validate_models()
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    crate::llama::preset_contents(&config.llama, false)
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    config.save(&state.paths)?;
    *current = config;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
struct Search {
    query: String,
}
#[derive(Deserialize)]
struct Details {
    id: String,
}
async fn search(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<Search>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let client = crate::huggingface::Client::new(crate::huggingface::find_token().await);
    Ok(Json(
        json!({"models":client.search(&body.query,&CancellationToken::default()).await?}),
    ))
}
async fn details(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<Details>,
) -> ApiResult<Json<Value>> {
    require_mutation(&state, &headers)?;
    let client = crate::huggingface::Client::new(crate::huggingface::find_token().await);
    Ok(Json(
        client
            .details(&body.id, &CancellationToken::default())
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    #[tokio::test]
    async fn local_models_require_login_and_save_native_presets_without_router_requests() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let folder = root.path().join("custom models");
        std::fs::create_dir(&folder).unwrap();
        let model_path = folder.join("fixture.gguf");
        std::fs::write(&model_path, b"fixture GGUF listing only").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut config = AppConfig {
            web_port: port,
            ..Default::default()
        };
        config.llama.models_dir = root.path().join("empty router folder");
        config.llama.model_search_dirs = vec![folder];
        config
            .llama
            .gpu_environment
            .insert("CUDA_VISIBLE_DEVICES".into(), "1".into());
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
            http.get(format!("{origin}/api/llama/status"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let listing: Value = http
            .get(format!("{origin}/api/llama/status"))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let model = listing["localModels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["path"] == model_path.to_str().unwrap())
            .unwrap();
        assert_eq!(listing["environment"]["CUDA_VISIBLE_DEVICES"], "1");
        assert_eq!(listing["catalog"], json!([]));
        let preset = crate::config::ModelPreset {
            id: model["id"].as_str().unwrap().into(),
            name: "Fixture local".into(),
            llama_model_path: model_path.clone(),
            ..Default::default()
        };
        let post = |value: &crate::config::ModelPreset| {
            http.post(format!("{origin}/api/llama/preset"))
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, &origin)
                .header("x-bashkitten-csrf", &login.csrf)
                .json(value)
        };
        assert_eq!(
            post(&preset)
                .header(header::ORIGIN, "http://foreign.example")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let result = post(&preset).send().await.unwrap();
        assert!(
            result.status().is_success(),
            "{}",
            result.text().await.unwrap()
        );
        let saved = AppConfig::load(&paths).unwrap();
        assert_eq!(saved.llama.models[0].llama_model_path, model_path);
        assert!(saved.llama.catalog.is_empty());
        assert!(
            std::fs::read_to_string(paths.config.join("llama-models.ini"))
                .unwrap()
                .contains(&format!("model = {}\n", model_path.display()))
        );
        let mut rejected = preset.clone();
        rejected.llama_model_path = root.path().join("outside.gguf");
        std::fs::write(&rejected.llama_model_path, b"outside configured folders").unwrap();
        assert_eq!(
            post(&rejected).send().await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            AppConfig::load(&paths).unwrap().llama.models[0].llama_model_path,
            model_path
        );
        server.abort();
    }

    #[tokio::test]
    async fn authenticated_router_workflow_restores_replaced_models_on_failure_and_cancel() {
        use axum::routing::any;
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let mode = Arc::new(AtomicU8::new(0));
        let catalog = Arc::new(Mutex::new(
            json!({"data":[{"id":"old","status":{"value":"loaded","args":["--api-key","secret-in-args"]}},{"id":"target","status":{"value":"unloaded"}}]}),
        ));
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = calls.clone();
        let entries = catalog.clone();
        let loading_mode = mode.clone();
        let mock = Router::new().fallback(any(move |request: axum::extract::Request| {
            let calls = observed.clone();
            let catalog = entries.clone();
            let mode = loading_mode.clone();
            async move {
                assert_eq!(request.headers()["authorization"], "Bearer fixture-key");
                let (parts, body) = request.into_parts();
                let bytes = axum::body::to_bytes(body, 65536).await.unwrap();
                let input: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let path = parts.uri.path();
                if path == "/models/sse" {
                    return (StatusCode::SERVICE_UNAVAILABLE, "fixture polling fallback")
                        .into_response();
                }
                if parts.method == axum::http::Method::POST {
                    calls
                        .lock()
                        .unwrap()
                        .push(format!("{path}:{}", input["model"].as_str().unwrap()));
                    let mut data = catalog.lock().unwrap();
                    let model = data["data"]
                        .as_array_mut()
                        .unwrap()
                        .iter_mut()
                        .find(|m| m["id"] == input["model"])
                        .unwrap();
                    model["status"] = if path == "/models/unload" {
                        json!({"value":"unloaded"})
                    } else if input["model"] == "old" || mode.load(Ordering::SeqCst) == 2 {
                        json!({"value":"loaded"})
                    } else if mode.load(Ordering::SeqCst) == 0 {
                        json!({"value":"unloaded","failed":true,"exit_code":8})
                    } else {
                        json!({"value":"loading"})
                    };
                    return Json(json!({})).into_response();
                }
                Json(catalog.lock().unwrap().clone()).into_response()
            }
        }));
        let router_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let router_port = router_listener.local_addr().unwrap().port();
        let router_server = tokio::spawn(async move {
            axum::serve(router_listener, mock).await.unwrap();
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut config = AppConfig {
            web_port: port,
            ..Default::default()
        };
        config.llama.port = router_port;
        config.llama.api_key = "fixture-key".into();
        config.save(&paths).unwrap();
        let login = auth::signup(&paths, "fixture", "fixture-password").unwrap();
        let app = super::super::router(paths.clone(), config).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = reqwest::Client::new();
        let origin = format!("http://127.0.0.1:{port}");
        let cookie = format!("{COOKIE}={}", login.token);
        let post = |path: &str| {
            http.post(format!("{origin}/api/llama/{path}"))
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, &origin)
                .header("x-bashkitten-csrf", &login.csrf)
        };
        assert_eq!(
            http.post(format!("{origin}/api/llama/refresh"))
                .header(header::ORIGIN, &origin)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            post("refresh")
                .header(header::ORIGIN, "http://foreign.example")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert!(calls.lock().unwrap().is_empty());
        let refreshed = post("refresh").send().await.unwrap().text().await.unwrap();
        assert!(!refreshed.contains("secret-in-args"));
        for run in 0..3 {
            mode.store(run, Ordering::SeqCst);
            calls.lock().unwrap().clear();
            let response = post("operation")
                .json(&json!({"action":"load","model":"target","retainOthers":run==2}))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            if run == 1 {
                tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        if calls
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|v| v == "/models/load:target")
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                assert!(post("cancel").send().await.unwrap().status().is_success());
            }
            let settled = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let value: Value = http
                        .get(format!("{origin}/api/llama/operation"))
                        .header(header::COOKIE, &cookie)
                        .send()
                        .await
                        .unwrap()
                        .json()
                        .await
                        .unwrap();
                    if !matches!(
                        value["operation"]["phase"].as_str(),
                        Some("running" | "cancelling")
                    ) {
                        break value["operation"].clone();
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                settled["phase"],
                ["failed", "cancelled", "complete"][run as usize]
            );
            if run == 0 {
                assert_eq!(settled["error"], "Model exited with code 8");
            }
            let calls = calls.lock().unwrap().clone();
            if run < 2 {
                assert_eq!(calls[0], "/models/unload:old");
                assert_eq!(calls[1], "/models/load:target");
                assert_eq!(calls.last().unwrap(), "/models/load:old");
                if run == 1 {
                    assert_eq!(calls[2], "/models/unload:target");
                }
            } else {
                assert_eq!(calls, vec!["/models/load:target"]);
            }
            assert_eq!(
                catalog.lock().unwrap()["data"][0]["status"]["value"],
                "loaded"
            );
            let saved = AppConfig::load(&paths).unwrap();
            assert_eq!(saved.llama.catalog[0]["status"]["value"], "loaded");
        }
        server.abort();
        router_server.abort();
    }
}
