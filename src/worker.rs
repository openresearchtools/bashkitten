//! Per-session process runtime. The Web server and GTK controller never own this
//! state; they communicate with it through the session's Unix socket.

use crate::agent::{
    self as agent, AgentMessage, AgentQueues, ContentBlock, DeliveryKind, MessageContent,
    SessionEntry, SessionEntryKind,
};
use crate::config::AppConfig;
use crate::models::{self, ModelInfo};
use crate::paths::AppPaths;
use crate::providers::{
    CodexEndpoint, ContentPart as ProviderContent, MessageRole, OpenAiCompatibleEndpoint,
    ProviderClient, ProviderEndpoint, ProviderMessage, ProviderRequest,
    StopReason as ProviderStopReason, ThinkingLevel, ToolDefinition as ProviderToolDefinition,
};
use crate::response::{PendingToolCall, ResponseAssembly};
use crate::session::{self, ControlReply, ControlRequest, Delivery, QueueAction, SessionHeader};
use crate::tools::{self, ToolContext};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use futures_util::StreamExt;
use futures_util::future::join_all;
use serde_json::{Map, Value, json};
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
    editing: bool,
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
    cancellation: std::sync::Mutex<tools::CancellationToken>,
    compaction: Mutex<Option<Option<String>>>,
    replay: std::sync::Mutex<LiveReplay>,
    usage: std::sync::Mutex<Value>,
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
            cancellation: std::sync::Mutex::new(tools::CancellationToken::default()),
            compaction: Mutex::new(None),
            replay: std::sync::Mutex::new(LiveReplay::default()),
            usage: std::sync::Mutex::new(Value::Null),
        }
    }

    fn emit(&self, event: Value) {
        let mut replay = self.replay.lock().expect("live replay lock");
        if event["type"] == "usage" {
            *self.usage.lock().expect("usage lock") = event["data"].clone();
        }
        replay.push(&event);
        let _ = self.events.send(event);
    }

    fn cancellation(&self) -> tools::CancellationToken {
        self.cancellation.lock().expect("cancellation lock").clone()
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.cancellation().cancel();
        self.notify.notify_waiters();
    }
}

/// Only the uncommitted turn, never the session's older JSONLs. Coalesce deltas
/// and output updates so a long-running command does not accumulate snapshots.
#[derive(Default)]
struct LiveReplay {
    events: Vec<Value>,
}

impl LiveReplay {
    fn push(&mut self, event: &Value) {
        let kind = event["type"].as_str().unwrap_or_default();
        if kind == "message" {
            self.events.retain(|value| {
                !matches!(
                    value["type"].as_str(),
                    Some(
                        "assistant_delta"
                            | "thinking_delta"
                            | "tool_call_start"
                            | "tool_call_delta"
                    )
                )
            });
        }
        if matches!(
            kind,
            "assistant_delta" | "thinking_delta" | "tool_call_delta"
        ) && let Some(previous) = self
            .events
            .iter_mut()
            .rev()
            .find(|value| value["type"] == event["type"] && value["index"] == event["index"])
        {
            let mut text = previous["delta"].as_str().unwrap_or_default().to_owned();
            text.push_str(event["delta"].as_str().unwrap_or_default());
            previous["delta"] = Value::String(text);
            return;
        }
        if matches!(kind, "tool_update" | "queue_state" | "usage") {
            self.events
                .retain(|value| value["type"] != event["type"] || value["id"] != event["id"]);
        }
        if kind == "tool_end" {
            self.events
                .retain(|value| value["type"] != "tool_update" || value["id"] != event["id"]);
        }
        self.events.push(event.clone());
    }
}

fn queued_message_value(message: &QueuedMessage) -> Value {
    json!({
        "id": message.id,
        "content": message.content,
        "delivery": message.delivery,
        "editing": message.editing,
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
    system_prompt: String,
    messages: Vec<ProviderMessage>,
    logical_messages: Vec<AgentMessage>,
    entries: Vec<SessionEntry>,
    pending_entries: Vec<SessionEntry>,
    last_entry_id: Option<String>,
    provider: ProviderClient,
    shared: Arc<Shared>,
    retry_attempt: u32,
    overflow_recovery_attempted: bool,
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
    let model = models::resolve_model(
        &config,
        &model_id,
        &restored_thinking,
        authenticated,
        llama_available(),
    )?;
    // The disk header is the segment's historical baseline; the in-memory
    // header supplies the active model checkpoint at the next compaction.
    header.model_parameters = model.parameters.clone();
    let last_entry_id = entries.last().map(|entry| entry.id.clone());
    let logical_messages = crate::agent::build_session_context(&entries, None);
    let messages = logical_messages
        .iter()
        .filter_map(to_provider_message)
        .collect();
    let system_prompt = crate::prompt::load(&paths, &header.cwd);
    let shared = Arc::new(Shared::new());
    let socket = session::control_socket(&paths, &id)?;
    let listener_shared = shared.clone();
    let socket_listener = bind_control_socket(&socket)?;
    let listener = tokio::spawn(async move { listen(socket_listener, listener_shared).await });

    let signal_shared = shared.clone();
    let signals = tokio::spawn(async move {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = terminate.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
        signal_shared.stop();
        Ok::<_, std::io::Error>(())
    });

    let mut runtime = Runtime {
        paths,
        id,
        header,
        config,
        model,
        thinking: restored_thinking,
        system_prompt,
        messages,
        logical_messages,
        entries,
        pending_entries: Vec::new(),
        last_entry_id,
        provider: ProviderClient::new()?,
        shared: shared.clone(),
        retry_attempt: 0,
        overflow_recovery_attempted: false,
    };
    runtime.emit_usage();
    runtime.shared.emit(json!({"type":"status","state":"idle"}));

    let result: Result<()> = async {
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
        Ok(())
    }
    .await;

    // Cancellation, model/provider errors and shutdown use the same persistence
    // boundary as a settled turn. Do not lose partial assistant/tool messages.
    let persisted = runtime.flush_pending();
    shared.stop();
    listener.abort();
    signals.abort();
    let _ = fs::remove_file(&socket);
    persisted?;
    result
}

#[cfg(test)]
async fn wait_for_socket(path: &Path) -> Result<()> {
    for _ in 0..100 {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    bail!("session control socket was not created")
}

fn bind_control_socket(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if session::socket_is_live(path) {
            bail!("Session already has a running worker");
        }
        fs::remove_file(path).with_context(|| format!("remove stale {}", path.display()))?;
    }
    let address = session::socket_address(path)?;
    let listener =
        UnixListener::bind(address.as_ref()).with_context(|| format!("bind {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn listen(listener: UnixListener, shared: Arc<Shared>) -> Result<()> {
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
            // Snapshot and subscription share the publisher lock: no event can
            // fall between them. Entry IDs deduplicate a concurrent disk flush.
            let (mut events, snapshot) = {
                let replay = shared.replay.lock().expect("live replay lock");
                (
                    shared.events.subscribe(),
                    json!({"events":replay.events,"busy":shared.busy.load(Ordering::SeqCst),"usage":*shared.usage.lock().expect("usage lock")}),
                )
            };
            write_reply(&mut write, true, "subscribed", snapshot).await?;
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
                editing: false,
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
            let mut state = state;
            state["usage"] = shared.usage.lock().expect("usage lock").clone();
            write_reply(&mut write, true, "status", state).await?;
        }
        ControlRequest::Stop => {
            write_reply(&mut write, true, "stopping", json!({})).await?;
            // Put the acknowledgment on the socket before waking an idle main
            // loop, which may otherwise exit the process before replying.
            shared.stop();
        }
        ControlRequest::Compact {
            custom_instructions,
        } => {
            *shared.compaction.lock().await = Some(custom_instructions);
            shared.cancellation().cancel();
            write_reply(&mut write, true, "compaction requested", json!({})).await?;
            shared.notify.notify_one();
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
                QueueAction::BeginEdit | QueueAction::CancelEdit => {
                    let editing = matches!(action, QueueAction::BeginEdit);
                    let queued = if let Some(queued) =
                        queues.follow_up.find_mut(|message| message.id == id)
                    {
                        queued
                    } else {
                        queues
                            .steering
                            .find_mut(|message| message.id == id)
                            .context("queued message no longer exists")?
                    };
                    queued.editing = editing;
                    if editing {
                        "queued message held for editing"
                    } else {
                        "queue edit cancelled"
                    }
                }
                QueueAction::Edit => {
                    let value = content.context("edited queue content is required")?;
                    let mut found = false;
                    if let Some(queued) = queues.follow_up.find_mut(|message| message.id == id) {
                        queued.content = value.clone();
                        queued.editing = false;
                        found = true;
                    }
                    if !found
                        && let Some(queued) = queues.steering.find_mut(|message| message.id == id)
                    {
                        queued.content = value;
                        queued.editing = false;
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
            shared.notify.notify_one();
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
            || self.shared.compaction.lock().await.is_some()
    }

    async fn process_available_work(&mut self) -> Result<bool> {
        let mut did_work = false;
        let compaction = { self.shared.compaction.lock().await.take() };
        if let Some(instructions) = compaction {
            self.flush_pending()?;
            {
                let mut token = self.shared.cancellation.lock().expect("cancellation lock");
                if self.shared.stop.load(Ordering::SeqCst) {
                    return Ok(false);
                }
                *token = tools::CancellationToken::default();
            }
            self.shared.busy.store(true, Ordering::SeqCst);
            let _ = self.compact("manual", false, instructions.as_deref()).await;
            self.shared.busy.store(false, Ordering::SeqCst);
            did_work = true;
        }
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
            if let Err(error) = self.apply_model_change(change).await {
                self.shared
                    .emit(json!({"type":"model_error","message":error.to_string()}));
            }
            did_work = true;
        }
        let drained = self
            .shared
            .queues
            .lock()
            .await
            .drain_at_boundary_if(true, |message| !message.editing);
        self.emit_queue_state().await;
        let Some(drained) = drained else {
            return Ok(did_work);
        };
        did_work = true;
        if let Some(last) = self
            .logical_messages
            .iter()
            .rev()
            .find(|message| matches!(message, AgentMessage::Assistant { .. }))
            .cloned()
        {
            let _ = self.check_compaction(&last, false).await?;
        }
        for queued in drained.messages {
            self.push_user_message(queued).await?;
        }
        self.run_agent_turn().await?;
        self.flush_pending()?;
        Ok(did_work)
    }

    fn apply_cwd_change(&mut self, cwd: PathBuf) -> Result<()> {
        let cwd = session::validate_cwd(&cwd)?;
        let system_prompt = crate::prompt::load(&self.paths, &cwd);
        self.flush_pending()?;
        // Folder updates must retain the segment's historical model baseline.
        // self.header tracks the live model and may already include later
        // model_change entries, which an earlier-message fork does not retain.
        let mut header = session::read_header(&self.paths.session_dir(&self.id))?;
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
        self.header.cwd = header.cwd;
        self.header.initial_cwd = header.initial_cwd;
        self.system_prompt = system_prompt;
        self.shared
            .emit(json!({"type":"cwd_change","cwd":self.header.cwd}));
        Ok(())
    }

    async fn apply_model_change(&mut self, change: PendingModel) -> Result<()> {
        let destination_config = AppConfig::load(&self.paths)?;
        let destination = models::resolve_model(
            &destination_config,
            &change.model,
            &change.thinking,
            provider_authenticated(&self.paths),
            llama_available(),
        )?;

        let entries: Vec<_> = self
            .entries
            .iter()
            .chain(&self.pending_entries)
            .cloned()
            .collect();
        let estimated = agent::current_context_usage(
            &entries,
            &self.logical_messages,
            self.model.context_window,
        )
        .and_then(|usage| usage.tokens)
        .unwrap_or_else(|| {
            self.logical_messages
                .iter()
                .map(agent::estimate_tokens)
                .sum()
        });
        if crate::agent::should_compact(
            estimated,
            destination.context_window,
            agent::CompactionSettings {
                enabled: true,
                ..self.config.compaction
            },
        ) {
            if !self.compact("model_switch", false, None).await? {
                bail!("model switch requires compaction, but there is nothing to compact");
            }
            let estimated: u64 = self
                .logical_messages
                .iter()
                .map(agent::estimate_tokens)
                .sum();
            if agent::should_compact(
                estimated,
                destination.context_window,
                agent::CompactionSettings {
                    enabled: true,
                    ..self.config.compaction
                },
            ) {
                bail!(
                    "compacted context still exceeds the destination model threshold; current model unchanged"
                );
            }
        }

        let (provider, model_id) = change
            .model
            .split_once('/')
            .context("model must be provider/model-id")?;
        // Pinned AgentSession.setModel records every explicit selection,
        // including reselecting the same model after its preset was edited.
        let entry = self.entry(SessionEntryKind::ModelChange {
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
        });
        self.pending_entries.push(entry);
        if self.thinking != change.thinking {
            let entry = self.entry(SessionEntryKind::ThinkingLevelChange {
                thinking_level: change.thinking.clone(),
            });
            self.pending_entries.push(entry);
        }
        self.config = destination_config;
        self.header.provider = provider.to_owned();
        self.header.model_id = model_id.to_owned();
        self.thinking = change.thinking;
        self.header.model_parameters = destination.parameters.clone();
        self.model = destination;
        self.flush_pending()?;
        self.shared
            .emit(json!({"type":"model_change","model":change.model,"thinking":self.thinking}));
        self.emit_usage();
        Ok(())
    }

    async fn emit_queue_state(&self) {
        let queues = self.shared.queues.lock().await;
        let state = queue_state_value(&queues, self.shared.busy.load(Ordering::SeqCst));
        self.shared.emit(json!({"type":"queue_state","data":state}));
    }

    async fn push_user_message(&mut self, queued: QueuedMessage) -> Result<()> {
        self.overflow_recovery_attempted = false;
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
            let path = attachment.clone();
            let processed = tokio::task::spawn_blocking(move || -> Result<_> {
                use std::io::Read as _;
                let mut prefix = Vec::new();
                fs::File::open(&path)?.take(4100).read_to_end(&mut prefix)?;
                Ok(if let Some(mime) = crate::image::detect_mime(&prefix) {
                    Some(crate::image::process(
                        &fs::read(&path)?,
                        mime,
                        true,
                        Default::default(),
                    ))
                } else {
                    None
                })
            })
            .await??;
            if let Some(processed) = processed {
                let hints = match processed {
                    Ok(image) => {
                        // Preserve images in logical history; each provider filters
                        // by the selected model when constructing its request.
                        images.push(ContentBlock::Image {
                            data: image.data,
                            mime_type: image.mime_type,
                        });
                        image.hints.join("\n")
                    }
                    Err(message) => message.to_owned(),
                };
                if !hints.is_empty() {
                    blocks.push(ContentBlock::text(format!(
                        "<file name=\"{}\">{hints}</file>\n",
                        attachment.display()
                    )));
                }
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
        self.emit_usage();
        Ok(())
    }

    async fn run_agent_turn(&mut self) -> Result<()> {
        self.shared.busy.store(true, Ordering::SeqCst);
        let result = async {
            loop {
                self.shared.emit(json!({"type":"agent_start"}));
                let result = self.run_agent_turn_inner().await;
                self.shared.emit(json!({"type":"agent_end"}));
                let Some(message) = result? else { break; };
                if self.shared.cancellation().is_cancelled() { break; }
                if self.prepare_retry(&message).await { continue; }
                if self.shared.cancellation().is_cancelled() { break; }
                if self.retry_attempt > 0 {
                    let error = match &message { AgentMessage::Assistant { error_message, .. } => error_message.clone(), _ => None };
                    self.shared.emit(json!({"type":"auto_retry_end","success":false,"attempt":self.retry_attempt,"finalError":error}));
                    self.retry_attempt = 0;
                }
                if self.check_compaction(&message, true).await? { continue; }
                break;
            }
            Ok(())
        }.await;
        self.shared.busy.store(false, Ordering::SeqCst);
        self.shared.emit(json!({"type":"agent_settled"}));
        result
    }

    async fn run_agent_turn_inner(&mut self) -> Result<Option<AgentMessage>> {
        let mut last_message = None;
        loop {
            if self.shared.cancellation().is_cancelled() {
                break;
            }
            self.shared.emit(json!({"type":"turn_start"}));
            let request = self.provider_request()?;
            let endpoint = self.endpoint()?;
            let response = self.collect_response(&endpoint, request, true).await;
            let message = response.message(
                &self.header.provider,
                &self.header.model_id,
                &self.model.cost,
            );
            last_message = Some(message.clone());
            if !matches!(
                response.stop_reason,
                ProviderStopReason::Error | ProviderStopReason::Length
            ) {
                self.overflow_recovery_attempted = false;
            }
            if response.stop_reason != ProviderStopReason::Error && self.retry_attempt > 0 {
                self.shared.emit(
                    json!({"type":"auto_retry_end","success":true,"attempt":self.retry_attempt}),
                );
                self.retry_attempt = 0;
            }
            let stop_reason = response.stop_reason.clone();
            let calls = response.calls;
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
            self.emit_usage();

            if matches!(
                stop_reason,
                ProviderStopReason::Error
                    | ProviderStopReason::Aborted
                    | ProviderStopReason::ContentFilter
            ) {
                self.shared.emit(json!({"type":"turn_end"}));
                break;
            }
            if calls.is_empty() {
                self.shared.emit(json!({"type":"turn_end"}));
                let steering = self
                    .shared
                    .queues
                    .lock()
                    .await
                    .drain_at_boundary_if(false, |message| !message.editing);
                self.emit_queue_state().await;
                if let Some(steering) = steering {
                    for message in steering.messages {
                        self.push_user_message(message).await?;
                    }
                    continue;
                }
                break;
            }

            self.execute_tools(
                calls.into_values().collect(),
                stop_reason == ProviderStopReason::Length,
            )
            .await?;
            self.shared.emit(json!({"type":"turn_end"}));
            let change = { self.shared.model_change.lock().await.take() };
            if let Some(change) = change
                && let Err(error) = self.apply_model_change(change).await
            {
                self.shared
                    .emit(json!({"type":"model_error","message":error.to_string()}));
            }
            let steering = self
                .shared
                .queues
                .lock()
                .await
                .drain_at_boundary_if(false, |message| !message.editing);
            self.emit_queue_state().await;
            if let Some(steering) = steering {
                for message in steering.messages {
                    self.push_user_message(message).await?;
                }
            }
        }
        Ok(last_message)
    }

    async fn collect_response(
        &self,
        endpoint: &ProviderEndpoint,
        request: ProviderRequest,
        publish: bool,
    ) -> ResponseAssembly {
        let mut response = ResponseAssembly::default();
        let cancellation = self.shared.cancellation();
        let stream = tokio::select! {
            biased;
            _ = cancellation.cancelled() => { response.abort(); None }
            result = self.provider.stream(endpoint, request) => match result {
                Ok(stream) => Some(stream),
                Err(error) => { response.fail(error.to_string()); None }
            }
        };
        if let Some(mut stream) = stream {
            loop {
                let event = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => { response.abort(); break; }
                    event = stream.next() => event
                };
                match event {
                    Some(Ok(event)) => {
                        if let Some(event) = response.push(event)
                            && publish
                        {
                            self.shared.emit(event);
                        }
                    }
                    Some(Err(error)) => {
                        response.fail(error.to_string());
                        break;
                    }
                    None => break,
                }
            }
        }
        response
    }

    fn remove_last_assistant_from_context(&mut self) {
        if matches!(
            self.logical_messages.last(),
            Some(AgentMessage::Assistant { .. })
        ) {
            self.logical_messages.pop();
            self.messages = self
                .logical_messages
                .iter()
                .filter_map(to_provider_message)
                .collect();
        }
    }

    async fn retry_delay(&self, delay_ms: u64) -> bool {
        let cancellation = self.shared.cancellation();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => false,
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => true,
        }
    }

    async fn prepare_retry(&mut self, message: &AgentMessage) -> bool {
        let policy = self.config.retry;
        if !policy.enabled
            || self.retry_attempt >= policy.max_retries
            || crate::recovery::is_overflow(message, self.model.context_window)
            || !crate::recovery::is_retryable(message)
        {
            return false;
        }
        self.retry_attempt += 1;
        let error = match message {
            AgentMessage::Assistant { error_message, .. } => {
                error_message.as_deref().unwrap_or("Unknown error")
            }
            _ => "Unknown error",
        };
        let delay = policy.delay_ms(self.retry_attempt);
        self.shared.emit(json!({"type":"auto_retry_start","attempt":self.retry_attempt,"maxAttempts":policy.max_retries,"delayMs":delay,"errorMessage":error}));
        self.remove_last_assistant_from_context();
        if !self.retry_delay(delay).await {
            self.shared.emit(json!({"type":"auto_retry_end","success":false,"attempt":self.retry_attempt,"finalError":"Retry cancelled"}));
            self.retry_attempt = 0;
            return false;
        }
        true
    }

    async fn check_compaction(
        &mut self,
        message: &AgentMessage,
        skip_aborted: bool,
    ) -> Result<bool> {
        let entries: Vec<_> = self
            .entries
            .iter()
            .chain(&self.pending_entries)
            .cloned()
            .collect();
        let Some(action) = crate::recovery::auto_compaction(
            message,
            &entries,
            &self.logical_messages,
            &self.model,
            self.config.compaction,
            skip_aborted,
        ) else {
            return Ok(false);
        };
        if action.will_retry {
            if self.overflow_recovery_attempted {
                let error = if crate::recovery::is_overflow(message, self.model.context_window) {
                    "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
                } else {
                    "Truncated response recovery failed after one compact-and-retry attempt."
                };
                self.shared.emit(json!({"type":"compaction_end","reason":"overflow","aborted":false,"willRetry":false,"errorMessage":error}));
                return Ok(false);
            }
            self.overflow_recovery_attempted = true;
            self.remove_last_assistant_from_context();
        }
        let completed = self
            .compact(action.reason, action.will_retry, None)
            .await
            .unwrap_or(false);
        if completed && action.will_retry {
            if matches!(
                self.logical_messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: agent::StopReason::Error | agent::StopReason::Length,
                    ..
                })
            ) {
                self.remove_last_assistant_from_context();
            }
            return Ok(true);
        }
        Ok(false)
    }

    async fn summarize(
        &self,
        prompt: String,
        max_tokens: u64,
        label: &str,
        reason: &str,
    ) -> Result<(String, agent::Usage)> {
        let mut request = ProviderRequest::new(
            &self.header.model_id,
            vec![ProviderMessage {
                role: MessageRole::User,
                content: vec![ProviderContent::Text {
                    text: prompt,
                    text_signature: None,
                }],
            }],
        );
        request.system_prompt = agent::SUMMARIZATION_SYSTEM_PROMPT.into();
        request.supports_images = self.model.input.iter().any(|input| input == "image");
        request.max_tokens = Some(max_tokens);
        request.session_id = Some(self.id.clone());
        request.http_idle_timeout_ms = Some(self.config.http_idle_timeout_ms);
        request.timeout_ms = Some(if self.config.http_idle_timeout_ms == 0 {
            i32::MAX as u64
        } else {
            self.config.http_idle_timeout_ms
        });
        request.transport = Some(self.config.transport);
        request.websocket_connect_timeout_ms = self.config.websocket_connect_timeout_ms;
        request.disable_cache = true;
        if self.model.reasoning {
            request.thinking = ThinkingLevel::from_str(&self.thinking)?;
        }
        let endpoint = self.endpoint()?;
        let policy = self.config.retry;
        let mut attempt = 0;
        let response = loop {
            let response = self
                .collect_response(&endpoint, request.clone(), false)
                .await;
            let message = response.message(
                &self.header.provider,
                &self.header.model_id,
                &self.model.cost,
            );
            if !policy.enabled
                || attempt >= policy.max_retries
                || !crate::recovery::is_retryable(&message)
            {
                if attempt > 0 {
                    self.shared
                        .emit(json!({"type":"summarization_retry_finished"}));
                }
                break response;
            }
            attempt += 1;
            let error = match &message {
                AgentMessage::Assistant { error_message, .. } => {
                    error_message.as_deref().unwrap_or("Unknown error")
                }
                _ => "Unknown error",
            };
            let delay = policy.delay_ms(attempt);
            self.shared.emit(json!({"type":"summarization_retry_scheduled","attempt":attempt,"maxAttempts":policy.max_retries,"delayMs":delay,"errorMessage":error}));
            if !self.retry_delay(delay).await {
                self.shared
                    .emit(json!({"type":"summarization_retry_finished"}));
                bail!("Compaction cancelled");
            }
            self.shared.emit(json!({"type":"summarization_retry_attempt_start","source":"compaction","reason":reason}));
        };
        if self.shared.cancellation().is_cancelled()
            || response.stop_reason == ProviderStopReason::Aborted
        {
            bail!("Compaction cancelled");
        }
        let AgentMessage::Assistant {
            content,
            usage,
            stop_reason,
            error_message,
            ..
        } = response.message(
            &self.header.provider,
            &self.header.model_id,
            &self.model.cost,
        )
        else {
            unreachable!()
        };
        if let Some(error) =
            agent::summarization_failure(stop_reason, error_message.as_deref(), label)
        {
            bail!("{error}");
        }
        if !response.calls.is_empty() {
            bail!("{label} attempted to call a tool");
        }
        let text = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok((text, usage))
    }

    async fn compact(
        &mut self,
        reason: &str,
        will_retry: bool,
        instructions: Option<&str>,
    ) -> Result<bool> {
        let entries: Vec<_> = self
            .entries
            .iter()
            .chain(&self.pending_entries)
            .cloned()
            .collect();
        let manual = reason == "manual";
        let was_busy = self.shared.busy.swap(true, Ordering::SeqCst);
        if manual {
            self.shared
                .emit(json!({"type":"compaction_start","reason":reason}));
        }
        let preparation = agent::prepare_compaction(&entries, self.config.compaction);
        if !manual && preparation.is_none() {
            self.shared.busy.store(was_busy, Ordering::SeqCst);
            return Ok(false);
        }
        if !manual {
            self.shared
                .emit(json!({"type":"compaction_start","reason":reason}));
        }
        let result: Result<Value> = async {
            let preparation = preparation.with_context(|| if matches!(entries.last().map(|entry| &entry.kind), Some(SessionEntryKind::Compaction { .. })) { "Already compacted" } else { "Nothing to compact (session too small)" })?;
            let budget = agent::summary_max_tokens(preparation.settings.reserve_tokens, self.model.max_tokens);
            let (mut summary, usage) = if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
                let history = if preparation.messages_to_summarize.is_empty() { None } else {
                    Some(self.summarize(agent::build_summarization_prompt(&preparation.messages_to_summarize, preparation.previous_summary.as_deref(), instructions), budget, "Summarization", reason).await?)
                };
                let (prefix, prefix_usage) = self.summarize(agent::build_turn_prefix_prompt(&preparation.turn_prefix_messages),
                    agent::turn_prefix_max_tokens(preparation.settings.reserve_tokens, self.model.max_tokens), "Turn prefix summarization", reason).await?;
                let usage = history.as_ref().map(|(_, usage)| agent::combine_usage(usage, &prefix_usage)).unwrap_or(prefix_usage);
                (agent::merge_split_turn_summary(history.as_ref().map(|(text, _)| text.as_str()), &prefix), usage)
            } else {
                self.summarize(agent::build_summarization_prompt(&preparation.messages_to_summarize, preparation.previous_summary.as_deref(), instructions), budget, "Summarization", reason).await?
            };
            let details = agent::compute_file_lists(&preparation.file_operations);
            summary.push_str(&agent::format_file_operations(&details));
            if self.shared.cancellation().is_cancelled() { bail!("Compaction cancelled"); }
            self.flush_pending()?;
            let kept = self.entries.iter().position(|entry| entry.id == preparation.first_kept_entry_id).context("First kept entry has no UUID - session may need migration")?;
            let mut header = self.header.clone();
            header.thinking_level = self.thinking.clone();
            header.initial_cwd = Some(header.cwd.clone());
            header.usage_before.merge(agent::session_usage_totals(&self.entries[..kept]));
            let previous_id = self.last_entry_id.clone();
            let entry = self.entry(SessionEntryKind::Compaction {
                summary: summary.clone(), first_kept_entry_id: preparation.first_kept_entry_id.clone(),
                tokens_before: preparation.tokens_before, details: Some(serde_json::to_value(&details)?),
                usage: Some(usage.clone()), from_hook: Some(false),
            });
            let mut retained = self.entries[kept..].to_vec();
            retained.push(entry.clone());
            let segment = match session::rotate_compaction(&self.paths, &header, &retained) {
                Ok(segment) => segment,
                Err(error) => { self.last_entry_id = previous_id; return Err(error); }
            };
            self.header = header;
            self.entries = retained;
            self.logical_messages = agent::build_session_context(&self.entries, None);
            self.messages = self.logical_messages.iter().filter_map(to_provider_message).collect();
            self.emit_usage();
            let estimated: u64 = self.logical_messages.iter().map(agent::estimate_tokens).sum();
            self.shared.emit(json!({"type":"segment_change","segment":segment,"entry":entry}));
            Ok(json!({"summary":summary,"firstKeptEntryId":preparation.first_kept_entry_id,"tokensBefore":preparation.tokens_before,"estimatedTokensAfter":estimated,"usage":usage,"details":details}))
        }.await;
        self.shared.busy.store(was_busy, Ordering::SeqCst);
        match result {
            Ok(result) => {
                self.shared.emit(json!({"type":"compaction_end","reason":reason,"result":result,"aborted":false,"willRetry":will_retry}));
                Ok(true)
            }
            Err(error) => {
                let aborted = self.shared.cancellation().is_cancelled()
                    || error.to_string() == "Compaction cancelled";
                let prefix = match reason {
                    "manual" => "Compaction failed",
                    "overflow" => "Context overflow recovery failed",
                    _ => "Auto-compaction failed",
                };
                let mut event = json!({"type":"compaction_end","reason":reason,"aborted":aborted,"willRetry":false});
                if !aborted {
                    event["errorMessage"] = json!(format!("{prefix}: {error}"));
                }
                self.shared.emit(event);
                Err(error)
            }
        }
    }

    async fn execute_tools(&mut self, calls: Vec<PendingToolCall>, truncated: bool) -> Result<()> {
        if truncated {
            // Pinned agent-loop.ts: no call from a length-limited response may
            // execute, even when its salvaged arguments happen to validate.
            for call in calls {
                self.shared.emit(json!({"type":"tool_start","id":call.id,"name":call.name,"arguments":crate::streaming_json::parse_streaming_json(&call.arguments)}));
                let error = format!(
                    "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                    call.name
                );
                self.shared.emit(json!({"type":"tool_end","id":call.id,"name":call.name,"result":{"content":[{"type":"text","text":error}],"details":{}},"isError":true}));
                self.record_tool_result(call, Err(anyhow::anyhow!(error)));
            }
            return Ok(());
        }
        let mut context = ToolContext::new(&self.header.cwd);
        context.cancellation = self.shared.cancellation();
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
            let arguments = crate::streaming_json::parse_streaming_json(&call.arguments);
            self.shared.emit(json!({
                "type":"tool_start",
                "id":call.id,
                "name":call.name,
                "arguments":arguments
            }));
        }
        let futures = calls.into_iter().map(|call| {
            let context = context.clone();
            let shared = self.shared.clone();
            async move {
                let arguments = crate::streaming_json::parse_streaming_json(&call.arguments);
                let update_shared = shared.clone();
                let update_id = call.id.clone();
                let update_name = call.name.clone();
                let on_update = move |partial: tools::ToolResult| {
                    update_shared.emit(json!({"type":"tool_update","id":update_id,"name":update_name,"partialResult":partial}));
                };
                let result = tools::execute_tool_with_updates(&call.name, arguments, &context, Some(&on_update)).await;
                let (output, is_error) = match &result {
                    Ok(value) => (serde_json::to_value(value).expect("tool result"), false),
                    Err(error) => (json!({"content":[{"type":"text","text":error.to_string()}],"details":{}}), true),
                };
                // Render each completion immediately, but commit tool-result
                // messages below in call order, as Pi's parallel loop does.
                shared.emit(json!({"type":"tool_end","id":call.id,"name":call.name,"result":output,"isError":is_error}));
                (call, result)
            }
        });
        let results = join_all(futures).await;
        for (call, result) in results {
            self.record_tool_result(call, result.map_err(anyhow::Error::from));
        }
        Ok(())
    }

    fn record_tool_result(&mut self, call: PendingToolCall, result: Result<tools::ToolResult>) {
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
                (MessageContent::text(&text), Some(json!({})), true)
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
        self.emit_usage();
    }

    fn provider_request(&self) -> Result<ProviderRequest> {
        let thinking = ThinkingLevel::from_str(&self.thinking)?;
        let mut request = ProviderRequest::new(&self.header.model_id, self.messages.clone());
        request.supports_images = self.model.input.iter().any(|input| input == "image");
        request.system_prompt = self.system_prompt.clone();
        request.logical_messages = Some(
            self.logical_messages
                .iter()
                .filter_map(|message| {
                    logical_provider_message(message, &self.paths.session_dir(&self.id))
                })
                .collect(),
        );
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
        request.http_idle_timeout_ms = Some(self.config.http_idle_timeout_ms);
        request.timeout_ms = Some(if self.config.http_idle_timeout_ms == 0 {
            i32::MAX as u64
        } else {
            self.config.http_idle_timeout_ms
        });
        request.transport = Some(self.config.transport);
        request.websocket_connect_timeout_ms = self.config.websocket_connect_timeout_ms;
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

    fn emit_usage(&self) {
        let entries: Vec<_> = self
            .entries
            .iter()
            .chain(&self.pending_entries)
            .cloned()
            .collect();
        let snapshot = crate::usage::snapshot(
            &entries,
            &self.logical_messages,
            self.header.usage_before,
            self.model.context_window,
            self.header.provider == "openai-codex",
            self.config.compaction.enabled,
        );
        self.shared.emit(json!({"type":"usage","data":snapshot}));
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
        self.shared
            .replay
            .lock()
            .expect("live replay lock")
            .events
            .clear();
        Ok(())
    }
}

pub fn saved_usage(
    paths: &AppPaths,
    id: &str,
    config: &AppConfig,
) -> Result<crate::usage::Snapshot> {
    session::validate_id(id)?;
    let dir = paths.session_dir(id);
    let header = session::read_header(&dir)?;
    let entries = load_current_entries(&dir)?;
    let (model_id, _) = session::effective_model(&dir, &header);
    let model = models::find_model(config, &model_id, false, false);
    let messages = agent::build_session_context(&entries, None);
    Ok(crate::usage::snapshot(
        &entries,
        &messages,
        header.usage_before,
        model
            .as_ref()
            .map(|model| model.context_window)
            .unwrap_or(0),
        model_id.starts_with("openai-codex/"),
        config.compaction.enabled,
    ))
}

fn provider_authenticated(paths: &AppPaths) -> bool {
    fs::read(paths.provider_auth_file())
        .ok()
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        .and_then(|value| value.get("openai-codex").cloned())
        .is_some()
}

fn llama_available() -> bool {
    crate::llama::detect_installation().is_some()
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

// Convert only BashKitten's documented attachment/custom-message representation
// before applying Pi's provider conversion. Preserve the complete Pi message,
// including timestamp, source identity, signatures and usage, for replay.
fn logical_provider_message(message: &AgentMessage, session_dir: &Path) -> Option<Value> {
    let original = serde_json::to_value(message).ok()?;
    let mut value = match message {
        AgentMessage::User { .. }
        | AgentMessage::Assistant { .. }
        | AgentMessage::ToolResult { .. } => original.clone(),
        _ => {
            let converted = to_provider_message(message)?;
            let mut value = crate::completions::fallback_messages(&[converted], &json!({}))
                .into_iter()
                .next()?;
            value["timestamp"] = original["timestamp"].clone();
            value
        }
    };
    if let Some(blocks) = value["content"].as_array_mut() {
        for block in blocks {
            if block["type"] == "attachment" {
                let recorded = block["path"].as_str().unwrap_or_default();
                let copied = session::attachment_relative_path(recorded)
                    .map(|relative| session_dir.join("attachments").join(relative))
                    .filter(|path| path.is_file());
                let path = copied.as_deref().unwrap_or_else(|| Path::new(recorded));
                *block = json!({"type":"text","text":format!(
                    "Attached file:\n- name: {}\n- path: {}\n- media type: {}",
                    block["name"].as_str().unwrap_or_default(),
                    path.to_string_lossy(),
                    block["mimeType"].as_str().unwrap_or_default()
                )});
            }
        }
    }
    Some(value)
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

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crate::config::{CompatibleAuth, CompatibleProvider, ModelPreset};
    use axum::{Router, body::Body, response::Response, routing::post};
    use std::convert::Infallible;
    use tokio::io::Lines;
    use tokio::net::unix::OwnedReadHalf;

    async fn fixture_server(
        body: &'static str,
        keep_open: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || async move {
                let output = async_stream::stream! {
                    yield Ok::<_, Infallible>(body);
                    if keep_open { std::future::pending::<()>().await; }
                };
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(output))
                    .unwrap()
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, task)
    }

    fn fixture_paths(root: &Path, url: &str) -> AppPaths {
        let paths = AppPaths {
            config: root.join("config"),
            data: root.join("data"),
            runtime: root.join("runtime"),
        };
        paths.ensure().unwrap();
        let mut config = AppConfig {
            default_cwd: root.to_owned(),
            default_model: "fixture/model".into(),
            default_thinking: "off".into(),
            ..Default::default()
        };
        config.compatible_providers.push(CompatibleProvider {
            id: "fixture".into(),
            name: "Offline test server".into(),
            base_url: url.into(),
            auth: CompatibleAuth::None,
            models: vec![ModelPreset {
                id: "model".into(),
                name: "Fixture".into(),
                ..Default::default()
            }],
        });
        config.save(&paths).unwrap();
        paths
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fork_uses_its_attachment_copy_without_rewriting_historical_messages() {
        let (url, requests, server) = sequence_server(vec![
            (200, completion("original answer", 10, "stop")),
            (200, completion("fork answer", 10, "stop")),
        ])
        .await;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        let (id, task) = worker(&paths, root.path()).await;
        let upload = root.path().join("file.txt");
        fs::write(&upload, "independent fork attachment").unwrap();
        let saved = session::copy_attachments(&paths, &id, &[upload]).unwrap();
        let text = format!(
            "Leave this historical text unchanged: {}",
            saved[0].display()
        );
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        let mut reply = connect(
            &paths,
            &id,
            ControlRequest::Send {
                delivery: Delivery::Queue,
                content: text.clone(),
                attachments: saved.clone(),
                source_session: None,
            },
        )
        .await;
        assert_eq!(next(&mut reply).await["ok"], true);
        until(&mut events, |event| event["type"] == "agent_settled").await;
        stop(&paths, &id, task).await;
        let original = session::read_segment(&paths, &id, 1).unwrap();
        let fork = session::fork_at(
            &paths,
            &id,
            original.last().unwrap()["id"].as_str().unwrap(),
        )
        .unwrap();
        let fork_before = session::read_segment(&paths, &fork, 1).unwrap();
        assert_eq!(fork_before[1..], original[1..]);
        fs::remove_file(&saved[0]).unwrap();
        let run_paths = paths.clone();
        let run_id = fork.clone();
        let task = tokio::spawn(async move { run_worker(run_paths, run_id).await });
        wait_for_socket(&session::control_socket(&paths, &fork).unwrap())
            .await
            .unwrap();
        let mut events = connect(&paths, &fork, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &fork).await;
        until(&mut events, |event| event["type"] == "agent_settled").await;
        stop(&paths, &fork, task).await;
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        let historical = &requests[1]["messages"][1]["content"];
        assert_eq!(historical[0]["text"], text);
        let copied = paths
            .session_dir(&fork)
            .join("attachments")
            .join(session::attachment_relative_path(saved[0].to_str().unwrap()).unwrap());
        assert!(
            historical[1]["text"]
                .as_str()
                .unwrap()
                .contains(copied.to_str().unwrap())
        );
        assert_eq!(
            fs::read_to_string(copied).unwrap(),
            "independent fork attachment"
        );
        let fork_after = session::read_segment(&paths, &fork, 1).unwrap();
        assert_eq!(fork_after[1..original.len()], original[1..]);
        server.abort();
    }

    async fn worker(paths: &AppPaths, cwd: &Path) -> (String, tokio::task::JoinHandle<Result<()>>) {
        seeded_worker(paths, cwd, &[]).await
    }

    async fn seeded_worker(
        paths: &AppPaths,
        cwd: &Path,
        history: &[Value],
    ) -> (String, tokio::task::JoinHandle<Result<()>>) {
        let id = session::create(
            paths,
            &session::NewSession {
                cwd: cwd.to_owned(),
                model: "fixture/model".into(),
                thinking: "off".into(),
                model_parameters: models::find_model(
                    &AppConfig::load(paths).unwrap(),
                    "fixture/model",
                    false,
                    false,
                )
                .unwrap()
                .parameters,
                prompt: "runtime fixture".into(),
                attachments: vec![],
                parent: None,
            },
        )
        .unwrap();
        session::append_values(paths, &id, history).unwrap();
        let run_paths = paths.clone();
        let run_id = id.clone();
        let task = tokio::spawn(async move { run_worker(run_paths, run_id).await });
        wait_for_socket(&session::control_socket(paths, &id).unwrap())
            .await
            .unwrap();
        (id, task)
    }

    async fn connect(
        paths: &AppPaths,
        id: &str,
        request: ControlRequest,
    ) -> Lines<BufReader<OwnedReadHalf>> {
        let address =
            session::socket_address(&session::control_socket(paths, id).unwrap()).unwrap();
        let connection = UnixStream::connect(address.as_ref()).await.unwrap();
        let (read, mut write) = connection.into_split();
        write
            .write_all(serde_json::to_string(&request).unwrap().as_bytes())
            .await
            .unwrap();
        write.write_all(b"\n").await.unwrap();
        BufReader::new(read).lines()
    }

    async fn next(lines: &mut Lines<BufReader<OwnedReadHalf>>) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("worker event timed out")
            .unwrap()
            .expect("worker stream closed");
        serde_json::from_str(&line).unwrap()
    }

    async fn until(
        lines: &mut Lines<BufReader<OwnedReadHalf>>,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        loop {
            let value = next(lines).await;
            if predicate(&value) {
                return value;
            }
        }
    }

    async fn prompt(paths: &AppPaths, id: &str) {
        let mut response = connect(
            paths,
            id,
            ControlRequest::Send {
                delivery: Delivery::Queue,
                content: "test".into(),
                attachments: vec![],
                source_session: None,
            },
        )
        .await;
        assert_eq!(next(&mut response).await["ok"], true);
    }

    async fn stop(paths: &AppPaths, id: &str, task: tokio::task::JoinHandle<Result<()>>) {
        let mut response = connect(paths, id, ControlRequest::Stop).await;
        assert_eq!(next(&mut response).await["ok"], true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    fn completion(text: &str, tokens: u64, stop: &str) -> String {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"id":"fixture","choices":[{"delta":{"content":text},"finish_reason":stop}],"usage":{"prompt_tokens":tokens,"completion_tokens":1,"total_tokens":tokens+1}})
        )
    }

    async fn sequence_server(
        responses: Vec<(u16, String)>,
    ) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let responses = Arc::new(Mutex::new(std::collections::VecDeque::from(responses)));
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<Value>| {
                let captured = captured.clone();
                let responses = responses.clone();
                async move {
                    captured.lock().await.push(body);
                    let (status, body) = responses
                        .lock()
                        .await
                        .pop_front()
                        .expect("unexpected provider request");
                    Response::builder()
                        .status(status)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(body))
                        .unwrap()
                }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, requests, task)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pi_loop_rejects_every_truncated_call_then_executes_repaired_followup() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/pi-loop.json")).unwrap();
        let replies = fixture["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|chunk| (200, format!("data: {chunk}\n\ndata: [DONE]\n\n")))
            .collect();
        let (url, requests, server) = sequence_server(replies).await;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        let (id, task) = worker(&paths, root.path()).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &id).await;
        until(&mut events, |event| event["type"] == "agent_settled").await;
        stop(&paths, &id, task).await;
        assert!(!root.path().join("blocked.txt").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("recovered.txt")).unwrap(),
            "recovered"
        );
        assert_eq!(
            requests.lock().await.len(),
            fixture["requests"].as_array().unwrap().len()
        );
        let history = session::read_segment(&paths, &id, 1).unwrap();
        let actual: Vec<_> = history
            .iter()
            .filter_map(|entry| entry.get("message"))
            .filter(|v| v["role"] != "user")
            .map(|v| {
                let mut value = v.clone();
                value.as_object_mut().unwrap().remove("timestamp");
                value
            })
            .collect();
        let expected: Vec<_> = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["role"] != "user")
            .cloned()
            .collect();
        fn numeric_values(value: &mut Value) {
            match value {
                Value::Number(number) => *value = json!(number.as_f64().unwrap()),
                Value::Array(values) => {
                    for value in values {
                        numeric_values(value);
                    }
                }
                Value::Object(values) => {
                    for value in values.values_mut() {
                        numeric_values(value);
                    }
                }
                _ => {}
            }
        }
        let mut actual = json!(actual);
        let mut expected = json!(expected);
        numeric_values(&mut actual);
        numeric_values(&mut expected);
        assert_eq!(actual, expected);
        server.abort();
    }

    fn compaction_history() -> Vec<Value> {
        vec![
            json!({"type":"message","id":"old-user","parentId":null,"timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"user","content":"old context ".repeat(500),"timestamp":1}}),
            json!({"type":"message","id":"old-answer","parentId":"old-user","timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"old answer".repeat(30)}],"api":"openai-completions","provider":"fixture","model":"model","stopReason":"stop","timestamp":2,"usage":{"input":1500,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":1501,"cost":{"input":0.1,"output":0.01,"cacheRead":0.0,"cacheWrite":0.0,"total":0.11}}}}),
        ]
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn model_switch_uses_presets_saved_after_worker_start() {
        let (url, requests, server) =
            sequence_server(vec![(200, completion("new model response", 10, "stop"))]).await;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        let (id, task) = seeded_worker(&paths, root.path(), &[]).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        let mut config = AppConfig::load(&paths).unwrap();
        let mut preset = config.compatible_providers[0].models[0].clone();
        preset.id = "added-later".into();
        config.compatible_providers[0].models.push(preset);
        config.save(&paths).unwrap();
        let mut reply = connect(
            &paths,
            &id,
            ControlRequest::ChangeModel {
                model: "fixture/added-later".into(),
                thinking: "off".into(),
            },
        )
        .await;
        assert_eq!(next(&mut reply).await["ok"], true);
        let switched = until(&mut events, |e| {
            e["type"] == "model_change" || e["type"] == "model_error"
        })
        .await;
        assert_eq!(switched["type"], "model_change", "{switched}");
        prompt(&paths, &id).await;
        until(&mut events, |e| e["type"] == "turn_end").await;
        stop(&paths, &id, task).await;
        assert_eq!(requests.lock().await[0]["model"], "added-later");
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn model_selection_then_folder_change_preserves_historical_forks() {
        let root = tempfile::tempdir().unwrap();
        let (url, _, server) =
            sequence_server(vec![(200, completion("old model answer", 10, "stop"))]).await;
        let paths = fixture_paths(root.path(), &url);
        let (id, task) = worker(&paths, root.path()).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &id).await;
        until(&mut events, |e| e["type"] == "agent_settled").await;
        let original_header = session::read_header(&paths.session_dir(&id)).unwrap();
        let before = session::read_segment(&paths, &id, 1).unwrap();
        let target = before
            .iter()
            .find(|e| e["message"]["role"] == "assistant")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut config = AppConfig::load(&paths).unwrap();
        let mut preset = config.compatible_providers[0].models[0].clone();
        preset.id = "later-model".into();
        config.compatible_providers[0].models.push(preset);
        config.save(&paths).unwrap();
        // Every explicit selection is recorded, including the same-ID case.
        for _ in 0..2 {
            let mut reply = connect(
                &paths,
                &id,
                ControlRequest::ChangeModel {
                    model: "fixture/later-model".into(),
                    thinking: "off".into(),
                },
            )
            .await;
            assert_eq!(next(&mut reply).await["ok"], true);
            until(&mut events, |e| e["type"] == "model_change").await;
        }
        let folder = root.path().join("new-folder");
        fs::create_dir(&folder).unwrap();
        let mut reply = connect(
            &paths,
            &id,
            ControlRequest::ChangeCwd {
                cwd: folder.clone(),
            },
        )
        .await;
        assert_eq!(next(&mut reply).await["ok"], true);
        until(&mut events, |e| e["type"] == "cwd_change").await;
        stop(&paths, &id, task).await;
        let header = session::read_header(&paths.session_dir(&id)).unwrap();
        assert_eq!(header.cwd, folder);
        assert_eq!(header.model_id, original_header.model_id);
        assert_eq!(header.model_parameters, original_header.model_parameters);
        assert_eq!(
            session::effective_model(&paths.session_dir(&id), &header).0,
            "fixture/later-model"
        );
        let current = session::read_segment(&paths, &id, 1).unwrap();
        assert_eq!(
            current
                .iter()
                .filter(|e| e["type"] == "model_change")
                .count(),
            2
        );
        for entry in before.iter().filter(|e| e["type"] != "session") {
            assert!(current.contains(entry));
        }
        let fork = session::fork_at(&paths, &id, &target).unwrap();
        let fork_header = session::read_header(&paths.session_dir(&fork)).unwrap();
        assert_eq!(fork_header.cwd, root.path());
        assert_eq!(
            session::effective_model(&paths.session_dir(&fork), &fork_header).0,
            "fixture/model"
        );
        assert_eq!(
            fork_header.model_parameters,
            original_header.model_parameters
        );
        server.abort();
    }

    fn configure_compaction(paths: &AppPaths) {
        let mut config = AppConfig::load(paths).unwrap();
        config.compaction = agent::CompactionSettings {
            enabled: true,
            reserve_tokens: 200,
            keep_recent_tokens: 20,
        };
        config.retry.base_delay_ms = 1;
        config.compatible_providers[0].models[0].context_window = 2000;
        config.save(paths).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn attachment_images_are_processed_once_and_replayed_after_switch_to_vision() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        use sha2::{Digest, Sha256};
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/pi-images.json")).unwrap();
        let case = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == "large-resize")
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let attachment = root.path().join("image-without-extension");
        fs::write(
            &attachment,
            STANDARD.decode(case["bytes"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        let (url, requests, server) = sequence_server(vec![
            (200, completion("text-only response", 10, "stop")),
            (200, completion("vision response", 20, "stop")),
        ])
        .await;
        let paths = fixture_paths(root.path(), &url);
        let mut config = AppConfig::load(&paths).unwrap();
        let mut vision = config.compatible_providers[0].models[0].clone();
        vision.id = "vision".into();
        vision.input = vec!["text".into(), "image".into()];
        config.compatible_providers[0].models.push(vision);
        config.save(&paths).unwrap();
        let (id, task) = worker(&paths, root.path()).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        let mut response = connect(
            &paths,
            &id,
            ControlRequest::Send {
                delivery: Delivery::Queue,
                content: "image input".into(),
                attachments: vec![attachment],
                source_session: None,
            },
        )
        .await;
        assert_eq!(next(&mut response).await["ok"], true);
        until(&mut events, |event| event["type"] == "agent_settled").await;
        let mut response = connect(
            &paths,
            &id,
            ControlRequest::ChangeModel {
                model: "fixture/vision".into(),
                thinking: "off".into(),
            },
        )
        .await;
        assert_eq!(next(&mut response).await["ok"], true);
        until(&mut events, |event| event["type"] == "model_change").await;
        prompt(&paths, &id).await;
        until(&mut events, |event| event["type"] == "agent_settled").await;
        stop(&paths, &id, task).await;
        let requests = requests.lock().await;
        assert!(!requests[0].to_string().contains("base64,"));
        let image = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .find(|block| block["type"] == "image_url")
            .unwrap();
        let data = image["image_url"]["url"]
            .as_str()
            .unwrap()
            .split_once(',')
            .unwrap()
            .1;
        assert_eq!(
            hex::encode(Sha256::digest(STANDARD.decode(data).unwrap())),
            case["expected"]["sha256"]
        );
        let history = session::read_segment(&paths, &id, 1).unwrap();
        let content = history
            .iter()
            .find(|entry| entry["message"]["role"] == "user")
            .unwrap()["message"]["content"]
            .as_array()
            .unwrap();
        assert!(
            content
                .iter()
                .any(|block| block["type"] == "image" && block["data"] == data)
        );
        assert!(content.iter().any(|block| {
            block["text"]
                .as_str()
                .is_some_and(|text| text.contains("original 2400x1600, displayed at 2000x1333"))
        }));
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn project_prompt_and_message_prefix_survive_turns_and_worker_restart() {
        let (url, requests, server) = sequence_server(vec![
            (200, completion("first answer", 10, "stop")),
            (200, completion("second answer", 20, "stop")),
            (200, completion("third answer", 30, "stop")),
        ])
        .await;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        fs::write(
            root.path().join("AGENTS.md"),
            "Project instruction with exact whitespace.\n",
        )
        .unwrap();
        let (id, task) = worker(&paths, root.path()).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        for _ in 0..2 {
            prompt(&paths, &id).await;
            until(&mut events, |event| event["type"] == "agent_settled").await;
        }
        stop(&paths, &id, task).await;
        let persisted = fs::read(paths.session_dir(&id).join("000001.jsonl")).unwrap();
        let run_paths = paths.clone();
        let run_id = id.clone();
        let task = tokio::spawn(async move { run_worker(run_paths, run_id).await });
        wait_for_socket(&session::control_socket(&paths, &id).unwrap())
            .await
            .unwrap();
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &id).await;
        until(&mut events, |event| event["type"] == "agent_settled").await;
        stop(&paths, &id, task).await;
        assert!(
            fs::read(paths.session_dir(&id).join("000001.jsonl"))
                .unwrap()
                .starts_with(&persisted)
        );
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(
            requests[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("Project instruction with exact whitespace.\n")
        );
        for pair in requests.windows(2) {
            let prefix = pair[0]["messages"].as_array().unwrap();
            let next = pair[1]["messages"].as_array().unwrap();
            assert_eq!(&next[..prefix.len()], prefix);
            let mut previous = pair[0].clone();
            let mut following = pair[1].clone();
            previous.as_object_mut().unwrap().remove("messages");
            following.as_object_mut().unwrap().remove("messages");
            assert_eq!(
                previous, following,
                "all non-message request fields stay stable"
            );
        }
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn long_session_path_supports_control_and_cannot_be_rebound_by_a_second_worker() {
        let (url, server) = fixture_server("data: {\"choices\":[{\"delta\":{\"content\":\"long path works\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",false).await;
        let root = tempfile::tempdir().unwrap();
        let long = root.path().join("long-data-directory-".repeat(10));
        fs::create_dir(&long).unwrap();
        let paths = fixture_paths(&long, &url);
        let (id, task) = worker(&paths, &long).await;
        let path = session::control_socket(&paths, &id).unwrap();
        assert!(path.as_os_str().as_encoded_bytes().len() > 108);
        assert!(
            bind_control_socket(&path)
                .unwrap_err()
                .to_string()
                .contains("already has")
        );
        assert!(session::socket_is_live(&path));
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &id).await;
        until(&mut events, |event| event["type"] == "agent_settled").await;
        stop(&paths, &id, task).await;
        assert!(!path.exists());
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn queue_edit_holds_fifo_head_and_preserves_attachments_until_saved() {
        let tool = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"wait","function":{"name":"bash","arguments":"{\"command\":\"sleep 0.25\"}"}}]},"finish_reason":"tool_calls"}]})
        );
        let (url, requests, server) = sequence_server(vec![
            (200, tool),
            (200, completion("initial done", 10, "stop")),
            (200, completion("edited done", 20, "stop")),
            (200, completion("last done", 30, "stop")),
        ])
        .await;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        let attachment = root.path().join("original.txt");
        fs::write(&attachment, "retained attachment").unwrap();
        let (id, task) = worker(&paths, root.path()).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &id).await;
        until(&mut events, |event| event["type"] == "tool_start").await;
        let mut reply = connect(
            &paths,
            &id,
            ControlRequest::Send {
                delivery: Delivery::Queue,
                content: "first draft".into(),
                attachments: vec![attachment.clone()],
                source_session: None,
            },
        )
        .await;
        let first = next(&mut reply).await["data"]["queuedMessages"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut reply = connect(
            &paths,
            &id,
            ControlRequest::QueueAction {
                id: first.clone(),
                action: QueueAction::BeginEdit,
                content: None,
            },
        )
        .await;
        assert_eq!(
            next(&mut reply).await["data"]["queuedMessages"][0]["editing"],
            true
        );
        let mut reply = connect(
            &paths,
            &id,
            ControlRequest::Send {
                delivery: Delivery::Queue,
                content: "second follow-up".into(),
                attachments: vec![],
                source_session: None,
            },
        )
        .await;
        assert_eq!(next(&mut reply).await["ok"], true);
        until(&mut events, |event| event["type"] == "agent_settled").await;
        let mut reply = connect(&paths, &id, ControlRequest::Status).await;
        let state = next(&mut reply).await;
        assert_eq!(state["data"]["queued"], 2);
        assert_eq!(requests.lock().await.len(), 2);
        let mut reply = connect(
            &paths,
            &id,
            ControlRequest::QueueAction {
                id: first,
                action: QueueAction::Edit,
                content: Some("first edited".into()),
            },
        )
        .await;
        assert_eq!(next(&mut reply).await["ok"], true);
        until(&mut events, |event| event["type"] == "agent_settled").await;
        until(&mut events, |event| event["type"] == "agent_settled").await;
        stop(&paths, &id, task).await;
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 4);
        let first = requests[2]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .to_string();
        assert!(first.contains("first edited"));
        assert!(first.contains("original.txt"));
        assert!(first.contains(attachment.to_str().unwrap()));
        assert!(!first.contains("second follow-up"));
        assert!(
            requests[3]["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .to_string()
                .contains("second follow-up")
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn manual_compaction_aborts_and_saves_active_response_without_continuation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |axum::Json(body): axum::Json<Value>| {
            let captured = captured.clone();
            async move {
                let summary = body["messages"][0]["content"] == agent::SUMMARIZATION_SYSTEM_PROMPT;
                captured.lock().await.push(body);
                let output = async_stream::stream! {
                    if summary { yield Ok::<_, Infallible>(completion("manual checkpoint", 20, "stop")); }
                    else {
                        yield Ok("data: {\"choices\":[{\"delta\":{\"content\":\"partial answer\"}}]}\n\n".to_owned());
                        std::future::pending::<()>().await;
                    }
                };
                Response::builder().header("content-type", "text/event-stream").body(Body::from_stream(output)).unwrap()
            }
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        configure_compaction(&paths);
        let (id, task) = seeded_worker(&paths, root.path(), &compaction_history()).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &id).await;
        until(&mut events, |event| event["type"] == "assistant_delta").await;
        let mut reply = connect(
            &paths,
            &id,
            ControlRequest::Compact {
                custom_instructions: Some("save progress".into()),
            },
        )
        .await;
        assert_eq!(next(&mut reply).await["ok"], true);
        let compacted = until(&mut events, |event| event["type"] == "compaction_end").await;
        assert_eq!(compacted["reason"], "manual");
        assert_eq!(compacted["aborted"], false);
        assert_eq!(compacted["willRetry"], false);
        stop(&paths, &id, task).await;
        assert_eq!(
            requests.lock().await.len(),
            2,
            "manual compaction must not continue interrupted work"
        );
        let old = session::read_segment(&paths, &id, 1).unwrap();
        assert!(
            old.iter()
                .any(|entry| entry["message"]["stopReason"] == "aborted"
                    && entry.to_string().contains("partial answer"))
        );
        assert_eq!(
            session::current_segment(&paths.session_dir(&id)).unwrap().0,
            2
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_summary_leaves_segment_and_model_unchanged_then_switch_compacts_with_old_model()
    {
        let (url, requests, server) = sequence_server(vec![
            (200, completion("unfinished checkpoint", 20, "length")),
            (200, completion("switch checkpoint", 20, "stop")),
        ])
        .await;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        configure_compaction(&paths);
        let mut config = AppConfig::load(&paths).unwrap();
        let mut small = config.compatible_providers[0].models[0].clone();
        small.id = "small".into();
        small.context_window = 600;
        config.compatible_providers[0].models.push(small);
        config.save(&paths).unwrap();
        let (id, task) = seeded_worker(&paths, root.path(), &compaction_history()).await;
        let before = fs::read(paths.session_dir(&id).join("000001.jsonl")).unwrap();
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        let switch = || ControlRequest::ChangeModel {
            model: "fixture/small".into(),
            thinking: "off".into(),
        };
        let mut reply = connect(&paths, &id, switch()).await;
        assert_eq!(next(&mut reply).await["ok"], true);
        let failed = until(&mut events, |event| event["type"] == "model_error").await;
        assert!(
            failed["message"]
                .as_str()
                .unwrap()
                .contains("summary is incomplete")
        );
        assert_eq!(
            fs::read(paths.session_dir(&id).join("000001.jsonl")).unwrap(),
            before
        );
        assert_eq!(
            session::current_segment(&paths.session_dir(&id)).unwrap().0,
            1
        );
        let mut reply = connect(&paths, &id, switch()).await;
        assert_eq!(next(&mut reply).await["ok"], true);
        until(&mut events, |event| event["type"] == "model_change").await;
        stop(&paths, &id, task).await;
        assert_eq!(
            session::current_segment(&paths.session_dir(&id)).unwrap().0,
            2
        );
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request["model"] == "model"));
        let current = load_current_entries(&paths.session_dir(&id)).unwrap();
        assert!(
            matches!(&current.last().unwrap().kind, SessionEntryKind::ModelChange { model_id, .. } if model_id == "small")
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retry_keeps_errors_on_disk_but_removes_them_from_provider_context() {
        let (url, requests, server) = sequence_server(vec![
            (503, json!({"error":{"message":"overloaded"}}).to_string()),
            (503, json!({"error":{"message":"overloaded"}}).to_string()),
            (200, completion("recovered", 10, "stop")),
        ])
        .await;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        configure_compaction(&paths);
        let (id, task) = worker(&paths, root.path()).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &id).await;
        let retry = until(&mut events, |event| event["type"] == "auto_retry_end").await;
        assert_eq!(retry["success"], true);
        assert_eq!(retry["attempt"], 2);
        until(&mut events, |event| event["type"] == "agent_settled").await;
        stop(&paths, &id, task).await;
        let entries = load_current_entries(&paths.session_dir(&id)).unwrap();
        assert_eq!(entries.len(), 4);
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|request| request["messages"].as_array().unwrap().len() == 2)
        );
        assert!(
            serde_json::to_string(&entries)
                .unwrap()
                .contains("overloaded")
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn compaction_rotates_atomically_and_resume_uses_summary_without_double_counting() {
        let (url, requests, server) = sequence_server(vec![
            (200, completion("first answer", 1950, "stop")),
            (200, completion("checkpoint", 20, "stop")),
            (200, completion("after restart", 30, "stop")),
        ])
        .await;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        configure_compaction(&paths);
        let (id, task) = seeded_worker(&paths, root.path(), &compaction_history()).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &id).await;
        let compacted = until(&mut events, |event| event["type"] == "compaction_end").await;
        assert_eq!(compacted["aborted"], false);
        assert_eq!(
            compacted["result"]["summary"],
            agent::merge_split_turn_summary(None, "checkpoint")
        );
        until(&mut events, |event| event["type"] == "agent_settled").await;
        stop(&paths, &id, task).await;
        let dir = paths.session_dir(&id);
        let old = fs::read(dir.join("000001.jsonl")).unwrap();
        let header = session::read_header(&dir).unwrap();
        let entries = load_current_entries(&dir).unwrap();
        assert_eq!(session::current_segment(&dir).unwrap().0, 2);
        let mut total = agent::session_usage_totals(&entries);
        total.merge(header.usage_before);
        assert_eq!(total.input, 3470);
        assert_eq!(total.output, 3);
        assert_eq!(total.cost, 0.11);
        let context = agent::build_session_context(&entries, None);
        assert!(
            matches!(context.first(), Some(AgentMessage::CompactionSummary { summary, .. }) if summary.ends_with("checkpoint"))
        );
        assert_eq!(
            agent::current_context_usage(&entries, &context, 2000)
                .unwrap()
                .tokens,
            None
        );
        let resumed_paths = paths.clone();
        let resumed_id = id.clone();
        let task = tokio::spawn(async move { run_worker(resumed_paths, resumed_id).await });
        wait_for_socket(&session::control_socket(&paths, &id).unwrap())
            .await
            .unwrap();
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &id).await;
        until(&mut events, |event| event["type"] == "agent_settled").await;
        stop(&paths, &id, task).await;
        assert_eq!(fs::read(dir.join("000001.jsonl")).unwrap(), old);
        assert_eq!(session::current_segment(&dir).unwrap().0, 2);
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[1]["messages"][0]["content"],
            agent::SUMMARIZATION_SYSTEM_PROMPT
        );
        assert!(requests[1].get("tools").is_none());
        // Pi reserves 4096 tokens when clamping a summary request. This tiny
        // fixture context leaves only its minimum one-token output allowance.
        assert_eq!(requests[1]["max_completion_tokens"].as_f64(), Some(1.0));
        assert!(
            requests[2]["messages"][1]
                .to_string()
                .contains("checkpoint")
        );
        assert!(!requests[2].to_string().contains("old context"));
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overflow_compacts_then_retries_once_without_replaying_the_failed_response() {
        let (url, requests, server) = sequence_server(vec![
            (
                400,
                json!({"error":{"message":"maximum context length is 2000 tokens"}}).to_string(),
            ),
            (200, completion("overflow checkpoint", 20, "stop")),
            (200, completion("recovered", 30, "stop")),
        ])
        .await;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        configure_compaction(&paths);
        let (id, task) = seeded_worker(&paths, root.path(), &compaction_history()).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &id).await;
        let compacted = until(&mut events, |event| event["type"] == "compaction_end").await;
        assert_eq!(compacted["reason"], "overflow");
        assert_eq!(compacted["willRetry"], true);
        until(&mut events, |event| event["type"] == "agent_settled").await;
        stop(&paths, &id, task).await;
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(!requests[2].to_string().contains("maximum context length"));
        assert_eq!(
            requests[2]["messages"].as_array().unwrap().last().unwrap()["role"],
            "user"
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stop_saves_partial_response_and_late_subscriber_recovers_memory() {
        let (url, server) = fixture_server("data: {\"id\":\"fixture\",\"choices\":[{\"delta\":{\"reasoning_content\":\"working thought\",\"content\":\"partial answer\"}}]}\n\n", true).await;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        let (id, task) = worker(&paths, root.path()).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        prompt(&paths, &id).await;
        until(&mut events, |value| value["type"] == "thinking_delta").await;

        let mut late = connect(&paths, &id, ControlRequest::Subscribe).await;
        let snapshot = next(&mut late).await;
        assert_eq!(snapshot["data"]["busy"], true);
        assert!(
            snapshot["data"]["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["type"] == "assistant_delta"
                    && event["delta"] == "partial answer")
        );
        assert!(
            load_current_entries(&paths.session_dir(&id))
                .unwrap()
                .is_empty(),
            "live deltas must not be written to JSONL"
        );

        stop(&paths, &id, task).await;
        let entries = load_current_entries(&paths.session_dir(&id)).unwrap();
        let value = serde_json::to_value(entries.last().unwrap()).unwrap();
        assert_eq!(value["message"]["stopReason"], "aborted");
        let content = value["message"]["content"].as_array().unwrap();
        assert!(
            content
                .iter()
                .any(|block| block["text"] == "partial answer")
        );
        assert!(
            content
                .iter()
                .any(|block| block["thinking"] == "working thought")
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bash_streams_before_completion_cancels_and_does_not_stop_sibling() {
        let (url, server) = fixture_server("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-fixture\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"printf early-output; sleep 60\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n", false).await;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path(), &url);
        let (id, task) = worker(&paths, root.path()).await;
        let (sibling, sibling_task) = worker(&paths, root.path()).await;
        let mut events = connect(&paths, &id, ControlRequest::Subscribe).await;
        next(&mut events).await;
        let mut sibling_events = connect(&paths, &sibling, ControlRequest::Subscribe).await;
        next(&mut sibling_events).await;
        prompt(&paths, &id).await;
        prompt(&paths, &sibling).await;
        until(&mut events, |value| {
            value["type"] == "tool_update" && value.to_string().contains("early-output")
        })
        .await;
        until(&mut sibling_events, |value| {
            value["type"] == "tool_update" && value.to_string().contains("early-output")
        })
        .await;
        assert!(
            load_current_entries(&paths.session_dir(&id))
                .unwrap()
                .is_empty()
        );
        stop(&paths, &id, task).await;
        assert!(
            !sibling_task.is_finished(),
            "stopping one worker must not stop another"
        );
        let entries = load_current_entries(&paths.session_dir(&id)).unwrap();
        let value = serde_json::to_value(entries.last().unwrap()).unwrap();
        assert_eq!(value["message"]["role"], "toolResult");
        assert_eq!(value["message"]["isError"], true);
        assert!(value.to_string().contains("early-output"));
        assert!(value.to_string().contains("aborted"));
        stop(&paths, &sibling, sibling_task).await;
        server.abort();
    }

    #[test]
    fn live_replay_coalesces_deltas_and_replaces_partial_tools() {
        let mut replay = LiveReplay::default();
        replay.push(&json!({"type":"assistant_delta","index":1,"delta":"a"}));
        replay.push(&json!({"type":"assistant_delta","index":1,"delta":"b"}));
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0]["delta"], "ab");
        replay.push(&json!({"type":"message","entryId":"saved"}));
        assert_eq!(replay.events.len(), 1);
        for number in 0..100 {
            replay.push(&json!({"type":"tool_update","id":"call","partialResult":number}));
        }
        assert_eq!(replay.events.len(), 2);
        assert_eq!(replay.events[1]["partialResult"], 99);
    }
}
