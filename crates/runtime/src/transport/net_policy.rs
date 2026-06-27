//! Outbound raw-TCP policy for `node:net`.
//!
//! The policy is a host-construction property: app JavaScript cannot
//! request or widen it. `Denied` is the default and makes `node:net`
//! unresolvable. `Allowlist` narrows connect targets to reviewed
//! host:port entries; `Trusted` skips host matching but still goes
//! through SSRF and socket caps.

use std::sync::atomic::{AtomicU32, Ordering};

/// Process-wide fallback cap. Operators can lower it via
/// `ZEROSHIP_NET_GLOBAL_MAX_SOCKETS`; tests use that hook to exercise
/// the global-cap branch without opening thousands of fds.
const DEFAULT_GLOBAL_MAX_SOCKETS: u32 = 4096;

static GLOBAL_ACTIVE_SOCKETS: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetPolicy {
    Denied,
    Allowlist {
        entries: Vec<HostPort>,
        max_sockets: u32,
        egress_ceiling_bytes: u64,
    },
    Trusted {
        max_sockets: u32,
    },
}

impl Default for NetPolicy {
    fn default() -> Self {
        Self::Denied
    }
}

impl NetPolicy {
    pub fn module_allowed(&self) -> bool {
        !matches!(self, Self::Denied)
    }

    pub fn max_sockets(&self) -> u32 {
        match self {
            Self::Denied => 0,
            Self::Allowlist { max_sockets, .. } | Self::Trusted { max_sockets } => *max_sockets,
        }
    }

    pub fn egress_ceiling_bytes(&self) -> Option<u64> {
        match self {
            Self::Allowlist {
                egress_ceiling_bytes,
                ..
            } => Some(*egress_ceiling_bytes),
            Self::Denied | Self::Trusted { .. } => None,
        }
    }

    pub fn allows_host_port(&self, host: &str, port: u16) -> bool {
        match self {
            Self::Denied => false,
            Self::Trusted { .. } => true,
            Self::Allowlist { entries, .. } => entries.iter().any(|e| e.matches(host, port)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPort {
    host: String,
    port: u16,
}

impl HostPort {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: normalize_host(&host.into()),
            port,
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn matches(&self, host: &str, port: u16) -> bool {
        if self.port != port {
            return false;
        }
        let host = normalize_host(host);
        if let Some(suffix) = self.host.strip_prefix("*.") {
            return host.len() > suffix.len()
                && host.ends_with(suffix)
                && host.as_bytes()[host.len() - suffix.len() - 1] == b'.';
        }
        self.host == host
    }
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

pub(crate) fn try_acquire_global_socket() -> Result<(), String> {
    let cap = configured_global_max_sockets();
    let mut cur = GLOBAL_ACTIVE_SOCKETS.load(Ordering::Relaxed);
    loop {
        if cur >= cap {
            return Err(format!(
                "process-wide node:net socket cap exceeded ({cap})"
            ));
        }
        match GLOBAL_ACTIVE_SOCKETS.compare_exchange_weak(
            cur,
            cur + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(()),
            Err(next) => cur = next,
        }
    }
}

pub(crate) fn release_global_socket() {
    let mut cur = GLOBAL_ACTIVE_SOCKETS.load(Ordering::Relaxed);
    while cur > 0 {
        match GLOBAL_ACTIVE_SOCKETS.compare_exchange_weak(
            cur,
            cur - 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(next) => cur = next,
        }
    }
}

fn configured_global_max_sockets() -> u32 {
    std::env::var("ZEROSHIP_NET_GLOBAL_MAX_SOCKETS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_GLOBAL_MAX_SOCKETS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostport_exact_and_wildcard_match() {
        assert!(HostPort::new("DB.Example.COM.", 5432).matches("db.example.com", 5432));
        assert!(HostPort::new("*.neon.tech", 5432).matches("a.neon.tech", 5432));
        assert!(!HostPort::new("*.neon.tech", 5432).matches("neon.tech", 5432));
        assert!(!HostPort::new("*.neon.tech", 5432).matches("a.neon.tech", 5433));
    }
}
