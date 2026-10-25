//! Poll a Routstr-compatible node: GET {endpoint}/v1/models, plus a cheap
//! GET {endpoint}/v1/info probe for latency and self-reported identity.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use tracing::debug;

use crate::pricing::CatalogHint;
use crate::store::{NodeInfo, ProviderNode, Quote};

use super::catalog::parse_models_payload_with_hint;
use super::netguard;

/// Node was not polled because its endpoint is not public (SSRF guard).
#[derive(Debug)]
pub struct Blocked(pub String);

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "blocked: {}", self.0)
    }
}

impl std::error::Error for Blocked {}

/// Picks the HTTP client per node: operator-seeded (`--node`) nodes are trusted,
/// everything else (Nostr announcements) goes through [`netguard`].
#[derive(Clone)]
pub struct NodeFetcher {
    open: reqwest::Client,
    /// `None` when `--allow-private-nodes` is set.
    guarded: Option<reqwest::Client>,
}

impl NodeFetcher {
    pub fn new(open: reqwest::Client, timeout: Duration, allow_private: bool) -> Result<Self> {
        let guarded = (!allow_private)
            .then(|| netguard::guarded_client(timeout))
            .transpose()?;
        Ok(Self { open, guarded })
    }

    /// Fetch a node's catalog, or fail with [`Blocked`] without sending a request.
    #[cfg(test)]
    pub async fn fetch(&self, node: &ProviderNode) -> Result<(Vec<Quote>, u64)> {
        fetch_node_quotes(self.client_for(node).await?, node).await
    }

    /// Catalog fetch and `/v1/info` probe side by side, each within `timeout`.
    /// [`Blocked`] means neither request was sent.
    pub async fn poll(&self, node: &ProviderNode, timeout: Duration) -> Result<NodePoll, Blocked> {
        let client = self.client_for(node).await?;
        let budget = |what: &'static str| {
            move || anyhow::anyhow!("{what} timed out after {}s", timeout.as_secs())
        };
        let (catalog, info) = tokio::join!(
            tokio::time::timeout(timeout, fetch_node_quotes(client, node)),
            tokio::time::timeout(timeout, fetch_node_info(client, node)),
        );
        Ok(NodePoll {
            catalog: catalog.unwrap_or_else(|_| Err(budget("catalog")())),
            info: info.unwrap_or_else(|_| Err(budget("info probe")())),
        })
    }

    async fn client_for(&self, node: &ProviderNode) -> Result<&reqwest::Client, Blocked> {
        match &self.guarded {
            Some(guarded) if node.discovered_via != "cli" => {
                netguard::check_endpoint(&node.endpoint)
                    .await
                    .map_err(Blocked)?;
                Ok(guarded)
            }
            _ => Ok(&self.open),
        }
    }
}

/// Results of one node poll: `(quotes, latency_ms)` and `(info, latency_ms)`.
pub struct NodePoll {
    pub catalog: Result<(Vec<Quote>, u64)>,
    pub info: Result<(NodeInfo, u64)>,
}

async fn fetch_node_info(client: &reqwest::Client, node: &ProviderNode) -> Result<(NodeInfo, u64)> {
    let base = node.endpoint.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/info")
    } else {
        format!("{base}/v1/info")
    };
    let started = std::time::Instant::now();
    let body = try_fetch(client, &url).await?;
    let latency_ms = started.elapsed().as_millis() as u64;
    let mut info: NodeInfo =
        serde_json::from_value(body).with_context(|| format!("info shape {url}"))?;
    for field in [&mut info.npub, &mut info.onion_url, &mut info.version] {
        if field.as_deref().is_some_and(|v| v.trim().is_empty()) {
            *field = None;
        }
    }
    Ok((info, latency_ms))
}

async fn fetch_node_quotes(
    client: &reqwest::Client,
    node: &ProviderNode,
) -> Result<(Vec<Quote>, u64)> {
    let base = node.endpoint.trim_end_matches('/');
    // `/v1` bases have one candidate; bare hosts try `/v1/models` then `/models`.
    let urls = if base.ends_with("/v1") {
        vec![format!("{base}/models")]
    } else {
        vec![format!("{base}/v1/models"), format!("{base}/models")]
    };

    let mut last_err = None;
    for url in &urls {
        let started = std::time::Instant::now();
        match try_fetch(client, url).await {
            Ok(body) => {
                let latency_ms = started.elapsed().as_millis() as u64;
                let models = parse_models_payload_with_hint(&body, CatalogHint::Routstr);
                debug!(%url, n = models.len(), latency_ms, "Routstr models response");
                let now = Utc::now();
                let quotes = models
                    .into_iter()
                    .map(|m| {
                        let prices = m.prices;
                        Quote {
                            source: "routstr".into(),
                            provider: node.name.clone(),
                            provider_id: node.provider_id.clone(),
                            model: m.id,
                            // Dual-unit: USD from `pricing`, sats from `sats_pricing` when present.
                            // Oracle fills any missing side via BTC/USD.
                            price_in_usd: prices.price_in_usd,
                            price_out_usd: prices.price_out_usd,
                            price_in_sats: prices.price_in_sats,
                            price_out_sats: prices.price_out_sats,
                            prev_price_out_usd: None,
                            prev_price_in_usd: None,
                            endpoint: Some(base.to_string()),
                            region: node.region.clone(),
                            context_length: m.context_length,
                            observed_at: now,
                            raw_kind: None,
                            kind: m.kind,
                        }
                    })
                    .collect();
                return Ok((quotes, latency_ms));
            }
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no models URL worked for {base}")))
}

async fn try_fetch(client: &reqwest::Client, url: &str) -> Result<serde_json::Value> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("GET {url} → {}", resp.status());
    }
    resp.json().await.with_context(|| format!("JSON {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn node(endpoint: &str, via: &str) -> ProviderNode {
        ProviderNode {
            provider_id: "n".into(),
            name: "n".into(),
            endpoint: endpoint.into(),
            onion: None,
            mint: None,
            version: None,
            region: None,
            pubkey: None,
            discovered_via: via.into(),
            last_seen: Utc::now(),
            reliability: 0.0,
            poll_ok: 0,
            poll_total: 0,
            last_latency_ms: None,
            skip_reason: None,
        }
    }

    /// Bound local listener + its base URL.
    async fn listen() -> (String, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        (
            format!("http://{}", listener.local_addr().unwrap()),
            listener,
        )
    }

    async fn answer_once(listener: &TcpListener, response: &str) -> bool {
        let accept = tokio::time::timeout(Duration::from_millis(500), listener.accept()).await;
        let Ok(Ok((mut sock, _))) = accept else {
            return false;
        };
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf).await;
        let _ = sock.write_all(response.as_bytes()).await;
        true
    }

    const MODELS: &str = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
        content-length: 21\r\nconnection: close\r\n\r\n{\"data\":[{\"id\":\"m\"}]}";

    fn fetcher(allow_private: bool) -> NodeFetcher {
        NodeFetcher::new(
            reqwest::Client::new(),
            Duration::from_secs(2),
            allow_private,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn nostr_loopback_node_is_never_requested() {
        let (url, listener) = listen().await;
        let f = fetcher(false);
        let n = node(&url, "nostr:38421");
        let (res, hit) = tokio::join!(f.fetch(&n), answer_once(&listener, MODELS));
        assert!(res.unwrap_err().downcast_ref::<Blocked>().is_some());
        assert!(!hit, "guard must not connect");
    }

    #[tokio::test]
    async fn cli_and_allowed_nodes_are_polled() {
        for (via, allow) in [("cli", false), ("nostr:38421", true)] {
            let (url, listener) = listen().await;
            let f = fetcher(allow);
            let n = node(&url, via);
            let (res, hit) = tokio::join!(f.fetch(&n), answer_once(&listener, MODELS));
            assert!(hit);
            assert_eq!(res.unwrap().0.len(), 1, "{via}");
        }
    }

    #[tokio::test]
    async fn poll_of_blocked_node_sends_nothing() {
        let (url, listener) = listen().await;
        let f = fetcher(false);
        let n = node(&url, "nostr:38421");
        let (res, hit) = tokio::join!(
            f.poll(&n, Duration::from_secs(1)),
            answer_once(&listener, MODELS)
        );
        assert!(res.is_err());
        assert!(!hit, "guard must not connect");
    }

    #[tokio::test]
    async fn info_probe_parses_live_shape() {
        let body = r#"{"name":"ARoutstrNode","description":"A Routstr Node","version":"0.5.0","npub":"npub10m73mluxnyw0v3y8szph5mrdh0mehwdcpk79ked3vvv37pzrkknshfwwd3","mints":["https://mint.minibits.cash/Bitcoin"],"http_url":"https://app.chatstr.ai","onion_url":""}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, listener) = listen().await;
        let n = node(&url, "cli");
        let client = reqwest::Client::new();
        let (res, hit) = tokio::join!(fetch_node_info(&client, &n), answer_once(&listener, &resp));
        assert!(hit);
        let (info, _) = res.unwrap();
        assert_eq!(info.version.as_deref(), Some("0.5.0"));
        assert_eq!(info.mints.len(), 1);
        assert_eq!(info.onion_url, None, "empty onion_url becomes None");
        assert!(info.npub.unwrap().starts_with("npub1"));
    }

    #[tokio::test]
    async fn guarded_client_refuses_private_redirect() {
        const REDIRECT: &str = "HTTP/1.1 302 Found\r\nlocation: http://169.254.169.254/latest\r\n\
            content-length: 0\r\nconnection: close\r\n\r\n";
        let (url, listener) = listen().await;
        let client = netguard::guarded_client(Duration::from_secs(2)).unwrap();
        let (res, hit) = tokio::join!(try_fetch(&client, &url), answer_once(&listener, REDIRECT));
        assert!(hit);
        let err = format!("{:#}", res.unwrap_err());
        assert!(err.contains("redirect blocked"), "{err}");
    }
}
