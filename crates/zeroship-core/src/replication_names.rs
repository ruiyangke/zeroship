//! Stable names shared by migration-time and worker-time CDC code.

use sha2::{Digest, Sha256};

/// Prefix reserved for zeroship logical-replication objects.
pub const OBJECT_PREFIX: &str = "__zs_";

/// The ONE publication on a datastore, owned by the relay.
///
/// It is a constant and not a derivation, and that is the design rather than a
/// simplification. A publication is an object of one `PostgreSQL` database, and
/// a datastore is reached through one; so "one publication per datastore" has
/// exactly one name to compose and nothing to compose it from. Naming it after
/// an app or a database would say the opposite of what this object is.
///
/// **Its membership is not a tenant filter.** The publication deliberately
/// spans every database on the datastore, so a consumer that read namespaces
/// out of it would admit every co-tenant's relations. What separates tenants in
/// a stream is the decoder's comparison against the schema of the ONE database
/// a subscriber is bound to.
///
/// **A database edits only its own member entries, under the datastore
/// publication mutex.** `ALTER PUBLICATION ... SET TABLE` names the whole
/// object, so one database issuing it would drop every other database's tables
/// out of the shared stream without an error anywhere.
pub const DATASTORE_PUBLICATION: &str = "__zs_pub_datastore";

/// Prefix reserved for the relay's own replication slots.
///
/// A slot is per (app, relay) and stays app-keyed while the publication does
/// not: slots replicate decode work rather than partitioning it, and the relay
/// drops by this prefix, so nothing but a relay slot may carry it.
pub const SLOT_PREFIX: &str = "__zs_relay_";

/// Why a caller-controlled application id cannot name a CDC object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReplicationNameError {
    #[error("app id must not be empty")]
    EmptyAppId,
    #[error("app id must not contain NUL")]
    NulAppId,
}

/// Compose the stable per-app relay slot name.
///
/// # Errors
///
/// [`ReplicationNameError`] when the app id is empty or carries a NUL - the two
/// spellings that would make one slot serve two apps, and a logical slot admits
/// exactly one consumer.
pub fn relay_slot_name(app_id: &str) -> Result<String, ReplicationNameError> {
    Ok(format!("{SLOT_PREFIX}{}", app_token(app_id)?))
}

/// The stable hex token an app's replication objects are named by.
fn app_token(app_id: &str) -> Result<String, ReplicationNameError> {
    if app_id.is_empty() {
        return Err(ReplicationNameError::EmptyAppId);
    }
    if app_id.contains('\0') {
        return Err(ReplicationNameError::NulAppId);
    }

    let digest = Sha256::digest(app_id.as_bytes());
    let mut token = String::with_capacity(28);
    for byte in &digest[..14] {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_names_are_stable_case_sensitive_identifiers() {
        assert_eq!(
            relay_slot_name("alpha").unwrap(),
            "__zs_relay_8ed3f6ad685b959ead7022518e1a"
        );
        assert_ne!(
            relay_slot_name("MyApp").unwrap(),
            relay_slot_name("myapp").unwrap()
        );
        assert!(relay_slot_name("").is_err());
        assert!(relay_slot_name("bad\0app").is_err());
    }

    /// The relay drops slots by [`SLOT_PREFIX`] and nothing else, so a name
    /// that lost the prefix would be a slot the relay never reclaims.
    #[test]
    fn every_slot_name_carries_the_prefix_the_relay_reclaims_by() {
        assert!(relay_slot_name("alpha").unwrap().starts_with(SLOT_PREFIX));
        assert!(SLOT_PREFIX.starts_with(OBJECT_PREFIX));
    }

    /// The shared publication and the per-app slots must not collide in the
    /// reserved namespace: the relay's prefix-scoped slot drop would otherwise
    /// name the publication.
    #[test]
    fn the_datastore_publication_is_not_reachable_by_the_slot_prefix() {
        assert!(DATASTORE_PUBLICATION.starts_with(OBJECT_PREFIX));
        assert!(!DATASTORE_PUBLICATION.starts_with(SLOT_PREFIX));
    }
}
