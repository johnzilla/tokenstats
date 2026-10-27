//! HTTP handlers: HTML dashboard + JSON API + CSV export.

use std::collections::{BTreeMap, HashMap};

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::feed::feed_status;
use super::AppState;
use crate::market::{
    apply_preset, best_now, blended_usd, canonical_model, delta_pct, EndpointSignals, Preset,
    Signals, DEFAULT_BLEND_RATIO, REFERENCE_SOURCES,
};
use crate::pricing::{blend_sort_key, is_rankable_blend};
use crate::store::{
    normalize_endpoint, Capabilities, EndpointStats, FetchStatus, ProbeStatus, ProviderNode, Quote,
};

pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let quotes = state.store.list_quotes();
    let last = freshest_ts(&quotes);
    let feed = feed_status(&state, &quotes);
    Json(serde_json::json!({
        // Liveness only; consult `status` / `feed` for data quality.
        "ok": true,
        "status": feed["status"],
        "feed": feed,
        "service": "tokenstats",
        "quotes": quotes.len(),
        "nodes": state.store.node_count(),
        "btc_usd": state.store.btc_usd(),
        "btc_usd_at": state.store.btc_usd_at(),
        "btc_usd_age_secs": btc_age_secs(&state),
        "last_updated": last.map(|t| t.to_rfc3339()),
        "ts": Utc::now().to_rfc3339(),
    }))
}

#[derive(Debug, Deserialize, Clone)]
pub struct QuotesQuery {
    /// Filter by source(s), comma-separated: openrouter | orcarouter | routstr | nostr
    pub source: Option<String>,
    /// Case-insensitive model id substring.
    pub model: Option<String>,
    /// Preset: frontier | fastest | private | local
    pub preset: Option<String>,
    /// Output:input ratio for blended cost (default 3 → 1:3).
    pub ratio: Option<f64>,
    /// Max rows (default 200 for API, 150 for HTML board).
    pub limit: Option<usize>,
    /// Only quotes observed within this many seconds.
    pub max_age_secs: Option<i64>,
    /// Only quotes whose endpoint succeeded at least this often over the last
    /// hour (0–1). Quotes with no poll history are excluded.
    pub min_reliability: Option<f64>,
    /// Model kind(s), comma-separated: chat | embedding | image | audio | rerank | other
    pub kind: Option<String>,
    /// Like `min_reliability` over the last 24h (hourly buckets, persisted).
    pub min_reliability_24h: Option<f64>,
    /// Only quotes whose endpoint's 24h p50 response time is at most this.
    /// Quotes with no latency samples are excluded.
    pub max_latency_ms: Option<u64>,
    /// `1`/`true` hides quotes older than the stale threshold.
    pub exclude_stale: Option<String>,
    /// `1`/`true` hides quotes priced >10× off the cross-source median.
    pub exclude_outliers: Option<String>,
    /// `1`/`true` keeps only quotes with `healthy: true`.
    pub healthy: Option<String>,
    /// Required capabilities, comma-separated: tools | reasoning | vision.
    /// A quote matches when each is `true` (published or inferred).
    pub capabilities: Option<String>,
}

impl QuotesQuery {
    fn blend_ratio(&self) -> f64 {
        self.ratio
            .filter(|r| *r > 0.0 && *r <= 20.0)
            .unwrap_or(DEFAULT_BLEND_RATIO)
    }

    fn preset(&self) -> Option<Preset> {
        self.preset.as_deref().and_then(Preset::from_str)
    }

    /// Negative ages are meaningless: ignored, like an invalid `ratio`.
    fn max_age(&self) -> Option<i64> {
        self.max_age_secs.filter(|s| *s >= 0)
    }

    fn qs(&self, overrides: &[(&str, Option<&str>)]) -> String {
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut set = |k: &str, v: Option<&str>| {
            if let Some(val) = v {
                if !val.is_empty() {
                    pairs.push((k.to_string(), val.to_string()));
                }
            }
        };

        let mut source = self.source.as_deref();
        let mut model = self.model.as_deref();
        let mut preset = self.preset.as_deref();
        let mut ratio = self.ratio.map(|r| format!("{r}"));
        let mut exclude_stale = self.exclude_stale.as_deref();
        let mut exclude_outliers = self.exclude_outliers.as_deref();
        let mut healthy = self.healthy.as_deref();
        let mut capabilities = self.capabilities.as_deref();

        for (k, v) in overrides {
            match *k {
                "source" => source = *v,
                "model" => model = *v,
                "preset" => preset = *v,
                "ratio" => ratio = v.map(|s| s.to_string()),
                "exclude_stale" => exclude_stale = *v,
                "exclude_outliers" => exclude_outliers = *v,
                "healthy" => healthy = *v,
                "capabilities" => capabilities = *v,
                "clear" if *v == Some("1") => {
                    source = None;
                    model = None;
                    preset = None;
                }
                _ => {}
            }
        }

        set("source", source);
        set("model", model);
        set("preset", preset);
        set("ratio", ratio.as_deref());
        // Data-quality filters ride along so board links and CSV export match.
        let max_age = self.max_age_secs.map(|v| v.to_string());
        let min_rel = self.min_reliability.map(|v| v.to_string());
        set("max_age_secs", max_age.as_deref());
        set("min_reliability", min_rel.as_deref());
        let min_rel_24h = self.min_reliability_24h.map(|v| v.to_string());
        let max_lat = self.max_latency_ms.map(|v| v.to_string());
        set("min_reliability_24h", min_rel_24h.as_deref());
        set("max_latency_ms", max_lat.as_deref());
        set("kind", self.kind.as_deref());
        set("exclude_stale", exclude_stale);
        set("exclude_outliers", exclude_outliers);
        set("healthy", healthy);
        set("capabilities", capabilities);

        if pairs.is_empty() {
            return String::new();
        }
        let body = pairs
            .iter()
            .map(|(k, v)| format!("{k}={}", percent_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("?{body}")
    }
}

/// Reliability floors are fractions: NaN or ≤ 0 means no filter (rather than
/// silently matching everything, unpolled endpoints included); above 1
/// clamps to 1.
fn reliability_floor(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite() && *x > 0.0).map(|x| x.min(1.0))
}

/// Percent-encode a query value: only RFC 3986 unreserved characters (plus `,`
/// for list params) pass through, so the result can never close an HTML
/// attribute or start a tag. Hrefs are still passed through `esc_attr`.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b',') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Apply source/model/quality/preset filters and sort by blend (unless preset sorts itself).
fn resolve_view(all: Vec<Quote>, q: &QuotesQuery, ctx: &QuoteCtx) -> Vec<Quote> {
    let ratio = q.blend_ratio();
    let mut filtered = filter_quotes(all, q, ctx);
    if let Some(p) = q.preset() {
        filtered = apply_preset(filtered, p, &ctx.signals);
    } else {
        // Cheapest-first: free / missing / invalid blends sort last (not as −$1e6).
        filtered.sort_by(|a, b| {
            let ba = blend_sort_key(blended_usd(a.price_in_usd, a.price_out_usd, ratio));
            let bb = blend_sort_key(blended_usd(b.price_in_usd, b.price_out_usd, ratio));
            ba.partial_cmp(&bb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.model.cmp(&b.model))
        });
    }
    filtered
}

pub async fn api_quotes(
    State(state): State<AppState>,
    Query(q): Query<QuotesQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(200).min(MAX_API_LIMIT);
    let all = state.store.list_quotes();
    let ctx = QuoteCtx::new(&state, &all, q.blend_ratio());
    let quotes = resolve_view(all, &q, &ctx);
    let enriched: Vec<_> = quotes
        .into_iter()
        .take(limit)
        .map(|quote| enrich_json(&quote, &ctx))
        .collect();
    Json(enriched)
}

pub async fn api_nodes(State(state): State<AppState>) -> impl IntoResponse {
    let fetches: HashMap<String, _> = state
        .store
        .fetch_statuses()
        .into_iter()
        .filter(|f| f.source == "routstr")
        .map(|f| (f.endpoint.clone(), f))
        .collect();
    let stats = state.store.endpoint_stats();
    let probes = state.store.probe_statuses();
    let nodes: Vec<_> = state
        .store
        .list_nodes()
        .into_iter()
        .map(|n| {
            let key = normalize_endpoint(&n.endpoint);
            let fetch = fetches.get(&key);
            let mut v = serde_json::to_value(&n).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                let st = stats.get(&key).cloned().unwrap_or_default();
                if let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(st) {
                    obj.extend(fields);
                }
                let probe = probes.get(&key);
                obj.insert(
                    "identity_verified".into(),
                    serde_json::json!(probe.and_then(|p| p.identity_verified)),
                );
                obj.insert(
                    "probe".into(),
                    serde_json::json!(probe.map(|p| serde_json::json!({
                        "probed_at": p.probed_at,
                        "ok": p.ok,
                        "error": p.error,
                    }))),
                );
                obj.insert(
                    "info".into(),
                    serde_json::json!(probe.and_then(|p| p.info.clone())),
                );
                obj.insert(
                    "last_success_at".into(),
                    serde_json::json!(fetch.and_then(|f| f.last_success_at)),
                );
                obj.insert(
                    "last_error".into(),
                    serde_json::json!(fetch.and_then(|f| f.last_error.clone())),
                );
                obj.insert(
                    "consecutive_failures".into(),
                    serde_json::json!(fetch.map(|f| f.consecutive_failures).unwrap_or(0)),
                );
                obj.insert(
                    "status".into(),
                    serde_json::json!(node_status(n.skip_reason.is_some(), fetch)),
                );
                obj.insert(
                    "failure_class".into(),
                    serde_json::json!(if n.skip_reason.is_some() {
                        Some("ssrf_skip")
                    } else {
                        fetch.and_then(|f| f.failure_class())
                    }),
                );
                obj.insert(
                    "next_poll_at".into(),
                    serde_json::json!(fetch.and_then(|f| f.next_poll_at(state.poll_interval))),
                );
            }
            v
        })
        .collect();
    Json(nodes)
}

pub async fn api_summary(State(state): State<AppState>) -> impl IntoResponse {
    let ratio = DEFAULT_BLEND_RATIO;
    let quotes = state.store.list_quotes();
    let mut by_source: BTreeMap<String, usize> = BTreeMap::new();
    for q in &quotes {
        *by_source.entry(q.source.clone()).or_default() += 1;
    }
    let models: std::collections::BTreeSet<_> = quotes.iter().map(|q| q.model.as_str()).collect();
    let best = best_now(&quotes, ratio);
    let last = freshest_ts(&quotes);
    let feed = feed_status(&state, &quotes);

    Json(serde_json::json!({
        "status": feed["status"],
        "feed": feed,
        "quotes": quotes.len(),
        "nodes": state.store.node_count(),
        "distinct_models": models.len(),
        "by_source": by_source,
        "btc_usd": state.store.btc_usd(),
        "btc_usd_at": state.store.btc_usd_at(),
        "btc_usd_age_secs": btc_age_secs(&state),
        "blend_ratio": ratio,
        "last_updated": last.map(|t| t.to_rfc3339()),
        "best_now": best.iter().map(|b| serde_json::json!({
            "family": b.family,
            "model": b.quote.model,
            "provider": b.quote.provider,
            "provider_id": b.quote.provider_id,
            "source": b.quote.source,
            "endpoint": b.quote.endpoint,
            "blended_usd": b.blended_usd,
            "price_in_usd": b.quote.price_in_usd,
            "price_out_usd": b.quote.price_out_usd,
            "delta_out_pct": delta_pct(b.quote.price_out_usd, b.quote.prev_price_out_usd),
            "observed_at": b.quote.observed_at,
        })).collect::<Vec<_>>(),
        "ts": Utc::now().to_rfc3339(),
    }))
}

/// One model (by `canonical_model`) as offered across sources.
struct CompareGroup {
    key: String,
    /// Cheapest blend first; free / unpriced offers last.
    quotes: Vec<Quote>,
}

impl CompareGroup {
    fn sources(&self) -> std::collections::BTreeSet<&str> {
        self.quotes.iter().map(|q| q.source.as_str()).collect()
    }

    /// Rankable, non-outlier blends, ascending (same order as `quotes`).
    /// Outliers are usually unit bugs, so they would make a fake cheapest
    /// offer and a meaningless spread.
    fn blends(&self, ctx: &QuoteCtx) -> Vec<f64> {
        self.quotes
            .iter()
            .filter(|q| !ctx.is_outlier(q))
            .filter_map(|q| blended_usd(q.price_in_usd, q.price_out_usd, ctx.ratio))
            .filter(|b| is_rankable_blend(Some(*b)))
            .collect()
    }

    /// Cheapest rankable, non-outlier offer that also passes `pred`.
    fn cheapest(&self, ctx: &QuoteCtx, pred: impl Fn(&Quote) -> bool) -> Option<&Quote> {
        self.quotes.iter().find(|q| {
            is_rankable_blend(blended_usd(q.price_in_usd, q.price_out_usd, ctx.ratio))
                && !ctx.is_outlier(q)
                && pred(q)
        })
    }
}

/// Group the filtered board by `canonical_model`. `model` matches as a
/// substring of the canonical key (so `claude-sonnet-4.5` finds
/// `claude-sonnet-4-5`); `preset` is ignored. An exact key match comes
/// first, then models offered by the most sources.
fn compare_groups(all: Vec<Quote>, q: &QuotesQuery, ctx: &QuoteCtx) -> Vec<CompareGroup> {
    let needle = q
        .model
        .as_deref()
        .map(canonical_model)
        .filter(|n| !n.is_empty());
    let unfiltered_model = QuotesQuery {
        model: None,
        ..q.clone()
    };
    let mut groups: BTreeMap<String, Vec<Quote>> = BTreeMap::new();
    for quote in filter_quotes(all, &unfiltered_model, ctx) {
        let key = canonical_model(&quote.model);
        if needle.as_deref().is_none_or(|n| key.contains(n)) {
            groups.entry(key).or_default().push(quote);
        }
    }
    let ratio = ctx.ratio;
    let mut out: Vec<CompareGroup> = groups
        .into_iter()
        .map(|(key, mut quotes)| {
            quotes.sort_by(|a, b| {
                let ba = blend_sort_key(blended_usd(a.price_in_usd, a.price_out_usd, ratio));
                let bb = blend_sort_key(blended_usd(b.price_in_usd, b.price_out_usd, ratio));
                ba.partial_cmp(&bb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.source.cmp(&b.source))
            });
            CompareGroup { key, quotes }
        })
        .collect();
    out.sort_by(|a, b| {
        let exact = |g: &CompareGroup| needle.as_deref() != Some(g.key.as_str());
        exact(a)
            .cmp(&exact(b))
            .then_with(|| b.sources().len().cmp(&a.sources().len()))
            .then_with(|| b.quotes.len().cmp(&a.quotes.len()))
            .then_with(|| a.key.cmp(&b.key))
    });
    out
}

fn compare_json(g: &CompareGroup, ctx: &QuoteCtx) -> serde_json::Value {
    let ratio = ctx.ratio;
    let blends = g.blends(ctx);
    let min = blends.first().copied();
    let models: std::collections::BTreeSet<&str> =
        g.quotes.iter().map(|q| q.model.as_str()).collect();
    let quotes: Vec<_> = g
        .quotes
        .iter()
        .map(|q| {
            let mut v = enrich_json(q, ctx);
            v["vs_cheapest_pct"] = serde_json::json!(vs_cheapest_pct(q, min, ratio));
            v
        })
        .collect();
    serde_json::json!({
        "canonical_model": g.key,
        "models": models,
        "sources": g.sources(),
        "offers": g.quotes.len(),
        "blend_ratio": ratio,
        "min_blended_usd": min,
        "median_blended_usd": median(&blends),
        "max_blended_usd": blends.last(),
        "spread_pct": spread_pct(&blends),
        "cheapest": g.cheapest(ctx, |_| true).map(|q| enrich_json(q, ctx)),
        "cheapest_healthy": g.cheapest(ctx, |q| ctx.is_healthy(q)).map(|q| enrich_json(q, ctx)),
        "quotes": quotes,
    })
}

/// How much pricier than the cheapest offer, in % (0 for the cheapest).
fn vs_cheapest_pct(q: &Quote, min: Option<f64>, ratio: f64) -> Option<f64> {
    let b = blended_usd(q.price_in_usd, q.price_out_usd, ratio).filter(|b| *b > 0.0)?;
    min.filter(|m| *m > 0.0).map(|m| (b - m) / m * 100.0)
}

/// (max − min) / min in %, over ascending rankable blends; needs two offers.
fn spread_pct(sorted: &[f64]) -> Option<f64> {
    match (sorted.first(), sorted.last()) {
        (Some(min), Some(max)) if sorted.len() >= 2 && *min > 0.0 => {
            Some((max - min) / min * 100.0)
        }
        _ => None,
    }
}

fn median(sorted: &[f64]) -> Option<f64> {
    let mid = sorted.len() / 2;
    match sorted.len() {
        0 => None,
        n if n % 2 == 0 => Some((sorted[mid - 1] + sorted[mid]) / 2.0),
        _ => Some(sorted[mid]),
    }
}

/// Same model across OpenRouter, OrcaRouter and Routstr nodes.
pub async fn api_compare(
    State(state): State<AppState>,
    Query(q): Query<QuotesQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(50).min(MAX_COMPARE_GROUPS);
    let all = state.store.list_quotes();
    let ctx = QuoteCtx::new(&state, &all, q.blend_ratio());
    let groups: Vec<_> = compare_groups(all, &q, &ctx)
        .iter()
        .take(limit)
        .map(|g| compare_json(g, &ctx))
        .collect();
    Json(groups)
}

/// `/api/compare` group cap.
const MAX_COMPARE_GROUPS: usize = 1_000;

/// `/api/quotes` row cap. Large enough for the whole board (~8k quotes on
/// tokenstats.ai), so a consumer that asks for everything isn't silently
/// handed only the cheapest slice; narrow with `source` / `kind` instead.
const MAX_API_LIMIT: usize = 20_000;

const APP_JS: &str = include_str!("static/app.js");
const APP_CSS: &str = include_str!("static/app.css");
const PLAUSIBLE_INIT_JS: &str = include_str!("static/plausible-init.js");

fn js(body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        body,
    )
        .into_response()
}

pub async fn app_js() -> Response {
    js(APP_JS)
}

pub async fn plausible_init_js(State(state): State<AppState>) -> Response {
    match state.analytics {
        Some(_) => js(PLAUSIBLE_INIT_JS),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Export the current filtered view as CSV.
pub async fn export_csv(State(state): State<AppState>, Query(q): Query<QuotesQuery>) -> Response {
    let ratio = q.blend_ratio();
    let limit = q.limit.unwrap_or(5000).min(20_000);
    let all = state.store.list_quotes();
    let ctx = QuoteCtx::new(&state, &all, ratio);
    let quotes = resolve_view(all, &q, &ctx);
    let mut csv = String::from(
        "source,provider,provider_id,model,price_in_usd,price_out_usd,blended_usd,blend_ratio,\
         price_in_sats,price_out_sats,delta_out_pct,endpoint,region,context_length,observed_at,\
         kind,age_secs,stale,reliability_1h,outlier,reliability_24h,reliability_7d,\
         latency_p50_ms,latency_p95_ms,latency_source,identity_verified,healthy,canonical_model,\
         tools,reasoning,vision,max_output_tokens,capabilities_inferred\n",
    );
    for quote in quotes.into_iter().take(limit) {
        let blend = blended_usd(quote.price_in_usd, quote.price_out_usd, ratio);
        let d = delta_pct(quote.price_out_usd, quote.prev_price_out_usd);
        let (caps, inferred) = ctx.caps(&quote);
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            csv_cell(&quote.source),
            csv_cell(&quote.provider),
            csv_cell(&quote.provider_id),
            csv_cell(&quote.model),
            opt_f(quote.price_in_usd),
            opt_f(quote.price_out_usd),
            opt_f(blend),
            ratio,
            opt_f(quote.price_in_sats),
            opt_f(quote.price_out_sats),
            opt_f(d),
            csv_cell(quote.endpoint.as_deref().unwrap_or("")),
            csv_cell(quote.region.as_deref().unwrap_or("")),
            quote
                .context_length
                .map(|c| c.to_string())
                .unwrap_or_default(),
            quote.observed_at.to_rfc3339(),
            csv_cell(&quote.kind),
            ctx.age_secs(&quote),
            ctx.is_stale(&quote),
            opt_f(ctx.reliability(&quote)),
            ctx.is_outlier(&quote),
            opt_f(ctx.stats(&quote).and_then(|s| s.reliability_24h)),
            opt_f(ctx.stats(&quote).and_then(|s| s.reliability_7d)),
            opt_u(ctx.stats(&quote).and_then(|s| s.latency_p50_ms)),
            opt_u(ctx.stats(&quote).and_then(|s| s.latency_p95_ms)),
            ctx.stats(&quote)
                .and_then(|s| s.latency_source)
                .unwrap_or_default(),
            ctx.identity_verified(&quote)
                .map(|v| v.to_string())
                .unwrap_or_default(),
            ctx.is_healthy(&quote),
            csv_cell(&canonical_model(&quote.model)),
            opt_b(caps.tools),
            opt_b(caps.reasoning),
            opt_b(caps.vision),
            opt_u(caps.max_output_tokens),
            inferred.join(";"),
        ));
    }

    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"tokenstats-{stamp}.csv\""))
            .unwrap_or_else(|_| {
                HeaderValue::from_static("attachment; filename=\"tokenstats.csv\"")
            }),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

    (StatusCode::OK, headers, csv).into_response()
}

pub async fn dashboard(
    State(state): State<AppState>,
    Query(q): Query<QuotesQuery>,
) -> impl IntoResponse {
    let ratio = q.blend_ratio();
    let all = state.store.list_quotes();
    let nodes = state.store.list_nodes();
    let btc = state.store.btc_usd();
    let page_generated = Utc::now();
    let last_data = freshest_ts(&all);
    let ctx = QuoteCtx::new(&state, &all, ratio);

    let best = best_now(&all, ratio);
    let best_rows: String = if best.is_empty() {
        r#"<tr><td colspan="7" class="empty">Waiting for catalog data…</td></tr>"#.into()
    } else {
        best.iter()
            .map(|b| {
                let d = delta_pct(b.quote.price_out_usd, b.quote.prev_price_out_usd);
                let node_label = node_display_name(b.quote);
                format!(
                    r#"<tr class="best-row quote-row" {attrs}>
                      <td><span class="family">{family}</span></td>
                      <td>
                        <strong class="model-id">{model}</strong>
                        <div class="sub muted" title="{at_full}">updated {age} ago{stale}</div>
                      </td>
                      <td>
                        <div class="num stack-price">{blend}</div>
                        <div class="sub node-name">{node}</div>
                      </td>
                      <td class="num muted">{io_in}/{io_out}</td>
                      <td><span class="src src-{source}">{source}</span></td>
                      <td>{delta}</td>
                      <td class="actions-cell">{actions}</td>
                    </tr>"#,
                    attrs = quote_data_attrs(b.quote),
                    family = esc(b.family),
                    model = esc(&b.quote.model),
                    at_full = b.quote.observed_at.to_rfc3339(),
                    age = fmt_age(ctx.age_secs(b.quote)),
                    stale = if ctx.is_stale(b.quote) {
                        r#" <span class="tag tag-stale">stale</span>"#
                    } else {
                        ""
                    },
                    blend = fmt_usd(Some(b.blended_usd)),
                    node = esc(&node_label),
                    io_in = fmt_usd(b.quote.price_in_usd),
                    io_out = fmt_usd(b.quote.price_out_usd),
                    source = esc(&b.quote.source),
                    delta = fmt_delta(d),
                    actions = row_actions_html(Some((b.quote, ratio))),
                )
            })
            .collect()
    };

    let mut filtered = resolve_view(all.clone(), &q, &ctx);
    let total_filtered = filtered.len();
    filtered.truncate(150);

    let mut by_source: BTreeMap<String, usize> = BTreeMap::new();
    for quote in &all {
        *by_source.entry(quote.source.clone()).or_default() += 1;
    }

    let rows: String = if filtered.is_empty() {
        r#"<tr><td colspan="13" class="empty">No quotes match this filter…</td></tr>"#.into()
    } else {
        filtered
            .iter()
            .map(|quote| quote_row(quote, &ctx))
            .collect()
    };

    let node_stats = state.store.endpoint_stats();
    let node_probes = state.store.probe_statuses();
    let node_fetches: HashMap<String, FetchStatus> = state
        .store
        .fetch_statuses()
        .into_iter()
        .filter(|f| f.source == "routstr")
        .map(|f| (f.endpoint.clone(), f))
        .collect();
    let status_of = |n: &ProviderNode| {
        let fetch = node_fetches.get(&normalize_endpoint(&n.endpoint));
        (node_status(n.skip_reason.is_some(), fetch), fetch)
    };
    // Answering nodes first; long-dead and refused ones sink to the bottom.
    let mut nodes = nodes;
    nodes.sort_by_key(|n| {
        let rank = match status_of(n).0 {
            "healthy" => 0,
            "failing" => 1,
            "pending" => 2,
            "dormant" => 3,
            _ => 4,
        };
        (rank, std::cmp::Reverse(n.last_seen))
    });
    let node_rows: String = if nodes.is_empty() {
        r#"<tr><td colspan="8" class="empty">No Routstr nodes discovered yet (kind 38421)…</td></tr>"#
            .into()
    } else {
        nodes
            .iter()
            .map(|n| {
                let st = node_stats.get(&normalize_endpoint(&n.endpoint));
                let (status, fetch) = status_of(n);
                let class = if n.skip_reason.is_some() {
                    Some("ssrf_skip")
                } else {
                    fetch.and_then(|f| f.failure_class())
                };
                let status_label = match class {
                    Some(c) => format!("{status} · {c}"),
                    None => status.to_string(),
                };
                let status_title = n
                    .skip_reason
                    .clone()
                    .or_else(|| fetch.and_then(|f| f.last_error.clone()))
                    .map(|e| match fetch.and_then(|f| f.next_poll_at(state.poll_interval)) {
                        Some(due) => format!("{e} · next poll {}", due.format("%H:%M UTC")),
                        None => e,
                    })
                    .unwrap_or_default();
                let score_pct = (n.reliability * 100.0).round() as i32;
                let bar_cls = if score_pct >= 90 {
                    "rel-good"
                } else if score_pct >= 60 {
                    "rel-mid"
                } else if n.poll_total == 0 {
                    "rel-none"
                } else {
                    "rel-bad"
                };
                format!(
                    r#"<tr>
                      <td><strong>{name}</strong></td>
                      <td><code>{id}</code></td>
                      <td class="muted">{endpoint}</td>
                      <td class="muted">{region}</td>
                      <td><span class="node-status node-{status}" title="{status_title}">{status_label}</span></td>
                      <td>
                        <div class="rel {bar_cls}" title="{ok}/{total} polls last hour · 24h {rel_24h} · 7d {rel_7d}">
                          <span class="rel-bar" style="width:{score}%"></span>
                          <span class="rel-label">{score}%</span>
                        </div>
                      </td>
                      <td class="muted">{lat}</td>
                      <td class="muted">{via}</td>
                    </tr>"#,
                    name = esc(&n.name),
                    id = esc(&short(&n.provider_id, 16)),
                    endpoint = esc(&n.endpoint),
                    region = n.region.as_deref().map(esc).unwrap_or_else(|| "—".into()),
                    status = status,
                    status_label = esc(&status_label),
                    status_title = esc_attr(&status_title),
                    ok = n.poll_ok,
                    total = n.poll_total,
                    score = score_pct,
                    bar_cls = bar_cls,
                    rel_24h = fmt_pct(st.and_then(|s| s.reliability_24h)),
                    rel_7d = fmt_pct(st.and_then(|s| s.reliability_7d)),
                    lat = match st.and_then(|s| s.latency_p50_ms.zip(s.latency_p95_ms)) {
                        Some((p50, p95)) => format!("p50 ≤{p50} · p95 ≤{p95} ms"),
                        None => n
                            .last_latency_ms
                            .map(|ms| format!("{ms} ms"))
                            .unwrap_or_else(|| "—".into()),
                    },
                    via = esc(&match node_probes
                        .get(&normalize_endpoint(&n.endpoint))
                        .and_then(|p| p.identity_verified)
                    {
                        Some(true) => format!("{} · id ✓", n.discovered_via),
                        Some(false) => format!("{} · id ✗", n.discovered_via),
                        None => n.discovered_via.clone(),
                    }),
                )
            })
            .collect()
    };

    let source_pills: String = by_source
        .iter()
        .map(|(s, n)| {
            let active = q.source.as_deref() == Some(s.as_str());
            let cls = if active { "pill active" } else { "pill" };
            let href = q.qs(&[("source", Some(s.as_str()))]);
            format!(
                r#"<a class="{cls}" href="{href}">{s} <span>{n}</span></a>"#,
                s = esc(s),
                n = n,
                cls = cls,
                href = esc_attr(&href),
            )
        })
        .collect::<Vec<_>>()
        .join("");

    let preset_pills: String = Preset::all()
        .iter()
        .map(|p| {
            let active = q.preset() == Some(*p);
            let cls = if active { "pill active" } else { "pill" };
            let href = q.qs(&[("preset", Some(p.slug()))]);
            format!(
                r#"<a class="{cls}" href="{href}">{label}</a>"#,
                label = esc(p.label()),
                cls = cls,
                href = esc_attr(&href),
            )
        })
        .collect::<Vec<_>>()
        .join("");

    let ratio_pills: String = [2.0_f64, 3.0, 4.0]
        .iter()
        .map(|r| {
            let active = (ratio - *r).abs() < 0.01;
            let cls = if active { "pill active" } else { "pill" };
            let label = format!("1:{r:.0}");
            let href = esc_attr(&q.qs(&[("ratio", Some(&format!("{r}")))]));
            format!(r#"<a class="{cls}" href="{href}">{label}</a>"#)
        })
        .collect::<Vec<_>>()
        .join("");

    let btc_label = btc
        .map(|r| format!("BTC ${r:.0}"))
        .unwrap_or_else(|| "BTC —".into());

    let last_label = match last_data {
        Some(t) => format!("data {}", t.format("%H:%M:%S UTC")),
        None => "data —".into(),
    };
    let page_label = format!("page {}", page_generated.format("%H:%M:%S UTC"));
    let feed = feed_status(&state, &all);
    let feed_strip = feed_strip_html(&feed, btc_age_secs(&state));

    let outliers = all.iter().filter(|x| ctx.is_outlier(x)).count();
    let healthy = all.iter().filter(|x| ctx.is_healthy(x)).count();
    let quality_pills: String = [
        ("healthy", "healthy only", healthy, q.healthy.as_deref()),
        (
            "exclude_stale",
            "hide stale",
            feed["stale_quotes"].as_u64().unwrap_or(0) as usize,
            q.exclude_stale.as_deref(),
        ),
        (
            "exclude_outliers",
            "hide outliers",
            outliers,
            q.exclude_outliers.as_deref(),
        ),
    ]
    .iter()
    .map(|(key, label, n, current)| toggle_pill(&q, "/", key, label, Some(*n), *current))
    .collect::<Vec<_>>()
    .join("");
    let all_caps: Vec<Capabilities> = all.iter().map(|x| ctx.caps(x).0).collect();
    let capability_pills: String = CAPABILITY_FLAGS
        .iter()
        .map(|flag| {
            let n = all_caps
                .iter()
                .filter(|c| c.flag(flag) == Some(true))
                .count();
            capability_pill(&q, "/", flag, Some(n))
        })
        .collect();

    let filter_note = format!(
        " · {shown}/{total} · blend 1:{r:.0}",
        shown = filtered.len().min(total_filtered),
        total = total_filtered,
        r = ratio
    );

    let all_href = q.qs(&[("clear", Some("1")), ("source", None), ("preset", None)]);
    let all_active = if q.source.is_none() && q.preset.is_none() {
        " active"
    } else {
        ""
    };
    let csv_href = {
        let qs = q.qs(&[]);
        if qs.is_empty() {
            "/export.csv".into()
        } else {
            format!("/export.csv{qs}")
        }
    };

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <meta http-equiv="refresh" content="15"/>
  <title>tokenstats — inference market</title>
  <style>
{css}  </style>
  <!-- Privacy-friendly analytics by Plausible -->
{analytics}
</head>
<body>
  <header>
    <h1><span>token</span>stats</h1>
    <span class="badge">LIVE</span>
    <span class="meta">{quotes} quotes · {nodes} nodes · {btc}{filter_note}</span>
    <span class="meta" title="Freshest quote observation vs page render">{last_label} · {page_label}</span>
    <span class="meta links">
      <a href="{csv_href}">Export CSV</a>
      <a href="/compare">Compare</a>
      <a href="/api/summary">/api/summary</a>
      <a href="/api/quotes">/api/quotes</a>
      <a href="/health">/health</a>
    </span>
  </header>
  {feed_strip}

  <div class="pills">
    <span class="label">Source</span>
    <a class="pill{all_active}" href="{all_href}">all <span>{quotes}</span></a>
    {source_pills}
  </div>
  <div class="pills">
    <span class="label">Preset</span>
    {preset_pills}
  </div>
  <div class="pills">
    <span class="label">Blend</span>
    {ratio_pills}
    <span class="meta">in:out workload for blended $/M</span>
  </div>
  <div class="pills">
    <span class="label">Quality</span>
    {quality_pills}
  </div>
  <div class="pills">
    <span class="label">Capabilities</span>
    {capability_pills}
    <span class="meta">dashed = inferred from OpenRouter / OrcaRouter</span>
  </div>

  <h2>
    Best Now
    <span class="hint">cheapest blended · node under price</span>
  </h2>
  <table>
    <thead>
      <tr>
        <th>Family</th>
        <th>Model</th>
        <th>Blended / node</th>
        <th>In / Out</th>
        <th>Source</th>
        <th>Δ out</th>
        <th>Copy</th>
      </tr>
    </thead>
    <tbody>
      {best_rows}
    </tbody>
  </table>

  <h2>
    Price board
    <span class="hint">USD · sats · blend 1:{ratio_i} · hover row to copy curl / OpenAI config</span>
    <a class="export" href="{csv_href}">↓ Export CSV (current view)</a>
  </h2>
  <table>
    <thead>
      <tr>
        <th>Source</th>
        <th>Provider</th>
        <th>Model</th>
        <th>In</th>
        <th>Out</th>
        <th>Blend</th>
        <th>Δ out</th>
        <th>In sats</th>
        <th>Out sats</th>
        <th>Ctx</th>
        <th title="Endpoint catalog-fetch success, 24h (hover for 1h / 7d)">Rel 24h</th>
        <th title="Endpoint response time, 24h p50 (hover for p95)">Latency</th>
        <th title="Time since the catalog last listed this quote">Age</th>
      </tr>
    </thead>
    <tbody>
      {rows}
    </tbody>
  </table>

  <h2>Nodes <span class="hint">dormant = 5+ failed polls in a row, polled on a backoff (hover status for the error) · reliability = successful polls / total (last hour; hover for 24h / 7d) · latency = /v1/info probe (else catalog fetch), 24h · id ✓ = /v1/info npub matches the announcing key</span></h2>
  <table>
    <thead>
      <tr>
        <th>Name</th>
        <th>Provider id</th>
        <th>Endpoint</th>
        <th>Region</th>
        <th>Status</th>
        <th>Reliability</th>
        <th>Latency</th>
        <th>Via</th>
      </tr>
    </thead>
    <tbody>
      {node_rows}
    </tbody>
  </table>

  <footer>
    Sovereign inference market observability · OpenRouter + OrcaRouter + Routstr · MPL-2.0
    · auto-refresh 15s · last data {last_label}
  </footer>
  <div id="toast" class="toast" role="status"></div>
  <script src="/static/app.js"></script>
</body>
</html>"#,
        quotes = all.len(),
        nodes = nodes.len(),
        btc = btc_label,
        filter_note = filter_note,
        last_label = last_label,
        analytics = analytics_tags(&state),
        feed_strip = feed_strip,
        quality_pills = quality_pills,
        capability_pills = capability_pills,
        page_label = page_label,
        all_active = all_active,
        all_href = if all_href.is_empty() {
            "/".into()
        } else {
            esc_attr(&all_href)
        },
        csv_href = esc_attr(&csv_href),
        source_pills = source_pills,
        preset_pills = preset_pills,
        ratio_pills = ratio_pills,
        best_rows = best_rows,
        rows = rows,
        node_rows = node_rows,
        ratio_i = ratio as i32,
        css = APP_CSS,
    );

    ([(header::CACHE_CONTROL, "no-store")], Html(html))
}

/// Groups shown on `/compare` (the API takes `limit`).
const COMPARE_PAGE_GROUPS: usize = 25;

/// `/compare?model=`: one table per model, every source's offer side by side.
pub async fn compare_page(
    State(state): State<AppState>,
    Query(q): Query<QuotesQuery>,
) -> impl IntoResponse {
    let ratio = q.blend_ratio();
    let all = state.store.list_quotes();
    let ctx = QuoteCtx::new(&state, &all, ratio);
    let groups = compare_groups(all, &q, &ctx);
    let total = groups.len();
    let model = q.model.as_deref().unwrap_or("").trim();

    let sections: String = if groups.is_empty() {
        format!(
            r#"<p class="empty">No model matches “{}” with these filters.</p>"#,
            esc(model)
        )
    } else {
        groups
            .iter()
            .take(COMPARE_PAGE_GROUPS)
            .map(|g| compare_section(g, &ctx))
            .collect()
    };
    let heading = if model.is_empty() {
        format!("Models offered by the most sources · {total} models")
    } else if total > COMPARE_PAGE_GROUPS {
        format!("{COMPARE_PAGE_GROUPS} of {total} models match")
    } else {
        format!("{total} models match")
    };

    // Everything but `model` rides along as hidden fields, so a new search
    // keeps the blend and quality filters.
    let hidden: String = [
        ("ratio", q.ratio.map(|r| r.to_string())),
        ("source", q.source.clone()),
        ("kind", q.kind.clone()),
        ("healthy", q.healthy.clone()),
        ("exclude_stale", q.exclude_stale.clone()),
        ("exclude_outliers", q.exclude_outliers.clone()),
        ("capabilities", q.capabilities.clone()),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.filter(|v| !v.is_empty()).map(|v| (k, v)))
    .map(|(k, v)| {
        format!(
            r#"<input type="hidden" name="{k}" value="{}"/>"#,
            esc_attr(&v)
        )
    })
    .collect();
    let ratio_pills: String = [2.0_f64, 3.0, 4.0]
        .iter()
        .map(|r| {
            let cls = if (ratio - *r).abs() < 0.01 {
                "pill active"
            } else {
                "pill"
            };
            let href = format!("/compare{}", q.qs(&[("ratio", Some(&r.to_string()))]));
            format!(
                r#"<a class="{cls}" href="{}">1:{r:.0}</a>"#,
                esc_attr(&href)
            )
        })
        .collect();
    let quality_pills: String = [
        ("healthy", "healthy only", q.healthy.as_deref()),
        ("exclude_stale", "hide stale", q.exclude_stale.as_deref()),
        (
            "exclude_outliers",
            "hide outliers",
            q.exclude_outliers.as_deref(),
        ),
    ]
    .iter()
    .map(|(key, label, current)| toggle_pill(&q, "/compare", key, label, None, *current))
    .collect();
    let capability_pills: String = CAPABILITY_FLAGS
        .iter()
        .map(|flag| capability_pill(&q, "/compare", flag, None))
        .collect();
    let api_href = format!("/api/compare{}", q.qs(&[]));
    let board_href = format!("/{}", q.qs(&[]));

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <title>tokenstats — compare {title}</title>
  <style>
{css}  </style>
{analytics}
</head>
<body>
  <header>
    <h1><span>token</span>stats</h1>
    <span class="meta">compare one model across sources · blend 1:{ratio_i}</span>
    <span class="meta links">
      <a href="{board_href}">Board</a>
      <a href="{api_href}">/api/compare</a>
    </span>
  </header>
  <form class="search" method="get" action="/compare">
    <input type="search" name="model" value="{model_attr}" placeholder="model id, e.g. claude-sonnet-4.5 or deepseek" aria-label="Model"/>
    {hidden}
    <button type="submit">Compare</button>
  </form>
  <div class="pills">
    <span class="label">Blend</span>
    {ratio_pills}
  </div>
  <div class="pills">
    <span class="label">Quality</span>
    {quality_pills}
  </div>
  <div class="pills">
    <span class="label">Capabilities</span>
    {capability_pills}
  </div>
  <p class="meta">{heading} · ids are joined on a canonical key (vendor prefix, case, <code>.</code>/<code>_</code> and release date ignored) · cheapest blend first</p>
  {sections}
  <footer>tokenstats · observe only: prices are catalog list prices, not measured spend</footer>
  <div id="toast" class="toast" role="status"></div>
  <script src="/static/app.js"></script>
</body>
</html>"#,
        title = esc(if model.is_empty() { "models" } else { model }),
        css = APP_CSS,
        analytics = analytics_tags(&state),
        ratio_i = ratio as i32,
        board_href = esc_attr(&board_href),
        api_href = esc_attr(&api_href),
        model_attr = esc_attr(model),
    );
    ([(header::CACHE_CONTROL, "no-store")], Html(html))
}

fn compare_section(g: &CompareGroup, ctx: &QuoteCtx) -> String {
    let ratio = ctx.ratio;
    let blends = g.blends(ctx);
    let min = blends.first().copied();
    let sources = g.sources();
    let healthy = g.cheapest(ctx, |q| ctx.is_healthy(q));
    let best = g.cheapest(ctx, |_| true);
    let mut hint = vec![format!(
        "{} offers · {}",
        g.quotes.len(),
        sources.into_iter().collect::<Vec<_>>().join(", ")
    )];
    if let Some(m) = min {
        hint.push(format!("cheapest {}", fmt_usd(Some(m))));
    }
    if let Some(s) = spread_pct(&blends) {
        hint.push(format!("spread {s:.0}%"));
    }
    if let Some(h) = healthy {
        hint.push(format!(
            "cheapest healthy {} ({})",
            fmt_usd(blended_usd(h.price_in_usd, h.price_out_usd, ratio)),
            node_display_name(h)
        ));
    }
    let rows: String = g
        .quotes
        .iter()
        .map(|q| {
            let blend = blended_usd(q.price_in_usd, q.price_out_usd, ratio);
            let st = ctx.stats(q);
            let rel = st.and_then(|s| s.reliability_24h.or(s.reliability_1h));
            let vs = match vs_cheapest_pct(q, min, ratio) {
                Some(v) if v.abs() < 0.05 => "cheapest".to_string(),
                Some(v) => format!("{v:+.0}%"),
                None => "—".into(),
            };
            let cls = if best.is_some_and(|b| std::ptr::eq(b, q)) {
                "quote-row cmp-best"
            } else {
                "quote-row"
            };
            let flags = [
                ctx.is_stale(q)
                    .then_some(r#"<span class="tag tag-stale">stale</span>"#),
                ctx.is_outlier(q)
                    .then_some(r#"<span class="tag tag-outlier">outlier</span>"#),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
            format!(
                r#"<tr class="{cls}" {attrs}>
          <td><span class="src src-{source}">{source}</span></td>
          <td>{provider}<div class="sub muted">{node}</div><div class="sub"><code>{model}</code>{caps}</div>{actions}</td>
          <td class="num">{pin}</td>
          <td class="num">{pout}</td>
          <td class="blend">{blend}</td>
          <td class="muted">{vs}</td>
          <td class="num-sats">{sats}</td>
          <td class="rel-txt">{rel}</td>
          <td class="muted">{lat}</td>
          <td class="muted">{age} {flags}</td>
          <td>{healthy}</td>
        </tr>"#,
                attrs = quote_data_attrs(q),
                source = esc(&q.source),
                provider = esc(&q.provider),
                node = esc(&node_display_name(q)),
                model = esc(&q.model),
                caps = cap_tags(ctx, q),
                actions = row_actions_html(None),
                pin = fmt_usd(q.price_in_usd),
                pout = fmt_usd(q.price_out_usd),
                blend = fmt_usd(blend),
                sats = fmt_sats(q.price_out_sats),
                rel = fmt_pct(rel),
                lat = st
                    .and_then(|s| s.latency_p50_ms)
                    .map(|ms| format!("≤{ms} ms"))
                    .unwrap_or_else(|| "—".into()),
                age = fmt_age(ctx.age_secs(q)),
                healthy = if ctx.is_healthy(q) {
                    r#"<span class="node-healthy">✓</span>"#
                } else {
                    r#"<span class="muted">·</span>"#
                },
            )
        })
        .collect();
    format!(
        r#"<h2>{key} <span class="hint">{hint}</span></h2>
  <div class="table-wrap"><table>
    <thead><tr>
      <th>Source</th><th>Provider / node / id</th><th>In</th><th>Out</th>
      <th>Blend</th><th>vs cheapest</th><th>Out sats</th><th>Rel 24h</th>
      <th>Latency</th><th>Age</th><th title="Last fetch OK, ≥90% reliable, probe OK, not stale">Healthy</th>
    </tr></thead>
    <tbody>{rows}</tbody>
  </table></div>"#,
        key = esc(&g.key),
        hint = esc(&hint.join(" · ")),
    )
}

fn quote_row(q: &Quote, ctx: &QuoteCtx) -> String {
    let blend = blended_usd(q.price_in_usd, q.price_out_usd, ctx.ratio);
    let d = delta_pct(q.price_out_usd, q.prev_price_out_usd);
    let st = ctx.stats(q);
    // 24h is what routing trusts; fall back to 1h right after a restart.
    let rel = st.and_then(|s| s.reliability_24h.or(s.reliability_1h));
    let rel_cls = match rel {
        Some(r) if r >= 0.9 => "rel-txt rel-good",
        Some(r) if r >= 0.6 => "rel-txt rel-mid",
        Some(_) => "rel-txt rel-bad",
        None => "rel-txt muted",
    };
    let rel_title = format!(
        "endpoint catalog-fetch success · 1h {} · 24h {} · 7d {}",
        fmt_pct(st.and_then(|s| s.reliability_1h)),
        fmt_pct(st.and_then(|s| s.reliability_24h)),
        fmt_pct(st.and_then(|s| s.reliability_7d)),
    );
    let (lat, lat_title) = match st.and_then(|s| s.latency_p50_ms.zip(s.latency_p95_ms)) {
        Some((p50, p95)) => (
            format!("≤{p50} ms"),
            format!(
                "24h p50 ≤{p50} · p95 ≤{p95} ms ({})",
                st.and_then(|s| s.latency_source).unwrap_or("catalog")
            ),
        ),
        None => ("—".into(), "no latency samples yet".into()),
    };
    let age_cls = if ctx.is_stale(q) {
        "tag tag-stale"
    } else {
        "muted"
    };
    let outlier = if ctx.is_outlier(q) {
        r#" <span class="tag tag-outlier" title="&gt;10× off the median for this model">outlier</span>"#
    } else {
        ""
    };
    format!(
        r#"<tr class="quote-row" {attrs}>
          <td><span class="src src-{source}">{source}</span></td>
          <td>
            {provider}
            <div class="sub muted">{node}</div>
          </td>
          <td>
            <strong class="model-id">{model}</strong>{caps}
            {actions}
          </td>
          <td class="num">{pin_usd}</td>
          <td class="num">{pout_usd}</td>
          <td class="blend">{blend}{outlier}</td>
          <td>{delta}</td>
          <td class="num-sats">{pin_sats}</td>
          <td class="num-sats">{pout_sats}</td>
          <td class="muted">{ctx}</td>
          <td><span class="{rel_cls}" title="{rel_title}">{rel}</span></td>
          <td class="muted" title="{lat_title}">{lat}</td>
          <td title="observed {at_full}"><span class="{age_cls}">{age}</span></td>
        </tr>"#,
        attrs = quote_data_attrs(q),
        source = esc(&q.source),
        provider = esc(&q.provider),
        node = esc(&node_display_name(q)),
        model = esc(&q.model),
        caps = cap_tags(ctx, q),
        actions = row_actions_html(Some((q, ctx.ratio))),
        pin_usd = fmt_usd(q.price_in_usd),
        pout_usd = fmt_usd(q.price_out_usd),
        blend = fmt_usd(blend),
        delta = fmt_delta(d),
        pin_sats = fmt_sats(q.price_in_sats),
        pout_sats = fmt_sats(q.price_out_sats),
        ctx = fmt_ctx(q.context_length),
        rel = fmt_pct(rel),
        rel_cls = rel_cls,
        rel_title = esc_attr(&rel_title),
        lat = lat,
        lat_title = esc_attr(&lat_title),
        age_cls = age_cls,
        age = fmt_age(ctx.age_secs(q)),
        outlier = outlier,
        at_full = q.observed_at.to_rfc3339(),
    )
}

/// `skipped` (SSRF guard) · `pending` (not fetched yet) · `healthy` ·
/// `dormant` (failing, polled on a backoff) · `failing`.
fn node_status(skipped: bool, fetch: Option<&FetchStatus>) -> &'static str {
    match fetch {
        _ if skipped => "skipped",
        None => "pending",
        Some(f) if f.is_healthy() => "healthy",
        Some(f) if f.is_dormant() => "dormant",
        Some(_) => "failing",
    }
}

/// Compact age: `42s`, `7m`, `3h`, `2d`.
fn fmt_age(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// One-line feed health under the header, from the `/health` `feed` object.
fn feed_strip_html(feed: &serde_json::Value, btc_age: Option<i64>) -> String {
    let status = feed["status"].as_str().unwrap_or("unknown");
    let mut parts: Vec<String> = vec![format!(
        r#"<span class="feed-status feed-{s}">feed {s}</span>"#,
        s = esc(status)
    )];
    let now = Utc::now();
    let cycle = &feed["last_cycle"];
    if let Some(at) = cycle["finished_at"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
    {
        let ago = (now - at.with_timezone(&Utc)).num_seconds().max(0);
        let took = cycle["elapsed_ms"].as_u64().unwrap_or(0) as f64 / 1000.0;
        parts.push(format!("last cycle {} ago ({took:.1}s)", fmt_age(ago)));
    }
    if let Some(sources) = feed["sources"].as_object() {
        for (name, s) in sources {
            let endpoints = s["endpoints"].as_u64().unwrap_or(0);
            let healthy = s["healthy"].as_u64().unwrap_or(0);
            let text = if name == "routstr" {
                match s["dormant"].as_u64().unwrap_or(0) {
                    0 => format!("routstr {healthy}/{endpoints} nodes OK"),
                    d => format!("routstr {healthy}/{endpoints} nodes OK ({d} dormant)"),
                }
            } else {
                let mark = if healthy == endpoints && endpoints > 0 {
                    "✓"
                } else {
                    "✗"
                };
                format!("{} {mark} {}", esc(name), s["models"].as_u64().unwrap_or(0))
            };
            let classes = s["failure_classes"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| format!("{k} {}", v.as_u64().unwrap_or(0)))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|c| !c.is_empty());
            let title = [
                classes.map(|c| format!("failing: {c}")),
                s["last_error"].as_str().map(|e| format!("last error: {e}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            let title = if title.is_empty() {
                String::new()
            } else {
                format!(" title=\"{}\"", esc_attr(&title))
            };
            parts.push(format!("<span{title}>{text}</span>"));
        }
    }
    parts.push(format!(
        "{} stale quotes",
        feed["stale_quotes"].as_u64().unwrap_or(0)
    ));
    if let Some(age) = btc_age {
        parts.push(format!("BTC rate {} old", fmt_age(age)));
    }
    if let Some(lag) = feed["discovery"]["lag_secs"].as_i64() {
        parts.push(format!("last announcement {} ago", fmt_age(lag)));
    }
    let reasons = feed["reasons"]
        .as_array()
        .map(|r| {
            r.iter()
                .filter_map(|v| v.as_str())
                .map(esc)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let reasons = if reasons.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="feed-reasons">{}</div>"#, reasons.join(" · "))
    };
    format!(
        r#"<div class="feed">{} <a href="/health">/health</a>{reasons}</div>"#,
        parts.join(" · ")
    )
}

/// Hover actions: copy buttons, plus a link to `/compare` for this model
/// (omitted on the compare page itself).
fn row_actions_html(compare: Option<(&Quote, f64)>) -> String {
    let link = compare
        .map(|(q, ratio)| {
            let mut href = format!(
                "/compare?model={}",
                percent_encode(&canonical_model(&q.model))
            );
            if (ratio - DEFAULT_BLEND_RATIO).abs() > f64::EPSILON {
                href.push_str(&format!("&ratio={}", percent_encode(&ratio.to_string())));
            }
            format!(
                r#"
      <a href="{}" title="Same model across sources">compare</a>"#,
                esc_attr(&href)
            )
        })
        .unwrap_or_default();
    format!(
        r#"<div class="row-actions">
      <button type="button" data-copy="curl" title="Copy curl for chat/completions">curl</button>
      <button type="button" data-copy="openai" title="Copy OpenAI-compatible config JSON">config</button>{link}
    </div>"#
    )
}

/// Capability flags exposed as fields, filters and tags.
const CAPABILITY_FLAGS: [&str; 3] = ["tools", "reasoning", "vision"];

/// A pill that adds / removes `flag` in the `capabilities` list.
fn capability_pill(q: &QuotesQuery, base: &str, flag: &str, count: Option<usize>) -> String {
    let mut items = list_items(q.capabilities.as_deref());
    let on = items.iter().any(|i| i == flag);
    if on {
        items.retain(|i| i != flag);
    } else {
        items.push(flag.to_string());
    }
    let joined = items.join(",");
    let value = (!joined.is_empty()).then_some(joined.as_str());
    let href = format!("{base}{}", q.qs(&[("capabilities", value)]));
    let cls = if on { "pill active" } else { "pill" };
    let count = count
        .map(|n| format!(" <span>{n}</span>"))
        .unwrap_or_default();
    format!(
        r#"<a class="{cls}" href="{}">{flag}{count}</a>"#,
        esc_attr(&href)
    )
}

/// Small tags for the flags a quote has (dashed when inferred).
fn cap_tags(ctx: &QuoteCtx, q: &Quote) -> String {
    let (caps, inferred) = ctx.caps(q);
    CAPABILITY_FLAGS
        .iter()
        .filter(|f| caps.flag(f) == Some(true))
        .map(|f| {
            if inferred.contains(f) {
                format!(
                    r#" <span class="tag tag-cap tag-inferred" title="{f}: inferred from a reference catalog listing this model">{f}</span>"#
                )
            } else {
                format!(r#" <span class="tag tag-cap" title="{f}: published by this catalog">{f}</span>"#)
            }
        })
        .collect()
}

/// A filter pill that toggles `key=1` on `base` (`/` or `/compare`).
fn toggle_pill(
    q: &QuotesQuery,
    base: &str,
    key: &str,
    label: &str,
    count: Option<usize>,
    current: Option<&str>,
) -> String {
    let on = truthy(current);
    let cls = if on { "pill active" } else { "pill" };
    let href = format!(
        "{base}{}",
        q.qs(&[(key, if on { None } else { Some("1") })])
    );
    let count = count
        .map(|n| format!(" <span>{n}</span>"))
        .unwrap_or_default();
    format!(
        r#"<a class="{cls}" href="{}">{label}{count}</a>"#,
        esc_attr(&href)
    )
}

fn quote_data_attrs(q: &Quote) -> String {
    format!(
        r#"data-source="{source}" data-provider="{provider}" data-model="{model}" data-endpoint="{endpoint}""#,
        source = esc_attr(&q.source),
        provider = esc_attr(&q.provider),
        model = esc_attr(&q.model),
        endpoint = esc_attr(q.endpoint.as_deref().unwrap_or("")),
    )
}

/// Prefer endpoint host / provider for "which node is cheapest".
fn node_display_name(q: &Quote) -> String {
    if let Some(ep) = q.endpoint.as_deref() {
        if let Some(host) = host_from_url(ep) {
            // OpenRouter is the catalog itself
            if host.contains("openrouter") {
                return format!("{} · {}", q.provider, host);
            }
            if host != q.provider {
                return format!("{} · {}", q.provider, host);
            }
            return host;
        }
    }
    if !q.provider_id.is_empty() && q.provider_id != q.provider {
        return format!("{} · {}", q.provider, short(&q.provider_id, 12));
    }
    q.provider.clone()
}

fn host_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split('/').next()?.split('@').next_back()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Plausible `<script>` tags when `--plausible-script` is set.
fn analytics_tags(state: &AppState) -> String {
    match &state.analytics {
        Some(a) => format!(
            r#"<script async src="{}"></script>
  <script src="/static/plausible-init.js"></script>"#,
            esc_attr(&a.script)
        ),
        None => String::new(),
    }
}

fn btc_age_secs(state: &AppState) -> Option<i64> {
    state
        .store
        .btc_usd_at()
        .map(|t| (Utc::now() - t).num_seconds().max(0))
}

fn freshest_ts(quotes: &[Quote]) -> Option<DateTime<Utc>> {
    quotes.iter().map(|q| q.observed_at).max()
}

/// A quote is an outlier when its blend is this many times off the median.
const OUTLIER_FACTOR: f64 = 10.0;
/// Quotes for the same model needed before outliers are judged.
const OUTLIER_MIN_PEERS: usize = 3;
/// Success rate an endpoint needs for its quotes to count as `healthy`.
const HEALTHY_MIN_RELIABILITY: f64 = 0.9;

/// Per-request context for freshness, reliability and outlier fields/filters.
struct QuoteCtx {
    now: DateTime<Utc>,
    stale_after_secs: i64,
    ratio: f64,
    /// Normalized endpoint → reliability windows + latency.
    stats: HashMap<String, EndpointStats>,
    /// Normalized endpoint → latest `/v1/info` probe.
    probes: HashMap<String, ProbeStatus>,
    /// Normalized endpoint → onion address (announcement or `/v1/info`).
    onions: HashMap<String, String>,
    /// Normalized endpoint → latest catalog fetch outcome.
    fetches: HashMap<String, FetchStatus>,
    /// What presets rank on (derived from the three maps above).
    signals: Signals,
    /// `canonical_model` → median rankable blend across all quotes.
    medians: HashMap<String, f64>,
    /// `canonical_model` → capabilities the reference catalogs publish, used
    /// to fill flags a row leaves out (Routstr nodes publish no
    /// `supported_parameters`).
    peer_caps: HashMap<String, Capabilities>,
}

impl QuoteCtx {
    fn new(state: &AppState, all: &[Quote], ratio: f64) -> Self {
        let mut blends: HashMap<String, Vec<f64>> = HashMap::new();
        for q in all {
            let b = blended_usd(q.price_in_usd, q.price_out_usd, ratio);
            if is_rankable_blend(b) {
                blends
                    .entry(canonical_model(&q.model))
                    .or_default()
                    .extend(b);
            }
        }
        let medians = blends
            .into_iter()
            .filter(|(_, v)| v.len() >= OUTLIER_MIN_PEERS)
            .filter_map(|(k, mut v)| {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                median(&v).map(|m| (k, m))
            })
            .collect();
        // OpenRouter first, so it wins when the catalogs disagree.
        let mut peer_caps: HashMap<String, Capabilities> = HashMap::new();
        for src in REFERENCE_SOURCES {
            for q in all.iter().filter(|q| q.source == *src) {
                peer_caps
                    .entry(canonical_model(&q.model))
                    .or_default()
                    .merge_missing(&q.caps);
            }
        }
        let stats = state.store.endpoint_stats();
        let probes = state.store.probe_statuses();
        let onions: HashMap<String, String> = state
            .store
            .list_nodes()
            .into_iter()
            .filter_map(|n| {
                let key = normalize_endpoint(&n.endpoint);
                let probed = probes
                    .get(&key)
                    .and_then(|p| p.info.as_ref())
                    .and_then(|i| i.onion_url.clone());
                n.onion
                    .filter(|o| !o.is_empty())
                    .or(probed)
                    .map(|o| (key, o))
            })
            .collect();
        let fetches: HashMap<String, FetchStatus> = state
            .store
            .fetch_statuses()
            .into_iter()
            .map(|f| (f.endpoint.clone(), f))
            .collect();
        let mut keys: Vec<&String> = stats.keys().chain(probes.keys()).collect();
        keys.extend(onions.keys());
        keys.extend(fetches.keys());
        let signals = keys
            .into_iter()
            .map(|k| {
                let st = stats.get(k);
                let sig = EndpointSignals {
                    latency_p50_ms: st.and_then(|s| s.latency_p50_ms),
                    reliability_24h: st.and_then(|s| s.reliability_24h),
                    onion: onions.contains_key(k),
                    identity_verified: probes.get(k).and_then(|p| p.identity_verified),
                    failing: fetches.get(k).is_some_and(|f| !f.is_healthy()),
                };
                (k.clone(), sig)
            })
            .collect();
        Self {
            now: Utc::now(),
            stale_after_secs: state.stale_after.as_secs() as i64,
            ratio,
            stats,
            probes,
            onions,
            fetches,
            signals,
            medians,
            peer_caps,
        }
    }

    fn age_secs(&self, q: &Quote) -> i64 {
        (self.now - q.observed_at).num_seconds().max(0)
    }

    fn is_stale(&self, q: &Quote) -> bool {
        self.age_secs(q) > self.stale_after_secs
    }

    fn stats(&self, q: &Quote) -> Option<&EndpointStats> {
        self.stats.get(&normalize_endpoint(q.endpoint.as_deref()?))
    }

    fn endpoint_key(q: &Quote) -> Option<String> {
        q.endpoint.as_deref().map(normalize_endpoint)
    }

    fn identity_verified(&self, q: &Quote) -> Option<bool> {
        self.probes.get(&Self::endpoint_key(q)?)?.identity_verified
    }

    fn onion(&self, q: &Quote) -> Option<&String> {
        self.onions.get(&Self::endpoint_key(q)?)
    }

    fn reliability(&self, q: &Quote) -> Option<f64> {
        self.stats(q).and_then(|s| s.reliability_1h)
    }

    /// Routable right now: the endpoint's last catalog fetch succeeded, it
    /// succeeds ≥ 90% of the time (24h, else 1h right after a restart), its
    /// `/v1/info` probe isn't failing, and the quote isn't stale. Needs
    /// current evidence, so a snapshot served with `--no-poll` has none.
    fn is_healthy(&self, q: &Quote) -> bool {
        let Some(key) = Self::endpoint_key(q) else {
            return false;
        };
        let st = self.stats.get(&key);
        self.fetches.get(&key).is_some_and(FetchStatus::is_healthy)
            && st
                .and_then(|s| s.reliability_24h.or(s.reliability_1h))
                .is_some_and(|r| r >= HEALTHY_MIN_RELIABILITY)
            && self.probes.get(&key).is_none_or(|p| p.ok)
            && !self.is_stale(q)
    }

    /// The quote's capabilities with unpublished flags filled from a
    /// reference catalog's listing of the same `canonical_model`, plus the
    /// names of the flags that were filled.
    fn caps(&self, q: &Quote) -> (Capabilities, Vec<&'static str>) {
        let mut caps = q.caps.clone();
        let inferred = self
            .peer_caps
            .get(&canonical_model(&q.model))
            .map(|peer| caps.fill_from(peer))
            .unwrap_or_default();
        (caps, inferred)
    }

    fn is_outlier(&self, q: &Quote) -> bool {
        let Some(b) = blended_usd(q.price_in_usd, q.price_out_usd, self.ratio)
            .filter(|b| is_rankable_blend(Some(*b)))
        else {
            return false;
        };
        self.medians
            .get(&canonical_model(&q.model))
            .is_some_and(|m| b < m / OUTLIER_FACTOR || b > m * OUTLIER_FACTOR)
    }
}

/// Lowercased, trimmed, non-empty items of a comma-separated param.
fn list_items(list: Option<&str>) -> Vec<String> {
    list.unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn truthy(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Comma-separated, case-insensitive membership (`None`/empty = match all).
fn in_list(list: Option<&str>, value: &str) -> bool {
    match list.map(str::trim).filter(|l| !l.is_empty()) {
        None => true,
        Some(l) => l
            .split(',')
            .map(str::trim)
            .any(|item| item.eq_ignore_ascii_case(value)),
    }
}

fn enrich_json(q: &Quote, ctx: &QuoteCtx) -> serde_json::Value {
    let ratio = ctx.ratio;
    let (caps, inferred) = ctx.caps(q);
    serde_json::json!({
        "source": q.source,
        "provider": q.provider,
        "provider_id": q.provider_id,
        "model": q.model,
        "canonical_model": canonical_model(&q.model),
        "price_in_usd": q.price_in_usd,
        "price_out_usd": q.price_out_usd,
        "price_in_sats": q.price_in_sats,
        "price_out_sats": q.price_out_sats,
        "blended_usd": blended_usd(q.price_in_usd, q.price_out_usd, ratio),
        "blend_ratio": ratio,
        "delta_out_pct": delta_pct(q.price_out_usd, q.prev_price_out_usd),
        "delta_in_pct": delta_pct(q.price_in_usd, q.prev_price_in_usd),
        "prev_price_out_usd": q.prev_price_out_usd,
        "prev_price_in_usd": q.prev_price_in_usd,
        "endpoint": q.endpoint,
        "region": q.region,
        "context_length": q.context_length,
        "observed_at": q.observed_at,
        "node": node_display_name(q),
        "kind": q.kind,
        "age_secs": ctx.age_secs(q),
        "stale": ctx.is_stale(q),
        "reliability_1h": ctx.reliability(q),
        "reliability_24h": ctx.stats(q).and_then(|s| s.reliability_24h),
        "reliability_7d": ctx.stats(q).and_then(|s| s.reliability_7d),
        "latency_p50_ms": ctx.stats(q).and_then(|s| s.latency_p50_ms),
        "latency_p95_ms": ctx.stats(q).and_then(|s| s.latency_p95_ms),
        "latency_source": ctx.stats(q).and_then(|s| s.latency_source),
        "identity_verified": ctx.identity_verified(q),
        "onion": ctx.onion(q),
        "outlier": ctx.is_outlier(q),
        "healthy": ctx.is_healthy(q),
        "tools": caps.tools,
        "reasoning": caps.reasoning,
        "vision": caps.vision,
        "input_modalities": (!caps.input_modalities.is_empty()).then_some(&caps.input_modalities),
        "max_output_tokens": caps.max_output_tokens,
        "capabilities_inferred": inferred,
    })
}

fn filter_quotes(quotes: Vec<Quote>, q: &QuotesQuery, ctx: &QuoteCtx) -> Vec<Quote> {
    let exclude_stale = truthy(q.exclude_stale.as_deref());
    let exclude_outliers = truthy(q.exclude_outliers.as_deref());
    let healthy_only = truthy(q.healthy.as_deref());
    let required_caps = list_items(q.capabilities.as_deref());
    quotes
        .into_iter()
        .filter(|quote| {
            if !in_list(q.source.as_deref(), &quote.source) {
                return false;
            }
            if !in_list(q.kind.as_deref(), &quote.kind) {
                return false;
            }
            if let Some(max) = q.max_age() {
                if ctx.age_secs(quote) > max {
                    return false;
                }
            }
            if exclude_stale && ctx.is_stale(quote) {
                return false;
            }
            if let Some(min) = reliability_floor(q.min_reliability) {
                if !ctx.reliability(quote).is_some_and(|r| r >= min) {
                    return false;
                }
            }
            if let Some(min) = reliability_floor(q.min_reliability_24h) {
                let rel = ctx.stats(quote).and_then(|s| s.reliability_24h);
                if !rel.is_some_and(|r| r >= min) {
                    return false;
                }
            }
            if let Some(max) = q.max_latency_ms {
                let p50 = ctx.stats(quote).and_then(|s| s.latency_p50_ms);
                if p50.is_none_or(|ms| ms > max) {
                    return false;
                }
            }
            if exclude_outliers && ctx.is_outlier(quote) {
                return false;
            }
            if healthy_only && !ctx.is_healthy(quote) {
                return false;
            }
            if !required_caps.is_empty() {
                let (caps, _) = ctx.caps(quote);
                if !required_caps.iter().all(|c| caps.flag(c) == Some(true)) {
                    return false;
                }
            }
            if let Some(ref m) = q.model {
                if !quote
                    .model
                    .to_ascii_lowercase()
                    .contains(&m.to_ascii_lowercase())
                {
                    return false;
                }
            }
            true
        })
        .collect()
}

fn fmt_usd(v: Option<f64>) -> String {
    match v {
        Some(0.0) => "free".into(),
        Some(x) if x < 0.01 => format!("${x:.6}"),
        Some(x) => format!("${x:.4}"),
        None => "—".into(),
    }
}

fn fmt_sats(v: Option<f64>) -> String {
    match v {
        Some(0.0) => "0".into(),
        Some(x) if x < 1.0 => format!("{x:.3}"),
        Some(x) => format!("{x:.1}"),
        None => "—".into(),
    }
}

fn fmt_ctx(ctx: Option<u64>) -> String {
    match ctx {
        Some(c) if c >= 1_000_000 => format!("{:.1}M", c as f64 / 1_000_000.0),
        Some(c) if c >= 1000 => format!("{}k", c / 1000),
        Some(c) => format!("{c}"),
        None => "—".into(),
    }
}

fn fmt_delta(d: Option<f64>) -> String {
    match d {
        Some(x) if x.abs() < 0.05 => r#"<span class="delta-flat">·</span>"#.into(),
        Some(x) if x > 0.0 => format!(r#"<span class="delta-up">↑{x:.1}%</span>"#),
        Some(x) => format!(r#"<span class="delta-down">↓{:.1}%</span>"#, x.abs()),
        None => r#"<span class="delta-flat">—</span>"#.into(),
    }
}

fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn fmt_pct(v: Option<f64>) -> String {
    v.map(|x| format!("{:.0}%", x * 100.0))
        .unwrap_or_else(|| "—".into())
}

fn opt_b(v: Option<bool>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn opt_u(v: Option<u64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn opt_f(v: Option<f64>) -> String {
    v.map(|x| format!("{x}")).unwrap_or_default()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn esc_attr(s: &str) -> String {
    esc(s).replace('\'', "&#39;")
}

fn short(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration as StdDuration;

    use crate::store::Store;

    fn quote(model: &str, endpoint: &str, out: f64, age_secs: i64, kind: &str) -> Quote {
        Quote {
            source: "routstr".into(),
            provider: "p".into(),
            provider_id: endpoint.into(),
            model: model.into(),
            price_in_usd: Some(out),
            price_out_usd: Some(out),
            price_in_sats: None,
            price_out_sats: None,
            prev_price_out_usd: None,
            prev_price_in_usd: None,
            endpoint: Some(endpoint.into()),
            region: None,
            context_length: None,
            observed_at: Utc::now() - chrono::Duration::seconds(age_secs),
            raw_kind: None,
            kind: kind.into(),
            caps: Default::default(),
        }
    }

    fn state() -> AppState {
        AppState {
            store: Arc::new(Store::new()),
            stale_after: StdDuration::from_secs(600),
            polling: true,
            poll_interval: StdDuration::from_secs(60),
            analytics: None,
        }
    }

    fn query(pairs: &str) -> QuotesQuery {
        let uri: axum::http::Uri = format!("/?{pairs}").parse().unwrap();
        Query::<QuotesQuery>::try_from_uri(&uri).unwrap().0
    }

    #[test]
    fn flags_low_outlier_against_cross_source_median() {
        let all = vec![
            quote("m", "https://a", 2.0, 0, "chat"),
            quote("m", "https://b", 2.2, 0, "chat"),
            quote("m", "https://c", 1.8, 0, "chat"),
            quote("m", "https://d", 0.01, 0, "chat"),
        ];
        let ctx = QuoteCtx::new(&state(), &all, 3.0);
        let flags: Vec<_> = all.iter().map(|q| ctx.is_outlier(q)).collect();
        assert_eq!(flags, [false, false, false, true]);
    }

    #[test]
    fn too_few_peers_never_outlier() {
        let all = vec![
            quote("m", "https://a", 2.0, 0, "chat"),
            quote("m", "https://d", 0.01, 0, "chat"),
        ];
        let ctx = QuoteCtx::new(&state(), &all, 3.0);
        assert!(!ctx.is_outlier(&all[1]));
    }

    #[test]
    fn filters_kind_stale_age_source_and_reliability() {
        let st = state();
        st.store.record_poll("https://good", true, Some(10));
        st.store.record_poll("https://bad", false, None);
        let all = vec![
            quote("fresh-chat", "https://good", 1.0, 5, "chat"),
            quote("old-chat", "https://good", 1.0, 3600, "chat"),
            quote("embed", "https://good", 1.0, 5, "embedding"),
            quote("flaky", "https://bad", 1.0, 5, "chat"),
            quote("unpolled", "https://new", 1.0, 5, "chat"),
        ];
        let ctx = QuoteCtx::new(&st, &all, 3.0);
        let models = |qs: &str| -> Vec<String> {
            let mut v: Vec<_> = filter_quotes(all.clone(), &query(qs), &ctx)
                .into_iter()
                .map(|q| q.model)
                .collect();
            v.sort();
            v
        };
        assert_eq!(models("kind=embedding"), ["embed"]);
        assert_eq!(models("kind=chat&exclude_stale=1").len(), 3);
        assert!(!models("max_age_secs=60").contains(&"old-chat".to_string()));
        assert_eq!(
            models("kind=chat&min_reliability=0.5"),
            ["fresh-chat", "old-chat"]
        );
        assert_eq!(models("source=openrouter,ROUTSTR").len(), 5);
        assert!(models("source=openrouter").is_empty());
    }

    #[test]
    fn enrich_json_adds_freshness_fields() {
        let st = state();
        st.store.record_poll("https://good", true, Some(10));
        let q = quote("m", "https://good/", 1.0, 900, "chat");
        let ctx = QuoteCtx::new(&st, std::slice::from_ref(&q), 3.0);
        let v = enrich_json(&q, &ctx);
        assert_eq!(v["kind"], "chat");
        assert_eq!(v["stale"], true);
        assert_eq!(v["reliability_1h"], 1.0);
        assert_eq!(v["outlier"], false);
        assert!(v["age_secs"].as_i64().unwrap() >= 900);
    }

    #[test]
    fn filters_on_24h_reliability_and_latency() {
        let st = state();
        for _ in 0..3 {
            st.store.record_poll("https://fast", true, Some(80));
        }
        st.store.record_poll("https://slow", true, Some(3000));
        st.store.record_poll("https://slow", false, None);
        let all = vec![
            quote("a", "https://fast", 1.0, 5, "chat"),
            quote("b", "https://slow", 1.0, 5, "chat"),
            quote("c", "https://none", 1.0, 5, "chat"),
        ];
        let ctx = QuoteCtx::new(&st, &all, 3.0);
        let models = |qs: &str| -> Vec<String> {
            filter_quotes(all.clone(), &query(qs), &ctx)
                .into_iter()
                .map(|q| q.model)
                .collect()
        };
        assert_eq!(models("max_latency_ms=500"), ["a"]);
        assert_eq!(models("min_reliability_24h=0.9"), ["a"]);
        let v = enrich_json(&all[1], &ctx);
        assert_eq!(v["reliability_24h"], 0.5);
        assert_eq!(v["latency_p50_ms"], 3150);
        assert_eq!(v["latency_source"], "catalog");
        assert_eq!(v["identity_verified"], serde_json::Value::Null);
    }

    const XSS: &str = "model=%22%3E%3Cscript%3Ealert(document.domain)%3C/script%3E&kind=%3Cb%3E";

    #[test]
    fn qs_percent_encodes_markup() {
        let qs = query(XSS).qs(&[("preset", Some("frontier"))]);
        assert!(!qs.contains(['"', '<', '>', '\'', ' ']), "{qs}");
        assert!(qs.contains("model=%22%3E%3Cscript%3Ealert%28document.domain%29"));
        assert_eq!(percent_encode("chat,embedding"), "chat,embedding");
        assert_eq!(percent_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }

    #[tokio::test]
    async fn dashboard_does_not_reflect_script_payload() {
        let resp = dashboard(State(state()), Query(query(XSS)))
            .await
            .into_response();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("<script>alert"), "payload reflected raw");
        assert!(!html.contains("\"><script>"), "attribute broken out of");
        assert!(!html.contains("<b>"), "kind payload reflected raw");
        // The filter is still carried through links, encoded.
        assert!(html.contains("model=%22%3E%3Cscript%3E"));
    }

    #[test]
    fn out_of_range_filters_are_normalized() {
        let st = state();
        st.store.record_poll("https://good", true, Some(10));
        let all = vec![
            quote("polled", "https://good", 1.0, 5, "chat"),
            quote("unpolled", "https://none", 1.0, 5, "chat"),
        ];
        let ctx = QuoteCtx::new(&st, &all, 3.0);
        let n = |qs: &str| filter_quotes(all.clone(), &query(qs), &ctx).len();
        // ≤ 0 / NaN floors and negative ages mean "no filter".
        assert_eq!(n("min_reliability=-1"), 2);
        assert_eq!(n("min_reliability=0"), 2);
        assert_eq!(n("min_reliability=NaN"), 2);
        assert_eq!(n("max_age_secs=-5"), 2);
        // > 1 clamps to 1 (perfect endpoints only), not "nothing".
        assert_eq!(n("min_reliability=5"), 1);
        assert_eq!(n("min_reliability_24h=1.5"), 1);
    }

    #[tokio::test]
    async fn api_limit_allows_the_whole_board() {
        let st = state();
        st.store
            .upsert_quotes((0..2500).map(|i| quote(&format!("m{i}"), "https://n", 1.0, 5, "chat")));
        let rows = |qs: &str| {
            let st = st.clone();
            let q = query(qs);
            async move {
                let resp = api_quotes(State(st), Query(q)).await.into_response();
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                serde_json::from_slice::<Vec<serde_json::Value>>(&body)
                    .unwrap()
                    .len()
            }
        };
        assert_eq!(rows("").await, 200);
        assert_eq!(rows("limit=5000").await, 2500);
        assert_eq!(rows("limit=999999").await, 2500);
    }

    #[test]
    fn qs_carries_quality_filters() {
        let q = query("kind=chat&exclude_stale=1&min_reliability=0.5");
        let qs = q.qs(&[]);
        assert!(qs.contains("kind=chat"));
        assert!(qs.contains("exclude_stale=1"));
        assert!(qs.contains("min_reliability=0.5"));
        // Quality toggles can be switched off without dropping other filters.
        let off = q.qs(&[("exclude_stale", None)]);
        assert!(!off.contains("exclude_stale") && off.contains("kind=chat"));
    }

    #[test]
    fn healthy_needs_a_current_reliable_fetch() {
        let st = state();
        let all = vec![
            quote("m/good", "https://good", 1.0, 5, "chat"),
            quote("m/down", "https://down", 1.0, 5, "chat"),
            quote("m/flaky", "https://flaky", 1.0, 5, "chat"),
            quote("m/old", "https://good", 1.0, 3600, "chat"),
            quote("m/unpolled", "https://new", 1.0, 5, "chat"),
        ];
        for (ep, oks) in [("https://good", [true; 10]), ("https://down", [true; 10])] {
            for ok in oks {
                st.store.record_poll(ep, ok, Some(100));
            }
        }
        for i in 0..10 {
            st.store.record_poll("https://flaky", i % 2 == 0, Some(100));
        }
        st.store
            .record_fetch("routstr", "good", "https://good", Ok(2));
        st.store
            .record_fetch("routstr", "flaky", "https://flaky", Ok(1));
        st.store
            .record_fetch("routstr", "down", "https://down", Err("dns error".into()));
        let ctx = QuoteCtx::new(&st, &all, 3.0);
        let flags: Vec<bool> = all.iter().map(|q| ctx.is_healthy(q)).collect();
        assert_eq!(flags, [true, false, false, false, false]);
        assert_eq!(enrich_json(&all[0], &ctx)["healthy"], true);

        let kept = filter_quotes(all.clone(), &query("healthy=1"), &ctx);
        assert_eq!(kept.len(), 1);
        assert_eq!(filter_quotes(all, &query("healthy=0"), &ctx).len(), 5);
    }

    #[test]
    fn node_status_reflects_skip_and_fetch_streak() {
        let store = Store::new();
        assert_eq!(node_status(false, None), "pending");
        assert_eq!(node_status(true, None), "skipped");
        store.record_fetch("routstr", "n", "https://n", Err("x".into()));
        let f = store.fetch_statuses().remove(0);
        assert_eq!(node_status(false, Some(&f)), "failing");
        for _ in 1..crate::store::DORMANT_AFTER_FAILURES {
            store.record_fetch("routstr", "n", "https://n", Err("x".into()));
        }
        let f = store.fetch_statuses().remove(0);
        assert_eq!(node_status(false, Some(&f)), "dormant");
        store.record_fetch("routstr", "n", "https://n", Ok(1));
        let f = store.fetch_statuses().remove(0);
        assert_eq!(node_status(false, Some(&f)), "healthy");
    }

    #[test]
    fn fmt_age_picks_one_unit() {
        assert_eq!(fmt_age(42), "42s");
        assert_eq!(fmt_age(7 * 60 + 5), "7m");
        assert_eq!(fmt_age(3 * 3600), "3h");
        assert_eq!(fmt_age(2 * 86_400 + 1), "2d");
    }

    #[test]
    fn feed_strip_summarizes_and_escapes() {
        let feed = serde_json::json!({
            "status": "degraded",
            "reasons": ["<b>no Routstr node is answering</b>"],
            "stale_quotes": 3,
            "last_cycle": {"finished_at": Utc::now().to_rfc3339(), "elapsed_ms": 1500},
            "sources": {
                "openrouter": {"endpoints": 1, "healthy": 1, "models": 400, "last_error": null},
                "routstr": {"endpoints": 4, "healthy": 1, "models": 9, "last_error": "x\"><script>"},
            },
            "discovery": {"lag_secs": 90},
        });
        let html = feed_strip_html(&feed, Some(30));
        assert!(html.contains("feed degraded"));
        assert!(html.contains("(1.5s)"));
        assert!(html.contains("openrouter ✓ 400"));
        assert!(html.contains("routstr 1/4 nodes OK"));
        assert!(html.contains("3 stale quotes"));
        assert!(html.contains("BTC rate 30s old"));
        assert!(html.contains("last announcement 1m ago"));
        assert!(!html.contains("<b>") && !html.contains("\"><script>"));
    }

    #[tokio::test]
    async fn board_shows_reliability_latency_and_age() {
        let st = state();
        st.store.upsert_quotes([
            quote("m/fresh", "https://a", 1.0, 5, "chat"),
            quote("m/old", "https://b", 2.0, 3600, "chat"),
        ]);
        st.store.record_poll("https://a", true, Some(120));
        let resp = dashboard(State(st), Query(query(""))).await.into_response();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Rel 24h") && html.contains(">Latency<") && html.contains(">Age<"));
        assert!(html.contains(r#"<span class="rel-txt rel-good""#));
        assert!(html.contains(r#"<span class="tag tag-stale">1h</span>"#));
        assert!(html.contains("hide stale <span>1</span>"));
        assert!(html.contains(r#"class="feed""#));
    }

    fn src(source: &str, mut q: Quote) -> Quote {
        q.source = source.into();
        q
    }

    async fn html_of(resp: Response) -> String {
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[test]
    fn compare_joins_spellings_across_sources() {
        let st = state();
        let all = vec![
            src(
                "openrouter",
                quote("anthropic/claude-sonnet-4.5", "https://or", 3.0, 5, "chat"),
            ),
            quote(
                "claude-sonnet-4-5-20250929",
                "https://cheap",
                1.5,
                5,
                "chat",
            ),
            quote("claude-sonnet-4-5", "https://ok", 2.0, 5, "chat"),
            // Unit bug: an outlier must not become the cheapest offer.
            quote("claude-sonnet-4.5", "https://bug", 0.001, 5, "chat"),
            quote("claude-sonnet-4-5:free", "https://ok", 0.0, 5, "chat"),
            quote("claude-sonnet-4", "https://ok", 1.0, 5, "chat"),
            src(
                "orcarouter",
                quote("deepseek/deepseek-v4", "https://orca", 0.2, 5, "chat"),
            ),
        ];
        for _ in 0..10 {
            st.store.record_poll("https://ok", true, Some(100));
        }
        st.store.record_fetch("routstr", "ok", "https://ok", Ok(3));
        let ctx = QuoteCtx::new(&st, &all, 3.0);

        let groups = compare_groups(all.clone(), &query("model=claude-sonnet-4.5"), &ctx);
        let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, ["claude-sonnet-4-5", "claude-sonnet-4-5:free"]);
        let g = compare_json(&groups[0], &ctx);
        assert_eq!(g["offers"], 4);
        assert_eq!(g["sources"], serde_json::json!(["openrouter", "routstr"]));
        assert_eq!(g["min_blended_usd"], 1.5);
        assert_eq!(g["max_blended_usd"], 3.0);
        assert_eq!(g["spread_pct"], 100.0);
        assert_eq!(g["cheapest"]["endpoint"], "https://cheap");
        assert_eq!(g["cheapest_healthy"]["endpoint"], "https://ok");
        let vs: Vec<_> = g["quotes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|q| q["vs_cheapest_pct"].as_f64().unwrap().round())
            .collect();
        assert_eq!(vs, [-100.0, 0.0, 33.0, 100.0]);
        assert_eq!(g["quotes"][0]["outlier"], true);
        assert_eq!(g["quotes"][0]["canonical_model"], "claude-sonnet-4-5");

        // No needle: models offered by the most sources first.
        let groups = compare_groups(all.clone(), &query(""), &ctx);
        assert_eq!(groups[0].key, "claude-sonnet-4-5");
        // Board filters still apply.
        let groups = compare_groups(all, &query("model=claude&source=openrouter"), &ctx);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].quotes.len(), 1);
    }

    #[test]
    fn spread_and_median_need_enough_offers() {
        assert_eq!(spread_pct(&[2.0]), None);
        assert_eq!(spread_pct(&[2.0, 3.0]), Some(50.0));
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[1.0, 2.0, 4.0]), Some(2.0));
        assert_eq!(median(&[1.0, 2.0, 4.0, 8.0]), Some(3.0));
    }

    #[test]
    fn outliers_are_judged_across_catalog_spellings() {
        let all = vec![
            src(
                "openrouter",
                quote("anthropic/claude-sonnet-4.5", "https://or", 3.0, 0, "chat"),
            ),
            quote("claude-sonnet-4-5", "https://a", 3.1, 0, "chat"),
            quote("claude-sonnet-4-5-20250929", "https://b", 2.9, 0, "chat"),
            quote("claude-sonnet-4.5", "https://c", 0.003, 0, "chat"),
        ];
        let ctx = QuoteCtx::new(&state(), &all, 3.0);
        let flags: Vec<_> = all.iter().map(|q| ctx.is_outlier(q)).collect();
        assert_eq!(flags, [false, false, false, true]);
    }

    #[tokio::test]
    async fn compare_page_escapes_and_board_links_to_it() {
        let st = state();
        st.store.upsert_quotes([quote(
            "anthropic/claude-sonnet-4.5",
            "https://a",
            1.0,
            5,
            "chat",
        )]);
        let html = html_of(
            compare_page(State(st.clone()), Query(query(XSS)))
                .await
                .into_response(),
        )
        .await;
        assert!(!html.contains("<script>alert"), "payload reflected raw");
        assert!(!html.contains("\"><script>"), "attribute broken out of");
        assert!(!html.contains("<b>"), "kind payload reflected raw");
        assert!(html.contains(r#"name="kind" value="&lt;b&gt;""#));

        let html = html_of(
            compare_page(
                State(st.clone()),
                Query(query("model=Claude-Sonnet-4.5&ratio=4")),
            )
            .await
            .into_response(),
        )
        .await;
        assert!(html.contains("<h2>claude-sonnet-4-5 <span"));
        assert!(html.contains(r#"<tr class="quote-row cmp-best""#));
        assert!(html.contains(r#"href="/compare?model=Claude-Sonnet-4.5&amp;ratio=2""#));
        assert!(
            !html.contains(">compare</a>"),
            "no self link on the compare page"
        );

        let html = html_of(
            dashboard(State(st), Query(query("ratio=4")))
                .await
                .into_response(),
        )
        .await;
        assert!(html.contains(r#"<a href="/compare?model=claude-sonnet-4-5&amp;ratio=4""#));
    }

    fn with_caps(mut q: Quote, tools: Option<bool>, vision: Option<bool>) -> Quote {
        q.caps.tools = tools;
        q.caps.vision = vision;
        q
    }

    #[tokio::test]
    async fn capabilities_are_inferred_filtered_and_shown() {
        let st = state();
        let all = vec![
            // OpenRouter publishes tools; the node only its modalities.
            with_caps(
                src(
                    "openrouter",
                    quote("anthropic/claude-sonnet-4.5", "https://or", 3.0, 5, "chat"),
                ),
                Some(true),
                Some(true),
            ),
            with_caps(
                quote("claude-sonnet-4-5", "https://node", 2.0, 5, "chat"),
                None,
                Some(false),
            ),
            with_caps(
                quote("mystery-model", "https://node", 1.0, 5, "chat"),
                None,
                None,
            ),
        ];
        let ctx = QuoteCtx::new(&st, &all, 3.0);
        let node = enrich_json(&all[1], &ctx);
        assert_eq!(node["tools"], true);
        // The node's own published value wins over the reference catalog.
        assert_eq!(node["vision"], false);
        assert_eq!(node["capabilities_inferred"], serde_json::json!(["tools"]));
        let or = enrich_json(&all[0], &ctx);
        assert_eq!(or["capabilities_inferred"], serde_json::json!([]));
        let unknown = enrich_json(&all[2], &ctx);
        assert!(unknown["tools"].is_null() && unknown["input_modalities"].is_null());

        let models = |qs: &str| -> Vec<String> {
            filter_quotes(all.clone(), &query(qs), &ctx)
                .into_iter()
                .map(|q| q.model)
                .collect()
        };
        assert_eq!(models("capabilities=tools").len(), 2);
        assert_eq!(
            models("capabilities=Tools,%20vision"),
            ["anthropic/claude-sonnet-4.5"]
        );
        assert!(models("capabilities=telepathy").is_empty());
        assert_eq!(models("capabilities=").len(), 3);
        assert!(query("capabilities=tools")
            .qs(&[])
            .contains("capabilities=tools"));

        st.store.upsert_quotes(all);
        let html = html_of(
            dashboard(State(st.clone()), Query(query("")))
                .await
                .into_response(),
        )
        .await;
        assert!(html.contains(r#"<span class="tag tag-cap tag-inferred""#));
        assert!(html.contains(r#"href="/?capabilities=tools">tools <span>2</span></a>"#));
        let html = html_of(
            dashboard(State(st.clone()), Query(query("capabilities=tools,vision")))
                .await
                .into_response(),
        )
        .await;
        assert!(html.contains(r#"<a class="pill active" href="/?capabilities=vision">tools"#));

        let csv = html_of(export_csv(State(st), Query(query(""))).await).await;
        let mut lines = csv.lines();
        let header = lines.next().unwrap().split(',').count();
        for line in lines {
            assert_eq!(line.split(',').count(), header, "{line}");
        }
        assert!(csv.contains(",true,,false,,tools\n"));
    }
}
