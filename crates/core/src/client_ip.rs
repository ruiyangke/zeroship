//! One client-address resolver for every service that keys a rate-limit
//! bucket or an audit record on the caller's IP.
//!
//! The gateway, the control plane and the auth service each used to answer
//! "who is the client?" their own way, and two of the three answers
//! disagreed. The gateway deferred to `ntex`'s `ConnectionInfo::remote()`,
//! which reads the LEFTMOST `X-Forwarded-For` token; control and auth read
//! the RIGHTMOST. Since a rate-limit bucket and an audit row are supposed to
//! name the same client, at most one of those could be right.
//!
//! # Rightmost, and why
//!
//! `X-Forwarded-For` grows left to right: each hop APPENDS the address it
//! saw. Only the last entry was written by the hop closest to us; everything
//! left of it was copied forward from what the caller sent, and the caller
//! can send anything. The proxy in front of this stack (`deploy/ops/Caddyfile`
//! -> Caddy `reverse_proxy`) appends the peer it accepted the connection
//! from, so the rightmost token is that peer and the leftmost is caller-
//! authored text.
//!
//! This assumes exactly ONE trusted hop, which is what `trust_proxy` means
//! today: a bool, not a hop count. Under a second fronting proxy the
//! rightmost token is the inner proxy's address, and pinning the real client
//! then needs a trusted-hop count or an ingress allowlist. That is the
//! separately tracked ingress-allowlist design; this module deliberately does
//! not anticipate it.
//!
//! # What this is NOT
//!
//! Resolving an address is not authenticating one. When `trust_proxy` is
//! false the forwarded header is ignored outright, and when it is true the
//! value is only ever as trustworthy as the proxy that appended it. Nothing
//! here promotes a header to authenticated input.

use std::net::{IpAddr, SocketAddr};

/// Resolve the client address from the forwarded header and the socket peer.
///
/// `forwarded_for` is the raw `X-Forwarded-For` value, if the request carried
/// one. `peer_ip` is the address of the TCP peer, absent only for exotic
/// transports and test fixtures.
///
/// With `trust_proxy` off, only `peer_ip` is consulted: anyone with a direct
/// path to the port would otherwise pick their own bucket key and their own
/// audit trail by sending a header.
///
/// With `trust_proxy` on, the rightmost usable `X-Forwarded-For` token wins,
/// falling back to `peer_ip` when the header is absent or holds nothing that
/// parses as an address.
///
/// The result is an [`IpAddr`], never a socket address: the port is part of a
/// connection, not of a client, and a port that reaches a rate-limit bucket
/// key gives every reconnect a fresh allowance.
#[must_use]
pub fn resolve_client_ip(
    forwarded_for: Option<&str>,
    peer_ip: Option<IpAddr>,
    trust_proxy: bool,
) -> Option<IpAddr> {
    if trust_proxy {
        if let Some(ip) = forwarded_for.and_then(trusted_forwarded_ip) {
            return Some(ip);
        }
    }
    peer_ip
}

/// The rightmost non-empty `X-Forwarded-For` token that parses as an address.
///
/// Empty tokens are skipped rather than treated as a terminator, so a
/// trailing comma does not silently discard the hop that wrote it.
fn trusted_forwarded_ip(header: &str) -> Option<IpAddr> {
    header
        .rsplit(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .find_map(parse_client_ip)
}

/// Parse one forwarded token into a bare address.
///
/// Accepts the four spellings a proxy may emit: `192.0.2.1`,
/// `192.0.2.1:443`, `2001:db8::1`, and `[2001:db8::1]:443`. A token that is
/// none of these -- RFC 7239's `unknown` and `_obfuscated` forms, or plain
/// junk from a caller -- yields `None`, because a bucket keyed on it would be
/// a bucket keyed on something that is not a client.
fn parse_client_ip(token: &str) -> Option<IpAddr> {
    if let Ok(ip) = token.parse::<IpAddr>() {
        return Some(ip);
    }
    if let Ok(addr) = token.parse::<SocketAddr>() {
        return Some(addr.ip());
    }
    // A bracketed IPv6 literal with no port: `[2001:db8::1]`.
    token
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .and_then(|inner| inner.parse::<IpAddr>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn without_trust_proxy_only_the_peer_is_consulted() {
        assert_eq!(
            resolve_client_ip(Some("203.0.113.77"), Some(ip("192.0.2.10")), false),
            Some(ip("192.0.2.10"))
        );
        // No peer and an untrusted header: nothing to resolve. The caller
        // decides what an unresolved client is called.
        assert_eq!(resolve_client_ip(Some("203.0.113.77"), None, false), None);
    }

    #[test]
    fn takes_the_rightmost_entry_not_the_leftmost() {
        // The caller prepended a claim; the trusted hop appended what it saw.
        assert_eq!(
            resolve_client_ip(Some("1.2.3.4, 203.0.113.7"), Some(ip("10.0.0.1")), true),
            Some(ip("203.0.113.7"))
        );
    }

    #[test]
    fn strips_a_port_so_one_client_keeps_one_key() {
        // Two connections from one client differ only in the ephemeral source
        // port. Keeping the port would give each its own rate-limit bucket.
        let a = resolve_client_ip(Some("192.0.2.43:40001"), None, true);
        let b = resolve_client_ip(Some("192.0.2.43:40002"), None, true);
        assert_eq!(a, Some(ip("192.0.2.43")));
        assert_eq!(a, b);
    }

    #[test]
    fn accepts_every_ipv6_spelling_a_proxy_may_emit() {
        assert_eq!(
            resolve_client_ip(Some("2001:db8::1"), None, true),
            Some(ip("2001:db8::1"))
        );
        assert_eq!(
            resolve_client_ip(Some("[2001:db8::1]:8080"), None, true),
            Some(ip("2001:db8::1"))
        );
        assert_eq!(
            resolve_client_ip(Some("[2001:db8::1]"), None, true),
            Some(ip("2001:db8::1"))
        );
    }

    #[test]
    fn an_unparseable_token_falls_back_to_the_peer() {
        // RFC 7239 permits `unknown` and `_obfuscated` identifiers, and a
        // caller can send arbitrary bytes. None of them is an address.
        for junk in ["unknown", "_hidden", "not-an-ip", "", "   "] {
            assert_eq!(
                resolve_client_ip(Some(junk), Some(ip("192.0.2.10")), true),
                Some(ip("192.0.2.10")),
                "token {junk:?} must not become a client address"
            );
        }
    }

    #[test]
    fn a_trailing_comma_does_not_discard_the_trusted_hop() {
        // `rsplit(',').next()` would yield the empty tail here and fall
        // through to the peer, silently ignoring the proxy's own entry.
        assert_eq!(
            resolve_client_ip(Some("1.2.3.4, 203.0.113.7, "), Some(ip("10.0.0.1")), true),
            Some(ip("203.0.113.7"))
        );
    }

    #[test]
    fn a_forged_rightmost_token_is_skipped_only_as_far_as_the_next_parseable_one() {
        // Documents a real limit rather than a guarantee: with one trusted
        // hop the rightmost token is proxy-authored, so this fallback is
        // unreachable. Reached only when the deployment's assumptions are
        // already broken, it degrades to the next-closest claim rather than
        // to no limiting at all.
        assert_eq!(
            resolve_client_ip(Some("203.0.113.7, junk"), Some(ip("10.0.0.1")), true),
            Some(ip("203.0.113.7"))
        );
    }
}
