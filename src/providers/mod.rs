//! HTTP polling of inference catalogs (OpenRouter + discovered Routstr nodes).

mod catalog;
mod openrouter;
mod routstr;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::store::Store;

#[derive(Debug, Clone)]
pub struct PollerConfig {
    pub interval: Duration,
    pub http_timeout: Duration,
    pub enable_openrouter: bool,
    pub openrouter_url: String,
    /// Max Routstr nodes polled at once.
    pub node_concurrency: usize,
    /// Wall-clock budget per node (all `/models` fallbacks together).
    pub node_timeout: Duration,
    /// Drop quotes not re-observed within this window (`None` = keep forever).
    pub quote_ttl: Option<Duration>,
}

pub async fn run_poller(
    store: Arc<Store>,
    cfg: PollerConfig,
    cancel: CancellationToken,
) -> Result<()> {
    info!(
        interval_secs = cfg.interval.as_secs(),
        openrouter = cfg.enable_openrouter,
        "starting provider poller"
    );

    let client = reqwest::Client::builder()
        .timeout(cfg.http_timeout)
        .user_agent(concat!("tokenstats/", env!("CARGO_PKG_VERSION")))
        .build()?;

    tokio::select! {
        _ = cancel.cancelled() => {
            info!("provider poller received shutdown during initial poll");
            return Ok(());
        }
        res = poll_once(&client, &store, &cfg) => {
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
                    res = poll_once(&client, &store, &cfg) => {
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

async fn poll_once(client: &reqwest::Client, store: &Store, cfg: &PollerConfig) -> Result<()> {
    let started = Instant::now();
    let mut delisted = 0usize;

    if cfg.enable_openrouter {
        match openrouter::fetch_quotes(client, &cfg.openrouter_url).await {
            Ok(quotes) => {
                let n = quotes.len();
                delisted += store.replace_catalog(
                    "openrouter",
                    "openrouter",
                    Some(openrouter::ENDPOINT),
                    quotes,
                );
                info!(models = n, "OpenRouter catalog updated");
            }
            Err(e) => warn!(error = %e, "OpenRouter poll failed"),
        }
    }

    // Poll nodes concurrently; results are applied here so tasks don't need the store.
    let sem = Arc::new(Semaphore::new(cfg.node_concurrency.max(1)));
    let mut tasks = JoinSet::new();
    for node in store.list_nodes() {
        let client = client.clone();
        let sem = Arc::clone(&sem);
        let timeout = cfg.node_timeout;
        tasks.spawn(async move {
            let _permit = sem.acquire_owned().await;
            let res =
                match tokio::time::timeout(timeout, routstr::fetch_node_quotes(&client, &node))
                    .await
                {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!("timed out after {}s", timeout.as_secs())),
                };
            (node, res)
        });
    }

    let (mut nodes_ok, mut nodes_failed) = (0usize, 0usize);
    while let Some(joined) = tasks.join_next().await {
        let Ok((node, res)) = joined else {
            continue;
        };
        match res {
            Ok((quotes, latency_ms)) => {
                nodes_ok += 1;
                let n = quotes.len();
                store.record_poll(&node.endpoint, true, Some(latency_ms));
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

    info!(
        nodes_ok,
        nodes_failed,
        delisted,
        expired,
        quotes = store.quote_count(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "poll cycle complete"
    );

    Ok(())
}
