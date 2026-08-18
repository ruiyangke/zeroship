//! **Step 4, consumer 3: the wire `FieldDef` map comes from the single fold.**
//!
//! `docs/proposals/single-fold-and-effects.md` section G step 4 moves the consumers off
//! their private walkers one at a time, in ascending blast radius.
//! `fold_to_field_defs` is the third and the first that is not artifact-only: its
//! output is `schema.runtime.json`'s `fields` block AND, on SQLite,
//! `LiveSchema::sqlite_schemas`, which the 12-step table rebuild renders its new
//! `CREATE TABLE` from. The proposal orders it third for exactly that reason
//! (section I: "Step 4 touches SQLite rebuild DDL").
//!
//! This file is the OFFLINE half and always runs. The live half is
//! `tests/sqlite_rebuild_field_defs_live.rs`, which deploys to a real SQLite file
//! through the shipped engine and reads the server's own `PRAGMA`s back.
//!
//! # What the move changes, measured rather than assumed
//!
//! A sweep of `fold_to_field_defs` against `FoldedSchema::project_field_defs` over
//! EVERY PREFIX of the 27 recorded fixtures and the carriers in
//! `tests/support/field_defs_corpus.rs`, on three dialects, compared 486 prefix/dialect
//! pairs (216 more were refused by BOTH, and there was no prefix on which one refused
//! and the other did not) and reported FIVE divergence families. The 27 recorded
//! fixtures contributed ZERO of them - every one is a carrier - which is the same shape
//! consumer 2 found and the reason the carriers exist.
//!
//! That sweep ran against 22 carriers; `column_level_reference_policy` was added
//! afterwards, when this file's coverage floor reported that the un-lift fix had removed
//! the golden's LAST `onDelete` row and left the facet unprotected. It is a positive
//! control rather than a divergence, and the golden was re-captured from the walker to
//! record it rather than blessed from the new path.
//!
//! Every family is one rule: the walker LIFTED a constraint's facet onto a column
//! eagerly and kept a private side map to un-lift it from, and that side map was never
//! kept in step with the constraint. The projection reads the constraints the model
//! still holds, so there is nothing to keep in step.
//!
//! | family | the walker | the model |
//! |---|---|---|
//! | a dropped `UNIQUE` constraint | `unique: true` survives it | derived from the live constraint set |
//! | a dropped `CHECK` bound | `min`/`max` survive it | same |
//! | a dropped `CHECK` membership | `enum` survives it | same |
//! | a column dropped and re-added under its old name | inherits the dropped column's FK policy and CHECK bounds | the constraint went with the column |
//! | `dropPartition` | the dropped relation stays in the map | a dropped partition is a dropped relation |
//!
//! Which side is right is not decided here by preference. For the first four it is
//! decided by `fold_ops`, the structural catalog oracle the live PostgreSQL, SQLite and
//! MySQL suites already run against real servers, which has removed the constraint at
//! the prefix where the two answers differ. For `dropPartition` it was decided by a live
//! PostgreSQL in consumer 2's `tests/env_db_ts_matches_the_server_pg.rs` - the relation
//! is gone from `pg_class` and the parent survives - and this move brings the second
//! artifact into line with the first.
//!
//! # The DDL leg's blast radius is EMPTY, and that is a measurement
//!
//! All five families change `schema.runtime.json`. NONE of them can reach a SQLite table
//! rebuild: `no_field_def_divergence_reaches_a_sqlite_rebuild` deploys each of them to a
//! real SQLite database through `MigrationEngine::deploy_envelopes` and each is REFUSED
//! before any DDL is emitted, for four different stated reasons. That is why this move
//! ships with a live SQLite file rather than only a golden - the claim that the most
//! dangerous consumer does not move is worth more than the claim that the artifacts do.
//!
//! # No re-bless affordance
//!
//! There is deliberately no environment variable that rewrites the golden, matching
//! `tests/op_fixture_goldens.rs` and both predecessors. The file was captured from the
//! OLD path by a SEPARATE, since-deleted binary before the consumer was switched, so the
//! side that produced the expectation is not the side under test.

use crate::support;

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::support::field_defs_corpus::{
    corpus_lines, measure_corpus, parse, read_stem, CARRIERS, DIALECTS, SCHEMA, STEMS,
};
use zero_migrate::model::ir::{MigrationIr, Op};
use zero_migrate::{render_artifacts, EffectivePolicy, SqlDialect};

const CORPUS_GOLDEN: &str = "tests/goldens/field_defs_artifacts.txt";

fn manifest_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn golden() -> String {
    let path = manifest_path(CORPUS_GOLDEN);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn golden_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

// ---------------------------------------------------------------------------
// The whole-artifact gate
// ---------------------------------------------------------------------------

/// **The behaviour-preservation gate for the move.**
///
/// 27 recorded streams and 23 carriers under 3 dialects, reduced to a hash per artifact,
/// a line per COLUMN carrying its whole wire `FieldDef`, and - on SQLite - the
/// `CREATE TABLE` the 12-step rebuild would render from the map. The hash and the
/// per-field lines are both here on purpose and neither substitutes for the other: the
/// hash catches a byte moving somewhere the reduction does not classify, and the field
/// lines say WHICH column moved when it does.
#[test]
fn the_recorded_corpus_renders_the_same_field_defs_through_the_fold() {
    let measured = measure_corpus();
    let recorded = golden();
    let expected = golden_lines(&recorded);

    let measured_set: BTreeSet<&str> = measured.iter().map(String::as_str).collect();
    let expected_set: BTreeSet<&str> = expected.iter().copied().collect();
    let unexpected: Vec<&&str> = measured_set.difference(&expected_set).collect();
    let missing: Vec<&&str> = expected_set.difference(&measured_set).collect();
    assert!(
        unexpected.is_empty() && missing.is_empty(),
        "the field defs moved.\n  MISSING (the recorded path emitted these, the fold \
         does not):\n    {}\n  UNEXPECTED (the fold emits these, the recorded path did \
         not):\n    {}",
        missing
            .iter()
            .map(|l| (**l).to_string())
            .collect::<Vec<_>>()
            .join("\n    "),
        unexpected
            .iter()
            .map(|l| (**l).to_string())
            .collect::<Vec<_>>()
            .join("\n    "),
    );
    assert_eq!(
        measured.len(),
        expected.len(),
        "the corpus must not quietly lose or gain rows"
    );
}

/// The corpus is only evidence if it covers the thing that moved, and every floor below
/// counts rows in the GOLDEN FILE rather than in the measurement - so a run in which the
/// code stopped emitting a shape fails here as well as above.
///
/// The floors are named for the shape they protect rather than being round numbers,
/// because a floor nobody can explain is a floor that gets lowered.
#[test]
fn the_corpus_golden_actually_covers_the_map_that_moved() {
    let recorded = golden();
    let lines = golden_lines(&recorded);
    let count = |needle: &str| lines.iter().filter(|line| line.contains(needle)).count();

    // The three artifact-level rows. `env.db.ts` is a CONTROL: it is rendered from
    // consumer 2's projection and this move must not touch it.
    assert!(
        count("|sha|runtime.json|") >= 40,
        "the golden must hash the artifact the map feeds, on most of the corpus: {}",
        count("|sha|runtime.json|")
    );
    assert_eq!(
        count("|sha|runtime.json|"),
        count("|sha|env.db.ts|"),
        "every rendered stream contributes BOTH hashes, or the control is thinner than \
         the subject and a byte could move in the artifact nobody hashed"
    );

    // The DDL leg. Without these rows the golden would pin JSON and say nothing about
    // the `CREATE TABLE` rows are copied through.
    assert!(
        count("|sqlite_create|") >= 10,
        "the golden must carry the rebuilt CREATE TABLE for real tables: {}",
        count("|sqlite_create|")
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("|sqlite_create|") && line.contains("FOREIGN KEY")),
        "including one with a FOREIGN KEY in it, or the golden says nothing about the \
         clause a wrong FK policy would land in"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("|sqlite_create|") && line.contains("NOT NULL")),
        "and one with a NOT NULL, which is the facet a wrong `required` would move"
    );

    // The FACETS. Each of these is a key the map carries into DDL, and each is a key one
    // of the five divergence families could have removed or invented.
    for facet in [
        "\"unique\":true",
        "\"onDelete\"",
        "\"enum\"",
        "\"default\"",
        "\"required\":true",
        "\"maxLength\"",
        "\"generated\"",
        "\"identity\"",
    ] {
        assert!(
            count(facet) > 0,
            "the golden carries no {facet} row at all, so this move could remove that \
             facet everywhere and the gate would stay green"
        );
    }

    // Both outcomes of `render_artifacts`, so the golden pins refusals as well as
    // renders. An all-rendered corpus would say nothing about over-refusal.
    assert!(
        count("|refused|") >= 20,
        "the golden must record refusals too: {}",
        count("|refused|")
    );
}

// ---------------------------------------------------------------------------
// The five divergence families, one probe each, read out of the artifact
// ---------------------------------------------------------------------------

fn runtime_field(
    ops: &[Op],
    dialect: SqlDialect,
    policy: &EffectivePolicy,
    table: &str,
    column: &str,
) -> serde_json::Value {
    let rendered = render_artifacts(ops, dialect, SCHEMA, policy).expect("the stream renders");
    let defs = support::field_defs_corpus::field_defs_from_runtime_json(&rendered.runtime_json);
    defs.get(table)
        .and_then(|schema| schema.get(column))
        .cloned()
        .unwrap_or_else(|| panic!("`{table}.{column}` is absent from schema.runtime.json"))
}

fn carrier(name: &str) -> Vec<Op> {
    CARRIERS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, source)| parse(source))
        .unwrap_or_else(|| panic!("the corpus has no carrier named {name}"))
}

/// FAMILY 1. `fold_to_field_defs` lifted a single-column `UNIQUE` onto the column and
/// had no arm that could take it back, so `schema.runtime.json` kept calling a column
/// unique after the constraint that made it so was dropped.
///
/// The two halves are asserted together because either alone is satisfiable by a bug:
/// "the dropped one is gone" is satisfied by dropping every `unique`, and "the kept one
/// is there" is satisfied by keeping every one.
#[test]
fn a_dropped_unique_constraint_does_not_outlive_itself() {
    let policy = support::no_inject(SCHEMA);
    for dialect in DIALECTS {
        let ops = carrier("unique_constraint_kept");
        assert_eq!(
            runtime_field(&ops, dialect, &policy, "users", "handle").get("unique"),
            None,
            "{dialect:?}: the dropped UNIQUE must not survive in the artifact"
        );
        assert_eq!(
            runtime_field(&ops, dialect, &policy, "users", "email").get("unique"),
            Some(&serde_json::json!(true)),
            "{dialect:?}: and the constraint that was NOT dropped must still be there, \
             or the line above is satisfied by dropping uniqueness everywhere"
        );

        // The COLUMN facet is a different grantor from the constraint, and a
        // `dropConstraint` aimed at another column must not disturb it.
        let ops = carrier("unique_column_facet_survives_a_drop");
        assert_eq!(
            runtime_field(&ops, dialect, &policy, "users", "email").get("unique"),
            Some(&serde_json::json!(true)),
            "{dialect:?}: `unique: true` authored ON THE COLUMN survives a dropConstraint \
             that targeted a different column's constraint"
        );
        assert_eq!(
            runtime_field(&ops, dialect, &policy, "users", "handle").get("unique"),
            None,
            "{dialect:?}: while the one whose constraint was dropped does not"
        );
    }
}

/// FAMILIES 2 and 3. The same un-lift hole on the CHECK-derived facets: a numeric
/// `min`/`max` bound and an `enum` membership.
///
/// PostgreSQL only, and that is measured rather than a convenience: `addConstraint(check)`
/// is refused off Postgres by the fold itself, so these two families do not exist on
/// SQLite or MySQL at all. The refusal is pinned in the golden.
#[test]
fn a_dropped_check_constraint_does_not_outlive_itself() {
    let policy = support::no_inject(SCHEMA);
    let pg = SqlDialect::Postgres;

    let kept = carrier("check_bound_kept");
    let bound = runtime_field(&kept, pg, &policy, "scores", "score");
    assert_eq!(
        (bound.get("min"), bound.get("max")),
        (Some(&serde_json::json!(1.0)), Some(&serde_json::json!(9.0))),
        "the bound really does reach the artifact while the constraint exists, or the \
         drop below proves nothing"
    );

    let dropped = runtime_field(
        &carrier("check_bound_dropped"),
        pg,
        &policy,
        "scores",
        "score",
    );
    assert_eq!(
        (dropped.get("min"), dropped.get("max")),
        (None, None),
        "a dropped CHECK takes its bound with it"
    );

    let membership = runtime_field(
        &carrier("check_membership_dropped"),
        pg,
        &policy,
        "issues",
        "status",
    );
    assert_eq!(membership.get("enum"), None, "and its membership with it");

    for dialect in [SqlDialect::Sqlite, SqlDialect::Mysql] {
        let error = render_artifacts(&carrier("check_bound_dropped"), dialect, SCHEMA, &policy)
            .expect_err("addConstraint(check) is PostgreSQL-only")
            .to_string();
        assert!(
            error.contains("addConstraint(check) is PostgreSQL-only"),
            "{dialect:?}: this family's absence off Postgres is a REFUSAL, which is why \
             it cannot reach a SQLite rebuild: {error}"
        );
    }
}

/// FAMILY 4. The walker kept its recovered CHECK and FK facets in a side map keyed by
/// TABLE and lifted them by column NAME once the whole stream had been walked, so
/// `dropColumn` never took a facet with its column - and a column re-added under the
/// same name inherited it.
///
/// This is the family the recorded corpus was furthest from: it needs a drop AND a
/// re-add of the same name, which no fixture does.
#[test]
fn a_re_added_column_does_not_inherit_the_dropped_columns_constraints() {
    let policy = support::no_inject(SCHEMA);
    for dialect in DIALECTS {
        let field = runtime_field(
            &carrier("fk_column_dropped_and_readded"),
            dialect,
            &policy,
            "orders",
            "owner_id",
        );
        assert_eq!(
            field.get("onDelete"),
            None,
            "{dialect:?}: `addColumn` declares no referential action, and the constraint \
             that did went away with the column it named. Keeping it puts an \
             `ON DELETE CASCADE` in the artifact that no catalog has."
        );
        assert_eq!(
            field.get("refTarget"),
            Some(&serde_json::json!("accounts")),
            "{dialect:?}: the reference the re-added column DOES declare is still there, \
             or the line above is satisfied by losing the column's type"
        );
    }

    let pg = SqlDialect::Postgres;
    let field = runtime_field(
        &carrier("check_column_dropped_and_readded"),
        pg,
        &policy,
        "scores",
        "score",
    );
    assert_eq!(
        (field.get("min"), field.get("max")),
        (None, None),
        "the CHECK half of the same hole: a re-added column inherits no bound"
    );
    assert_eq!(
        field.get("type"),
        Some(&serde_json::json!("int")),
        "and it is still the column `addColumn` declared"
    );
}

/// FAMILY 5. A dropped partition is a dropped RELATION, so it leaves the map.
///
/// Adjudicated against a live PostgreSQL by consumer 2 in
/// `tests/env_db_ts_matches_the_server_pg.rs`: after the drop, `pg_class` no longer
/// holds the child and the parent survives. `env.db.ts` has agreed since that move; this
/// is `schema.runtime.json` catching up, so the two halves of ONE `render_artifacts`
/// call stop disagreeing about which relations exist.
///
/// `detachPartition` is the control that stops "a partition op removes the table" from
/// being applied to the op next door - a detached partition survives as a standalone
/// table under the same name.
#[test]
fn a_dropped_partition_leaves_the_field_def_map_too() {
    let policy = support::no_inject(SCHEMA);
    for dialect in DIALECTS {
        let rendered = render_artifacts(
            &carrier("attached_partition_dropped"),
            dialect,
            SCHEMA,
            &policy,
        )
        .expect("the stream renders");
        let defs = support::field_defs_corpus::field_defs_from_runtime_json(&rendered.runtime_json);
        assert!(
            !defs.contains_key("p1"),
            "{dialect:?}: schema.runtime.json still describes a relation the migration \
             dropped: {:?}",
            defs.keys().collect::<Vec<_>>()
        );
        assert!(
            defs.contains_key("par"),
            "{dialect:?}: the parent must survive its child, or the case proves only that \
             everything went away"
        );
        assert!(
            !rendered.env_db_ts.contains("p1: {"),
            "{dialect:?}: and the two halves of ONE render_artifacts call agree about it"
        );

        let rendered = render_artifacts(
            &carrier("attached_partition_detached"),
            dialect,
            SCHEMA,
            &policy,
        )
        .expect("the stream renders");
        let defs = support::field_defs_corpus::field_defs_from_runtime_json(&rendered.runtime_json);
        assert!(
            defs.contains_key("p1"),
            "{dialect:?}: a DETACHED partition survives as a standalone table, which is \
             what stops the rule above from being applied to the wrong op"
        );
    }
}

// ---------------------------------------------------------------------------
// The over-refusal control
// ---------------------------------------------------------------------------

/// Streams chosen to put BOTH refusal directions under load: a dialect leg selection, a
/// catalog lifecycle refusal, a named-type refusal, and a policy-resolution refusal.
const REFUSAL_PROBES: &[(&str, &str)] = &[
    (
        "dialect_leg_selection",
        r#"[
  {"op":"dialectal","pg":[{"op":"createTable","name":"docs","columns":[{"name":"id","type":"text","nullable":false},{"name":"pg_only","type":"text"}],"primaryKey":["id"]}],"sqlite":[{"op":"createTable","name":"docs","columns":[{"name":"id","type":"text","nullable":false},{"name":"sqlite_only","type":"text"}],"primaryKey":["id"]}],"mysql":[{"op":"createTable","name":"docs","columns":[{"name":"id","type":"text","nullable":false},{"name":"mysql_only","type":"text"}],"primaryKey":["id"]}]}
]"#,
    ),
    (
        "table_created_twice",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]}
]"#,
    ),
    (
        "drop_a_constraint_that_does_not_exist",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text"}],"primaryKey":["id"]},
  {"op":"dropConstraint","table":"users","name":"users_ghost_uq"}
]"#,
    ),
    (
        "duplicate_enum",
        r#"[
  {"op":"createEnum","name":"tier","values":["free","paid"]},
  {"op":"createEnum","name":"tier","values":["free","paid","pro"]}
]"#,
    ),
    (
        "column_names_a_dropped_enum",
        r#"[
  {"op":"createEnum","name":"tier","values":["free"]},
  {"op":"dropEnum","name":"tier"},
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false},{"name":"plan","type":{"enum":{"name":"tier"}}}],"primaryKey":["id"]}
]"#,
    ),
    (
        "enum_recreated_after_drop",
        r#"[
  {"op":"createEnum","name":"tier","values":["free"]},
  {"op":"dropEnum","name":"tier"},
  {"op":"createEnum","name":"tier","values":["free","paid"]},
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false},{"name":"plan","type":{"enum":{"name":"tier"}}}],"primaryKey":["id"]}
]"#,
    ),
    (
        "add_a_column_to_a_missing_table",
        r#"[
  {"op":"addColumn","table":"ghosts","column":"x","type":"text"}
]"#,
    ),
];

fn control_cases() -> Vec<(String, Vec<Op>, bool)> {
    let mut cases: Vec<(String, Vec<Op>, bool)> = Vec::new();
    for stem in STEMS {
        cases.push((format!("fixture:{stem}"), read_stem(stem), true));
    }
    for (name, source) in CARRIERS {
        cases.push((format!("carrier:{name}"), parse(source), false));
    }
    for (name, source) in REFUSAL_PROBES {
        cases.push((format!("probe:{name}"), parse(source), false));
    }
    cases
}

/// **The over-refusal control: `render_artifacts` accepts exactly the streams the
/// coherence oracle accepts, and refuses with the same message.**
///
/// An equality gate is STRUCTURALLY BLIND to a refusal change, because it only compares
/// streams that produced an answer on both sides: a stream that starts erroring simply
/// leaves the sample and the count falls, which reads as green everywhere except in a
/// pinned total. So the property is asserted as a BICONDITIONAL, and both directions
/// panic.
///
/// # Be precise about what this is worth HERE
///
/// It is a REGRESSION GUARD, not a discovery instrument, and the reason is specific.
/// The walker this move deletes ran `fold_ops` itself as its fail-closed gate, and
/// `single_fold::fold` runs the same catalog replay through `fold_ops_onto` before any
/// authored rule executes. So the two refusal sets are equal BY CONSTRUCTION, and that
/// was not a prediction: the sweep behind this move compared the walker and the
/// projection over 702 prefix/dialect pairs and found ZERO on which one refused and the
/// other did not - 486 both answered, 216 both refused.
///
/// What it can still catch, and what no other gate in this change can: the deletion
/// removing a refusal the walker's own authored replay made and the fold's does not -
/// `ResolvedInject::for_table`, `create_enum` and `create_domain` are fallible in both,
/// and an arm that stopped calling one would show up here as UNDER-REFUSAL.
///
/// The comparison is against `fold_ops` rather than against the deleted walker, and that
/// is weaker in one stateable way: `fold_ops` cannot see a refusal the walker's AUTHORED
/// half made that the catalog half does not. What covers that gap is the golden, whose
/// `refused|` lines were captured from the OLD path in full and are compared line for
/// line above.
#[test]
fn the_move_changed_no_refusal_that_the_old_path_already_made() {
    let confined = support::confined_charter();
    let open = support::no_inject(SCHEMA);

    let mut refused = 0_usize;
    let mut accepted = 0_usize;
    for (label, ops, is_confined) in control_cases() {
        let policy: &EffectivePolicy = if is_confined { &confined } else { &open };
        for dialect in DIALECTS {
            let resolved = zero_migrate::resolve_create_table_policy(
                &MigrationIr {
                    inverse_ops: None,
                    irreversible: None,
                    ir_version: zero_migrate::CURRENT_IR_VERSION,
                    name: "over_refusal_control".to_string(),
                    owner_app: String::new(),
                    ops: ops.clone(),
                    flags: Default::default(),
                    depends_on: Vec::new(),
                    supersedes: Vec::new(),
                    preconditions: Vec::new(),
                    checksum: None,
                },
                policy,
                SCHEMA,
            );
            let Ok(resolved) = resolved else {
                // The resolve step is BEFORE the fold and this move did not touch it; a
                // stream it rejects reaches neither side.
                continue;
            };
            let oracle = zero_migrate::fold_ops(&resolved.ops, dialect, SCHEMA, policy);
            let now = render_artifacts(&ops, dialect, SCHEMA, policy);

            match (&oracle, &now) {
                (Ok(_), Ok(_)) => accepted += 1,
                (Err(old), Err(new)) => {
                    refused += 1;
                    assert!(
                        new.to_string().contains(&old.to_string()),
                        "{label}/{dialect:?}: both refuse, but with DIFFERENT messages. \
                         The coherence oracle said:\n  {old}\nand `render_artifacts` now \
                         says:\n  {new}"
                    );
                }
                (Ok(_), Err(new)) => panic!(
                    "{label}/{dialect:?}: OVER-REFUSAL. The coherence oracle accepts this \
                     stream and `render_artifacts` refuses it:\n  {new}\nThat is a stream \
                     that used to produce artifacts and now produces none."
                ),
                (Err(old), Ok(_)) => panic!(
                    "{label}/{dialect:?}: UNDER-REFUSAL. The coherence oracle refuses this \
                     stream and `render_artifacts` renders it anyway:\n  {old}"
                ),
            }
        }
    }

    // Two-sided, and PINNED as well as floored. The floors stop the control passing
    // vacuously; the pin stops a case quietly changing sides while still clearing a
    // floor.
    assert_eq!(
        (refused, accepted),
        (CONTROL_REFUSALS, CONTROL_ACCEPTANCES),
        "(refused, accepted)"
    );
    assert!(
        refused >= 20,
        "the control must actually drive refusals, or it says nothing about \
         over-refusal: {refused}"
    );
    assert!(
        accepted >= 20,
        "the control must also drive acceptances, or it says nothing about the streams \
         that still render: {accepted}"
    );
}

/// Stream/dialect cases in which BOTH sides refuse.
///
/// 74 of 171 — 57 streams (27 fixtures, 23 carriers, 7 probes) times 3 dialects, with
/// none rejected by the policy resolution before either side is reached (74 + 97 = 171,
/// which is the identity that says so).
///
/// This is the number the control exists for. It was MEASURED at 74 with the walker
/// still in place and it is 74 after the switch: the move refuses exactly what the old
/// path refused, on every one of these streams.
const CONTROL_REFUSALS: usize = 74;
/// Stream/dialect cases in which BOTH sides accept. 97 of the same 171.
///
/// Measured at 94 before the `column_level_reference_policy` carrier was added and 97
/// after, and the +3 is that carrier on three dialects and nothing else — which is why
/// a moving number here is still readable rather than alarming.
const CONTROL_ACCEPTANCES: usize = 97;

/// The control above compares two Rust functions. This one checks that the probe streams
/// still reach the arms they were written for, by asserting the exact refusal each
/// produces - so a probe that silently stopped being a refusal stream passes the
/// biconditional trivially on both sides being `Ok` and fails HERE.
#[test]
fn the_refusal_probes_still_exercise_the_arms_they_name() {
    let open = support::no_inject(SCHEMA);
    let outcome = |name: &str, dialect: SqlDialect| {
        let (_, source) = REFUSAL_PROBES
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("probe `{name}` exists"));
        render_artifacts(&parse(source), dialect, SCHEMA, &open)
            .map(|_| "rendered".to_string())
            .unwrap_or_else(|e| e.to_string())
    };
    let pg = SqlDialect::Postgres;
    assert!(
        outcome("table_created_twice", pg).contains("users"),
        "the duplicate-create probe must name the table: {}",
        outcome("table_created_twice", pg)
    );
    assert!(
        outcome("drop_a_constraint_that_does_not_exist", pg).contains("users_ghost_uq"),
        "the missing-constraint probe must name the constraint - and it is here because \
         `dropConstraint` is the op this whole move changes the meaning of: {}",
        outcome("drop_a_constraint_that_does_not_exist", pg)
    );
    assert!(
        outcome("duplicate_enum", pg).contains("tier"),
        "the duplicate-enum probe must be refused as a duplicate enum: {}",
        outcome("duplicate_enum", pg)
    );
    // MEASURED, not assumed: this one RENDERS on PostgreSQL. A native enum column needs
    // only the type NAME to fold, so `genArtifacts` - which is DB-free by contract and
    // never reads a catalog - has no way to prove the membership is missing and does not
    // fabricate one. SQLite and MySQL INLINE the value list into the column's storage, so
    // their folds fail closed. The probe is asserted on the dialect where it is a
    // refusal, and the PostgreSQL render is kept beside it as the contrast rather than
    // deleted, because "it is refused" would be false for a third of the matrix.
    assert_eq!(
        outcome("column_names_a_dropped_enum", pg),
        "rendered",
        "PostgreSQL folds a column that names a dropped enum, because the native type \
         reference needs only the name"
    );
    assert!(
        outcome("column_names_a_dropped_enum", SqlDialect::Sqlite).contains("tier"),
        "but SQLite inlines the value list, so it fails closed and names the type: {}",
        outcome("column_names_a_dropped_enum", SqlDialect::Sqlite)
    );
    assert!(
        outcome("add_a_column_to_a_missing_table", pg).contains("ghosts"),
        "the missing-table probe must name the table: {}",
        outcome("add_a_column_to_a_missing_table", pg)
    );
    // And the controls that stop the four above from passing for the wrong reason.
    assert_eq!(
        outcome("enum_recreated_after_drop", pg),
        "rendered",
        "a drop-and-recreate is NOT a duplicate, and a control that refused it would make \
         the duplicate probe pass for the wrong reason"
    );
    assert_eq!(
        outcome("dialect_leg_selection", pg),
        "rendered",
        "and a dialectal stream renders on every leg, or the probe list is measuring a \
         parse failure rather than a fold decision"
    );
}

/// One more control on the golden, in the direction the over-refusal test cannot reach:
/// the corpus must contain BOTH refused and rendered rows for the SAME reduction, or the
/// golden is pinning only half of what `render_artifacts` does.
#[test]
fn the_corpus_golden_records_both_refusals_and_renders() {
    let recorded = golden();
    let lines = golden_lines(&recorded);
    let refused = lines.iter().filter(|l| l.contains("|refused|")).count();
    let rendered = lines
        .iter()
        .filter(|l| l.contains("|sha|runtime.json|"))
        .count();
    assert!(
        refused >= 20 && rendered >= 40,
        "the golden must record both outcomes: {refused} refused, {rendered} rendered"
    );

    // And the reduction really does emit a refusal line rather than skipping the stream,
    // which is the difference between "no rows" and "a recorded refusal".
    let mut out = Vec::new();
    corpus_lines(
        "probe:duplicate_enum",
        &parse(
            REFUSAL_PROBES
                .iter()
                .find(|(n, _)| *n == "duplicate_enum")
                .expect("probe")
                .1,
        ),
        &support::no_inject(SCHEMA),
        SqlDialect::Postgres,
        &mut out,
    );
    assert_eq!(
        out.len(),
        1,
        "a refused stream reduces to exactly one line: {out:?}"
    );
    assert!(
        out[0].contains("|refused|"),
        "and that line says it was refused: {}",
        out[0]
    );
}
