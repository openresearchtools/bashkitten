use axum::{
    Router,
    body::Body,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use bashkitten::providers::{
    EndpointAuth, OpenAiCompatibleEndpoint, ProviderClient, ProviderRequest,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;

#[tokio::test]
async fn pinned_compatible_http_errors_retries_and_affinity_headers() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/pi-chat-http.json")).unwrap();
    let responses = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
    let inputs = responses.clone();
    let outputs = captured.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap| {
            let inputs = inputs.clone();
            let outputs = outputs.clone();
            async move {
                let mut output = outputs.lock().await;
                let index = output.len();
                output.push(headers);
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
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        *responses.lock().await = case["responses"].as_array().unwrap().clone();
        captured.lock().await.clear();
        let options = &case["options"];
        let endpoint = OpenAiCompatibleEndpoint {
            base_url: url.clone(),
            auth: EndpointAuth::Bearer("fixture-key".into()),
            headers: BTreeMap::new(),
            pi_model: case["model"].clone(),
        };
        let mut request = ProviderRequest::new("fixture", vec![]);
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
            .stream_openai_compatible(&endpoint, request)
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
        for (actual, expected) in requests.iter().zip(case["requests"].as_array().unwrap()) {
            for (key, value) in expected["headers"].as_object().unwrap() {
                // Explicit no-telemetry difference: no SDK runtime reporting.
                if key.starts_with("x-stainless-") {
                    assert!(!actual.contains_key(key));
                    continue;
                }
                assert_eq!(
                    actual.get(key).and_then(|v| v.to_str().ok()),
                    value.as_str(),
                    "{name}: {key}"
                );
            }
            for key in [
                "session_id",
                "x-session-id",
                "x-session-affinity",
                "x-client-request-id",
            ] {
                assert_eq!(
                    actual.contains_key(key),
                    expected["headers"].get(key).is_some(),
                    "{name}: {key}"
                );
            }
        }
    }
    server.abort();
}

#[tokio::test]
async fn compatible_header_timeout_and_backoff_are_cancellable_and_errors_redact_tokens() {
    use std::time::Duration;
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = count.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let endpoint = OpenAiCompatibleEndpoint {
        base_url: url,
        auth: EndpointAuth::None,
        headers: BTreeMap::new(),
        pi_model: json!({}),
    };
    let mut request = ProviderRequest::new("fixture", vec![]);
    request.timeout_ms = Some(10);
    request.max_retries = Some(2);
    let result = tokio::time::timeout(
        Duration::from_millis(80),
        ProviderClient::default().stream_openai_compatible(&endpoint, request),
    )
    .await;
    assert!(result.is_err());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    let mut request = ProviderRequest::new("fixture", vec![]);
    request.timeout_ms = Some(10);
    let result = ProviderClient::default()
        .stream_openai_compatible(&endpoint, request)
        .await;
    assert_eq!(result.err().unwrap().to_string(), "Request timed out.");
    let error = bashkitten::provider_http::error_message(
        400,
        r#"{"error":{"message":"bad known-secret","access_token":"other-secret","refresh_token":"refresh-secret"}}"#,
        &["known-secret".into()],
    );
    for secret in ["known-secret", "other-secret", "refresh-secret"] {
        assert!(!error.contains(secret));
    }
    assert!(error.contains("<redacted>"));
    server.abort();
}

// User-directed network rule: configured API URLs work directly over HTTP or
// HTTPS. Environment proxy variables must not divert a local vLLM-style API.
#[test]
fn local_api_stays_direct_with_proxy_environment_variables() {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .arg("--exact")
        .arg("pinned_compatible_http_errors_retries_and_affinity_headers");
    for name in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        child.env(name, "http://127.0.0.1:1");
    }
    child.env_remove("NO_PROXY").env_remove("no_proxy");
    let result = child.output().unwrap();
    assert!(
        result.status.success(),
        "Direct API requests failed: {} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
