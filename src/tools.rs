//! Pinned Pi-compatible implementations of the seven built-in Linux tools.
//!
//! The model-visible contracts in [`tool_definitions`] intentionally mirror Pi.
//! Tool failures are returned as [`ToolError`] so the caller can expose them as
//! an `isError` tool result without losing the useful error text.

#[cfg(test)]
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};
use tokio::time::{Instant, sleep_until};
use unicode_normalization::UnicodeNormalization;

pub const DEFAULT_MAX_LINES: usize = 2_000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters: Value,
    pub prompt_snippet: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_guidelines: Vec<String>,
}

fn string_schema(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn number_schema(description: &str) -> Value {
    json!({ "type": "number", "description": description })
}

fn number_value(value: f64) -> Value {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value < i64::MAX as f64 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn bool_schema(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    let mut schema = json!({ "type": "object", "properties": properties });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

/// The exact model-visible tool descriptions, schemas, snippets, and guidelines
/// from the commit in PI_UPSTREAM.md. BashKitten exposes all seven tools at once.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "bash".into(),
            label: "bash".into(),
            description: "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.".into(),
            parameters: object_schema(
                json!({
                    "command": string_schema("Shell command to execute"),
                    "timeout": number_schema("Timeout in seconds (optional, no default timeout)")
                }),
                &["command"],
            ),
            prompt_snippet: "Execute bash commands (ls, grep, find, etc.)".into(),
            prompt_guidelines: vec!["You can inspect PI_* environment variables for current model and session details.".into()],
        },
        ToolDefinition {
            name: "read".into(),
            label: "read".into(),
            description: "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.".into(),
            parameters: object_schema(
                json!({
                    "path": string_schema("Path to the file to read (relative or absolute)"),
                    "offset": number_schema("Line number to start reading from (1-indexed)"),
                    "limit": number_schema("Maximum number of lines to read")
                }),
                &["path"],
            ),
            prompt_snippet: "Read file contents".into(),
            prompt_guidelines: vec!["Use read to examine files instead of cat or sed.".into()],
        },
        ToolDefinition {
            name: "edit".into(),
            label: "edit".into(),
            description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".into(),
            parameters: object_schema(
                json!({
                    "path": string_schema("Path to the file to edit (relative or absolute)"),
                    "edits": {
                        "type": "array",
                        "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                        "items": object_schema(
                            json!({
                                "oldText": string_schema("Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."),
                                "newText": string_schema("Replacement text for this targeted edit.")
                            }),
                            &["oldText", "newText"],
                        )
                    }
                }),
                &["path", "edits"],
            ),
            prompt_snippet: "Make precise file edits with exact text replacement, including multiple disjoint edits in one call".into(),
            prompt_guidelines: vec![
                "Use edit for precise changes (edits[].oldText must match exactly)".into(),
                "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls".into(),
                "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.".into(),
                "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.".into(),
            ],
        },
        ToolDefinition {
            name: "write".into(),
            label: "write".into(),
            description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".into(),
            parameters: object_schema(
                json!({
                    "path": string_schema("Path to the file to write (relative or absolute)"),
                    "content": string_schema("Content to write to the file")
                }),
                &["path", "content"],
            ),
            prompt_snippet: "Create or overwrite files".into(),
            prompt_guidelines: vec!["Use write only for new files or complete rewrites.".into()],
        },
        ToolDefinition {
            name: "grep".into(),
            label: "grep".into(),
            description: "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars.".into(),
            parameters: object_schema(
                json!({
                    "pattern": string_schema("Search pattern (regex or literal string)"),
                    "path": string_schema("Directory or file to search (default: current directory)"),
                    "glob": string_schema("Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"),
                    "ignoreCase": bool_schema("Case-insensitive search (default: false)"),
                    "literal": bool_schema("Treat pattern as literal string instead of regex (default: false)"),
                    "context": number_schema("Number of lines to show before and after each match (default: 0)"),
                    "limit": number_schema("Maximum number of matches to return (default: 100)")
                }),
                &["pattern"],
            ),
            prompt_snippet: "Search file contents for patterns (respects .gitignore)".into(),
            prompt_guidelines: vec![],
        },
        ToolDefinition {
            name: "find".into(),
            label: "find".into(),
            description: "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first).".into(),
            parameters: object_schema(
                json!({
                    "pattern": string_schema("Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"),
                    "path": string_schema("Directory to search in (default: current directory)"),
                    "limit": number_schema("Maximum number of results (default: 1000)")
                }),
                &["pattern"],
            ),
            prompt_snippet: "Find files by glob pattern (respects .gitignore)".into(),
            prompt_guidelines: vec![],
        },
        ToolDefinition {
            name: "ls".into(),
            label: "ls".into(),
            description: "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first).".into(),
            parameters: object_schema(
                json!({
                    "path": string_schema("Directory to list (default: current directory)"),
                    "limit": number_schema("Maximum number of entries to return (default: 500)")
                }),
                &[],
            ),
            prompt_snippet: "List directory contents".into(),
            prompt_guidelines: vec![],
        },
    ]
}

pub fn tool_definition(name: &str) -> Option<ToolDefinition> {
    tool_definitions()
        .into_iter()
        .find(|tool| tool.name == name)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolResult {
    fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text { text: text.into() }],
            details: None,
        }
    }

    pub fn text_content(&self) -> Option<&str> {
        self.content.iter().find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Image { .. } => None,
        })
    }
}

#[derive(Debug)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<io::Error> for ToolError {
    fn from(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

pub type ToolUpdateCallback = dyn Fn(ToolResult) + Send + Sync;

#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationInner>,
}

#[derive(Debug, Default)]
struct CancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub cancellation: CancellationToken,
    pub command_prefix: Option<String>,
    pub session_environment: HashMap<String, String>,
    pub model_supports_images: bool,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            cancellation: CancellationToken::default(),
            command_prefix: None,
            session_environment: HashMap::new(),
            model_supports_images: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

fn line_slices(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines = line_slices(content);
    let total_lines = lines.len();
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.into(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }
    if lines.first().is_some_and(|line| line.len() > max_bytes) {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }
    let mut output = Vec::new();
    let mut bytes = 0;
    let mut truncated_by = TruncatedBy::Lines;
    for (index, line) in lines.iter().take(max_lines).enumerate() {
        let line_bytes = line.len() + usize::from(index > 0);
        if bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output.push(*line);
        bytes += line_bytes;
    }
    if output.len() >= max_lines && bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    let content = output.join("\n");
    TruncationResult {
        output_bytes: content.len(),
        output_lines: output.len(),
        content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines = line_slices(content);
    let total_lines = lines.len();
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.into(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }
    let mut output = Vec::new();
    let mut bytes = 0;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    for line in lines.iter().rev().take(max_lines) {
        let line_bytes = line.len() + usize::from(!output.is_empty());
        if bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if output.is_empty() {
                let mut start = line.len().saturating_sub(max_bytes);
                while start < line.len() && !line.is_char_boundary(start) {
                    start += 1;
                }
                output.push(&line[start..]);
                last_line_partial = true;
            }
            break;
        }
        output.push(*line);
        bytes += line_bytes;
    }
    output.reverse();
    if output.len() >= max_lines && bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    let content = output.join("\n");
    TruncationResult {
        output_bytes: content.len(),
        output_lines: output.len(),
        content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

fn throw_if_cancelled(context: &ToolContext) -> Result<(), ToolError> {
    if context.cancellation.is_cancelled() {
        Err(ToolError::new("Operation aborted"))
    } else {
        Ok(())
    }
}

fn normalize_input_path(path: &str) -> String {
    let path = path.strip_prefix('@').unwrap_or(path);
    path.chars()
        .map(|character| match character {
            '\u{00a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

fn expand_tilde(path: &str) -> PathBuf {
    if (path == "~" || path.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        let mut expanded = PathBuf::from(home);
        if path.len() > 2 {
            expanded.push(&path[2..]);
        }
        return expanded;
    }
    PathBuf::from(path)
}

pub fn resolve_tool_path(path: &str, cwd: &Path) -> PathBuf {
    let normalized = normalize_input_path(path);
    let path = if normalized.starts_with("file://") {
        url::Url::parse(&normalized)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .unwrap_or_else(|| PathBuf::from(&normalized))
    } else {
        expand_tilde(&normalized)
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn resolve_read_path(path: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_tool_path(path, cwd);
    if resolved.exists() {
        return resolved;
    }
    let text = resolved.to_string_lossy();
    let am_pm = regex::Regex::new(r"(?i) (AM|PM)\.")
        .unwrap()
        .replace_all(&text, "\u{202f}$1.")
        .into_owned();
    let nfd: String = text.nfd().collect();
    for variant in [
        am_pm,
        nfd.clone(),
        text.replace('\'', "\u{2019}"),
        nfd.replace('\'', "\u{2019}"),
    ] {
        let candidate = PathBuf::from(variant);
        if candidate != resolved && candidate.exists() {
            return candidate;
        }
    }
    resolved
}

fn reject_nul_path(path: &Path) -> Result<(), ToolError> {
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        Err(ToolError::new(format!(
            "The argument 'path' must be a string, Uint8Array, or URL without null bytes. Received {}",
            crate::ecmascript::inspect_argument_string(&path.to_string_lossy())
        )))
    } else {
        Ok(())
    }
}

fn node_fs_error(error: io::Error, operation: &str, path: Option<&Path>) -> ToolError {
    let description = match error.raw_os_error() {
        Some(libc::ENOENT) => "ENOENT: no such file or directory",
        Some(libc::EISDIR) => "EISDIR: illegal operation on a directory",
        Some(libc::ENOTDIR) => "ENOTDIR: not a directory",
        Some(libc::EACCES) => "EACCES: permission denied",
        Some(libc::EPERM) => "EPERM: operation not permitted",
        Some(libc::ELOOP) => "ELOOP: too many symbolic links encountered",
        Some(libc::ENOSPC) => "ENOSPC: no space left on device",
        Some(libc::EROFS) => "EROFS: read-only file system",
        Some(libc::ENAMETOOLONG) => "ENAMETOOLONG: name too long",
        _ => return ToolError::new(error.to_string()),
    };
    ToolError::new(format!(
        "{description}, {operation}{}",
        path.map(|path| format!(" '{}'", path.display()))
            .unwrap_or_default()
    ))
}

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: Value) -> Result<T, ToolError> {
    serde_json::from_value(arguments)
        .map_err(|error| ToolError::new(format!("Invalid tool arguments: {error}")))
}

#[derive(Debug, Deserialize)]
pub struct ReadArgs {
    pub path: String,
    pub offset: Option<f64>,
    pub limit: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub ignore_case: Option<bool>,
    pub literal: Option<bool>,
    pub context: Option<f64>,
    pub limit: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct FindArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub limit: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct LsArgs {
    pub path: Option<String>,
    pub limit: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEdit {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Deserialize)]
pub struct EditArgs {
    pub path: String,
    pub edits: Vec<TextEdit>,
}

#[derive(Debug, Deserialize)]
pub struct BashArgs {
    pub command: String,
    pub timeout: Option<f64>,
}

pub async fn execute_tool(
    name: &str,
    arguments: Value,
    context: &ToolContext,
) -> Result<ToolResult, ToolError> {
    execute_tool_with_updates(name, arguments, context, None).await
}

pub async fn execute_tool_with_updates(
    name: &str,
    arguments: Value,
    context: &ToolContext,
    on_update: Option<&ToolUpdateCallback>,
) -> Result<ToolResult, ToolError> {
    let arguments = crate::tool_validation::prepare(name, arguments)?;
    match name {
        "read" => read(parse_args(arguments)?, context).await,
        "write" => write(parse_args(arguments)?, context).await,
        "edit" => edit(parse_args(arguments)?, context).await,
        "grep" => grep(parse_args(arguments)?, context).await,
        "find" => find(parse_args(arguments)?, context).await,
        "ls" => ls(parse_args(arguments)?, context).await,
        "bash" => bash(parse_args(arguments)?, context, on_update).await,
        _ => Err(ToolError::new(format!("Unknown tool: {name}"))),
    }
}

fn mutation_lock(path: &Path) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().expect("file mutation lock poisoned");
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

fn mutation_key(path: &Path) -> Result<PathBuf, ToolError> {
    // Pinned file-mutation-queue.ts resolves the complete path before entering
    // the operation (including its first cancellation check). Only missing-path
    // failures fall back to the lexical absolute path; other errors propagate.
    reject_nul_path(path)?;
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {
            Ok(path.to_path_buf())
        }
        Err(error) => Err(node_fs_error(error, "realpath", Some(path))),
    }
}

pub async fn read(args: ReadArgs, context: &ToolContext) -> Result<ToolResult, ToolError> {
    throw_if_cancelled(context)?;
    let path = resolve_read_path(&args.path, &context.cwd);
    reject_nul_path(&path)?;
    let access_path =
        std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("NUL checked above");
    if unsafe { libc::access(access_path.as_ptr(), libc::R_OK) } != 0 {
        return Err(node_fs_error(
            io::Error::last_os_error(),
            "access",
            Some(&path),
        ));
    }
    let bytes = fs::read(&path).map_err(|error| {
        if error.raw_os_error() == Some(libc::EISDIR) {
            node_fs_error(error, "read", None)
        } else {
            node_fs_error(error, "open", Some(&path))
        }
    })?;
    throw_if_cancelled(context)?;
    if let Some(mime_type) = crate::image::detect_mime(&bytes) {
        let processing = tokio::task::spawn_blocking(move || {
            crate::image::process(&bytes, mime_type, true, Default::default())
        });
        let processed = tokio::select! {
            result = processing => result.map_err(|error| ToolError::new(error.to_string()))?,
            _ = context.cancellation.cancelled() => return Err(ToolError::new("Operation aborted")),
        };
        let (mut note, image) = match processed {
            Ok(image) => {
                let mut note = format!("Read image file [{}]", image.mime_type);
                for hint in image.hints {
                    note.push('\n');
                    note.push_str(&hint);
                }
                (
                    note,
                    Some(ContentBlock::Image {
                        data: image.data,
                        mime_type: image.mime_type,
                    }),
                )
            }
            Err(message) => (format!("Read image file [{mime_type}]\n{message}"), None),
        };
        if !context.model_supports_images {
            note.push_str("\n[Current model does not support images. The image will be omitted from this request.]");
        }
        let mut content = vec![ContentBlock::Text { text: note }];
        content.extend(image);
        return Ok(ToolResult {
            content,
            details: None,
        });
    }

    let text = String::from_utf8_lossy(&bytes).into_owned();
    let all_lines: Vec<&str> = text.split('\n').collect();
    let total_file_lines = all_lines.len();
    let start = (args.offset.unwrap_or(0.0) - 1.0).max(0.0);
    let offset = start + 1.0;
    if start >= all_lines.len() as f64 {
        return Err(ToolError::new(format!(
            "Offset {} is beyond end of file ({} lines total)",
            crate::ecmascript::number_string(args.offset.unwrap_or(0.0)),
            all_lines.len()
        )));
    }
    let (selected, user_limited_lines) = if let Some(limit) = args.limit {
        let end = (start + limit).min(all_lines.len() as f64);
        let index = if end.trunc() < 0.0 {
            (all_lines.len() as f64 + end.trunc()).max(0.0) as usize
        } else {
            end as usize
        };
        (
            all_lines
                .get(start as usize..index)
                .unwrap_or_default()
                .join("\n"),
            Some(end - start),
        )
    } else {
        (all_lines[start as usize..].join("\n"), None)
    };
    let truncation = truncate_head(&selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut details = None;
    let output = if truncation.first_line_exceeds_limit {
        if start.fract() != 0.0 {
            // Pi indexes allLines[startLine] here without Array.slice's integer
            // coercion, then passes undefined to Buffer.byteLength.
            return Err(ToolError::new(
                "The \"string\" argument must be of type string or an instance of Buffer or ArrayBuffer. Received undefined",
            ));
        }
        details = Some(json!({ "truncation": truncation }));
        let offset = crate::ecmascript::number_string(offset);
        format!(
            "[Line {offset} is {}, exceeds {} limit. Use bash: sed -n '{offset}p' {} | head -c {DEFAULT_MAX_BYTES}]",
            format_size(all_lines[start as usize].len()),
            format_size(DEFAULT_MAX_BYTES),
            args.path
        )
    } else if truncation.truncated {
        let end = offset + truncation.output_lines as f64 - 1.0;
        let next = end + 1.0;
        let (offset, end, next) = (
            crate::ecmascript::number_string(offset),
            crate::ecmascript::number_string(end),
            crate::ecmascript::number_string(next),
        );
        let notice = if truncation.truncated_by == Some(TruncatedBy::Lines) {
            format!(
                "[Showing lines {offset}-{end} of {total_file_lines}. Use offset={next} to continue.]"
            )
        } else {
            format!(
                "[Showing lines {offset}-{end} of {total_file_lines} ({} limit). Use offset={next} to continue.]",
                format_size(DEFAULT_MAX_BYTES)
            )
        };
        let content = format!("{}\n\n{notice}", truncation.content);
        details = Some(json!({ "truncation": truncation }));
        content
    } else if let Some(limited) = user_limited_lines {
        if start + limited < all_lines.len() as f64 {
            let remaining = all_lines.len() as f64 - (start + limited);
            let next = start + limited + 1.0;
            let remaining = crate::ecmascript::number_string(remaining);
            let next = crate::ecmascript::number_string(next);
            format!(
                "{}\n\n[{remaining} more lines in file. Use offset={next} to continue.]",
                truncation.content
            )
        } else {
            truncation.content
        }
    } else {
        truncation.content
    };
    Ok(ToolResult {
        content: vec![ContentBlock::Text { text: output }],
        details,
    })
}

pub async fn write(args: WriteArgs, context: &ToolContext) -> Result<ToolResult, ToolError> {
    let path = resolve_tool_path(&args.path, &context.cwd);
    let lock = mutation_lock(&mutation_key(&path)?);
    let _guard = lock.lock().await;
    throw_if_cancelled(context)?;
    reject_nul_path(&path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| node_fs_error(error, "mkdir", Some(parent)))?;
    }
    throw_if_cancelled(context)?;
    fs::write(&path, args.content.as_bytes())
        .map_err(|error| node_fs_error(error, "open", Some(&path)))?;
    throw_if_cancelled(context)?;
    Ok(ToolResult::text(format!(
        "Successfully wrote to {}",
        args.path
    )))
}

pub async fn ls(args: LsArgs, context: &ToolContext) -> Result<ToolResult, ToolError> {
    throw_if_cancelled(context)?;
    let raw_path = args.path.as_deref().unwrap_or(".");
    let path = resolve_tool_path(raw_path, &context.cwd);
    if !path.exists() {
        return Err(ToolError::new(format!(
            "Path not found: {}",
            path.display()
        )));
    }
    if !path.metadata()?.is_dir() {
        return Err(ToolError::new(format!(
            "Not a directory: {}",
            path.display()
        )));
    }
    let effective_limit = args.limit.unwrap_or(500.0);
    let entries = fs::read_dir(&path)
        .map_err(|error| ToolError::new(format!("Cannot read directory: {error}")))?;
    let mut entries: Vec<OsString> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    let collator = icu_collator::Collator::try_new(Default::default(), Default::default())
        .expect("compiled ICU collation data");
    entries.sort_by(|left, right| {
        collator.compare(
            &left.to_string_lossy().to_lowercase(),
            &right.to_string_lossy().to_lowercase(),
        )
    });
    let mut results = Vec::new();
    let mut entry_limit_reached = false;
    for entry in entries {
        throw_if_cancelled(context)?;
        if results.len() as f64 >= effective_limit {
            entry_limit_reached = true;
            break;
        }
        let full_path = path.join(&entry);
        let Ok(metadata) = full_path.metadata() else {
            continue;
        };
        let mut name = entry.to_string_lossy().into_owned();
        if metadata.is_dir() {
            name.push('/');
        }
        results.push(name);
    }
    if results.is_empty() {
        return Ok(ToolResult::text("(empty directory)"));
    }
    let truncation = truncate_head(
        &results.join("\n"),
        9_007_199_254_740_991,
        DEFAULT_MAX_BYTES,
    );
    let mut output = truncation.content.clone();
    let mut notices = Vec::new();
    let mut detail = serde_json::Map::new();
    if entry_limit_reached {
        notices.push(format!(
            "{} entries limit reached. Use limit={} for more",
            crate::ecmascript::number_string(effective_limit),
            crate::ecmascript::number_string(effective_limit * 2.0)
        ));
        detail.insert("entryLimitReached".into(), number_value(effective_limit));
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        detail.insert("truncation".into(), json!(truncation));
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(ToolResult {
        content: vec![ContentBlock::Text { text: output }],
        details: (!detail.is_empty()).then_some(Value::Object(detail)),
    })
}

fn find_program(names: &[&str]) -> Option<PathBuf> {
    for name in names {
        if name.contains('/') {
            let path = PathBuf::from(name);
            if path.is_file() {
                return Some(path);
            }
            continue;
        }
        for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            let path = directory.join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

struct CapturedProcess {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stopped_at_limit: bool,
}

fn reject_nul_arguments(arguments: &[OsString]) -> Result<(), ToolError> {
    for (i, argument) in arguments.iter().enumerate() {
        if argument.as_encoded_bytes().contains(&0) {
            return Err(ToolError::new(format!(
                "The argument 'args[{i}]' must be a string without null bytes. Received {}",
                crate::ecmascript::inspect_argument_string(&argument.to_string_lossy())
            )));
        }
    }
    Ok(())
}

fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

async fn capture_process(
    program: &Path,
    arguments: &[OsString],
    cwd: &Path,
    cancellation: &CancellationToken,
    match_limit: Option<f64>,
) -> Result<CapturedProcess, ToolError> {
    if cancellation.is_cancelled() {
        return Err(ToolError::new("Operation aborted"));
    }
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.as_std_mut().process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| ToolError::new(error.to_string()))?;
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let stdout_task = tokio::spawn(async move {
        let mut data = Vec::new();
        if let Some(limit) = match_limit {
            use tokio::io::AsyncBufReadExt as _;
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut count = 0;
            loop {
                let start = data.len();
                if reader.read_until(b'\n', &mut data).await? == 0 {
                    break;
                }
                if serde_json::from_slice::<Value>(&data[start..])
                    .ok()
                    .is_some_and(|value| value["type"] == "match")
                {
                    count += 1;
                    if count as f64 >= limit {
                        if let Some(pid) = pid {
                            unsafe {
                                libc::kill(pid as i32, libc::SIGTERM);
                            }
                        }
                        return Ok::<_, io::Error>((data, true));
                    }
                }
            }
            Ok((data, false))
        } else {
            stdout.read_to_end(&mut data).await.map(|_| (data, false))
        }
    });
    let stderr_task = tokio::spawn(async move {
        let mut data = Vec::new();
        stderr.read_to_end(&mut data).await.map(|_| data)
    });
    let status = tokio::select! {
        result = child.wait() => result?,
        _ = cancellation.cancelled() => {
            kill_process_group(pid);
            let _ = child.wait().await;
            return Err(ToolError::new("Operation aborted"));
        }
    };
    let (stdout, stopped_at_limit) = stdout_task
        .await
        .map_err(|error| ToolError::new(error.to_string()))??;
    let stderr = stderr_task
        .await
        .map_err(|error| ToolError::new(error.to_string()))??;
    Ok(CapturedProcess {
        status,
        stdout,
        stderr,
        stopped_at_limit,
    })
}

fn truncate_line(line: &str) -> (String, bool) {
    if line.encode_utf16().count() <= GREP_MAX_LINE_LENGTH {
        return (line.into(), false);
    }
    let prefix = String::from_utf16_lossy(
        &line
            .encode_utf16()
            .take(GREP_MAX_LINE_LENGTH)
            .collect::<Vec<_>>(),
    );
    (format!("{prefix}... [truncated]"), true)
}

pub async fn grep(args: GrepArgs, context: &ToolContext) -> Result<ToolResult, ToolError> {
    throw_if_cancelled(context)?;
    let rg = find_program(&["rg"]).ok_or_else(|| {
        ToolError::new("ripgrep (rg) is not available and could not be downloaded")
    })?;
    let raw_search_path = args.path.as_deref().unwrap_or(".");
    let search_path = resolve_tool_path(raw_search_path, &context.cwd);
    let metadata = fs::metadata(&search_path)
        .map_err(|_| ToolError::new(format!("Path not found: {}", search_path.display())))?;
    let is_directory = metadata.is_dir();
    let context_lines = args.context.unwrap_or(0.0).max(0.0);
    let effective_limit = args.limit.unwrap_or(100.0).max(1.0);
    let mut command_args: Vec<OsString> = ["--json", "--line-number", "--color=never", "--hidden"]
        .into_iter()
        .map(Into::into)
        .collect();
    if args.ignore_case.unwrap_or(false) {
        command_args.push("--ignore-case".into());
    }
    if args.literal.unwrap_or(false) {
        command_args.push("--fixed-strings".into());
    }
    if let Some(glob) = &args.glob
        && !glob.is_empty()
    {
        command_args.extend(["--glob".into(), glob.into()]);
    }
    command_args.extend([
        "--".into(),
        args.pattern.clone().into(),
        search_path.as_os_str().into(),
    ]);
    reject_nul_arguments(&command_args)?;
    let captured = capture_process(
        &rg,
        &command_args,
        &context.cwd,
        &context.cancellation,
        Some(effective_limit),
    )
    .await
    .map_err(|error| {
        if error.to_string() == "Operation aborted" {
            error
        } else {
            ToolError::new(format!("Failed to run ripgrep: {error}"))
        }
    })?;
    let code = captured.status.code().unwrap_or(0);
    if !captured.stopped_at_limit && code != 0 && code != 1 {
        let stderr = String::from_utf8_lossy(&captured.stderr).trim().to_string();
        return Err(ToolError::new(if stderr.is_empty() {
            format!("ripgrep exited with code {code}")
        } else {
            stderr
        }));
    }
    #[derive(Debug)]
    struct Match {
        path: PathBuf,
        line: usize,
        text: Option<String>,
    }
    let mut matches = Vec::new();
    let mut match_count = 0;
    for line in String::from_utf8_lossy(&captured.stdout).lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) != Some("match") {
            continue;
        }
        match_count += 1;
        let Some(path) = event.pointer("/data/path/text").and_then(Value::as_str) else {
            continue;
        };
        let Some(line_number) = event.pointer("/data/line_number").and_then(Value::as_u64) else {
            continue;
        };
        let text = event
            .pointer("/data/lines/text")
            .and_then(Value::as_str)
            .map(str::to_owned);
        matches.push(Match {
            path: PathBuf::from(path),
            line: line_number as usize,
            text,
        });
        if match_count as f64 >= effective_limit {
            break;
        }
    }
    if match_count == 0 {
        return Ok(ToolResult::text("No matches found"));
    }
    let match_limit_reached = match_count as f64 >= effective_limit;
    let mut file_cache: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
    let mut lines_truncated = false;
    let mut output_lines = Vec::new();
    for matched in matches {
        throw_if_cancelled(context)?;
        let shown_path = if is_directory {
            let relative = matched
                .path
                .strip_prefix(&search_path)
                .unwrap_or(&matched.path);
            relative.to_string_lossy().replace('\\', "/")
        } else {
            matched
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        if context_lines == 0.0 && matched.text.is_some() {
            let value = matched
                .text
                .unwrap_or_default()
                .replace("\r\n", "\n")
                .replace('\r', "");
            let value = value.strip_suffix('\n').unwrap_or(&value).to_string();
            let (value, truncated) = truncate_line(&value);
            lines_truncated |= truncated;
            output_lines.push(format!("{shown_path}:{}: {value}", matched.line));
        } else {
            let file_lines = file_cache.entry(matched.path.clone()).or_insert_with(|| {
                fs::read(&matched.path).ok().map(|content| {
                    String::from_utf8_lossy(&content)
                        .replace("\r\n", "\n")
                        .replace('\r', "\n")
                        .split('\n')
                        .map(str::to_owned)
                        .collect()
                })
            });
            let Some(file_lines) = file_lines else {
                output_lines.push(format!(
                    "{shown_path}:{}: (unable to read file)",
                    matched.line
                ));
                continue;
            };
            let mut current = (matched.line as f64 - context_lines).max(1.0);
            let end = (matched.line as f64 + context_lines).min(file_lines.len() as f64);
            while current <= end {
                let value = if current.fract() == 0.0 {
                    file_lines
                        .get(current as usize - 1)
                        .map(String::as_str)
                        .unwrap_or("")
                } else {
                    ""
                };
                let (value, truncated) = truncate_line(&value.replace('\r', ""));
                lines_truncated |= truncated;
                if current == matched.line as f64 {
                    output_lines.push(format!(
                        "{shown_path}:{}: {value}",
                        crate::ecmascript::number_string(current)
                    ));
                } else {
                    output_lines.push(format!(
                        "{shown_path}-{}- {value}",
                        crate::ecmascript::number_string(current)
                    ));
                }
                current += 1.0;
            }
        }
    }
    let truncation = truncate_head(
        &output_lines.join("\n"),
        9_007_199_254_740_991,
        DEFAULT_MAX_BYTES,
    );
    let mut output = truncation.content.clone();
    let mut notices = Vec::new();
    let mut detail = serde_json::Map::new();
    if match_limit_reached {
        notices.push(format!(
            "{} matches limit reached. Use limit={} for more, or refine pattern",
            crate::ecmascript::number_string(effective_limit),
            crate::ecmascript::number_string(effective_limit * 2.0)
        ));
        detail.insert("matchLimitReached".into(), number_value(effective_limit));
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        detail.insert("truncation".into(), json!(truncation));
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        ));
        detail.insert("linesTruncated".into(), json!(true));
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(ToolResult {
        content: vec![ContentBlock::Text { text: output }],
        details: (!detail.is_empty()).then_some(Value::Object(detail)),
    })
}

fn inside_git_repository(path: &Path) -> bool {
    let mut current = Some(path);
    while let Some(directory) = current {
        if directory.join(".git").exists() {
            return true;
        }
        current = directory.parent();
    }
    false
}

fn program_help_contains(program: &Path, option: &str) -> bool {
    std::process::Command::new(program)
        .arg("--help")
        .output()
        .ok()
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains(option))
}

pub async fn find(args: FindArgs, context: &ToolContext) -> Result<ToolResult, ToolError> {
    throw_if_cancelled(context)?;
    let fd = find_program(&["fd", "fdfind"])
        .ok_or_else(|| ToolError::new("fd is not available and could not be downloaded"))?;
    let search_path = resolve_tool_path(args.path.as_deref().unwrap_or("."), &context.cwd);
    let effective_limit = args.limit.unwrap_or(1_000.0);
    let mut command_args: Vec<OsString> = ["--glob", "--color=never", "--hidden"]
        .into_iter()
        .map(Into::into)
        .collect();
    // fd 10 added --no-require-git. Older Debian fd versions already apply
    // ignore files without requiring a repository and reject this switch.
    if !inside_git_repository(&search_path) && program_help_contains(&fd, "--no-require-git") {
        command_args.push("--no-require-git".into());
    }
    command_args.extend([
        "--max-results".into(),
        crate::ecmascript::number_string(effective_limit).into(),
    ]);
    let mut effective_pattern = args.pattern;
    if effective_pattern.contains('/') {
        command_args.push("--full-path".into());
        if !effective_pattern.starts_with('/')
            && !effective_pattern.starts_with("**/")
            && effective_pattern != "**"
        {
            effective_pattern = format!("**/{effective_pattern}");
        }
    }
    command_args.extend([
        "--".into(),
        effective_pattern.into(),
        search_path.as_os_str().into(),
    ]);
    reject_nul_arguments(&command_args)?;
    let captured = capture_process(
        &fd,
        &command_args,
        &context.cwd,
        &context.cancellation,
        None,
    )
    .await
    .map_err(|error| {
        if error.to_string() == "Operation aborted" {
            error
        } else {
            ToolError::new(format!("Failed to run fd: {error}"))
        }
    })?;
    let raw = String::from_utf8_lossy(&captured.stdout);
    if !captured.status.success() && raw.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&captured.stderr).trim().to_string();
        let code = captured.status.code().unwrap_or(0);
        return Err(ToolError::new(if stderr.is_empty() {
            format!("fd exited with code {code}")
        } else {
            stderr
        }));
    }
    let lines: Vec<String> = raw
        .lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r').trim();
            if line.is_empty() {
                return None;
            }
            let source = Path::new(line);
            let relative = if source.is_absolute() {
                source.strip_prefix(&search_path).unwrap_or(source)
            } else {
                source
            };
            let mut output = relative.to_string_lossy().replace('\\', "/");
            if (line.ends_with('/') || line.ends_with(std::path::MAIN_SEPARATOR))
                && !output.ends_with('/')
            {
                output.push('/');
            }
            Some(output)
        })
        .collect();
    if lines.is_empty() {
        return Ok(ToolResult::text("No files found matching pattern"));
    }
    let result_limit_reached = lines.len() as f64 >= effective_limit;
    let truncation = truncate_head(&lines.join("\n"), 9_007_199_254_740_991, DEFAULT_MAX_BYTES);
    let mut output = truncation.content.clone();
    let mut notices = Vec::new();
    let mut detail = serde_json::Map::new();
    if result_limit_reached {
        notices.push(format!(
            "{} results limit reached. Use limit={} for more, or refine pattern",
            crate::ecmascript::number_string(effective_limit),
            crate::ecmascript::number_string(effective_limit * 2.0)
        ));
        detail.insert("resultLimitReached".into(), number_value(effective_limit));
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        detail.insert("truncation".into(), json!(truncation));
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(ToolResult {
        content: vec![ContentBlock::Text { text: output }],
        details: (!detail.is_empty()).then_some(Value::Object(detail)),
    })
}

#[cfg(test)]
fn parse_edit_args(arguments: Value) -> Result<EditArgs, ToolError> {
    parse_args(crate::tool_validation::prepare("edit", arguments)?)
}

fn normalize_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn normalize_for_fuzzy_match(text: &str) -> String {
    text.nfkc()
        .collect::<String>()
        .split('\n')
        .map(|line| line.trim_end_matches(crate::ecmascript::whitespace))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .map(|character| match character {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

fn count_occurrences(content: &str, needle: &str) -> usize {
    let content = normalize_for_fuzzy_match(content);
    let needle = normalize_for_fuzzy_match(needle);
    content.match_indices(&needle).count()
}

#[derive(Clone, Debug)]
struct MatchedEdit {
    edit_index: usize,
    start: usize,
    length: usize,
    replacement: String,
}

fn find_edit(content: &str, old_text: &str) -> Option<(usize, usize, bool)> {
    if let Some(index) = content.find(old_text) {
        return Some((index, old_text.len(), false));
    }
    let content = normalize_for_fuzzy_match(content);
    let old_text = normalize_for_fuzzy_match(old_text);
    content
        .find(&old_text)
        .map(|index| (index, old_text.len(), true))
}

fn split_lines_with_endings(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut start = 0;
    for (index, character) in content.char_indices() {
        if character == '\n' {
            result.push(&content[start..index + 1]);
            start = index + 1;
        }
    }
    if start < content.len() {
        result.push(&content[start..]);
    }
    result
}

fn apply_replacements(content: &str, edits: &[MatchedEdit], offset: usize) -> String {
    let mut result = content.to_string();
    for edit in edits.iter().rev() {
        let start = edit.start - offset;
        result.replace_range(start..start + edit.length, &edit.replacement);
    }
    result
}

fn preserve_unchanged_lines(
    original: &str,
    normalized: &str,
    edits: &[MatchedEdit],
) -> Result<String, ToolError> {
    let original_lines = split_lines_with_endings(original);
    let normalized_lines = split_lines_with_endings(normalized);
    if original_lines.len() != normalized_lines.len() {
        return Err(ToolError::new(
            "Cannot preserve unchanged lines because the base content has a different line count.",
        ));
    }
    let mut spans = Vec::with_capacity(normalized_lines.len());
    let mut offset = 0;
    for line in &normalized_lines {
        spans.push((offset, offset + line.len()));
        offset += line.len();
    }
    #[derive(Debug)]
    struct Group {
        start_line: usize,
        end_line: usize,
        edits: Vec<MatchedEdit>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for edit in edits {
        let start_line = spans
            .iter()
            .position(|&(start, end)| edit.start >= start && edit.start < end)
            .ok_or_else(|| ToolError::new("Replacement range is outside the base content."))?;
        let replacement_end = edit.start + edit.length;
        let mut end_line = start_line;
        while end_line < spans.len() && spans[end_line].1 < replacement_end {
            end_line += 1;
        }
        if end_line >= spans.len() {
            return Err(ToolError::new(
                "Replacement range is outside the base content.",
            ));
        }
        end_line += 1;
        if let Some(group) = groups
            .last_mut()
            .filter(|group| start_line < group.end_line)
        {
            group.end_line = group.end_line.max(end_line);
            group.edits.push(edit.clone());
        } else {
            groups.push(Group {
                start_line,
                end_line,
                edits: vec![edit.clone()],
            });
        }
    }
    let mut result = String::new();
    let mut original_line = 0;
    for group in groups {
        result.push_str(&original_lines[original_line..group.start_line].concat());
        let start = spans[group.start_line].0;
        let end = spans[group.end_line - 1].1;
        result.push_str(&apply_replacements(
            &normalized[start..end],
            &group.edits,
            start,
        ));
        original_line = group.end_line;
    }
    result.push_str(&original_lines[original_line..].concat());
    Ok(result)
}

fn apply_edits(
    content: &str,
    edits: &[TextEdit],
    path: &str,
) -> Result<(String, String), ToolError> {
    let edits: Vec<TextEdit> = edits
        .iter()
        .map(|edit| TextEdit {
            old_text: normalize_lf(&edit.old_text),
            new_text: normalize_lf(&edit.new_text),
        })
        .collect();
    for (index, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            let message = if edits.len() == 1 {
                format!("oldText must not be empty in {path}.")
            } else {
                format!("edits[{index}].oldText must not be empty in {path}.")
            };
            return Err(ToolError::new(message));
        }
    }
    let used_fuzzy = edits
        .iter()
        .any(|edit| find_edit(content, &edit.old_text).is_some_and(|match_| match_.2));
    let replacement_base = if used_fuzzy {
        normalize_for_fuzzy_match(content)
    } else {
        content.to_string()
    };
    let mut matched = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let Some((start, length, _)) = find_edit(&replacement_base, &edit.old_text) else {
            let message = if edits.len() == 1 {
                format!(
                    "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                )
            } else {
                format!(
                    "Could not find edits[{index}] in {path}. The oldText must match exactly including all whitespace and newlines."
                )
            };
            return Err(ToolError::new(message));
        };
        let occurrences = count_occurrences(&replacement_base, &edit.old_text);
        if occurrences > 1 {
            let message = if edits.len() == 1 {
                format!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                )
            } else {
                format!(
                    "Found {occurrences} occurrences of edits[{index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
                )
            };
            return Err(ToolError::new(message));
        }
        matched.push(MatchedEdit {
            edit_index: index,
            start,
            length,
            replacement: edit.new_text.clone(),
        });
    }
    matched.sort_by_key(|edit| edit.start);
    for pair in matched.windows(2) {
        if pair[0].start + pair[0].length > pair[1].start {
            return Err(ToolError::new(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                pair[0].edit_index, pair[1].edit_index
            )));
        }
    }
    let new_content = if used_fuzzy {
        preserve_unchanged_lines(content, &replacement_base, &matched)?
    } else {
        apply_replacements(&replacement_base, &matched, 0)
    };
    if content == new_content {
        let message = if edits.len() == 1 {
            format!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        };
        return Err(ToolError::new(message));
    }
    Ok((content.to_string(), new_content))
}

use crate::edit_diff::{display_diff, unified_patch};

pub async fn edit(args: EditArgs, context: &ToolContext) -> Result<ToolResult, ToolError> {
    if args.edits.is_empty() {
        return Err(ToolError::new(
            "Edit tool input is invalid. edits must contain at least one replacement.",
        ));
    }
    let path = resolve_tool_path(&args.path, &context.cwd);
    let lock = mutation_lock(&mutation_key(&path)?);
    let _guard = lock.lock().await;
    throw_if_cancelled(context)?;
    reject_nul_path(&path)?;
    let access_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .expect("NUL checked by mutation queue");
    if unsafe { libc::access(access_path.as_ptr(), libc::R_OK | libc::W_OK) } != 0 {
        let error = io::Error::last_os_error();
        throw_if_cancelled(context)?;
        let code = error
            .raw_os_error()
            .map(errno_name)
            .unwrap_or_else(|| error.to_string());
        return Err(ToolError::new(format!(
            "Could not edit file: {}. Error code: {code}.",
            args.path
        )));
    }
    throw_if_cancelled(context)?;
    let raw_bytes = fs::read(&path).map_err(|error| {
        if error.raw_os_error() == Some(libc::EISDIR) {
            node_fs_error(error, "read", None)
        } else {
            node_fs_error(error, "open", Some(&path))
        }
    })?;
    let raw = String::from_utf8_lossy(&raw_bytes).into_owned();
    throw_if_cancelled(context)?;
    let (bom, content) = raw
        .strip_prefix('\u{feff}')
        .map(|content| ("\u{feff}", content))
        .unwrap_or(("", raw.as_str()));
    let ending = if content
        .find("\r\n")
        .is_some_and(|crlf| content.find('\n').is_none_or(|lf| crlf < lf))
    {
        "\r\n"
    } else {
        "\n"
    };
    let normalized = normalize_lf(content);
    let (base, changed) = apply_edits(&normalized, &args.edits, &args.path)?;
    throw_if_cancelled(context)?;
    let restored = if ending == "\r\n" {
        changed.replace('\n', "\r\n")
    } else {
        changed.clone()
    };
    fs::write(&path, format!("{bom}{restored}"))
        .map_err(|error| node_fs_error(error, "open", Some(&path)))?;
    throw_if_cancelled(context)?;
    let (diff, first_changed_line) = display_diff(&base, &changed);
    Ok(ToolResult {
        content: vec![ContentBlock::Text {
            text: format!(
                "Successfully replaced {} block(s) in {}.",
                args.edits.len(),
                args.path
            ),
        }],
        details: Some(
            json!({ "diff": diff, "patch": unified_patch(&args.path, &base, &changed), "firstChangedLine": first_changed_line }),
        ),
    })
}

fn errno_name(number: i32) -> String {
    match number {
        libc::ENOENT => "ENOENT".into(),
        libc::EACCES => "EACCES".into(),
        libc::EISDIR => "EISDIR".into(),
        libc::EROFS => "EROFS".into(),
        other => other.to_string(),
    }
}

#[derive(Debug)]
struct OutputSnapshot {
    content: String,
    truncation: TruncationResult,
    full_output_path: Option<PathBuf>,
}

#[derive(Debug)]
struct OutputAccumulator {
    max_lines: usize,
    max_bytes: usize,
    max_rolling_bytes: usize,
    prefix: &'static str,
    buffered: Vec<u8>,
    tail: Vec<u8>,
    tail_starts_at_line_boundary: bool,
    total_bytes: usize,
    completed_lines: usize,
    has_open_line: bool,
    current_line_bytes: usize,
    temp_path: Option<PathBuf>,
    temp_file: Option<File>,
}

impl OutputAccumulator {
    fn new() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            max_rolling_bytes: DEFAULT_MAX_BYTES * 2,
            prefix: "pi-bash",
            buffered: Vec::new(),
            tail: Vec::new(),
            tail_starts_at_line_boundary: true,
            total_bytes: 0,
            completed_lines: 0,
            has_open_line: false,
            current_line_bytes: 0,
            temp_path: None,
            temp_file: None,
        }
    }

    fn total_lines(&self) -> usize {
        self.completed_lines + usize::from(self.has_open_line)
    }

    fn append(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.total_bytes += data.len();
        if let Some(last_newline) = data.iter().rposition(|byte| *byte == b'\n') {
            self.completed_lines += data.iter().filter(|byte| **byte == b'\n').count();
            self.current_line_bytes = data.len() - last_newline - 1;
            self.has_open_line = self.current_line_bytes > 0;
        } else {
            self.current_line_bytes += data.len();
            self.has_open_line = true;
        }

        if let Some(file) = &mut self.temp_file {
            file.write_all(data)?;
        } else {
            self.buffered.extend_from_slice(data);
        }
        self.tail.extend_from_slice(data);
        if self.tail.len() > self.max_rolling_bytes * 2 {
            let mut start = self.tail.len() - self.max_rolling_bytes;
            while start < self.tail.len() && (self.tail[start] & 0xc0) == 0x80 {
                start += 1;
            }
            self.tail_starts_at_line_boundary =
                start == 0 || self.tail.get(start.wrapping_sub(1)) == Some(&b'\n');
            self.tail.drain(..start);
        }
        if self.should_persist() {
            self.ensure_temp_file()?;
        }
        Ok(())
    }

    fn should_persist(&self) -> bool {
        self.total_bytes > self.max_bytes || self.total_lines() > self.max_lines
    }

    fn ensure_temp_file(&mut self) -> io::Result<()> {
        if self.temp_path.is_some() {
            return Ok(());
        }
        let mut random = [0u8; 8];
        rand::rng().fill_bytes(&mut random);
        let path =
            std::env::temp_dir().join(format!("{}-{}.log", self.prefix, hex::encode(random)));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&self.buffered)?;
        self.buffered.clear();
        self.temp_path = Some(path);
        self.temp_file = Some(file);
        Ok(())
    }

    fn snapshot(&mut self, persist_if_truncated: bool) -> io::Result<OutputSnapshot> {
        let visible = if self.tail_starts_at_line_boundary {
            self.tail.as_slice()
        } else if let Some(newline) = self.tail.iter().position(|byte| *byte == b'\n') {
            &self.tail[newline + 1..]
        } else {
            self.tail.as_slice()
        };
        let visible = String::from_utf8_lossy(visible);
        let mut truncation = truncate_tail(&visible, self.max_lines, self.max_bytes);
        truncation.truncated =
            self.total_lines() > self.max_lines || self.total_bytes > self.max_bytes;
        if truncation.truncated && truncation.truncated_by.is_none() {
            truncation.truncated_by = Some(if self.total_bytes > self.max_bytes {
                TruncatedBy::Bytes
            } else {
                TruncatedBy::Lines
            });
        }
        truncation.total_lines = self.total_lines();
        truncation.total_bytes = self.total_bytes;
        truncation.max_lines = self.max_lines;
        truncation.max_bytes = self.max_bytes;
        if persist_if_truncated && truncation.truncated {
            self.ensure_temp_file()?;
        }
        Ok(OutputSnapshot {
            content: truncation.content.clone(),
            truncation,
            full_output_path: self.temp_path.clone(),
        })
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.should_persist() {
            self.ensure_temp_file()?;
        }
        if let Some(file) = &mut self.temp_file {
            file.flush()?;
        }
        Ok(())
    }
}

fn bash_details(snapshot: &OutputSnapshot) -> Option<Value> {
    snapshot.truncation.truncated.then(|| {
        json!({
            "truncation": snapshot.truncation,
            "fullOutputPath": snapshot.full_output_path,
        })
    })
}

fn format_bash_output(snapshot: &OutputSnapshot, last_line_bytes: usize, empty: &str) -> String {
    let mut text = if snapshot.content.is_empty() {
        empty.to_string()
    } else {
        snapshot.content.clone()
    };
    if snapshot.truncation.truncated {
        let total = snapshot.truncation.total_lines;
        let start = total
            .saturating_sub(snapshot.truncation.output_lines)
            .saturating_add(1);
        let full_path = snapshot
            .full_output_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let notice = if snapshot.truncation.last_line_partial {
            format!(
                "[Showing last {} of line {total} (line is {}). Full output: {full_path}]",
                format_size(snapshot.truncation.output_bytes),
                format_size(last_line_bytes)
            )
        } else if snapshot.truncation.truncated_by == Some(TruncatedBy::Lines) {
            format!("[Showing lines {start}-{total} of {total}. Full output: {full_path}]")
        } else {
            format!(
                "[Showing lines {start}-{total} of {total} ({} limit). Full output: {full_path}]",
                format_size(DEFAULT_MAX_BYTES)
            )
        };
        text.push_str(&format!("\n\n{notice}"));
    }
    text
}

fn emit_bash_update(
    accumulator: &mut OutputAccumulator,
    callback: Option<&ToolUpdateCallback>,
) -> Result<(), ToolError> {
    let Some(callback) = callback else {
        return Ok(());
    };
    let snapshot = accumulator.snapshot(true)?;
    let mut details = serde_json::Map::new();
    if snapshot.truncation.truncated {
        details.insert("truncation".into(), json!(snapshot.truncation));
    }
    if let Some(path) = snapshot.full_output_path {
        details.insert("fullOutputPath".into(), json!(path));
    }
    callback(ToolResult {
        content: vec![ContentBlock::Text {
            text: snapshot.content,
        }],
        details: Some(Value::Object(details)),
    });
    Ok(())
}

async fn read_pipe<R: tokio::io::AsyncRead + Unpin>(
    mut pipe: R,
    sender: mpsc::UnboundedSender<Vec<u8>>,
) {
    let mut buffer = vec![0u8; 8192];
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if sender.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

pub async fn bash(
    args: BashArgs,
    context: &ToolContext,
    on_update: Option<&ToolUpdateCallback>,
) -> Result<ToolResult, ToolError> {
    const MAX_TIMEOUT_SECONDS: f64 = 2_147_483_647.0 / 1000.0;
    const UPDATE_INTERVAL: Duration = Duration::from_millis(100);
    if let Some(callback) = on_update {
        callback(ToolResult {
            content: vec![],
            details: None,
        });
    }
    let timeout_seconds = match args.timeout {
        None => None,
        Some(value) if !value.is_finite() || value <= 0.0 => {
            return Err(ToolError::new(
                "Invalid timeout: must be a finite number of seconds",
            ));
        }
        Some(value) if value > MAX_TIMEOUT_SECONDS => {
            return Err(ToolError::new(format!(
                "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
            )));
        }
        Some(value) => Some(value),
    };
    if context.cancellation.is_cancelled() {
        return Err(ToolError::new("Command aborted"));
    }
    if !context.cwd.exists() {
        return Err(ToolError::new(format!(
            "Working directory does not exist: {}\nCannot execute bash commands.",
            context.cwd.display()
        )));
    }
    let shell = if Path::new("/bin/bash").is_file() {
        PathBuf::from("/bin/bash")
    } else {
        find_program(&["bash", "sh"]).ok_or_else(|| ToolError::new("No usable shell found"))?
    };
    let command_text = context
        .command_prefix
        .as_ref()
        .map(|prefix| format!("{prefix}\n{}", args.command))
        .unwrap_or(args.command);
    reject_nul_arguments(&["-c".into(), command_text.clone().into()])?;
    let mut command = Command::new(shell);
    command
        .arg("-c")
        .arg(command_text)
        .current_dir(&context.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.as_std_mut().process_group(0);
    for key in [
        "PI_SESSION_ID",
        "PI_SESSION_FILE",
        "PI_PROVIDER",
        "PI_MODEL",
        "PI_REASONING_LEVEL",
    ] {
        command.env_remove(key);
    }
    command.envs(&context.session_environment);
    let mut child = command
        .spawn()
        .map_err(|error| ToolError::new(error.to_string()))?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(read_pipe(stdout, sender.clone()));
    let stderr_task = tokio::spawn(read_pipe(stderr, sender));
    let mut accumulator = OutputAccumulator::new();
    let deadline = timeout_seconds.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
    let mut stop_reason: Option<&'static str> = None;
    let mut last_update = Instant::now()
        .checked_sub(UPDATE_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut update_dirty = false;
    let mut update_deadline = None;
    let mut idle_deadline = None;
    let mut status = None;
    let mut pipes_closed = false;
    let mut wait = Box::pin(child.wait());
    loop {
        if status.is_some() && pipes_closed {
            break;
        }
        tokio::select! {
            result = &mut wait, if status.is_none() => {
                status = Some(result?);
                idle_deadline = Some(Instant::now() + UPDATE_INTERVAL);
            }
            chunk = receiver.recv(), if !pipes_closed => {
                if let Some(chunk) = chunk {
                    accumulator.append(&chunk)?;
                    if status.is_some() { idle_deadline = Some(Instant::now() + UPDATE_INTERVAL); }
                    if on_update.is_some() {
                        update_dirty = true;
                        if last_update.elapsed() >= UPDATE_INTERVAL {
                            emit_bash_update(&mut accumulator, on_update)?;
                            update_dirty = false;
                            last_update = Instant::now();
                            update_deadline = None;
                        } else {
                            update_deadline = Some(last_update + UPDATE_INTERVAL);
                        }
                    }
                } else {
                    pipes_closed = true;
                }
            }
            _ = wait_for_deadline(update_deadline), if update_dirty => {
                emit_bash_update(&mut accumulator, on_update)?;
                update_dirty = false;
                last_update = Instant::now();
                update_deadline = None;
            }
            _ = wait_for_deadline(idle_deadline), if status.is_some() => break,
            _ = context.cancellation.cancelled(), if stop_reason.is_none() => {
                stop_reason = Some("aborted");
                kill_process_group(pid);
            }
            _ = wait_for_deadline(deadline), if stop_reason.is_none() && deadline.is_some() => {
                stop_reason = Some("timeout");
                kill_process_group(pid);
            }
        }
    }
    drop(wait);
    // Match Pi's 100 ms post-exit idle grace while retaining live updates,
    // cancellation and timeout handling during descendant output above.
    stdout_task.abort();
    stderr_task.abort();
    accumulator.finish()?;
    if update_dirty {
        emit_bash_update(&mut accumulator, on_update)?;
    }
    let snapshot = accumulator.snapshot(true)?;
    let last_line_bytes = accumulator.current_line_bytes;
    match stop_reason {
        Some("aborted") => {
            let text = format_bash_output(&snapshot, last_line_bytes, "");
            return Err(ToolError::new(if text.is_empty() {
                "Command aborted".into()
            } else {
                format!("{text}\n\nCommand aborted")
            }));
        }
        Some("timeout") => {
            let text = format_bash_output(&snapshot, last_line_bytes, "");
            let seconds = crate::ecmascript::number_string(timeout_seconds.unwrap());
            let status = format!("Command timed out after {seconds} seconds");
            return Err(ToolError::new(if text.is_empty() {
                status
            } else {
                format!("{text}\n\n{status}")
            }));
        }
        _ => {}
    }
    let output = format_bash_output(&snapshot, last_line_bytes, "(no output)");
    if let Some(code) = status
        .expect("loop waits for shell exit")
        .code()
        .filter(|code| *code != 0)
    {
        return Err(ToolError::new(format!(
            "{output}\n\nCommand exited with code {code}"
        )));
    }
    Ok(ToolResult {
        content: vec![ContentBlock::Text { text: output }],
        details: bash_details(&snapshot),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn text(result: &ToolResult) -> &str {
        result.text_content().expect("text result")
    }

    #[test]
    fn definitions_are_the_seven_pinned_pi_contracts() {
        let definitions = tool_definitions();
        assert_eq!(
            definitions
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["bash", "read", "edit", "write", "grep", "find", "ls"]
        );
        assert_eq!(definitions[0].parameters["required"], json!(["command"]));
        assert_eq!(
            definitions[1].parameters["properties"]["offset"]["description"],
            "Line number to start reading from (1-indexed)"
        );
        assert_eq!(
            definitions[2].parameters["properties"]["edits"]["items"]["required"],
            json!(["oldText", "newText"])
        );
        assert_eq!(
            definitions[3].prompt_guidelines,
            ["Use write only for new files or complete rewrites."]
        );
        assert!(definitions[4].description.contains("100 matches or 50KB"));
        assert!(definitions[5].description.contains("1000 results or 50KB"));
        assert!(definitions[6].description.contains("500 entries or 50KB"));
    }

    #[test]
    fn truncation_matches_pi_head_and_tail_edges() {
        let head = truncate_head("a\nb\nc", 2, 100);
        assert_eq!(head.content, "a\nb");
        assert_eq!(head.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!((head.total_lines, head.output_lines), (3, 2));

        let tail = truncate_tail("a\nb\nc", 2, 100);
        assert_eq!(tail.content, "b\nc");
        assert_eq!(tail.truncated_by, Some(TruncatedBy::Lines));

        let overlong = truncate_head("abcdef\nnext", 20, 5);
        assert!(overlong.first_line_exceeds_limit);
        assert_eq!(overlong.content, "");

        let partial = truncate_tail("0123456789", 20, 5);
        assert_eq!(partial.content, "56789");
        assert!(partial.last_line_partial);

        let complete = truncate_head("a\n", 1, 2);
        assert!(!complete.truncated);
        assert_eq!(complete.total_lines, 1);
    }

    #[test]
    fn truncation_keeps_utf8_boundaries() {
        let result = truncate_tail("aé日", 10, 4);
        assert_eq!(result.content, "日");
        assert!(result.last_line_partial);
        assert!(result.content.is_char_boundary(0));
    }

    #[tokio::test]
    async fn read_supports_offsets_limits_notices_and_images() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("text.txt"), "one\ntwo\nthree\n").unwrap();
        let context = ToolContext::new(directory.path());
        let result = read(
            ReadArgs {
                path: "text.txt".into(),
                offset: Some(2.0),
                limit: Some(1.0),
            },
            &context,
        )
        .await
        .unwrap();
        assert_eq!(
            text(&result),
            "two\n\n[2 more lines in file. Use offset=3 to continue.]"
        );

        let error = read(
            ReadArgs {
                path: "text.txt".into(),
                offset: Some(10.0),
                limit: None,
            },
            &context,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Offset 10 is beyond end of file (4 lines total)"
        );

        let png = include_bytes!("../tests/fixtures/images/small.png").to_vec();
        fs::write(directory.path().join("image.bin"), &png).unwrap();
        let image = read(
            ReadArgs {
                path: "image.bin".into(),
                offset: None,
                limit: None,
            },
            &context,
        )
        .await
        .unwrap();
        assert_eq!(image.content.len(), 2);
        assert_eq!(text(&image), "Read image file [image/png]");
        assert!(
            matches!(&image.content[1], ContentBlock::Image { mime_type, data }
            if mime_type == "image/png" && BASE64_STANDARD.decode(data).unwrap() == png)
        );
    }

    #[tokio::test]
    async fn read_reports_oversized_first_line_and_standard_truncation() {
        let directory = tempdir().unwrap();
        let context = ToolContext::new(directory.path());
        fs::write(
            directory.path().join("long.txt"),
            format!("{}\nsecond", "x".repeat(DEFAULT_MAX_BYTES + 1)),
        )
        .unwrap();
        let result = read(
            ReadArgs {
                path: "long.txt".into(),
                offset: None,
                limit: None,
            },
            &context,
        )
        .await
        .unwrap();
        assert_eq!(
            text(&result),
            format!(
                "[Line 1 is 50.0KB, exceeds 50.0KB limit. Use bash: sed -n '1p' long.txt | head -c {DEFAULT_MAX_BYTES}]"
            )
        );
        assert!(result.details.is_some());

        let body = (1..=DEFAULT_MAX_LINES + 1)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(directory.path().join("lines.txt"), body).unwrap();
        let result = read(
            ReadArgs {
                path: "lines.txt".into(),
                offset: None,
                limit: None,
            },
            &context,
        )
        .await
        .unwrap();
        assert!(text(&result).ends_with(&format!(
            "[Showing lines 1-{DEFAULT_MAX_LINES} of {}. Use offset={} to continue.]",
            DEFAULT_MAX_LINES + 1,
            DEFAULT_MAX_LINES + 1
        )));
    }

    #[tokio::test]
    async fn write_creates_parents_and_overwrites() {
        let directory = tempdir().unwrap();
        let context = ToolContext::new(directory.path());
        let result = write(
            WriteArgs {
                path: "nested/file.txt".into(),
                content: "a😀".into(),
            },
            &context,
        )
        .await
        .unwrap();
        assert_eq!(text(&result), "Successfully wrote to nested/file.txt");
        assert_eq!(
            fs::read_to_string(directory.path().join("nested/file.txt")).unwrap(),
            "a😀"
        );
    }

    #[test]
    fn legacy_edit_arguments_are_accepted() {
        let args = parse_edit_args(json!({ "path": "a", "oldText": "x", "newText": "y" })).unwrap();
        assert_eq!(args.edits.len(), 1);
        let args = parse_edit_args(
            json!({ "path": "a", "edits": "{\"oldText\":\"x\",\"newText\":\"y\"}" }),
        )
        .unwrap();
        assert_eq!(args.edits.len(), 1);
    }

    #[tokio::test]
    async fn edit_matches_original_rejects_duplicates_and_overlap() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("edit.txt");
        let context = ToolContext::new(directory.path());
        fs::write(&path, "alpha\nbeta\ngamma\ndelta\n").unwrap();
        let result = edit(
            EditArgs {
                path: "edit.txt".into(),
                edits: vec![
                    TextEdit {
                        old_text: "alpha\n".into(),
                        new_text: "ALPHA\n".into(),
                    },
                    TextEdit {
                        old_text: "gamma\n".into(),
                        new_text: "GAMMA\n".into(),
                    },
                ],
            },
            &context,
        )
        .await
        .unwrap();
        assert_eq!(
            text(&result),
            "Successfully replaced 2 block(s) in edit.txt."
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "ALPHA\nbeta\nGAMMA\ndelta\n"
        );
        assert!(
            result.details.as_ref().unwrap()["patch"]
                .as_str()
                .unwrap()
                .contains("@@")
        );

        fs::write(&path, "foo foo foo").unwrap();
        let duplicate = edit(
            EditArgs {
                path: "edit.txt".into(),
                edits: vec![TextEdit {
                    old_text: "foo".into(),
                    new_text: "bar".into(),
                }],
            },
            &context,
        )
        .await
        .unwrap_err();
        assert!(duplicate.to_string().contains("Found 3 occurrences"));

        fs::write(&path, "abcdef").unwrap();
        let overlap = edit(
            EditArgs {
                path: "edit.txt".into(),
                edits: vec![
                    TextEdit {
                        old_text: "abc".into(),
                        new_text: "A".into(),
                    },
                    TextEdit {
                        old_text: "bcde".into(),
                        new_text: "B".into(),
                    },
                ],
            },
            &context,
        )
        .await
        .unwrap_err();
        assert!(
            overlap
                .to_string()
                .contains("edits[0] and edits[1] overlap")
        );
    }

    #[tokio::test]
    async fn edit_fuzzy_matching_preserves_untouched_lines_crlf_and_bom() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("fuzzy.txt");
        let context = ToolContext::new(directory.path());
        fs::write(&path, "\u{feff}keep  \r\nＡＢＣ１２３  \r\ncafe\u{301}\r\n").unwrap();
        edit(
            EditArgs {
                path: "fuzzy.txt".into(),
                edits: vec![TextEdit {
                    old_text: "ABC123\ncafé".into(),
                    new_text: "XYZ\ncoffee".into(),
                }],
            },
            &context,
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "\u{feff}keep  \r\nXYZ\r\ncoffee\r\n"
        );

        fs::write(&path, "console.log(‘hello’);\nhello\u{a0}world\n").unwrap();
        edit(
            EditArgs {
                path: "fuzzy.txt".into(),
                edits: vec![
                    TextEdit {
                        old_text: "console.log('hello');\n".into(),
                        new_text: "console.log('world');\n".into(),
                    },
                    TextEdit {
                        old_text: "hello world\n".into(),
                        new_text: "hello universe\n".into(),
                    },
                ],
            },
            &context,
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "console.log('world');\nhello universe\n"
        );
    }

    #[tokio::test]
    async fn ls_includes_dotfiles_directories_and_limit_notice() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("b"), "").unwrap();
        fs::write(directory.path().join(".hidden"), "").unwrap();
        fs::create_dir(directory.path().join("A")).unwrap();
        let context = ToolContext::new(directory.path());
        let result = ls(
            LsArgs {
                path: None,
                limit: None,
            },
            &context,
        )
        .await
        .unwrap();
        assert_eq!(text(&result), ".hidden\nA/\nb");
        let limited = ls(
            LsArgs {
                path: None,
                limit: Some(1.0),
            },
            &context,
        )
        .await
        .unwrap();
        assert!(text(&limited).contains("1 entries limit reached. Use limit=2 for more"));
        assert_eq!(limited.details.as_ref().unwrap()["entryLimitReached"], 1);
    }

    #[tokio::test]
    async fn grep_formats_matches_context_limits_and_long_lines() {
        if find_program(&["rg"]).is_none() {
            return;
        }
        let directory = tempdir().unwrap();
        fs::write(
            directory.path().join("a.txt"),
            format!("before\nneedle {}\nafter\n", "x".repeat(600)),
        )
        .unwrap();
        let context = ToolContext::new(directory.path());
        let result = grep(
            GrepArgs {
                pattern: "needle".into(),
                path: None,
                glob: Some("*.txt".into()),
                ignore_case: None,
                literal: Some(true),
                context: Some(1.0),
                limit: None,
            },
            &context,
        )
        .await
        .unwrap();
        assert!(text(&result).contains("a.txt-1- before"));
        assert!(text(&result).contains("a.txt:2: needle"));
        assert!(text(&result).contains("... [truncated]"));
        assert_eq!(result.details.as_ref().unwrap()["linesTruncated"], true);

        let none = grep(
            GrepArgs {
                pattern: "absent".into(),
                path: None,
                glob: None,
                ignore_case: None,
                literal: None,
                context: None,
                limit: None,
            },
            &context,
        )
        .await
        .unwrap();
        assert_eq!(text(&none), "No matches found");
    }

    #[tokio::test]
    async fn find_uses_fd_semantics_when_available() {
        if find_program(&["fd", "fdfind"]).is_none() {
            return;
        }
        let directory = tempdir().unwrap();
        fs::create_dir_all(directory.path().join("src/nested")).unwrap();
        fs::write(directory.path().join("src/a.rs"), "").unwrap();
        fs::write(directory.path().join("src/nested/b.rs"), "").unwrap();
        let context = ToolContext::new(directory.path());
        let result = find(
            FindArgs {
                pattern: "src/**/*.rs".into(),
                path: None,
                limit: None,
            },
            &context,
        )
        .await
        .unwrap();
        assert!(text(&result).contains("src/a.rs"));
        assert!(text(&result).contains("src/nested/b.rs"));
    }

    #[tokio::test]
    async fn bash_handles_success_errors_timeout_and_truncation() {
        let directory = tempdir().unwrap();
        let context = ToolContext::new(directory.path());
        let result = bash(
            BashArgs {
                command: "printf hello".into(),
                timeout: None,
            },
            &context,
            None,
        )
        .await
        .unwrap();
        assert_eq!(text(&result), "hello");
        let empty = bash(
            BashArgs {
                command: ":".into(),
                timeout: None,
            },
            &context,
            None,
        )
        .await
        .unwrap();
        assert_eq!(text(&empty), "(no output)");
        let failed = bash(
            BashArgs {
                command: "printf bad; exit 7".into(),
                timeout: None,
            },
            &context,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(failed.to_string(), "bad\n\nCommand exited with code 7");
        let timed_out = bash(
            BashArgs {
                command: "sleep 2".into(),
                timeout: Some(0.05),
            },
            &context,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(
            timed_out.to_string(),
            "Command timed out after 0.05 seconds"
        );

        let result = bash(
            BashArgs {
                command: format!(
                    "for i in $(seq 1 {}); do echo line$i; done",
                    DEFAULT_MAX_LINES + 1
                ),
                timeout: None,
            },
            &context,
            None,
        )
        .await
        .unwrap();
        assert!(result.details.is_some());
        assert!(text(&result).contains(&format!(
            "Showing lines 2-{} of {}",
            DEFAULT_MAX_LINES + 1,
            DEFAULT_MAX_LINES + 1
        )));
        let full_path = result.details.as_ref().unwrap()["fullOutputPath"]
            .as_str()
            .unwrap();
        assert!(Path::new(full_path).exists());
        let _ = fs::remove_file(full_path);
    }

    #[tokio::test]
    async fn cancellation_prevents_mutating_tools_and_kills_bash_group() {
        let directory = tempdir().unwrap();
        let context = ToolContext::new(directory.path());
        context.cancellation.cancel();
        let error = write(
            WriteArgs {
                path: "never".into(),
                content: "x".into(),
            },
            &context,
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "Operation aborted");
        assert!(!directory.path().join("never").exists());

        let running = ToolContext::new(directory.path());
        let cancel = running.cancellation.clone();
        let task = tokio::spawn(async move {
            bash(
                BashArgs {
                    command: "sleep 10".into(),
                    timeout: None,
                },
                &running,
                None,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
        assert_eq!(
            task.await.unwrap().unwrap_err().to_string(),
            "Command aborted"
        );
    }
}
