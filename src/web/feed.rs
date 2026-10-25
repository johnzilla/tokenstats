//! Feed health: is the board fresh and complete enough to act on?

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use super::AppState;
use crate::market::REFERENCE_SOURCES;
use crate::store::{FetchStatus, Quote};

/// Feed status shared by `/health` and `/api/summary`.
///
/// `status` is one of:
/// - `ok`: catalogs fresh, poller cycling, BTC rate known
/// - `degraded`: usable but something is off (see `reasons`)
/// - `down`: no source fetched successfully within the stale threshold
/// - `starting`: polling enabled, first cycle not finished
/// - `offline`: polling disabled (serving a snapshot)
///
/// Many dead Routstr nodes alone is market state, not a feed problem, so it
/// only degrades the feed when *no* node is healthy.
pub fn feed_status(state: &AppState, quotes: &[Quote]) -> Value {
    let now = Utc::now();
    let stale_after = Duration::from_std(state.stale_after).unwrap_or(Duration::minutes(10));
    let store = &state.store;
    let fetches = store.fetch_statuses();
    let cycle = store.last_cycle();

    let stale_quotes = quotes
        .iter()
        .filter(|q| now - q.observed_at > stale_after)
        .count();

    let reference: Vec<(&str, SourceSummary)> = REFERENCE_SOURCES
        .iter()
        .map(|s| (*s, source_summary(&fetches, s)))
        .collect();
    let routstr = source_summary(&fetches, "routstr");
    let fresh = |t: Option<DateTime<Utc>>| t.is_some_and(|t| now - t <= stale_after);

    let mut reasons: Vec<String> = Vec::new();
    let status = if !state.polling {
        "offline"
    } else if cycle.is_none() {
        "starting"
    } else if !fetches.iter().any(|f| fresh(f.last_success_at)) {
        reasons.push("no catalog fetched successfully within stale_after_secs".into());
        "down"
    } else {
        if cycle
            .as_ref()
            .is_some_and(|c| now - c.finished_at > stale_after * 2)
        {
            reasons.push("poll cycle has not completed recently".into());
        }
        for (name, summary) in &reference {
            if summary.attempted && !fresh(summary.last_success_at) {
                reasons.push(format!("{name} catalog is stale"));
            }
        }
        if routstr.endpoints > 0 && routstr.healthy == 0 {
            reasons.push("no Routstr node is answering".into());
        }
        match (store.btc_usd(), store.btc_usd_at()) {
            (None, _) => reasons.push("BTC/USD rate unknown; sats prices missing".into()),
            (Some(_), None) => {
                reasons.push("BTC/USD rate age unknown (restored without timestamp)".into())
            }
            (Some(_), Some(at)) if now - at > stale_after => reasons.push(format!(
                "BTC/USD rate is stale ({}s old); sats prices may be off",
                (now - at).num_seconds()
            )),
            _ => {}
        }
        if reasons.is_empty() {
            "ok"
        } else {
            "degraded"
        }
    };

    let mut sources: serde_json::Map<String, Value> = reference
        .iter()
        .filter(|(_, s)| s.attempted)
        .map(|(name, s)| (name.to_string(), s.to_json()))
        .collect();
    sources.insert("routstr".into(), routstr.to_json());

    let last_announcement = store.last_announcement_at();
    json!({
        "status": status,
        "reasons": reasons,
        "stale_after_secs": stale_after.num_seconds(),
        "stale_quotes": stale_quotes,
        "last_cycle": cycle,
        "sources": sources,
        "discovery": {
            "nodes": store.node_count(),
            "last_announcement_at": last_announcement,
            "lag_secs": last_announcement.map(|t| (now - t).num_seconds().max(0)),
        },
    })
}

struct SourceSummary {
    attempted: bool,
    endpoints: usize,
    healthy: usize,
    models: usize,
    last_success_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

impl SourceSummary {
    fn to_json(&self) -> Value {
        json!({
            "endpoints": self.endpoints,
            "healthy": self.healthy,
            "failing": self.endpoints - self.healthy,
            "models": self.models,
            "last_success_at": self.last_success_at,
            "last_error": self.last_error,
        })
    }
}

/// Aggregate fetch outcomes for one source. `healthy` = last fetch succeeded.
fn source_summary(fetches: &[FetchStatus], source: &str) -> SourceSummary {
    let rows: Vec<&FetchStatus> = fetches.iter().filter(|f| f.source == source).collect();
    let healthy: Vec<&&FetchStatus> = rows
        .iter()
        .filter(|f| f.consecutive_failures == 0 && f.last_success_at.is_some())
        .collect();
    SourceSummary {
        attempted: !rows.is_empty(),
        endpoints: rows.len(),
        healthy: healthy.len(),
        models: healthy.iter().map(|f| f.models).sum(),
        last_success_at: rows.iter().filter_map(|f| f.last_success_at).max(),
        // Most recent failure, so a single-endpoint source shows its error.
        last_error: rows
            .iter()
            .filter(|f| f.consecutive_failures > 0)
            .max_by_key(|f| f.last_attempt_at)
            .and_then(|f| f.last_error.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch(source: &str, endpoint: &str, ok: bool, age_secs: i64) -> FetchStatus {
        let at = Utc::now() - Duration::seconds(age_secs);
        FetchStatus {
            source: source.into(),
            provider_id: endpoint.into(),
            endpoint: endpoint.into(),
            first_attempt_at: at,
            last_attempt_at: at,
            last_success_at: ok.then_some(at),
            last_error: (!ok).then(|| "boom".into()),
            consecutive_failures: u32::from(!ok),
            models: if ok { 5 } else { 0 },
        }
    }

    fn state_with_fresh_catalog() -> AppState {
        let store = std::sync::Arc::new(crate::store::Store::new());
        store.record_fetch("openrouter", "openrouter", "https://or", Ok(5));
        store.record_cycle(crate::store::PollCycle {
            finished_at: Utc::now(),
            elapsed_ms: 1,
            nodes_ok: 0,
            nodes_failed: 0,
            delisted: 0,
            expired: 0,
        });
        AppState {
            store,
            stale_after: std::time::Duration::from_secs(600),
            polling: true,
            analytics: None,
        }
    }

    #[test]
    fn btc_rate_age_drives_status() {
        let st = state_with_fresh_catalog();
        st.store.set_btc_usd(80_000.0, Some(Utc::now()));
        assert_eq!(feed_status(&st, &[])["status"], "ok");

        st.store
            .set_btc_usd(80_000.0, Some(Utc::now() - Duration::hours(2)));
        let f = feed_status(&st, &[]);
        assert_eq!(f["status"], "degraded");
        assert!(f["reasons"][0].as_str().unwrap().contains("stale"));

        st.store.set_btc_usd(80_000.0, None);
        let f = feed_status(&st, &[]);
        assert!(f["reasons"][0].as_str().unwrap().contains("age unknown"));
    }

    #[test]
    fn source_summary_counts_healthy_and_latest_error() {
        let rows = vec![
            fetch("routstr", "a", true, 5),
            fetch("routstr", "b", false, 5),
            fetch("openrouter", "or", true, 5),
        ];
        let s = source_summary(&rows, "routstr");
        assert_eq!((s.endpoints, s.healthy, s.models), (2, 1, 5));
        assert_eq!(s.last_error.as_deref(), Some("boom"));
        assert!(source_summary(&rows, "openrouter").last_error.is_none());
        assert!(!source_summary(&rows, "orcarouter").attempted);
    }
}
