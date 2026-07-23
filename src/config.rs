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
    #[arg(long, global = true, env = "TOKENSTATS_LOG_JSON", default_value_t = false)]
    pub log_json: bool,

    /// Include module targets in log lines.
    #[arg(long, global = true, env = "TOKENSTATS_LOG_TARGETS", default_value_t = true)]
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

        /// Disable BTC/USD oracle loop (sats fields stay empty unless restored).
        #[arg(long, default_value_t = false, env = "TOKENSTATS_NO_ORACLE")]
        no_oracle: bool,

        /// Seed Routstr-compatible node endpoints to poll (repeatable).
        #[arg(long, env = "TOKENSTATS_NODES", value_delimiter = ',')]
        node: Vec<String>,

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

        /// OpenRouter models catalog URL.
        #[arg(long, default_value = DEFAULT_OPENROUTER_URL, env = "TOKENSTATS_OPENROUTER_URL")]
        openrouter_url: String,

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
    pub db_path: PathBuf,
    pub persist: bool,
    pub enable_nostr: bool,
    pub enable_poll: bool,
    pub enable_openrouter: bool,
    pub enable_oracle: bool,
    pub poll_interval: Duration,
    pub btc_rate_interval: Duration,
    pub normalize_interval: Duration,
    pub persist_interval: Duration,
    pub http_timeout: Duration,
    pub openrouter_url: String,
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
            no_oracle,
            node,
            db,
            no_persist,
            poll_interval_secs,
            btc_interval_secs,
            normalize_interval_secs,
            persist_interval_secs,
            http_timeout_secs,
            openrouter_url,
            btc_usd_url,
        } = &cli.command;

        Self {
            bind: *bind,
            relays: relay.clone(),
            seed_nodes: node.clone(),
            db_path: db.clone(),
            persist: !no_persist,
            enable_nostr: !no_nostr,
            enable_poll: !no_poll,
            enable_openrouter: !no_openrouter,
            enable_oracle: !no_oracle,
            poll_interval: Duration::from_secs((*poll_interval_secs).max(5)),
            btc_rate_interval: Duration::from_secs((*btc_interval_secs).max(5)),
            normalize_interval: Duration::from_secs((*normalize_interval_secs).max(5)),
            persist_interval: Duration::from_secs((*persist_interval_secs).max(5)),
            http_timeout: Duration::from_secs((*http_timeout_secs).max(1)),
            openrouter_url: openrouter_url.clone(),
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
            enable_oracle = self.enable_oracle,
            relays = self.relays.len(),
            seed_nodes = self.seed_nodes.len(),
            poll_interval_secs = self.poll_interval.as_secs(),
            btc_interval_secs = self.btc_rate_interval.as_secs(),
            persist_interval_secs = self.persist_interval.as_secs(),
            http_timeout_secs = self.http_timeout.as_secs(),
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
        assert_eq!(cfg.poll_interval.as_secs(), 60);
        assert_eq!(cfg.relays.len(), 3);
    }

    #[test]
    fn flags_disable_sources() {
        let cli = Cli::try_parse_from([
            "tokenstats",
            "serve",
            "--no-nostr",
            "--no-openrouter",
            "--no-persist",
            "--poll-interval-secs",
            "10",
        ])
        .expect("parse");
        let cfg = Config::from_cli(&cli);
        assert!(!cfg.enable_nostr);
        assert!(!cfg.enable_openrouter);
        assert!(!cfg.persist);
        assert_eq!(cfg.poll_interval.as_secs(), 10);
    }
}
