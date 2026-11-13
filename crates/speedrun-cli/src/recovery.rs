//! Offline recovery bundles. The daemon lock freezes the event database and
//! device keys; the import lock prevents legacy spool files moving while the
//! SQLite outbox is snapshotted.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use speedrun_core::{hash::sha256_hex, sign};

const MANIFEST: &str = "manifest.json";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    files: BTreeMap<String, String>,
}

fn permitted(path: &str) -> bool {
    matches!(
        path,
        "speedrun.sqlite"
            | "device.key"
            | "device.key.next"
            | "prompt.key"
            | "content.key"
            | "trusted-projects.json"
            | "preferences.json"
            | "spool/pending.sqlite3"
            | "spool/outbox.key"
            | "spool/pending.jsonl"
            | "spool/rejected.jsonl"
    ) || path
        .strip_prefix("spool/")
        .is_some_and(|name| name.starts_with("flushing-") && !name.contains('/'))
        || path
            .strip_prefix("disclosure/")
            .is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    Ok(())
}
fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn regular(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "{} is not a regular file",
        path.display()
    );
    Ok(())
}
fn check_dir(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "{} is not a directory",
        path.display()
    );
    Ok(())
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}
fn snapshot(src: &Path, dest: &Path) -> Result<()> {
    snapshot_with_timeout(src, dest, Duration::from_secs(30))
}
fn snapshot_with_timeout(src: &Path, dest: &Path, timeout: Duration) -> Result<()> {
    regular(src)?;
    let db = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Recovery can overlap an active outbox writer. Wait for transient SQLite
    // contention explicitly instead of relying on the driver's default timeout.
    db.busy_timeout(timeout)?;
    let dest = dest.to_str().context("snapshot destination is not UTF-8")?;
    db.execute("VACUUM INTO ?1", [dest])?;
    fs::set_permissions(dest, fs::Permissions::from_mode(0o600))?;
    Ok(())
}
fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    regular(src)?;
    write_private(dst, &fs::read(src)?)
}
fn copy_optional(src: &Path, dst: &Path) -> Result<()> {
    match fs::symlink_metadata(src) {
        Ok(_) => copy_file(src, dst),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn inventory(dir: &Path, prefix: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
    check_dir(dir)?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let rel = prefix.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            ensure!(
                rel == Path::new("spool") || rel == Path::new("disclosure"),
                "unexpected directory {}",
                rel.display()
            );
            inventory(&entry.path(), &rel, files)?;
        } else if kind.is_file() {
            if rel == Path::new(MANIFEST) {
                continue;
            }
            let name = rel.to_str().context("non-UTF-8 filename")?.to_owned();
            ensure!(
                files.insert(name, sha256_hex(&fs::read(entry.path())?)).is_none(),
                "duplicate path"
            );
        } else {
            bail!("{} is not a regular file or directory", entry.path().display());
        }
    }
    Ok(())
}
fn verify_sqlite(path: &Path) -> Result<()> {
    regular(path)?;
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let result: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(
        result == "ok",
        "SQLite integrity check failed for {}: {result}",
        path.display()
    );
    Ok(())
}
fn validate(dir: &Path) -> Result<()> {
    check_dir(dir)?;
    regular(&dir.join(MANIFEST))?;
    let manifest: Manifest = serde_json::from_slice(&fs::read(dir.join(MANIFEST))?)?;
    ensure!(manifest.version == 1, "unsupported recovery bundle version");
    let expected = manifest.files;
    for path in expected.keys() {
        ensure!(permitted(path), "unexpected bundle file {path}");
        ensure!(
            !path.is_empty()
                && Path::new(path)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "invalid manifest path"
        );
    }
    let mut actual = BTreeMap::new();
    inventory(dir, Path::new(""), &mut actual)?;
    ensure!(
        actual == expected,
        "bundle files or checksums do not match the manifest"
    );
    for name in ["speedrun.sqlite", "device.key", "prompt.key", "content.key"] {
        ensure!(expected.contains_key(name), "bundle is missing {name}");
    }
    verify_sqlite(&dir.join("speedrun.sqlite"))?;
    if expected.contains_key("spool/pending.sqlite3") {
        verify_sqlite(&dir.join("spool/pending.sqlite3"))?;
        crate::client::validate_existing_outbox(&dir.join("spool/pending.sqlite3"))?;
    }
    if expected.keys().any(|name| {
        name.strip_prefix("spool/")
            .is_some_and(crate::client::ingress::is_sidecar)
    }) {
        crate::client::ingress::validate_sidecars(&dir.join("spool"))?;
    }
    for name in ["prompt.key", "content.key"] {
        sign::parse_seed(&fs::read_to_string(dir.join(name))?).map_err(anyhow::Error::msg)?;
    }
    let db = Connection::open_with_flags(dir.join("speedrun.sqlite"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let content_key = sign::parse_seed(&fs::read_to_string(dir.join("content.key"))?)
        .map_err(anyhow::Error::msg)?
        .to_bytes();
    let prompt_key = sign::parse_seed(&fs::read_to_string(dir.join("prompt.key"))?)
        .map_err(anyhow::Error::msg)?
        .to_bytes();
    let mut runs: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    let mut query = db.prepare("SELECT run_id, event_id, body FROM events")?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (run_id, event_id, body) = row?;
        let body: serde_json::Value = serde_json::from_str(&body)?;
        ensure!(
            body["run_id"].as_str() == Some(&run_id) && body["event_id"].as_str() == Some(&event_id),
            "event index does not match body"
        );
        runs.entry(run_id).or_default().push(body);
    }
    let mut prompt_digests = BTreeMap::new();
    for (run_id, bodies) in runs {
        speedrun_core::hash::verify_stored_chain(&bodies)
            .map_err(|e| anyhow::anyhow!("invalid chain for {run_id}: {e:?}"))?;
        let events: Vec<speedrun_core::Event> = bodies
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<_, _>>()?;
        let signatures = sign::verify_event_signatures(&events);
        ensure!(
            signatures.ok(),
            "invalid signatures for {run_id}: {:?}",
            signatures.error
        );
        for event in events {
            match event.payload {
                speedrun_core::EventPayload::RunStarted {
                    disclosure_commitment: Some(commitment),
                    ..
                } => {
                    ensure!(
                        Path::new(&event.run_id).components().count() == 1
                            && Path::new(&event.run_id)
                                .components()
                                .all(|c| matches!(c, Component::Normal(_))),
                        "invalid run id"
                    );
                    let path = dir.join("disclosure").join(&event.run_id);
                    regular(&path)?;
                    let key = sign::parse_seed(&fs::read_to_string(path)?)
                        .map_err(anyhow::Error::msg)?
                        .to_bytes();
                    ensure!(
                        speedrun_core::digest::key_commitment(&key) == commitment,
                        "disclosure key does not match run {}",
                        event.run_id
                    );
                }
                speedrun_core::EventPayload::PromptSubmitted { content_digest, .. } => {
                    prompt_digests.insert(event.event_id, content_digest);
                }
                _ => {}
            }
        }
    }
    let cipher = XChaCha20Poly1305::new((&content_key).into());
    let mut query = db.prepare("SELECT event_id, ciphertext FROM prompts WHERE ciphertext IS NOT NULL")?;
    let rows = query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (event_id, ciphertext) = row?;
        ensure!(ciphertext.len() >= 24, "invalid encrypted prompt");
        let text = cipher
            .decrypt(
                XNonce::from_slice(&ciphertext[..24]),
                Payload {
                    msg: &ciphertext[24..],
                    aad: event_id.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("content key does not authenticate retained prompt {event_id}"))?;
        let text = String::from_utf8(text)?;
        let expected = prompt_digests
            .get(&event_id)
            .context("retained prompt has no signed event")?;
        ensure!(
            speedrun_core::digest::text_digest(&prompt_key, &text) == *expected,
            "prompt key does not authenticate retained prompt {event_id}"
        );
    }
    let pubkey: Option<String> = db
        .query_row("SELECT value FROM meta WHERE key='signing_pubkey'", [], |r| {
            r.get(0)
        })
        .optional()?;
    let seed = fs::read_to_string(dir.join("device.key"))?;
    let key = sign::parse_seed(&seed).map_err(anyhow::Error::msg)?;
    if let Some(pubkey) = pubkey {
        let matches_current = sign::pubkey_hex(&key) == pubkey;
        let pending = dir.join("device.key.next");
        let matches_pending = if present(&pending)? {
            sign::parse_seed(&fs::read_to_string(pending)?)
                .map_err(anyhow::Error::msg)
                .map(|k| sign::pubkey_hex(&k) == pubkey)?
        } else {
            false
        };
        ensure!(
            matches_current || matches_pending,
            "device key does not match database signing key"
        );
    } else {
        let id: String = db.query_row("SELECT value FROM meta WHERE key='device_id'", [], |r| r.get(0))?;
        ensure!(
            sign::fingerprint(&sign::pubkey_hex(&key)).map_err(anyhow::Error::msg)? == id,
            "device key does not match database identity"
        );
    }
    Ok(())
}

// Sync all staged data before publishing the directory entry. A process killed
// before publication can leave an identifiable sibling staging directory.
fn sync_tree(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            fs::File::open(entry.path())?.sync_all()?;
        }
    }
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn publish(
    destination: &Path,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
    operation: &str,
    build: impl FnOnce(&Path, &mut dyn FnMut(&str) -> Result<()>) -> Result<()>,
) -> Result<()> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stage = tempfile::Builder::new()
        .prefix(".speedrun-recovery-")
        .tempdir_in(parent)?;
    let payload = stage.path().join("data");
    build(&payload, checkpoint)?;
    sync_tree(&payload)?;
    fs::File::open(stage.path())?.sync_all()?;
    checkpoint(&format!("{operation}.ready"))?;
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        &payload,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    fs::File::open(parent)?.sync_all()?;
    checkpoint(&format!("{operation}.published"))?;
    Ok(())
}

pub fn create(source: &Path, destination: &Path) -> Result<()> {
    create_with_checkpoints(source, destination, &mut |_| Ok(()))
}

fn create_with_checkpoints(
    source: &Path,
    destination: &Path,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    publish(destination, checkpoint, "create", |dest, checkpoint| {
        create_in_place(source, dest, checkpoint)
    })
}

fn create_in_place(
    source: &Path,
    destination: &Path,
    checkpoint: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    check_dir(source)?;
    let lock_path = source.join("speedrund.lock");
    if present(&lock_path)? {
        regular(&lock_path)?;
    }
    let lock = Connection::open(&lock_path)?;
    lock.busy_timeout(Duration::ZERO)?;
    lock.execute_batch("BEGIN IMMEDIATE")
        .context("data directory is busy; stop the daemon first")?;
    let spool = source.join("spool");
    let import_lock = if present(&spool)? {
        check_dir(&spool)?;
        let path = spool.join("import.lock");
        if present(&path)? {
            regular(&path)?;
        }
        let f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        ensure!(
            crate::client::ingress::lock_until(&f, Some(Instant::now() + Duration::from_secs(2)))?,
            "legacy spool import is busy"
        );
        Some(f)
    } else {
        None
    };
    // While the import lock is held, new admissions use immutable spill files.
    // Capture their names once so the bundle does not mix different points in time.
    let sidecars = if import_lock.is_some() {
        crate::client::ingress::snapshot_files(&spool)?
    } else {
        Vec::new()
    };
    private_dir(destination)?;
    let result = (|| -> Result<()> {
        checkpoint("create.directory")?;
        snapshot(
            &source.join("speedrun.sqlite"),
            &destination.join("speedrun.sqlite"),
        )?;
        checkpoint("create.database")?;
        for name in [
            "device.key",
            "device.key.next",
            "prompt.key",
            "content.key",
            "trusted-projects.json",
            "preferences.json",
        ] {
            copy_optional(&source.join(name), &destination.join(name))?;
            checkpoint(&format!("create.file.{name}"))?;
        }
        let disclosure = source.join("disclosure");
        if present(&disclosure)? {
            check_dir(&disclosure)?;
            private_dir(&destination.join("disclosure"))?;
            for item in fs::read_dir(disclosure)? {
                let item = item?;
                copy_file(
                    &item.path(),
                    &destination.join("disclosure").join(item.file_name()),
                )?;
                checkpoint("create.disclosure.file")?;
            }
        }
        checkpoint("create.disclosure")?;
        if import_lock.is_some() {
            private_dir(&destination.join("spool"))?;
            let outbox = spool.join("pending.sqlite3");
            if present(&outbox)? {
                snapshot(&outbox, &destination.join("spool/pending.sqlite3"))?;
                checkpoint("create.outbox")?;
            }
            for item in fs::read_dir(&spool)? {
                let item = item?;
                let name = item.file_name();
                let n = name.to_str().context("non-UTF-8 spool filename")?;
                if n == "outbox.key"
                    || n == "pending.jsonl"
                    || n == "rejected.jsonl"
                    || (n.starts_with("flushing-") && !crate::client::ingress::is_sidecar(n))
                {
                    copy_file(&item.path(), &destination.join("spool").join(&name))?;
                    checkpoint(&format!("create.spool.{n}"))?;
                }
            }
            for name in &sidecars {
                copy_file(&spool.join(name), &destination.join("spool").join(name))?;
                checkpoint(&format!("create.spool.{name}"))?;
            }
        }
        checkpoint("create.spool")?;
        let mut files = BTreeMap::new();
        inventory(destination, Path::new(""), &mut files)?;
        write_private(
            &destination.join(MANIFEST),
            &serde_json::to_vec_pretty(&Manifest { version: 1, files })?,
        )?;
        checkpoint("create.manifest")?;
        validate(destination)?;
        checkpoint("create.validated")
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

pub fn restore(bundle: &Path, destination: &Path) -> Result<()> {
    restore_with_checkpoints(bundle, destination, &mut |_| Ok(()))
}

fn restore_with_checkpoints(
    bundle: &Path,
    destination: &Path,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    publish(destination, checkpoint, "restore", |dest, checkpoint| {
        restore_in_place(bundle, dest, checkpoint)
    })
}

fn restore_in_place(
    bundle: &Path,
    destination: &Path,
    checkpoint: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    validate(bundle)?;
    checkpoint("restore.source_validated")?;
    private_dir(destination)?;
    let result = (|| -> Result<()> {
        checkpoint("restore.directory")?;
        let mut files = BTreeMap::new();
        inventory(bundle, Path::new(""), &mut files)?;
        for rel in files.keys() {
            let path = PathBuf::from(rel);
            let dst = destination.join(&path);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
            copy_file(&bundle.join(path), &dst)?;
            checkpoint(&format!("restore.file.{rel}"))?;
        }
        // Validate the copy as well as the source, including checksums.
        copy_file(&bundle.join(MANIFEST), &destination.join(MANIFEST))?;
        checkpoint("restore.manifest")?;
        validate(destination)?;
        checkpoint("restore.validated")?;
        fs::remove_file(destination.join(MANIFEST))?;
        checkpoint("restore.finished")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StopOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn source(root: &Path) -> PathBuf {
        let dir = root.join("source");
        private_dir(&dir).unwrap();
        let key = sign::generate();
        write_private(&dir.join("device.key"), sign::seed_hex(&key).as_bytes()).unwrap();
        write_private(&dir.join("prompt.key"), "42".repeat(32).as_bytes()).unwrap();
        write_private(&dir.join("content.key"), "43".repeat(32).as_bytes()).unwrap();
        let db = Connection::open(dir.join("speedrun.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE prompts (event_id TEXT, ciphertext BLOB); CREATE TABLE events (run_id TEXT, event_id TEXT, body TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO meta VALUES ('device_id', ?1)",
            [sign::fingerprint(&sign::pubkey_hex(&key)).unwrap()],
        )
        .unwrap();
        private_dir(&dir.join("spool")).unwrap();
        let spool = Connection::open(dir.join("spool/pending.sqlite3")).unwrap();
        spool
            .execute_batch(r#"CREATE TABLE outbox (id INTEGER PRIMARY KEY, entry TEXT NOT NULL, state TEXT NOT NULL, owner TEXT, lease_until INTEGER, source TEXT UNIQUE); INSERT INTO outbox(entry,state) VALUES ('{"path":"/v1/prompts","body":{"event_id":"pending-1","content":"legacy"}}','pending');"#)
            .unwrap();
        write_private(&dir.join("spool/outbox.key"), b"outbox key").unwrap();
        dir
    }

    fn fault_source(root: &Path) -> PathBuf {
        let dir = source(root);
        write_private(
            &dir.join("device.key.next"),
            sign::seed_hex(&sign::generate()).as_bytes(),
        )
        .unwrap();
        write_private(&dir.join("trusted-projects.json"), b"{}").unwrap();
        write_private(
            &dir.join("preferences.json"),
            &serde_json::to_vec(&speedrun_core::preferences::Preferences::default()).unwrap(),
        )
        .unwrap();
        private_dir(&dir.join("disclosure")).unwrap();
        let disclosure_key = sign::generate();
        write_private(
            &dir.join("disclosure/run"),
            sign::seed_hex(&disclosure_key).as_bytes(),
        )
        .unwrap();
        start(
            &dir,
            Some(speedrun_core::digest::key_commitment(&disclosure_key.to_bytes())),
        );
        for name in ["pending.jsonl", "rejected.jsonl", "flushing-fixture"] {
            write_private(&dir.join("spool").join(name), b"fixture").unwrap();
        }
        dir
    }

    fn add_event(
        source: &Path,
        payload: serde_json::Value,
        event_id: &str,
        previous: Option<&speedrun_core::Event>,
    ) -> speedrun_core::Event {
        let key = sign::parse_seed(&fs::read_to_string(source.join("device.key")).unwrap()).unwrap();
        let data = serde_json::json!({
            "event_id": event_id, "schema_version": 2, "run_id": "run", "seq": previous.map_or(1, |e| e.seq + 1),
            "device_id": "device", "occurred_at": "2026-01-01T00:00:00Z", "recorded_at": "2026-01-01T00:00:00Z",
            "payload": payload, "prev_hash": previous.map(|e| e.hash.clone()), "hash": ""
        });
        let mut event: speedrun_core::Event = serde_json::from_value(data).unwrap();
        event.hash = speedrun_core::hash::compute_hash(&event);
        event.sig = Some(sign::sign_hash(&key, &event.hash));
        let db = Connection::open(source.join("speedrun.sqlite")).unwrap();
        db.execute(
            "INSERT INTO events VALUES (?1, ?2, ?3)",
            rusqlite::params![
                event.run_id,
                event.event_id,
                serde_json::to_string(&event).unwrap()
            ],
        )
        .unwrap();
        event
    }
    fn start(source: &Path, commitment: Option<String>) -> speedrun_core::Event {
        let key = sign::parse_seed(&fs::read_to_string(source.join("device.key")).unwrap()).unwrap();
        add_event(
            source,
            serde_json::json!({"type":"run_started", "project_id":"project", "category_id":"any-percent", "category_version":2, "device_pubkey":sign::pubkey_hex(&key), "disclosure_commitment":commitment}),
            "start",
            None,
        )
    }

    #[test]
    fn roundtrip_preserves_data_and_refuses_clobber() {
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        let bundle = root.path().join("bundle");
        let dest = root.path().join("restored");
        create(&source, &bundle).unwrap();
        restore(&bundle, &dest).unwrap();
        assert_eq!(fs::read(dest.join("spool/outbox.key")).unwrap(), b"outbox key");
        let db = Connection::open(dest.join("spool/pending.sqlite3")).unwrap();
        assert_eq!(
            db.query_row("SELECT state FROM outbox", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "pending"
        );
        assert!(restore(&bundle, &dest).is_err());
    }

    #[test]
    fn snapshot_waits_for_a_transient_exclusive_writer() {
        use std::sync::mpsc::{self, RecvTimeoutError};

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.sqlite");
        let destination = root.path().join("snapshot.sqlite");
        let writer = Connection::open(&source).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE data (value TEXT); INSERT INTO data VALUES ('saved'); BEGIN EXCLUSIVE;",
            )
            .unwrap();
        let fast_source = source.clone();
        let fast_destination = root.path().join("busy.sqlite");
        let (fast_tx, fast_rx) = mpsc::channel();
        let fast_worker = std::thread::spawn(move || {
            let _ = fast_tx.send(snapshot_with_timeout(
                &fast_source,
                &fast_destination,
                Duration::ZERO,
            ));
        });
        let immediate = fast_rx.recv_timeout(Duration::from_secs(1));
        writer.execute_batch("ROLLBACK;").unwrap();
        fast_worker.join().unwrap();
        assert!(
            immediate
                .expect("zero timeout must not wait for the writer")
                .is_err()
        );
        writer.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        let (tx, rx) = mpsc::channel();
        let worker_destination = destination.clone();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(snapshot_with_timeout(
                &source,
                &worker_destination,
                Duration::from_secs(10),
            ));
        });
        let early = rx.recv_timeout(Duration::from_millis(100));
        // Release even if the worker returned unexpectedly so it can be joined.
        writer.execute_batch("ROLLBACK;").unwrap();
        let result = match early {
            Err(RecvTimeoutError::Timeout) => rx.recv_timeout(Duration::from_secs(15)).unwrap(),
            other => panic!("snapshot did not wait for the writer: {other:?}"),
        };
        worker.join().unwrap();
        result.unwrap();
        let saved = Connection::open(&destination).unwrap();
        assert_eq!(
            saved
                .query_row("SELECT value FROM data", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "saved"
        );
    }

    #[test]
    fn refuses_running_daemon_and_busy_import() {
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        let lock = Connection::open(source.join("speedrund.lock")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(create(&source, &root.path().join("busy")).is_err());
        drop(lock);
        let import = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(source.join("spool/import.lock"))
            .unwrap();
        import.lock().unwrap();
        assert!(create(&source, &root.path().join("import-busy")).is_err());
    }

    #[test]
    fn missing_key_and_mismatched_identity_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        fs::remove_file(source.join("content.key")).unwrap();
        assert!(create(&source, &root.path().join("missing")).is_err());
        write_private(&source.join("content.key"), "43".repeat(32).as_bytes()).unwrap();
        fs::write(source.join("device.key"), sign::seed_hex(&sign::generate())).unwrap();
        assert!(create(&source, &root.path().join("mismatch")).is_err());
    }

    #[test]
    fn content_key_must_authenticate_every_retained_prompt() {
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        let first = start(&source, None);
        let prompt_key = sign::parse_seed(&"42".repeat(32)).unwrap().to_bytes();
        add_event(
            &source,
            serde_json::json!({"type":"prompt_submitted", "session_id":"s", "host":"test", "model":null, "content_digest":speedrun_core::digest::text_digest(&prompt_key, "prompt"), "length":6, "redacted":false}),
            "event",
            Some(&first),
        );
        let db = Connection::open(source.join("speedrun.sqlite")).unwrap();
        let key = sign::parse_seed(&"43".repeat(32)).unwrap().to_bytes();
        let cipher = XChaCha20Poly1305::new((&key).into());
        let nonce = [7u8; 24];
        let mut blob = nonce.to_vec();
        blob.extend(
            cipher
                .encrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: b"prompt",
                        aad: b"event",
                    },
                )
                .unwrap(),
        );
        db.execute(
            "INSERT INTO prompts VALUES (?1, ?2)",
            rusqlite::params!["event", blob],
        )
        .unwrap();
        drop(db);
        create(&source, &root.path().join("valid")).unwrap();
        fs::write(source.join("content.key"), "44".repeat(32)).unwrap();
        assert!(create(&source, &root.path().join("wrong")).is_err());
        fs::write(source.join("content.key"), "43".repeat(32)).unwrap();
        fs::write(source.join("prompt.key"), "45".repeat(32)).unwrap();
        assert!(create(&source, &root.path().join("wrong-prompt")).is_err());
    }

    #[test]
    fn disclosure_key_must_match_run_commitment() {
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        private_dir(&source.join("disclosure")).unwrap();
        let key = sign::generate();
        write_private(&source.join("disclosure/run"), sign::seed_hex(&key).as_bytes()).unwrap();
        start(
            &source,
            Some(speedrun_core::digest::key_commitment(&key.to_bytes())),
        );
        create(&source, &root.path().join("valid")).unwrap();
        fs::write(source.join("disclosure/run"), sign::seed_hex(&sign::generate())).unwrap();
        assert!(create(&source, &root.path().join("wrong")).is_err());
    }

    #[test]
    fn refuses_tampering_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        let bundle = root.path().join("bundle");
        create(&source, &bundle).unwrap();
        fs::write(bundle.join("device.key"), b"modified").unwrap();
        assert!(restore(&bundle, &root.path().join("bad")).is_err());
        fs::remove_file(bundle.join("device.key")).unwrap();
        std::os::unix::fs::symlink(source.join("device.key"), bundle.join("device.key")).unwrap();
        assert!(restore(&bundle, &root.path().join("link")).is_err());
    }
    #[test]
    fn interruption_at_every_checkpoint_preserves_inputs_and_allows_retry() {
        let root = tempfile::tempdir().unwrap();
        let source = fault_source(root.path());
        let bundle = root.path().join("bundle");
        let mut create_points = Vec::new();
        create_with_checkpoints(&source, &bundle, &mut |p| {
            create_points.push(p.to_owned());
            Ok(())
        })
        .unwrap();
        let mut restore_points = Vec::new();
        let restored = root.path().join("restored");
        restore_with_checkpoints(&bundle, &restored, &mut |p| {
            restore_points.push(p.to_owned());
            Ok(())
        })
        .unwrap();
        for (i, point) in create_points.iter().enumerate() {
            let target = root.path().join(format!("create-{i}"));
            assert!(
                create_with_checkpoints(&source, &target, &mut |p| {
                    if p == point {
                        bail!("injected failure at {p}");
                    }
                    Ok(())
                })
                .is_err(),
                "{point}"
            );
            if point == "create.published" {
                validate(&target).unwrap();
                fs::remove_dir_all(&target).unwrap();
            }
            assert!(!target.exists(), "failed create left {point}");
            create(&source, &target).unwrap();
            validate(&target).unwrap();
        }
        for (i, point) in restore_points.iter().enumerate() {
            let target = root.path().join(format!("restore-{i}"));
            assert!(
                restore_with_checkpoints(&bundle, &target, &mut |p| {
                    if p == point {
                        bail!("injected failure at {p}");
                    }
                    Ok(())
                })
                .is_err(),
                "{point}"
            );
            if point == "restore.published" {
                assert!(target.join("speedrun.sqlite").exists());
                fs::remove_dir_all(&target).unwrap();
            }
            assert!(!target.exists(), "failed restore left {point}");
            validate(&bundle).unwrap();
            restore(&bundle, &target).unwrap();
        }
        validate(&bundle).unwrap();
        assert!(source.join("speedrun.sqlite").exists());
    }

    // A killed process cannot run cleanup. Only a fully built destination is
    // published; abandoned sibling staging directories do not block retry.
    #[test]
    fn process_death_at_every_checkpoint_preserves_inputs_and_never_clobbers() {
        if let Ok(spec) = std::env::var("SPEEDRUN_RECOVERY_TEST_CHILD") {
            let mut parts = spec.splitn(4, '|');
            let mode = parts.next().unwrap();
            let input = PathBuf::from(parts.next().unwrap());
            let output = PathBuf::from(parts.next().unwrap());
            let point = parts.next().unwrap();
            let source = PathBuf::from(std::env::var_os("SPEEDRUN_RECOVERY_SOURCE").unwrap());
            let project = PathBuf::from(std::env::var_os("SPEEDRUN_RECOVERY_PROJECT").unwrap());
            let session = std::env::var("SPEEDRUN_RECOVERY_SESSION").unwrap();
            let mut kill = |p: &str| -> Result<()> {
                if p == point {
                    use std::io::Write;
                    let mut hook = std::process::Command::new(std::env::current_exe()?)
                        .args([
                            "--exact",
                            "recovery::tests::real_hook_processes_enqueue_during_recovery",
                            "--quiet",
                        ])
                        .env("SPEEDRUN_RECOVERY_HOOK_CHILD", "1")
                        .env("SPEEDRUN_HOME", &source)
                        .env("HOME", &source)
                        .env("SPEEDRUN_NO_AUTOSTART", "1")
                        .env("SPEEDRUN_ADDR", "127.0.0.1:1")
                        .current_dir(&project)
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::null())
                        .spawn()?;
                    write!(
                        hook.stdin.take().unwrap(),
                        "{}",
                        serde_json::json!({"prompt": "checkpoint fixture", "session_id": session})
                    )?;
                    ensure!(hook.wait()?.success(), "checkpoint hook failed");
                    let count =
                        crate::client::ingress::test_count_entries(&source.join("spool"), Some(&session))?;
                    ensure!(count == 1, "checkpoint hook was not durably queued");
                    std::process::Command::new("kill")
                        .args(["-KILL", &std::process::id().to_string()])
                        .status()
                        .unwrap();
                    unreachable!();
                }
                Ok(())
            };
            if mode == "create" {
                create_with_checkpoints(&input, &output, &mut kill).unwrap();
            } else {
                restore_with_checkpoints(&input, &output, &mut kill).unwrap();
            }
            panic!("checkpoint not reached");
        }
        let root = tempfile::tempdir().unwrap();
        let source = fault_source(root.path());
        fs::remove_file(source.join("spool/outbox.key")).unwrap();
        crate::client::test_enqueue_prompt(&source.join("spool"), "before-crashes").unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(project.join(".speedrun")).unwrap();
        fs::write(project.join(".speedrun/project.json"), r#"{"project_id":"01KAUDITPROJECT0000000000","category_id":"any-percent","category_version":1,"created_at":"2026-11-01T00:00:00Z"}"#).unwrap();
        fs::write(source.join("trusted-projects.json"), serde_json::json!([{"project_id":"01KAUDITPROJECT0000000000","root":project.canonicalize().unwrap()}]).to_string()).unwrap();
        fs::write(source.join("token"), "fixture-token").unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _stop_on_drop = StopOnDrop(stop.clone());
        let worker_stop = stop.clone();
        let worker_source = source.clone();
        let worker_project = project.clone();
        let (ready, wait_ready) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            use std::io::Write;
            use std::sync::atomic::Ordering;
            let mut count = 0;
            while !worker_stop.load(Ordering::SeqCst) {
                let mut hook = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "recovery::tests::real_hook_processes_enqueue_during_recovery",
                        "--quiet",
                    ])
                    .env("SPEEDRUN_RECOVERY_HOOK_CHILD", "1")
                    .env("SPEEDRUN_HOME", &worker_source)
                    .env("HOME", &worker_source)
                    .env("SPEEDRUN_NO_AUTOSTART", "1")
                    .env("SPEEDRUN_ADDR", "127.0.0.1:1")
                    .current_dir(&worker_project)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                write!(
                    hook.stdin.take().unwrap(),
                    "{{\"prompt\":\"crash-{}\",\"session_id\":\"session\"}}",
                    count
                )
                .unwrap();
                assert!(hook.wait().unwrap().success());
                count += 1;
                if count == 1 {
                    ready.send(()).unwrap();
                }
            }
            count
        });
        wait_ready.recv().unwrap();
        let bundle = root.path().join("bundle");
        let mut creates = Vec::new();
        create_with_checkpoints(&source, &bundle, &mut |p| {
            creates.push(p.to_owned());
            Ok(())
        })
        .unwrap();
        let mut restores = Vec::new();
        restore_with_checkpoints(&bundle, &root.path().join("initial"), &mut |p| {
            restores.push(p.to_owned());
            Ok(())
        })
        .unwrap();
        let spill_count = creates
            .iter()
            .filter(|point| {
                point.starts_with("create.spool.flushing-fast-")
                    || point.starts_with("create.spool.flushing-outcome-")
            })
            .count();
        assert_eq!(creates.len(), 20 + spill_count);
        assert_eq!(restores.len(), 20 + spill_count);
        let checkpoint_hooks = (creates.len() + restores.len()) as i64;
        for (mode, input, points) in [("create", &source, creates), ("restore", &bundle, restores)] {
            for (i, point) in points.iter().enumerate() {
                let output = root.path().join(format!("killed-{mode}-{i}"));
                let spec = format!("{mode}|{}|{}|{point}", input.display(), output.display());
                let session = format!("checkpoint-{mode}-{i}");
                let status = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "recovery::tests::process_death_at_every_checkpoint_preserves_inputs_and_never_clobbers", "--quiet"])
                    .env("SPEEDRUN_RECOVERY_TEST_CHILD", spec)
                    .env("SPEEDRUN_RECOVERY_SOURCE", &source)
                    .env("SPEEDRUN_RECOVERY_PROJECT", &project)
                    .env("SPEEDRUN_RECOVERY_SESSION", &session)
                    .status().unwrap();
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(status.signal(), Some(9), "{mode} {point}");
                let accepted =
                    crate::client::ingress::test_count_entries(&source.join("spool"), Some(&session))
                        .unwrap();
                assert_eq!(accepted, 1, "checkpoint hook lost after {mode} {point}");
                if point.ends_with(".published") {
                    assert!(output.exists(), "published destination missing at {point}");
                    if mode == "create" {
                        validate(&output).unwrap();
                    } else {
                        assert!(output.join("speedrun.sqlite").is_file());
                    }
                } else {
                    assert!(!output.exists(), "partial destination visible at {point}");
                }
                if output.exists() {
                    let sentinel = output.join("sentinel");
                    fs::write(&sentinel, b"keep").unwrap();
                    assert!(
                        if mode == "create" {
                            create(input, &output)
                        } else {
                            restore(input, &output)
                        }
                        .is_err()
                    );
                    assert_eq!(fs::read(sentinel).unwrap(), b"keep");
                    fs::remove_dir_all(&output).unwrap();
                }
                if mode == "create" {
                    create(input, &output).unwrap();
                    validate(&output).unwrap();
                } else {
                    validate(input).unwrap();
                    restore(input, &output).unwrap();
                }
            }
        }
        validate(&bundle).unwrap();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let hooks = worker.join().unwrap();
        let count = crate::client::ingress::test_count_entries(&source.join("spool"), None).unwrap();
        assert_eq!(count, hooks + 2 + checkpoint_hooks); // two fixture rows plus one hook per checkpoint
        crate::client::validate_existing_outbox(&source.join("spool/pending.sqlite3")).unwrap();
    }

    #[test]
    fn concurrent_enqueues_have_a_sqlite_snapshot_cutoff() {
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        let spool = source.join("spool");
        fs::remove_file(spool.join("outbox.key")).unwrap();
        crate::client::test_enqueue_prompt(&spool, "initial").unwrap();
        let bundle = root.path().join("bundle");
        let mut accepted = 1_i64;
        let mut cutoff = None;
        create_with_checkpoints(&source, &bundle, &mut |p| {
            if p == "create.outbox" {
                cutoff = Some(accepted);
            }
            let dir = spool.clone();
            let id = format!("concurrent-{accepted}");
            std::thread::spawn(move || crate::client::test_enqueue_prompt(&dir, &id).unwrap())
                .join()
                .unwrap();
            accepted += 1;
            Ok(())
        })
        .unwrap();
        validate(&bundle).unwrap();
        let snapshot = Connection::open(bundle.join("spool/pending.sqlite3")).unwrap();
        // One legacy row predates the encrypted enqueues.
        let count: i64 = snapshot
            .query_row("SELECT count(*) FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, cutoff.unwrap() + 1);
        let live = Connection::open(spool.join("pending.sqlite3")).unwrap();
        let count: i64 = live
            .query_row("SELECT count(*) FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, accepted + 1);
        restore(&bundle, &root.path().join("restored-concurrent")).unwrap();
    }

    #[test]
    fn recovery_preserves_ordered_spills_and_excludes_later_admissions() {
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        let spool = source.join("spool");
        fs::remove_file(spool.join("outbox.key")).unwrap();
        crate::client::test_enqueue_prompt(&spool, "before-database").unwrap();
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(spool.join("import.lock"))
            .unwrap();
        lock.lock().unwrap();
        crate::client::test_admit_prompt(&spool, "before-spill").unwrap();
        drop(lock);
        let bundle = root.path().join("ordered-bundle");
        let mut admitted_after_cut = false;
        create_with_checkpoints(&source, &bundle, &mut |point| {
            if point == "create.outbox" {
                crate::client::test_admit_prompt(&spool, "after-cut").unwrap();
                admitted_after_cut = true;
            }
            Ok(())
        })
        .unwrap();
        assert!(admitted_after_cut);
        validate(&bundle).unwrap();
        let restored = root.path().join("ordered-restored");
        restore(&bundle, &restored).unwrap();
        let ids = crate::client::test_drain_prompt_ids(&restored.join("spool")).unwrap();
        assert_eq!(ids, ["pending-1", "before-database", "before-spill"]);
        let live = crate::client::test_drain_prompt_ids(&spool).unwrap();
        assert_eq!(
            live,
            ["pending-1", "before-database", "before-spill", "after-cut"]
        );
    }

    #[test]
    fn recovery_rejects_a_tampered_spill_even_with_updated_manifest_checksum() {
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        let spool = source.join("spool");
        fs::remove_file(spool.join("outbox.key")).unwrap();
        crate::client::test_enqueue_prompt(&spool, "initial").unwrap();
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(spool.join("import.lock"))
            .unwrap();
        lock.lock().unwrap();
        crate::client::test_admit_prompt(&spool, "spill").unwrap();
        drop(lock);
        let bundle = root.path().join("tampered-bundle");
        create(&source, &bundle).unwrap();
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(bundle.join(MANIFEST)).unwrap()).unwrap();
        let name = manifest
            .files
            .keys()
            .find(|name| name.starts_with("spool/flushing-fast-"))
            .unwrap()
            .clone();
        fs::write(bundle.join(&name), b"tampered\n").unwrap();
        manifest
            .files
            .insert(name.clone(), sha256_hex(&fs::read(bundle.join(&name)).unwrap()));
        fs::write(bundle.join(MANIFEST), serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(validate(&bundle).is_err());
    }

    #[test]
    fn first_enqueue_after_absent_spool_check_is_after_cutoff() {
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        fs::remove_dir_all(source.join("spool")).unwrap();
        let bundle = root.path().join("bundle");
        let mut queued = false;
        create_with_checkpoints(&source, &bundle, &mut |p| {
            if p == "create.database" {
                let spool = source.join("spool");
                std::thread::spawn(move || crate::client::test_enqueue_prompt(&spool, "first").unwrap())
                    .join()
                    .unwrap();
                queued = true;
            }
            Ok(())
        })
        .unwrap();
        assert!(queued);
        assert!(!bundle.join("spool/pending.sqlite3").exists());
        crate::client::validate_existing_outbox(&source.join("spool/pending.sqlite3")).unwrap();
    }
    #[test]
    fn writer_runs_during_outbox_snapshot() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        let spool = source.join("spool");
        fs::remove_file(spool.join("outbox.key")).unwrap();
        crate::client::test_enqueue_prompt(&spool, "before").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_on_drop = StopOnDrop(stop.clone());
        let worker_stop = stop.clone();
        let worker_spool = spool.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut count = 0;
            while !worker_stop.load(Ordering::SeqCst) {
                crate::client::test_enqueue_prompt(&worker_spool, &format!("overlap-{count}")).unwrap();
                count += 1;
                if count == 1 {
                    ready_tx.send(()).unwrap();
                }
            }
            count
        });
        ready_rx.recv().unwrap();
        let bundle = root.path().join("overlap");
        create(&source, &bundle).unwrap();
        stop.store(true, Ordering::SeqCst);
        let writes = worker.join().unwrap();
        crate::client::test_enqueue_prompt(&spool, "after").unwrap();
        validate(&bundle).unwrap();
        let snapshot = Connection::open(bundle.join("spool/pending.sqlite3")).unwrap();
        let snapshot_count: i64 = snapshot
            .query_row("SELECT count(*) FROM outbox", [], |r| r.get(0))
            .unwrap();
        let live = Connection::open(spool.join("pending.sqlite3")).unwrap();
        let live_count: i64 = live
            .query_row("SELECT count(*) FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert!(snapshot_count >= 3 && snapshot_count < live_count);
        assert_eq!(live_count, writes + 3);
    }
    #[test]
    fn real_hook_processes_enqueue_during_recovery() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        if std::env::var_os("SPEEDRUN_RECOVERY_HOOK_CHILD").is_some() {
            crate::hook::claude_code("user-prompt-submit");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let source = source(root.path());
        fs::remove_dir_all(source.join("spool")).unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(project.join(".speedrun")).unwrap();
        fs::write(project.join(".speedrun/project.json"), r#"{"project_id":"01KAUDITPROJECT0000000000","category_id":"any-percent","category_version":1,"created_at":"2026-11-01T00:00:00Z"}"#).unwrap();
        fs::write(source.join("trusted-projects.json"), serde_json::json!([{"project_id":"01KAUDITPROJECT0000000000","root":project.canonicalize().unwrap()}]).to_string()).unwrap();
        fs::write(source.join("token"), "fixture-token").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_on_drop = StopOnDrop(stop.clone());
        let worker_stop = stop.clone();
        let worker_source = source.clone();
        let worker_project = project.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut count = 0;
            while !worker_stop.load(Ordering::SeqCst) {
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "recovery::tests::real_hook_processes_enqueue_during_recovery",
                        "--quiet",
                    ])
                    .env("SPEEDRUN_RECOVERY_HOOK_CHILD", "1")
                    .env("SPEEDRUN_HOME", &worker_source)
                    .env("HOME", &worker_source)
                    .env("SPEEDRUN_NO_AUTOSTART", "1")
                    .env("SPEEDRUN_ADDR", "127.0.0.1:1")
                    .current_dir(&worker_project)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap();
                write!(
                    child.stdin.take().unwrap(),
                    "{{\"prompt\":\"fixture-{}\",\"session_id\":\"session\"}}",
                    count
                )
                .unwrap();
                assert!(child.wait().unwrap().success());
                count += 1;
                if count == 1 {
                    tx.send(()).unwrap();
                }
            }
            count
        });
        rx.recv().unwrap();
        let bundle = root.path().join("hook-bundle");
        create(&source, &bundle).unwrap();
        stop.store(true, Ordering::SeqCst);
        let count = worker.join().unwrap();
        validate(&bundle).unwrap();
        let snapshot = Connection::open(bundle.join("spool/pending.sqlite3")).unwrap();
        let included: i64 = snapshot
            .query_row("SELECT count(*) FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert!(included >= 1 && included <= count);
        let live = Connection::open(source.join("spool/pending.sqlite3")).unwrap();
        let live_count: i64 = live
            .query_row("SELECT count(*) FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(live_count, count);
        restore(&bundle, &root.path().join("hook-restored")).unwrap();
    }
}
