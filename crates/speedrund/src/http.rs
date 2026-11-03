//! Local HTTP API. Bound to loopback. Every route except /v1/health needs a
//! bearer token from the data directory: the full token for everything, the
//! agent token for the six routes an agent tool may use, or the display token
//! for the read-only live routes, which never expose prompts.

use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration as StdDuration;

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{self, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use speedrun_core::Claimant;
use speedrun_core::api::{
    ActiveRunResponse, AppendRequest, BackupRequest, CategoriesResponse, ErrorBody, ProjectAppendRequest,
    PromptRequest,
};
use speedrun_core::commons::LinkClaim;
use tokio::sync::watch;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::health::Health;
use crate::provenance::admit;
use crate::service::{Appended, Error, Service};
use speedrun_verify::Verifier;

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<Mutex<Service>>,
    pub token: Arc<String>,
    pub display_token: Arc<String>,
    pub agent_token: Arc<String>,
    /// What this daemon tells a client about itself, and how it proves it.
    pub health: Arc<Health>,
    pub verifier: Arc<Verifier>,
    /// Bumped after every successful write; live streams wake on it.
    pub changes: Arc<watch::Sender<u64>>,
    pub live_hub: Arc<LiveHub>,
    /// The daemon's stop signal: live streams end on it, and `/v1/shutdown`
    /// asks it to start. The channel stays inside `Shutdown`.
    pub shutdown: crate::shutdown::Shutdown,
}

/// Live streams resend a snapshot at least this often, so displays
/// self-correct even if they missed something.
const LIVE_REFRESH: StdDuration = StdDuration::from_secs(60);

/// Header the client sets on a write it is replaying from its spool.
const REPLAY_HEADER: &str = "x-speedrun-replay";

/// A spooled write is exempt from the last-event clock bound; nothing in the
/// body can claim that exemption.
fn is_replay(headers: &HeaderMap) -> bool {
    headers.contains_key(REPLAY_HEADER)
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            Error::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Error::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            Error::NotFound(m) => (StatusCode::NOT_FOUND, format!("not found: {m}")),
            Error::Conflict(m) => (StatusCode::CONFLICT, m),
            Error::Internal(e) => {
                tracing::error!("internal error: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (status, Json(ErrorBody { error: msg })).into_response()
    }
}

/// Run a service call on the blocking pool; SQLite I/O must not stall the runtime.
async fn blocking<T, F>(state: &AppState, f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce(&mut Service) -> Result<T, Error> + Send + 'static,
{
    let service = state.service.clone();
    tokio::task::spawn_blocking(move || {
        let mut guard = service.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard)
    })
    .await
    .map_err(|e| Error::Internal(e.into()))?
}

/// Notify from the blocking task itself, even if the requesting client disconnects.
async fn blocking_write<T, F>(state: &AppState, f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce(&mut Service) -> Result<T, Error> + Send + 'static,
{
    let changes = state.changes.clone();
    blocking(state, move |service| {
        let result = f(service);
        changes.send_modify(|n| *n = n.wrapping_add(1));
        result
    })
    .await
}

async fn call<T, F>(state: &AppState, f: F) -> Result<Json<T>, Error>
where
    T: Serialize + Send + 'static,
    F: FnOnce(&mut Service) -> Result<T, Error> + Send + 'static,
{
    blocking(state, f).await.map(Json)
}

pub fn router(state: AppState, cors_origins: &[String]) -> Router {
    let display = Router::new()
        .route("/v1/live", get(live))
        .route("/v1/live/stream", get(live_stream))
        .route_layer(middleware::from_fn_with_state(state.clone(), display_auth));
    let categories = Router::new()
        .route("/v1/categories", get(categories))
        .route_layer(middleware::from_fn_with_state(state.clone(), categories_auth));
    let mut readable = display.merge(categories);
    if !cors_origins.is_empty() {
        let origins: Vec<HeaderValue> = cors_origins.iter().filter_map(|o| o.parse().ok()).collect();
        readable = readable.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET])
                .allow_headers([header::AUTHORIZATION]),
        );
    }
    let participant = Router::new()
        .route("/v1/events", post(append))
        .route("/v1/projects/{project_id}/active-run", get(active_run))
        .route("/v1/projects/{project_id}/events", post(append_to_project))
        .route("/v1/runs/{run_id}", get(run))
        .route("/v1/runs/{run_id}/events", get(events))
        .route("/v1/verifications/{event_id}", get(verification_status))
        .route_layer(middleware::from_fn_with_state(state.clone(), participant_auth));
    let api = Router::new()
        .route("/v1/projects/{project_id}/runs", get(project_history))
        .route(
            "/v1/runs/{run_id}/local-tests",
            post(crate::local_test::run).get(local_test_attempts),
        )
        .route("/v1/device", get(device))
        .route("/v1/link-claims", post(link_claim))
        .route("/v1/prompts", post(record_prompt))
        .route("/v1/prompts/{event_id}", get(prompt_content).delete(erase_prompt))
        .route("/v1/device/rotate", post(rotate_device))
        .route("/v1/shutdown", post(shutdown_daemon))
        .route("/v1/backup", post(backup))
        .route("/v1/runs/{run_id}/verify", get(verify))
        .route("/v1/runs/{run_id}/export", get(export))
        .route("/v1/runs/{run_id}/bundle", get(bundle))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .route("/v1/health", get(health))
        .merge(api)
        .merge(participant)
        .merge(readable)
        .with_state(state)
}

#[derive(Deserialize)]
struct HealthQuery {
    /// A fresh random value from the client; its presence makes this a proof request.
    nonce: Option<String>,
}

/// `/v1/health`: the daemon's identity, and proof of its token when asked.
/// Never the token itself.
async fn health(State(s): State<AppState>, Query(q): Query<HealthQuery>) -> impl IntoResponse {
    Json(s.health.response(s.token.as_str(), q.nonce.as_deref()))
}

impl AppState {
    fn token_policy(&self) -> TokenPolicy<'_> {
        TokenPolicy {
            full: self.token.as_str(),
            display: self.display_token.as_str(),
            agent: self.agent_token.as_str(),
        }
    }
}

/// Constant-time comparison so the token cannot be guessed byte by byte.
fn token_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A secret and the channel that carried it.
enum Credential<'a> {
    /// An `Authorization: Bearer ...` header.
    Header(&'a str),
    /// A `?token=...` query parameter.
    Query(&'a str),
}

fn bearer(req: &Request) -> Option<&str> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

fn query_token(req: &Request) -> Option<&str> {
    req.uri()
        .query()?
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
}

/// The first credential the request offers: a bearer header, else `?token=`.
fn credential(req: &Request) -> Option<Credential<'_>> {
    bearer(req)
        .map(Credential::Header)
        .or_else(|| query_token(req).map(Credential::Query))
}

/// The only place that decides which token is acceptable in which channel.
struct TokenPolicy<'a> {
    full: &'a str,
    display: &'a str,
    agent: &'a str,
}

impl TokenPolicy<'_> {
    fn is_full(&self, candidate: &str) -> bool {
        token_eq(candidate.as_bytes(), self.full.as_bytes())
    }

    fn is_display(&self, candidate: &str) -> bool {
        token_eq(candidate.as_bytes(), self.display.as_bytes())
    }

    fn is_agent(&self, candidate: &str) -> bool {
        token_eq(candidate.as_bytes(), self.agent.as_bytes())
    }

    /// Full access is header-only: the full token must never be written into a URL.
    fn allows_full_access(&self, presented: Option<Credential<'_>>) -> bool {
        matches!(presented, Some(Credential::Header(token)) if self.is_full(token))
    }

    /// Display routes take either read token in the header, but only the
    /// display token as `?token=` (browsers' EventSource cannot set headers).
    /// The agent token is not a read token: it stays out of `/v1/live`.
    fn allows_display(&self, presented: Option<Credential<'_>>) -> bool {
        match presented {
            Some(Credential::Header(token)) => self.is_full(token) || self.is_display(token),
            Some(Credential::Query(token)) => self.is_display(token),
            None => false,
        }
    }

    /// Participant routes take a write token in the header and report which
    /// one it was, since that is what decides a claim's provenance.
    fn participant(&self, presented: Option<Credential<'_>>) -> Option<Claimant> {
        match presented {
            Some(Credential::Header(token)) if self.is_full(token) => Some(Claimant::User),
            Some(Credential::Header(token)) if self.is_agent(token) => Some(Claimant::Agent),
            _ => None,
        }
    }

    /// Categories are readable with the full or agent token in the header,
    /// and with the display token in either channel.
    fn allows_categories(&self, presented: Option<Credential<'_>>) -> bool {
        match presented {
            Some(Credential::Header(token)) => {
                self.is_full(token) || self.is_agent(token) || self.is_display(token)
            }
            Some(Credential::Query(token)) => self.is_display(token),
            None => false,
        }
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorBody {
            error: "missing or invalid bearer token".into(),
        }),
    )
        .into_response()
}

/// Full access: the full token, in the header only.
async fn auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if state.token_policy().allows_full_access(credential(&req)) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

/// Read-only display routes: either token in the header, or the display token
/// as `?token=` (browsers' EventSource cannot set headers).
async fn display_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if state.token_policy().allows_display(credential(&req)) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

/// Participant routes: a write token in the header. The identity it proves is
/// recorded for the write paths that must distinguish the user from an agent.
async fn participant_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    match state.token_policy().participant(credential(&req)) {
        Some(claimant) => {
            req.extensions_mut().insert(claimant);
            next.run(req).await
        }
        None => unauthorized(),
    }
}

/// Categories reads take the full, agent or display token (the last in either channel).
async fn categories_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if state.token_policy().allows_categories(credential(&req)) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

/// Wake live streams after a write.
fn notify(state: &AppState) {
    state.changes.send_modify(|n| *n = n.wrapping_add(1));
}

async fn record_prompt(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PromptRequest>,
) -> impl IntoResponse {
    let from_spool = is_replay(&headers);
    let res = blocking_write(&s, move |svc| {
        svc.check_prompt_occurred_at(&req.event_id, &req.project_id, req.occurred_at, from_spool)?;
        svc.record_prompt(req)
    })
    .await;
    res.map(Json)
}

/// `/v1/backup`: copy the database. Behind the full-token `auth` layer: an
/// agent may not make the daemon write files.
async fn backup(State(s): State<AppState>, Json(req): Json<BackupRequest>) -> impl IntoResponse {
    call(&s, move |svc| svc.backup(&req)).await
}

async fn rotate_device(State(s): State<AppState>) -> impl IntoResponse {
    blocking_write(&s, |svc| svc.rotate_device()).await.map(Json)
}

/// `/v1/device`: the key this daemon signs with now, and its fingerprint.
async fn device(State(s): State<AppState>) -> impl IntoResponse {
    call(&s, |svc| svc.device_key()).await
}

/// `/v1/link-claims`: the device's half of a GitHub link (COMMONS.md §20).
async fn link_claim(State(s): State<AppState>, Json(claim): Json<LinkClaim>) -> impl IntoResponse {
    call(&s, move |svc| svc.sign_link_claim(&claim)).await
}

/// `/v1/shutdown`: stop the daemon by the path a signal takes. The router's
/// full-token `auth` layer runs first, so an agent or display token never
/// reaches here. Answer 202, then let the server drain.
async fn shutdown_daemon(State(s): State<AppState>) -> impl IntoResponse {
    s.shutdown.request();
    StatusCode::ACCEPTED
}

async fn append(
    State(s): State<AppState>,
    Extension(claimant): Extension<Claimant>,
    headers: HeaderMap,
    Json(mut req): Json<AppendRequest>,
) -> Result<Json<speedrun_core::api::AppendResponse>, Error> {
    admit(claimant, &mut req.payload)?;
    let from_spool = is_replay(&headers);
    let res = blocking_write(&s, move |svc| {
        svc.check_occurred_at(&req.event_id, &req.run_id, req.occurred_at, from_spool)?;
        svc.append(req)
    })
    .await;
    finish_append(s, res).await
}

/// Notify, then run any evidence check without holding the service (it may
/// take seconds on the network) and answer with its outcome. The check runs
/// to completion even if the client hangs up.
async fn finish_append(
    s: AppState,
    res: Result<Appended, Error>,
) -> Result<Json<speedrun_core::api::AppendResponse>, Error> {
    let Appended { mut response, job } = res?;
    let Some(job) = job else {
        return Ok(Json(response));
    };
    let task_state = s.clone();
    let task = tokio::spawn(async move {
        let id = job.cause_event_id.clone();
        let claim_id = id.clone();
        if !blocking_write(&task_state, move |svc| svc.start_verification_job(&claim_id)).await? {
            return Ok(None);
        }
        let result = run_verification(task_state.clone(), job).await;
        if result.is_err() {
            let _ = blocking_write(&task_state, move |svc| svc.retry_verification_job(&id)).await;
        }
        result.map(Some)
    });
    let Some((outcome, derived)) = task.await.map_err(|e| Error::Internal(e.into()))?? else {
        let event_id = response.event.event_id.clone();
        response.verification_job = Some(blocking(&s, move |svc| svc.verification_status(&event_id)).await?);
        return Ok(Json(response));
    };
    crate::logging::verification_failed(&outcome);
    response.derived.extend(derived);
    response.verification = Some(outcome.report());
    let event_id = response.event.event_id.clone();
    response.verification_job = Some(blocking(&s, move |svc| svc.verification_status(&event_id)).await?);
    Ok(Json(response))
}

fn verification_slots() -> &'static Arc<tokio::sync::Semaphore> {
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
}

async fn run_verification(
    s: AppState,
    job: speedrun_verify::Job,
) -> Result<(speedrun_verify::Outcome, Vec<speedrun_core::Event>), Error> {
    let _permit = verification_slots()
        .acquire()
        .await
        .map_err(|e| Error::Internal(e.into()))?;
    let (service, verifier, changes) = (s.service.clone(), s.verifier.clone(), s.changes.clone());
    tokio::task::spawn_blocking(move || {
        let mut outcome = verifier.verify(&job);
        let mut guard = service.lock().unwrap_or_else(|p| p.into_inner());
        let result = guard.record_verification_job(&job, &mut outcome);
        changes.send_modify(|n| *n = n.wrapping_add(1));
        Ok((outcome, result?))
    })
    .await
    .map_err(|e| Error::Internal(e.into()))?
}

pub fn start_verification_recovery(s: AppState) {
    tokio::spawn(async move {
        loop {
            match blocking(&s, |svc| svc.take_verification_job()).await {
                Ok(Some(job)) => {
                    notify(&s);
                    let id = job.cause_event_id.clone();
                    if let Err(error) = run_verification(s.clone(), job).await {
                        tracing::error!(?error, "verification job failed");
                        if let Err(error) = blocking(&s, move |svc| svc.retry_verification_job(&id)).await {
                            tracing::error!(?error, "could not reschedule verification job");
                        }
                        tokio::time::sleep(StdDuration::from_secs(1)).await;
                    }
                    notify(&s);
                }
                Ok(None) => tokio::time::sleep(StdDuration::from_secs(1)).await,
                Err(error) => {
                    tracing::error!(?error, "could not read verification jobs");
                    tokio::time::sleep(StdDuration::from_secs(1)).await;
                }
            }
        }
    });
}

async fn verification_status(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    call(&s, move |svc| svc.verification_status(&id)).await
}

async fn local_test_attempts(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    call(&s, move |svc| svc.local_test_attempts(&id)).await
}

async fn prompt_content(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    call(&s, move |svc| svc.prompt_text(&id)).await
}

async fn erase_prompt(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    call(&s, move |svc| svc.erase_prompt(&id)).await
}

async fn project_history(State(s): State<AppState>, Path(project_id): Path<String>) -> impl IntoResponse {
    call(&s, move |svc| svc.project_history(&project_id)).await
}

async fn active_run(State(s): State<AppState>, Path(project_id): Path<String>) -> impl IntoResponse {
    call(&s, move |svc| {
        Ok(ActiveRunResponse {
            run: svc.active_run(&project_id)?,
        })
    })
    .await
}

async fn append_to_project(
    State(s): State<AppState>,
    Extension(claimant): Extension<Claimant>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(mut req): Json<ProjectAppendRequest>,
) -> Result<Json<speedrun_core::api::AppendResponse>, Error> {
    admit(claimant, &mut req.payload)?;
    let from_spool = is_replay(&headers);
    let res = blocking_write(&s, move |svc| svc.append_to_project(&project_id, req, from_spool)).await;
    finish_append(s, res).await
}

#[derive(Deserialize)]
struct LiveQuery {
    /// Only runs with activity in the last this-many hours. Default one week.
    #[serde(default = "default_window")]
    since_hours: u32,
}

fn default_window() -> u32 {
    24 * 7
}

async fn live(State(s): State<AppState>, Query(q): Query<LiveQuery>) -> impl IntoResponse {
    let window = chrono::Duration::hours(q.since_hours.into());
    call(&s, move |svc| svc.live(window)).await
}

/// Resolve when the daemon starts shutting down. A live stream ends on it, so
/// an open display cannot outlast the shutdown deadline.
async fn shutdown_started(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// One producer serves all active streams and disappears after the last one.
#[derive(Default)]
pub struct LiveHub {
    producer: Mutex<Weak<LiveProducer>>,
    #[cfg(test)]
    computations: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    refresh_period: Mutex<Option<StdDuration>>,
}

type LiveMessage = Option<(u64, Result<Arc<speedrun_core::live::LiveSnapshot>, ()>)>;
struct LiveProducer {
    output: watch::Sender<LiveMessage>,
    refresh: watch::Sender<u64>,
}

impl LiveHub {
    fn subscribe(self: &Arc<Self>, state: AppState) -> (watch::Receiver<LiveMessage>, u64) {
        let mut slot = self.producer.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(producer) = slot.upgrade() {
            let mut rx = producer.output.subscribe();
            rx.borrow_and_update();
            producer.refresh.send_modify(|n| *n = n.wrapping_add(1));
            let revision = *producer.refresh.borrow();
            return (rx, revision);
        }
        let (output, rx) = watch::channel(None);
        let (refresh, mut refresh_rx) = watch::channel(0u64);
        let producer = Arc::new(LiveProducer { output, refresh });
        *slot = Arc::downgrade(&producer);
        let hub = self.clone();
        tokio::spawn(async move {
            let mut changes = state.changes.subscribe();
            let mut shutdown = state.shutdown.watch();
            loop {
                let refresh_revision = *refresh_rx.borrow_and_update();
                #[cfg(test)]
                hub.computations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let message = tokio::select! {
                    () = shutdown_started(&mut shutdown) => break,
                    result = blocking(&state, |svc| svc.live_all()) => result.map(Arc::new).map_err(|_| ()),
                };
                {
                    let mut slot = hub.producer.lock().unwrap_or_else(|p| p.into_inner());
                    if producer.output.send(Some((refresh_revision, message))).is_err() {
                        *slot = Weak::new();
                        break;
                    }
                }
                #[cfg(test)]
                let refresh_period = hub.refresh_period.lock().unwrap().unwrap_or(LIVE_REFRESH);
                #[cfg(not(test))]
                let refresh_period = LIVE_REFRESH;
                tokio::select! {
                    result = changes.changed() => if result.is_err() { break },
                    result = refresh_rx.changed() => if result.is_err() { break },
                    () = shutdown_started(&mut shutdown) => break,
                    () = producer.output.closed() => {
                        let mut slot = hub.producer.lock().unwrap_or_else(|p| p.into_inner());
                        if producer.output.receiver_count() == 0 {
                            *slot = Weak::new();
                            break;
                        }
                    },
                    _ = tokio::time::sleep(refresh_period) => {}
                }
            }
        });
        (rx, 0)
    }
}

/// Each message replaces the display state. Slow receivers coalesce updates.
async fn live_stream(
    State(s): State<AppState>,
    Query(q): Query<LiveQuery>,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>> {
    let (rx, minimum_revision) = s.live_hub.subscribe(s.clone());
    let hours = q.since_hours;
    let stream = futures_util::stream::unfold(rx, move |mut rx| async move {
        let message = loop {
            if rx.changed().await.is_err() {
                return None;
            }
            if let Some((revision, message)) = rx.borrow_and_update().clone()
                && revision >= minimum_revision
            {
                break message;
            }
        };
        let event = match message {
            Ok(snapshot) => {
                let mut snapshot = (*snapshot).clone();
                let window = chrono::Duration::hours(hours.into());
                if let Some(cutoff) = snapshot.as_of.checked_sub_signed(window) {
                    snapshot.runs.retain(|run| run.last_event_at >= cutoff);
                }
                sse::Event::default()
                    .event("snapshot")
                    .id(snapshot.cursor.to_string())
                    .json_data(&snapshot)
                    .unwrap_or_else(|_| sse::Event::default().event("error").data("serialize failed"))
            }
            _ => sse::Event::default()
                .event("error")
                .data("snapshot failed; retrying on next change"),
        };
        Some((Ok(event), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn run(State(s): State<AppState>, Path(run_id): Path<String>) -> impl IntoResponse {
    call(&s, move |svc| svc.run(&run_id)).await
}

async fn events(State(s): State<AppState>, Path(run_id): Path<String>) -> impl IntoResponse {
    call(&s, move |svc| svc.events(&run_id)).await
}

async fn verify(State(s): State<AppState>, Path(run_id): Path<String>) -> impl IntoResponse {
    call(&s, move |svc| svc.verify_chain(&run_id)).await
}

#[derive(Deserialize)]
struct ExportQuery {
    /// Only tier 2 is defined; anything else is refused, not defaulted.
    tier: u8,
}

async fn export(
    State(s): State<AppState>,
    Path(run_id): Path<String>,
    Query(q): Query<ExportQuery>,
) -> impl IntoResponse {
    call(&s, move |svc| svc.export(&run_id, q.tier)).await
}

#[derive(Deserialize)]
struct BundleQuery {
    /// 0, 1 or 2; anything else is refused, not defaulted.
    tier: u8,
}

async fn bundle(
    State(s): State<AppState>,
    Path(run_id): Path<String>,
    Query(q): Query<BundleQuery>,
) -> impl IntoResponse {
    call(&s, move |svc| svc.bundle(&run_id, q.tier)).await
}

async fn categories(State(s): State<AppState>) -> impl IntoResponse {
    call(&s, |svc| {
        Ok(CategoriesResponse {
            categories: svc.categories().to_vec(),
        })
    })
    .await
}

#[cfg(test)]
mod live_hub_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Deliberately block the service to test cancellation and shutdown.
    async fn subscribers_share_revisions_and_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            crate::store::Store::open(&dir.path().join("db"), &dir.path().join("device.key")).unwrap();
        let state = AppState {
            service: Arc::new(Mutex::new(Service::new(
                store,
                speedrun_core::category::builtin().unwrap(),
                "test".into(),
            ))),
            token: Arc::new("test".into()),
            display_token: Arc::new("test".into()),
            agent_token: Arc::new("test".into()),
            health: Arc::new(Health::resolve(dir.path()).unwrap()),
            verifier: Arc::new(Verifier::default()),
            changes: Arc::new(watch::channel(0).0),
            live_hub: Arc::default(),
            shutdown: crate::shutdown::Shutdown::install().unwrap(),
        };
        let mut receivers: Vec<_> = (0..8)
            .map(|_| state.live_hub.subscribe(state.clone()).0)
            .collect();
        for receiver in &mut receivers {
            receiver.changed().await.unwrap();
            receiver.borrow_and_update();
        }
        let before = state.live_hub.computations.load(Ordering::SeqCst);
        notify(&state);
        let mut first = None;
        for receiver in &mut receivers {
            receiver.changed().await.unwrap();
            let message = receiver.borrow_and_update().clone().unwrap().1.unwrap();
            if let Some(first) = &first {
                assert!(Arc::ptr_eq(first, &message));
            } else {
                first = Some(message);
            }
        }
        assert_eq!(state.live_hub.computations.load(Ordering::SeqCst), before + 1);
        // The periodic refresh advances as_of even without a write.
        *state.live_hub.refresh_period.lock().unwrap() = Some(StdDuration::from_millis(20));
        notify(&state);
        for receiver in &mut receivers {
            receiver.changed().await.unwrap();
            receiver.borrow_and_update();
        }
        let previous = receivers[0].borrow().as_ref().unwrap().1.as_ref().unwrap().as_of;
        tokio::time::timeout(StdDuration::from_secs(2), receivers[0].changed())
            .await
            .unwrap()
            .unwrap();
        let next = receivers[0]
            .borrow_and_update()
            .as_ref()
            .unwrap()
            .1
            .as_ref()
            .unwrap()
            .as_of;
        assert!(next > previous);
        // Restore a long interval and synchronize the other receivers.
        *state.live_hub.refresh_period.lock().unwrap() = None;
        for receiver in &mut receivers[1..] {
            receiver.borrow_and_update();
        }
        // Poll the request until its blocking task is queued, then cancel it
        // while the service lock is held. Its eventual commit still notifies.
        let mut changes = state.changes.subscribe();
        let guard = state.service.lock().unwrap();
        let mut write = Box::pin(blocking_write(&state, |svc| svc.rotate_device()));
        assert!(matches!(
            futures_util::poll!(write.as_mut()),
            std::task::Poll::Pending
        ));
        drop(write);
        drop(guard);
        tokio::time::timeout(StdDuration::from_secs(2), changes.changed())
            .await
            .unwrap()
            .unwrap();
        for receiver in &mut receivers {
            receiver.changed().await.unwrap();
            receiver.borrow_and_update();
        }
        // Shutdown must close the streams even if a computation is waiting on
        // the service mutex; the blocking read can finish after shutdown.
        let guard = state.service.lock().unwrap();
        let before = state.live_hub.computations.load(Ordering::SeqCst);
        notify(&state);
        tokio::time::timeout(StdDuration::from_secs(2), async {
            while state.live_hub.computations.load(Ordering::SeqCst) == before {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        state.shutdown.request();
        for receiver in &mut receivers {
            assert!(
                tokio::time::timeout(StdDuration::from_secs(2), receiver.changed())
                    .await
                    .unwrap()
                    .is_err()
            );
        }
        drop(guard);
    }
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Holds the service to control in-flight computations.
    async fn joining_and_reconnecting_during_computation_gets_fresh_output() {
        async fn receive_at_least(rx: &mut watch::Receiver<LiveMessage>, minimum: u64) -> u64 {
            tokio::time::timeout(StdDuration::from_secs(2), async {
                loop {
                    rx.changed().await.unwrap();
                    if let Some((revision, Ok(_))) = rx.borrow_and_update().as_ref()
                        && *revision >= minimum
                    {
                        return *revision;
                    }
                }
            })
            .await
            .unwrap()
        }
        async fn wait_for_computation(hub: &LiveHub, before: usize) {
            tokio::time::timeout(StdDuration::from_secs(2), async {
                while hub.computations.load(Ordering::SeqCst) == before {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let store =
            crate::store::Store::open(&dir.path().join("db"), &dir.path().join("device.key")).unwrap();
        let state = AppState {
            service: Arc::new(Mutex::new(Service::new(
                store,
                speedrun_core::category::builtin().unwrap(),
                "test".into(),
            ))),
            token: Arc::new("test".into()),
            display_token: Arc::new("test".into()),
            agent_token: Arc::new("test".into()),
            health: Arc::new(Health::resolve(dir.path()).unwrap()),
            verifier: Arc::new(Verifier::default()),
            changes: Arc::new(watch::channel(0).0),
            live_hub: Arc::default(),
            shutdown: crate::shutdown::Shutdown::install().unwrap(),
        };
        let (mut first, minimum) = state.live_hub.subscribe(state.clone());
        receive_at_least(&mut first, minimum).await;
        let guard = state.service.lock().unwrap();
        let before = state.live_hub.computations.load(Ordering::SeqCst);
        notify(&state);
        wait_for_computation(&state.live_hub, before).await;
        let (mut joining, join_revision) = state.live_hub.subscribe(state.clone());
        drop(guard);
        assert!(receive_at_least(&mut joining, join_revision).await >= join_revision);
        drop(first);
        let guard = state.service.lock().unwrap();
        let before = state.live_hub.computations.load(Ordering::SeqCst);
        notify(&state);
        wait_for_computation(&state.live_hub, before).await;
        drop(joining);
        let (mut reconnect, reconnect_revision) = state.live_hub.subscribe(state.clone());
        drop(guard);
        assert!(receive_at_least(&mut reconnect, reconnect_revision).await >= reconnect_revision);
        drop(reconnect);
        let (mut again, again_revision) = state.live_hub.subscribe(state.clone());
        assert!(receive_at_least(&mut again, again_revision).await >= again_revision);
        state.shutdown.request();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_sse_subscribers_share_one_change() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        async fn snapshot(
            reader: &mut BufReader<tokio::net::TcpStream>,
        ) -> speedrun_core::live::LiveSnapshot {
            tokio::time::timeout(StdDuration::from_secs(5), async {
                let mut line = String::new();
                loop {
                    line.clear();
                    assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
                    if let Some(json) = line.strip_prefix("data: ") {
                        return serde_json::from_str(json.trim()).unwrap();
                    }
                }
            })
            .await
            .unwrap()
        }
        let dir = tempfile::tempdir().unwrap();
        let store =
            crate::store::Store::open(&dir.path().join("db"), &dir.path().join("device.key")).unwrap();
        let mut service = Service::new(store, speedrun_core::category::builtin().unwrap(), "test".into());
        for (name, age) in [("recent", 0), ("hours", 2), ("days", 48)] {
            service
                .record_prompt(PromptRequest {
                    event_id: format!("{name}-prompt"),
                    project_id: name.into(),
                    project_root: None,
                    category_id: "any-percent".into(),
                    category_version: None,
                    session_id: "test".into(),
                    host: "test".into(),
                    model: None,
                    content: "test".into(),
                    metadata_only: None,
                    occurred_at: Some(chrono::Utc::now() - chrono::Duration::hours(age)),
                    starts_turn: true,
                })
                .unwrap();
        }
        // Age recorded timestamps in this isolated display fixture; the live
        // window is based on recording time, not occurrence time.
        let conn = rusqlite::Connection::open(dir.path().join("db")).unwrap();
        conn.execute_batch("DROP TRIGGER events_no_update").unwrap();
        for (project, age) in [("hours", 2), ("days", 48)] {
            let at = (chrono::Utc::now() - chrono::Duration::hours(age)).to_rfc3339();
            conn.execute("UPDATE events SET recorded_at = ?1, body = json_set(body, '$.recorded_at', ?1) WHERE run_id IN (SELECT run_id FROM runs WHERE project_id = ?2)", rusqlite::params![at, project]).unwrap();
        }
        drop(conn);
        let state = AppState {
            service: Arc::new(Mutex::new(service)),
            token: Arc::new("test".into()),
            display_token: Arc::new("test".into()),
            agent_token: Arc::new("test".into()),
            health: Arc::new(Health::resolve(dir.path()).unwrap()),
            verifier: Arc::new(Verifier::default()),
            changes: Arc::new(watch::channel(0).0),
            live_hub: Arc::default(),
            shutdown: crate::shutdown::Shutdown::install().unwrap(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_state = state.clone();
        let server =
            tokio::spawn(async move { axum::serve(listener, router(server_state, &[])).await.unwrap() });
        let mut readers = Vec::new();
        let mut latest = None;
        let mut cursor = 0;
        for (hours, expected) in [(0, 0), (1, 1), (24, 2), (u32::MAX, 3)] {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            stream.write_all(format!("GET /v1/live/stream?since_hours={hours} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test\r\n\r\n").as_bytes()).await.unwrap();
            let mut reader = BufReader::new(stream);
            let initial = snapshot(&mut reader).await;
            assert_eq!(initial.runs.len(), expected);
            cursor = initial.cursor;
            latest = Some(initial.as_of);
            readers.push(reader);
        }
        let latest = latest.unwrap();
        for reader in &mut readers[..3] {
            while snapshot(reader).await.as_of < latest {}
        }
        let before = state.live_hub.computations.load(Ordering::SeqCst);
        notify(&state);
        let mut as_of = None;
        for reader in &mut readers {
            let next = snapshot(reader).await;
            assert_eq!(next.cursor, cursor);
            if let Some(expected) = as_of {
                assert_eq!(next.as_of, expected);
            } else {
                as_of = Some(next.as_of);
            }
        }
        assert_eq!(state.live_hub.computations.load(Ordering::SeqCst), before + 1);

        let run_id = state
            .service
            .lock()
            .unwrap()
            .active_run("recent")
            .unwrap()
            .unwrap()
            .run_id;
        let appended = blocking_write(&state, move |svc| {
            svc.append(AppendRequest {
                event_id: "verify-claim".into(),
                run_id,
                occurred_at: None,
                payload: speedrun_core::EventPayload::MilestoneReached {
                    milestone_id: "happy-path-demo".into(),
                    trust: speedrun_core::Trust::SelfDeclared,
                    evidence: vec![speedrun_core::Evidence::Url {
                        url: "https://example.com".into(),
                    }],
                    claimant: Claimant::User,
                },
            })
        })
        .await
        .unwrap();
        let job = appended.job.unwrap();
        let pending = snapshot(&mut readers[1]).await;
        let cursor = pending.cursor;
        let id = job.cause_event_id.clone();
        assert!(
            blocking_write(&state, move |svc| svc.start_verification_job(&id))
                .await
                .unwrap()
        );
        let checking = snapshot(&mut readers[1]).await;
        assert_eq!(checking.cursor, cursor);
        let state_of = |snapshot: &speedrun_core::live::LiveSnapshot| {
            snapshot
                .runs
                .iter()
                .flat_map(|run| &run.milestones)
                .filter_map(|milestone| milestone.verification.as_ref())
                .find(|verification| verification.cause_event_id == "verify-claim")
                .unwrap()
                .state
        };
        assert_eq!(
            state_of(&checking),
            speedrun_core::api::VerificationJobState::Checking
        );
        blocking_write(&state, move |svc| {
            let mut outcome = speedrun_verify::Outcome {
                problems: vec!["test failure".into()],
                ..Default::default()
            };
            svc.record_verification_job(&job, &mut outcome)
        })
        .await
        .unwrap();
        let failed = snapshot(&mut readers[1]).await;
        assert_eq!(failed.cursor, cursor);
        assert_eq!(
            state_of(&failed),
            speedrun_core::api::VerificationJobState::Failed
        );
        state.shutdown.request();
        server.abort();
    }
}
