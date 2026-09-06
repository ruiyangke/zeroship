//! First-seen platform CLI grant materialization.
//!
//! Bearer verification falls back to the platform CLI's default grant set only
//! while a principal has no identity marker. A service with the control role
//! must turn that temporary fallback into operator-visible rows on first sight.

use std::error::Error as StdError;
use std::fmt;

use compio_postgres::GenericClient;
use uuid::Uuid;
use zeroship_core::device_grant::{PLATFORM_CLI_ISSUABLE_SCOPES, PLATFORM_PROVIDER};

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
        assert!(
            PlatformCliGrantMaterialization::AlreadyMaterialized
                .requires_entitlement_refresh()
        );
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
/// The writable CTE makes marker and grant creation one PostgreSQL statement:
/// either all default rows and the marker commit, or none of them do.
///
/// # Errors
///
/// Returns [`PlatformCliGrantError`] if the principal does not exist or if
/// PostgreSQL cannot execute the materialization statement.
#[allow(clippy::future_not_send)]
pub async fn materialize_default_grants(
    pg: &(impl GenericClient + ?Sized),
    principal_id: Uuid,
) -> Result<PlatformCliGrantMaterialization, PlatformCliGrantError> {
    let provider_subject = principal_id.to_string();
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
                &principal_id,
                &PLATFORM_PROVIDER,
                &provider_subject,
                &grants,
            ],
        )
        .await
        .map_err(|error| {
            PlatformCliGrantError(format!(
                "platform CLI grant materialization for {principal_id}: {}",
                describe(&error)
            ))
        })?;

    if !row.get::<_, bool>("principal_exists") {
        return Err(PlatformCliGrantError(format!(
            "platform CLI grant materialization principal does not exist: {principal_id}"
        )));
    }

    Ok(if row.get::<_, bool>("materialized") {
        PlatformCliGrantMaterialization::Materialized
    } else {
        PlatformCliGrantMaterialization::AlreadyMaterialized
    })
}
