//! Live PostgreSQL oracle for what a `setColumnType` does to a column's OTHER
//! facets, and the false-drift control for the ones drift compares.
//!
//! The offline verdict lives on `render::lower::retype_field_descriptor` and in
//! `fold_ops`'s `Op::SetColumnType` arm. This file is where the two claims that
//! decided it are held to the SERVER rather than to a doc comment:
//!
//!   1. a retype off a VALUE-FORMAT column is refused by the fold because
//!      PostgreSQL refuses the ALTER — measured here on the exact SQL the lower
//!      emits, so a future PostgreSQL that stops refusing makes this test fail
//!      rather than leaving a stale justification in a comment; and
//!   2. the facets the fold now CLEARS (`case_sensitive`, `collation`,
//!      `inline_checks`) are the ones the server does not carry across the ALTER,
//!      so clearing them is what makes the retyped column drift-clean.
//!
//! Before the fix, (2) failed loudly. Measured on PostgreSQL 18.4 through the real
//! path (author → lower → apply → introspect → diff), a `citext → varchar(40)`
//! retype reported `case_sensitive expected "false" actual ""` on a schema that was
//! exactly what had been deployed, and a `typeId(usr) text → varchar(50)` retype
//! reported `collation`, `format` AND an unexpected `<table>_v_check` constraint.
//!
//! WHAT IS NOT HERE, and why. The `case_sensitive` leg is the one that produced the
//! sharpest measurement above, and it is NOT retained as a live test, because
//! `render::declarative` hardcodes `public.citext` as the PostgreSQL spelling of a
//! case-insensitive column and an extension name is unique PER DATABASE, not per
//! schema. `drop_extension_rollback_pg` already owns `citext` in this database and
//! says so in its own header ("the two tests here cannot share one:
//! `pg_extension_name_index` rejects the second creator"). An earlier draft of this
//! file created the extension `IF NOT EXISTS` and left it installed, and
//! `rolling_back_a_dropped_extension_restores_it` failed with
//! `extension "citext" already exists` - correctly. Creating and dropping it here
//! instead would only narrow the window, since cargo runs the two binaries in
//! parallel.
//!
//! So the `case_sensitive` verdict is pinned OFFLINE, in `set_column_type_facets`,
//! against all three replays; its live measurement is recorded in the review log
//! with the exact drift output rather than re-run on every suite. The legs that
//! remain here need no extension.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    diff_snapshots, fold_ops, resolve_create_table_policy, snapshot_schema, Approval,
    EffectivePolicy, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine,
    MigrationIr, PostgresBackend, SqlDialect, StructuralDrift,
};

/// The test-side PostgreSQL identifier spelling, written out here rather than
/// imported from the crate. It used to be
/// `zero_migrate::schema::query::quote_ident`, which was `pub`, un-dialected, and a
/// SECOND physical home for the spelling `render::backends::ansi_double_quote_ident`
/// owns; it is gone. A probe that builds its expectation by calling the emitter it is
/// checking is not an oracle anyway, so the replacement is deliberately independent —
/// the same shape the sibling `fold_rename_column_constraint_definition_pg` and
/// `fold_rename_column_check_body_pg` probes already use.
fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

const OWNER: &str = "app_set_column_type_facets";
const TABLE: &str = "retyped";

fn token(tag: &str) -> String {
    format!(
        "zm_retype_{tag}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    )
}

/// The operator charter these legs need: cross-schema, table creation in the
/// per-test schema, and the destructive-op allowance a type change lowers under.
fn charter(schema: &str) -> EffectivePolicy {
    let toml = format!(
        r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#
    );
    zero_migrate::effective_policy_from_charter_toml(&toml).expect("charter parses")
}

/// One envelope: `createTable` then `setColumnType`, so the ownership-registry
/// entry the create installs is in scope for the alter.
fn envelope(column: &str, to_type: Option<&str>) -> String {
    let retype = to_type.map_or_else(String::new, |to| {
        format!(r#",{{"op":"setColumnType","table":"{TABLE}","column":"v","toType":{to}}}"#)
    });
    format!(
        r#"{{"ir_version":1,"name":"m","owner_app":"{OWNER}","ops":[
            {{"op":"createTable","name":"{TABLE}","columns":[
                {{"name":"c0","type":"int","nullable":false}},
                {column}
            ],"primaryKey":["c0"]}}{retype}
        ]}}"#
    )
}

struct Applied {
    drift: StructuralDrift,
}

/// Author → lower → apply `source`, then run `native_sql`, then compare the fold of
/// the SAME ops against the live catalog.
async fn deploy(tag: &str, source: &str, native_sql: &[&str]) -> Option<Applied> {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return None;
    };
    let session = PgDevSession::connect(&url);
    let schema = token(tag);
    let policy = charter(&schema);
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.confinement.meta_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create the isolated retype schema");

    let work: Result<Applied, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;
        let policy = charter(&cfg.project_schema);

        let authored: MigrationIr =
            serde_json::from_str(source).map_err(|error| format!("parse test IR: {error}"))?;
        let resolved = resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
            .map_err(|error| format!("resolve create-table policy: {error}"))?;
        let resolved_source = serde_json::to_string(&resolved)
            .map_err(|error| format!("serialize resolved test IR: {error}"))?;
        let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
        let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
        let artifact = author
            .load_and_lower_guarded(
                &resolved_source,
                OWNER,
                &BTreeMap::new(),
                &LiveSchema::default(),
                &guard,
            )
            .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;
        MigrationEngine::new()
            .apply_plan(
                &artifact.plan.steps,
                Approval::Approved,
                &backend,
                &cfg,
                "set-column-type-facets-pg",
                LockMode::Acquire,
            )
            .await
            .map_err(|error| format!("apply IR plan: {error}"))?;

        for statement in native_sql {
            let statement = statement.replace("{schema}", &quoted_schema);
            session
                .batch(&statement)
                .await
                .map_err(|error| format!("NATIVE `{statement}`: {error}"))?;
        }

        let expected = fold_ops(
            &resolved.ops,
            SqlDialect::Postgres,
            &cfg.project_schema,
            &policy,
        )
        .map_err(|error| format!("fold the applied PostgreSQL ops: {error}"))?;
        let actual = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot the live PostgreSQL schema: {error}"))?;
        Ok(Applied {
            drift: diff_snapshots(&expected, &actual),
        })
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(applied), Ok(())) => Some(applied),
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(_), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}

fn assert_clean(label: &str, drift: &StructuralDrift) {
    assert!(
        drift.altered_objects.is_empty()
            && drift.missing_objects.is_empty()
            && drift.unexpected_objects.is_empty(),
        "{label}: applying a migration and changing NOTHING must leave the fold and \
         the catalog agreeing. altered={:?} missing={:?} unexpected={:?}",
        drift.altered_objects,
        drift.missing_objects,
        drift.unexpected_objects,
    );
}

const STRING_24: &str = r#"{"name":"v","type":{"string":{"length":24}}}"#;

// ---------------------------------------------------------------------------
// FALSE-DRIFT CONTROL. Apply, change nothing, assert clean.
// ---------------------------------------------------------------------------

#[compio::test]
async fn a_plain_retype_reports_no_difference_that_does_not_exist() {
    for (label, to_type) in [
        ("varchar(24) -> varchar(40)", r#"{"string":{"length":40}}"#),
        ("varchar(24) -> integer", r#""int""#),
        ("varchar(24) -> text", r#""text""#),
    ] {
        let Some(applied) = deploy("plain", &envelope(STRING_24, Some(to_type)), &[]).await else {
            return;
        };
        assert_clean(label, &applied.drift);
    }
}

#[compio::test]
async fn a_column_that_is_never_retyped_stays_clean() {
    // The control's control. Without it, a "fix" that stopped modelling the
    // facets AT ALL would pass the test above just as well.
    let Some(applied) = deploy("untouched", &envelope(STRING_24, None), &[]).await else {
        return;
    };
    assert_clean("varchar(24), never retyped", &applied.drift);
}

// ---------------------------------------------------------------------------
// THE SERVER ORACLE for the refusal.
// ---------------------------------------------------------------------------

/// The value-format retype the fold refuses, run against the server as the exact
/// SQL `render_alter_column_type` would emit for it.
///
/// The fold refuses this op, so the engine never issues this statement - which is
/// precisely why the justification has to be measured HERE. `native_sql` runs after
/// a successful deploy of the typed-id column, so the CHECK it hits is the engine's
/// own, not a hand-written imitation.
async fn server_verdict(tag: &str, rendered_type: &str) -> Option<String> {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return None;
    };
    let session = PgDevSession::connect(&url);
    let schema = token(tag);
    let policy = charter(&schema);
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.confinement.meta_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create the isolated retype schema");

    let work: Result<String, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;
        let policy = charter(&cfg.project_schema);
        let source = envelope(
            r#"{"name":"v","type":"text","valueFormat":{"typeId":{"prefix":"usr"}}}"#,
            None,
        );
        let authored: MigrationIr =
            serde_json::from_str(&source).map_err(|error| format!("parse test IR: {error}"))?;
        let resolved = resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
            .map_err(|error| format!("resolve create-table policy: {error}"))?;
        let resolved_source = serde_json::to_string(&resolved)
            .map_err(|error| format!("serialize resolved test IR: {error}"))?;
        let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
        let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
        let artifact = author
            .load_and_lower_guarded(
                &resolved_source,
                OWNER,
                &BTreeMap::new(),
                &LiveSchema::default(),
                &guard,
            )
            .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;
        MigrationEngine::new()
            .apply_plan(
                &artifact.plan.steps,
                Approval::Approved,
                &backend,
                &cfg,
                "set-column-type-facets-pg-oracle",
                LockMode::Acquire,
            )
            .await
            .map_err(|error| format!("apply IR plan: {error}"))?;

        // Byte-for-byte the shape of `DeclarativeRenderer::render_alter_column_type`.
        let alter = format!(
            "ALTER TABLE {quoted_schema}.{} ALTER COLUMN {} TYPE {rendered_type} USING {}::{rendered_type}",
            quote_ident(TABLE),
            quote_ident("v"),
            quote_ident("v"),
        );
        Ok(match session.batch(&alter).await {
            Ok(()) => String::new(),
            Err(error) => format!("{error}"),
        })
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(verdict), Ok(())) => Some(verdict),
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(_), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}

#[compio::test]
async fn postgresql_refuses_a_value_format_retype_to_a_non_text_type() {
    let Some(verdict) = server_verdict("vf_int", "integer").await else {
        return;
    };
    assert!(
        verdict.contains("octet_length(integer) does not exist"),
        "the reason the fold refuses this op is that the SERVER refuses it: the \
         engine-emitted format CHECK is re-parsed against the new type and no \
         longer type-checks. If PostgreSQL ever stops refusing, this assertion is \
         where the refusal's justification has to be revisited. Got: {verdict:?}"
    );
}

#[compio::test]
async fn postgresql_accepts_a_value_format_retype_to_a_text_type_and_keeps_an_unrecognisable_check()
{
    let Some(verdict) = server_verdict("vf_vc", "character varying(50)").await else {
        return;
    };
    // The half the previous investigation guessed at and this measures: the ALTER
    // SUCCEEDS and the CHECK survives. It is NOT a licence to keep `value_format` -
    // PostgreSQL re-parses the body with casts injected
    // (`octet_length((v)::text)`), which `recover_format_check` does not recognise,
    // so the live side loses the facet and reports the CHECK as an unexpected
    // constraint. Both spellings of the fold - keep it, clear it - false-drift, so
    // the op is refused instead. The refusal test above covers the non-text half;
    // this one records that the text half is refused for a DIFFERENT reason and
    // must not be quietly re-enabled on the strength of the server accepting it.
    assert_eq!(
        verdict, "",
        "PostgreSQL accepts a value-format retype to a text-family type; the fold \
         refuses it because introspection cannot recover the rewritten CHECK, not \
         because the server rejects it"
    );
}
