use crate::{protection, tx_lanes};

pub(crate) fn native_fields(fields: crate::value::Value) -> crate::schema::FieldMap {
    crate::schema::CollectionSchema::from_fields(&fields)
        .unwrap()
        .into_fields()
}

pub(crate) fn generated_schema(fields: crate::value::Value) -> crate::schema::FieldMap {
    native_fields(super::schema::generated_fields(fields))
}

/// Install a descriptor for an isolated test binding.
pub(crate) fn cache_schema(app_id: &str, collection: &str, schema: crate::value::Value) {
    cache_schema_for_deploy(
        &crate::tests::fixtures::harness_binding(app_id),
        collection,
        schema,
    );
}

/// Test helper: [`cache_schema`] for an explicit binding, so a
/// fixture can install two deploys of one app and assert they do not see each
/// other's descriptor entries.
pub(crate) fn cache_schema_for_deploy(
    binding: &zeroship_data_orm::binding::DbBinding,
    collection: &str,
    schema: crate::value::Value,
) {
    zeroship_data_orm::schema_cache::with_mut(|c| {
        c.insert_one(binding, collection, generated_schema(schema))
    });
}

pub(crate) fn reset_engine() {
    tx_lanes::reset_for_tests();
    protection::mask_policy::reset_for_tests();
    protection::protection_floor::reset_for_tests();
    zeroship_data_orm::schema_cache::reset_for_tests();
}

#[cfg(test)]
mod tests {
    /// Deleting `tx_lanes::reset_for_tests()` from `reset_engine`
    /// must fail this.
    ///
    /// Claims and withdrawal tombstones have different lifetimes, so a partial
    /// reset could plausibly clear only one store: the
    /// lane map is emptied by ordinary retirement, whereas the withdrawal
    /// tombstone is documented to outlive its lane and to be cleared only by
    /// the next `admit_transaction`. The tombstone is therefore the residue
    /// most likely to survive a reset that looks correct.
    #[test]
    fn a_mid_test_reset_drops_a_claim_and_a_withdrawal_tombstone() {
        let app = &crate::tests::fixtures::harness_route("app_reset_guard");

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

        crate::tests::fixtures::reset_engine();

        assert!(
            !crate::tx_lanes::with(|l| l.tx_claimed_by(app)),
            "reset_engine left a transaction claim behind: the lane \
             thread-local was not reset"
        );
        assert!(
            !crate::tx_lanes::with(|l| l.tx_session_withdrawn(app)),
            "reset_engine left a withdrawal tombstone behind: the \
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

    /// Reset must clear descriptors as well as transaction state. A stale schema
    /// could otherwise expose the wrong projection in the next test phase.
    #[test]
    fn a_mid_test_reset_drops_an_installed_descriptor() {
        let binding = crate::tests::fixtures::harness_binding("app_reset_schema");

        zeroship_data_orm::schema_cache::with_mut(|c| {
            c.insert_one(
                &binding,
                "users",
                super::native_fields(crate::value!({ "email": { "type": "string" } })),
            );
        });
        assert!(zeroship_data_orm::schema_cache::with(|c| c.get(&binding, "users")).is_some());

        crate::tests::fixtures::reset_engine();

        assert!(
            zeroship_data_orm::schema_cache::with(|c| c.get(&binding, "users")).is_none(),
            "reset_engine left a descriptor entry behind: the descriptor \
             store was not reset"
        );
    }
}
