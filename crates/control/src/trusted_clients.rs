//! First-party platform OAuth clients that get skip_consent=true.
//!
//! Adding an entry requires a code review (this file is in git) + redeploy.
//! When server-config-unification ships, this list moves to
//! `ops/zeroship.toml` under `[auth].trusted_oauth_clients`.

pub const TRUSTED_OAUTH_CLIENTS: &[&str] = &[
    "zeroship-builder",
    // Future: "zeroship-dashboard", "zeroship-cli" - add as those binaries ship
];

#[must_use]
pub fn is_trusted(client_id: &str) -> bool {
    TRUSTED_OAUTH_CLIENTS.contains(&client_id)
}
