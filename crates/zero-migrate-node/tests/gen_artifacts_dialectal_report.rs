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
fn an_absent_target_leg_fails_closed() {
    for dialect in ["sqlite", "mysql"] {
        let reply = gen_artifacts_from_envelopes(
            &postgres_only_leg_history(),
            dialect,
            Some(SCHEMA),
            &[charter().as_str()],
        );
        assert!(
            !reply.ok,
            "{dialect} must refuse a dialectal op without its target leg: {:?}",
            reply.error
        );
        assert_eq!(
            reply.has_dialectal_ops, None,
            "a refused fold has no dialectal-op answer",
        );
        assert!(
            reply.env_db_ts.is_none() && reply.runtime_json.is_none(),
            "a refused fold must emit no artifacts",
        );
        assert!(
            reply
                .error
                .as_deref()
                .is_some_and(|error| error.contains("dialectal op has no leg for target dialect")),
            "the refusal must name the absent target leg: {:?}",
            reply.error,
        );
    }
}

#[test]
fn pg_alias_and_misspelled_postgres_leg_fail_closed() {
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
            !reply.ok,
            "{wrong_key:?} is not a postgres alias and must leave postgres uncovered: {:?}",
            reply.error
        );
        assert_eq!(reply.has_dialectal_ops, None);
        assert!(
            reply.env_db_ts.is_none() && reply.runtime_json.is_none(),
            "a misspelled leg must emit no artifacts",
        );
        assert!(
            reply
                .error
                .as_deref()
                .is_some_and(|error| error.contains("dialectal op has no leg for target dialect")),
            "{wrong_key:?} must fail as an absent postgres leg: {:?}",
            reply.error,
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
