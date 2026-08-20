//! **What a `t.string({ maxLength: n })` column is worth after a descriptor round trip.**
//!
//! `ColType::String { length }` and `ColType::Text` are two different columns and
//! `render::lower::col_type_to_token` spells them with the SAME token, `"string"`. The
//! width does not ride in the token; it rides BESIDE it, as the descriptor's
//! `max_length` / the field def's `maxLength`, exactly the way `charLen` rides beside
//! `char`. Every OUTBOUND leg already threads it — `ir_column_to_field` derives it from
//! the `ColType`, `field_to_sdk_def` puts it on the def, and
//! `schema::query`'s PostgreSQL `column_type` reads it back out and answers
//! `character varying(n)`.
//!
//! The INVERSE did not. `render::fold::token_to_col_type` matched `"string" =>
//! ColType::Text`, flat, with no look at `max_length` — so the moment a descriptor was
//! turned back into ops the width was gone and could not come back.
//!
//! # Which carrier decides, measured rather than assumed
//!
//! The same question the decimal defect answered three different ways
//! (`sqlite_decimal_rebuild_live.rs`), asked again for the width. Here it has TWO
//! answers, and only one of them was wrong:
//!
//! * [`a_bounded_string_authored_as_ops_is_a_varchar_the_server_enforces`] is the ops
//!   route: `createTable` with `{"string":{"length":64}}` → `ir_column_to_field` →
//!   `maxLength: 64` → `character varying(64)`. THE FIELD-DEF CARRIER DECIDES AND IT
//!   WAS ALREADY RIGHT. Green before and after; it is the tripwire, and it is also what
//!   makes the next case a DISAGREEMENT between two carriers rather than a preference.
//! * [`a_bounded_string_through_the_descriptor_producer_reaches_the_server_unbounded`]
//!   is the producer route — the one `genArtifactsFromDescriptors` and
//!   `render_schema_export_from_descriptors` take, and the one a host takes when it
//!   feeds an exported `SchemaExport` back in. THE PRODUCER DECIDES, and it answered
//!   `text`.
//! * [`a_reimported_bounded_string_phantom_diffs_the_bound_off_a_live_column`] is the
//!   consequence one layer out: `MigrationEngine::plan_declarative`, handed the
//!   re-imported descriptors and a live `character varying(64)`, authors an
//!   `ALTER TABLE … TYPE text` against a schema nobody changed. That is not a cosmetic
//!   diff — applying it REMOVES a constraint from a live server.
//!
//! # The oracle is the server, twice over
//!
//! `information_schema.columns.character_maximum_length` is the DECLARATION, and a
//! declaration is only a claim. So every case also pushes a 200-character value at the
//! column and reports whether PostgreSQL took it. A `character varying(64)` raises
//! SQLSTATE 22001 (`string_data_right_truncation`); an unbounded `text` stores all 200
//! characters and says nothing. That INSERT is the whole difference between "a facet
//! went missing" and "a constraint the author asked for is not enforced", and nothing
//! in this file compares one function in this repo against another.
//!
//! # What this file does NOT prove
//!
//! * Nothing about SQLite. `schema::query`'s SQLite `column_type` answers `TEXT` for
//!   every `string` token, faceted or not, and SQLite enforces no VARCHAR bound in any
//!   case, so the width is lost identically on both carriers there and no live case can
//!   tell them apart. That is why this file is PostgreSQL-only.
//! * Nothing about MySQL. `mysql_bounded_string_producer_live.rs` is the companion that
//!   measures it, and the consequence there is the same shape with a 65535 ceiling on
//!   it plus a change of storage family. It is a separate file because the prediction
//!   about MySQL was wrong and the server said so.
//! * Not that the export WIRE carries the width. It does, and
//!   `zero-migrate-node/tests/collection_export_round_trip.rs` is where that is pinned.
//!   This file is about the leg after it.
//!
//! Gated on `ZERO_MIGRATE_TEST_PG_URL` and skips cleanly without it, so read the SKIP
//! banner before reading the pass count.

use crate::support;

use std::collections::HashMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::Op;
use zero_migrate::render::declarative::{CollectionDescriptor, DeclarativeAuthor, FieldDescriptor};
use zero_migrate::{
    descriptors_to_create_ops, desired_snapshot_for_dialect, render_schema_export_from_descriptors,
    resolve_create_table_policy, Approval, EffectivePolicy, ExecutorConfig, GuardConfig, IrAuthor,
    LiveSchema, LockMode, MigrationEngine, MigrationIr, PostgresBackend, SqlDialect,
    TableRuntimeOptions,
};

const OWNER: &str = "app_bounded_string";
const TABLE: &str = "profiles";
const COLUMN: &str = "first_name";
/// The declared bound. Small enough that a 200-character value is unambiguously over
/// it, and not 191/255 so it cannot be confused with a dialect default.
const BOUND: i64 = 64;

/// A value that is over [`BOUND`] and under nothing else. `character varying(64)`
/// refuses it; `text` takes it.
fn over_long() -> String {
    "x".repeat(200)
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "bounded_string_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// The `createTable` ops an AUTHOR writes for a bounded string column: the width is
/// inside the `ColType`, where it cannot be dropped.
fn authored_ops(schema: &str, policy: &EffectivePolicy) -> Vec<Op> {
    let source = format!(
        r#"{{"ir_version":1,"name":"bounded_string","ops":[
          {{"op":"createTable","name":"{TABLE}","columns":[
            {{"name":"id","type":"text","nullable":false}},
            {{"name":"{COLUMN}","type":{{"string":{{"length":{BOUND}}}}},"nullable":true}}
          ],"primaryKey":["id"]}}
        ]}}"#
    );
    let raw: MigrationIr = serde_json::from_str(&source).expect("the bounded-string IR parses");
    resolve_create_table_policy(&raw, policy, schema)
        .expect("the bounded-string IR resolves")
        .ops
}

/// The descriptor set a host declares — or gets back from `render_schema_export` and
/// feeds in again — carrying the width as the `maxLength` facet beside the token.
fn authored_descriptors() -> Vec<CollectionDescriptor> {
    vec![CollectionDescriptor {
        name: TABLE.to_string(),
        owner_app: OWNER.to_string(),
        fields: vec![
            FieldDescriptor {
                name: "id".to_string(),
                ty: "string".to_string(),
                required: true,
                ..FieldDescriptor::default()
            },
            FieldDescriptor {
                name: COLUMN.to_string(),
                ty: "string".to_string(),
                max_length: Some(BOUND),
                ..FieldDescriptor::default()
            },
        ],
        indexes: Vec::new(),
        runtime_options: TableRuntimeOptions::default(),
    }]
}

/// What the SERVER holds for [`COLUMN`], and what it did when asked to store a value
/// that is over the declared bound.
#[derive(Debug)]
struct Measured {
    /// `information_schema.columns.data_type`: `character varying` or `text`.
    data_type: String,
    /// `information_schema.columns.character_maximum_length`. `None` for an unbounded
    /// `text` column — which is the whole finding when a bound was declared.
    max_length: Option<i32>,
    /// `Ok(())` when PostgreSQL STORED a 200-character value in a column the author
    /// bounded at 64; `Err(sqlstate-or-message)` when it refused. The declaration above
    /// is a claim; this is the enforcement.
    over_long_insert: Result<(), String>,
}

/// Apply `ops` to a throwaway schema, measure the column, and drop the schema on every
/// exit path including an unwind.
async fn measure(
    label: &str,
    make_ops: impl FnOnce(&str, &EffectivePolicy) -> Vec<Op>,
) -> Measured {
    let Some(url) = support::pg_url() else {
        unreachable!("callers gate on `skip_if_no_pg!` before reaching here")
    };
    let session = PgDevSession::connect(&url);
    let schema = token();
    let policy = support::no_inject(&schema);
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta = quote_ident(&cfg.confinement.meta_schema);
    let _guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create the isolated bounded-string test schema");

    let work: Result<Measured, String> = async {
        let ops = make_ops(&cfg.project_schema, &policy);
        apply_ops(&session, &cfg, &policy, &ops).await?;
        measure_column(&session, &cfg.project_schema).await
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(measured), Ok(())) => measured,
        (Err(work), Ok(())) => panic!("{label}: {work}"),
        (Ok(_), Err(cleanup)) => panic!("{label}: drop the test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{label}: {work}; cleanup failed: {cleanup}"),
    }
}

/// Lower `ops` through the shipped author and apply them through the shipped engine.
async fn apply_ops(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    ops: &[Op],
) -> Result<(), String> {
    let backend = PostgresBackend::new_generic(session);
    backend
        .ensure_journal(cfg)
        .await
        .map_err(|error| format!("ensure the migration journal: {error}"))?;
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, policy);
    let ir = ir_from_ops(ops);
    let steps = author
        .lower_steps(&ir, &LiveSchema::default())
        .map_err(|error| format!("lower the bounded-string ops: {error}"))?;
    MigrationEngine::new()
        .apply_plan(
            &steps,
            Approval::Approved,
            &backend,
            cfg,
            "bounded-string-pg",
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply the bounded-string plan: {error}"))?;
    Ok(())
}

/// The collection descriptors the fold recovers from `ops` — the EXPORT half, which
/// carries the width as `max_length` and is not the leg under test.
fn descriptors_from_ops(
    ops: &[Op],
    schema: &str,
    policy: &EffectivePolicy,
) -> Result<Vec<CollectionDescriptor>, String> {
    let export = zero_migrate::render_schema_export(ops, SqlDialect::Postgres, schema, policy)
        .map_err(|error| format!("render the schema export: {error}"))?;
    Ok(export.collections.into_values().collect())
}

/// Wrap a resolved op list back into the `MigrationIr` the author lowers.
fn ir_from_ops(ops: &[Op]) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": "bounded_string",
        "ops": ops,
    }))
    .expect("a resolved op list re-parses as a MigrationIr")
}

/// Read the column's declaration out of `information_schema`, then ask the server to
/// store an over-long value in it.
async fn measure_column(session: &PgDevSession, schema: &str) -> Result<Measured, String> {
    let rows = session
        .query(
            "SELECT data_type::text AS data_type, \
                    character_maximum_length AS max_length \
             FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[schema.into(), TABLE.into(), COLUMN.into()],
        )
        .await
        .map_err(|error| format!("read the live column declaration: {error}"))?;
    let row = rows
        .first()
        .ok_or_else(|| format!("the server holds no `{TABLE}.{COLUMN}` column at all"))?;
    let data_type: String = row
        .try_get("data_type")
        .map_err(|error| format!("decode the live data type: {error}"))?;
    let max_length: Option<i32> = row
        .try_get("max_length")
        .map_err(|error| format!("decode the live character maximum length: {error}"))?;

    let over_long_insert = session
        .batch(&format!(
            "INSERT INTO {}.{} (id, {}) VALUES ('over-long', '{}')",
            quote_ident(schema),
            quote_ident(TABLE),
            quote_ident(COLUMN),
            over_long()
        ))
        .await
        .map_err(|error| error.to_string());

    Ok(Measured {
        data_type,
        max_length,
        over_long_insert,
    })
}

/// **The tripwire. The ops route was already right, so the next case is a
/// DISAGREEMENT.**
///
/// A `createTable` carrying `ColType::String { length: 64 }` reaches PostgreSQL as
/// `character varying(64)` and the server refuses a 200-character value. Green before
/// the producer fix and after it: it is here so that the producer's `text` answer is
/// measurably a second, contradictory opinion about the same column rather than the
/// only opinion there is.
#[compio::test]
async fn a_bounded_string_authored_as_ops_is_a_varchar_the_server_enforces() {
    let _url = skip_if_no_pg!();
    let measured = measure("a bounded string authored as ops", |schema, policy| {
        authored_ops(schema, policy)
    })
    .await;

    assert_eq!(
        (measured.data_type.as_str(), measured.max_length),
        ("character varying", Some(BOUND as i32)),
        "the ops carrier must reach the server as a bounded varchar, or nothing below \
         is a disagreement: {measured:?}"
    );
    assert!(
        measured.over_long_insert.is_err(),
        "PostgreSQL accepted a 200-character value into a column declared \
         `character varying({BOUND})`, so this file's enforcement oracle is broken \
         rather than the bound being real"
    );
}

/// **The defect, adjudicated by the server: the producer unbounds the column.**
///
/// The SAME width, declared as a descriptor facet instead of inside a `ColType`, and
/// run through the shipped `descriptors_to_create_ops` — the producer behind
/// `render_schema_export_from_descriptors` and the addon's
/// `genArtifactsFromDescriptors`. `token_to_col_type` mapped the `"string"` token to
/// `ColType::Text` without consulting `max_length`, so the ops it produced declared an
/// unbounded column and PostgreSQL stored 200 characters in a field the author bounded
/// at 64.
///
/// The insert is the point. A missing `character_maximum_length` is a lost facet; a
/// stored over-long row is a lost CONSTRAINT.
#[compio::test]
async fn a_bounded_string_through_the_descriptor_producer_reaches_the_server_unbounded() {
    let _url = skip_if_no_pg!();
    let measured = measure(
        "a bounded string through the descriptor producer",
        |schema, policy| {
            descriptors_to_create_ops(&authored_descriptors(), schema, policy)
                .expect("the descriptor set produces createTable ops")
        },
    )
    .await;

    assert_eq!(
        (measured.data_type.as_str(), measured.max_length),
        ("character varying", Some(BOUND as i32)),
        "the descriptor producer dropped the declared width: the server holds \
         {measured:?}, where the ops carrier for the same column holds \
         `character varying({BOUND})`"
    );
    assert!(
        measured.over_long_insert.is_err(),
        "PostgreSQL STORED a 200-character value in a column the author bounded at \
         {BOUND}: the producer did not merely lose a facet, it removed a constraint"
    );
}

/// **The consequence one layer out: a deploy that changes nothing strips the bound off
/// a live column.**
///
/// The round trip a host actually performs. `render_schema_export` hands back the
/// folded collections; the host feeds them back as its `DesiredSchema`.
/// `token_to_col_type` narrowed them on the way through, so the desired column is now
/// `text` while the live one is `character varying(64)` — and
/// `MigrationEngine::plan_declarative` authors the `ALTER` that reconciles them by
/// DROPPING the bound.
///
/// This is the string analogue of the decimal defect's phantom rebuild, and it is
/// worse in one respect: the decimal's phantom diff was a SQLite rebuild of an
/// unchanged table, while this one is a live PostgreSQL `ALTER TABLE` that silently
/// widens a constraint the author is still declaring.
#[compio::test]
async fn a_reimported_bounded_string_phantom_diffs_the_bound_off_a_live_column() {
    let _url = skip_if_no_pg!();
    let Some(url) = support::pg_url() else {
        unreachable!("gated above")
    };
    let session = PgDevSession::connect(&url);
    let schema = token();
    let policy = support::no_inject(&schema);
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta = quote_ident(&cfg.confinement.meta_schema);
    let _guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create the isolated re-import test schema");

    let work: Result<(Measured, Vec<String>), String> = async {
        // The live table, created from the ops carrier, which is right.
        let ops = authored_ops(&cfg.project_schema, &policy);
        apply_ops(&session, &cfg, &policy, &ops).await?;
        let measured = measure_column(&session, &cfg.project_schema).await?;

        // The round trip a host performs: the SAME ops, exported as descriptors and fed
        // straight back through the producer. Nothing about the schema changes across
        // it - only the carrier it is spelled through.
        let exported: Vec<CollectionDescriptor> =
            descriptors_from_ops(&ops, &cfg.project_schema, &policy)?;
        let reexport = render_schema_export_from_descriptors(
            &exported,
            SqlDialect::Postgres,
            &cfg.project_schema,
            &policy,
        )
        .map_err(|error| format!("re-fold the re-imported descriptors: {error}"))?;
        let desired_descriptors: Vec<CollectionDescriptor> = reexport
            .collections
            .into_values()
            // The projection stamps a synthetic fold owner; a re-deploy presents the
            // descriptors under the app that owns them, or the differ's cross-app guard
            // refuses a structural change to a table it does not attribute.
            .map(|descriptor| CollectionDescriptor {
                owner_app: OWNER.to_string(),
                ..descriptor
            })
            .collect();
        let desired = desired_snapshot_for_dialect(
            &cfg.project_schema,
            &desired_descriptors,
            SqlDialect::Postgres,
            &policy,
        )
        .map_err(|error| format!("resolve the re-imported descriptors: {error}"))?;

        // Diff the re-imported desired schema against the live server.
        let backend = PostgresBackend::new_generic(&session);
        let live = backend
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("introspect the live schema: {error}"))?;
        let ownership: HashMap<String, String> = live
            .tables
            .keys()
            .map(|table| (table.clone(), OWNER.to_string()))
            .collect();
        let plan = MigrationEngine::new()
            .plan_declarative(
                &desired,
                &live,
                &ownership,
                &DeclarativeAuthor::new_for_dialect(
                    &cfg.project_schema,
                    OWNER,
                    SqlDialect::Postgres,
                ),
                &[],
                &GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres),
                &policy,
            )
            .map_err(|error| format!("plan the re-imported schema declaratively: {error}"))?;
        let statements: Vec<String> = plan
            .plain
            .items
            .iter()
            .map(|item| item.migration.up.clone())
            .collect();
        Ok((measured, statements))
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta} CASCADE"
        ))
        .await;
    let (measured, statements) = match (work, cleanup) {
        (Ok(pair), Ok(())) => pair,
        (Err(work), Ok(())) => panic!("re-imported bounded string: {work}"),
        (Ok(_), Err(cleanup)) => panic!("re-imported bounded string: drop schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => {
            panic!("re-imported bounded string: {work}; cleanup failed: {cleanup}")
        }
    };

    // THE ORACLE, first: the live column really is bounded, so a plan that alters it is
    // altering something rather than reporting a fixture that never got set up.
    assert_eq!(
        (measured.data_type.as_str(), measured.max_length),
        ("character varying", Some(BOUND as i32)),
        "the live column must be a bounded varchar before the re-import is diffed \
         against it: {measured:?}"
    );

    assert!(
        statements.is_empty(),
        "re-importing the exported schema authored a migration against a schema \
         nobody changed, and it strips the declared bound off a live column:\n  {}",
        statements.join("\n  ")
    );
}
