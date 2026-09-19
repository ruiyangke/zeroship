//! What a cluster answers about itself, and the version floor it has to clear.
//!
//! A datastore is not declared in a file and not declared in a migration. The
//! service holding a cluster's privileged credential connects and asks the
//! cluster who it is, and control records what that answer said. Keying the
//! registry row on the cluster's own identity is what makes registration
//! idempotent: two services configured against one cluster converge on one
//! row, and a mistyped DSN either fails to connect or lands on a different
//! cluster, where it becomes a visibly new row rather than a silent duplicate.

use compio_postgres::Client;

/// The oldest `PostgreSQL` this platform's tenant fence exists on.
///
/// Below 16 `pg_auth_members` carries no `inherit_option` and no `set_option`
/// column, so `WITH INHERIT FALSE` and `WITH SET FALSE` are not a weaker fence,
/// they are NO fence: the grants the reconciler emits would either be refused
/// as syntax or - on a server that accepted the words - record a membership
/// that inherits, which makes every binding's privileges ambient on the shared
/// worker login.
///
/// `crates/zeroship-data-orm/tests/postgres_tenant_fence.rs` measures the whole
/// chain against this floor and states the same number for the same reason.
pub const MINIMUM_SERVER_VERSION_NUM: i32 = 160_000;

/// What one cluster says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterIdentity {
    system_identifier: i64,
    server_version_num: i32,
}

impl ClusterIdentity {
    /// The cluster's own identity, from `pg_control_system()`.
    ///
    /// Stable for the life of a cluster, identical from every database in it,
    /// and carried forward by a promoted physical replica. It is the natural
    /// key of `zeroship.datastores`.
    #[must_use]
    pub const fn system_identifier(&self) -> i64 {
        self.system_identifier
    }

    /// `server_version_num`, as the server reports it.
    #[must_use]
    pub const fn server_version_num(&self) -> i32 {
        self.server_version_num
    }

    /// Refuse a cluster this platform has no fence on.
    ///
    /// BY VERSION, and deliberately not by letting the grant syntax fail. A
    /// pre-16 cluster would fail later anyway, which is fail-closed, but it
    /// would fail as a syntax error while an operator was adding capacity.
    /// This refusal says which server was found, which is needed, and why.
    ///
    /// # Errors
    ///
    /// [`UnsupportedServerVersion`] when the cluster is below
    /// [`MINIMUM_SERVER_VERSION_NUM`].
    pub const fn require_supported(&self) -> Result<(), UnsupportedServerVersion> {
        if self.server_version_num < MINIMUM_SERVER_VERSION_NUM {
            return Err(UnsupportedServerVersion {
                server_version_num: self.server_version_num,
                minimum_server_version_num: MINIMUM_SERVER_VERSION_NUM,
            });
        }
        Ok(())
    }
}

/// A cluster whose `PostgreSQL` is older than the tenant fence needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "this cluster reports server_version_num {server_version_num} and the datastore bootstrap \
     requires {minimum_server_version_num} or above: below PostgreSQL 16 pg_auth_members carries \
     no inherit_option and no set_option, so WITH INHERIT FALSE and WITH SET FALSE record no \
     fence at all"
)]
pub struct UnsupportedServerVersion {
    pub server_version_num: i32,
    pub minimum_server_version_num: i32,
}

/// Ask the cluster who it is and which server it runs.
///
/// One round trip for both, because a reconciler that read them separately
/// could report an identity from one server and a version from another after a
/// failover.
///
/// # Errors
///
/// Any driver or server failure, unchanged. `pg_control_system()` is
/// superuser-restricted by default, so a login without it is refused here
/// rather than at the first grant.
pub async fn read_cluster_identity(
    client: &Client,
) -> Result<ClusterIdentity, compio_postgres::Error> {
    let row = client
        .query_one(
            "SELECT (SELECT system_identifier FROM pg_control_system()) AS system_identifier, \
                    current_setting('server_version_num')::int AS server_version_num",
            &[],
        )
        .await?;
    Ok(ClusterIdentity {
        system_identifier: row.get("system_identifier"),
        server_version_num: row.get("server_version_num"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(server_version_num: i32) -> ClusterIdentity {
        ClusterIdentity {
            system_identifier: 7_413_921_558_233_990_142,
            server_version_num,
        }
    }

    /// The floor is a refusal on one side and a pass on the other.
    ///
    /// Paired, because a gate that refused everything would satisfy the
    /// refusal arm alone while making every cluster unbootstrappable.
    #[test]
    fn the_version_floor_refuses_below_sixteen_and_admits_sixteen() {
        assert_eq!(
            identity(150_013).require_supported(),
            Err(UnsupportedServerVersion {
                server_version_num: 150_013,
                minimum_server_version_num: MINIMUM_SERVER_VERSION_NUM,
            }),
            "PostgreSQL 15 has no per-membership inherit option"
        );
        assert_eq!(
            identity(159_999).require_supported(),
            Err(UnsupportedServerVersion {
                server_version_num: 159_999,
                minimum_server_version_num: MINIMUM_SERVER_VERSION_NUM,
            }),
            "the boundary is exclusive below"
        );
        assert_eq!(
            identity(MINIMUM_SERVER_VERSION_NUM).require_supported(),
            Ok(()),
            "the control: the deploy pin itself must bootstrap"
        );
        assert_eq!(
            identity(170_004).require_supported(),
            Ok(()),
            "the control: a newer major must bootstrap too"
        );
    }

    /// The refusal names the number it found and the number it wanted.
    ///
    /// An operator adding capacity reads this text and nothing else, so a
    /// message that only said "unsupported" would send them to the source.
    #[test]
    fn the_version_refusal_names_both_versions() {
        let refusal = identity(140_010)
            .require_supported()
            .expect_err("PostgreSQL 14 must be refused")
            .to_string();
        assert!(refusal.contains("140010"), "{refusal}");
        assert!(refusal.contains("160000"), "{refusal}");
        assert!(refusal.contains("inherit_option"), "{refusal}");
    }
}
