//! **The corpus behind step 4 consumer 3, and the reduction both halves share.**
//!
//! `docs/proposals/single-fold-and-effects.md` section G step 4 moves
//! `fold_to_field_defs` onto `FoldedSchema::project_field_defs`. The wire `FieldDef`
//! map that walker produced is not an artifact a human reads: it is
//! `schema.runtime.json`'s `fields` block AND, on SQLite, `LiveSchema::sqlite_schemas`,
//! which is what the 12-step table rebuild renders its new `CREATE TABLE` from
//! (`render/lower.rs`'s SQLite `renameColumn` leg -> `lower_ir_rename` ->
//! `render/declarative.rs`'s `desired.sqlite_schemas.get(table)`). So this reduction
//! carries the DDL, not just the JSON.
//!
//! # Why this lives in `support` rather than in the gate that reads it
//!
//! The golden this produces was captured from the OLD path, by a SEPARATE test binary,
//! BEFORE the consumer was switched. A capture harness living in the same binary as the
//! comparison is how a golden blesses itself: the harness rewrites the file from the new
//! path and the comparison then passes against it. Keeping the REDUCTION here and the
//! two drivers in two binaries means the capture and the gate cannot disagree about what
//! a line is, and the capture binary can be deleted afterwards without the gate losing
//! its definition.
//!
//! # Everything is read out of the ARTIFACT
//!
//! Nothing here names `fold_to_field_defs` or `project_field_defs`. Every line is
//! derived from one [`zero_migrate::render_artifacts`] call, which is the real entry
//! point on both sides of the move, so this file is byte-identical before and after the
//! switch and the capture measures the walker purely by having been run first.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;
use zero_migrate::manifest_entry::sha256_hex;
use zero_migrate::model::ir::{MigrationIr, Op};
use zero_migrate::schema::query::{
    build_create_table_with_fks_for_dialect_scoped_statements, FkEmission, SqliteEmitScope,
};
use zero_migrate::{render_artifacts, EffectivePolicy, SqlDialect};

pub const SCHEMA: &str = "public";

pub const DIALECTS: [SqlDialect; 3] = [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql];

/// The recorded op fixtures - the same 27 `tests/op_fixture_goldens.rs` owns and the
/// same list consumers 1 and 2 drove. Real drained recorder envelopes, already
/// policy-resolved, so they fold under the confined charter that produced them.
pub const STEMS: [&str; 27] = [
    "alter_primary_key",
    "comments_indexes",
    "constraint_not_valid",
    "ddl_addcol_constraints",
    "ddl_alter",
    "ddl_create",
    "ddl_drop",
    "ddl_rename_table",
    "dialectal_ops",
    "dml",
    "dml_upsert",
    "edge_scalars",
    "enums_domains",
    "fluent_ddl",
    "fluent_dml",
    "fluent_scalars",
    "fluent_scalars_dml",
    "grouped_views",
    "in_list_scalars",
    "p2a_facets",
    "partition",
    "pg_aggregates",
    "pg_vendor",
    "runtime_options",
    "sequences_exclusion",
    "synchronize_identity",
    "views",
];

/// The carrier streams, folded into the SAME golden as the recorded fixtures.
///
/// They exist because the recorded corpus does not cover what moved. The step 3
/// byte-for-byte gate measured the `field_defs` leg at 677 equal and SIX differing
/// comparisons, and all six are one defect on one stream - `fold_to_field_defs` never
/// un-lifting a dropped `UNIQUE`. Six differing comparisons is what a corpus that
/// happens to contain one `addConstraint`/`dropConstraint` pair can say; it is not a
/// measurement of the constraint LIFECYCLE, which is the whole of what this move
/// changes. The streams below cross every lift the map performs with every way its
/// grantor can go away: dropped by name, dropped by DERIVED name, dropped with its
/// column, and dropped with its table.
pub const CARRIERS: &[(&str, &str)] = &[
    // ---- the lifts, and their grantors going away -------------------------------
    (
        "unique_constraint_dropped",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"users","constraint":{"name":"users_email_uq","kind":{"kind":"unique","columns":["email"]}}},
  {"op":"dropConstraint","table":"users","name":"users_email_uq"}
]"#,
    ),
    (
        "unique_constraint_kept",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false},{"name":"handle","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"users","constraint":{"name":"users_email_uq","kind":{"kind":"unique","columns":["email"]}}},
  {"op":"addConstraint","table":"users","constraint":{"name":"users_handle_uq","kind":{"kind":"unique","columns":["handle"]}}},
  {"op":"dropConstraint","table":"users","name":"users_handle_uq"}
]"#,
    ),
    (
        "unique_column_facet_survives_a_drop",
        r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false,"unique":true},{"name":"handle","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"users","constraint":{"name":"users_handle_uq","kind":{"kind":"unique","columns":["handle"]}}},
  {"op":"dropConstraint","table":"users","name":"users_handle_uq"}
]"#,
    ),
    (
        "check_bound_dropped",
        r#"[
  {"op":"createTable","name":"scores","columns":[{"name":"id","type":"text","nullable":false},{"name":"score","type":"int"}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"scores","constraint":{"name":"scores_score_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"and","lhs":{"node":"binOp","op":"ge","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":1}},"rhs":{"node":"binOp","op":"le","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":9}}}}}},
  {"op":"dropConstraint","table":"scores","name":"scores_score_ck"}
]"#,
    ),
    (
        "check_bound_kept",
        r#"[
  {"op":"createTable","name":"scores","columns":[{"name":"id","type":"text","nullable":false},{"name":"score","type":"int"}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"scores","constraint":{"name":"scores_score_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"and","lhs":{"node":"binOp","op":"ge","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":1}},"rhs":{"node":"binOp","op":"le","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":9}}}}}}
]"#,
    ),
    (
        "check_membership_dropped",
        r#"[
  {"op":"createTable","name":"issues","columns":[{"name":"id","type":"text","nullable":false},{"name":"status","type":"text"}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"issues","constraint":{"name":"issues_status_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"or","lhs":{"node":"binOp","op":"eq","lhs":{"node":"colRef","name":"status"},"rhs":{"node":"literal","value":"open"}},"rhs":{"node":"binOp","op":"eq","lhs":{"node":"colRef","name":"status"},"rhs":{"node":"literal","value":"closed"}}}}}},
  {"op":"dropConstraint","table":"issues","name":"issues_status_ck"}
]"#,
    ),
    (
        "named_fk_policy_dropped",
        r#"[
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"owner_id","type":{"ref":{"references":"accounts"}}}],"primaryKey":["id"],"constraints":[{"name":"orders_owner_fk","kind":{"kind":"fk","columns":["owner_id"],"referencesTable":"accounts","referencesColumns":["id"],"onDelete":"cascade"}}]},
  {"op":"dropConstraint","table":"orders","name":"orders_owner_fk"}
]"#,
    ),
    // THE UNNAMED sibling of the row above, and the reason it is here. The walker
    // recorded the constraint's `name` as authored - `None` for an unnamed inline FK -
    // and un-lifted by matching on it, so an unnamed FK's ON DELETE could never be
    // un-lifted at all. The model names every constraint at `createTable`
    // (`named_constraint`), which is also the name the catalog replay gives it and the
    // name a `dropConstraint` must use, so the projection un-lifts this one.
    (
        "unnamed_fk_policy_dropped_by_derived_name",
        r#"[
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"owner_id","type":{"ref":{"references":"accounts"}}}],"primaryKey":["id"],"constraints":[{"kind":{"kind":"fk","columns":["owner_id"],"referencesTable":"accounts","referencesColumns":["id"],"onDelete":"cascade"}}]},
  {"op":"dropConstraint","table":"orders","name":"orders_owner_id_fkey"}
]"#,
    ),
    (
        "unnamed_fk_policy_kept",
        r#"[
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"owner_id","type":{"ref":{"references":"accounts"}}}],"primaryKey":["id"],"constraints":[{"kind":{"kind":"fk","columns":["owner_id"],"referencesTable":"accounts","referencesColumns":["id"],"onDelete":"cascade"}}]}
]"#,
    ),
    // The grantor leaving WITH ITS COLUMN, then the column coming back under the same
    // name. The walker's recovered CHECK / FK facets are a side map keyed by table and
    // lifted onto whatever column carries the name at the END, so a re-added column
    // inherits the facet of the one that was dropped.
    (
        "fk_column_dropped_and_readded",
        r#"[
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"owner_id","type":{"ref":{"references":"accounts"}}}],"primaryKey":["id"],"constraints":[{"name":"orders_owner_fk","kind":{"kind":"fk","columns":["owner_id"],"referencesTable":"accounts","referencesColumns":["id"],"onDelete":"cascade"}}]},
  {"op":"dropColumn","table":"orders","column":"owner_id"},
  {"op":"addColumn","table":"orders","column":"owner_id","type":{"ref":{"references":"accounts"}}}
]"#,
    ),
    (
        "check_column_dropped_and_readded",
        r#"[
  {"op":"createTable","name":"scores","columns":[{"name":"id","type":"text","nullable":false},{"name":"score","type":"int"}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"scores","constraint":{"name":"scores_score_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"and","lhs":{"node":"binOp","op":"ge","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":1}},"rhs":{"node":"binOp","op":"le","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":9}}}}}},
  {"op":"dropColumn","table":"scores","column":"score"},
  {"op":"addColumn","table":"scores","column":"score","type":"int"}
]"#,
    ),
    (
        "check_column_renamed",
        r#"[
  {"op":"createTable","name":"scores","columns":[{"name":"id","type":"text","nullable":false},{"name":"score","type":"int"}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"scores","constraint":{"name":"scores_score_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"and","lhs":{"node":"binOp","op":"ge","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":1}},"rhs":{"node":"binOp","op":"le","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":9}}}}}},
  {"op":"renameColumn","table":"scores","from":"score","to":"points","type":"int"}
]"#,
    ),
    (
        "table_dropped_with_its_constraints",
        r#"[
  {"op":"createTable","name":"scores","columns":[{"name":"id","type":"text","nullable":false},{"name":"score","type":"int"}],"primaryKey":["id"]},
  {"op":"addConstraint","table":"scores","constraint":{"name":"scores_score_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"ge","lhs":{"node":"colRef","name":"score"},"rhs":{"node":"literal","value":1}}}}},
  {"op":"dropTable","table":"scores"},
  {"op":"createTable","name":"scores","columns":[{"name":"id","type":"text","nullable":false},{"name":"score","type":"int"}],"primaryKey":["id"]}
]"#,
    ),
    // THE POSITIVE CONTROL for the referential-policy facet, and it exists because the
    // corpus's coverage floor demanded it. After the un-lift fix, EVERY `onDelete` row
    // this golden had was one of the defect rows - so the golden would have carried no
    // `onDelete` at all, and a later change could have deleted the facet everywhere
    // without moving a line. A policy declared ON THE COLUMN needs no lift and no
    // un-lift: it rides on `IrColumn::references` straight through
    // `ir_column_to_field`, so it is the shape that must survive. It is also the shape
    // the deploy path accepts - a `ColType::Ref` column paired with a table-level FK to
    // a text key is refused by the load gate as a logical-type mismatch, which is why
    // `tests/sqlite_rebuild_field_defs_live.rs` uses this spelling too.
    (
        "column_level_reference_policy",
        r#"[
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"owner_id","type":"text","references":{"table":"accounts","column":"id","onDelete":"cascade","onUpdate":"restrict"}}],"primaryKey":["id"]}
]"#,
    ),
    // ---- the facets that reach the rebuilt CREATE TABLE --------------------------
    (
        "retype_across_parameterised_types",
        r#"[
  {"op":"createTable","name":"shapes","columns":[{"name":"id","type":"text","nullable":false},{"name":"narrow","type":{"string":{"length":24}}},{"name":"fixed","type":{"char":{"length":4}}}],"primaryKey":["id"]},
  {"op":"setColumnType","table":"shapes","column":"narrow","toType":{"string":{"length":40}}},
  {"op":"setColumnType","table":"shapes","column":"fixed","toType":{"char":{"length":8}}}
]"#,
    ),
    (
        "nullability_and_default_both_ways",
        r#"[
  {"op":"createTable","name":"prefs","columns":[{"name":"id","type":"text","nullable":false},{"name":"theme","type":"text"},{"name":"count","type":"int"}],"primaryKey":["id"]},
  {"op":"setColumnDefault","table":"prefs","column":"theme","value":{"literal":{"value":"dark"}}},
  {"op":"setColumnNotNull","table":"prefs","column":"theme"},
  {"op":"setColumnDefault","table":"prefs","column":"count","value":{"literal":{"value":3}}},
  {"op":"dropColumnDefault","table":"prefs","column":"count"},
  {"op":"dropColumnNotNull","table":"prefs","column":"theme"}
]"#,
    ),
    (
        "named_enum_and_domain_columns",
        r#"[
  {"op":"createEnum","name":"tier","values":["free","paid"]},
  {"op":"createDomain","name":"positive_number","as":"int"},
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false},{"name":"plan","type":{"enum":{"name":"tier"}}},{"name":"credit","type":{"domain":{"name":"positive_number"}}}],"primaryKey":["id"]}
]"#,
    ),
    (
        "table_renamed_with_a_ref_column",
        r#"[
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"owner_id","type":{"ref":{"references":"accounts"}}}],"primaryKey":["id"]},
  {"op":"renameTable","table":"accounts","to":"tenants"}
]"#,
    ),
    (
        "generated_column_follows_a_rename",
        r#"[
  {"op":"createTable","name":"line_items","columns":[{"name":"id","type":"text","nullable":false},{"name":"quantity","type":"int"},{"name":"total_cents","type":"int","generated":{"expr":{"node":"binOp","op":"mul","lhs":{"node":"colRef","name":"quantity"},"rhs":{"node":"literal","value":100}},"stored":true}}],"primaryKey":["id"]},
  {"op":"renameColumn","table":"line_items","from":"quantity","to":"qty","type":"int"}
]"#,
    ),
    // The CONTROL for the row below, and the corpus's only surviving `identity` facet:
    // the carrier beside it CLEARS the identity, so on its own it would let the whole
    // facet be dropped everywhere without a golden row moving.
    (
        "identity_kept",
        r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"int","nullable":false,"identity":{"always":false}},{"name":"legacy_id","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]}
]"#,
    ),
    (
        "identity_cleared_by_a_primary_key_replace",
        r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"int","nullable":false,"identity":{"always":false}},{"name":"legacy_id","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["id"],"columns":["legacy_id"],"dropIdentityFrom":["id"]}}
]"#,
    ),
    (
        "attached_partition_dropped",
        r#"[
  {"op":"createTable","name":"par","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}],"partitionBy":{"kind":"range","columns":["bucket"]}},
  {"op":"createTable","name":"p1","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}]},
  {"op":"attachPartition","parent":"par","name":"p1","bound":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":100}]}},
  {"op":"dropPartition","parent":"par","name":"p1"}
]"#,
    ),
    (
        "attached_partition_detached",
        r#"[
  {"op":"createTable","name":"par","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}],"partitionBy":{"kind":"range","columns":["bucket"]}},
  {"op":"createTable","name":"p1","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}]},
  {"op":"attachPartition","parent":"par","name":"p1","bound":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":100}]}},
  {"op":"detachPartition","parent":"par","name":"p1"}
]"#,
    ),
];

fn manifest_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

pub fn parse(ops: &str) -> Vec<Op> {
    serde_json::from_str(ops).expect("the stream parses")
}

pub fn read_stem(stem: &str) -> Vec<Op> {
    let path = manifest_path(&format!("tests/op_fixtures/{stem}.golden.json"));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str::<MigrationIr>(&text)
        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
        .ops
}

/// The wire `FieldDef` map, read back out of `schema.runtime.json`.
///
/// `render_runtime_descriptor_v1` puts each table's map into `collections.<t>.fields`
/// verbatim, so this recovers exactly the value the SQLite rebuild would be handed -
/// without naming the function that produced it, which is the point: this reduction is
/// byte-identical before and after the consumer moves.
pub fn field_defs_from_runtime_json(runtime_json: &str) -> BTreeMap<String, Value> {
    let parsed: Value = serde_json::from_str(runtime_json).expect("runtime.json parses");
    let collections = parsed
        .get("collections")
        .and_then(Value::as_object)
        .expect("runtime.json carries a collections object");
    collections
        .iter()
        .map(|(table, entry)| {
            (
                table.clone(),
                entry
                    .get("fields")
                    .cloned()
                    .expect("every collection carries a fields map"),
            )
        })
        .collect()
}

/// The `CREATE TABLE` the SQLite 12-step rebuild would render from one table's map.
///
/// This is the DDL leg, reproduced through the SAME emitter and the SAME arguments
/// `render/declarative.rs` passes on the rebuild path: `FkEmission::Inline` and
/// `SqliteEmitScope::MainUnqualified`. It is the reason this corpus exists rather than
/// a JSON-only one - `min`/`max`/`enum` become an inline `CHECK`, a `ref` becomes an
/// inline `REFERENCES … ON DELETE …`, and both of those are copied over real rows.
pub fn sqlite_rebuild_create(
    table: &str,
    schema: &Value,
    policy: &EffectivePolicy,
) -> Result<String, String> {
    build_create_table_with_fks_for_dialect_scoped_statements(
        SCHEMA,
        table,
        schema,
        &FkEmission::Inline,
        SqlDialect::Sqlite,
        SqliteEmitScope::MainUnqualified,
        policy,
    )
    .map_err(|error| error.to_string())?
    .into_iter()
    .next()
    .map(|sql| collapse_whitespace(&sql))
    .ok_or_else(|| "the emitter produced no CREATE TABLE".to_string())
}

/// One statement onto one line.
///
/// The emitter pretty-prints a `CREATE TABLE` across as many lines as it has columns,
/// and the golden is line-oriented, so a multi-line value would silently become N
/// unlabelled rows. Collapsing runs of whitespace keeps the whole statement in one
/// comparable unit; it cannot hide a difference, because SQL that differs only in
/// whitespace is the same statement.
fn collapse_whitespace(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One stream under one dialect, reduced to lines.
///
/// Four kinds of line, and each answers a question the others cannot:
///
/// * `sha|runtime.json` - the WHOLE artifact the map feeds, so a byte moving anywhere
///   this reduction does not classify still fails;
/// * `sha|env.db.ts` - the CONTROL. `env.db.ts` is rendered from a DIFFERENT projection
///   (consumer 2's), so this move must not touch it at all;
/// * `field|…` - one line per COLUMN carrying its whole wire `FieldDef`, which is the
///   per-field probe. A `sha` line that moves without a `field` line beside it means the
///   change is in the descriptor's envelope rather than in a column;
/// * `sqlite_create|…` - the DDL the rebuild would run. Emitted for the SQLite dialect
///   only, because that is the only dialect on which this map reaches a `CREATE TABLE`.
pub fn corpus_lines(
    label: &str,
    ops: &[Op],
    policy: &EffectivePolicy,
    dialect: SqlDialect,
    out: &mut Vec<String>,
) {
    let d = format!("{dialect:?}");
    let rendered = match render_artifacts(ops, dialect, SCHEMA, policy) {
        Ok(rendered) => rendered,
        Err(error) => {
            out.push(format!("{label}|{d}|refused|{error}"));
            return;
        }
    };
    out.push(format!(
        "{label}|{d}|sha|runtime.json|{}",
        sha256_hex(rendered.runtime_json.as_bytes())
    ));
    out.push(format!(
        "{label}|{d}|sha|env.db.ts|{}",
        sha256_hex(rendered.env_db_ts.as_bytes())
    ));
    let defs = field_defs_from_runtime_json(&rendered.runtime_json);
    for (table, schema) in &defs {
        let columns = schema
            .as_object()
            .expect("a table's FieldDef map is an object");
        for (column, def) in columns {
            out.push(format!(
                "{label}|{d}|field|{table}|{column}|{}",
                serde_json::to_string(def).expect("a FieldDef serialises")
            ));
        }
        if dialect == SqlDialect::Sqlite {
            match sqlite_rebuild_create(table, schema, policy) {
                Ok(sql) => out.push(format!("{label}|{d}|sqlite_create|{table}|{sql}")),
                Err(error) => {
                    out.push(format!("{label}|{d}|sqlite_create_refused|{table}|{error}"));
                }
            }
        }
    }
}

/// Drive the whole corpus through the real entry point and reduce it to lines.
pub fn measure_corpus() -> Vec<String> {
    let mut measured = Vec::new();
    let confined = super::confined_charter();
    for stem in STEMS {
        let ops = read_stem(stem);
        for dialect in DIALECTS {
            corpus_lines(
                &format!("fixture:{stem}"),
                &ops,
                &confined,
                dialect,
                &mut measured,
            );
        }
    }
    let open = super::no_inject(SCHEMA);
    for (name, source) in CARRIERS {
        let ops = parse(source);
        for dialect in DIALECTS {
            corpus_lines(
                &format!("carrier:{name}"),
                &ops,
                &open,
                dialect,
                &mut measured,
            );
        }
    }
    measured
}
