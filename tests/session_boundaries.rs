//! Filesystem boundaries for BashKitten's documented numbered-JSONL layout.
//! No model, credentials, systemd services, browser or network is used here.
use bashkitten::{config::AppConfig, paths::AppPaths, session, worker};
use fs2::FileExt;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;

fn fixture(root: &Path) -> (AppPaths, String) {
    let paths = AppPaths {
        config: root.join("config"),
        data: root.join("data"),
        runtime: root.join("runtime"),
    };
    paths.ensure().unwrap();
    let id = session::create(
        &paths,
        &session::NewSession {
            cwd: root.to_owned(),
            model: "removed/model-a".into(),
            thinking: "off".into(),
            model_parameters: json!({"contextWindow":8192,"maxTokens":1024}),
            prompt: "Filesystem boundary fixture".into(),
            attachments: vec![],
            parent: None,
        },
    )
    .unwrap();
    (paths, id)
}

fn message(id: &str, parent: Option<&str>, text: &str) -> Value {
    json!({"type":"message","id":id,"parentId":parent,
        "timestamp":"2026-09-07T00:00:00.000Z",
        "message":{"role":"user","content":text,"timestamp":1}})
}
fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}
fn broaden(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn history_reader_waits_for_the_complete_locked_append() {
    let root = tempfile::tempdir().unwrap();
    let (paths, id) = fixture(root.path());
    let segment = paths.session_dir(&id).join("000001.jsonl");
    let entry = message("appended", None, &"a complete large message ".repeat(32768));
    let mut bytes = serde_json::to_vec(&entry).unwrap();
    bytes.push(b'\n');
    let split = bytes.len() / 2;
    let mut writer = OpenOptions::new().append(true).open(&segment).unwrap();
    writer.lock_exclusive().unwrap();
    writer.write_all(&bytes[..split]).unwrap();
    writer.sync_all().unwrap();
    // The file now deliberately contains an invalid partial final JSON line.
    // A reader without the shared lock would return a parse failure here.
    assert!(serde_json::from_slice::<Value>(&bytes[..split]).is_err());
    let barrier = Arc::new(Barrier::new(2));
    let started = barrier.clone();
    let reader_paths = paths.clone();
    let reader_id = id.clone();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        started.wait();
        sender
            .send(session::read_segment(&reader_paths, &reader_id, 1))
            .unwrap();
    });
    barrier.wait();
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    writer.write_all(&bytes[split..]).unwrap();
    writer.sync_all().unwrap();
    FileExt::unlock(&writer).unwrap();
    let entries = receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    reader.join().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1], entry);
}

#[test]
fn session_append_waits_for_an_existing_history_reader() {
    let root = tempfile::tempdir().unwrap();
    let (paths, id) = fixture(root.path());
    let segment = paths.session_dir(&id).join("000001.jsonl");
    let reader = fs::File::open(&segment).unwrap();
    FileExt::lock_shared(&reader).unwrap();
    let before = fs::read(&segment).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let started = barrier.clone();
    let writer_paths = paths.clone();
    let writer_id = id.clone();
    let entry = message("after-reader", None, "Atomic turn boundary");
    let write_entry = entry.clone();
    let (sender, receiver) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        started.wait();
        sender
            .send(session::append_values(
                &writer_paths,
                &writer_id,
                &[write_entry],
            ))
            .unwrap();
    });
    barrier.wait();
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(fs::read(&segment).unwrap(), before);
    FileExt::unlock(&reader).unwrap();
    receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    writer.join().unwrap();
    assert_eq!(session::read_segment(&paths, &id, 1).unwrap()[1], entry);
}

#[test]
fn reading_an_older_segment_corrects_its_file_and_session_directory_modes() {
    let root = tempfile::tempdir().unwrap();
    let (paths, id) = fixture(root.path());
    let entry = message("older", None, "Older history stays readable and private");
    session::append_values(&paths, &id, std::slice::from_ref(&entry)).unwrap();
    let dir = paths.session_dir(&id);
    let header = session::read_header(&dir).unwrap();
    assert_eq!(session::rotate_compaction(&paths, &header, &[]).unwrap(), 2);
    broaden(&dir, 0o777);
    broaden(&dir.join("000001.jsonl"), 0o666);
    // Direct older-segment reads must not depend on having opened the newest
    // header first: this is the route used by upward history pagination.
    assert_eq!(session::read_segment(&paths, &id, 1).unwrap()[1], entry);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("000001.jsonl")), 0o600);
    assert_eq!(mode(&dir.join("000002.jsonl")), 0o600);
}

#[test]
fn fork_corrects_existing_source_modes_and_copies_only_retained_private_attachments() {
    let root = tempfile::tempdir().unwrap();
    let (paths, id) = fixture(root.path());
    let input = root.path().join("retained.txt");
    fs::write(&input, b"retained attachment").unwrap();
    let retained = session::copy_attachments(&paths, &id, &[input])
        .unwrap()
        .remove(0);
    let input = root.path().join("later.txt");
    fs::write(&input, b"excluded later attachment").unwrap();
    let excluded = session::copy_attachments(&paths, &id, &[input])
        .unwrap()
        .remove(0);
    let kept = message(
        "kept-message",
        None,
        &format!("Retain {}", retained.display()),
    );
    session::append_values(
        &paths,
        &id,
        &[
            kept.clone(),
            message(
                "later-message",
                Some("kept-message"),
                &excluded.to_string_lossy(),
            ),
        ],
    )
    .unwrap();
    let source = paths.session_dir(&id);
    let history_before = fs::read(source.join("000001.jsonl")).unwrap();
    for path in [
        &source,
        &source.join("attachments"),
        retained.parent().unwrap(),
    ] {
        broaden(path, 0o777);
    }
    for path in [
        &source.join("000001.jsonl"),
        &source.join("title"),
        &retained,
    ] {
        broaden(path, 0o666);
    }
    let fork = session::fork_at(&paths, &id, "kept-message").unwrap();
    let destination = paths.session_dir(&fork);
    for path in [
        &source,
        &source.join("attachments"),
        retained.parent().unwrap(),
    ] {
        assert_eq!(mode(path), 0o700, "source {}", path.display());
    }
    for path in [
        &source.join("000001.jsonl"),
        &source.join("title"),
        &retained,
    ] {
        assert_eq!(mode(path), 0o600, "source {}", path.display());
    }
    assert_eq!(
        fs::read(source.join("000001.jsonl")).unwrap(),
        history_before
    );
    let entries = session::read_segment(&paths, &fork, 1).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["parentSession"], id);
    assert_eq!(entries[1], kept);
    let copy = destination.join(retained.strip_prefix(&source).unwrap());
    assert_eq!(fs::read(&copy).unwrap(), b"retained attachment");
    assert!(
        !destination
            .join(excluded.strip_prefix(&source).unwrap())
            .exists()
    );
    for entry in walkdir::WalkDir::new(&destination) {
        let entry = entry.unwrap();
        assert_eq!(
            mode(entry.path()),
            if entry.file_type().is_dir() {
                0o700
            } else {
                0o600
            },
            "fork {}",
            entry.path().display()
        );
    }
}

#[test]
fn saved_usage_uses_a_removed_presets_context_only_for_its_matching_header_model() {
    let root = tempfile::tempdir().unwrap();
    let (paths, id) = fixture(root.path());
    session::append_values(&paths, &id, &[message("first", None, "Four")]).unwrap();
    let config = AppConfig::default();
    let usage = worker::saved_usage(&paths, &id, &config).unwrap();
    assert_eq!(usage.context.unwrap().context_window, 8192);
    assert!(usage.text.contains("/8.2k"));
    session::append_values(&paths, &id, &[json!({"type":"model_change","id":"changed","parentId":"first","timestamp":"2026-09-07T00:00:01Z","provider":"removed","modelId":"model-b"})]).unwrap();
    let usage = worker::saved_usage(&paths, &id, &config).unwrap();
    assert!(
        usage.context.is_none(),
        "a missing different model must not inherit model-a's capacity"
    );
    assert!(!usage.text.contains("/8.2k"));
    session::append_values(&paths, &id, &[json!({"type":"model_change","id":"restored","parentId":"changed","timestamp":"2026-09-07T00:00:02Z","provider":"removed","modelId":"model-a"})]).unwrap();
    assert_eq!(
        worker::saved_usage(&paths, &id, &config)
            .unwrap()
            .context
            .unwrap()
            .context_window,
        8192
    );
}
