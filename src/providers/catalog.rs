//! Shared OpenAI / OpenRouter / Routstr-compatible models catalog parsing.

use serde::Deserialize;
use serde_json::Value;

use crate::pricing::sanitize_price;

/// One model entry after loose JSON parsing.
#[derive(Debug, Clone)]
pub struct CatalogModel {
    pub id: String,
    #[allow(dead_code)]
    pub name: Option<String>,
    /// USD (or sats — caller decides) per *token* for prompt, if raw string present.
    pub prompt_per_token: Option<f64>,
    pub completion_per_token: Option<f64>,
    /// Already-normalized per-1M values when the source uses that unit.
    pub prompt_per_mtok: Option<f64>,
    pub completion_per_mtok: Option<f64>,
    pub context_length: Option<u64>,
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
}

#[derive(Debug, Deserialize)]
struct ModelRow {
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    pricing: Option<Pricing>,
    #[serde(default)]
    context_length: Option<u64>,
    // alternate shapes
    #[serde(default)]
    input_cost: Option<f64>,
    #[serde(default)]
    output_cost: Option<f64>,
}

/// Parse a models list JSON body into catalog rows.
/// Accepts `{ "data": [...] }`, `{ "models": [...] }`, or a bare array.
pub fn parse_models_payload(body: &Value) -> Vec<CatalogModel> {
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

    arr.iter().filter_map(parse_one).collect()
}

fn parse_one(v: &Value) -> Option<CatalogModel> {
    let row: ModelRow = serde_json::from_value(v.clone()).ok()?;
    let id = row.id.filter(|s| !s.is_empty())?;

    let mut prompt_per_token = None;
    let mut completion_per_token = None;
    let mut prompt_per_mtok = None;
    let mut completion_per_mtok = None;

    if let Some(p) = row.pricing {
        prompt_per_token = num_from_value(p.prompt.as_ref().or(p.input.as_ref()));
        completion_per_token = num_from_value(p.completion.as_ref().or(p.output.as_ref()));
    }

    // Admin-style fields sometimes store cost per 1k or absolute — treat as per-1M if large.
    if let Some(c) = row.input_cost {
        if prompt_per_token.is_none() && prompt_per_mtok.is_none() {
            // Heuristic: values > 1 are likely USD/1M; tiny values are per-token.
            // Negatives/sentinels are dropped by sanitize_price.
            if let Some(c) = sanitize_price(c) {
                if c > 1.0 {
                    prompt_per_mtok = Some(c);
                } else {
                    prompt_per_token = Some(c);
                }
            }
        }
    }
    if let Some(c) = row.output_cost {
        if completion_per_token.is_none() && completion_per_mtok.is_none() {
            if let Some(c) = sanitize_price(c) {
                if c > 1.0 {
                    completion_per_mtok = Some(c);
                } else {
                    completion_per_token = Some(c);
                }
            }
        }
    }

    Some(CatalogModel {
        id,
        name: row.name,
        prompt_per_token,
        completion_per_token,
        prompt_per_mtok,
        completion_per_mtok,
        context_length: row.context_length,
    })
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

/// Convert per-token price → per-1M tokens.
pub fn per_token_to_mtok(per_token: f64) -> f64 {
    per_token * 1_000_000.0
}

impl CatalogModel {
    /// Best-effort USD (or native unit) per 1M tokens.
    /// Invalid / dynamic sentinels are returned as `None`.
    pub fn price_in_per_mtok(&self) -> Option<f64> {
        self.prompt_per_mtok
            .and_then(sanitize_price)
            .or_else(|| {
                self.prompt_per_token
                    .and_then(sanitize_price)
                    .map(per_token_to_mtok)
                    .and_then(sanitize_price)
            })
    }

    pub fn price_out_per_mtok(&self) -> Option<f64> {
        self.completion_per_mtok
            .and_then(sanitize_price)
            .or_else(|| {
                self.completion_per_token
                    .and_then(sanitize_price)
                    .map(per_token_to_mtok)
                    .and_then(sanitize_price)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let models = parse_models_payload(&body);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "acme/model");
        assert!((models[0].price_in_per_mtok().unwrap() - 1.0).abs() < 1e-9);
        assert!((models[0].price_out_per_mtok().unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(models[0].context_length, Some(8192));
    }

    #[test]
    fn parse_models_key_and_bare_array() {
        let body = json!({ "models": [{ "id": "x", "pricing": { "prompt": "0", "completion": "0" } }] });
        assert_eq!(parse_models_payload(&body).len(), 1);
        let bare = json!([{ "id": "y", "input_cost": 5.0, "output_cost": 15.0 }]);
        let m = parse_models_payload(&bare);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].price_in_per_mtok(), Some(5.0));
    }

    #[test]
    fn openrouter_dynamic_sentinel_becomes_none() {
        // OpenRouter uses "-1" for variable-price routers (auto, etc.)
        let body = json!({
            "data": [{
                "id": "openrouter/auto",
                "pricing": { "prompt": "-1", "completion": "-1" }
            }]
        });
        let m = parse_models_payload(&body);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].price_in_per_mtok(), None);
        assert_eq!(m[0].price_out_per_mtok(), None);
        assert_eq!(m[0].prompt_per_token, None);
        assert_eq!(m[0].completion_per_token, None);
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
        assert_eq!(m[0].price_in_per_mtok(), None);
        assert_eq!(m[0].price_out_per_mtok(), None);
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
        assert_eq!(m[0].price_in_per_mtok(), Some(0.0));
        assert_eq!(m[0].price_out_per_mtok(), Some(0.0));
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
        assert!((m[0].price_in_per_mtok().unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(m[0].price_out_per_mtok(), None);
    }
}
