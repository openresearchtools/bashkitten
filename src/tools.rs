//! Pi v0.84.4-compatible implementations of the seven built-in Linux tools.
//!
//! The model-visible contracts in [`tool_definitions`] intentionally mirror Pi.
//! Tool failures are returned as [`ToolError`] so the caller can expose them as
//! an `isError` tool result without losing the useful error text.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
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
use tokio::time::{Instant, sleep_until, timeout};

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
/// from Pi v0.84.4. BashKitten exposes all seven tools at once.
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
            '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}' => ' ',
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
    let path = expand_tilde(&normalize_input_path(path));
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: Value) -> Result<T, ToolError> {
    serde_json::from_value(arguments)
        .map_err(|error| ToolError::new(format!("Invalid tool arguments: {error}")))
}

fn usize_number(
    value: Option<f64>,
    default: usize,
    minimum: usize,
    name: &str,
) -> Result<usize, ToolError> {
    match value {
        None => Ok(default),
        Some(number) if number.is_finite() && number.fract() == 0.0 && number >= minimum as f64 => {
            Ok(number as usize)
        }
        Some(_) => Err(ToolError::new(format!(
            "{name} must be an integer greater than or equal to {minimum}"
        ))),
    }
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
    match name {
        "read" => read(parse_args(arguments)?, context).await,
        "write" => write(parse_args(arguments)?, context).await,
        "edit" => edit(parse_edit_args(arguments)?, context).await,
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

fn mutation_key(path: &Path) -> PathBuf {
    if let Ok(path) = fs::canonicalize(path) {
        return path;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(parent) = fs::canonicalize(parent)
    {
        return parent.join(name);
    }
    path.to_path_buf()
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

fn read_u16_le(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32_le(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn read_i32_le(bytes: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut checked = Vec::with_capacity(4 + data.len());
    checked.extend_from_slice(kind);
    checked.extend_from_slice(data);
    output.extend_from_slice(&crc32(&checked).to_be_bytes());
}

/// Convert ordinary uncompressed 24/32-bit BMPs to PNG, matching Pi's rule that
/// BMP is never passed directly to model APIs.
fn bmp_to_png(bytes: &[u8]) -> Result<Vec<u8>, ToolError> {
    let pixel_offset =
        read_u32_le(bytes, 10).ok_or_else(|| ToolError::new("Invalid BMP image"))? as usize;
    let dib_size = read_u32_le(bytes, 14).ok_or_else(|| ToolError::new("Invalid BMP image"))?;
    let width = read_i32_le(bytes, 18).ok_or_else(|| ToolError::new("Invalid BMP image"))?;
    let height = read_i32_le(bytes, 22).ok_or_else(|| ToolError::new("Invalid BMP image"))?;
    let planes = read_u16_le(bytes, 26).ok_or_else(|| ToolError::new("Invalid BMP image"))?;
    let bits = read_u16_le(bytes, 28).ok_or_else(|| ToolError::new("Invalid BMP image"))?;
    let compression = read_u32_le(bytes, 30).ok_or_else(|| ToolError::new("Invalid BMP image"))?;
    if dib_size < 40
        || width <= 0
        || height == 0
        || planes != 1
        || !matches!(bits, 24 | 32)
        || compression != 0
    {
        return Err(ToolError::new(
            "Unsupported BMP image; expected uncompressed 24-bit or 32-bit pixels",
        ));
    }
    let width = width as usize;
    let rows = height.unsigned_abs() as usize;
    let bytes_per_pixel = (bits / 8) as usize;
    let stride = (width * bytes_per_pixel + 3) & !3;
    let image_end = pixel_offset
        .checked_add(
            stride
                .checked_mul(rows)
                .ok_or_else(|| ToolError::new("Invalid BMP image"))?,
        )
        .ok_or_else(|| ToolError::new("Invalid BMP image"))?;
    if image_end > bytes.len() {
        return Err(ToolError::new("Invalid BMP image"));
    }

    let mut raw = Vec::with_capacity(rows * (1 + width * 4));
    for output_row in 0..rows {
        raw.push(0); // PNG filter type: None.
        let source_row = if height > 0 {
            rows - 1 - output_row
        } else {
            output_row
        };
        let row = pixel_offset + source_row * stride;
        for column in 0..width {
            let pixel = row + column * bytes_per_pixel;
            raw.extend_from_slice(&[
                bytes[pixel + 2],
                bytes[pixel + 1],
                bytes[pixel],
                if bytes_per_pixel == 4 {
                    bytes[pixel + 3]
                } else {
                    255
                },
            ]);
        }
    }
    // A standards-compliant zlib stream using uncompressed DEFLATE blocks.
    let mut zlib = vec![0x78, 0x01];
    let mut remaining = raw.as_slice();
    while !remaining.is_empty() {
        let take = remaining.len().min(u16::MAX as usize);
        let final_block = take == remaining.len();
        zlib.push(u8::from(final_block));
        let length = take as u16;
        zlib.extend_from_slice(&length.to_le_bytes());
        zlib.extend_from_slice(&(!length).to_le_bytes());
        zlib.extend_from_slice(&remaining[..take]);
        remaining = &remaining[take..];
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(rows as u32).to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    png_chunk(&mut png, b"IHDR", &header);
    png_chunk(&mut png, b"IDAT", &zlib);
    png_chunk(&mut png, b"IEND", &[]);
    Ok(png)
}

pub async fn read(args: ReadArgs, context: &ToolContext) -> Result<ToolResult, ToolError> {
    throw_if_cancelled(context)?;
    let path = resolve_tool_path(&args.path, &context.cwd);
    let bytes = fs::read(&path).map_err(|error| ToolError::new(error.to_string()))?;
    throw_if_cancelled(context)?;
    if let Some(mut mime_type) = image_mime(&bytes) {
        let processed = if mime_type == "image/bmp" {
            mime_type = "image/png";
            bmp_to_png(&bytes)?
        } else {
            bytes
        };
        let mut note = format!("Read image file [{mime_type}]");
        if !context.model_supports_images {
            note.push_str("\n[Current model does not support images. The image will be omitted from this request.]");
        }
        return Ok(ToolResult {
            content: vec![
                ContentBlock::Text { text: note },
                ContentBlock::Image {
                    data: BASE64_STANDARD.encode(processed),
                    mime_type: mime_type.into(),
                },
            ],
            details: None,
        });
    }

    let text = String::from_utf8_lossy(&bytes).into_owned();
    let all_lines: Vec<&str> = text.split('\n').collect();
    let total_file_lines = all_lines.len();
    let offset = usize_number(args.offset, 1, 1, "offset")?;
    let start = offset - 1;
    if start >= all_lines.len() {
        return Err(ToolError::new(format!(
            "Offset {offset} is beyond end of file ({} lines total)",
            all_lines.len()
        )));
    }
    let (selected, user_limited_lines) = if let Some(limit) = args.limit {
        let limit = usize_number(Some(limit), 0, 0, "limit")?;
        let end = start.saturating_add(limit).min(all_lines.len());
        (all_lines[start..end].join("\n"), Some(end - start))
    } else {
        (all_lines[start..].join("\n"), None)
    };
    let truncation = truncate_head(&selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut details = None;
    let output = if truncation.first_line_exceeds_limit {
        details = Some(json!({ "truncation": truncation }));
        format!(
            "[Line {offset} is {}, exceeds {} limit. Use bash: sed -n '{offset}p' {} | head -c {DEFAULT_MAX_BYTES}]",
            format_size(all_lines[start].len()),
            format_size(DEFAULT_MAX_BYTES),
            args.path
        )
    } else if truncation.truncated {
        let end = offset + truncation.output_lines.saturating_sub(1);
        let next = end + 1;
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
        if start + limited < all_lines.len() {
            let remaining = all_lines.len() - (start + limited);
            let next = start + limited + 1;
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
    let lock = mutation_lock(&mutation_key(&path));
    let _guard = lock.lock().await;
    throw_if_cancelled(context)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    throw_if_cancelled(context)?;
    fs::write(&path, args.content.as_bytes())?;
    throw_if_cancelled(context)?;
    // Pi uses JavaScript string length here, which is UTF-16 code units despite
    // the wording saying bytes.
    Ok(ToolResult::text(format!(
        "Successfully wrote {} bytes to {}",
        args.content.encode_utf16().count(),
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
    let effective_limit = usize_number(args.limit, 500, 0, "limit")?;
    let entries = fs::read_dir(&path)
        .map_err(|error| ToolError::new(format!("Cannot read directory: {error}")))?;
    let mut entries: Vec<OsString> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    entries.sort_by(|left, right| {
        left.to_string_lossy()
            .to_lowercase()
            .cmp(&right.to_string_lossy().to_lowercase())
    });
    let mut results = Vec::new();
    let mut entry_limit_reached = false;
    for entry in entries {
        throw_if_cancelled(context)?;
        if results.len() >= effective_limit {
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
    let truncation = truncate_head(&results.join("\n"), usize::MAX, DEFAULT_MAX_BYTES);
    let mut output = truncation.content.clone();
    let mut notices = Vec::new();
    let mut detail = serde_json::Map::new();
    if entry_limit_reached {
        notices.push(format!(
            "{effective_limit} entries limit reached. Use limit={} for more",
            effective_limit.saturating_mul(2)
        ));
        detail.insert("entryLimitReached".into(), json!(effective_limit));
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
        stdout.read_to_end(&mut data).await.map(|_| data)
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
    let stdout = stdout_task
        .await
        .map_err(|error| ToolError::new(error.to_string()))??;
    let stderr = stderr_task
        .await
        .map_err(|error| ToolError::new(error.to_string()))??;
    Ok(CapturedProcess {
        status,
        stdout,
        stderr,
    })
}

fn truncate_line(line: &str) -> (String, bool) {
    // Pi counts JavaScript UTF-16 code units. These tools overwhelmingly receive
    // ASCII source; this preserves Unicode scalar boundaries for safe Rust text.
    if line.chars().count() <= GREP_MAX_LINE_LENGTH {
        return (line.into(), false);
    }
    let prefix: String = line.chars().take(GREP_MAX_LINE_LENGTH).collect();
    (format!("{prefix}... [truncated]"), true)
}

pub async fn grep(args: GrepArgs, context: &ToolContext) -> Result<ToolResult, ToolError> {
    throw_if_cancelled(context)?;
    let rg =
        find_program(&["rg"]).ok_or_else(|| ToolError::new("ripgrep (rg) is not available"))?;
    let raw_search_path = args.path.as_deref().unwrap_or(".");
    let search_path = resolve_tool_path(raw_search_path, &context.cwd);
    let metadata = fs::metadata(&search_path)
        .map_err(|_| ToolError::new(format!("Path not found: {}", search_path.display())))?;
    let is_directory = metadata.is_dir();
    let context_lines = if args.context.unwrap_or(0.0) > 0.0 {
        usize_number(args.context, 0, 0, "context")?
    } else {
        0
    };
    let effective_limit = usize_number(args.limit, 100, 1, "limit")?;
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
    if let Some(glob) = &args.glob {
        command_args.extend(["--glob".into(), glob.into()]);
    }
    command_args.extend([
        "--".into(),
        args.pattern.clone().into(),
        search_path.as_os_str().into(),
    ]);
    let captured = capture_process(&rg, &command_args, &context.cwd, &context.cancellation)
        .await
        .map_err(|error| {
            if error.to_string() == "Operation aborted" {
                error
            } else {
                ToolError::new(format!("Failed to run ripgrep: {error}"))
            }
        })?;
    let code = captured.status.code().unwrap_or(0);
    if code != 0 && code != 1 {
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
    for line in String::from_utf8_lossy(&captured.stdout).lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) != Some("match") {
            continue;
        }
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
        if matches.len() >= effective_limit {
            break;
        }
    }
    if matches.is_empty() {
        return Ok(ToolResult::text("No matches found"));
    }
    let match_limit_reached = matches.len() >= effective_limit;
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
        if context_lines == 0 {
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
                fs::read_to_string(&matched.path).ok().map(|content| {
                    content
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
            let start = matched.line.saturating_sub(context_lines).max(1);
            let end = matched
                .line
                .saturating_add(context_lines)
                .min(file_lines.len());
            for current in start..=end {
                let (value, truncated) = truncate_line(
                    file_lines
                        .get(current - 1)
                        .map(String::as_str)
                        .unwrap_or(""),
                );
                lines_truncated |= truncated;
                if current == matched.line {
                    output_lines.push(format!("{shown_path}:{current}: {value}"));
                } else {
                    output_lines.push(format!("{shown_path}-{current}- {value}"));
                }
            }
        }
    }
    let truncation = truncate_head(&output_lines.join("\n"), usize::MAX, DEFAULT_MAX_BYTES);
    let mut output = truncation.content.clone();
    let mut notices = Vec::new();
    let mut detail = serde_json::Map::new();
    if match_limit_reached {
        notices.push(format!(
            "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
            effective_limit.saturating_mul(2)
        ));
        detail.insert("matchLimitReached".into(), json!(effective_limit));
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
    let fd =
        find_program(&["fd", "fdfind"]).ok_or_else(|| ToolError::new("fd is not available"))?;
    let search_path = resolve_tool_path(args.path.as_deref().unwrap_or("."), &context.cwd);
    if !search_path.exists() {
        return Err(ToolError::new(format!(
            "Path not found: {}",
            search_path.display()
        )));
    }
    let effective_limit = usize_number(args.limit, 1_000, 0, "limit")?;
    let mut command_args: Vec<OsString> = ["--glob", "--color=never", "--hidden"]
        .into_iter()
        .map(Into::into)
        .collect();
    // fd 10 added --no-require-git. Older Debian fd versions already apply
    // ignore files without requiring a repository and reject this switch.
    if !inside_git_repository(&search_path) && program_help_contains(&fd, "--no-require-git") {
        command_args.push("--no-require-git".into());
    }
    command_args.extend(["--max-results".into(), effective_limit.to_string().into()]);
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
    let captured = capture_process(&fd, &command_args, &context.cwd, &context.cancellation)
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
    let result_limit_reached = lines.len() >= effective_limit;
    let truncation = truncate_head(&lines.join("\n"), usize::MAX, DEFAULT_MAX_BYTES);
    let mut output = truncation.content.clone();
    let mut notices = Vec::new();
    let mut detail = serde_json::Map::new();
    if result_limit_reached {
        notices.push(format!(
            "{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
            effective_limit.saturating_mul(2)
        ));
        detail.insert("resultLimitReached".into(), json!(effective_limit));
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

fn parse_edit_args(mut arguments: Value) -> Result<EditArgs, ToolError> {
    let Some(object) = arguments.as_object_mut() else {
        return parse_args(arguments);
    };
    if let Some(edits) = object.get_mut("edits") {
        if let Some(encoded) = edits.as_str() {
            if let Ok(parsed) = serde_json::from_str::<Value>(encoded) {
                if parsed.is_array() {
                    *edits = parsed;
                } else if parsed.is_object() {
                    *edits = Value::Array(vec![parsed]);
                }
            }
        } else if edits.is_object() {
            *edits = Value::Array(vec![edits.take()]);
        }
    }
    if object.get("oldText").is_some_and(Value::is_string)
        && object.get("newText").is_some_and(Value::is_string)
    {
        let old_text = object.remove("oldText").unwrap();
        let new_text = object.remove("newText").unwrap();
        let edit = json!({ "oldText": old_text, "newText": new_text });
        match object.entry("edits") {
            serde_json::map::Entry::Occupied(mut entry) if entry.get().is_array() => {
                entry.get_mut().as_array_mut().unwrap().push(edit);
            }
            serde_json::map::Entry::Occupied(_) => {}
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(Value::Array(vec![edit]));
            }
        }
    }
    parse_args(arguments)
}

fn normalize_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn compose_acute(base: char) -> Option<char> {
    Some(match base {
        'a' => 'á',
        'e' => 'é',
        'i' => 'í',
        'o' => 'ó',
        'u' => 'ú',
        'y' => 'ý',
        'A' => 'Á',
        'E' => 'É',
        'I' => 'Í',
        'O' => 'Ó',
        'U' => 'Ú',
        'Y' => 'Ý',
        _ => return None,
    })
}

/// The compatibility forms seen in Pi's fixtures plus its explicit punctuation
/// folding. This avoids introducing a large Unicode dependency just for edit.
fn compatibility_normalize(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\u{0301}' {
            if let Some(previous) = output.pop() {
                if let Some(composed) = compose_acute(previous) {
                    output.push(composed);
                } else {
                    output.push(previous);
                    output.push(character);
                }
            } else {
                output.push(character);
            }
            continue;
        }
        let character = match character {
            '\u{ff01}'..='\u{ff5e}' => char::from_u32(character as u32 - 0xfee0).unwrap(),
            '\u{3000}' => ' ',
            '\u{fb00}' => {
                output.push('f');
                'f'
            }
            '\u{fb01}' => {
                output.push('f');
                'i'
            }
            '\u{fb02}' => {
                output.push('f');
                'l'
            }
            '\u{fb03}' => {
                output.extend(['f', 'f']);
                'i'
            }
            '\u{fb04}' => {
                output.extend(['f', 'f']);
                'l'
            }
            other => other,
        };
        output.push(character);
    }
    output
}

pub fn normalize_for_fuzzy_match(text: &str) -> String {
    compatibility_normalize(text)
        .split('\n')
        .map(str::trim_end)
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

fn patch_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    if content.is_empty() {
        lines.clear();
    }
    lines
}

fn unified_patch(path: &str, old: &str, new: &str) -> String {
    let old_lines = patch_lines(old);
    let new_lines = patch_lines(new);
    let old_start = usize::from(!old_lines.is_empty());
    let new_start = usize::from(!new_lines.is_empty());
    let mut output = format!(
        "--- {path}\n+++ {path}\n@@ -{old_start},{} +{new_start},{} @@\n",
        old_lines.len(),
        new_lines.len()
    );
    for line in old_lines {
        output.push('-');
        output.push_str(line);
        output.push('\n');
    }
    if !old.is_empty() && !old.ends_with('\n') {
        output.push_str("\\ No newline at end of file\n");
    }
    for line in new_lines {
        output.push('+');
        output.push_str(line);
        output.push('\n');
    }
    if !new.is_empty() && !new.ends_with('\n') {
        output.push_str("\\ No newline at end of file\n");
    }
    output
}

fn display_diff(old: &str, new: &str) -> (String, Option<usize>) {
    let old_lines = patch_lines(old);
    let new_lines = patch_lines(new);
    let mut first = 0;
    while first < old_lines.len().min(new_lines.len()) && old_lines[first] == new_lines[first] {
        first += 1;
    }
    let first_changed = Some(first + 1);
    let mut old_tail = old_lines.len();
    let mut new_tail = new_lines.len();
    while old_tail > first && new_tail > first && old_lines[old_tail - 1] == new_lines[new_tail - 1]
    {
        old_tail -= 1;
        new_tail -= 1;
    }
    let context_start = first.saturating_sub(4);
    let old_context_end = (old_tail + 4).min(old_lines.len());
    let new_context_end = (new_tail + 4).min(new_lines.len());
    let width = old_lines.len().max(new_lines.len()).to_string().len();
    let mut output = Vec::new();
    if context_start > 0 {
        output.push(format!(" {} ...", " ".repeat(width)));
    }
    for (index, line) in old_lines[context_start..first].iter().enumerate() {
        output.push(format!(" {:>width$} {line}", context_start + index + 1));
    }
    for (index, line) in old_lines[first..old_tail].iter().enumerate() {
        output.push(format!("-{:>width$} {line}", first + index + 1));
    }
    for (index, line) in new_lines[first..new_tail].iter().enumerate() {
        output.push(format!("+{:>width$} {line}", first + index + 1));
    }
    let common_after = (old_lines.len() - old_tail)
        .min(new_lines.len() - new_tail)
        .min(4);
    for index in 0..common_after {
        output.push(format!(
            " {:>width$} {}",
            old_tail + index + 1,
            old_lines[old_tail + index]
        ));
    }
    if old_context_end < old_lines.len() || new_context_end < new_lines.len() {
        output.push(format!(" {} ...", " ".repeat(width)));
    }
    (output.join("\n"), first_changed)
}

pub async fn edit(args: EditArgs, context: &ToolContext) -> Result<ToolResult, ToolError> {
    if args.edits.is_empty() {
        return Err(ToolError::new(
            "Edit tool input is invalid. edits must contain at least one replacement.",
        ));
    }
    let path = resolve_tool_path(&args.path, &context.cwd);
    let lock = mutation_lock(&mutation_key(&path));
    let _guard = lock.lock().await;
    throw_if_cancelled(context)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| {
            let code = error
                .raw_os_error()
                .map(errno_name)
                .unwrap_or_else(|| error.to_string());
            ToolError::new(format!(
                "Could not edit file: {}. Error code: {code}.",
                args.path
            ))
        })?;
    let mut raw_bytes = Vec::new();
    file.read_to_end(&mut raw_bytes)?;
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
    fs::write(&path, format!("{bom}{restored}"))?;
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
    callback(ToolResult {
        content: vec![ContentBlock::Text {
            text: snapshot.content,
        }],
        details: Some(json!({
            "truncation": snapshot.truncation.truncated.then_some(snapshot.truncation.clone()),
            "fullOutputPath": snapshot.full_output_path,
        })),
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
    throw_if_cancelled(context)?;
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
    if let Some(callback) = on_update {
        callback(ToolResult {
            content: vec![],
            details: None,
        });
    }
    let mut accumulator = OutputAccumulator::new();
    let deadline = timeout_seconds.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
    let mut stop_reason: Option<&'static str> = None;
    let mut last_update = Instant::now()
        .checked_sub(Duration::from_millis(100))
        .unwrap_or_else(Instant::now);
    let mut wait = Box::pin(child.wait());
    let status = loop {
        tokio::select! {
            result = &mut wait => break result?,
            Some(chunk) = receiver.recv() => {
                accumulator.append(&chunk)?;
                if on_update.is_some() && last_update.elapsed() >= Duration::from_millis(100) {
                    emit_bash_update(&mut accumulator, on_update)?;
                    last_update = Instant::now();
                }
            }
            _ = context.cancellation.cancelled(), if stop_reason.is_none() => {
                stop_reason = Some("aborted");
                kill_process_group(pid);
            }
            _ = wait_for_deadline(deadline), if stop_reason.is_none() && deadline.is_some() => {
                stop_reason = Some("timeout");
                kill_process_group(pid);
            }
        }
    };
    drop(wait);
    // Detached descendants can retain inherited descriptors forever. Pi waits
    // only while data continues to arrive, then stops after 100 ms of silence.
    while let Ok(Some(chunk)) = timeout(Duration::from_millis(100), receiver.recv()).await {
        accumulator.append(&chunk)?;
    }
    stdout_task.abort();
    stderr_task.abort();
    accumulator.finish()?;
    if on_update.is_some() {
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
            let seconds = timeout_seconds.unwrap();
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
    if let Some(code) = status.code().filter(|code| *code != 0) {
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

        let png = b"\x89PNG\r\n\x1a\nrest";
        fs::write(directory.path().join("image.bin"), png).unwrap();
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
    async fn write_creates_parents_overwrites_and_uses_js_string_length() {
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
        assert_eq!(
            text(&result),
            "Successfully wrote 3 bytes to nested/file.txt"
        );
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
