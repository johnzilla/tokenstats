//! Market helpers: popular families, blended cost, filter presets.

use crate::store::Quote;

/// Default output:input token ratio for blended cost (1 in : N out → ratio = N).
pub const DEFAULT_BLEND_RATIO: f64 = 3.0;

/// Popular model families for the "Best Now" strip.
pub const POPULAR_FAMILIES: &[PopularFamily] = &[
    PopularFamily {
        label: "Claude Sonnet",
        patterns: &["claude-sonnet", "claude-3.5-sonnet", "claude-3.7-sonnet", "sonnet-4", "sonnet-3.5"],
    },
    PopularFamily {
        label: "Claude Opus",
        patterns: &["claude-opus", "opus-4", "opus-3"],
    },
    PopularFamily {
        label: "Claude Haiku",
        patterns: &["claude-haiku", "haiku"],
    },
    PopularFamily {
        label: "Grok",
        patterns: &["x-ai/grok", "grok-3", "grok-4", "grok-2", "grok-beta"],
    },
    PopularFamily {
        label: "DeepSeek",
        patterns: &["deepseek", "deepseek-chat", "deepseek-r1", "deepseek-v3"],
    },
    PopularFamily {
        label: "GPT-4o",
        patterns: &["gpt-4o", "openai/gpt-4o"],
    },
    PopularFamily {
        label: "GPT-4.1",
        patterns: &["gpt-4.1", "openai/gpt-4.1"],
    },
    PopularFamily {
        label: "o3 / o4",
        patterns: &["openai/o3", "openai/o4", "/o3-", "/o4-"],
    },
    PopularFamily {
        label: "Gemini Flash",
        patterns: &["gemini-2.5-flash", "gemini-2.0-flash", "gemini-flash", "gemini-3"],
    },
    PopularFamily {
        label: "Gemini Pro",
        patterns: &["gemini-2.5-pro", "gemini-2.0-pro", "gemini-pro", "gemini-1.5-pro"],
    },
    PopularFamily {
        label: "Llama 70B+",
        patterns: &["llama-3.3-70b", "llama-3.1-70b", "llama-4", "70b-instruct"],
    },
    PopularFamily {
        label: "Mistral Large",
        patterns: &["mistral-large", "mistral-medium", "mistral-small"],
    },
];

pub struct PopularFamily {
    pub label: &'static str,
    pub patterns: &'static [&'static str],
}

/// Dashboard filter presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    CheapestFrontier,
    Fastest,
    MostPrivate,
    LocalFirst,
}

impl Preset {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "frontier" | "cheapest-frontier" | "cheapest_frontier" => Some(Self::CheapestFrontier),
            "fastest" | "fast" => Some(Self::Fastest),
            "private" | "most-private" | "most_private" => Some(Self::MostPrivate),
            "local" | "local-first" | "local_first" => Some(Self::LocalFirst),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::CheapestFrontier => "frontier",
            Self::Fastest => "fastest",
            Self::MostPrivate => "private",
            Self::LocalFirst => "local",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::CheapestFrontier => "Cheapest Frontier",
            Self::Fastest => "Fastest",
            Self::MostPrivate => "Most Private",
            Self::LocalFirst => "Local-first",
        }
    }

    pub fn all() -> &'static [Preset] {
        &[
            Self::CheapestFrontier,
            Self::Fastest,
            Self::MostPrivate,
            Self::LocalFirst,
        ]
    }
}

/// Weighted blended USD per 1M tokens for an input:output ratio of 1:r.
/// `blend = (in + r * out) / (1 + r)`.
pub fn blended_usd(price_in: Option<f64>, price_out: Option<f64>, ratio: f64) -> Option<f64> {
    let r = if ratio <= 0.0 { DEFAULT_BLEND_RATIO } else { ratio };
    match (price_in, price_out) {
        (Some(i), Some(o)) => Some((i + r * o) / (1.0 + r)),
        (Some(i), None) => Some(i),
        (None, Some(o)) => Some(o),
        (None, None) => None,
    }
}

/// Percent change: positive = more expensive, negative = cheaper.
pub fn delta_pct(current: Option<f64>, previous: Option<f64>) -> Option<f64> {
    match (current, previous) {
        (Some(c), Some(p)) if p > 0.0 && c >= 0.0 => Some(((c - p) / p) * 100.0),
        _ => None,
    }
}

pub fn matches_family(model: &str, family: &PopularFamily) -> bool {
    let m = model.to_ascii_lowercase();
    family.patterns.iter().any(|p| m.contains(&p.to_ascii_lowercase()))
}

/// Best (cheapest non-zero blended) quote per popular family.
/// Prefers full/flagship variants over mini/nano/lite when available.
pub fn best_now<'a>(quotes: &'a [Quote], ratio: f64) -> Vec<BestNowRow<'a>> {
    let mut out = Vec::new();
    for family in POPULAR_FAMILIES {
        let candidates: Vec<_> = quotes
            .iter()
            .filter(|q| matches_family(&q.model, family))
            .filter_map(|q| {
                let b = blended_usd(q.price_in_usd, q.price_out_usd, ratio)?;
                if b <= 0.0 {
                    return None;
                }
                Some((q, b))
            })
            .collect();

        let full: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|(q, _)| !is_derivative_model(&q.model))
            .collect();
        let pool = if full.is_empty() { &candidates } else { &full };

        if let Some((q, blend)) = pool
            .iter()
            .copied()
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        {
            out.push(BestNowRow {
                family: family.label,
                quote: q,
                blended_usd: blend,
            });
        }
    }
    out
}

fn is_derivative_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    ["-mini", "mini-", "-nano", "nano-", "-lite", "lite-", "-tiny", "tiny-"]
        .iter()
        .any(|k| m.contains(k))
}

pub struct BestNowRow<'a> {
    pub family: &'static str,
    pub quote: &'a Quote,
    pub blended_usd: f64,
}

pub fn apply_preset(quotes: Vec<Quote>, preset: Preset) -> Vec<Quote> {
    match preset {
        Preset::CheapestFrontier => {
            let mut v: Vec<_> = quotes
                .into_iter()
                .filter(|q| POPULAR_FAMILIES.iter().any(|f| matches_family(&q.model, f)))
                .filter(|q| q.price_out_usd.map(|p| p > 0.0).unwrap_or(false))
                .collect();
            v.sort_by(|a, b| {
                let ba = blended_usd(a.price_in_usd, a.price_out_usd, DEFAULT_BLEND_RATIO)
                    .unwrap_or(f64::MAX);
                let bb = blended_usd(b.price_in_usd, b.price_out_usd, DEFAULT_BLEND_RATIO)
                    .unwrap_or(f64::MAX);
                ba.partial_cmp(&bb).unwrap_or(std::cmp::Ordering::Equal)
            });
            v
        }
        Preset::Fastest => {
            let mut v: Vec<_> = quotes
                .into_iter()
                .filter(|q| is_fast_model(&q.model))
                .collect();
            // Prefer cheaper flash-class models first among "fast" names.
            v.sort_by(|a, b| {
                let ba = blended_usd(a.price_in_usd, a.price_out_usd, DEFAULT_BLEND_RATIO)
                    .unwrap_or(f64::MAX);
                let bb = blended_usd(b.price_in_usd, b.price_out_usd, DEFAULT_BLEND_RATIO)
                    .unwrap_or(f64::MAX);
                ba.partial_cmp(&bb).unwrap_or(std::cmp::Ordering::Equal)
            });
            v
        }
        Preset::MostPrivate => {
            let mut v: Vec<_> = quotes
                .into_iter()
                .filter(|q| {
                    q.source == "routstr"
                        || q.source == "nostr"
                        || q.endpoint
                            .as_deref()
                            .map(|e| e.contains(".onion") || e.contains("localhost") || e.contains("127.0.0.1"))
                            .unwrap_or(false)
                })
                .collect();
            v.sort_by(|a, b| {
                // onion first, then routstr, then blended price
                let sa = privacy_rank(a);
                let sb = privacy_rank(b);
                sa.cmp(&sb).then_with(|| {
                    let ba = blended_usd(a.price_in_usd, a.price_out_usd, DEFAULT_BLEND_RATIO)
                        .unwrap_or(f64::MAX);
                    let bb = blended_usd(b.price_in_usd, b.price_out_usd, DEFAULT_BLEND_RATIO)
                        .unwrap_or(f64::MAX);
                    ba.partial_cmp(&bb).unwrap_or(std::cmp::Ordering::Equal)
                })
            });
            v
        }
        Preset::LocalFirst => {
            let mut v: Vec<_> = quotes.into_iter().filter(|q| is_local_candidate(q)).collect();
            v.sort_by(|a, b| a.model.cmp(&b.model));
            v
        }
    }
}

fn is_fast_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    ["flash", "mini", "turbo", "haiku", "nano", "lite", "fast", "small", "instant"]
        .iter()
        .any(|k| m.contains(k))
}

fn is_local_candidate(q: &Quote) -> bool {
    let m = q.model.to_ascii_lowercase();
    let p = q.provider.to_ascii_lowercase();
    let e = q.endpoint.as_deref().unwrap_or("").to_ascii_lowercase();
    let hay = format!("{m} {p} {e}");
    [
        "ollama",
        "lmstudio",
        "lm-studio",
        "vllm",
        "localhost",
        "127.0.0.1",
        "local-",
        "/local",
        "kobold",
        "text-generation-webui",
        "llama.cpp",
        "llamacpp",
    ]
    .iter()
    .any(|k| hay.contains(k))
        || q.price_in_usd == Some(0.0) && q.price_out_usd == Some(0.0) && q.source != "openrouter"
}

fn privacy_rank(q: &Quote) -> u8 {
    let onion = q
        .endpoint
        .as_deref()
        .map(|e| e.contains(".onion"))
        .unwrap_or(false);
    if onion {
        0
    } else if q.source == "routstr" || q.source == "nostr" {
        1
    } else {
        2
    }
}
