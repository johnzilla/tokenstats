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
use chrono::Utc;
use nostr_sdk::prelude::*;
use tracing::{debug, info, warn};

use crate::store::{normalize_endpoint, ProviderNode, Store};

/// RIP-02 Provider Information Event kind.
pub const ROUTSTR_PROVIDER_KIND: u16 = 38421;

/// Roadmap / older docs sometimes cite kind 40500 — subscribe for forward-compat.
pub const ROUTSTR_LEGACY_KIND: u16 = 40500;

/// Connect to relays and subscribe to Routstr provider announcements.
pub async fn run_listener(store: Arc<Store>, relays: Vec<String>, kinds: Vec<u16>) -> Result<()> {
    info!(?relays, ?kinds, "starting Nostr listener");

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

    let kinds_u64: Vec<Kind> = kinds
        .iter()
        .map(|k| Kind::Custom(u64::from(*k)))
        .collect();
    let filter = Filter::new().kinds(kinds_u64).limit(500);
    let sub_id = client.subscribe(vec![filter], None).await;
    info!(%sub_id, ?kinds, "subscribed to Routstr provider announcements");

    let mut notifications = client.notifications();

    loop {
        tokio::select! {
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
                        tokio::time::sleep(Duration::from_secs(1)).await;
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

    client.disconnect().await.context("disconnect")?;
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

    let d_id = tags
        .d
        .clone()
        .unwrap_or_else(|| event.pubkey.to_string());
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
        last_seen: Utc::now(),
        reliability: 0.0,
        poll_ok: 0,
        poll_total: 0,
        last_latency_ms: None,
    };

    info!(
        endpoint = %node.endpoint,
        provider = %node.name,
        kind,
        "discovered Routstr node"
    );
    store.upsert_node(node);
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

    meta.name = v
        .get("name")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    meta.region = v
        .get("region")
        .and_then(|x| x.as_str())
        .map(str::to_string);

    if let Some(endpoints) = v.get("endpoints") {
        meta.http_endpoint = endpoints
            .get("http")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        meta.onion_endpoint = endpoints
            .get("onion")
            .and_then(|x| x.as_str())
            .map(str::to_string);
    } else {
        meta.http_endpoint = v
            .get("endpoint")
            .or_else(|| v.get("url"))
            .and_then(|x| x.as_str())
            .map(str::to_string);
    }

    meta
}

fn short_id(s: &str) -> String {
    s.chars().take(8).collect()
}
