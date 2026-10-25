//! HTTP handlers: HTML dashboard + JSON API + CSV export.

use std::collections::{BTreeMap, HashMap};

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::feed::feed_status;
use super::AppState;
use crate::market::{apply_preset, best_now, blended_usd, delta_pct, Preset, DEFAULT_BLEND_RATIO};
use crate::pricing::{blend_sort_key, is_rankable_blend};
use crate::store::{normalize_endpoint, EndpointStats, Quote};

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

        for (k, v) in overrides {
            match *k {
                "source" => source = *v,
                "model" => model = *v,
                "preset" => preset = *v,
                "ratio" => ratio = v.map(|s| s.to_string()),
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
        set("exclude_stale", self.exclude_stale.as_deref());
        set("exclude_outliers", self.exclude_outliers.as_deref());

        if pairs.is_empty() {
            return String::new();
        }
        let body = pairs
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencoding_minimal(v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("?{body}")
    }
}

fn urlencoding_minimal(s: &str) -> String {
    s.replace(' ', "%20")
        .replace('&', "%26")
        .replace('=', "%3D")
}

/// Apply source/model/quality/preset filters and sort by blend (unless preset sorts itself).
fn resolve_view(all: Vec<Quote>, q: &QuotesQuery, ctx: &QuoteCtx) -> Vec<Quote> {
    let ratio = q.blend_ratio();
    let mut filtered = filter_quotes(all, q, ctx);
    if let Some(p) = q.preset() {
        filtered = apply_preset(filtered, p);
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
    let limit = q.limit.unwrap_or(200).min(2000);
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
         latency_p50_ms,latency_p95_ms\n",
    );
    for quote in quotes.into_iter().take(limit) {
        let blend = blended_usd(quote.price_in_usd, quote.price_out_usd, ratio);
        let d = delta_pct(quote.price_out_usd, quote.prev_price_out_usd);
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
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
                        <div class="sub muted">updated {updated}</div>
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
                    updated = b.quote.observed_at.format("%H:%M:%S UTC"),
                    blend = fmt_usd(Some(b.blended_usd)),
                    node = esc(&node_label),
                    io_in = fmt_usd(b.quote.price_in_usd),
                    io_out = fmt_usd(b.quote.price_out_usd),
                    source = esc(&b.quote.source),
                    delta = fmt_delta(d),
                    actions = row_actions_html(),
                )
            })
            .collect()
    };

    let ctx = QuoteCtx::new(&state, &all, ratio);
    let mut filtered = resolve_view(all.clone(), &q, &ctx);
    let total_filtered = filtered.len();
    filtered.truncate(150);

    let mut by_source: BTreeMap<String, usize> = BTreeMap::new();
    for quote in &all {
        *by_source.entry(quote.source.clone()).or_default() += 1;
    }

    let rows: String = if filtered.is_empty() {
        r#"<tr><td colspan="11" class="empty">No quotes match this filter…</td></tr>"#.into()
    } else {
        filtered
            .iter()
            .map(|quote| quote_row(quote, ratio))
            .collect()
    };

    let node_stats = state.store.endpoint_stats();
    let node_rows: String = if nodes.is_empty() {
        r#"<tr><td colspan="7" class="empty">No Routstr nodes discovered yet (kind 38421)…</td></tr>"#
            .into()
    } else {
        nodes
            .iter()
            .map(|n| {
                let st = node_stats.get(&normalize_endpoint(&n.endpoint));
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
                    via = esc(&n.discovered_via),
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
                href = href,
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
                href = href,
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
            let href = q.qs(&[("ratio", Some(&format!("{r}")))]);
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
    let feed_label = format!("feed {}", feed["status"].as_str().unwrap_or("unknown"));
    let feed_title = feed["reasons"]
        .as_array()
        .map(|r| {
            r.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "Feed health (see /health)".into());

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
    :root {{
      --bg: #0b0f14;
      --panel: #121820;
      --border: #1e2a38;
      --text: #e6edf3;
      --muted: #8b9cb3;
      --accent: #3dffa8;
      --warn: #ffb020;
      --blue: #5eb1ff;
      --down: #3dffa8;
      --up: #ff6b6b;
      --best: #1a2e24;
      font-family: "IBM Plex Mono", "SF Mono", ui-monospace, monospace;
    }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0; padding: 1.5rem 2rem 3rem;
      background: var(--bg); color: var(--text);
      min-height: 100vh;
    }}
    header {{
      display: flex; flex-wrap: wrap; align-items: baseline;
      gap: 0.75rem 1.5rem; margin-bottom: 1rem;
      border-bottom: 1px solid var(--border); padding-bottom: 1rem;
    }}
    h1 {{
      margin: 0; font-size: 1.25rem; font-weight: 600;
      letter-spacing: 0.04em;
    }}
    h1 span {{ color: var(--accent); }}
    h2 {{
      margin: 1.5rem 0 0.65rem; font-size: 0.8rem; font-weight: 500;
      color: var(--muted); text-transform: uppercase; letter-spacing: 0.08em;
      display: flex; flex-wrap: wrap; align-items: baseline; gap: 0.5rem 1rem;
    }}
    h2 .hint {{ font-weight: 400; text-transform: none; letter-spacing: 0; }}
    h2 .export {{
      margin-left: auto; font-weight: 400; text-transform: none; letter-spacing: 0;
      font-size: 0.75rem;
    }}
    .meta {{ color: var(--muted); font-size: 0.85rem; }}
    .badge {{
      display: inline-block; padding: 0.15rem 0.5rem;
      border: 1px solid var(--border); border-radius: 999px;
      color: var(--accent); font-size: 0.75rem;
    }}
    .pills {{ display: flex; flex-wrap: wrap; gap: 0.4rem; margin: 0.45rem 0; align-items: center; }}
    .pills .label {{ color: var(--muted); font-size: 0.7rem; text-transform: uppercase; letter-spacing: 0.06em; margin-right: 0.25rem; }}
    .pill {{
      display: inline-flex; align-items: center; gap: 0.35rem;
      padding: 0.25rem 0.65rem; border: 1px solid var(--border);
      border-radius: 999px; color: var(--muted); text-decoration: none;
      font-size: 0.75rem; background: var(--panel);
    }}
    .pill span {{ color: var(--accent); }}
    .pill.active {{ border-color: var(--accent); color: var(--text); }}
    .pill:hover {{ border-color: var(--muted); }}
    table {{
      width: 100%; border-collapse: collapse;
      background: var(--panel); border: 1px solid var(--border);
      border-radius: 8px; overflow: hidden;
    }}
    th, td {{
      text-align: left; padding: 0.5rem 0.7rem;
      border-bottom: 1px solid var(--border); font-size: 0.8rem;
      vertical-align: top;
    }}
    th {{
      color: var(--muted); font-weight: 500; font-size: 0.68rem;
      text-transform: uppercase; letter-spacing: 0.06em;
      background: #0e141c;
    }}
    tr:last-child td {{ border-bottom: none; }}
    tr:hover td {{ background: #151d28; }}
    tr.best-row td {{ background: var(--best); }}
    tr.best-row:hover td {{ background: #20352c; }}
    .num {{ font-variant-numeric: tabular-nums; color: var(--accent); }}
    .num-sats {{ font-variant-numeric: tabular-nums; color: var(--warn); font-size: 0.75rem; }}
    .blend {{ font-variant-numeric: tabular-nums; color: var(--blue); font-weight: 600; }}
    .stack-price {{ font-size: 0.95rem; font-weight: 700; }}
    .sub {{ font-size: 0.7rem; margin-top: 0.15rem; line-height: 1.3; }}
    .node-name {{ color: var(--warn); max-width: 220px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }}
    .model-id {{ word-break: break-all; }}
    .muted {{ color: var(--muted); }}
    .empty {{ text-align: center; color: var(--muted); padding: 2rem; }}
    code {{ font-size: 0.75rem; color: var(--warn); }}
    .src {{
      display: inline-block; padding: 0.1rem 0.4rem; border-radius: 4px;
      font-size: 0.68rem; border: 1px solid var(--border);
    }}
    .src-openrouter, .src-orcarouter {{ color: var(--blue); }}
    .src-routstr {{ color: var(--accent); }}
    .src-nostr {{ color: var(--warn); }}
    .family {{
      color: var(--accent); font-size: 0.75rem; font-weight: 600;
      letter-spacing: 0.02em;
    }}
    .delta-up {{ color: var(--up); font-variant-numeric: tabular-nums; }}
    .delta-down {{ color: var(--down); font-variant-numeric: tabular-nums; }}
    .delta-flat {{ color: var(--muted); }}
    .rel {{
      position: relative; display: inline-flex; align-items: center;
      width: 72px; height: 18px; background: #0e141c;
      border: 1px solid var(--border); border-radius: 4px; overflow: hidden;
    }}
    .rel-bar {{
      position: absolute; left: 0; top: 0; bottom: 0;
      background: var(--accent); opacity: 0.35;
    }}
    .rel-good .rel-bar {{ background: var(--accent); }}
    .rel-mid .rel-bar {{ background: var(--warn); }}
    .rel-bad .rel-bar {{ background: var(--up); }}
    .rel-none .rel-bar {{ background: var(--border); width: 0 !important; }}
    .rel-label {{
      position: relative; z-index: 1; width: 100%; text-align: center;
      font-size: 0.68rem; color: var(--text);
    }}
    .actions-cell {{ position: relative; min-width: 4.5rem; white-space: nowrap; }}
    .row-actions {{
      display: none; gap: 0.3rem; flex-wrap: wrap;
    }}
    tr.quote-row:hover .row-actions,
    tr.quote-row:focus-within .row-actions {{
      display: inline-flex;
    }}
    .row-actions button {{
      cursor: pointer; border: 1px solid var(--border); background: #0e141c;
      color: var(--muted); font: inherit; font-size: 0.65rem;
      padding: 0.15rem 0.4rem; border-radius: 4px;
    }}
    .row-actions button:hover {{ color: var(--accent); border-color: var(--accent); }}
    .toast {{
      position: fixed; bottom: 1.25rem; right: 1.25rem;
      background: var(--panel); border: 1px solid var(--accent);
      color: var(--text); padding: 0.55rem 0.9rem; border-radius: 8px;
      font-size: 0.8rem; opacity: 0; pointer-events: none;
      transition: opacity 0.2s; z-index: 50;
    }}
    .toast.show {{ opacity: 1; }}
    footer {{
      margin-top: 1.25rem; color: var(--muted); font-size: 0.75rem;
    }}
    a {{ color: var(--accent); }}
    .links a {{ margin-right: 0.75rem; }}
  </style>
  <!-- Privacy-friendly analytics by Plausible -->
  <script async src="https://plausible.io/js/pa-04Cyl_CguuqqiZxs7vLO7.js"></script>
  <script>
    window.plausible=window.plausible||function(){{(plausible.q=plausible.q||[]).push(arguments)}},plausible.init=plausible.init||function(i){{plausible.o=i||{{}}}};
    plausible.init()
  </script>
</head>
<body>
  <header>
    <h1><span>token</span>stats</h1>
    <span class="badge">LIVE</span>
    <span class="meta">{quotes} quotes · {nodes} nodes · {btc}{filter_note}</span>
    <span class="meta" title="Freshest quote observation vs page render">{last_label} · {page_label}</span>
    <span class="meta" title="{feed_title}">{feed_label}</span>
    <span class="meta links">
      <a href="{csv_href}">Export CSV</a>
      <a href="/api/summary">/api/summary</a>
      <a href="/api/quotes">/api/quotes</a>
      <a href="/health">/health</a>
    </span>
  </header>

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
        <th>Updated</th>
      </tr>
    </thead>
    <tbody>
      {rows}
    </tbody>
  </table>

  <h2>Nodes <span class="hint">reliability = successful polls / total (last hour; hover for 24h / 7d) · latency = catalog fetch, 24h</span></h2>
  <table>
    <thead>
      <tr>
        <th>Name</th>
        <th>Provider id</th>
        <th>Endpoint</th>
        <th>Region</th>
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
    Sovereign inference market observability · OpenRouter + Routstr · MPL-2.0
    · auto-refresh 15s · last data {last_label}
  </footer>
  <div id="toast" class="toast" role="status"></div>
  <script>
    function showToast(msg) {{
      const t = document.getElementById('toast');
      t.textContent = msg;
      t.classList.add('show');
      clearTimeout(window.__toastTimer);
      window.__toastTimer = setTimeout(() => t.classList.remove('show'), 1800);
    }}
    function baseUrl(row) {{
      let ep = row.dataset.endpoint || '';
      if (!ep) {{
        if (row.dataset.source === 'openrouter') ep = 'https://openrouter.ai/api/v1';
        else ep = 'https://api.openai.com/v1';
      }}
      ep = ep.replace(/\/$/, '');
      if (!/\/v1$/i.test(ep)) ep = ep + '/v1';
      return ep;
    }}
    function curlFor(row) {{
      const base = baseUrl(row);
      const model = row.dataset.model || '';
      const body = JSON.stringify({{
        model,
        messages: [{{ role: 'user', content: 'hello' }}]
      }});
      return `curl -sS "${{base}}/chat/completions" \\
  -H "Authorization: Bearer $API_KEY" \\
  -H "Content-Type: application/json" \\
  -d '${{body.replace(/'/g, "'\\''")}}'`;
    }}
    function openaiConfigFor(row) {{
      const base = baseUrl(row);
      const model = row.dataset.model || '';
      const provider = row.dataset.provider || '';
      return JSON.stringify({{
        provider,
        model,
        base_url: base,
        api_key_env: 'API_KEY',
        openai_python: `from openai import OpenAI\\nclient = OpenAI(base_url="${{base}}", api_key=os.environ["API_KEY"])\\nr = client.chat.completions.create(model="${{model}}", messages=[{{"role":"user","content":"hello"}}])`
      }}, null, 2);
    }}
    async function copyText(text, label) {{
      try {{
        await navigator.clipboard.writeText(text);
        showToast('Copied ' + label);
      }} catch (e) {{
        // fallback
        const ta = document.createElement('textarea');
        ta.value = text;
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        document.body.removeChild(ta);
        showToast('Copied ' + label);
      }}
    }}
    document.addEventListener('click', (ev) => {{
      const btn = ev.target.closest('[data-copy]');
      if (!btn) return;
      const row = btn.closest('tr.quote-row');
      if (!row) return;
      const kind = btn.getAttribute('data-copy');
      if (kind === 'curl') copyText(curlFor(row), 'curl');
      else if (kind === 'openai') copyText(openaiConfigFor(row), 'OpenAI config');
    }});
  </script>
</body>
</html>"#,
        quotes = all.len(),
        nodes = nodes.len(),
        btc = btc_label,
        filter_note = filter_note,
        last_label = last_label,
        feed_label = esc(&feed_label),
        feed_title = esc_attr(&feed_title),
        page_label = page_label,
        all_active = all_active,
        all_href = if all_href.is_empty() {
            "/".into()
        } else {
            all_href
        },
        csv_href = csv_href,
        source_pills = source_pills,
        preset_pills = preset_pills,
        ratio_pills = ratio_pills,
        best_rows = best_rows,
        rows = rows,
        node_rows = node_rows,
        ratio_i = ratio as i32,
    );

    ([(header::CACHE_CONTROL, "no-store")], Html(html))
}

fn quote_row(q: &Quote, ratio: f64) -> String {
    let blend = blended_usd(q.price_in_usd, q.price_out_usd, ratio);
    let d = delta_pct(q.price_out_usd, q.prev_price_out_usd);
    format!(
        r#"<tr class="quote-row" {attrs}>
          <td><span class="src src-{source}">{source}</span></td>
          <td>
            {provider}
            <div class="sub muted">{node}</div>
          </td>
          <td>
            <strong class="model-id">{model}</strong>
            {actions}
          </td>
          <td class="num">{pin_usd}</td>
          <td class="num">{pout_usd}</td>
          <td class="blend">{blend}</td>
          <td>{delta}</td>
          <td class="num-sats">{pin_sats}</td>
          <td class="num-sats">{pout_sats}</td>
          <td class="muted">{ctx}</td>
          <td class="muted" title="{at_full}">{at}</td>
        </tr>"#,
        attrs = quote_data_attrs(q),
        source = esc(&q.source),
        provider = esc(&q.provider),
        node = esc(&node_display_name(q)),
        model = esc(&q.model),
        actions = row_actions_html(),
        pin_usd = fmt_usd(q.price_in_usd),
        pout_usd = fmt_usd(q.price_out_usd),
        blend = fmt_usd(blend),
        delta = fmt_delta(d),
        pin_sats = fmt_sats(q.price_in_sats),
        pout_sats = fmt_sats(q.price_out_sats),
        ctx = fmt_ctx(q.context_length),
        at = q.observed_at.format("%H:%M:%S"),
        at_full = q.observed_at.to_rfc3339(),
    )
}

fn row_actions_html() -> String {
    r#"<div class="row-actions">
      <button type="button" data-copy="curl" title="Copy curl for chat/completions">curl</button>
      <button type="button" data-copy="openai" title="Copy OpenAI-compatible config JSON">config</button>
    </div>"#
        .into()
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

fn freshest_ts(quotes: &[Quote]) -> Option<DateTime<Utc>> {
    quotes.iter().map(|q| q.observed_at).max()
}

/// A quote is an outlier when its blend is this many times off the median.
const OUTLIER_FACTOR: f64 = 10.0;
/// Quotes for the same model needed before outliers are judged.
const OUTLIER_MIN_PEERS: usize = 3;

/// Per-request context for freshness, reliability and outlier fields/filters.
struct QuoteCtx {
    now: DateTime<Utc>,
    stale_after_secs: i64,
    ratio: f64,
    /// Normalized endpoint → reliability windows + latency.
    stats: HashMap<String, EndpointStats>,
    /// Lowercased model id → median rankable blend across all quotes.
    medians: HashMap<String, f64>,
}

impl QuoteCtx {
    fn new(state: &AppState, all: &[Quote], ratio: f64) -> Self {
        let mut blends: HashMap<String, Vec<f64>> = HashMap::new();
        for q in all {
            let b = blended_usd(q.price_in_usd, q.price_out_usd, ratio);
            if is_rankable_blend(b) {
                blends
                    .entry(q.model.to_ascii_lowercase())
                    .or_default()
                    .extend(b);
            }
        }
        let medians = blends
            .into_iter()
            .filter(|(_, v)| v.len() >= OUTLIER_MIN_PEERS)
            .map(|(k, mut v)| {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let mid = v.len() / 2;
                let m = if v.len() % 2 == 0 {
                    (v[mid - 1] + v[mid]) / 2.0
                } else {
                    v[mid]
                };
                (k, m)
            })
            .collect();
        Self {
            now: Utc::now(),
            stale_after_secs: state.stale_after.as_secs() as i64,
            ratio,
            stats: state.store.endpoint_stats(),
            medians,
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

    fn reliability(&self, q: &Quote) -> Option<f64> {
        self.stats(q).and_then(|s| s.reliability_1h)
    }

    fn is_outlier(&self, q: &Quote) -> bool {
        let Some(b) = blended_usd(q.price_in_usd, q.price_out_usd, self.ratio)
            .filter(|b| is_rankable_blend(Some(*b)))
        else {
            return false;
        };
        self.medians
            .get(&q.model.to_ascii_lowercase())
            .is_some_and(|m| b < m / OUTLIER_FACTOR || b > m * OUTLIER_FACTOR)
    }
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
    serde_json::json!({
        "source": q.source,
        "provider": q.provider,
        "provider_id": q.provider_id,
        "model": q.model,
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
        "outlier": ctx.is_outlier(q),
    })
}

fn filter_quotes(quotes: Vec<Quote>, q: &QuotesQuery, ctx: &QuoteCtx) -> Vec<Quote> {
    let exclude_stale = truthy(q.exclude_stale.as_deref());
    let exclude_outliers = truthy(q.exclude_outliers.as_deref());
    quotes
        .into_iter()
        .filter(|quote| {
            if !in_list(q.source.as_deref(), &quote.source) {
                return false;
            }
            if !in_list(q.kind.as_deref(), &quote.kind) {
                return false;
            }
            if let Some(max) = q.max_age_secs {
                if ctx.age_secs(quote) > max {
                    return false;
                }
            }
            if exclude_stale && ctx.is_stale(quote) {
                return false;
            }
            if let Some(min) = q.min_reliability {
                if !ctx.reliability(quote).is_some_and(|r| r >= min) {
                    return false;
                }
            }
            if let Some(min) = q.min_reliability_24h {
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
        }
    }

    fn state() -> AppState {
        AppState {
            store: Arc::new(Store::new()),
            stale_after: StdDuration::from_secs(600),
            polling: true,
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
    }

    #[test]
    fn qs_carries_quality_filters() {
        let q = query("kind=chat&exclude_stale=1&min_reliability=0.5");
        let qs = q.qs(&[]);
        assert!(qs.contains("kind=chat"));
        assert!(qs.contains("exclude_stale=1"));
        assert!(qs.contains("min_reliability=0.5"));
    }
}
