//! PostgreSQL role names shared by the migration and data planes.
//!
//! PostgreSQL stores at most 63 bytes of an identifier under its default
//! `NAMEDATALEN`. It truncates a longer name with only a notice, so a role name
//! used as an authorization fence must be refused rather than shortened. A
//! truncation could otherwise make a newly derived name resolve to a role that
//! was meant to be reaped.

/// PostgreSQL's default identifier limit (`NAMEDATALEN - 1`), in bytes.
pub const POSTGRES_IDENTIFIER_MAX_BYTES: usize = 63;

/// Why a per-app PostgreSQL role name could not be composed safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PerAppRoleNameError {
    /// The complete, unmodified role name does not fit in a PostgreSQL
    /// identifier. The composer never truncates or hashes authorization roles.
    #[error("per-app PostgreSQL role name is {actual_bytes} bytes; maximum is {max_bytes} bytes")]
    TooLong {
        actual_bytes: usize,
        max_bytes: usize,
    },
}

/// Compose the per-app PostgreSQL role name.
///
/// The input remains a string because current production callers use zeroship
/// typed IDs, hyphenated UUIDs, and derived app-schema identifiers. The full
/// name is preserved byte-for-byte so distinct inputs cannot collapse onto one
/// role.
///
/// # Errors
///
/// Returns [`PerAppRoleNameError::TooLong`] rather than allowing PostgreSQL to
/// silently truncate a name beyond [`POSTGRES_IDENTIFIER_MAX_BYTES`].
pub fn per_app_role_name(app_id: &str) -> Result<String, PerAppRoleNameError> {
    let role = format!("app_{app_id}_role");
    if role.len() > POSTGRES_IDENTIFIER_MAX_BYTES {
        return Err(PerAppRoleNameError::TooLong {
            actual_bytes: role.len(),
            max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
        });
    }
    Ok(role)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_app_role_name_keeps_the_current_shape() {
        assert_eq!(per_app_role_name("app_demo").unwrap(), "app_app_demo_role");
        assert_eq!(
            per_app_role_name("0191e7a2-b3c4-4d5e-8f90-123456789abc").unwrap(),
            "app_0191e7a2-b3c4-4d5e-8f90-123456789abc_role"
        );
    }

    #[test]
    fn per_app_role_name_does_not_collapse_distinct_app_ids() {
        assert_ne!(
            per_app_role_name("app-demo").unwrap(),
            per_app_role_name("app_demo").unwrap(),
            "hyphen and underscore app ids must map to distinct quoted roles"
        );
    }

    #[test]
    fn per_app_role_name_accepts_exactly_63_bytes() {
        let app_id = "a".repeat(54);
        let role = per_app_role_name(&app_id).expect("63-byte role name");
        assert_eq!(role.len(), POSTGRES_IDENTIFIER_MAX_BYTES);
    }

    #[test]
    fn per_app_role_name_refuses_64_bytes_without_shortening() {
        let app_id = "a".repeat(55);
        assert_eq!(
            per_app_role_name(&app_id),
            Err(PerAppRoleNameError::TooLong {
                actual_bytes: 64,
                max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
            })
        );
    }

    #[test]
    fn per_app_role_name_limit_counts_bytes() {
        let app_id = "\u{e9}".repeat(28);
        assert_eq!(
            per_app_role_name(&app_id),
            Err(PerAppRoleNameError::TooLong {
                actual_bytes: 65,
                max_bytes: POSTGRES_IDENTIFIER_MAX_BYTES,
            })
        );
    }
}
