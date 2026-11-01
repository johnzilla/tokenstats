//! Price history: one point per published price change, per quote key.
//!
//! Only the unit a catalog publishes is tracked (USD when it lists USD, else
//! sats). The converted side moves with BTC/USD on every poll, so tracking it
//! would turn every row into a series of noise.

use std::collections::VecDeque;

use crate::market::blended_usd;

/// Relative move below which a price counts as unchanged (0.1%). Absorbs float
/// noise and nodes whose USD is derived from sats at their own BTC rate.
pub const PRICE_CHANGE_EPSILON: f64 = 0.001;
/// Points kept per series; the oldest go first.
pub const MAX_POINTS_PER_SERIES: usize = 1_000;
/// Lookback windows for per-quote blend changes, in seconds.
pub const CHANGE_WINDOWS: [i64; 3] = [86_400, 7 * 86_400, 30 * 86_400];

/// Which unit a series is tracked in (what the catalog publishes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceUnit {
    Usd,
    Sats,
}

impl PriceUnit {
    pub fn as_str(self) -> &'static str {
        match self {
            PriceUnit::Usd => "usd",
            PriceUnit::Sats => "sats",
        }
    }
}

/// A price as first observed at `at`, in the catalog's own unit (the other
/// unit's fields are `None`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PricePoint {
    /// Unix seconds.
    pub at: i64,
    pub price_in_usd: Option<f64>,
    pub price_out_usd: Option<f64>,
    pub price_in_sats: Option<f64>,
    pub price_out_sats: Option<f64>,
}

impl PricePoint {
    /// Keep the published unit of a freshly fetched quote: USD when either
    /// USD side is set, else sats.
    pub fn native(
        at: i64,
        price_in_usd: Option<f64>,
        price_out_usd: Option<f64>,
        price_in_sats: Option<f64>,
        price_out_sats: Option<f64>,
    ) -> Self {
        if price_in_usd.is_some() || price_out_usd.is_some() {
            Self {
                at,
                price_in_usd,
                price_out_usd,
                price_in_sats: None,
                price_out_sats: None,
            }
        } else {
            Self {
                at,
                price_in_usd: None,
                price_out_usd: None,
                price_in_sats,
                price_out_sats,
            }
        }
    }

    pub fn unit(&self) -> PriceUnit {
        if self.price_in_usd.is_some() || self.price_out_usd.is_some() {
            PriceUnit::Usd
        } else {
            PriceUnit::Sats
        }
    }

    /// (in, out) per 1M tokens in [`Self::unit`].
    pub fn prices(&self) -> (Option<f64>, Option<f64>) {
        match self.unit() {
            PriceUnit::Usd => (self.price_in_usd, self.price_out_usd),
            PriceUnit::Sats => (self.price_in_sats, self.price_out_sats),
        }
    }

    /// Blend in [`Self::unit`].
    pub fn blended(&self, ratio: f64) -> Option<f64> {
        let (i, o) = self.prices();
        blended_usd(i, o, ratio)
    }

    fn same_price(&self, other: &PricePoint) -> bool {
        let (ai, ao) = self.prices();
        let (bi, bo) = other.prices();
        self.unit() == other.unit() && near(ai, bi) && near(ao, bo)
    }
}

fn near(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() <= PRICE_CHANGE_EPSILON * x.abs().max(y.abs()),
        (None, None) => true,
        _ => false,
    }
}

/// Append `p` unless it repeats the latest price. Returns whether it was kept.
pub fn record(series: &mut VecDeque<PricePoint>, p: PricePoint) -> bool {
    if let Some(last) = series.back() {
        if last.same_price(&p) {
            return false;
        }
        // Clock went backwards (or two fetches in one second): latest wins.
        if last.at >= p.at {
            series.pop_back();
        }
    }
    series.push_back(p);
    while series.len() > MAX_POINTS_PER_SERIES {
        series.pop_front();
    }
    true
}

/// Drop points older than `cutoff`, except the newest of them: that is the
/// price in effect at `cutoff`. Returns the number dropped.
pub fn prune(series: &mut VecDeque<PricePoint>, cutoff: i64) -> usize {
    let older = series.partition_point(|p| p.at < cutoff);
    let drop = older.saturating_sub(1);
    series.drain(..drop);
    drop
}

/// The price in effect at `t`: the newest point at or before it.
pub fn price_at(series: &VecDeque<PricePoint>, t: i64) -> Option<PricePoint> {
    series
        .partition_point(|p| p.at <= t)
        .checked_sub(1)
        .map(|i| series[i])
}

/// What the API needs per quote without copying whole series.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceSummary {
    /// First retained point: the price is known for every moment since.
    pub since: i64,
    /// Latest change; `None` while the series has a single point.
    pub changed_at: Option<i64>,
    pub current: PricePoint,
    /// Price in effect [`CHANGE_WINDOWS`] ago; `None` when tracking began later.
    pub ago: [Option<PricePoint>; 3],
}

impl PriceSummary {
    pub fn of(series: &VecDeque<PricePoint>, now: i64) -> Option<Self> {
        let first = series.front()?;
        let current = *series.back()?;
        Some(Self {
            since: first.at,
            changed_at: (series.len() > 1).then_some(current.at),
            current,
            ago: CHANGE_WINDOWS.map(|w| price_at(series, now - w)),
        })
    }

    /// % change of the blend over `CHANGE_WINDOWS[window]`; `None` without a
    /// full window of history, a unit switch, or an unpriced end.
    pub fn blend_change_pct(&self, window: usize, ratio: f64) -> Option<f64> {
        let then = self.ago.get(window).copied().flatten()?;
        if then.unit() != self.current.unit() {
            return None;
        }
        let b0 = then.blended(ratio).filter(|b| *b > 0.0)?;
        let b1 = self.current.blended(ratio).filter(|b| *b >= 0.0)?;
        Some((b1 - b0) / b0 * 100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(at: i64, i: f64, o: f64) -> PricePoint {
        PricePoint::native(at, Some(i), Some(o), Some(i * 1500.0), Some(o * 1500.0))
    }

    #[test]
    fn native_keeps_the_published_unit() {
        let p = usd(1, 1.0, 2.0);
        assert_eq!(p.unit(), PriceUnit::Usd);
        assert_eq!(p.price_in_sats, None);
        let s = PricePoint::native(1, None, None, Some(10.0), Some(20.0));
        assert_eq!(s.unit(), PriceUnit::Sats);
        assert_eq!(s.prices(), (Some(10.0), Some(20.0)));
    }

    #[test]
    fn record_skips_repeats_and_tiny_moves() {
        let mut s = VecDeque::new();
        assert!(record(&mut s, usd(100, 1.0, 2.0)));
        assert!(!record(&mut s, usd(160, 1.0, 2.0)));
        assert!(!record(&mut s, usd(220, 1.0005, 2.0)));
        assert!(record(&mut s, usd(280, 1.0, 2.5)));
        // Dropping a side is a change too.
        assert!(record(
            &mut s,
            PricePoint::native(340, Some(1.0), None, None, None)
        ));
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn record_caps_series_length() {
        let mut s = VecDeque::new();
        for i in 0..(MAX_POINTS_PER_SERIES as i64 + 10) {
            record(&mut s, usd(i, 1.0 + (i % 2) as f64, 2.0));
        }
        assert_eq!(s.len(), MAX_POINTS_PER_SERIES);
        assert_eq!(s.front().unwrap().at, 10);
    }

    #[test]
    fn prune_keeps_the_price_in_effect_at_cutoff() {
        let mut s: VecDeque<_> = [usd(10, 1.0, 1.0), usd(20, 2.0, 2.0), usd(40, 3.0, 3.0)].into();
        assert_eq!(prune(&mut s, 30), 1);
        assert_eq!(s.iter().map(|p| p.at).collect::<Vec<_>>(), [20, 40]);
        assert_eq!(prune(&mut s, 30), 0);
    }

    #[test]
    fn price_at_is_a_step_function() {
        let s: VecDeque<_> = [usd(10, 1.0, 1.0), usd(20, 2.0, 2.0)].into();
        assert_eq!(price_at(&s, 5), None);
        assert_eq!(price_at(&s, 10).unwrap().at, 10);
        assert_eq!(price_at(&s, 19).unwrap().at, 10);
        assert_eq!(price_at(&s, 99).unwrap().at, 20);
    }

    #[test]
    fn summary_blend_change_needs_a_full_window() {
        let day = 86_400;
        let now = 40 * day;
        let s: VecDeque<_> = [usd(0, 1.0, 1.0), usd(now - day / 2, 2.0, 2.0)].into();
        let sum = PriceSummary::of(&s, now).unwrap();
        assert_eq!(sum.since, 0);
        assert_eq!(sum.changed_at, Some(now - day / 2));
        assert_eq!(sum.blend_change_pct(0, 3.0), Some(100.0));
        assert_eq!(sum.blend_change_pct(2, 3.0), Some(100.0));

        // Tracking started 2 days ago: no 7d / 30d figure.
        let young: VecDeque<_> = [usd(now - 2 * day, 1.0, 1.0)].into();
        let sum = PriceSummary::of(&young, now).unwrap();
        assert_eq!(sum.changed_at, None);
        assert_eq!(sum.blend_change_pct(0, 3.0), Some(0.0));
        assert_eq!(sum.blend_change_pct(1, 3.0), None);
    }

    #[test]
    fn unit_switch_has_no_change_pct() {
        let day = 86_400;
        let s: VecDeque<_> = [
            PricePoint::native(0, None, None, Some(10.0), Some(10.0)),
            usd(day * 2 + 60, 1.0, 1.0),
        ]
        .into();
        let sum = PriceSummary::of(&s, day * 3).unwrap();
        assert_eq!(sum.blend_change_pct(0, 3.0), None);
    }
}
