//! Trusted first-party OAuth client resolution.
//!
//! First-party OAuth clients (the platform's own **console**) skip the
//! first-party consent prompt: a consent prompt for the platform's own
//! first-party surface is meaningless. The trusted set is resolved from the shared
//! `[auth].trusted_oauth_clients` file overlay; when the key is absent the
//! compiled default is **empty** (fail-closed). This lives in `zeroship-core`
//! so both the control plane and any other service that needs to consult the
//! trusted set share one byte-identical implementation.
//!
//! ## Deployment MUST configure the console client id (fail-closed default)
//!
//! There is intentionally **no** compiled-in trusted client. The console's
//! OAuth `client_id` is host-derived and not knowable here (core has no
//! console host), so the deployment MUST set `[auth].trusted_oauth_clients` to
//! the console's `oac_<base62>` client id — i.e.
//! `client_id_for_app(console_app_id(console_host))`, which
//! `zeroship-control --bootstrap-console` prints at boot. The **control plane**
//! resolves this set (via [`resolve_trusted_oauth_clients`] /
//! [`is_trusted_client_id`]) to mark the first-party console client
//! `skip_consent=true`, so the immersive framed `/oauth2/auth → /login →
//! /consent` login dance AUTO-ACCEPTS identity consent — a consent prompt for
//! the platform's own console is meaningless. Until the overlay names the
//! console client, no client is trusted and the framed console login would
//! render a consent screen — the secure default. Creator apps are **never**
//! trusted here (spec §5.2): their per-app clients always run the consent
//! prompt.
//!
//! The browser-enforced `frame-ancestors` allowlist on the auth login routes is
//! the separate anti-clickjacking gate (it replaced the deleted gateway
//! credential-oracle first-party gate); this set is purely about skipping
//! first-party consent for the console.

use std::collections::HashSet;

use crate::config::AuthSection;

/// Compiled default for first-party OAuth clients that skip first-party consent.
///
/// **Empty by design (fail-closed).** No client is trusted unless the shared
/// `[auth].trusted_oauth_clients` file overlay names it. The console's
/// `oac_<base62>` client id is host-derived and unknowable in core, so the
/// deployment must list it explicitly (see the module doc). Returning a
/// non-empty default here would silently trust a hard-coded client id and is a
/// security footgun.
#[must_use]
pub fn default_trusted_oauth_clients() -> HashSet<String> {
    HashSet::new()
}

/// Resolve trusted OAuth clients from the optional shared auth config.
///
/// `None` (key absent) uses the compiled default set (empty, fail-closed);
/// `Some(vec)` is exactly that set, where an empty vec also means "no trusted
/// clients".
#[must_use]
pub fn resolve_trusted_oauth_clients(auth: &AuthSection) -> HashSet<String> {
    match &auth.trusted_oauth_clients {
        None => default_trusted_oauth_clients(),
        Some(clients) => clients.iter().cloned().collect(),
    }
}

/// Return whether `client_id` is present in a trusted-client set.
#[must_use]
pub fn is_trusted_client_id(trusted_oauth_clients: &HashSet<String>, client_id: &str) -> bool {
    trusted_oauth_clients.contains(client_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty_fail_closed() {
        // The compiled default trusts NO client (fail-closed). The console's
        // host-derived oac_ client id must be named in
        // `[auth].trusted_oauth_clients` by the deployment instead.
        assert!(
            default_trusted_oauth_clients().is_empty(),
            "default trusted set must be empty (fail-closed)"
        );
    }

    #[test]
    fn absent_overlay_trusts_nothing() {
        let auth = AuthSection {
            trusted_oauth_clients: None,
            ..AuthSection::default()
        };
        let trusted = resolve_trusted_oauth_clients(&auth);
        assert!(trusted.is_empty());
        // The retired hard-coded builder id is NOT trusted by default anymore.
        assert!(!is_trusted_client_id(&trusted, "zeroship-builder"));
    }

    #[test]
    fn overlay_set_is_exactly_the_configured_clients() {
        let auth = AuthSection {
            trusted_oauth_clients: Some(vec!["oac_console".to_string()]),
            ..AuthSection::default()
        };
        let trusted = resolve_trusted_oauth_clients(&auth);
        assert!(is_trusted_client_id(&trusted, "oac_console"));
        assert!(!is_trusted_client_id(&trusted, "oac_other"));
    }
}
