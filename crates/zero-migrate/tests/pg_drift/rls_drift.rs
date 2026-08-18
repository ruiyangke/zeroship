//! Row-level security is visible to structural drift.
//!
//! `setRls` wrote row-level security and nothing ever read it back: the live
//! snapshot did not introspect `pg_class.relrowsecurity`, the expected snapshot
//! had nowhere to record it, and the diff compared neither. So disabling RLS out
//! of band left every policy on the table unenforced while drift reported clean.
//!
//! THE ORDER THE PIECES HAD TO LAND IN is the interesting part. Collecting the
//! live state, folding the authored state, and comparing them are three separate
//! changes, and comparing before both sides are populated is WORSE than the blind
//! spot - one side empty against the other full marks every RLS-enabled table as
//! drifted, forever. Each half shipped and gated on its own before the diff was
//! wired.
//!
//! THE ABSENT-SIDE RULE. The diff skips any table the ACTUAL snapshot does not
//! mention. That is what makes this safe on SQLite and MySQL, which have no
//! row-level security at all and therefore leave the map empty - they cannot
//! report drift on a facet they do not have. It also covers a live snapshot that
//! simply did not reach a table.

use zero_migrate::model::snapshot::SchemaSnapshot;

fn snapshot_with(rls: &[(&str, bool)]) -> SchemaSnapshot {
    let mut snap = SchemaSnapshot::default();
    for (table, enabled) in rls {
        snap.table_rls.insert((*table).to_string(), *enabled);
    }
    snap
}

#[test]
fn an_rls_difference_is_reported_as_drift() {
    let expected = snapshot_with(&[("orders", true)]);
    let actual = snapshot_with(&[("orders", false)]);
    let drift = zero_migrate::diff_snapshots(&expected, &actual);
    assert!(
        !drift.is_clean(),
        "RLS turned off out of band must not read as a clean schema"
    );
    let altered = drift
        .altered_objects
        .iter()
        .find(|a| a.field == "row_level_security")
        .expect("the divergence must be reported against the row_level_security field");
    assert_eq!(altered.table, "orders");
    assert_eq!(altered.expected, "true");
    assert_eq!(altered.actual, "false");
}

#[test]
fn matching_rls_state_is_clean() {
    // The control that stops the test above passing because EVERYTHING drifts.
    let drift = zero_migrate::diff_snapshots(
        &snapshot_with(&[("orders", true)]),
        &snapshot_with(&[("orders", true)]),
    );
    assert!(drift.is_clean(), "identical RLS state is not drift");
}

#[test]
fn a_dialect_without_row_level_security_cannot_drift_on_it() {
    // SQLite and MySQL leave the map empty. An expected side that knows about RLS
    // must not accuse them of having lost it - this is the false-drift trap that
    // sank the first design, and it is closed by skipping tables the actual side
    // never mentions rather than by special-casing dialects.
    let expected = snapshot_with(&[("orders", true)]);
    let actual = SchemaSnapshot::default();
    assert!(
        zero_migrate::diff_snapshots(&expected, &actual).is_clean(),
        "an engine with no row-level security must not report RLS drift"
    );
}
