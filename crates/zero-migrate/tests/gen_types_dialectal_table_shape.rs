//! **A table created inside a dialect leg gets the charter's injected shape.**
//!
//! `resolve_create_table_policy` is what turns the recorder's RAW author-only
//! `createTable` into the shape the charter mandates - the injected system columns,
//! the pinned primary key, the injected indexes. It ran BEFORE leg expansion and
//! walked only the top level, matching `Op::CreateTable` and `continue`-ing past
//! everything else, so a create authored inside `dialect({ pg, ... })` never reached
//! it.
//!
//! The consequence is not cosmetic. The injected shape IS the confinement the charter
//! exists to impose, so a table declared in a leg came out author-shaped: no `id`, no
//! `created_at`, none of the mandatory columns its top-level twin gets from the same
//! charter. Two tables authored under one policy, differing only in whether a
//! `dialect()` wrapper stood between them and the resolver.
//!
//! The arms below are a DIFFERENTIAL rather than a golden: the same author columns are
//! declared twice in one history, once at the top level and once inside the selected
//! leg, and the injected column set of the two must match. That way the test states
//! "the wrapper changes nothing" without hardcoding which columns this charter
//! happens to inject, so it keeps its meaning if the charter changes.
//!
//! No live database is needed. Resolution is offline and the oracle is the emitted
//! artifact.

mod support;

use std::collections::BTreeSet;

use serde_json::Value;

use zero_migrate::model::ir::{MigrationIr, Op};
use zero_migrate::{render_artifacts, SqlDialect};

const SCHEMA: &str = "public";

/// `plain` at the top level and `wrapped` inside the PostgreSQL leg, with the SAME
/// author-declared column. Any difference in their emitted field sets is the wrapper's
/// doing and nothing else.
fn history() -> Vec<Op> {
    let source = r#"{
  "ir_version": 1,
  "name": "dialectal_table_shape",
  "owner_app": "app_test",
  "ops": [
    {"op":"createTable","name":"plain","columns":[
      {"name":"title","type":"text","nullable":true}
    ]},
    {"op":"dialectal",
     "pg":[
       {"op":"createTable","name":"wrapped","columns":[
         {"name":"title","type":"text","nullable":true}
       ]}
     ]}
  ]
}"#;
    serde_json::from_str::<MigrationIr>(source)
        .expect("the dialectal table-shape IR parses")
        .ops
}

/// The emitted field names of one collection, out of `schema.runtime.json`.
fn field_names(collection: &str) -> BTreeSet<String> {
    let artifacts = render_artifacts(
        &history(),
        SqlDialect::Postgres,
        SCHEMA,
        &support::confined_charter(),
    )
    .expect("the dialectal table-shape history renders artifacts");
    let doc: Value =
        serde_json::from_str(&artifacts.runtime_json).expect("schema.runtime.json parses");
    let fields = doc
        .pointer(&format!("/collections/{collection}/fields"))
        .unwrap_or_else(|| panic!("the descriptor should carry `{collection}`: {doc:#}"));
    fields
        .as_object()
        .expect("fields is an object")
        .keys()
        .cloned()
        .collect()
}

/// The control, and the reason this test can trust its own oracle: the top-level table
/// really does get more than its author declared. If the charter ever stopped injecting,
/// this fails and the differential below would otherwise pass vacuously.
#[test]
fn the_top_level_table_is_injected_by_the_charter() {
    let plain = field_names("plain");
    assert!(
        plain.len() > 1,
        "the confined charter injects mandatory columns, so `plain` must carry more \
         than the single authored `title`: {plain:?}"
    );
    assert!(
        plain.contains("title"),
        "the authored column survives resolution: {plain:?}"
    );
}

/// The reported defect: the same charter, the same authored column, one `dialect()`
/// wrapper in between, and the injected shape went missing.
#[test]
fn a_table_created_inside_a_leg_gets_the_same_injected_shape() {
    let plain = field_names("plain");
    let wrapped = field_names("wrapped");
    assert_eq!(
        wrapped, plain,
        "`wrapped` is authored identically to `plain` and resolved under the same \
         charter, so a `dialect()` wrapper must not change which columns the charter \
         injects"
    );
}

/// EVERY leg is resolved, not the one some target would select - and the artifact
/// cannot show this, which is why the oracle here is the resolved IR itself. A table
/// declared in the SQLite leg contributes nothing to a PostgreSQL fold, so it never
/// reaches `schema.runtime.json` on any run the arms above make.
///
/// The property matters because `resolve_create_table_policy` takes NO dialect: its
/// output is what the checksum is folded over. Resolving only a selected leg would
/// make one authored file resolve differently per target and carry a different
/// checksum on each, so the same migration would have a different identity depending
/// on which database it was authored against.
#[test]
fn a_create_in_an_unselected_leg_is_resolved_too() {
    let source = r#"{
  "ir_version": 1,
  "name": "unselected_leg_shape",
  "owner_app": "app_test",
  "ops": [
    {"op":"dialectal",
     "sqlite":[
       {"op":"createTable","name":"only_sqlite","columns":[
         {"name":"title","type":"text","nullable":true}
       ]}
     ]}
  ]
}"#;
    let ir: MigrationIr = serde_json::from_str(source).expect("the unselected-leg IR parses");
    let resolved =
        zero_migrate::resolve_create_table_policy(&ir, &support::confined_charter(), SCHEMA)
            .expect("the confined charter resolves the leg's create");

    let Some(Op::Dialectal { sqlite, .. }) = resolved.ops.first() else {
        panic!("resolution keeps the wrapper in place: {:#?}", resolved.ops);
    };
    let leg = sqlite.as_ref().expect("the sqlite leg survives resolution");
    let Some(Op::CreateTable { columns, .. }) = leg.first() else {
        panic!("the leg still holds its createTable: {leg:#?}");
    };
    let names: BTreeSet<String> = columns.iter().map(|column| column.name.clone()).collect();
    assert!(
        names.len() > 1 && names.contains("title"),
        "the SQLite leg's create is resolved under the same charter as any other, so \
         it carries the injected columns alongside the authored `title`: {names:?}"
    );
}
