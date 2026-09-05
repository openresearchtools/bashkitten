use axum::{Router, body::Body, response::Response, routing::post};
use bashkitten::{
    agent::ModelCost,
    codex_websocket::Transport,
    providers::{
        CodexEndpoint, EndpointAuth, OpenAiCompatibleEndpoint, ProviderAuthStore, ProviderClient,
        ProviderCredential, ProviderEvent, ProviderRequest,
    },
    response::ResponseAssembly,
};
use futures_util::StreamExt;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;
fn normalize(value: Value) -> Value {
    match value {
        Value::Object(value) => Value::Object(
            value
                .into_iter()
                .filter(|(k, _)| k != "timestamp")
                .map(|(k, v)| (k, normalize(v)))
                .collect(),
        ),
        Value::Array(value) => Value::Array(value.into_iter().map(normalize).collect()),
        Value::Number(value) => serde_json::json!(value.as_f64().unwrap()),
        value => value,
    }
}
#[tokio::test]
async fn pinned_http_idle_and_abort_preserve_partial_messages() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/pi-idle.json")).unwrap();
    let payload = Arc::new(Mutex::new(String::new()));
    let data = payload.clone();
    let app=Router::new().fallback(post(move||{let data=data.clone();async move {let payload=data.lock().await.clone();let stream=async_stream::stream!{yield Ok::<_,std::io::Error>(bytes::Bytes::from(payload));std::future::pending::<()>().await;};Response::builder().header("content-type","text/event-stream").body(Body::from_stream(stream)).unwrap()}}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
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
        let model = &case["model"];
        *payload.lock().await = case["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect();
        let mut request = ProviderRequest::new(model["id"].as_str().unwrap(), vec![]);
        request.transport = Some(Transport::Sse);
        request.http_idle_timeout_ms = Some(10);
        let client = ProviderClient::default();
        let mut stream = if case["kind"] == "codex" {
            client
                .stream_openai_codex(
                    &CodexEndpoint {
                        base_url: format!("{url}/backend-api"),
                        auth_store: auth.clone(),
                        headers: BTreeMap::new(),
                    },
                    request,
                )
                .await
                .unwrap()
        } else {
            client
                .stream_openai_compatible(
                    &OpenAiCompatibleEndpoint {
                        base_url: format!("{url}/v1"),
                        auth: EndpointAuth::None,
                        headers: BTreeMap::new(),
                        pi_model: model.clone(),
                    },
                    request,
                )
                .await
                .unwrap()
        };
        let mut response = ResponseAssembly::default();
        while let Some(event) = stream.next().await {
            match event {
                Ok(event) => {
                    let abort =
                        case["abort"] == true && matches!(event, ProviderEvent::TextDelta { .. });
                    response.push(event);
                    if abort {
                        response.abort();
                        break;
                    }
                }
                Err(error) => {
                    response.fail(error.to_string());
                    break;
                }
            }
        }
        drop(stream);
        let actual = serde_json::to_value(response.message(
            model["provider"].as_str().unwrap(),
            model["id"].as_str().unwrap(),
            &ModelCost::default(),
        ))
        .unwrap();
        assert_eq!(
            normalize(actual),
            normalize(case["expected"].clone()),
            "{}",
            case["name"]
        );
    }
    server.abort();
}
