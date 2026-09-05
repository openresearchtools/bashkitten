use axum::{Router, body::Body, http::HeaderMap, response::Response, routing::post};
use bashkitten::providers::{
    CodexEndpoint, ProviderAuthStore, ProviderClient, ProviderCredential, ProviderRequest,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;

#[tokio::test]
async fn pinned_codex_http_errors_retries_compression_and_cache_headers() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/pi-codex-http.json")).unwrap();
    assert_eq!(fixture["pin"], bashkitten::PI_REFERENCE_COMMIT);
    let responses = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::new(Mutex::new(Vec::<(HeaderMap, Value)>::new()));
    let inputs = responses.clone();
    let outputs = captured.clone();
    let app = Router::new().route(
        "/backend-api/codex/responses",
        post(move |headers: HeaderMap, body: axum::body::Bytes| {
            let inputs = inputs.clone();
            let outputs = outputs.clone();
            async move {
                assert_eq!(headers["content-encoding"], "zstd");
                let decoded = zstd::bulk::decompress(&body, 1_000_000).unwrap();
                let json: Value = serde_json::from_slice(&decoded).unwrap();
                let mut output = outputs.lock().await;
                let index = output.len();
                output.push((headers, json));
                let input = inputs.lock().await;
                let response = &input[index.min(input.len() - 1)];
                let mut builder = Response::builder()
                    .status(response["status"].as_u64().unwrap() as u16)
                    .header(
                        "content-type",
                        if response["status"] == 200 {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    );
                for (name, value) in response["headers"].as_object().into_iter().flatten() {
                    builder = builder.header(name, value.as_str().unwrap());
                }
                builder
                    .body(Body::from(response["body"].as_str().unwrap().to_owned()))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let auth = ProviderAuthStore::new(temp.path().join("auth.json"));
    auth.set_codex(Some(ProviderCredential::OAuth {
        access: fixture["token"].as_str().unwrap().into(),
        refresh: "fixture-refresh".into(),
        expires: chrono::Utc::now().timestamp_millis() + 3_600_000,
        extra: BTreeMap::new(),
    }))
    .await
    .unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        *responses.lock().await = case["responses"].as_array().unwrap().clone();
        captured.lock().await.clear();
        let options = &case["options"];
        let endpoint = CodexEndpoint {
            base_url: case["model"]["baseUrl"].as_str().unwrap().replacen(
                "http://localhost",
                &host,
                1,
            ),
            auth_store: auth.clone(),
            headers: case["model"]["headers"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().into()))
                .collect(),
        };
        let mut request = ProviderRequest::new("gpt-5.5", vec![]);
        request.transport = Some(bashkitten::codex_websocket::Transport::Sse);
        request.logical_messages =
            Some(vec![json!({"role":"user","content":"test","timestamp":0})]);
        request.session_id = Some(format!("session-{}", "x".repeat(80)));
        request.disable_cache = options["cacheRetention"] == "none";
        request.max_retries = options["maxRetries"].as_u64().map(|v| v as u32);
        request.max_retry_delay_ms = options["maxRetryDelayMs"].as_u64();
        request.headers = options["headers"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().into()))
            .collect();
        let mut error = None;
        match ProviderClient::default()
            .stream_openai_codex(&endpoint, request)
            .await
        {
            Ok(mut stream) => {
                while let Some(event) = stream.next().await {
                    if let Err(e) = event {
                        error = Some(e.to_string());
                        break;
                    }
                }
            }
            Err(e) => error = Some(e.to_string()),
        }
        assert_eq!(json!(error), case["expectedError"], "{name}");
        let requests = captured.lock().await;
        assert_eq!(
            requests.len(),
            case["requests"].as_array().unwrap().len(),
            "{name}"
        );
        for ((headers, body), expected) in requests.iter().zip(case["requests"].as_array().unwrap())
        {
            assert_eq!(body, &expected["body"], "{name} request");
            for (key, value) in expected["headers"].as_object().unwrap() {
                assert_eq!(
                    headers.get(key).and_then(|v| v.to_str().ok()),
                    value.as_str(),
                    "{name} header {key}"
                );
            }
            if options["cacheRetention"] == "none" {
                assert!(!headers.contains_key("session-id"));
            }
        }
    }
    server.abort();
}
