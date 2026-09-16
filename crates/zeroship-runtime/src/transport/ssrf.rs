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
//! Both layers share `is_blocked_ip` as the single blocklist source of truth,
//! and both consult it through `is_blocked_ip_under_dev`, which is where the
//! ONE narrowing lives: a process that stated the dev relaxation reaches
//! loopback and nothing else. `transport::egress::filter_answer` - the
//! `node:net` / `node:tls` / outbound WebSocket floor - reads the same
//! predicate, so all three agree on what dev opens.
//!
//! DEV-NESS IS A STATED INPUT, NOT AN ENVIRONMENT READ. `dev_mode_enabled`
//! answers only what `set_dev_mode` stored; `dev_mode_from_process_env` is a
//! separate function that no gate calls, and its one caller is `zeroship serve`
//! in `crates/zeroship-cli/src/main.rs`.
//!
//! Note: cyper does **not** follow HTTP redirects automatically. The
//! native fetch in `crate::fetch_native` does redirect handling and
//! re-validates each hop; this module exposes the building blocks it
//! consumes.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};

use cyper::resolve::Resolve;
use futures::Stream;
use futures::stream;
use http::Uri;

/// Maximum response body size: 10 MB.
pub const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;

/// The process-level dev-relaxation cell. Written ONLY by [`set_dev_mode`];
/// a process in which nothing called it holds `false`.
static DEV_MODE: AtomicBool = AtomicBool::new(false);

/// Development-only network relaxation gate.
///
/// FALSE UNLESS A CALLER STATED OTHERWISE. There is no environment arm: this
/// returns exactly what [`set_dev_mode`] last stored, and a process in which
/// nothing stated a mode runs the whole guard.
///
/// An environment variable is not a construction boundary: a
/// `ZEROSHIP_DEV=1` exported into a production `zeroship-worker` must not
/// turn SSRF validation off for every `fetch` that worker makes, which is
/// why no gate reads one. The worker applies the same standard to its
/// database backend (`crates/zeroship-worker/src/main.rs`,
/// `worker_rejects_db_url`).
///
/// The surviving read is [`dev_mode_from_process_env`], whose one caller is
/// `cmd_serve` in `crates/zeroship-cli/src/main.rs` - the binary that IS the
/// dev tier by identity.
#[must_use]
pub fn dev_mode_enabled() -> bool {
    DEV_MODE.load(Ordering::Relaxed)
}

/// Read `ZEROSHIP_DEV` from the process environment, at the construction
/// boundary of a binary that is the dev tier BY IDENTITY.
///
/// NO GATE CALLS THIS. [`validate_url`], [`SsrfResolver`] and
/// `egress::filter_answer` read [`dev_mode_enabled`], which answers only what
/// [`set_dev_mode`] stated. The environment therefore reaches a security
/// decision exactly once - when a dev-tier binary chooses to pass this value
/// to the setter - instead of on every gate evaluation in every process.
///
/// The one caller is `cmd_serve` in `crates/zeroship-cli/src/main.rs`: the
/// single-process runtime `@zeroship/vite-plugin` spawns as
/// `zeroship serve <entry>` with `ZEROSHIP_DEV=1`
/// (`packages/vite-plugin/src/dev-server.ts`). `zeroship-worker` does not call
/// it, which is what makes a leaked `ZEROSHIP_DEV=1` inert there.
#[must_use]
pub fn dev_mode_from_process_env() -> bool {
    dev_mode_from_env_value(
        zeroship_core::declared_env!(dev, "ZEROSHIP_DEV", crate::RuntimeConsumer).as_deref(),
    )
}

/// Which spellings of `ZEROSHIP_DEV` mean dev: exactly `1`, and nothing else.
///
/// Split out from [`dev_mode_from_process_env`] so the question is answerable
/// without an environment at all.
fn dev_mode_from_env_value(raw: Option<&str>) -> bool {
    zeroship_core::config::env_is_exact(raw, "1")
}

/// State the dev relaxation for the rest of the process.
///
/// THE ONLY WRITER of the cell [`dev_mode_enabled`] reads, and therefore the
/// only way anything can relax the SSRF floor. A caller - an embedding
/// process, or a test - SAYS which mode it wants; nothing infers one.
///
/// The alternative a test would otherwise reach for is
/// `std::env::set_var("ZEROSHIP_DEV", ..)`, which mutates the process-global
    /// environment underneath every other thread and races libc `getenv`
    /// (undefined behaviour, and `unsafe` in Rust 2024). The environment does
    /// not reach the gate, so that spelling would not even work.
///
/// A test that toggles the mode still needs its own mutual exclusion: this
/// cell is process-wide, so two tests disagreeing about the mode still
/// disagree. The unit tests in this file resolve that by running each stated
/// mode alone, in a child process.
pub fn set_dev_mode(enabled: bool) {
    DEV_MODE.store(enabled, Ordering::Relaxed);
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

/// True for the loopback forms a developer machine actually reaches:
/// `127.0.0.0/8`, `::1`, and the v4-mapped spelling `::ffff:127.0.0.0/8` a
/// dual-stack `getaddrinfo` can hand back for `localhost`.
///
/// DELIBERATELY NARROWER than "an address that would end up at 127.0.0.1".
/// The NAT64 (`64:ff9b::/96`) and IPv4-compatible (`::a.b.c.d`) embeddings
/// [`is_blocked_ip`] also unwraps are NOT loopback here: reaching 127.0.0.1
/// through either needs a translating gateway no `pnpm dev` box has, so
/// admitting them would widen the relaxation for no workflow.
#[must_use]
pub fn is_loopback_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// The SSRF floor, evaluated under a stated dev relaxation.
///
/// ONE definition of what dev opens - loopback, and nothing else - shared by
/// every place the floor is consulted: [`validate_url`] (the string fast
/// path), [`SsrfResolver`] (the DNS layer) and `egress::filter_answer` (the
/// `node:net` / `node:tls` / outbound WebSocket connect path). They diverged
/// once already: dev turned the first two off wholesale while the third kept
/// its rule phases, so "what dev allows" had three different answers and only
/// one of them was written down.
#[must_use]
pub fn is_blocked_ip_under_dev(addr: IpAddr, dev_loopback: bool) -> bool {
    is_blocked_ip(addr) && !(dev_loopback && is_loopback_ip(addr))
}

/// Validate the URL to prevent SSRF attacks (string-level fast path).
///
/// Blocks non-HTTP(S) schemes and literal private/loopback/link-local/etc IPs
/// embedded in the URL. A second layer of protection runs at DNS resolution
/// time via [`SsrfResolver`] — domain names that resolve into blocked ranges
/// are rejected there, since this function cannot see them.
///
/// Under a stated dev relaxation ([`set_dev_mode`]) LOOPBACK ONLY is allowed,
/// so the Vite plugin's `ModuleRunner` can fetch modules from the Vite dev
/// server. Every other blocked range stays refused in dev.
pub fn validate_url(url: &str) -> Result<(), String> {
    validate_url_under(url, dev_mode_enabled())
}

/// [`validate_url`], with the dev relaxation supplied rather than read.
///
/// Exists so the matrix in the tests is a pure function of its arguments. The
/// cell is process-wide: a unit test that flipped it to cover the dev arm
/// would be deciding the result of every other test in the binary, and which
/// result depended on which test ran first.
fn validate_url_under(url: &str, dev_loopback: bool) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL: {e}"))?;

    // `fetch` is the only caller. WebSocket URLs are decided by
    // `transport::egress::evaluate` on the handshake path, not here, so
    // accepting `ws`/`wss` would only mean `fetch("ws://...")` getting past
    // the scheme check to fail further down.
    //
    // Note this function has never checked PORTS, for any scheme. Ports are
    // decided by the egress rule set, which carries one, and are not a thing
    // the string-level fast path can usefully bound for `fetch` - which is the
    // ungated egress by design.
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => return Err(format!("Blocked URL scheme: {scheme}")),
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?
        .to_lowercase();

    // Block localhost. This is the ONE name the dev relaxation admits, and it
    // is the name the Vite dev server is addressed by: the ModuleRunner
    // transport fetches `${ZEROSHIP_VITE_ORIGIN}/__zeroship_fetch`
    // (`packages/vite-plugin/src/dev-bootstrap/transport.ts`,
    // `MODULE_FETCH_PATH`) and the plugin sets that origin to
    // `http://localhost:<vitePort>` (`packages/vite-plugin/src/dev-server.ts`).
    // The runtime is spawned as a CHILD of the Vite process, so the dev
    // server is always on this host and loopback always reaches it.
    if host == "localhost" {
        if dev_loopback {
            return Ok(());
        }
        return Err("Blocked request to localhost".to_string());
    }

    // Try to parse as IP address (handles both bare IPs and bracket-stripped IPv6)
    let ip: Option<IpAddr> = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok();

    if let Some(addr) = ip
        && is_blocked_ip_under_dev(addr, dev_loopback)
    {
        return Err(format!("Blocked request to private/internal IP: {addr}"));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// SsrfResolver — DNS resolver that filters out private/loopback addresses
// ---------------------------------------------------------------------------

/// Custom cyper resolver. Performs the same work as the default (std DNS
/// lookup) then strips every `IpAddr` that [`is_blocked_ip_under_dev`]
/// rejects. If the remaining set is empty, returns an error so cyper fails the
/// connection.
///
/// This closes the SSRF hole where a public hostname resolves to an RFC1918
/// address — the caller sees a generic connect error instead of reaching the
/// internal service.
///
/// IT MUST BE INSTALLED IN BOTH MODES. A client built with NO custom resolver
/// at all would let `fetch("http://metadata.internal.example/")` connect when
/// the name resolves into link-local space, and narrowing [`validate_url`]
/// alone would leave that intact while reading as closed: the string fast
/// path cannot see a hostname's addresses, which is the whole reason this
/// layer exists.
pub struct SsrfResolver {
    /// Whether loopback survives the filter. Everything else
    /// [`is_blocked_ip`] rejects is stripped either way.
    dev_loopback: bool,
}

impl SsrfResolver {
    /// The production resolver: every blocked range is stripped.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            dev_loopback: false,
        }
    }

    /// The resolver for a process that stated the dev relaxation: loopback
    /// survives so `pnpm dev` can reach the Vite dev server by name, and every
    /// other blocked range is still stripped.
    #[must_use]
    pub const fn dev_loopback() -> Self {
        Self { dev_loopback: true }
    }

    /// Whether this resolver lets loopback through. The one observable
    /// difference between the two constructors, and what
    /// `transport::client` pins its selection with.
    #[must_use]
    pub const fn admits_loopback(&self) -> bool {
        self.dev_loopback
    }
}

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
            .filter(|ip| !is_blocked_ip_under_dev(*ip, self.dev_loopback))
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
    /// Dev mode is a cached process-level cell, so no test can ask the
    /// environment twice; the spelling question is asked here, and the
    /// integration tests ask the separate question of whether an off mode
    /// still refuses.
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

    // NO TEST THAT THE ORDINARY `cargo test` RUN EXECUTES IN THIS PROCESS
    // CALLS `set_dev_mode`.
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
    // The rows below that DO need a stated mode are `#[ignore]`, so the
    // ordinary run never executes them, and each has a spawner that runs it -
    // alone, under `--exact` - in a CHILD copy of this same test binary. A
    // child runs exactly one test, so the cell it writes is observed by
    // nothing else and no neighbour can have written it first. That is the
    // whole ordering argument: in the parent process the cell is never
    // written, and in each child it is written by the only test running.
    //
    // `set_dev_mode` is additionally exercised in both directions by the
    // integration binaries, which serialise on a per-binary `ENV_LOCK`:
    // `tests/node_net.rs` (`SettingsGuard::set(false, ..)` versus
    // `set(true, ..)`) and `tests/node_net_security.rs`
    // (`dev_mode_off_does_not_relax_ssrf` versus the `dev_mode: true` rows).

    /// Run ONE test of this same binary in a CHILD process; return
    /// `(the child passed, its combined output)`.
    ///
    /// `--exact` plus a single name is what keeps the child's process-wide
    /// state private to that one test. Every caller MUST also assert on
    /// `1 passed`: a name matching nothing runs zero tests and libtest still
    /// exits 0, so a typo in the path would otherwise read as a green.
    fn run_one_test_in_child(
        name: &str,
        ignored: bool,
        apply: impl FnOnce(&mut std::process::Command),
    ) -> (bool, String) {
        let exe = std::env::current_exe().expect("current_exe");
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--exact")
            .arg(name)
            .arg("--nocapture")
            .arg("--test-threads=1");
        if ignored {
            cmd.arg("--ignored");
        }
        apply(&mut cmd);
        let out = cmd.output().expect("spawn a child copy of this test binary");
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text)
    }

    /// The addresses this guard exists for, asserted about the SHIPPED entry
    /// point rather than about `is_blocked_ip` in isolation: a blocklist test
    /// cannot see a caller that never consults the blocklist.
    ///
    /// This is also the body the environment regression test re-runs in a
    /// child process, so keep it free of process-wide state.
    #[test]
    fn validate_url_refuses_metadata_and_rfc1918() {
        for url in [
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://100.64.0.1/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
        ] {
            assert!(validate_url(url).is_err(), "the SSRF guard must refuse {url}");
        }
    }

    /// THE REGRESSION TEST. `ZEROSHIP_DEV=1` in the process environment must
    /// not disable SSRF validation.
    ///
    /// The variable is set on a CHILD process, never with `std::env::set_var`:
    /// that mutates the environment underneath every other thread and races
    /// libc `getenv` (undefined behaviour, and `unsafe` in Rust 2024).
    #[test]
    fn zeroship_dev_in_the_environment_cannot_disable_the_guard() {
        let (passed, out) = run_one_test_in_child(
            "transport::ssrf::tests::validate_url_refuses_metadata_and_rfc1918",
            false,
            |cmd| {
                cmd.env("ZEROSHIP_DEV", "1");
            },
        );
        assert!(
            passed,
            "ZEROSHIP_DEV=1 in the environment disabled the SSRF guard:\n{out}"
        );
        assert!(
            out.contains("1 passed"),
            "the child ran no test, so it proved nothing:\n{out}"
        );
    }

    /// `ZEROSHIP_DEV` reaches the dev relaxation through NO path at all.
    ///
    /// Sharper than the row above, which would also pass if the guard read the
    /// variable and then happened to refuse the addresses asked about: this
    /// asserts the cell itself is off in a process whose environment carries
    /// the affirmative spelling.
    #[test]
    #[ignore = "asserts a process-wide cell; its spawner runs it alone in a child"]
    fn dev_relaxation_ignores_the_environment() {
        assert!(
            !dev_mode_enabled(),
            "ZEROSHIP_DEV=1 in the environment must not resolve the dev relaxation"
        );
        assert!(validate_url("http://127.0.0.1:5173/x").is_err());
        assert!(validate_url("http://localhost:5173/x").is_err());
    }

    #[test]
    fn dev_relaxation_ignores_the_environment_in_an_isolated_process() {
        let (passed, out) = run_one_test_in_child(
            "transport::ssrf::tests::dev_relaxation_ignores_the_environment",
            true,
            |cmd| {
                cmd.env("ZEROSHIP_DEV", "1");
            },
        );
        assert!(passed, "the environment still reaches the dev cell:\n{out}");
        assert!(
            out.contains("1 passed"),
            "the child ran no test, so it proved nothing:\n{out}"
        );
    }

    /// THE NARROWING. With the relaxation stated through the typed setter,
    /// loopback is reachable - that is the whole of its stated purpose, the
    /// Vite dev server - and every other blocked range is still refused.
    #[test]
    #[ignore = "states a process-wide cell; its spawner runs it alone in a child"]
    fn dev_relaxation_is_loopback_only() {
        set_dev_mode(true);
        for url in [
            "http://localhost:5173/@vite/client",
            "http://127.0.0.1:5173/x",
            "http://127.0.0.2:5173/x",
            "http://[::1]:5173/x",
            "https://example.com/x",
        ] {
            assert!(
                validate_url(url).is_ok(),
                "the dev relaxation must still reach {url}"
            );
        }
        for url in [
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://100.64.0.1/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://0.0.0.0/",
        ] {
            assert!(
                validate_url(url).is_err(),
                "the dev relaxation must not open {url}"
            );
        }
        // The scheme check sits above the relaxation and stays above it.
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("wss://example.com/").is_err());
    }

    #[test]
    fn dev_relaxation_is_loopback_only_in_an_isolated_process() {
        let (passed, out) = run_one_test_in_child(
            "transport::ssrf::tests::dev_relaxation_is_loopback_only",
            true,
            |cmd| {
                cmd.env_remove("ZEROSHIP_DEV");
            },
        );
        assert!(passed, "the dev relaxation is wider than loopback:\n{out}");
        assert!(
            out.contains("1 passed"),
            "the child ran no test, so it proved nothing:\n{out}"
        );
    }

    /// FAIL CLOSED. A process in which nothing stated a mode runs the full
    /// guard, loopback included.
    #[test]
    #[ignore = "asserts a process-wide cell; its spawner runs it alone in a child"]
    fn absent_dev_relaxation_fails_closed() {
        assert!(
            !dev_mode_enabled(),
            "an unstated dev relaxation must read as off"
        );
        assert!(validate_url("http://127.0.0.1:5173/x").is_err());
        assert!(validate_url("http://localhost:5173/x").is_err());
        assert!(validate_url("http://169.254.169.254/").is_err());
    }

    #[test]
    fn absent_dev_relaxation_fails_closed_in_an_isolated_process() {
        let (passed, out) = run_one_test_in_child(
            "transport::ssrf::tests::absent_dev_relaxation_fails_closed",
            true,
            |cmd| {
                cmd.env_remove("ZEROSHIP_DEV");
            },
        );
        assert!(
            passed,
            "an unstated dev relaxation did not fail closed:\n{out}"
        );
        assert!(
            out.contains("1 passed"),
            "the child ran no test, so it proved nothing:\n{out}"
        );
    }

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

    /// What the dev relaxation opens and what it leaves shut, asked of the
    /// pure form so the matrix needs no process-wide state and cannot be
    /// decided by which test ran first.
    ///
    /// The `false` column is the CONTROL: it differs from the `true` column in
    /// exactly one variable, so a green here says the loopback rows are about
    /// the relaxation and not about the address being reachable anyway.
    #[test]
    fn validate_url_under_dev_opens_loopback_and_nothing_else() {
        // (url, allowed under dev, allowed under production)
        let rows = [
            ("http://localhost:5173/@vite/client", true, false),
            ("http://127.0.0.1:5173/x", true, false),
            ("http://127.0.0.2:5173/x", true, false),
            ("http://[::1]:5173/x", true, false),
            ("http://[::ffff:127.0.0.1]:5173/x", true, false),
            ("https://example.com/x", true, true),
            ("http://169.254.169.254/latest/meta-data/", false, false),
            ("http://10.0.0.1/", false, false),
            ("http://172.16.0.1/", false, false),
            ("http://192.168.1.1/", false, false),
            ("http://100.64.0.1/", false, false),
            ("http://0.0.0.0/", false, false),
            ("http://[fd00::1]/", false, false),
            ("http://[fe80::1]/", false, false),
            ("http://[64:ff9b::7f00:1]/", false, false),
            ("file:///etc/passwd", false, false),
            ("wss://example.com/", false, false),
        ];
        for (url, dev_ok, prod_ok) in rows {
            assert_eq!(
                validate_url_under(url, true).is_ok(),
                dev_ok,
                "dev relaxation verdict for {url}"
            );
            assert_eq!(
                validate_url_under(url, false).is_ok(),
                prod_ok,
                "production verdict for {url}"
            );
        }
    }

    #[test]
    fn is_loopback_ip_is_narrower_than_is_blocked_ip() {
        for loopback in ["127.0.0.1", "127.0.0.2", "::1", "::ffff:127.0.0.1"] {
            let addr: IpAddr = loopback.parse().unwrap();
            assert!(is_loopback_ip(addr), "{loopback} is loopback");
            assert!(is_blocked_ip(addr), "{loopback} is still on the blocklist");
            assert!(
                !is_blocked_ip_under_dev(addr, true),
                "{loopback} must be reachable under the dev relaxation"
            );
            assert!(
                is_blocked_ip_under_dev(addr, false),
                "{loopback} must stay blocked in production"
            );
        }
        // Blocked, and NOT opened by the relaxation. `64:ff9b::7f00:1` and
        // `::7f00:1` reach 127.0.0.1 only through a translating gateway, so
        // they are deliberately outside what dev admits.
        for blocked in [
            "169.254.169.254",
            "10.0.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "fd00::1",
            "fe80::1",
            "64:ff9b::7f00:1",
            "::7f00:1",
        ] {
            let addr: IpAddr = blocked.parse().unwrap();
            assert!(!is_loopback_ip(addr), "{blocked} is not loopback");
            assert!(
                is_blocked_ip_under_dev(addr, true),
                "the dev relaxation must not open {blocked}"
            );
        }
        // Public space is unaffected in both directions.
        let public: IpAddr = "93.184.216.34".parse().unwrap();
        assert!(!is_blocked_ip_under_dev(public, true));
        assert!(!is_blocked_ip_under_dev(public, false));
    }

    /// THE DNS LAYER, which the string fast path cannot stand in for: a
    /// hostname's addresses are invisible to `validate_url`, so a dev client
    /// built with no resolver at all would let
    /// `fetch("http://metadata.internal.example/")` connect even with the
    /// string path narrowed.
    ///
    /// Both arms use IP literals, which `to_socket_addrs` resolves without a
    /// nameserver, so the row is deterministic on a machine with no DNS.
    #[test]
    fn ssrf_resolver_admits_loopback_only_under_the_dev_relaxation() {
        use futures::StreamExt;

        let strict = SsrfResolver::strict();
        let dev = SsrfResolver::dev_loopback();
        let loopback: Uri = "http://127.0.0.1:9/".parse().unwrap();
        let metadata: Uri = "http://169.254.169.254:80/".parse().unwrap();

        futures::executor::block_on(async {
            assert!(
                strict.resolve(&loopback).await.is_err(),
                "production must strip loopback"
            );
            let admitted: Vec<IpAddr> = dev
                .resolve(&loopback)
                .await
                .expect("the dev relaxation must admit loopback")
                .collect()
                .await;
            assert_eq!(admitted, vec!["127.0.0.1".parse::<IpAddr>().unwrap()]);

            assert!(
                strict.resolve(&metadata).await.is_err(),
                "production must strip the metadata address"
            );
            assert!(
                dev.resolve(&metadata).await.is_err(),
                "the dev relaxation must NOT open the metadata address at the \
                 DNS layer either"
            );
        });
    }
}
