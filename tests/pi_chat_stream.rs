use axum::{Router, body::Body, response::Response, routing::post};
use bashkitten::{
    agent::ModelCost,
    providers::{EndpointAuth, OpenAiCompatibleEndpoint, ProviderClient, ProviderRequest},
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
async fn pinned_compatible_streams_preserve_messages_errors_usage_and_partial_arguments() {
    let fixture: Value =
        bashkitten::lossless_json::from_str(include_str!("fixtures/pi-chat-stream.json")).unwrap();
    let payload = Arc::new(Mutex::new(String::new()));
    let response_payload = payload.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/v1/chat/completions",
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
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mut data = case["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|chunk| {
                format!(
                    "data: {}\n\n",
                    chunk["raw"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| bashkitten::lossless_json::to_string(chunk).unwrap())
                )
            })
            .collect::<String>();
        if case["done"] == true {
            data.push_str("data: [DONE]\n\n");
        }
        *payload.lock().await = data;
        let endpoint = OpenAiCompatibleEndpoint {
            base_url: url.clone(),
            auth: EndpointAuth::Bearer("fixture-key".into()),
            headers: BTreeMap::new(),
            pi_model: case["model"].clone(),
        };
        let mut events = client
            .stream_openai_compatible(&endpoint, ProviderRequest::new("fixture", vec![]))
            .await
            .unwrap();
        let mut assembly = ResponseAssembly::default();
        while let Some(event) = events.next().await {
            match event {
                Ok(event) => {
                    assembly.push(event);
                }
                Err(error) => {
                    assembly.fail(bashkitten::json_error::exception_message(&error));
                    break;
                }
            }
        }
        let cost: ModelCost = serde_json::from_value(case["model"]["cost"].clone()).unwrap();
        let mut actual = serde_json::to_value(assembly.message(
            case["model"]["provider"].as_str().unwrap(),
            "fixture",
            &cost,
        ))
        .unwrap();
        actual.as_object_mut().unwrap().remove("timestamp");
        compare(&actual, &case["expected"], name);
    }
    server.abort();
}
#[test]
fn pinned_partial_json_and_repair() {
    let fixture: Value =
        bashkitten::lossless_json::from_str(include_str!("fixtures/pi-chat-stream.json")).unwrap();
    for case in fixture["jsonCases"].as_array().unwrap() {
        let input = bashkitten::lossless_json::JsString::from_value(&case["input"]).unwrap();
        compare(
            &bashkitten::streaming_json::parse_streaming_json_js(&input),
            &case["expected"],
            input.as_str(),
        );
    }
}
