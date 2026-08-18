//! **The differential corpus.** Step 1 of `docs/proposals/single-fold-and-effects.md`.
//!
//! One op stream is replayed through all four independent answers -
//! [`fold_ops`], [`fold_to_field_defs`], [`super::authoring_tables_from_ops`]
//! and the runtime collection metadata - and what each one produces is
//! recorded. The fourth was `super::runtime_metadata_from_ops` until step 4 of
//! that proposal deleted it; its entry point is now
//! `FoldedSchema::project_runtime_metadata`, so this corpus keeps cross-checking
//! the same QUESTION against a different producer. It fixes nothing. It makes every later step of that proposal
//! falsifiable, and it turns into a suite the comparison the review log has
//! been running BY HAND for every row of the proposal's section B.
//!
//! **This records CURRENT behaviour, and current behaviour includes shipped
//! defects.** A corpus that only says "the walkers still do what they did" is a
//! bug-preservation machine: the later fix reads as the regression. So every
//! row carries a [`Status`] as well as a verdict, and the three readings are
//! kept apart on purpose:
//!
//! - [`Verdict::Agreed`] - every walker that can answer says the same thing.
//!   With [`Status::Consistent`] this is safe to lock in.
//! - [`Verdict::Divergent`] - they disagree. Each walker's actual answer is
//!   listed, and the row is either [`Status::ByDesign`] with a stated reason or
//!   [`Status::Defect`] with a `docs/review-log.md` pointer. A `Defect` row says
//!   THIS IS WHAT HAPPENS TODAY AND IT IS WRONG; the single fold must not be
//!   held to it.
//! - [`Verdict::Sole`] / [`Verdict::NoAuthority`] - fewer than two walkers can
//!   answer, so nothing cross-checks the one that can. Recorded as
//!   [`Status::Uncorroborated`]. **Swallowing an op is not answering about it**,
//!   and a corpus that let a swallowed op read as agreement would be measuring
//!   its own silence.
//!
//! `Status::Defect` is also reachable from an AGREED verdict, because all four
//! walkers agreeing does not make them right: the encrypted-domain sentinel
//! (`docs/review-log.md:29590-29606`) was a case where two producers agreed and
//! were both wrong, and the reviewer's "disagreement" was a scaffold artifact.
//!
//! # What is measured, and what is not
//!
//! Part 1 is a REACH matrix: for every `Op` variant and every dialect, whether
//! appending that op moved each walker's answer, measured by prefix sweep over
//! the whole corpus. It is a behavioural measurement, not an arm count. Section
//! A of the proposal counted `match` arms and reported that
//! `runtime_metadata_from_ops` handles 12 of 56 and `authoring_tables_from_ops`
//! 15 of 56; [`REACH`] is the measured answer to the same question, and where
//! the two differ the measurement wins.
//!
//! Part 2 is the differential cases: one stream per divergence the review log
//! recorded, plus the multi-op shapes that only misbehave in sequence, each
//! asked a small set of questions the four vocabularies can all answer.
//!
//! **Deliberately not asked: "what type is this column".** `ColumnSnapshot`
//! speaks catalog types, `FieldDescriptor` speaks DSL authoring tokens, and
//! `AuthoringTable` speaks `ColType`. A cross-vocabulary type comparator
//! written here would be a FIFTH interpreter of what an op means, which is the
//! thing this corpus exists to stop growing. The questions that survive
//! translation are which objects a walker names, and whether a stated fact
//! still appears in what it says about them - which is exactly what the log
//! compared by hand.
//!
//! # Where this lives, and why
//!
//! In-crate rather than under `tests/`, for the same reason as
//! `guard_vendor_lower_tests`: `authoring_tables_from_ops` is private to [`super`],
//! and the alternative to a child module is widening a production function's
//! visibility so a test can reach it. This module is `cfg(test)` and ships in nothing.
//!
//! Offline throughout. These are fold and replay functions; no database is
//! opened, so there is no skip that could read as a pass. Runtime is under two
//! seconds for the whole sweep.
//!
//! # Blessing
//!
//! There is deliberately NO re-bless environment variable, matching
//! `tests/op_fixture_goldens.rs`. A mismatch prints the full measurement so a
//! human can read it; updating [`ROWS`] or [`REACH`] is a hand edit that shows
//! up in review as one. An easy update affordance is precisely what converts a
//! corpus into a mirror of whatever the code emits today.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::ir::{MigrationIr, Op};
use crate::render::fold::{fold_ops, fold_to_field_defs};
use crate::SqlDialect;

use super::authoring_tables_from_ops;

/// The schema unqualified objects resolve under.
pub(super) const SCHEMA: &str = "public";

/// Every walker that takes a dialect is run under all three. Several of the
/// divergences the log recorded were dialect-specific, and two of the rows this
/// file records are visible on exactly one dialect.
pub(super) const DIALECTS: [SqlDialect; 3] =
    [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql];

/// A named op stream, written as the wire JSON an author's recorder emits.
pub(super) struct Stream {
    pub(super) name: &'static str,
    pub(super) ops: &'static str,
}

fn fixtures_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/op_fixtures")
}

pub(super) fn read_golden(stem: &str) -> MigrationIr {
    let path = fixtures_dir().join(format!("{stem}.golden.json"));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// The recorded op fixtures, reused rather than re-authored. These are real
/// drained envelopes, already policy-resolved, and between them they construct
/// every one of the 56 `Op` variants - so the reach matrix is measured against
/// production-shaped streams and not only against streams written to please it.
/// The list is the same one `tests/op_fixture_goldens.rs` owns.
pub(super) const STEMS: [&str; 27] = [
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
/// Self-contained op streams. Every stream creates what it later alters, so a
/// PREFIX of it is coherent too and the prefix sweep below has an observation at
/// every position.
pub(super) const STREAMS: &[Stream] = &[
    Stream {
        name: "v_table_lifecycle",
        ops: r#"[
  {"op":"createTable","name":"boxes","columns":[{"name":"id","type":"text","nullable":false},{"name":"label","type":"text"}],"primaryKey":["id"]},
  {"op":"setTableOptions","table":"boxes","options":{"softDelete":true,"versioning":true}},
  {"op":"comment","target":{"kind":"table","name":"boxes"},"comment":"crates"},
  {"op":"renameTable","table":"boxes","to":"crates"},
  {"op":"dropTable","table":"crates"}
]"#,
    },
    Stream {
        name: "v_column_lifecycle",
        ops: r#"[
  {"op":"createTable","name":"notes","columns":[{"name":"id","type":"text","nullable":false},{"name":"body","type":"text"}],"primaryKey":["id"]},
  {"op":"addColumn","table":"notes","column":"tag","type":"text"},
  {"op":"setColumnNotNull","table":"notes","column":"body"},
  {"op":"dropColumnNotNull","table":"notes","column":"body"},
  {"op":"setColumnDefault","table":"notes","column":"body","value":{"literal":{"value":"none"}}},
  {"op":"dropColumnDefault","table":"notes","column":"body"},
  {"op":"renameColumn","table":"notes","from":"body","to":"text_body","type":"text"},
  {"op":"dropColumn","table":"notes","column":"tag"}
]"#,
    },
    Stream {
        name: "v_index_and_constraint",
        ops: r#"[
  {"op":"createTable","name":"users","columns":[{"name":"id","type":"text","nullable":false},{"name":"email","type":"text","nullable":false},{"name":"age","type":"int"}],"primaryKey":["id"]},
  {"op":"createIndex","table":"users","columns":[{"kind":"column","name":"email"}],"name":"users_email_idx"},
  {"op":"addConstraint","table":"users","constraint":{"name":"users_email_uq","kind":{"kind":"unique","columns":["email"]}}},
  {"op":"validateConstraint","table":"users","name":"users_email_uq"},
  {"op":"dropConstraint","table":"users","name":"users_email_uq"},
  {"op":"dropIndex","name":"users_email_idx","table":"users"}
]"#,
    },
    Stream {
        name: "v_primary_key",
        ops: r#"[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["id"],"columns":["legacy_id"]}},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"drop","expectedColumns":["legacy_id"]}}
]"#,
    },
    Stream {
        name: "v_identity",
        ops: r#"[
  {"op":"createTable","name":"tickets","columns":[{"name":"id","type":"int","nullable":false,"identity":{"always":true}},{"name":"title","type":"text"}],"primaryKey":["id"]},
  {"op":"synchronizeIdentity","table":"tickets","column":"id","writesQuiesced":"tickets_window"}
]"#,
    },
    Stream {
        name: "v_partitions",
        ops: r#"[
  {"op":"createTable","name":"events","columns":[{"name":"id","type":"text","nullable":false},{"name":"at","type":"timestamp","nullable":false}],"primaryKey":["id","at"],"partitionBy":{"kind":"range","columns":["at"],"collapse":true}},
  {"op":"createPartition","name":"events_a","of":"events","bounds":{"kind":"range","from":[{"kind":"string","value":"2026-01-01T00:00:00Z"}],"to":[{"kind":"string","value":"2026-02-01T00:00:00Z"}]}},
  {"op":"detachPartition","parent":"events","name":"events_a"},
  {"op":"attachPartition","parent":"events","name":"events_a","bound":{"kind":"range","from":[{"kind":"string","value":"2026-01-01T00:00:00Z"}],"to":[{"kind":"string","value":"2026-02-01T00:00:00Z"}]}},
  {"op":"dropPartition","parent":"events","name":"events_a"}
]"#,
    },
    Stream {
        name: "v_views",
        ops: r#"[
  {"op":"createTable","name":"people","columns":[{"name":"id","type":"text","nullable":false},{"name":"active","type":"boolean","nullable":false}],"primaryKey":["id"]},
  {"op":"createView","name":"active_people","columns":["id"],"query":{"kind":"structured","select":{"from":{"name":"people"},"projection":[{"kind":"colRef","name":"id"}]}}},
  {"op":"dropView","name":"active_people"}
]"#,
    },
    Stream {
        name: "v_named_types",
        ops: r#"[
  {"op":"createEnum","name":"tier","values":["free","paid"]},
  {"op":"createDomain","name":"positive_number","as":"int","check":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"VALUE"},"rhs":{"node":"literal","value":0}}},
  {"op":"createTable","name":"accounts","columns":[{"name":"id","type":"text","nullable":false},{"name":"plan","type":{"enum":{"name":"tier"}}},{"name":"credits","type":{"domain":{"name":"positive_number"}}}],"primaryKey":["id"]},
  {"op":"dropTable","table":"accounts"},
  {"op":"dropDomain","name":"positive_number"},
  {"op":"dropEnum","name":"tier"}
]"#,
    },
    Stream {
        name: "v_sequences",
        ops: r#"[
  {"op":"createSequence","name":"invoice_seq"},
  {"op":"alterSequence","name":"invoice_seq","increment":2},
  {"op":"dropSequence","name":"invoice_seq"}
]"#,
    },
    Stream {
        name: "v_schema_extension_role",
        ops: r#"[
  {"op":"createSchema","name":"reporting","ifNotExists":true},
  {"op":"createExtension","name":"citext","ifNotExists":true},
  {"op":"createRole","name":"reader","login":true,"ifNotExists":true},
  {"op":"alterRole","name":"reader","setSearchPath":["reporting","public"]},
  {"op":"grant","privileges":["select"],"on":{"kind":"schema","names":["reporting"]},"to":["reader"]},
  {"op":"revoke","privileges":["select"],"on":{"kind":"schema","names":["reporting"]},"from":["reader"]},
  {"op":"dropOwnedBy","roles":["reader"]},
  {"op":"dropRole","name":"reader","ifExists":true},
  {"op":"dropExtension","name":"citext","ifExists":true},
  {"op":"dropSchema","name":"reporting","ifExists":true,"cascade":true}
]"#,
    },
    Stream {
        name: "v_security_and_routines",
        ops: r#"[
  {"op":"createTable","name":"secrets","columns":[{"name":"id","type":"text","nullable":false},{"name":"app_id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"setRls","table":"secrets","enabled":true,"forced":true},
  {"op":"createPolicy","name":"tenant_isolation","table":"secrets","forCmd":"all","using":{"node":"unaryOp","op":"isNotNull","operand":{"node":"colRef","name":"app_id"}}},
  {"op":"createFunction","name":"block_tamper","returns":"trigger","language":"plpgsql","replace":true,"body":"BEGIN RAISE EXCEPTION 'append-only'; END;"},
  {"op":"createTrigger","name":"secrets_block","table":"secrets","timing":"before","events":["update"],"forEach":"row","action":{"kind":"executeFunction","name":"block_tamper"}},
  {"op":"pgRaw","sql":"SELECT set_config('zero_migrate.tenant_app', 'demo', false)","reason":"corpus vehicle"},
  {"op":"dropTrigger","name":"secrets_block","table":"secrets","ifExists":true},
  {"op":"dropFunction","name":"block_tamper","ifExists":true},
  {"op":"dropPolicy","name":"tenant_isolation","table":"secrets","ifExists":true}
]"#,
    },
    Stream {
        name: "v_dml",
        ops: r#"[
  {"op":"createTable","name":"codes","columns":[{"name":"code","type":"int","nullable":false},{"name":"label","type":"text"}],"primaryKey":["code"]},
  {"op":"insert","table":"codes","columns":["code","label"],"rows":[[200,"ok"]]},
  {"op":"update","table":"codes","set":{"label":"fixed"}},
  {"op":"backfill","table":"codes","cursorColumns":["code"],"cursorStability":{"mode":"guardUpdates"},"batchSize":100,"set":{"label":"filled"},"name":"backfill_codes"},
  {"op":"delete","table":"codes","where":{"node":"unaryOp","op":"isNull","operand":{"node":"colRef","name":"label"}}}
]"#,
    },
];

/// The differential cases: one op stream per divergence the review log recorded,
/// plus the multi-op shapes that only misbehave in sequence.
pub(super) const CASES: &[Stream] = &[
    Stream {
        // Section B rows 7 and 8 in one stream: a table that exists ONLY inside
        // an `Op::Dialectal` leg, whose PostgreSQL leg alone declares a runtime
        // option and a plain index. Row 7 (docs/review-log.md:18536-18539) was
        // runtime_metadata_from_ops not descending into the leg; row 8
        // (docs/review-log.md:2931-2942) was two artifact walkers hard-coding
        // Postgres regardless of target.
        name: "c_dialectal_leg_selection",
        ops: r#"[
  {"op":"dialectal","pg":[{"op":"createTable","name":"docs","columns":[{"name":"id","type":"text","nullable":false},{"name":"pg_only","type":"text"}],"primaryKey":["id"],"runtimeOptions":{"softDelete":true,"versioning":false},"indexes":[{"name":"docs_pg_idx","columns":[{"kind":"column","name":"pg_only"}]}]}],"sqlite":[{"op":"createTable","name":"docs","columns":[{"name":"id","type":"text","nullable":false},{"name":"sqlite_only","type":"text"}],"primaryKey":["id"]}],"mysql":[{"op":"createTable","name":"docs","columns":[{"name":"id","type":"text","nullable":false},{"name":"mysql_only","type":"text"}],"primaryKey":["id"]}]}
]"#,
    },
    Stream {
        // A view reaches exactly ONE of the four walkers. Present so the corpus
        // records at least one question it cannot cross-check, rather than
        // implying that everything it looks at has a second opinion.
        name: "c_view_is_seen_by_one_walker",
        ops: r#"[
  {"op":"createTable","name":"people","columns":[{"name":"id","type":"text","nullable":false},{"name":"active","type":"boolean","nullable":false}],"primaryKey":["id"]},
  {"op":"createView","name":"active_people","columns":["id"],"query":{"kind":"structured","select":{"from":{"name":"people"},"projection":[{"kind":"colRef","name":"id"}]}}}
]"#,
    },
    Stream {
        name: "c_rename_column_generated_expr",
        ops: r#"[
  {"op":"createTable","name":"line_items","columns":[{"name":"id","type":"text","nullable":false},{"name":"qty_on_hand","type":"int","nullable":false},{"name":"total_cents","type":"int","generated":{"expr":{"node":"binOp","op":"add","lhs":{"node":"colRef","name":"qty_on_hand"},"rhs":{"node":"literal","value":1}},"stored":true}}],"primaryKey":["id"]},
  {"op":"renameColumn","table":"line_items","from":"qty_on_hand","to":"quantity","type":"int"}
]"#,
    },
    Stream {
        name: "c_rename_table_in_generated_expr",
        ops: r#"[
  {"op":"createTable","name":"order_lines","columns":[{"name":"id","type":"text","nullable":false},{"name":"qty","type":"int","nullable":false},{"name":"doubled","type":"int","generated":{"expr":{"node":"binOp","op":"add","lhs":{"node":"colRef","name":"qty","table":"order_lines"},"rhs":{"node":"colRef","name":"qty","table":"order_lines"}},"stored":true}}],"primaryKey":["id"]},
  {"op":"renameTable","table":"order_lines","to":"line_items"}
]"#,
    },
    Stream {
        name: "c_retype_drops_case_sensitivity",
        ops: r#"[
  {"op":"createTable","name":"labels","columns":[{"name":"id","type":"text","nullable":false},{"name":"tag","type":"text","caseSensitive":false}],"primaryKey":["id"]},
  {"op":"setColumnType","table":"labels","column":"tag","toType":"int"}
]"#,
    },
    Stream {
        name: "c_retype_type_parameters",
        ops: r#"[
  {"op":"createTable","name":"shapes","columns":[{"name":"id","type":"text","nullable":false},{"name":"narrow","type":{"string":{"length":24}}},{"name":"fixed","type":{"char":{"length":8}}},{"name":"embedding","type":{"vector":{"vector":3}}}],"primaryKey":["id"]},
  {"op":"setColumnType","table":"shapes","column":"narrow","toType":{"string":{"length":40}}},
  {"op":"setColumnType","table":"shapes","column":"fixed","toType":"int"},
  {"op":"setColumnType","table":"shapes","column":"embedding","toType":{"vector":{"vector":5}}}
]"#,
    },
    Stream {
        name: "c_retype_of_a_value_format_column",
        ops: r#"[
  {"op":"createTable","name":"refs","columns":[{"name":"id","type":"text","nullable":false},{"name":"owner","type":"text","valueFormat":{"typeId":{"prefix":"usr"}}}],"primaryKey":["id"]},
  {"op":"setColumnType","table":"refs","column":"owner","toType":"int"}
]"#,
    },
    Stream {
        name: "c_named_enum_membership",
        ops: r#"[
  {"op":"createEnum","name":"issue_status","values":["open","closed"]},
  {"op":"createTable","name":"issues","columns":[{"name":"id","type":"text","nullable":false},{"name":"status","type":{"enum":{"name":"issue_status"}}}],"primaryKey":["id"]}
]"#,
    },
    Stream {
        name: "c_domain_base_type",
        ops: r#"[
  {"op":"createDomain","name":"positive_number","as":"int","check":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"VALUE"},"rhs":{"node":"literal","value":0}}},
  {"op":"createTable","name":"amounts","columns":[{"name":"id","type":"text","nullable":false},{"name":"amount","type":{"domain":{"name":"positive_number"}}}],"primaryKey":["id"]}
]"#,
    },
    Stream {
        name: "c_encrypted_domain_column",
        ops: r#"[
  {"op":"createDomain","name":"positive_number","as":"int","check":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"VALUE"},"rhs":{"node":"literal","value":0}}},
  {"op":"createTable","name":"amounts","columns":[{"name":"id","type":"text","nullable":false},{"name":"amount","type":{"encrypted":{"of":{"domain":{"name":"positive_number"}}}}}],"primaryKey":["id"]}
]"#,
    },
    Stream {
        name: "c_rename_column_inline_check_body",
        ops: r#"[
  {"op":"createTable","name":"issues","columns":[{"name":"id","type":"text","nullable":false},{"name":"state_token","type":"text","nullable":false}],"primaryKey":["id"],"constraints":[{"name":"issues_state_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"ne","lhs":{"node":"colRef","name":"state_token"},"rhs":{"node":"literal","value":""}}}}]},
  {"op":"renameColumn","table":"issues","from":"state_token","to":"status_token","type":"text"}
]"#,
    },
    Stream {
        name: "c_rename_column_fk_definition",
        ops: r#"[
  {"op":"createTable","name":"owners","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"createTable","name":"issues","columns":[{"name":"id","type":"text","nullable":false},{"name":"owner_id","type":"text","nullable":false}],"primaryKey":["id"],"constraints":[{"name":"issues_owner_fkey","kind":{"kind":"fk","columns":["owner_id"],"referencesTable":"owners","referencesColumns":["id"]}}]},
  {"op":"renameColumn","table":"issues","from":"owner_id","to":"holder_id","type":"text"}
]"#,
    },
    Stream {
        name: "c_rename_column_index_key_columns",
        ops: r#"[
  {"op":"createTable","name":"stock","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_qty","type":"int","nullable":false},{"name":"note","type":"text"}],"primaryKey":["id"]},
  {"op":"createIndex","table":"stock","columns":[{"kind":"column","name":"legacy_qty"}],"name":"stock_qty_idx"},
  {"op":"createIndex","table":"stock","columns":[{"kind":"column","name":"note"},{"kind":"column","name":"legacy_qty"}],"name":"stock_multi_idx"},
  {"op":"renameColumn","table":"stock","from":"legacy_qty","to":"quantity","type":"int"}
]"#,
    },
    Stream {
        name: "c_rename_column_index_expression",
        ops: r#"[
  {"op":"createTable","name":"stock","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_qty","type":"int","nullable":false}],"primaryKey":["id"]},
  {"op":"createIndex","table":"stock","columns":[{"kind":"expr","expr":{"node":"binOp","op":"add","lhs":{"node":"colRef","name":"legacy_qty"},"rhs":{"node":"literal","value":1}}}],"name":"stock_expr_idx"},
  {"op":"renameColumn","table":"stock","from":"legacy_qty","to":"quantity","type":"int"}
]"#,
    },
    Stream {
        name: "c_rename_column_index_include_and_predicate",
        ops: r#"[
  {"op":"createTable","name":"stock","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_qty","type":"int","nullable":false},{"name":"note","type":"text"}],"primaryKey":["id"]},
  {"op":"createIndex","table":"stock","columns":[{"kind":"column","name":"note"}],"include":["legacy_qty"],"name":"stock_include_idx"},
  {"op":"createIndex","table":"stock","columns":[{"kind":"column","name":"note"}],"where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"legacy_qty"},"rhs":{"node":"literal","value":0}},"name":"stock_partial_idx"},
  {"op":"renameColumn","table":"stock","from":"legacy_qty","to":"quantity","type":"int"}
]"#,
    },
    Stream {
        name: "c_drop_column_cascade_dialectal_expr",
        ops: r#"[
  {"op":"createTable","name":"legs","columns":[{"name":"id","type":"text","nullable":false},{"name":"kept","type":"int","nullable":false},{"name":"doomed","type":"int","nullable":false}],"primaryKey":["id"],"constraints":[{"name":"legs_leg_ck","kind":{"kind":"check","expr":{"node":"binOp","op":"gt","lhs":{"node":"dialect","pg":{"node":"colRef","name":"kept"},"sqlite":{"node":"colRef","name":"doomed"},"mysql":{"node":"colRef","name":"kept"}},"rhs":{"node":"literal","value":0}}}}]},
  {"op":"dropColumn","table":"legs","column":"doomed"}
]"#,
    },
    Stream {
        name: "c_create_then_add_then_retype",
        ops: r#"[
  {"op":"createTable","name":"invoices","columns":[{"name":"id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"addColumn","table":"invoices","column":"total","type":{"string":{"length":24}}},
  {"op":"setColumnType","table":"invoices","column":"total","toType":"bigInt"}
]"#,
    },
    Stream {
        name: "c_two_renames_in_sequence",
        ops: r#"[
  {"op":"createTable","name":"stages","columns":[{"name":"id","type":"text","nullable":false},{"name":"first_name","type":"text","nullable":false},{"name":"derived","type":"text","generated":{"expr":{"node":"fnCall","fn":"lower","args":[{"node":"colRef","name":"first_name"}]},"stored":true}}],"primaryKey":["id"]},
  {"op":"renameColumn","table":"stages","from":"first_name","to":"middle_name","type":"text"},
  {"op":"renameColumn","table":"stages","from":"middle_name","to":"last_name","type":"text"}
]"#,
    },
    Stream {
        name: "c_rename_table_then_rename_column",
        ops: r#"[
  {"op":"createTable","name":"widgets","columns":[{"name":"id","type":"text","nullable":false},{"name":"old_label","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"renameTable","table":"widgets","to":"gadgets"},
  {"op":"renameColumn","table":"gadgets","from":"old_label","to":"new_label","type":"text"}
]"#,
    },
    Stream {
        name: "c_create_drop_recreate_same_name",
        ops: r#"[
  {"op":"createTable","name":"drafts","columns":[{"name":"id","type":"text","nullable":false},{"name":"first_shape","type":"text"}],"primaryKey":["id"],"runtimeOptions":{"softDelete":true,"versioning":false},"indexes":[{"name":"drafts_first_idx","columns":[{"kind":"column","name":"first_shape"}]}]},
  {"op":"dropTable","table":"drafts"},
  {"op":"createTable","name":"drafts","columns":[{"name":"id","type":"text","nullable":false},{"name":"second_shape","type":"int"}],"primaryKey":["id"]}
]"#,
    },
];
pub(super) fn parse(ops: &str) -> Vec<Op> {
    serde_json::from_str(ops).expect("corpus stream parses")
}

fn variant(op: &Op) -> String {
    serde_json::to_value(op)
        .expect("op serializes")
        .get("op")
        .and_then(|v| v.as_str())
        .expect("every op serializes with its discriminant")
        .to_string()
}

// ---------------------------------------------------------------------------
// Driving the four walkers
// ---------------------------------------------------------------------------

/// The four independent replays this corpus compares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Walker {
    /// `fold_ops` -> `SchemaSnapshot`.
    Fo,
    /// `fold_to_field_defs` -> the per-table wire `FieldDef` map.
    Ffd,
    /// `authoring_tables_from_ops` -> the `env.db.ts` source model.
    Ato,
    /// The runtime collection metadata - options and plain indexes. Since step 4
    /// moved the consumer, its real entry point is
    /// `FoldedSchema::project_runtime_metadata` and the walker it replaced is gone.
    Rmo,
}

const WALKERS: [Walker; 4] = [Walker::Fo, Walker::Ffd, Walker::Ato, Walker::Rmo];

impl Walker {
    fn short(self) -> &'static str {
        match self {
            Walker::Fo => "FO",
            Walker::Ffd => "FFD",
            Walker::Ato => "ATO",
            Walker::Rmo => "RMO",
        }
    }
}

/// One stream replayed through all four walkers under one dialect. Every walker
/// is driven through its own real entry point; nothing here re-derives what an
/// op means.
struct Replay {
    fo: Result<crate::model::snapshot::SchemaSnapshot, String>,
    ffd: Result<BTreeMap<String, serde_json::Value>, String>,
    ato: Result<BTreeMap<String, super::AuthoringTable>, String>,
    rmo: Result<BTreeMap<String, super::RuntimeCollectionMetadata>, String>,
}

pub(super) fn policy(confined: bool) -> crate::EffectivePolicy {
    if confined {
        crate::test_fixtures::confined_charter()
    } else {
        crate::test_fixtures::no_inject(SCHEMA)
    }
}

impl Replay {
    fn run(ops: &[Op], d: SqlDialect, confined: bool) -> Self {
        let p = policy(confined);
        Self {
            fo: fold_ops(ops, d, SCHEMA, &p).map_err(|e| e.to_string()),
            ffd: fold_to_field_defs(ops, d, SCHEMA, &p).map_err(|e| e.to_string()),
            ato: authoring_tables_from_ops(ops, d).map_err(|e| e.to_string()),
            rmo: crate::render::fold::single_fold::fold(ops, d, SCHEMA, &p)
                .map(|folded| folded.project_runtime_metadata())
                .map_err(|e| e.to_string()),
        }
    }

    /// The whole of one walker's answer, canonicalised to text. Only the
    /// `Carries` question and the reach sweep read this, and both read it as an
    /// opaque haystack.
    fn text(&self, w: Walker) -> Result<String, &str> {
        match w {
            Walker::Fo => self
                .fo
                .as_ref()
                .map(|v| format!("{v:#?}"))
                .map_err(String::as_str),
            Walker::Ffd => self
                .ffd
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).expect("field defs serialize"))
                .map_err(String::as_str),
            Walker::Ato => self
                .ato
                .as_ref()
                .map(|v| format!("{v:#?}"))
                .map_err(String::as_str),
            Walker::Rmo => self
                .rmo
                .as_ref()
                .map(|v| format!("{v:#?}"))
                .map_err(String::as_str),
        }
    }
}

// ---------------------------------------------------------------------------
// Part 1: which op variants each walker actually reaches
// ---------------------------------------------------------------------------

/// Whether appending one op changed what a walker says.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Appending this op changed the walker's answer, or turned it into a
    /// refusal. The walker interprets the variant.
    Reaches,
    /// The walker's answer was byte-identical before and after. The variant
    /// passes through it. On a walker with a catch-all arm this is the
    /// swallowed set; SILENT is NOT agreement and never counts as one.
    Silent,
    /// No usable observation: every stream carrying this variant already had
    /// the walker in a refused state before the op was appended.
    Unobserved,
}

impl Reach {
    fn short(self) -> &'static str {
        match self {
            Reach::Reaches => "R",
            Reach::Silent => "S",
            Reach::Unobserved => "-",
        }
    }
}

/// Measure reach by PREFIX SWEEP: for every stream and every position `i`,
/// replay `ops[..i]` and `ops[..=i]` and ask whether the walker's answer moved.
///
/// This is a measurement, not a hand-maintained list of which arms exist. A
/// walker gains an arm and the table moves; a walker loses one and the table
/// moves the other way. Counting `match` arms by eye would prove nothing about
/// behaviour, and section A's own numbers were arm counts.
fn sweep_reach(acc: &mut BTreeMap<(String, String), [Reach; 4]>, ops: &[Op], confined: bool) {
    for d in DIALECTS {
        let dname = format!("{d:?}");
        let mut prev: Option<[Result<String, String>; 4]> = None;
        for i in 0..=ops.len() {
            let replay = Replay::run(&ops[..i], d, confined);
            let cur: [Result<String, String>; 4] = [
                replay.text(Walker::Fo).map_err(ToString::to_string),
                replay.text(Walker::Ffd).map_err(ToString::to_string),
                replay.text(Walker::Ato).map_err(ToString::to_string),
                replay.text(Walker::Rmo).map_err(ToString::to_string),
            ];
            if i > 0 {
                let before = prev.as_ref().expect("a previous prefix exists for i > 0");
                let key = (variant(&ops[i - 1]), dname.clone());
                let cell = acc.entry(key).or_insert([Reach::Unobserved; 4]);
                for w in 0..4 {
                    let moved = match (&before[w], &cur[w]) {
                        (Ok(a), Ok(b)) => Some(a != b),
                        // Turning a coherent prefix into a refusal IS reaching
                        // the op: the walker decided something about it.
                        (Ok(_), Err(_)) => Some(true),
                        // The walker was already refusing before this op, so it
                        // never saw it. Not an observation in either direction.
                        (Err(_), _) => None,
                    };
                    match moved {
                        // One stream reaching a variant is enough; REACHES never
                        // downgrades.
                        Some(true) => cell[w] = Reach::Reaches,
                        Some(false) if cell[w] == Reach::Unobserved => {
                            cell[w] = Reach::Silent;
                        }
                        Some(false) | None => {}
                    }
                }
            }
            prev = Some(cur);
        }
    }
}

fn measured_reach() -> BTreeMap<(String, String), [Reach; 4]> {
    let mut acc = BTreeMap::new();
    // The recorded op fixtures are real drained envelopes and are already
    // policy-resolved, so they fold under the same confined charter that
    // produced them.
    for stem in STEMS {
        sweep_reach(&mut acc, &read_golden(stem).ops, true);
    }
    for s in STREAMS.iter().chain(CASES) {
        sweep_reach(&mut acc, &parse(s.ops), false);
    }
    acc
}

fn reach_lines(acc: &BTreeMap<(String, String), [Reach; 4]>) -> Vec<String> {
    acc.iter()
        .map(|((v, d), cells)| {
            format!(
                "{v}|{d}|{}{}{}{}",
                cells[0].short(),
                cells[1].short(),
                cells[2].short(),
                cells[3].short()
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Part 2: questions the four vocabularies can all be asked
// ---------------------------------------------------------------------------

/// A question at least two of the four walkers can answer in their OWN
/// vocabulary.
///
/// "What type is this column" is deliberately ABSENT. `ColumnSnapshot` speaks
/// catalog types (`character varying(40)`), `FieldDescriptor` speaks DSL
/// authoring tokens (`string` + `maxLength`), and `AuthoringTable` speaks
/// `ColType`. Comparing them directly would require a translation table, and a
/// translation table written here would be a FIFTH interpreter of what an op
/// means, which is the thing this corpus exists to stop growing.
///
/// What the vocabularies DO share is which objects a walker names, and whether
/// a given fact still appears in what it says about them. Both survive
/// translation; both are what the review log actually compared by hand.
#[derive(Clone, Copy)]
enum Question {
    /// The set of table / collection names in the walker's answer.
    Tables,
    /// The set of column / field names the walker gives one table.
    Columns(&'static str),
    /// The set of index names the walker gives one table.
    Indexes(&'static str),
    /// The runtime-visible collection options one table carries. Only two
    /// walkers model them, and one of the two is the artifact producer, so this
    /// is the narrowest cross-check in the corpus and the one section B row 7
    /// was about.
    RuntimeOptions(&'static str),
    /// The set of view names. Exactly ONE walker models a view, so this
    /// question always classifies as [`Verdict::Sole`]: recorded, and recorded
    /// as uncorroborated.
    Views,
    /// Does the walker's answer for ONE COLUMN still carry any of these tokens?
    /// The spellings are alternatives because each walker spells a facet its
    /// own way (`case_sensitive: Some(` in a snapshot, `"caseSensitive"` on the
    /// wire); the question is whether the FACT survived, not how it is written.
    ColumnCarries(&'static str, &'static str, &'static [&'static str]),
    /// Does the walker's WHOLE answer still spell this identifier? The rename
    /// probe: a walker still naming the pre-rename column has not followed the
    /// rename into one of its carriers.
    Carries(&'static [&'static str]),
}

impl Question {
    fn label(self) -> String {
        match self {
            Question::Tables => "tables".to_string(),
            Question::Columns(t) => format!("columns({t})"),
            Question::Indexes(t) => format!("indexes({t})"),
            Question::RuntimeOptions(t) => format!("runtime_options({t})"),
            Question::Views => "views".to_string(),
            Question::ColumnCarries(t, c, toks) => {
                format!("column_carries({t}.{c} ~ {})", toks.join("|"))
            }
            Question::Carries(toks) => format!("carries({})", toks.join("|")),
        }
    }
}

/// What one walker said when asked one question.
#[derive(PartialEq, Eq, Clone)]
enum Answer {
    /// The walker's output type cannot express the question. NOT the same as
    /// agreement, and never counted as one.
    NotApplicable,
    /// The walker refused the whole stream. A refusal is an answer ABOUT the
    /// stream, so it takes part in the verdict.
    Refused,
    /// The walker answered.
    Says(String),
}

impl Answer {
    fn render(&self) -> String {
        match self {
            Answer::NotApplicable => "n/a".to_string(),
            Answer::Refused => "refused".to_string(),
            Answer::Says(s) => s.clone(),
        }
    }
}

/// `haystack` contains `needle` at an identifier boundary.
///
/// The boundary rule is applied per END, and only where the needle's own edge
/// character is itself an identifier character. Without the first half a probe
/// for `qty` matches `qty_on_hand` and every rename question answers yes
/// forever. Without the second half a probe for `case_sensitive: Some(` is
/// REJECTED, because the character after the needle is the `f` of `false` -- a
/// broken instrument that fails toward "the facet is gone", which is the
/// direction that reads as a fix.
fn carries_ident(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let n = needle.len();
    if n == 0 {
        return false;
    }
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let guard_start = word(needle_bytes[0]);
    let guard_end = word(needle_bytes[n - 1]);
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(needle) {
        let at = from + rel;
        let before_ok = !guard_start || at == 0 || !word(bytes[at - 1]);
        let after_ok = !guard_end || at + n >= bytes.len() || !word(bytes[at + n]);
        if before_ok && after_ok {
            return true;
        }
        from = at + 1;
    }
    false
}

#[test]
fn the_identifier_probe_is_not_a_broken_instrument() {
    // Both failure directions of the instrument itself. A characterization
    // suite whose probe is wrong records a fiction and calls it a baseline, and
    // both of these bugs were live in this file before they were caught.
    assert!(!carries_ident("qty_on_hand", "qty"), "no substring matches");
    assert!(
        carries_ident("\"qty\" + 1", "qty"),
        "a quoted token matches"
    );
    assert!(
        carries_ident("total_cents", "total_cents"),
        "a whole token matches"
    );
    assert!(
        carries_ident(
            "case_sensitive: Some(\n    false,\n)",
            "case_sensitive: Some("
        ),
        "a needle ending in a non-identifier character is not boundary-guarded, \
         or every facet probe silently answers \"the facet is gone\""
    );
    assert!(
        !carries_ident("case_sensitive: None,", "case_sensitive: Some("),
        "an absent facet does not match a present one"
    );
    assert!(!carries_ident("abc", ""), "an empty needle matches nothing");
}

fn set(mut names: Vec<String>) -> String {
    names.sort_unstable();
    names.dedup();
    format!("{{{}}}", names.join(","))
}

fn yes_no(text: &str, toks: &[&str]) -> String {
    if toks.iter().any(|tok| carries_ident(text, tok)) {
        "yes".to_string()
    } else {
        "no".to_string()
    }
}

const NO_TABLE: &str = "<no such table>";

fn ask(replay: &Replay, w: Walker, q: Question) -> Answer {
    // A refusal short-circuits every question: the walker produced no answer at
    // all, and letting it read as agreement with a walker that did is the exact
    // silence this corpus exists to remove.
    if replay.text(w).is_err() {
        return Answer::Refused;
    }
    match q {
        Question::Tables => Answer::Says(match w {
            Walker::Fo => set(replay
                .fo
                .as_ref()
                .expect("ok")
                .tables
                .keys()
                .cloned()
                .collect()),
            Walker::Ffd => set(replay.ffd.as_ref().expect("ok").keys().cloned().collect()),
            Walker::Ato => set(replay.ato.as_ref().expect("ok").keys().cloned().collect()),
            Walker::Rmo => set(replay.rmo.as_ref().expect("ok").keys().cloned().collect()),
        }),
        Question::Columns(t) => match w {
            Walker::Fo => Answer::Says(replay.fo.as_ref().expect("ok").tables.get(t).map_or_else(
                || NO_TABLE.to_string(),
                |table| set(table.columns.iter().map(|c| c.name.clone()).collect()),
            )),
            Walker::Ffd => Answer::Says(
                replay
                    .ffd
                    .as_ref()
                    .expect("ok")
                    .get(t)
                    .and_then(|v| v.as_object())
                    .map_or_else(
                        || NO_TABLE.to_string(),
                        |o| set(o.keys().cloned().collect()),
                    ),
            ),
            Walker::Ato => Answer::Says(replay.ato.as_ref().expect("ok").get(t).map_or_else(
                || NO_TABLE.to_string(),
                |a| set(a.columns.keys().cloned().collect()),
            )),
            // The runtime-metadata replay carries options and indexes only; it
            // never names a column.
            Walker::Rmo => Answer::NotApplicable,
        },
        Question::Indexes(t) => match w {
            Walker::Fo => Answer::Says(replay.fo.as_ref().expect("ok").tables.get(t).map_or_else(
                || NO_TABLE.to_string(),
                |table| set(table.indexes.iter().map(|i| i.name.clone()).collect()),
            )),
            // The FieldDef projection carries no index at all.
            Walker::Ffd => Answer::NotApplicable,
            Walker::Ato => Answer::Says(replay.ato.as_ref().expect("ok").get(t).map_or_else(
                || NO_TABLE.to_string(),
                |a| {
                    set(a
                        .indexes
                        .iter()
                        .map(|i| i.name.clone().unwrap_or_else(|| "<unnamed>".to_string()))
                        .collect())
                },
            )),
            Walker::Rmo => Answer::Says(replay.rmo.as_ref().expect("ok").get(t).map_or_else(
                || NO_TABLE.to_string(),
                |m| set(m.indexes.iter().map(|i| i.name.clone()).collect()),
            )),
        },
        Question::RuntimeOptions(t) => match w {
            Walker::Fo => Answer::Says(replay.fo.as_ref().expect("ok").tables.get(t).map_or_else(
                || NO_TABLE.to_string(),
                |table| format!("{:?}", table.runtime_options),
            )),
            Walker::Rmo => Answer::Says(
                replay
                    .rmo
                    .as_ref()
                    .expect("ok")
                    .get(t)
                    .map_or_else(|| NO_TABLE.to_string(), |m| format!("{:?}", m.options)),
            ),
            Walker::Ffd | Walker::Ato => Answer::NotApplicable,
        },
        Question::Views => match w {
            Walker::Fo => Answer::Says(set(replay
                .fo
                .as_ref()
                .expect("ok")
                .views
                .keys()
                .cloned()
                .collect())),
            // No artifact walker models a view at all.
            Walker::Ffd | Walker::Ato | Walker::Rmo => Answer::NotApplicable,
        },
        Question::ColumnCarries(t, c, toks) => {
            let text = match w {
                Walker::Fo => replay
                    .fo
                    .as_ref()
                    .expect("ok")
                    .tables
                    .get(t)
                    .and_then(|table| table.columns.iter().find(|col| col.name == c))
                    .map(|col| format!("{col:#?}")),
                Walker::Ffd => replay
                    .ffd
                    .as_ref()
                    .expect("ok")
                    .get(t)
                    .and_then(|v| v.as_object())
                    .and_then(|o| o.get(c))
                    .map(ToString::to_string),
                Walker::Ato => replay
                    .ato
                    .as_ref()
                    .expect("ok")
                    .get(t)
                    .and_then(|a| a.columns.get(c))
                    .map(|col| format!("{col:#?}")),
                Walker::Rmo => return Answer::NotApplicable,
            };
            Answer::Says(text.map_or_else(
                || "<no such column>".to_string(),
                |text| yes_no(&text, toks),
            ))
        }
        Question::Carries(toks) => Answer::Says(yes_no(&replay.text(w).expect("ok"), toks)),
    }
}

// ---------------------------------------------------------------------------
// The three-way classification
// ---------------------------------------------------------------------------

/// The recorded verdict for one question, one stream, one dialect.
///
/// The distinction between [`Verdict::Agreed`] and [`Verdict::Sole`] is the
/// point of the whole file. A walker that swallows an op is not agreeing about
/// it, and a question only one walker can answer has NO cross-check at all: a
/// defect there is invisible to this corpus by construction, and the row is
/// required to say so.
enum Verdict {
    /// Two or more walkers can answer and they all say the same thing.
    Agreed(String),
    /// Exactly one walker can answer. Recorded, but nothing corroborates it.
    Sole(String, String),
    /// Two or more walkers can answer and they disagree. Every one of them is
    /// listed, and the row carries its evidence.
    Divergent(Vec<(String, String)>),
    /// No walker can answer.
    NoAuthority,
}

impl Verdict {
    fn render(&self) -> String {
        match self {
            Verdict::Agreed(a) => format!("AGREED {a}"),
            Verdict::Sole(w, a) => format!("SOLE {w}={a}"),
            Verdict::Divergent(rows) => format!(
                "DIVERGENT {}",
                rows.iter()
                    .map(|(w, a)| format!("{w}={a}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Verdict::NoAuthority => "NO-AUTHORITY".to_string(),
        }
    }
}

fn classify(replay: &Replay, q: Question) -> Verdict {
    let answers: Vec<(Walker, Answer)> = WALKERS
        .iter()
        .map(|&w| (w, ask(replay, w, q)))
        .filter(|(_, a)| *a != Answer::NotApplicable)
        .collect();
    match answers.len() {
        0 => Verdict::NoAuthority,
        1 => Verdict::Sole(answers[0].0.short().to_string(), answers[0].1.render()),
        _ => {
            let first = answers[0].1.render();
            if answers.iter().all(|(_, a)| a.render() == first) {
                Verdict::Agreed(first)
            } else {
                Verdict::Divergent(
                    answers
                        .into_iter()
                        .map(|(w, a)| (w.short().to_string(), a.render()))
                        .collect(),
                )
            }
        }
    }
}

fn case_lines() -> Vec<String> {
    let mut lines = Vec::new();
    for c in CASES {
        let ops = parse(c.ops);
        for d in DIALECTS {
            let replay = Replay::run(&ops, d, false);
            for q in probes_for(c.name) {
                lines.push(format!(
                    "{}|{d:?}|{}|{}",
                    c.name,
                    q.label(),
                    classify(&replay, q).render()
                ));
            }
        }
    }
    lines.sort();
    lines
}
/// The questions asked of each case. Kept small and load-bearing on purpose:
/// every question here is one the review log asked BY HAND about some shipped
/// divergence, and each is answerable by at least two of the four walkers.
fn probes_for(case: &str) -> Vec<Question> {
    match case {
        "c_dialectal_leg_selection" => vec![
            Question::Columns("docs"),
            Question::Indexes("docs"),
            Question::RuntimeOptions("docs"),
        ],
        "c_view_is_seen_by_one_walker" => vec![Question::Views, Question::Tables],
        "c_rename_column_generated_expr" => vec![
            Question::Columns("line_items"),
            Question::Carries(&["qty_on_hand"]),
        ],
        "c_rename_table_in_generated_expr" => {
            vec![Question::Tables, Question::Carries(&["order_lines"])]
        }
        "c_retype_drops_case_sensitivity" => vec![Question::ColumnCarries(
            "labels",
            "tag",
            &["case_sensitive: Some(", "\"caseSensitive\""],
        )],
        "c_retype_type_parameters" => vec![
            Question::ColumnCarries("shapes", "narrow", &["24"]),
            Question::ColumnCarries("shapes", "fixed", &["8"]),
            Question::ColumnCarries("shapes", "embedding", &["3"]),
        ],
        "c_retype_of_a_value_format_column" => vec![Question::Tables],
        "c_named_enum_membership" => vec![Question::ColumnCarries("issues", "status", &["open"])],
        "c_domain_base_type" => vec![
            Question::ColumnCarries("amounts", "amount", &["positive_number"]),
            Question::ColumnCarries("amounts", "amount", &["int", "Int", "integer", "INTEGER"]),
        ],
        "c_encrypted_domain_column" => vec![Question::ColumnCarries(
            "amounts",
            "amount",
            &["int", "Int", "integer", "INTEGER"],
        )],
        "c_rename_column_inline_check_body" => vec![
            Question::Columns("issues"),
            Question::Carries(&["state_token"]),
        ],
        "c_rename_column_fk_definition" => vec![Question::Carries(&["owner_id"])],
        "c_rename_column_index_key_columns" => vec![
            Question::Indexes("stock"),
            Question::Carries(&["legacy_qty"]),
        ],
        "c_rename_column_index_expression" | "c_rename_column_index_include_and_predicate" => {
            vec![Question::Carries(&["legacy_qty"])]
        }
        "c_drop_column_cascade_dialectal_expr" => vec![
            Question::Columns("legs"),
            Question::Carries(&["legs_leg_ck"]),
        ],
        "c_create_then_add_then_retype" => vec![
            Question::Columns("invoices"),
            Question::ColumnCarries("invoices", "total", &["24"]),
        ],
        "c_two_renames_in_sequence" => vec![
            Question::Columns("stages"),
            Question::Carries(&["first_name"]),
            Question::Carries(&["middle_name"]),
        ],
        "c_rename_table_then_rename_column" => {
            vec![Question::Tables, Question::Carries(&["old_label"])]
        }
        "c_create_drop_recreate_same_name" => vec![
            Question::Columns("drafts"),
            Question::Indexes("drafts"),
            Question::Carries(&["first_shape"]),
            Question::Carries(&["drafts_first_idx"]),
        ],
        other => panic!("no probes declared for case {other}"),
    }
}
// ---------------------------------------------------------------------------
// The recorded corpus
// ---------------------------------------------------------------------------

/// How a recorded row should be READ. The verdict says what the walkers do; the
/// status says whether that is acceptable, and a row that records a defect is
/// required to name its evidence.
#[derive(Clone, Copy)]
enum Status {
    /// Every walker that can answer agrees, and the agreed answer is right.
    /// Safe to lock in: a later single fold must reproduce it.
    Consistent,
    /// They differ, and the difference is a property of the vocabularies rather
    /// than a defect. The reason is stated here so it can be argued with rather
    /// than inherited.
    ByDesign(&'static str),
    /// They differ, or they agree, and at least one of them is WRONG. This row
    /// records WHAT HAPPENS TODAY AND IT IS WRONG. It is not a specification,
    /// and the single fold must NOT be held to it.
    ///
    /// Currently UNCONSTRUCTED: the rename carrier sweep re-measured the last
    /// three defect rows to `AGREED`. The variant stays because the corpus's
    /// job is to have somewhere honest to put the next one - deleting the
    /// vocabulary the moment the count reaches zero is how a corpus stops being
    /// able to record bad news. `the_corpus_has_the_shape_it_claims` counts it,
    /// and `every_defect_row_names_its_evidence` still governs how one is
    /// written.
    #[expect(
        dead_code,
        reason = "the defect vocabulary outlives the current defect count; see the \
                  doc comment"
    )]
    Defect(&'static str),
    /// Fewer than two walkers can answer. Recorded, uncorroborated: a defect
    /// here is invisible to this corpus, by construction.
    Uncorroborated(&'static str),
}

/// One recorded row: a measured verdict plus how to read it.
struct Row {
    /// `case|Dialect|question`.
    key: &'static str,
    /// The measured verdict, verbatim.
    verdict: &'static str,
    status: Status,
}

/// A primary key is a CONSTRAINT in the authoring vocabulary and an INDEX in a
/// catalog. `fold_ops` models the catalog and stamps the backing index; the two
/// artifact walkers model what the author wrote and do not. Recorded rather
/// than smoothed over: it is the single most common shape of index divergence
/// in this corpus, and a single fold has to keep both readings available.
const PK_IS_AN_INDEX_ONLY_IN_A_CATALOG: &str =
    "fold_ops stamps the primary key's backing index because a catalog has one; \
     the two artifact walkers describe authored objects, where a primary key is a \
     constraint and not an index";

/// The three walkers that can describe a column say three different true things
/// about a named type, each in its own vocabulary. `docs/review-log.md:29149-29156`
/// tabulates exactly this and calls the three-way spread correct.
const A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS: &str =
    "the three column-describing walkers speak three vocabularies about a named \
     type: fold_ops reports the STORAGE the dialect gives it, \
     authoring_tables_from_ops keeps the type NAME the author wrote, and \
     fold_to_field_defs reports the resolved base the runtime validates against \
     (docs/review-log.md:29149-29156)";

/// A `Carries` question over a walker whose vocabulary cannot contain the thing
/// asked about answers "no" truthfully and uninformatively, and the classifier
/// cannot tell that apart from a disagreement. Named so the rows that hit it are
/// readable rather than alarming, and so the limitation is stated once.
const A_TEXT_PROBE_CANNOT_SEE_AN_EMPTY_VOCABULARY: &str =
    "fold_to_field_defs carries no constraint and the runtime metadata \
     carries only plain indexes, so their 'no' is the absence of a vocabulary \
     rather than a contradiction; the two answers that DO name constraints agree";

/// ONE walker left in this corpus has no coherence gate.
///
/// It was two until step 4. The runtime collection metadata became a projection of
/// the fold, which fails closed, so it now refuses every stream the catalog replay
/// refuses - which is why the rows carrying this note read `RMO=refused` where they
/// used to carry an answer. `authoring_tables_from_ops` is the one that still
/// answers, and it is harmless at the composite entry point for the same reason it
/// always was: `render_artifacts` returns the fold's error and emits neither
/// artifact. Verified by reading `render_artifacts` and pinned by
/// `tests/gen_types_runtime_metadata_from_the_fold.rs`'s over-refusal control.
const ONLY_THE_FOLD_BACKED_WALKERS_FAIL_CLOSED: &str =
    "authoring_tables_from_ops has no coherence gate and answers about a stream \
     the fold refuses; harmless at the composite entry point, because \
     render_artifacts returns the fold's error and emits neither artifact";

// THE THREE RENDERED-SQL RENAME CARRIERS THIS CORPUS FOUND ARE NOW CLOSED, and
// their rows below read `AGREED no` / `Consistent` rather than
// `DIVERGENT FO=yes` / `Defect`. Recorded here because the constants that named
// them are gone and a reader diffing this file deserves to know why.
//
//   c_rename_column_inline_check_body|Postgres|carries(state_token)
//     -- a table-level CHECK's `ConstraintSnapshot::definition`. This corpus
//        found it; `docs/review-log.md:6837-6847` had listed four carriers and
//        this was a fifth of the same kind, and the field IS compared by
//        `ConstraintSnapshot`'s hand-written `PartialEq`.
//   c_rename_column_index_expression|{Postgres,Sqlite}|carries(legacy_qty)
//     -- `IndexElementSnapshot::Expr`, one of that list's four.
//   c_rename_column_index_include_and_predicate|{Postgres,Sqlite}|carries(legacy_qty)
//     -- `IndexSnapshot::predicate`, another of them.
//
// What changed is the TOOL, not the appetite for risk. Every one of them was
// left stale on the stated grounds that substituting a column name inside
// rendered SQL would corrupt a string literal spelling the same word
// (`WHERE (note <> 'a')` becoming `WHERE (note <> 'b')`). That reason outlived
// the objection it answered: `render::declarative::rename_quoted_column_in_sql`
// walks the fragment as QUOTED RUNS, so a literal is copied through whole and
// the trap is structurally unreachable. It was written for `inline_checks` and
// nobody went back to the three siblings it also solved - which is the F113
// pattern (fixing one instance suppresses the search for its class) with the
// tool, rather than the fix, as the thing not carried across.
//
// Measured end to end against live PostgreSQL 18.4 and real SQLite by
// `rename_carrier_sweep_pg` / `rename_carrier_sweep_sqlite`, which also pin the
// literal's SURVIVAL - a rewrite that corrupts it is worse than the staleness.

const MYSQL_HAS_NO_EXPRESSION_OR_PARTIAL_INDEX: &str =
    "MySQL has no partial or expression index in this engine, so the two \
     fold-backed walkers refuse the stream while the artifact walkers, which \
     apply no capability gate, answer about it";

const PG_ONLY_TABLE_LEVEL_CHECK: &str =
    "a createTable table-level CHECK is PostgreSQL-only, so the two fold-backed \
     walkers refuse off Postgres while the artifact walkers answer";

/// Exactly one of the four walkers models a view. Recorded so the corpus states
/// its own blind spots rather than implying every question it asks has a second
/// opinion behind it.
const A_VIEW_HAS_ONE_AUTHORITY: &str =
    "only fold_ops models a view; no artifact walker has anywhere to put one, so \
     this corpus cannot cross-check anything fold_ops decides about views, and a \
     defect there would be invisible here";

// One row per line, so the corpus reads as a table and a change to it reads as a
// one-line diff. Wrapped rows would put the key, the verdict and the evidence on
// separate lines and make a moved verdict look like four changes.
#[rustfmt::skip]
const ROWS: &[Row] = &[
    // --- Op::Dialectal, section B rows 7 and 8 ------------------------------
    //
    // Both FIXED. Leg selection now follows the target on every walker (row 8,
    // docs/review-log.md:2931-2942), and the PostgreSQL leg's runtime option and
    // plain index reach runtime_metadata_from_ops (row 7,
    // docs/review-log.md:18536-18539) -- which is what the `indexes(docs)` row
    // shows on Postgres and does not show on the other two.
    Row { key: "c_dialectal_leg_selection|Postgres|columns(docs)", verdict: "AGREED {id,pg_only}", status: Status::Consistent },
    Row { key: "c_dialectal_leg_selection|Sqlite|columns(docs)", verdict: "AGREED {id,sqlite_only}", status: Status::Consistent },
    Row { key: "c_dialectal_leg_selection|Mysql|columns(docs)", verdict: "AGREED {id,mysql_only}", status: Status::Consistent },
    Row { key: "c_dialectal_leg_selection|Postgres|runtime_options(docs)", verdict: "AGREED TableRuntimeOptions { soft_delete: true, versioning: false, strictness: Strict }", status: Status::Consistent },
    Row { key: "c_dialectal_leg_selection|Sqlite|runtime_options(docs)", verdict: "AGREED TableRuntimeOptions { soft_delete: false, versioning: false, strictness: Strict }", status: Status::Consistent },
    Row { key: "c_dialectal_leg_selection|Mysql|runtime_options(docs)", verdict: "AGREED TableRuntimeOptions { soft_delete: false, versioning: false, strictness: Strict }", status: Status::Consistent },
    Row { key: "c_dialectal_leg_selection|Postgres|indexes(docs)", verdict: "DIVERGENT FO={docs_pg_idx,docs_pkey} ATO={docs_pg_idx} RMO={docs_pg_idx}", status: Status::ByDesign(PK_IS_AN_INDEX_ONLY_IN_A_CATALOG) },
    Row { key: "c_dialectal_leg_selection|Sqlite|indexes(docs)", verdict: "DIVERGENT FO={docs_pkey} ATO={} RMO={}", status: Status::ByDesign(PK_IS_AN_INDEX_ONLY_IN_A_CATALOG) },
    Row { key: "c_dialectal_leg_selection|Mysql|indexes(docs)", verdict: "DIVERGENT FO={docs_pkey} ATO={} RMO={}", status: Status::ByDesign(PK_IS_AN_INDEX_ONLY_IN_A_CATALOG) },

    // --- a question with no second opinion ----------------------------------
    Row { key: "c_view_is_seen_by_one_walker|Postgres|tables", verdict: "AGREED {people}", status: Status::Consistent },
    Row { key: "c_view_is_seen_by_one_walker|Sqlite|tables", verdict: "AGREED {people}", status: Status::Consistent },
    Row { key: "c_view_is_seen_by_one_walker|Mysql|tables", verdict: "AGREED {people}", status: Status::Consistent },
    Row { key: "c_view_is_seen_by_one_walker|Postgres|views", verdict: "SOLE FO={active_people}", status: Status::Uncorroborated(A_VIEW_HAS_ONE_AUTHORITY) },
    Row { key: "c_view_is_seen_by_one_walker|Sqlite|views", verdict: "SOLE FO={active_people}", status: Status::Uncorroborated(A_VIEW_HAS_ONE_AUTHORITY) },
    Row { key: "c_view_is_seen_by_one_walker|Mysql|views", verdict: "SOLE FO={active_people}", status: Status::Uncorroborated(A_VIEW_HAS_ONE_AUTHORITY) },

    // --- the shipped section B divergences, re-measured ---------------------
    //
    // Row 1 of section B (`renameColumn` + generated expression,
    // docs/review-log.md:26342-26360): FIXED, and it reproduces as agreement.
    Row { key: "c_rename_column_generated_expr|Postgres|columns(line_items)", verdict: "AGREED {id,quantity,total_cents}", status: Status::Consistent },
    Row { key: "c_rename_column_generated_expr|Sqlite|columns(line_items)", verdict: "AGREED {id,quantity,total_cents}", status: Status::Consistent },
    Row { key: "c_rename_column_generated_expr|Mysql|columns(line_items)", verdict: "AGREED {id,quantity,total_cents}", status: Status::Consistent },
    Row { key: "c_rename_column_generated_expr|Postgres|carries(qty_on_hand)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_generated_expr|Sqlite|carries(qty_on_hand)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_generated_expr|Mysql|carries(qty_on_hand)", verdict: "AGREED no", status: Status::Consistent },

    // Row 2 (`renameTable` inside a generated expression,
    // docs/review-log.md:26370-26374, the row where ONE renderArtifacts call
    // produced THREE answers): FIXED, and it reproduces as agreement.
    Row { key: "c_rename_table_in_generated_expr|Postgres|tables", verdict: "AGREED {line_items}", status: Status::Consistent },
    Row { key: "c_rename_table_in_generated_expr|Sqlite|tables", verdict: "AGREED {line_items}", status: Status::Consistent },
    Row { key: "c_rename_table_in_generated_expr|Mysql|tables", verdict: "AGREED {line_items}", status: Status::Consistent },
    Row { key: "c_rename_table_in_generated_expr|Postgres|carries(order_lines)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_table_in_generated_expr|Sqlite|carries(order_lines)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_table_in_generated_expr|Mysql|carries(order_lines)", verdict: "AGREED no", status: Status::Consistent },

    // Rows 3 and 6 (`setColumnType` facet residue, docs/review-log.md:26375-26382
    // and 26717-26733): FIXED on all three dialects.
    Row { key: "c_retype_drops_case_sensitivity|Postgres|column_carries(labels.tag ~ case_sensitive: Some(|\"caseSensitive\")", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_retype_drops_case_sensitivity|Sqlite|column_carries(labels.tag ~ case_sensitive: Some(|\"caseSensitive\")", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_retype_drops_case_sensitivity|Mysql|column_carries(labels.tag ~ case_sensitive: Some(|\"caseSensitive\")", verdict: "AGREED no", status: Status::Consistent },

    // Row 4 (`setColumnType` losing a parameter going in and keeping it coming
    // out, docs/review-log.md:26632-26654): FIXED on Postgres and SQLite when that
    // row shipped, and FIXED on MySQL afterwards - the MySQL half rode on
    // `mysql_physical_type`, a field that row never looked at, and the fold now
    // re-derives that contract from the column the replay finished with rather than
    // from the type it briefly had. The three walkers needed no matching change:
    // FFD and ATO already answered `no` here, and only FO was wrong.
    Row { key: "c_retype_type_parameters|Postgres|column_carries(shapes.narrow ~ 24)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_retype_type_parameters|Sqlite|column_carries(shapes.narrow ~ 24)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_retype_type_parameters|Mysql|column_carries(shapes.narrow ~ 24)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_retype_type_parameters|Postgres|column_carries(shapes.fixed ~ 8)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_retype_type_parameters|Sqlite|column_carries(shapes.fixed ~ 8)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_retype_type_parameters|Mysql|column_carries(shapes.fixed ~ 8)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_retype_type_parameters|Postgres|column_carries(shapes.embedding ~ 3)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_retype_type_parameters|Sqlite|column_carries(shapes.embedding ~ 3)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_retype_type_parameters|Mysql|column_carries(shapes.embedding ~ 3)", verdict: "AGREED no", status: Status::Consistent },

    // Row 5 (`setColumnType` on a `value_format` column,
    // docs/review-log.md:26663-26715): the verdict there was REFUSE, and the two
    // fold-backed walkers now do. The other two do not, which is what this row
    // is really recording.
    Row { key: "c_retype_of_a_value_format_column|Postgres|tables", verdict: "DIVERGENT FO=refused FFD=refused ATO={refs} RMO=refused", status: Status::ByDesign(ONLY_THE_FOLD_BACKED_WALKERS_FAIL_CLOSED) },
    Row { key: "c_retype_of_a_value_format_column|Sqlite|tables", verdict: "DIVERGENT FO=refused FFD=refused ATO={refs} RMO=refused", status: Status::ByDesign(ONLY_THE_FOLD_BACKED_WALKERS_FAIL_CLOSED) },
    Row { key: "c_retype_of_a_value_format_column|Mysql|tables", verdict: "DIVERGENT FO=refused FFD=refused ATO={refs} RMO=refused", status: Status::ByDesign(ONLY_THE_FOLD_BACKED_WALKERS_FAIL_CLOSED) },

    // Row 9 (named enum members, docs/review-log.md:27941-27944): FIXED --
    // fold_to_field_defs carries the membership on all three dialects. Note what
    // the row also shows: on Postgres NOTHING ELSE carries it, so nothing here
    // cross-checks the fix.
    Row { key: "c_named_enum_membership|Postgres|column_carries(issues.status ~ open)", verdict: "DIVERGENT FO=no FFD=yes ATO=no", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },
    Row { key: "c_named_enum_membership|Sqlite|column_carries(issues.status ~ open)", verdict: "DIVERGENT FO=yes FFD=yes ATO=no", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },
    Row { key: "c_named_enum_membership|Mysql|column_carries(issues.status ~ open)", verdict: "DIVERGENT FO=yes FFD=yes ATO=no", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },

    // Row 10 (domain base type, docs/review-log.md:29149-29156): FIXED --
    // fold_to_field_defs reports the base type rather than "string".
    Row { key: "c_domain_base_type|Postgres|column_carries(amounts.amount ~ positive_number)", verdict: "DIVERGENT FO=yes FFD=no ATO=yes", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },
    Row { key: "c_domain_base_type|Sqlite|column_carries(amounts.amount ~ positive_number)", verdict: "DIVERGENT FO=no FFD=no ATO=yes", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },
    Row { key: "c_domain_base_type|Mysql|column_carries(amounts.amount ~ positive_number)", verdict: "DIVERGENT FO=no FFD=no ATO=yes", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },
    Row { key: "c_domain_base_type|Postgres|column_carries(amounts.amount ~ int|Int|integer|INTEGER)", verdict: "DIVERGENT FO=no FFD=yes ATO=no", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },
    Row { key: "c_domain_base_type|Sqlite|column_carries(amounts.amount ~ int|Int|integer|INTEGER)", verdict: "DIVERGENT FO=yes FFD=yes ATO=no", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },
    Row { key: "c_domain_base_type|Mysql|column_carries(amounts.amount ~ int|Int|integer|INTEGER)", verdict: "DIVERGENT FO=yes FFD=yes ATO=no", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },

    // The encrypted-domain sentinel fix (docs/review-log.md:29643-29667): the
    // resolved base reaches fold_to_field_defs on all three dialects.
    Row { key: "c_encrypted_domain_column|Postgres|column_carries(amounts.amount ~ int|Int|integer|INTEGER)", verdict: "DIVERGENT FO=no FFD=yes ATO=no", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },
    Row { key: "c_encrypted_domain_column|Sqlite|column_carries(amounts.amount ~ int|Int|integer|INTEGER)", verdict: "DIVERGENT FO=no FFD=yes ATO=no", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },
    Row { key: "c_encrypted_domain_column|Mysql|column_carries(amounts.amount ~ int|Int|integer|INTEGER)", verdict: "DIVERGENT FO=no FFD=yes ATO=no", status: Status::ByDesign(A_NAMED_TYPE_HAS_THREE_TRUE_SPELLINGS) },

    // Row 11 (`renameColumn` + a CHECK body, docs/review-log.md:28198-28204):
    // the COLUMN-level inline check was fixed. A TABLE-level CHECK's
    // `definition` is a different carrier and is still stale.
    Row { key: "c_rename_column_inline_check_body|Postgres|columns(issues)", verdict: "AGREED {id,status_token}", status: Status::Consistent },
    Row { key: "c_rename_column_inline_check_body|Sqlite|columns(issues)", verdict: "DIVERGENT FO=refused FFD=refused ATO={id,status_token} RMO=refused", status: Status::ByDesign(PG_ONLY_TABLE_LEVEL_CHECK) },
    Row { key: "c_rename_column_inline_check_body|Mysql|columns(issues)", verdict: "DIVERGENT FO=refused FFD=refused ATO={id,status_token} RMO=refused", status: Status::ByDesign(PG_ONLY_TABLE_LEVEL_CHECK) },
    Row { key: "c_rename_column_inline_check_body|Postgres|carries(state_token)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_inline_check_body|Sqlite|carries(state_token)", verdict: "DIVERGENT FO=refused FFD=refused ATO=no RMO=refused", status: Status::ByDesign(PG_ONLY_TABLE_LEVEL_CHECK) },
    Row { key: "c_rename_column_inline_check_body|Mysql|carries(state_token)", verdict: "DIVERGENT FO=refused FFD=refused ATO=no RMO=refused", status: Status::ByDesign(PG_ONLY_TABLE_LEVEL_CHECK) },

    // Row 12 (`renameColumn` + FK constraint definition,
    // docs/review-log.md:28404-28416): FIXED, no walker keeps the old name.
    Row { key: "c_rename_column_fk_definition|Postgres|carries(owner_id)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_fk_definition|Sqlite|carries(owner_id)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_fk_definition|Mysql|carries(owner_id)", verdict: "AGREED no", status: Status::Consistent },

    // Row 13 (`renameColumn` + IndexSnapshot name lists,
    // docs/review-log.md:6813-6829): the KEY COLUMN lists were fixed (F113)...
    Row { key: "c_rename_column_index_key_columns|Postgres|carries(legacy_qty)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_index_key_columns|Sqlite|carries(legacy_qty)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_index_key_columns|Mysql|carries(legacy_qty)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_index_key_columns|Postgres|indexes(stock)", verdict: "DIVERGENT FO={stock_multi_idx,stock_pkey,stock_qty_idx} ATO={stock_multi_idx,stock_qty_idx} RMO={stock_multi_idx,stock_qty_idx}", status: Status::ByDesign(PK_IS_AN_INDEX_ONLY_IN_A_CATALOG) },
    Row { key: "c_rename_column_index_key_columns|Sqlite|indexes(stock)", verdict: "DIVERGENT FO={stock_multi_idx,stock_pkey,stock_qty_idx} ATO={stock_multi_idx,stock_qty_idx} RMO={stock_multi_idx,stock_qty_idx}", status: Status::ByDesign(PK_IS_AN_INDEX_ONLY_IN_A_CATALOG) },
    Row { key: "c_rename_column_index_key_columns|Mysql|indexes(stock)", verdict: "DIVERGENT FO={stock_multi_idx,stock_pkey,stock_qty_idx} ATO={stock_multi_idx,stock_qty_idx} RMO={stock_multi_idx,stock_qty_idx}", status: Status::ByDesign(PK_IS_AN_INDEX_ONLY_IN_A_CATALOG) },

    // ... and the two rendered-SQL index carriers that entry recorded as still
    // open at 6837-6847 are still open. This is the corpus reproducing an
    // OPEN defect, which is the one thing that proves it can see one.
    Row { key: "c_rename_column_index_expression|Postgres|carries(legacy_qty)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_index_expression|Sqlite|carries(legacy_qty)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_index_expression|Mysql|carries(legacy_qty)", verdict: "DIVERGENT FO=refused FFD=refused ATO=no RMO=refused", status: Status::ByDesign(MYSQL_HAS_NO_EXPRESSION_OR_PARTIAL_INDEX) },
    Row { key: "c_rename_column_index_include_and_predicate|Postgres|carries(legacy_qty)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_index_include_and_predicate|Sqlite|carries(legacy_qty)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_column_index_include_and_predicate|Mysql|carries(legacy_qty)", verdict: "DIVERGENT FO=refused FFD=refused ATO=no RMO=refused", status: Status::ByDesign(MYSQL_HAS_NO_EXPRESSION_OR_PARTIAL_INDEX) },

    // Row 14 (`dropColumn` cascade + `Expr::Dialectal`,
    // docs/review-log.md:6873-6878): FIXED on Postgres -- the CHECK whose
    // SELECTED leg names only the surviving column is kept by both walkers that
    // model constraints.
    Row { key: "c_drop_column_cascade_dialectal_expr|Postgres|columns(legs)", verdict: "AGREED {id,kept}", status: Status::Consistent },
    Row { key: "c_drop_column_cascade_dialectal_expr|Sqlite|columns(legs)", verdict: "DIVERGENT FO=refused FFD=refused ATO={id,kept} RMO=refused", status: Status::ByDesign(PG_ONLY_TABLE_LEVEL_CHECK) },
    Row { key: "c_drop_column_cascade_dialectal_expr|Mysql|columns(legs)", verdict: "DIVERGENT FO=refused FFD=refused ATO={id,kept} RMO=refused", status: Status::ByDesign(PG_ONLY_TABLE_LEVEL_CHECK) },
    Row { key: "c_drop_column_cascade_dialectal_expr|Postgres|carries(legs_leg_ck)", verdict: "DIVERGENT FO=yes FFD=no ATO=yes RMO=no", status: Status::ByDesign(A_TEXT_PROBE_CANNOT_SEE_AN_EMPTY_VOCABULARY) },
    Row { key: "c_drop_column_cascade_dialectal_expr|Sqlite|carries(legs_leg_ck)", verdict: "DIVERGENT FO=refused FFD=refused ATO=no RMO=refused", status: Status::ByDesign(PG_ONLY_TABLE_LEVEL_CHECK) },
    Row { key: "c_drop_column_cascade_dialectal_expr|Mysql|carries(legs_leg_ck)", verdict: "DIVERGENT FO=refused FFD=refused ATO=yes RMO=refused", status: Status::ByDesign(PG_ONLY_TABLE_LEVEL_CHECK) },

    // --- multi-op streams ---------------------------------------------------
    //
    // Several of the defects in section B appeared only in sequence. These three
    // streams are the sequences, and one of them is where the MySQL retype
    // residue above showed up a second time, through addColumn rather than
    // createTable -- which is what said the defect was in the retype arm and not
    // in one column constructor. Both rows moved together when it was fixed,
    // which is the same evidence read the other way round.
    Row { key: "c_create_then_add_then_retype|Postgres|columns(invoices)", verdict: "AGREED {id,total}", status: Status::Consistent },
    Row { key: "c_create_then_add_then_retype|Sqlite|columns(invoices)", verdict: "AGREED {id,total}", status: Status::Consistent },
    Row { key: "c_create_then_add_then_retype|Mysql|columns(invoices)", verdict: "AGREED {id,total}", status: Status::Consistent },
    Row { key: "c_create_then_add_then_retype|Postgres|column_carries(invoices.total ~ 24)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_create_then_add_then_retype|Sqlite|column_carries(invoices.total ~ 24)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_create_then_add_then_retype|Mysql|column_carries(invoices.total ~ 24)", verdict: "AGREED no", status: Status::Consistent },

    Row { key: "c_two_renames_in_sequence|Postgres|columns(stages)", verdict: "AGREED {derived,id,last_name}", status: Status::Consistent },
    Row { key: "c_two_renames_in_sequence|Sqlite|columns(stages)", verdict: "AGREED {derived,id,last_name}", status: Status::Consistent },
    Row { key: "c_two_renames_in_sequence|Mysql|columns(stages)", verdict: "AGREED {derived,id,last_name}", status: Status::Consistent },
    Row { key: "c_two_renames_in_sequence|Postgres|carries(first_name)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_two_renames_in_sequence|Sqlite|carries(first_name)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_two_renames_in_sequence|Mysql|carries(first_name)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_two_renames_in_sequence|Postgres|carries(middle_name)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_two_renames_in_sequence|Sqlite|carries(middle_name)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_two_renames_in_sequence|Mysql|carries(middle_name)", verdict: "AGREED no", status: Status::Consistent },

    Row { key: "c_rename_table_then_rename_column|Postgres|tables", verdict: "AGREED {gadgets}", status: Status::Consistent },
    Row { key: "c_rename_table_then_rename_column|Sqlite|tables", verdict: "AGREED {gadgets}", status: Status::Consistent },
    Row { key: "c_rename_table_then_rename_column|Mysql|tables", verdict: "AGREED {gadgets}", status: Status::Consistent },
    Row { key: "c_rename_table_then_rename_column|Postgres|carries(old_label)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_table_then_rename_column|Sqlite|carries(old_label)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_rename_table_then_rename_column|Mysql|carries(old_label)", verdict: "AGREED no", status: Status::Consistent },

    Row { key: "c_create_drop_recreate_same_name|Postgres|columns(drafts)", verdict: "AGREED {id,second_shape}", status: Status::Consistent },
    Row { key: "c_create_drop_recreate_same_name|Sqlite|columns(drafts)", verdict: "AGREED {id,second_shape}", status: Status::Consistent },
    Row { key: "c_create_drop_recreate_same_name|Mysql|columns(drafts)", verdict: "AGREED {id,second_shape}", status: Status::Consistent },
    Row { key: "c_create_drop_recreate_same_name|Postgres|carries(first_shape)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_create_drop_recreate_same_name|Sqlite|carries(first_shape)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_create_drop_recreate_same_name|Mysql|carries(first_shape)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_create_drop_recreate_same_name|Postgres|carries(drafts_first_idx)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_create_drop_recreate_same_name|Sqlite|carries(drafts_first_idx)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_create_drop_recreate_same_name|Mysql|carries(drafts_first_idx)", verdict: "AGREED no", status: Status::Consistent },
    Row { key: "c_create_drop_recreate_same_name|Postgres|indexes(drafts)", verdict: "DIVERGENT FO={drafts_pkey} ATO={} RMO={}", status: Status::ByDesign(PK_IS_AN_INDEX_ONLY_IN_A_CATALOG) },
    Row { key: "c_create_drop_recreate_same_name|Sqlite|indexes(drafts)", verdict: "DIVERGENT FO={drafts_pkey} ATO={} RMO={}", status: Status::ByDesign(PK_IS_AN_INDEX_ONLY_IN_A_CATALOG) },
    Row { key: "c_create_drop_recreate_same_name|Mysql|indexes(drafts)", verdict: "DIVERGENT FO={drafts_pkey} ATO={} RMO={}", status: Status::ByDesign(PK_IS_AN_INDEX_ONLY_IN_A_CATALOG) },
];

/// Which op variants each walker reaches, measured by prefix sweep, one line
/// per `variant|Dialect`. Cells are `FO FFD ATO RMO`, `R`eaches / `S`ilent /
/// `-` unobserved.
const REACH: &[&str] = &[
    "addColumn|Mysql|RRRR",
    "addColumn|Postgres|RRRR",
    "addColumn|Sqlite|RRRR",
    "addConstraint|Mysql|RRRR",
    "addConstraint|Postgres|RRRR",
    "addConstraint|Sqlite|RRRR",
    "alterPrimaryKey|Mysql|RRSR",
    "alterPrimaryKey|Postgres|RRSR",
    "alterPrimaryKey|Sqlite|RRSR",
    "alterRole|Mysql|SSSS",
    "alterRole|Postgres|SSSS",
    "alterRole|Sqlite|SSSS",
    "alterSequence|Mysql|RSSS",
    "alterSequence|Postgres|RSSS",
    "alterSequence|Sqlite|RSSS",
    "attachPartition|Mysql|RRSR",
    "attachPartition|Postgres|RRSR",
    "attachPartition|Sqlite|RRSR",
    "backfill|Mysql|SSSS",
    "backfill|Postgres|SSSS",
    "backfill|Sqlite|SSSS",
    "comment|Mysql|RSSS",
    "comment|Postgres|RSSS",
    "comment|Sqlite|RSSS",
    "createDomain|Mysql|SSSS",
    "createDomain|Postgres|RSSS",
    "createDomain|Sqlite|SSSS",
    "createEnum|Mysql|SSSS",
    "createEnum|Postgres|RSSS",
    "createEnum|Sqlite|SSSS",
    "createExtension|Mysql|RSSS",
    "createExtension|Postgres|RSSS",
    "createExtension|Sqlite|RSSS",
    "createFunction|Mysql|RSSS",
    "createFunction|Postgres|RSSS",
    "createFunction|Sqlite|RSSS",
    "createIndex|Mysql|RRRR",
    "createIndex|Postgres|RSRR",
    "createIndex|Sqlite|RSRR",
    "createPartition|Mysql|RSSS",
    "createPartition|Postgres|RSSS",
    "createPartition|Sqlite|RSSS",
    "createPolicy|Mysql|RSSS",
    "createPolicy|Postgres|RSSS",
    "createPolicy|Sqlite|RSSS",
    "createRole|Mysql|RSSS",
    "createRole|Postgres|RSSS",
    "createRole|Sqlite|RSSS",
    "createSchema|Mysql|RSSS",
    "createSchema|Postgres|RSSS",
    "createSchema|Sqlite|RSSS",
    "createSequence|Mysql|RSSS",
    "createSequence|Postgres|RSSS",
    "createSequence|Sqlite|RSSS",
    "createTable|Mysql|RRRR",
    "createTable|Postgres|RRRR",
    "createTable|Sqlite|RRRR",
    "createTrigger|Mysql|RSSS",
    "createTrigger|Postgres|RSSS",
    "createTrigger|Sqlite|RSSS",
    "createView|Mysql|RSSS",
    "createView|Postgres|RSSS",
    "createView|Sqlite|RSSS",
    "delete|Mysql|SSSS",
    "delete|Postgres|SSSS",
    "delete|Sqlite|SSSS",
    "detachPartition|Mysql|RSSS",
    "detachPartition|Postgres|RSSS",
    "detachPartition|Sqlite|RSSS",
    "dialectal|Mysql|RRRR",
    "dialectal|Postgres|RRRR",
    "dialectal|Sqlite|RRRR",
    "dropColumn|Mysql|RRRS",
    "dropColumn|Postgres|RRRS",
    "dropColumn|Sqlite|RRRS",
    "dropColumnDefault|Mysql|RRRS",
    "dropColumnDefault|Postgres|RRRS",
    "dropColumnDefault|Sqlite|RRRS",
    "dropColumnNotNull|Mysql|RRRS",
    "dropColumnNotNull|Postgres|RRRS",
    "dropColumnNotNull|Sqlite|RRRS",
    "dropConstraint|Mysql|RSRS",
    "dropConstraint|Postgres|RRRR",
    "dropConstraint|Sqlite|RSRS",
    "dropDomain|Mysql|SSSS",
    "dropDomain|Postgres|RSSS",
    "dropDomain|Sqlite|SSSS",
    "dropEnum|Mysql|SSSS",
    "dropEnum|Postgres|RSSS",
    "dropEnum|Sqlite|SSSS",
    "dropExtension|Mysql|RSSS",
    "dropExtension|Postgres|RSSS",
    "dropExtension|Sqlite|RSSS",
    "dropFunction|Mysql|RSSS",
    "dropFunction|Postgres|RSSS",
    "dropFunction|Sqlite|RSSS",
    "dropIndex|Mysql|RRRR",
    "dropIndex|Postgres|RRRR",
    "dropIndex|Sqlite|RRRR",
    "dropOwnedBy|Mysql|SSSS",
    "dropOwnedBy|Postgres|SSSS",
    "dropOwnedBy|Sqlite|SSSS",
    "dropPartition|Mysql|RRSR",
    "dropPartition|Postgres|RRSR",
    "dropPartition|Sqlite|RRSR",
    "dropPolicy|Mysql|RSSS",
    "dropPolicy|Postgres|RSSS",
    "dropPolicy|Sqlite|RSSS",
    "dropRole|Mysql|RSSS",
    "dropRole|Postgres|RSSS",
    "dropRole|Sqlite|RSSS",
    "dropSchema|Mysql|RSSS",
    "dropSchema|Postgres|RSSS",
    "dropSchema|Sqlite|RSSS",
    "dropSequence|Mysql|RRSR",
    "dropSequence|Postgres|RRSR",
    "dropSequence|Sqlite|RRSR",
    "dropTable|Mysql|RRRR",
    "dropTable|Postgres|RRRR",
    "dropTable|Sqlite|RRRR",
    "dropTrigger|Mysql|RSSS",
    "dropTrigger|Postgres|RSSS",
    "dropTrigger|Sqlite|RSSS",
    "dropView|Mysql|RRSR",
    "dropView|Postgres|RRSR",
    "dropView|Sqlite|RRSR",
    "grant|Mysql|SSSS",
    "grant|Postgres|SSSS",
    "grant|Sqlite|SSSS",
    "insert|Mysql|SSSS",
    "insert|Postgres|SSSS",
    "insert|Sqlite|SSSS",
    "pgRaw|Mysql|SSSS",
    "pgRaw|Postgres|SSSS",
    "pgRaw|Sqlite|SSSS",
    "renameColumn|Mysql|RRRR",
    "renameColumn|Postgres|RRRR",
    "renameColumn|Sqlite|RRRR",
    "renameTable|Mysql|RRRR",
    "renameTable|Postgres|RRRR",
    "renameTable|Sqlite|RRRR",
    "revoke|Mysql|SSSS",
    "revoke|Postgres|SSSS",
    "revoke|Sqlite|SSSS",
    "setColumnDefault|Mysql|RRRS",
    "setColumnDefault|Postgres|RRRS",
    "setColumnDefault|Sqlite|RRRS",
    "setColumnNotNull|Mysql|RRRS",
    "setColumnNotNull|Postgres|RRRS",
    "setColumnNotNull|Sqlite|RRRS",
    "setColumnType|Mysql|RRRR",
    "setColumnType|Postgres|RRRR",
    "setColumnType|Sqlite|RRRR",
    "setRls|Mysql|RSSS",
    "setRls|Postgres|RSSS",
    "setRls|Sqlite|RSSS",
    "setTableOptions|Mysql|RSSR",
    "setTableOptions|Postgres|RSSR",
    "setTableOptions|Sqlite|RSSR",
    "synchronizeIdentity|Mysql|RRSR",
    "synchronizeIdentity|Postgres|RRSR",
    "synchronizeIdentity|Sqlite|RRSR",
    "update|Mysql|SSSS",
    "update|Postgres|SSSS",
    "update|Sqlite|SSSS",
    "validateConstraint|Mysql|SSSS",
    "validateConstraint|Postgres|SSSS",
    "validateConstraint|Sqlite|SSSS",
];

fn rows_map() -> BTreeMap<&'static str, &'static Row> {
    ROWS.iter().map(|r| (r.key, r)).collect()
}

fn diff_report(name: &str, expected: &[String], actual: &[String]) -> String {
    let e: std::collections::BTreeSet<&String> = expected.iter().collect();
    let a: std::collections::BTreeSet<&String> = actual.iter().collect();
    let mut out = String::new();
    for line in a.difference(&e) {
        out.push_str(&format!("  MEASURED, NOT RECORDED: {line}\n"));
    }
    for line in e.difference(&a) {
        out.push_str(&format!("  RECORDED, NOT MEASURED: {line}\n"));
    }
    if out.is_empty() {
        return String::new();
    }
    format!(
        "{name} moved:\n{out}\nthe full measurement is:\n{}\n",
        actual.join("\n")
    )
}

#[test]
fn the_corpus_constructs_every_op_variant() {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut record = |ops: &[Op]| {
        for op in ops {
            seen.insert(variant(op));
            if let Op::Dialectal {
                default,
                pg,
                sqlite,
                mysql,
            } = op
            {
                for leg in [default, pg, sqlite, mysql].into_iter().flatten() {
                    for inner in leg {
                        seen.insert(variant(inner));
                    }
                }
            }
        }
    };
    for stem in STEMS {
        record(&read_golden(stem).ops);
    }
    for s in STREAMS.iter().chain(CASES) {
        record(&parse(s.ops));
    }
    assert_eq!(
        seen.len(),
        OP_VARIANT_COUNT,
        "the corpus must construct every Op variant; it constructs {seen:?}"
    );
}

/// `Op`'s variant count, restated here so a variant added to the IR fails THIS
/// file rather than silently widening the swallowed set of four walkers.
const OP_VARIANT_COUNT: usize = 56;

#[test]
fn each_walker_reaches_exactly_the_recorded_op_variants() {
    let measured = reach_lines(&measured_reach());
    let expected: Vec<String> = REACH.iter().map(ToString::to_string).collect();
    let report = diff_report("the op-variant reach matrix", &expected, &measured);
    assert!(report.is_empty(), "{report}");
}

#[test]
fn every_case_produces_exactly_the_recorded_verdict() {
    let measured = case_lines();
    let map = rows_map();
    let expected: Vec<String> = ROWS
        .iter()
        .map(|r| format!("{}|{}", r.key, r.verdict))
        .collect();
    let report = diff_report("the differential corpus", &expected, &measured);
    assert!(report.is_empty(), "{report}");
    assert_eq!(map.len(), ROWS.len(), "the recorded keys are unique");
}

#[test]
fn a_recorded_divergence_never_reads_as_correct() {
    for row in ROWS {
        let (shape, status) = (row.verdict, row.status);
        match status {
            Status::Consistent => assert!(
                shape.starts_with("AGREED"),
                "{}: only an AGREED row may be Consistent",
                row.key
            ),
            Status::ByDesign(why) | Status::Defect(why) => {
                assert!(
                    shape.starts_with("AGREED") || shape.starts_with("DIVERGENT"),
                    "{}: {why}",
                    row.key
                );
                assert!(!why.trim().is_empty(), "{}: a reason is required", row.key);
            }
            Status::Uncorroborated(why) => {
                assert!(
                    shape.starts_with("SOLE") || shape.starts_with("NO-AUTHORITY"),
                    "{}: only an uncross-checked row may be Uncorroborated",
                    row.key
                );
                assert!(!why.trim().is_empty(), "{}: a reason is required", row.key);
            }
        }
        if let Status::Defect(evidence) = status {
            assert!(
                evidence.contains("review-log.md:") || evidence.contains("UNRECORDED"),
                "{}: a defect row names its review-log evidence, or says plainly \
                 that this corpus is the first record of it",
                row.key
            );
        }
    }
}

/// The corpus's own shape, pinned. A corpus that quietly loses its divergence
/// rows still passes every other test in this file, because every other test
/// compares the recorded set against itself. This one says how many of each
/// kind there are, so shrinking the evidence is a failure rather than a
/// smaller green run.
#[test]
fn the_corpus_has_the_shape_it_claims() {
    let count = |f: fn(&Row) -> bool| ROWS.iter().filter(|r| f(r)).count();
    let agreed = count(|r| r.verdict.starts_with("AGREED"));
    let divergent = count(|r| r.verdict.starts_with("DIVERGENT"));
    let sole = count(|r| r.verdict.starts_with("SOLE"));
    let defects = count(|r| matches!(r.status, Status::Defect(_)));

    assert_eq!(ROWS.len(), 114, "recorded rows");
    assert_eq!(agreed, 76, "AGREED rows");
    assert_eq!(divergent, 35, "DIVERGENT rows");
    assert_eq!(sole, 3, "SOLE rows, which cross-check nothing");
    assert_eq!(
        agreed + divergent + sole,
        ROWS.len(),
        "no other verdict shape"
    );
    assert_eq!(
        defects, 0,
        "rows recording a defect. Lowering this number means a walker was fixed \
         AND its row re-measured, or that evidence was deleted; the two read the \
         same in a diff, so the number is stated here. It went 8 -> 5 when the \
         MySQL retype stopped folding the pre-retype physical contract: three rows \
         on one defect, all re-measured to AGREED against a live MySQL oracle \
         (tests/fold_retype_physical_type_mysql.rs). It went 5 -> 0 with the RENAME \
         CARRIER SWEEP, which closed the corpus's last three defects at once rather \
         than one at a time: a CHECK ConstraintSnapshot::definition (the fifth \
         carrier this corpus itself found), IndexSnapshot::predicate and an \
         IndexElementSnapshot::Expr, five rows across two dialects, all re-measured \
         to AGREED with live oracles in tests/rename_carrier_sweep_pg.rs \
         (PostgreSQL 18.4) and tests/rename_carrier_sweep_sqlite.rs. \
         ZERO IS NOT A CLAIM THAT THE WALKERS ARE CORRECT. It says every \
         disagreement THIS corpus can see is now agreed or recorded as by-design, \
         and its sight is bounded by CASES - the sweep itself found a carrier no \
         case here probes (TableSnapshot::partition_by, which TableSnapshot's \
         PartialEq compares and which therefore reported real drift)"
    );
    assert_eq!(CASES.len(), 20, "differential cases");
    assert_eq!(
        REACH.len(),
        OP_VARIANT_COUNT * DIALECTS.len(),
        "reach cells"
    );
}

#[test]
fn every_case_declares_its_probes_and_every_probe_is_recorded() {
    for c in CASES {
        assert!(
            !probes_for(c.name).is_empty(),
            "{} declares no probe, so it asserts nothing",
            c.name
        );
    }
    let recorded: BTreeSet<&str> = ROWS
        .iter()
        .map(|r| r.key.split('|').next().expect("a key names its case"))
        .collect();
    for c in CASES {
        assert!(recorded.contains(c.name), "{} has no recorded row", c.name);
    }
}
