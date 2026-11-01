//! Local HTTP API. Bound to loopback. Every route except /v1/health needs a
//! bearer token from the data directory: the full token for everything, or
//! the display token for the read-only live routes, which never expose prompts.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{self, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use speedrun_core::api::{
    ActiveRunResponse, AppendRequest, CategoriesResponse, ErrorBody, ProjectAppendRequest, PromptRequest,
};
use tokio::sync::watch;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::service::{Appended, Error, Service};
use crate::verify::Verifier;

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<Mutex<Service>>,
    pub token: Arc<String>,
    pub display_token: Arc<String>,
    pub verifier: Arc<Verifier>,
    /// Bumped after every successful write; live streams wake on it.
    pub changes: Arc<watch::Sender<u64>>,
}

/// Live streams resend a snapshot at least this often, so displays
/// self-correct even if they missed something.
const LIVE_REFRESH: StdDuration = StdDuration::from_secs(60);

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            Error::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
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
    let mut display = Router::new()
        .route("/v1/live", get(live))
        .route("/v1/live/stream", get(live_stream))
        .route("/v1/categories", get(categories))
        .route_layer(middleware::from_fn_with_state(state.clone(), display_auth));
    if !cors_origins.is_empty() {
        let origins: Vec<HeaderValue> = cors_origins.iter().filter_map(|o| o.parse().ok()).collect();
        display = display.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET])
                .allow_headers([header::AUTHORIZATION]),
        );
    }
    let api = Router::new()
        .route("/v1/prompts", post(record_prompt))
        .route("/v1/events", post(append))
        .route("/v1/projects/{project_id}/active-run", get(active_run))
        .route("/v1/projects/{project_id}/events", post(append_to_project))
        .route("/v1/runs/{run_id}", get(run))
        .route("/v1/runs/{run_id}/events", get(events))
        .route("/v1/runs/{run_id}/verify", get(verify))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .route(
            "/v1/health",
            get(|| async { Json(serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })) }),
        )
        .merge(api)
        .merge(display)
        .with_state(state)
}

impl AppState {
    fn token_policy(&self) -> TokenPolicy<'_> {
        TokenPolicy {
            full: self.token.as_str(),
            display: self.display_token.as_str(),
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
}

impl TokenPolicy<'_> {
    fn is_full(&self, candidate: &str) -> bool {
        token_eq(candidate.as_bytes(), self.full.as_bytes())
    }

    fn is_display(&self, candidate: &str) -> bool {
        token_eq(candidate.as_bytes(), self.display.as_bytes())
    }

    /// Full access is header-only: the full token must never be written into a URL.
    fn allows_full_access(&self, presented: Option<Credential<'_>>) -> bool {
        matches!(presented, Some(Credential::Header(token)) if self.is_full(token))
    }

    /// Display routes take either token in the header, but only the display
    /// token as `?token=` (browsers' EventSource cannot set headers).
    fn allows_display(&self, presented: Option<Credential<'_>>) -> bool {
        match presented {
            Some(Credential::Header(token)) => self.is_full(token) || self.is_display(token),
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

/// Wake live streams after a write.
fn notify(state: &AppState) {
    state.changes.send_modify(|n| *n = n.wrapping_add(1));
}

async fn record_prompt(State(s): State<AppState>, Json(req): Json<PromptRequest>) -> impl IntoResponse {
    let res = call(&s, move |svc| svc.record_prompt(req)).await;
    notify(&s);
    res
}

async fn append(State(s): State<AppState>, Json(req): Json<AppendRequest>) -> impl IntoResponse {
    let res = blocking(&s, move |svc| svc.append(req)).await;
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
    let (service, verifier) = (s.service.clone(), s.verifier.clone());
    let (outcome, derived) = tokio::task::spawn_blocking(move || {
        let outcome = verifier.verify(&job);
        let mut guard = service.lock().unwrap_or_else(|p| p.into_inner());
        let derived = guard.finish_verification(&job, &outcome);
        (outcome, derived)
    })
    .await
    .map_err(|e| Error::Internal(e.into()))?;
    let derived = derived?;
    crate::logging::verification_failed(&outcome);
    if !derived.is_empty() {
        notify(&s);
    }
    response.derived.extend(derived);
    response.verification = Some(outcome.report());
    Ok(Json(response))
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
    Path(project_id): Path<String>,
    Json(req): Json<ProjectAppendRequest>,
) -> impl IntoResponse {
    let res = blocking(&s, move |svc| svc.append_to_project(&project_id, req)).await;
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
            // Wake on a write or the periodic refresh, whichever comes first.
            tokio::select! {
                changed = rx.changed() => if changed.is_err() { return None },
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

async fn categories(State(s): State<AppState>) -> impl IntoResponse {
    call(&s, |svc| {
        Ok(CategoriesResponse {
            categories: svc.categories().to_vec(),
        })
    })
    .await
}
