//! In-memory observation store for inference quotes and discovered nodes.

use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

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
    pub observed_at: DateTime<Utc>,
    pub raw_kind: Option<u32>,
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
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn quote_key(source: &str, provider_id: &str, model: &str) -> String {
        format!("{source}:{provider_id}:{model}")
    }

    pub fn upsert_quotes(&self, quotes: impl IntoIterator<Item = Quote>) {
        if let Ok(mut map) = self.quotes.write() {
            for mut q in quotes {
                let k = Self::quote_key(&q.source, &q.provider_id, &q.model);
                if let Some(prev) = map.get(&k) {
                    // Preserve previous prices for delta unless caller already set them.
                    if q.prev_price_out_usd.is_none() {
                        q.prev_price_out_usd = prev.price_out_usd;
                    }
                    if q.prev_price_in_usd.is_none() {
                        q.prev_price_in_usd = prev.price_in_usd;
                    }
                    // If price didn't change, keep the older previous so deltas stay meaningful
                    // across multi-step normalizations (oracle re-upsert).
                    if prices_eq(q.price_out_usd, prev.price_out_usd)
                        && prices_eq(q.price_in_usd, prev.price_in_usd)
                    {
                        q.prev_price_out_usd = prev.prev_price_out_usd.or(prev.price_out_usd);
                        q.prev_price_in_usd = prev.prev_price_in_usd.or(prev.price_in_usd);
                    }
                }
                map.insert(k, q);
            }
        }
    }

    /// Replace sats fields without resetting price-delta baselines.
    pub fn apply_sats_normalization(&self, btc_usd: f64) {
        if btc_usd <= 0.0 {
            return;
        }
        let Ok(mut map) = self.quotes.write() else {
            return;
        };
        for q in map.values_mut() {
            if let Some(usd) = q.price_in_usd {
                q.price_in_sats = Some(usd_to_sats(usd, btc_usd));
            } else if let Some(sats) = q.price_in_sats {
                q.price_in_usd = Some(sats_to_usd(sats, btc_usd));
            }
            if let Some(usd) = q.price_out_usd {
                q.price_out_sats = Some(usd_to_sats(usd, btc_usd));
            } else if let Some(sats) = q.price_out_sats {
                q.price_out_usd = Some(sats_to_usd(sats, btc_usd));
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
            }
            map.insert(key, node);
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
    pub fn load_quotes(&self, quotes: Vec<Quote>) {
        if let Ok(mut map) = self.quotes.write() {
            for q in quotes {
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
        });
        store.record_poll("https://node.example", true, Some(12));
        store.record_poll("https://node.example", false, None);
        store.record_poll("https://node.example", true, Some(20));
        let n = store.list_nodes().into_iter().next().unwrap();
        assert_eq!(n.poll_total, 3);
        assert_eq!(n.poll_ok, 2);
        assert!((n.reliability - (2.0 / 3.0)).abs() < 1e-9);
    }
}
