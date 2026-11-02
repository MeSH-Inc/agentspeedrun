//! The commons' HTTP interface (COMMONS.md §19).

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use ed25519_dalek::SigningKey;
use speedrun_core::commons::CommonsHealth;
use speedrun_verify::Verifier;

use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub key: Arc<SigningKey>,
    pub verifier: Arc<Verifier>,
}

pub fn router(state: AppState) -> Router {
    Router::new().route("/v1/health", get(health)).with_state(state)
}

async fn health(State(s): State<AppState>) -> Result<Json<CommonsHealth>, (axum::http::StatusCode, String)> {
    let tree_size = s
        .store
        .tree_size()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(CommonsHealth {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        commons_pubkey: speedrun_core::sign::pubkey_hex(&s.key),
        tree_size,
    }))
}
