//! The CLUSTER-GLOBAL apply lock, and the classifier that decides when to take it.
//!
//! # Why the project lock is not enough
//!
//! `run_platform_migrations` already brackets its whole run in the engine's
//! project lock — `SELECT pg_advisory_lock(hashtext('zeroship'))`
//! (`third_party/zero-migrate/.../postgres/session.rs:60`). Every caller in this
//! tree passes the same `--project-id zeroship`, so the KEY is identical for any
//! two concurrent runs. It still does not serialize them, because
//!
//! > **PostgreSQL advisory locks are DATABASE-scoped.**
//!
//! The lock tag carries `MyDatabaseId`, so the same key taken from two different
//! databases on one cluster is two different locks. Measured on the 5440 cluster
//! (PostgreSQL 17): holding `pg_advisory_lock(hashtext('zeroship')::bigint)` in
//! database A, `pg_try_advisory_lock` of the same key returns `t` (acquired) from
//! database B and `f` (correctly blocked) from a second session on A.
//!
//! Every test suite migrates its own scratch database
//! (`zeroship_auth_test_<pid>_<ns>`, `zeroship_billing_test_<pid>_<ns>`, …), so
//! the per-run database isolation those suites rely on is exactly what defeats
//! the project lock.
//!
//! # What actually races
//!
//! Roles are not the only cluster-global object, and `CREATE ROLE` is not the
//! statement that fails most often. Asking the server for its own authority —
//! `SELECT relname FROM pg_class WHERE relisshared` on PG 17 — yields eleven
//! shared catalogs: `pg_auth_members`, `pg_authid`, `pg_database`,
//! `pg_db_role_setting`, `pg_parameter_acl`, `pg_replication_origin`,
//! `pg_shdepend`, `pg_shdescription`, `pg_shseclabel`, `pg_subscription`,
//! `pg_tablespace`. A write to any of them is cluster-global no matter which
//! database issued it.
//!
//! `db/migrations-ts/20260702000100_schema_roles_extensions.ts` writes three of
//! them, and `db/migrations-ts/20260702000900_grants.ts` writes two more —
//! the cluster-global surface is NOT confined to one file:
//!
//! - `CREATE ROLE` → `pg_authid`. The DSL's `ifNotExists` renders as a PL/pgSQL
//!   `DO $$ IF NOT EXISTS (SELECT 1 FROM pg_roles …) THEN CREATE ROLE … $$`
//!   probe, which is check-then-act: two sessions both pass the probe and both
//!   insert.
//! - `ALTER ROLE … SET search_path` → `pg_db_role_setting` (`setdatabase = 0`,
//!   i.e. cluster-wide, since the statement carries no `IN DATABASE`). This one
//!   is emitted UNCONDITIONALLY — the renderer appends it as a separate
//!   statement after the guarded create
//!   (`third_party/zero-migrate/.../render/vendor.rs:394-401`), so `ifNotExists`
//!   does not suppress it. It therefore races on EVERY run, including runs
//!   against a cluster where all eleven roles already exist.
//! - `GRANT <role> TO <role>` → `pg_auth_members`
//!   (`20260702000900_grants.ts:19`).
//!
//! Reproduced directly on the 5440 cluster from two different databases, no
//! migrate binary involved — three distinct aborts, all of them the shape that
//! arrives with no test name attached:
//!
//! ```text
//! ERROR:  tuple concurrently updated
//! ERROR:  duplicate key value violates unique constraint "pg_db_role_setting_databaseid_rol_index"
//! ERROR:  duplicate key value violates unique constraint "pg_authid_rolname_index"
//! ```
//!
//! # The lock
//!
//! Because an advisory lock is database-scoped, a cluster-wide one has to be
//! taken somewhere every run agrees on. [`ClusterGlobalLock`] opens a SECOND
//! session to a fixed coordination database on the SAME cluster — the caller's
//! own DSN with `dbname` swapped, defaulting to the `postgres` maintenance
//! database — and takes a session advisory lock there. Two runs migrating two
//! different databases now meet on one key in one database, which is the only
//! place they can meet.
//!
//! The alternative shapes lose:
//!
//! - A `LOCK TABLE` on the contended shared catalog IS cluster-wide (measured:
//!   `pg_db_role_setting` blocks cross-database, the per-database `pg_class`
//!   does not), but the holder's own `CREATE ROLE` needs `RowExclusiveLock` on
//!   that same catalog, so a self-excluding mode deadlocks the holder against
//!   its own bracket.
//! - Pre-provisioning the roles once per cluster leaves
//!   `ALTER ROLE … SET search_path` racing anyway (it is unconditional), and
//!   would only cover callers that route through the provisioning script.
//! - Refusing a second concurrent run turns a race into a hard stop, when what
//!   is wanted is for concurrent runs to WORK.
//!
//! Deadlock-freedom: the two locks are always taken in the same order (project
//! lock first, in the target database; cluster lock second, in the coordination
//! database) and no path takes the cluster lock while waiting for a project
//! lock, so there is no cycle to close.
//!
//! Release is belt-and-braces: [`ClusterGlobalLock::release`] issues
//! `pg_advisory_unlock`, and the lock is session-scoped, so dropping the
//! coordination session — including on a panic or a killed process — frees it
//! regardless.

use compio_postgres::Config;
use zero_migrate::driver::SqlSession;

use super::PlatformMigrateError;
use crate::CompioPgSession;

/// The database every concurrent migrate run coordinates through. `postgres` is
/// the maintenance database `initdb` creates and the official container image
/// ships; it is the conventional "connect to the cluster, not to a database"
/// target. Overridable per run because it CAN be dropped on a hardened cluster,
/// in which case the acquisition fails loudly and names the override rather than
/// silently skipping the lock.
pub const DEFAULT_CLUSTER_LOCK_DATABASE: &str = "postgres";

/// How long a run waits for a peer's cluster-global section before giving up.
///
/// The lock is held only across the apply of the cluster-global FILES, not the
/// whole run, so a legitimate wait is seconds. This bound exists so a run whose
/// peer wedged reports that fact with the holding backends named, instead of
/// hanging until the CI job times out with no diagnostic.
const CLUSTER_LOCK_WAIT: &str = "600s";

/// A stable 64-bit advisory-lock key derived from a namespace string.
///
/// FNV-1a, computed HERE rather than by `hashtext()` on the server. The engine's
/// project lock uses `hashtext`, which yields 32 bits and whose own doc comment
/// records the resulting collision caveat; a full 64-bit key has no such
/// caveat. Computing it in Rust also keeps the key a plain integer literal in
/// the SQL, so the statement carries no interpolated identifier at all.
fn advisory_key(namespace: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in namespace.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash as i64
}

/// The lock namespace. Runs of DIFFERENT platform projects on one cluster still
/// contend, deliberately: the objects at stake (roles, databases, tablespaces)
/// are cluster-global, so they are shared across projects too.
const CLUSTER_LOCK_NAMESPACE: &str = "zeroship:platform-migrate:cluster-global";

/// A held cluster-wide advisory lock plus the coordination session holding it.
#[derive(Debug)]
pub struct ClusterGlobalLock {
    session: CompioPgSession,
    key: i64,
}

impl ClusterGlobalLock {
    /// Connect to the coordination database and block until the cluster-global
    /// lock is held.
    ///
    /// # Errors
    /// Fails if the coordination DSN cannot be derived or connected, or if the
    /// wait exceeds [`CLUSTER_LOCK_WAIT`].
    pub async fn acquire(
        database_url: &str,
        lock_database: &str,
        file: &str,
    ) -> Result<Self, PlatformMigrateError> {
        let mut config: Config =
            database_url
                .parse()
                .map_err(|error| PlatformMigrateError::Apply {
                    file: file.to_string(),
                    message: format!(
                        "cluster lock: the migrate DSN does not parse, so the coordination \
                         DSN cannot be derived from it: {error}"
                    ),
                })?;
        config.dbname(lock_database);

        let session = CompioPgSession::connect_with_config(&config)
            .await
            .map_err(|error| PlatformMigrateError::Apply {
                file: file.to_string(),
                message: format!(
                    "cluster lock: cannot connect to the coordination database \
                     '{lock_database}' on this cluster: {error}. This migration writes \
                     cluster-global objects (roles / databases / tablespaces), which no \
                     per-database lock can protect; pass --cluster-lock-database with a \
                     database every concurrent migrate run on this cluster can reach."
                ),
            })?;

        let key = advisory_key(CLUSTER_LOCK_NAMESPACE);
        session
            .batch(&format!("SET lock_timeout = '{CLUSTER_LOCK_WAIT}'"))
            .await
            .map_err(|error| PlatformMigrateError::Apply {
                file: file.to_string(),
                message: format!("cluster lock: cannot bound the lock wait: {error}"),
            })?;

        // A plain integer literal: `key` is an i64 computed in Rust, so nothing
        // here is interpolated from a caller-controlled string.
        if let Err(error) = session
            .batch(&format!("SELECT pg_advisory_lock({key})"))
            .await
        {
            let holders = Self::describe_holders(&session).await;
            return Err(PlatformMigrateError::Apply {
                file: file.to_string(),
                message: format!(
                    "cluster lock: waited {CLUSTER_LOCK_WAIT} for the cluster-global apply \
                     lock on database '{lock_database}' and did not get it: {error}.{holders}"
                ),
            });
        }

        Ok(Self { session, key })
    }

    /// Best-effort diagnostic naming the backends holding or waiting on the
    /// coordination lock, so a wedged peer is identified rather than merely
    /// timed out against.
    ///
    /// Reports every single-key advisory lock in the coordination database
    /// rather than reconstructing the bigint key from `classid`/`objid`. That
    /// reconstruction is a signed/unsigned trap (`classid` is an unsigned oid
    /// while the key is an i64), and the coordination database exists only to
    /// carry this lock, so the unfiltered list is already precise. Best effort
    /// by construction: this runs on the error path, and a failure to produce a
    /// diagnostic must not replace the real error.
    async fn describe_holders(session: &CompioPgSession) -> String {
        let sql = "SELECT l.pid::text, coalesce(a.application_name, '?'), \
                   CASE WHEN l.granted THEN 'holding' ELSE 'waiting' END \
                   FROM pg_locks l LEFT JOIN pg_stat_activity a ON a.pid = l.pid \
                   WHERE l.locktype = 'advisory' AND l.objsubid = 1";
        let Ok(rows) = session.query(sql, &[]).await else {
            return String::new();
        };
        let described: Vec<String> = rows
            .iter()
            .filter_map(|row| {
                let pid: String = row.try_get(0).ok()?;
                let app: String = row.try_get(1).ok()?;
                let state: String = row.try_get(2).ok()?;
                Some(format!("pid {pid} ({app}) {state}"))
            })
            .collect();
        if described.is_empty() {
            String::new()
        } else {
            format!(" Advisory-lock backends there: {}.", described.join("; "))
        }
    }

    /// Release the lock and close the coordination session.
    ///
    /// # Errors
    /// Fails if the unlock statement itself errors. The lock is session-scoped,
    /// so the hold ends when the session drops regardless of this call.
    pub async fn release(self, file: &str) -> Result<(), PlatformMigrateError> {
        let key = self.key;
        self.session
            .batch(&format!("SELECT pg_advisory_unlock({key})"))
            .await
            .map_err(|error| PlatformMigrateError::Apply {
                file: file.to_string(),
                message: format!("cluster lock: release failed: {error}"),
            })
    }
}

/// Does this rendered statement write a cluster-global (shared) catalog?
///
/// Derived from the server's own `pg_class.relisshared` set (enumerated in the
/// module header), mapped back to the SQL that writes each one. Deliberately
/// OVER-inclusive: a false positive costs one extra serialized file, a false
/// negative costs the race this module exists to close.
///
/// The per-database near-misses this must NOT match are the reason the
/// role-membership test is `GRANT`/`REVOKE` without an `ON` target:
/// `GRANT USAGE ON SCHEMA zeroship TO …` and
/// `ALTER DEFAULT PRIVILEGES IN SCHEMA … REVOKE … ON TABLES FROM …` write
/// `pg_namespace` / `pg_default_acl`, which are per-database.
#[must_use]
pub fn sql_touches_cluster_global(sql: &str) -> bool {
    // Normalize so line breaks and runs of whitespace in a rendered DO block
    // cannot hide a two-word marker. Leading/trailing spaces make the ` ON `
    // and `GRANT ` boundary tests total.
    let mut normalized = String::with_capacity(sql.len() + 2);
    normalized.push(' ');
    let mut in_space = false;
    for ch in sql.chars() {
        if ch.is_whitespace() {
            in_space = true;
            continue;
        }
        if in_space {
            normalized.push(' ');
            in_space = false;
        }
        normalized.extend(ch.to_uppercase());
    }
    normalized.push(' ');

    // pg_authid / pg_auth_members / pg_db_role_setting — role DDL in every
    // spelling PostgreSQL accepts, plus the two OWNED forms that rewrite
    // pg_shdepend across the cluster.
    const ROLE_MARKERS: [&str; 11] = [
        "CREATE ROLE ",
        "ALTER ROLE ",
        "DROP ROLE ",
        "CREATE USER ",
        "ALTER USER ",
        "DROP USER ",
        "CREATE GROUP ",
        "ALTER GROUP ",
        "DROP GROUP ",
        "REASSIGN OWNED ",
        "DROP OWNED ",
    ];
    // pg_database / pg_tablespace / pg_subscription / pg_parameter_acl, and the
    // shared COMMENT + SECURITY LABEL catalogs (pg_shdescription, pg_shseclabel)
    // reached via `ON ROLE` / `ON DATABASE` / `ON TABLESPACE`.
    const OBJECT_MARKERS: [&str; 13] = [
        "CREATE DATABASE ",
        "ALTER DATABASE ",
        "DROP DATABASE ",
        "CREATE TABLESPACE ",
        "ALTER TABLESPACE ",
        "DROP TABLESPACE ",
        "CREATE SUBSCRIPTION ",
        "ALTER SUBSCRIPTION ",
        "DROP SUBSCRIPTION ",
        "ALTER SYSTEM ",
        " ON DATABASE ",
        " ON TABLESPACE ",
        " ON ROLE ",
    ];
    // `GRANT … ON PARAMETER <guc> TO <role>` writes pg_parameter_acl.
    const PARAMETER_MARKER: &str = " ON PARAMETER ";

    if ROLE_MARKERS.iter().any(|m| normalized.contains(m))
        || OBJECT_MARKERS.iter().any(|m| normalized.contains(m))
        || normalized.contains(PARAMETER_MARKER)
    {
        return true;
    }

    // Role membership: `GRANT a TO b` / `REVOKE a FROM b` write pg_auth_members.
    // They are exactly the GRANT/REVOKE statements with no `ON <object>` target.
    let trimmed = normalized.trim_start();
    (trimmed.starts_with("GRANT ") || trimmed.starts_with("REVOKE "))
        && !normalized.contains(" ON ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key must not depend on the server, the process, or the build: two
    /// runs that cannot agree on it are two runs that do not exclude each other.
    #[test]
    fn the_advisory_key_is_stable_and_nonzero() {
        let key = advisory_key(CLUSTER_LOCK_NAMESPACE);
        assert_eq!(
            key,
            advisory_key(CLUSTER_LOCK_NAMESPACE),
            "the key must be a pure function of the namespace"
        );
        assert_ne!(key, 0, "a zero key would collide with an uninitialized one");
        assert_ne!(
            key,
            advisory_key("zeroship:platform-migrate:something-else"),
            "distinct namespaces must not collide"
        );
    }

    /// The statements the platform migrations ACTUALLY render for the three
    /// shared catalogs they write. Sources, in order: the `ifNotExists` DO-block
    /// from vendor.rs:361-366, the unconditional search_path push from
    /// vendor.rs:394-401, and the two raw() statements at grants.ts:10-21.
    #[test]
    fn the_real_cluster_global_statements_are_classified_as_such() {
        for sql in [
            "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_auth') \
             THEN CREATE ROLE \"zeroship_auth\" LOGIN PASSWORD 'zeroship_auth' BYPASSRLS; \
             END IF; END $$",
            "ALTER ROLE \"zeroship_auth\" SET search_path = zeroship, public",
            "ALTER ROLE zeroship_worker WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE INHERIT \
             REPLICATION BYPASSRLS",
            "GRANT zeroship_workflow_owner TO zeroship_worker",
            "DROP ROLE IF EXISTS \"zeroship_app\"",
            "GRANT CONNECT ON DATABASE zeroship TO zeroship_app",
            "CREATE DATABASE other",
            "ALTER SYSTEM SET work_mem = '64MB'",
            "COMMENT ON ROLE zeroship_app IS 'x'",
        ] {
            assert!(
                sql_touches_cluster_global(sql),
                "must be cluster-global: {sql}"
            );
        }
    }

    /// The one-variable partners. These are the per-database statements that sit
    /// NEXT TO the cluster-global ones in the very same two migration files; if
    /// the classifier matched these too it would serialize the whole run and the
    /// test above would still pass. `pg_namespace`, `pg_default_acl` and
    /// per-relation ACLs are all per-database.
    #[test]
    fn the_per_database_statements_beside_them_are_not() {
        for sql in [
            "CREATE SCHEMA IF NOT EXISTS \"zeroship\"",
            "CREATE EXTENSION IF NOT EXISTS \"citext\" SCHEMA public",
            "GRANT USAGE ON SCHEMA zeroship TO zeroship_auth",
            "GRANT SELECT, INSERT ON TABLE zeroship.users TO zeroship_auth",
            "REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA zeroship FROM zeroship_worker",
            "ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship REVOKE ALL PRIVILEGES ON TABLES \
             FROM zeroship_worker",
            "CREATE TABLE zeroship.apps (id text primary key)",
            "CREATE SEQUENCE zeroship.audit_events_id_seq AS bigint START 1",
            "COMMENT ON TABLE zeroship.apps IS 'x'",
        ] {
            assert!(
                !sql_touches_cluster_global(sql),
                "must NOT be cluster-global: {sql}"
            );
        }
    }

    /// A rendered DO block arrives with newlines in it. Matching on the raw text
    /// would miss `CREATE\n  ROLE`; the classifier normalizes first.
    #[test]
    fn whitespace_and_case_do_not_hide_a_marker() {
        assert!(sql_touches_cluster_global("create\n\trole   \"x\" login"));
        assert!(sql_touches_cluster_global(
            "grant\n  zeroship_workflow_owner\n  to zeroship_worker"
        ));
    }
}
