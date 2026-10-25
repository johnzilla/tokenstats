//! Shared OpenAI / OpenRouter / Routstr-compatible models catalog parsing.

use serde::Deserialize;
use serde_json::Value;

use crate::pricing::{
    normalize_catalog_prices, sanitize_price, CatalogHint, ExplicitPerMillion, NormalizedPrices,
};

/// One model entry after loose JSON parsing.
#[derive(Debug, Clone)]
pub struct CatalogModel {
    pub id: String,
    #[allow(dead_code)]
    pub name: Option<String>,
    /// Normalized dual-unit prices (per 1M tokens).
    pub prices: NormalizedPrices,
    pub context_length: Option<u64>,
    /// See [`model_kind`].
    pub kind: String,
}

#[derive(Debug, Deserialize)]
struct Pricing {
    #[serde(default)]
    prompt: Option<Value>,
    #[serde(default)]
    completion: Option<Value>,
    // Routstr / alternate field names
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    output: Option<Value>,
    // Explicit per-1M rates (OrcaRouter); preferred over the magnitude heuristic.
    #[serde(default)]
    prompt_per_million: Option<Value>,
    #[serde(default)]
    completion_per_million: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ModelRow {
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    pricing: Option<Pricing>,
    /// Routstr dual-unit field: sats rates (typically per token).
    #[serde(default)]
    sats_pricing: Option<Pricing>,
    #[serde(default)]
    context_length: Option<u64>,
    /// Kept loose: catalogs send `output_modalities: null` and other shapes,
    /// and metadata must never cost us the price row.
    #[serde(default)]
    architecture: Option<Value>,
    // alternate shapes
    #[serde(default)]
    input_cost: Option<f64>,
    #[serde(default)]
    output_cost: Option<f64>,
}

/// Parse a models list JSON body into catalog rows.
/// Accepts `{ "data": [...] }`, `{ "models": [...] }`, or a bare array.
///
/// Unit interpretation uses [`CatalogHint::Routstr`] (safe for dual-fill; OpenRouter
/// simply has no `sats_pricing`).
#[cfg(test)]
pub fn parse_models_payload(body: &Value) -> Vec<CatalogModel> {
    parse_models_payload_with_hint(body, CatalogHint::Routstr)
}

/// Parse with an explicit catalog hint (OpenRouter vs Routstr).
pub fn parse_models_payload_with_hint(body: &Value, hint: CatalogHint) -> Vec<CatalogModel> {
    let rows = body
        .get("data")
        .or_else(|| body.get("models"))
        .cloned()
        .unwrap_or_else(|| {
            if body.is_array() {
                body.clone()
            } else {
                Value::Array(vec![])
            }
        });

    let Some(arr) = rows.as_array() else {
        return Vec::new();
    };

    arr.iter().filter_map(|v| parse_one(v, hint)).collect()
}

fn parse_one(v: &Value, hint: CatalogHint) -> Option<CatalogModel> {
    let row: ModelRow = serde_json::from_value(v.clone()).ok()?;
    let id = row.id.filter(|s| !s.is_empty())?;

    let mut explicit = ExplicitPerMillion::default();
    let (pricing_prompt, pricing_completion) = match &row.pricing {
        Some(p) => {
            explicit.usd_in = num_from_value(p.prompt_per_million.as_ref());
            explicit.usd_out = num_from_value(p.completion_per_million.as_ref());
            (
                num_from_value(p.prompt.as_ref().or(p.input.as_ref())),
                num_from_value(p.completion.as_ref().or(p.output.as_ref())),
            )
        }
        None => (None, None),
    };

    let (sats_prompt, sats_completion) = match &row.sats_pricing {
        Some(p) => {
            explicit.sats_in = num_from_value(p.prompt_per_million.as_ref());
            explicit.sats_out = num_from_value(p.completion_per_million.as_ref());
            (
                num_from_value(p.prompt.as_ref().or(p.input.as_ref())),
                num_from_value(p.completion.as_ref().or(p.output.as_ref())),
            )
        }
        None => (None, None),
    };

    let prices = normalize_catalog_prices(
        pricing_prompt,
        pricing_completion,
        sats_prompt,
        sats_completion,
        row.input_cost,
        row.output_cost,
        hint,
    )
    .with_explicit_per_million(explicit);

    let modalities: Vec<String> = row
        .architecture
        .as_ref()
        .and_then(|a| a.get("output_modalities"))
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(|m| m.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let kind = model_kind(&id, &modalities);

    Some(CatalogModel {
        id,
        name: row.name,
        prices,
        context_length: row.context_length,
        kind,
    })
}

/// Classify a model by what it outputs. Uses OpenRouter-style
/// `architecture.output_modalities` when published, else id heuristics;
/// anything unrecognized is assumed to be `chat` (the common case).
pub fn model_kind(id: &str, output_modalities: &[String]) -> String {
    let has = |m: &str| output_modalities.iter().any(|o| o.eq_ignore_ascii_case(m));
    let kind = if has("embedding") || has("embeddings") {
        "embedding"
    } else if has("text") {
        "chat"
    } else if has("image") {
        "image"
    } else if has("audio") {
        "audio"
    } else if !output_modalities.is_empty() {
        "other"
    } else {
        let id = id.to_ascii_lowercase();
        if id.contains("embed") {
            "embedding"
        } else if id.contains("rerank") {
            "rerank"
        } else if ["whisper", "tts", "transcribe"]
            .iter()
            .any(|k| id.contains(k))
        {
            "audio"
        } else {
            "chat"
        }
    };
    kind.to_string()
}

fn num_from_value(v: Option<&Value>) -> Option<f64> {
    let n = match v? {
        Value::String(s) => s.parse().ok()?,
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    // Drop OpenRouter-style dynamic sentinels (-1) and other invalids early.
    sanitize_price(n)
}

/// Accessors used by tests; production code reads `prices` directly.
#[cfg(test)]
impl CatalogModel {
    /// USD per 1M input tokens (if known).
    pub fn price_in_usd(&self) -> Option<f64> {
        self.prices.price_in_usd
    }

    /// USD per 1M output tokens (if known).
    pub fn price_out_usd(&self) -> Option<f64> {
        self.prices.price_out_usd
    }

    /// Sats per 1M input tokens (if known from `sats_pricing`).
    pub fn price_in_sats(&self) -> Option<f64> {
        self.prices.price_in_sats
    }

    /// Sats per 1M output tokens (if known from `sats_pricing`).
    pub fn price_out_sats(&self) -> Option<f64> {
        self.prices.price_out_sats
    }

    pub fn primary(&self) -> crate::pricing::PricePrimary {
        self.prices.primary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::PricePrimary;
    use serde_json::json;

    #[test]
    fn parse_openrouter_style_data_array() {
        let body = json!({
            "data": [{
                "id": "acme/model",
                "name": "Acme",
                "pricing": { "prompt": "0.000001", "completion": "0.000002" },
                "context_length": 8192
            }]
        });
        let models = parse_models_payload_with_hint(&body, CatalogHint::OpenRouter);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "acme/model");
        assert!((models[0].price_in_usd().unwrap() - 1.0).abs() < 1e-9);
        assert!((models[0].price_out_usd().unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(models[0].price_in_sats(), None);
        assert_eq!(models[0].context_length, Some(8192));
        assert_eq!(models[0].primary(), PricePrimary::Usd);
    }

    #[test]
    fn explicit_per_million_beats_magnitude_heuristic() {
        // A catalog stating $0.005 / 1M: read through the heuristic alone, the
        // `prompt` value (< 0.01) would be taken as per-token and become
        // $5,000 / 1M. The explicit per-1M field wins.
        let body = json!({ "data": [
            { "id": "tiny", "pricing": {
                "prompt": "0.005", "completion": "0.008",
                "prompt_per_million": "0.005", "completion_per_million": "0.008"
            } },
            { "id": "sats", "sats_pricing": {
                "prompt": "0.5", "prompt_per_million": "0.5"
            } },
            { "id": "heuristic-only", "pricing": { "prompt": "0.000002" } }
        ]});
        let m = parse_models_payload_with_hint(&body, CatalogHint::OpenRouter);
        assert!((m[0].price_in_usd().unwrap() - 0.005).abs() < 1e-12);
        assert!((m[0].price_out_usd().unwrap() - 0.008).abs() < 1e-12);
        assert!((m[1].price_in_sats().unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(m[1].primary(), PricePrimary::Sats);
        // No explicit field: unchanged heuristic (per-token → per-1M).
        assert!((m[2].price_in_usd().unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn kind_from_output_modalities_then_id() {
        let body = json!({ "data": [
            { "id": "a/chat", "architecture": { "output_modalities": ["text"] } },
            { "id": "a/vec", "architecture": { "output_modalities": ["embeddings"] } },
            { "id": "a/pic", "architecture": { "output_modalities": ["image"] } },
            { "id": "a/odd", "architecture": { "output_modalities": ["decisions"] } },
            { "id": "null-mods-embed", "architecture": { "output_modalities": null } },
            { "id": "bad-arch", "architecture": "text->text" },
            { "id": "text-embedding-bge-m3" },
            { "id": "whisper-1" },
            { "id": "plain-model" }
        ]});
        let kinds: Vec<_> = parse_models_payload_with_hint(&body, CatalogHint::Routstr)
            .into_iter()
            .map(|m| m.kind)
            .collect();
        // Null / odd `architecture` must not drop the row; falls back to id.
        let want = ["chat", "embedding", "image", "other", "embedding", "chat"];
        assert_eq!(kinds[..6], want);
        assert_eq!(kinds[6..], ["embedding", "audio", "chat"]);
    }

    #[test]
    fn parse_models_key_and_bare_array() {
        let body =
            json!({ "models": [{ "id": "x", "pricing": { "prompt": "0", "completion": "0" } }] });
        assert_eq!(parse_models_payload(&body).len(), 1);
        let bare = json!([{ "id": "y", "input_cost": 5.0, "output_cost": 15.0 }]);
        let m = parse_models_payload(&bare);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].price_in_usd(), Some(5.0));
    }

    #[test]
    fn openrouter_dynamic_sentinel_becomes_none() {
        let body = json!({
            "data": [{
                "id": "openrouter/auto",
                "pricing": { "prompt": "-1", "completion": "-1" }
            }]
        });
        let m = parse_models_payload_with_hint(&body, CatalogHint::OpenRouter);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].price_in_usd(), None);
        assert_eq!(m[0].price_out_usd(), None);
        assert_eq!(m[0].primary(), PricePrimary::None);
    }

    #[test]
    fn negative_numeric_pricing_dropped() {
        let body = json!({
            "data": [{
                "id": "auto-beta",
                "pricing": { "prompt": -1.15, "completion": -1.15 }
            }]
        });
        let m = parse_models_payload(&body);
        assert_eq!(m[0].price_in_usd(), None);
        assert_eq!(m[0].price_out_usd(), None);
    }

    #[test]
    fn free_zero_pricing_kept() {
        let body = json!({
            "data": [{
                "id": "free/model",
                "pricing": { "prompt": "0", "completion": "0" }
            }]
        });
        let m = parse_models_payload(&body);
        assert_eq!(m[0].price_in_usd(), Some(0.0));
        assert_eq!(m[0].price_out_usd(), Some(0.0));
    }

    #[test]
    fn mixed_valid_and_sentinel_sides() {
        let body = json!({
            "data": [{
                "id": "half",
                "pricing": { "prompt": "0.000001", "completion": "-1" }
            }]
        });
        let m = parse_models_payload(&body);
        assert!((m[0].price_in_usd().unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(m[0].price_out_usd(), None);
    }

    #[test]
    fn parses_sats_pricing_dual_unit() {
        // Shaped like live PlebChat / satsandsports Routstr nodes.
        let body = json!({
            "data": [{
                "id": "claude-opus-5-fast",
                "pricing": {
                    "prompt": 1.15e-5,
                    "completion": 5.75e-5
                },
                "sats_pricing": {
                    "prompt": 0.0179,
                    "completion": 0.0895
                },
                "context_length": 1_000_000
            }]
        });
        let m = parse_models_payload_with_hint(&body, CatalogHint::Routstr);
        assert_eq!(m.len(), 1);
        assert!((m[0].price_in_usd().unwrap() - 11.5).abs() < 1e-9);
        assert!((m[0].price_out_usd().unwrap() - 57.5).abs() < 1e-9);
        assert!((m[0].price_in_sats().unwrap() - 17_900.0).abs() < 1.0);
        assert!((m[0].price_out_sats().unwrap() - 89_500.0).abs() < 1.0);
        assert_eq!(m[0].primary(), PricePrimary::Both);
    }

    #[test]
    fn sats_pricing_only() {
        let body = json!({
            "data": [{
                "id": "local-llm",
                "sats_pricing": { "prompt": 0.002, "completion": 0.006 }
            }]
        });
        let m = parse_models_payload(&body);
        assert_eq!(m[0].price_in_usd(), None);
        assert!((m[0].price_in_sats().unwrap() - 2000.0).abs() < 1e-6);
        assert!((m[0].price_out_sats().unwrap() - 6000.0).abs() < 1e-6);
        assert_eq!(m[0].primary(), PricePrimary::Sats);
    }
}
