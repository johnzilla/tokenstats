//! HTTP polling of inference catalogs (OpenRouter + discovered Routstr nodes).

mod catalog;
mod openrouter;
mod routstr;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{info, warn};

use crate::store::Store;

/// Interval between full provider poll cycles.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

pub async fn run_poller(store: Arc<Store>) -> Result<()> {
    info!("starting provider poller");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("tokenstats/", env!("CARGO_PKG_VERSION")))
        .build()?;

    if let Err(e) = poll_once(&client, &store).await {
        warn!(error = %e, "initial provider poll failed");
    }

    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.tick().await;
    loop {
        interval.tick().await;
        if let Err(e) = poll_once(&client, &store).await {
            warn!(error = %e, "provider poll failed");
        }
    }
}

async fn poll_once(client: &reqwest::Client, store: &Store) -> Result<()> {
    match openrouter::fetch_quotes(client).await {
        Ok(quotes) => {
            let n = quotes.len();
            store.upsert_quotes(quotes);
            info!(models = n, "OpenRouter catalog updated");
        }
        Err(e) => warn!(error = %e, "OpenRouter poll failed"),
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
