//! Oracle: BTC/USD rate + dual-unit normalization + market summary.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::market::blended_usd;
use crate::store::{Quote, Store};

const RATE_INTERVAL: Duration = Duration::from_secs(60);
const NORMALIZE_INTERVAL: Duration = Duration::from_secs(15);

/// Coinbase public spot price (no API key).
const BTC_USD_URL: &str = "https://api.coinbase.com/v2/prices/BTC-USD/spot";

pub async fn run_oracle(store: Arc<Store>) -> Result<()> {
    info!("starting oracle loop");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("tokenstats/", env!("CARGO_PKG_VERSION")))
        .build()?;

    if let Err(e) = refresh_btc_usd(&client, &store).await {
        warn!(error = %e, "initial BTC/USD fetch failed");
    }

    let store_rate = Arc::clone(&store);
    let client_rate = client.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RATE_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(e) = refresh_btc_usd(&client_rate, &store_rate).await {
                warn!(error = %e, "BTC/USD refresh failed");
            }
        }
    });

    let mut interval = tokio::time::interval(NORMALIZE_INTERVAL);
    loop {
        interval.tick().await;
        if let Some(btc_usd) = store.btc_usd() {
            store.apply_sats_normalization(btc_usd);
            debug!(btc_usd, "normalized quotes to dual USD/sats");
        }
        log_summary(&store);
    }
}

async fn refresh_btc_usd(client: &reqwest::Client, store: &Store) -> Result<()> {
    let v: serde_json::Value = client
        .get(BTC_USD_URL)
        .send()
        .await
        .context("coinbase request")?
        .error_for_status()
        .context("coinbase status")?
        .json()
        .await
        .context("coinbase json")?;

    let amount = v
        .pointer("/data/amount")
        .and_then(|x| x.as_str())
        .context("missing data.amount")?
        .parse::<f64>()
        .context("parse BTC/USD")?;

    store.set_btc_usd(amount);
    info!(btc_usd = amount, "BTC/USD rate updated");
    Ok(())
}

fn log_summary(store: &Store) {
    let quotes = store.list_quotes();
    let n = quotes.len();
    let nodes = store.node_count();
    let models: std::collections::BTreeSet<_> = quotes.iter().map(|q| q.model.as_str()).collect();
    let cheapest = cheapest_blended(&quotes);
    debug!(
        quotes = n,
        nodes,
        distinct_models = models.len(),
        cheapest_blend = ?cheapest.map(|(m, p)| format!("{m} ${p:.4}/M blend")),
        "oracle tick"
    );
}

fn cheapest_blended(quotes: &[Quote]) -> Option<(String, f64)> {
    quotes
        .iter()
        .filter_map(|q| {
            let b = blended_usd(q.price_in_usd, q.price_out_usd, 3.0)?;
            if b <= 0.0 {
                return None;
            }
            Some((q.model.clone(), b))
        })
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
}
