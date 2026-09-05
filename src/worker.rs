//! Per-session process runtime. The Web server and GTK controller never own this
//! state; they communicate with it through the session's Unix socket.

use crate::agent::{
    AgentMessage, AgentQueues, ContentBlock, DeliveryKind, MessageContent, ModelCost,
    OrderedJsonValue, SessionEntry, SessionEntryKind, StopReason, Usage, calculate_cost,
};
use crate::config::AppConfig;
use crate::models::{self, ModelInfo};
use crate::paths::AppPaths;
use crate::providers::{
    CodexEndpoint, ContentPart as ProviderContent, MessageRole, NormalizedUsage,
    OpenAiCompatibleEndpoint, ProviderClient, ProviderEndpoint, ProviderEvent, ProviderMessage,
    ProviderRequest, StopReason as ProviderStopReason, ThinkingLevel,
    ToolDefinition as ProviderToolDefinition,
};
use crate::session::{self, ControlReply, ControlRequest, Delivery, QueueAction, SessionHeader};
use crate::tools::{self, ToolContext};
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::Utc;
use futures_util::StreamExt;
use futures_util::future::join_all;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, broadcast};

#[derive(Clone, Debug)]
struct QueuedMessage {
    id: String,
    content: String,
    attachments: Vec<PathBuf>,
    source_session: Option<String>,
    delivery: DeliveryKind,
}

#[derive(Clone, Debug)]
struct PendingModel {
    model: String,
    thinking: String,
}

struct Shared {
    queues: Mutex<AgentQueues<QueuedMessage>>,
    model_change: Mutex<Option<PendingModel>>,
    cwd_change: Mutex<Option<PathBuf>>,
    notify: Notify,
    events: broadcast::Sender<Value>,
    busy: AtomicBool,
    stop: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        let (events, _) = broadcast::channel(512);
        Self {
            queues: Mutex::new(AgentQueues::default()),
            model_change: Mutex::new(None),
            cwd_change: Mutex::new(None),
            notify: Notify::new(),
            events,
            busy: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        }
    }

    fn emit(&self, event: Value) {
        let _ = self.events.send(event);
    }
}

fn queued_message_value(message: &QueuedMessage) -> Value {
    json!({
        "id": message.id,
        "content": message.content,
        "delivery": message.delivery,
        "attachments": message.attachments.iter().filter_map(|path| path.file_name()).map(|name| name.to_string_lossy()).collect::<Vec<_>>()
    })
}

fn queue_state_value(queues: &AgentQueues<QueuedMessage>, busy: bool) -> Value {
    json!({
        "busy": busy,
        "steering": queues.steering.len(),
        "queued": queues.follow_up.len(),
        "steeringMessages": queues.steering.iter().map(queued_message_value).collect::<Vec<_>>(),
        "queuedMessages": queues.follow_up.iter().map(queued_message_value).collect::<Vec<_>>()
    })
}

struct Runtime {
    paths: AppPaths,
    id: String,
    header: SessionHeader,
    config: AppConfig,
    model: ModelInfo,
    thinking: String,
    messages: Vec<ProviderMessage>,
    logical_messages: Vec<AgentMessage>,
    entries: Vec<SessionEntry>,
    pending_entries: Vec<SessionEntry>,
    last_entry_id: Option<String>,
    provider: ProviderClient,
    shared: Arc<Shared>,
}

pub async fn run_worker(paths: AppPaths, id: String) -> Result<()> {
    session::validate_id(&id)?;
    let session_dir = paths.session_dir(&id);
    let mut header = session::read_header(&session_dir)?;
    let entries = load_current_entries(&session_dir)?;
    let mut restored_thinking = header.thinking_level.clone();
    for entry in &entries {
        match &entry.kind {
            SessionEntryKind::ModelChange { provider, model_id } => {
                header.provider = provider.clone();
                header.model_id = model_id.clone();
            }
            SessionEntryKind::ThinkingLevelChange { thinking_level } => {
                restored_thinking = thinking_level.clone();
            }
            _ => {}
        }
    }
    let config = AppConfig::load(&paths)?;
    let authenticated = provider_authenticated(&paths);
    let model_id = format!("{}/{}", header.provider, header.model_id);
    let model = models::find_model(&config, &model_id, authenticated, llama_available())
        .with_context(|| format!("session model is unavailable: {model_id}"))?;
    let last_entry_id = entries.last().map(|entry| entry.id.clone());
    let logical_messages = entries
        .iter()
        .flat_map(SessionEntry::context_messages)
        .collect::<Vec<_>>();
    let messages = logical_messages
        .iter()
        .filter_map(to_provider_message)
        .collect();
    let shared = Arc::new(Shared::new());
    let socket = session::control_socket(&paths, &id)?;
    let listener_shared = shared.clone();
    let listener_socket = socket.clone();
    let listener = tokio::spawn(async move { listen(listener_socket, listener_shared).await });

    wait_for_socket(&socket).await?;
    let mut runtime = Runtime {
        paths,
        id,
        header,
        config,
        model,
        thinking: restored_thinking,
        messages,
        logical_messages,
        entries,
        pending_entries: Vec::new(),
        last_entry_id,
        provider: ProviderClient::new()?,
        shared: shared.clone(),
    };
    runtime.shared.emit(json!({"type":"status","state":"idle"}));

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let had_work = runtime.process_available_work().await?;
        if had_work {
            continue;
        }
        if tokio::time::timeout(Duration::from_millis(1_500), shared.notify.notified())
            .await
            .is_err()
            && !runtime.has_work().await
        {
            break;
        }
    }

    shared.stop.store(true, Ordering::SeqCst);
    listener.abort();
    let _ = fs::remove_file(&socket);
    Ok(())
}

async fn wait_for_socket(path: &Path) -> Result<()> {
    for _ in 0..100 {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    bail!("session control socket was not created")
}

async fn listen(path: PathBuf, shared: Arc<Shared>) -> Result<()> {
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("remove stale {}", path.display()))?;
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let shared = shared.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, shared).await {
                eprintln!("control connection: {error:#}");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, shared: Arc<Shared>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        // Liveness probes connect and immediately close. They are expected and
        // must not turn into noisy worker errors.
        return Ok(());
    };
    let request: ControlRequest = serde_json::from_str(&line)?;
    match request {
        ControlRequest::Subscribe => {
            // Subscribe first, then acknowledge. Once the Web client receives
            // this reply it can safely dispatch work without losing an event.
            let mut events = shared.events.subscribe();
            write_reply(&mut write, true, "subscribed", json!({})).await?;
            while let Ok(event) = events.recv().await {
                write
                    .write_all(serde_json::to_string(&event)?.as_bytes())
                    .await?;
                write.write_all(b"\n").await?;
            }
        }
        ControlRequest::Send {
            delivery,
            content,
            attachments,
            source_session,
        } => {
            let message = QueuedMessage {
                id: uuid::Uuid::now_v7().to_string(),
                content,
                attachments,
                source_session,
                delivery: match delivery {
                    Delivery::Steer => DeliveryKind::Steer,
                    Delivery::Queue => DeliveryKind::Queue,
                },
            };
            let mut queues = shared.queues.lock().await;
            match delivery {
                Delivery::Steer => queues.steering.enqueue(message),
                Delivery::Queue => queues.follow_up.enqueue(message),
            }
            let state = queue_state_value(&queues, shared.busy.load(Ordering::SeqCst));
            drop(queues);
            shared.emit(json!({"type":"queue_state","data":state}));
            shared.notify.notify_one();
            write_reply(&mut write, true, "message queued", state).await?;
        }
        ControlRequest::Status => {
            let queues = shared.queues.lock().await;
            let state = queue_state_value(&queues, shared.busy.load(Ordering::SeqCst));
            write_reply(&mut write, true, "status", state).await?;
        }
        ControlRequest::Stop => {
            shared.stop.store(true, Ordering::SeqCst);
            shared.notify.notify_waiters();
            write_reply(&mut write, true, "stopping", json!({})).await?;
        }
        ControlRequest::ChangeModel { model, thinking } => {
            *shared.model_change.lock().await = Some(PendingModel { model, thinking });
            shared.notify.notify_one();
            write_reply(&mut write, true, "model change queued", json!({})).await?;
        }
        ControlRequest::ChangeCwd { cwd } => {
            let cwd = session::validate_cwd(&cwd)?;
            *shared.cwd_change.lock().await = Some(cwd);
            shared.notify.notify_one();
            write_reply(
                &mut write,
                true,
                "Folder change will apply when the current turn settles",
                json!({}),
            )
            .await?;
        }
        ControlRequest::QueueAction {
            id,
            action,
            content,
        } => {
            let mut queues = shared.queues.lock().await;
            let message = match action {
                QueueAction::Edit => {
                    let value = content.context("edited queue content is required")?;
                    let mut found = false;
                    if let Some(queued) = queues.follow_up.find_mut(|message| message.id == id) {
                        queued.content = value.clone();
                        found = true;
                    }
                    if !found
                        && let Some(queued) = queues.steering.find_mut(|message| message.id == id)
                    {
                        queued.content = value;
                        found = true;
                    }
                    if !found {
                        bail!("queued message no longer exists");
                    }
                    "queued message edited"
                }
                QueueAction::Promote => {
                    let mut queued = queues
                        .follow_up
                        .remove_first(|message| message.id == id)
                        .context("queued message no longer exists")?;
                    queued.delivery = DeliveryKind::Steer;
                    queues.steering.enqueue(queued);
                    "queued message promoted to steering"
                }
                QueueAction::Remove => {
                    let removed = queues
                        .follow_up
                        .remove_first(|message| message.id == id)
                        .is_some()
                        || queues
                            .steering
                            .remove_first(|message| message.id == id)
                            .is_some();
                    if !removed {
                        bail!("queued message no longer exists");
                    }
                    "queued message removed"
                }
            };
            let state = queue_state_value(&queues, shared.busy.load(Ordering::SeqCst));
            drop(queues);
            shared.emit(json!({"type":"queue_state","data":state}));
            write_reply(&mut write, true, message, state).await?;
        }
    }
    Ok(())
}

async fn write_reply(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    ok: bool,
    message: &str,
    data: Value,
) -> Result<()> {
    let reply = ControlReply {
        ok,
        message: message.to_owned(),
        data,
    };
    write
        .write_all(serde_json::to_string(&reply)?.as_bytes())
        .await?;
    write.write_all(b"\n").await?;
    Ok(())
}

impl Runtime {
    async fn has_work(&self) -> bool {
        self.shared.queues.lock().await.has_messages()
            || self.shared.model_change.lock().await.is_some()
            || self.shared.cwd_change.lock().await.is_some()
    }

    async fn process_available_work(&mut self) -> Result<bool> {
        let mut did_work = false;
        let cwd = { self.shared.cwd_change.lock().await.take() };
        if let Some(cwd) = cwd {
            if let Err(error) = self.apply_cwd_change(cwd) {
                self.shared
                    .emit(json!({"type":"cwd_error","message":error.to_string()}));
            }
            did_work = true;
        }
        let change = { self.shared.model_change.lock().await.take() };
        if let Some(change) = change {
            self.apply_model_change(change).await?;
            did_work = true;
        }
        let drained = self.shared.queues.lock().await.drain_at_boundary(true);
        self.emit_queue_state().await;
        let Some(drained) = drained else {
            return Ok(did_work);
        };
        did_work = true;
        for queued in drained.messages {
            self.push_user_message(queued)?;
        }
        self.run_agent_turn().await?;
        self.flush_pending()?;
        Ok(did_work)
    }

    fn apply_cwd_change(&mut self, cwd: PathBuf) -> Result<()> {
        let cwd = session::validate_cwd(&cwd)?;
        self.flush_pending()?;
        let mut header = self.header.clone();
        header.initial_cwd.get_or_insert_with(|| header.cwd.clone());
        header.cwd = cwd;
        let previous_id = self.last_entry_id.clone();
        let event = self.entry(SessionEntryKind::Custom {
            custom_type: "bashkitten.cwd".into(),
            data: Some(json!({"cwd":header.cwd})),
        });
        if let Err(error) = session::replace_current_header(
            &self.paths.session_dir(&self.id),
            &header,
            Some(&serde_json::to_value(&event)?),
        ) {
            self.last_entry_id = previous_id;
            return Err(error);
        }
        self.entries.push(event);
        self.header = header;
        self.shared
            .emit(json!({"type":"cwd_change","cwd":self.header.cwd}));
        Ok(())
    }

    async fn apply_model_change(&mut self, change: PendingModel) -> Result<()> {
        let destination = models::find_model(
            &self.config,
            &change.model,
            provider_authenticated(&self.paths),
            llama_available(),
        )
        .with_context(|| format!("unknown or unavailable model: {}", change.model))?;
        if !destination
            .thinking_levels
            .iter()
            .any(|level| level == &change.thinking)
        {
            bail!("thinking level is not supported by {}", change.model);
        }

        let estimated = crate::agent::estimate_context_tokens(&self.logical_messages).tokens;
        if crate::agent::should_compact(
            estimated,
            destination.context_window,
            crate::agent::CompactionSettings::default(),
        ) {
            bail!(
                "model switch requires compaction before moving to the smaller context; automatic compaction could not safely complete"
            );
        }

        let (provider, model_id) = change
            .model
            .split_once('/')
            .context("model must be provider/model-id")?;
        if self.header.provider != provider || self.header.model_id != model_id {
            let entry = self.entry(SessionEntryKind::ModelChange {
                provider: provider.to_owned(),
                model_id: model_id.to_owned(),
            });
            self.pending_entries.push(entry);
        }
        if self.thinking != change.thinking {
            let entry = self.entry(SessionEntryKind::ThinkingLevelChange {
                thinking_level: change.thinking.clone(),
            });
            self.pending_entries.push(entry);
        }
        self.header.provider = provider.to_owned();
        self.header.model_id = model_id.to_owned();
        self.thinking = change.thinking;
        self.model = destination;
        self.flush_pending()?;
        self.shared
            .emit(json!({"type":"model_change","model":change.model,"thinking":self.thinking}));
        Ok(())
    }

    async fn emit_queue_state(&self) {
        let queues = self.shared.queues.lock().await;
        let state = queue_state_value(&queues, self.shared.busy.load(Ordering::SeqCst));
        self.shared.emit(json!({"type":"queue_state","data":state}));
    }

    fn push_user_message(&mut self, queued: QueuedMessage) -> Result<()> {
        let mut blocks = Vec::new();
        if !queued.content.is_empty() {
            blocks.push(ContentBlock::text(queued.content));
        }
        let mut images = Vec::new();
        for attachment in queued.attachments {
            let attachment = if attachment.is_absolute() {
                attachment
            } else {
                self.header.cwd.join(attachment)
            };
            let attachment = fs::canonicalize(&attachment)
                .with_context(|| format!("open attachment {}", attachment.display()))?;
            if !attachment.is_file() {
                bail!("attachment is not a file: {}", attachment.display());
            }
            let name = attachment
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "attachment".into());
            let mime_type = mime_guess::from_path(&attachment)
                .first_or_octet_stream()
                .essence_str()
                .to_owned();
            blocks.push(ContentBlock::attachment(
                &name,
                attachment.to_string_lossy(),
                &mime_type,
            ));
            let supported_image = matches!(
                mime_type.as_str(),
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            );
            if supported_image && self.model.input.iter().any(|input| input == "image") {
                let data = fs::read(&attachment)
                    .with_context(|| format!("read attachment {}", attachment.display()))?;
                images.push(ContentBlock::Image {
                    data: STANDARD.encode(data),
                    mime_type,
                });
            }
        }
        blocks.extend(images);
        let message = AgentMessage::User {
            content: MessageContent::Blocks(blocks),
            timestamp: Utc::now().timestamp_millis(),
            source_session: queued.source_session,
            delivery: Some(queued.delivery),
        };
        self.logical_messages.push(message.clone());
        if let Some(provider) = to_provider_message(&message) {
            self.messages.push(provider);
        }
        let entry = self.entry(SessionEntryKind::Message {
            message: message.clone(),
        });
        let entry_id = entry.id.clone();
        self.pending_entries.push(entry);
        self.shared
            .emit(json!({"type":"message","message":message,"entryId":entry_id}));
        Ok(())
    }

    async fn run_agent_turn(&mut self) -> Result<()> {
        self.shared.busy.store(true, Ordering::SeqCst);
        self.shared.emit(json!({"type":"agent_start"}));
        let result = self.run_agent_turn_inner().await;
        self.shared.busy.store(false, Ordering::SeqCst);
        self.shared.emit(json!({"type":"agent_end"}));
        result
    }

    async fn run_agent_turn_inner(&mut self) -> Result<()> {
        loop {
            if self.shared.stop.load(Ordering::SeqCst) {
                break;
            }
            self.shared.emit(json!({"type":"turn_start"}));
            let request = self.provider_request()?;
            let endpoint = self.endpoint()?;
            let stream = self.provider.stream(&endpoint, request).await;
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    self.push_provider_error(error.to_string());
                    return Ok(());
                }
            };
            let mut text = String::new();
            let mut thinking = String::new();
            let mut thinking_signature = None;
            let mut response_id = None;
            let mut text_signature = None;
            let mut usage = NormalizedUsage::default();
            let mut calls: BTreeMap<u64, PendingToolCall> = BTreeMap::new();
            let mut stop_reason = ProviderStopReason::Stop;
            let mut raw_stop_reason = None;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(ProviderEvent::Start { response_id: value }) => response_id = value,
                    Ok(ProviderEvent::TextDelta { delta }) => {
                        text.push_str(&delta);
                        self.shared
                            .emit(json!({"type":"assistant_delta","delta":delta}));
                    }
                    Ok(ProviderEvent::TextDone {
                        text: final_text,
                        text_signature: signature,
                    }) => {
                        if let Some(final_text) = final_text {
                            text = final_text;
                        }
                        text_signature = signature;
                    }
                    Ok(ProviderEvent::ThinkingDelta { delta }) => {
                        thinking.push_str(&delta);
                        self.shared
                            .emit(json!({"type":"thinking_delta","delta":delta}));
                    }
                    Ok(ProviderEvent::ThinkingDone {
                        encrypted_content, ..
                    }) => thinking_signature = encrypted_content,
                    Ok(ProviderEvent::ToolCallStart { index, id, name }) => {
                        calls.insert(
                            index,
                            PendingToolCall {
                                id,
                                name,
                                arguments: String::new(),
                            },
                        );
                    }
                    Ok(ProviderEvent::ToolCallDelta {
                        index,
                        arguments_delta,
                    }) => {
                        if let Some(call) = calls.get_mut(&index) {
                            call.arguments.push_str(&arguments_delta);
                        }
                    }
                    Ok(ProviderEvent::ToolCallDone {
                        index,
                        id,
                        name,
                        arguments,
                    }) => {
                        calls.insert(
                            index,
                            PendingToolCall {
                                id,
                                name,
                                arguments,
                            },
                        );
                    }
                    Ok(ProviderEvent::Usage { usage: value }) => usage = value,
                    Ok(ProviderEvent::Done { reason, raw_reason }) => {
                        stop_reason = reason;
                        raw_stop_reason = raw_reason;
                    }
                    Err(error) => {
                        self.push_provider_error(error.to_string());
                        return Ok(());
                    }
                }
            }

            let mut content = Vec::new();
            if !thinking.is_empty() || thinking_signature.is_some() {
                content.push(ContentBlock::Thinking {
                    thinking,
                    thinking_signature,
                    redacted: None,
                });
            }
            if !text.is_empty() {
                content.push(ContentBlock::Text {
                    text,
                    text_signature,
                });
            }
            for call in calls.values() {
                let arguments =
                    serde_json::from_str::<Value>(&call.arguments).unwrap_or_else(|_| json!({}));
                content.push(ContentBlock::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: OrderedJsonValue::from(arguments),
                    thought_signature: None,
                    namespace: None,
                });
            }
            let message = AgentMessage::Assistant {
                content,
                api: if self.header.provider == "openai-codex" {
                    "openai-codex-responses"
                } else {
                    "openai-completions"
                }
                .into(),
                provider: self.header.provider.clone(),
                model: self.header.model_id.clone(),
                response_model: None,
                response_id,
                diagnostics: None,
                usage: normalized_usage(&usage, &self.model.cost),
                stop_reason: map_stop_reason(&stop_reason),
                deferred: None,
                error_message: None,
                raw_stop_reason,
                end_turn: None,
                timestamp: Utc::now().timestamp_millis(),
            };
            self.logical_messages.push(message.clone());
            if let Some(provider) = to_provider_message(&message) {
                self.messages.push(provider);
            }
            let entry = self.entry(SessionEntryKind::Message {
                message: message.clone(),
            });
            let entry_id = entry.id.clone();
            self.pending_entries.push(entry);
            self.shared
                .emit(json!({"type":"message","message":message,"entryId":entry_id}));

            if matches!(
                stop_reason,
                ProviderStopReason::Length | ProviderStopReason::Error
            ) {
                break;
            }
            if calls.is_empty() {
                let steering = self.shared.queues.lock().await.drain_at_boundary(false);
                self.emit_queue_state().await;
                if let Some(steering) = steering {
                    for message in steering.messages {
                        self.push_user_message(message)?;
                    }
                    continue;
                }
                break;
            }

            self.execute_tools(calls.into_values().collect()).await?;
            let steering = self.shared.queues.lock().await.drain_at_boundary(false);
            self.emit_queue_state().await;
            if let Some(steering) = steering {
                for message in steering.messages {
                    self.push_user_message(message)?;
                }
            }
        }
        self.shared.emit(json!({"type":"turn_end"}));
        Ok(())
    }

    fn push_provider_error(&mut self, error: String) {
        let message = AgentMessage::Assistant {
            content: Vec::new(),
            api: String::new(),
            provider: self.header.provider.clone(),
            model: self.header.model_id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            deferred: None,
            error_message: Some(error.clone()),
            raw_stop_reason: None,
            end_turn: None,
            timestamp: Utc::now().timestamp_millis(),
        };
        self.logical_messages.push(message.clone());
        let entry = self.entry(SessionEntryKind::Message {
            message: message.clone(),
        });
        let entry_id = entry.id.clone();
        self.pending_entries.push(entry);
        self.shared
            .emit(json!({"type":"message","message":message,"entryId":entry_id,"error":error}));
    }

    async fn execute_tools(&mut self, calls: Vec<PendingToolCall>) -> Result<()> {
        let mut context = ToolContext::new(&self.header.cwd);
        context.model_supports_images = self.model.input.iter().any(|input| input == "image");
        context
            .session_environment
            .insert("BASHKITTEN_SESSION_ID".into(), self.id.clone());
        context
            .session_environment
            .insert("PI_SESSION_ID".into(), self.id.clone());
        context
            .session_environment
            .insert("PI_PROVIDER".into(), self.header.provider.clone());
        context
            .session_environment
            .insert("PI_MODEL".into(), self.header.model_id.clone());
        context
            .session_environment
            .insert("PI_REASONING_LEVEL".into(), self.thinking.clone());
        if let Some(parent) = &self.header.parent_session {
            context
                .session_environment
                .insert("BASHKITTEN_PARENT_ID".into(), parent.clone());
        }
        for call in &calls {
            let arguments =
                serde_json::from_str::<Value>(&call.arguments).unwrap_or_else(|_| json!({}));
            self.shared.emit(json!({
                "type":"tool_start",
                "id":call.id,
                "name":call.name,
                "arguments":arguments
            }));
        }
        let futures = calls.into_iter().map(|call| {
            let context = context.clone();
            async move {
                let arguments = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
                let result = tools::execute_tool(&call.name, arguments, &context).await;
                (call, result)
            }
        });
        let results = join_all(futures).await;
        for (call, result) in results {
            let (content, details, is_error) = match result {
                Ok(result) => {
                    let mut blocks = Vec::new();
                    for block in result.content {
                        match block {
                            tools::ContentBlock::Text { text } => {
                                blocks.push(ContentBlock::text(text));
                            }
                            tools::ContentBlock::Image { data, mime_type } => {
                                blocks.push(ContentBlock::Image { data, mime_type });
                            }
                        }
                    }
                    (MessageContent::Blocks(blocks), result.details, false)
                }
                Err(error) => {
                    let text = error.to_string();
                    (MessageContent::text(&text), None, true)
                }
            };
            let message = AgentMessage::ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                content,
                details,
                usage: None,
                added_tool_names: None,
                is_error,
                timestamp: Utc::now().timestamp_millis(),
            };
            self.logical_messages.push(message.clone());
            if let Some(provider) = to_provider_message(&message) {
                self.messages.push(provider);
            }
            let entry = self.entry(SessionEntryKind::Message {
                message: message.clone(),
            });
            let entry_id = entry.id.clone();
            self.pending_entries.push(entry);
            self.shared
                .emit(json!({"type":"message","message":message,"entryId":entry_id}));
        }
        Ok(())
    }

    fn provider_request(&self) -> Result<ProviderRequest> {
        let thinking = ThinkingLevel::from_str(&self.thinking)?;
        let mut request = ProviderRequest::new(&self.header.model_id, self.messages.clone());
        request.system_prompt = system_prompt(&self.header.cwd);
        request.tools = tools::tool_definitions()
            .into_iter()
            .map(|tool| ProviderToolDefinition {
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
                strict: false,
            })
            .collect();
        request.thinking = thinking;
        request.max_tokens = Some(self.model.max_tokens);
        request.session_id = Some(self.id.clone());
        request.request_parameters = self.request_parameters();
        Ok(request)
    }

    fn request_parameters(&self) -> Map<String, Value> {
        let preset = if self.header.provider == "llama.cpp" {
            self.config
                .llama
                .models
                .iter()
                .find(|model| model.id == self.header.model_id)
        } else {
            self.config
                .compatible_providers
                .iter()
                .find(|provider| provider.id == self.header.provider)
                .and_then(|provider| {
                    provider
                        .models
                        .iter()
                        .find(|model| model.id == self.header.model_id)
                })
        };
        preset
            .and_then(|preset| preset.request_parameters.as_object().cloned())
            .unwrap_or_default()
    }

    fn endpoint(&self) -> Result<ProviderEndpoint> {
        match self.header.provider.as_str() {
            "openai-codex" => Ok(ProviderEndpoint::OpenAiCodex(CodexEndpoint::for_paths(
                &self.paths,
            ))),
            "llama.cpp" => {
                let preset = self
                    .config
                    .llama
                    .models
                    .iter()
                    .find(|model| model.id == self.header.model_id);
                Ok(ProviderEndpoint::LlamaCpp(
                    OpenAiCompatibleEndpoint::from_llama_config(&self.config.llama, preset),
                ))
            }
            provider_id => {
                let provider = self
                    .config
                    .compatible_providers
                    .iter()
                    .find(|provider| provider.id == provider_id)
                    .with_context(|| format!("configured provider not found: {provider_id}"))?;
                let preset = provider
                    .models
                    .iter()
                    .find(|model| model.id == self.header.model_id)
                    .with_context(|| {
                        format!("configured model not found: {}", self.header.model_id)
                    })?;
                Ok(ProviderEndpoint::OpenAiCompatible(
                    OpenAiCompatibleEndpoint::from_config(provider, preset),
                ))
            }
        }
    }

    fn entry(&mut self, kind: SessionEntryKind) -> SessionEntry {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
        let entry = SessionEntry {
            id: id.clone(),
            parent_id: self.last_entry_id.clone(),
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            kind,
        };
        self.last_entry_id = Some(id);
        entry
    }

    fn flush_pending(&mut self) -> Result<()> {
        if self.pending_entries.is_empty() {
            return Ok(());
        }
        let values = self
            .pending_entries
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        session::append_values(&self.paths, &self.id, &values)?;
        self.entries.append(&mut self.pending_entries);
        Ok(())
    }
}

#[derive(Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn normalized_usage(usage: &NormalizedUsage, model_cost: &ModelCost) -> Usage {
    let mut normalized = Usage {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_tokens,
        cache_write: usage.cache_write_tokens,
        cache_write_1h: None,
        reasoning: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),
        total_tokens: usage.total_tokens,
        cost: Default::default(),
    };
    calculate_cost(&mut normalized, model_cost);
    normalized
}

fn map_stop_reason(reason: &ProviderStopReason) -> StopReason {
    match reason {
        ProviderStopReason::Stop => StopReason::Stop,
        ProviderStopReason::ToolUse => StopReason::ToolUse,
        ProviderStopReason::Length => StopReason::Length,
        ProviderStopReason::Error | ProviderStopReason::ContentFilter => StopReason::Error,
        ProviderStopReason::Unknown => StopReason::Stop,
    }
}

fn provider_authenticated(paths: &AppPaths) -> bool {
    fs::read(paths.provider_auth_file())
        .ok()
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        .and_then(|value| value.get("openai-codex").cloned())
        .is_some()
}

fn llama_available() -> bool {
    Path::new("/usr/bin/llama-server").is_file()
}

fn load_current_entries(dir: &Path) -> Result<Vec<SessionEntry>> {
    let (_, path) = session::current_segment(dir)?;
    let data = fs::read_to_string(&path)?;
    Ok(data
        .lines()
        .skip(1)
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

fn provider_tool_result_output(content: &MessageContent) -> Value {
    match content {
        MessageContent::Text(text) => Value::String(text.clone()),
        MessageContent::Blocks(blocks) => {
            let mut text = Vec::new();
            let mut rich = Vec::new();
            let mut has_image = false;
            for block in blocks {
                match block {
                    ContentBlock::Text { text: value, .. } => {
                        text.push(value.clone());
                        rich.push(json!({ "type": "input_text", "text": value }));
                    }
                    ContentBlock::Image { data, mime_type } => {
                        has_image = true;
                        rich.push(json!({
                            "type": "input_image",
                            "detail": "auto",
                            "image_url": format!("data:{mime_type};base64,{data}"),
                        }));
                    }
                    _ => {}
                }
            }
            if has_image {
                Value::Array(rich)
            } else if text.is_empty() {
                Value::String("(no tool output)".into())
            } else {
                Value::String(text.join("\n"))
            }
        }
    }
}

fn provider_blocks(content: &MessageContent) -> Vec<ProviderContent> {
    match content {
        MessageContent::Text(text) => vec![ProviderContent::Text {
            text: text.clone(),
            text_signature: None,
        }],
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text {
                    text,
                    text_signature,
                } => ProviderContent::Text {
                    text: text.clone(),
                    text_signature: text_signature.clone(),
                },
                ContentBlock::Image { data, mime_type } => ProviderContent::Image {
                    source: crate::providers::ImageSource::Base64 {
                        media_type: mime_type.clone(),
                        data: data.clone(),
                        detail: None,
                    },
                },
                ContentBlock::Attachment {
                    name,
                    path,
                    mime_type,
                } => ProviderContent::Text {
                    text: format!(
                        "Attached file:\n- name: {name}\n- path: {path}\n- media type: {mime_type}"
                    ),
                    text_signature: None,
                },
                ContentBlock::Thinking {
                    thinking,
                    thinking_signature,
                    ..
                } => ProviderContent::Thinking {
                    text: thinking.clone(),
                    id: None,
                    encrypted_content: thinking_signature.clone(),
                },
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => ProviderContent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: serde_json::to_value(arguments).unwrap_or_else(|_| json!({})),
                },
            })
            .collect(),
    }
}

fn to_provider_message(message: &AgentMessage) -> Option<ProviderMessage> {
    match message {
        AgentMessage::User { content, .. } => Some(ProviderMessage {
            role: MessageRole::User,
            content: provider_blocks(content),
        }),
        AgentMessage::Assistant { content, .. } => Some(ProviderMessage {
            role: MessageRole::Assistant,
            content: provider_blocks(&MessageContent::Blocks(content.clone())),
        }),
        AgentMessage::ToolResult {
            tool_call_id,
            content,
            is_error,
            ..
        } => Some(ProviderMessage {
            role: MessageRole::Tool,
            content: vec![ProviderContent::ToolResult {
                tool_call_id: tool_call_id.clone(),
                output: provider_tool_result_output(content),
                is_error: *is_error,
            }],
        }),
        AgentMessage::BashExecution { .. } => {
            crate::agent::bash_execution_to_text(message).map(|text| ProviderMessage {
                role: MessageRole::User,
                content: vec![ProviderContent::Text {
                    text,
                    text_signature: None,
                }],
            })
        }
        AgentMessage::Custom { content, .. } => Some(ProviderMessage {
            role: MessageRole::User,
            content: provider_blocks(content),
        }),
        AgentMessage::BranchSummary { summary, .. } => Some(ProviderMessage {
            role: MessageRole::User,
            content: vec![ProviderContent::Text {
                text: format!(
                    "{}{}{}",
                    crate::agent::BRANCH_SUMMARY_PREFIX,
                    summary,
                    crate::agent::BRANCH_SUMMARY_SUFFIX
                ),
                text_signature: None,
            }],
        }),
        AgentMessage::CompactionSummary { summary, .. } => Some(ProviderMessage {
            role: MessageRole::User,
            content: vec![ProviderContent::Text {
                text: format!(
                    "{}{}{}",
                    crate::agent::COMPACTION_SUMMARY_PREFIX,
                    summary,
                    crate::agent::COMPACTION_SUMMARY_SUFFIX
                ),
                text_signature: None,
            }],
        }),
    }
}

fn system_prompt(cwd: &Path) -> String {
    let definitions = tools::tool_definitions();
    let tool_list = definitions
        .iter()
        .map(|tool| format!("- {}: {}", tool.name, tool.prompt_snippet))
        .collect::<Vec<_>>()
        .join("\n");
    let mut seen = Vec::new();
    for guideline in definitions.iter().flat_map(|tool| &tool.prompt_guidelines) {
        if !seen.contains(guideline) {
            seen.push(guideline.clone());
        }
    }
    let mut guidelines = seen
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>();
    guidelines.push("- Be concise in your responses".into());
    guidelines.push("- Show file paths clearly when working with files".into());
    format!(
        "You are an expert coding assistant operating inside BashKitten, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n{tool_list}\n\nGuidelines:\n{}\n\nOptional skills are ordinary Markdown files in ~/.config/bashkitten/skills/. When a task may benefit from one, use ls to inspect the filenames and read only the files you consider relevant. You may create or edit skill files with the ordinary tools when useful or requested.\n\nCurrent working directory: {}",
        guidelines.join("\n"),
        cwd.display()
    )
}
