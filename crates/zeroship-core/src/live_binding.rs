//! The one spelling of "this app holds a LIVE binding to this database".
//!
//! Three services ask that question and they must not be able to answer it
//! differently: Control serves the binding an isolate is built from, the CDC
//! relay decides which schema a subscriber is entitled to, and the migration
//! service decides whose schema an apply may write. A binding that one of them
//! calls live and another does not is a tenant-boundary disagreement, and the
//! failure it produces - an apply into a database the app was revoked from -
//! is silent on the side that got it wrong.
//!
//! LIVE has three conjuncts and each is load-bearing:
//!
//! - `database_bindings.status = 'active'`. A `pending` binding has been
//!   declared by Control and nothing has converged it; a `revoking` one is on
//!   its way out.
//! - `database_bindings.observed_generation >= generation`. Control declares
//!   into ITS database and a per-cluster reconciler makes a tenant cluster
//!   match, with no transaction spanning the two. Until the reconciler has
//!   caught up, the roles the binding names do not exist.
//! - `databases.status = 'active'`. A database still `provisioning` has a
//!   schema epoch no cluster has minted roles for, and one `deleting` is being
//!   taken away.
//!
//! [`LIVE_BINDINGS_FROM_WHERE`] is the shared fragment rather than a shared
//! whole statement because the readers project different columns: one wants the
//! database, one wants only existence. What they must agree on is the FROM and
//! the WHERE, so that is what is spelled once. `$1` is always the app id, bound
//! as `text`; a caller adding its own conjunct appends it and numbers its
//! placeholders from `$2`.

/// The `FROM` and `WHERE` of every live-binding read, with `$1` the app id.
///
/// Aliases are part of the contract: `b` is `zeroship.database_bindings` and
/// `d` is `zeroship.databases`, so a caller's `SELECT` list and its extra
/// conjuncts name the same tables this clause joined.
pub const LIVE_BINDINGS_FROM_WHERE: &str = "FROM zeroship.database_bindings b \
      JOIN zeroship.databases d ON d.id = b.database_id \
     WHERE b.app_id = $1 \
       AND b.status = 'active' \
       AND b.observed_generation >= b.generation \
       AND d.status = 'active'";

#[cfg(test)]
mod tests {
    use super::*;

    /// Each conjunct is present. A predicate that lost one would still be
    /// valid SQL and would still return rows - it would just return rows for
    /// bindings nothing has converged.
    #[test]
    fn the_predicate_carries_all_three_liveness_conjuncts() {
        let mut checked = 0;
        for conjunct in [
            "b.status = 'active'",
            "b.observed_generation >= b.generation",
            "d.status = 'active'",
        ] {
            assert!(
                LIVE_BINDINGS_FROM_WHERE.contains(conjunct),
                "the live-binding predicate must carry {conjunct}: {LIVE_BINDINGS_FROM_WHERE}"
            );
            checked += 1;
        }
        assert_eq!(checked, 3, "the arm must not pass over an empty list");
    }

    /// The app id is `$1` and nothing else is bound, so a caller appending its
    /// own conjunct starts at `$2`.
    #[test]
    fn the_app_id_is_the_only_placeholder() {
        assert!(LIVE_BINDINGS_FROM_WHERE.contains("b.app_id = $1"));
        assert!(!LIVE_BINDINGS_FROM_WHERE.contains("$2"));
    }
}
