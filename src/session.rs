use crate::config::atomic_private_json;
use crate::paths::{AppPaths, ensure_private_dir, set_private_file};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const SESSION_FORMAT_VERSION: u32 = 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: PathBuf,
    #[serde(
        rename = "initialCwd",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub initial_cwd: Option<PathBuf>,
    pub provider: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: String,
    #[serde(
        rename = "modelParameters",
        default,
        skip_serializing_if = "Value::is_null"
    )]
    pub model_parameters: Value,
    #[serde(rename = "parentSession", skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(
        rename = "usageBefore",
        default,
        skip_serializing_if = "crate::agent::UsageTotals::is_zero"
    )]
    pub usage_before: crate::agent::UsageTotals,
    #[serde(
        rename = "cacheHitRateBefore",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_hit_rate_before: Option<f64>,
    #[serde(
        rename = "goalBefore",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub goal_before: Option<crate::goal::Goal>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewSession {
    pub cwd: PathBuf,
    pub model: String,
    pub thinking: String,
    #[serde(default)]
    pub model_parameters: Value,
    pub prompt: String,
    #[serde(default)]
    pub attachments: Vec<PathBuf>,
    pub parent: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub running: bool,
    pub modified: i64,
    pub current_segment: u32,
    pub cwd: Option<PathBuf>,
    pub model: Option<String>,
    pub thinking: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    Send {
        delivery: Delivery,
        content: String,
        #[serde(default)]
        attachments: Vec<PathBuf>,
        #[serde(default)]
        source_session: Option<String>,
    },
    GoalMessage {
        content: String,
        #[serde(default)]
        attachments: Vec<PathBuf>,
    },
    Subscribe,
    Goal {
        #[serde(flatten)]
        action: crate::goal::Action,
    },
    Status,
    Stop,
    Compact {
        #[serde(default)]
        custom_instructions: Option<String>,
    },
    ChangeModel {
        model: String,
        thinking: String,
    },
    ChangeCwd {
        cwd: PathBuf,
    },
    QueueAction {
        id: String,
        action: QueueAction,
        #[serde(default)]
        content: Option<String>,
        #[serde(default)]
        edit_token: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueAction {
    BeginEdit,
    TakeEdit,
    CancelEdit,
    Edit,
    Promote,
    Remove,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Steer,
    Queue,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlReply {
    pub ok: bool,
    pub message: String,
    #[serde(default)]
    pub data: Value,
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.len() < 8 || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        bail!("Invalid session ID");
    }
    Ok(())
}

fn split_model(full: &str) -> Result<(String, String)> {
    let (provider, model) = full
        .split_once('/')
        .context("Model must be provider/model-id")?;
    if provider.is_empty() || model.is_empty() {
        bail!("Model must be provider/model-id");
    }
    Ok((provider.into(), model.into()))
}

pub fn shorten_title(prompt: &str) -> String {
    let collapsed = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Image session")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let count = collapsed.chars().count();
    if count <= 60 {
        collapsed
    } else {
        format!("{}…", collapsed.chars().take(59).collect::<String>())
    }
}

pub fn create(paths: &AppPaths, request: &NewSession) -> Result<String> {
    if !request.cwd.is_dir() {
        bail!(
            "Working directory does not exist: {}",
            request.cwd.display()
        );
    }
    let (provider, model_id) = split_model(&request.model)?;
    let id = Uuid::now_v7().to_string();
    let dir = paths.session_dir(&id);
    ensure_private_dir(&dir)?;
    ensure_private_dir(&dir.join("attachments"))?;

    let title_path = dir.join("title");
    let mut title = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&title_path)?;
    writeln!(title, "{}", shorten_title(&request.prompt))?;

    let header = SessionHeader {
        kind: "session".into(),
        version: SESSION_FORMAT_VERSION,
        id: id.clone(),
        timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        cwd: fs::canonicalize(&request.cwd).unwrap_or_else(|_| request.cwd.clone()),
        initial_cwd: None,
        provider,
        model_id,
        thinking_level: request.thinking.clone(),
        model_parameters: request.model_parameters.clone(),
        parent_session: request.parent.clone(),
        usage_before: Default::default(),
        cache_hit_rate_before: None,
        goal_before: None,
    };
    let segment = dir.join("000001.jsonl");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&segment)?;
    set_private_file(&segment)?;
    crate::lossless_json::to_writer(&mut file, &header)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(id)
}

pub fn control_socket(paths: &AppPaths, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(paths.session_dir(id).join("control.sock"))
}

/// Keep the required on-disk socket location even when its absolute path exceeds
/// Linux sockaddr_un.sun_path. The descriptor must outlive bind/connect.
pub struct SocketAddress {
    path: PathBuf,
    _directory: Option<fs::File>,
}

impl AsRef<Path> for SocketAddress {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

pub fn socket_address(path: &Path) -> Result<SocketAddress> {
    use std::os::fd::AsRawFd;
    if path.as_os_str().as_encoded_bytes().len() < 108 {
        return Ok(SocketAddress {
            path: path.to_owned(),
            _directory: None,
        });
    }
    let directory = fs::File::open(path.parent().context("Socket has no parent directory")?)?;
    let address = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
        .join(path.file_name().context("Socket has no filename")?);
    Ok(SocketAddress {
        path: address,
        _directory: Some(directory),
    })
}

pub fn current_segment(dir: &Path) -> Result<(u32, PathBuf)> {
    crate::paths::set_private_dir(dir)?;
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() == 12
            && name.ends_with(".jsonl")
            && let Ok(number) = name[..6].parse::<u32>()
            && best.as_ref().is_none_or(|(n, _)| number > *n)
        {
            best = Some((number, entry.path()));
        }
    }
    best.context("Session has no JSONL segment")
}

pub fn read_header(dir: &Path) -> Result<SessionHeader> {
    let (_, file) = current_segment(dir)?;
    set_private_file(&file)?;
    let line = BufReader::new(fs::File::open(file)?)
        .lines()
        .next()
        .context("Empty session")??;
    let header: SessionHeader = crate::lossless_json::from_str(&line)?;
    if header.kind != "session" {
        bail!("Current segment does not begin with a session header");
    }
    Ok(header)
}

pub fn validate_cwd(cwd: &Path) -> Result<PathBuf> {
    if !cwd.is_absolute() {
        bail!("Working folder must be an absolute path");
    }
    let cwd = fs::canonicalize(cwd).context("Working folder does not exist")?;
    if !cwd.is_dir() {
        bail!("Working folder must be a directory");
    }
    fs::read_dir(&cwd).context("Working folder is not readable")?;
    Ok(cwd)
}

/// Called only by the session owner at a settled-turn boundary (or on an
/// unpublished fork). Original message lines remain byte-for-byte unchanged.
pub fn replace_current_header(
    dir: &Path,
    header: &SessionHeader,
    event: Option<&Value>,
) -> Result<()> {
    let (_, path) = current_segment(dir)?;
    set_private_file(&path)?;
    let bytes = fs::read(&path)?;
    let first_newline = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .context("Missing session header")?;
    let first: Value = crate::lossless_json::from_slice(&bytes[..first_newline])?;
    if first["type"] != "session" {
        bail!("Current segment has no session header");
    }
    let temporary = dir.join(format!(".{}.header", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        crate::lossless_json::to_writer(&mut output, header)?;
        output.write_all(b"\n")?;
        output.write_all(&bytes[first_newline + 1..])?;
        if let Some(event) = event {
            if !bytes.ends_with(b"\n") {
                output.write_all(b"\n")?;
            }
            crate::lossless_json::to_writer(&mut output, event)?;
            output.write_all(b"\n")?;
        }
        output.sync_all()?;
        fs::rename(&temporary, &path)?;
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn restore_fork_cwd(dir: &Path) -> Result<()> {
    let mut header = read_header(dir)?;
    if let Some(initial) = &header.initial_cwd {
        header.cwd = initial.clone();
        let (_, segment) = current_segment(dir)?;
        for line in BufReader::new(fs::File::open(segment)?).lines() {
            let value: Value = crate::lossless_json::from_str(&line?)?;
            if value["type"] == "custom"
                && value["customType"] == "bashkitten.cwd"
                && let Some(cwd) = value["data"]["cwd"].as_str()
            {
                header.cwd = cwd.into();
            }
        }
        replace_current_header(dir, &header, None)?;
    }
    Ok(())
}

pub fn effective_model(dir: &Path, header: &SessionHeader) -> (String, String) {
    let Ok((_, file)) = current_segment(dir) else {
        return (
            format!("{}/{}", header.provider, header.model_id),
            header.thinking_level.clone(),
        );
    };
    let mut provider = header.provider.clone();
    let mut model_id = header.model_id.clone();
    let mut thinking = header.thinking_level.clone();
    if let Ok(file) = fs::File::open(file) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(value) = crate::lossless_json::from_str::<Value>(&line) else {
                continue;
            };
            match value.get("type").and_then(Value::as_str) {
                Some("model_change") => {
                    if let Some(value) = value.get("provider").and_then(Value::as_str) {
                        provider = value.to_owned();
                    }
                    if let Some(value) = value.get("modelId").and_then(Value::as_str) {
                        model_id = value.to_owned();
                    }
                }
                Some("thinking_level_change") => {
                    if let Some(value) = value.get("thinkingLevel").and_then(Value::as_str) {
                        thinking = value.to_owned();
                    }
                }
                _ => {}
            }
        }
    }
    (format!("{provider}/{model_id}"), thinking)
}

pub fn list(paths: &AppPaths) -> Result<Vec<SessionSummary>> {
    let mut sessions = Vec::new();
    for entry in fs::read_dir(paths.sessions_dir())? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if validate_id(&id).is_err() {
            continue;
        }
        let dir = entry.path();
        let Ok((segment, jsonl)) = current_segment(&dir) else {
            continue;
        };
        let modified = jsonl
            .metadata()?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let title_path = dir.join("title");
        let title = if title_path.exists() {
            set_private_file(&title_path)?;
            fs::read_to_string(&title_path)?.trim().to_owned()
        } else {
            "Untitled session".into()
        };
        let header = read_header(&dir).ok();
        sessions.push(SessionSummary {
            id: id.clone(),
            title,
            running: socket_is_live(&dir.join("control.sock")),
            modified,
            current_segment: segment,
            cwd: header.as_ref().map(|h| h.cwd.clone()),
            model: header
                .as_ref()
                .map(|h| format!("{}/{}", h.provider, h.model_id)),
            thinking: header.as_ref().map(|h| h.thinking_level.clone()),
        });
    }
    sessions.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(sessions)
}

pub fn socket_is_live(path: &Path) -> bool {
    socket_address(path).is_ok_and(|address| UnixStream::connect(address.as_ref()).is_ok())
}

pub fn read_segment(paths: &AppPaths, id: &str, number: u32) -> Result<Vec<Value>> {
    validate_id(id)?;
    if number == 0 || number > 999_999 {
        bail!("Invalid segment number");
    }
    let path = paths.session_dir(id).join(format!("{number:06}.jsonl"));
    crate::paths::set_private_dir(&paths.session_dir(id))?;
    set_private_file(&path)?;
    let file = fs::File::open(path)?;
    FileExt::lock_shared(&file)?;
    BufReader::new(file)
        .lines()
        .map(|line| Ok(crate::lossless_json::from_str(&line?)?))
        .collect()
}

pub fn copy_attachments(paths: &AppPaths, id: &str, sources: &[PathBuf]) -> Result<Vec<PathBuf>> {
    validate_id(id)?;
    let attachments_dir = paths.session_dir(id).join("attachments");
    ensure_private_dir(&attachments_dir)?;
    let mut copied = Vec::with_capacity(sources.len());
    for source in sources {
        let source = fs::canonicalize(source)
            .with_context(|| format!("open attachment {}", source.display()))?;
        if !source.is_file() {
            bail!("attachment is not a file: {}", source.display());
        }
        let name = source.file_name().context("attachment has no filename")?;
        let upload_dir = attachments_dir.join(Uuid::new_v4().to_string());
        ensure_private_dir(&upload_dir)?;
        let destination = upload_dir.join(name);
        fs::copy(&source, &destination).with_context(|| {
            format!(
                "copy attachment {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        set_private_file(&destination)?;
        copied.push(destination);
    }
    Ok(copied)
}

/// Upload paths are stable within forks even when the recorded absolute path
/// still names an ancestor session. Never interpret traversal as an upload.
pub(crate) fn attachment_relative_path(path: &str) -> Option<&Path> {
    let (_, relative) = path.rsplit_once("/attachments/")?;
    let relative = Path::new(relative);
    let components = relative.components().collect::<Vec<_>>();
    (components.len() == 2
        && components
            .iter()
            .all(|part| matches!(part, std::path::Component::Normal(_))))
    .then_some(relative)
}

fn collect_history_strings(value: &Value, strings: &mut Vec<String>) {
    if let Some(text) = crate::lossless_json::JsString::from_value(value) {
        // Attachment paths are valid filesystem UTF-8. A lone code unit
        // elsewhere in the same logical text must not hide the reference.
        strings.push(text.as_str().to_owned());
        return;
    }
    match value {
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_history_strings(value, strings)),
        Value::Object(_) => crate::lossless_json::object_entries(value)
            .expect("logical object")
            .into_iter()
            .for_each(|(_, value)| collect_history_strings(value, strings)),
        _ => {}
    }
}

fn copy_referenced_attachments(
    source_dir: &Path,
    destination_dir: &Path,
    retained_strings: &[String],
) -> Result<()> {
    let source_attachments = source_dir.join("attachments");
    let destination_attachments = destination_dir.join("attachments");
    ensure_private_dir(&destination_attachments)?;
    if !source_attachments.is_dir() {
        return Ok(());
    }
    crate::paths::set_private_dir(&source_attachments)?;
    for entry in walkdir::WalkDir::new(&source_attachments)
        .min_depth(1)
        .follow_links(false)
    {
        let entry = entry?;
        if entry.file_type().is_dir() {
            crate::paths::set_private_dir(entry.path())?;
        }
        let relative = entry.path().strip_prefix(&source_attachments)?;
        let destination = destination_attachments.join(relative);
        let reference = format!("/attachments/{}", relative.to_string_lossy());
        if entry.file_type().is_file()
            && retained_strings
                .iter()
                .any(|text| text.contains(&reference))
        {
            set_private_file(entry.path())?;
            if let Some(parent) = destination.parent() {
                ensure_private_dir(parent)?;
            }
            fs::copy(entry.path(), &destination).with_context(|| {
                format!(
                    "copy fork attachment {} to {}",
                    entry.path().display(),
                    destination.display()
                )
            })?;
            set_private_file(&destination)?;
        }
    }
    Ok(())
}

/// Clone a session through one exact persisted entry. The browser supplies only
/// the entry ID; all history and attachment copying happens here in Rust.
pub fn fork_at(paths: &AppPaths, source_id: &str, target_entry_id: &str) -> Result<String> {
    validate_id(source_id)?;
    if target_entry_id.is_empty()
        || target_entry_id.len() > 64
        || !target_entry_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        bail!("Invalid fork entry ID");
    }

    let source_dir = paths.session_dir(source_id);
    if !source_dir.is_dir() {
        bail!("Session does not exist");
    }
    let source_dir = fs::canonicalize(&source_dir)?;
    crate::paths::set_private_dir(&source_dir)?;
    let new_id = Uuid::now_v7().to_string();
    let final_dir = paths.session_dir(&new_id);
    let temporary_dir = paths.sessions_dir().join(format!(".{new_id}.forking"));
    if final_dir.exists() || temporary_dir.exists() {
        bail!("Fork destination already exists");
    }
    ensure_private_dir(&temporary_dir)?;

    let result = (|| -> Result<String> {
        let mut segments = fs::read_dir(&source_dir)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                (name.len() == 12
                    && name.ends_with(".jsonl")
                    && name[..6].bytes().all(|byte| byte.is_ascii_digit()))
                .then_some((name, entry.path()))
            })
            .collect::<Vec<_>>();
        segments.sort_by(|left, right| left.0.cmp(&right.0));

        let mut retained_strings = Vec::new();
        let mut found = false;

        'segments: for (name, source_segment) in segments {
            set_private_file(&source_segment)?;
            let destination_segment = temporary_dir.join(name);
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&destination_segment)?;
            set_private_file(&destination_segment)?;
            for line in BufReader::new(fs::File::open(&source_segment)?).lines() {
                let line = line?;
                let mut value: Value = crate::lossless_json::from_str(&line)?;
                let is_header = value.get("type").and_then(Value::as_str) == Some("session");
                let is_target = value.get("id").and_then(Value::as_str) == Some(target_entry_id);
                if is_target && value.get("type").and_then(Value::as_str) != Some("message") {
                    bail!("Fork target is not a message");
                }
                collect_history_strings(&value, &mut retained_strings);
                if is_header {
                    let object = value.as_object_mut().context("Invalid session header")?;
                    object.insert("id".into(), Value::String(new_id.clone()));
                    object.insert(
                        "timestamp".into(),
                        Value::String(
                            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        ),
                    );
                    object.insert("parentSession".into(), Value::String(source_id.to_owned()));
                }
                if is_header {
                    crate::lossless_json::to_writer(&mut output, &value)?;
                } else {
                    output.write_all(line.as_bytes())?;
                }
                output.write_all(b"\n")?;
                if is_target {
                    found = true;
                    output.sync_all()?;
                    break 'segments;
                }
            }
            output.sync_all()?;
        }
        if !found {
            bail!("Fork message is not yet available in session history");
        }
        restore_fork_cwd(&temporary_dir)?;

        let title_path = temporary_dir.join("title");
        let source_title = source_dir.join("title");
        let title = if source_title.exists() {
            set_private_file(&source_title)?;
            fs::read_to_string(&source_title)?
        } else {
            "Untitled session\n".into()
        };
        let mut title_file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&title_path)?;
        title_file.write_all(title.as_bytes())?;
        copy_referenced_attachments(&source_dir, &temporary_dir, &retained_strings)?;
        fs::rename(&temporary_dir, &final_dir)?;
        Ok(new_id.clone())
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary_dir);
    }
    result
}

pub fn append_values(paths: &AppPaths, id: &str, values: &[Value]) -> Result<()> {
    validate_id(id)?;
    let dir = paths.session_dir(id);
    let (_, path) = current_segment(&dir)?;
    set_private_file(&path)?;
    let mut bytes = Vec::new();
    for value in values {
        crate::lossless_json::to_writer(&mut bytes, value)?;
        bytes.push(b'\n');
    }
    let mut file = OpenOptions::new().append(true).open(&path)?;
    file.lock_exclusive()?;
    let length = file.metadata()?.len();
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        // A retry must not duplicate a partially written batch or leave a
        // partial JSON line ahead of the next completed turn.
        file.set_len(length)
            .and_then(|_| file.sync_all())
            .with_context(|| {
                format!("append failed ({error}); could not restore completed history boundary")
            })?;
        return Err(error.into());
    }
    Ok(())
}

/// Publish a complete compaction checkpoint without changing any older JSONL.
/// A hard link provides atomic create-if-absent rather than overwriting a segment.
pub fn rotate_compaction(
    paths: &AppPaths,
    header: &SessionHeader,
    entries: &[crate::agent::SessionEntry],
) -> Result<u32> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = paths.session_dir(&header.id);
    let (current, _) = current_segment(&dir)?;
    let next = current
        .checked_add(1)
        .filter(|number| *number <= 999_999)
        .context("session segment number overflow")?;
    let destination = dir.join(format!("{next:06}.jsonl"));
    let temporary = dir.join(format!(".compaction-{}", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        crate::lossless_json::to_writer(&mut file, header)?;
        file.write_all(b"\n")?;
        for entry in entries {
            crate::lossless_json::to_writer(&mut file, entry)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        let directory = fs::File::open(&dir)?;
        directory.sync_all()?;
        fs::hard_link(&temporary, &destination)?;
        if let Err(error) = directory.sync_all() {
            // This exact file was just published by this operation. The complete
            // pre-compaction history remains in the unchanged previous segment.
            fs::remove_file(&destination)?;
            let _ = directory.sync_all();
            return Err(error.into());
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result?;
    Ok(next)
}

pub fn send(paths: &AppPaths, id: &str, request: &ControlRequest) -> Result<ControlReply> {
    let socket = control_socket(paths, id)?;
    let address = socket_address(&socket)?;
    let mut stream = UnixStream::connect(address.as_ref())
        .with_context(|| format!("connect {}", socket.display()))?;
    crate::lossless_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(crate::lossless_json::from_str(&line)?)
}

/// Serialize storage mutations with worker launch without a sidecar lock file.
pub fn lock_session(paths: &AppPaths, id: &str) -> Result<fs::File> {
    use std::os::unix::fs::MetadataExt;
    validate_id(id)?;
    let dir = paths.session_dir(id);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(&dir)
        .context("Session folder is unavailable or is not an ordinary directory")?;
    file.lock_exclusive()?;
    let current = fs::symlink_metadata(&dir)?;
    let opened = file.metadata()?;
    anyhow::ensure!(
        current.is_dir() && current.dev() == opened.dev() && current.ino() == opened.ino(),
        "Session folder changed while waiting"
    );
    anyhow::ensure!(
        read_header(&dir)?.id == id,
        "Session header does not match its folder"
    );
    Ok(file)
}

pub fn rename(paths: &AppPaths, id: &str, name: &str) -> Result<String> {
    let title = name.split_whitespace().collect::<Vec<_>>().join(" ");
    anyhow::ensure!(!title.is_empty(), "Chat name cannot be empty");
    let _guard = lock_session(paths, id)?;
    let dir = paths.session_dir(id);
    let temporary = dir.join(format!(".title-{}", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        writeln!(file, "{title}")?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join("title"))?;
        fs::File::open(&dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result?;
    Ok(title)
}

pub fn delete(paths: &AppPaths, id: &str) -> Result<()> {
    let _guard = lock_session(paths, id)?;
    let dir = paths.session_dir(id);
    let canonical = fs::canonicalize(&dir)?;
    let header = read_header(&dir)?;
    let cwd = fs::canonicalize(&header.cwd).unwrap_or(header.cwd);
    anyhow::ensure!(
        !cwd.starts_with(&canonical),
        "Working folder is inside this session folder; move it before deleting the chat"
    );
    stop_worker(paths, id)?;
    // remove_dir_all unlinks contained symlinks without following their targets.
    // Only the ID-derived session directory is ever passed here.
    fs::remove_dir_all(&dir).context("Remove chat history and attachments")?;
    fs::File::open(paths.sessions_dir())?.sync_all()?;
    Ok(())
}

pub fn start_worker(paths: &AppPaths, id: &str) -> Result<()> {
    let _guard = lock_session(paths, id)?;
    let executable = std::env::current_exe().context("locate BashKitten executable")?;
    let binary = std::env::var_os("BASHKITTEN_AGENT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| executable.with_file_name("bashkitten-agent"));
    // The CLI invoked through bash must belong to the same installation as the
    // process launching this session, including user-local installations.
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let search_path = std::env::join_paths(
        executable
            .parent()
            .into_iter()
            .map(Path::to_path_buf)
            .chain(std::env::split_paths(&inherited_path)),
    )
    .context("construct session command path")?;
    let mut path_env = std::ffi::OsString::from("PATH=");
    path_env.push(search_path);
    let unit = format!("bashkitten-session-{id}.service");
    let header = read_header(&paths.session_dir(id))?;
    let session_env = format!("BASHKITTEN_SESSION_ID={id}");
    let parent_env = format!(
        "BASHKITTEN_PARENT_ID={}",
        header.parent_session.as_deref().unwrap_or("")
    );
    let config_env = format!("BASHKITTEN_CONFIG_DIR={}", paths.config.display());
    let data_env = format!("BASHKITTEN_DATA_DIR={}", paths.data.display());
    let runtime_env = format!("BASHKITTEN_RUNTIME_DIR={}", paths.runtime.display());
    let status = Command::new("systemd-run")
        .args([
            "--user",
            "--quiet",
            "--collect",
            "--unit",
            &unit,
            "--property=PartOf=bashkitten.target",
            "--property=KillMode=control-group",
            "--setenv",
            &session_env,
            "--setenv",
            &parent_env,
            "--setenv",
            &config_env,
            "--setenv",
            &data_env,
            "--setenv",
            &runtime_env,
        ])
        .arg("--setenv")
        .arg(path_env)
        .arg(&binary)
        .args(["--session", id])
        .status()
        .context("start session through systemd-run")?;
    if !status.success() {
        bail!("systemd-run failed with {status}");
    }
    let socket = control_socket(paths, id)?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if socket_is_live(&socket) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("Agent session did not create its control socket")
}

pub fn stop_worker(paths: &AppPaths, id: &str) -> Result<Value> {
    validate_id(id)?;
    let socket = control_socket(paths, id)?;
    if !socket_is_live(&socket) {
        return Ok(Value::Null);
    }
    let reply = send(paths, id, &ControlRequest::Stop)?;
    let started = Instant::now();
    while socket_is_live(&socket) {
        if started.elapsed() >= Duration::from_secs(30) {
            bail!(
                "Session {id} has not finished saving after cancellation; it has not been force-killed"
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(reply.data)
}

pub fn new_message_entry(role: &str, content: Value) -> Value {
    json!({
        "type": "message",
        "id": &Uuid::new_v4().simple().to_string()[..8],
        "parentId": Value::Null,
        "timestamp": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "message": { "role": role, "content": content, "timestamp": Utc::now().timestamp_millis() }
    })
}

pub fn save_provider_auth(paths: &AppPaths, value: &Value) -> Result<()> {
    atomic_private_json(&paths.provider_auth_file(), value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn rename_and_delete_only_touch_the_selected_session_storage() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let cwd = root.path().join("project");
        fs::create_dir(&cwd).unwrap();
        fs::write(cwd.join("keep.py"), "project stays").unwrap();
        let request = NewSession {
            cwd: cwd.clone(),
            model: "p/m".into(),
            thinking: "off".into(),
            model_parameters: json!({}),
            prompt: "Original chat".into(),
            attachments: vec![],
            parent: None,
        };
        let id = create(&paths, &request).unwrap();
        let sibling = create(&paths, &request).unwrap();
        let dir = paths.session_dir(&id);
        let history = fs::read(dir.join("000001.jsonl")).unwrap();
        fs::write(dir.join("attachments/example.txt"), "private copy").unwrap();
        symlink(&cwd, dir.join("attachments/project-link")).unwrap();
        assert_eq!(
            rename(&paths, &id, "  Renamed  chat 🐈  ").unwrap(),
            "Renamed chat 🐈"
        );
        assert_eq!(fs::read(dir.join("000001.jsonl")).unwrap(), history);
        assert_eq!(
            fs::metadata(dir.join("title"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(rename(&paths, &id, "  ").is_err());
        delete(&paths, &id).unwrap();
        assert!(!dir.exists());
        assert!(paths.session_dir(&sibling).is_dir());
        assert_eq!(
            fs::read_to_string(cwd.join("keep.py")).unwrap(),
            "project stays"
        );
        assert!(lock_session(&paths, &id).is_err());
        assert!(delete(&paths, "../project").is_err());
        let linked = "linked-session";
        symlink(&cwd, paths.session_dir(linked)).unwrap();
        assert!(delete(&paths, linked).is_err());
        assert!(rename(&paths, linked, "Bad").is_err());
        assert!(cwd.join("keep.py").exists());
    }

    #[test]
    fn deletion_refuses_a_working_folder_inside_session_storage() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            runtime: root.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let id = create(
            &paths,
            &NewSession {
                cwd: root.path().into(),
                model: "p/m".into(),
                thinking: "off".into(),
                model_parameters: json!({}),
                prompt: "Nested".into(),
                attachments: vec![],
                parent: None,
            },
        )
        .unwrap();
        let dir = paths.session_dir(&id);
        let project = dir.join("project");
        fs::create_dir(&project).unwrap();
        let mut header = read_header(&dir).unwrap();
        header.cwd = project.clone();
        let mut file = fs::File::create(dir.join("000001.jsonl")).unwrap();
        serde_json::to_writer(&mut file, &header).unwrap();
        writeln!(file).unwrap();
        assert!(
            delete(&paths, &id)
                .unwrap_err()
                .to_string()
                .contains("Working folder")
        );
        assert!(project.exists());
    }

    #[test]
    fn title_is_local_and_short() {
        assert_eq!(shorten_title("\n  hello   there \nsecond"), "hello there");
        assert!(shorten_title(&"x".repeat(80)).ends_with('…'));
    }

    #[test]
    fn changed_folder_persists_regroups_and_forks_at_historical_folder() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: temp.path().join("c"),
            data: temp.path().join("d"),
            runtime: temp.path().join("r"),
        };
        paths.ensure().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let id = create(
            &paths,
            &NewSession {
                cwd: first.clone(),
                model: "p/m".into(),
                thinking: "off".into(),
                model_parameters: json!({}),
                prompt: "folder test".into(),
                attachments: vec![],
                parent: None,
            },
        )
        .unwrap();
        let message = |id: &str| json!({"type":"message","id":id,"parentId":null,"timestamp":"2026-09-05T00:00:00Z","message":{"role":"user","content":id,"timestamp":1}});
        append_values(&paths, &id, &[message("before-change")]).unwrap();
        let dir = paths.session_dir(&id);
        let old = fs::read(dir.join("000001.jsonl")).unwrap();
        let mut header = read_header(&dir).unwrap();
        header.initial_cwd = Some(first.clone());
        header.cwd = second.clone();
        let event = json!({"type":"custom","id":"folder-change","parentId":"before-change","timestamp":"2026-09-05T00:00:01Z","customType":"bashkitten.cwd","data":{"cwd":second}});
        replace_current_header(&dir, &header, Some(&event)).unwrap();
        append_values(&paths, &id, &[message("after-change")]).unwrap();
        assert_eq!(read_header(&dir).unwrap().cwd, second);
        assert_eq!(list(&paths).unwrap()[0].cwd, Some(second.clone()));
        let saved = fs::read(dir.join("000001.jsonl")).unwrap();
        let original_messages = &old[old.iter().position(|byte| *byte == b'\n').unwrap() + 1..];
        assert!(
            saved
                .windows(original_messages.len())
                .any(|part| part == original_messages)
        );
        let before = fork_at(&paths, &id, "before-change").unwrap();
        let after = fork_at(&paths, &id, "after-change").unwrap();
        assert_eq!(read_header(&paths.session_dir(&before)).unwrap().cwd, first);
        assert_eq!(read_header(&paths.session_dir(&after)).unwrap().cwd, second);
        assert!(validate_cwd(Path::new("relative")).is_err());
        assert!(validate_cwd(&dir.join("title")).is_err());
        assert!(validate_cwd(&temp.path().join("missing")).is_err());
        assert_eq!(
            dir.join("000001.jsonl")
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn session_layout() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: temp.path().join("c"),
            data: temp.path().join("d"),
            runtime: temp.path().join("r"),
        };
        paths.ensure().unwrap();
        let id = create(
            &paths,
            &NewSession {
                cwd: temp.path().to_owned(),
                model: "p/m".into(),
                thinking: "off".into(),
                model_parameters: json!({}),
                prompt: "hello".into(),
                attachments: vec![],
                parent: None,
            },
        )
        .unwrap();
        assert!(paths.session_dir(&id).join("000001.jsonl").exists());
        assert_eq!(list(&paths).unwrap()[0].title, "hello");

        let source = temp.path().join("notes for agent.txt");
        fs::write(&source, "attachment body").unwrap();
        let copied = copy_attachments(&paths, &id, &[source]).unwrap();
        assert_eq!(copied[0].file_name().unwrap(), "notes for agent.txt");
        assert!(copied[0].starts_with(paths.session_dir(&id).join("attachments")));
        assert_eq!(fs::read_to_string(&copied[0]).unwrap(), "attachment body");
        assert_eq!(
            copied[0].metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn fork_preserves_exact_history_and_copies_attachments_through_nested_forks() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config: temp.path().join("c"),
            data: temp.path().join("d"),
            runtime: temp.path().join("r"),
        };
        paths.ensure().unwrap();
        let source_id = create(
            &paths,
            &NewSession {
                cwd: temp.path().to_owned(),
                model: "p/m".into(),
                thinking: "off".into(),
                model_parameters: json!({}),
                prompt: "fork source".into(),
                attachments: vec![],
                parent: None,
            },
        )
        .unwrap();
        let upload = Uuid::new_v4().to_string();
        let attachment_dir = paths
            .session_dir(&source_id)
            .join("attachments")
            .join(&upload);
        ensure_private_dir(&attachment_dir).unwrap();
        let attachment = attachment_dir.join("quoted \"model\".txt");
        fs::write(&attachment, "kept attachment").unwrap();
        set_private_file(&attachment).unwrap();
        append_values(
            &paths,
            &source_id,
            &[
                json!({
                    "type": "message",
                    "id": "first-entry",
                    "parentId": null,
                    "timestamp": "2026-01-01T00:00:00.000Z",
                    "message": {
                        "role": "user",
                        "content": [
                            {"type": "text", "text": format!("Keep the exact historical reference: {}", attachment.display())},
                            {"type": "attachment", "name": attachment.file_name().unwrap().to_str().unwrap(), "path": attachment, "mimeType": "text/plain"}
                        ],
                        "timestamp": 1
                    }
                }),
                json!({
                    "type": "message",
                    "id": "second-entry",
                    "parentId": "first-entry",
                    "timestamp": "2026-01-01T00:00:01.000Z",
                    "message": {"role": "user", "content": [{"type": "text", "text": "second"}], "timestamp": 2}
                }),
            ],
        )
        .unwrap();

        let fork_id = fork_at(&paths, &source_id, "first-entry").unwrap();
        let entries = read_segment(&paths, &fork_id, 1).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["id"], fork_id);
        assert_eq!(entries[0]["parentSession"], source_id);
        assert_eq!(entries[1]["id"], "first-entry");
        let original = read_segment(&paths, &source_id, 1).unwrap();
        assert_eq!(entries[1], original[1]);
        let original_line = fs::read_to_string(paths.session_dir(&source_id).join("000001.jsonl"))
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()
            .to_owned();
        let fork_line = fs::read_to_string(paths.session_dir(&fork_id).join("000001.jsonl"))
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()
            .to_owned();
        assert_eq!(fork_line, original_line);
        let nested_id = fork_at(&paths, &fork_id, "first-entry").unwrap();
        let nested = read_segment(&paths, &nested_id, 1).unwrap();
        assert_eq!(nested[1], original[1]);
        assert_eq!(nested[0]["parentSession"], fork_id);
        for id in [&fork_id, &nested_id] {
            let copied = paths
                .session_dir(id)
                .join("attachments")
                .join(&upload)
                .join(attachment.file_name().unwrap());
            assert_eq!(fs::read_to_string(&copied).unwrap(), "kept attachment");
            assert_eq!(
                fs::metadata(copied).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
