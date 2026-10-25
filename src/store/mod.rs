//! In-memory observation store for inference quotes and discovered nodes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::RwLock;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::pricing::sanitize_opt;

/// How long to keep poll samples for reliability scoring.
const RELIABILITY_WINDOW: Duration = Duration::hours(1);
/// Max samples retained per node (1/min * 60 ≈ 60; keep headroom).
const MAX_POLL_SAMPLES: usize = 120;

/// A single normalized price observation (provider catalog or Nostr node).
#[derive(Debug, Clone, Serialize)]
pub struct Quote {
    /// Origin of this row: `openrouter`, `routstr`, `nostr`, …
    pub source: String,
    /// Human-readable provider / node name.
    pub provider: String,
    /// Stable provider key (pubkey hex, openrouter, host, …).
    pub provider_id: String,
    pub model: String,
    /// USD per 1M input tokens (normalized).
    pub price_in_usd: Option<f64>,
    /// USD per 1M output tokens (normalized).
    pub price_out_usd: Option<f64>,
    /// Sats per 1M input tokens (normalized, when BTC rate known).
    pub price_in_sats: Option<f64>,
    /// Sats per 1M output tokens.
    pub price_out_sats: Option<f64>,
    /// Previous out price (USD) before last upsert — for delta indicators.
    pub prev_price_out_usd: Option<f64>,
    /// Previous in price (USD).
    pub prev_price_in_usd: Option<f64>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub context_length: Option<u64>,
    /// Last successful catalog fetch that listed this model.
    pub observed_at: DateTime<Utc>,
    pub raw_kind: Option<u32>,
    /// What the model produces: `chat`, `embedding`, `image`, `audio`, `rerank`, `other`.
    pub kind: String,
}

/// A Routstr (or compatible) node discovered via Nostr or config.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderNode {
    pub provider_id: String,
    pub name: String,
    pub endpoint: String,
    pub onion: Option<String>,
    pub mint: Option<String>,
    pub version: Option<String>,
    pub region: Option<String>,
    pub pubkey: Option<String>,
    pub discovered_via: String,
    pub last_seen: DateTime<Utc>,
    /// Rolling reliability 0.0–1.0 over the last hour (success rate).
    pub reliability: f64,
    /// Successful polls in the reliability window.
    pub poll_ok: u32,
    /// Total polls in the reliability window.
    pub poll_total: u32,
    /// Last poll latency in milliseconds (if known).
    pub last_latency_ms: Option<u64>,
    /// Why the poller refuses to fetch this node (e.g. private address); `None` = polled.
    pub skip_reason: Option<String>,
}

/// Outcome of the latest catalog fetch for one provider endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct FetchStatus {
    pub source: String,
    pub provider_id: String,
    pub endpoint: String,
    pub last_attempt_at: DateTime<Utc>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    /// Models listed by the last successful fetch.
    pub models: usize,
}

/// Summary of the most recent completed poll cycle.
#[derive(Debug, Clone, Serialize)]
pub struct PollCycle {
    pub finished_at: DateTime<Utc>,
    pub elapsed_ms: u64,
    pub nodes_ok: usize,
    pub nodes_failed: usize,
    pub delisted: usize,
    pub expired: usize,
}

#[derive(Debug, Clone)]
struct PollSample {
    at: DateTime<Utc>,
    ok: bool,
    latency_ms: Option<u64>,
}

/// Thread-safe in-memory store.
#[derive(Debug, Default)]
pub struct Store {
    /// Keyed by `{source}:{provider_id}:{model}` for latest-wins updates.
    quotes: RwLock<HashMap<String, Quote>>,
    /// Keyed by endpoint URL (normalized).
    nodes: RwLock<HashMap<String, ProviderNode>>,
    /// Poll history per endpoint for reliability.
    poll_history: RwLock<HashMap<String, VecDeque<PollSample>>>,
    /// Last known BTC/USD rate for sats conversion.
    btc_usd: RwLock<Option<f64>>,
    /// Latest fetch outcome per `{source}|{endpoint}`.
    fetches: RwLock<HashMap<String, FetchStatus>>,
    last_cycle: RwLock<Option<PollCycle>>,
    /// When the last Nostr provider announcement was received.
    last_announcement_at: RwLock<Option<DateTime<Utc>>>,
    /// USD per 1M above which a price is treated as a unit error (nulled).
    price_ceiling_usd: RwLock<Option<f64>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn quote_key(source: &str, provider_id: &str, model: &str) -> String {
        format!("{source}:{provider_id}:{model}")
    }

    pub fn set_price_ceiling_usd(&self, ceiling: Option<f64>) {
        if let Ok(mut g) = self.price_ceiling_usd.write() {
            *g = ceiling.filter(|c| *c > 0.0);
        }
    }

    fn price_ceiling(&self) -> Option<f64> {
        self.price_ceiling_usd.read().ok().and_then(|g| *g)
    }

    pub fn upsert_quotes(&self, quotes: impl IntoIterator<Item = Quote>) {
        let ceiling = self.price_ceiling();
        if let Ok(mut map) = self.quotes.write() {
            for mut q in quotes {
                // Drop dynamic sentinels / invalids before they can poison sorts or SQLite.
                q = sanitize_quote(q, ceiling);
                let k = Self::quote_key(&q.source, &q.provider_id, &q.model);
                if let Some(prev) = map.get(&k) {
                    // Preserve previous prices for delta unless caller already set them.
                    if q.prev_price_out_usd.is_none() {
                        q.prev_price_out_usd = sanitize_opt(prev.price_out_usd);
                    }
                    if q.prev_price_in_usd.is_none() {
                        q.prev_price_in_usd = sanitize_opt(prev.price_in_usd);
                    }
                    // If price didn't change, keep the older previous so deltas stay meaningful
                    // across multi-step normalizations (oracle re-upsert).
                    if prices_eq(q.price_out_usd, prev.price_out_usd)
                        && prices_eq(q.price_in_usd, prev.price_in_usd)
                    {
                        q.prev_price_out_usd =
                            sanitize_opt(prev.prev_price_out_usd.or(prev.price_out_usd));
                        q.prev_price_in_usd =
                            sanitize_opt(prev.prev_price_in_usd.or(prev.price_in_usd));
                    }
                }
                map.insert(k, q);
            }
        }
    }

    /// Apply one successful catalog fetch: upsert `quotes` and drop models that
    /// `source`/`provider_id` at `endpoint` no longer lists. An empty fetch
    /// changes nothing, so a bad payload can't wipe a provider.
    /// Returns the number of delisted quotes removed.
    pub fn replace_catalog(
        &self,
        source: &str,
        provider_id: &str,
        endpoint: Option<&str>,
        quotes: Vec<Quote>,
    ) -> usize {
        if quotes.is_empty() {
            return 0;
        }
        let listed: HashSet<String> = quotes
            .iter()
            .map(|q| Self::quote_key(&q.source, &q.provider_id, &q.model))
            .collect();
        self.upsert_quotes(quotes);

        let endpoint = endpoint.map(normalize_endpoint);
        let Ok(mut map) = self.quotes.write() else {
            return 0;
        };
        let before = map.len();
        map.retain(|k, q| {
            listed.contains(k)
                || q.source != source
                || q.provider_id != provider_id
                || q.endpoint.as_deref().map(normalize_endpoint) != endpoint
        });
        before - map.len()
    }

    /// Drop quotes not re-observed within `ttl` (provider down or gone).
    /// Returns the number removed.
    pub fn prune_stale_quotes(&self, ttl: Duration) -> usize {
        let cutoff = Utc::now() - ttl;
        let Ok(mut map) = self.quotes.write() else {
            return 0;
        };
        let before = map.len();
        map.retain(|_, q| q.observed_at >= cutoff);
        before - map.len()
    }

    /// Replace sats fields without resetting price-delta baselines.
    pub fn apply_sats_normalization(&self, btc_usd: f64) {
        if btc_usd <= 0.0 {
            return;
        }
        let ceiling = self.price_ceiling();
        let Ok(mut map) = self.quotes.write() else {
            return;
        };
        for q in map.values_mut() {
            // Re-sanitize in case a legacy SQLite row loaded bad values.
            *q = sanitize_quote(q.clone(), ceiling);
            if let Some(usd) = q.price_in_usd {
                q.price_in_sats = sanitize_opt(Some(usd_to_sats(usd, btc_usd)));
            } else if let Some(sats) = sanitize_opt(q.price_in_sats) {
                q.price_in_usd = sanitize_opt(Some(sats_to_usd(sats, btc_usd)));
                q.price_in_sats = Some(sats);
            } else {
                q.price_in_sats = None;
            }
            if let Some(usd) = q.price_out_usd {
                q.price_out_sats = sanitize_opt(Some(usd_to_sats(usd, btc_usd)));
            } else if let Some(sats) = sanitize_opt(q.price_out_sats) {
                q.price_out_usd = sanitize_opt(Some(sats_to_usd(sats, btc_usd)));
                q.price_out_sats = Some(sats);
            } else {
                q.price_out_sats = None;
            }
            // Sats-only rows only get USD here, so bound them again.
            if ceiling.is_some() {
                *q = sanitize_quote(q.clone(), ceiling);
            }
        }
    }

    pub fn list_quotes(&self) -> Vec<Quote> {
        let Ok(map) = self.quotes.read() else {
            return Vec::new();
        };
        let mut v: Vec<_> = map.values().cloned().collect();
        v.sort_by(|a, b| {
            a.model
                .cmp(&b.model)
                .then_with(|| a.provider.cmp(&b.provider))
                .then_with(|| a.source.cmp(&b.source))
        });
        v
    }

    pub fn quote_count(&self) -> usize {
        self.quotes.read().map(|m| m.len()).unwrap_or(0)
    }

    pub fn upsert_node(&self, mut node: ProviderNode) {
        let key = normalize_endpoint(&node.endpoint);
        // Attach current reliability stats if we already have history.
        let (rel, ok, total, lat) = self.reliability_for(&key);
        node.reliability = rel;
        node.poll_ok = ok;
        node.poll_total = total;
        if node.last_latency_ms.is_none() {
            node.last_latency_ms = lat;
        }
        if let Ok(mut map) = self.nodes.write() {
            if let Some(existing) = map.get(&key) {
                // Preserve last_latency if new node doesn't set it.
                if node.last_latency_ms.is_none() {
                    node.last_latency_ms = existing.last_latency_ms;
                }
                // Re-announcements keep the poller's verdict until the next cycle.
                if node.skip_reason.is_none() {
                    node.skip_reason = existing.skip_reason.clone();
                }
            }
            map.insert(key, node);
        }
    }

    /// Set (or clear) why the poller skipped a node; shown in `/api/nodes`.
    pub fn set_node_skip_reason(&self, endpoint: &str, reason: Option<String>) {
        if let Ok(mut map) = self.nodes.write() {
            if let Some(n) = map.get_mut(&normalize_endpoint(endpoint)) {
                n.skip_reason = reason;
            }
        }
    }

    pub fn list_nodes(&self) -> Vec<ProviderNode> {
        let Ok(map) = self.nodes.read() else {
            return Vec::new();
        };
        // Refresh reliability from history on read.
        let mut v: Vec<_> = map
            .values()
            .cloned()
            .map(|mut n| {
                let (rel, ok, total, lat) = self.reliability_for(&normalize_endpoint(&n.endpoint));
                n.reliability = rel;
                n.poll_ok = ok;
                n.poll_total = total;
                if lat.is_some() {
                    n.last_latency_ms = lat;
                }
                n
            })
            .collect();
        v.sort_by(|a, b| {
            b.reliability
                .partial_cmp(&a.reliability)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.cmp(&b.name))
        });
        v
    }

    pub fn node_count(&self) -> usize {
        self.nodes.read().map(|m| m.len()).unwrap_or(0)
    }

    /// Record a poll attempt against a node endpoint (success or failure).
    pub fn record_poll(&self, endpoint: &str, ok: bool, latency_ms: Option<u64>) {
        let key = normalize_endpoint(endpoint);
        let now = Utc::now();
        if let Ok(mut hist) = self.poll_history.write() {
            let q = hist.entry(key.clone()).or_default();
            q.push_back(PollSample {
                at: now,
                ok,
                latency_ms,
            });
            while q.len() > MAX_POLL_SAMPLES {
                q.pop_front();
            }
            // Drop samples older than the window.
            let cutoff = now - RELIABILITY_WINDOW;
            while q.front().map(|s| s.at < cutoff).unwrap_or(false) {
                q.pop_front();
            }
        }
        // Touch node last_seen / latency if present.
        if let Ok(mut map) = self.nodes.write() {
            if let Some(n) = map.get_mut(&key) {
                if ok {
                    n.last_seen = now;
                }
                if latency_ms.is_some() {
                    n.last_latency_ms = latency_ms;
                }
                let (rel, pok, total, _) = reliability_from_samples(
                    self.poll_history
                        .read()
                        .ok()
                        .as_ref()
                        .and_then(|h| h.get(&key)),
                );
                n.reliability = rel;
                n.poll_ok = pok;
                n.poll_total = total;
            }
        }
    }

    fn reliability_for(&self, endpoint_key: &str) -> (f64, u32, u32, Option<u64>) {
        let hist = self.poll_history.read().ok();
        let samples = hist.as_ref().and_then(|h| h.get(endpoint_key));
        let (rel, ok, total, _) = reliability_from_samples(samples);
        let lat = samples.and_then(|s| s.iter().rev().find_map(|x| x.latency_ms));
        (rel, ok, total, lat)
    }

    /// Success rate over the reliability window per normalized endpoint
    /// (only endpoints with samples).
    pub fn reliability_by_endpoint(&self) -> HashMap<String, f64> {
        let Ok(hist) = self.poll_history.read() else {
            return HashMap::new();
        };
        hist.iter()
            .filter_map(|(k, samples)| {
                let (rel, _, total, _) = reliability_from_samples(Some(samples));
                (total > 0).then(|| (k.clone(), rel))
            })
            .collect()
    }

    /// Record a catalog fetch outcome (`Ok(models)` or `Err(message)`).
    pub fn record_fetch(
        &self,
        source: &str,
        provider_id: &str,
        endpoint: &str,
        outcome: Result<usize, String>,
    ) {
        let endpoint = normalize_endpoint(endpoint);
        let now = Utc::now();
        let Ok(mut map) = self.fetches.write() else {
            return;
        };
        let st = map
            .entry(format!("{source}|{endpoint}"))
            .or_insert_with(|| FetchStatus {
                source: source.to_string(),
                provider_id: provider_id.to_string(),
                endpoint: endpoint.clone(),
                last_attempt_at: now,
                last_success_at: None,
                last_error: None,
                consecutive_failures: 0,
                models: 0,
            });
        st.provider_id = provider_id.to_string();
        st.last_attempt_at = now;
        match outcome {
            Ok(models) => {
                st.last_success_at = Some(now);
                st.last_error = None;
                st.consecutive_failures = 0;
                st.models = models;
            }
            Err(e) => {
                st.last_error = Some(e);
                st.consecutive_failures += 1;
            }
        }
    }

    pub fn fetch_statuses(&self) -> Vec<FetchStatus> {
        self.fetches
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn record_cycle(&self, cycle: PollCycle) {
        if let Ok(mut g) = self.last_cycle.write() {
            *g = Some(cycle);
        }
    }

    pub fn last_cycle(&self) -> Option<PollCycle> {
        self.last_cycle.read().ok().and_then(|g| g.clone())
    }

    pub fn record_announcement(&self) {
        if let Ok(mut g) = self.last_announcement_at.write() {
            *g = Some(Utc::now());
        }
    }

    pub fn last_announcement_at(&self) -> Option<DateTime<Utc>> {
        self.last_announcement_at.read().ok().and_then(|g| *g)
    }

    pub fn set_btc_usd(&self, rate: f64) {
        if let Ok(mut g) = self.btc_usd.write() {
            *g = Some(rate);
        }
    }

    pub fn btc_usd(&self) -> Option<f64> {
        self.btc_usd.read().ok().and_then(|g| *g)
    }

    /// Snapshot all quotes for persistence.
    pub fn all_quotes_raw(&self) -> Vec<Quote> {
        self.quotes
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Snapshot all nodes for persistence.
    pub fn all_nodes_raw(&self) -> Vec<ProviderNode> {
        self.nodes
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Load quotes bulk (startup restore) — no delta chaining.
    /// Invalid prices from older snapshots are sanitized on load.
    pub fn load_quotes(&self, quotes: Vec<Quote>) {
        let ceiling = self.price_ceiling();
        if let Ok(mut map) = self.quotes.write() {
            for q in quotes {
                let q = sanitize_quote(q, ceiling);
                let k = Self::quote_key(&q.source, &q.provider_id, &q.model);
                map.insert(k, q);
            }
        }
    }

    pub fn load_nodes(&self, nodes: Vec<ProviderNode>) {
        if let Ok(mut map) = self.nodes.write() {
            for n in nodes {
                let key = normalize_endpoint(&n.endpoint);
                map.insert(key, n);
            }
        }
    }
}

fn reliability_from_samples(samples: Option<&VecDeque<PollSample>>) -> (f64, u32, u32, ()) {
    let Some(samples) = samples else {
        return (0.0, 0, 0, ());
    };
    let cutoff = Utc::now() - RELIABILITY_WINDOW;
    let recent: Vec<_> = samples.iter().filter(|s| s.at >= cutoff).collect();
    let total = recent.len() as u32;
    if total == 0 {
        return (0.0, 0, 0, ());
    }
    let ok = recent.iter().filter(|s| s.ok).count() as u32;
    let rel = ok as f64 / total as f64;
    (rel, ok, total, ())
}

fn prices_eq(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() < 1e-12,
        (None, None) => true,
        _ => false,
    }
}

/// Null invalid / dynamic-sentinel price fields on a quote. A USD price above
/// `ceiling` is almost always a unit error (per-1k or per-token read as per-1M),
/// so that side is nulled in both units rather than ranked.
fn sanitize_quote(mut q: Quote, ceiling: Option<f64>) -> Quote {
    q.price_in_usd = sanitize_opt(q.price_in_usd);
    q.price_out_usd = sanitize_opt(q.price_out_usd);
    q.price_in_sats = sanitize_opt(q.price_in_sats);
    q.price_out_sats = sanitize_opt(q.price_out_sats);
    q.prev_price_in_usd = sanitize_opt(q.prev_price_in_usd);
    q.prev_price_out_usd = sanitize_opt(q.prev_price_out_usd);
    if let Some(max) = ceiling {
        let over = |v: Option<f64>| v.is_some_and(|x| x > max);
        if over(q.price_in_usd) {
            q.price_in_usd = None;
            q.price_in_sats = None;
        }
        if over(q.price_out_usd) {
            q.price_out_usd = None;
            q.price_out_sats = None;
        }
        if over(q.prev_price_in_usd) {
            q.prev_price_in_usd = None;
        }
        if over(q.prev_price_out_usd) {
            q.prev_price_out_usd = None;
        }
    }
    q
}

fn usd_to_sats(usd: f64, btc_usd: f64) -> f64 {
    (usd / btc_usd) * 100_000_000.0
}

fn sats_to_usd(sats: f64, btc_usd: f64) -> f64 {
    (sats / 100_000_000.0) * btc_usd
}

/// Strip trailing slash for stable node keys.
pub fn normalize_endpoint(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(model: &str, out: f64) -> Quote {
        Quote {
            source: "t".into(),
            provider: "p".into(),
            provider_id: "p".into(),
            model: model.into(),
            price_in_usd: Some(1.0),
            price_out_usd: Some(out),
            price_in_sats: None,
            price_out_sats: None,
            prev_price_out_usd: None,
            prev_price_in_usd: None,
            endpoint: None,
            region: None,
            context_length: None,
            observed_at: Utc::now(),
            raw_kind: None,
            kind: "chat".into(),
        }
    }

    #[test]
    fn upsert_tracks_previous_price() {
        let store = Store::new();
        store.upsert_quotes([sample("m", 2.0)]);
        store.upsert_quotes([sample("m", 4.0)]);
        let q = store.list_quotes().into_iter().next().unwrap();
        assert_eq!(q.price_out_usd, Some(4.0));
        assert_eq!(q.prev_price_out_usd, Some(2.0));
    }

    #[test]
    fn upsert_strips_negative_and_sentinel_prices() {
        let store = Store::new();
        let mut q = sample("auto", -1.0);
        q.price_in_usd = Some(-1.0);
        q.price_out_usd = Some(-1_000_000.0);
        q.price_in_sats = Some(-1e9);
        store.upsert_quotes([q]);
        let got = store.list_quotes().into_iter().next().unwrap();
        assert_eq!(got.price_in_usd, None);
        assert_eq!(got.price_out_usd, None);
        assert_eq!(got.price_in_sats, None);
        // Model row is still present (observe the model, just no fake cost).
        assert_eq!(got.model, "auto");
    }

    #[test]
    fn load_quotes_sanitizes_legacy_snapshot() {
        let store = Store::new();
        let mut q = sample("legacy", -1150000.0);
        q.price_in_usd = Some(-1150000.0);
        store.load_quotes(vec![q]);
        let got = store.list_quotes().into_iter().next().unwrap();
        assert_eq!(got.price_out_usd, None);
        assert_eq!(got.price_in_usd, None);
    }

    #[test]
    fn sats_normalization_does_not_reset_delta() {
        let store = Store::new();
        store.upsert_quotes([sample("m", 2.0)]);
        store.upsert_quotes([sample("m", 4.0)]);
        store.set_btc_usd(50_000.0);
        store.apply_sats_normalization(50_000.0);
        let q = store.list_quotes().into_iter().next().unwrap();
        assert_eq!(q.prev_price_out_usd, Some(2.0));
        assert!(q.price_out_sats.unwrap() > 0.0);
    }

    fn at(model: &str, endpoint: &str) -> Quote {
        let mut q = sample(model, 2.0);
        q.endpoint = Some(endpoint.into());
        q
    }

    #[test]
    fn replace_catalog_drops_delisted_models() {
        let store = Store::new();
        store.upsert_quotes([at("a", "https://n1"), at("b", "https://n1")]);
        let removed =
            store.replace_catalog("t", "p", Some("https://n1/"), vec![at("a", "https://n1")]);
        assert_eq!(removed, 1);
        let models: Vec<_> = store.list_quotes().into_iter().map(|q| q.model).collect();
        assert_eq!(models, vec!["a"]);
    }

    #[test]
    fn replace_catalog_keeps_other_endpoints_and_empty_fetch() {
        let store = Store::new();
        let mut other = at("b", "https://n2");
        other.provider_id = "q".into();
        store.upsert_quotes([at("a", "https://n1"), other]);
        // Different provider at another endpoint is untouched.
        store.replace_catalog("t", "p", Some("https://n1"), vec![at("c", "https://n1")]);
        assert_eq!(store.quote_count(), 2);
        // Empty fetch never prunes.
        assert_eq!(
            store.replace_catalog("t", "p", Some("https://n1"), vec![]),
            0
        );
        assert_eq!(store.quote_count(), 2);
    }

    #[test]
    fn prune_stale_quotes_by_observed_at() {
        let store = Store::new();
        let mut old = sample("old", 2.0);
        old.observed_at = Utc::now() - Duration::hours(25);
        store.upsert_quotes([old, sample("new", 2.0)]);
        assert_eq!(store.prune_stale_quotes(Duration::hours(24)), 1);
        assert_eq!(store.list_quotes()[0].model, "new");
    }

    #[test]
    fn price_ceiling_nulls_unit_errors_in_both_units() {
        let store = Store::new();
        store.set_price_ceiling_usd(Some(1000.0));
        let mut q = sample("per-1k-misread", 15_000.0);
        q.price_out_sats = Some(1e9);
        store.upsert_quotes([q]);
        let got = store.list_quotes().into_iter().next().unwrap();
        assert_eq!(got.price_out_usd, None);
        assert_eq!(got.price_out_sats, None);
        assert_eq!(got.price_in_usd, Some(1.0));

        // Sats-only rows are bounded once the oracle derives USD.
        let mut sats_only = sample("sats-only", 1.0);
        sats_only.price_in_usd = None;
        sats_only.price_in_sats = Some(5e9); // ≈ $2,500/1M at $50k
        store.upsert_quotes([sats_only]);
        store.apply_sats_normalization(50_000.0);
        let got = store
            .list_quotes()
            .into_iter()
            .find(|q| q.model == "sats-only")
            .unwrap();
        assert_eq!(got.price_in_usd, None);
        assert_eq!(got.price_in_sats, None);
    }

    #[test]
    fn record_fetch_tracks_success_and_failures() {
        let store = Store::new();
        store.record_fetch("routstr", "n1", "https://n1/", Err("down".into()));
        store.record_fetch("routstr", "n1", "https://n1", Err("down".into()));
        let f = &store.fetch_statuses()[0];
        assert_eq!((f.consecutive_failures, f.last_success_at), (2, None));
        store.record_fetch("routstr", "n1", "https://n1", Ok(7));
        let f = &store.fetch_statuses()[0];
        assert_eq!((f.consecutive_failures, f.models), (0, 7));
        assert!(f.last_success_at.is_some() && f.last_error.is_none());
    }

    #[test]
    fn normalize_endpoint_strips_slash() {
        assert_eq!(normalize_endpoint("https://x.com/v1/"), "https://x.com/v1");
    }

    #[test]
    fn poll_reliability_window() {
        let store = Store::new();
        store.upsert_node(ProviderNode {
            provider_id: "n1".into(),
            name: "n1".into(),
            endpoint: "https://node.example".into(),
            onion: None,
            mint: None,
            version: None,
            region: None,
            pubkey: None,
            discovered_via: "test".into(),
            last_seen: Utc::now(),
            reliability: 0.0,
            poll_ok: 0,
            poll_total: 0,
            last_latency_ms: None,
            skip_reason: None,
        });
        store.record_poll("https://node.example", true, Some(12));
        store.record_poll("https://node.example", false, None);
        store.record_poll("https://node.example", true, Some(20));
        let n = store.list_nodes().into_iter().next().unwrap();
        assert_eq!(n.poll_total, 3);
        assert_eq!(n.poll_ok, 2);
        assert!((n.reliability - (2.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn skip_reason_survives_reannounce() {
        let store = Store::new();
        let node = ProviderNode {
            provider_id: "n1".into(),
            name: "n1".into(),
            endpoint: "http://127.0.0.1:18131".into(),
            onion: None,
            mint: None,
            version: None,
            region: None,
            pubkey: None,
            discovered_via: "nostr:38421".into(),
            last_seen: Utc::now(),
            reliability: 0.0,
            poll_ok: 0,
            poll_total: 0,
            last_latency_ms: None,
            skip_reason: None,
        };
        store.upsert_node(node.clone());
        store.set_node_skip_reason("http://127.0.0.1:18131/", Some("loopback".into()));
        store.upsert_node(node);
        let n = store.list_nodes().into_iter().next().unwrap();
        assert_eq!(n.skip_reason.as_deref(), Some("loopback"));
        store.set_node_skip_reason("http://127.0.0.1:18131", None);
        assert!(store.list_nodes()[0].skip_reason.is_none());
    }
}
