//! HTTP polling of inference catalogs (OpenRouter + discovered Routstr nodes).

mod catalog;
mod openrouter;
mod routstr;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::store::Store;

#[derive(Debug, Clone)]
pub struct PollerConfig {
    pub interval: Duration,
    pub http_timeout: Duration,
    pub enable_openrouter: bool,
    pub openrouter_url: String,
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

    if let Err(e) = poll_once(&client, &store, &cfg).await {
        warn!(error = %e, "initial provider poll failed");
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
                if let Err(e) = poll_once(&client, &store, &cfg).await {
                    warn!(error = %e, "provider poll failed");
                }
            }
        }
    }

    Ok(())
}

async fn poll_once(client: &reqwest::Client, store: &Store, cfg: &PollerConfig) -> Result<()> {
    if cfg.enable_openrouter {
        match openrouter::fetch_quotes(client, &cfg.openrouter_url).await {
            Ok(quotes) => {
                let n = quotes.len();
                store.upsert_quotes(quotes);
                info!(models = n, "OpenRouter catalog updated");
            }
            Err(e) => warn!(error = %e, "OpenRouter poll failed"),
        }
    }

    let nodes = store.list_nodes();
    for node in nodes {
        match routstr::fetch_node_quotes(client, &node).await {
            Ok((quotes, latency_ms)) => {
                let n = quotes.len();
                store.record_poll(&node.endpoint, true, Some(latency_ms));
                store.upsert_quotes(quotes);
                info!(
                    endpoint = %node.endpoint,
                    models = n,
                    latency_ms,
                    "Routstr node catalog updated"
                );
            }
            Err(e) => {
                store.record_poll(&node.endpoint, false, None);
                warn!(
                    endpoint = %node.endpoint,
                    error = %e,
                    "Routstr node poll failed"
                );
            }
        }
    }

    Ok(())
}
