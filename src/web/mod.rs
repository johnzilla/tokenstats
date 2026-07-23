//! Lightweight Axum dashboard.

mod handlers;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use tower_http::trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
}

pub fn router(state: AppState) -> Router {
    let trace = TraceLayer::new_for_http()
        .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
        .on_response(DefaultOnResponse::new().level(Level::INFO))
        .on_failure(DefaultOnFailure::new().level(Level::WARN));

    Router::new()
        .route("/", get(handlers::dashboard))
        .route("/export.csv", get(handlers::export_csv))
        .route("/api/quotes", get(handlers::api_quotes))
        .route("/api/nodes", get(handlers::api_nodes))
        .route("/api/summary", get(handlers::api_summary))
        .route("/health", get(handlers::health))
        .layer(trace)
        .with_state(state)
}
