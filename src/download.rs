//! Native SimpleHF download engine integrated into BashKitten.
//! Source: a7dccac659e4ee71d0652807f198eda20d0e7cdc; see THIRD_PARTY_NOTICES.md.
//!
//! The architecture and adaptive chunk strategy are derived from
//! rust-hf-downloader by Johannes Bertens, used under the MIT License. See
//! THIRD_PARTY_NOTICES.md and licenses/rust-hf-downloader-MIT.txt.

use futures_util::StreamExt;
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Semaphore, watch};

const MIN_CHUNK: u64 = 8 * 1024 * 1024;
const MAX_CHUNK: u64 = 128 * 1024 * 1024;
const TARGET_CHUNKS: u64 = 24;
const RETRIES: usize = 5;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub repo_id: String,
    #[serde(default = "default_revision")]
    pub revision: String,
    pub destination: PathBuf,
    pub files: Vec<ManifestFile>,
    #[serde(default = "default_connections")]
    pub connections: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Job {
        status: &'static str,
        error: Option<String>,
    },
    File {
        index: usize,
        status: &'static str,
        downloaded: u64,
        total: u64,
        error: Option<String>,
    },
}

struct FileState {
    downloaded: AtomicU64,
    total: u64,
    last_emit: Mutex<Instant>,
    emit: Arc<dyn Fn(Event) + Send + Sync>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlState {
    Running,
    Paused,
    Cancelled,
}

#[derive(Clone)]
pub struct Control {
    state: watch::Sender<ControlState>,
    emit: Arc<dyn Fn(Event) + Send + Sync>,
}

impl Control {
    pub fn new(emit: Arc<dyn Fn(Event) + Send + Sync>) -> Self {
        let (state, _) = watch::channel(ControlState::Running);
        Self { state, emit }
    }

    pub fn set(&self, state: ControlState) {
        self.state.send_replace(state);
    }

    async fn checkpoint(&self) -> Result<(), String> {
        let mut state = self.state.subscribe();
        while *state.borrow_and_update() == ControlState::Paused {
            if state.changed().await.is_err() {
                break;
            }
        }
        if *state.borrow() == ControlState::Cancelled {
            return Err("Download cancelled".into());
        }
        Ok(())
    }
    async fn cancelled(&self) {
        let mut state = self.state.subscribe();
        loop {
            if *state.borrow_and_update() == ControlState::Cancelled {
                return;
            }
            if state.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
struct ByteRange {
    start: u64,
    end: u64,
}

fn default_connections() -> usize {
    8
}

fn default_revision() -> String {
    "main".into()
}
fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

pub fn validate_repo_id(repo_id: &str) -> Result<(), String> {
    let parts: Vec<_> = repo_id.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || *part == "."
                || *part == ".."
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        return Err("repository ID must be organization/model".into());
    }
    Ok(())
}

pub fn safe_relative(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\0')
        || path.is_absolute()
        || value.contains('\\')
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!("unsafe repository path: {value}"));
    }
    Ok(path.to_path_buf())
}

fn chunk_ranges(size: u64) -> Vec<ByteRange> {
    if size == 0 {
        return vec![];
    }
    let chunk = (size / TARGET_CHUNKS).clamp(MIN_CHUNK, MAX_CHUNK);
    (0..size)
        .step_by(chunk as usize)
        .map(|start| ByteRange {
            start,
            end: (start + chunk - 1).min(size - 1),
        })
        .collect()
}

async fn probe(client: &Client, url: &str, hinted: Option<u64>) -> Result<(u64, bool), String> {
    let response = client
        .get(url)
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|error| format!("size request failed: {}", error.without_url()))?;
    if matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return Err("access denied; check the token and gated-model access".into());
    }
    if response.status() == StatusCode::RANGE_NOT_SATISFIABLE
        && response
            .headers()
            .get(header::CONTENT_RANGE)
            .is_some_and(|v| v == "bytes */0")
    {
        return Ok((0, false));
    }
    let response = response
        .error_for_status()
        .map_err(|error| error.without_url().to_string())?;
    let ranged = response.status() == StatusCode::PARTIAL_CONTENT;
    let total = response
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit('/').next())
        .and_then(|value| value.parse().ok())
        .or(hinted)
        .or_else(|| response.content_length())
        .ok_or_else(|| "server did not report a file size".to_string())?;
    Ok((total, ranged))
}

fn read_completed(partial: &Path, path: &Path, total: u64) -> BTreeSet<ByteRange> {
    let partial_matches = partial
        .metadata()
        .map(|metadata| metadata.len() == total)
        .unwrap_or(false);
    let completed: Option<BTreeSet<ByteRange>> = std::fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok());
    let planned: BTreeSet<_> = chunk_ranges(total).into_iter().collect();
    if partial_matches
        && completed
            .as_ref()
            .map(|ranges| ranges.iter().all(|range| planned.contains(range)))
            .unwrap_or(false)
    {
        return completed.unwrap();
    }
    let _ = std::fs::remove_file(path);
    BTreeSet::new()
}

fn write_completed(path: &Path, ranges: &BTreeSet<ByteRange>) -> Result<(), String> {
    let temp = path.with_extension("ranges.tmp");
    crate::config::atomic_private_bytes(&temp, &serde_json::to_vec(ranges).unwrap())
        .map_err(|error| error.to_string())?;
    std::fs::rename(temp, path).map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments)] // Preserve the ported range-worker interface.
async fn fetch_range(
    client: &Client,
    url: &str,
    partial: &Path,
    range: ByteRange,
    index: usize,
    state: &FileState,
    control: &Control,
) -> Result<u64, String> {
    let expected = range.end - range.start + 1;
    let mut last_error = String::new();
    for attempt in 0..=RETRIES {
        let mut received = 0;
        let result = async {
            control.checkpoint().await?;
            let response = client
                .get(url)
                .header(
                    header::RANGE,
                    format!("bytes={}-{}", range.start, range.end),
                )
                .send()
                .await
                .map_err(|error| error.without_url().to_string())?;
            if response.status() != StatusCode::PARTIAL_CONTENT {
                return Err(format!(
                    "server ignored byte range (HTTP {})",
                    response.status()
                ));
            }
            let actual_range = response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if actual_range != format!("bytes {}-{}/{}", range.start, range.end, state.total) {
                return Err("Server returned a different byte range".into());
            }
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(partial)
                .await
                .map_err(|error| error.to_string())?;
            file.seek(std::io::SeekFrom::Start(range.start))
                .await
                .map_err(|error| error.to_string())?;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                control.checkpoint().await?;
                let chunk = chunk.map_err(|error| error.to_string())?;
                if received + chunk.len() as u64 > expected {
                    return Err("range response exceeded requested size".into());
                }
                file.write_all(&chunk)
                    .await
                    .map_err(|error| error.to_string())?;
                received += chunk.len() as u64;
                let downloaded = state
                    .downloaded
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed)
                    + chunk.len() as u64;
                maybe_emit(index, state, downloaded, "downloading");
            }
            file.sync_data().await.map_err(|error| error.to_string())?;
            if received != expected {
                return Err(format!(
                    "short range: expected {expected}, received {received}"
                ));
            }
            Ok(received)
        }
        .await;
        match result {
            Ok(value) => return Ok(value),
            Err(error) => {
                if received > 0 {
                    state.downloaded.fetch_sub(received, Ordering::Relaxed);
                }
                last_error = error;
            }
        }
        if attempt < RETRIES {
            tokio::time::sleep(Duration::from_secs(1 << attempt.min(4))).await;
        }
    }
    Err(last_error)
}

async fn fetch_whole(
    client: &Client,
    url: &str,
    partial: &Path,
    index: usize,
    state: &FileState,
    control: &Control,
) -> Result<(), String> {
    control.checkpoint().await?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.without_url().to_string())?;
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(partial)
        .await
        .map_err(|error| error.to_string())?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        control.checkpoint().await?;
        let chunk = chunk.map_err(|error| error.to_string())?;
        output
            .write_all(&chunk)
            .await
            .map_err(|error| error.to_string())?;
        let downloaded = state
            .downloaded
            .fetch_add(chunk.len() as u64, Ordering::Relaxed)
            + chunk.len() as u64;
        maybe_emit(index, state, downloaded, "downloading");
    }
    output.sync_data().await.map_err(|error| error.to_string())
}

fn maybe_emit(index: usize, state: &FileState, downloaded: u64, status: &'static str) {
    let mut last = state.last_emit.lock().unwrap();
    if last.elapsed() >= Duration::from_millis(200) || downloaded == state.total {
        *last = Instant::now();
        (state.emit)(Event::File {
            index,
            status,
            downloaded,
            total: state.total,
            error: None,
        });
    }
}

async fn download_file(
    client: Client,
    semaphore: Arc<Semaphore>,
    manifest: Arc<Manifest>,
    index: usize,
    control: Control,
    base_url: String,
) -> Result<(), String> {
    let item = &manifest.files[index];
    let relative = safe_relative(&item.path)?;
    let final_path = manifest.destination.join(&manifest.repo_id).join(relative);
    let partial = final_path.with_file_name(format!(
        "{}.part",
        final_path.file_name().unwrap().to_string_lossy()
    ));
    let ranges_path = partial.with_extension("part.ranges");
    prepare_paths(&manifest.destination, &final_path, &partial, &ranges_path)?;
    let url = format!(
        "{}/{}/resolve/{}/{}?download=true",
        base_url,
        manifest.repo_id,
        encode(&manifest.revision),
        item.path
            .split('/')
            .map(encode)
            .collect::<Vec<_>>()
            .join("/")
    );
    let (total, ranged) = probe(&client, &url, item.size).await?;
    let identity_path = final_path.with_file_name(format!(
        "{}.download.json",
        final_path.file_name().unwrap().to_string_lossy()
    ));
    if let Ok(meta) = std::fs::symlink_metadata(&identity_path)
        && (!meta.is_file() || meta.file_type().is_symlink())
    {
        return Err("Download identity is not a regular file".into());
    }
    let identity = serde_json::json!({"repository":manifest.repo_id,"revision":manifest.revision,"path":item.path,"size":total});
    let previous = std::fs::read(&identity_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    let same_revision = previous
        .as_ref()
        .is_some_and(|value| value["identity"] == identity);
    let already_complete = previous
        .as_ref()
        .is_some_and(|value| value["complete"] == true);
    if !same_revision {
        let _ = std::fs::remove_file(&ranges_path);
    }

    if same_revision
        && already_complete
        && final_path
            .metadata()
            .map(|meta| meta.len() == total)
            .unwrap_or(false)
    {
        (control.emit)(Event::File {
            index,
            status: "complete",
            downloaded: total,
            total,
            error: None,
        });
        return Ok(());
    }
    crate::config::atomic_private_json(
        &identity_path,
        &serde_json::json!({"identity":identity,"complete":false}),
    )
    .map_err(|e| e.to_string())?;
    (control.emit)(Event::File {
        index,
        status: "downloading",
        downloaded: 0,
        total,
        error: None,
    });
    let state = Arc::new(FileState {
        downloaded: AtomicU64::new(0),
        total,
        last_emit: Mutex::new(Instant::now()),
        emit: control.emit.clone(),
    });
    if ranged {
        let completed = Arc::new(Mutex::new(read_completed(&partial, &ranges_path, total)));
        let already = completed
            .lock()
            .unwrap()
            .iter()
            .map(|range| range.end - range.start + 1)
            .sum();
        state.downloaded.store(already, Ordering::Relaxed);
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&partial)
            .await
            .map_err(|error| error.to_string())?;
        file.set_len(total)
            .await
            .map_err(|error| error.to_string())?;
        drop(file);
        let pending: Vec<_> = chunk_ranges(total)
            .into_iter()
            .filter(|range| !completed.lock().unwrap().contains(range))
            .collect();
        let mut tasks = tokio::task::JoinSet::new();
        for range in pending {
            let (client, permit_pool, partial, ranges_path, completed, state, url, control) = (
                client.clone(),
                semaphore.clone(),
                partial.clone(),
                ranges_path.clone(),
                completed.clone(),
                state.clone(),
                url.clone(),
                control.clone(),
            );
            tasks.spawn(async move {
                let _permit = permit_pool
                    .acquire()
                    .await
                    .map_err(|error| error.to_string())?;
                fetch_range(&client, &url, &partial, range, index, &state, &control).await?;
                {
                    let mut done = completed.lock().unwrap();
                    done.insert(range);
                    write_completed(&ranges_path, &done)?;
                }
                Ok::<(), String>(())
            });
        }
        let mut range_error = None;
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    range_error.get_or_insert(error);
                }
                Err(error) => {
                    range_error.get_or_insert(error.to_string());
                }
            };
        }
        if let Some(error) = range_error {
            return Err(error);
        }
    } else {
        let _permit = semaphore
            .acquire()
            .await
            .map_err(|error| error.to_string())?;
        let mut last_error = None;
        for attempt in 0..=RETRIES {
            state.downloaded.store(0, Ordering::Relaxed);
            match fetch_whole(&client, &url, &partial, index, &state, &control).await {
                Ok(()) => {
                    last_error = None;
                    break;
                }
                Err(error) => last_error = Some(error),
            }
            if attempt < RETRIES {
                tokio::time::sleep(Duration::from_secs(1 << attempt.min(4))).await;
            }
        }
        if let Some(error) = last_error {
            return Err(error);
        }
    }
    let actual = tokio::fs::metadata(&partial)
        .await
        .map_err(|error| error.to_string())?
        .len();
    if actual != total {
        return Err(format!(
            "size mismatch: expected {total}, received {actual}"
        ));
    }
    tokio::fs::rename(&partial, &final_path)
        .await
        .map_err(|error| error.to_string())?;
    let _ = tokio::fs::remove_file(ranges_path).await;
    crate::config::atomic_private_json(
        &identity_path,
        &serde_json::json!({"identity":identity,"complete":true}),
    )
    .map_err(|e| e.to_string())?;
    (control.emit)(Event::File {
        index,
        status: "complete",
        downloaded: total,
        total,
        error: None,
    });
    Ok(())
}

pub fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    validate_repo_id(&manifest.repo_id)?;
    if manifest.revision.is_empty() || manifest.revision.contains(['\0', '\n', '\r']) {
        return Err("Invalid repository revision".into());
    }
    if !manifest.destination.is_absolute() {
        return Err("Download folder must be absolute".into());
    }
    if manifest.files.is_empty() {
        return Err("Select at least one repository file".into());
    }
    let mut seen = BTreeSet::new();
    for file in &manifest.files {
        let path = safe_relative(&file.path)?;
        if !seen.insert(path) {
            return Err("Duplicate repository file".into());
        }
    }
    Ok(())
}

fn prepare_paths(
    root: &Path,
    final_path: &Path,
    partial: &Path,
    ranges: &Path,
) -> Result<(), String> {
    crate::paths::ensure_private_dir(root).map_err(|e| e.to_string())?;
    let relative = final_path.strip_prefix(root).map_err(|e| e.to_string())?;
    let mut at = root.to_path_buf();
    for part in relative.parent().unwrap_or(Path::new("")).components() {
        at.push(part);
        if let Ok(meta) = std::fs::symlink_metadata(&at)
            && (meta.file_type().is_symlink() || !meta.is_dir())
        {
            return Err("Download folder contains an unsafe link or file".into());
        }
        crate::paths::ensure_private_dir(&at).map_err(|e| e.to_string())?;
    }
    for path in [
        final_path,
        partial,
        ranges,
        &ranges.with_extension("ranges.tmp"),
    ] {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err("Download target is not a regular file".into());
            }
            crate::paths::set_private_file(path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub async fn run(
    manifest: Manifest,
    token: Option<String>,
    control: Control,
) -> Result<(), String> {
    run_at(manifest, token, control, "https://huggingface.co".into()).await
}

async fn run_at(
    manifest: Manifest,
    token: Option<String>,
    control: Control,
    base_url: String,
) -> Result<(), String> {
    validate_manifest(&manifest)?;
    let mut headers = header::HeaderMap::new();
    if let Some(token) = token.as_ref().filter(|s| !s.trim().is_empty()) {
        let mut value = header::HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .map_err(|_| "Invalid Hugging Face token")?;
        value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, value);
    }
    // Allowed network category: an explicit user-selected HF file download.
    let client = Client::builder()
        .default_headers(headers)
        .no_proxy()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.without_url().to_string())?;
    let manifest = Arc::new(manifest);
    let semaphore = Arc::new(Semaphore::new(manifest.connections.clamp(1, 32)));
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..manifest.files.len() {
        let (client, semaphore, manifest, control, base_url) = (
            client.clone(),
            semaphore.clone(),
            manifest.clone(),
            control.clone(),
            base_url.clone(),
        );
        tasks.spawn(async move {
            (
                index,
                download_file(client, semaphore, manifest, index, control, base_url).await,
            )
        });
    }
    let mut failure = None;
    loop {
        let next = tokio::select! { biased; _=control.cancelled()=>{ tasks.abort_all(); while tasks.join_next().await.is_some() {} return Err("Download cancelled".into()); }, next=tasks.join_next()=>next };
        let Some(next) = next else {
            break;
        };
        match next {
            Ok((index, Err(error))) => {
                let error = token
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .map_or(error.clone(), |token| error.replace(token, "[redacted]"));
                (control.emit)(Event::File {
                    index,
                    status: "failed",
                    downloaded: 0,
                    total: 0,
                    error: Some(error.clone()),
                });
                failure.get_or_insert(error);
            }
            Err(error) => {
                failure.get_or_insert(error.to_string());
            }
            _ => {}
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn authenticated_ranges_resume_original_names_and_nested_paths() {
        use axum::{Router, body::Body, http::Response, routing::get};
        let root = tempfile::tempdir().unwrap();
        let size = MIN_CHUNK + 37;
        let bytes = Arc::new((0..size).map(|n| (n % 251) as u8).collect::<Vec<_>>());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let served = bytes.clone();
        let app = Router::new().fallback(get(move |request: axum::extract::Request| {
            let recorded = recorded.clone();
            let served = served.clone();
            async move {
                assert_eq!(
                    request.headers()[header::AUTHORIZATION],
                    "Bearer fixture-token"
                );
                assert_eq!(
                    request.uri().path(),
                    "/org/model/resolve/revision/weights/model%20Q4.gguf"
                );
                let range = request.headers()[header::RANGE]
                    .to_str()
                    .unwrap()
                    .to_owned();
                recorded.lock().unwrap().push(range.clone());
                let (a, b) = range
                    .strip_prefix("bytes=")
                    .unwrap()
                    .split_once('-')
                    .unwrap();
                let a: usize = a.parse().unwrap();
                let b: usize = b.parse().unwrap();
                Response::builder()
                    .status(206)
                    .header(
                        header::CONTENT_RANGE,
                        format!("bytes {a}-{b}/{}", served.len()),
                    )
                    .body(Body::from(served[a..=b].to_vec()))
                    .unwrap()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let manifest = Manifest {
            repo_id: "org/model".into(),
            revision: "revision".into(),
            destination: root.path().to_path_buf(),
            files: vec![ManifestFile {
                path: "weights/model Q4.gguf".into(),
                size: Some(size),
            }],
            connections: 2,
        };
        let target = root.path().join("org/model/weights/model Q4.gguf");
        let partial = target.with_file_name("model Q4.gguf.part");
        let journal = partial.with_extension("part.ranges");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&partial, &*bytes).unwrap();
        write_completed(&journal, &BTreeSet::from([chunk_ranges(size)[0]])).unwrap();
        crate::config::atomic_private_json(&target.with_file_name("model Q4.gguf.download.json"),&serde_json::json!({"identity":{"repository":"org/model","revision":"revision","path":"weights/model Q4.gguf","size":size},"complete":false})).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let output = events.clone();
        run_at(
            manifest,
            Some("fixture-token".into()),
            Control::new(Arc::new(move |e| output.lock().unwrap().push(e))),
            base,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), *bytes);
        assert!(
            !std::fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!partial.exists());
        assert!(!journal.exists());
        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                "bytes=0-0".to_string(),
                format!("bytes={}-{}", MIN_CHUNK, size - 1)
            ]
        );
        assert!(matches!(
            events.lock().unwrap().last(),
            Some(Event::File {
                status: "complete",
                ..
            })
        ));
        server.abort();
    }

    #[tokio::test]
    async fn cancelling_a_paused_download_stops_all_range_tasks() {
        let root = tempfile::tempdir().unwrap();
        let control = Control::new(Arc::new(|_| {}));
        control.set(ControlState::Cancelled);
        let manifest = Manifest {
            repo_id: "org/model".into(),
            revision: "main".into(),
            destination: root.path().into(),
            files: vec![ManifestFile {
                path: "model.gguf".into(),
                size: Some(10),
            }],
            connections: 2,
        };
        assert_eq!(
            run_at(manifest, None, control, "http://127.0.0.1:1".into())
                .await
                .unwrap_err(),
            "Download cancelled"
        );
        assert!(!root.path().join("org/model/model.gguf").exists());
    }

    #[test]
    fn rejects_symlink_download_targets() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("org")).unwrap();
        let target = root.path().join("org/model/file.gguf");
        assert!(
            prepare_paths(
                root.path(),
                &target,
                &target.with_extension("part"),
                &target.with_extension("ranges")
            )
            .is_err()
        );
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn rejects_paths_that_escape_destination() {
        for path in ["../token", "/etc/passwd", "folder/../../secret", "a\\b"] {
            assert!(safe_relative(path).is_err(), "accepted {path}");
        }
        assert_eq!(
            safe_relative("weights/model.safetensors").unwrap(),
            PathBuf::from("weights/model.safetensors")
        );
    }

    #[test]
    fn adaptive_ranges_cover_file_exactly() {
        let size = 257 * 1024 * 1024 + 17;
        let ranges = chunk_ranges(size);
        assert_eq!(ranges.first().unwrap().start, 0);
        assert_eq!(ranges.last().unwrap().end, size - 1);
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].end + 1, pair[1].start);
        }
        assert_eq!(
            ranges
                .iter()
                .map(|range| range.end - range.start + 1)
                .sum::<u64>(),
            size
        );
    }

    #[test]
    fn resume_requires_matching_partial_and_current_ranges() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("simplehf-resume-{}-{unique}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let partial = directory.join("model.part");
        let ranges_path = directory.join("model.part.ranges");
        let total = 1024;
        let planned: BTreeSet<_> = chunk_ranges(total).into_iter().collect();

        std::fs::File::create(&partial)
            .unwrap()
            .set_len(total)
            .unwrap();
        write_completed(&ranges_path, &planned).unwrap();
        assert_eq!(read_completed(&partial, &ranges_path, total), planned);

        std::fs::remove_file(&partial).unwrap();
        assert!(read_completed(&partial, &ranges_path, total).is_empty());
        assert!(!ranges_path.exists());

        std::fs::File::create(&partial)
            .unwrap()
            .set_len(total)
            .unwrap();
        let stale = BTreeSet::from([ByteRange { start: 1, end: 2 }]);
        write_completed(&ranges_path, &stale).unwrap();
        assert!(read_completed(&partial, &ranges_path, total).is_empty());
        assert!(!ranges_path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn requires_namespaced_repository_ids() {
        assert!(validate_repo_id("org/model").is_ok());
        assert!(validate_repo_id("model").is_err());
        assert!(validate_repo_id("org/../model").is_err());
    }

    #[tokio::test]
    async fn pause_blocks_work_until_resume() {
        let control = Control::new(Arc::new(|_| {}));
        control.set(ControlState::Paused);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), control.checkpoint())
                .await
                .is_err()
        );
        control.set(ControlState::Running);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), control.checkpoint())
                .await
                .is_ok()
        );
    }
}
