//! LAYER 1 of the backend conformance kit: the dialect table's declarations,
//! answered by a live server.
//!
//! `dialect_table_faithfulness.rs` proves the sidecar, the generated
//! `DIALECT_TABLE` and `Op::support()` agree with each other. All three are the
//! ENGINE'S OWN OPINION, and no connection is opened anywhere in that file. So a
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
//! (`docs/proposals/backend-conformance.md`, layer 1):
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
//! WHAT HAD TO CHANGE to make the corpus EXECUTABLE. The representatives were
//! built to be CONSTRUCTIBLE and to select a support branch. Nothing in them
//! assumes its referents exist, and several of them name objects that are unique
//! per DATABASE or per CLUSTER rather than per schema. Three additions, and they
//! are the whole of the delta:
//!
//!   1. A PRELUDE per row ([`prelude`]), itself authored as IR and applied through
//!      the same production path - never hand-written DDL. `dropIndex` needs the
//!      index; `validateConstraint` needs a NOT VALID constraint; `addConstraint`
//!      needs a target table with a matching key; `update t SET a = x` needs `a`
//!      and `x` to be the same type, which is why there is no single fixture table
//!      and the prelude is chosen per row.
//!   2. A LOCALIZATION of the names that are not schema-scoped ([`localize`]).
//!      A PostgreSQL ROLE is cluster-scoped and an EXTENSION is database-scoped,
//!      so the corpus's `r` and `citext` would collide with any test binary cargo
//!      happens to run in parallel - `drop_extension_rollback_pg.rs` says in its
//!      own header that it cannot share `citext` with a second creator. Roles and
//!      the `createSchema`/`dropSchema` name get a per-row unique suffix; the
//!      extension becomes `pgcrypto`, which no live test in the tree claims.
//!      `nextval`'s hard-coded `app` schema is retargeted at the probe schema,
//!      because otherwise a cross-schema POLICY refusal would mask the capability
//!      answer the row is asking about.
//!   3. A LIVE SCHEMA read back from the catalog after the prelude, so the subject
//!      op lowers against what actually exists rather than against
//!      `LiveSchema::default()`.
//!
//! ISOLATION. Every PostgreSQL row runs in its own schema pair (project + meta),
//! created and dropped per row, plus a per-row role and the one extension. The
//! PostgreSQL sweep counts `pg_namespace` before and after itself and fails if a
//! `zmconf_%` schema it is answerable for survives - inside the sweep, not beside
//! it, because a check whose subject is another test's in-flight state cannot be
//! its own `#[test]`. Every SQLite row runs in its own `TempDir`.
//!
//! LEAK CHECK. "Answerable for" is decided by the pid in the schema name, never by
//! the `zmconf_` prefix alone. One PostgreSQL instance is shared by every agent and
//! every gate run working in this tree, so at any moment a `zmconf_%` schema is as
//! likely to be a sibling run's live probe as it is to be a leak; a guard that reads
//! the first as the second fails suites that are working, which is what it used to
//! do. [`probe_owner`] sorts the prefix match three ways: this process's own pid is
//! always this run's leak; a foreign pid that is still running is a sibling
//! mid-flight and is ignored; a foreign pid that has exited leaked its schema and
//! still fails the run. Two limits are known and measured, both narrow:
//!
//!   - PID REUSE. A leak from a dead run whose pid has since been recycled by some
//!     unrelated live process reads as in-flight and is not reported until that
//!     process exits. Detection is delayed, never dropped - the schema keeps
//!     failing later runs until it is cleaned up. `/proc/sys/kernel/pid_max` is
//!     4194304 on the machine this suite is developed against, so a wrap takes
//!     millions of spawns. The reverse case is safe by construction: a run that
//!     inherits a leaker's pid reads the leftover as its own and fails loudly.
//!   - NON-LINUX. [`process_is_running`] reads `/proc`. On a target without it,
//!     every foreign pid reads as running, which keeps concurrent runs green and
//!     gives up only the cross-run half of the check. This run's own leftovers are
//!     still caught, because "is this pid mine" needs no `/proc`.
//!
//! SCOPE. PostgreSQL and SQLite. MySQL is deliberately absent: there is no live
//! MySQL Rust coverage in this tree at all (`ZERO_MIGRATE_MYSQL_URL` appears in one
//! non-test file), and adding it is a separate piece of work. What a MySQL author
//! must add is written down at [`MYSQL_TODO`].
//!
//! COST. This is a live suite with one schema round-trip per row. Its wall clock is
//! recorded in `docs/review-log.md`; it is gated on `ZERO_MIGRATE_TEST_PG_URL` for
//! the PostgreSQL half exactly like every other live suite here, so a database-free
//! `cargo test` pays only the SQLite half.

mod dialect_corpus;
#[macro_use]
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{json, Value};
use tempfile::TempDir;

use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::{ApplyError, LockMode};
use zero_migrate::driver::{DbError, SqlSession};
use zero_migrate::model::dialect_table::{Disposition, DIALECT_TABLE};
use zero_migrate::model::ir::Op;
use zero_migrate::render::lower::{
    IrGuardedLowerError, IrLowerError, LoadAndLowerGuardedError, LoweredArtifact,
};
use zero_migrate::{
    resolve_create_table_policy, Approval, DeclarativeApplyError, EffectivePolicy, EngineError,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine, MigrationIr,
    PostgresBackend, SqlDialect, SqliteBackend,
};

/// What a MySQL author has to add to make this a three-dialect suite. Recorded as
/// a constant so it is in the file a MySQL author will open, not only in a doc.
///
/// 1. A live MySQL DSN env var + skip macro in `tests/support/mod.rs`, in the shape
///    of `PG_URL_ENV` / `skip_if_no_pg!`, and a `MysqlDevSession` implementing
///    `driver::SqlSession` over a blocking client, in the shape of `PgDevSession`.
///    None of this exists: `ZERO_MIGRATE_MYSQL_URL` occurs in exactly one file in
///    `crates/`, and it is `src/apply/backend/mysql/mod.rs`, not a test.
/// 2. A `Probe` arm for MySQL. MySQL has no CREATE SCHEMA that is not a DATABASE,
///    so the per-row isolation unit is a throwaway DATABASE, and the `pg_namespace`
///    leak check becomes a `SHOW DATABASES` check.
/// 3. Per-row prelude review. [`prelude`] already takes the dialect, and the
///    PostgreSQL/SQLite split it performs (a trigger prelude is a function on PG
///    and a body on SQLite) is the same axis MySQL will need.
/// 4. `expected_outcome` needs nothing: it already reads `row.mysql` from the
///    generated table.
///
/// The one thing a MySQL author must NOT do is relax [`Outcome::ServerError`].
const MYSQL_TODO: &str = "see the module doc and this constant";

// ---------------------------------------------------------------------------
// Outcome classes
// ---------------------------------------------------------------------------

/// The layer-1 outcome vocabulary. Exactly one of these per (row, dialect).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
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
    const fn token(&self) -> &'static str {
        match self {
            Self::Applied => "Applied",
            Self::RefusedByCapability => "RefusedByCapability",
            Self::RefusedByPolicy => "RefusedByPolicy",
            Self::ServerError => "ServerError",
            Self::EngineError => "EngineError",
            Self::NotExecutable => "NotExecutable",
        }
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
const CAPABILITY_CODES: &[&str] = &[
    zero_migrate::CODE_UNSUPPORTED,
    zero_migrate::CODE_DIALECT_UNSUPPORTED,
    zero_migrate::CODE_EXPR_NOT_PORTABLE,
    zero_migrate::CODE_DIALECT_SCOPE_PGONLY,
    zero_migrate::CODE_PARTITION_COMPOSITE_KEY_UNSUPPORTED,
    zero_migrate::CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE,
    zero_migrate::CODE_PARTITION_HASH_DROP_UNDERIVABLE,
];

/// Validate codes that mean "the CHARTER did not authorize this", which is a
/// different question from what the dialect can do.
const POLICY_CODES: &[&str] = &[
    zero_migrate::model::validate::CODE_VENDOR_OP_DENIED,
    zero_migrate::model::validate::CODE_CROSS_SCHEMA,
    zero_migrate::model::validate::CODE_TABLE_SHAPE_POLICY,
    zero_migrate::model::validate::CODE_INVALID_SCHEMA_IDENT,
];

fn classify_lower(error: &LoadAndLowerGuardedError) -> Verdict {
    let detail = error.to_string();
    match error {
        LoadAndLowerGuardedError::Load(load) => {
            if let zero_migrate::model::load::IrLoadError::Validate(authoring) = load {
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
        | IrLowerError::MysqlAlterColumnUnsupported(_)
        | IrLowerError::SqliteSchemaUnsupported(_)
        | IrLowerError::SqliteRebuildOnly(_)
        | IrLowerError::VendorPgOnly(_)
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
        DeclarativeApplyError::Expand(zero_migrate::OnlineError::Apply(apply)) => apply,
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
    if let Some(sqlite) =
        source.downcast_ref::<zero_migrate::apply::backend::sqlite::SqliteActorError>()
    {
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
    dialect: SqlDialect,
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
    let author = IrAuthor::new(schema, OWNER, dialect, policy);
    let guard = GuardConfig::from_policy(policy.clone(), dialect);
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
fn table_other() -> Value {
    json!({
        "op": "createTable",
        "name": "other",
        "columns": [
            col("id", json!("bigInt"), false),
            col("x", json!("bigInt"), false),
            col("other_col", json!("text"), false),
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
        "language": "plpgsql", "body": "BEGIN RETURN NEW; END",
    })
}

/// The ops that must already have applied for this row's representative to have
/// its referents. Authored as IR and applied through the SAME production path -
/// never hand-written DDL.
fn prelude(kind: &str, variant: &str, dialect: SqlDialect, probe: &Names) -> Vec<Value> {
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
        | ("pgRaw", _)
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
                col("id", json!("text"), false),
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
            text(),
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
            text(),
            json!({ "op": "addConstraint", "table": "t",
                    "constraint": { "name": "c",
                                    "kind": { "kind": "unique", "columns": ["a"] } } }),
        ],
        ("validateConstraint", _) => vec![
            table_other(),
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
        ("dropTrigger", _) => match dialect {
            SqlDialect::Postgres => vec![
                text(),
                create_function(),
                json!({ "op": "createTrigger", "name": "tg", "table": "t",
                        "timing": "before", "events": ["insert"], "forEach": "row",
                        "action": { "kind": "executeFunction", "name": "f" } }),
            ],
            _ => vec![
                text(),
                json!({ "op": "createTrigger", "name": "tg", "table": "t",
                        "timing": "before", "events": ["insert"], "forEach": "row",
                        "action": { "kind": "body", "statements": [
                            { "stmt": "select", "expr": { "node": "colRef", "name": "x" } }] } }),
            ],
        },

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
        ("createIndex", _) => vec![text()],

        ("setColumnType", _) => vec![varchar()],
        ("setColumnDefault", "base") => vec![int()],
        ("setColumnDefault", "containerOrJson") => vec![jsonb()],
        ("setColumnDefault", "nextval") => vec![bigint(), create_sequence()],

        ("renameColumn", _) => vec![table_t_without_b()],

        ("addConstraint", "fkNonId") => vec![table_other(), text()],
        ("addConstraint", "unique" | "check") => vec![text()],
        ("addConstraint", "exclusion") => vec![text()],
        ("addConstraint", _) => vec![table_other(), bigint()],

        ("insert", "base") => vec![text()],
        ("insert", _) => vec![
            text(),
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

fn nonce(kind: &str, variant: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, Ordering::SeqCst);
    let slug: String = format!("{kind}_{variant}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let slug: String = slug.to_ascii_lowercase().chars().take(28).collect();
    format!("{}{slug}_{seq}", probe_prefix_for(std::process::id()))
}

/// The schema-name prefix every probe schema carries, so the leak check can find
/// them all with one `pg_namespace` predicate.
const PROBE_PREFIX: &str = "zmconf_";

/// The prefix a probe schema carries when the process whose id is `pid` created it.
///
/// The pid is the FIRST segment after [`PROBE_PREFIX`], not a middle one, for two
/// reasons. A row appends `_aux`, `_migrations` and `_role` to the base name, so
/// only the head of the name is stable; and PostgreSQL truncates an identifier
/// past 63 bytes from the tail, so only the head is guaranteed to survive.
fn probe_prefix_for(pid: u32) -> String {
    format!("{PROBE_PREFIX}{pid}_")
}

async fn pg_verdict(url: &str, kind: &str, variant: &str, op: &Op) -> Verdict {
    let session = PgDevSession::connect(url);
    let base = nonce(kind, variant);
    let probe = Names {
        schema: base.clone(),
        aux_schema: format!("{base}_aux"),
        role: format!("{base}_role"),
        extension: "pgcrypto",
    };
    let policy = support::operator_charter(&probe.schema);
    let cfg = ExecutorConfig::new(format!("prj_{base}"), &probe.schema, policy.clone());
    let _guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.pg.meta_schema.clone(),
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
        SqlDialect::Postgres,
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
    let _ = session
        .batch(&format!("DROP EXTENSION IF EXISTS {}", probe.extension))
        .await;
    verdict
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
    let journal: PathBuf = dir.path().join("probe.migrations.sqlite");
    let backend = match SqliteBackend::open(&app, &journal) {
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
        extension: "pgcrypto",
    };
    let policy = support::operator_charter(SQLITE_PROJECT);
    let cfg = ExecutorConfig::new(SQLITE_PROJECT, SQLITE_PROJECT, policy.clone());
    run_row(
        kind,
        variant,
        op,
        SqlDialect::Sqlite,
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

async fn run_row<B: MigrationBackend>(
    kind: &str,
    variant: &str,
    op: &Op,
    dialect: SqlDialect,
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
                if let Err(error) = MigrationEngine::new()
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
    //    SQLite additionally needs `sqlite_schemas`: its 12-step rebuild is authored
    //    from the SDK field maps, not from the catalog, so a catalog-only LiveSchema
    //    makes every rebuild-shaped op refuse with "the live table snapshot is
    //    incomplete" whatever the dialect table says. `engine::refresh_historical_live`
    //    folds the applied history for exactly this, and the prelude IS this row's
    //    history, so folding it here is the production derivation, not a shortcut.
    let mut live = match backend.snapshot_schema(cfg).await {
        Ok(snapshot) => LiveSchema::from_catalog_snapshot(snapshot, OWNER),
        Err(_) => LiveSchema::from_tables(BTreeSet::new()),
    };
    if dialect == SqlDialect::Sqlite && !prelude_ops.is_empty() {
        let history: Vec<Op> = prelude_ops
            .iter()
            .filter_map(|op| serde_json::from_value(op.clone()).ok())
            .collect();
        if let Ok(defs) =
            zero_migrate::fold_to_field_defs(&history, dialect, &cfg.project_schema, policy)
        {
            live.sqlite_schemas = defs;
        }
    }

    // 3. SUBJECT.
    let subject = localize(kind, op, probe);
    let is_dml = matches!(kind, "insert" | "update" | "delete" | "backfill");
    let source = envelope(&format!("{kind}_{variant}"), &[subject], is_dml);
    let verdict = match lower(&source, &cfg.project_schema, policy, dialect, &live) {
        Err(verdict) => verdict,
        Ok(artifact) => match MigrationEngine::new()
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
/// This check exists because of a MEASURED limit on layer 1 as the proposal
/// specifies it. `Op::support()` READS the dialect table, so a cell declared
/// `unsupported` makes validate refuse on the table's own say-so: the required
/// outcome `RefusedByCapability` is satisfied BY CONSTRUCTION, and the check
/// cannot fail. Demonstrated by flipping `dropTable/base` on PostgreSQL - an op
/// PostgreSQL obviously supports - to `unsupported`: the suite stayed GREEN.
///
/// What that flip DID change is the operator's message, which became the sentinel
/// above. So the message is the one observable that a too-conservative declaration
/// still moves, and pinning it recovers a real check from a tautological cell.
struct PlaceholderReason {
    kind: &'static str,
    variant: &'static str,
    dialect: &'static str,
    why: &'static str,
}

include!("dialect_conformance/expectations.rs");

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

fn disposition_for(kind: &str, variant: &str, dialect: &str) -> Disposition {
    let row = DIALECT_TABLE
        .iter()
        .find(|row| row.kind == kind && row.variant == variant)
        .unwrap_or_else(|| panic!("no DIALECT_TABLE row for {kind}/{variant}"));
    match dialect {
        "postgres" => row.postgres,
        "sqlite" => row.sqlite,
        other => panic!("this suite covers postgres and sqlite, not {other}"),
    }
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

/// Who a `zmconf_%` schema in `pg_namespace` belongs to.
///
/// The prefix alone cannot answer this. Every agent working in this tree points at
/// ONE PostgreSQL instance, so a `zmconf_%` schema is as likely to be a sibling
/// run's live probe as it is to be a leak, and reading the first as the second
/// fails a suite that is working correctly. The name carries the pid of the process
/// that created it, which is the evidence that separates the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOwner {
    /// THIS process created it. No other process can produce this name, so it is
    /// unconditionally this run's to answer for.
    Mine,
    /// Another process created it and that process is still running: a sibling
    /// suite mid-flight, whose schemas are not this run's to judge.
    SiblingInFlight,
    /// No running process can still drop it. A finished run leaked it.
    Abandoned,
}

/// The pid a probe schema's name carries, or `None` when there is no number where
/// [`probe_prefix_for`] puts one.
fn creating_pid(schema: &str) -> Option<u32> {
    schema
        .strip_prefix(PROBE_PREFIX)?
        .split('_')
        .next()?
        .parse()
        .ok()
}

/// Whether `pid` is still running, which is the only evidence available that a
/// probe schema is in flight rather than leaked.
///
/// `pg_namespace` is shared, but the pids in these names belong to the machine
/// running the suite, so `/proc` is the right authority for them. On a target
/// without `/proc` this answers "running", which keeps concurrent runs passing at
/// the cost of the cross-run half of the leak check - see the LEAK CHECK note in
/// the module header.
fn process_is_running(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    } else {
        true
    }
}

/// A binary built before the pid moved to the front of the name wrote it in the
/// middle (`zmconf_<slug>_<pid>_<seq>`). Such runs share this server right now, so
/// a live pid anywhere in a name this layout cannot parse still means "sibling in
/// flight". Delete this once no binary that old can still be running.
fn legacy_layout_is_in_flight(schema: &str) -> bool {
    schema
        .split('_')
        .filter_map(|segment| segment.parse::<u32>().ok())
        .any(process_is_running)
}

/// Judge one `zmconf_%` schema on behalf of the process whose id is `me`.
fn probe_owner(schema: &str, me: u32) -> ProbeOwner {
    match creating_pid(schema) {
        Some(pid) if pid == me => ProbeOwner::Mine,
        Some(pid) if process_is_running(pid) => ProbeOwner::SiblingInFlight,
        Some(_) => ProbeOwner::Abandoned,
        None if legacy_layout_is_in_flight(schema) => ProbeOwner::SiblingInFlight,
        None => ProbeOwner::Abandoned,
    }
}

/// The `zmconf_%` schemas the catalog holds, split by [`ProbeOwner`].
struct ProbeCensus {
    mine: Vec<String>,
    sibling_in_flight: Vec<String>,
    abandoned: Vec<String>,
}

impl ProbeCensus {
    fn take(schemas: Vec<String>, me: u32) -> Self {
        let mut census = Self {
            mine: Vec::new(),
            sibling_in_flight: Vec::new(),
            abandoned: Vec::new(),
        };
        for schema in schemas {
            match probe_owner(&schema, me) {
                ProbeOwner::Mine => census.mine.push(schema),
                ProbeOwner::SiblingInFlight => census.sibling_in_flight.push(schema),
                ProbeOwner::Abandoned => census.abandoned.push(schema),
            }
        }
        census
    }

    /// Every schema no running process can still drop. Both buckets are leaks; they
    /// are reported apart because they accuse different runs.
    fn leaked(&self) -> bool {
        !self.mine.is_empty() || !self.abandoned.is_empty()
    }
}

/// Re-read the catalog and keep only the `candidates` still there.
///
/// [`ProbeCensus`] reads `pg_namespace` and then reads `/proc`, and a sibling can
/// finish between the two: its rows are already in hand while its pid is already
/// gone, which reads as abandoned. That sibling dropped its schemas on its way out,
/// so a second look tells a stale row from a real leak.
async fn still_in_the_catalog(session: &PgDevSession, candidates: &[String]) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let (_, present) = probe_schemas(session).await;
    candidates
        .iter()
        .filter(|candidate| present.contains(candidate))
        .cloned()
        .collect()
}

/// Every `zmconf_%` schema currently in `pg_namespace`, and the whole-catalog
/// count beside it. The database is shared with every other live suite in this
/// crate, so the global count moves for reasons that are not this file's; the
/// prefixed list is the one [`ProbeCensus`] then sorts into this run's and others'.
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
            &[zero_migrate::driver::Bind::Text(format!("{PROBE_PREFIX}%"))],
        )
        .await
        .expect("query pg_namespace for probe schemas");
    let prefixed = rows
        .iter()
        .filter_map(|row| row.try_get::<_, String>("nspname").ok())
        .collect();
    (total, prefixed)
}

#[compio::test]
async fn every_postgres_row_of_the_dialect_table_answers_to_a_live_server() {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return;
    };
    let session = PgDevSession::connect(&url);

    // The isolation claim is VERIFIED, not asserted, and it is verified HERE rather
    // than in its own `#[test]`. It used to be its own test, and libtest ran it
    // concurrently with this sweep: it observed
    // `zmconf_alterprimarykey_base_..._1` - the FIRST row's schema, mid-flight -
    // and reported a leak that was a live probe. A check whose subject is another
    // test's in-progress state has to be sequenced with it, so it is a step of the
    // sweep.
    //
    // The census is by pid, not by prefix. `zmconf_%` alone answers "somebody on
    // this server is or was running this suite", and every agent in this tree points
    // at one instance: a sibling's live probe schema read as a leak fails a run that
    // is working. What a leak actually is - a schema no running process can still
    // drop - is what the pid in the name settles.
    let me = std::process::id();
    let (before_total, before_prefixed) = probe_schemas(&session).await;
    let mut before = ProbeCensus::take(before_prefixed, me);
    before.abandoned = still_in_the_catalog(&session, &before.abandoned).await;
    assert!(
        !before.leaked(),
        "probe schemas are on the server that no running process can still drop:\n  \
         left by THIS process ({me}): {:?}\n  \
         left by a process that has since exited: {:?}\n\
         ({} more carry the pid of a process that is still running - a sibling suite \
         mid-flight - and are not this run's to judge.)",
        before.mine,
        before.abandoned,
        before.sibling_in_flight.len(),
    );

    let mut ledger: Vec<(String, String, Verdict)> = Vec::new();
    for (kind, variant, op) in dialect_corpus::corpus() {
        let verdict = pg_verdict(&url, kind, variant, &op).await;
        ledger.push((kind.to_string(), variant.to_string(), verdict));
    }

    // This half of the check needs no liveness reasoning: `me` is running by
    // definition, so anything still carrying this pid is this run's own leak.
    let (after_total, after_prefixed) = probe_schemas(&session).await;
    let after = ProbeCensus::take(after_prefixed, me);
    println!(
        "LEDGER postgres pg_namespace before={before_total} after={after_total} \
         probe_schemas_mine_before={} probe_schemas_mine_after={} \
         probe_schemas_sibling_in_flight_after={}",
        before.mine.len(),
        after.mine.len(),
        after.sibling_in_flight.len(),
    );
    assert!(
        after.mine.is_empty(),
        "this suite creates two schemas per row and drops both, but pg_namespace still \
         holds {} of this process's own ({me}): {:?}. A leaked probe schema means a row's \
         guard did not run, and a run that inherits this pid will fail its CREATE SCHEMA \
         against it.",
        after.mine.len(),
        after.mine,
    );

    report("postgres", &ledger);
    judge("postgres", &ledger);
    let _ = MYSQL_TODO;
}

/// The leak check reads the pid in the name, not just the prefix - measured against
/// a real `pg_namespace` row and a real process, in all three directions.
///
/// The schema this plants carries a pid that is ALIVE and is not this process's, so
/// the sweep above - which may be running concurrently in this same binary - reads
/// it as a sibling in flight and ignores it, exactly as it would a real sibling's.
/// A schema carrying a DEAD pid is deliberately never put on the shared server: that
/// is a real leak by every run's reckoning, and planting one would fail a sibling
/// that is working. The dead-pid verdict is measured on the same name string once
/// its process has actually been reaped.
#[compio::test]
async fn a_probe_schema_is_judged_by_the_pid_in_its_name_not_by_its_prefix() {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return;
    };
    let session = PgDevSession::connect(&url);
    let me = std::process::id();

    // A real, running, foreign process. `sleep` outlives every assertion that needs
    // it alive, and is reaped before the one that needs it dead.
    let mut sibling = std::process::Command::new("sleep")
        .arg("120")
        .spawn()
        .expect("spawn a real sibling process to own a probe schema");
    let sibling_pid = sibling.id();
    assert_ne!(sibling_pid, me, "the sibling has to be a different process");
    assert!(
        process_is_running(sibling_pid),
        "the sibling was just spawned; it has to read as running"
    );

    let sibling_schema = format!("{}leakcheck_0", probe_prefix_for(sibling_pid));
    let mine_schema = format!("{}leakcheck_0", probe_prefix_for(me));

    {
        let _guard = support::SchemaGuard::arm(&session, [sibling_schema.clone()]);
        session
            .batch(&format!("CREATE SCHEMA \"{sibling_schema}\""))
            .await
            .expect("create the sibling's probe schema");

        // The real catalog read the sweep uses, over a real row.
        let (_, prefixed) = probe_schemas(&session).await;
        assert!(
            prefixed.contains(&sibling_schema),
            "the census must see the schema it is being asked about; saw {prefixed:?}"
        );

        let census = ProbeCensus::take(prefixed, me);
        assert!(
            census.sibling_in_flight.contains(&sibling_schema),
            "a running foreign pid's schema is a sibling in flight, not a leak"
        );
        assert!(
            !census.mine.contains(&sibling_schema),
            "another process's schema is never this run's"
        );
        assert!(
            !census.abandoned.contains(&sibling_schema),
            "its creator is still running, so nothing has been abandoned"
        );
    }

    // Scoping by pid must not blind the guard to this run's OWN leftovers.
    assert_eq!(
        probe_owner(&mine_schema, me),
        ProbeOwner::Mine,
        "a schema carrying this process's pid is this run's own leak"
    );
    assert!(
        ProbeCensus::take(vec![mine_schema.clone()], me).leaked(),
        "a census holding this run's own leftover has to read as leaked"
    );

    // ... nor to a schema whose creator has exited, which is what a leak from a
    // finished run looks like.
    sibling.kill().expect("stop the sibling process");
    sibling.wait().expect("reap the sibling process");
    assert!(
        !process_is_running(sibling_pid),
        "the sibling was killed and reaped; it has to read as gone"
    );
    assert_eq!(
        probe_owner(&sibling_schema, me),
        ProbeOwner::Abandoned,
        "once its creator has exited, a probe schema is a leak to every run"
    );
    assert!(
        ProbeCensus::take(vec![sibling_schema], me).leaked(),
        "a census holding a finished run's leftover has to read as leaked"
    );
}

#[compio::test]
async fn every_sqlite_row_of_the_dialect_table_answers_to_a_live_database() {
    let mut ledger: Vec<(String, String, Verdict)> = Vec::new();
    for (kind, variant, op) in dialect_corpus::corpus() {
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
