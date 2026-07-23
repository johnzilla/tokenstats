//! Lightweight Axum dashboard.

mod handlers;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::dashboard))
        .route("/export.csv", get(handlers::export_csv))
        .route("/api/quotes", get(handlers::api_quotes))
        .route("/api/nodes", get(handlers::api_nodes))
        .route("/api/summary", get(handlers::api_summary))
        .route("/health", get(handlers::health))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
