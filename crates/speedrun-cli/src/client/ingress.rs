//! Ordered, encrypted admission when SQLite is busy. The admission lock orders
//! direct rows and spill files; the import lock excludes claims while a file's
//! transaction and durable removal are being reconciled.
use super::*;

const FAST: &str = "flushing-fast-";
const OUTCOME: &str = "flushing-outcome-";
const IMPORT_BATCH: usize = 32;

pub(super) enum Admission {
    Leased(i64),
    Deferred,
}

#[derive(Serialize, Deserialize)]
pub(super) enum Transition {
    Release,
    Accepted,
    Rejected(Option<u16>),
}

#[derive(Serialize, Deserialize)]
struct Outcome {
    id: i64,
    owner: String,
    transition: Transition,
}

fn open_lock(dir: &Path, name: &str) -> anyhow::Result<fs::File> {
    Ok(OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join(name))?)
}

pub(crate) fn lock_until(file: &fs::File, deadline: Option<Instant>) -> anyhow::Result<bool> {
    let Some(deadline) = deadline else {
        file.lock()?;
        return Ok(true);
    };
    loop {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        match file.try_lock() {
            Ok(()) => return Ok(true),
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(5)),
                );
            }
            Err(fs::TryLockError::WouldBlock) => return Ok(false),
        }
    }
}

fn queue_lock(dir: &Path, deadline: Option<Instant>) -> anyhow::Result<Option<fs::File>> {
    let lock = open_lock(dir, ".queue.lock")?;
    if lock_until(&lock, deadline)? {
        Ok(Some(lock))
    } else {
        Ok(None)
    }
}

fn try_import_lock(dir: &Path) -> anyhow::Result<Option<fs::File>> {
    let lock = open_lock(dir, "import.lock")?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn import_lock(dir: &Path, deadline: Option<Instant>) -> anyhow::Result<Option<fs::File>> {
    let lock = open_lock(dir, "import.lock")?;
    if lock_until(&lock, deadline)? {
        Ok(Some(lock))
    } else {
        Ok(None)
    }
}

fn files_with_prefix(dir: &Path, prefix: &str) -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    for item in fs::read_dir(dir)? {
        let item = item?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF-8 spool filename"))?;
        if name.starts_with(prefix) {
            anyhow::ensure!(
                item.file_type()?.is_file(),
                "spool item is not a regular file: {name}"
            );
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

fn next_fast_name(dir: &Path) -> anyhow::Result<String> {
    let mut last = 0u64;
    for name in files_with_prefix(dir, FAST)? {
        let suffix = name.strip_prefix(FAST).expect("prefix checked");
        let (number, _) = suffix
            .split_once('-')
            .ok_or_else(|| anyhow::anyhow!("invalid spill filename"))?;
        anyhow::ensure!(number.len() == 20, "invalid spill sequence");
        last = last.max(number.parse()?);
    }
    let next = last
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("spill sequence exhausted"))?;
    Ok(format!("{FAST}{next:020}-{}", speedrun_core::new_id()))
}

fn publish<T: Serialize>(db: &Connection, dir: &Path, name: &str, value: &T) -> anyhow::Result<()> {
    let plaintext = serde_json::to_vec(value)?;
    let sealed = seal(&read_outbox_key(db)?, &plaintext, name.as_bytes())?;
    let mut temp = tempfile::Builder::new()
        .prefix(".outbox-stage-")
        .tempfile_in(dir)?;
    serde_json::to_writer(&mut temp, &sealed)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(dir.join(name)).map_err(|e| e.error)?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn read<T: DeserializeOwned>(db: &Connection, dir: &Path, name: &str) -> anyhow::Result<T> {
    let envelope: Value = serde_json::from_slice(&fs::read(dir.join(name))?)?;
    let plaintext = open_sealed(&read_outbox_key(db)?, &envelope, name.as_bytes())?;
    Ok(serde_json::from_slice(&plaintext)?)
}

pub(super) fn admit(
    db: &Connection,
    dir: &Path,
    entry: &SpoolEntry,
    owner: &str,
    deadline: Instant,
) -> anyhow::Result<Admission> {
    let _queue = queue_lock(dir, Some(deadline))?
        .ok_or_else(|| anyhow::anyhow!("timed out waiting for outbox admission"))?;
    // A recovery snapshot or an importer holds this lock. Never wait for it
    // while holding admission: add a spill file instead.
    let import = try_import_lock(dir)?;
    if import.is_some() && files_with_prefix(dir, FAST)?.is_empty() {
        db.busy_timeout(Duration::ZERO)?;
        match enqueue(db, entry, Some(owner)) {
            Ok(id) => return Ok(Admission::Leased(id)),
            Err(error) if is_sqlite_busy(&error) => {
                anyhow::ensure!(db.is_autocommit(), "busy enqueue left an unfinished transaction");
            }
            Err(error) => return Err(error),
        }
    }
    let name = next_fast_name(dir)?;
    publish(db, dir, &name, entry)?;
    Ok(Admission::Deferred)
}

pub(super) fn preserve_transition(
    db: &Connection,
    dir: &Path,
    id: i64,
    owner: &str,
    transition: Transition,
) -> anyhow::Result<()> {
    let name = format!("{OUTCOME}{}", speedrun_core::new_id());
    publish(
        db,
        dir,
        &name,
        &Outcome {
            id,
            owner: owner.to_owned(),
            transition,
        },
    )
}

fn apply_outcomes(db: &Connection, dir: &Path, deadline: Option<Instant>) -> anyhow::Result<bool> {
    for name in files_with_prefix(dir, OUTCOME)? {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(false);
        }
        let outcome: Outcome = read(db, dir, &name)?;
        set_busy_deadline(db, deadline)?;
        let current: Option<(String, Option<String>)> = db
            .query_row("SELECT state,owner FROM outbox WHERE id=?1", [outcome.id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        if current
            .as_ref()
            .is_some_and(|(state, owner)| state == "leased" && owner.as_deref() == Some(&outcome.owner))
        {
            match outcome.transition {
                Transition::Release => release(db, outcome.id, &outcome.owner)?,
                Transition::Accepted => {
                    finish_with_deadline(db, outcome.id, &outcome.owner, false, deadline)?
                }
                Transition::Rejected(status) => {
                    finish_outbox(db, outcome.id, &outcome.owner, true, status, deadline)?
                }
            }
        }
        fs::remove_file(dir.join(name))?;
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(true)
}

fn import_fast(db: &Connection, dir: &Path, deadline: Option<Instant>) -> anyhow::Result<bool> {
    let names = files_with_prefix(dir, FAST)?;
    for batch in names.chunks(IMPORT_BATCH) {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(false);
        }
        set_busy_deadline(db, deadline)?;
        let tx = db.unchecked_transaction()?;
        for name in batch {
            let entry: SpoolEntry = read(&tx, dir, name)?;
            let source = format!("ordered-spill:{name}");
            tx.execute(
                "INSERT OR IGNORE INTO outbox(id,entry,state,source) VALUES ((SELECT highwater+1 FROM outbox_ids WHERE id=1),?1,'pending',?2)",
                params![serde_json::to_string(&protect_entry(&tx, &entry)?)?, source],
            )?;
        }
        tx.commit()?;
        // Serialize removal with admission's sequence allocation. The import
        // lock prevents another flusher claiming rows before removal is durable.
        let Some(_queue) = queue_lock(dir, deadline)? else {
            return Ok(false);
        };
        for name in batch {
            fs::remove_file(dir.join(name))?;
        }
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(true)
}

pub(super) fn claim_next(
    db: &Connection,
    dir: &Path,
    owner: &str,
    deadline: Option<Instant>,
) -> anyhow::Result<Option<(i64, SpoolEntry)>> {
    let Some(_import) = import_lock(dir, deadline)? else {
        return Ok(None);
    };
    loop {
        if !apply_outcomes(db, dir, deadline)? || !import_fast(db, dir, deadline)? {
            return Ok(None);
        }
        let Some(_queue) = queue_lock(dir, deadline)? else {
            return Ok(None);
        };
        if !files_with_prefix(dir, FAST)?.is_empty() {
            continue;
        }
        set_busy_deadline(db, deadline)?;
        return claim(db, owner);
    }
}

/// Capture the immutable sidecars present at the recovery snapshot boundary.
pub(crate) fn snapshot_files(dir: &Path) -> anyhow::Result<Vec<String>> {
    let _queue = queue_lock(dir, None)?.expect("unbounded lock");
    let mut names = files_with_prefix(dir, FAST)?;
    names.extend(files_with_prefix(dir, OUTCOME)?);
    Ok(names)
}

pub(crate) fn is_sidecar(name: &str) -> bool {
    name.starts_with(FAST) || name.starts_with(OUTCOME)
}

pub(crate) fn is_outcome(name: &str) -> bool {
    name.starts_with(OUTCOME)
}

pub(crate) fn validate_sidecars(dir: &Path) -> anyhow::Result<()> {
    let mut names = files_with_prefix(dir, FAST)?;
    names.extend(files_with_prefix(dir, OUTCOME)?);
    if names.is_empty() {
        return Ok(());
    }
    let db = Connection::open_with_flags(
        dir.join("pending.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    for name in names {
        if name.starts_with(FAST) {
            let _: SpoolEntry = read(&db, dir, &name)?;
        } else {
            let _: Outcome = read(&db, dir, &name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_count_entries(dir: &Path, session: Option<&str>) -> anyhow::Result<i64> {
    let _queue = queue_lock(dir, None)?.expect("unbounded lock");
    test_count_snapshot_entries(dir, session)
}

// A completed recovery bundle is immutable and must not gain a lock file.
#[cfg(test)]
pub(crate) fn test_count_snapshot_entries(dir: &Path, session: Option<&str>) -> anyhow::Result<i64> {
    let db = Connection::open_with_flags(
        dir.join("pending.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut count: i64 = if let Some(session) = session {
        db.query_row(
            "SELECT COUNT(*) FROM outbox WHERE json_extract(entry, '$.body.session_id')=?1",
            [session],
            |r| r.get(0),
        )?
    } else {
        db.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))?
    };
    for name in files_with_prefix(dir, FAST)? {
        let entry: SpoolEntry = read(&db, dir, &name)?;
        if session.is_none_or(|session| entry.body["session_id"].as_str() == Some(session)) {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> SpoolEntry {
        SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"event_id": id, "content": format!("private prompt {id}")}),
            credential: Credential::Full,
        }
    }

    fn defer(db: &Connection, dir: &Path, id: &str) {
        assert!(matches!(
            admit(
                db,
                dir,
                &entry(id),
                "fresh",
                Instant::now() + Duration::from_secs(10)
            )
            .unwrap(),
            Admission::Deferred
        ));
    }

    #[test]
    fn busy_admission_is_encrypted_and_later_writes_keep_their_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let first = enqueue(&db, &entry("first"), None).unwrap();
        let writer = Connection::open(dir.path().join("pending.sqlite3")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        defer(&db, dir.path(), "second");
        writer.execute_batch("ROLLBACK").unwrap();
        // The DB is writable again, but a later event must not overtake the spill.
        defer(&db, dir.path(), "third");
        let names = files_with_prefix(dir.path(), FAST).unwrap();
        assert_eq!(names.len(), 2);
        for name in &names {
            let bytes = fs::read(dir.path().join(name)).unwrap();
            assert!(
                !bytes
                    .windows(b"private prompt".len())
                    .any(|w| w == b"private prompt")
            );
        }
        for expected in ["first", "second", "third"] {
            let (id, actual) = claim_next(&db, dir.path(), "replay", None).unwrap().unwrap();
            assert_eq!(actual.body["event_id"], expected);
            if expected == "first" {
                assert_eq!(id, first);
            }
            finish(&db, id, "replay", false).unwrap();
        }
        assert!(claim_next(&db, dir.path(), "replay", None).unwrap().is_none());
        assert!(files_with_prefix(dir.path(), FAST).unwrap().is_empty());
    }

    #[test]
    fn committed_spill_is_reconciled_before_another_flusher_can_claim_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let lock = open_lock(dir.path(), "import.lock").unwrap();
        lock.lock().unwrap();
        defer(&db, dir.path(), "once");
        drop(lock);
        let name = files_with_prefix(dir.path(), FAST).unwrap().pop().unwrap();
        let stored: SpoolEntry = read(&db, dir.path(), &name).unwrap();
        // Model a process dying just after the import transaction committed.
        db.execute(
            "INSERT INTO outbox(id,entry,state,source) VALUES ((SELECT highwater+1 FROM outbox_ids WHERE id=1),?1,'pending',?2)",
            params![serde_json::to_string(&protect_entry(&db, &stored).unwrap()).unwrap(), format!("ordered-spill:{name}")],
        ).unwrap();
        let (id, actual) = claim_next(&db, dir.path(), "worker", None).unwrap().unwrap();
        assert_eq!(actual.body["event_id"], "once");
        finish(&db, id, "worker", false).unwrap();
        assert!(claim_next(&db, dir.path(), "other", None).unwrap().is_none());
        assert!(!dir.path().join(name).exists());
    }

    #[test]
    fn failed_release_can_be_recovered_without_waiting_for_the_lease() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let id = enqueue(&db, &entry("leased"), Some("original")).unwrap();
        let writer = Connection::open(dir.path().join("pending.sqlite3")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        db.busy_timeout(Duration::ZERO).unwrap();
        assert!(release(&db, id, "original").is_err());
        preserve_transition(&db, dir.path(), id, "original", Transition::Release).unwrap();
        writer.execute_batch("ROLLBACK").unwrap();
        let (recovered, actual) = claim_next(&db, dir.path(), "new", None).unwrap().unwrap();
        assert_eq!(recovered, id);
        assert_eq!(actual.body["event_id"], "leased");
        assert!(files_with_prefix(dir.path(), OUTCOME).unwrap().is_empty());
    }

    #[test]
    fn accepted_and_rejected_outcomes_are_reconciled_without_resending() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let accepted = enqueue(&db, &entry("accepted"), Some("a")).unwrap();
        let rejected = enqueue(&db, &entry("rejected"), Some("b")).unwrap();
        preserve_transition(&db, dir.path(), accepted, "a", Transition::Accepted).unwrap();
        preserve_transition(&db, dir.path(), rejected, "b", Transition::Rejected(Some(422))).unwrap();
        assert!(claim_next(&db, dir.path(), "worker", None).unwrap().is_none());
        let state: String = db
            .query_row("SELECT state FROM outbox WHERE id=?1", [rejected], |r| r.get(0))
            .unwrap();
        let status: i64 = db
            .query_row(
                "SELECT status FROM outbox_rejections WHERE id=?1",
                [rejected],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!((state.as_str(), status), ("rejected", 422));
        assert_eq!(
            db.query_row("SELECT count(*) FROM outbox WHERE id=?1", [accepted], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn corrupted_spill_fails_closed_and_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let lock = open_lock(dir.path(), "import.lock").unwrap();
        lock.lock().unwrap();
        defer(&db, dir.path(), "private");
        drop(lock);
        let name = files_with_prefix(dir.path(), FAST).unwrap().pop().unwrap();
        fs::write(dir.path().join(&name), b"corrupted").unwrap();
        assert!(claim_next(&db, dir.path(), "worker", None).is_err());
        assert!(dir.path().join(&name).exists());
        assert!(validate_sidecars(dir.path()).is_err());
    }

    #[test]
    fn stale_outcome_cannot_release_a_new_owner() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let id = enqueue(&db, &entry("protected"), Some("old")).unwrap();
        preserve_transition(&db, dir.path(), id, "old", Transition::Release).unwrap();
        db.execute("UPDATE outbox SET owner='new' WHERE id=?1", [id])
            .unwrap();
        assert!(claim_next(&db, dir.path(), "other", None).unwrap().is_none());
        let owner: String = db
            .query_row("SELECT owner FROM outbox WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(owner, "new");
    }

    #[test]
    fn concurrent_spills_preserve_each_writers_order_without_loss() {
        use std::collections::HashSet;
        use std::sync::Barrier;

        const WORKERS: usize = 8;
        const EACH: usize = 16;
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let lock = open_lock(dir.path(), "import.lock").unwrap();
        lock.lock().unwrap();
        let barrier = Barrier::new(WORKERS);
        std::thread::scope(|scope| {
            let mut threads = Vec::new();
            for worker in 0..WORKERS {
                let path = dir.path();
                let barrier = &barrier;
                threads.push(scope.spawn(move || {
                    let db = open_outbox_at(path, None).unwrap();
                    barrier.wait();
                    for item in 0..EACH {
                        defer(&db, path, &format!("{worker}:{item}"));
                    }
                }));
            }
            for thread in threads {
                thread.join().unwrap();
            }
        });
        drop(lock);
        let mut next = [0usize; WORKERS];
        let mut seen = HashSet::new();
        while let Some((id, value)) = claim_next(&db, dir.path(), "replay", None).unwrap() {
            let event = value.body["event_id"].as_str().unwrap();
            let (worker, item) = event.split_once(':').unwrap();
            let (worker, item) = (worker.parse::<usize>().unwrap(), item.parse::<usize>().unwrap());
            assert_eq!(item, next[worker]);
            next[worker] += 1;
            assert!(seen.insert(event.to_owned()));
            finish(&db, id, "replay", false).unwrap();
        }
        assert_eq!(seen.len(), WORKERS * EACH);
        assert_eq!(next, [EACH; WORKERS]);
    }
}
