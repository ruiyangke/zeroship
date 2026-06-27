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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewedAllowlist {
    entries: Vec<HostPort>,
}

impl ReviewedAllowlist {
    /// Construct an operator/control-plane reviewed allowlist.
    ///
    /// This is intentionally not a JS/user-code surface. It validates every
    /// entry before a runtime ever sees it, rejecting broad wildcards and
    /// wildcard entries that front shared infrastructure.
    pub fn operator_reviewed(entries: Vec<HostPort>) -> Result<Self, String> {
        for entry in &entries {
            entry.validate_reviewed()?;
        }
        Ok(Self { entries })
    }

    pub fn iter(&self) -> impl Iterator<Item = &HostPort> {
        self.entries.iter()
    }

    pub fn as_slice(&self) -> &[HostPort] {
        &self.entries
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPort {
    host: String,
    port: u16,
}

impl HostPort {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self::try_new(host, port).expect("invalid node:net allowlist entry")
    }

    pub fn try_new(host: impl Into<String>, port: u16) -> Result<Self, String> {
        let host = normalize_host(&host.into());
        let entry = Self { host, port };
        entry.validate_reviewed()?;
        Ok(entry)
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

    fn validate_reviewed(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("allowlist port must be between 1 and 65535".to_string());
        }
        if self.host.is_empty() {
            return Err("allowlist host must not be empty".to_string());
        }
        if self.host == "*" {
            return Err("bare '*' is not a valid node:net allowlist host".to_string());
        }
        let star_count = self.host.bytes().filter(|b| *b == b'*').count();
        if star_count > 0 && !self.host.starts_with("*.") {
            return Err(format!(
                "wildcard allowlist host '{}' must use the '*.example.com' form",
                self.host
            ));
        }
        if let Some(suffix) = self.host.strip_prefix("*.") {
            validate_wildcard_suffix(suffix)?;
        }
        Ok(())
    }
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn validate_wildcard_suffix(suffix: &str) -> Result<(), String> {
    if suffix.is_empty() || !suffix.contains('.') {
        return Err("wildcard allowlist suffix must contain at least two labels".to_string());
    }
    if suffix.parse::<std::net::IpAddr>().is_ok() {
        return Err("wildcard allowlist suffix must be a DNS name, not an IP".to_string());
    }
    if FRONTABLE_WILDCARD_SUFFIXES
        .iter()
        .any(|blocked| suffix == *blocked || suffix.ends_with(&format!(".{blocked}")))
    {
        return Err(format!(
            "wildcard allowlist suffix '{suffix}' fronts shared infrastructure"
        ));
    }
    Ok(())
}

/// Operator-curated suffixes where a wildcard would authorize arbitrary
/// third-party tenants behind shared infrastructure. Exact host entries remain
/// possible for reviewed destinations; broad wildcards are refused.
const FRONTABLE_WILDCARD_SUFFIXES: &[&str] = &[
    "workers.dev",
    "pages.dev",
    "vercel.app",
    "netlify.app",
    "herokuapp.com",
    "fly.dev",
    "railway.app",
    "render.com",
    "onrender.com",
    "neon.tech",
    "supabase.co",
    "amazonaws.com",
    "cloudfront.net",
];

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
