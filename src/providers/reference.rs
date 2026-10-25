//! Centralized reference catalogs (OpenRouter, OrcaRouter): public
//! OpenRouter-shaped `/models` lists with USD pricing.

use anyhow::{Context, Result};
use chrono::Utc;

use crate::store::Quote;

use super::catalog::parse_models_payload;

/// One centralized catalog. `source` doubles as `provider_id` on its quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceCatalog {
    pub source: &'static str,
    /// Display name on quotes.
    pub provider: &'static str,
    /// OpenAI-compatible base URL advertised on quotes.
    pub endpoint: &'static str,
}

pub const OPENROUTER: ReferenceCatalog = ReferenceCatalog {
    source: "openrouter",
    provider: "OpenRouter",
    endpoint: "https://openrouter.ai/api/v1",
};

/// Zero-markup router; models without per-token pricing (per-request image
/// models, meta-routers) are kept with null prices.
pub const ORCAROUTER: ReferenceCatalog = ReferenceCatalog {
    source: "orcarouter",
    provider: "OrcaRouter",
    endpoint: "https://api.orcarouter.ai/v1",
};

pub async fn fetch_quotes(
    client: &reqwest::Client,
    url: &str,
    cat: &ReferenceCatalog,
) -> Result<Vec<Quote>> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("{} request", cat.provider))?
        .error_for_status()
        .with_context(|| format!("{} status", cat.provider))?
        .json::<serde_json::Value>()
        .await
        .with_context(|| format!("{} JSON", cat.provider))?;

    Ok(quotes_from_payload(&resp, cat))
}

fn quotes_from_payload(body: &serde_json::Value, cat: &ReferenceCatalog) -> Vec<Quote> {
    let now = Utc::now();
    parse_models_payload(body)
        .into_iter()
        .map(|m| {
            let prices = m.prices;
            Quote {
                source: cat.source.into(),
                provider: cat.provider.into(),
                provider_id: cat.source.into(),
                model: m.id,
                price_in_usd: prices.price_in_usd,
                price_out_usd: prices.price_out_usd,
                price_in_sats: prices.price_in_sats,
                price_out_sats: prices.price_out_sats,
                prev_price_out_usd: None,
                prev_price_in_usd: None,
                endpoint: Some(cat.endpoint.into()),
                region: None,
                context_length: m.context_length,
                observed_at: now,
                raw_kind: None,
                kind: m.kind,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Trimmed rows from the live `api.orcarouter.ai/v1/models` payload.
    #[test]
    fn orcarouter_payload_shapes() {
        let body = json!({ "data": [
            {
                "id": "anthropic/claude-opus-5",
                "architecture": { "output_modalities": ["text"] },
                "context_length": 1000000,
                "pricing": {
                    "prompt": "0.0000050000",
                    "completion": "0.0000250000",
                    "prompt_per_million": "5.000000",
                    "completion_per_million": "25.000000",
                    "input_cache_read": "0.0000005000"
                }
            },
            { "id": "google/gemini-3-pro-image-preview", "pricing": { "request": "0.240000" } },
            { "id": "orcarouter/fusion", "context_length": 1000000 }
        ]});
        let q = quotes_from_payload(&body, &ORCAROUTER);
        assert_eq!(q.len(), 3);
        assert_eq!(q[0].source, "orcarouter");
        assert_eq!(
            q[0].endpoint.as_deref(),
            Some("https://api.orcarouter.ai/v1")
        );
        assert!((q[0].price_in_usd.unwrap() - 5.0).abs() < 1e-9);
        assert!((q[0].price_out_usd.unwrap() - 25.0).abs() < 1e-9);
        assert_eq!(q[0].kind, "chat");
        // Per-request pricing is not a token price.
        assert_eq!((q[1].price_in_usd, q[1].price_out_usd), (None, None));
        // Meta-router without pricing.
        assert_eq!((q[2].price_in_usd, q[2].price_out_usd), (None, None));
    }
}
