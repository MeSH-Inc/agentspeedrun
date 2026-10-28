//! Local HTTP API. Bound to loopback; every route except /v1/health needs
//! the bearer token from the data directory.

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use speedrun_core::api::{
    ActiveRunResponse, AppendRequest, CategoriesResponse, ErrorBody, ProjectAppendRequest, PromptRequest,
};

use crate::service::{Error, Service};

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<Mutex<Service>>,
    pub token: Arc<String>,
}

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
async fn call<T, F>(state: &AppState, f: F) -> Result<Json<T>, Error>
where
    T: Serialize + Send + 'static,
    F: FnOnce(&mut Service) -> Result<T, Error> + Send + 'static,
{
    let service = state.service.clone();
    tokio::task::spawn_blocking(move || {
        let mut guard = service.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard)
    })
    .await
    .map_err(|e| Error::Internal(e.into()))?
    .map(Json)
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/v1/prompts", post(record_prompt))
        .route("/v1/events", post(append))
        .route("/v1/projects/{project_id}/active-run", get(active_run))
        .route("/v1/projects/{project_id}/events", post(append_to_project))
        .route("/v1/runs/{run_id}", get(run))
        .route("/v1/runs/{run_id}/events", get(events))
        .route("/v1/runs/{run_id}/verify", get(verify))
        .route("/v1/categories", get(categories))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .route(
            "/v1/health",
            get(|| async { Json(serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })) }),
        )
        .merge(api)
        .with_state(state)
}

/// Constant-time comparison so the token cannot be guessed byte by byte.
fn token_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(t) if token_eq(t.as_bytes(), state.token.as_bytes()) => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody {
                error: "missing or invalid bearer token".into(),
            }),
        )
            .into_response(),
    }
}

async fn record_prompt(State(s): State<AppState>, Json(req): Json<PromptRequest>) -> impl IntoResponse {
    call(&s, move |svc| svc.record_prompt(req)).await
}

async fn append(State(s): State<AppState>, Json(req): Json<AppendRequest>) -> impl IntoResponse {
    call(&s, move |svc| svc.append(req)).await
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
    call(&s, move |svc| svc.append_to_project(&project_id, req)).await
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
