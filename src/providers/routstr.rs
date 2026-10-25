//! Poll a Routstr-compatible node: GET {endpoint}/v1/models

use anyhow::{Context, Result};
use chrono::Utc;
use tracing::debug;

use crate::pricing::CatalogHint;
use crate::store::{ProviderNode, Quote};

use super::catalog::parse_models_payload_with_hint;

pub async fn fetch_node_quotes(
    client: &reqwest::Client,
    node: &ProviderNode,
) -> Result<(Vec<Quote>, u64)> {
    let base = node.endpoint.trim_end_matches('/');
    // `/v1` bases have one candidate; bare hosts try `/v1/models` then `/models`.
    let urls = if base.ends_with("/v1") {
        vec![format!("{base}/models")]
    } else {
        vec![format!("{base}/v1/models"), format!("{base}/models")]
    };

    let mut last_err = None;
    for url in &urls {
        let started = std::time::Instant::now();
        match try_fetch(client, url).await {
            Ok(body) => {
                let latency_ms = started.elapsed().as_millis() as u64;
                let models = parse_models_payload_with_hint(&body, CatalogHint::Routstr);
                debug!(%url, n = models.len(), latency_ms, "Routstr models response");
                let now = Utc::now();
                let quotes = models
                    .into_iter()
                    .map(|m| {
                        let prices = m.prices;
                        Quote {
                            source: "routstr".into(),
                            provider: node.name.clone(),
                            provider_id: node.provider_id.clone(),
                            model: m.id,
                            // Dual-unit: USD from `pricing`, sats from `sats_pricing` when present.
                            // Oracle fills any missing side via BTC/USD.
                            price_in_usd: prices.price_in_usd,
                            price_out_usd: prices.price_out_usd,
                            price_in_sats: prices.price_in_sats,
                            price_out_sats: prices.price_out_sats,
                            prev_price_out_usd: None,
                            prev_price_in_usd: None,
                            endpoint: Some(base.to_string()),
                            region: node.region.clone(),
                            context_length: m.context_length,
                            observed_at: now,
                            raw_kind: None,
                            kind: m.kind,
                        }
                    })
                    .collect();
                return Ok((quotes, latency_ms));
            }
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no models URL worked for {base}")))
}

async fn try_fetch(client: &reqwest::Client, url: &str) -> Result<serde_json::Value> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("GET {url} → {}", resp.status());
    }
    resp.json().await.with_context(|| format!("JSON {url}"))
}
