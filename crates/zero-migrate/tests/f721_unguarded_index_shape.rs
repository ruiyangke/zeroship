//! An unguarded `createIndex` whose name already exists LIVE with a DIFFERENT
//! shape must be refused, not silently skipped.
//!
//! MEASURED, and this is a data-integrity fail-open rather than a diagnostic one:
//!
//!     -- live: ix is a NON-UNIQUE index on (v)
//!     CREATE UNIQUE INDEX IF NOT EXISTS "ix" ON "public"."a" ("w")
//!         NOTICE: relation "ix" already exists, skipping
//!         CREATE INDEX          <- succeeds
//!     -- pg_indexes still shows: CREATE INDEX ix ON a USING btree (v)
//!
//! The statement succeeds, the unit journals green, and the UNIQUE index the
//! author asked for does not exist. An author who added it to enforce a business
//! invariant has no uniqueness and no error.
//!
//! WHY THE PROBE DOES NOT CATCH IT. `render/lower.rs` stamps a `GuardProbe::Index`
//! in two shapes: a GUARDED create carries `expect: Some((unique, columns))` - a
//! real shape verify - while an UNGUARDED create gets `ownership_only: true`,
//! which asks only WHICH TABLE owns the name. Same table, so it passes.
//!
//! WHY THIS FIX DOES NOT TOUCH THE PROBE. The recorded reasoning for
//! `ownership_only` is that the same-table re-run must stay the `IF NOT EXISTS`
//! no-op that crash recovery replays, with "no satisfied no-op" in the probe.
//! Changing that risks turning a met precondition into a SKIPPED statement, which
//! would alter unguarded-create semantics rather than tighten them.
//!
//! So this refuses at LOWER instead, using data already in hand: `LiveSchema`
//! carries `table_snapshots`, and an `IndexSnapshot` carries `unique` and
//! `columns`. No probe semantics change, and nothing new is introduced into the
//! apply path.
//!
//! FAIL-OPEN WHERE IT CANNOT KNOW, deliberately. The refusal fires only when the
//! live snapshot POSITIVELY shows a same-named index of a different shape. An
//! absent or unpopulated snapshot behaves exactly as before, so no path that
//! previously worked starts failing for want of information.

mod support;

use std::collections::BTreeMap;
use zero_migrate::model::snapshot::{IndexSnapshot, TableSnapshot};
use zero_migrate::render::lower::{IrAuthor, LiveSchema};
use zero_migrate::schema::query::SqlDialect;
use zero_migrate::MigrationIr;

fn index(name: &str, unique: bool, columns: &[&str]) -> IndexSnapshot {
    IndexSnapshot {
        name: name.to_string(),
        unique,
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        elements: Vec::new(),
        access_method: "btree".to_string(),
        predicate: None,
        include: Vec::new(),
        with: None,
        only: false,
        opclass: None,
        nulls_not_distinct: false,
        comment: None,
        expr_cascade_columns: None,
    }
}

/// A live schema whose table `a` already carries the given index.
fn live_with(existing: IndexSnapshot) -> LiveSchema {
    let mut live = LiveSchema::default();
    live.tables.insert("a".to_string());
    let mut snap = TableSnapshot {
        columns: Vec::new(),
        indexes: vec![existing],
        constraints: Vec::new(),
        runtime_options: Default::default(),
        partition_by: None,
        comment: None,
        stored_create_sql: None,
    };
    snap.indexes.shrink_to_fit();
    let mut tables = BTreeMap::new();
    tables.insert("a".to_string(), snap);
    live.table_snapshots = tables;
    live
}

fn lower(op: &str, live: &LiveSchema) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{op}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let author = IrAuthor::new(
        "public",
        "f721",
        SqlDialect::Postgres,
        &support::confined_charter(),
    );
    author
        .lower_steps(&ir, live)
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

const UNIQUE_ON_W: &str = r#"{"op":"createIndex","name":"ix","table":"a","unique":true,"columns":[{"kind":"column","name":"w"}]}"#;
const PLAIN_ON_V: &str =
    r#"{"op":"createIndex","name":"ix","table":"a","columns":[{"kind":"column","name":"v"}]}"#;

#[test]
fn a_unique_index_over_a_live_non_unique_one_of_the_same_name_is_refused() {
    let refusal = lower(UNIQUE_ON_W, &live_with(index("ix", false, &["v"])))
        .expect_err("IF NOT EXISTS would silently skip this and leave the old index");
    assert!(
        refusal.to_lowercase().contains("ix"),
        "the refusal must name the index: {refusal}"
    );
}

#[test]
fn a_differing_column_list_under_the_same_name_is_refused() {
    lower(PLAIN_ON_V, &live_with(index("ix", false, &["w"])))
        .expect_err("same name, different columns, silently skipped today");
}

// ---------------------------------------------------------------------------
// Controls. The first is the one the recorded design reasoning demands.
// ---------------------------------------------------------------------------

#[test]
fn an_identical_index_still_lowers_so_crash_recovery_replay_is_untouched() {
    // THE CONTROL THAT SHAPES THE FIX. Crash recovery re-runs a statement the
    // ENGINE issued, so the live shape MATCHES what the op asks for. That must
    // remain an ordinary `IF NOT EXISTS` no-op, exactly as before.
    lower(PLAIN_ON_V, &live_with(index("ix", false, &["v"])))
        .expect("an identical re-run must stay a no-op, not become a refusal");
}

#[test]
fn a_name_that_is_not_live_still_lowers() {
    lower(UNIQUE_ON_W, &live_with(index("other", false, &["v"])))
        .expect("a fresh index name is ordinary");
}

#[test]
fn an_empty_live_snapshot_still_lowers() {
    // FAIL-OPEN WHERE IT CANNOT KNOW: with no snapshot the engine has no grounds
    // to refuse, and must behave exactly as it did before this check existed.
    let mut live = LiveSchema::default();
    live.tables.insert("a".to_string());
    lower(UNIQUE_ON_W, &live).expect("no snapshot means no grounds to refuse");
}
