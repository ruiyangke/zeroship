// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

use socket2::TcpKeepalive;
use std::time::Duration;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct KeepaliveConfig {
    pub idle: Duration,
    pub interval: Option<Duration>,
    pub retries: Option<u32>,
}

/// Translate the configured values into the socket options to actually set.
///
/// A ZERO IS NOT A VALUE, IT IS THE ABSENCE OF ONE. PostgreSQL documents all
/// three of `keepalives_idle`, `keepalives_interval` and `keepalives_count`
/// with the same sentence - "A value of zero uses the system default" - so a
/// zero must leave the corresponding option alone. Handing it to `setsockopt`
/// instead is not a near-miss: Linux answers `TCP_KEEPIDLE`, `TCP_KEEPINTVL`
/// and `TCP_KEEPCNT` of 0 with EINVAL, and `connect_socket` turns that into a
/// failed connection. `keepalives_count=0` in a DSN did exactly that, because
/// `Config::keepalives_count` parses into `u32` and so has no "> 0" guard of
/// the kind the idle and interval parameters carry.
///
/// Dropping the zero HERE rather than at parse time is deliberate: this is the
/// single point every entry path passes through. The builder API
/// (`Config::keepalives_idle(Duration::ZERO)` and its two peers) never sees a
/// DSN, so a parse-time guard would leave it broken.
///
/// `TcpKeepalive::new()` leaves each field `None`, and `set_tcp_keepalive`
/// enables `SO_KEEPALIVE` before applying only the fields that are set - so an
/// all-zero config still turns keepalives ON with the system's own timings,
/// which is what the documentation promises.
///
/// THIS DELIBERATELY DIVERGES FROM libpq, so do not "restore parity" by
/// deleting the guards. Measured against PostgreSQL 16 on 2026-08-23, `psql`
/// refuses all three zero values with `setsockopt(TCP_KEEPIDLE /
/// TCP_KEEPINTVL / TCP_KEEPCNT) failed: Invalid argument` - it clamps a
/// negative to zero and then sets it unconditionally, contradicting the
/// documentation it ships. The documented contract is what a caller can read,
/// so it is the one followed here.
impl From<&KeepaliveConfig> for TcpKeepalive {
    fn from(keepalive_config: &KeepaliveConfig) -> Self {
        let mut tcp_keepalive = Self::new();

        if !keepalive_config.idle.is_zero() {
            tcp_keepalive = tcp_keepalive.with_time(keepalive_config.idle);
        }

        #[cfg(not(any(
            target_os = "aix",
            target_os = "redox",
            target_os = "solaris",
            target_os = "openbsd"
        )))]
        if let Some(interval) = keepalive_config.interval.filter(|i| !i.is_zero()) {
            tcp_keepalive = tcp_keepalive.with_interval(interval);
        }

        #[cfg(not(any(
            target_os = "aix",
            target_os = "redox",
            target_os = "solaris",
            target_os = "windows",
            target_os = "openbsd"
        )))]
        if let Some(retries) = keepalive_config.retries.filter(|r| *r != 0) {
            tcp_keepalive = tcp_keepalive.with_retries(retries);
        }

        tcp_keepalive
    }
}
