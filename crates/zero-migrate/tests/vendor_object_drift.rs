//! Functions, policies and triggers are visible to structural drift.
//!
//! Three maps on `SchemaSnapshot` were filled ONLY by the offline fold. The live
//! PostgreSQL snapshot never read `pg_proc`, `pg_policy` or `pg_trigger`, and the
//! structural diff compared none of them. So an out-of-band `DROP POLICY` left a
//! table with row-level security ENABLED and nothing enforcing it, and drift
//! reported the schema clean.
//!
//! **IDENTITY, NEVER BODIES.** PostgreSQL does not store any of these as written.
//! Measured on PostgreSQL 18.4:
//!
//! ```text
//!   authored  CREATE FUNCTION spk.f(x int) ...
//!   catalog   spk.f(x integer)
//!   authored  USING (owner = current_user AND v > 0)
//!   catalog   ((owner = CURRENT_USER) AND (v > 0))
//!   authored  WHEN (NEW.v > 0)
//!   catalog   WHEN ((new.v > 0))
//! ```
//!
//! Comparing that text is the same defect `constraint_definition_is_comparable`
//! and `index_expression_bodies_are_comparable` already answer for a CHECK body and
//! a partial-index predicate: permanent false drift on every project. So the
//! comparison surface is the STRUCTURAL residue - what a policy is FOR, who it
//! applies TO, whether it is permissive; a trigger's timing and event set; a
//! function's canonicalised argument vector - and the predicates and bodies are not
//! collected at all.
//!
//! **THE ABSENT-SIDE RULE.** `SchemaSnapshot::vendor_objects` is an `Option`.
//! `None` means the snapshot does not speak about these objects - a SQLite or MySQL
//! catalog read, or a fold for either dialect - and the diff compares nothing.
//! `Some` means the side is authoritative, so an EMPTY map is the positive claim
//! that the schema holds none, and a policy that has vanished from it is drift.
//! Collapsing those two into one empty map is what would make a dropped policy
//! indistinguishable from an engine that has no policies.
//!
//! That `Option` is also why the field is EXCLUDED from `SchemaSnapshot`'s
//! hand-written `PartialEq`, alongside the three authored maps it projects from.
//! Equality has no skip and cannot be given one - making `None` equal `Some({})`
//! would break the transitivity `Eq` requires - so it would either report a false
//! difference or destroy the distinction. Drift does not need it: the diff compares
//! the field itself, and `fold_roundtrip_pg::trigger_and_function_lifecycle` runs
//! that comparison against a live catalog.

mod support;

use zero_migrate::model::ir::{PolicyCmd, TriggerEvent, TriggerTiming};
use zero_migrate::model::snapshot::{
    FunctionKey, PolicyIdentity, PolicyKey, SchemaSnapshot, TriggerIdentity, TriggerKey,
    VendorObjectIdentities,
};
use zero_migrate::schema::query::SqlDialect;

fn function(name: &str, arg_types: &[&str]) -> FunctionKey {
    FunctionKey {
        schema: "app".to_string(),
        name: name.to_string(),
        arg_types: arg_types.iter().map(|t| (*t).to_string()).collect(),
    }
}

fn policy_key(name: &str, table: &str) -> PolicyKey {
    PolicyKey {
        schema: "app".to_string(),
        table: table.to_string(),
        name: name.to_string(),
    }
}

fn trigger_key(name: &str, table: &str) -> TriggerKey {
    TriggerKey {
        schema: "app".to_string(),
        table: table.to_string(),
        name: name.to_string(),
    }
}

fn select_policy() -> PolicyIdentity {
    PolicyIdentity {
        for_cmd: PolicyCmd::Select,
        to: Vec::new(),
        permissive: true,
    }
}

fn audit_trigger() -> TriggerIdentity {
    TriggerIdentity {
        timing: TriggerTiming::After,
        events: vec![TriggerEvent::Insert, TriggerEvent::Update],
    }
}

/// A snapshot that DOES speak about vendor objects, holding exactly what is given.
fn speaking(vendor: VendorObjectIdentities) -> SchemaSnapshot {
    SchemaSnapshot {
        vendor_objects: Some(vendor),
        ..SchemaSnapshot::default()
    }
}

fn one_of_each() -> VendorObjectIdentities {
    let mut vendor = VendorObjectIdentities::default();
    vendor.functions.insert(function("audit", &["int4"]));
    vendor
        .policies
        .insert(policy_key("orders_owner", "orders"), select_policy());
    vendor
        .triggers
        .insert(trigger_key("orders_audit", "orders"), audit_trigger());
    vendor
}

#[test]
fn a_dropped_policy_is_reported_as_drift() {
    let expected = speaking(one_of_each());
    let mut actual_vendor = one_of_each();
    actual_vendor.policies.clear();
    let drift = zero_migrate::diff_snapshots(&expected, &speaking(actual_vendor));

    assert!(
        !drift.is_clean(),
        "a policy dropped out of band must not read as a clean schema"
    );
    assert!(
        drift
            .missing_objects
            .iter()
            .any(|o| o == "policy orders_owner on app.orders"),
        "the dropped policy must be named in missing_objects: {:#?}",
        drift.missing_objects
    );
}

#[test]
fn a_dropped_function_and_trigger_are_reported_as_drift() {
    let expected = speaking(one_of_each());
    let mut actual_vendor = one_of_each();
    actual_vendor.functions.clear();
    actual_vendor.triggers.clear();
    let drift = zero_migrate::diff_snapshots(&expected, &speaking(actual_vendor));

    assert!(
        drift
            .missing_objects
            .iter()
            .any(|o| o == "function app.audit(int4)"),
        "the dropped function must be named in missing_objects: {:#?}",
        drift.missing_objects
    );
    assert!(
        drift
            .missing_objects
            .iter()
            .any(|o| o == "trigger orders_audit on app.orders"),
        "the dropped trigger must be named in missing_objects: {:#?}",
        drift.missing_objects
    );
}

#[test]
fn an_out_of_band_creation_is_reported_as_unexpected() {
    // The other direction: the live database holds objects no migration authored.
    let expected = speaking(VendorObjectIdentities::default());
    let drift = zero_migrate::diff_snapshots(&expected, &speaking(one_of_each()));

    let mut unexpected = drift.unexpected_objects;
    unexpected.sort();
    assert_eq!(
        unexpected,
        vec![
            "function app.audit(int4)".to_string(),
            "policy orders_owner on app.orders".to_string(),
            "trigger orders_audit on app.orders".to_string(),
        ],
        "every hand-created vendor object must be reported"
    );
}

#[test]
fn a_policy_whose_scope_roles_or_permissiveness_changed_is_reported_as_altered() {
    let expected = speaking(one_of_each());
    let mut actual_vendor = one_of_each();
    actual_vendor.policies.insert(
        policy_key("orders_owner", "orders"),
        PolicyIdentity {
            for_cmd: PolicyCmd::All,
            to: vec!["analyst".to_string()],
            permissive: false,
        },
    );
    let drift = zero_migrate::diff_snapshots(&expected, &speaking(actual_vendor));

    // `diff_snapshots` sorts its report, so this is the set, in its sorted order.
    let fields: Vec<&str> = drift
        .altered_objects
        .iter()
        .filter(|a| a.object == "policy orders_owner on app.orders")
        .map(|a| a.field.as_str())
        .collect();
    assert_eq!(
        fields,
        vec!["command", "permissive", "roles"],
        "all three comparable policy facets must be reported: {:#?}",
        drift.altered_objects
    );
    let command = drift
        .altered_objects
        .iter()
        .find(|a| a.field == "command")
        .expect("a command divergence");
    assert_eq!(command.table, "orders");
    assert_eq!(command.expected, "SELECT");
    assert_eq!(command.actual, "ALL");
}

#[test]
fn a_trigger_whose_timing_or_events_changed_is_reported_as_altered() {
    let expected = speaking(one_of_each());
    let mut actual_vendor = one_of_each();
    actual_vendor.triggers.insert(
        trigger_key("orders_audit", "orders"),
        TriggerIdentity {
            timing: TriggerTiming::Before,
            events: vec![TriggerEvent::Delete],
        },
    );
    let drift = zero_migrate::diff_snapshots(&expected, &speaking(actual_vendor));

    let fields: Vec<&str> = drift
        .altered_objects
        .iter()
        .filter(|a| a.object == "trigger orders_audit on app.orders")
        .map(|a| a.field.as_str())
        .collect();
    assert_eq!(
        fields,
        vec!["events", "timing"],
        "both comparable trigger facets must be reported: {:#?}",
        drift.altered_objects
    );
    let events = drift
        .altered_objects
        .iter()
        .find(|a| a.field == "events")
        .expect("an events divergence");
    assert_eq!(events.expected, "INSERT OR UPDATE");
    assert_eq!(events.actual, "DELETE");
}

#[test]
fn identical_vendor_objects_are_clean() {
    // The control that stops every test above passing because EVERYTHING drifts.
    let drift = zero_migrate::diff_snapshots(&speaking(one_of_each()), &speaking(one_of_each()));
    assert!(
        drift.is_clean(),
        "an unchanged schema is not drift: {drift:#?}"
    );
}

#[test]
fn a_re_ordered_trigger_event_list_is_not_drift() {
    // `pg_trigger.tgtype` is a BIT SET, so the authored order is unrecoverable and
    // both sides normalise. Without that, `INSERT OR UPDATE` and `UPDATE OR INSERT`
    // would report drift on a trigger nobody touched.
    let expected = speaking(one_of_each());
    let mut actual_vendor = one_of_each();
    actual_vendor.triggers.insert(
        trigger_key("orders_audit", "orders"),
        TriggerIdentity {
            timing: TriggerTiming::After,
            events: TriggerIdentity::sorted_events(vec![
                TriggerEvent::Update,
                TriggerEvent::Insert,
            ]),
        },
    );
    assert!(
        zero_migrate::diff_snapshots(&expected, &speaking(actual_vendor)).is_clean(),
        "the event ORDER is not a facet PostgreSQL retains"
    );
}

#[test]
fn an_alias_spelled_argument_type_is_not_drift() {
    // The catalog rewrites `int` to `integer` and drops a type modifier, so a
    // signature compared as authored text would report the SAME function as one
    // missing and one unexpected, on every snapshot, forever.
    let mut expected_vendor = VendorObjectIdentities::default();
    expected_vendor
        .functions
        .insert(function("audit", &["int", "varchar(255)"]).canonicalized());
    let mut actual_vendor = VendorObjectIdentities::default();
    actual_vendor
        .functions
        .insert(function("audit", &["integer", "character varying"]).canonicalized());

    assert!(
        zero_migrate::diff_snapshots(&speaking(expected_vendor), &speaking(actual_vendor))
            .is_clean(),
        "an alias spelling is not a different function"
    );
}

#[test]
fn a_dialect_that_does_not_introspect_them_reports_nothing() {
    // SQLite and MySQL leave `vendor_objects` at `None`, and so does a fold for
    // either dialect. An expected side that knows about a policy must not accuse
    // them of having lost it - this is the false-drift trap the row-level-security
    // work already paid for, closed here by the same absent-side rule rather than
    // by naming a dialect.
    let expected = speaking(one_of_each());
    let silent = SchemaSnapshot::default();
    assert!(
        zero_migrate::diff_snapshots(&expected, &silent).is_clean(),
        "an engine that never reads pg_policy must not report policy drift"
    );
    assert!(
        zero_migrate::diff_snapshots(&silent, &expected).is_clean(),
        "and neither must the mirror case, where only the LIVE side speaks"
    );
}

#[test]
fn looking_and_finding_none_is_not_the_same_claim_as_not_looking() {
    // The whole model rests on this. `Some({})` asserts the schema holds no vendor
    // objects and so DOES contradict a side that has one; `None` asserts nothing and
    // contradicts nothing. Collapsing them would make a dropped policy
    // indistinguishable from an engine that never reads `pg_policy`.
    let has_one = speaking(one_of_each());
    let found_none = speaking(VendorObjectIdentities::default());
    let did_not_look = SchemaSnapshot::default();

    assert!(
        !zero_migrate::diff_snapshots(&has_one, &found_none).is_clean(),
        "a side that looked and found none contradicts a side that has one"
    );
    assert!(
        zero_migrate::diff_snapshots(&has_one, &did_not_look).is_clean(),
        "a side that did not look contradicts nothing"
    );

    // And this is why the field stays OUT of `SchemaSnapshot`'s hand-written
    // `PartialEq`, alongside the three authored maps it projects from. Equality has
    // no skip, and it cannot be taught one: making `None` equal `Some({})` would
    // break the transitivity `Eq` requires. Comparing strictly instead would make
    // every hand-built expectation unequal to a fold for not having thought about
    // the field. Drift does not need equality - it compares the field itself.
    assert_eq!(
        has_one, found_none,
        "structural equality deliberately does not speak about vendor objects"
    );
}

#[test]
fn folding_onto_a_base_that_looked_does_not_erase_the_claim() {
    // `fold_ops_onto` is a CONTINUATION, so the same carry-through `table_rls` gets:
    // a base that asserted "no policies here" must not have that assertion dropped
    // by folding a no-op onto it under another dialect, which would silently turn a
    // comparable snapshot into a silent one.
    let base = SchemaSnapshot {
        vendor_objects: Some(VendorObjectIdentities::default()),
        ..SchemaSnapshot::default()
    };
    for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
        let folded = zero_migrate::render::fold::fold_ops_onto(
            &base,
            &[],
            dialect,
            "app",
            &support::no_inject("app"),
        )
        .expect("an empty op list folds");
        assert!(
            folded.vendor_objects.is_some(),
            "{dialect:?}: folding onto a base that looked must keep saying so"
        );
    }

    // The mirror: a base that never looked stays silent off PostgreSQL, which is
    // what keeps the SQLite and MySQL round-trip oracles comparing like with like.
    let silent = SchemaSnapshot::default();
    for (dialect, expected) in [
        (SqlDialect::Postgres, true),
        (SqlDialect::Sqlite, false),
        (SqlDialect::Mysql, false),
    ] {
        let folded = zero_migrate::render::fold::fold_ops_onto(
            &silent,
            &[],
            dialect,
            "app",
            &support::no_inject("app"),
        )
        .expect("an empty op list folds");
        assert_eq!(
            folded.vendor_objects.is_some(),
            expected,
            "{dialect:?}: only the engine whose catalog is read may claim to have looked"
        );
    }
}
