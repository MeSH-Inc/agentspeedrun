//! Local HTTP API. Bound to loopback. Every route except /v1/health needs a
//! bearer token from the data directory: the full token for everything, the
//! agent token for the six routes an agent tool may use, or the display token
//! for the read-only live routes, which never expose prompts.

use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock};
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
    let res = call(&s, move |svc| {
        svc.check_prompt_occurred_at(&req.event_id, &req.project_id, req.occurred_at, from_spool)?;
        svc.record_prompt(req)
    })
    .await;
    notify(&s);
    res
}

/// `/v1/backup`: copy the database. Behind the full-token `auth` layer: an
/// agent may not make the daemon write files.
async fn backup(State(s): State<AppState>, Json(req): Json<BackupRequest>) -> impl IntoResponse {
    call(&s, move |svc| svc.backup(&req)).await
}

async fn rotate_device(State(s): State<AppState>) -> impl IntoResponse {
    call(&s, |svc| svc.rotate_device()).await
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
    let res = blocking(&s, move |svc| {
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
    notify(&s);
    let Appended { mut response, job } = res?;
    let Some(job) = job else {
        return Ok(Json(response));
    };
    let task_state = s.clone();
    let task = tokio::spawn(async move {
        let id = job.cause_event_id.clone();
        let claim_id = id.clone();
        if !blocking(&task_state, move |svc| svc.start_verification_job(&claim_id)).await? {
            return Ok(None);
        }
        let result = run_verification(task_state.clone(), job).await;
        if result.is_err() {
            let _ = blocking(&task_state, move |svc| svc.retry_verification_job(&id)).await;
        }
        result.map(Some)
    });
    let Some((outcome, derived)) = task.await.map_err(|e| Error::Internal(e.into()))?? else {
        let event_id = response.event.event_id.clone();
        response.verification_job = Some(blocking(&s, move |svc| svc.verification_status(&event_id)).await?);
        return Ok(Json(response));
    };
    crate::logging::verification_failed(&outcome);
    if !derived.is_empty() {
        notify(&s);
    }
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
    let (service, verifier) = (s.service.clone(), s.verifier.clone());
    tokio::task::spawn_blocking(move || {
        let mut outcome = verifier.verify(&job);
        let mut guard = service.lock().unwrap_or_else(|p| p.into_inner());
        let derived = guard.record_verification_job(&job, &mut outcome)?;
        Ok((outcome, derived))
    })
    .await
    .map_err(|e| Error::Internal(e.into()))?
}

pub fn start_verification_recovery(s: AppState) {
    tokio::spawn(async move {
        loop {
            match blocking(&s, |svc| svc.take_verification_job()).await {
                Ok(Some(job)) => {
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
    let res = blocking(&s, move |svc| svc.append_to_project(&project_id, req, from_spool)).await;
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

/// Server-sent events. Every message is a complete `LiveSnapshot` (event
/// name `snapshot`, id = cursor): clients replace their state, so there is
/// nothing to merge and reconnecting needs no resume logic.
async fn live_stream(
    State(s): State<AppState>,
    Query(q): Query<LiveQuery>,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>> {
    let window = chrono::Duration::hours(q.since_hours.into());
    let rx = s.changes.subscribe();
    let stream = futures_util::stream::unfold((s, rx, true), move |(s, mut rx, first)| async move {
        if !first {
            // Wake on a write, the periodic refresh, or shutdown, whichever comes first.
            let mut shutdown = s.shutdown.watch();
            tokio::select! {
                changed = rx.changed() => if changed.is_err() { return None },
                () = shutdown_started(&mut shutdown) => return None,
                _ = tokio::time::sleep(LIVE_REFRESH) => {}
            }
        }
        rx.borrow_and_update();
        let event = match call(&s, move |svc| svc.live(window)).await {
            Ok(Json(snap)) => sse::Event::default()
                .event("snapshot")
                .id(snap.cursor.to_string())
                .json_data(&snap)
                .unwrap_or_else(|_| sse::Event::default().event("error").data("serialize failed")),
            Err(_) => sse::Event::default()
                .event("error")
                .data("snapshot failed; retrying on next change"),
        };
        Some((Ok(event), (s, rx, false)))
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
