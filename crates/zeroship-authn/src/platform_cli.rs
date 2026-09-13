//! First-seen platform CLI grant materialization.
//!
//! Bearer verification falls back to the platform CLI's default grant set only
//! while a principal has no identity marker. A service with the control role
//! must turn that temporary fallback into operator-visible rows on first sight.

use std::error::Error as StdError;
use std::fmt;

use compio_postgres::GenericClient;
use zeroship_core::device_grant::{PLATFORM_CLI_ISSUABLE_SCOPES, PLATFORM_PROVIDER};
use zeroship_core::UserId;

/// Whether this call wrote the first-seen marker and default grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformCliGrantMaterialization {
    Materialized,
    AlreadyMaterialized,
}

impl PlatformCliGrantMaterialization {
    /// Whether another request won the first-seen race.
    ///
    /// A caller that authorized against fallback defaults must reload live
    /// entitlement before proceeding in this case. The winning request may
    /// already have materialized the marker and an operator may already have
    /// narrowed its grants.
    #[must_use]
    pub const fn requires_entitlement_refresh(self) -> bool {
        matches!(self, Self::AlreadyMaterialized)
    }
}

#[cfg(test)]
mod tests {
    use super::PlatformCliGrantMaterialization;

    #[test]
    fn only_a_raced_existing_marker_requires_live_entitlement_refresh() {
        assert!(!PlatformCliGrantMaterialization::Materialized.requires_entitlement_refresh());
        assert!(PlatformCliGrantMaterialization::AlreadyMaterialized.requires_entitlement_refresh());
    }
}

/// Failure to atomically materialize a platform CLI principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformCliGrantError(String);

impl fmt::Display for PlatformCliGrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl StdError for PlatformCliGrantError {}

fn describe(error: &compio_postgres::Error) -> String {
    error.as_db_error().map_or_else(
        || error.to_string(),
        |db| format!("{error}: {} ({})", db.message(), db.code().code()),
    )
}

/// Atomically insert a platform identity marker and the exact CLI defaults.
///
/// THIS FUNCTION IS THE SUCCESSOR TO A DELETED ONE, and the reason it looks the
/// way it does is that history. Control's `identity_bridge::provision_or_link`
/// was just-in-time provisioning kept deliberately OFF the bearer authz read
/// path, on the device-approval write path instead. It ended up on both, and
/// that was FORCED rather than chosen: once `zeroship login` moved to the OP's
/// own device grant, control stopped being on the login path at all, so a
/// platform-native creator's first bearer request became the only moment
/// control still saw them. The choice stopped being "approval path vs authz
/// path" and became "authz path vs nowhere". `provision_or_link` was then left
/// with test callers only and is deleted.
///
/// What the old separation protected is what the marker guard below buys back:
/// the write is attempted once per principal and every later request settles
/// into a pure read, so being on the hot path costs a read rather than a write.
///
/// The identity marker, not the current grant count, is the first-seen test.
/// Once the marker exists this statement writes nothing, so an operator may
/// remove any or all grants without a later bearer request recreating them.
/// That is a REAL capability rather than an accident of the implementation, and
/// `an_operator_deleting_a_grant_row_narrows_the_next_cli_request`
/// (`crates/zeroship-control/tests/authz_guard_oauth_test.rs`) asserts exactly
/// it, including that a request must not re-seed what an operator deleted.
/// The writable CTE makes marker and grant creation one PostgreSQL statement:
/// either all default rows and the marker commit, or none of them do.
///
/// # The consequence, which is easy to miss and bit a change on 2026-09-08
///
/// Seeding is once-per-principal, so WIDENING
/// [`PLATFORM_CLI_ISSUABLE_SCOPES`] does not reach a principal who has already
/// been linked. Their stored rows keep the older set, and
/// `platform_cli_entitlement` intersects the token's scopes against those rows,
/// so the new scope is stripped at request time. Nothing in the platform
/// re-seeds - no admin API, no CLI verb, no sweep - so for an existing
/// principal the widening never takes effect at all.
///
/// This is not hypothetical: `env:read`, `env:write` and `secrets:write` were
/// added to that constant so `zeroship var` and `zeroship secret` would stop
/// answering 403, and for already-linked principals they did not.
///
/// It is left alone rather than "fixed" by reconciling on every call, because
/// reconciling would silently delete the operator capability above - "revoked"
/// and "not yet seeded" are the SAME state under a marker-guarded seed, so no
/// reconciliation can tell them apart. Making both work needs revocation to
/// become explicit (a per-grant revoked marker this statement reads and
/// refuses to overwrite), and then re-seeding is safe. Until that exists, a
/// ceiling change reaches new principals only, and an operator widening an
/// existing one does it the same way they narrow one: by hand.
///
/// # Errors
///
/// Returns [`PlatformCliGrantError`] if the principal does not exist or if
/// PostgreSQL cannot execute the materialization statement.
#[allow(clippy::future_not_send)]
pub async fn materialize_default_grants(
    pg: &(impl GenericClient + ?Sized),
    principal_id: &UserId,
) -> Result<PlatformCliGrantMaterialization, PlatformCliGrantError> {
    let provider_subject = principal_id.as_str();
    let grants: Vec<String> = PLATFORM_CLI_ISSUABLE_SCOPES
        .iter()
        .map(|grant| (*grant).to_owned())
        .collect();
    let row = pg
        .query_one(
            "WITH target AS ( \
                 SELECT id, email::text AS email \
                 FROM zeroship.users \
                 WHERE id = $1 \
             ), inserted_link AS ( \
                 INSERT INTO zeroship.identity_links \
                     (principal_id, provider, provider_subject, email) \
                 SELECT id, $2, $3, email \
                 FROM target \
                 WHERE NOT EXISTS ( \
                     SELECT 1 FROM zeroship.identity_links \
                     WHERE principal_id = $1 \
                 ) \
                 ON CONFLICT (provider, provider_subject) DO NOTHING \
                 RETURNING principal_id \
             ), inserted_grants AS ( \
                 INSERT INTO zeroship.principal_grants (principal_id, grant_name) \
                 SELECT inserted_link.principal_id, grant_name \
                 FROM inserted_link \
                 CROSS JOIN unnest($4::TEXT[]) AS grant_name \
                 ON CONFLICT (principal_id, grant_name) DO NOTHING \
                 RETURNING grant_name \
             ) \
             SELECT EXISTS (SELECT 1 FROM target) AS principal_exists, \
                    EXISTS (SELECT 1 FROM inserted_link) AS materialized, \
                    EXISTS (SELECT 1 FROM inserted_grants) AS grants_written",
            &[
                &principal_id.as_str(),
                &PLATFORM_PROVIDER,
                &provider_subject,
                &grants,
            ],
        )
        .await
        .map_err(|error| {
            PlatformCliGrantError(format!(
                "platform CLI grant materialization for {}: {}",
                principal_id.as_str(),
                describe(&error)
            ))
        })?;

    if !row.get::<_, bool>("principal_exists") {
        return Err(PlatformCliGrantError(format!(
            "platform CLI grant materialization principal does not exist: {}",
            principal_id.as_str()
        )));
    }

    Ok(if row.get::<_, bool>("materialized") {
        PlatformCliGrantMaterialization::Materialized
    } else {
        PlatformCliGrantMaterialization::AlreadyMaterialized
    })
}
