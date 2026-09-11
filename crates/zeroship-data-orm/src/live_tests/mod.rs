//! Live engine verification lives with the internals it exercises.
mod column_grants;
pub(crate) mod host;
mod integration;
mod mask_flip;
mod sc1_driver;
mod sc1_live;
#[path = "../../../../tests/fixtures/data/schema.rs"]
pub(crate) mod schema_fixture;
mod search_ir_live;
mod search_tx_lane;
mod sqlite_integration;
pub(crate) mod support;
mod unmask_tx_lane;

mod roles;

/// `reset_context_for_tests` must clear ALL THREE thread-locals.
///
/// The context was one struct until 2026-09-02. It is now three owners with
/// three thread-locals - the adapter context, the engine.s lanes, and
/// data-core.s descriptor store - so "reset" became three calls, any one of
/// which could be dropped without an existing test noticing. One test per
/// store, because a single test asserting all three would pass while two of
/// them regressed.
///
/// **Both halves live in ONE test on purpose.** The obvious shape - claim in
/// test A, assert clean in test B - proves nothing here: libtest gives every
/// `#[test]` its own OS thread even under `--test-threads=1` (measured
/// 2026-09-01), so B would read a fresh thread-local and pass whatever the
/// helper does. Such a guard is green by construction. The real hazard is a
/// MID-TEST reset - scenario setup between phases, and the `ContextReset` drop
/// guard in `transaction/mod.rs` - and that is what this reproduces.
#[cfg(test)]
mod reset_clears_every_thread_local {
    /// Deleting `tx_lanes::reset_for_tests()` from `reset_context_for_tests`
    /// must fail this.
    ///
    /// `TxLanes` has TWO stores and both are asserted, because they have
    /// different lifetimes and a partial reset could plausibly clear one: the
    /// lane map is emptied by ordinary retirement, whereas the withdrawal
    /// tombstone is documented to outlive its lane and to be cleared only by
    /// the next `admit_transaction`. The tombstone is therefore the residue
    /// most likely to survive a reset that looks correct.
    #[test]
    fn a_mid_test_reset_drops_a_claim_and_a_withdrawal_tombstone() {
        let app = "app_reset_guard";

        assert!(
            crate::tx_lanes::with_mut(|l| l.try_claim_tx(app)),
            "an unclaimed app claims on a fresh thread"
        );
        // `tx_claimed_by`, not `has_tx_for`: claiming opens the lane, and
        // `has_tx_for` additionally requires the BEGIN to have landed a
        // session. The claim without a session is exactly the window this
        // helper has to clean up, so it is the one to assert on.
        assert!(crate::tx_lanes::with(|l| l.tx_claimed_by(app)));

        crate::tx_lanes::with_mut(|l| l.withdraw_tx_session(app));
        assert!(crate::tx_lanes::with(|l| l.tx_session_withdrawn(app)));

        crate::reset_engine_for_tests();

        assert!(
            !crate::tx_lanes::with(|l| l.tx_claimed_by(app)),
            "reset_context_for_tests left a transaction claim behind: the lane \
             thread-local was not reset"
        );
        assert!(
            !crate::tx_lanes::with(|l| l.tx_session_withdrawn(app)),
            "reset_context_for_tests left a withdrawal tombstone behind: the \
             next phase's session would be destroyed on return instead of parked"
        );
        // Re-claiming is the stronger statement, and it is the one a later
        // phase of a multi-phase test actually makes: `tx_claimed_by` could
        // read false off a half-cleared lane that still refuses a new claim.
        assert!(
            crate::tx_lanes::with_mut(|l| l.try_claim_tx(app)),
            "the app is claimable again after a reset"
        );
    }

    /// The descriptor store is the THIRD thread-local the reset must clear, and
    /// the one furthest from the helper: it moved to `zeroship-data-core` on
    /// 2026-09-02, so a reset that forgot it would leave a stale schema visible
    /// to the next phase of a test - the exact L24 shape, where serving a read
    /// against the wrong descriptor is what drops the projection allowlist.
    #[test]
    fn a_mid_test_reset_drops_an_installed_descriptor() {
        let binding = zeroship_data_orm::binding::DbBinding::cold_start("app_reset_schema");

        zeroship_data_orm::schema_cache::with_mut(|c| {
            c.insert_one(
                &binding,
                "users",
                zeroship_data_sql::value!({ "email": { "type": "string" } }),
            );
        });
        assert!(zeroship_data_orm::schema_cache::with(|c| c.get(&binding, "users")).is_some());

        crate::reset_engine_for_tests();

        assert!(
            zeroship_data_orm::schema_cache::with(|c| c.get(&binding, "users")).is_none(),
            "reset_context_for_tests left a descriptor entry behind: the schema \
             thread-local in data-core was not reset"
        );
    }
}
