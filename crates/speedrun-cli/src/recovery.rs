//! Offline recovery bundles. The daemon lock freezes the event database and
//! device keys; the import lock prevents legacy spool files moving while the
//! SQLite outbox is snapshotted.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

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
    regular(src)?;
    let db = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
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

pub fn create(source: &Path, destination: &Path) -> Result<()> {
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
        f.try_lock().context("legacy spool import is busy")?;
        Some(f)
    } else {
        None
    };
    private_dir(destination)?;
    let result = (|| -> Result<()> {
        snapshot(
            &source.join("speedrun.sqlite"),
            &destination.join("speedrun.sqlite"),
        )?;
        for name in [
            "device.key",
            "device.key.next",
            "prompt.key",
            "content.key",
            "trusted-projects.json",
            "preferences.json",
        ] {
            copy_optional(&source.join(name), &destination.join(name))?;
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
            }
        }
        if import_lock.is_some() {
            private_dir(&destination.join("spool"))?;
            let outbox = spool.join("pending.sqlite3");
            if present(&outbox)? {
                snapshot(&outbox, &destination.join("spool/pending.sqlite3"))?;
            }
            for item in fs::read_dir(&spool)? {
                let item = item?;
                let name = item.file_name();
                let n = name.to_str().context("non-UTF-8 spool filename")?;
                if n == "outbox.key"
                    || n == "pending.jsonl"
                    || n == "rejected.jsonl"
                    || n.starts_with("flushing-")
                {
                    copy_file(&item.path(), &destination.join("spool").join(name))?;
                }
            }
        }
        let mut files = BTreeMap::new();
        inventory(destination, Path::new(""), &mut files)?;
        write_private(
            &destination.join(MANIFEST),
            &serde_json::to_vec_pretty(&Manifest { version: 1, files })?,
        )?;
        validate(destination)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

pub fn restore(bundle: &Path, destination: &Path) -> Result<()> {
    validate(bundle)?;
    private_dir(destination)?;
    let result = (|| -> Result<()> {
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
        }
        // Validate the copy as well as the source, including checksums.
        copy_file(&bundle.join(MANIFEST), &destination.join(MANIFEST))?;
        validate(destination)?;
        fs::remove_file(destination.join(MANIFEST))?;
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
}
