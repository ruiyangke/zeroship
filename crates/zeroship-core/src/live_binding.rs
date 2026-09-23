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
//! - `databases.status = 'active'`. A database still `provisioning` has no
//!   roles any cluster has minted, and one `deleting` is being taken away.
//!
//! [`LIVE_BINDINGS_FROM_WHERE`] is the shared fragment rather than a shared
//! whole statement because the readers project different columns: one wants the
//! database, one wants only existence. What they must agree on is the FROM and
//! the WHERE, so that is what is spelled once. `$1` is always the app id, bound
//! as `text`; a caller adding its own conjunct appends it and numbers its
//! placeholders from `$2`.
//!
//! [`LIVE_BINDINGS_FROM_WHERE_EVERY_APP`] is the same predicate over every
//! app's bindings at once, for the reader that builds a per-app projection in
//! one pass rather than one statement per app. It is generated from the same
//! conjuncts as the app-scoped one, so the two cannot drift: a liveness rule
//! added to one is added to both.

/// The live-binding `FROM` and `WHERE`, with `$app` the caller's app predicate
/// and the liveness conjuncts spelled once for every projection of it.
macro_rules! live_bindings_from_where {
    ($app:literal) => {
        concat!(
            "FROM zeroship.database_bindings b \
              JOIN zeroship.databases d ON d.id = b.database_id \
             WHERE ",
            $app,
            "b.status = 'active' \
               AND b.observed_generation >= b.generation \
               AND d.status = 'active'"
        )
    };
}

/// The `FROM` and `WHERE` of every live-binding read, with `$1` the app id.
///
/// Aliases are part of the contract: `b` is `zeroship.database_bindings` and
/// `d` is `zeroship.databases`, so a caller's `SELECT` list and its extra
/// conjuncts name the same tables this clause joined.
pub const LIVE_BINDINGS_FROM_WHERE: &str = live_bindings_from_where!("b.app_id = $1 AND ");

/// The same predicate over EVERY app, binding no placeholder.
///
/// The reader here is the worker version feed, which carries each app's LIVE
/// BINDING SET so a worker can tell that the set its resident isolate was built
/// from is no longer the set Control serves. It projects `b.app_id` and groups
/// in the caller: one statement per app would be one round trip per app on
/// every poll.
pub const LIVE_BINDINGS_FROM_WHERE_EVERY_APP: &str = live_bindings_from_where!("");

#[cfg(test)]
mod tests {
    use super::*;

    /// Each conjunct is present in EVERY projection. A predicate that lost one
    /// would still be valid SQL and would still return rows - it would just
    /// return rows for bindings nothing has converged.
    #[test]
    fn every_projection_carries_all_three_liveness_conjuncts() {
        let mut checked = 0;
        for predicate in [LIVE_BINDINGS_FROM_WHERE, LIVE_BINDINGS_FROM_WHERE_EVERY_APP] {
            for conjunct in [
                "b.status = 'active'",
                "b.observed_generation >= b.generation",
                "d.status = 'active'",
            ] {
                assert!(
                    predicate.contains(conjunct),
                    "the live-binding predicate must carry {conjunct}: {predicate}"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 6, "the arm must not pass over an empty list");
    }

    /// The app id is `$1` and nothing else is bound, so a caller appending its
    /// own conjunct starts at `$2`.
    #[test]
    fn the_app_id_is_the_only_placeholder() {
        assert!(LIVE_BINDINGS_FROM_WHERE.contains("b.app_id = $1"));
        assert!(!LIVE_BINDINGS_FROM_WHERE.contains("$2"));
    }

    /// The every-app projection narrows by no app and binds no placeholder, so
    /// a caller passing parameters to it would be passing them to nothing.
    ///
    /// Its control is the app-scoped constant above, which must still carry
    /// both - otherwise this pair would pass over a module that had simply
    /// stopped narrowing at all.
    #[test]
    fn the_every_app_projection_binds_no_placeholder() {
        assert!(!LIVE_BINDINGS_FROM_WHERE_EVERY_APP.contains("b.app_id"));
        assert!(!LIVE_BINDINGS_FROM_WHERE_EVERY_APP.contains('$'));
        assert!(LIVE_BINDINGS_FROM_WHERE.contains("b.app_id"));
        assert!(LIVE_BINDINGS_FROM_WHERE.contains('$'));
    }
}
