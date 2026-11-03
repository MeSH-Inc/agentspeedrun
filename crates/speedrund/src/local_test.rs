//! Execute an explicitly requested local test and retain only daemon-observed results.
use crate::{http::AppState, service::Error};
use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
pub use speedrun_core::api::{LocalTestRequest, TestObservation, TestStatus};
use std::{
    path::{Path as FsPath, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

const TIMEOUT: Duration = Duration::from_secs(60);
const OUTPUT_LIMIT: usize = 64 * 1024;
static EXECUTIONS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

#[derive(Deserialize)]
struct TrustedProject {
    project_id: String,
    root: PathBuf,
}

type AttemptWriter = Box<dyn FnMut(&TestObservation) -> Result<(), Error> + Send>;

struct AttemptGuard {
    observation: TestObservation,
    write: AttemptWriter,
    finished: bool,
}

impl AttemptGuard {
    fn finish(&mut self, status: TestStatus) -> Result<(), Error> {
        self.observation.status = status;
        self.observation.observed_at = chrono::Utc::now();
        (self.write)(&self.observation)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.observation.status = TestStatus::Interrupted;
            self.observation.observed_at = chrono::Utc::now();
            if let Err(error) = (self.write)(&self.observation) {
                tracing::error!("could not persist interrupted local test: {error:?}");
            }
        }
    }
}

#[cfg(unix)]
struct ProcessGroupGuard(Option<rustix::process::Pid>);

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(group) = self.0.take() {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}

async fn execute(root: &FsPath, argv: &[String], limit: Duration) -> Result<(TestStatus, Vec<u8>), Error> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| Error::BadRequest(format!("cannot start command: {e}")))?;
    #[cfg(unix)]
    let mut process_group = ProcessGroupGuard(
        child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw),
    );
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    async fn drain(mut input: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
        let mut kept = Vec::new();
        let mut buf = [0; 8192];
        loop {
            let n = input.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&buf[..n.min(OUTPUT_LIMIT.saturating_sub(kept.len()))]);
        }
        Ok(kept)
    }
    let result = tokio::time::timeout(limit, async {
        let (status, out, err) = tokio::join!(child.wait(), drain(stdout), drain(stderr));
        let mut output = out?;
        output.extend(err?.into_iter().take(OUTPUT_LIMIT.saturating_sub(output.len())));
        let status = status?;
        let result = if status.success() {
            TestStatus::Success
        } else {
            TestStatus::Failed {
                exit_code: status.code(),
            }
        };
        Ok::<_, std::io::Error>((result, output))
    })
    .await;
    let result = match result {
        Ok(result) => result,
        Err(_) => {
            #[cfg(unix)]
            {
                drop(process_group);
            }
            let _ = child.kill().await;
            return Ok((TestStatus::TimedOut, Vec::new()));
        }
    };
    #[cfg(unix)]
    {
        process_group.0 = None;
    }
    Ok(result.unwrap_or((TestStatus::Failed { exit_code: None }, Vec::new())))
}

async fn git(root: &FsPath, args: &[&str]) -> Result<String, Error> {
    let argv = std::iter::once("git".to_string())
        .chain(args.iter().map(|s| s.to_string()))
        .collect::<Vec<_>>();
    let (ok, output) = execute(root, &argv, Duration::from_secs(10)).await?;
    if ok != TestStatus::Success {
        return Err(Error::BadRequest("could not inspect git repository".into()));
    }
    String::from_utf8(output).map_err(|_| Error::BadRequest("invalid git output".into()))
}

async fn clean_head(root: &FsPath) -> Result<String, Error> {
    let status = git(root, &["status", "--porcelain", "--untracked-files=all"]).await?;
    if !status.is_empty() {
        return Err(Error::BadRequest("working tree must be clean".into()));
    }
    Ok(git(root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .await?
        .trim()
        .to_owned())
}

pub async fn run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(req): Json<LocalTestRequest>,
) -> Result<Json<TestObservation>, Error> {
    if req.argv.is_empty()
        || req.argv.first().is_some_and(String::is_empty)
        || req.argv.len() > 128
        || req.argv.iter().any(|s| s.len() > 8192 || s.contains('\0'))
    {
        return Err(Error::BadRequest("provide a nonempty bounded argv".into()));
    }
    let view = {
        let service = state.service.lock().unwrap_or_else(|p| p.into_inner());
        service.run(&run_id)?
    };
    if view.closed {
        return Err(Error::BadRequest("run is closed".into()));
    }
    let root = view
        .project_root
        .ok_or_else(|| Error::BadRequest("run has no project root".into()))?;
    let root =
        std::fs::canonicalize(root).map_err(|_| Error::BadRequest("project root is unavailable".into()))?;
    let trusted: Vec<TrustedProject> =
        std::fs::read(speedrun_core::paths::data_dir().join("trusted-projects.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
    if !trusted
        .iter()
        .any(|p| p.project_id == view.project_id && p.root == root)
    {
        return Err(Error::Forbidden("project root is not trusted".into()));
    }
    let _permit = EXECUTIONS
        .try_acquire()
        .map_err(|_| Error::Conflict("local test execution limit reached".into()))?;
    let head = clean_head(&root).await?;
    // The command is executed exactly as requested, but arguments may contain
    // arbitrary secrets. Keep them out of observations and their history.
    let argv = req.argv;
    let observation = TestObservation {
        id: speedrun_core::new_id(),
        run_id,
        project_id: view.project_id,
        head,
        argv: argv.iter().map(|_| "[argument omitted]".to_owned()).collect(),
        status: TestStatus::Running,
        observed_at: chrono::Utc::now(),
        observer: "speedrund/local-test@1".into(),
        scope: speedrun_core::api::VerificationScope::Local,
    };
    let service = state.service.clone();
    let mut guard = AttemptGuard {
        observation,
        write: Box::new(move |attempt| {
            service
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .record_local_test_attempt(attempt)
        }),
        finished: true,
    };
    (guard.write)(&guard.observation)?;
    guard.finished = false;
    let (mut status, _output) = match execute(&root, &argv, TIMEOUT).await {
        Ok(result) => result,
        Err(error) => {
            guard.finish(TestStatus::Failed { exit_code: None })?;
            return Err(error);
        }
    };
    if status == TestStatus::Success
        && !matches!(clean_head(&root).await, Ok(ref after) if *after == guard.observation.head)
    {
        status = TestStatus::ContextChanged;
    }
    guard.finish(status)?;
    let observation = guard.observation.clone();
    match &observation.status {
        TestStatus::Success => {}
        TestStatus::Running | TestStatus::Interrupted => unreachable!("terminal status expected"),
        TestStatus::Failed { .. } => {
            return Err(Error::BadRequest(format!(
                "test command failed (attempt {}); command output is not retained",
                observation.id
            )));
        }
        TestStatus::TimedOut => {
            return Err(Error::BadRequest(format!(
                "test command timed out (attempt {})",
                observation.id
            )));
        }
        TestStatus::ContextChanged => {
            return Err(Error::BadRequest(format!(
                "repository changed during test (attempt {})",
                observation.id
            )));
        }
    }
    Ok(Json(observation))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn argv_is_not_interpreted_by_a_shell_and_output_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        let (ok, output) = execute(
            root.path(),
            &["printf".into(), "%s".into(), "$(touch injected)".into()],
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(ok, TestStatus::Success);
        assert_eq!(output, b"$(touch injected)");
        assert!(!root.path().join("injected").exists());
        let (ok, output) = execute(
            root.path(),
            &["head".into(), "-c".into(), "100000".into(), "/dev/zero".into()],
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(ok, TestStatus::Success);
        assert_eq!(output.len(), OUTPUT_LIMIT);
    }

    #[tokio::test]
    async fn command_timeout_is_enforced() {
        let root = tempfile::tempdir().unwrap();
        let result = execute(
            root.path(),
            &["sleep".into(), "5".into()],
            Duration::from_millis(30),
        )
        .await;
        assert!(matches!(result, Ok((TestStatus::TimedOut, _))));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_descendants() {
        let root = tempfile::tempdir().unwrap();
        let result = execute(
            root.path(),
            &[
                "sh".into(),
                "-c".into(),
                "(sleep 0.3; touch escaped) & wait".into(),
            ],
            Duration::from_millis(50),
        )
        .await;
        assert!(matches!(result, Ok((TestStatus::TimedOut, _))));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!root.path().join("escaped").exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_descendants() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        let task = tokio::spawn(async move {
            execute(
                &path,
                &[
                    "sh".into(),
                    "-c".into(),
                    "(touch started; sleep 0.5; touch escaped) & wait".into(),
                ],
                Duration::from_secs(10),
            )
            .await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !root.path().join("started").exists() {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(!root.path().join("escaped").exists());
    }
    #[test]
    fn dropping_attempt_guard_records_interruption() {
        use std::sync::{Arc, Mutex};
        let writes = Arc::new(Mutex::new(Vec::new()));
        let recorded = writes.clone();
        let guard = AttemptGuard {
            observation: TestObservation {
                id: "id".into(),
                run_id: "run".into(),
                project_id: "project".into(),
                head: "head".into(),
                argv: vec!["true".into()],
                status: TestStatus::Running,
                observed_at: chrono::Utc::now(),
                observer: "test".into(),
                scope: speedrun_core::api::VerificationScope::Local,
            },
            write: Box::new(move |attempt| {
                recorded.lock().unwrap().push(attempt.status.clone());
                Ok(())
            }),
            finished: false,
        };
        drop(guard);
        assert_eq!(*writes.lock().unwrap(), vec![TestStatus::Interrupted]);
    }
}
