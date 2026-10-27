//! Lightweight Axum dashboard.

mod feed;
mod handlers;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::extract::State;
use axum::http::{header, HeaderValue};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use tower_http::trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    /// Age past which quotes are flagged `stale`.
    pub stale_after: Duration,
    /// Whether the catalog poller runs (feed status reports `offline` if not).
    pub polling: bool,
    /// Catalog poll interval (dormant-node backoff is a multiple of it).
    pub poll_interval: Duration,
    /// Plausible analytics, when `--plausible-script` is set.
    pub analytics: Option<Analytics>,
}

/// A third-party analytics script and the origin the CSP must allow for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Analytics {
    pub script: String,
    pub origin: String,
}

impl Analytics {
    /// Accept only absolute `https://` script URLs (they go into the CSP).
    pub fn parse(script: &str) -> Result<Self> {
        let url = reqwest::Url::parse(script.trim())
            .with_context(|| format!("invalid --plausible-script URL {script:?}"))?;
        if url.scheme() != "https" || url.host_str().is_none() {
            bail!("--plausible-script must be an https:// URL, got {script:?}");
        }
        Ok(Self {
            script: url.to_string(),
            origin: url.origin().ascii_serialization(),
        })
    }
}

/// Content-Security-Policy for every response. No inline script is allowed:
/// dashboard JS is served from `/static/`. Inline styles stay allowed (the
/// page's `<style>` block and reliability bar widths).
pub fn content_security_policy(analytics: Option<&Analytics>) -> String {
    let extra = analytics
        .map(|a| format!(" {}", a.origin))
        .unwrap_or_default();
    format!(
        "default-src 'none'; script-src 'self'{extra}; connect-src 'self'{extra}; \
         style-src 'unsafe-inline'; img-src 'self' data:; base-uri 'none'; \
         form-action 'self'; frame-ancestors 'none'"
    )
}

async fn security_headers(State(csp): State<HeaderValue>, mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, csp);
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

pub fn router(state: AppState) -> Router {
    let csp = HeaderValue::from_str(&content_security_policy(state.analytics.as_ref()))
        .expect("CSP is ASCII");
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
        .route("/api/compare", get(handlers::api_compare))
        .route("/compare", get(handlers::compare_page))
        .route("/health", get(handlers::health))
        .route("/static/app.js", get(handlers::app_js))
        .route(
            "/static/plausible-init.js",
            get(handlers::plausible_init_js),
        )
        .layer(axum::middleware::map_response_with_state(
            csp,
            security_headers,
        ))
        .layer(trace)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analytics_url_must_be_https() {
        let a = Analytics::parse("https://plausible.io/js/pa-abc.js").unwrap();
        assert_eq!(a.origin, "https://plausible.io");
        assert!(Analytics::parse("http://plausible.io/js/x.js").is_err());
        assert!(Analytics::parse("javascript:alert(1)").is_err());
        assert!(Analytics::parse("not a url").is_err());
    }

    #[test]
    fn csp_allows_analytics_origin_only_when_set() {
        let strict = content_security_policy(None);
        assert!(strict.contains("script-src 'self';"));
        assert!(!strict.contains("https://"), "{strict}");
        let a = Analytics::parse("https://plausible.io/js/pa-abc.js").unwrap();
        let with = content_security_policy(Some(&a));
        assert!(with.contains("script-src 'self' https://plausible.io;"));
        assert!(with.contains("connect-src 'self' https://plausible.io;"));
    }

    async fn get(state: AppState, path: &str) -> Response {
        use tower::Service;
        let req = axum::http::Request::get(path)
            .body(axum::body::Body::empty())
            .unwrap();
        router(state).call(req).await.unwrap()
    }

    fn state(analytics: Option<Analytics>) -> AppState {
        AppState {
            store: Arc::new(Store::new()),
            stale_after: Duration::from_secs(600),
            polling: true,
            poll_interval: Duration::from_secs(60),
            analytics,
        }
    }

    async fn body(resp: Response) -> String {
        let b = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(b.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn every_response_gets_csp_and_nosniff() {
        for path in [
            "/",
            "/compare",
            "/health",
            "/api/quotes",
            "/api/compare",
            "/static/app.js",
            "/nope",
        ] {
            let resp = get(state(None), path).await;
            let h = resp.headers();
            assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{path}");
            assert!(h.contains_key(header::CONTENT_SECURITY_POLICY), "{path}");
        }
    }

    #[tokio::test]
    async fn dashboard_has_no_inline_script_and_analytics_is_opt_in() {
        for path in ["/", "/compare"] {
            let html = body(get(state(None), path).await).await;
            assert!(html.contains(r#"<script src="/static/app.js"></script>"#));
            assert!(
                !html.contains("<script>"),
                "inline script would violate CSP on {path}"
            );
            assert!(!html.contains("plausible"));
        }
        assert_eq!(
            get(state(None), "/static/plausible-init.js").await.status(),
            404
        );

        let a = Analytics::parse("https://plausible.io/js/pa-abc.js").unwrap();
        let html = body(get(state(Some(a.clone())), "/").await).await;
        assert!(html.contains(r#"<script async src="https://plausible.io/js/pa-abc.js"></script>"#));
        assert!(!html.contains("<script>"));
        let init = get(state(Some(a)), "/static/plausible-init.js").await;
        assert_eq!(init.status(), 200);
        assert!(body(init).await.contains("plausible.init()"));
    }
}
