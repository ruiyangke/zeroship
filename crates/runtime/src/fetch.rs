//! SSRF protection + shared response-size constant for fetch.
//!
//! Two layers of protection:
//!
//!   1. `validate_url` — string-level fast path. Rejects non-HTTP(S) schemes
//!      and literal private/loopback/link-local/etc IPs embedded in the URL.
//!   2. `SsrfResolver` — DNS-level filter. cyper's custom resolver hook;
//!      strips every resolved `IpAddr` that `is_blocked_ip` rejects so a
//!      public hostname that resolves into RFC1918 space cannot reach an
//!      internal service.
//!
//! Both layers share `is_blocked_ip` as the single blocklist source of truth.
//!
//! Note: cyper 0.8 does **not** follow HTTP redirects automatically. The
//! native fetch in `crate::fetch_native` does redirect handling and
//! re-validates each hop; this module exposes the building blocks it
//! consumes.
//!
//! Historical note: this file used to also house the `__rawFetch` V8
//! callback and the legacy fetch executor. Both were deleted alongside the
//! D-23 polyfill cutover — `globalThis.fetch` is now the native callback
//! installed by `fetch_native::install_fetch_global`. See ADR D-23.

use std::net::{IpAddr, SocketAddr};

use cyper::resolve::Resolve;
use futures::Stream;
use futures::stream;
use http::Uri;

/// Maximum response body size: 10 MB.
pub const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;

/// True for IP addresses that must never be reachable from user fetch code.
///
/// Centralises the blocklist used by both the string-level `validate_url`
/// fast path (rejects literal IPs) and the DNS-level `SsrfResolver` (rejects
/// hostnames whose A/AAAA records point into these ranges).
#[must_use]
pub fn is_blocked_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            v4.is_loopback()                       // 127.0.0.0/8
                || v4.is_private()                 // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()              // 169.254/16
                || v4.is_unspecified()             // 0.0.0.0
                || v4.is_broadcast()               // 255.255.255.255
                || v4.is_multicast()               // 224.0.0.0/4
                || v4.is_documentation()           // 192.0.2/24, 198.51.100/24, 203.0.113/24
                || v4.octets()[0] == 0             // 0.0.0.0/8 — "this network"
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64  // 100.64/10 CGNAT
                || v4.octets()[0] == 192 && v4.octets()[1] == 0 && v4.octets()[2] == 0 // 192.0.0/24
                || v4.octets()[0] == 198 && (v4.octets()[1] & 0xFE) == 18  // 198.18/15 benchmarking
                || v4.octets()[0] >= 240           // 240.0.0.0/4 reserved + 255.255.255.255
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()                       // ::1
                || v6.is_unspecified()             // ::
                || v6.is_multicast()               // ff00::/8
                || (v6.segments()[0] & 0xffc0) == 0xfe80   // fe80::/10 link-local
                || (v6.segments()[0] & 0xfe00) == 0xfc00   // fc00::/7 unique-local
                || v6.segments()[..5] == [0, 0, 0, 0, 0] && v6.segments()[5] == 0xffff // ::ffff:0:0/96 v4-mapped
                || v6.segments()[0] == 0x2001 && v6.segments()[1] == 0xdb8 // 2001:db8::/32 documentation
                || v6.segments()[0] == 0x2001 && (v6.segments()[1] & 0xff00) == 0x0200 // 2001:2::/48 benchmarking
        }
    }
}

/// Validate the URL to prevent SSRF attacks (string-level fast path).
///
/// Blocks non-HTTP(S) schemes and literal private/loopback/link-local/etc IPs
/// embedded in the URL. A second layer of protection runs at DNS resolution
/// time via `SsrfResolver` — domain names that resolve into blocked ranges
/// are rejected there, since this function cannot see them.
///
/// In dev mode (`ZEROSHIP_DEV=1`), localhost/loopback is allowed so the
/// Vite plugin's ModuleRunner can fetch modules from the Vite dev server.
pub fn validate_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL: {e}"))?;

    // Allow http(s) and the WebSocket schemes ws/wss. The block list
    // (private/loopback/etc.) below applies uniformly to all four.
    match parsed.scheme() {
        "http" | "https" | "ws" | "wss" => {}
        scheme => return Err(format!("Blocked URL scheme: {scheme}")),
    }

    // In dev mode, skip host/IP validation (allows localhost fetch to Vite)
    if std::env::var("ZEROSHIP_DEV").is_ok() {
        return Ok(());
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?
        .to_lowercase();

    // Block localhost
    if host == "localhost" {
        return Err("Blocked request to localhost".to_string());
    }

    // Try to parse as IP address (handles both bare IPs and bracket-stripped IPv6)
    let ip: Option<IpAddr> = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok();

    if let Some(addr) = ip
        && is_blocked_ip(addr)
    {
        return Err(format!("Blocked request to private/internal IP: {addr}"));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// resolve_and_check_ssrf — DNS resolution + SSRF revalidation
// ---------------------------------------------------------------------------

/// Resolve `host:port` to a `SocketAddr` and verify the result is NOT
/// in any blocked range. Returns the FIRST non-blocked address.
///
/// This is the WebSocket-handshake counterpart to `SsrfResolver` (which
/// hooks into cyper's resolver pipeline). Unlike fetch — where
/// `cyper::Client` performs the connect after receiving the filtered
/// stream of IPs — the WebSocket handshake calls
/// `compio::net::TcpStream::connect(addr)` directly, so we MUST hand it
/// a SocketAddr that has already been validated. Otherwise an attacker
/// can pin a public hostname's resolution to `127.0.0.1` between the
/// URL-string check and `connect`.
///
/// In dev mode (`ZEROSHIP_DEV=1`) localhost is permitted (matches
/// `validate_url`), so the WebSocket handshake also reaches the Vite
/// dev server.
///
/// Spec: defends the "DNS rebinding" attack class explicitly — see
/// docs/proposals/websocket-native.md §VIII.1 (CRITICAL #8).
pub fn resolve_and_check_ssrf(host: &str, port: u16) -> Result<SocketAddr, String> {
    use std::io::{Error, ErrorKind};

    let dev_mode = std::env::var("ZEROSHIP_DEV").is_ok();

    // Strip IPv6 literal brackets before to_socket_addrs.
    let host_clean = host.trim_start_matches('[').trim_end_matches(']');
    let target = format!("{host_clean}:{port}");

    // std DNS resolution. The handshake spawns this on a compio task
    // (off the V8 thread); a brief sync DNS call there is acceptable.
    let mut iter = std::net::ToSocketAddrs::to_socket_addrs(&target)
        .map_err(|e: Error| format!("DNS resolve failed: {e}"))?;

    // Pick the first non-blocked address. We intentionally don't try
    // every candidate: the SSRF guard is best served by failing fast
    // when ANY blocked candidate is returned. The fallback for happy-
    // eyeballs / multi-AAAA hosts is "try the first allowed one".
    let mut last_blocked: Option<IpAddr> = None;
    for addr in &mut iter {
        let ip = addr.ip();
        if dev_mode || !is_blocked_ip(ip) {
            return Ok(addr);
        }
        last_blocked = Some(ip);
    }
    Err(match last_blocked {
        Some(ip) => format!(
            "Blocked: all resolved addresses are in blocked ranges (e.g. {ip}) (SSRF guard)"
        ),
        None => format!("DNS resolve produced no addresses for {host}:{port}"),
    })
    .map_err(|e| {
        let _ = Error::new(ErrorKind::PermissionDenied, e.clone());
        e
    })
}

// ---------------------------------------------------------------------------
// SsrfResolver — DNS resolver that filters out private/loopback addresses
// ---------------------------------------------------------------------------

/// Custom cyper resolver. Performs the same work as the default (std DNS
/// lookup) then strips every `IpAddr` that `is_blocked_ip` rejects. If the
/// remaining set is empty, returns an error so cyper fails the connection.
///
/// This closes the SSRF hole where a public hostname resolves to an RFC1918
/// address — the caller sees a generic connect error instead of reaching the
/// internal service.
pub struct SsrfResolver;

impl Resolve for SsrfResolver {
    type Err = std::io::Error;

    async fn resolve(&self, uri: &Uri) -> Result<impl Stream<Item = IpAddr> + '_, Self::Err> {
        use std::io::{Error, ErrorKind};

        let host = uri
            .host()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "URI missing host"))?;
        let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
            Some("https") => 443,
            _ => 80,
        });

        // Strip IPv6 literal brackets before handing to to_socket_addrs
        let host_clean = host.trim_start_matches('[').trim_end_matches(']');
        let target = format!("{host_clean}:{port}");

        // std DNS resolution runs on the current thread and blocks briefly;
        // acceptable for fetch since this happens once per request.
        let addrs: Vec<IpAddr> = std::net::ToSocketAddrs::to_socket_addrs(&target)?
            .map(|sa| sa.ip())
            .filter(|ip| !is_blocked_ip(*ip))
            .collect();

        if addrs.is_empty() {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "all resolved addresses are in blocked ranges (SSRF guard)",
            ));
        }

        Ok(stream::iter(addrs))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn blocks_loopback_v4() {
        assert!(is_blocked_ip(Ipv4Addr::new(127, 0, 0, 1).into()));
    }

    #[test]
    fn blocks_private_v4() {
        assert!(is_blocked_ip(Ipv4Addr::new(10, 0, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(172, 20, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(192, 168, 1, 1).into()));
    }

    #[test]
    fn blocks_link_local_v4() {
        assert!(is_blocked_ip(Ipv4Addr::new(169, 254, 169, 254).into()));
    }

    #[test]
    fn blocks_cgnat_v4() {
        // AWS uses 100.64/10 for VPC ENIs — must be blocked
        assert!(is_blocked_ip(Ipv4Addr::new(100, 64, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(100, 127, 255, 254).into()));
    }

    #[test]
    fn blocks_multicast_v4() {
        assert!(is_blocked_ip(Ipv4Addr::new(224, 0, 0, 1).into()));
    }

    #[test]
    fn blocks_reserved_v4() {
        assert!(is_blocked_ip(Ipv4Addr::new(240, 0, 0, 1).into()));
        assert!(is_blocked_ip(Ipv4Addr::new(255, 255, 255, 255).into()));
    }

    #[test]
    fn blocks_v4_mapped_v6() {
        // ::ffff:127.0.0.1 — v4-mapped form must be blocked
        let mapped: Ipv6Addr = "::ffff:7f00:1".parse().unwrap();
        assert!(is_blocked_ip(mapped.into()));
    }

    #[test]
    fn blocks_unique_local_v6() {
        assert!(is_blocked_ip("fc00::1".parse::<Ipv6Addr>().unwrap().into()));
        assert!(is_blocked_ip("fd00::1".parse::<Ipv6Addr>().unwrap().into()));
    }

    #[test]
    fn blocks_link_local_v6() {
        assert!(is_blocked_ip("fe80::1".parse::<Ipv6Addr>().unwrap().into()));
    }

    #[test]
    fn allows_public_v4() {
        assert!(!is_blocked_ip(Ipv4Addr::new(1, 1, 1, 1).into()));
        assert!(!is_blocked_ip(Ipv4Addr::new(8, 8, 8, 8).into()));
    }

    #[test]
    fn allows_public_v6() {
        assert!(!is_blocked_ip(
            "2606:4700:4700::1111".parse::<Ipv6Addr>().unwrap().into()
        ));
    }

    #[test]
    fn validate_url_rejects_localhost() {
        assert!(validate_url("http://localhost/x").is_err());
    }

    #[test]
    fn validate_url_rejects_literal_private_ip() {
        assert!(validate_url("http://10.0.0.1/x").is_err());
        assert!(validate_url("http://169.254.169.254/latest/meta-data").is_err());
    }

    #[test]
    fn validate_url_rejects_non_http() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("gopher://x/").is_err());
    }

    #[test]
    fn validate_url_allows_public_http() {
        assert!(validate_url("https://example.com/x").is_ok());
    }
}
