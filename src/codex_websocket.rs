//! Pinned Codex WebSocket transport, session cache and continuation rules.
//! Source: packages/ai/src/api/openai-codex-responses.ts at PI_REFERENCE_COMMIT.
use crate::{
    agent::ModelCost,
    providers::{ProviderEvent as Event, ProviderRequest, ProviderStream, StopReason},
    response::ResponseAssembly,
};
use anyhow::{Result, anyhow};
use async_stream::try_stream;
use futures_util::{SinkExt, StreamExt};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpStream, sync::OwnedMutexGuard, task::AbortHandle};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    #[default]
    Auto,
    Sse,
    Websocket,
    WebsocketCached,
}
impl Transport {
    fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Sse => "sse",
            Self::Websocket => "websocket",
            Self::WebsocketCached => "websocket-cached",
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Continuation {
    pub last_request_body: Value,
    pub last_response_id: String,
    pub last_response_items: Vec<Value>,
}
/// JSON.stringify comparison is intentional, including object insertion order.
pub fn cached_body(body: &Value, continuation: &mut Option<Continuation>) -> Value {
    let Some(previous) = continuation.as_ref() else {
        return body.clone();
    };
    let without_input = |v: &Value| {
        let mut v = v.clone();
        if let Some(o) = v.as_object_mut() {
            o.shift_remove("input");
            o.shift_remove("previous_response_id");
        }
        v.to_string()
    };
    let current = body["input"].as_array().cloned().unwrap_or_default();
    let mut baseline = previous.last_request_body["input"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    baseline.extend(previous.last_response_items.clone());
    if previous.last_response_id.is_empty()
        || without_input(body) != without_input(&previous.last_request_body)
        || current.len() < baseline.len()
        || serde_json::to_string(&current[..baseline.len()]).ok()
            != serde_json::to_string(&baseline).ok()
    {
        *continuation = None;
        return body.clone();
    }
    let mut value = body.clone();
    value["previous_response_id"] = json!(previous.last_response_id);
    value["input"] = json!(current[baseline.len()..]);
    value
}
pub fn resolve_url(base: &str) -> String {
    let value = crate::codex_http::resolve_url(base);
    if let Ok(mut url) = url::Url::parse(&value) {
        let scheme = match url.scheme() {
            "https" => "wss",
            "http" => "ws",
            _ => return url.to_string(),
        };
        let _ = url.set_scheme(scheme);
        url.to_string()
    } else {
        value
    }
}
pub fn headers(mut headers: HeaderMap, request_id: &str) -> Result<HeaderMap> {
    headers.remove("accept");
    headers.remove("content-type");
    headers.remove("openai-beta");
    headers.insert(
        "openai-beta",
        HeaderValue::from_static("responses_websockets=2026-02-06"),
    );
    let id = HeaderValue::from_str(request_id)?;
    headers.insert("session-id", id.clone());
    headers.insert("x-client-request-id", id);
    Ok(headers)
}
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Key = (String, String);
struct Entry {
    socket: Arc<tokio::sync::Mutex<Socket>>,
    created: Instant,
    busy: AtomicBool,
    alive: AtomicBool,
    continuation: Mutex<Option<Continuation>>,
    idle: Mutex<Option<AbortHandle>>,
}
#[derive(Default)]
struct Pool {
    entries: Mutex<HashMap<Key, Arc<Entry>>>,
    fallback: Mutex<HashSet<String>>,
}
fn pool() -> Arc<Pool> {
    static POOL: OnceLock<Arc<Pool>> = OnceLock::new();
    POOL.get_or_init(|| Arc::new(Pool::default())).clone()
}
fn remove(pool: &Pool, key: &Key, entry: &Arc<Entry>) {
    let mut map = pool.entries.lock().unwrap();
    if map.get(key).is_some_and(|v| Arc::ptr_eq(v, entry)) {
        map.remove(key);
    }
}
async fn close(socket: &mut Socket, reason: &'static str) {
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        socket.close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: reason.into(),
        })),
    )
    .await;
}
struct Lease {
    entry: Arc<Entry>,
    guard: Option<OwnedMutexGuard<Socket>>,
    key: Option<Key>,
    pool: Arc<Pool>,
    keep: bool,
}
impl Lease {
    fn keep(mut self) {
        self.keep = true;
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let entry = self.entry.clone();
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        if self.keep
            && let Some(key) = self.key.clone()
        {
            drop(guard);
            entry.busy.store(false, Ordering::SeqCst);
            let weak_pool = Arc::downgrade(&self.pool);
            let idle_entry = entry.clone();
            let task = tokio::spawn(async move {
                let mut socket = idle_entry.socket.lock().await;
                let timer = tokio::time::sleep(Duration::from_secs(300));
                tokio::pin!(timer);
                loop {
                    tokio::select! {
                        _=&mut timer=>{if !idle_entry.busy.load(Ordering::SeqCst){close(&mut socket,"idle_timeout").await;}break;}
                        message=socket.next()=>match message {Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break,Some(Ok(Message::Ping(_)))=>{if socket.flush().await.is_err(){break;}},_=>{}}
                    }
                }
                idle_entry.alive.store(false, Ordering::SeqCst);
                if let Some(pool) = weak_pool.upgrade() {
                    remove(&pool, &key, &idle_entry);
                }
            });
            *entry.idle.lock().unwrap() = Some(task.abort_handle());
        } else {
            entry.alive.store(false, Ordering::SeqCst);
            if let Some(key) = &self.key {
                remove(&self.pool, key, &entry);
            }
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    close(&mut guard, "done").await;
                });
            }
        }
    }
}
async fn connect(url: &str, headers: &HeaderMap, timeout: Option<u64>) -> Result<Socket> {
    let mut request = url.into_client_request()?;
    for (name, value) in headers {
        request.headers_mut().insert(name.clone(), value.clone());
    }
    let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = None;
    config.max_frame_size = None;
    // Allowed network: explicitly selected OpenAI subscription inference endpoint.
    let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), false);
    let ms = timeout.unwrap_or(15000);
    let result = if ms == 0 {
        connect.await
    } else {
        tokio::time::timeout(Duration::from_millis(ms), connect)
            .await
            .map_err(|_| anyhow!("WebSocket connect timeout after {ms}ms"))?
    };
    Ok(result.map_err(|error| anyhow!("{error}"))?.0)
}
async fn acquire(
    url: &str,
    headers: &HeaderMap,
    session: Option<&str>,
    account: &str,
    timeout: Option<u64>,
) -> Result<Lease> {
    let pool = pool();
    let key = session
        .filter(|s| !s.is_empty())
        .map(|s| (s.into(), account.into()));
    let cached = key
        .as_ref()
        .and_then(|key| pool.entries.lock().unwrap().get(key).cloned());
    let mut ephemeral = key.is_none();
    if let Some(entry) = cached {
        if let Some(task) = entry.idle.lock().unwrap().take() {
            task.abort();
        }
        if !entry.busy.load(Ordering::SeqCst)
            && entry.created.elapsed() >= Duration::from_secs(55 * 60)
        {
            remove(&pool, key.as_ref().unwrap(), &entry);
            let old = entry.clone();
            tokio::spawn(async move {
                close(&mut *old.socket.lock().await, "connection_age_limit").await;
            });
        } else if entry.alive.load(Ordering::SeqCst)
            && entry
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            let guard = entry.socket.clone().lock_owned().await;
            return Ok(Lease {
                entry,
                guard: Some(guard),
                key,
                pool,
                keep: false,
            });
        } else if entry.busy.load(Ordering::SeqCst) {
            ephemeral = true;
        } else {
            remove(&pool, key.as_ref().unwrap(), &entry);
        }
    }
    let socket = connect(url, headers, timeout).await?;
    let entry = Arc::new(Entry {
        socket: Arc::new(tokio::sync::Mutex::new(socket)),
        created: Instant::now(),
        busy: AtomicBool::new(true),
        alive: AtomicBool::new(true),
        continuation: Mutex::new(None),
        idle: Mutex::new(None),
    });
    let key = if ephemeral { None } else { key };
    if let Some(key) = &key {
        pool.entries
            .lock()
            .unwrap()
            .insert(key.clone(), entry.clone());
    }
    let guard = entry.socket.clone().lock_owned().await;
    Ok(Lease {
        entry,
        guard: Some(guard),
        key,
        pool,
        keep: false,
    })
}
#[derive(Debug)]
struct Failure {
    message: String,
    name: &'static str,
    code: Option<Value>,
    non_transport: bool,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Failure {}
fn api_failure(event: &Value, error: anyhow::Error) -> anyhow::Error {
    let code = if event["type"] == "response.failed" {
        event["response"]["error"]["code"].as_str()
    } else {
        event["code"]
            .as_str()
            .or_else(|| event["error"]["code"].as_str())
    };
    Failure {
        message: error.to_string(),
        name: "CodexApiError",
        code: code.map(|v| json!(v)),
        non_transport: true,
    }
    .into()
}
fn closed(frame: Option<CloseFrame>) -> anyhow::Error {
    let code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(1006);
    let reason = frame
        .as_ref()
        .map(|f| f.reason.as_str())
        .unwrap_or_default();
    let reason = if reason.is_empty() && code == 1009 {
        "message too big"
    } else {
        reason
    };
    Failure {
        message: format!(
            "WebSocket closed {code}{}",
            if reason.is_empty() {
                String::new()
            } else {
                format!(" {reason}")
            }
        ),
        name: "WebSocketCloseError",
        code: Some(json!(code)),
        non_transport: false,
    }
    .into()
}
fn attempt(
    url: String,
    headers: HeaderMap,
    body: Value,
    request: ProviderRequest,
    account: String,
    mut output: ResponseAssembly,
) -> ProviderStream {
    Box::pin(try_stream! {
        let session=if request.disable_cache{None}else{request.session_id.as_deref()};
        let mut lease=acquire(&url,&headers,session,&account,request.websocket_connect_timeout_ms).await?;
        let cached=matches!(request.transport,Some(Transport::Auto|Transport::WebsocketCached)) && lease.key.is_some();
        let request_body=if cached {cached_body(&body,&mut lease.entry.continuation.lock().unwrap())}else{body.clone()};
        let mut payload=json!({"type":"response.create"});payload.as_object_mut().unwrap().extend(request_body.as_object().ok_or_else(||anyhow!("invalid request body"))?.clone());
        let socket=lease.guard.as_mut().unwrap();socket.send(Message::Text(payload.to_string().into())).await?;
        let previous=serde_json::to_value(output.message("openai-codex",&request.model,&ModelCost::default()))?;
        let mut state=crate::codex_stream::State::with_blocks(request.model.clone(),previous["content"].as_array().cloned().unwrap_or_default()).with_service_tier(request.service_tier.clone());let mut started=false;
        loop {
            let next=socket.next();let message=if let Some(ms)=request.timeout_ms.filter(|ms|*ms>0){tokio::time::timeout(Duration::from_millis(ms),next).await.map_err(|_|anyhow!("WebSocket idle timeout after {ms}ms"))?}else{next.await};
            let text=match message.transpose()? {
                Some(Message::Text(value))=>value.to_string(),Some(Message::Binary(value))=>String::from_utf8_lossy(&value).into_owned(),Some(Message::Close(frame))=>Err(closed(frame))?,None=>Err(closed(None))?,Some(Message::Ping(_))=>{socket.flush().await?;continue;},_=>continue,
            };
            if text.is_empty(){continue;}
            let value:Value=serde_json::from_str(&text).map_err(|error|Failure{message:format!("Invalid Codex WebSocket JSON: {error}"),name:"CodexProtocolError",code:None,non_transport:true})?;
            if value["type"].as_str().is_none_or(str::is_empty){continue;}
            // Pi maps API failures before emitting start.
            if matches!(value["type"].as_str(),Some("error"|"response.failed")){state.push(&value).map_err(|error|api_failure(&value,error))?;}
            if !started{started=true;yield Event::Start{response_id:None};}
            let events=state.push(&value).map_err(|error|if matches!(value["type"].as_str(),Some("error"|"response.failed")){api_failure(&value,error)}else{error})?;
            for event in events {output.push(event.clone());yield event;}
            if state.done{break;}
        }
        if cached {
            let message=serde_json::to_value(output.message("openai-codex",&request.model,&ModelCost::default()))?;
            if let Some(id)=message["responseId"].as_str().filter(|v|!v.is_empty()).map(str::to_owned) {
                let model=crate::codex::catalog().into_iter().find(|v|v["id"]==request.model).unwrap_or_else(||json!({"id":request.model,"api":"openai-codex-responses","provider":"openai-codex","input":["text","image"]}));
                let items=crate::codex::convert_messages(&model,&json!({"messages":[message]}))?.into_iter().filter(|v|v["type"]!="function_call_output"&&v["type"]!="custom_tool_call_output").collect();
                *lease.entry.continuation.lock().unwrap()=Some(Continuation{last_request_body:body,last_response_id:id,last_response_items:items});
            }
        }
        lease.keep();
    })
}
/// The public stream owns every network operation. Dropping it cancels the
/// pending handshake, message read, HTTP request, backoff and leased socket.
pub fn stream(
    client: Client,
    url: String,
    sse_headers: HeaderMap,
    body: Value,
    request: ProviderRequest,
    account: String,
    token: String,
) -> Result<ProviderStream> {
    let session = if request.disable_cache {
        None
    } else {
        request.session_id.clone()
    };
    let id = session
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(|v| v.chars().take(64).collect::<String>())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let ws_headers = headers(sse_headers.clone(), &id)?;
    Ok(Box::pin(try_stream! {
        let transport=request.transport.unwrap_or_default();let pool=pool();let mut output=ResponseAssembly::default();let mut start_emitted=false;
        let fallback=session.as_ref().is_some_and(|s|pool.fallback.lock().unwrap().contains(s));
        if transport!=Transport::Sse && !fallback{
            let mut retried_missing=false;let mut retried_limit=false;
            loop {
                let mut started=false;let mut error=None;
                let mut events=attempt(resolve_url(&url),ws_headers.clone(),body.clone(),request.clone(),account.clone(),output.clone());
                while let Some(event)=events.next().await{match event{Ok(event)=>{if matches!(event,Event::Start{..}){started=true;if start_emitted{continue;}start_emitted=true;}output.push(event.clone());yield event;},Err(failure)=>{error=Some(failure);break;}}}
                let Some(error)=error.or_else(||matches!(output.stop_reason,StopReason::Error|StopReason::Aborted).then(||anyhow!(output.error_message().unwrap_or("An unknown error occurred").to_owned())))else{return};
                let failure=error.downcast_ref::<Failure>();let code=failure.and_then(|v|v.code.as_ref()).and_then(Value::as_str);
                if code==Some("previous_response_not_found")&&!retried_missing{retried_missing=true;continue;}
                let limit=!started&&code==Some("websocket_connection_limit_reached");if limit&&!retried_limit{retried_limit=true;continue;}
                if failure.is_some_and(|v|v.non_transport)&&!limit{Err(error)?;unreachable!();}
                let mut details=json!({"configuredTransport":transport.name(),"eventsEmitted":started,"phase":if started{"after_message_stream_start"}else{"before_message_stream_start"},"requestBytes":body.to_string().len()});
                if !started {details["fallbackTransport"]=json!("sse");}
                let mut error_info=json!({"name":failure.map(|v|v.name).unwrap_or("Error"),"message":crate::provider_http::safe_error_body(&error.to_string(),std::slice::from_ref(&token))});if let Some(code)=failure.and_then(|v|v.code.clone()){error_info["code"]=code;}
                yield Event::Diagnostic{diagnostic:json!({"type":"provider_transport_failure","timestamp":chrono::Utc::now().timestamp_millis(),"error":error_info,"details":details})};
                if let Some(session)=&session{pool.fallback.lock().unwrap().insert(session.clone());}
                if started{Err(error)?;unreachable!();}break;
            }
        }
        let response=crate::codex_http::send(&client,url,sse_headers,body,&request,std::slice::from_ref(&token)).await?;
        let mut events=crate::providers::parse_codex_responses_stream(response,request.model,request.service_tier,request.http_idle_timeout_ms);
        while let Some(event)=events.next().await{let event=event?;if matches!(event,Event::Start{..}){if start_emitted{continue;}start_emitted=true;}yield event;}
    }))
}
