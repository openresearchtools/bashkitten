use bashkitten::{
    agent::ModelCost,
    codex_websocket::{Continuation, cached_body, resolve_url},
    providers::{
        CodexEndpoint, ProviderAuthStore, ProviderClient, ProviderCredential, ProviderRequest,
    },
    response::ResponseAssembly,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};
use tokio_tungstenite::tungstenite::{
    Message,
    protocol::{CloseFrame, frame::coding::CloseCode},
};
fn normalize(v: Value) -> Value {
    match v {
        Value::Object(v) => Value::Object(
            v.into_iter()
                .filter(|(k, _)| k != "timestamp" && k != "stack")
                .map(|(k, v)| (k, normalize(v)))
                .collect(),
        ),
        Value::Array(v) => Value::Array(v.into_iter().map(normalize).collect()),
        Value::Number(v) => json!(v.as_f64().unwrap()),
        v => v,
    }
}
#[test]
fn pinned_cached_body_and_websocket_url() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pi-codex-websocket.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let mut previous = case
            .get("continuation")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value::<Continuation>(v.clone()).unwrap());
        let actual = cached_body(&case["body"], &mut previous);
        assert_eq!(actual, case["expected"], "{}", case["name"]);
        assert_eq!(
            actual.to_string(),
            case["serialized"].as_str().unwrap(),
            "{}",
            case["name"]
        );
        assert_eq!(previous.is_some(), case["retained"].as_bool().unwrap());
    }
    for case in fixture["urls"].as_array().unwrap() {
        assert_eq!(
            resolve_url(case["base"].as_str().unwrap()),
            case["expected"].as_str().unwrap()
        );
    }
}
#[derive(Default)]
struct Captured {
    requests: Vec<Value>,
    fetch_requests: Vec<Value>,
    connections: usize,
}
fn success(id: &str) -> Vec<Value> {
    vec![
        json!({"type":"response.created","response":{"id":id}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":format!("msg_{id}"),"role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":format!("Answer {id}"),"annotations":[]}]}}),
        json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[]}}),
    ]
}
#[tokio::test]
async fn pinned_websocket_reuse_continuation_retry_and_fallback() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pi-codex-websocket-stream.json")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let auth = ProviderAuthStore::new(tmp.path().join("auth.json"));
    auth.set_codex(Some(ProviderCredential::OAuth {
        access: fixture["token"].as_str().unwrap().into(),
        refresh: "fixture-refresh".into(),
        expires: chrono::Utc::now().timestamp_millis() + 3_600_000,
        extra: BTreeMap::new(),
    }))
    .await
    .unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let spec = &case["spec"];
        let name = spec["name"].as_str().unwrap();
        let plans = Arc::new(Mutex::new(spec["plans"].as_array().unwrap().clone()));
        let captured = Arc::new(Mutex::new(Captured::default()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/backend-api", listener.local_addr().unwrap());
        let response_plans = plans.clone();
        let received = captured.clone();
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let plans = response_plans.clone();
                let received = received.clone();
                tasks.spawn(async move{let mut start=[0;4];let n=stream.peek(&mut start).await.unwrap();if n==0{return;}
if start.starts_with(b"GET "){
   received.lock().await.connections+=1;let mut socket=tokio_tungstenite::accept_async(stream).await.unwrap();while let Some(Ok(message))=socket.next().await {let Message::Text(text)=message else {continue};received.lock().await.requests.push(serde_json::from_str(&text).unwrap());let plan={let mut plans=plans.lock().await;assert!(!plans.is_empty(),"unexpected WebSocket request");plans.remove(0)};for frame in plan.as_array().unwrap(){tokio::time::sleep(std::time::Duration::from_millis(2)).await;if frame["wait"]==true{continue;}
if let Some(close)=frame.get("close"){let _=socket.send(Message::Close(Some(CloseFrame{code:CloseCode::from(close["code"].as_u64().unwrap() as u16),reason:close["reason"].as_str().unwrap().to_owned().into()}))).await;return;}
if socket.send(Message::Text(frame["raw"].as_str().map(str::to_owned).unwrap_or_else(||frame.to_string()).into())).await.is_err(){return;}}}
  }else{
   let mut bytes=Vec::new();let end=loop{let mut chunk=[0;4096];let n=stream.read(&mut chunk).await.unwrap();if n==0{return;}bytes.extend_from_slice(&chunk[..n]);if let Some(i)=bytes.windows(4).position(|v|v==b"\r\n\r\n"){break i+4;}};let headers=String::from_utf8_lossy(&bytes[..end]);let length=headers.lines().find_map(|l|l.to_lowercase().strip_prefix("content-length: ").map(|v|v.parse::<usize>().unwrap())).unwrap();while bytes.len()-end<length{let mut chunk=[0;4096];let n=stream.read(&mut chunk).await.unwrap();if n==0{return;}bytes.extend_from_slice(&chunk[..n]);}let decoded=zstd::bulk::decompress(&bytes[end..end+length],1_000_000).unwrap();let mut captured=received.lock().await;captured.fetch_requests.push(serde_json::from_slice(&decoded).unwrap());let body=success(&format!("sse{}",captured.fetch_requests.len())).iter().map(|v|format!("data: {v}\n\n")).collect::<String>();drop(captured);let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());stream.write_all(response.as_bytes()).await.unwrap();
  }});
            }
        });
        let client = ProviderClient::default();
        let endpoint = CodexEndpoint {
            base_url: url,
            auth_store: auth.clone(),
            headers: BTreeMap::new(),
        };
        let mut messages = Vec::new();
        for turn in 0..spec["turns"].as_u64().unwrap() {
            messages
                .push(json!({"role":"user","content":format!("Question {turn}"),"timestamp":turn}));
            let mut request = ProviderRequest::new("gpt-5.5", vec![]);
            request.logical_messages = Some(messages.clone());
            request.system_prompt = if spec["changed"] == true && turn > 0 {
                "Changed"
            } else {
                "System"
            }
            .into();
            request.session_id = Some(format!("fixture-{name}"));
            request.transport = if spec["absent"] == true {
                None
            } else {
                Some(
                    serde_json::from_value(spec.get("transport").cloned().unwrap_or(json!("auto")))
                        .unwrap(),
                )
            };
            request.disable_cache = spec["none"] == true;
            request.timeout_ms = spec["timeoutMs"].as_u64();
            let mut events = client
                .stream_openai_codex(&endpoint, request)
                .await
                .unwrap();
            let mut output = ResponseAssembly::default();
            while let Some(event) = events.next().await {
                match event {
                    Ok(event) => {
                        output.push(event);
                    }
                    Err(error) => {
                        output.fail(bashkitten::json_error::exception_message(&error));
                        break;
                    }
                }
            }
            let message = serde_json::to_value(output.message(
                "openai-codex",
                "gpt-5.5",
                &ModelCost::default(),
            ))
            .unwrap();
            assert_eq!(
                normalize(message.clone()),
                normalize(case["outputs"][turn as usize]["message"].clone()),
                "{name} turn {turn}"
            );
            messages.push(message);
        }
        let result = captured.lock().await;
        assert_eq!(
            result.connections,
            case["connections"].as_u64().unwrap() as usize,
            "{name} connections"
        );
        assert_eq!(json!(result.requests), case["requests"], "{name} requests");
        assert_eq!(
            json!(result.fetch_requests),
            case["fetchRequests"],
            "{name} fallback requests"
        );
        server.abort();
    }
}

#[tokio::test]
async fn cancellation_closes_handshake_and_active_socket_and_allows_fresh_request() {
    use bashkitten::codex_websocket::Transport;
    use bashkitten::providers::ProviderEvent;
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pi-codex-websocket-stream.json")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let auth = ProviderAuthStore::new(tmp.path().join("auth.json"));
    auth.set_codex(Some(ProviderCredential::OAuth {
        access: fixture["token"].as_str().unwrap().into(),
        refresh: "fixture-refresh".into(),
        expires: chrono::Utc::now().timestamp_millis() + 3_600_000,
        extra: BTreeMap::new(),
    }))
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = CodexEndpoint {
        base_url: format!("http://{}/backend-api", listener.local_addr().unwrap()),
        auth_store: auth.clone(),
        headers: BTreeMap::new(),
    };
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 8192];
        let n = socket.read(&mut bytes).await.unwrap();
        assert!(n > 0);
        seen_tx.send(()).unwrap();
        loop {
            if socket.read(&mut bytes).await.unwrap() == 0 {
                break;
            }
        }
    });
    let mut request = ProviderRequest::new("gpt-5.5", vec![]);
    request.transport = Some(Transport::Auto);
    request.websocket_connect_timeout_ms = Some(5000);
    request.session_id = Some(uuid::Uuid::now_v7().to_string());
    let mut stream = ProviderClient::default()
        .stream_openai_codex(&endpoint, request.clone())
        .await
        .unwrap();
    let read = tokio::spawn(async move { stream.next().await });
    seen_rx.await.unwrap();
    read.abort();
    assert!(read.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = CodexEndpoint {
        base_url: format!("http://{}/backend-api", listener.local_addr().unwrap()),
        auth_store: auth,
        headers: BTreeMap::new(),
    };
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        socket.next().await.unwrap().unwrap();
        for value in [
            json!({"type":"response.created","response":{"id":"partial"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_partial","content":[]}}),
            json!({"type":"response.output_text.delta","output_index":0,"delta":"preserve partial"}),
        ] {
            socket
                .send(Message::Text(value.to_string().into()))
                .await
                .unwrap();
        }
        let message = socket.next().await.unwrap().unwrap();
        let Message::Close(Some(frame)) = message else {
            panic!("expected close")
        };
        assert_eq!(frame.code, CloseCode::Normal);
        closed_tx.send(()).unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        let message = socket.next().await.unwrap().unwrap();
        let request: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        assert!(request.get("previous_response_id").is_none());
        for value in success("fresh") {
            socket
                .send(Message::Text(value.to_string().into()))
                .await
                .unwrap();
        }
    });
    let mut stream = ProviderClient::default()
        .stream_openai_codex(&endpoint, request.clone())
        .await
        .unwrap();
    let (partial_tx, partial_rx) = tokio::sync::oneshot::channel();
    let read = tokio::spawn(async move {
        let mut notify = Some(partial_tx);
        let mut response = ResponseAssembly::default();
        while let Some(event) = stream.next().await {
            let event = event.unwrap();
            if matches!(event, ProviderEvent::TextDelta { .. })
                && let Some(notify) = notify.take()
            {
                notify.send(()).unwrap();
            }
            response.push(event);
        }
    });
    partial_rx.await.unwrap();
    read.abort();
    assert!(read.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(1), closed_rx)
        .await
        .unwrap()
        .unwrap();
    let mut stream = ProviderClient::default()
        .stream_openai_codex(&endpoint, request)
        .await
        .unwrap();
    let mut response = ResponseAssembly::default();
    while let Some(event) = stream.next().await {
        response.push(event.unwrap());
    }
    let value =
        serde_json::to_value(response.message("openai-codex", "gpt-5.5", &ModelCost::default()))
            .unwrap();
    assert_eq!(value["content"][0]["text"], "Answer fresh");
    server.await.unwrap();
}
