//! **The ROUND-TRIP ORACLE for `fold_ops` on real MySQL.**
//!
//! The MySQL leg of the fold oracle, and the FIRST in-crate Rust test of any kind to
//! drive a live MySQL server. Until it existed, MySQL was covered at RENDER level
//! (unit tests over the emitted SQL text) and at CLI/HOST level (TypeScript over
//! `mysql2`), with nothing in between: `ZERO_MIGRATE_MYSQL_URL` appeared in 52 files
//! under `packages/` and in exactly one under `crates/`, and that one was source.
//!
//! Same shape as its two siblings. APPLY the corpus through the REAL pipeline
//! (`load_and_lower_guarded` + `MigrationEngine::apply_plan` over
//! `MysqlBackend`), INTROSPECT with the shipped `snapshot_schema`, FOLD the SAME
//! resolved ops offline under `SqlDialect::Mysql`, and require `diff_snapshots(...)`
//! to be clean. The comparison runs after EVERY stage rather than once at the end,
//! for the reason `fold_roundtrip_sqlite.rs` states: a create and a drop of the same
//! object cancel in the folded snapshot, so a single trailing comparison would
//! observe neither half of the pair.
//!
//! A clean drift result is strictly narrower than saying the snapshots agree -
//! `IndexSnapshot` equality excludes `opclass`, `nulls_not_distinct` and `only` - so
//! this file does not claim `fold_ops == snapshot_schema` in full, the same caveat
//! `fold_roundtrip_pg.rs` carries.
//!
//! MySQL has no schemas inside a database: a "schema" IS a database. So where the PG
//! oracle creates and drops a schema, this one creates and drops a DATABASE, and the
//! engine creates the `<db>_migrations` meta database itself on the first
//! `ensure_journal`. [`support::mysql::DatabaseGuard`] guards both.
//!
//! Gated on `ZERO_MIGRATE_MYSQL_URL` through [`skip_if_no_mysql!`], which routes into
//! the same `announce_live_db_skip` the PostgreSQL suites use: a skip prints a banner
//! that survives libtest's output capture, and `ZERO_MIGRATE_REQUIRE_LIVE_DB=1` turns
//! it into a failure. A skip must never read as a pass.

use crate::support;

use std::collections::BTreeMap;

use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use zero_migrate::apply::backend::{MigrationBackend, MysqlBackend};
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    diff_snapshots, fold_ops, model::ir::Op, resolve_create_table_policy, Approval, ExecutorConfig,
    GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr, SqlDialect,
};

const OWNER: &str = "app_fold_roundtrip_mysql";

fn cfg_for(database: &str) -> ExecutorConfig {
    ExecutorConfig::new(
        format!("project_{database}"),
        database,
        support::no_inject(database),
    )
}

/// Apply one IR doc through the REAL MySQL pipeline, returning the RESOLVED ops so
/// the caller accumulates the exact stream the fold replays.
///
/// `resolve_create_table_policy` first, then `load_and_lower_guarded`, then
/// `apply_plan` - the same three steps the napi bridge performs for a live MySQL
/// deploy, in the same order.
async fn apply_doc(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    source: &str,
    registry: &BTreeMap<String, String>,
    live: &LiveSchema,
    approval: Approval,
) -> Result<Vec<Op>, String> {
    let policy = support::no_inject(&cfg.project_schema);
    let authored: MigrationIr =
        serde_json::from_str(source).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    let resolved_source = serde_json::to_string(&resolved)
        .map_err(|error| format!("serialize resolved test IR: {error}"))?;
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Mysql, &policy);
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Mysql);
    let artifact = author
        .load_and_lower_guarded(&resolved_source, OWNER, registry, live, &guard)
        .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;

    let backend = MysqlBackend::new_generic(session);
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            approval,
            &backend,
            cfg,
            "fold-roundtrip-mysql",
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply IR plan: {error}"))?;

    Ok(resolved.ops)
}

/// Fold everything applied so far and compare it against live introspection.
async fn assert_matches_live(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    ops: &[Op],
    stage: &str,
) -> Result<(), String> {
    let expected = fold_ops(
        ops,
        SqlDialect::Mysql,
        &cfg.project_schema,
        &support::no_inject(&cfg.project_schema),
    )
    .map_err(|error| format!("{stage}: fold the corpus offline: {error}"))?;
    let actual = MysqlBackend::new_generic(session)
        .snapshot_schema(cfg)
        .await
        .map_err(|error| format!("{stage}: snapshot the live MySQL schema: {error}"))?;
    let drift = diff_snapshots(&expected, &actual);
    if drift.is_clean() {
        Ok(())
    } else {
        Err(format!(
            "{stage}: fold_ops(corpus) must equal the live introspected MySQL \
             snapshot, but they drifted: {drift:#?}"
        ))
    }
}

fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(table, owner)| ((*table).to_string(), (*owner).to_string()))
        .collect()
}

#[compio::test]
async fn fold_equals_introspect_mysql() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("fold");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated fold round-trip database");

    let result: Result<(), String> = async {
        let mut all_ops: Vec<Op> = Vec::new();
        let mut live = LiveSchema::default();
        let mut tables: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        // (1) createTable with plain columns of varied types plus an extra index.
        // The type spread is the point: MySQL's `information_schema.COLUMNS` reports
        // `COLUMN_TYPE` (the modifier-bearing spelling, `int`/`tinyint(1)`/
        // `varchar(255)`), while the fold builds its snapshot from the IR. A fold
        // that modelled any of these differently from what the emitter wrote shows
        // up here and nowhere else in the Rust suite.
        //
        // `body` is a bare `text` and is never keyed. That is not an oversight: a
        // key over a bare TEXT column is MySQL error 1170, so `body` proves the TEXT
        // storage shape round-trips while every keyed column below is a bounded
        // string. See `mysql_text_column_key_gate.rs` for what happens when a bare
        // TEXT column IS keyed, and for the cross-envelope hole this corpus found.
        let notes = r#"{"ir_version":1,"name":"create_notes","ops":[
            {"op":"createTable","name":"notes","columns":[
                {"name":"id","type":"int","nullable":false},
                {"name":"title","type":{"string":{"length":200}},"nullable":false},
                {"name":"body","type":"text","nullable":true},
                {"name":"rank","type":"int","nullable":true},
                {"name":"score","type":"double","nullable":true},
                {"name":"done","type":"boolean","nullable":false}
            ],
            "primaryKey":["id"],
            "indexes":[
                {"name":"notes_rank_idx","columns":[{"kind":"column","name":"rank"}]}
            ]}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                notes,
                &registry(&[]),
                &live,
                Approval::Approved,
            )
            .await?,
        );
        tables.insert("notes".to_string());
        live = LiveSchema::from_tables(tables.clone());
        assert_matches_live(&session, &cfg, &all_ops, "create table").await?;

        let notes_registry = registry(&[("notes", OWNER)]);

        // (2) addColumn.
        let add_column = r#"{"ir_version":1,"name":"add_col","ops":[
            {"op":"addColumn","table":"notes","column":"tag",
             "type":{"string":{"length":120}},"nullable":true}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                add_column,
                &notes_registry,
                &live,
                Approval::Approved,
            )
            .await?,
        );
        assert_matches_live(&session, &cfg, &all_ops, "add column").await?;

        // (3) createIndex, then dropIndex it. Each is compared before the next runs,
        // so neither can be cancelled by its counterpart before anything observes it.
        let make_index = r#"{"ir_version":1,"name":"mk_idx","ops":[
            {"op":"createIndex","table":"notes","name":"notes_tag_idx",
             "columns":[{"kind":"column","name":"tag"}]}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                make_index,
                &notes_registry,
                &live,
                Approval::Approved,
            )
            .await?,
        );
        assert_matches_live(&session, &cfg, &all_ops, "create index").await?;

        let drop_index = r#"{"ir_version":1,"name":"drop_idx","ops":[
            {"op":"dropIndex","name":"notes_tag_idx","table":"notes"}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                drop_index,
                &notes_registry,
                &live,
                Approval::Approved,
            )
            .await?,
        );
        assert_matches_live(&session, &cfg, &all_ops, "drop index").await?;

        // (4) A UNIQUE index, which MySQL materializes as a KEY the catalog reports
        // through `information_schema.STATISTICS.NON_UNIQUE` rather than as a
        // separate constraint object the way PostgreSQL does.
        let unique_index = r#"{"ir_version":1,"name":"unique_idx","ops":[
            {"op":"createIndex","table":"notes","name":"notes_title_key",
             "columns":[{"kind":"column","name":"title"}],"unique":true}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                unique_index,
                &notes_registry,
                &live,
                Approval::Approved,
            )
            .await?,
        );
        assert_matches_live(&session, &cfg, &all_ops, "create unique index").await?;

        // (5) A DESC index element. MySQL 8 stores a descending key for real (unlike
        // MySQL 5.7, which parsed and ignored the token), so `STATISTICS.COLLATION`
        // reads back `D` and a fold that dropped the order would drift here.
        let desc_index = r#"{"ir_version":1,"name":"desc_idx","ops":[
            {"op":"createIndex","table":"notes","name":"notes_rank_desc_idx",
             "columns":[{"kind":"column","name":"rank","order":"desc"}]}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                desc_index,
                &notes_registry,
                &live,
                Approval::Approved,
            )
            .await?,
        );
        assert_matches_live(&session, &cfg, &all_ops, "create descending index").await?;

        // (6) dropColumn.
        let drop_column = r#"{"ir_version":1,"name":"drop_col","ops":[
            {"op":"dropColumn","table":"notes","column":"score"}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                drop_column,
                &notes_registry,
                &live,
                Approval::Approved,
            )
            .await?,
        );
        assert_matches_live(&session, &cfg, &all_ops, "drop column").await?;

        // (7) A second table with a case-insensitive column and a foreign key back to
        // the first. MySQL carries case-insensitivity as a per-column COLLATION
        // (`utf8mb4_0900_ai_ci`), which introspection reads out of
        // `COLUMNS.COLLATION_NAME` and normalizes into the portable `caseSensitive`
        // intent, so this stage compares a facet that only round-trips if the emitter
        // and the recovery agree on one spelling.
        //
        // `email` is a bare `text` rather than a bounded string because the
        // `caseSensitive` facet is refused on anything else: "column \"email\"
        // declares caseSensitive:false but is not a text column". It is never keyed,
        // so MySQL error 1170 does not apply to it.
        let tags = r#"{"ir_version":1,"name":"create_tags","ops":[
            {"op":"createTable","name":"tags","columns":[
                {"name":"id","type":"int","nullable":false},
                {"name":"email","type":"text","nullable":false,"caseSensitive":false},
                {"name":"note_id","type":"int","nullable":true}
            ],
            "primaryKey":["id"],
            "constraints":[
                {"name":"tags_note_id_fkey","kind":{"kind":"fk",
                 "columns":["note_id"],
                 "referencesTable":"notes","referencesColumns":["id"],
                 "onDelete":"cascade","onUpdate":"noAction"}}
            ]}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                tags,
                &registry(&[]),
                &live,
                Approval::Approved,
            )
            .await?,
        );
        tables.insert("tags".to_string());
        live = LiveSchema::from_tables(tables.clone());
        assert_matches_live(
            &session,
            &cfg,
            &all_ops,
            "create table with a case-insensitive column and a foreign key",
        )
        .await?;

        let both = registry(&[("notes", OWNER), ("tags", OWNER)]);

        // (8) renameTable.
        let rename = r#"{"ir_version":1,"name":"rename_tbl","ops":[
            {"op":"renameTable","table":"tags","to":"labels"}
        ]}"#;
        all_ops.extend(apply_doc(&session, &cfg, rename, &both, &live, Approval::Approved).await?);
        tables.remove("tags");
        tables.insert("labels".to_string());
        live = LiveSchema::from_tables(tables.clone());
        assert_matches_live(&session, &cfg, &all_ops, "rename table").await?;

        let renamed = registry(&[("notes", OWNER), ("labels", OWNER)]);

        // (9) NAMED TYPES, which is the one stage here whose answer is not in the op
        // that asks for it. `createEnum` and `createDomain` state the members and the
        // base type; the `createTable` TWO OPS LATER is what needs them. So this stage
        // measures the fold's CROSS-OP state rather than any single arm.
        //
        // MySQL is the dialect where that state is checked STRUCTURALLY: it inlines a
        // domain as its BASE type, so the registry's answer lands in
        // `ColumnSnapshot::data_type`, which `diff_snapshots` compares against
        // `information_schema.COLUMNS.COLUMN_TYPE`. `bigInt` rather than `int` on
        // purpose - a registry that answered with the wrong base type has to be
        // DISTINGUISHABLE from one that answered with the column's own declared type.
        // On PostgreSQL the type is MATERIALIZED and `domain_schema_or` /
        // `enum_schema_or` fall back to the project schema, so a fold that lost the
        // registry answers IDENTICALLY there. Measured: resetting the registry per op
        // fails this stage and the SQLite one and leaves
        // `fold_roundtrip_pg::named_type_lifecycle` green.
        //
        // NO ENUM COLUMN HERE, and the reason is a defect this stage found rather than
        // a gap in it. MySQL inlines an enum as the column type `enum('free','pro')`,
        // which would make this the only oracle that compares the MEMBERSHIP in a
        // compared field. But `mysql_type_takes_collation`
        // (`render/declarative.rs`) lists VARCHAR/CHAR/TEXT and NOT ENUM, while MySQL
        // treats ENUM as a character type. So every other character column is emitted
        // with an explicit `COLLATE utf8mb4_0900_as_cs` and an enum column is emitted
        // with none, inherits the `utf8mb4_0900_ai_ci` default, and introspects to
        // `case_sensitive: false` against a fold that says nothing:
        //
        //     altered_objects: [ accounts / column tier / case_sensitive
        //                        expected "" actual "false" ]
        //
        // That is permanent phantom drift on every MySQL enum column, and the drift is
        // the SMALLER half - the column is genuinely case-INSENSITIVE on MySQL and
        // case-sensitive on the other two backends, so `'FREE'` and `'free'` are one
        // value there and two values everywhere else. Neither half is caused by the
        // fold, and fixing it is a renderer change with its own live gate; recorded
        // here so the next reader does not rediscover it by writing this same fixture.
        let named_types = r#"{"ir_version":1,"name":"named_types","ops":[
            {"op":"createEnum","name":"plan_tier","values":["free","pro"]},
            {"op":"createDomain","name":"seat_quota","as":"bigInt"},
            {"op":"createTable","name":"accounts","columns":[
                {"name":"id","type":"int","nullable":false},
                {"name":"seats","type":{"domain":{"name":"seat_quota"}},"nullable":true}
            ],
            "primaryKey":["id"]}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                named_types,
                &renamed,
                &live,
                Approval::Approved,
            )
            .await?,
        );
        tables.insert("accounts".to_string());
        live = LiveSchema::from_tables(tables.clone());
        assert_matches_live(&session, &cfg, &all_ops, "create table using named types").await?;

        let with_accounts = registry(&[("notes", OWNER), ("labels", OWNER), ("accounts", OWNER)]);

        // (10) dropTable last: the fold must forget the table AND everything hanging
        // off it - its columns, its indexes, its constraints - and a fold that
        // dropped the table while keeping a child object would drift only here.
        let drop_table = r#"{"ir_version":1,"name":"drop_tbl","ops":[
            {"op":"dropTable","table":"labels"}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                drop_table,
                &with_accounts,
                &live,
                Approval::Approved,
            )
            .await?,
        );
        tables.remove("labels");
        live = LiveSchema::from_tables(tables.clone());
        assert_matches_live(&session, &cfg, &all_ops, "drop table").await?;

        let _ = live;
        Ok(())
    }
    .await;

    result.unwrap_or_else(|error| panic!("{error}"));
}
