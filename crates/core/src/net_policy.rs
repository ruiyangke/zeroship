//! Shared raw-TCP allowlist validation.
//!
//! Runtime owns enforcement, but the control plane needs the exact same
//! host/port checks at the AUTHORING boundary — which is a creator-facing API
//! (`/api/apps/{id}/net-grants`), not an operator's console. Keep this module
//! V8-free so control can reject bad grants without depending on the runtime
//! crate.
//!
//! Because the author is the creator, these checks are the shape bound on
//! creator input, not a guardrail on an operator's typing. They constrain
//! WILDCARDS only: an exact `host:port` is accepted for any public host. What
//! keeps the resulting reach narrow is elsewhere — deny-by-default per app,
//! the plan's `max_grants`/`max_sockets`/`egress_ceiling_bytes` caps, and the
//! runtime's SSRF check on the connect itself.

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
        let valid_wildcard = self
            .host
            .strip_prefix("*.")
            .is_some_and(|suffix| !suffix.is_empty());
        if star_count > 0 && (star_count != 1 || !valid_wildcard) {
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
    if is_registry_level_suffix(suffix) {
        return Err(format!(
            "wildcard allowlist suffix '{suffix}' is a registry-level suffix; \
             name a registrable domain instead"
        ));
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

/// Whether `suffix` is a registry-level suffix such as `co.uk` — one under
/// which anyone can register a domain, so a `*.co.uk` wildcard reaches every
/// registrant rather than one organisation.
///
/// This is a structural test, not a public-suffix list: exactly two labels, a
/// two-letter ccTLD, and a generic second level. It therefore catches the
/// `co.uk` / `com.br` / `ac.jp` family without carrying a PSL dependency, and
/// deliberately does not catch anything else. `*.example.co.uk` (three labels)
/// and `*.example.io` (non-generic second level) stay legal.
fn is_registry_level_suffix(suffix: &str) -> bool {
    const GENERIC_SECOND_LEVELS: &[&str] = &[
        "ac", "biz", "co", "com", "edu", "go", "gov", "gr", "info", "int", "mil", "ne", "net",
        "nom", "or", "org", "sch", "web",
    ];
    let labels: Vec<&str> = suffix.split('.').collect();
    labels.len() == 2
        && labels[1].len() == 2
        && labels[1].bytes().all(|b| b.is_ascii_alphabetic())
        && GENERIC_SECOND_LEVELS.contains(&labels[0])
}

fn suffix_matches(suffix: &str, blocked: &str) -> bool {
    let blocked = normalize_host(blocked);
    suffix == blocked || suffix.ends_with(&format!(".{blocked}"))
}

/// Compiled-in backstop for the operator-editable frontable-suffix catalog.
/// Exact host entries remain possible for reviewed destinations; broad
/// wildcards are refused.
///
/// The membership criterion is a single question: **can an arbitrary third
/// party obtain a hostname under this suffix?** If yes, a wildcard over it
/// reaches other tenants' deployments rather than the grantee's own, which is
/// the reach the allowlist exists to deny. Suffixes an organisation controls
/// end to end do not belong here; name them exactly instead.
///
/// This list is a backstop, not a public-suffix list, and does not pretend to
/// be exhaustive — operators extend it through `net_policy_catalog`. The
/// registry-level shapes (`co.uk`, `com.br`) are caught structurally by
/// [`is_registry_level_suffix`] rather than by enumeration.
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
    // Added when creator self-service made wildcard grants a creator input
    // rather than an operator's typing: each is a suffix under which anyone
    // can obtain a hostname.
    "appspot.com",
    "azureedge.net",
    "azurewebsites.net",
    "cloudflarestorage.com",
    "cloudfunctions.net",
    "core.windows.net",
    "deno.dev",
    "digitaloceanspaces.com",
    "firebaseapp.com",
    "github.io",
    "githubusercontent.com",
    "gitlab.io",
    "glitch.me",
    "googleapis.com",
    "ngrok.app",
    "ngrok.io",
    "pythonanywhere.com",
    "repl.co",
    "replit.dev",
    "run.app",
    "surge.sh",
    "trycloudflare.com",
    "web.app",
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
    fn operator_review_host_validation_accepts_literal_and_single_wildcard() {
        assert!(HostPort::try_new("example.com", 443).is_ok());
        assert!(HostPort::try_new("*.example.com", 443).is_ok());
    }

    #[test]
    fn operator_review_host_validation_rejects_bare_wildcard_suffix() {
        assert!(
            HostPort::try_new("*.", 443).is_err(),
            "bare wildcard suffix must be rejected"
        );
    }

    #[test]
    fn operator_review_host_validation_rejects_additional_wildcards() {
        assert!(
            HostPort::try_new("*.*.example.com", 443).is_err(),
            "additional wildcards must be rejected"
        );
    }

    /// A wildcard suffix that is itself a registry-level suffix (`co.uk`)
    /// grants every registrable domain under it. The check that catches this is
    /// structural, not a public-suffix list: two labels, a two-letter ccTLD, and
    /// a generic second level. `*.example.co.uk` is three labels and stays
    /// legal; `*.example.io` keeps a non-generic second level and stays legal.
    ///
    /// What this does NOT check: suffixes outside that shape. `*.example.com`
    /// remains as broad as the registrable domain the creator names, by design.
    #[test]
    fn operator_review_rejects_registry_level_wildcard_suffixes() {
        assert!(HostPort::try_new("*.co.uk", 443).is_err());
        assert!(HostPort::try_new("*.com.br", 443).is_err());
        assert!(HostPort::try_new("*.ac.jp", 443).is_err());
        assert!(HostPort::try_new("*.example.co.uk", 443).is_ok());
        assert!(HostPort::try_new("*.example.io", 443).is_ok());
        assert!(HostPort::try_new("*.example.com", 443).is_ok());
    }

    /// The backstop names suffixes under which an arbitrary third party can
    /// obtain a hostname, so a wildcard over one reaches other tenants'
    /// deployments rather than the creator's own.
    #[test]
    fn operator_review_rejects_multi_tenant_hosting_suffixes() {
        for host in [
            "*.github.io",
            "*.appspot.com",
            "*.azurewebsites.net",
            "*.blob.core.windows.net",
            "*.run.app",
            "*.deno.dev",
            "*.ngrok.io",
            "*.trycloudflare.com",
        ] {
            assert!(
                HostPort::try_new(host, 443).is_err(),
                "{host} fronts shared multi-tenant infrastructure"
            );
        }
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
