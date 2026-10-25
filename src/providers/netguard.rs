//! SSRF guard for Nostr-announced node endpoints.
//!
//! Anyone on Nostr can announce a "node", so before polling one we refuse hosts that
//! are (or resolve to) loopback, private, CGNAT, link-local / cloud-metadata,
//! unique-local or unspecified addresses. The guarded client re-checks at connect
//! time via its DNS resolver (no rebinding window) and on every redirect hop.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{redirect, Url};

/// Why an IP must not be fetched for an untrusted node (`None` = public).
pub fn blocked_ip_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => blocked_v4(v4),
            None => blocked_v6(v6),
        },
    }
}

fn blocked_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let [a, b, ..] = ip.octets();
    if ip.is_loopback() {
        Some("loopback")
    } else if a == 0 {
        Some("unspecified")
    } else if ip.is_private() {
        Some("private (RFC1918)")
    } else if a == 100 && (b & 0xc0) == 64 {
        Some("CGNAT (100.64/10)")
    } else if ip.is_link_local() {
        Some("link-local / cloud metadata")
    } else if ip.is_broadcast() || ip.is_multicast() {
        Some("broadcast / multicast")
    } else {
        None
    }
}

fn blocked_v6(ip: Ipv6Addr) -> Option<&'static str> {
    let seg = ip.segments();
    let first = seg[0];
    let embedded_v4 = || {
        Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        )
    };
    if ip.is_loopback() {
        Some("loopback")
    } else if ip.is_unspecified() {
        Some("unspecified")
    } else if seg[..6] == [0; 6] {
        // ::a.b.c.d, IPv4-compatible (RFC 4291 §2.5.5.1, deprecated): no
        // defined routing, so never fetch it whatever it embeds.
        Some("IPv4-compatible IPv6 (deprecated)")
    } else if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] && blocked_v4(embedded_v4()).is_some() {
        // NAT64 well-known prefix (RFC 6052): a local NAT64 gateway would
        // forward to the embedded private IPv4 address.
        Some("NAT64 to a non-public IPv4 address")
    } else if (first & 0xfe00) == 0xfc00 {
        Some("unique-local (fc00::/7)")
    } else if (first & 0xffc0) == 0xfe80 {
        Some("link-local")
    } else if ip.is_multicast() {
        Some("multicast")
    } else {
        None
    }
}

/// Checks that need no DNS. `Ok(Some((host, port)))` still needs a lookup.
fn check_url_literal(url: &Url) -> Result<Option<(String, u16)>, String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("unsupported scheme {}", url.scheme()));
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let host = url.host_str().ok_or("missing host")?;
    // The URL parser already normalizes IPv4 spellings (`0x7f.1`, `2130706433`).
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return check_ip(ip).map(|_| None);
    }
    let d = host.trim_end_matches('.').to_ascii_lowercase();
    if d == "localhost" || d.ends_with(".localhost") {
        Err(format!("{d} is loopback"))
    } else {
        Ok(Some((d, port)))
    }
}

fn check_ip(ip: IpAddr) -> Result<(), String> {
    match blocked_ip_reason(ip) {
        Some(why) => Err(format!("{ip} is {why}")),
        None => Ok(()),
    }
}

/// Pre-poll check for an untrusted endpoint. `Err` carries a human-readable reason.
///
/// DNS failures pass here (the fetch then fails normally); the guarded resolver
/// still refuses private answers at connect time.
pub async fn check_endpoint(endpoint: &str) -> Result<(), String> {
    let url = Url::parse(endpoint).map_err(|e| format!("invalid URL: {e}"))?;
    let Some((host, port)) = check_url_literal(&url)? else {
        return Ok(());
    };
    let Ok(addrs) = tokio::net::lookup_host((host.as_str(), port)).await else {
        return Ok(());
    };
    for addr in addrs {
        if let Some(why) = blocked_ip_reason(addr.ip()) {
            return Err(format!("{host} resolves to {} ({why})", addr.ip()));
        }
    }
    Ok(())
}

/// Resolver that drops non-public answers, so the connection uses a checked address.
struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| blocked_ip_reason(a.ip()).is_none())
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} has no public addresses").into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// HTTP client for untrusted nodes: public-only DNS + redirect re-checks.
pub fn guarded_client(timeout: Duration) -> Result<reqwest::Client> {
    let policy = redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        // Domains are re-checked by the resolver; literals and localhost here.
        match check_url_literal(attempt.url()) {
            Ok(_) => attempt.follow(),
            Err(why) => attempt.error(format!("redirect blocked: {why}")),
        }
    });
    Ok(reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("tokenstats/", env!("CARGO_PKG_VERSION")))
        .redirect(policy)
        .dns_resolver(Arc::new(PublicOnlyResolver))
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(s: &str) -> bool {
        blocked_ip_reason(s.parse().unwrap()).is_some()
    }

    #[test]
    fn classifies_ipv4() {
        for ip in [
            "127.0.0.1",
            "127.8.9.10",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.169.254",
            "255.255.255.255",
            "224.0.0.1",
        ] {
            assert!(blocked(ip), "{ip} should be blocked");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "100.128.0.1",
            "100.63.255.255",
        ] {
            assert!(!blocked(ip), "{ip} should be allowed");
        }
    }

    #[test]
    fn classifies_ipv6() {
        for ip in [
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "::10.0.0.1",
            "::8.8.8.8",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
        ] {
            assert!(blocked(ip), "{ip} should be blocked");
        }
        for ip in [
            "2606:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(!blocked(ip), "{ip} should be allowed");
        }
    }

    fn literal(u: &str) -> Result<Option<(String, u16)>, String> {
        check_url_literal(&Url::parse(u).unwrap())
    }

    #[test]
    fn url_literals() {
        assert!(literal("http://127.0.0.1:18131").is_err());
        assert!(literal("http://[::1]:8080/v1").is_err());
        assert!(literal("http://169.254.169.254/latest/meta-data").is_err());
        // Alternate IPv4 spellings are normalized by the URL parser.
        assert!(literal("http://2130706433/").is_err());
        assert!(literal("http://0x7f.1/").is_err());
        assert!(literal("http://localhost:3000").is_err());
        assert!(literal("http://LOCALHOST./").is_err());
        assert!(literal("http://api.localhost/").is_err());
        assert!(literal("file:///etc/passwd").is_err());
        assert_eq!(
            literal("https://node.example.com/v1").unwrap(),
            Some(("node.example.com".into(), 443))
        );
        assert_eq!(literal("https://8.8.8.8/").unwrap(), None);
    }

    #[tokio::test]
    async fn check_endpoint_blocks_literals() {
        assert!(check_endpoint("http://127.0.0.1:18131").await.is_err());
        assert!(check_endpoint("not a url").await.is_err());
        assert!(check_endpoint("https://1.1.1.1").await.is_ok());
    }
}
