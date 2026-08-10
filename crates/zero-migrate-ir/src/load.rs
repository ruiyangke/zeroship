//! The fail-closed **IR envelope load gate**.
//!
//! This is the SINGLE production seam every creator-authored IR envelope passes
//! through before the engine lowers it (`IrAuthor::lower`, the per-dialect DDL
//! compiler — a later wave) or checksums it. The gate is fail-closed and ordered
//! so a hostile / newer-engine / cross-tenant artifact is rejected BEFORE any
//! checksum or lowering runs:
//!
//! 1. **deserialize** the bytes into the typed [`MigrationIr`] — the closed `Op`/
//!    `Expr` AST + the constrained [`IrScalar`](crate::ir::IrScalar) numeric
//!    domain reject a malformed/lossy/unknown-node artifact at this step.
//! 2. **`ir_version` fail-closed** ([`MigrationIr::check_ir_version`]): a FUTURE
//!    wire-format version this engine build does not understand is refused.
//! 3. **structural validation** (`the structural validator`): the authoritative
//!    structural gate over EVERY embedded `Expr` slot for the deploy-target
//!    dialect — out-of-envelope `splitPart`, an unresolved `ColRef` in
//!    a self-contained `createTable`, a non-portable shape.
//! 4. **server-stamped ownership** ([`enforce_ir_ownership`]): `owner_app` is
//!    overwritten with the deploying app's id (a spoofed value is discarded), and
//!    every op targeting a table must resolve to the deploying app in the project
//!    ownership registry — a table absent from the registry FAILS CLOSED
//!    (mirroring the declarative drop-ownership check in `declarative.rs`).
//!    A `createTable` establishes ownership for its NEW table (the deploying app),
//!    exactly as the declarative union does.
//! 5. **advisory checksum-hint compare**: when the artifact carries
//!    the optional `checksum` hint, the engine RECOMPUTES the hint-domain checksum
//!    and a mismatch is a hard error (genuine drift / tamper). The engine is
//!    authoritative; the hint is advisory and need not be present.
//!
//! Between the structural validation and the ownership check the chain also runs
//! [`enforce_ir_finite_timeouts`], which refuses a `flags.timeout_ms` /
//! `flags.lock_timeout_ms` of `0` (the engines' "no limit" sentinel). That one is
//! author feedback rather than a security gate: apply enforces the same rule
//! where the effective budget is resolved, which is the boundary a hand-built
//! `Migration` or a config-sourced zero also crosses.
//!
//! Lowering the validated IR to an executable `zero_migrate::render::plan::AppliedPlan`
//! (`IrAuthor::lower`, the snapshot-builder + per-dialect DDL render) is the
//! next wave; this module is the load + gate that MUST run first.

use std::collections::BTreeMap;

use crate::ir::{IrVersionError, MigrationIr, Op, ViewQuery};
use crate::migration::{Checksum, MigrationFlags};
use crate::validate::AuthoringError;

/// A failure of the IR envelope load gate. Each variant maps to one ordered
/// fail-closed step.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IrLoadError {
    /// The bytes did not deserialize into a well-formed [`MigrationIr`] — a
    /// malformed JSON document, an unknown op/expr node tag, an out-of-domain
    /// numeric scalar, or a non-nullary synth default. Carries the
    /// serde message (which embeds the structured code, e.g.
    /// [`EXPR_INVALID_NUMERIC`](crate::ir::EXPR_INVALID_NUMERIC), for matching).
    #[error("malformed IR envelope: {0}")]
    Deserialize(String),
    /// The artifact declared a FUTURE `ir_version` this engine cannot interpret.
    /// Fail-closed.
    #[error(transparent)]
    Version(#[from] IrVersionError),
    /// An embedded expression-AST node failed the structural validator for the
    /// deploy-target dialect. Carries the authoring envelope.
    #[error(transparent)]
    Validate(#[from] AuthoringError),
    /// An op targets a table the deploying app does not own, OR a table absent
    /// from the project ownership registry (unknown-owner fail-closed).
    #[error(
        "ownership violation: op {op_index} targets table {table:?} owned by {owner} \
         (deploying app is {deploying_app:?}); an app may only migrate tables it owns, \
         and a table with no ownership-registry entry is refused fail-closed"
    )]
    NotTableOwner {
        /// The op whose target table is not owned by the deploying app.
        op_index: usize,
        /// The offending target table.
        table: String,
        /// The registered owner (or the unknown-owner sentinel).
        owner: String,
        /// The deploying app's id.
        deploying_app: String,
    },
    /// The artifact's advisory `checksum` hint did not match the engine's
    /// recomputed hint-domain checksum — genuine drift / tamper.
    #[error(
        "checksum hint mismatch: the IR envelope advisory hint {hint:?} does not match the \
         engine-recomputed hint-domain checksum {recomputed:?} (the op list / flags / deps \
         changed since the hint was stamped, or the artifact was tampered with)"
    )]
    ChecksumHintMismatch {
        /// The advisory hint the artifact carried.
        hint: String,
        /// The engine's authoritative recompute over the hint domain.
        recomputed: String,
    },
    /// The artifact carried an advisory `checksum` hint AND a field whose
    /// contribution to the hint domain this engine build cannot yet compute
    /// (a non-empty `depends_on`/`supersedes`, or a non-default `flags`
    /// override). The hint domain is
    /// `ops + flags + depends_on + supersedes + preconditions`, but the
    /// `IrFlagsOverride`→`MigrationFlags` + `String`→`MigrationId` merge is a
    /// later wave, so the engine refuses fail-closed rather than compare a
    /// PARTIAL domain (which would both false-reject a spec-correct hint and
    /// false-accept tampering of the un-folded fields). Authoring those fields
    /// WITHOUT a hint is unaffected.
    #[error(
        "checksum hint not yet computable: the IR envelope carries an advisory checksum hint \
         alongside {field} ({detail}), which this engine build cannot fold into the hint \
         domain yet (the flags/deps merge is a later wave). Drop the advisory hint, or omit \
         {field}, until the merge lands — the engine refuses to validate a hint against a \
         partial domain"
    )]
    ChecksumHintNotComputable {
        /// The field present alongside the hint that is not yet foldable.
        field: &'static str,
        /// A human-readable detail of the offending value.
        detail: String,
    },
    /// A `flags.timeout_ms` / `flags.lock_timeout_ms` override of `0`. Both
    /// PostgreSQL and MySQL spell "no limit" as `0`, so a zero override does not
    /// tighten the budget, it removes it, and the DDL waits indefinitely while
    /// holding the locks it already took. The overrides exist to raise a FINITE
    /// budget for one planned migration, so zero is outside their domain.
    ///
    /// This is the author-facing half of the rule: it fails the artifact at load,
    /// where the author can still edit it. The binding half runs at apply, where
    /// the effective value is resolved (`zero_migrate::apply::timeout`), because
    /// an embedder can build the `Migration` directly and a zero can also come
    /// from executor config that never passes through this gate.
    #[error(
        "indefinite timeout: flags.{field} is 0, which PostgreSQL and MySQL both read as \
         \"no limit\" rather than a zero budget: the migration would wait indefinitely \
         while holding the locks it already took. Set a finite number of milliseconds, \
         or omit {field} to inherit the executor default"
    )]
    IndefiniteTimeoutFlag {
        /// The zero-valued override (`timeout_ms` or `lock_timeout_ms`).
        field: &'static str,
    },
}

/// The sentinel an ownership lookup yields for a table absent from the registry.
/// Surfaced in [`IrLoadError::NotTableOwner::owner`] so the fail-closed unknown-
/// owner case is legible in the error.
pub const UNKNOWN_OWNER: &str = "<unregistered>";

/// The table NEWLY CREATED by an [`Op`], if any.
///
/// Ownership registration is deliberately shape-agnostic: a `createTable`
/// establishes attachability for the named table no matter which profile
/// resolved it, which system fields were injected or omitted, or which primary
/// key shape it carries.
#[must_use]
pub fn op_created_table(op: &Op) -> Option<&str> {
    match op {
        Op::CreateTable { name, .. } | Op::CreatePartition { name, .. } => Some(name),
        _ => None,
    }
}

fn collect_created_tables<'a>(op: &'a Op, out: &mut Vec<&'a str>) {
    if let Op::Dialectal {
        default,
        pg,
        sqlite,
        mysql,
    } = op
    {
        for leg in [
            default.as_deref(),
            pg.as_deref(),
            sqlite.as_deref(),
            mysql.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            for inner in leg {
                collect_created_tables(inner, out);
            }
        }
    } else if let Some(table) = op_created_table(op) {
        out.push(table);
    }
}

/// Every table name `ops` creates, descending into `Op::Dialectal` legs.
///
/// The aggregate form of [`collect_created_tables`], for callers that need the whole
/// stream's creations rather than one op's. Use this rather than mapping
/// [`op_created_table`] over `ops`: that helper answers for a SINGLE op and returns
/// `None` for a wrapper, so a create authored inside `dialect({ ... })` disappears.
///
/// EVERY leg, matching [`enforce_ir_ownership`], because the answer feeds ownership
/// rather than execution. A name declared in any leg is a name this app authored, and
/// over-claiming costs a refusal that names the owner while under-claiming lets
/// another app take the table.
#[must_use]
pub fn ir_created_tables(ops: &[Op]) -> Vec<&str> {
    let mut out = Vec::new();
    for op in ops {
        collect_created_tables(op, &mut out);
    }
    out
}

/// The single target table of an [`Op`] for the ownership check. Every op the IR
/// admits operates on exactly one table (the closed `Op` enum carries a `name`
/// for `createTable`, a `table` for every alter/DML op, and `DropIndex` carries
/// an optional owning-table hint). A `DropIndex` with no `table` hint has no
/// ownership-checkable target and returns `None`.
///
/// **A bare-name `DropIndex` (`table: None`) is REJECTED UPSTREAM fail-closed**
/// by `validate_op` (a name-only index drop is not
/// ownership-checkable, so it would let a migration drop another app's index by
/// name). So by the time this function runs, every `DropIndex` reaching the
/// ownership pass carries a `table` hint and IS ownership-checked. The `None` arm
/// below is retained as defense-in-depth — if a future caller invokes the
/// ownership pass without the validator, a bare-name `DropIndex` still finds no
/// checkable target rather than silently passing as an owned op.
#[must_use]
fn op_target_table(op: &Op) -> Option<&str> {
    match op {
        Op::CreateTable { name, .. } => Some(name),
        Op::CreatePartition { of, .. }
        | Op::AttachPartition { parent: of, .. }
        | Op::DetachPartition { parent: of, .. }
        | Op::DropPartition { parent: of, .. } => Some(of),
        Op::SetTableOptions { table, .. } => Some(table),
        // The ownership gate checks the EXISTING (old) table — a rename of a table
        // the deploying app does not own is refused on the source name.
        Op::DropTable { table, .. }
        | Op::RenameTable { table, .. }
        | Op::AddColumn { table, .. }
        | Op::DropColumn { table, .. }
        | Op::CreateIndex { table, .. }
        | Op::SetColumnType { table, .. }
        | Op::SetColumnNotNull { table, .. }
        | Op::DropColumnNotNull { table, .. }
        | Op::SetColumnDefault { table, .. }
        | Op::DropColumnDefault { table, .. }
        | Op::RenameColumn { table, .. }
        | Op::AlterPrimaryKey { table, .. }
        | Op::SynchronizeIdentity { table, .. }
        | Op::AddConstraint { table, .. }
        | Op::DropConstraint { table, .. }
        | Op::ValidateConstraint { table, .. }
        | Op::Insert { table, .. }
        | Op::Update { table, .. }
        | Op::Delete { table, .. }
        | Op::Backfill { table, .. } => Some(table),
        Op::DropIndex { table, .. } => table.as_deref(),
        Op::Comment { target, .. } => target.touched_table(),
        // VENDOR — table-scoped vendor ops (RLS/policy/trigger) are ownership-checked
        // against their table; the database-/role-/schema-level vendor ops have no
        // table to check (they are operator-gated by the capability gate, not the
        // per-table ownership pass).
        Op::SetRls { table, .. }
        | Op::CreatePolicy { table, .. }
        | Op::DropPolicy { table, .. }
        | Op::CreateTrigger { table, .. }
        | Op::DropTrigger { table, .. } => Some(table),
        Op::CreateSchema { .. }
        | Op::DropSchema { .. }
        | Op::CreateExtension { .. }
        | Op::DropExtension { .. }
        | Op::CreateEnum { .. }
        | Op::DropEnum { .. }
        | Op::CreateDomain { .. }
        | Op::DropDomain { .. }
        | Op::CreateSequence { .. }
        | Op::AlterSequence { .. }
        | Op::DropSequence { .. }
        | Op::CreateRole { .. }
        | Op::AlterRole { .. }
        | Op::DropRole { .. }
        | Op::DropOwnedBy { .. }
        | Op::Grant { .. }
        | Op::Revoke { .. }
        | Op::CreateView { .. }
        | Op::DropView { .. }
        | Op::CreateFunction { .. }
        | Op::DropFunction { .. }
        | Op::PgRaw { .. }
        | Op::Dialectal { .. } => None,
    }
}

fn collect_target_tables<'a>(op: &'a Op, out: &mut Vec<&'a str>) {
    if let Op::Dialectal {
        default,
        pg,
        sqlite,
        mysql,
    } = op
    {
        for leg in [
            default.as_deref(),
            pg.as_deref(),
            sqlite.as_deref(),
            mysql.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            for inner in leg {
                collect_target_tables(inner, out);
            }
        }
    } else if let Op::CreateView {
        query: ViewQuery::Structured { select },
        ..
    } = op
    {
        // A view READS from its FROM/JOIN source tables, so those source tables
        // are the ownership-checkable targets — `op_target_table` returns `None`
        // for `CreateView` (the view NAME is a created object, not a touched
        // table). Without this, a confined creator could author a view SELECTing
        // ANOTHER app's tables in the same permitted schema — a read-only
        // cross-tenant disclosure the ownership gate otherwise closes. A source
        // table the deploying app does not own (or that is unregistered) fails
        // closed, exactly like any other targeted table.
        //
        // The `ViewQuery::Raw` body is an opaque SQL string whose referenced
        // tables cannot be extracted here; it is separately gated by
        // `VendorCapability::RawViewBody` (operator approval).
        out.push(&select.from.name);
        for join in &select.joins {
            out.push(&join.table.name);
        }
    } else if let Some(table) = op_target_table(op) {
        out.push(table);
    }
}

/// IR-path per-table ownership enforcement — the IR mirror of the
/// declarative path's two-part ownership check (in `declarative.rs`).
///
/// `registry` is the project's CURRENT table→owner map. This pass:
/// 1. First registers every `createTable`'s NEW table as owned by `deploying_app`
///    (a freshly declared table is owned by its declarer, exactly as the
///    declarative union assigns ownership to the declaring app), into a working
///    copy of the registry.
/// 2. Then, for EVERY op that targets a table, looks the table up in the
///    (augmented) registry and refuses the migration if the owner is not
///    `deploying_app`. **A table absent from the registry FAILS CLOSED**
///    ([`IrLoadError::NotTableOwner`] with [`UNKNOWN_OWNER`]) — a DML/DDL op on a
///    never-declared table is refused, exactly as the declarative drop path
///    refuses an unknown-owner drop.
///
/// **The `createTable` pre-pass is intentionally op-ORDER-INDEPENDENT.** Because
/// step 1 registers every `createTable` in the migration BEFORE step 2's per-op
/// check, a DML/alter op that appears *positionally before* its `createTable` in
/// the op list still passes ownership (the table is already pre-registered to the
/// deploying app). This is deliberate: ownership is a WHO-MAY-TOUCH question, not
/// an apply-ORDER-VALIDITY question — and it mirrors the declarative/snapshot
/// path, whose set-semantics union has no op order at all. Apply-order
/// correctness (you cannot INSERT into a table the same migration has not yet
/// created at execution time) is the EXECUTOR's concern and surfaces there; it is
/// NOT a security relaxation in this gate.
///
/// # Errors
/// [`IrLoadError::NotTableOwner`] on a non-owned or unregistered target table.
pub fn enforce_ir_ownership(
    ir: &MigrationIr,
    deploying_app: &str,
    registry: &BTreeMap<String, String>,
) -> Result<(), IrLoadError> {
    // Working registry = the project registry + this migration's createTable
    // declarations (owned by the deploying app). A createTable for a table that
    // ALREADY has a different owner is still caught by the per-op check below
    // (we only insert when absent, so an existing owner is not silently
    // overwritten — a createTable colliding with another app's table is refused).
    let mut owners: BTreeMap<&str, &str> = registry
        .iter()
        .map(|(t, o)| (t.as_str(), o.as_str()))
        .collect();
    for op in &ir.ops {
        let mut created = Vec::new();
        collect_created_tables(op, &mut created);
        for table in created {
            owners.entry(table).or_insert(deploying_app);
        }
    }
    for (op_index, op) in ir.ops.iter().enumerate() {
        let mut targets = Vec::new();
        collect_target_tables(op, &mut targets);
        for table in targets {
            let owner = owners.get(table).copied().unwrap_or(UNKNOWN_OWNER);
            if owner != deploying_app {
                return Err(IrLoadError::NotTableOwner {
                    op_index,
                    table: table.to_string(),
                    owner: owner.to_string(),
                    deploying_app: deploying_app.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Refuse an IR envelope whose per-migration timeout override is `0`.
///
/// `0` is how both PostgreSQL and MySQL spell "no limit": `SET lock_timeout = 0`
/// and `SET statement_timeout = 0` read back as `0`, not `0ms`. So an override of
/// zero disables the very timeout it claims to set, and the migration's DDL waits
/// indefinitely while holding whatever it already acquired. The overrides are
/// documented as raising a FINITE budget for one planned migration, so zero was
/// never in their domain.
///
/// This gate is early AUTHOR feedback, not the boundary: it catches the artifact
/// while the author can still edit it. The binding refusal runs at apply, where
/// the effective value is resolved.
///
/// # Errors
/// [`IrLoadError::IndefiniteTimeoutFlag`] naming the zero-valued override.
pub fn enforce_ir_finite_timeouts(ir: &MigrationIr) -> Result<(), IrLoadError> {
    for (field, value) in [
        ("timeout_ms", ir.flags.timeout_ms),
        ("lock_timeout_ms", ir.flags.lock_timeout_ms),
    ] {
        if value.is_some_and(|ms| ms.get() == 0) {
            return Err(IrLoadError::IndefiniteTimeoutFlag { field });
        }
    }
    Ok(())
}

/// Recompute the **advisory hint domain** checksum for a loaded IR.
///
/// The hint domain (see the [`MigrationIr::checksum`] doc) is
/// `ops + flags + depends_on + supersedes + preconditions`, with `owner_app = ""`
/// (server-stamped and so unpredictable to the builder — excluded).
///
/// This only folds the SUBSET it can compute faithfully: the op region (which
/// fully determines the artifact's logical content) + preconditions + the
/// dialect-neutral DEFAULT flags + EMPTY deps/supersedes. The
/// [`IrFlagsOverride`](crate::ir::IrFlagsOverride)→[`MigrationFlags`] and
/// `String`→`MigrationId` merges are a later wave, so this recompute is ONLY
/// valid for an IR whose `flags`/`depends_on`/`supersedes` are at their
/// defaults — the caller MUST gate on that ([`hint_domain_uncomputable_field`])
/// and refuse a hint over a wider domain rather than compare a partial one (a
/// partial compare both false-rejects a spec-correct hint and false-accepts
/// tampering of the un-folded fields). The result is what
/// `zero_migrate::model::load::load_ir_document`
/// compares to a present `checksum` hint, only after the gate passes.
#[must_use]
pub fn recompute_hint_domain_checksum(ir: &MigrationIr) -> Checksum {
    debug_assert!(
        hint_domain_uncomputable_field(ir).is_none(),
        "recompute_hint_domain_checksum called on an IR with a not-yet-foldable \
         flags/deps/supersedes domain — the caller must gate first"
    );
    Checksum::of_ir(
        &crate::ir::CanonicalOpList(&ir.ops),
        &MigrationFlags::default(),
        "", // owner_app excluded from the hint domain (server-stamped)
        &[],
        &[],
        &ir.preconditions,
    )
}

/// The **authoritative, dialect-neutral plan checksum** over a loaded
/// [`MigrationIr`] — the drift anchor the deploy path journals.
///
/// This is `Checksum::of_ir` over every apply-relevant typed field: the canonical
/// op list, the effective flag overrides, the server-stamped `owner_app`, exact
/// `depends_on` and `supersedes` lists, and preconditions. It differs from
/// [`recompute_hint_domain_checksum`] because the advisory hint helper is limited
/// to its historical default-metadata domain and excludes `owner_app`.
///
/// **Why this is the drift anchor and the rendered SQL is NOT.** The anchor is
/// the checksum over the canonical op list — one plan checksum over the canonical
/// op list, not the rendered SQL. Because the op list
/// is dialect-NEUTRAL, the SAME IR envelope re-deployed on PG or `SQLite` re-derives
/// the SAME anchor — so a re-deploy detects drift against the logical artifact, not
/// a PG-specific SQL spelling. Editing the authoring `.ts` changes the op list ⇒
/// changes this checksum ⇒ the executor's net-applied drift gate aborts
/// (`drift.rs` compares the journaled checksum to the lowered `Migration.checksum`,
/// which the IR Lower stamps with THIS value - see
/// `zero_migrate::render::lower::IrAuthor::lower_plan`).
///
/// `name` remains part of the stable plan identity rather than its content, and
/// `ir_version` only selects the already-validated wire interpretation. The
/// advisory `checksum` field is excluded to avoid self-reference.
#[must_use]
pub fn authoritative_ir_checksum(ir: &MigrationIr) -> Checksum {
    let mut flags = MigrationFlags::default();
    if let Some(value) = ir.flags.transactional {
        flags.transactional = value;
    }
    if let Some(value) = ir.flags.destructive {
        flags.destructive = value;
    }
    if let Some(value) = ir.flags.online {
        flags.online = value;
    }
    if let Some(value) = ir.flags.requires_approval {
        flags.requires_approval = value;
    }
    if let Some(value) = ir.flags.repeatable {
        flags.repeatable = value;
    }
    if let Some(value) = ir.flags.engine_goodie_ddl {
        flags.engine_goodie_ddl = value;
    }
    flags.timeout_ms = ir.flags.timeout_ms.map(crate::ir::SafeU64::get);
    flags.lock_timeout_ms = ir.flags.lock_timeout_ms.map(crate::ir::SafeU64::get);
    flags.phase = ir.flags.phase;

    Checksum::of_ir_strings(
        &crate::ir::CanonicalOpList(&ir.ops),
        &flags,
        &ir.owner_app,
        &ir.depends_on,
        &ir.supersedes,
        &ir.preconditions,
    )
}

/// Return the hint-domain field this engine build cannot yet fold for `ir`,
/// or `None` when the hint domain IS fully computable (flags at default + no
/// deps/supersedes). Used to fail closed on a hint over a not-yet-foldable
/// domain (the `IrFlagsOverride`/`MigrationId` merges are a later wave).
///
/// Public so the build-time checksum fold (the JS builder's
/// `typed_checksum`/`checksum_of_committed`) can gate on the SAME domain as the
/// engine's load gate — refusing to anchor a partial checksum over an IR carrying
/// non-default flags/deps/supersedes rather than silently folding a partial domain
/// the engine's load gate would later refuse.
#[must_use]
pub fn hint_domain_uncomputable_field(ir: &MigrationIr) -> Option<(&'static str, String)> {
    if !ir.depends_on.is_empty() {
        return Some(("depends_on", format!("{:?}", ir.depends_on)));
    }
    if !ir.supersedes.is_empty() {
        return Some(("supersedes", format!("{:?}", ir.supersedes)));
    }
    if ir.flags != crate::ir::IrFlagsOverride::default() {
        return Some(("flags", format!("{:?}", ir.flags)));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{AlterPrimaryKeyAction, IrFlagsOverride, CURRENT_IR_VERSION};

    fn alter_primary_key_ir() -> MigrationIr {
        MigrationIr {
            ir_version: CURRENT_IR_VERSION,
            name: "replace orders primary key".to_string(),
            owner_app: "untrusted-wire-hint".to_string(),
            ops: vec![Op::AlterPrimaryKey {
                table: "orders".to_string(),
                action: AlterPrimaryKeyAction::Replace {
                    expected_columns: vec!["id".to_string()],
                    columns: vec!["tenant_id".to_string(), "order_id".to_string()],
                    drop_identity_from: Some(vec!["id".to_string()]),
                },
                schema: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        }
    }

    fn synchronize_identity_ir() -> MigrationIr {
        MigrationIr {
            ir_version: CURRENT_IR_VERSION,
            name: "synchronize imported orders".to_string(),
            owner_app: "untrusted-wire-hint".to_string(),
            ops: vec![Op::SynchronizeIdentity {
                table: "orders".to_string(),
                column: "id".to_string(),
                writes_quiesced: "orders_import_window".to_string(),
                schema: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        }
    }

    #[test]
    fn alter_primary_key_participates_in_table_ownership_gate() {
        let ir = alter_primary_key_ir();
        let owned = BTreeMap::from([("orders".to_string(), "orders-app".to_string())]);
        enforce_ir_ownership(&ir, "orders-app", &owned)
            .expect("the owning app may alter its table primary key");

        let error = enforce_ir_ownership(&ir, "other-app", &owned).unwrap_err();
        assert_eq!(
            error,
            IrLoadError::NotTableOwner {
                op_index: 0,
                table: "orders".to_string(),
                owner: "orders-app".to_string(),
                deploying_app: "other-app".to_string(),
            }
        );
    }

    #[test]
    fn synchronize_identity_participates_in_table_ownership_gate() {
        let ir = synchronize_identity_ir();
        let owned = BTreeMap::from([("orders".to_string(), "orders-app".to_string())]);
        enforce_ir_ownership(&ir, "orders-app", &owned)
            .expect("the owning app may synchronize its identity generator");

        let error = enforce_ir_ownership(&ir, "other-app", &owned)
            .expect_err("a different app must not synchronize the generator");
        assert!(error.to_string().contains("orders"), "{error}");
    }

    fn create_view_ir(from: &str, join: Option<&str>) -> MigrationIr {
        use crate::expr::Expr;
        use crate::ir::{Join, JoinKind, SelectAst, SelectItem, TableRef, ViewQuery};
        let joins = join
            .map(|t| {
                vec![Join {
                    kind: JoinKind::Inner,
                    table: TableRef {
                        name: t.to_string(),
                        schema: None,
                        alias: None,
                    },
                    on: Expr::UuidV4,
                }]
            })
            .unwrap_or_default();
        MigrationIr {
            ir_version: CURRENT_IR_VERSION,
            name: "create reporting view".to_string(),
            owner_app: "untrusted-wire-hint".to_string(),
            ops: vec![Op::CreateView {
                name: "report".to_string(),
                schema: None,
                columns: None,
                query: ViewQuery::Structured {
                    select: Box::new(SelectAst {
                        from: TableRef {
                            name: from.to_string(),
                            schema: None,
                            alias: None,
                        },
                        projection: vec![SelectItem::ColRef {
                            table: None,
                            name: "id".to_string(),
                            alias: None,
                        }],
                        joins,
                        r#where: None,
                        group_by: Vec::new(),
                        having: None,
                        order_by: None,
                        limit: None,
                    }),
                },
                replace: None,
                materialized: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        }
    }

    #[test]
    fn create_view_from_table_participates_in_table_ownership_gate() {
        // A view READS from its FROM table, so the source table is ownership-checked:
        // the owning app may build a view over its own table, but another app may
        // not author a view SELECTing it (a cross-tenant read the gate must refuse).
        let ir = create_view_ir("orders", None);
        let owned = BTreeMap::from([("orders".to_string(), "orders-app".to_string())]);
        enforce_ir_ownership(&ir, "orders-app", &owned)
            .expect("the owning app may build a view over its own table");

        let error = enforce_ir_ownership(&ir, "other-app", &owned).unwrap_err();
        assert_eq!(
            error,
            IrLoadError::NotTableOwner {
                op_index: 0,
                table: "orders".to_string(),
                owner: "orders-app".to_string(),
                deploying_app: "other-app".to_string(),
            }
        );
    }

    #[test]
    fn create_view_join_to_another_apps_table_is_refused() {
        // Even the app that owns the FROM table cannot smuggle in another app's
        // table through a JOIN: every source table in the SELECT is checked.
        let ir = create_view_ir("orders", Some("secrets"));
        let owned = BTreeMap::from([
            ("orders".to_string(), "orders-app".to_string()),
            ("secrets".to_string(), "secret-app".to_string()),
        ]);
        let error = enforce_ir_ownership(&ir, "orders-app", &owned).unwrap_err();
        assert_eq!(
            error,
            IrLoadError::NotTableOwner {
                op_index: 0,
                table: "secrets".to_string(),
                owner: "secret-app".to_string(),
                deploying_app: "orders-app".to_string(),
            }
        );
    }
}
