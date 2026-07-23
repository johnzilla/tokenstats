//! OpenRouter public models catalog — reference centralized pricing.

use anyhow::{Context, Result};
use chrono::Utc;

use crate::store::Quote;

use super::catalog::parse_models_payload;

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

    let models = parse_models_payload(&resp);
    let now = Utc::now();

    let quotes = models
        .into_iter()
        .map(|m| {
            let pin = m.price_in_per_mtok();
            let pout = m.price_out_per_mtok();
            Quote {
                source: "openrouter".into(),
                provider: "OpenRouter".into(),
                provider_id: "openrouter".into(),
                model: m.id,
                price_in_usd: pin,
                price_out_usd: pout,
                price_in_sats: None,
                price_out_sats: None,
                prev_price_out_usd: None,
                prev_price_in_usd: None,
                endpoint: Some("https://openrouter.ai/api/v1".into()),
                region: None,
                context_length: m.context_length,
                observed_at: now,
                raw_kind: None,
            }
        })
        .collect();

    Ok(quotes)
}
