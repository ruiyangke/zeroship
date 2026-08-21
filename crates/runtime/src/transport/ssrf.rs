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
//! callback and the legacy fetch executor. Both were deleted alongside
//! the native fetch cutover; `globalThis.fetch` is now the native
//! callback installed by `fetch_native::install_fetch_global`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU8, Ordering};

use cyper::resolve::Resolve;
use futures::Stream;
use futures::stream;
use http::Uri;

/// Maximum response body size: 10 MB.
pub const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;

/// The process-level dev-relaxation cell. `0` = not yet resolved, `1` = off,
/// `2` = on. Written once by [`dev_mode_enabled`] on the first read, or
/// explicitly at any time by [`set_dev_mode`].
static DEV_MODE: AtomicU8 = AtomicU8::new(DEV_MODE_UNRESOLVED);

const DEV_MODE_UNRESOLVED: u8 = 0;
const DEV_MODE_OFF: u8 = 1;
const DEV_MODE_ON: u8 = 2;

/// Development-only network relaxation gate.
///
/// PRECEDENCE. An explicit [`set_dev_mode`] always wins. Otherwise the first
/// call resolves the mode ONCE from the environment - the dev runtime's
/// parent sets `ZEROSHIP_DEV=1` on the child (`sdks/vite-plugin/src/
/// constants.ts`, `tests/e2e_durable_workflows.sh`) - and every later call
/// returns that same answer. Any other value, including `0` or the empty
/// string, is non-dev and fails closed.
///
/// The parent-to-child environment contract is unchanged; what the cell
/// removes is the need for anything INSIDE this process to mutate the
/// environment to change the answer. `std::env::set_var` races concurrent
/// libc `getenv` (undefined behaviour, which is why Rust 2024 marks it
/// `unsafe`), so a caller that wants a mode states it with [`set_dev_mode`]
/// instead.
#[must_use]
pub fn dev_mode_enabled() -> bool {
    match DEV_MODE.load(Ordering::Relaxed) {
        DEV_MODE_ON => true,
        DEV_MODE_OFF => false,
        _ => {
            let enabled = dev_mode_from_env_value(
                zeroship_core::declared_env!(dev, "ZEROSHIP_DEV", crate::RuntimeConsumer)
                    .as_deref(),
            );
            DEV_MODE.store(
                if enabled { DEV_MODE_ON } else { DEV_MODE_OFF },
                Ordering::Relaxed,
            );
            enabled
        }
    }
}

/// Which spellings of `ZEROSHIP_DEV` mean dev: exactly `1`, and nothing else.
///
/// Split out from [`dev_mode_enabled`] so the question is answerable without
/// an environment at all - the cached cell above resolves this once per
/// process, so a test cannot ask it twice by any other route.
fn dev_mode_from_env_value(raw: Option<&str>) -> bool {
    zeroship_core::config::env_is_exact(raw, "1")
}

/// State the dev relaxation explicitly, overriding `ZEROSHIP_DEV` for the rest
/// of the process.
///
/// PRECEDENCE. This wins over the environment, whether or not
/// [`dev_mode_enabled`] has already resolved it, and it wins permanently -
/// there is no arm that re-reads the environment afterwards.
///
/// It exists so a caller - an embedding process, or a test - can SAY which
/// mode it wants. The alternative a test would otherwise reach for is
/// `std::env::set_var("ZEROSHIP_DEV", ..)`, which mutates the process-global
/// environment underneath every other thread and races libc `getenv`. A test
/// that toggles the mode still needs its own mutual exclusion: this cell is
/// process-wide, so two tests disagreeing about the mode still disagree.
pub fn set_dev_mode(enabled: bool) {
    DEV_MODE.store(
        if enabled { DEV_MODE_ON } else { DEV_MODE_OFF },
        Ordering::Relaxed,
    );
}

fn ipv4_from_segments(high: u16, low: u16) -> Ipv4Addr {
    Ipv4Addr::new(
        (high >> 8) as u8,
        high as u8,
        (low >> 8) as u8,
        low as u8,
    )
}

fn nat64_embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    if s[..6] == [0x0064, 0xff9b, 0, 0, 0, 0] {
        Some(ipv4_from_segments(s[6], s[7]))
    } else {
        None
    }
}

fn ipv4_compatible_embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    if s[..6] == [0, 0, 0, 0, 0, 0] && (s[6] != 0 || s[7] != 0) {
        Some(ipv4_from_segments(s[6], s[7]))
    } else {
        None
    }
}

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
            if let Some(v4) = nat64_embedded_ipv4(v6)
                && is_blocked_ip(IpAddr::V4(v4))
            {
                return true;
            }
            if let Some(v4) = ipv4_compatible_embedded_ipv4(v6)
                && is_blocked_ip(IpAddr::V4(v4))
            {
                return true;
            }
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
/// In dev mode (see [`dev_mode_enabled`]), localhost/loopback is allowed so
/// the Vite plugin's ModuleRunner can fetch modules from the Vite dev server.
pub fn validate_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL: {e}"))?;

    // `fetch` is the only caller. `ws`/`wss` used to be accepted here because
    // the WebSocket handshake shared this function; it now goes through
    // `transport::egress::evaluate` instead, so leaving them accepted would
    // only mean `fetch("ws://...")` getting past the scheme check to fail
    // further down.
    //
    // Note this function has never checked PORTS, for any scheme. Ports are
    // decided by the egress rule set, which carries one, and are not a thing
    // the string-level fast path can usefully bound for `fetch` - which is the
    // ungated egress by design.
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => return Err(format!("Blocked URL scheme: {scheme}")),
    }

    // In dev mode, skip host/IP validation (allows localhost fetch to Vite).
    if dev_mode_enabled() {
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

    /// `ZEROSHIP_DEV` relaxes the guard only when it is exactly `1`. `0`, the
    /// empty string, an unset variable and anything truthy-looking are all
    /// non-dev, and each must fail CLOSED.
    ///
    /// This lived in `tests/node_net_security.rs`, which looped a live
    /// `set_var("ZEROSHIP_DEV", ..)` over two non-affirmative spellings and
    /// asserted the connect was still refused. Dev mode is a cached
    /// process-level cell now, so no test can ask the environment twice; the
    /// spelling question is asked here, and the integration tests ask the
    /// separate question of whether an off mode still refuses.
    #[test]
    fn only_exactly_one_is_dev_mode() {
        assert!(dev_mode_from_env_value(Some("1")));
        assert!(!dev_mode_from_env_value(Some("0")));
        assert!(!dev_mode_from_env_value(Some("")));
        assert!(!dev_mode_from_env_value(Some("true")));
        assert!(!dev_mode_from_env_value(Some("yes")));
        assert!(!dev_mode_from_env_value(Some("11")));
        assert!(!dev_mode_from_env_value(None));
    }

    // NO TEST IN THIS MODULE CALLS `set_dev_mode`, deliberately.
    //
    // The cell is process-wide, and `dev_mode_enabled` is read by
    // `validate_url` here AND by `egress::filter_answer`, whose floor tests
    // (`ssrf_floor_beats_a_granted_name`, `ssrf_floor_beats_a_granted_range`
    // and every other `evaluate` row) assert refusals that only hold while
    // dev mode is off. cargo runs the lib tests on several threads, so a unit
    // test here that flipped the cell would intermittently run those under a
    // mode they never asked for, and the failure would surface in a module
    // that changed nothing.
    //
    // `set_dev_mode` is exercised in both directions by the integration
    // binaries instead, which already serialise on a per-binary `ENV_LOCK`:
    // `tests/node_net.rs` (`SettingsGuard::set(false, ..)` versus
    // `set(true, ..)`) and `tests/node_net_security.rs`
    // (`dev_mode_off_does_not_relax_ssrf` versus the `dev_mode: true` rows).
    // What is checked HERE is the part with no cell in it: which spellings of
    // the environment value mean dev.

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
    fn blocks_nat64_embedded_blocked_v4() {
        assert!(is_blocked_ip(
            "64:ff9b::a9fe:a9fe".parse::<Ipv6Addr>().unwrap().into()
        ));
        assert!(is_blocked_ip(
            "64:ff9b::0a00:0001".parse::<Ipv6Addr>().unwrap().into()
        ));
        assert!(is_blocked_ip(
            "64:ff9b::7f00:0001".parse::<Ipv6Addr>().unwrap().into()
        ));
    }

    #[test]
    fn blocks_ipv4_compatible_embedded_blocked_v4() {
        assert!(is_blocked_ip("::a9fe:a9fe".parse::<Ipv6Addr>().unwrap().into()));
        assert!(is_blocked_ip("::0a00:0001".parse::<Ipv6Addr>().unwrap().into()));
        assert!(is_blocked_ip("::7f00:0001".parse::<Ipv6Addr>().unwrap().into()));
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
        // `fetch` is the only caller now. A WebSocket URL is decided by the
        // egress rule set on the handshake path, not here, so accepting one
        // here would be permissiveness with no consumer.
        assert!(validate_url("wss://example.com/").is_err());
    }

    #[test]
    fn validate_url_allows_public_http() {
        assert!(validate_url("https://example.com/x").is_ok());
    }
}
