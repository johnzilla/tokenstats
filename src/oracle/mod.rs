//! Oracle: BTC/USD rate + dual-unit normalization + market summary.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::market::blended_usd;
use crate::store::{Quote, Store};

#[derive(Debug, Clone)]
pub struct OracleConfig {
    pub rate_interval: Duration,
    pub normalize_interval: Duration,
    pub http_timeout: Duration,
    pub btc_usd_url: String,
}

pub async fn run_oracle(
    store: Arc<Store>,
    cfg: OracleConfig,
    cancel: CancellationToken,
) -> Result<()> {
    info!(
        rate_interval_secs = cfg.rate_interval.as_secs(),
        normalize_interval_secs = cfg.normalize_interval.as_secs(),
        "starting oracle loop"
    );

    let client = reqwest::Client::builder()
        .timeout(cfg.http_timeout)
        .user_agent(concat!("tokenstats/", env!("CARGO_PKG_VERSION")))
        .build()?;

    if let Err(e) = refresh_btc_usd(&client, &store, &cfg.btc_usd_url).await {
        warn!(error = %e, "initial BTC/USD fetch failed");
    }

    let mut rate_interval = tokio::time::interval(cfg.rate_interval);
    rate_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    rate_interval.tick().await;

    let mut normalize_interval = tokio::time::interval(cfg.normalize_interval);
    normalize_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    normalize_interval.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("oracle received shutdown");
                break;
            }
            _ = rate_interval.tick() => {
                if let Err(e) = refresh_btc_usd(&client, &store, &cfg.btc_usd_url).await {
                    warn!(error = %e, "BTC/USD refresh failed");
                }
            }
            _ = normalize_interval.tick() => {
                if let Some(btc_usd) = store.btc_usd() {
                    store.apply_sats_normalization(btc_usd);
                    debug!(btc_usd, "normalized quotes to dual USD/sats");
                }
                log_summary(&store);
            }
        }
    }

    Ok(())
}

async fn refresh_btc_usd(client: &reqwest::Client, store: &Store, url: &str) -> Result<()> {
    let v: serde_json::Value = client
        .get(url)
        .send()
        .await
        .context("btc usd request")?
        .error_for_status()
        .context("btc usd status")?
        .json()
        .await
        .context("btc usd json")?;

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
