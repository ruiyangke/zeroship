//! Trusted first-party OAuth client resolution.
//!
//! First-party OAuth clients (e.g. the builder) skip Hydra consent. The
//! trusted set is resolved from the shared `[auth].trusted_oauth_clients`
//! file overlay, falling back to a compiled default when the key is absent.
//! This lives in `zeroship-core` so both the control plane and any other
//! service that needs to consult the trusted set share one byte-identical
//! implementation.

use std::collections::HashSet;

use crate::config::AuthSection;

/// Client ID of the first-party `zeroship-builder` OAuth client.
///
/// Lives here (rather than control's `bootstrap_builder`) so the compiled
/// default trusted set can reference it without a control → core dependency
/// cycle; control re-exports it from `bootstrap_builder`.
pub const BUILDER_CLIENT_ID: &str = "zeroship-builder";

/// Compiled default for first-party OAuth clients that skip Hydra consent.
///
/// The shared `[auth].trusted_oauth_clients` file overlay replaces this list
/// when present. Keeping the builder client as the no-file default gives local
/// dev a sensible zero-config default.
#[must_use]
pub fn default_trusted_oauth_clients() -> HashSet<String> {
    [BUILDER_CLIENT_ID.to_string()].into()
}

/// Resolve trusted OAuth clients from the optional shared auth config.
///
/// `None` (key absent) uses the compiled default set; `Some(vec)` is exactly
/// that set, where an empty vec means "no trusted clients".
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
