use crate::config::atomic_private_json;
use crate::paths::{AppPaths, ensure_private_dir, set_private_file};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
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
    #[serde(rename = "parentSession", skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewSession {
    pub cwd: PathBuf,
    pub model: String,
    pub thinking: String,
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
    Subscribe,
    Status,
    Stop,
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
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueAction {
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
    fs::write(&title_path, format!("{}\n", shorten_title(&request.prompt)))?;
    set_private_file(&title_path)?;

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
        parent_session: request.parent.clone(),
    };
    let segment = dir.join("000001.jsonl");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&segment)?;
    set_private_file(&segment)?;
    serde_json::to_writer(&mut file, &header)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(id)
}

pub fn control_socket(paths: &AppPaths, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(paths.session_dir(id).join("control.sock"))
}

pub fn current_segment(dir: &Path) -> Result<(u32, PathBuf)> {
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
    let reader = BufReader::new(fs::File::open(file)?);
    for line in reader.lines() {
        let line = line?;
        let value: Value = serde_json::from_str(&line)?;
        if value.get("type").and_then(Value::as_str) == Some("session") {
            return Ok(serde_json::from_value(value)?);
        }
    }
    // New compaction segments contain a compacted header rather than the original session header.
    let (_, first) = (1_u32, dir.join("000001.jsonl"));
    let line = BufReader::new(fs::File::open(first)?)
        .lines()
        .next()
        .context("Empty session")??;
    Ok(serde_json::from_str(&line)?)
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
    let bytes = fs::read(&path)?;
    let first_newline = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .context("Missing session header")?;
    let first: Value = serde_json::from_slice(&bytes[..first_newline])?;
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
        serde_json::to_writer(&mut output, header)?;
        output.write_all(b"\n")?;
        output.write_all(&bytes[first_newline + 1..])?;
        if let Some(event) = event {
            if !bytes.ends_with(b"\n") {
                output.write_all(b"\n")?;
            }
            serde_json::to_writer(&mut output, event)?;
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
            let value: Value = serde_json::from_str(&line?)?;
            if value["type"] == "custom" && value["customType"] == "bashkitten.cwd" {
                if let Some(cwd) = value["data"]["cwd"].as_str() {
                    header.cwd = cwd.into();
                }
            }
        }
        replace_current_header(dir, &header, None)?;
    }
    Ok(())
}

fn effective_model(dir: &Path, header: &SessionHeader) -> (String, String) {
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
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
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
        let title = fs::read_to_string(dir.join("title"))
            .unwrap_or_else(|_| "Untitled session".into())
            .trim()
            .to_owned();
        let header = read_header(&dir).ok();
        let effective = header.as_ref().map(|header| effective_model(&dir, header));
        sessions.push(SessionSummary {
            id: id.clone(),
            title,
            running: socket_is_live(&dir.join("control.sock")),
            modified,
            current_segment: segment,
            cwd: header.as_ref().map(|h| h.cwd.clone()),
            model: effective.as_ref().map(|(model, _)| model.clone()),
            thinking: effective.map(|(_, thinking)| thinking),
        });
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    Ok(sessions)
}

pub fn socket_is_live(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

pub fn read_segment(paths: &AppPaths, id: &str, number: u32) -> Result<Vec<Value>> {
    validate_id(id)?;
    if number == 0 || number > 999_999 {
        bail!("Invalid segment number");
    }
    let path = paths.session_dir(id).join(format!("{number:06}.jsonl"));
    let file = fs::File::open(path)?;
    BufReader::new(file)
        .lines()
        .map(|line| Ok(serde_json::from_str(&line?)?))
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

fn rewrite_attachment_paths(value: &mut Value, source: &str, destination: &str) {
    match value {
        Value::String(text) => {
            if text.contains(source) {
                *text = text.replace(source, destination);
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_attachment_paths(value, source, destination);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                rewrite_attachment_paths(value, source, destination);
            }
        }
        _ => {}
    }
}

fn copy_referenced_attachments(
    source_dir: &Path,
    destination_dir: &Path,
    retained_json: &str,
) -> Result<()> {
    let source_attachments = source_dir.join("attachments");
    let destination_attachments = destination_dir.join("attachments");
    ensure_private_dir(&destination_attachments)?;
    if !source_attachments.is_dir() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(&source_attachments)
        .min_depth(1)
        .follow_links(false)
    {
        let entry = entry?;
        let relative = entry.path().strip_prefix(&source_attachments)?;
        let destination = destination_attachments.join(relative);
        if entry.file_type().is_dir() {
            ensure_private_dir(&destination)?;
        } else if entry.file_type().is_file()
            && retained_json.contains(&entry.path().to_string_lossy().to_string())
        {
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

        let source_attachment_prefix = source_dir.join("attachments");
        let destination_attachment_prefix = final_dir.join("attachments");
        let source_attachment_prefix = source_attachment_prefix.to_string_lossy().into_owned();
        let destination_attachment_prefix =
            destination_attachment_prefix.to_string_lossy().into_owned();
        let mut retained_json = String::new();
        let mut found = false;

        'segments: for (name, source_segment) in segments {
            let destination_segment = temporary_dir.join(name);
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&destination_segment)?;
            set_private_file(&destination_segment)?;
            for line in BufReader::new(fs::File::open(&source_segment)?).lines() {
                let mut value: Value = serde_json::from_str(&line?)?;
                let is_header = value.get("type").and_then(Value::as_str) == Some("session");
                let is_target = value.get("id").and_then(Value::as_str) == Some(target_entry_id);
                if is_target && value.get("type").and_then(Value::as_str) != Some("message") {
                    bail!("Fork target is not a message");
                }
                retained_json.push_str(&serde_json::to_string(&value)?);
                retained_json.push('\n');
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
                rewrite_attachment_paths(
                    &mut value,
                    &source_attachment_prefix,
                    &destination_attachment_prefix,
                );
                serde_json::to_writer(&mut output, &value)?;
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
        let title = fs::read_to_string(source_dir.join("title"))
            .unwrap_or_else(|_| "Untitled session\n".into());
        fs::write(&title_path, title)?;
        set_private_file(&title_path)?;
        copy_referenced_attachments(&source_dir, &temporary_dir, &retained_json)?;
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
    let mut file = OpenOptions::new().append(true).open(&path)?;
    for value in values {
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    Ok(())
}

pub fn send(paths: &AppPaths, id: &str, request: &ControlRequest) -> Result<ControlReply> {
    let socket = control_socket(paths, id)?;
    let mut stream =
        UnixStream::connect(&socket).with_context(|| format!("connect {}", socket.display()))?;
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

pub fn start_worker(paths: &AppPaths, id: &str) -> Result<()> {
    validate_id(id)?;
    let binary = std::env::var("BASHKITTEN_AGENT_BIN")
        .unwrap_or_else(|_| "/usr/bin/bashkitten-agent".into());
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
            &binary,
            "--session",
            id,
        ])
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

pub fn stop_worker(paths: &AppPaths, id: &str) -> Result<()> {
    validate_id(id)?;
    let socket = control_socket(paths, id)?;
    if !socket_is_live(&socket) {
        return Ok(());
    }
    send(paths, id, &ControlRequest::Stop)?;
    let started = Instant::now();
    while socket_is_live(&socket) {
        if started.elapsed() >= Duration::from_secs(30) {
            bail!(
                "Session {id} has not finished saving after cancellation; it has not been force-killed"
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
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
    fn fork_copies_history_only_through_target_and_rewrites_attachments() {
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
        let attachment = attachment_dir.join("example.txt");
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
                            {"type": "text", "text": "first"},
                            {"type": "attachment", "name": "example.txt", "path": attachment, "mimeType": "text/plain"}
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
        let fork_attachment = entries[1]["message"]["content"][1]["path"]
            .as_str()
            .unwrap();
        assert!(fork_attachment.contains(&fork_id));
        assert!(!fork_attachment.contains(&source_id));
        assert_eq!(
            fs::read_to_string(fork_attachment).unwrap(),
            "kept attachment"
        );
        assert_eq!(
            fs::metadata(fork_attachment).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
