//! **`genArtifacts` reports whether the history it folded is dialect-sensitive.**
//!
//! `GenArtifactsSource.dialect` is REQUIRED and has no default, because the fold
//! selects `Op::Dialectal` legs and a history authored with
//! `dialect({ postgres, mysql })`
//! yields a different column set per target. A host that does not know its target
//! therefore cannot call this verb correctly - but nothing in the reply let it find
//! out whether the value it supplied mattered, so a host that hardcodes one is right
//! or wrong silently. `has_dialectal_ops` is that missing fact.
//!
//! WHAT THE FIELD REPORTS: on a successful fold, the history CONTAINS an op-level
//! `dialect()` wrapper. A wrapper without a leg for the target fails closed, so a
//! refused call reports no answer rather than treating an absent leg as a no-op.
//!
//! WHAT `false` DOES NOT MEAN: that the artifacts are dialect-independent. Other
//! fold rules key on the dialect too - the materialized enum/domain capability gates
//! and the identity/primary-key reuse rules, per the `render_artifacts` contract at
//! `crates/zero-migrate-core/src/render/gen_types.rs`. The field answers one narrow
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

/// The same history plus a column authored inside a POSTGRES-ONLY `dialect()` leg. The
/// plain `createTable` is kept so every dialect still folds something and the arms
/// differ only in the wrapper, not in whether the fold has work to do.
fn postgres_only_leg_history() -> Vec<Value> {
    let mut history = leg_free_history();
    history.push(json!({
        "ir_version": 1,
        "name": "postgres_only_column",
        "ops": [{
            "op": "dialectal",
            "legs": {
                "postgres": [{
                    "op": "addColumn",
                    "table": "notes",
                    "column": "postgres_only",
                    "type": "text",
                }],
            },
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
        &postgres_only_leg_history(),
        "postgres",
        Some(SCHEMA),
        &[charter().as_str()],
    );
    assert!(
        reply.ok,
        "postgres leg under postgres should fold: {:?}",
        reply.error
    );
    assert_eq!(
        reply.has_dialectal_ops,
        Some(true),
        "the history carries a dialect() wrapper and postgres selected its leg",
    );
}

/// A target absent from `legs` is a refusal, not an empty op.
#[test]
fn an_absent_target_leg_emits_nothing() {
    for dialect in ["sqlite", "mysql"] {
        let reply = gen_artifacts_from_envelopes(
            &postgres_only_leg_history(),
            dialect,
            Some(SCHEMA),
            &[charter().as_str()],
        );
        assert!(
            reply.ok,
            "{dialect} must fold the history and skip the absent op leg: {:?}",
            reply.error
        );
        assert_eq!(
            reply.has_dialectal_ops,
            Some(true),
            "{dialect} selected no leg, but the dialect argument is what emptied the op",
        );
        // The fold SUCCEEDED, so it owes artifacts. Asserting their presence is the
        // inverse of the fail-closed spelling's emptiness check, and it is what
        // separates "skipped one op" from "produced nothing at all".
        assert!(
            reply.env_db_ts.is_some() && reply.runtime_json.is_some(),
            "a successful fold must still emit its artifacts on {dialect}",
        );
        assert!(
            !reply
                .runtime_json
                .as_deref()
                .expect("successful fold renders runtime JSON")
                .contains("postgres_only"),
            "{dialect} must not select the postgres-only column",
        );
    }
}

/// An unregistered leg key is INDISTINGUISHABLE from a deliberate skip, by design.
///
/// `pg` and `postgre` are well-formed [`DialectId`] strings that match no backend,
/// so they contribute nothing on every target and the fold reports success. That is
/// the accepted cost of the emit-nothing rule: refusing here would mean that
/// SHIPPING a new backend retroactively refuses every migration authored before it
/// existed, and migrations are checksummed history that cannot be edited forward.
///
/// The typo hazard this leaves open is real and is tracked separately: the fix is to
/// reject unregistered leg keys at AUTHORING time only, never on replay of recorded
/// history.
#[test]
fn pg_alias_and_misspelled_postgres_leg_emit_nothing() {
    for wrong_key in ["pg", "postgre"] {
        let mut history = postgres_only_leg_history();
        let legs = history[1]["ops"][0]["legs"]
            .as_object_mut()
            .expect("fixture carries a legs map");
        let postgres = legs
            .remove("postgres")
            .expect("fixture carries the canonical postgres leg");
        legs.insert(wrong_key.to_string(), postgres);

        let reply =
            gen_artifacts_from_envelopes(&history, "postgres", Some(SCHEMA), &[charter().as_str()]);
        assert!(
            reply.ok,
            "{wrong_key:?} is an unselected op leg, not a postgres alias: {:?}",
            reply.error
        );
        assert_eq!(reply.has_dialectal_ops, Some(true));
        assert!(
            !reply
                .runtime_json
                .as_deref()
                .expect("successful fold renders runtime JSON")
                .contains("postgres_only"),
            "{wrong_key:?} must not select the postgres-only column",
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
