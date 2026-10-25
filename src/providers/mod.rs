//! HTTP polling of inference catalogs (OpenRouter + discovered Routstr nodes).

mod catalog;
mod netguard;
mod reference;
mod routstr;

pub use catalog::model_kind;
pub use reference::{ReferenceCatalog, OPENROUTER, ORCAROUTER};

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::store::{PollCycle, Store};

#[derive(Debug, Clone)]
pub struct PollerConfig {
    pub interval: Duration,
    pub http_timeout: Duration,
    /// Enabled centralized catalogs and the `/models` URL to fetch for each.
    pub reference_catalogs: Vec<(ReferenceCatalog, String)>,
    /// Max Routstr nodes polled at once.
    pub node_concurrency: usize,
    /// Wall-clock budget per node (all `/models` fallbacks together).
    pub node_timeout: Duration,
    /// Drop quotes not re-observed within this window (`None` = keep forever).
    pub quote_ttl: Option<Duration>,
    /// Poll Nostr-announced nodes on private/loopback addresses (trusted LAN only).
    pub allow_private_nodes: bool,
}

pub async fn run_poller(
    store: Arc<Store>,
    cfg: PollerConfig,
    cancel: CancellationToken,
) -> Result<()> {
    info!(
        interval_secs = cfg.interval.as_secs(),
        reference = ?cfg
            .reference_catalogs
            .iter()
            .map(|(c, _)| c.source)
            .collect::<Vec<_>>(),
        "starting provider poller"
    );

    let client = reqwest::Client::builder()
        .timeout(cfg.http_timeout)
        .user_agent(concat!("tokenstats/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let nodes =
        routstr::NodeFetcher::new(client.clone(), cfg.http_timeout, cfg.allow_private_nodes)?;

    tokio::select! {
        _ = cancel.cancelled() => {
            info!("provider poller received shutdown during initial poll");
            return Ok(());
        }
        res = poll_once(&client, &nodes, &store, &cfg) => {
            if let Err(e) = res {
                warn!(error = %e, "initial provider poll failed");
            }
        }
    }

    let mut interval = tokio::time::interval(cfg.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick completes immediately; skip so we don't double-poll after initial.
    interval.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("provider poller received shutdown");
                break;
            }
            _ = interval.tick() => {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("provider poller received shutdown mid-cycle");
                        break;
                    }
                    res = poll_once(&client, &nodes, &store, &cfg) => {
                        if let Err(e) = res {
                            warn!(error = %e, "provider poll failed");
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

async fn poll_once(
    client: &reqwest::Client,
    nodes: &routstr::NodeFetcher,
    store: &Store,
    cfg: &PollerConfig,
) -> Result<()> {
    let started = Instant::now();
    let mut delisted = 0usize;

    for (cat, url) in &cfg.reference_catalogs {
        let started = Instant::now();
        match reference::fetch_quotes(client, url, cat).await {
            Ok(quotes) => {
                let n = quotes.len();
                let latency_ms = started.elapsed().as_millis() as u64;
                store.record_poll(cat.endpoint, true, Some(latency_ms));
                store.record_fetch(cat.source, cat.source, cat.endpoint, Ok(n));
                delisted +=
                    store.replace_catalog(cat.source, cat.source, Some(cat.endpoint), quotes);
                info!(
                    source = cat.source,
                    models = n,
                    latency_ms,
                    "reference catalog updated"
                );
            }
            Err(e) => {
                store.record_poll(cat.endpoint, false, None);
                store.record_fetch(cat.source, cat.source, cat.endpoint, Err(format!("{e:#}")));
                warn!(source = cat.source, error = %e, "reference catalog poll failed");
            }
        }
    }

    // Poll nodes concurrently; results are applied here so tasks don't need the store.
    let sem = Arc::new(Semaphore::new(cfg.node_concurrency.max(1)));
    let mut tasks = JoinSet::new();
    for node in store.list_nodes() {
        let nodes = nodes.clone();
        let sem = Arc::clone(&sem);
        let timeout = cfg.node_timeout;
        tasks.spawn(async move {
            let _permit = sem.acquire_owned().await;
            let res = match tokio::time::timeout(timeout, nodes.fetch(&node)).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!("timed out after {}s", timeout.as_secs())),
            };
            (node, res)
        });
    }

    let (mut nodes_ok, mut nodes_failed, mut nodes_blocked) = (0usize, 0usize, 0usize);
    while let Some(joined) = tasks.join_next().await {
        let Ok((node, res)) = joined else {
            continue;
        };
        let blocked = res
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<routstr::Blocked>());
        store.set_node_skip_reason(&node.endpoint, blocked.map(|b| b.0.clone()));
        if blocked.is_some() {
            // Never requested: no reliability sample, no catalog change.
            nodes_blocked += 1;
            continue;
        }
        match res {
            Ok((quotes, latency_ms)) => {
                nodes_ok += 1;
                let n = quotes.len();
                store.record_poll(&node.endpoint, true, Some(latency_ms));
                store.record_fetch("routstr", &node.provider_id, &node.endpoint, Ok(n));
                delisted += store.replace_catalog(
                    "routstr",
                    &node.provider_id,
                    Some(&node.endpoint),
                    quotes,
                );
                info!(
                    endpoint = %node.endpoint,
                    models = n,
                    latency_ms,
                    "Routstr node catalog updated"
                );
            }
            Err(e) => {
                nodes_failed += 1;
                store.record_poll(&node.endpoint, false, None);
                store.record_fetch(
                    "routstr",
                    &node.provider_id,
                    &node.endpoint,
                    Err(format!("{e:#}")),
                );
                warn!(
                    endpoint = %node.endpoint,
                    error = %e,
                    "Routstr node poll failed"
                );
            }
        }
    }

    let expired = cfg
        .quote_ttl
        .and_then(|ttl| chrono::Duration::from_std(ttl).ok())
        .map(|ttl| store.prune_stale_quotes(ttl))
        .unwrap_or(0);

    let elapsed_ms = started.elapsed().as_millis() as u64;
    store.record_cycle(PollCycle {
        finished_at: chrono::Utc::now(),
        elapsed_ms,
        nodes_ok,
        nodes_failed,
        delisted,
        expired,
    });
    info!(
        nodes_ok,
        nodes_failed,
        nodes_blocked,
        delisted,
        expired,
        quotes = store.quote_count(),
        elapsed_ms,
        "poll cycle complete"
    );

    Ok(())
}
