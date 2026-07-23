//! tokenstats — sovereign inference market observability + oracle.

mod config;
mod market;
mod nostr;
mod oracle;
mod persist;
mod providers;
mod store;
mod web;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn, Level};
use tracing_subscriber::EnvFilter;

use config::{Cli, Config};
use store::{normalize_endpoint, ProviderNode, Store};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose, cli.log_json, cli.log_targets);

    let cfg = Config::from_cli(&cli);
    if let Err(e) = run_serve(cfg).await {
        error!(error = %e, "tokenstats exited with error");
        return Err(e);
    }
    info!("tokenstats stopped cleanly");
    Ok(())
}

fn init_tracing(verbose: u8, json: bool, show_targets: bool) {
    let level = match verbose {
        0 => Level::INFO,
        1 => Level::DEBUG,
        _ => Level::TRACE,
    };

    // Prefer RUST_LOG; otherwise derive from -v and quiet noisy deps at info.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let base = match verbose {
            0 => format!(
                "info,tokenstats={level},tower_http=info,hyper=warn,reqwest=warn,nostr_sdk=warn,nostr_relay_pool=warn",
                level = level.as_str().to_ascii_lowercase()
            ),
            1 => format!(
                "debug,tokenstats=debug,tower_http=info,hyper=warn,reqwest=warn,nostr_sdk=info"
            ),
            _ => "trace".into(),
        };
        EnvFilter::new(base)
    });

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(show_targets)
        .with_thread_ids(false)
        .with_file(false);

    if json {
        builder.json().flatten_event(true).init();
    } else {
        builder.compact().init();
    }
}

async fn run_serve(cfg: Config) -> Result<()> {
    let cancel = CancellationToken::new();
    install_signal_handlers(cancel.clone())?;

    let store = Arc::new(Store::new());
    info!(version = env!("CARGO_PKG_VERSION"), "tokenstats starting");
    cfg.log_summary();

    // --- Persistence ---
    let db = if cfg.persist {
        match persist::Db::open(&cfg.db_path) {
            Ok(db) => {
                let db = Arc::new(db);
                if let Err(e) = db.load_into(&store) {
                    warn!(error = %e, "sqlite restore failed (starting empty)");
                }
                Some(db)
            }
            Err(e) => {
                return Err(e).context(format!(
                    "failed to open sqlite at {}",
                    cfg.db_path.display()
                ));
            }
        }
    } else {
        info!("persistence disabled");
        None
    };

    // Seed nodes from config.
    for (i, endpoint) in cfg.seed_nodes.iter().enumerate() {
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
        info!(%endpoint, "seeded node from config");
    }

    // --- Background tasks (all observe cancel) ---
    if cfg.enable_nostr {
        let task_cancel = cancel.child_token();
        let nostr_store = Arc::clone(&store);
        let relays = cfg.relays.clone();
        let kinds = vec![nostr::ROUTSTR_PROVIDER_KIND, nostr::ROUTSTR_LEGACY_KIND];
        tokio::spawn(async move {
            match nostr::run_listener(nostr_store, relays, kinds, task_cancel).await {
                Ok(()) => info!("nostr listener stopped"),
                Err(e) => error!(error = %e, "nostr listener failed"),
            }
        });
        info!("nostr listener task spawned");
    } else {
        info!("nostr listener disabled");
    }

    if cfg.enable_poll {
        let task_cancel = cancel.child_token();
        let poll_store = Arc::clone(&store);
        let poll_cfg = providers::PollerConfig {
            interval: cfg.poll_interval,
            http_timeout: cfg.http_timeout,
            enable_openrouter: cfg.enable_openrouter,
            openrouter_url: cfg.openrouter_url.clone(),
        };
        tokio::spawn(async move {
            match providers::run_poller(poll_store, poll_cfg, task_cancel).await {
                Ok(()) => info!("provider poller stopped"),
                Err(e) => error!(error = %e, "provider poller failed"),
            }
        });
        info!("provider poller task spawned");
    } else {
        info!("provider poller disabled");
    }

    if cfg.enable_oracle {
        let task_cancel = cancel.child_token();
        let oracle_store = Arc::clone(&store);
        let oracle_cfg = oracle::OracleConfig {
            rate_interval: cfg.btc_rate_interval,
            normalize_interval: cfg.normalize_interval,
            http_timeout: cfg.http_timeout,
            btc_usd_url: cfg.btc_usd_url.clone(),
        };
        tokio::spawn(async move {
            match oracle::run_oracle(oracle_store, oracle_cfg, task_cancel).await {
                Ok(()) => info!("oracle stopped"),
                Err(e) => error!(error = %e, "oracle failed"),
            }
        });
        info!("oracle task spawned");
    } else {
        info!("oracle disabled");
    }

    if let Some(ref db) = db {
        let task_cancel = cancel.child_token();
        let persist_store = Arc::clone(&store);
        let persist_db = Arc::clone(db);
        let interval = cfg.persist_interval;
        tokio::spawn(async move {
            persist::run_persist_loop(persist_store, persist_db, interval, task_cancel).await;
            info!("persist loop stopped");
        });
        info!("persist loop spawned");
    }

    // --- HTTP server with graceful shutdown ---
    let app_state = web::AppState {
        store: Arc::clone(&store),
    };
    let app = web::router(app_state);
    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("failed to bind {}", cfg.bind))?;

    info!(bind = %cfg.bind, "dashboard listening");

    let shutdown = cancel.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown.cancelled().await;
        info!("http graceful shutdown started");
    });

    // Drive server until it finishes (cancel or fatal error).
    if let Err(e) = server.await {
        error!(error = %e, "http server error");
        cancel.cancel();
        // still attempt final persist below
    } else {
        // Server returned cleanly after shutdown signal.
        cancel.cancel();
    }

    // Final snapshot so we don't lose recent quotes on SIGTERM.
    if let Some(db) = db {
        info!("writing final sqlite snapshot");
        let store = Arc::clone(&store);
        match tokio::task::spawn_blocking(move || db.save_from(&store)).await {
            Ok(Ok(())) => info!("final snapshot saved"),
            Ok(Err(e)) => warn!(error = %e, "final snapshot failed"),
            Err(e) => warn!(error = %e, "final snapshot join failed"),
        }
    }

    // Brief pause so child tasks observe cancel and exit cleanly.
    tokio::time::sleep(Duration::from_millis(150)).await;
    Ok(())
}

fn install_signal_handlers(cancel: CancellationToken) -> Result<()> {
    // Ctrl-C
    let c = cancel.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!(signal = "SIGINT", "shutdown requested");
                c.cancel();
            }
            Err(e) => warn!(error = %e, "failed to listen for ctrl_c"),
        }
    });

    // SIGTERM (containers / systemd)
    #[cfg(unix)]
    {
        let c = cancel.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::terminate()) {
                Ok(mut sig) => {
                    sig.recv().await;
                    info!(signal = "SIGTERM", "shutdown requested");
                    c.cancel();
                }
                Err(e) => warn!(error = %e, "failed to listen for SIGTERM"),
            }
        });
    }

    let _ = cancel; // silence unused on non-unix if only ctrl_c used — both use clone
    Ok(())
}
