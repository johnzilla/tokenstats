//! OpenRouter public models catalog — reference centralized pricing.

use anyhow::{Context, Result};
use chrono::Utc;

use crate::pricing::CatalogHint;
use crate::store::Quote;

use super::catalog::parse_models_payload_with_hint;

/// OpenAI-compatible base URL advertised on OpenRouter quotes.
pub const ENDPOINT: &str = "https://openrouter.ai/api/v1";

pub async fn fetch_quotes(client: &reqwest::Client, url: &str) -> Result<Vec<Quote>> {
    let resp = client
        .get(url)
        .send()
        .await
        .context("OpenRouter request")?
        .error_for_status()
        .context("OpenRouter status")?
        .json::<serde_json::Value>()
        .await
        .context("OpenRouter JSON")?;

    let models = parse_models_payload_with_hint(&resp, CatalogHint::OpenRouter);
    let now = Utc::now();

    let quotes = models
        .into_iter()
        .map(|m| {
            let prices = m.prices;
            Quote {
                source: "openrouter".into(),
                provider: "OpenRouter".into(),
                provider_id: "openrouter".into(),
                model: m.id,
                price_in_usd: prices.price_in_usd,
                price_out_usd: prices.price_out_usd,
                price_in_sats: prices.price_in_sats,
                price_out_sats: prices.price_out_sats,
                prev_price_out_usd: None,
                prev_price_in_usd: None,
                endpoint: Some(ENDPOINT.into()),
                region: None,
                context_length: m.context_length,
                observed_at: now,
                raw_kind: None,
            }
        })
        .collect();

    Ok(quotes)
}
