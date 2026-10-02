//! LAYER 1 of the backend conformance kit: the dialect table's declarations,
//! answered by a live server.
//!
//! `dialect_table_faithfulness.rs` proves the sidecar and generated
//! `DIALECT_TABLE` agree; the generated file's lib test pins all 276 cells to the
//! registered backend policies that production support queries. Those are still
//! the ENGINE'S OWN OPINION, and no connection is opened by either proof. So a
//! row may declare `portable`, fail against a real PostgreSQL, and pass it. This
//! file is the layer that closes exactly that gap, and nothing else: for every
//! `(op-kind, variant)` row of `dialect-support.toml` it drives the SAME
//! representative op (`tests/dialect_corpus`) through the production path -
//! `resolve_create_table_policy` -> `IrAuthor::load_and_lower_guarded` ->
//! `MigrationEngine::apply_plan` - and records ONE outcome class:
//!
//! ```text
//!   Applied               the server accepted the engine's own SQL
//!   RefusedByCapability   the engine refused before emitting, naming the dialect
//!   RefusedByPolicy       the charter or the SQL guard refused
//!   ServerError           the server REJECTED engine-emitted SQL
//!   EngineError           anything else
//! ```
//!
//! The rule is a total function of the declared disposition
//! (layer 1):
//!
//! | declared                | required outcome      |
//! |-------------------------|-----------------------|
//! | `portable` / `vendor`   | `Applied`             |
//! | `transparentDegradable` | `Applied`             |
//! | `unsupported`           | `RefusedByCapability` |
//!
//! and `ServerError` is a conformance failure on EVERY disposition, because it is
//! the shape of a migration that clears validate and preview and then dies
//! partway through applying.
//!
//! That last sentence is ENFORCED rather than asserted. The exception file cannot
//! record a `ServerError`: an `ALLOWANCES` entry naming one fails the const-eval
//! guard beside the `include!` and the suite does not BUILD.
//!
//! TO MAKE THE CORPUS EXECUTABLE. The representatives are built to be
//! CONSTRUCTIBLE and to select a support branch. Nothing in them assumes its
//! referents exist, and several of them name objects that are unique per DATABASE or
//! per CLUSTER rather than per schema. Three additions make them runnable:
//!
//!   1. A PRELUDE per row ([`prelude`]), itself authored as IR and applied through
//!      the same production path - never hand-written DDL. `dropIndex` needs the
//!      index; `validateConstraint` needs a NOT VALID constraint; `addConstraint`
//!      needs a target table with a matching key; `update t SET a = x` needs `a`
//!      and `x` to be the same type, which is why there is no single fixture table
//!      and the prelude is chosen per row.
//!   2. A LOCALIZATION of the names that are not schema-scoped ([`localize`]).
//!      A PostgreSQL ROLE is cluster-scoped, so the corpus's `r` would collide with
//!      a sibling test of this binary on the server they share. Roles and the
//!      `createSchema`/`dropSchema` name get a per-row unique suffix. The extension
//!      becomes [`PROBE_EXTENSION`], one of the two `code.extension` in
//!      `crate::support::operator_charter` allowlists. `nextval`'s hard-coded `app` schema
//!      is retargeted at the probe schema, because otherwise a cross-schema POLICY
//!      refusal would mask the capability answer the row is asking about.
//!   3. A LIVE SCHEMA read back from the catalog after the prelude, so the subject
//!      op lowers against what actually exists rather than against
//!      `LiveSchema::default()`.
//!
//! ISOLATION. The PostgreSQL sweep runs in a database of its own
//! ([`crate::support::PgDatabase`]), so the one DATABASE-scoped name the rows touch - the
//! extension - is this sweep's alone. Inside it, every row runs in its own schema
//! pair (project + meta), created and dropped per row, plus a per-row role, and the
//! two rows that install the extension drop it again before the next row runs
//! ([`touches_the_extension`]). Every SQLite row runs in its own `TempDir`. Every
//! MySQL row runs in its own throwaway DATABASE plus the `_migrations` meta database
//! the engine creates beside it, both reclaimed by `crate::support::mysql::DatabaseGuard`.
//!
//! LEAK CHECK. Each sweep ends by reading the catalog for a `zmconf_%` name that
//! survived it - `pg_namespace` for PostgreSQL, `information_schema.SCHEMATA` for
//! MySQL - and fails if it finds one: a survivor is a row whose guard did not run.
//! The check is a step of the sweep rather than its own `#[test]`, because a check
//! whose subject is another test's in-flight state has to be sequenced with it. Every
//! `zmconf_%` name on either server is this sweep's: the PostgreSQL catalog it reads is
//! its own database's, and no other test of the binary uses the prefix on the MySQL
//! server they share.
//!
//! SCOPE. PostgreSQL, SQLite and MySQL - all three dialects the engine emits for.
//! The MySQL leg's isolation unit is a throwaway DATABASE rather than a schema,
//! because MySQL has no CREATE SCHEMA that is not a CREATE DATABASE, and its leak
//! check reads `information_schema.SCHEMATA` where the PostgreSQL leg reads
//! `pg_namespace`. What the MySQL leg needed, and what it turned out NOT to need, is
//! written down at [`MYSQL_LEG`].
//!
//! COST. This is a live suite with one schema round-trip per row, against the
//! PostgreSQL and MySQL servers this binary owns.


use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{json, Value};
use tempfile::TempDir;

use crate::dialect_matrix::dialect_table::{Disposition, DIALECT_TABLE};
use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use crate::support::PgDevSession;
use zeroship_migrate::apply::backend::MigrationBackend;
use zeroship_migrate::apply::executor::{ApplyError, LockMode};
use zeroship_migrate::driver::{DbError, SqlSession};
use zeroship_migrate::model::ir::Op;
use zeroship_migrate::render::fold::single_fold;
use zeroship_migrate::render::lower::{
    IrGuardedLowerError, IrLowerError, LoadAndLowerGuardedError, LoweredArtifact,
};
use zeroship_migrate::{
    resolve_create_table_policy, Approval, DeclarativeApplyError, EffectivePolicy, EngineError,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine, MigrationIr,
};
use zeroship_migrate_mysql::MysqlBackend;
use zeroship_migrate_postgres::PostgresBackend;
use zeroship_migrate_sqlite::SqliteBackend;

/// What the MySQL leg needed, and where each piece of it lives. Recorded as a
/// constant so it is in the file a MySQL author opens, not only in a doc.
///
/// 1. Shared support, in `tests/integration/support/mysql.rs`: `mysql_url()`, the DSN of the
///    MySQL server this binary owns, and `MysqlDevSession`, which implements
///    `driver::SqlSession` over the blocking `mysql` crate exactly as `PgDevSession`
///    does over the PostgreSQL one. `DatabaseGuard` is the `SchemaGuard` sibling.
///    Three live MySQL suites already ride it (`tests/integration/fold_live/*_mysql.rs`).
/// 2. Here: [`mysql_verdict`]. MySQL has no CREATE SCHEMA that is not a
///    DATABASE, so the per-row isolation unit is a throwaway DATABASE and the
///    `pg_namespace` leak check becomes an `information_schema.SCHEMATA` check -
///    [`mysql_probe_databases`].
/// 3. Here: the per-row prelude review, [`prelude`]'s `MYSQL` dialect
///    arms. The findings are recorded beside them.
/// 4. Needed nothing: `disposition_for` reads `row.mysql` from the
///    generated table like the other two columns.
///
/// The one thing a MySQL author must NOT do is relax [`Outcome::ServerError`], and
/// the obvious way of relaxing it - recording one in the expectations file - is
/// refused by the compiler rather than by this sentence.
const MYSQL_LEG: &str = "see the module doc and this constant";

// ---------------------------------------------------------------------------
// Outcome classes
// ---------------------------------------------------------------------------

/// The layer-1 outcome vocabulary. Exactly one of these per (row, dialect).
///
/// `pub(crate)` because LAYER 2 records its refusals in this same vocabulary and takes
/// the tokens FROM here rather than re-spelling them
/// (`op_refused_observation.rs`). Two files spelling "RefusedByPolicy" independently
/// is two vocabularies that happen to agree today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The server accepted the engine-emitted SQL.
    Applied,
    /// The engine refused before emitting, naming the dialect's capability.
    RefusedByCapability,
    /// The charter or the SQL guard refused.
    RefusedByPolicy,
    /// The server REJECTED engine-emitted SQL. Always a conformance failure.
    ServerError,
    /// Anything else the engine returned.
    EngineError,
    /// The row's PRELUDE could not be established on this dialect, so the subject
    /// op was never asked. Never silently a pass: the set of these is pinned.
    NotExecutable,
}

impl Outcome {
    pub(crate) const fn token(&self) -> &'static str {
        match self {
            Self::Applied => "Applied",
            Self::RefusedByCapability => "RefusedByCapability",
            Self::RefusedByPolicy => "RefusedByPolicy",
            Self::ServerError => "ServerError",
            Self::EngineError => "EngineError",
            Self::NotExecutable => "NotExecutable",
        }
    }

    /// Whether this is the one outcome no allowance may name. `const` because the
    /// SERVER-ERROR GUARD that reads it - the anonymous `const _` block just below
    /// the `include!` of the expectations file - runs at COMPILE time, not at test
    /// time.
    const fn is_server_error(&self) -> bool {
        matches!(self, Self::ServerError)
    }
}

/// One row's live verdict, with the exact words whatever refused it used.
#[derive(Debug, Clone)]
struct Verdict {
    outcome: Outcome,
    /// The verbatim message - the server's own `message` field for a
    /// `ServerError`, the engine's `Display` otherwise. Empty when applied.
    detail: String,
}

impl Verdict {
    fn applied() -> Self {
        Self {
            outcome: Outcome::Applied,
            detail: String::new(),
        }
    }
    fn of(outcome: Outcome, detail: impl Into<String>) -> Self {
        Self {
            outcome,
            detail: detail.into(),
        }
    }
}

/// The outcome a disposition REQUIRES, per the proposal's layer-1 table.
const fn required_outcome(disposition: Disposition) -> Outcome {
    match disposition {
        Disposition::Portable | Disposition::Vendor | Disposition::TransparentDegradable => {
            Outcome::Applied
        }
        Disposition::Unsupported => Outcome::RefusedByCapability,
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Validate/lower codes that mean "this DIALECT cannot express this op".
///
/// `pub(crate)` for the same reason [`Outcome`] is: layer 2's `op_refused` observation
/// sorts a refusal into the SAME two classes and must sort it by the same list.
pub(crate) const CAPABILITY_CODES: &[&str] = &[
    zeroship_migrate::CODE_UNSUPPORTED,
    zeroship_migrate::CODE_DIALECT_UNSUPPORTED,
    zeroship_migrate::CODE_EXPR_NOT_PORTABLE,
    zeroship_migrate::CODE_PARTITION_COMPOSITE_KEY_UNSUPPORTED,
    zeroship_migrate::CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE,
    zeroship_migrate::CODE_PARTITION_HASH_DROP_UNDERIVABLE,
];

/// Validate codes that mean "the CHARTER did not authorize this", which is a
/// different question from what the dialect can do.
pub(crate) const POLICY_CODES: &[&str] = &[
    zeroship_migrate::model::validate::CODE_VENDOR_OP_DENIED,
    zeroship_migrate::model::validate::CODE_CROSS_SCHEMA,
    zeroship_migrate::model::validate::CODE_TABLE_SHAPE_POLICY,
    zeroship_migrate::model::validate::CODE_INVALID_SCHEMA_IDENT,
];

fn classify_lower(error: &LoadAndLowerGuardedError) -> Verdict {
    let detail = error.to_string();
    match error {
        LoadAndLowerGuardedError::Load(load) => {
            if let zeroship_migrate::model::load::IrLoadError::Validate(authoring) = load {
                let code = authoring.code.as_str();
                if CAPABILITY_CODES.contains(&code) {
                    return Verdict::of(Outcome::RefusedByCapability, detail);
                }
                if POLICY_CODES.contains(&code) {
                    return Verdict::of(Outcome::RefusedByPolicy, detail);
                }
            }
            Verdict::of(Outcome::EngineError, detail)
        }
        LoadAndLowerGuardedError::Lower(IrGuardedLowerError::Denied(_)) => {
            Verdict::of(Outcome::RefusedByPolicy, detail)
        }
        LoadAndLowerGuardedError::Lower(IrGuardedLowerError::Lower(lower)) => {
            classify_ir_lower(lower, detail)
        }
        LoadAndLowerGuardedError::Lower(IrGuardedLowerError::ReassemblyMismatch { .. }) => {
            Verdict::of(Outcome::EngineError, detail)
        }
    }
}

fn classify_ir_lower(error: &IrLowerError, detail: String) -> Verdict {
    match error {
        IrLowerError::VendorCapabilityDenied { .. }
        | IrLowerError::DefaultSchemaOutOfScope(_)
        | IrLowerError::LowerCrossSchema(_) => Verdict::of(Outcome::RefusedByPolicy, detail),
        IrLowerError::UnsupportedOp(_)
        | IrLowerError::AlterColumnNeedsWholeDefinition { .. }
        | IrLowerError::SchemaQualifierUnsupported { .. }
        | IrLowerError::TableRebuildUnavailable { .. }
        | IrLowerError::VendorUnsupported { .. }
        | IrLowerError::TriggerUnsupported { .. }
        | IrLowerError::ViewUnsupported { .. }
        | IrLowerError::SequenceUnsupported { .. }
        | IrLowerError::ExclusionConstraintUnsupported { .. }
        | IrLowerError::ColumnUnsupported { .. }
        | IrLowerError::IdentityColumnTypeUnsupported { .. } => {
            Verdict::of(Outcome::RefusedByCapability, detail)
        }
        IrLowerError::DmlValidate(authoring) => {
            let code = authoring.code.as_str();
            if CAPABILITY_CODES.contains(&code) {
                Verdict::of(Outcome::RefusedByCapability, detail)
            } else if POLICY_CODES.contains(&code) {
                Verdict::of(Outcome::RefusedByPolicy, detail)
            } else {
                Verdict::of(Outcome::EngineError, detail)
            }
        }
        _ => Verdict::of(Outcome::EngineError, detail),
    }
}

/// The server's own words for an apply failure, or `None` when the failure never
/// reached the server.
fn server_words(error: &DeclarativeApplyError) -> Option<String> {
    let apply = match error {
        DeclarativeApplyError::Plain(EngineError::Apply(apply)) => apply,
        DeclarativeApplyError::Expand(zeroship_migrate::OnlineError::Apply(apply)) => apply,
        _ => return None,
    };
    // `ApplyError::Backend(String)` is how the SQLite actor reports a statement the
    // database rejected: its whole error surface is a pre-formatted string, so a
    // SQLite `ServerError` arrives here and nowhere else. Leaving it in the
    // `EngineError` bucket would have hidden the one class this layer exists to
    // catch on the one dialect that always has a database.
    if let ApplyError::Backend(text) = apply {
        return Some(text.clone());
    }
    let source = match apply {
        ApplyError::Db(source) => source,
        ApplyError::MigrationFailed { source, .. } => source,
        _ => return None,
    };
    if let Some(db) = source.downcast_ref::<DbError>() {
        return Some(match &db.sqlstate {
            Some(state) => format!("[{state}] {}", db.message),
            None => db.message.clone(),
        });
    }
    if let Some(sqlite) = source.downcast_ref::<zeroship_migrate_sqlite::backend::SqliteActorError>() {
        return Some(sqlite.to_string());
    }
    Some(source.to_string())
}

fn classify_apply(error: &DeclarativeApplyError) -> Verdict {
    if let Some(words) = server_words(error) {
        return Verdict::of(Outcome::ServerError, words);
    }
    let detail = error.to_string();
    match error {
        DeclarativeApplyError::Plain(EngineError::Denied(_))
        | DeclarativeApplyError::Plain(EngineError::Apply(ApplyError::Guard { .. })) => {
            Verdict::of(Outcome::RefusedByPolicy, detail)
        }
        _ => Verdict::of(Outcome::EngineError, detail),
    }
}

// ---------------------------------------------------------------------------
// The authoring path, shared by prelude and subject
// ---------------------------------------------------------------------------

const OWNER: &str = "app_conformance";

/// Tables the corpus and its preludes can name. Pre-registered so an ownership
/// refusal never masquerades as a capability answer.
/// `v` is here because `Op::touched_table` answers with a trigger's TARGET, and
/// the `INSTEAD OF` row's target is a view. Without it that row would report an
/// ownership refusal instead of the capability answer it is asking about.
const OWNED_TABLES: &[&str] = &["t", "t2", "other", "p", "v"];

fn registry() -> BTreeMap<String, String> {
    OWNED_TABLES
        .iter()
        .map(|t| ((*t).to_string(), OWNER.to_string()))
        .collect()
}

fn envelope(name: &str, ops: &[Value], irreversible: bool) -> String {
    let mut doc = json!({ "ir_version": 1, "name": name, "ops": ops });
    if irreversible {
        doc["irreversible"] = json!("conformance probe: DML has no recorded inverse");
    }
    doc.to_string()
}

/// Author + lower one envelope through the production guarded path.
fn lower(
    envelope: &str,
    schema: &str,
    policy: &EffectivePolicy,
    dialect: &zeroship_migrate::DialectId,
    live: &LiveSchema,
) -> Result<LoweredArtifact, Verdict> {
    let authored: MigrationIr = match serde_json::from_str(envelope) {
        Ok(ir) => ir,
        Err(error) => {
            return Err(Verdict::of(
                Outcome::EngineError,
                format!("the conformance envelope did not parse: {error}"),
            ))
        }
    };
    let resolved = match resolve_create_table_policy(&authored, policy, schema) {
        Ok(ir) => ir,
        Err(error) => {
            return Err(Verdict::of(
                Outcome::RefusedByPolicy,
                format!("resolve create-table policy: {error}"),
            ))
        }
    };
    let source = match serde_json::to_string(&resolved) {
        Ok(text) => text,
        Err(error) => {
            return Err(Verdict::of(
                Outcome::EngineError,
                format!("re-serialize the resolved envelope: {error}"),
            ))
        }
    };
    let author = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        schema,
        OWNER,
        dialect,
        policy,
    );
    let guard = GuardConfig::from_policy(policy.clone(), dialect.clone(), schema);
    author
        .load_and_lower_guarded(&source, OWNER, &registry(), live, &guard)
        .map_err(|error| classify_lower(&error))
}

// ---------------------------------------------------------------------------
// Localization of the names that are not schema-scoped
// ---------------------------------------------------------------------------

/// Rewrite the corpus op's cluster-scoped / database-scoped names, and retarget the
/// hard-coded `app` sequence schema at the probe schema.
///
/// See the module header: a PostgreSQL ROLE is cluster-scoped and an EXTENSION is
/// database-scoped, so leaving the corpus's literal `r` / `citext` in place makes
/// this suite collide with any other test binary cargo runs beside it. Nothing here
/// changes which support BRANCH the op selects - `op_variant` keys on shape, not on
/// identifiers, and `dialect_table_faithfulness.rs` still proves that over the
/// unmodified corpus.
fn localize(kind: &str, op: &Op, probe: &Names) -> Value {
    let mut value = serde_json::to_value(op).expect("a corpus op serializes");
    match kind {
        "createSchema" | "dropSchema" => value["name"] = json!(probe.aux_schema),
        "createExtension" | "dropExtension" => value["name"] = json!(probe.extension),
        "createRole" | "alterRole" | "dropRole" => value["name"] = json!(probe.role),
        "dropOwnedBy" => value["roles"] = json!([probe.role]),
        "grant" => value["to"] = json!([probe.role]),
        "revoke" => value["from"] = json!([probe.role]),
        _ => {}
    }
    retarget_sequence_schema(&mut value, &probe.schema);
    value
}

/// Point every `{"name": _, "schema": "app"}` sequence reference at `schema`.
fn retarget_sequence_schema(value: &mut Value, schema: &str) {
    match value {
        Value::Object(map) => {
            if map.get("schema").and_then(Value::as_str) == Some("app") && map.contains_key("name")
            {
                map.insert("schema".to_string(), json!(schema));
            }
            for child in map.values_mut() {
                retarget_sequence_schema(child, schema);
            }
        }
        Value::Array(items) => {
            for child in items {
                retarget_sequence_schema(child, schema);
            }
        }
        _ => {}
    }
}

/// The per-row unique names a probe needs.
struct Names {
    schema: String,
    aux_schema: String,
    role: String,
    extension: &'static str,
}

// ---------------------------------------------------------------------------
// Preludes
// ---------------------------------------------------------------------------

fn col(name: &str, ty: Value, nullable: bool) -> Value {
    json!({ "name": name, "type": ty, "nullable": nullable })
}

/// `createTable t` with `a` of the given type, plus `b` of the same type and a
/// boolean `x` (the predicate column every `where` / `check` / `using` in the
/// corpus refers to).
fn table_t(a_type: Value, a_nullable: bool, with_pk: bool) -> Value {
    let mut op = json!({
        "op": "createTable",
        "name": "t",
        "columns": [
            col("id", json!("bigInt"), false),
            col("a", a_type.clone(), a_nullable),
            col("b", a_type, true),
            col("x", json!("boolean"), true),
        ],
        "constraints": [],
        "indexes": [],
    });
    if with_pk {
        op["primaryKey"] = json!(["id"]);
    }
    op
}

/// `t` without an `a` column, for the `addColumn` rows.
fn table_t_without_a() -> Value {
    json!({
        "op": "createTable",
        "name": "t",
        "columns": [
            col("id", json!("bigInt"), false),
            col("b", json!("text"), true),
            col("x", json!("boolean"), true),
        ],
        "primaryKey": ["id"],
        "constraints": [],
        "indexes": [],
    })
}

/// `t` with `a` but no `b`, for the `renameColumn a -> b` rows.
fn table_t_without_b() -> Value {
    json!({
        "op": "createTable",
        "name": "t",
        "columns": [
            col("id", json!("bigInt"), false),
            col("a", json!("text"), true),
            col("x", json!("boolean"), true),
        ],
        "primaryKey": ["id"],
        "constraints": [],
        "indexes": [],
    })
}

/// The FK target the `addConstraint` rows reference.
///
/// `other_col` takes the caller's string type rather than a literal `text`, because
/// it is UNIQUELY INDEXED below and MySQL refuses a key over a TEXT column with no
/// prefix length. See [`prelude`]'s `keyable`.
fn table_other(string_type: Value) -> Value {
    json!({
        "op": "createTable",
        "name": "other",
        "columns": [
            col("id", json!("bigInt"), false),
            col("x", json!("bigInt"), false),
            col("other_col", string_type, false),
        ],
        "primaryKey": ["id"],
        // Unique INDEXES, not table-level unique CONSTRAINTS: SQLite refuses the
        // latter at lower ("SQLite createTable table-level unique constraints are
        // not threaded into the emitter"), and an FK target only needs a unique
        // index for PostgreSQL to accept the reference.
        "constraints": [],
        "indexes": [
            { "name": "other_id_x_uq", "unique": true,
              "columns": [{ "kind": "column", "name": "id" },
                          { "kind": "column", "name": "x" }] },
            { "name": "other_col_uq", "unique": true,
              "columns": [{ "kind": "column", "name": "other_col" }] },
        ],
    })
}

fn table_t_partitioned() -> Value {
    json!({
        "op": "createTable",
        "name": "t",
        "columns": [col("id", json!("bigInt"), false)],
        "primaryKey": ["id"],
        "constraints": [],
        "indexes": [],
        "partitionBy": { "kind": "range", "columns": ["id"] },
    })
}

fn create_sequence() -> Value {
    json!({ "op": "createSequence", "name": "s" })
}

fn create_function() -> Value {
    json!({
        "op": "createFunction", "name": "f", "returns": "trigger",
        "language": "procedural", "body": "BEGIN RETURN NEW; END",
    })
}

/// The ops that must already have applied for this row's representative to have
/// its referents. Authored as IR and applied through the SAME production path -
/// never hand-written DDL.
fn prelude(
    kind: &str,
    variant: &str,
    dialect: &zeroship_migrate::DialectId,
    probe: &Names,
) -> Vec<Value> {
    // THE MySQL AXIS, and it is the exact counterpart of the SQLite one below.
    //
    // `text` renders as MySQL TEXT storage, and MySQL refuses a key over a TEXT or
    // BLOB column that carries no prefix length (error 1170). Wherever THIS FIXTURE
    // puts a key on a string column - `alterPrimaryKey`'s `id`, the unique constraint
    // `dropConstraint` drops, the index `dropIndex` drops, the `other_col` the FK rows
    // reference, and the columns `createIndex` / `addConstraint(unique)` /
    // `insert(onConflict…)` key - the column has to be a BOUNDED string on MySQL, or
    // the row measures MySQL's TEXT-key limit rather than the question it is asking.
    // Same class of fixture choice as `alterPrimaryKey`'s SQLite `id` below: pick the
    // type that does not provoke an unrelated refusal. `fold_roundtrip_mysql.rs`
    // records the same rule from the other end ("a key over a bare TEXT column is
    // MySQL error 1170, so ... every keyed column below is a bounded string").
    //
    // MEASURED, and worth recording because it is a real asymmetry in the engine: with
    // a `text` column here, the engine REFUSES the key at lower when it is written
    // inside `createTable.indexes` or a `createTable` constraint ("createTable.indexes
    // keys t.id, which renders as MySQL TEXT storage"), but does NOT refuse it for a
    // STANDALONE `createIndex` or `addConstraint(unique)`. Those two reached the
    // server and died there with `[42000] BLOB/TEXT column 'a' used in key
    // specification without a key length` - a ServerError. Bounding the column is the
    // FIXTURE half of the answer; the ungated standalone lane is an engine finding,
    // not something this fixture can repair.
    let keyable = || {
        if dialect == &zeroship_migrate_mysql::DIALECT {
            json!({ "string": { "length": 24 } })
        } else {
            json!("text")
        }
    };
    // A `t` whose `a` (and `b`) this fixture is about to KEY.
    let keyed = || table_t(keyable(), true, true);
    let text = || table_t(json!("text"), true, true);
    let bigint = || table_t(json!("bigInt"), true, true);
    let boolean = || table_t(json!("boolean"), true, true);
    let jsonb = || table_t(json!("json"), true, true);
    let int = || table_t(json!("int"), true, true);
    let varchar = || table_t(json!({ "string": { "length": 24 } }), true, true);

    match (kind, variant) {
        // The sequence a `nextval` default resolves against. This arm MUST precede
        // the `("createTable", _)` catch-all below; when it did not, the row applied
        // its CREATE TABLE and PostgreSQL answered
        // `relation "...s" does not exist` - a ServerError produced entirely by the
        // fixture, which is why a conformance suite has to separate a missing
        // referent from a wrong declaration before it reports anything.
        ("createTable", "nextvalDefault") => vec![create_sequence()],

        // The subject creates `t` itself, or names nothing that must exist.
        ("createTable", _)
        | ("createEnum", _)
        | ("createDomain", "base")
        | ("createSequence", _)
        | ("createSchema", _)
        | ("createExtension", _)
        | ("createRole", _)
        | ("createFunction", _)
        | ("raw", _)
        | ("dialectal", _) => vec![],

        ("createDomain", "nextvalDefault") => vec![create_sequence()],

        // `id` is TEXT here, not the `bigInt` every other prelude uses. SQLite turns
        // an INTEGER primary key into a rowid alias, and the engine's own
        // primary-key lifecycle precondition refuses an add that would introduce
        // rowid generation. That refusal is correct and is not what this row asks
        // about, so the fixture picks a key type that does not provoke it.
        // ... and the unique index, because SQLite's primary-key lifecycle refuses
        // an add whose target key has no exact pre-existing UNIQUE key.
        ("alterPrimaryKey", _) => vec![json!({
            "op": "createTable", "name": "t",
            "columns": [
                col("id", keyable(), false),
                col("a", json!("text"), true),
                col("b", json!("text"), true),
                col("x", json!("boolean"), true),
            ],
            "constraints": [],
            "indexes": [{ "name": "t_id_uq", "unique": true,
                          "columns": [{ "kind": "column", "name": "id" }] }],
        })],
        ("synchronizeIdentity", _) => vec![json!({
            "op": "createTable", "name": "t",
            "columns": [
                { "name": "id", "type": "bigInt", "nullable": false,
                  "identity": { "always": false } },
            ],
            "primaryKey": ["id"], "constraints": [], "indexes": [],
        })],

        ("dropIndex", _) => vec![
            keyed(),
            json!({ "op": "createIndex", "table": "t", "name": "i",
                    "columns": [{ "kind": "column", "name": "a" }] }),
        ],
        ("dropColumnNotNull", _) => vec![table_t(json!("text"), false, true)],
        ("dropColumnDefault", _) => vec![
            int(),
            json!({ "op": "setColumnDefault", "table": "t", "column": "a",
                    "value": { "literal": { "value": 1 } } }),
        ],
        ("dropConstraint", _) => vec![
            keyed(),
            json!({ "op": "addConstraint", "table": "t",
                    "constraint": { "name": "c",
                                    "kind": { "kind": "unique", "columns": ["a"] } } }),
        ],
        ("validateConstraint", _) => vec![
            table_other(keyable()),
            bigint(),
            json!({ "op": "addConstraint", "table": "t",
                    "constraint": { "name": "c",
                                    "kind": { "kind": "fk", "columns": ["a"],
                                              "referencesTable": "other",
                                              "referencesColumns": ["id"],
                                              "notValid": true } } }),
        ],

        // `update t SET a = x` and the backfill's `set` need `a` and `x` to be the
        // SAME type, which no single fixture table can give every row.
        ("update", _) | ("backfill", _) => vec![boolean()],

        ("dropEnum", _) => vec![json!({ "op": "createEnum", "name": "e", "values": ["a"] })],
        ("dropDomain", _) => vec![json!({ "op": "createDomain", "name": "d", "as": "text" })],
        // The trigger a `dropTrigger` drops, and the third axis this fixture splits
        // three ways rather than two. A PostgreSQL trigger EXECUTES A FUNCTION; a
        // SQLite trigger carries a BODY, and a bare `SELECT x` is a legal SQLite
        // body. MySQL takes a body too, but MySQL forbids a trigger from returning a
        // result set (`[0A000] Not allowed to return a result set from a trigger`),
        // so `SELECT x` cannot establish this row's referent there.
        //
        // A DELETE is a legal MySQL trigger statement, provided its target is NOT the
        // table the trigger is attached to, so the prelude supplies `t2` and deletes
        // from it. What this row asks is whether `dropTrigger` drops a trigger, not
        // what the trigger's body says.
        ("dropTrigger", _) => {
            if dialect == &zeroship_migrate_mysql::DIALECT {
                vec![
                    text(),
                    json!({ "op": "createTable", "name": "t2",
                        "columns": [col("id", json!("bigInt"), false),
                                    col("x", json!("boolean"), true)],
                        "primaryKey": ["id"], "constraints": [], "indexes": [] }),
                    json!({ "op": "createTrigger", "name": "tg", "table": "t",
                        "timing": "before", "events": ["insert"], "forEach": "row",
                        "action": { "kind": "body", "statements": [
                            { "stmt": "delete", "table": "t2",
                              "where": { "node": "colRef", "name": "x" } }] } }),
                ]
            } else if dialect == &zeroship_migrate_postgres::DIALECT {
                vec![
                    text(),
                    create_function(),
                    json!({ "op": "createTrigger", "name": "tg", "table": "t",
                            "timing": "before", "events": ["insert"], "forEach": "row",
                            "action": { "kind": "executeFunction", "name": "f" } }),
                ]
            } else if dialect == &zeroship_migrate_sqlite::DIALECT {
                vec![
                    text(),
                    json!({ "op": "createTrigger", "name": "tg", "table": "t",
                            "timing": "before", "events": ["insert"], "forEach": "row",
                            "action": { "kind": "body", "statements": [
                                { "stmt": "select", "expr": { "node": "colRef", "name": "x" } }] } }),
                ]
            } else {
                panic!("unregistered test dialect {dialect}")
            }
        }

        ("createPartition", _) => vec![table_t_partitioned()],
        ("attachPartition", _) => vec![
            table_t_partitioned(),
            json!({ "op": "createTable", "name": "p",
                    "columns": [col("id", json!("bigInt"), false)],
                    "primaryKey": ["id"], "constraints": [], "indexes": [] }),
        ],
        ("detachPartition", _) | ("dropPartition", _) => vec![
            table_t_partitioned(),
            json!({ "op": "createPartition", "name": "p", "of": "t",
                    "bounds": { "kind": "default" } }),
        ],

        ("alterSequence", _) | ("dropSequence", _) => vec![create_sequence()],
        ("dropSchema", _) => vec![json!({ "op": "createSchema", "name": probe.aux_schema })],
        ("dropExtension", _) => {
            vec![json!({ "op": "createExtension", "name": probe.extension })]
        }
        ("alterRole", _) | ("dropRole", _) | ("dropOwnedBy", _) => {
            vec![json!({ "op": "createRole", "name": probe.role })]
        }
        ("grant", _) | ("revoke", _) => {
            vec![text(), json!({ "op": "createRole", "name": probe.role })]
        }
        ("dropFunction", _) => vec![create_function()],
        ("dropPolicy", _) => vec![
            text(),
            json!({ "op": "createPolicy", "name": "p", "table": "t", "forCmd": "all",
                    "using": { "node": "colRef", "name": "x" } }),
        ],

        ("addColumn", "nextvalDefault") => vec![table_t_without_a(), create_sequence()],
        ("addColumn", _) => vec![table_t_without_a()],

        ("createIndex", "pgOnlyMethodOrFeature") => vec![jsonb()],
        ("createIndex", _) => vec![keyed()],

        ("setColumnType", _) => vec![varchar()],
        ("setColumnDefault", "base") => vec![int()],
        ("setColumnDefault", "containerOrJson") => vec![jsonb()],
        ("setColumnDefault", "nextval") => vec![bigint(), create_sequence()],

        ("renameColumn", _) => vec![table_t_without_b()],

        ("addConstraint", "fkSimple") => vec![table_other(keyable()), keyed()],
        ("addConstraint", "unique" | "check") => vec![keyed()],
        ("addConstraint", "exclusion") => vec![text()],
        ("addConstraint", _) => vec![table_other(keyable()), bigint()],

        ("insert", "base") => vec![text()],
        ("insert", _) => vec![
            keyed(),
            json!({ "op": "createIndex", "table": "t", "name": "t_a_uq", "unique": true,
                    "columns": [{ "kind": "column", "name": "a" }] }),
        ],

        ("dropView", "materialized") => vec![
            text(),
            json!({ "op": "createView", "name": "v", "materialized": true,
                    "query": { "kind": "structured",
                               "select": { "from": { "name": "t" }, "projection": [] } } }),
        ],
        ("dropView", _) => vec![
            text(),
            json!({ "op": "createView", "name": "v",
                    "query": { "kind": "structured",
                               "select": { "from": { "name": "t" }, "projection": [] } } }),
        ],

        ("createTrigger", "executeFunction") => vec![text(), create_function()],

        // The ONLY row whose target is a VIEW. An INSTEAD OF trigger has no valid
        // form on a table on either dialect, so the representative names `v` and
        // the prelude has to supply it. See `IrLowerError::InsteadOfTriggerTargetIsATable`.
        ("createTrigger", "bodyInsteadOf") => vec![
            text(),
            json!({ "op": "createView", "name": "v",
                    "query": { "kind": "structured",
                               "select": { "from": { "name": "t" }, "projection": [] } } }),
        ],

        // Everything else needs a plain `t` with a text `a` and a boolean `x`.
        _ => vec![text()],
    }
}

// ---------------------------------------------------------------------------
// The PostgreSQL probe
// ---------------------------------------------------------------------------

/// A per-row unique base name. A row appends `_aux`, `_migrations` and `_role` to
/// it, and PostgreSQL truncates an identifier past 63 bytes from the tail, so the
/// slug is capped and the sequence number that makes the name unique sits inside the
/// bound.
fn nonce(kind: &str, variant: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, Ordering::SeqCst);
    let slug: String = format!("{kind}_{variant}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let slug: String = slug.to_ascii_lowercase().chars().take(28).collect();
    format!("{PROBE_PREFIX}{slug}_{seq}")
}

/// The name prefix every probe schema and database carries, so the leak check can
/// find them all with one catalog predicate.
const PROBE_PREFIX: &str = "zmconf_";

// ---------------------------------------------------------------------------
// The extension
// ---------------------------------------------------------------------------

/// The extension the `createExtension` and `dropExtension` rows install and drop.
///
/// `support`'s operator charter allowlists `code.extension = ["citext", "pgcrypto"]`,
/// so a row that named anything else would be refused by POLICY and would stop asking
/// its question.
const PROBE_EXTENSION: &str = "pgcrypto";

/// Whether this row installs [`PROBE_EXTENSION`], as its subject or its prelude.
///
/// An extension is installed per DATABASE, not per schema, so the per-row schema
/// pair does not reclaim it and the next row would inherit it: the `dropExtension`
/// row's prelude would then answer `already exists`, an answer about the leftover
/// rather than about the declaration. [`pg_verdict`] drops it at the end of these two
/// rows and no others.
fn touches_the_extension(kind: &str) -> bool {
    matches!(kind, "createExtension" | "dropExtension")
}

async fn pg_verdict(url: &str, kind: &str, variant: &str, op: &Op) -> Verdict {
    let session = PgDevSession::connect(url);
    let base = nonce(kind, variant);
    let probe = Names {
        schema: base.clone(),
        aux_schema: format!("{base}_aux"),
        role: format!("{base}_role"),
        extension: PROBE_EXTENSION,
    };
    let policy = crate::support::operator_charter(&probe.schema);
    let cfg = ExecutorConfig::new(format!("prj_{base}"), &probe.schema, policy.clone());
    let _guard = crate::support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
            probe.aux_schema.clone(),
        ],
    );
    if let Err(error) = session
        .batch(&format!("CREATE SCHEMA \"{}\"", cfg.project_schema))
        .await
    {
        return Verdict::of(
            Outcome::NotExecutable,
            format!("could not create the probe schema: {error}"),
        );
    }
    let backend = PostgresBackend::new_generic(&session);
    if let Err(error) = backend.ensure_journal(&cfg).await {
        return Verdict::of(
            Outcome::NotExecutable,
            format!("could not create the journal: {error}"),
        );
    }

    let verdict = run_row(
        kind,
        variant,
        op,
        &zeroship_migrate_postgres::DIALECT,
        &probe,
        &policy,
        &cfg,
        &backend,
    )
    .await;

    // Roles are CLUSTER-scoped, so the schema guard cannot reclaim them.
    let _ = session
        .batch(&format!(
            "DROP OWNED BY \"{role}\" CASCADE; DROP ROLE IF EXISTS \"{role}\"",
            role = probe.role
        ))
        .await;
    // A failed drop leaves the extension for the next row's prelude to trip over,
    // which would be reported against that row; it stops the sweep here instead.
    if touches_the_extension(kind) {
        if let Err(error) = session
            .batch(&format!("DROP EXTENSION IF EXISTS \"{PROBE_EXTENSION}\""))
            .await
        {
            panic!("drop {PROBE_EXTENSION} after the {kind}/{variant} row: {error}");
        }
    }
    verdict
}

// ---------------------------------------------------------------------------
// The MySQL probe
// ---------------------------------------------------------------------------

/// One row, in its own throwaway MySQL DATABASE.
///
/// MySQL has no namespace INSIDE a database, so what the PostgreSQL leg does with a
/// schema pair this leg does with a database pair: the probe database, and the
/// `<db>_migrations` meta database the engine's own `ensure_journal` creates beside
/// it. [`DatabaseGuard`] is armed over both BEFORE the first `CREATE DATABASE`, for
/// the reason its own header records - a drop written as the last statement of a
/// test only runs when the test reaches it.
///
/// The name is [`nonce`]'s, unchanged. MySQL caps an identifier at 64 bytes, and the
/// cap on `nonce`'s slug leaves room under it for the `_migrations` the engine appends
/// and the `_aux` a `createSchema` row would add.
async fn mysql_verdict(url: &str, kind: &str, variant: &str, op: &Op) -> Verdict {
    let session = MysqlDevSession::connect(url);
    let base = nonce(kind, variant);
    let probe = Names {
        schema: base.clone(),
        aux_schema: format!("{base}_aux"),
        role: format!("{base}_role"),
        extension: PROBE_EXTENSION,
    };
    let policy = crate::support::operator_charter(&probe.schema);
    let cfg = ExecutorConfig::new(format!("prj_{base}"), &probe.schema, policy.clone());
    let _guard = DatabaseGuard::arm(&session, [probe.schema.clone(), probe.aux_schema.clone()]);
    if let Err(error) = session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&probe.schema)))
        .await
    {
        return Verdict::of(
            Outcome::NotExecutable,
            format!("could not create the probe database: {error}"),
        );
    }
    let backend = MysqlBackend::new_generic(&session);
    if let Err(error) = backend.ensure_journal(&cfg).await {
        return Verdict::of(
            Outcome::NotExecutable,
            format!("could not create the journal: {error}"),
        );
    }

    run_row(
        kind,
        variant,
        op,
        &zeroship_migrate_mysql::DIALECT,
        &probe,
        &policy,
        &cfg,
        &backend,
    )
    .await
}

/// Every `zmconf_%` DATABASE `information_schema` holds, and the whole-server
/// database count beside it.
///
/// The MySQL sibling of [`probe_schemas`], and it answers the same two questions in
/// the same order: the global count moves for reasons that are not this file's (the
/// other live MySQL tests of this binary create databases on the same server), and
/// the prefixed list is the one the leak check fails on.
///
/// `information_schema.SCHEMATA` rather than `SHOW DATABASES`, because it takes a
/// bind and returns a named column, which the seam's `query` contract needs. The
/// underscore in `zmconf_%` is a LIKE wildcard on both servers and is left as one
/// for the same reason the PostgreSQL query leaves it: it can only widen the match,
/// and nothing but this sweep names a database `zmconf`.
async fn mysql_probe_databases(session: &MysqlDevSession) -> (i64, Vec<String>) {
    let total: i64 = session
        .query_one("SELECT count(*) AS n FROM information_schema.SCHEMATA", &[])
        .await
        .expect("count information_schema.SCHEMATA")
        .try_get::<_, i64>("n")
        .expect("decode the information_schema.SCHEMATA count");
    let rows = session
        .query(
            "SELECT SCHEMA_NAME AS nspname FROM information_schema.SCHEMATA \
             WHERE SCHEMA_NAME LIKE ? ORDER BY SCHEMA_NAME",
            &[zeroship_migrate::driver::Bind::Text(format!("{PROBE_PREFIX}%"))],
        )
        .await
        .expect("query information_schema.SCHEMATA for probe databases");
    let prefixed = rows
        .iter()
        .filter_map(|row| row.try_get::<_, String>("nspname").ok())
        .collect();
    (total, prefixed)
}

// ---------------------------------------------------------------------------
// The SQLite probe
// ---------------------------------------------------------------------------

const SQLITE_PROJECT: &str = "prj_conformance";

async fn sqlite_verdict(kind: &str, variant: &str, op: &Op) -> Verdict {
    let dir: TempDir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(error) => {
            return Verdict::of(
                Outcome::NotExecutable,
                format!("could not create a temp dir: {error}"),
            )
        }
    };
    let app: PathBuf = dir.path().join("probe.sqlite");
    let backend = match SqliteBackend::open(&app) {
        Ok(backend) => backend,
        Err(error) => {
            return Verdict::of(
                Outcome::NotExecutable,
                format!("could not open the probe database: {error}"),
            )
        }
    };
    let base = nonce(kind, variant);
    let probe = Names {
        schema: SQLITE_PROJECT.to_string(),
        aux_schema: format!("{base}_aux"),
        role: format!("{base}_role"),
        extension: PROBE_EXTENSION,
    };
    let policy = crate::support::operator_charter(SQLITE_PROJECT);
    let cfg = ExecutorConfig::new(SQLITE_PROJECT, SQLITE_PROJECT, policy.clone());
    run_row(
        kind,
        variant,
        op,
        &zeroship_migrate_sqlite::DIALECT,
        &probe,
        &policy,
        &cfg,
        &backend,
    )
    .await
}

// ---------------------------------------------------------------------------
// One row, one dialect
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_row<B: MigrationBackend>(
    kind: &str,
    variant: &str,
    op: &Op,
    dialect: &zeroship_migrate::DialectId,
    probe: &Names,
    policy: &EffectivePolicy,
    cfg: &ExecutorConfig,
    backend: &B,
) -> Verdict {
    // 1. PRELUDE, through the same production path.
    let prelude_ops = prelude(kind, variant, dialect, probe);
    let mut prelude_failure: Option<String> = None;
    if !prelude_ops.is_empty() {
        let source = envelope(&format!("{kind}_{variant}_prelude"), &prelude_ops, false);
        match lower(
            &source,
            &cfg.project_schema,
            policy,
            dialect,
            &LiveSchema::default(),
        ) {
            Err(verdict) => prelude_failure = Some(format!("prelude lower: {}", verdict.detail)),
            Ok(artifact) => {
                if let Err(error) = MigrationEngine::new(zeroship_migrate::shipping_vendors())
                    .apply_plan(
                        &artifact.plan.steps,
                        Approval::Approved,
                        backend,
                        cfg,
                        "dialect-conformance-prelude",
                        LockMode::Acquire,
                    )
                    .await
                {
                    prelude_failure = Some(format!("prelude apply: {error}"));
                }
            }
        }
    }

    // 2. LIVE SCHEMA read back from the catalog, so the subject lowers against what
    //    actually exists rather than against a default.
    //
    //    SQLite additionally needs `sdk_schemas`: its 12-step rebuild is authored
    //    from the SDK field maps, not from the catalog, so a catalog-only LiveSchema
    //    makes every rebuild-shaped op refuse with "the live table snapshot is
    //    incomplete" whatever the dialect table says. `engine::refresh_historical_live`
    //    folds the applied history for exactly this, and the prelude IS this row's
    //    history, so folding it here is the production derivation, not a shortcut.
    let mut live = match backend.snapshot_schema(cfg).await {
        Ok(snapshot) => LiveSchema::from_catalog_snapshot(snapshot, OWNER),
        Err(_) => LiveSchema::from_tables(BTreeSet::new()),
    };
    if dialect == &zeroship_migrate_sqlite::DIALECT && !prelude_ops.is_empty() {
        let history: Vec<Op> = prelude_ops
            .iter()
            .filter_map(|op| serde_json::from_value(op.clone()).ok())
            .collect();
        if let Ok(defs) = single_fold::fold(
            zeroship_migrate::shipping_vendors(),
            &history,
            dialect,
            &cfg.project_schema,
            policy,
        )
        .map(|folded| folded.project_field_defs(zeroship_migrate::shipping_vendors()))
        {
            live.sdk_schemas = defs;
        }
    }

    // 3. SUBJECT.
    let subject = localize(kind, op, probe);
    let is_dml = matches!(kind, "insert" | "update" | "delete" | "backfill");
    let source = envelope(&format!("{kind}_{variant}"), &[subject], is_dml);
    let verdict = match lower(&source, &cfg.project_schema, policy, dialect, &live) {
        Err(verdict) => verdict,
        Ok(artifact) => match MigrationEngine::new(zeroship_migrate::shipping_vendors())
            .apply_plan(
                &artifact.plan.steps,
                Approval::Approved,
                backend,
                cfg,
                "dialect-conformance",
                LockMode::Acquire,
            )
            .await
        {
            Ok(_) => Verdict::applied(),
            Err(error) => classify_apply(&error),
        },
    };

    // A prelude that could not be established makes an APPLY meaningless, but a
    // capability refusal is decided before the server is touched, so it is still a
    // true answer. Only demote the rows whose answer the prelude could have changed.
    match (prelude_failure, &verdict.outcome) {
        (Some(_), Outcome::RefusedByCapability) => verdict,
        (Some(why), _) => Verdict::of(
            Outcome::NotExecutable,
            format!(
                "{why} || subject: {} {}",
                verdict.outcome.token(),
                verdict.detail
            ),
        ),
        (None, _) => verdict,
    }
}

// ---------------------------------------------------------------------------
// The expectation, and the named allowances
// ---------------------------------------------------------------------------

/// A row whose declaration and the server DISAGREE, recorded rather than fixed.
///
/// This is NOT a suppression. An allowance NAMES the other value: the outcome the
/// server actually produces and a substring of its exact words. If the row starts
/// agreeing, or disagrees DIFFERENTLY, the test fails - so an allowance can go red
/// in both directions, which an `#[ignore]` cannot.
struct Allowance {
    kind: &'static str,
    variant: &'static str,
    dialect: &'static str,
    /// The outcome actually observed, which must still be observed.
    observed: Outcome,
    /// A substring of the verbatim refusal, which must still appear.
    words: &'static str,
    /// Why this is recorded rather than fixed. Read by a human.
    why: &'static str,
}

/// Rows whose representative could not be made executable at all, with the reason.
/// Pinned so the set cannot quietly grow.
struct NotExecutableRow {
    kind: &'static str,
    variant: &'static str,
    dialect: &'static str,
    why: &'static str,
}

/// The engine's internal sentinel for "this cell is declared unsupported and
/// nobody wrote the operator-facing reason", from `op_support.rs`.
const NO_REASON_SENTINEL: &str = "internal: supported cell has no refusal reason";

/// Rows that currently show [`NO_REASON_SENTINEL`] to the operator, pinned.
///
/// This check exists because of a limit on layer 1 as the proposal specifies it.
/// Production support reads the selected backend policy, whose answers are pinned to
/// the generated table by the cell-parity test. An `unsupported` answer makes validate
/// refuse on that backend's own say-so: the required outcome `RefusedByCapability` is
/// satisfied BY CONSTRUCTION, and the check cannot fail. Flipping a supported op to
/// `unsupported` leaves the suite green.
///
/// What that flip DOES change is the operator's message, which becomes the sentinel
/// above. So the message is the one observable that a too-conservative declaration
/// still moves, and pinning it recovers a real check from a tautological cell.
struct PlaceholderReason {
    kind: &'static str,
    variant: &'static str,
    dialect: &'static str,
    why: &'static str,
}

include!("../dialect_conformance/expectations.rs");

// ---------------------------------------------------------------------------
// THE SERVER-ERROR GUARD
// ---------------------------------------------------------------------------

/// A [`Outcome::ServerError`] allowance is rejected AT LOAD, and "load" here means
/// the compiler: this block is const-evaluated, so an `ALLOWANCES` entry naming
/// `ServerError` does not fail the suite, it fails the BUILD of the suite.
///
/// A `ServerError` is never absorbed by an allowance. The judge below has a
/// `verdict.outcome != Outcome::ServerError` conjunct, but read what it buys:
/// `required_outcome` returns only `Applied` or `RefusedByCapability`, never
/// `ServerError`, so that conjunct is already implied by `verdict.outcome ==
/// required` and changes nothing. All it does is stop a `ServerError` counting as
/// AGREEMENT - which it could not have done anyway. The row then FALLS THROUGH to
/// the `ALLOWANCES` lookup, where an entry carrying `observed: Outcome::ServerError`
/// would match, satisfy both `assert_eq!(verdict.outcome, allowance.observed)` and
/// the `words` assertion, land in `used`, and excuse the row. That is the exact
/// failure this layer exists to catch: a migration that clears validate and preview
/// and then dies partway through applying, waved through by the exception file.
///
/// SCOPE, so this is not read as more than it is. This rejects the DECLARATION, not
/// the observation. A row that actually produces a `ServerError` is still judged at
/// run time by [`judge`], and with this block in place it can only reach the
/// `unexplained` list or fail the `assert_eq!` against some other allowance's
/// `observed` - both of which are red. What this block removes is the one path by
/// which that red could have been declared away.
const _: () = {
    let mut i = 0;
    while i < ALLOWANCES.len() {
        assert!(
            !ALLOWANCES[i].observed.is_server_error(),
            "an ALLOWANCES entry names Outcome::ServerError. A ServerError is the \
             server REJECTING engine-emitted SQL - a migration that clears validate \
             and preview and then dies partway through applying - and it is a \
             conformance failure on EVERY disposition. It is not an exception that \
             can be recorded; remove the entry and FIX THE ROW. If the engine should \
             have refused the op before emitting, teach it to (see \
             createTrigger/bodySimple on mysql in the expectations file, which was \
             exactly this and became a RefusedByCapability allowance once \
             render/renderer.rs learned to refuse the statement at lower)."
        );
        i += 1;
    }
};

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// The table's disposition for a (kind, variant) on a dialect NAMED BY ITS ID.
///
/// `dialect` is the dialect id, so this is a table lookup rather than a
/// three-arm match on vendor names — the row is keyed by `DialectId` now. It
/// still panics on a name the table does not carry, which is the same failure the
/// old `other =>` arm produced, and is what keeps a typo'd dialect from silently
/// resolving to nothing.
fn disposition_for(kind: &str, variant: &str, dialect: &str) -> Disposition {
    let row = DIALECT_TABLE
        .iter()
        .find(|row| row.kind == kind && row.variant == variant)
        .unwrap_or_else(|| panic!("no DIALECT_TABLE row for {kind}/{variant}"));
    row.dispositions
        .iter()
        .find(|(id, _)| id.as_str() == dialect)
        .map(|(_, disposition)| *disposition)
        .unwrap_or_else(|| panic!("the dialect table has no {dialect} cell for {kind}/{variant}"))
}

/// Compare the whole ledger against the declarations and the named allowances.
fn judge(dialect: &str, ledger: &[(String, String, Verdict)]) {
    assert_eq!(
        ledger.len(),
        DIALECT_TABLE.len(),
        "{dialect}: every dialect-table row must have been asked; the corpus and the \
         table are in bijection (dialect_table_faithfulness.rs) so a short ledger \
         means a row was skipped silently"
    );

    let mut unexplained: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let mut used: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
    let mut not_executable: BTreeSet<(String, String)> = BTreeSet::new();

    // The sentinel sweep. Independent of the disposition rule: a row can AGREE with
    // its declaration and still hand the operator an internal placeholder.
    let mut sentinels: BTreeSet<(String, String)> = BTreeSet::new();
    for (kind, variant, verdict) in ledger {
        if !verdict.detail.contains(NO_REASON_SENTINEL) {
            continue;
        }
        sentinels.insert((kind.clone(), variant.clone()));
        if !PLACEHOLDER_REASONS
            .iter()
            .any(|row| row.kind == kind && row.variant == variant && row.dialect == dialect)
        {
            unexplained.push(format!(
                "  {kind}/{variant} [{dialect}] refuses with the INTERNAL sentinel \
                 {NO_REASON_SENTINEL:?} instead of an operator-facing reason. A cell was \
                 declared `unsupported` in dialect-support.toml without adding the matching \
                 arm to op_support.rs::unsupported_reason."
            ));
        }
    }
    for pinned in PLACEHOLDER_REASONS
        .iter()
        .filter(|row| row.dialect == dialect)
    {
        if !sentinels.contains(&(pinned.kind.to_string(), pinned.variant.to_string())) {
            stale.push(format!(
                "  {}/{} [{dialect}] is pinned as showing the internal sentinel but no \
                 longer does. Remove the pin. ({})",
                pinned.kind, pinned.variant, pinned.why
            ));
        }
    }

    for (kind, variant, verdict) in ledger {
        if verdict.outcome == Outcome::NotExecutable {
            not_executable.insert((kind.clone(), variant.clone()));
            let pinned = NOT_EXECUTABLE
                .iter()
                .any(|row| row.kind == kind && row.variant == variant && row.dialect == dialect);
            if !pinned {
                unexplained.push(format!(
                    "  {kind}/{variant} [{dialect}] could not be executed and is not pinned \
                     in NOT_EXECUTABLE: {}",
                    verdict.detail
                ));
            }
            continue;
        }

        let declared = disposition_for(kind, variant, dialect);
        let required = required_outcome(declared);
        // The `!= ServerError` conjunct is belt to the const guard's braces, and it
        // is the WEAKER of the two: `required_outcome` never returns `ServerError`,
        // so it can only ever be redundant here. What keeps a `ServerError` from
        // being excused is that no allowance can name one - the const-eval block
        // above the tests. Without that, this line only stopped a `ServerError`
        // reading as AGREEMENT and let it fall through to the allowance lookup.
        if verdict.outcome == required && verdict.outcome != Outcome::ServerError {
            // Agreement. An allowance for an agreeing row is dead weight.
            if let Some(allowance) = ALLOWANCES
                .iter()
                .find(|a| a.kind == kind && a.variant == variant && a.dialect == dialect)
            {
                stale.push(format!(
                    "  {kind}/{variant} [{dialect}] now AGREES with its {declared:?} \
                     declaration, but an allowance still claims {}: remove it. ({})",
                    allowance.observed.token(),
                    allowance.why
                ));
            }
            continue;
        }

        match ALLOWANCES
            .iter()
            .find(|a| a.kind == kind && a.variant == variant && a.dialect == dialect)
        {
            Some(allowance) => {
                used.insert((allowance.kind, allowance.variant, allowance.dialect));
                assert_eq!(
                    verdict.outcome,
                    allowance.observed,
                    "{kind}/{variant} [{dialect}]: the allowance names {} but the run \
                     produced {}. An allowance must NAME the other value, so a changed \
                     value is a failure, not a silent update. Detail: {}",
                    allowance.observed.token(),
                    verdict.outcome.token(),
                    verdict.detail
                );
                assert!(
                    verdict.detail.contains(allowance.words),
                    "{kind}/{variant} [{dialect}]: the allowance pins the words {:?} but \
                     the run said {:?}",
                    allowance.words,
                    verdict.detail
                );
            }
            None => unexplained.push(format!(
                "  {kind}/{variant} [{dialect}] declares {declared:?} (requires {}) but the \
                 server produced {}: {}",
                required.token(),
                verdict.outcome.token(),
                verdict.detail
            )),
        }
    }

    for allowance in ALLOWANCES
        .iter()
        .filter(|a| a.dialect == dialect)
        .filter(|a| !used.contains(&(a.kind, a.variant, a.dialect)))
    {
        if not_executable.contains(&(allowance.kind.to_string(), allowance.variant.to_string())) {
            continue;
        }
        stale.push(format!(
            "  {}/{} [{dialect}] has an allowance that no row used. ({})",
            allowance.kind, allowance.variant, allowance.why
        ));
    }

    for pinned in NOT_EXECUTABLE.iter().filter(|row| row.dialect == dialect) {
        if !not_executable.contains(&(pinned.kind.to_string(), pinned.variant.to_string())) {
            stale.push(format!(
                "  {}/{} [{dialect}] is pinned NOT_EXECUTABLE but the run executed it. \
                 Remove the pin. ({})",
                pinned.kind, pinned.variant, pinned.why
            ));
        }
    }

    assert!(
        unexplained.is_empty() && stale.is_empty(),
        "\n{dialect}: the dialect table and the live server disagree.\n\n\
         UNEXPLAINED ({}):\n{}\n\nSTALE ({}):\n{}\n",
        unexplained.len(),
        unexplained.join("\n"),
        stale.len(),
        stale.join("\n"),
    );
}

/// Every `zmconf_%` schema currently in `pg_namespace`, and the whole-catalog
/// count beside it. The catalog is the sweep's own database's, so the prefixed list
/// is exactly the probe schemas the sweep still holds.
async fn probe_schemas(session: &PgDevSession) -> (i64, Vec<String>) {
    let total: i64 = session
        .query_one("SELECT count(*)::bigint AS n FROM pg_namespace", &[])
        .await
        .expect("count pg_namespace")
        .try_get::<_, i64>("n")
        .expect("decode the pg_namespace count");
    let rows = session
        .query(
            "SELECT nspname FROM pg_namespace WHERE nspname LIKE $1 ORDER BY nspname",
            &[zeroship_migrate::driver::Bind::Text(format!("{PROBE_PREFIX}%"))],
        )
        .await
        .expect("query pg_namespace for probe schemas");
    let prefixed = rows
        .iter()
        .filter_map(|row| row.try_get::<_, String>("nspname").ok())
        .collect();
    (total, prefixed)
}

/// The name the leak checks plant to prove they can see a leftover at all.
const LEAK_CONTROL: &str = "zmconf_leak_control";

#[compio::test]
async fn every_postgres_row_of_the_dialect_table_answers_to_a_live_server() {
    let url = crate::support::pg_database();
    let session = PgDevSession::connect(&url);

    // The leak check's rejection control, before it is relied on: a planted probe
    // schema has to show up in the census, or an empty census at the end proves
    // nothing about the rows.
    session
        .batch(&format!("CREATE SCHEMA \"{LEAK_CONTROL}\""))
        .await
        .expect("plant the leak check's control schema");
    let (_, planted) = probe_schemas(&session).await;
    assert_eq!(
        planted,
        vec![LEAK_CONTROL.to_string()],
        "the census must see a probe schema that is there, and nothing else yet"
    );
    session
        .batch(&format!("DROP SCHEMA \"{LEAK_CONTROL}\""))
        .await
        .expect("drop the leak check's control schema");

    let (before_total, _) = probe_schemas(&session).await;
    let mut ledger: Vec<(String, String, Verdict)> = Vec::new();
    for (kind, variant, op) in crate::dialect_corpus::corpus() {
        let verdict = pg_verdict(&url, kind, variant, &op).await;
        ledger.push((kind.to_string(), variant.to_string(), verdict));
    }

    // A step of the sweep rather than its own `#[test]`: a check whose subject is
    // another test's in-progress state has to be sequenced with it.
    let (after_total, leaked) = probe_schemas(&session).await;
    println!(
        "LEDGER postgres pg_namespace before={before_total} after={after_total} \
         probe_schemas_after={}",
        leaked.len(),
    );
    assert!(
        leaked.is_empty(),
        "this suite creates two schemas per row and drops both, but pg_namespace still \
         holds {}: {:?}. A leaked probe schema means a row's guard did not run.",
        leaked.len(),
        leaked,
    );

    report("postgres", &ledger);
    judge("postgres", &ledger);
    let _ = MYSQL_LEG;
}

#[compio::test]
async fn every_mysql_row_of_the_dialect_table_answers_to_a_live_server() {
    let url = crate::support::mysql::mysql_url();
    let session = MysqlDevSession::connect(&url);

    // Name the server, in the ledger, before anything is measured: a ledger that
    // does not name its scope cannot be trusted for the run it describes.
    println!("LEDGER mysql SERVER version={}", session.server_version());

    // The same rejection control the PostgreSQL sweep plants, over
    // `information_schema.SCHEMATA`.
    {
        let _control = DatabaseGuard::arm(&session, [LEAK_CONTROL]);
        session
            .batch(&format!("CREATE DATABASE {}", quote_ident(LEAK_CONTROL)))
            .await
            .expect("plant the leak check's control database");
        let (_, planted) = mysql_probe_databases(&session).await;
        assert!(
            planted.iter().any(|name| name == LEAK_CONTROL),
            "the census must see a probe database that is there; saw {planted:?}"
        );
    }

    let (before_total, before) = mysql_probe_databases(&session).await;
    assert!(
        before.is_empty(),
        "no probe database may be on the server before the sweep creates one: {before:?}"
    );
    let mut ledger: Vec<(String, String, Verdict)> = Vec::new();
    for (kind, variant, op) in crate::dialect_corpus::corpus() {
        let verdict = mysql_verdict(&url, kind, variant, &op).await;
        ledger.push((kind.to_string(), variant.to_string(), verdict));
    }

    let (after_total, leaked) = mysql_probe_databases(&session).await;
    println!(
        "LEDGER mysql schemata before={before_total} after={after_total} \
         probe_databases_after={}",
        leaked.len(),
    );
    assert!(
        leaked.is_empty(),
        "this suite creates a probe database and its `_migrations` meta database per \
         row and drops both, but information_schema still holds {}: {:?}. A leaked \
         probe database means a row's guard did not run.",
        leaked.len(),
        leaked,
    );

    report("mysql", &ledger);
    judge("mysql", &ledger);
}

#[compio::test]
async fn every_sqlite_row_of_the_dialect_table_answers_to_a_live_database() {
    let mut ledger: Vec<(String, String, Verdict)> = Vec::new();
    for (kind, variant, op) in crate::dialect_corpus::corpus() {
        let verdict = sqlite_verdict(kind, variant, &op).await;
        ledger.push((kind.to_string(), variant.to_string(), verdict));
    }
    report("sqlite", &ledger);
    judge("sqlite", &ledger);
}

/// The ledger, printed so `--nocapture` gives the whole picture rather than the
/// first failure.
fn report(dialect: &str, ledger: &[(String, String, Verdict)]) {
    let mut applied = 0usize;
    let mut refused = 0usize;
    let mut other = 0usize;
    for (kind, variant, verdict) in ledger {
        match verdict.outcome {
            Outcome::Applied => applied += 1,
            Outcome::RefusedByCapability => refused += 1,
            _ => other += 1,
        }
        println!(
            "LEDGER {dialect}\t{kind}\t{variant}\t{}\t{}",
            verdict.outcome.token(),
            verdict.detail.replace('\n', " ")
        );
    }
    println!(
        "LEDGER {dialect} TOTAL rows={} applied={applied} refusedByCapability={refused} other={other}",
        ledger.len()
    );
}
