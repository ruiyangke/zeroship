//! **`genArtifacts` reports whether the history it folded is dialect-sensitive.**
//!
//! `GenArtifactsSource.dialect` is REQUIRED and has no default, because the fold
//! selects `Op::Dialectal` legs and a history authored with `dialect({ pg, mysql })`
//! yields a different column set per target. A host that does not know its target
//! therefore cannot call this verb correctly - but nothing in the reply let it find
//! out whether the value it supplied mattered, so a host that hardcodes one is right
//! or wrong silently. `has_dialectal_ops` is that missing fact.
//!
//! WHAT THE FIELD REPORTS, and the distinction the arms below exist to pin: the
//! history CONTAINS an op-level `dialect()` wrapper - NOT that a leg was selected.
//! Those come apart in the case that matters most. A pg-only wrapper folded under
//! SQLite selects nothing at all (`selected_dialectal_leg` finds no `sqlite` leg and
//! no `default`, so the op contributes zero ops and the fold succeeds), which is
//! exactly the silent divergence a caller needs told about. A field reporting
//! SELECTION would read `false` there - the wrong answer at the only moment the
//! answer is load-bearing.
//!
//! WHAT `false` DOES NOT MEAN: that the artifacts are dialect-independent. Other
//! fold rules key on the dialect too - the materialized enum/domain capability gates
//! and the identity/primary-key reuse rules, per the `render_artifacts` contract at
//! `crates/zero-migrate/src/render/gen_types.rs`. The field answers one narrow
//! question and the reply doc says so.
//!
//! Runs on the napi-free build (`--no-default-features`), so no `.node` and no Node
//! runtime are needed.

mod support;

use serde_json::{json, Value};

use zero_migrate_node::api::gen_artifacts_from_envelopes;

const SCHEMA: &str = "public";

fn charter() -> String {
    support::no_inject_charter_toml(SCHEMA)
}

/// A history with no `dialect()` wrapper anywhere.
fn leg_free_history() -> Vec<Value> {
    vec![json!({
        "ir_version": 1,
        "name": "create_notes",
        "ops": [{
            "op": "createTable",
            "name": "notes",
            "columns": [{ "name": "body", "type": "text" }],
        }],
    })]
}

/// The same history plus a column authored inside a PG-ONLY `dialect()` leg. The
/// plain `createTable` is kept so every dialect still folds something and the arms
/// differ only in the wrapper, not in whether the fold has work to do.
fn pg_only_leg_history() -> Vec<Value> {
    let mut history = leg_free_history();
    history.push(json!({
        "ir_version": 1,
        "name": "pg_only_column",
        "ops": [{
            "op": "dialectal",
            "pg": [{
                "op": "addColumn",
                "table": "notes",
                "column": "pg_only",
                "type": "text",
            }],
        }],
    }));
    history
}

#[test]
fn a_history_with_no_dialectal_leg_reports_false() {
    let reply = gen_artifacts_from_envelopes(
        &leg_free_history(),
        "postgres",
        Some(SCHEMA),
        &[charter().as_str()],
    );
    assert!(reply.ok, "leg-free history should fold: {:?}", reply.error);
    assert_eq!(
        reply.has_dialectal_ops,
        Some(false),
        "no dialect() wrapper in the history, so the dialect argument changed no leg selection",
    );
}

#[test]
fn a_selected_dialectal_leg_reports_true() {
    let reply = gen_artifacts_from_envelopes(
        &pg_only_leg_history(),
        "postgres",
        Some(SCHEMA),
        &[charter().as_str()],
    );
    assert!(
        reply.ok,
        "pg leg under postgres should fold: {:?}",
        reply.error
    );
    assert_eq!(
        reply.has_dialectal_ops,
        Some(true),
        "the history carries a dialect() wrapper and postgres selected its leg",
    );
}

/// The arm that makes PRESENCE the right predicate rather than SELECTION.
///
/// Under SQLite and MySQL the pg-only wrapper matches no leg and has no `default`,
/// so it contributes nothing and the fold still succeeds. The dialect argument
/// decided the whole content of that op, so the honest answer is still `true`.
/// Re-implementing the field as "a leg was selected" flips these two to `false` and
/// this is the test that catches it.
#[test]
fn an_unselected_dialectal_leg_still_reports_true() {
    for dialect in ["sqlite", "mysql"] {
        let reply = gen_artifacts_from_envelopes(
            &pg_only_leg_history(),
            dialect,
            Some(SCHEMA),
            &[charter().as_str()],
        );
        assert!(
            reply.ok,
            "{dialect} should fold the history and skip the absent leg: {:?}",
            reply.error
        );
        assert_eq!(
            reply.has_dialectal_ops,
            Some(true),
            "{dialect} selected no leg, but the dialect argument is what emptied the op",
        );
    }
}

/// `None` rather than `Some(false)` on failure, so a host cannot read a refusal as
/// "this history is dialect-free". The consumer-side assertion is `=== false`, which
/// an absent field must not satisfy.
#[test]
fn a_refused_call_reports_no_answer_rather_than_false() {
    let reply = gen_artifacts_from_envelopes(
        &leg_free_history(),
        "oracle",
        Some(SCHEMA),
        &[charter().as_str()],
    );
    assert!(!reply.ok, "an unknown dialect spelling is refused");
    assert_eq!(
        reply.has_dialectal_ops, None,
        "a refusal folded nothing, so it has no answer to report",
    );
}
