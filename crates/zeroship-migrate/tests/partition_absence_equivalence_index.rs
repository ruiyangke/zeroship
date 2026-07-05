const ABSENCE_EQUIVALENCE_PROOF_LEGS: &[(&str, &str)] = &[
    (
        "range child drop collapse",
        "partition_render::collapse_bounded_child_drop_deletes_bound_rows_on_sqlite",
    ),
    (
        "default residual drop collapse",
        "partition_render::collapse_default_child_drop_deletes_residual_rows_on_sqlite",
    ),
    (
        "mirror guard dirty and clean arms",
        "partition_render::collapse_create_bounded_child_mirror_guard_errors_only_when_default_has_matching_rows_sqlite",
    ),
    (
        "auto-down round trip",
        "partition_render::collapse_child_create_down_drops_rows_before_parent_drop_on_sqlite",
    ),
    (
        "minValue lower-bound absence",
        "partition_render::collapse_range_min_value_omits_lower_delete_bound_on_sqlite",
    ),
    (
        "collapsed table no child-DDL absence",
        "partition_render::collapse_affirmed_events_apply_as_plain_table_on_sqlite",
    ),
];

#[test]
fn partition_absence_equivalence_suite_index_is_discoverable() {
    assert_eq!(
        ABSENCE_EQUIVALENCE_PROOF_LEGS.len(),
        6,
        "the named partition absence-equivalence index should list each proof leg"
    );
    for (claim, test_name) in ABSENCE_EQUIVALENCE_PROOF_LEGS {
        assert!(!claim.trim().is_empty(), "claim label must be explicit");
        assert!(
            test_name.starts_with("partition_render::"),
            "{claim} should reference the existing partition_render proof, got {test_name}"
        );
    }
}
