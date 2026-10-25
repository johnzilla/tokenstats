//! Runtime configuration (CLI + env). Defaults keep the current MVP behavior.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

/// Default HTTP bind address for the dashboard.
pub const DEFAULT_BIND: &str = "127.0.0.1:8080";

/// Default Nostr relays for Routstr discovery.
pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.nostr.band",
];

pub const DEFAULT_DB: &str = "data/tokenstats.db";
pub const DEFAULT_OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/models";
pub const DEFAULT_ORCAROUTER_URL: &str = "https://api.orcarouter.ai/v1/models";
pub const DEFAULT_BTC_USD_URL: &str = "https://api.coinbase.com/v2/prices/BTC-USD/spot";

#[derive(Debug, Parser)]
#[command(
    name = "tokenstats",
    version,
    about = "Sovereign inference market observability + oracle",
    long_about = "Live observability layer and price oracle for decentralized inference \
                  markets (Routstr + providers). Listens on Nostr, polls provider catalogs, \
                  normalizes quotes to USD/sats, persists to SQLite, and serves a dashboard."
)]
pub struct Cli {
    /// Increase log verbosity (-v, -vv). Overridden by RUST_LOG when set.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Emit JSON logs (better for containers / log aggregators).
    #[arg(
        long,
        global = true,
        env = "TOKENSTATS_LOG_JSON",
        default_value_t = false
    )]
    pub log_json: bool,

    /// Include module targets in log lines.
    #[arg(
        long,
        global = true,
        env = "TOKENSTATS_LOG_TARGETS",
        default_value_t = true
    )]
    pub log_targets: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run the full observability stack (Nostr + pollers + dashboard).
    Serve {
        /// HTTP bind address for the dashboard.
        #[arg(long, default_value = DEFAULT_BIND, env = "TOKENSTATS_BIND")]
        bind: SocketAddr,

        /// Nostr relays (comma-separated or repeatable).
        #[arg(
            long,
            value_delimiter = ',',
            env = "TOKENSTATS_RELAYS",
            default_values_t = default_relays()
        )]
        relay: Vec<String>,

        /// Disable Nostr listener.
        #[arg(long, default_value_t = false, env = "TOKENSTATS_NO_NOSTR")]
        no_nostr: bool,

        /// Disable all HTTP provider polling (OpenRouter + nodes).
        #[arg(long, default_value_t = false, env = "TOKENSTATS_NO_POLL")]
        no_poll: bool,

        /// Disable OpenRouter catalog (still poll discovered Routstr nodes if poll enabled).
        #[arg(long, default_value_t = false, env = "TOKENSTATS_NO_OPENROUTER")]
        no_openrouter: bool,

        /// Disable OrcaRouter catalog.
        #[arg(long, default_value_t = false, env = "TOKENSTATS_NO_ORCAROUTER")]
        no_orcarouter: bool,

        /// Disable BTC/USD oracle loop (sats fields stay empty unless restored).
        #[arg(long, default_value_t = false, env = "TOKENSTATS_NO_ORACLE")]
        no_oracle: bool,

        /// Seed Routstr-compatible node endpoints to poll (repeatable).
        #[arg(long, env = "TOKENSTATS_NODES", value_delimiter = ',')]
        node: Vec<String>,

        /// Also poll Nostr-announced nodes on loopback/private/link-local addresses.
        /// Only for trusted LANs: announcements are unauthenticated (SSRF risk).
        #[arg(long, default_value_t = false, env = "TOKENSTATS_ALLOW_PRIVATE_NODES")]
        allow_private_nodes: bool,

        /// SQLite database path (created if missing).
        #[arg(long, default_value = DEFAULT_DB, env = "TOKENSTATS_DB")]
        db: PathBuf,

        /// Skip loading/saving SQLite (pure in-memory).
        #[arg(long, default_value_t = false, env = "TOKENSTATS_NO_PERSIST")]
        no_persist: bool,

        /// Seconds between full provider poll cycles.
        #[arg(long, default_value_t = 60, env = "TOKENSTATS_POLL_INTERVAL_SECS")]
        poll_interval_secs: u64,

        /// Seconds between BTC/USD rate refreshes.
        #[arg(long, default_value_t = 60, env = "TOKENSTATS_BTC_INTERVAL_SECS")]
        btc_interval_secs: u64,

        /// Seconds between sats normalization ticks.
        #[arg(long, default_value_t = 15, env = "TOKENSTATS_NORMALIZE_INTERVAL_SECS")]
        normalize_interval_secs: u64,

        /// Seconds between SQLite snapshots.
        #[arg(long, default_value_t = 30, env = "TOKENSTATS_PERSIST_INTERVAL_SECS")]
        persist_interval_secs: u64,

        /// HTTP client timeout (seconds) for catalog/oracle requests.
        #[arg(long, default_value_t = 20, env = "TOKENSTATS_HTTP_TIMEOUT_SECS")]
        http_timeout_secs: u64,

        /// Max Routstr nodes polled concurrently per cycle.
        #[arg(long, default_value_t = 16, env = "TOKENSTATS_NODE_CONCURRENCY")]
        node_concurrency: usize,

        /// Per-node poll budget (seconds) across all `/models` fallbacks.
        #[arg(long, default_value_t = 10, env = "TOKENSTATS_NODE_TIMEOUT_SECS")]
        node_timeout_secs: u64,

        /// Max Nostr-discovered nodes kept (0 = unlimited). `--node` seeds don't count.
        #[arg(long, default_value_t = 512, env = "TOKENSTATS_MAX_NODES")]
        max_nodes: usize,

        /// Drop discovered nodes not announced or successfully polled within
        /// this many seconds (0 = never).
        #[arg(long, default_value_t = 604_800, env = "TOKENSTATS_NODE_TTL_SECS")]
        node_ttl_secs: u64,

        /// Drop quotes not re-observed within this many seconds (0 = never).
        #[arg(long, default_value_t = 86_400, env = "TOKENSTATS_QUOTE_TTL_SECS")]
        quote_ttl_secs: u64,

        /// Quotes older than this are flagged `stale` in the API (and feed status).
        #[arg(long, default_value_t = 600, env = "TOKENSTATS_STALE_AFTER_SECS")]
        stale_after_secs: u64,

        /// USD per 1M tokens above which a price is treated as a unit error and
        /// nulled at ingest (0 = no ceiling).
        #[arg(
            long,
            default_value_t = 1000.0,
            env = "TOKENSTATS_MAX_PRICE_USD_PER_1M"
        )]
        max_price_usd_per_1m: f64,

        /// Plausible analytics script URL (https). Unset = no third-party script
        /// on the dashboard and a CSP that allows only this origin's assets.
        #[arg(long, env = "TOKENSTATS_PLAUSIBLE_SCRIPT")]
        plausible_script: Option<String>,

        /// OpenRouter models catalog URL.
        #[arg(long, default_value = DEFAULT_OPENROUTER_URL, env = "TOKENSTATS_OPENROUTER_URL")]
        openrouter_url: String,

        /// OrcaRouter models catalog URL.
        #[arg(long, default_value = DEFAULT_ORCAROUTER_URL, env = "TOKENSTATS_ORCAROUTER_URL")]
        orcarouter_url: String,

        /// BTC/USD spot price URL (JSON with data.amount).
        #[arg(long, default_value = DEFAULT_BTC_USD_URL, env = "TOKENSTATS_BTC_USD_URL")]
        btc_usd_url: String,
    },
}

fn default_relays() -> Vec<String> {
    DEFAULT_RELAYS.iter().map(|s| (*s).to_string()).collect()
}

/// Fully resolved runtime configuration for `serve`.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub relays: Vec<String>,
    pub seed_nodes: Vec<String>,
    pub allow_private_nodes: bool,
    pub db_path: PathBuf,
    pub persist: bool,
    pub enable_nostr: bool,
    pub enable_poll: bool,
    pub enable_openrouter: bool,
    pub enable_orcarouter: bool,
    pub enable_oracle: bool,
    pub poll_interval: Duration,
    pub btc_rate_interval: Duration,
    pub normalize_interval: Duration,
    pub persist_interval: Duration,
    pub http_timeout: Duration,
    pub node_concurrency: usize,
    pub node_timeout: Duration,
    pub quote_ttl: Option<Duration>,
    pub max_nodes: usize,
    pub node_ttl: Option<Duration>,
    pub stale_after: Duration,
    pub max_price_usd_per_1m: Option<f64>,
    pub openrouter_url: String,
    pub orcarouter_url: String,
    pub plausible_script: Option<String>,
    pub btc_usd_url: String,
}

impl Config {
    pub fn from_cli(cli: &Cli) -> Self {
        let Commands::Serve {
            bind,
            relay,
            no_nostr,
            no_poll,
            no_openrouter,
            no_orcarouter,
            no_oracle,
            node,
            allow_private_nodes,
            db,
            no_persist,
            poll_interval_secs,
            btc_interval_secs,
            normalize_interval_secs,
            persist_interval_secs,
            http_timeout_secs,
            node_concurrency,
            node_timeout_secs,
            quote_ttl_secs,
            max_nodes,
            node_ttl_secs,
            stale_after_secs,
            max_price_usd_per_1m,
            openrouter_url,
            orcarouter_url,
            plausible_script,
            btc_usd_url,
        } = &cli.command;

        Self {
            bind: *bind,
            relays: relay.clone(),
            seed_nodes: node.clone(),
            allow_private_nodes: *allow_private_nodes,
            db_path: db.clone(),
            persist: !no_persist,
            enable_nostr: !no_nostr,
            enable_poll: !no_poll,
            enable_openrouter: !no_openrouter,
            enable_orcarouter: !no_orcarouter,
            enable_oracle: !no_oracle,
            poll_interval: Duration::from_secs((*poll_interval_secs).max(5)),
            btc_rate_interval: Duration::from_secs((*btc_interval_secs).max(5)),
            normalize_interval: Duration::from_secs((*normalize_interval_secs).max(5)),
            persist_interval: Duration::from_secs((*persist_interval_secs).max(5)),
            http_timeout: Duration::from_secs((*http_timeout_secs).max(1)),
            node_concurrency: (*node_concurrency).max(1),
            node_timeout: Duration::from_secs((*node_timeout_secs).max(1)),
            quote_ttl: (*quote_ttl_secs > 0).then(|| Duration::from_secs(*quote_ttl_secs)),
            max_nodes: *max_nodes,
            node_ttl: (*node_ttl_secs > 0).then(|| Duration::from_secs(*node_ttl_secs)),
            stale_after: Duration::from_secs((*stale_after_secs).max(1)),
            max_price_usd_per_1m: Some(*max_price_usd_per_1m).filter(|m| *m > 0.0),
            openrouter_url: openrouter_url.clone(),
            orcarouter_url: orcarouter_url.clone(),
            plausible_script: plausible_script.clone().filter(|s| !s.trim().is_empty()),
            btc_usd_url: btc_usd_url.clone(),
        }
    }

    /// Log a one-line summary of effective settings (no secrets).
    pub fn log_summary(&self) {
        tracing::info!(
            bind = %self.bind,
            persist = self.persist,
            db = %self.db_path.display(),
            enable_nostr = self.enable_nostr,
            enable_poll = self.enable_poll,
            enable_openrouter = self.enable_openrouter,
            enable_orcarouter = self.enable_orcarouter,
            analytics = self.plausible_script.is_some(),
            enable_oracle = self.enable_oracle,
            relays = self.relays.len(),
            seed_nodes = self.seed_nodes.len(),
            allow_private_nodes = self.allow_private_nodes,
            poll_interval_secs = self.poll_interval.as_secs(),
            btc_interval_secs = self.btc_rate_interval.as_secs(),
            persist_interval_secs = self.persist_interval.as_secs(),
            http_timeout_secs = self.http_timeout.as_secs(),
            node_concurrency = self.node_concurrency,
            node_timeout_secs = self.node_timeout.as_secs(),
            quote_ttl_secs = self.quote_ttl.map(|d| d.as_secs()).unwrap_or(0),
            max_nodes = self.max_nodes,
            node_ttl_secs = self.node_ttl.map(|d| d.as_secs()).unwrap_or(0),
            stale_after_secs = self.stale_after.as_secs(),
            max_price_usd_per_1m = self.max_price_usd_per_1m.unwrap_or(0.0),
            "configuration"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn default_serve_parses() {
        let cli = Cli::try_parse_from(["tokenstats", "serve"]).expect("parse");
        let cfg = Config::from_cli(&cli);
        assert_eq!(cfg.bind.to_string(), "127.0.0.1:8080");
        assert!(cfg.persist);
        assert!(cfg.enable_nostr);
        assert!(cfg.enable_poll);
        assert!(cfg.enable_openrouter);
        assert!(cfg.enable_orcarouter);
        assert_eq!(cfg.plausible_script, None);
        assert_eq!(cfg.poll_interval.as_secs(), 60);
        assert_eq!(cfg.relays.len(), 3);
        assert_eq!(cfg.node_concurrency, 16);
        assert_eq!(cfg.node_timeout.as_secs(), 10);
        assert_eq!(cfg.quote_ttl, Some(Duration::from_secs(86_400)));
        assert_eq!(cfg.max_nodes, 512);
        assert_eq!(cfg.node_ttl, Some(Duration::from_secs(604_800)));
        assert_eq!(cfg.stale_after.as_secs(), 600);
        assert_eq!(cfg.max_price_usd_per_1m, Some(1000.0));
        assert!(!cfg.allow_private_nodes);
    }

    #[test]
    fn allow_private_nodes_flag() {
        let cli =
            Cli::try_parse_from(["tokenstats", "serve", "--allow-private-nodes"]).expect("parse");
        assert!(Config::from_cli(&cli).allow_private_nodes);
    }

    #[test]
    fn zero_quote_ttl_disables_expiry() {
        let cli =
            Cli::try_parse_from(["tokenstats", "serve", "--quote-ttl-secs", "0"]).expect("parse");
        assert_eq!(Config::from_cli(&cli).quote_ttl, None);
    }

    #[test]
    fn flags_disable_sources() {
        let cli = Cli::try_parse_from([
            "tokenstats",
            "serve",
            "--no-nostr",
            "--no-openrouter",
            "--no-orcarouter",
            "--no-persist",
            "--poll-interval-secs",
            "10",
        ])
        .expect("parse");
        let cfg = Config::from_cli(&cli);
        assert!(!cfg.enable_nostr);
        assert!(!cfg.enable_openrouter);
        assert!(!cfg.enable_orcarouter);
        assert!(!cfg.persist);
        assert_eq!(cfg.poll_interval.as_secs(), 10);
    }
}
