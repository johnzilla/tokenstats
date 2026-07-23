//! SQLite persistence for quotes, nodes, and poll samples.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection};
use tracing::{debug, info, warn};

use crate::store::{ProviderNode, Quote, Store};

const SAVE_INTERVAL: Duration = Duration::from_secs(30);

pub struct Db {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create db dir {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("open sqlite {}", path.display()))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            CREATE TABLE IF NOT EXISTS quotes (
                key TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                provider TEXT NOT NULL,
                provider_id TEXT NOT NULL,
                model TEXT NOT NULL,
                price_in_usd REAL,
                price_out_usd REAL,
                price_in_sats REAL,
                price_out_sats REAL,
                prev_price_out_usd REAL,
                prev_price_in_usd REAL,
                endpoint TEXT,
                region TEXT,
                context_length INTEGER,
                observed_at TEXT NOT NULL,
                raw_kind INTEGER
            );
            CREATE TABLE IF NOT EXISTS nodes (
                endpoint TEXT PRIMARY KEY,
                provider_id TEXT NOT NULL,
                name TEXT NOT NULL,
                onion TEXT,
                mint TEXT,
                version TEXT,
                region TEXT,
                pubkey TEXT,
                discovered_via TEXT NOT NULL,
                last_seen TEXT NOT NULL,
                reliability REAL NOT NULL DEFAULT 0,
                poll_ok INTEGER NOT NULL DEFAULT 0,
                poll_total INTEGER NOT NULL DEFAULT 0,
                last_latency_ms INTEGER
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            ",
        )?;
        info!(path = %path.display(), "sqlite opened");
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load_into(&self, store: &Store) -> Result<()> {
        let conn = self.conn.lock().expect("db lock");

        // BTC rate
        if let Ok(rate) = conn.query_row(
            "SELECT value FROM meta WHERE key = 'btc_usd'",
            [],
            |row| row.get::<_, String>(0),
        ) {
            if let Ok(r) = rate.parse::<f64>() {
                store.set_btc_usd(r);
            }
        }

        let mut stmt = conn.prepare(
            "SELECT source, provider, provider_id, model,
                    price_in_usd, price_out_usd, price_in_sats, price_out_sats,
                    prev_price_out_usd, prev_price_in_usd,
                    endpoint, region, context_length, observed_at, raw_kind
             FROM quotes",
        )?;
        let quotes: Vec<Quote> = stmt
            .query_map([], |row| {
                Ok(Quote {
                    source: row.get(0)?,
                    provider: row.get(1)?,
                    provider_id: row.get(2)?,
                    model: row.get(3)?,
                    price_in_usd: row.get(4)?,
                    price_out_usd: row.get(5)?,
                    price_in_sats: row.get(6)?,
                    price_out_sats: row.get(7)?,
                    prev_price_out_usd: row.get(8)?,
                    prev_price_in_usd: row.get(9)?,
                    endpoint: row.get(10)?,
                    region: row.get(11)?,
                    context_length: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
                    observed_at: parse_ts(&row.get::<_, String>(13)?),
                    raw_kind: row.get::<_, Option<i64>>(14)?.map(|v| v as u32),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        let n_quotes = quotes.len();
        store.load_quotes(quotes);

        let mut stmt = conn.prepare(
            "SELECT endpoint, provider_id, name, onion, mint, version, region, pubkey,
                    discovered_via, last_seen, reliability, poll_ok, poll_total, last_latency_ms
             FROM nodes",
        )?;
        let nodes: Vec<ProviderNode> = stmt
            .query_map([], |row| {
                Ok(ProviderNode {
                    endpoint: row.get(0)?,
                    provider_id: row.get(1)?,
                    name: row.get(2)?,
                    onion: row.get(3)?,
                    mint: row.get(4)?,
                    version: row.get(5)?,
                    region: row.get(6)?,
                    pubkey: row.get(7)?,
                    discovered_via: row.get(8)?,
                    last_seen: parse_ts(&row.get::<_, String>(9)?),
                    reliability: row.get(10)?,
                    poll_ok: row.get::<_, i64>(11)? as u32,
                    poll_total: row.get::<_, i64>(12)? as u32,
                    last_latency_ms: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        let n_nodes = nodes.len();
        store.load_nodes(nodes);

        info!(quotes = n_quotes, nodes = n_nodes, "restored from sqlite");
        Ok(())
    }

    pub fn save_from(&self, store: &Store) -> Result<()> {
        let conn = self.conn.lock().expect("db lock");
        let tx = conn.unchecked_transaction()?;

        tx.execute("DELETE FROM quotes", [])?;
        tx.execute("DELETE FROM nodes", [])?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO quotes (
                    key, source, provider, provider_id, model,
                    price_in_usd, price_out_usd, price_in_sats, price_out_sats,
                    prev_price_out_usd, prev_price_in_usd,
                    endpoint, region, context_length, observed_at, raw_kind
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            )?;
            for q in store.all_quotes_raw() {
                let key = Store::quote_key(&q.source, &q.provider_id, &q.model);
                stmt.execute(params![
                    key,
                    q.source,
                    q.provider,
                    q.provider_id,
                    q.model,
                    q.price_in_usd,
                    q.price_out_usd,
                    q.price_in_sats,
                    q.price_out_sats,
                    q.prev_price_out_usd,
                    q.prev_price_in_usd,
                    q.endpoint,
                    q.region,
                    q.context_length.map(|v| v as i64),
                    q.observed_at.to_rfc3339(),
                    q.raw_kind.map(|v| v as i64),
                ])?;
            }
        }

        {
            let mut stmt = tx.prepare(
                "INSERT INTO nodes (
                    endpoint, provider_id, name, onion, mint, version, region, pubkey,
                    discovered_via, last_seen, reliability, poll_ok, poll_total, last_latency_ms
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            )?;
            for n in store.all_nodes_raw() {
                stmt.execute(params![
                    n.endpoint,
                    n.provider_id,
                    n.name,
                    n.onion,
                    n.mint,
                    n.version,
                    n.region,
                    n.pubkey,
                    n.discovered_via,
                    n.last_seen.to_rfc3339(),
                    n.reliability,
                    n.poll_ok as i64,
                    n.poll_total as i64,
                    n.last_latency_ms.map(|v| v as i64),
                ])?;
            }
        }

        if let Some(rate) = store.btc_usd() {
            tx.execute(
                "INSERT INTO meta(key, value) VALUES('btc_usd', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![rate.to_string()],
            )?;
        }

        tx.commit()?;
        debug!(
            quotes = store.quote_count(),
            nodes = store.node_count(),
            "sqlite snapshot saved"
        );
        Ok(())
    }
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc.timestamp_opt(0, 0).single().unwrap_or_else(Utc::now))
}

/// Periodic background snapshot of the in-memory store.
pub async fn run_persist_loop(store: Arc<Store>, db: Arc<Db>) {
    info!(path = %db.path().display(), "persist loop started");
    let mut interval = tokio::time::interval(SAVE_INTERVAL);
    loop {
        interval.tick().await;
        let store = Arc::clone(&store);
        let db = Arc::clone(&db);
        let res = tokio::task::spawn_blocking(move || db.save_from(&store)).await;
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "sqlite save failed"),
            Err(e) => warn!(error = %e, "sqlite save task join failed"),
        }
    }
}
