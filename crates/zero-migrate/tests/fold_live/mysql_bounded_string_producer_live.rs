//! **The MySQL half of the bounded-string producer: the bound goes, and so does the
//! storage family.**
//!
//! `pg_bounded_string_producer_live.rs` is the primary oracle and this is its companion
//! on the one other dialect whose DDL the fix CHANGES. It exists because a changed
//! emitter arm with no live coverage is exactly what the decimal fix had to declare
//! untested, and because the consequence here was not the one predicted.
//!
//! **What was predicted and is WRONG.** `schema::query`'s
//! `mysql_base_column_type_for_def` answers `VARCHAR(191)` for a widthless `string`, so
//! the guess was that dropping `maxLength` would NARROW a `t.string({ maxLength: 500 })`
//! column to 191 and cost the author a legal write. Measured against MySQL 8, it does
//! not: `render::fold::token_to_col_type` produced `ColType::Text`, `ir_column_to_field`
//! then set `unbounded_text` from that very `ColType`, and
//! `render::declarative::mysql_base_column_type` reads that marker BEFORE the type map
//! and answers a bare `TEXT`. The column came out `TEXT`, `CHARACTER_MAXIMUM_LENGTH`
//! 65535. The prediction was refuted by the server, which is why it is written down
//! here rather than quietly corrected.
//!
//! **What actually happens** is the same shape PostgreSQL showed, with a ceiling on it:
//! the declared bound is gone, MySQL accepts values the author forbade, and the column
//! additionally changes STORAGE FAMILY. `TEXT` is not a wide `VARCHAR` on MySQL — it
//! takes no bare literal `DEFAULT` (error 1101), an index over it needs a prefix length
//! (error 1170), and `mysql_type_family` classifies it separately for exactly those
//! reasons. So the producer did not merely lose a number; it moved the column into the
//! family whose DDL rules are different.
//!
//! # The oracle is the server
//!
//! `information_schema.COLUMNS.CHARACTER_MAXIMUM_LENGTH` is the declaration and
//! `DATA_TYPE` is the family, but a declaration is only a claim, so each case also
//! writes two values: one INSIDE the declared bound, which must survive intact on any
//! correct column, and one OUTSIDE it, which a `VARCHAR(500)` must not store. Reading
//! `CHAR_LENGTH` back is what makes the second assertion survive a server whose
//! `sql_mode` truncates instead of erroring — a truncated write and a refused one are
//! both "the value is not in the database", and a test that asserted on the error alone
//! would pass on one server and fail on the other.
//!
//! Gated on `ZERO_MIGRATE_MYSQL_URL` through `skip_if_no_mysql!`, so read the SKIP
//! banner before reading the pass count.

use crate::support;

use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use zero_migrate::apply::backend::{MigrationBackend, MysqlBackend};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::Op;
use zero_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor};
use zero_migrate::{
    descriptors_to_create_ops, resolve_create_table_policy, Approval, EffectivePolicy,
    ExecutorConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr, SqlDialect,
    TableRuntimeOptions,
};

const OWNER: &str = "app_bounded_string_mysql";
const TABLE: &str = "profiles";
const COLUMN: &str = "bio";
/// The declared bound. Above MySQL's widthless-`string` default of 191 and far below
/// `TEXT`'s 65535, so neither of the two answers a dropped facet can produce coincides
/// with it.
const BOUND: i64 = 500;
/// Inside [`BOUND`]. Any correct column keeps all of it; a case where this does not
/// survive is a broken fixture rather than a finding.
const INSIDE_LEN: usize = 300;
/// Outside [`BOUND`] and inside `TEXT`'s 65535. This is the discriminating value: a
/// `VARCHAR(500)` must not end up holding it, a `TEXT` will.
const OUTSIDE_LEN: usize = 1_000;

fn cfg_for(database: &str, policy: &EffectivePolicy) -> ExecutorConfig {
    ExecutorConfig::new(format!("project_{database}"), database, policy.clone())
}

/// The `createTable` ops an AUTHOR writes: the width is inside the `ColType`, where
/// nothing can drop it.
fn authored_ops(schema: &str, policy: &EffectivePolicy) -> Vec<Op> {
    let source = format!(
        r#"{{"ir_version":1,"name":"bounded_string","ops":[
          {{"op":"createTable","name":"{TABLE}","columns":[
            {{"name":"id","type":"int","nullable":false}},
            {{"name":"{COLUMN}","type":{{"string":{{"length":{BOUND}}}}},"nullable":true}}
          ],"primaryKey":["id"]}}
        ]}}"#
    );
    let raw: MigrationIr = serde_json::from_str(&source).expect("the bounded-string IR parses");
    resolve_create_table_policy(&raw, policy, schema)
        .expect("the bounded-string IR resolves")
        .ops
}

/// The same column declared as a descriptor, carrying the width as the `maxLength`
/// facet beside the token.
fn authored_descriptors() -> Vec<CollectionDescriptor> {
    vec![CollectionDescriptor {
        name: TABLE.to_string(),
        owner_app: OWNER.to_string(),
        fields: vec![
            FieldDescriptor {
                name: "id".to_string(),
                ty: "int".to_string(),
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

/// What MySQL declared for the column, and what it kept of the two values written into
/// it.
#[derive(Debug)]
struct Measured {
    /// `information_schema.COLUMNS.DATA_TYPE`: `varchar` or `text`. The STORAGE FAMILY,
    /// which changes here as well as the bound.
    data_type: String,
    /// `information_schema.COLUMNS.CHARACTER_MAXIMUM_LENGTH`.
    declared_max_length: Option<i64>,
    /// `CHAR_LENGTH` after writing an [`INSIDE_LEN`]-character value, or the refusal.
    inside_bound: Result<i64, String>,
    /// `CHAR_LENGTH` after writing an [`OUTSIDE_LEN`]-character value, or the refusal.
    /// `Ok(OUTSIDE_LEN)` means the server stored, in full, a value the author's declared
    /// bound forbids.
    outside_bound: Result<i64, String>,
}

/// Apply `make_ops` to a throwaway database, measure the column, and drop the database
/// on every exit path.
async fn measure(
    label: &str,
    make_ops: impl FnOnce(&str, &EffectivePolicy) -> Vec<Op>,
) -> Measured {
    let Some(url) = support::mysql::mysql_url() else {
        unreachable!("callers gate on `skip_if_no_mysql!` before reaching here")
    };
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("bstr");
    let policy = support::no_inject(&database);
    let cfg = cfg_for(&database, &policy);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated bounded-string database");

    let measured: Result<Measured, String> = async {
        let ops = make_ops(&cfg.project_schema, &policy);
        let backend = MysqlBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure the migration journal: {error}"))?;
        let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Mysql, &policy);
        let ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "bounded_string",
            "ops": ops,
        }))
        .map_err(|error| format!("re-parse the resolved op list: {error}"))?;
        let steps = author
            .lower_steps(&ir, &LiveSchema::default())
            .map_err(|error| format!("lower the bounded-string ops: {error}"))?;
        MigrationEngine::new()
            .apply_plan(
                &steps,
                Approval::Approved,
                &backend,
                &cfg,
                "bounded-string-mysql",
                LockMode::Acquire,
            )
            .await
            .map_err(|error| format!("apply the bounded-string plan: {error}"))?;
        measure_column(&session, &cfg.project_schema).await
    }
    .await;

    match measured {
        Ok(measured) => measured,
        Err(error) => panic!("{label}: {error}"),
    }
}

async fn measure_column(session: &MysqlDevSession, database: &str) -> Result<Measured, String> {
    let rows = session
        .query(
            "SELECT DATA_TYPE AS data_type, CHARACTER_MAXIMUM_LENGTH AS max_length \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?",
            &[database.into(), TABLE.into(), COLUMN.into()],
        )
        .await
        .map_err(|error| format!("read the live column declaration: {error}"))?;
    let row = rows
        .first()
        .ok_or_else(|| format!("the server holds no `{TABLE}.{COLUMN}` column at all"))?;
    let data_type: String = row
        .try_get("data_type")
        .map_err(|error| format!("decode the live data type: {error}"))?;
    let declared_max_length: Option<i64> = row
        .try_get("max_length")
        .map_err(|error| format!("decode the live character maximum length: {error}"))?;

    let inside_bound = write_and_read_back(session, database, 1, INSIDE_LEN).await?;
    let outside_bound = write_and_read_back(session, database, 2, OUTSIDE_LEN).await?;

    Ok(Measured {
        data_type,
        declared_max_length,
        inside_bound,
        outside_bound,
    })
}

/// Write a `len`-character value as row `id` and report what `CHAR_LENGTH` says is
/// there afterwards.
///
/// A refused write and a truncated one are both "the value is not in the database", and
/// which of the two a MySQL server does is a `sql_mode` setting rather than a property
/// of the column, so both are reported and the caller compares the LENGTH.
async fn write_and_read_back(
    session: &MysqlDevSession,
    database: &str,
    id: i64,
    len: usize,
) -> Result<Result<i64, String>, String> {
    let written = session
        .batch(&format!(
            "INSERT INTO {}.{} (id, {}) VALUES ({id}, '{}')",
            quote_ident(database),
            quote_ident(TABLE),
            quote_ident(COLUMN),
            "x".repeat(len)
        ))
        .await;
    if let Err(error) = written {
        return Ok(Err(error.to_string()));
    }
    let rows = session
        .query(
            &format!(
                "SELECT CHAR_LENGTH({}) AS len FROM {}.{} WHERE id = {id}",
                quote_ident(COLUMN),
                quote_ident(database),
                quote_ident(TABLE)
            ),
            &[],
        )
        .await
        .map_err(|error| format!("read the stored value back: {error}"))?;
    let stored: i64 = rows
        .first()
        .ok_or_else(|| format!("the row written as id {id} is not there"))?
        .try_get("len")
        .map_err(|error| format!("decode the stored length: {error}"))?;
    Ok(Ok(stored))
}

/// **The tripwire: the ops carrier was already right on MySQL too.**
///
/// Green before the producer fix and after it. It is what makes the next case a
/// disagreement between two carriers describing one column rather than a single
/// opinion this file happens to dislike.
#[compio::test]
async fn a_bounded_string_authored_as_ops_is_a_varchar_mysql_enforces() {
    let _url = skip_if_no_mysql!();
    let measured = measure("a bounded string authored as ops", authored_ops).await;

    assert_eq!(
        (measured.data_type.as_str(), measured.declared_max_length),
        ("varchar", Some(BOUND)),
        "the ops carrier must reach MySQL as VARCHAR({BOUND}), or nothing below is a \
         disagreement: {measured:?}"
    );
    assert_eq!(
        measured.inside_bound,
        Ok(INSIDE_LEN as i64),
        "MySQL must keep all {INSIDE_LEN} characters of a value INSIDE the declared \
         bound, or this fixture is broken rather than informative"
    );
    assert_ne!(
        measured.outside_bound,
        Ok(OUTSIDE_LEN as i64),
        "MySQL kept all {OUTSIDE_LEN} characters of a value OUTSIDE a declared \
         VARCHAR({BOUND}), so this file's enforcement oracle proves nothing"
    );
}

/// **The defect's MySQL face: the bound is gone and the storage family changed with
/// it.**
///
/// The same width, declared as a descriptor facet and run through the shipped
/// `descriptors_to_create_ops`. `token_to_col_type` mapped the `"string"` token to
/// `ColType::Text` without consulting `max_length`; `ir_column_to_field` derived
/// `unbounded_text` from that same `ColType`; and `mysql_base_column_type` reads the
/// marker ahead of the type map and emits a bare `TEXT`. Measured before the fix:
/// `DATA_TYPE = "text"`, `CHARACTER_MAXIMUM_LENGTH = 65535`, and the 1000-character
/// value stored in full.
#[compio::test]
async fn a_bounded_string_through_the_descriptor_producer_loses_its_bound_and_its_family() {
    let _url = skip_if_no_mysql!();
    let measured = measure(
        "a bounded string through the descriptor producer",
        |schema, policy| {
            descriptors_to_create_ops(&authored_descriptors(), schema, policy)
                .expect("the descriptor set produces createTable ops")
        },
    )
    .await;

    assert_eq!(
        (measured.data_type.as_str(), measured.declared_max_length),
        ("varchar", Some(BOUND)),
        "the descriptor producer dropped the declared width, and the widthless \
         `ColType::Text` it produced also carried the column into MySQL's TEXT storage \
         family - which takes no bare literal DEFAULT and needs a prefix length to be \
         indexed: {measured:?}"
    );
    assert_eq!(
        measured.inside_bound,
        Ok(INSIDE_LEN as i64),
        "a value inside the declared bound must survive on either carrier"
    );
    assert_ne!(
        measured.outside_bound,
        Ok(OUTSIDE_LEN as i64),
        "MySQL stored, in full, a {OUTSIDE_LEN}-character value in a column the author \
         bounded at {BOUND}: the producer did not merely lose a facet, it removed a \
         constraint the server was enforcing on the other carrier"
    );
}
