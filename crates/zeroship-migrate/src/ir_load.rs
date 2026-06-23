//! The fail-closed **`.ir.json` load gate** (design §5.2 / §5.3 / §6 / §8.6).
//!
//! This is the SINGLE production seam every creator-authored `.ir.json` passes
//! through before the engine lowers it (`IrAuthor::lower`, the per-dialect DDL
//! compiler — a later wave) or checksums it. The gate is fail-closed and ordered
//! so a hostile / newer-engine / cross-tenant artifact is rejected BEFORE any
//! checksum or lowering runs:
//!
//! 1. **deserialize** the bytes into the typed [`MigrationIr`] — the closed `Op`/
//!    `Expr` AST + the constrained [`IrScalar`](crate::ir::IrScalar) numeric
//!    domain reject a malformed/lossy/unknown-node artifact at this step (§2.5,
//!    §3.3.1.1).
//! 2. **`ir_version` fail-closed** ([`MigrationIr::check_ir_version`]): a FUTURE
//!    wire-format version this engine build does not understand is refused
//!    (§5.3, design line 888).
//! 3. **structural validation** ([`validate::validate_ir`]): the authoritative
//!    structural gate over EVERY embedded `Expr` slot for the deploy-target
//!    dialect (§3.3.1.1) — out-of-envelope `splitPart`, an unresolved `ColRef` in
//!    a self-contained `createTable`, a non-portable shape.
//! 4. **server-stamped ownership** ([`enforce_ir_ownership`]): `owner_app` is
//!    overwritten with the deploying app's id (a spoofed value is discarded), and
//!    every op targeting a table must resolve to the deploying app in the project
//!    ownership registry — a table absent from the registry FAILS CLOSED (§8.6,
//!    mirroring the declarative drop-ownership check, `declarative.rs:2820-2826`).
//!    A `createTable` establishes ownership for its NEW table (the deploying app),
//!    exactly as the declarative union does.
//! 5. **advisory checksum-hint compare** (§2.4 point 2): when the artifact carries
//!    the optional `checksum` hint, the engine RECOMPUTES the hint-domain checksum
//!    and a mismatch is a hard error (genuine drift / tamper). The engine is
//!    authoritative; the hint is advisory and need not be present.
//!
//! Lowering the validated IR to an executable [`AppliedPlan`](crate::plan::AppliedPlan)
//! (`IrAuthor::lower`, the §6.5 snapshot-builder + per-dialect DDL render) is the
//! next wave; this module is the load + gate that MUST run first.

use std::collections::BTreeMap;

use crate::ir::{IrVersionError, MigrationIr, Op};
use crate::migration::{Checksum, MigrationFlags};
use crate::validate::{validate_ir, AuthoringError, Dialect};

/// A failure of the `.ir.json` load gate. Each variant maps to one ordered
/// fail-closed step (§5.2).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IrLoadError {
    /// The bytes did not deserialize into a well-formed [`MigrationIr`] — a
    /// malformed JSON document, an unknown op/expr node tag, an out-of-domain
    /// numeric scalar (§2.5), or a non-nullary synth default (§4.3). Carries the
    /// serde message (which embeds the structured code, e.g.
    /// [`EXPR_INVALID_NUMERIC`](crate::ir::EXPR_INVALID_NUMERIC), for matching).
    #[error("malformed .ir.json: {0}")]
    Deserialize(String),
    /// The artifact declared a FUTURE `ir_version` this engine cannot interpret
    /// (§5.3). Fail-closed.
    #[error(transparent)]
    Version(#[from] IrVersionError),
    /// An embedded expression-AST node failed the structural validator for the
    /// deploy-target dialect (§3.3.1.1). Carries the §8.8 authoring envelope.
    #[error(transparent)]
    Validate(#[from] AuthoringError),
    /// An op targets a table the deploying app does not own, OR a table absent
    /// from the project ownership registry (unknown-owner fail-closed, §8.6).
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
    /// recomputed hint-domain checksum — genuine drift / tamper (§2.4 point 2).
    #[error(
        "checksum hint mismatch: the .ir.json advisory hint {hint:?} does not match the \
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
    /// override). The §2.4-point-2 hint domain is
    /// `ops + flags + depends_on + supersedes + preconditions`, but the
    /// `IrFlagsOverride`→`MigrationFlags` + `String`→`MigrationId` merge is a
    /// later wave, so the engine refuses fail-closed rather than compare a
    /// PARTIAL domain (which would both false-reject a spec-correct hint and
    /// false-accept tampering of the un-folded fields). Authoring those fields
    /// WITHOUT a hint is unaffected.
    #[error(
        "checksum hint not yet computable: the .ir.json carries an advisory checksum hint \
         alongside {field} ({detail}), which this engine build cannot fold into the §2.4 hint \
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
}

/// The sentinel an ownership lookup yields for a table absent from the registry.
/// Surfaced in [`IrLoadError::NotTableOwner::owner`] so the fail-closed unknown-
/// owner case is legible in the error.
const UNKNOWN_OWNER: &str = "<unregistered>";

/// The single target table of an [`Op`] for the ownership check. Every op the IR
/// admits operates on exactly one table (the closed `Op` enum carries a `name`
/// for `createTable`, a `table` for every alter/DML op, and `DropIndex` carries
/// an optional owning-table hint). A `DropIndex` with no `table` hint has no
/// ownership-checkable target and returns `None` (it names only an index; the
/// index's table ownership was enforced when the index was created).
#[must_use]
fn op_target_table(op: &Op) -> Option<&str> {
    match op {
        Op::CreateTable { name, .. } => Some(name),
        Op::DropTable { table, .. }
        | Op::AddColumn { table, .. }
        | Op::DropColumn { table, .. }
        | Op::CreateIndex { table, .. }
        | Op::AlterColumnType { table, .. }
        | Op::AlterColumnNullability { table, .. }
        | Op::RenameColumn { table, .. }
        | Op::AddConstraint { table, .. }
        | Op::DropConstraint { table, .. }
        | Op::Insert { table, .. }
        | Op::Update { table, .. }
        | Op::Delete { table, .. }
        | Op::Backfill { table, .. } => Some(table),
        Op::DropIndex { table, .. } => table.as_deref(),
    }
}

/// IR-path per-table ownership enforcement (§8.6) — the IR mirror of the
/// declarative path's two-part ownership check (`declarative.rs:2820-2826`).
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
    let mut owners: BTreeMap<&str, &str> =
        registry.iter().map(|(t, o)| (t.as_str(), o.as_str())).collect();
    for op in &ir.ops {
        if let Op::CreateTable { name, .. } = op {
            owners.entry(name.as_str()).or_insert(deploying_app);
        }
    }
    for (op_index, op) in ir.ops.iter().enumerate() {
        let Some(table) = op_target_table(op) else {
            continue; // an op with no ownership-checkable target (DropIndex w/o table hint)
        };
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
    Ok(())
}

/// Recompute the §2.4-point-2 **advisory hint domain** checksum for a loaded IR.
///
/// The hint domain (design §2.4 point 2 + [`MigrationIr::checksum`] doc) is
/// `ops + flags + depends_on + supersedes + preconditions`, with `owner_app = ""`
/// (server-stamped and so unpredictable to the builder — excluded, §2.4 /
/// line 1094).
///
/// **PR1 only folds the SUBSET it can compute faithfully**: the op region (which
/// fully determines the artifact's logical content) + preconditions + the
/// dialect-neutral DEFAULT flags + EMPTY deps/supersedes. The
/// [`IrFlagsOverride`](crate::ir::IrFlagsOverride)→[`MigrationFlags`] and
/// `String`→`MigrationId` merges are a later wave, so this recompute is ONLY
/// valid for an IR whose `flags`/`depends_on`/`supersedes` are at their
/// defaults — the caller MUST gate on that ([`hint_domain_uncomputable_field`])
/// and refuse a hint over a wider domain rather than compare a partial one (a
/// partial compare both false-rejects a spec-correct hint and false-accepts
/// tampering of the un-folded fields). The result is what [`load_ir_document`]
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

/// Return the §2.4 hint-domain field this engine build cannot yet fold for `ir`,
/// or `None` when the hint domain IS fully computable (flags at default + no
/// deps/supersedes). Used to fail closed on a hint over a not-yet-foldable
/// domain (the `IrFlagsOverride`/`MigrationId` merges are a later wave).
#[must_use]
fn hint_domain_uncomputable_field(ir: &MigrationIr) -> Option<(&'static str, String)> {
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

/// Load + GATE a `.ir.json` document (the fail-closed §5.2 chain). Returns the
/// validated, ownership-checked [`MigrationIr`] with its `owner_app` STAMPED to
/// `deploying_app` (a spoofed/absent value in the artifact is discarded) — ready
/// for `IrAuthor::lower` (a later wave).
///
/// The steps run in the security-critical order (§5.2): deserialize →
/// `ir_version` → `validate_ir` (for `target_dialect`) → ownership → checksum-hint
/// compare. Every step is fail-closed; lowering NEVER sees an artifact that
/// failed any gate.
///
/// `registry` is the project's table→owner map; `target_dialect` is threaded from
/// the deploy backend selection (`deploy_migrate.rs` / `--engine`, design §2.4.1).
///
/// # Errors
/// [`IrLoadError`] for a malformed document, an unknown future `ir_version`, a
/// structural-validation failure, an ownership violation, or a checksum-hint
/// mismatch.
pub fn load_ir_document(
    bytes: &str,
    deploying_app: &str,
    target_dialect: Dialect,
    registry: &BTreeMap<String, String>,
) -> Result<MigrationIr, IrLoadError> {
    // 1. deserialize (closed AST + numeric domain reject malformed/lossy here).
    let mut ir: MigrationIr =
        serde_json::from_str(bytes).map_err(|e| IrLoadError::Deserialize(e.to_string()))?;

    // 2. ir_version fail-closed — BEFORE any checksum/lower (§5.3).
    ir.check_ir_version()?;

    // 3. structural validation — the authoritative gate over every Expr slot.
    validate_ir(&ir, target_dialect, &[])?;

    // 4. ownership — over the ARTIFACT's claimed owner is irrelevant; the check is
    //    against the deploying app + the project registry (fail-closed unknown).
    enforce_ir_ownership(&ir, deploying_app, registry)?;

    // 5. advisory checksum-hint compare (§2.4 point 2) — recompute + compare, then
    //    DROP the hint (it never folds into the authoritative checksum). Done
    //    against the artifact's claimed hint BEFORE we stamp owner_app, since the
    //    hint domain excludes owner_app anyway.
    if let Some(hint) = ir.checksum.clone() {
        // Fail closed if the §2.4 hint domain is not yet fully computable for this
        // IR (a non-default flags / deps / supersedes contribution the Wave-C
        // merge has not landed). Never compare a hint against a PARTIAL domain.
        if let Some((field, detail)) = hint_domain_uncomputable_field(&ir) {
            return Err(IrLoadError::ChecksumHintNotComputable { field, detail });
        }
        let recomputed = recompute_hint_domain_checksum(&ir);
        if recomputed.as_str() != hint {
            return Err(IrLoadError::ChecksumHintMismatch {
                hint,
                recomputed: recomputed.as_str().to_string(),
            });
        }
    }

    // Server-stamp the owner (a spoofed/absent artifact value is discarded, §8.6).
    // owner_app is excluded from the hint domain, so stamping it never invalidates
    // the just-verified hint.
    ir.owner_app = deploying_app.to_string();
    Ok(ir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{ColType, IrColumn, Op};

    fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(t, o)| (t.to_string(), o.to_string())).collect()
    }

    fn create_table(name: &str) -> Op {
        Op::CreateTable {
            name: name.into(),
            columns: vec![IrColumn {
                name: "first".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None,
            }],
            constraints: vec![],
            indexes: vec![],
        }
    }

    fn ir_json(ops_json: &str, extra: &str) -> String {
        format!(
            r#"{{"ir_version": 1, "name": "m", "ops": {ops_json}{extra}}}"#
        )
    }

    // ── ir_version fail-closed on the PRODUCTION path (finding 3) ────────────

    #[test]
    fn load_rejects_future_ir_version_before_anything_else() {
        let bytes = r#"{"ir_version": 999, "name": "m", "ops": [{"op":"dropTable","table":"t"}]}"#;
        let reg = registry(&[("t", "app_a")]);
        let err = load_ir_document(bytes, "app_a", Dialect::Postgres, &reg).unwrap_err();
        assert!(matches!(err, IrLoadError::Version(_)), "got: {err}");
    }

    // ── validate_ir wired as the loader's gate (finding 3) ──────────────────
    // A hostile .ir.json driven through the REAL loader (not the validator unit
    // test) must have the structural gate FIRE on the production path.

    #[test]
    fn load_runs_validate_ir_on_the_production_path() {
        // A createTable whose Check references a column NOT on the table — rule
        // (c). The gate must reject it via validate_ir on the real load path.
        let ops = r#"[{"op":"createTable","name":"users","columns":[{"name":"first","type":"text"}],"constraints":[{"kind":{"kind":"check","expr":{"node":"unaryOp","op":"isNotNull","operand":{"node":"colRef","name":"ghost"}}}}]}]"#;
        let bytes = ir_json(ops, "");
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).unwrap_err();
        match err {
            IrLoadError::Validate(ae) => {
                assert_eq!(ae.code, crate::validate::CODE_UNSUPPORTED);
            }
            other => panic!("expected a structural Validate error, got: {other}"),
        }
    }

    #[test]
    fn load_rejects_out_of_envelope_split_part_on_sqlite_target() {
        // Multi-char splitPart delim in an update set: PG-renderable, SQLite-reject.
        // The gate threads target_dialect into validate_ir, so a SQLite deploy
        // refuses it on the production path.
        let ops = r#"[{"op":"update","table":"users","set":{"name":{"node":"fnSynth","fn":"splitPart","args":[{"node":"colRef","name":"first"},{"node":"literal","value":", "},{"node":"literal","value":1}]}}}]"#;
        let bytes = ir_json(ops, "");
        let reg = registry(&[("users", "app_a")]);
        // PG accepts (validation OK), SQLite rejects.
        assert!(load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).is_ok());
        let err = load_ir_document(&bytes, "app_a", Dialect::Sqlite, &reg).unwrap_err();
        assert!(matches!(err, IrLoadError::Validate(_)), "got: {err}");
    }

    // ── deserialize gate: unknown node tag / out-of-domain scalar (finding) ──

    #[test]
    fn load_rejects_unknown_expr_node_tag_at_deserialize() {
        let ops = r#"[{"op":"delete","table":"users","where":{"node":"evilNode"}}]"#;
        let bytes = ir_json(ops, "");
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).unwrap_err();
        assert!(matches!(err, IrLoadError::Deserialize(_)), "got: {err}");
    }

    #[test]
    fn load_rejects_lossy_numeric_scalar_at_deserialize() {
        let ops = r#"[{"op":"insert","table":"users","columns":["a"],"rows":[[9007199254740992]]}]"#;
        let bytes = ir_json(ops, "");
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).unwrap_err();
        match err {
            IrLoadError::Deserialize(msg) => {
                assert!(msg.contains(crate::ir::EXPR_INVALID_NUMERIC), "got: {msg}");
            }
            other => panic!("expected Deserialize, got: {other}"),
        }
    }

    // ── ownership fail-closed (finding 6) ───────────────────────────────────

    #[test]
    fn load_refuses_op_on_another_apps_table() {
        let ops = r#"[{"op":"dropColumn","table":"users","column":"x"}]"#;
        let bytes = ir_json(ops, "");
        let reg = registry(&[("users", "app_owner")]); // owned by a DIFFERENT app
        let err = load_ir_document(&bytes, "app_intruder", Dialect::Postgres, &reg).unwrap_err();
        match err {
            IrLoadError::NotTableOwner { table, owner, deploying_app, op_index } => {
                assert_eq!(table, "users");
                assert_eq!(owner, "app_owner");
                assert_eq!(deploying_app, "app_intruder");
                assert_eq!(op_index, 0);
            }
            other => panic!("expected NotTableOwner, got: {other}"),
        }
    }

    #[test]
    fn load_refuses_op_on_table_absent_from_registry_fail_closed() {
        // A DML op targeting a never-declared table with NO registry entry is
        // refused fail-closed (unknown-owner), exactly as the declarative drop
        // path refuses an unknown-owner drop.
        let ops = r#"[{"op":"delete","table":"never_declared","where":{"node":"literal","value":true}}]"#;
        let bytes = ir_json(ops, "");
        let reg = registry(&[("users", "app_a")]); // no entry for `never_declared`
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).unwrap_err();
        match err {
            IrLoadError::NotTableOwner { table, owner, .. } => {
                assert_eq!(table, "never_declared");
                assert_eq!(owner, UNKNOWN_OWNER, "unknown owner must fail closed");
            }
            other => panic!("expected NotTableOwner (unknown-owner), got: {other}"),
        }
    }

    #[test]
    fn load_allows_create_table_of_a_new_table_by_its_declarer() {
        // A createTable establishes ownership for its NEW table (the declarer);
        // a following op on that same new table is then allowed.
        let ops = r#"[{"op":"createTable","name":"fresh","columns":[{"name":"first","type":"text"}]},{"op":"addColumn","table":"fresh","column":"x","type":"int"}]"#;
        let bytes = ir_json(ops, "");
        let reg = registry(&[]); // `fresh` is brand new — not in the project registry
        let ir = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).unwrap();
        assert_eq!(ir.owner_app, "app_a", "owner_app must be server-stamped");
    }

    #[test]
    fn load_refuses_create_table_colliding_with_another_apps_table() {
        // A createTable for a table ALREADY owned by another app does not silently
        // take ownership — the per-op check refuses it (the working registry only
        // inserts when ABSENT).
        let ops = r#"[{"op":"createTable","name":"users","columns":[{"name":"first","type":"text"}]}]"#;
        let bytes = ir_json(ops, "");
        let reg = registry(&[("users", "app_owner")]);
        let err = load_ir_document(&bytes, "app_intruder", Dialect::Postgres, &reg).unwrap_err();
        assert!(matches!(err, IrLoadError::NotTableOwner { .. }), "got: {err}");
    }

    #[test]
    fn enforce_ir_ownership_unit_create_then_use() {
        let ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: String::new(),
            ops: vec![create_table("fresh"), Op::DropColumn {
                table: "fresh".into(),
                column: "x".into(),
                if_exists: None,
            }],
            flags: crate::ir::IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        assert!(enforce_ir_ownership(&ir, "app_a", &registry(&[])).is_ok());
    }

    // ── checksum-hint compare wired (finding 5) ─────────────────────────────

    #[test]
    fn load_accepts_a_correct_checksum_hint() {
        // Build the IR, compute the CORRECT hint domain checksum, embed it, and
        // assert the load accepts it.
        let ops_vec = vec![Op::DropTable { table: "users".into(), if_exists: None, cascade: None }];
        let ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: String::new(),
            ops: ops_vec,
            flags: crate::ir::IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let correct = recompute_hint_domain_checksum(&ir);
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = ir_json(ops, &format!(r#", "checksum": "{}""#, correct.as_str()));
        let reg = registry(&[("users", "app_a")]);
        let loaded = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).unwrap();
        // The hint is carried through (the engine recomputes; it does not strip it).
        assert_eq!(loaded.checksum.as_deref(), Some(correct.as_str()));
    }

    #[test]
    fn load_rejects_a_wrong_checksum_hint() {
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = ir_json(ops, r#", "checksum": "deadbeefdeadbeef""#);
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).unwrap_err();
        match err {
            IrLoadError::ChecksumHintMismatch { hint, recomputed } => {
                assert_eq!(hint, "deadbeefdeadbeef");
                assert_ne!(recomputed, "deadbeefdeadbeef");
            }
            other => panic!("expected ChecksumHintMismatch, got: {other}"),
        }
    }

    #[test]
    fn load_accepts_an_absent_checksum_hint() {
        // The hint is advisory and need not be present (§2.4).
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = ir_json(ops, "");
        let reg = registry(&[("users", "app_a")]);
        assert!(load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).is_ok());
    }

    // ── HIGH: hint domain is not yet fully computable (deps/supersedes/flags) ─
    // Until the Wave-C IrFlagsOverride→MigrationFlags + String→MigrationId merge
    // lands, the recompute folds ONLY ops+preconditions (neutral flags, empty
    // deps). A hint-bearing IR that ALSO carries depends_on/supersedes or
    // non-default flags must NOT be silently compared against a PARTIAL domain
    // (that would both false-reject a spec-correct hint AND false-accept tampering
    // of the un-folded fields). The loader fails closed with a clear error.

    #[test]
    fn load_rejects_hint_bearing_ir_with_depends_on_fail_closed() {
        // A hint over an IR that carries a depends_on entry: the hint domain
        // (per §2.4 / ir.rs doc) includes depends_on, but PR1 cannot fold it, so
        // the loader refuses rather than compare a partial domain.
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = ir_json(
            ops,
            r#", "depends_on": ["m_0001"], "checksum": "deadbeefdeadbeef""#,
        );
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).unwrap_err();
        assert!(
            matches!(err, IrLoadError::ChecksumHintNotComputable { .. }),
            "a hint over a depends_on-bearing IR must fail closed, got: {err}"
        );
    }

    #[test]
    fn load_rejects_hint_bearing_ir_with_supersedes_fail_closed() {
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = ir_json(
            ops,
            r#", "supersedes": ["m_0001"], "checksum": "deadbeefdeadbeef""#,
        );
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).unwrap_err();
        assert!(
            matches!(err, IrLoadError::ChecksumHintNotComputable { .. }),
            "a hint over a supersedes-bearing IR must fail closed, got: {err}"
        );
    }

    #[test]
    fn load_rejects_hint_bearing_ir_with_non_default_flags_fail_closed() {
        // A non-default flag override (transactional:false) is outside the PR1
        // foldable domain (neutral defaults), so a hint over it fails closed.
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = ir_json(
            ops,
            r#", "flags": {"transactional": false}, "checksum": "deadbeefdeadbeef""#,
        );
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).unwrap_err();
        assert!(
            matches!(err, IrLoadError::ChecksumHintNotComputable { .. }),
            "a hint over a non-default-flags IR must fail closed, got: {err}"
        );
    }

    #[test]
    fn load_allows_depends_on_when_no_hint_is_present() {
        // The fail-closed gate is HINT-SPECIFIC: a depends_on-bearing IR with NO
        // advisory hint is fine (nothing to compare), so authoring deps is not
        // blocked — only a hint OVER an uncomputable domain is.
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = ir_json(ops, r#", "depends_on": ["m_0001"]"#);
        let reg = registry(&[("users", "app_a")]);
        assert!(
            load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg).is_ok(),
            "a depends_on-bearing IR WITHOUT a hint must load"
        );
    }

    // ── order: ir_version is checked BEFORE validate/ownership/checksum ──────

    #[test]
    fn version_gate_precedes_ownership_and_checksum() {
        // A future-version artifact that ALSO has an ownership violation + bad hint
        // must surface the VERSION error (the first fail-closed gate), proving the
        // ordering (§5.2).
        let bytes = r#"{"ir_version": 999, "name": "m", "ops": [{"op":"dropTable","table":"foreign"}], "checksum": "deadbeef"}"#;
        let reg = registry(&[("foreign", "other_app")]);
        let err = load_ir_document(bytes, "app_a", Dialect::Postgres, &reg).unwrap_err();
        assert!(matches!(err, IrLoadError::Version(_)), "version gate must precede others, got: {err}");
    }

}
