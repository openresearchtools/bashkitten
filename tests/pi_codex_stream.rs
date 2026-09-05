use axum::{Router, body::Body, response::Response, routing::post};
use bashkitten::{
    agent::ModelCost,
    providers::{
        CodexEndpoint, ProviderAuthStore, ProviderClient, ProviderCredential, ProviderRequest,
    },
    response::ResponseAssembly,
};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

fn compare(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.as_f64(), b.as_f64(), "{path}"),
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: {actual} != {expected}");
            for (key, value) in b {
                compare(&a[key], value, &format!("{path}.{key}"));
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: {actual} != {expected}");
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                compare(a, b, &format!("{path}[{i}]"));
            }
        }
        _ => assert_eq!(actual, expected, "{path}"),
    }
}
#[tokio::test]
async fn pinned_codex_streams_preserve_messages_errors_usage_and_partial_arguments() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pi-codex-stream.json")).unwrap();
    let payload = Arc::new(Mutex::new(String::new()));
    let response_payload = payload.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/backend-api", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/backend-api/codex/responses",
        post(move || {
            let payload = response_payload.clone();
            async move {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(payload.lock().await.clone()))
                    .unwrap()
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = ProviderClient::default();
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
        let data = case["payload"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                case["chunks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|chunk| format!("data: {chunk}\n\n"))
                    .collect::<String>()
            });
        *payload.lock().await = data;
        let endpoint = CodexEndpoint {
            base_url: url.clone(),
            auth_store: auth.clone(),
            headers: BTreeMap::new(),
        };
        let mut events = client
            .stream_openai_codex(&endpoint, {
                let mut request =
                    ProviderRequest::new(case["model"]["id"].as_str().unwrap(), vec![]);
                request.service_tier = case["options"]["serviceTier"].as_str().map(str::to_owned);
                request.transport = Some(bashkitten::codex_websocket::Transport::Sse);
                request
            })
            .await
            .unwrap();
        let mut assembly = ResponseAssembly::default();
        while let Some(event) = events.next().await {
            match event {
                Ok(event) => {
                    assembly.push(event);
                }
                Err(error) => {
                    assembly.fail(error.to_string());
                    break;
                }
            }
        }
        let cost: ModelCost = serde_json::from_value(case["model"]["cost"].clone()).unwrap();
        let mut actual = serde_json::to_value(assembly.message(
            case["model"]["provider"].as_str().unwrap(),
            case["model"]["id"].as_str().unwrap(),
            &cost,
        ))
        .unwrap();
        actual.as_object_mut().unwrap().remove("timestamp");
        compare(&actual, &case["expected"], name);
    }
    server.abort();
}

#[test]
fn malformed_later_frame_preserves_prior_events_and_split_utf8() {
    let mut decoder = bashkitten::codex_stream::Decoder::default();
    let value =
        serde_json::json!({"type":"response.output_text.delta","output_index":0,"delta":"😀 café"});
    let bytes = format!("data: {value}\n\ndata: {{broken\n\n").into_bytes();
    let split = bytes.windows(4).position(|v| v == "😀".as_bytes()).unwrap() + 2;
    assert!(decoder.feed(&bytes[..split], false).is_empty());
    let mut events = decoder.feed(&bytes[split..], false).into_iter();
    assert_eq!(events.next().unwrap().unwrap(), value);
    assert!(
        events
            .next()
            .unwrap()
            .unwrap_err()
            .to_string()
            .starts_with("Invalid Codex SSE JSON:")
    );
    assert!(events.next().is_none());
}
