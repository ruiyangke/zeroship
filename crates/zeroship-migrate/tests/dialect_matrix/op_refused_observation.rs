//! LAYER 2 of the backend conformance kit, the SECOND observation: `op_refused`, and
//! the ORACLE that compares its value ACROSS backends.
//!
//! # What it observes
//!
//! `op_refused(op) -> Option<reason_class>` - "does a posture mean the same thing on
//! every target". One op stream is driven through the PRODUCTION apply path against a
//! live PostgreSQL, a live MySQL and SQLite, and the answer is the REASON CLASS of the
//! refusal, or `None` when the op applied.
//!
//! Layer 1 ([`crate::dialect_conformance_live`]) already records an outcome class per
//! corpus row, but it does so under ONE charter - `support::operator_charter`, which
//! grants `safety.destructive_ops = "allow"`. Every refusal a POSTURE could cause is
//! therefore invisible to it by construction. This file is the other axis: one op, two
//! postures, three backends.
//!
//! # Why this is not `destructive-ops-dialect-parity.test.ts` again
//!
//! `packages/zero-migrate-cli/tests/host/destructive-ops-dialect-parity.test.ts` is
//! the defect this observation is seeded from, and it is a good test. Two things here
//! are different, and both are the point.
//!
//! 1. **Its three targets never meet.** Each target asserts its own CLI exit code
//!    against a constant - `assert.equal(dropped.code, 1)`, `assert.equal(made.code,
//!    0)`. Three tests asserting `a == K`, `b == K`, `c == K` do prove `a == b == c`,
//!    but only for arms whose right answer someone already knew how to write down. The
//!    [`oracle`] below needs no such constant: it takes the three values and requires
//!    them to be EQUAL TO EACH OTHER, so a case nobody has an expected value for is
//!    still a conformance test.
//! 2. **It observes an exit code and a substring.** Its content check is
//!    `assert.match(dropped.text, /destructive/i)` over merged stdout+stderr, which is
//!    a check on WORDING. The value here is [`ReasonClass`]: layer 1's outcome token
//!    plus the stable rule id the refusal named. Both are `&'static str` constants out
//!    of neutral crates, so two backends refusing for the same reason in different
//!    words produce the SAME value - see [`rule_of`], which deliberately drops the
//!    `dialect` field `GuardError::RawSqlRejected` carries.
//!
//! # The three arms, and why two of them exist only to stop a false pass
//!
//! Carried over from the host test, which records why each is there:
//!
//! - [`Subject::DropTable`] under [`Posture::Default`] - the policy never mentions the
//!   knob, so the registry default applies. REFUSED, and the table is still there.
//! - [`Subject::BoundedUpdate`] under [`Posture::Default`] - APPLIED. Without this the
//!   file passes on a build that refuses far too much: the first version of the
//!   original fix reused `Op::is_destructive`, the APPROVAL notion, which includes row
//!   DML, and made two dialects refuse an `update` PostgreSQL allows.
//! - [`Subject::DropTable`] under [`Posture::Allow`] - APPLIED, and the table is gone.
//!   Without this every default-posture arm passes on a build where drops never work
//!   at all.
//!
//! # The observation is corroborated by the SERVER, not just by the engine
//!
//! A refusal class read off an engine error is the engine's own opinion, which is the
//! gap layer 1's header names about the dialect table. [`corroborate`] closes it: after
//! the subject op is attempted, the leg READS the database and requires the server's
//! state to agree with the class it just recorded. A leg that reports a refusal but
//! whose table vanished, or reports `None` but whose row never moved, fails on the
//! spot. Every arm therefore ends in a live read on all three backends.
//!
//! # The saturation check, and why it is the right analogue
//!
//! `row_order_observation.rs` FAILS rather than skips on a server that cannot
//! distinguish its two fixtures, because every claim would then hold vacuously. The
//! equivalent question here has two halves and both are answered by failing:
//!
//! - OFFLINE, [`the_two_postures_this_file_contrasts_are_different`]: the two charters
//!   must actually resolve to different `destructive_ops` postures. If the registry
//!   default ever became `allow`, or the `Allow` charter stopped composing, every arm
//!   below would still "pass" while measuring one posture twice.
//! - LIVE, inside each leg: an `op_refused` refusal is decided BEFORE any SQL reaches
//!   the server, so a refusal arm would pass against a database that was never touched
//!   at all. Each leg therefore asserts, after the setup and before the subject, that
//!   the target object exists and the seed row is present. A leg whose setup did not
//!   reach the server cannot answer this observation and says so.
//!
//! There is no skip path anywhere here: a missing DSN panics in `require_live_pg!` /
//! `require_live_mysql!`, for the reason `support::require_live_db_dsn` records.
//!
//! # The live red, and what building it found
//!
//! [`op_refused_still_disagrees_across_the_three_backends_on_an_unclassified_drop`] is
//! the LIVE red, and it is a REAL residual gap rather than a constructed one. The
//! defect this observation is seeded from was fixed by
//! `zeroship_migrate_backend::guard::check_ir_data_security_policy`, a neutral walk over
//! the structured ops that refuses `Op::is_destructive` minus row DML and raw. The
//! parser-backed guard refuses on a different question: it refuses anything its
//! classifier cannot POSITIVELY vouch for as non-destructive
//! (`DataSecurityClass::Unknown`). Those two rules do not coincide, and `dropTrigger`
//! is where they part:
//!
//! - `Op::DropTrigger` is not `Op::is_destructive`, so the neutral walk lets it past;
//! - `DROP TRIGGER` parses to a `DropStmt` whose `remove_type` is neither in the
//!   destructive drop table nor in the non-destructive allowlist (only an INDEX drop
//!   is), so the parser-backed classifier answers `Unknown`.
//!
//! MEASURED offline before this file existed, by running the parser-backed
//! classifier over the statements this fixture emits: `DROP TRIGGER` -> `Unknown`,
//! `DROP TABLE` -> `Destructive("DROP TABLE")`, `DROP INDEX` -> `NonDestructive`,
//! `CREATE TRIGGER ... EXECUTE FUNCTION` -> `NonDestructive`. So under the DEFAULT
//! posture PostgreSQL refuses a `dropTrigger` that MySQL and SQLite apply - the same
//! shape as the original defect, with the dialects on the other side of it.
//!
//! `dropTrigger` is believed to be the ONLY op with that property, and the belief is a
//! READING rather than an executed census, so here is the method to re-run it. An op
//! splits the two rules exactly when it is (a) declared for all three dialects, (b) not
//! `Op::is_destructive`, and (c) not in the parser's non-destructive allowlist. Walking
//! `Op::is_destructive`'s own non-destructive arm against
//! `is_non_destructive_statement`, every other member renders to a statement that
//! allowlist names, and the remaining `Unknown` producers (`dropExtension`,
//! `dropPolicy`) are PostgreSQL-only, so they never reach a second backend to disagree
//! with. If a future op is added, that walk is what has to be redone.
//!
//! IF THIS RED EVER GOES GREEN the gap closed, which is good news and a REQUIRED edit,
//! not a licence to delete the test: replace the fixture with whatever still splits
//! the two rules, or record that nothing does. A red that silently becomes vacuous is
//! the failure mode this whole layer exists to stop.
//!
//! # The red was MEASURED, by mutation, and every arm has its own
//!
//! `row_order_observation.rs` records that its FIRST mutation was a false green,
//! because it flipped the one leg its fixture could not move. The check that avoids
//! that shape here is per-ARM rather than per-leg: an arm whose failure could only
//! come from a mutation another arm already catches is redundant coverage, so each was
//! mutated separately and each went red on its own, with the split NAMED:
//!
//! - Inverting the neutral walk's gate in
//!   `zeroship_migrate_backend::guard::check_ir_data_security_policy` restores the
//!   original defect exactly - `["postgres"] -> RefusedByPolicy` against `["mysql",
//!   "sqlite"] -> None`, which is the review entry's own sentence. Arm 1 red, the
//!   other three green.
//! - Widening `posture_denies` back to bare `Op::is_destructive` restores the
//!   over-block regression - `["postgres"] -> None` against `["mysql", "sqlite"] ->
//!   RefusedByPolicy`. Arm 1b red, the other three green.
//! - Making the walk ignore the posture VALUE denies under `allow` too. Arm 2 red, the
//!   other three green.
//! - Adding `ObjectTrigger` to the parser's non-destructive allowlist closes the
//!   residual gap, all three answer `None`, and the LIVE RED goes red with the "either
//!   the gap closed or the harness is not asking three servers" message it promises.
//!
//! Two of the three backends move under each of the first three mutations and the
//! third moves alone, so no arm here is passing on a leg that cannot move.
//!
//! # What this observation CANNOT see
//!
//! The oracle's verdict is "the three backends agree", never "the three backends are
//! right", exactly as `row_order_observation.rs` records. Each arm here happens to
//! have a knowable right answer, so the agreeing value is additionally required to be
//! it.
//!
//! It also cannot see a posture that is enforced at a DIFFERENT layer with the same
//! net effect. `op_refused` is a function of what the engine did, and two backends
//! that both refuse - one at the guard, one at the server - agree here even though an
//! operator's experience of them differs. Layer 1's `ServerError` class is what makes
//! that distinction, and this file refuses to record one at all: a server error is a
//! broken fixture, and [`observe_subject`] turns it into a failure rather than a
//! value.
//!
//! # One oracle written twice
//!
//! [`oracle`] is the same rule `row_order_observation::oracle` states, over a
//! different value type. Extracting one generic oracle both observations share is the
//! natural next step and it is deliberately NOT taken here: that file's module header
//! explains its own oracle in place, including why the proposal's `[[case.differs]]`
//! seam is absent, and a mechanical extraction would rewrite claims this change did
//! not measure. Recorded as a finding rather than done quietly.

use crate::support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tempfile::TempDir;

use crate::dialect_conformance_live::Outcome;
use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use crate::support::PgDevSession;
use zeroship_migrate::apply::backend::MigrationBackend;
use zeroship_migrate::apply::executor::LockMode;
use zeroship_migrate::driver::SqlSession;
use zeroship_migrate::guard::{data_security_rule, GuardConfig, GuardError};
use zeroship_migrate::model::ir::Op;
use zeroship_migrate::model::load::IrLoadError;
use zeroship_migrate::model::policy::DestructiveOps;
use zeroship_migrate::render::fold::single_fold;
use zeroship_migrate::render::lower::{IrGuardedLowerError, IrLowerError, LoadAndLowerGuardedError};
use zeroship_migrate::{
    resolve_create_table_policy, Approval, DialectId, EffectivePolicy, ExecutorConfig, IrAuthor,
    LiveSchema, MigrationEngine, MigrationIr,
};
use zeroship_migrate_mysql::MysqlBackend;
use zeroship_migrate_postgres::PostgresBackend;
use zeroship_migrate_sqlite::SqliteBackend;

const OWNER: &str = "app_op_refused";

/// The table every arm operates on, the trigger the live red drops, and the auxiliary
/// table MySQL's trigger body needs a target for.
const TABLE: &str = "t";
const AUX_TABLE: &str = "t2";
const TRIGGER: &str = "tg";
const FUNCTION: &str = "f";

/// The seed row, and what a bounded update must move it to.
///
/// Named rather than written inline because [`corroborate`] reads them back off the
/// server and the whole point is that the two are distinguishable.
const SEED_ID: i64 = 1;
const SEED_VAL: i64 = 1;
const BUMPED_VAL: i64 = 2;

// ---------------------------------------------------------------------------
// The observed value
// ---------------------------------------------------------------------------

/// The refusal reason class - LAYER 1's vocabulary, not a parallel one.
///
/// `class` is taken FROM [`Outcome`] by calling its own `token()`, so the two layers
/// cannot drift into two vocabularies that happen to agree today. `rule` is the stable
/// rule id the refusal named, which is a `&'static str` const out of a neutral crate
/// (`zeroship_migrate_backend::guard::data_security_rule`) and is identical on every
/// dialect that raises it.
///
/// `rule` is what makes an arm's claim specific. Without it every policy refusal
/// collapses to one value, and a cross-schema denial would satisfy an arm about the
/// destructive posture - the failure the host test guards against with its
/// `/destructive/i` content check, done by CONSTANT rather than by substring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReasonClass {
    class: &'static str,
    rule: &'static str,
}

/// Layer 1's own tokens. Spelling `"RefusedByPolicy"` here instead would be the
/// parallel vocabulary this reuse exists to avoid.
const BY_POLICY: &str = Outcome::RefusedByPolicy.token();
const BY_CAPABILITY: &str = Outcome::RefusedByCapability.token();

/// The refusal named no stable rule id of its own.
const RULE_UNNAMED: &str = "(no rule id)";

impl ReasonClass {
    const fn policy(rule: &'static str) -> Self {
        Self {
            class: BY_POLICY,
            rule,
        }
    }
    const fn capability(rule: &'static str) -> Self {
        Self {
            class: BY_CAPABILITY,
            rule,
        }
    }
}

/// One backend's answer to `op_refused` on one case. `None` means the op APPLIED.
type Refusal = Option<ReasonClass>;

/// The neutral rule id a guard refusal named.
///
/// `RawSqlRejected` carries the refusing `dialect`, and it is dropped ON PURPOSE: two
/// backends with no raw door refuse for the SAME reason and must produce the same
/// value, or the oracle measures which vendor spoke rather than what it decided. The
/// same reasoning is why no arm of this function reaches for the error's `Display`.
fn rule_of(error: &GuardError) -> &'static str {
    match error {
        GuardError::Denied { rule, .. }
        | GuardError::DataSecurityPolicy { rule, .. }
        | GuardError::NamespacePolicy { rule, .. } => rule,
        GuardError::CrossSchema { .. } => "GUARD_CROSS_SCHEMA",
        GuardError::Parse(_) => "GUARD_PARSE",
        GuardError::RawSqlRejected { .. } => "GUARD_RAW_SQL_REJECTED",
    }
}

/// Classify a guarded-load failure into a reason class, or report that it is not one.
///
/// `Err` is never a value: an engine error is a broken fixture, and the caller turns
/// it into a failure naming the backend. That is what keeps this observation's value
/// space to exactly "applied, or one of layer 1's two refusal classes".
fn classify(error: &LoadAndLowerGuardedError) -> Result<ReasonClass, String> {
    match error {
        LoadAndLowerGuardedError::Load(IrLoadError::Validate(authoring)) => {
            let code = authoring.code.as_str();
            if crate::dialect_conformance_live::CAPABILITY_CODES.contains(&code) {
                Ok(ReasonClass::capability(leaked_static(code)))
            } else if crate::dialect_conformance_live::POLICY_CODES.contains(&code) {
                Ok(ReasonClass::policy(leaked_static(code)))
            } else {
                Err(format!(
                    "validate code {code} is not a reason class: {error}"
                ))
            }
        }
        LoadAndLowerGuardedError::Lower(IrGuardedLowerError::Denied(denied)) => {
            Ok(ReasonClass::policy(rule_of(&denied.source)))
        }
        LoadAndLowerGuardedError::Lower(IrGuardedLowerError::Lower(lower)) => match lower {
            IrLowerError::VendorCapabilityDenied { .. }
            | IrLowerError::DefaultSchemaOutOfScope(_)
            | IrLowerError::LowerCrossSchema(_) => Ok(ReasonClass::policy(RULE_UNNAMED)),
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
                Ok(ReasonClass::capability(RULE_UNNAMED))
            }
            other => Err(format!("lower error is not a reason class: {other}")),
        },
        other => Err(format!("this failure is not a reason class: {other}")),
    }
}

/// A validate code is already a stable id, but it arrives borrowed from the error.
/// Interning it keeps [`ReasonClass`] `Copy` and comparable by pointer-free equality.
fn leaked_static(code: &str) -> &'static str {
    Box::leak(code.to_string().into_boxed_str())
}

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

/// One backend's answer to one observation on one case.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observed {
    backend: &'static str,
    value: Refusal,
}

/// The ORACLE. Default rule: for one case and one observation, every backend must
/// produce the same value.
///
/// `Ok(value)` is the agreed value; `Err(report)` NAMES the split, grouped by value,
/// so the failure says which backends disagreed with which. A differential whose
/// failure message is "not equal" costs a debugging session per occurrence.
fn oracle(observations: &[Observed]) -> Result<Refusal, String> {
    assert!(
        observations.len() >= 2,
        "a differential over fewer than two backends is not a differential"
    );
    let mut by_value: Vec<(Refusal, Vec<&'static str>)> = Vec::new();
    for observed in observations {
        if let Some(entry) = by_value.iter_mut().find(|(v, _)| *v == observed.value) {
            entry.1.push(observed.backend);
        } else {
            by_value.push((observed.value, vec![observed.backend]));
        }
    }
    if by_value.len() == 1 {
        return Ok(by_value.remove(0).0);
    }
    let mut report = format!(
        "{} backends produced {} different values for this observation:",
        observations.len(),
        by_value.len()
    );
    for (value, backends) in &by_value {
        report.push_str(&format!("\n  {backends:?} -> {value:?}"));
    }
    Err(report)
}

// ---------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------

/// Whether the charter MENTIONS `safety.destructive_ops`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Posture {
    /// The policy never names the knob, so the registry default applies. This is the
    /// case an operator gets by writing nothing, and it is not the same thing as
    /// granting the default value explicitly.
    Default,
    /// `safety.destructive_ops = "allow"`.
    Allow,
}

impl Posture {
    const fn grant(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Allow => Some("allow"),
        }
    }
    const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Allow => "allow",
        }
    }
}

/// The op the observation is taken over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subject {
    /// A `dropTable`, which every dialect's rule agrees is destructive.
    DropTable,
    /// A WHERE-bounded `update`, which is row DML and which the posture must NOT
    /// cover - PostgreSQL applies it, so anything stricter is a regression.
    BoundedUpdate,
    /// A `dropTrigger`. See the module header: this is where the neutral walk and the
    /// parser-backed classifier genuinely part company.
    DropTrigger,
}

impl Subject {
    const fn label(self) -> &'static str {
        match self {
            Self::DropTable => "drop_table",
            Self::BoundedUpdate => "bounded_update",
            Self::DropTrigger => "drop_trigger",
        }
    }
    /// Whether the subject is DML, which decides which envelope it rides in: the
    /// loader refuses DML mixed with DDL, and refuses forward DML that declares
    /// neither `inverse_ops` nor `irreversible`.
    const fn is_dml(self) -> bool {
        matches!(self, Self::BoundedUpdate)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Case {
    posture: Posture,
    subject: Subject,
}

// ---------------------------------------------------------------------------
// The fixture, authored through the PUBLIC path
// ---------------------------------------------------------------------------

fn table_t() -> Value {
    json!({
        "op": "createTable", "name": TABLE,
        "columns": [
            { "name": "id", "type": "bigInt", "nullable": false },
            { "name": "val", "type": "int", "nullable": false },
        ],
        "primaryKey": ["id"], "constraints": [], "indexes": [],
    })
}

/// The tables the case needs before its seed row lands.
///
/// MySQL's trigger body needs a DELETE target that is not the trigger's own table, so
/// the auxiliary table is created here rather than beside the trigger.
fn table_ops(subject: Subject, dialect: &DialectId) -> Vec<Value> {
    let mut ops = vec![table_t()];
    if subject == Subject::DropTrigger && dialect == &zeroship_migrate_mysql::DIALECT {
        ops.push(json!({
            "op": "createTable", "name": AUX_TABLE,
            "columns": [
                { "name": "id", "type": "bigInt", "nullable": false },
                { "name": "flag", "type": "boolean", "nullable": true },
            ],
            "primaryKey": ["id"], "constraints": [], "indexes": [],
        }));
    }
    ops
}

/// The trigger the live red drops, and it is created AFTER the seed row.
///
/// The order was measured, not chosen. MySQL refuses structured DML against a table
/// that carries a trigger - "zero-migrate cannot prove transactional side effects, so
/// structured data migrations fail closed" - so a fixture that created the trigger
/// first could never seed its row there, and the leg died before observing anything.
///
/// The grammar genuinely differs three ways: PostgreSQL EXECUTES A FUNCTION, SQLite
/// and MySQL carry a BODY, and MySQL refuses a body that returns a result set, so its
/// body deletes from an auxiliary table instead. Those three shapes are
/// `dialect_conformance_live::prelude`'s, measured there against all three servers.
///
/// The SUBJECT op is byte-identical on all three dialects in every case; only the
/// setup splits. A subject that differed per dialect would be three observations
/// wearing one name.
fn trigger_ops(subject: Subject, dialect: &DialectId) -> Vec<Value> {
    let mut ops: Vec<Value> = Vec::new();
    if subject != Subject::DropTrigger {
        return ops;
    }
    if dialect == &zeroship_migrate_postgres::DIALECT {
        ops.push(json!({
            "op": "createFunction", "name": FUNCTION, "returns": "trigger",
            "language": "procedural", "body": "BEGIN RETURN NEW; END",
        }));
        ops.push(json!({
            "op": "createTrigger", "name": TRIGGER, "table": TABLE,
            "timing": "before", "events": ["insert"], "forEach": "row",
            "action": { "kind": "executeFunction", "name": FUNCTION },
        }));
    } else if dialect == &zeroship_migrate_mysql::DIALECT {
        ops.push(json!({
            "op": "createTrigger", "name": TRIGGER, "table": TABLE,
            "timing": "before", "events": ["insert"], "forEach": "row",
            "action": { "kind": "body", "statements": [
                { "stmt": "delete", "table": AUX_TABLE,
                  "where": { "node": "colRef", "name": "flag" } }] },
        }));
    } else if dialect == &zeroship_migrate_sqlite::DIALECT {
        ops.push(json!({
            "op": "createTrigger", "name": TRIGGER, "table": TABLE,
            "timing": "before", "events": ["insert"], "forEach": "row",
            "action": { "kind": "body", "statements": [
                { "stmt": "select", "expr": { "node": "colRef", "name": "val" } }] },
        }));
    } else {
        panic!("unregistered test dialect {dialect}");
    }
    ops
}

/// The subject op. One op, one envelope, identical on every dialect.
fn subject_op(subject: Subject) -> Value {
    match subject {
        Subject::DropTable => json!({ "op": "dropTable", "table": TABLE }),
        Subject::DropTrigger => {
            json!({ "op": "dropTrigger", "name": TRIGGER, "table": TABLE })
        }
        Subject::BoundedUpdate => json!({
            "op": "update", "table": TABLE,
            "set": { "val": { "node": "literal", "value": BUMPED_VAL } },
            "where": { "node": "binOp", "op": "gt",
                       "lhs": { "node": "colRef", "name": "id" },
                       "rhs": { "node": "literal", "value": 0 } },
        }),
    }
}

/// Wrap ops in an envelope. DML envelopes carry `irreversible`, because an `update`
/// has no recorded inverse and the loader refuses a reversible envelope containing
/// one - the same split `row_order_observation.rs` authors.
fn envelope(name: &str, ops: &[Value], dml: bool) -> String {
    let reason = if dml {
        r#""irreversible":"op_refused fixture: DML has no recorded inverse","#
    } else {
        ""
    };
    let ops = serde_json::to_string(ops).expect("fixture ops serialize");
    format!(r#"{{"ir_version":1,"name":"{name}",{reason}"ops":{ops}}}"#)
}

fn registry() -> BTreeMap<String, String> {
    BTreeMap::from([
        (TABLE.to_string(), OWNER.to_string()),
        (AUX_TABLE.to_string(), OWNER.to_string()),
    ])
}

// ---------------------------------------------------------------------------
// The production apply path
// ---------------------------------------------------------------------------

/// Author + lower one envelope through the production guarded path, then apply it
/// through `MigrationEngine`.
///
/// This is the sequence layer 1's `run_row` and `row_order_observation`'s
/// `apply_envelope` both use: resolve the charter's create-table policy,
/// `load_and_lower_guarded`, `apply_plan`. Nothing here hands SQL to `session.batch`.
///
/// The guarded-load failure is returned SEPARATELY from the apply failure, because
/// they are different observations: the first is a refusal and has a reason class, the
/// second is a server or engine error and is a broken fixture.
async fn apply_envelope<B: MigrationBackend>(
    source: &str,
    tag: &str,
    backend: &B,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    dialect: &DialectId,
    live: &LiveSchema,
) -> Result<Result<(), LoadAndLowerGuardedError>, String> {
    let authored: MigrationIr = serde_json::from_str(source)
        .map_err(|error| format!("{tag}: the fixture envelope did not parse: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, policy, &cfg.project_schema)
        .map_err(|error| format!("{tag}: resolve create-table policy: {error}"))?;
    let source = serde_json::to_string(&resolved)
        .map_err(|error| format!("{tag}: re-serialize the resolved envelope: {error}"))?;
    let artifact = match IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        &cfg.project_schema,
        OWNER,
        dialect,
        policy,
    )
    .load_and_lower_guarded(
        &source,
        OWNER,
        &registry(),
        live,
        &GuardConfig::from_policy(policy.clone(), dialect.clone()),
    ) {
        Ok(artifact) => artifact,
        Err(error) => return Ok(Err(error)),
    };
    MigrationEngine::new(zeroship_migrate::shipping_vendors())
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            backend,
            cfg,
            tag,
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("{tag}: apply: {error}"))?;
    Ok(Ok(()))
}

/// The live schema the next envelope lowers against, read back from the CATALOG.
///
/// An op that lowers against a schema the server does not have is answering a
/// different question, which is layer 1's finding. SQLite additionally needs
/// `sdk_schemas`, folded from the DDL that just applied - which IS this stream's own
/// history.
async fn live_schema_now<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    dialect: &DialectId,
    history: &[Op],
) -> Result<LiveSchema, String> {
    let snapshot = backend
        .snapshot_schema(cfg)
        .await
        .map_err(|error| format!("read the applied schema back: {error}"))?;
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);
    if dialect == &zeroship_migrate_sqlite::DIALECT {
        if let Ok(defs) = single_fold::fold(
            zeroship_migrate::shipping_vendors(),
            history,
            dialect,
            &cfg.project_schema,
            policy,
        )
        .map(|folded| folded.project_field_defs(zeroship_migrate::shipping_vendors()))
        {
            live.sdk_schemas = defs;
        }
    }
    Ok(live)
}

/// Apply the setup - the table, whatever the case's subject needs to exist, and the
/// seed row - under the CASE'S OWN posture.
///
/// The posture is the case's rather than a permissive one on purpose: the host test
/// creates its table under the same policy it later drops it under, and a setup run
/// under a different charter would be a different migration history.
///
/// Any failure here is a BROKEN FIXTURE, never an observation. A setup that a posture
/// refuses would silently turn every arm into a claim about the setup.
async fn apply_setup<B: MigrationBackend>(
    case: Case,
    backend: &B,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    dialect: &DialectId,
) -> Result<Vec<Op>, String> {
    let tables = table_ops(case.subject, dialect);
    let tables_source = envelope("op_refused_setup_tables", &tables, false);
    apply_envelope(
        &tables_source,
        "op-refused-setup-tables",
        backend,
        cfg,
        policy,
        dialect,
        &LiveSchema::default(),
    )
    .await?
    .map_err(|error| format!("the setup DDL was refused, so this fixture is broken: {error}"))?;

    let mut history: Vec<Op> = serde_json::from_str::<MigrationIr>(&tables_source)
        .map_err(|error| format!("re-parse the setup DDL: {error}"))?
        .ops;
    let live = live_schema_now(backend, cfg, policy, dialect, &history).await?;

    let seed = envelope(
        "op_refused_setup_dml",
        &[json!({
            "op": "insert", "table": TABLE, "columns": ["id", "val"],
            "rows": [[SEED_ID, SEED_VAL]],
        })],
        true,
    );
    apply_envelope(
        &seed,
        "op-refused-setup-dml",
        backend,
        cfg,
        policy,
        dialect,
        &live,
    )
    .await?
    .map_err(|error| format!("the seed row was refused, so this fixture is broken: {error}"))?;

    let triggers = trigger_ops(case.subject, dialect);
    if !triggers.is_empty() {
        let triggers_source = envelope("op_refused_setup_triggers", &triggers, false);
        let live = live_schema_now(backend, cfg, policy, dialect, &history).await?;
        apply_envelope(
            &triggers_source,
            "op-refused-setup-triggers",
            backend,
            cfg,
            policy,
            dialect,
            &live,
        )
        .await?
        .map_err(|error| {
            format!("the setup trigger was refused, so this fixture is broken: {error}")
        })?;
        history.extend(
            serde_json::from_str::<MigrationIr>(&triggers_source)
                .map_err(|error| format!("re-parse the setup trigger DDL: {error}"))?
                .ops,
        );
    }
    Ok(history)
}

/// Apply the SUBJECT op and record the observation.
///
/// A server or engine failure is turned into `Err` rather than into a value: layer 1's
/// `ServerError` is always a conformance failure, and a `ServerError` recorded as an
/// `op_refused` value would read as "this backend refused" when the truth is "this
/// backend tried and died".
async fn observe_subject<B: MigrationBackend>(
    case: Case,
    backend: &B,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    dialect: &DialectId,
    history: &[Op],
) -> Result<Refusal, String> {
    let live = live_schema_now(backend, cfg, policy, dialect, history).await?;
    let source = envelope(
        "op_refused_subject",
        &[subject_op(case.subject)],
        case.subject.is_dml(),
    );
    match apply_envelope(
        &source,
        "op-refused-subject",
        backend,
        cfg,
        policy,
        dialect,
        &live,
    )
    .await?
    {
        Ok(()) => Ok(None),
        Err(error) => classify(&error).map(Some),
    }
}

// ---------------------------------------------------------------------------
// The server's corroboration
// ---------------------------------------------------------------------------

/// What one backend answered, plus what its server says afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Leg {
    refusal: Refusal,
    /// Does the object the subject op targets still exist - the table for a
    /// `dropTable`, the trigger for a `dropTrigger`?
    target_exists: bool,
    /// The seed row's `val`, or `None` when the row is gone.
    val: Option<i64>,
}

/// Require the SERVER to agree with the class the engine reported.
///
/// This is what makes the observation observable from outside rather than a reading of
/// the engine's own error type. Each case states both directions, so a leg cannot pass
/// by refusing and dropping, or by applying and doing nothing.
fn corroborate(subject: Subject, leg: &Leg) -> Result<(), String> {
    let refused = leg.refusal.is_some();
    match subject {
        Subject::DropTable => {
            if refused && !(leg.target_exists && leg.val == Some(SEED_VAL)) {
                return Err(format!(
                    "the drop was refused, so the table and its row must both survive; got {leg:?}"
                ));
            }
            if !refused && (leg.target_exists || leg.val.is_some()) {
                return Err(format!(
                    "the drop applied, so the table must be gone; got {leg:?}"
                ));
            }
        }
        Subject::BoundedUpdate => {
            let want = if refused { SEED_VAL } else { BUMPED_VAL };
            if !leg.target_exists || leg.val != Some(want) {
                return Err(format!(
                    "an update never drops the table, and the row must read {want} for a \
                     refused={refused} leg; got {leg:?}"
                ));
            }
        }
        Subject::DropTrigger => {
            if leg.val != Some(SEED_VAL) {
                return Err(format!("a trigger drop never touches the row; got {leg:?}"));
            }
            if refused != leg.target_exists {
                return Err(format!(
                    "the trigger must survive exactly when the drop was refused; got {leg:?}"
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-backend drivers: isolate, set up, observe, read back
// ---------------------------------------------------------------------------

/// A per-process-unique name. `zmoprefused_` rather than layer 1's `zmconf_`, because
/// that suite's leak census claims every `zmconf_%` name it can attribute to a dead
/// pid, and these are not its to judge.
fn token(case: Case) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "zmoprefused_{}_{}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst),
        case.subject.label(),
        case.posture.label()
    )
}

async fn pg_leg(url: &str, case: Case) -> Result<Leg, String> {
    let session = PgDevSession::connect(url);
    let schema = token(case);
    let policy = support::operator_charter_with_destructive_ops(&schema, case.posture.grant());
    let cfg = ExecutorConfig::new(format!("prj_{schema}"), &schema, policy.clone());
    let _guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA \"{}\"", cfg.project_schema))
        .await
        .map_err(|error| format!("create the probe schema: {error}"))?;
    let backend = PostgresBackend::new_generic(&session);
    backend
        .ensure_journal(&cfg)
        .await
        .map_err(|error| format!("create the journal: {error}"))?;

    let dialect = zeroship_migrate_postgres::DIALECT;
    let history = apply_setup(case, &backend, &cfg, &policy, &dialect).await?;

    let count = |sql: String| {
        let session = &session;
        async move {
            let row = session
                .query_one(&sql, &[])
                .await
                .map_err(|error| format!("read the server back: {error}"))?;
            row.try_get::<_, i64>(0)
                .map_err(|error| format!("count did not decode: {error}"))
        }
    };
    let table_sql = format!(
        "SELECT count(*) FROM information_schema.tables \
         WHERE table_schema = '{schema}' AND table_name = '{TABLE}'"
    );
    let trigger_sql = format!(
        "SELECT count(*) FROM pg_trigger tg \
           JOIN pg_class c ON c.oid = tg.tgrelid \
           JOIN pg_namespace ns ON ns.oid = c.relnamespace \
          WHERE NOT tg.tgisinternal AND ns.nspname = '{schema}' AND tg.tgname = '{TRIGGER}'"
    );
    let val_sql =
        format!("SELECT count(*) FROM \"{schema}\".\"{TABLE}\" WHERE id = {SEED_ID} AND val = ");

    let target_sql = match case.subject {
        Subject::DropTrigger => trigger_sql,
        _ => table_sql.clone(),
    };
    require_the_setup_reached_the_server(
        "postgres",
        count(target_sql.clone()).await? == 1,
        count(format!("{val_sql}{SEED_VAL}")).await? == 1,
    )?;

    let refusal = observe_subject(case, &backend, &cfg, &policy, &dialect, &history).await?;

    let target_exists = count(target_sql).await? == 1;
    let val = if count(table_sql).await? == 1 {
        let seed = count(format!("{val_sql}{SEED_VAL}")).await?;
        let bumped = count(format!("{val_sql}{BUMPED_VAL}")).await?;
        read_one_val(seed, bumped)?
    } else {
        None
    };
    Ok(Leg {
        refusal,
        target_exists,
        val,
    })
}

async fn mysql_leg(url: &str, case: Case) -> Result<Leg, String> {
    let session = MysqlDevSession::connect(url);
    let database = token(case);
    let policy = support::operator_charter_with_destructive_ops(&database, case.posture.grant());
    let cfg = ExecutorConfig::new(format!("prj_{database}"), &database, policy.clone());
    let _guard = DatabaseGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!(
            "CREATE DATABASE {}",
            quote_ident(&cfg.project_schema)
        ))
        .await
        .map_err(|error| format!("create the probe database: {error}"))?;
    let backend = MysqlBackend::new_generic(&session);
    backend
        .ensure_journal(&cfg)
        .await
        .map_err(|error| format!("create the journal: {error}"))?;

    let dialect = zeroship_migrate_mysql::DIALECT;
    let history = apply_setup(case, &backend, &cfg, &policy, &dialect).await?;

    let count = |sql: String| {
        let session = &session;
        async move {
            let row = session
                .query_one(&sql, &[])
                .await
                .map_err(|error| format!("read the server back: {error}"))?;
            row.try_get::<_, i64>(0)
                .map_err(|error| format!("count did not decode: {error}"))
        }
    };
    let table_sql = format!(
        "SELECT COUNT(*) FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = '{database}' AND TABLE_NAME = '{TABLE}'"
    );
    let trigger_sql = format!(
        "SELECT COUNT(*) FROM information_schema.TRIGGERS \
         WHERE TRIGGER_SCHEMA = '{database}' AND TRIGGER_NAME = '{TRIGGER}'"
    );
    let quoted = quote_ident(&database);
    let val_sql =
        format!("SELECT COUNT(*) FROM {quoted}.`{TABLE}` WHERE id = {SEED_ID} AND val = ");

    let target_sql = match case.subject {
        Subject::DropTrigger => trigger_sql,
        _ => table_sql.clone(),
    };
    require_the_setup_reached_the_server(
        "mysql",
        count(target_sql.clone()).await? == 1,
        count(format!("{val_sql}{SEED_VAL}")).await? == 1,
    )?;

    let refusal = observe_subject(case, &backend, &cfg, &policy, &dialect, &history).await?;

    let target_exists = count(target_sql).await? == 1;
    let val = if count(table_sql).await? == 1 {
        let seed = count(format!("{val_sql}{SEED_VAL}")).await?;
        let bumped = count(format!("{val_sql}{BUMPED_VAL}")).await?;
        read_one_val(seed, bumped)?
    } else {
        None
    };
    Ok(Leg {
        refusal,
        target_exists,
        val,
    })
}

const SQLITE_PROJECT: &str = "prj_op_refused";

async fn sqlite_leg(case: Case) -> Result<Leg, String> {
    let dir: TempDir =
        tempfile::tempdir().map_err(|error| format!("create a temp dir: {error}"))?;
    let app: PathBuf = dir.path().join("probe.sqlite");
    let journal: PathBuf = dir.path().join("probe.migrations.sqlite");
    let backend = SqliteBackend::open(&app, &journal)
        .map_err(|error| format!("open the probe database: {error}"))?;
    let policy =
        support::operator_charter_with_destructive_ops(SQLITE_PROJECT, case.posture.grant());
    let cfg = ExecutorConfig::new(SQLITE_PROJECT, SQLITE_PROJECT, policy.clone());

    let dialect = zeroship_migrate_sqlite::DIALECT;
    let history = apply_setup(case, &backend, &cfg, &policy, &dialect).await?;

    let table_sql =
        format!("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '{TABLE}'");
    let trigger_sql =
        format!("SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name = '{TRIGGER}'");
    let val_sql = format!("SELECT count(*) FROM \"{TABLE}\" WHERE id = {SEED_ID} AND val = ");
    let target_sql = match case.subject {
        Subject::DropTrigger => trigger_sql,
        _ => table_sql.clone(),
    };

    require_the_setup_reached_the_server(
        "sqlite",
        sqlite_count(&app, &target_sql)? == 1,
        sqlite_count(&app, &format!("{val_sql}{SEED_VAL}"))? == 1,
    )?;

    let refusal = observe_subject(case, &backend, &cfg, &policy, &dialect, &history).await?;

    let target_exists = sqlite_count(&app, &target_sql)? == 1;
    let val = if sqlite_count(&app, &table_sql)? == 1 {
        let seed = sqlite_count(&app, &format!("{val_sql}{SEED_VAL}"))?;
        let bumped = sqlite_count(&app, &format!("{val_sql}{BUMPED_VAL}"))?;
        read_one_val(seed, bumped)?
    } else {
        None
    };
    Ok(Leg {
        refusal,
        target_exists,
        val,
    })
}

/// SQLite is read through a SEPARATE connection on the app FILE, not through the
/// backend's own hardened actor. An observation an operator could not run by hand is
/// not an observation, and the actor's authorizer modes are the engine's business.
fn sqlite_count(app: &Path, sql: &str) -> Result<i64, String> {
    let conn = rusqlite::Connection::open(app)
        .map_err(|error| format!("reopen the app file raw: {error}"))?;
    conn.query_row(sql, [], |row| row.get::<_, i64>(0))
        .map_err(|error| format!("read the app file back: {error}"))
}

/// Turn the two count probes into the seed row's `val`.
fn read_one_val(seed: i64, bumped: i64) -> Result<Option<i64>, String> {
    match (seed, bumped) {
        (1, 0) => Ok(Some(SEED_VAL)),
        (0, 1) => Ok(Some(BUMPED_VAL)),
        (0, 0) => Ok(None),
        _ => Err(format!(
            "the seed row read back as neither {SEED_VAL} nor {BUMPED_VAL} \
             (seed={seed}, bumped={bumped})"
        )),
    }
}

/// FAIL, never skip: an `op_refused` refusal is decided before any SQL reaches the
/// server, so a leg whose setup never landed would still produce a refusal value and
/// read exactly like a working one.
fn require_the_setup_reached_the_server(
    backend: &str,
    target_exists: bool,
    seed_present: bool,
) -> Result<(), String> {
    if target_exists && seed_present {
        return Ok(());
    }
    Err(format!(
        "{backend}: the setup did not reach the server (target_exists={target_exists}, \
         seed_present={seed_present}), so nothing this leg reports about a refusal is \
         about a database"
    ))
}

/// Run one case on all three backends.
async fn observe(pg_url: &str, mysql_url: &str, case: Case) -> Vec<Observed> {
    let pg = pg_leg(pg_url, case)
        .await
        .unwrap_or_else(|error| panic!("postgres: {error}"));
    let mysql = mysql_leg(mysql_url, case)
        .await
        .unwrap_or_else(|error| panic!("mysql: {error}"));
    let sqlite = sqlite_leg(case)
        .await
        .unwrap_or_else(|error| panic!("sqlite: {error}"));
    println!(
        "LEDGER op_refused subject={} posture={} pg={pg:?} mysql={mysql:?} sqlite={sqlite:?}",
        case.subject.label(),
        case.posture.label()
    );
    for (backend, leg) in [("postgres", &pg), ("mysql", &mysql), ("sqlite", &sqlite)] {
        corroborate(case.subject, leg)
            .unwrap_or_else(|error| panic!("{backend}: the server contradicts the class: {error}"));
    }
    vec![
        Observed {
            backend: "postgres",
            value: pg.refusal,
        },
        Observed {
            backend: "mysql",
            value: mysql.refusal,
        },
        Observed {
            backend: "sqlite",
            value: sqlite.refusal,
        },
    ]
}

// ---------------------------------------------------------------------------
// The instrument checks
// ---------------------------------------------------------------------------

/// The oracle's mechanism check, offline. It has to separate the two cases and it has
/// to name the split, or the live red below would be indistinguishable from a stub
/// that always returns `Err`.
#[test]
fn the_oracle_separates_agreement_from_disagreement() {
    let forbidden = Some(ReasonClass::policy(
        data_security_rule::DESTRUCTIVE_OPS_FORBID,
    ));

    let agreed = oracle(&[
        Observed {
            backend: "postgres",
            value: forbidden,
        },
        Observed {
            backend: "mysql",
            value: forbidden,
        },
        Observed {
            backend: "sqlite",
            value: forbidden,
        },
    ])
    .expect("three equal values agree");
    assert_eq!(agreed, forbidden, "the agreed value is the value observed");

    let split = oracle(&[
        Observed {
            backend: "postgres",
            value: Some(ReasonClass::policy(
                data_security_rule::UNCLASSIFIED_OP_DENIED_UNDER_FORBID,
            )),
        },
        Observed {
            backend: "mysql",
            value: None,
        },
        Observed {
            backend: "sqlite",
            value: None,
        },
    ])
    .expect_err("a two-way split is a disagreement");
    assert!(
        split.contains("\"postgres\"") && split.contains("\"mysql\", \"sqlite\""),
        "the report must name WHICH backends took which value; got:\n{split}"
    );
    assert!(
        split.contains("2 different values"),
        "the report must count the distinct values; got:\n{split}"
    );

    // Two refusals that differ ONLY in their rule id are still a disagreement. Without
    // this the oracle would collapse every policy refusal to one value and an arm
    // about the destructive posture would be satisfied by any denial at all.
    let same_class = oracle(&[
        Observed {
            backend: "postgres",
            value: Some(ReasonClass::policy(
                data_security_rule::UNCLASSIFIED_OP_DENIED_UNDER_FORBID,
            )),
        },
        Observed {
            backend: "mysql",
            value: forbidden,
        },
    ])
    .expect_err("one class, two rule ids, is a disagreement");
    assert!(
        same_class.contains(data_security_rule::DESTRUCTIVE_OPS_FORBID),
        "the report must name the rules that split; got:\n{same_class}"
    );

    // And the capability class is a different value from the policy class even when
    // the rule id is the same, so the two halves of layer 1's vocabulary cannot be
    // confused for each other.
    assert_ne!(
        ReasonClass::policy(RULE_UNNAMED),
        ReasonClass::capability(RULE_UNNAMED),
        "RefusedByPolicy and RefusedByCapability are different observations"
    );
}

/// The SATURATION check, offline. The two postures this file contrasts must actually
/// be different, or every arm below measures one posture twice and holds vacuously.
///
/// FAIL rather than skip, for the reason `row_order_observation.rs` records about a
/// `C`/`POSIX` PostgreSQL: a vacuous green is the failure mode this layer exists to
/// stop. It is asserted on every dialect, because `GuardConfig` resolves the posture
/// from the composed policy and a dialect that resolved it differently would make the
/// arms below incomparable.
#[test]
fn the_two_postures_this_file_contrasts_are_different() {
    for dialect in [
        zeroship_migrate_postgres::DIALECT,
        zeroship_migrate_mysql::DIALECT,
        zeroship_migrate_sqlite::DIALECT,
    ] {
        for (posture, want) in [
            (Posture::Default, DestructiveOps::Forbid),
            (Posture::Allow, DestructiveOps::Allow),
        ] {
            let policy = support::operator_charter_with_destructive_ops("probe", posture.grant());
            let resolved = GuardConfig::from_policy(policy, dialect.clone()).destructive_ops();
            assert_eq!(
                resolved,
                want,
                "on {dialect}, the {} charter must resolve to {want:?}; a charter that \
                 does not makes every arm in this file a claim about one posture \
                 written twice",
                posture.label()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The arms
// ---------------------------------------------------------------------------

/// ARM 1. The default posture refuses a drop, identically, on all three backends.
#[compio::test]
async fn a_drop_is_refused_under_the_default_posture_on_every_backend() {
    let pg_url = require_live_pg!();
    let mysql_url = require_live_mysql!();
    let case = Case {
        posture: Posture::Default,
        subject: Subject::DropTable,
    };
    let observations = observe(&pg_url, &mysql_url, case).await;

    let agreed = oracle(&observations).unwrap_or_else(|report| {
        panic!(
            "the default destructive posture must mean the same thing on every target:\n{report}"
        )
    });

    // The differential proves AGREEMENT, never CORRECTNESS. Three backends agreeing on
    // "applied" would pass the line above. This arm's right answer is knowable, so it
    // is also asserted - and by the rule CONSTANT, not by a substring of the message.
    assert_eq!(
        agreed,
        Some(ReasonClass::policy(
            data_security_rule::DESTRUCTIVE_OPS_FORBID
        )),
        "the three backends agree, but not on a destructive-posture refusal"
    );
}

/// ARM 1b, the OVER-BLOCK regression arm. A WHERE-bounded row update APPLIES under the
/// default posture on every backend.
///
/// The posture covers object drops, not row DML. PostgreSQL applies this because DML
/// lowers to a bound `PlanStep::Dml` its SQL-text guard never inspects, and the neutral
/// IR walk excludes `Update` / `Delete` / `Backfill` to match. The first version of the
/// original fix did not, and made two dialects STRICTER than PostgreSQL.
///
/// [`corroborate`] additionally requires the row to have MOVED, so this cannot pass on
/// a build that reports success without applying anything.
#[compio::test]
async fn a_bounded_update_applies_under_the_default_posture_on_every_backend() {
    let pg_url = require_live_pg!();
    let mysql_url = require_live_mysql!();
    let case = Case {
        posture: Posture::Default,
        subject: Subject::BoundedUpdate,
    };
    let observations = observe(&pg_url, &mysql_url, case).await;

    let agreed = oracle(&observations).unwrap_or_else(|report| {
        panic!(
            "the destructive posture must cover the same OPS on every target, and row \
             DML is not one of them:\n{report}"
        )
    });
    assert_eq!(
        agreed, None,
        "a bounded update must apply under the default posture, as it does on PostgreSQL"
    );
}

/// ARM 2, the control. An explicit `allow` applies the very same drop, so arm 1 is
/// about the POSTURE and not about drops being broken.
#[compio::test]
async fn the_same_drop_applies_under_an_explicit_allow_on_every_backend() {
    let pg_url = require_live_pg!();
    let mysql_url = require_live_mysql!();
    let case = Case {
        posture: Posture::Allow,
        subject: Subject::DropTable,
    };
    let observations = observe(&pg_url, &mysql_url, case).await;

    let agreed = oracle(&observations).unwrap_or_else(|report| {
        panic!("an explicit allow must mean the same thing on every target:\n{report}")
    });
    assert_eq!(
        agreed, None,
        "an explicit allow must let the drop through, or arm 1 is measuring a broken drop"
    );
}

/// The LIVE RED, and the cross-check for the three arms above.
///
/// The default posture on a `dropTrigger`. See the module header for the census: the
/// neutral IR walk asks "is this op destructive", the parser-backed guard asks "can I
/// vouch for this statement as non-destructive", and `dropTrigger` is the one op
/// declared on all three dialects where those two questions differ. PostgreSQL refuses
/// it as UNCLASSIFIED; MySQL and SQLite apply it.
///
/// If this ever agrees, the residual gap closed. FIX THE FIXTURE, do not delete the
/// test - without a red that the oracle can SEE, the three arms above could all be
/// passing because the harness returns one value three times for a reason that has
/// nothing to do with the servers.
#[compio::test]
async fn op_refused_still_disagrees_across_the_three_backends_on_an_unclassified_drop() {
    let pg_url = require_live_pg!();
    let mysql_url = require_live_mysql!();
    let case = Case {
        posture: Posture::Default,
        subject: Subject::DropTrigger,
    };
    let observations = observe(&pg_url, &mysql_url, case).await;

    let report = oracle(&observations).err().unwrap_or_else(|| {
        panic!(
            "the same posture answered identically on all three backends for an op the \
             two enforcement rules classify differently, so either the gap closed - in \
             which case this fixture needs replacing - or the harness is not asking \
             three servers. Observations: {observations:?}"
        )
    });
    assert!(
        report.contains("\"postgres\"") && report.contains(BY_POLICY),
        "PostgreSQL must be the side that refuses; got:\n{report}"
    );
    assert!(
        report.contains(data_security_rule::UNCLASSIFIED_OP_DENIED_UNDER_FORBID),
        "and it must refuse because it could not CLASSIFY the statement, which is the \
         rule the neutral walk has no counterpart for; got:\n{report}"
    );

    let applied: Vec<&'static str> = observations
        .iter()
        .filter(|o| o.value.is_none())
        .map(|o| o.backend)
        .collect();
    assert_eq!(
        applied,
        vec!["mysql", "sqlite"],
        "the two backends whose only destructive-posture enforcement is the neutral IR \
         walk must both apply it, or this is a different split from the one named here"
    );
}
