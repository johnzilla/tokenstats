//! Hourly per-endpoint poll history (7 days) with latency histograms.
//!
//! Raw samples cover only the last hour (`reliability_1h`); these buckets back
//! the 24h / 7d windows and latency percentiles, and are small enough to persist
//! (~170 rows per endpoint).

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Buckets kept per endpoint (7 days).
pub const HISTORY_HOURS: i64 = 24 * 7;

/// Upper edges (ms) of the latency histogram bins, ~25% apart. One overflow
/// bin follows; percentiles that land there report the last edge.
pub const LATENCY_BINS_MS: &[u64] = &[
    25, 32, 40, 50, 63, 80, 100, 125, 160, 200, 250, 315, 400, 500, 630, 800, 1000, 1250, 1600,
    2000, 2500, 3150, 4000, 5000, 6300, 8000, 10000, 12500, 16000, 20000,
];

/// Poll outcomes for one endpoint during one clock hour.
#[derive(Debug, Clone, PartialEq)]
pub struct HourBucket {
    /// Hours since the Unix epoch (see [`hour_of`]).
    pub hour: i64,
    pub ok: u32,
    pub total: u32,
    /// Latency counts of successful polls, one per bin plus overflow.
    pub latency_hist: Vec<u32>,
}

impl HourBucket {
    pub fn new(hour: i64) -> Self {
        Self {
            hour,
            ok: 0,
            total: 0,
            latency_hist: vec![0; LATENCY_BINS_MS.len() + 1],
        }
    }

    pub fn record(&mut self, ok: bool, latency_ms: Option<u64>) {
        self.total += 1;
        if ok {
            self.ok += 1;
            if let Some(ms) = latency_ms {
                self.latency_hist[latency_bin(ms)] += 1;
            }
        }
    }

    /// Compact `a,b,c` form for SQLite (trailing zero bins dropped).
    pub fn hist_to_string(&self) -> String {
        let end = self
            .latency_hist
            .iter()
            .rposition(|c| *c > 0)
            .map_or(0, |i| i + 1);
        self.latency_hist[..end]
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Inverse of [`Self::hist_to_string`]; tolerates a different bin count.
    pub fn hist_from_str(s: &str) -> Vec<u32> {
        let mut hist: Vec<u32> = s
            .split(',')
            .filter(|p| !p.is_empty())
            .map(|p| p.trim().parse().unwrap_or(0))
            .collect();
        hist.resize(LATENCY_BINS_MS.len() + 1, 0);
        hist
    }
}

pub fn hour_of(t: DateTime<Utc>) -> i64 {
    t.timestamp().div_euclid(3600)
}

fn latency_bin(ms: u64) -> usize {
    LATENCY_BINS_MS
        .iter()
        .position(|edge| ms <= *edge)
        .unwrap_or(LATENCY_BINS_MS.len())
}

/// Append a sample to the endpoint's buckets and drop expired hours.
pub fn record(
    buckets: &mut VecDeque<HourBucket>,
    now_hour: i64,
    ok: bool,
    latency_ms: Option<u64>,
) {
    if buckets.back().map(|b| b.hour) != Some(now_hour) {
        buckets.push_back(HourBucket::new(now_hour));
    }
    if let Some(b) = buckets.back_mut() {
        b.record(ok, latency_ms);
    }
    prune(buckets, now_hour);
}

pub fn prune(buckets: &mut VecDeque<HourBucket>, now_hour: i64) {
    while buckets
        .front()
        .is_some_and(|b| b.hour <= now_hour - HISTORY_HOURS)
    {
        buckets.pop_front();
    }
}

/// Totals over the last `hours` clock hours, current (partial) hour included.
#[derive(Debug, Clone, Default)]
pub struct Window {
    pub ok: u32,
    pub total: u32,
    pub hist: Vec<u32>,
}

impl Window {
    pub fn over(buckets: &VecDeque<HourBucket>, now_hour: i64, hours: i64) -> Self {
        let mut w = Window {
            hist: vec![0; LATENCY_BINS_MS.len() + 1],
            ..Default::default()
        };
        for b in buckets.iter().filter(|b| b.hour > now_hour - hours) {
            w.ok += b.ok;
            w.total += b.total;
            for (acc, c) in w.hist.iter_mut().zip(&b.latency_hist) {
                *acc += c;
            }
        }
        w
    }

    pub fn reliability(&self) -> Option<f64> {
        (self.total > 0).then(|| self.ok as f64 / self.total as f64)
    }

    /// Upper bin edge (ms) at or below which a fraction `p` of successful
    /// polls finished; `None` without latency samples.
    pub fn percentile(&self, p: f64) -> Option<u64> {
        let n: u64 = self.hist.iter().map(|c| u64::from(*c)).sum();
        if n == 0 {
            return None;
        }
        let rank = ((p * n as f64).ceil() as u64).clamp(1, n);
        let mut seen = 0u64;
        for (i, c) in self.hist.iter().enumerate() {
            seen += u64::from(*c);
            if seen >= rank {
                return Some(LATENCY_BINS_MS[i.min(LATENCY_BINS_MS.len() - 1)]);
            }
        }
        LATENCY_BINS_MS.last().copied()
    }
}

/// Reliability and latency for one endpoint, as served by the API.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct EndpointStats {
    /// Success rate of raw polls over the last 60 minutes.
    pub reliability_1h: Option<f64>,
    pub reliability_24h: Option<f64>,
    pub reliability_7d: Option<f64>,
    /// Response time of successful polls over 24h (bucketed): the `/v1/info`
    /// probe when it has samples, else the catalog fetch (`latency_source`).
    pub latency_p50_ms: Option<u64>,
    pub latency_p95_ms: Option<u64>,
    /// `probe` | `catalog` | null (no latency samples).
    pub latency_source: Option<&'static str>,
    /// Polls in the 24h window (how much the figures above rest on).
    pub polls_24h: u32,
}

impl EndpointStats {
    /// Reliability always comes from catalog polls (that is what keeps quotes
    /// fresh); latency prefers the lighter `/v1/info` probe.
    #[cfg(test)]
    pub fn from_buckets(
        catalog: Option<&VecDeque<HourBucket>>,
        probe: Option<&VecDeque<HourBucket>>,
        reliability_1h: Option<f64>,
        now_hour: i64,
    ) -> Self {
        let empty = VecDeque::new();
        let day = Window::over(catalog.unwrap_or(&empty), now_hour, 24);
        let week = Window::over(catalog.unwrap_or(&empty), now_hour, HISTORY_HOURS);
        let probe_day = Window::over(probe.unwrap_or(&empty), now_hour, 24);
        Self::from_windows(&day, &week, &probe_day, reliability_1h)
    }

    /// Build from precomputed windows, so callers can read each history map
    /// under its own lock. Reliability comes from catalog polls (that is what
    /// keeps quotes fresh); latency prefers the lighter `/v1/info` probe.
    pub fn from_windows(
        day: &Window,
        week: &Window,
        probe_day: &Window,
        reliability_1h: Option<f64>,
    ) -> Self {
        let (lat, source) = if probe_day.percentile(0.5).is_some() {
            (probe_day, Some("probe"))
        } else if day.percentile(0.5).is_some() {
            (day, Some("catalog"))
        } else {
            (day, None)
        };
        Self {
            reliability_1h,
            reliability_24h: day.reliability(),
            reliability_7d: week.reliability(),
            latency_p50_ms: lat.percentile(0.50),
            latency_p95_ms: lat.percentile(0.95),
            latency_source: source,
            polls_24h: day.total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bins_and_percentiles() {
        let mut b = VecDeque::new();
        for ms in [90, 95, 99, 100, 400, 5000] {
            record(&mut b, 10, true, Some(ms));
        }
        record(&mut b, 10, false, None);
        let w = Window::over(&b, 10, 24);
        assert_eq!((w.ok, w.total), (6, 7));
        // 4 of 6 latencies fall in the (80, 100] bin.
        assert_eq!(w.percentile(0.50), Some(100));
        assert_eq!(w.percentile(0.95), Some(5000));
        assert!(Window::default().percentile(0.5).is_none());
    }

    #[test]
    fn windows_and_expiry() {
        let mut b = VecDeque::new();
        record(&mut b, 100, true, Some(50));
        record(&mut b, 100 + 30, false, None); // 30h later
        let now = 100 + 30;
        let stats = EndpointStats::from_buckets(Some(&b), None, None, now);
        assert_eq!(stats.reliability_24h, Some(0.0));
        assert_eq!(stats.reliability_7d, Some(0.5));
        assert_eq!(stats.latency_p50_ms, None);
        // A week on, the first bucket is gone.
        record(&mut b, 100 + HISTORY_HOURS, true, None);
        assert_eq!(b.front().unwrap().hour, 130);
    }

    #[test]
    fn latency_prefers_probe_samples() {
        let mut catalog = VecDeque::new();
        let mut probe = VecDeque::new();
        record(&mut catalog, 5, true, Some(2000));
        let s = EndpointStats::from_buckets(Some(&catalog), Some(&probe), None, 5);
        assert_eq!(
            (s.latency_p50_ms, s.latency_source),
            (Some(2000), Some("catalog"))
        );
        record(&mut probe, 5, true, Some(60));
        let s = EndpointStats::from_buckets(Some(&catalog), Some(&probe), None, 5);
        assert_eq!(
            (s.latency_p50_ms, s.latency_source),
            (Some(63), Some("probe"))
        );
        // Reliability stays catalog-based.
        record(&mut probe, 5, false, None);
        let s = EndpointStats::from_buckets(Some(&catalog), Some(&probe), None, 5);
        assert_eq!(s.reliability_24h, Some(1.0));
    }

    #[test]
    fn hist_roundtrip_and_overflow() {
        let mut b = HourBucket::new(1);
        b.record(true, Some(30));
        b.record(true, Some(60_000));
        let s = b.hist_to_string();
        assert_eq!(HourBucket::hist_from_str(&s), b.latency_hist);
        assert_eq!(
            HourBucket::hist_from_str("").len(),
            LATENCY_BINS_MS.len() + 1
        );
        let w = Window::over(&VecDeque::from([b]), 1, 24);
        assert_eq!(w.percentile(1.0), Some(20000));
    }
}
