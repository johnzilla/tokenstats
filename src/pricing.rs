//! Price hygiene for catalog quotes.
//!
//! OpenRouter and some Routstr mirrors publish sentinel / dynamic prices
//! (`-1`, negative floats). Treating those as real per-token rates and
//! scaling to per-1M produces absurd negatives (e.g. −$1e6/M) that poison
//! cheapest-first sorts and downstream consumers (arbstr).

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
}
