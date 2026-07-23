//! tokenstats — sovereign inference market observability + oracle.
//!
//! - CLI entry (`serve`)
//! - Nostr listener for Routstr discovery (RIP-02 kind 38421)
//! - Provider polling (OpenRouter + node `/v1/models`)
//! - In-memory store + BTC/USD dual-unit normalization
//! - SQLite persistence
//! - Axum HTML dashboard (Best Now, blend, presets, deltas, reliability)

mod market;
mod nostr;
mod oracle;
mod persist;
mod providers;
mod store;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{info, Level};
use tracing_subscriber::EnvFilter;

use chrono::Utc;
use store::{normalize_endpoint, ProviderNode, Store};

/// Default HTTP bind address for the dashboard.
const DEFAULT_BIND: &str = "127.0.0.1:8080";

/// Default Nostr relays for Routstr discovery.
const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.nostr.band",
];

const DEFAULT_DB: &str = "data/tokenstats.db";

#[derive(Debug, Parser)]
#[command(
    name = "tokenstats",
    version,
    about = "Sovereign inference market observability + oracle",
    long_about = "Live observability layer and price oracle for decentralized inference \
                  markets (Routstr + providers). Listens on Nostr, polls provider catalogs, \
                  normalizes quotes to USD/sats, persists to SQLite, and serves a dashboard."
)]
struct Cli {
    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
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

        /// Disable Nostr listener (dashboard + provider poll only).
        #[arg(long, default_value_t = false)]
        no_nostr: bool,

        /// Disable provider HTTP polling.
        #[arg(long, default_value_t = false)]
        no_poll: bool,

        /// Seed Routstr-compatible node endpoints to poll (repeatable).
        #[arg(long, env = "TOKENSTATS_NODES", value_delimiter = ',')]
        node: Vec<String>,

        /// SQLite database path (created if missing). Use empty string to disable.
        #[arg(long, default_value = DEFAULT_DB, env = "TOKENSTATS_DB")]
        db: PathBuf,

        /// Skip loading/saving SQLite (pure in-memory).
        #[arg(long, default_value_t = false)]
        no_persist: bool,
    },
}

fn default_relays() -> Vec<String> {
    DEFAULT_RELAYS.iter().map(|s| (*s).to_string()).collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Commands::Serve {
            bind,
            relay,
            no_nostr,
            no_poll,
            node,
            db,
            no_persist,
        } => run_serve(bind, relay, no_nostr, no_poll, node, db, no_persist).await,
    }
}

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => Level::INFO,
        1 => Level::DEBUG,
        _ => Level::TRACE,
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level.to_string()));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

async fn run_serve(
    bind: SocketAddr,
    relays: Vec<String>,
    no_nostr: bool,
    no_poll: bool,
    seed_nodes: Vec<String>,
    db_path: PathBuf,
    no_persist: bool,
) -> Result<()> {
    let store = Arc::new(Store::new());

    info!(%bind, "tokenstats starting");
    info!(
        relays = ?relays,
        kind = nostr::ROUTSTR_PROVIDER_KIND,
        "Routstr discovery config (RIP-02)"
    );

    // --- Persistence ---
    let db = if no_persist {
        info!("persistence disabled (--no-persist)");
        None
    } else {
        let db = Arc::new(persist::Db::open(&db_path)?);
        if let Err(e) = db.load_into(&store) {
            tracing::warn!(error = %e, "sqlite restore failed (starting empty)");
        }
        Some(db)
    };

    // Manually seeded nodes.
    for (i, endpoint) in seed_nodes.iter().enumerate() {
        let endpoint = normalize_endpoint(endpoint);
        store.upsert_node(ProviderNode {
            provider_id: format!("seed-{i}"),
            name: endpoint.clone(),
            endpoint: endpoint.clone(),
            onion: None,
            mint: None,
            version: None,
            region: None,
            pubkey: None,
            discovered_via: "cli".into(),
            last_seen: Utc::now(),
            reliability: 0.0,
            poll_ok: 0,
            poll_total: 0,
            last_latency_ms: None,
        });
        info!(%endpoint, "seeded node from CLI");
    }

    let app_state = web::AppState {
        store: Arc::clone(&store),
    };

    if !no_nostr {
        let nostr_store = Arc::clone(&store);
        let nostr_relays = relays.clone();
        let kinds = vec![nostr::ROUTSTR_PROVIDER_KIND, nostr::ROUTSTR_LEGACY_KIND];
        tokio::spawn(async move {
            if let Err(e) = nostr::run_listener(nostr_store, nostr_relays, kinds).await {
                tracing::error!(error = %e, "Nostr listener exited");
            }
        });
        info!("Nostr listener task spawned");
    } else {
        info!("Nostr listener disabled (--no-nostr)");
    }

    if !no_poll {
        let poll_store = Arc::clone(&store);
        tokio::spawn(async move {
            if let Err(e) = providers::run_poller(poll_store).await {
                tracing::error!(error = %e, "Provider poller exited");
            }
        });
        info!("Provider poller task spawned");
    } else {
        info!("Provider poller disabled (--no-poll)");
    }

    {
        let oracle_store = Arc::clone(&store);
        tokio::spawn(async move {
            if let Err(e) = oracle::run_oracle(oracle_store).await {
                tracing::error!(error = %e, "Oracle task exited");
            }
        });
    }

    if let Some(db) = db {
        let persist_store = Arc::clone(&store);
        tokio::spawn(async move {
            persist::run_persist_loop(persist_store, db).await;
        });
        info!("persist loop spawned");
    }

    let app = web::router(app_state);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;

    info!(%bind, "dashboard listening — open http://{bind}/");
    axum::serve(listener, app)
        .await
        .context("HTTP server error")?;

    Ok(())
}
