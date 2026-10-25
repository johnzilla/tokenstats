//! Price hygiene and dual-unit normalization (USD + sats per 1M tokens).
//!
//! # Hygiene (P0)
//! OpenRouter and some Routstr mirrors publish sentinel / dynamic prices
//! (`-1`, negative floats). Those must not be scaled into real costs.
//!
//! # Units (P1 / RIP-05)
//! Catalogs mix several shapes:
//! - **OpenRouter `pricing`**: USD **per token** (tiny floats, e.g. `1.15e-5`)
//! - **Routstr `sats_pricing`**: sats **per token** (e.g. `0.0179` → ~17.9k sats/1M)
//! - **Admin / alt fields**: sometimes already per-1M
//!
//! Public storage on quotes is always **per 1M tokens** in both USD and sats.

/// Sanitize a single price value (any unit: per-token or per-1M).
///
/// Returns `None` when the value must not be used for ranking or display as cost:
/// - non-finite (`NaN`, `±Inf`)
/// - negative (OpenRouter `-1` dynamic sentinel, broken catalogs)
///
/// Zero is kept (free models).
pub fn sanitize_price(p: f64) -> Option<f64> {
    if p.is_finite() && p >= 0.0 {
        Some(p)
    } else {
        None
    }
}

/// Plausible BTC/USD bounds. A feed glitch outside them (e.g. `0.000001`)
/// would silently corrupt every sats price, so such rates are rejected.
pub const BTC_USD_MIN: f64 = 100.0;
pub const BTC_USD_MAX: f64 = 10_000_000.0;

pub fn sane_btc_usd(rate: f64) -> bool {
    rate.is_finite() && (BTC_USD_MIN..=BTC_USD_MAX).contains(&rate)
}

/// Sanitize an optional price field.
pub fn sanitize_opt(p: Option<f64>) -> Option<f64> {
    p.and_then(sanitize_price)
}

/// Sort key for cheapest-first ranking.
///
/// Rankable blends must be **strictly positive**. Free (`0`), missing, and
/// non-positive values sort last (`f64::MAX`).
pub fn blend_sort_key(blend: Option<f64>) -> f64 {
    match blend {
        Some(b) if b > 0.0 && b.is_finite() => b,
        _ => f64::MAX,
    }
}

/// Whether a blended cost is usable for "cheapest" ranking / Best Now.
pub fn is_rankable_blend(blend: Option<f64>) -> bool {
    matches!(blend, Some(b) if b > 0.0 && b.is_finite())
}

/// Convert a per-token rate to per-1M tokens.
pub fn per_token_to_mtok(per_token: f64) -> f64 {
    per_token * 1_000_000.0
}

/// Which catalog side we treated as the native rate source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricePrimary {
    /// USD path only (typical OpenRouter).
    Usd,
    /// Sats path only (`sats_pricing` without usable USD pricing).
    Sats,
    /// Both sides filled from catalog.
    Both,
    /// No usable prices.
    None,
}

/// Dual-unit prices ready for a [`crate::store::Quote`] (always **per 1M**).
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedPrices {
    pub price_in_usd: Option<f64>,
    pub price_out_usd: Option<f64>,
    pub price_in_sats: Option<f64>,
    pub price_out_sats: Option<f64>,
    pub primary: PricePrimary,
}

impl NormalizedPrices {
    pub fn empty() -> Self {
        Self {
            price_in_usd: None,
            price_out_usd: None,
            price_in_sats: None,
            price_out_sats: None,
            primary: PricePrimary::None,
        }
    }
}

/// Threshold below which a raw `pricing.*` value is treated as **USD per token**
/// (OpenRouter style). At/above this, treat as already **per 1M** (USD or sats).
///
/// Live OpenRouter/Routstr USD-per-token rates are typically `≪ 1e-3`.
const USD_PER_TOKEN_MAX: f64 = 0.01;

/// Threshold below which a raw `sats_pricing.*` value is treated as **sats per token**.
/// Live samples are typically `≪ 1`; per-1M sats rates for non-free models are often ≥ 1.
const SATS_PER_TOKEN_MAX: f64 = 1.0;

/// Normalize catalog rate fields into dual-unit per-1M prices.
///
/// Decision tree:
/// 1. Sanitize all inputs (drop negatives / non-finite).
/// 2. If `sats_pricing` present → convert to sats/1M (per-token if `v < 1`, else already /1M).
/// 3. If `pricing` present → convert to USD/1M (per-token if `v < 0.01`, else already /1M).
/// 4. Else fall back to `input_cost` / `output_cost` with the same USD heuristics.
/// 5. Missing side is left `None` for the oracle to fill via BTC/USD.
pub fn normalize_catalog_prices(
    pricing_prompt: Option<f64>,
    pricing_completion: Option<f64>,
    sats_prompt: Option<f64>,
    sats_completion: Option<f64>,
    input_cost: Option<f64>,
    output_cost: Option<f64>,
) -> NormalizedPrices {
    let pricing_prompt = sanitize_opt(pricing_prompt);
    let pricing_completion = sanitize_opt(pricing_completion);
    let sats_prompt = sanitize_opt(sats_prompt);
    let sats_completion = sanitize_opt(sats_completion);
    let input_cost = sanitize_opt(input_cost);
    let output_cost = sanitize_opt(output_cost);

    let mut out = NormalizedPrices::empty();

    // --- sats side (prefer explicit sats_pricing) ---
    let (sats_in, sats_out) = match (sats_prompt, sats_completion) {
        (None, None) => (None, None),
        (sp, sc) => (
            sp.map(sats_rate_to_per_mtok).and_then(sanitize_price),
            sc.map(sats_rate_to_per_mtok).and_then(sanitize_price),
        ),
    };
    out.price_in_sats = sats_in;
    out.price_out_sats = sats_out;

    // --- USD side (pricing object, OpenRouter-shaped) ---
    let (usd_in, usd_out) = match (pricing_prompt, pricing_completion) {
        (None, None) => (None, None),
        (pp, pc) => (
            pp.map(usd_rate_to_per_mtok).and_then(sanitize_price),
            pc.map(usd_rate_to_per_mtok).and_then(sanitize_price),
        ),
    };
    out.price_in_usd = usd_in;
    out.price_out_usd = usd_out;

    // --- admin fallback when pricing object empty ---
    if out.price_in_usd.is_none() && out.price_out_usd.is_none() {
        if let Some(c) = input_cost {
            out.price_in_usd = Some(usd_rate_to_per_mtok(c)).and_then(sanitize_price);
        }
        if let Some(c) = output_cost {
            out.price_out_usd = Some(usd_rate_to_per_mtok(c)).and_then(sanitize_price);
        }
    }

    out.primary = out.derive_primary();
    out
}

/// Rates a catalog states explicitly per 1M tokens (e.g. OrcaRouter's
/// `pricing.prompt_per_million`). Already in our units, so no guessing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ExplicitPerMillion {
    pub usd_in: Option<f64>,
    pub usd_out: Option<f64>,
    pub sats_in: Option<f64>,
    pub sats_out: Option<f64>,
}

impl NormalizedPrices {
    /// Explicit per-1M fields win over the per-token vs per-1M magnitude
    /// heuristic for their side: a genuine price under $0.01/1M (or 1 sat/1M)
    /// would otherwise be read as per-token and inflated 1,000,000×.
    pub fn with_explicit_per_million(mut self, explicit: ExplicitPerMillion) -> Self {
        let pairs = [
            (&mut self.price_in_usd, explicit.usd_in),
            (&mut self.price_out_usd, explicit.usd_out),
            (&mut self.price_in_sats, explicit.sats_in),
            (&mut self.price_out_sats, explicit.sats_out),
        ];
        for (side, value) in pairs {
            if let Some(v) = sanitize_opt(value) {
                *side = Some(v);
            }
        }
        self.primary = self.derive_primary();
        self
    }

    fn derive_primary(&self) -> PricePrimary {
        let has_usd = self.price_in_usd.is_some() || self.price_out_usd.is_some();
        let has_sats = self.price_in_sats.is_some() || self.price_out_sats.is_some();
        match (has_usd, has_sats) {
            (true, true) => PricePrimary::Both,
            (true, false) => PricePrimary::Usd,
            (false, true) => PricePrimary::Sats,
            (false, false) => PricePrimary::None,
        }
    }
}

/// Map a USD catalog rate to USD per 1M tokens.
fn usd_rate_to_per_mtok(v: f64) -> f64 {
    if v < USD_PER_TOKEN_MAX {
        per_token_to_mtok(v)
    } else {
        // Already looks like per-1M (or a large absolute unit).
        v
    }
}

/// Map a sats catalog rate to sats per 1M tokens.
fn sats_rate_to_per_mtok(v: f64) -> f64 {
    if v < SATS_PER_TOKEN_MAX {
        per_token_to_mtok(v)
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_negative_and_sentinel() {
        assert_eq!(sanitize_price(-1.0), None);
        assert_eq!(sanitize_price(-1.15), None);
        assert_eq!(sanitize_price(-0.000001), None);
    }

    #[test]
    fn rejects_non_finite() {
        assert_eq!(sanitize_price(f64::NAN), None);
        assert_eq!(sanitize_price(f64::INFINITY), None);
        assert_eq!(sanitize_price(f64::NEG_INFINITY), None);
    }

    #[test]
    fn keeps_zero_and_positive() {
        assert_eq!(sanitize_price(0.0), Some(0.0));
        assert_eq!(sanitize_price(1.15e-5), Some(1.15e-5));
        assert_eq!(sanitize_price(3.0), Some(3.0));
    }

    #[test]
    fn blend_sort_key_orders_positives_ahead_of_junk() {
        assert!(blend_sort_key(Some(0.5)) < blend_sort_key(Some(1.0)));
        assert!(blend_sort_key(Some(1.0)) < blend_sort_key(Some(0.0)));
        assert!(blend_sort_key(Some(1.0)) < blend_sort_key(None));
        assert!(blend_sort_key(Some(1.0)) < blend_sort_key(Some(-1e6)));
        assert_eq!(blend_sort_key(Some(0.0)), f64::MAX);
        assert_eq!(blend_sort_key(None), f64::MAX);
    }

    #[test]
    fn is_rankable_blend_strictly_positive() {
        assert!(is_rankable_blend(Some(0.001)));
        assert!(!is_rankable_blend(Some(0.0)));
        assert!(!is_rankable_blend(Some(-1.0)));
        assert!(!is_rankable_blend(None));
    }

    #[test]
    fn openrouter_usd_per_token_to_mtok() {
        let n = normalize_catalog_prices(Some(0.000001), Some(0.000002), None, None, None, None);
        assert!((n.price_in_usd.unwrap() - 1.0).abs() < 1e-9);
        assert!((n.price_out_usd.unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(n.price_in_sats, None);
        assert_eq!(n.primary, PricePrimary::Usd);
    }

    #[test]
    fn routstr_dual_sats_pricing_and_usd() {
        // Live-shaped: USD 1.15e-5/token → $11.5/M; sats 0.0179/token → 17900 sats/M
        let n = normalize_catalog_prices(
            Some(1.15e-5),
            Some(5.75e-5),
            Some(0.0179),
            Some(0.0895),
            None,
            None,
        );
        assert!((n.price_in_usd.unwrap() - 11.5).abs() < 1e-9);
        assert!((n.price_out_usd.unwrap() - 57.5).abs() < 1e-9);
        assert!((n.price_in_sats.unwrap() - 17_900.0).abs() < 1.0);
        assert!((n.price_out_sats.unwrap() - 89_500.0).abs() < 1.0);
        assert_eq!(n.primary, PricePrimary::Both);
    }

    #[test]
    fn sats_only_catalog() {
        let n = normalize_catalog_prices(None, None, Some(0.001), Some(0.003), None, None);
        assert_eq!(n.price_in_usd, None);
        assert!((n.price_in_sats.unwrap() - 1000.0).abs() < 1e-6);
        assert!((n.price_out_sats.unwrap() - 3000.0).abs() < 1e-6);
        assert_eq!(n.primary, PricePrimary::Sats);
    }

    #[test]
    fn sats_already_per_mtok() {
        // Values ≥ 1 treated as sats/1M already
        let n = normalize_catalog_prices(None, None, Some(1500.0), Some(4500.0), None, None);
        assert_eq!(n.price_in_sats, Some(1500.0));
        assert_eq!(n.price_out_sats, Some(4500.0));
    }

    #[test]
    fn usd_already_per_mtok() {
        let n = normalize_catalog_prices(Some(3.0), Some(15.0), None, None, None, None);
        assert_eq!(n.price_in_usd, Some(3.0));
        assert_eq!(n.price_out_usd, Some(15.0));
    }

    #[test]
    fn dynamic_sentinel_clears_both_sides() {
        let n =
            normalize_catalog_prices(Some(-1.0), Some(-1.0), Some(-1.0), Some(-1.0), None, None);
        assert_eq!(n.primary, PricePrimary::None);
        assert_eq!(n.price_in_usd, None);
        assert_eq!(n.price_in_sats, None);
    }

    #[test]
    fn free_zero_dual() {
        let n = normalize_catalog_prices(Some(0.0), Some(0.0), Some(0.0), Some(0.0), None, None);
        assert_eq!(n.price_in_usd, Some(0.0));
        assert_eq!(n.price_out_sats, Some(0.0));
        assert_eq!(n.primary, PricePrimary::Both);
    }
}
