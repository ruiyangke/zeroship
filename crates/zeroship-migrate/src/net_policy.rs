//! Shared raw-TCP allowlist validation.
//!
//! Vendored byte-identically from `zeroship_core::net_policy` (extraction Phase B)
//! so the migrate engine can be embedded as a lean, self-contained library without
//! a normal-graph dependency on `zeroship-core`. The reviewed host/port checks are
//! security-relevant: the recorder-sandbox / MySQL JS-driver isolate's outbound
//! `NetPolicy` names these types, so their parse/validate behaviour must match the
//! original exactly. The adapter crate (`zeroship-migrate-runtime`) reconstructs a
//! `zeroship_runtime::NetPolicy` (whose entries are the `zeroship-core` copy) from
//! these vendored entries at the injection edge.

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

    pub fn try_new_with_frontable_suffixes(
        host: impl Into<String>,
        port: u16,
        catalog_suffixes: &[String],
        catalog_available: bool,
    ) -> Result<Self, String> {
        let host = normalize_host(&host.into());
        let entry = Self { host, port };
        entry.validate_reviewed_with_catalog(catalog_suffixes, catalog_available)?;
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
        self.validate_reviewed_with_catalog(&[], true)
    }

    fn validate_reviewed_with_catalog(
        &self,
        catalog_suffixes: &[String],
        catalog_available: bool,
    ) -> Result<(), String> {
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
            validate_wildcard_suffix(suffix, catalog_suffixes, catalog_available)?;
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

/// Normalize an operator-editable frontable wildcard suffix catalog.
///
/// Suffixes use the same DNS-name normalization as allowlist hosts, are sorted
/// for stable wire output, and are deduplicated after normalization.
pub fn normalize_frontable_suffixes(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(raw.len());
    for suffix in raw {
        let suffix = normalize_host(suffix);
        if suffix.is_empty() {
            return Err("suffix must not be empty".to_string());
        }
        if suffix.contains('*') {
            return Err(format!("suffix {suffix:?} must not contain '*'"));
        }
        if !suffix.contains('.') {
            return Err(format!("suffix {suffix:?} must contain at least two labels"));
        }
        if suffix.parse::<std::net::IpAddr>().is_ok() {
            return Err(format!("suffix {suffix:?} must be a DNS name, not an IP"));
        }
        if !out.contains(&suffix) {
            out.push(suffix);
        }
    }
    out.sort();
    Ok(out)
}

fn validate_wildcard_suffix(
    suffix: &str,
    catalog_suffixes: &[String],
    catalog_available: bool,
) -> Result<(), String> {
    if suffix.is_empty() || !suffix.contains('.') {
        return Err("wildcard allowlist suffix must contain at least two labels".to_string());
    }
    if suffix.parse::<std::net::IpAddr>().is_ok() {
        return Err("wildcard allowlist suffix must be a DNS name, not an IP".to_string());
    }
    if !catalog_available {
        return Err(
            "wildcard allowlist suffix review catalog unavailable; refusing wildcard grant"
                .to_string(),
        );
    }
    if FRONTABLE_WILDCARD_SUFFIXES
        .iter()
        .any(|blocked| suffix_matches(suffix, blocked))
        || catalog_suffixes
            .iter()
            .any(|blocked| suffix_matches(suffix, blocked))
    {
        return Err(format!(
            "wildcard allowlist suffix '{suffix}' fronts shared infrastructure"
        ));
    }
    Ok(())
}

fn suffix_matches(suffix: &str, blocked: &str) -> bool {
    let blocked = normalize_host(blocked);
    suffix == blocked || suffix.ends_with(&format!(".{blocked}"))
}

/// Compiled-in backstop for the operator-editable frontable-suffix catalog.
/// Exact host entries remain possible for reviewed destinations; broad
/// wildcards are refused.
pub const FRONTABLE_WILDCARD_SUFFIXES: &[&str] = &[
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostport_matches_exact_and_wildcard_hosts() {
        assert!(HostPort::new("DB.Example.COM.", 5432).matches("db.example.com", 5432));
        assert!(HostPort::new("*.db.example.com", 5432).matches("a.db.example.com", 5432));
        assert!(!HostPort::new("*.db.example.com", 5432).matches("db.example.com", 5432));
        assert!(!HostPort::new("*.db.example.com", 5432).matches("a.db.example.com", 5433));
    }

    #[test]
    fn operator_review_rejects_broad_or_frontable_wildcards() {
        assert!(HostPort::try_new("*", 443).is_err());
        assert!(HostPort::try_new("*.com", 443).is_err());
        assert!(HostPort::try_new("*.workers.dev", 443).is_err());
        assert!(HostPort::try_new("*.neon.tech", 5432).is_err());
        assert!(HostPort::try_new("db.neon.tech", 5432).is_ok());
    }

    #[test]
    fn operator_review_uses_catalog_suffixes_and_fails_closed_when_missing() {
        let catalog = vec!["db.example.com".to_string()];
        assert!(
            HostPort::try_new_with_frontable_suffixes("*.db.example.com", 5432, &catalog, true)
                .is_err()
        );
        assert!(
            HostPort::try_new_with_frontable_suffixes("*.tenant.example", 5432, &[], false)
                .is_err(),
            "wildcard grants must fail closed if the operator catalog is unavailable"
        );
        assert!(
            HostPort::try_new_with_frontable_suffixes("db.tenant.example", 5432, &[], false)
                .is_ok(),
            "exact host grants do not depend on wildcard suffix catalog availability"
        );
    }
}
