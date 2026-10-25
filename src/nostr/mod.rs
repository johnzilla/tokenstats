//! Nostr listener for Routstr discovery (RIP-02 kind 38421 provider announcements).
//!
//! Spec: https://github.com/Routstr/protocol/blob/main/RIP-02.md
//!
//! ```text
//! kind 38421 tags:
//!   ["d", "<unique-provider-identifier>"]
//!   ["u", "https://..."]            // HTTP endpoint
//!   ["u", "<tor-onion-address>"]    // optional onion
//!   ["mint", "https://mint…"]
//!   ["version", "0.0.1"]
//!   ["g", "US"]                     // region (docs)
//! ```
//!
//! Models and pricing are fetched from the node's `/v1/models` (not the event body).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use nostr_sdk::prelude::*;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::store::{normalize_endpoint, ProviderNode, Store};

/// Does a node's self-reported `npub` (from `/v1/info`) match the key that
/// announced it on Nostr? `None` when there is nothing to compare; a reported
/// key that doesn't parse counts as a mismatch.
pub fn same_pubkey(announced: Option<&str>, reported: Option<&str>) -> Option<bool> {
    let announced = PublicKey::parse(announced?).ok()?;
    Some(PublicKey::parse(reported?).is_ok_and(|r| r == announced))
}

/// RIP-02 Provider Information Event kind.
pub const ROUTSTR_PROVIDER_KIND: u16 = 38421;

/// Roadmap / older docs sometimes cite kind 40500 — subscribe for forward-compat.
pub const ROUTSTR_LEGACY_KIND: u16 = 40500;

/// Connect to relays and subscribe to Routstr provider announcements.
pub async fn run_listener(
    store: Arc<Store>,
    relays: Vec<String>,
    kinds: Vec<u16>,
    cancel: CancellationToken,
) -> Result<()> {
    info!(relays = relays.len(), ?kinds, "starting Nostr listener");

    let keys = Keys::generate();
    let client = Client::new(&keys);

    client
        .add_relays(relays.clone())
        .await
        .context("failed to add relays")?;
    for url in &relays {
        info!(%url, "relay configured");
    }

    client.connect().await;

    let kinds_u64: Vec<Kind> = kinds.iter().map(|k| Kind::Custom(u64::from(*k))).collect();
    let filter = Filter::new().kinds(kinds_u64).limit(500);
    let sub_id = client.subscribe(vec![filter], None).await;
    info!(%sub_id, ?kinds, "subscribed to Routstr provider announcements");

    let mut notifications = client.notifications();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("nostr listener received shutdown");
                break;
            }
            notification = notifications.recv() => {
                match notification {
                    Ok(RelayPoolNotification::Event { event, .. }) => {
                        if let Err(e) = handle_event(&store, &event) {
                            debug!(error = %e, "skipped Nostr event");
                        }
                    }
                    Ok(RelayPoolNotification::Message { .. }) => {}
                    Ok(RelayPoolNotification::RelayStatus { relay_url, status }) => {
                        debug!(%relay_url, ?status, "relay status");
                    }
                    Ok(RelayPoolNotification::Stop) => {
                        info!("Nostr pool stopped");
                        break;
                    }
                    Ok(RelayPoolNotification::Shutdown) => {
                        info!("Nostr pool shutdown");
                        break;
                    }
                    Err(e) => {
                        warn!(error = %e, "notification channel error");
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                debug!(
                    nodes = store.node_count(),
                    quotes = store.quote_count(),
                    "Nostr listener heartbeat"
                );
            }
        }
    }

    if let Err(e) = client.disconnect().await {
        warn!(error = %e, "nostr disconnect error (ignored during shutdown)");
    }
    Ok(())
}

fn handle_event(store: &Store, event: &Event) -> Result<()> {
    let kind = event.kind.as_u32();
    debug!(
        id = %event.id,
        kind,
        pubkey = %event.pubkey,
        "Nostr event"
    );

    let tags = parse_tags(&event.tags);
    let content_meta = parse_content_meta(event.content.trim());

    let http = tags
        .urls
        .iter()
        .find(|u| u.starts_with("http://") || u.starts_with("https://"))
        .cloned()
        .or(content_meta.http_endpoint);

    let onion = tags
        .urls
        .iter()
        .find(|u| u.contains(".onion"))
        .cloned()
        .or(content_meta.onion_endpoint);

    let endpoint = match http.or_else(|| onion.clone()) {
        Some(e) => e,
        None => {
            debug!(id = %event.id, "no endpoint in kind {kind} event");
            return Ok(());
        }
    };

    let d_id = tags.d.clone().unwrap_or_else(|| event.pubkey.to_string());
    let name = content_meta
        .name
        .unwrap_or_else(|| format!("node-{}", short_id(&d_id)));
    let region = tags.region.or(content_meta.region);

    let node = ProviderNode {
        provider_id: d_id,
        name,
        endpoint: normalize_endpoint(&endpoint),
        onion,
        mint: tags.mint,
        version: tags.version,
        region,
        pubkey: Some(event.pubkey.to_string()),
        discovered_via: format!("nostr:{kind}"),
        // When the node announced itself, not when a relay replayed it; clamped
        // so a future-dated event can't dodge the node TTL.
        last_seen: DateTime::from_timestamp(event.created_at.as_u64() as i64, 0)
            .unwrap_or_else(Utc::now)
            .min(Utc::now()),
        reliability: 0.0,
        poll_ok: 0,
        poll_total: 0,
        last_latency_ms: None,
        skip_reason: None,
    };

    info!(
        endpoint = %node.endpoint,
        provider = %node.name,
        kind,
        "discovered Routstr node"
    );
    store.record_announcement();
    let endpoint = node.endpoint.clone();
    if !store.upsert_node(node) {
        // debug, not warn: a flood would otherwise flood the logs too.
        debug!(%endpoint, "node cap reached; ignoring announcement (see --max-nodes)");
    }
    Ok(())
}

#[derive(Default)]
struct TagFields {
    d: Option<String>,
    urls: Vec<String>,
    mint: Option<String>,
    version: Option<String>,
    region: Option<String>,
}

fn parse_tags(tags: &[Tag]) -> TagFields {
    let mut out = TagFields::default();
    for t in tags {
        let v = t.as_vec();
        let Some(kind) = v.first().map(String::as_str) else {
            continue;
        };
        let val = v.get(1).cloned();
        match kind {
            "d" => out.d = val,
            "u" => {
                if let Some(u) = val {
                    out.urls.push(u);
                }
            }
            "mint" => out.mint = val,
            "version" => out.version = val,
            "g" => out.region = val, // geohash / region letter
            "region" => out.region = val,
            _ => {}
        }
    }
    out
}

#[derive(Default)]
struct ContentMeta {
    name: Option<String>,
    region: Option<String>,
    http_endpoint: Option<String>,
    onion_endpoint: Option<String>,
}

/// Optional kind:0-style metadata or legacy JSON content (docs show embedded objects).
fn parse_content_meta(content: &str) -> ContentMeta {
    let mut meta = ContentMeta::default();
    if content.is_empty() {
        return meta;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else {
        // plain-text name fallback
        if content.len() < 80 {
            meta.name = Some(content.to_string());
        }
        return meta;
    };

    meta.name = v.get("name").and_then(|x| x.as_str()).map(str::to_string);
    meta.region = v.get("region").and_then(|x| x.as_str()).map(str::to_string);

    if let Some(endpoints) = v.get("endpoints") {
        if let Some(list) = endpoints.as_array() {
            // Array shape: ["https://…", "http://….onion"] or [{"url": "…"}, …].
            for url in list.iter().filter_map(|e| {
                e.as_str()
                    .or_else(|| e.get("url").and_then(|u| u.as_str()))
                    .or_else(|| e.get("http").and_then(|u| u.as_str()))
            }) {
                let slot = if is_onion(url) {
                    &mut meta.onion_endpoint
                } else {
                    &mut meta.http_endpoint
                };
                slot.get_or_insert_with(|| url.to_string());
            }
        } else {
            meta.http_endpoint = endpoints
                .get("http")
                .and_then(|x| x.as_str())
                .map(str::to_string);
            meta.onion_endpoint = endpoints
                .get("onion")
                .and_then(|x| x.as_str())
                .map(str::to_string);
        }
    } else {
        meta.http_endpoint = v
            .get("endpoint")
            .or_else(|| v.get("url"))
            .and_then(|x| x.as_str())
            .map(str::to_string);
    }

    meta
}

fn is_onion(url: &str) -> bool {
    url.contains(".onion")
}

fn short_id(s: &str) -> String {
    s.chars().take(8).collect()
}

#[cfg(test)]
mod identity_tests {
    use super::same_pubkey;

    #[test]
    fn npub_matches_announcing_hex_key() {
        // From a live node: npub reported by /v1/info vs. event pubkey (hex).
        let hex = "7efd1dff86991cf6448780837a6c6dbbf79bb9b80dbc5b65b163191f0443b5a7";
        let npub = "npub10m73mluxnyw0v3y8szph5mrdh0mehwdcpk79ked3vvv37pzrkknshfwwd3";
        assert_eq!(same_pubkey(Some(hex), Some(npub)), Some(true));
        let other = nostr_sdk::Keys::generate().public_key().to_hex();
        assert_eq!(same_pubkey(Some(&other), Some(npub)), Some(false));
        assert_eq!(same_pubkey(Some(hex), Some("not-a-key")), Some(false));
        assert_eq!(same_pubkey(None, Some(npub)), None);
        assert_eq!(same_pubkey(Some(hex), None), None);
    }
}

#[cfg(test)]
mod content_meta_tests {
    use super::parse_content_meta;

    #[test]
    fn endpoints_object_and_array_shapes() {
        let obj = parse_content_meta(
            r#"{"endpoints":{"http":"https://a.example","onion":"http://x.onion"}}"#,
        );
        assert_eq!(obj.http_endpoint.as_deref(), Some("https://a.example"));
        assert_eq!(obj.onion_endpoint.as_deref(), Some("http://x.onion"));

        let arr = parse_content_meta(
            r#"{"endpoints":["http://y.onion","https://b.example","https://c.example"]}"#,
        );
        assert_eq!(arr.http_endpoint.as_deref(), Some("https://b.example"));
        assert_eq!(arr.onion_endpoint.as_deref(), Some("http://y.onion"));

        let objs = parse_content_meta(
            r#"{"endpoints":[{"url":"https://d.example"},{"type":"tor","url":"http://z.onion"}]}"#,
        );
        assert_eq!(objs.http_endpoint.as_deref(), Some("https://d.example"));
        assert_eq!(objs.onion_endpoint.as_deref(), Some("http://z.onion"));
    }
}
