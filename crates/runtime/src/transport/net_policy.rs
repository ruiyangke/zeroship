//! Outbound raw-TCP policy for `node:net`.
//!
//! The policy is a trusted-Rust construction property: app JavaScript cannot
//! request or widen it. `Denied` is the default and makes `node:net`
//! unresolvable. `Allowlist` narrows connect targets to operator-reviewed
//! host:port entries; `Trusted` skips host matching but still goes through
//! SSRF, socket caps, and egress caps.
//!
//! The allowlist is deliberately scoped as a compromised-dependency
//! blast-radius control, not a malicious-creator exfiltration control. Runtime
//! construction must receive reviewed entries from the operator/control-plane
//! path; creator code never self-declares them. Broad wildcards and wildcards
//! fronting shared infrastructure are rejected at construction time. The
//! malicious-creator controls are egress attribution, spend enforcement, and
//! hard egress ceilings.

use std::sync::atomic::{AtomicU32, Ordering};

pub use zeroship_core::net_policy::{HostPort, ReviewedAllowlist};

/// Process-wide fallback cap. Operators can lower it via
/// `ZEROSHIP_NET_GLOBAL_MAX_SOCKETS`; tests use that hook to exercise
/// the global-cap branch without opening thousands of fds.
const DEFAULT_GLOBAL_MAX_SOCKETS: u32 = 4096;

static GLOBAL_ACTIVE_SOCKETS: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetPolicy {
    Denied,
    Allowlist {
        entries: ReviewedAllowlist,
        max_sockets: u32,
        egress_ceiling_bytes: u64,
    },
    Trusted {
        max_sockets: u32,
        egress_ceiling_bytes: u64,
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
            Self::Allowlist { max_sockets, .. } | Self::Trusted { max_sockets, .. } => *max_sockets,
        }
    }

    pub fn egress_ceiling_bytes(&self) -> Option<u64> {
        match self {
            Self::Allowlist {
                egress_ceiling_bytes,
                ..
            }
            | Self::Trusted {
                egress_ceiling_bytes,
                ..
            } => Some(*egress_ceiling_bytes),
            Self::Denied => None,
        }
    }

    pub fn allows_host_port(&self, host: &str, port: u16) -> bool {
        match self {
            Self::Denied => false,
            Self::Trusted { .. } => true,
            Self::Allowlist { entries, .. } => entries.iter().any(|e| e.matches(host, port)),
        }
    }

    pub fn allowlist(
        entries: Vec<HostPort>,
        max_sockets: u32,
        egress_ceiling_bytes: u64,
    ) -> Result<Self, String> {
        Ok(Self::Allowlist {
            entries: ReviewedAllowlist::operator_reviewed(entries)?,
            max_sockets,
            egress_ceiling_bytes,
        })
    }

    pub fn trusted(max_sockets: u32, egress_ceiling_bytes: u64) -> Self {
        Self::Trusted {
            max_sockets,
            egress_ceiling_bytes,
        }
    }
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
        assert!(HostPort::new("*.db.example.com", 5432).matches("a.db.example.com", 5432));
        assert!(!HostPort::new("*.db.example.com", 5432).matches("db.example.com", 5432));
        assert!(!HostPort::new("*.db.example.com", 5432).matches("a.db.example.com", 5433));
    }

    #[test]
    fn allowlist_rejects_bare_and_fronting_wildcards() {
        assert!(HostPort::try_new("*", 443).is_err());
        assert!(HostPort::try_new("*.com", 443).is_err());
        assert!(HostPort::try_new("*.workers.dev", 443).is_err());
        assert!(HostPort::try_new("*.neon.tech", 5432).is_err());
        assert!(HostPort::try_new("db.neon.tech", 5432).is_ok());
    }
}
