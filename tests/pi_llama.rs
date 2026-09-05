use bashkitten::{huggingface, llama, tools::CancellationToken};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(k, a)| b.get(k).is_some_and(|b| equal(a, b)))
        }
        _ => a == b,
    }
}

#[tokio::test]
async fn pinned_router_and_huggingface_fixtures() {
    use axum::{
        Router,
        body::Body,
        http::{Request, Response},
        routing::any,
    };
    let fixture: Value = serde_json::from_str(include_str!("fixtures/pi-llama.json")).unwrap();
    assert_eq!(fixture["pin"], bashkitten::PI_REFERENCE_COMMIT);
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let kind = case["kind"].as_str().unwrap();
        let input = &case["input"];
        let result: anyhow::Result<Value> = match kind {
            "url" => llama::normalize_server_url(input.as_str().unwrap()).map(|v| json!(v)),
            "bytes" => Ok(json!(llama::format_bytes(input.as_f64().unwrap()))),
            "loadProgress" => Ok(serde_json::to_value(llama::load_progress(input)).unwrap()),
            "downloadProgress" => {
                Ok(serde_json::to_value(llama::download_progress(input)).unwrap())
            }
            "hfDetails" => huggingface::model_details("a b/model!", input["payload"].clone()),
            "hfSearch" => huggingface::search_results(input["payload"].clone()),
            "hfError" => {
                let mut headers = reqwest::header::HeaderMap::new();
                for (k, v) in input["headers"].as_object().into_iter().flatten() {
                    headers.insert(
                        reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                        v.as_str().unwrap().parse().unwrap(),
                    );
                }
                Err(anyhow::anyhow!(huggingface::http_error(
                    input["status"].as_u64().unwrap() as u16,
                    &headers,
                    &input["payload"]
                )))
            }
            _ => {
                let input = input.clone();
                let received = Arc::new(Mutex::new(Vec::new()));
                let captured = received.clone();
                let app=Router::new().fallback(any(move |request:Request<Body>|{let input=input.clone();let captured=captured.clone();async move {
                    let (parts,body)=request.into_parts();let bytes=axum::body::to_bytes(body,65536).await.unwrap();
                    captured.lock().unwrap().push(json!({"path":parts.uri.to_string(),"method":parts.method.as_str(),"authorization":parts.headers.get("authorization").and_then(|v|v.to_str().ok()),"body":serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null)}));
                    Response::builder().status(input["status"].as_u64().unwrap_or(200) as u16).body(Body::from(input["raw"].as_str().map(str::to_string).unwrap_or_else(||input["payload"].to_string()))).unwrap()
                }}));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}", listener.local_addr().unwrap());
                let server = tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                });
                let client = llama::Client::new(
                    &url,
                    if kind == "routerError" {
                        ""
                    } else {
                        "fixture-key"
                    },
                )
                .unwrap();
                let cancel = CancellationToken::default();
                let result = match kind {
                    "list" => client.list(true, &cancel).await.map(|v| json!(v)),
                    "props" => client.props(&cancel).await,
                    "load" => client
                        .load("test/model:Q4_K_M", &cancel)
                        .await
                        .map(|_| Value::Null),
                    "unload" => client
                        .unload("test/model:Q4_K_M", &cancel)
                        .await
                        .map(|_| Value::Null),
                    "download" => client
                        .download("test/model:Q4_K_M", &cancel)
                        .await
                        .map(|_| Value::Null),
                    "routerError" => client.list(false, &cancel).await.map(|v| json!(v)),
                    _ => panic!("Unknown kind {kind}"),
                };
                if let Some(expected) = case["requests"].as_array() {
                    let actual = received.lock().unwrap();
                    assert_eq!(actual.len(), expected.len());
                    for (a, e) in actual.iter().zip(expected) {
                        let mut e = e.clone();
                        let url = url::Url::parse(e["url"].as_str().unwrap()).unwrap();
                        e.as_object_mut().unwrap().remove("url");
                        e["path"] = json!(format!(
                            "{}{}",
                            url.path(),
                            url.query().map(|s| format!("?{s}")).unwrap_or_default()
                        ));
                        assert_eq!(a, &e, "{} request", case["name"]);
                    }
                }
                server.abort();
                result
            }
        };
        let actual = match result {
            Ok(value) => json!({"expected":value}),
            Err(error) => json!({"error":error.to_string()}),
        };
        let expected = if case.get("error").is_some() {
            json!({"error":case["error"]})
        } else {
            json!({"expected":case["expected"]})
        };
        if !equal(&actual, &expected) {
            failures.push(format!(
                "{} actual {actual} expected {expected}",
                case["name"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn router_load_download_polling_and_cancellation_use_actual_http() {
    use axum::{Json, Router, body::Body, http::Request, response::IntoResponse, routing::any};
    let calls = Arc::new(Mutex::new(Vec::new()));
    let captured = calls.clone();
    let app = Router::new().fallback(any(move |request: Request<Body>| {
        let captured = captured.clone();
        async move {
            let path = request.uri().path_and_query().unwrap().as_str().to_string();
            let method = request.method().to_string();
            captured.lock().unwrap().push(format!("{method} {path}"));
            if path == "/models/sse" {
                return (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "offline events",
                )
                    .into_response();
            }
            if method == "POST" {
                return Json(json!({})).into_response();
            }
            let count = captured
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.as_str() == "GET /models")
                .count();
            let status = if count < 2 { "loading" } else { "loaded" };
            Json(json!({"data":[{"id":"fixture","status":{"value":status}}]})).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client =
        llama::Client::new(&format!("http://{}", listener.local_addr().unwrap()), "").unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let progress = Arc::new(Mutex::new(Vec::new()));
    let captured = progress.clone();
    let cancel = CancellationToken::default();
    let model = client
        .load_and_wait(
            "fixture",
            Arc::new(move |p| captured.lock().unwrap().push(p)),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(model["status"]["value"], "loaded");
    assert_eq!(progress.lock().unwrap()[0].message, "Loading model");
    let downloaded = client
        .download_and_wait("fixture", Arc::new(|_| {}), &cancel)
        .await
        .unwrap();
    assert_eq!(downloaded.len(), 1);
    assert!(
        calls
            .lock()
            .unwrap()
            .contains(&"GET /models?reload=1".into())
    );
    let before = calls.lock().unwrap().len();
    cancel.cancel();
    assert_eq!(
        client
            .unload("fixture", &cancel)
            .await
            .unwrap_err()
            .to_string(),
        "This operation was aborted"
    );
    assert_eq!(calls.lock().unwrap().len(), before);
    server.abort();
}

#[test]
fn native_presets_apply_model_overrides_without_cli_defaults_overriding_them() {
    use bashkitten::config::{GpuLayers, LlamaConfig, ModelPreset};
    let root = tempfile::tempdir().unwrap();
    let paths = bashkitten::paths::AppPaths {
        config: root.path().join("config"),
        data: root.path().join("data"),
        runtime: root.path().join("runtime"),
    };
    let config = LlamaConfig {
        context_size: 32768,
        gpu_layers: GpuLayers::Count(17),
        models: vec![ModelPreset {
            id: "fixture".into(),
            context_window: 4096,
            llama_options: "ngl = 9\nbatch-size = 64".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let ini = llama::preset_contents(&config, true).unwrap();
    assert!(ini.contains("[*]\njinja = true\nctx-size = 32768\nngl = 17\n"));
    assert!(ini.contains("[fixture]\nctx-size = 4096\nngl = 9\nbatch-size = 64\n"));
    let args = llama::managed_launch_arguments(&config, &paths, false, true).unwrap();
    assert!(args.contains(&"--models-preset".into()));
    for key in ["--ctx-size", "-ngl", "--batch-size"] {
        assert!(
            !args.contains(&key.into()),
            "{key} would override the model INI"
        );
    }
    let mut advanced = config;
    advanced.extra_arguments = vec!["--ctx-size".into(), "8192".into()];
    assert!(
        llama::managed_launch_arguments(&advanced, &paths, false, true)
            .unwrap()
            .ends_with(&["--ctx-size".into(), "8192".into()])
    );
}
