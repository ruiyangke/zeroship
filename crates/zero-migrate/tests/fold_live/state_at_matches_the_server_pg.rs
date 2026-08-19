//! **`state_at(N)` adjudicated by a live PostgreSQL server, over a NON-EMPTY `live_at_0`.**
//!
//! `docs/proposals/single-fold-and-effects.md` names this check itself, and the
//! wording is the specification:
//!
//! > **`state_at(N)` against a live server.** For a plan of N steps, apply steps
//! > 0..N-1 for real, introspect, and assert the introspected model equals
//! > `state_at(N)`. This is the only check that proves the effect model rather than
//! > asserting it.
//!
//! # What was already adjudicated, and what was not
//!
//! `fold_roundtrip_pg::assert_lifecycle_roundtrip` already applies op-by-op and
//! compares a fold against live introspection at checkpoints. It proves a great deal
//! - but it folds with `fold_ops`, which is `fold_ops_onto(&SchemaSnapshot::default(),
//! ..)`, and it lowers against `LiveSchema::default()`. Every object it compares was
//! created by an op in the same plan.
//!
//! `state_at(N) = live_at_0 (+) fold(effects[0..N])` has two terms, and on PostgreSQL
//! every live test pinned the special case `live_at_0 = {}`, where the identity
//! degenerates into the round-trip property `fold(plan) == introspect(apply(plan))`.
//! The in-crate unit tests (`render::fold::effects`) carry the offline half, and the
//! one that touches a non-empty base -
//! `state_at_carries_the_base_it_did_not_create` - only checks that a base object the
//! prefix NEVER TOUCHES survives.
//!
//! The exception, stated because the generalisation would otherwise be wrong:
//! `fold_retype_physical_type_mysql` DOES fold onto a live-introspected base against a
//! real server. Its base is built by an earlier ENGINE deploy and read back, so every
//! object in it is one the fold could have emitted itself, and it compares one final
//! state rather than a prefix.
//!
//! What is new here is therefore narrower than "the base term is untested", and worth
//! saying exactly: this is `state_at`'s FIRST caller of any kind outside its own
//! module's unit tests, the first PostgreSQL live test to fold onto an introspected
//! base, the first on any dialect whose base objects were created by RAW SQL rather
//! than by the engine, and the first to compare at every PREFIX rather than at the
//! end. A pre-existing index, CHECK constraint, view or partition parent is a
//! dependent the fold has to follow without ever having seen it created.
//!
//! # The identity is NOT universal, and the last test here is the counter-example
//!
//! On PostgreSQL, `Op::RenameColumn` lowers to an ONLINE expand-contract whose
//! contract phase is a SEPARATE LATER DEPLOY. `render/fold.rs` states the consequence
//! at the `Op::RenameColumn` fold arm:
//!
//! > Between expand and contract, live PG carries BOTH the `from` and `to` columns
//! > while this fold (which collapses the rename to the final `to` name) shows only
//! > `to`. That divergence is correctly EXCLUDED from the fold==live equality oracle.
//!
//! So `state_at(N)` deliberately models the LOGICAL post-contract shape, and the
//! server at step N is mid-expand. The proposal's identity does not hold for that op,
//! and `the_identity_does_not_hold_across_an_online_rename` PINS the disagreement
//! rather than routing around it - the existing live rename tests all drive the
//! rename with native `ALTER TABLE ... RENAME COLUMN` precisely to avoid it, which is
//! why the gap survived. A test that merely omitted the op would leave a reader
//! believing the identity is total.
//!
//! The consequence reaches past this file. Section E promises that five existence
//! assertions - `TableExists`, `TableNotExists`, `ColumnExists`, `ColumnNotExists`,
//! `RowCount` - are "answered exactly at `state_at(N)`". Two of them answer WRONG
//! across this rename, and in the UNSAFE direction: at prefix 1 `state_at` says
//! `person.nick` does not exist while the server still has it, so a hoisted
//! `ColumnNotExists("nick")` would be SATISFIED at preflight against a database where
//! it is false. That is a wrong ACCEPT, which is the opposite of the wrong-refusal
//! direction `effect_of` is deliberately tuned toward. This file measures it; fixing
//! it is step 6's problem.
//!
//! # The introspection surface, named
//!
//! `expected` is `render::fold::effects::state_at(&live_at_0, &ops, k, ..)`.
//! `actual` is `snapshot_schema(&session, project_schema)` - the engine's own
//! introspection, the SAME function that produced `live_at_0`, returning the SAME
//! `SchemaSnapshot` type `state_at` returns. Nothing is hand-rolled from the catalog,
//! so the comparison is between two values of one representation.
//!
//! # What the comparator does NOT see, stated rather than hidden
//!
//! The verdict is `diff_snapshots(..).is_clean()`, and it is much narrower than `==`.
//! Measured against `apply::drift`, it does not compare: a view's `definition`,
//! `columns` or `authored_query` (only `materialized` and `comment`); a CHECK or
//! EXCLUDE constraint BODY; an index's `opclass`, `nulls_not_distinct`, `only` or
//! `expr_cascade_columns`; an index expression or partial-predicate BODY whenever the
//! live side filled `expr_cascade_columns`, which the PostgreSQL introspector always
//! does; a generated column's expression; column ORDER; `partition_by`; or the
//! authored `functions` / `policies` / `triggers` maps.
//!
//! A clean prefix is therefore NOT a claim that the two snapshots are equal. Two
//! things keep that from making this file vacuous:
//!
//! 1. **`PREFIX 0` pins the floor.** The loop compares at EVERY prefix in `0..=N`,
//!    and prefix 0 asserts that folding NOTHING onto an introspected raw-SQL schema
//!    reproduces it - with every seeded object asserted present BY NAME in `actual`.
//!    A comparator blind to a seeded object would report every prefix clean; the
//!    named-presence check is what stops a later clean prefix from being clean
//!    because the object stopped being compared.
//! 2. **The view cases assert the body directly, off the comparator.** Live
//!    introspection populates `ViewSnapshot::definition` from `pg_get_viewdef` even
//!    though the differ ignores it, so the tests below read that field themselves.
//!    That is not a hand-rolled catalog read - it is a field of the same introspection
//!    surface - and it is the only way the `CREATE OR REPLACE VIEW` shape is anything
//!    but vacuous.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::render::fold::effects::state_at;
use zero_migrate::{
    diff_snapshots, resolve_create_table_policy, snapshot_schema, Approval, EffectivePolicy,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr,
    PostgresBackend, SchemaSnapshot, SqlDialect, StructuralDrift,
};

const OWNER: &str = "app_state_at_matches_the_server_pg";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "state_at_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// An object the seed created by raw SQL, asserted present in `live_at_0` and in the
/// prefix-0 re-introspection.
///
/// This is the floor the module doc describes. Naming the kind separately from the
/// name keeps the assertion honest about WHICH map has to carry it: a partition child
/// is not in `tables`, and a view is in neither.
#[derive(Debug, Clone, Copy)]
enum Seeded {
    /// A relation in `SchemaSnapshot::tables`.
    Table(&'static str),
    /// An index on the named table.
    Index(&'static str, &'static str),
    /// A constraint on the named table.
    Constraint(&'static str, &'static str),
    /// A relation in `SchemaSnapshot::views`.
    View(&'static str),
}

impl Seeded {
    /// Whether the snapshot carries this object. A `false` at prefix 0 means the
    /// comparator is blind to it, which would make every later prefix vacuous for it.
    fn present_in(self, snapshot: &SchemaSnapshot) -> bool {
        match self {
            Self::Table(table) => snapshot.tables.contains_key(table),
            Self::Index(table, index) => snapshot
                .tables
                .get(table)
                .is_some_and(|snap| snap.indexes.iter().any(|idx| idx.name == index)),
            Self::Constraint(table, constraint) => snapshot
                .tables
                .get(table)
                .is_some_and(|snap| snap.constraints.iter().any(|con| con.name == constraint)),
            Self::View(view) => snapshot.views.contains_key(view),
        }
    }
}

/// What one prefix of the plan measured: the state `state_at` predicted, the state
/// the server actually held, and the differ's verdict on the pair.
///
/// Both snapshots are handed back rather than just the verdict, because the differ is
/// blind to whole facets (see the module doc) and some cases have to read a field it
/// never looks at.
struct Prefix {
    expected: SchemaSnapshot,
    actual: SchemaSnapshot,
    drift: StructuralDrift,
}

/// Seed a schema by raw SQL, introspect it as `live_at_0`, then apply `source`'s ops
/// ONE AT A TIME, capturing `state_at(k)` against live introspection at every prefix
/// `k` in `0..=N`.
///
/// `{schema}` in `seed` is replaced with the quoted test schema. Nothing in `seed`
/// goes through the engine, so every object it creates reaches the fold only through
/// introspection - which is the point of the file.
///
/// Returns `None` when no live PostgreSQL is configured, so each caller skips the way
/// the rest of the theme does.
async fn prefixes(
    label: &str,
    seed: &str,
    seeded: &[Seeded],
    source: &str,
    policy_for: fn(&str) -> EffectivePolicy,
) -> Option<Vec<Prefix>> {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return None;
    };
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy_for(&schema));
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated state_at schema");

    let work: Result<Vec<Prefix>, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        // ---------------------------------------------------------------
        // live_at_0. Raw SQL, then the shipped introspection - no op has
        // run, and none of these objects appears in any op stream.
        // ---------------------------------------------------------------
        session
            .batch(&seed.replace("{schema}", &quoted_schema))
            .await
            .map_err(|error| format!("seed the pre-existing schema: {error}"))?;

        let live_at_0 = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("introspect live_at_0: {error}"))?;

        for object in seeded {
            if !object.present_in(&live_at_0) {
                return Err(format!(
                    "live_at_0 does not carry the seeded {object:?}; every later prefix \
                     would be vacuous for it"
                ));
            }
        }

        // A second deploy against an existing schema has BOTH of these, and lowering
        // with `LiveSchema::default()` would lower a DIFFERENT statement than a real
        // deploy does.
        let live = LiveSchema::from_catalog_snapshot(live_at_0.clone(), OWNER);
        let registry: BTreeMap<String, String> = live_at_0
            .tables
            .keys()
            .map(|table| (table.clone(), OWNER.to_string()))
            .collect();

        let policy = policy_for(&cfg.project_schema);
        let authored: MigrationIr =
            serde_json::from_str(source).map_err(|error| format!("parse test IR: {error}"))?;
        let resolved = resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
            .map_err(|error| format!("resolve create-table policy: {error}"))?;
        let resolved_source = serde_json::to_string(&resolved)
            .map_err(|error| format!("serialize resolved test IR: {error}"))?;
        let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
        let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
        let artifact = author
            .load_and_lower_guarded(&resolved_source, OWNER, &registry, &live, &guard)
            .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;

        if artifact.op_spans.len() != resolved.ops.len() {
            return Err(format!(
                "lowered {} operation spans for {} resolved ops",
                artifact.op_spans.len(),
                resolved.ops.len()
            ));
        }

        // Every step must belong to some op's span, or the loop below leaves steps
        // unapplied and any disagreement is about COVERAGE rather than about the
        // effect model.
        let covered: usize = artifact
            .op_spans
            .iter()
            .map(|span| {
                span.step_range.len()
                    + span
                        .additional_step_ranges
                        .iter()
                        .map(std::ops::Range::len)
                        .sum::<usize>()
            })
            .sum();
        if covered != artifact.plan.steps.len() {
            return Err(format!(
                "op spans cover {covered} of {} plan steps; applying per-op would skip the rest",
                artifact.plan.steps.len()
            ));
        }

        let engine = MigrationEngine::new();
        let mut measured = Vec::with_capacity(resolved.ops.len() + 1);

        // ---------------------------------------------------------------
        // The identity, at every prefix. Measure at k, THEN apply op k, so
        // k = 0 is the untouched base and k = ops.len() is the whole plan.
        // ---------------------------------------------------------------
        for k in 0..=resolved.ops.len() {
            let expected = state_at(
                &live_at_0,
                &resolved.ops,
                k,
                SqlDialect::Postgres,
                &cfg.project_schema,
                &policy,
            )
            .map_err(|error| format!("prefix {k}: state_at must fold: {error:?}"))?;
            let actual = snapshot_schema(&session, &cfg.project_schema)
                .await
                .map_err(|error| format!("prefix {k}: introspect the live schema: {error}"))?;

            if k == 0 {
                // The floor. See the module doc: without this, a comparator blind to
                // a seeded object would report every prefix clean for the wrong
                // reason.
                for object in seeded {
                    if !object.present_in(&actual) {
                        return Err(format!(
                            "prefix 0: the re-introspected schema lost the seeded {object:?}"
                        ));
                    }
                }
            }

            let drift = diff_snapshots(&expected, &actual);
            measured.push(Prefix {
                expected,
                actual,
                drift,
            });

            if k == resolved.ops.len() {
                break;
            }

            let span = &artifact.op_spans[k];
            if span.op != resolved.ops[k] {
                return Err(format!(
                    "lowered operation span {k} does not match the resolved operation"
                ));
            }
            let mut ranges = vec![span.step_range.clone()];
            ranges.extend(span.additional_step_ranges.iter().cloned());
            ranges.sort_by_key(|range| range.start);
            for range in ranges {
                engine
                    .apply_plan(
                        &artifact.plan.steps[range],
                        Approval::Approved,
                        &backend,
                        &cfg,
                        "state-at-matches-the-server-pg",
                        LockMode::Acquire,
                    )
                    .await
                    .map_err(|error| format!("apply IR plan operation {}: {error}", k + 1))?;
            }
        }

        Ok(measured)
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(measured), Ok(())) => Some(measured),
        (Err(work), Ok(())) => panic!("{label}: {work}"),
        (Ok(_), Err(cleanup)) => panic!("{label}: drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => {
            panic!("{label}: {work}; cleanup failed: {cleanup}")
        }
    }
}

/// Assert the identity holds at every prefix, and report the first prefix it does not.
fn assert_every_prefix_agrees(label: &str, measured: &[Prefix]) {
    for (k, prefix) in measured.iter().enumerate() {
        assert!(
            prefix.drift.is_clean(),
            "{label}: prefix {k}: state_at({k}) and the server disagree after applying {k} of \
             {} ops: {:#?}",
            measured.len() - 1,
            prefix.drift
        );
    }
}

/// The live view body, as `pg_get_viewdef` deparsed it. The differ never compares
/// this field, so every caller below reads it explicitly.
fn live_view_body<'a>(prefix: &'a Prefix, view: &str) -> &'a str {
    prefix
        .actual
        .views
        .get(view)
        .unwrap_or_else(|| panic!("live introspection must carry the view {view}"))
        .definition
        .as_deref()
        .unwrap_or_else(|| panic!("live introspection must populate {view}'s definition"))
}

/// A drop whose cascade reaches objects only `live_at_0` knows about.
///
/// PostgreSQL drops `acct_email_idx` and `acct_email_ck` as a consequence of dropping
/// the column; neither was ever created by an op, so the fold has to reach them
/// through the introspected base. `acct_region_idx` must SURVIVE - a fold that
/// cascaded by table rather than by column would pass the first half and fail here.
/// Index and constraint EXISTENCE is a facet the differ does compare, so this case is
/// carried by the comparator rather than by a side assertion.
#[compio::test]
async fn a_drop_cascades_through_dependents_only_the_live_base_carries() {
    let label = "drop cascade over a pre-existing index and CHECK";
    let Some(measured) = prefixes(
        label,
        "CREATE TABLE {schema}.acct (id integer PRIMARY KEY, email text, region text); \
         CREATE INDEX acct_email_idx ON {schema}.acct (email); \
         CREATE INDEX acct_region_idx ON {schema}.acct (region); \
         ALTER TABLE {schema}.acct ADD CONSTRAINT acct_email_ck CHECK (email <> '');",
        &[
            Seeded::Table("acct"),
            Seeded::Index("acct", "acct_email_idx"),
            Seeded::Index("acct", "acct_region_idx"),
            Seeded::Constraint("acct", "acct_email_ck"),
        ],
        r#"{"ir_version":1,"name":"drop_cascade","owner_app":"app_state_at_matches_the_server_pg",
            "ops":[{"op":"dropColumn","table":"acct","column":"email"}]}"#,
        support::no_inject,
    )
    .await
    else {
        return;
    };
    assert_every_prefix_agrees(label, &measured);

    // The cascade actually happened, rather than the two sides agreeing on a schema
    // where nothing moved.
    let after = &measured[1];
    assert!(
        Seeded::Index("acct", "acct_email_idx").present_in(&measured[0].actual)
            && !Seeded::Index("acct", "acct_email_idx").present_in(&after.actual),
        "the server must have cascaded the pre-existing index away"
    );
    assert!(
        !Seeded::Constraint("acct", "acct_email_ck").present_in(&after.actual),
        "the server must have cascaded the pre-existing CHECK away"
    );
    assert!(
        Seeded::Index("acct", "acct_region_idx").present_in(&after.actual),
        "an unrelated pre-existing index must survive"
    );
}

/// **The shape that motivated the effect model.**
///
/// `CREATE OR REPLACE VIEW` reads as a creation and silently recomputes the view's
/// dependency edges. The seeded view reads `src.a`; the replacement reads `src.b`
/// under the same output name and type, which is the one rewrite PostgreSQL permits
/// and the one a parse tree cannot see. Op 1 then drops `src.a` - a statement the
/// server would REFUSE at prefix 1 if op 0 had not moved the edge, so the plan
/// applying at all is server-side proof that the blocker moved.
///
/// The differ compares NOTHING about a view body, so a clean drift here would be
/// vacuous for the thing under test. That is measured, not assumed: neutering the
/// fold so a `replace` KEEPS the pre-existing body left every test in the
/// `zero-migrate` crate green - all 2988 of them, live PostgreSQL suites included -
/// because `diff_snapshots` compares only a view's `materialized` flag and `comment`.
///
/// So this case reads BOTH bodies itself, off fields the differ ignores: the live one
/// from `ViewSnapshot::definition` (`pg_get_viewdef`), and the predicted one from
/// `ViewSnapshot::authored_query`, which is what the fold records. They are different
/// representations of the same claim - deparsed SQL and the authored AST - so they are
/// asserted SEPARATELY against the column the body must read rather than against each
/// other. Two independent readings landing on `b` is the evidence; neither is
/// normalised into the other.
#[compio::test]
async fn a_replaced_view_moves_the_blocker_a_later_drop_needs() {
    let label = "CREATE OR REPLACE VIEW over a pre-existing view, then the drop it unblocks";
    let Some(measured) = prefixes(
        label,
        "CREATE TABLE {schema}.src (id integer PRIMARY KEY, a text, b text); \
         CREATE VIEW {schema}.labelled AS SELECT id, a AS label FROM {schema}.src;",
        &[Seeded::Table("src"), Seeded::View("labelled")],
        r#"{"ir_version":1,"name":"replace_view","owner_app":"app_state_at_matches_the_server_pg",
            "ops":[
              {"op":"createView","name":"labelled","replace":true,
               "query":{"kind":"structured","select":{"from":{"name":"src"},
                 "projection":[{"kind":"colRef","name":"id"},
                               {"kind":"colRef","name":"b","alias":"label"}]}}},
              {"op":"dropColumn","table":"src","column":"a"}
            ]}"#,
        support::operator_charter,
    )
    .await
    else {
        return;
    };
    assert_every_prefix_agrees(label, &measured);

    // The seeded body really did read `a` - otherwise the replacement moves nothing.
    let before = live_view_body(&measured[0], "labelled");
    assert!(
        before.contains(" a AS label") || before.contains("a AS label"),
        "the seeded view must read src.a: {before}"
    );

    // After the replace, the live body reads `b` and no longer reads `a`. The differ
    // compares neither, so this is the assertion that carries the case.
    let replaced = live_view_body(&measured[1], "labelled");
    assert!(
        replaced.contains("b AS label"),
        "CREATE OR REPLACE VIEW must have moved the body onto src.b: {replaced}"
    );

    // And what state_at PREDICTED the view reads, which is the half the live body
    // cannot speak to. `authored_query` is the fold's own record of the body; a fold
    // that treated the replace as a no-op would leave the seeded query here, and the
    // differ would not notice. Asserted through the serialized query so the check is
    // about the projected COLUMN rather than about the AST's shape.
    let predicted = serde_json::to_string(
        measured[1].expected.views["labelled"]
            .authored_query
            .as_ref()
            .expect("state_at must record the authored body of a replaced view"),
    )
    .expect("the authored view query serializes");
    assert!(
        predicted.contains(r#""name":"b""#),
        "state_at must predict the replaced body reads src.b: {predicted}"
    );
    assert!(
        !predicted.contains(r#""name":"a""#),
        "state_at must not still predict the pre-replacement body over src.a: {predicted}"
    );

    // And the server accepted DROP COLUMN a, which it refuses while a view depends on
    // it. `prefixes` would have returned an error from `apply_plan` otherwise, so
    // reaching prefix 2 at all is the proof; assert the outcome so the reason is
    // legible in the failure rather than implied by an absent panic.
    assert!(
        !measured[2].actual.tables["src"]
            .columns
            .iter()
            .any(|column| column.name == "a"),
        "the drop the replaced view unblocked must have landed"
    );
}

/// A rename the server follows into a pre-existing view's stored body.
///
/// PostgreSQL records a view's dependency by OID, so renaming the table re-renders
/// the body under the new name with no statement naming the view. The view is in
/// `live_at_0` only.
#[compio::test]
async fn a_table_rename_is_followed_into_a_pre_existing_view() {
    let label = "rename a table a pre-existing view reads";
    let Some(measured) = prefixes(
        label,
        "CREATE TABLE {schema}.orig (id integer PRIMARY KEY, v text); \
         CREATE VIEW {schema}.over_orig AS SELECT id, v FROM {schema}.orig;",
        &[Seeded::Table("orig"), Seeded::View("over_orig")],
        r#"{"ir_version":1,"name":"rename_table","owner_app":"app_state_at_matches_the_server_pg",
            "ops":[{"op":"renameTable","table":"orig","to":"renamed"}]}"#,
        support::operator_charter,
    )
    .await
    else {
        return;
    };
    assert_every_prefix_agrees(label, &measured);

    let after = &measured[1];
    assert!(
        after.actual.tables.contains_key("renamed") && !after.actual.tables.contains_key("orig"),
        "the server must have renamed the table"
    );
    assert!(
        live_view_body(after, "over_orig").contains("renamed"),
        "PostgreSQL must re-render the dependent view body under the new table name"
    );
}

/// Attach and detach, where BOTH the parent and the child predate the plan.
///
/// `snapshot_schema` reports a relation with `relispartition = true` under
/// `partitions` rather than `tables`, so the attach MOVES the child between two maps
/// of the snapshot and the detach moves it back. The differ compares both maps, so a
/// fold that recorded the membership without relocating the relation reports twice.
#[compio::test]
async fn an_attach_and_detach_relocate_a_pre_existing_relation() {
    let label = "attach then detach a pre-existing table as a partition";
    let Some(measured) = prefixes(
        label,
        "CREATE TABLE {schema}.evt (bucket integer NOT NULL, id integer NOT NULL) \
           PARTITION BY RANGE (bucket); \
         CREATE TABLE {schema}.evt_early (bucket integer NOT NULL, id integer NOT NULL);",
        &[Seeded::Table("evt"), Seeded::Table("evt_early")],
        r#"{"ir_version":1,"name":"attach_detach","owner_app":"app_state_at_matches_the_server_pg",
            "ops":[
              {"op":"attachPartition","parent":"evt","name":"evt_early",
               "bound":{"kind":"range","from":[{"kind":"int","value":0}],
                        "to":[{"kind":"int","value":100}]}},
              {"op":"detachPartition","parent":"evt","name":"evt_early"}
            ]}"#,
        support::operator_charter,
    )
    .await
    else {
        return;
    };
    assert_every_prefix_agrees(label, &measured);

    // The relocation really happened in both directions, so the three clean prefixes
    // are not three readings of one unchanged schema.
    assert!(
        measured[0].actual.tables.contains_key("evt_early")
            && !measured[0].actual.partitions.contains_key("evt_early"),
        "the child starts as a standalone table"
    );
    assert!(
        measured[1].actual.partitions.contains_key("evt_early")
            && !measured[1].actual.tables.contains_key("evt_early"),
        "the attach must move the child out of tables and into partitions"
    );
    assert!(
        measured[2].actual.tables.contains_key("evt_early")
            && !measured[2].actual.partitions.contains_key("evt_early"),
        "the detach must move it back"
    );
}

/// **The counter-example: the identity does not hold across a PostgreSQL column
/// rename, and this pins exactly how it fails.**
///
/// `Op::RenameColumn` lowers to an online expand-contract. At prefix 1 the server has
/// EXPANDED - it carries the old column, the new column, and a shadow trigger and
/// function keeping them in sync - while `state_at(1)` reports the collapsed
/// post-contract shape. `render/fold.rs` documents this as deliberately excluded from
/// the fold==live oracle, and every other live rename test drives the rename with
/// native `ALTER TABLE ... RENAME COLUMN` to avoid it. That is why no test had ever
/// caught it, and why omitting the op here would leave the identity looking total.
///
/// The disagreement is asserted in SHAPE, not merely in existence: a future change
/// that makes the contract phase part of the same deploy, or that stops emitting the
/// shadow objects, breaks this test and should - the answer would then be that the
/// identity holds and this test should become an agreement case.
#[compio::test]
async fn the_identity_does_not_hold_across_an_online_rename() {
    let label = "an online rename leaves the server mid-expand";
    let Some(measured) = prefixes(
        label,
        "CREATE TABLE {schema}.person (id integer PRIMARY KEY, nick text); \
         CREATE INDEX person_nick_idx ON {schema}.person (nick); \
         ALTER TABLE {schema}.person ADD CONSTRAINT person_nick_ck CHECK (nick <> '');",
        &[
            Seeded::Table("person"),
            Seeded::Index("person", "person_nick_idx"),
            Seeded::Constraint("person", "person_nick_ck"),
        ],
        r#"{"ir_version":1,"name":"rename_online","owner_app":"app_state_at_matches_the_server_pg",
            "ops":[{"op":"renameColumn","table":"person","from":"nick","to":"handle","type":"text"}]}"#,
        support::no_inject,
    )
    .await
    else {
        return;
    };

    // Prefix 0 still holds: the base folds onto itself. Only the op diverges.
    assert!(
        measured[0].drift.is_clean(),
        "{label}: prefix 0 must still agree: {:#?}",
        measured[0].drift
    );

    let after = &measured[1];
    assert!(
        !after.drift.is_clean(),
        "{label}: the online rename is expected to DIVERGE at prefix 1. A clean result \
         means the expand-contract lowering changed and this test should become an \
         agreement case."
    );

    // The server is mid-expand: both columns live, plus the shadow pair.
    let unexpected = after.drift.unexpected_objects.join("\n");
    assert!(
        unexpected.contains("person.nick"),
        "{label}: the server must still carry the PRE-CONTRACT column: {unexpected}"
    );
    assert!(
        unexpected.contains("zsdw_person_nick_handle_trg"),
        "{label}: the expand phase must have installed its sync trigger: {unexpected}"
    );
    assert!(
        unexpected.contains("zsdw_person_nick_handle_fn"),
        "{label}: the expand phase must have installed its sync function: {unexpected}"
    );

    // ...and the pre-existing index still keys the OLD name, because the physical
    // rename has not happened yet, while state_at already reports the new one.
    assert!(
        after.drift.altered_objects.iter().any(|altered| {
            altered.object == "index person_nick_idx"
                && altered.field == "columns"
                && altered.expected == "handle"
                && altered.actual == "nick"
        }),
        "{label}: state_at must predict the post-contract index key while the server \
         still holds the pre-contract one: {:#?}",
        after.drift.altered_objects
    );

    // The fold is not merely behind - it is AHEAD, reporting the collapsed shape. So
    // nothing the fold predicts is absent from the server; the divergence is entirely
    // expand-phase residue the fold does not model.
    assert!(
        after.drift.missing_objects.is_empty(),
        "{label}: the divergence must be one-sided residue, not a fold that lost an \
         object: {:#?}",
        after.drift.missing_objects
    );
    assert!(
        after.expected.tables["person"]
            .columns
            .iter()
            .any(|column| column.name == "handle"),
        "{label}: state_at reports the post-contract name"
    );
    assert!(
        !after.expected.tables["person"]
            .columns
            .iter()
            .any(|column| column.name == "nick"),
        "{label}: state_at has already collapsed the rename"
    );
}
