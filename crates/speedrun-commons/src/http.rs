//! The commons' HTTP interface (COMMONS.md §19).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ed25519_dalek::SigningKey;
use serde::Deserialize;
use speedrun_core::bundle::Bundle;
use speedrun_core::commons::{CommonsHealth, InclusionProof, PublishReceipt, PublishedRun, TreeHead};
use speedrun_verify::Verifier;

use crate::log::{self, LogError};
use crate::record::{self, PublishError};
use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub key: Arc<SigningKey>,
    pub verifier: Arc<Verifier>,
    /// Held for the whole of one publication, so two concurrent requests
    /// cannot both compute the same next leaf index.
    pub publishing: Arc<tokio::sync::Mutex<()>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/bundles", post(publish))
        .route("/v1/bundles/{digest}", get(published_bundle))
        .route("/v1/runs/{run_id}", get(published_run))
        .route("/v1/log/head", get(log_head))
        .route("/v1/log/proof/{leaf_index}", get(log_proof))
        .with_state(state)
}

async fn health(State(s): State<AppState>) -> Result<Json<CommonsHealth>, (StatusCode, String)> {
    let tree_size = s.store.tree_size().await.map_err(internal)?;
    Ok(Json(CommonsHealth {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        commons_pubkey: speedrun_core::sign::pubkey_hex(s.key.as_ref()),
        tree_size,
    }))
}

async fn log_head(State(s): State<AppState>) -> Result<Json<TreeHead>, (StatusCode, String)> {
    let head = log::latest(s.store.as_ref(), s.key.as_ref())
        .await
        .map_err(log_error)?;
    Ok(Json(head))
}

#[derive(Deserialize)]
struct ProofQuery {
    tree_size: Option<u64>,
}

async fn log_proof(
    State(s): State<AppState>,
    Path(leaf_index): Path<u64>,
    Query(query): Query<ProofQuery>,
) -> Result<Json<InclusionProof>, (StatusCode, String)> {
    let proof = log::inclusion(s.store.as_ref(), leaf_index, query.tree_size)
        .await
        .map_err(log_error)?;
    Ok(Json(proof))
}

async fn publish(
    State(s): State<AppState>,
    body: Bytes,
) -> Result<Json<PublishReceipt>, (StatusCode, String)> {
    let bundle: Bundle = serde_json::from_slice(&body).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("the request body is not a bundle: {error}"),
        )
    })?;
    let body = String::from_utf8(body.to_vec()).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("the request body is not UTF-8: {error}"),
        )
    })?;
    let _publishing = s.publishing.lock().await;
    let receipt = record::publish(
        s.store.as_ref(),
        s.key.as_ref(),
        s.verifier.as_ref(),
        bundle,
        body,
    )
    .await
    .map_err(publish_error)?;
    Ok(Json(receipt))
}

async fn published_bundle(
    State(s): State<AppState>,
    Path(digest): Path<String>,
) -> Result<Json<PublishedRun>, (StatusCode, String)> {
    let stored = s.store.publication(&digest).await.map_err(internal)?;
    match stored {
        Some(stored) => Ok(Json(record::published_run(stored).map_err(publish_error)?)),
        None => Err((StatusCode::NOT_FOUND, format!("no bundle {digest}"))),
    }
}

async fn published_run(
    State(s): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Json<PublishedRun>, (StatusCode, String)> {
    let stored = s
        .store
        .latest_publication_for_run(&run_id)
        .await
        .map_err(internal)?;
    match stored {
        Some(stored) => Ok(Json(record::published_run(stored).map_err(publish_error)?)),
        None => Err((StatusCode::NOT_FOUND, format!("no run {run_id}"))),
    }
}

fn publish_error(error: PublishError) -> (StatusCode, String) {
    match error {
        PublishError::Refused(message) => (StatusCode::BAD_REQUEST, message),
        PublishError::Internal(error) => internal(error),
    }
}

fn log_error(error: LogError) -> (StatusCode, String) {
    match error {
        LogError::NotInTree => (StatusCode::NOT_FOUND, "the leaf is not in that tree".into()),
        LogError::BeyondLog => (StatusCode::BAD_REQUEST, "tree_size exceeds the log".into()),
        LogError::Internal(error) => internal(error),
    }
}

fn internal(error: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
}
