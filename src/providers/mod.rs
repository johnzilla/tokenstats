//! HTTP polling of inference catalogs (OpenRouter + discovered Routstr nodes).

mod catalog;
mod netguard;
mod reference;
mod routstr;

pub use catalog::model_kind;

/// Drop pooled connections idle this long. Polls run every ~60 s, longer than
/// many servers keep idle keep-alive connections, so reusing one races the
/// server's close ("connection closed before message completed"): OrcaRouter
/// fetches from the Droplet failed ~17% of the time that way.
pub const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// HTTP client for catalog / oracle polling.
pub fn http_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .user_agent(concat!("tokenstats/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// GET with one retry when the request died at the connection level (not a
/// timeout, not an HTTP status): a server closing a reused connection, a reset,
/// a refused connect. Used for the few fixed upstreams (reference catalogs,
/// BTC/USD) where a spurious failure costs a whole poll interval.
pub async fn get_with_retry(
    client: &reqwest::Client,
    url: &str,
) -> reqwest::Result<reqwest::Response> {
    match client.get(url).send().await {
        Err(e) if !e.is_timeout() && (e.is_request() || e.is_connect()) => {
            tracing::debug!(%url, error = %e, "connection-level failure; retrying once");
            client.get(url).send().await
        }
        other => other,
    }
}
pub use reference::{ReferenceCatalog, OPENROUTER, ORCAROUTER};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::store::{normalize_endpoint, FetchStatus, PollCycle, Store};

#[derive(Debug, Clone)]
pub struct PollerConfig {
    pub interval: Duration,
    pub http_timeout: Duration,
    /// Enabled centralized catalogs and the `/models` URL to fetch for each.
    pub reference_catalogs: Vec<(ReferenceCatalog, String)>,
    /// Max Routstr nodes polled at once.
    pub node_concurrency: usize,
    /// Wall-clock budget per node (all `/models` fallbacks together).
    pub node_timeout: Duration,
    /// Drop quotes not re-observed within this window (`None` = keep forever).
    pub quote_ttl: Option<Duration>,
    /// Drop discovered nodes not seen within this window (`None` = keep forever).
    pub node_ttl: Option<Duration>,
    /// Poll Nostr-announced nodes on private/loopback addresses (trusted LAN only).
    pub allow_private_nodes: bool,
}

pub async fn run_poller(
    store: Arc<Store>,
    cfg: PollerConfig,
    cancel: CancellationToken,
) -> Result<()> {
    info!(
        interval_secs = cfg.interval.as_secs(),
        reference = ?cfg
            .reference_catalogs
            .iter()
            .map(|(c, _)| c.source)
            .collect::<Vec<_>>(),
        "starting provider poller"
    );

    let client = http_client(cfg.http_timeout)?;
    let nodes =
        routstr::NodeFetcher::new(client.clone(), cfg.http_timeout, cfg.allow_private_nodes)?;

    tokio::select! {
        _ = cancel.cancelled() => {
            info!("provider poller received shutdown during initial poll");
            return Ok(());
        }
        res = poll_once(&client, &nodes, &store, &cfg) => {
            if let Err(e) = res {
                warn!(error = %e, "initial provider poll failed");
            }
        }
    }

    let mut interval = tokio::time::interval(cfg.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick completes immediately; skip so we don't double-poll after initial.
    interval.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("provider poller received shutdown");
                break;
            }
            _ = interval.tick() => {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("provider poller received shutdown mid-cycle");
                        break;
                    }
                    res = poll_once(&client, &nodes, &store, &cfg) => {
                        if let Err(e) = res {
                            warn!(error = %e, "provider poll failed");
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

async fn poll_once(
    client: &reqwest::Client,
    nodes: &routstr::NodeFetcher,
    store: &Store,
    cfg: &PollerConfig,
) -> Result<()> {
    let started = Instant::now();
    let mut delisted = 0usize;

    for (cat, url) in &cfg.reference_catalogs {
        let started = Instant::now();
        match reference::fetch_quotes(client, url, cat).await {
            Ok(quotes) => {
                let n = quotes.len();
                let latency_ms = started.elapsed().as_millis() as u64;
                store.record_poll(cat.endpoint, true, Some(latency_ms));
                store.record_fetch(cat.source, cat.source, cat.endpoint, Ok(n));
                delisted +=
                    store.replace_catalog(cat.source, cat.source, Some(cat.endpoint), quotes);
                info!(
                    source = cat.source,
                    models = n,
                    latency_ms,
                    "reference catalog updated"
                );
            }
            Err(e) => {
                store.record_poll(cat.endpoint, false, None);
                store.record_fetch(cat.source, cat.source, cat.endpoint, Err(format!("{e:#}")));
                warn!(source = cat.source, error = %e, "reference catalog poll failed");
            }
        }
    }

    // Poll nodes concurrently; results are applied here so tasks don't need the store.
    let sem = Arc::new(Semaphore::new(cfg.node_concurrency.max(1)));
    let mut tasks = JoinSet::new();
    let fetches: HashMap<String, FetchStatus> = store
        .fetch_statuses()
        .into_iter()
        .filter(|f| f.source == "routstr")
        .map(|f| (f.endpoint.clone(), f))
        .collect();
    let now = chrono::Utc::now();
    let mut nodes_dormant = 0usize;
    for node in store.list_nodes() {
        // Long-dead nodes (relays keep replaying their announcements) are
        // polled on a backoff instead of every cycle.
        if fetches
            .get(&normalize_endpoint(&node.endpoint))
            .is_some_and(|f| !f.poll_due(node.last_seen, cfg.interval, now))
        {
            nodes_dormant += 1;
            continue;
        }
        let nodes = nodes.clone();
        let sem = Arc::clone(&sem);
        let timeout = cfg.node_timeout;
        tasks.spawn(async move {
            let _permit = sem.acquire_owned().await;
            // poll() bounds each request; this only guards the address check.
            let res = tokio::time::timeout(timeout * 2, nodes.poll(&node, timeout))
                .await
                .unwrap_or_else(|_| {
                    Ok(routstr::NodePoll {
                        catalog: Err(anyhow::anyhow!("timed out after {}s", timeout.as_secs())),
                        info: Err(anyhow::anyhow!("timed out after {}s", timeout.as_secs())),
                    })
                });
            (node, res)
        });
    }

    let (mut nodes_ok, mut nodes_failed, mut nodes_blocked) = (0usize, 0usize, 0usize);
    while let Some(joined) = tasks.join_next().await {
        let Ok((node, res)) = joined else {
            continue;
        };
        let poll = match res {
            Ok(poll) => {
                store.set_node_skip_reason(&node.endpoint, None);
                poll
            }
            Err(blocked) => {
                // Never requested: no reliability sample, no catalog change.
                store.set_node_skip_reason(&node.endpoint, Some(blocked.0));
                nodes_blocked += 1;
                continue;
            }
        };
        store.record_probe(
            &node.endpoint,
            poll.info
                .map(|(info, ms)| {
                    let verified =
                        crate::nostr::same_pubkey(node.pubkey.as_deref(), info.npub.as_deref());
                    (info, ms, verified)
                })
                .map_err(|e| format!("{e:#}")),
        );
        match poll.catalog {
            Ok((quotes, latency_ms)) => {
                nodes_ok += 1;
                let n = quotes.len();
                store.record_poll(&node.endpoint, true, Some(latency_ms));
                store.record_fetch("routstr", &node.provider_id, &node.endpoint, Ok(n));
                delisted += store.replace_catalog(
                    "routstr",
                    &node.provider_id,
                    Some(&node.endpoint),
                    quotes,
                );
                info!(
                    endpoint = %node.endpoint,
                    models = n,
                    latency_ms,
                    "Routstr node catalog updated"
                );
            }
            Err(e) => {
                nodes_failed += 1;
                store.record_poll(&node.endpoint, false, None);
                store.record_fetch(
                    "routstr",
                    &node.provider_id,
                    &node.endpoint,
                    Err(format!("{e:#}")),
                );
                warn!(
                    endpoint = %node.endpoint,
                    error = %e,
                    "Routstr node poll failed"
                );
            }
        }
    }

    let expired = cfg
        .quote_ttl
        .and_then(|ttl| chrono::Duration::from_std(ttl).ok())
        .map(|ttl| store.prune_stale_quotes(ttl))
        .unwrap_or(0);

    let nodes_expired = cfg
        .node_ttl
        .and_then(|ttl| chrono::Duration::from_std(ttl).ok())
        .map(|ttl| store.prune_stale_nodes(ttl))
        .unwrap_or(0);
    if nodes_expired > 0 {
        info!(nodes_expired, "expired nodes not seen within node TTL");
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    store.record_cycle(PollCycle {
        finished_at: chrono::Utc::now(),
        elapsed_ms,
        nodes_ok,
        nodes_failed,
        nodes_dormant,
        delisted,
        expired,
    });
    info!(
        nodes_ok,
        nodes_failed,
        nodes_dormant,
        nodes_blocked,
        delisted,
        expired,
        quotes = store.quote_count(),
        elapsed_ms,
        "poll cycle complete"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// First connection: read the request, then hang up without answering
    /// (what a server closing a reused keep-alive connection looks like).
    /// Second connection: answer 200.
    async fn flaky_server(listener: TcpListener) {
        let mut buf = [0u8; 1024];
        if let Ok((mut sock, _)) = listener.accept().await {
            let _ = sock.read(&mut buf).await;
            drop(sock);
        }
        if let Ok((mut sock, _)) = listener.accept().await {
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .await;
        }
    }

    #[tokio::test]
    async fn retries_once_after_connection_dropped_mid_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/models", listener.local_addr().unwrap());
        let server = tokio::spawn(flaky_server(listener));
        let client = http_client(Duration::from_secs(2)).unwrap();
        let resp = get_with_retry(&client, &url).await.expect("retry succeeds");
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn plain_request_fails_on_the_same_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/models", listener.local_addr().unwrap());
        let server = tokio::spawn(flaky_server(listener));
        let client = http_client(Duration::from_secs(2)).unwrap();
        let err = client.get(&url).send().await.unwrap_err();
        assert!(err.is_request() && !err.is_timeout(), "{err:#}");
        server.abort();
    }

    #[tokio::test]
    async fn http_status_errors_are_not_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/models", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
            // A retry would connect again; make sure nobody does.
            tokio::time::timeout(Duration::from_millis(300), listener.accept())
                .await
                .is_err()
        });
        let client = http_client(Duration::from_secs(2)).unwrap();
        assert_eq!(get_with_retry(&client, &url).await.unwrap().status(), 503);
        assert!(server.await.unwrap(), "503 must not be retried");
    }
}
