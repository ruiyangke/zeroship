//! The fail-closed IR envelope load gate (the POLICY-bound half).
//!
//! The policy-free pieces of the load gate — [`IrLoadError`], the ownership
//! checker [`enforce_ir_ownership`], the checksum helpers, and the table-collection
//! walkers — live in the [`zero_migrate_ir::load`] leaf crate and are re-exported
//! below. THIS module keeps [`load_ir_document`]: the full load chain, which
//! threads a [`SchemaScope`](crate::model::policy::SchemaScope) into the POLICY
//! validator ([`validate_ir_scoped`](crate::model::validate::validate_ir_scoped))
//! and therefore cannot live in the leaf. (Table-shape injection + author-PK
//! conformance ride on the composed `EffectivePolicy` in
//! `crate::model::table_shape::resolve_create_table_policy`, not a `PolicyProfile`.)

use std::collections::BTreeMap;

use crate::model::ir::MigrationIr;
use crate::model::validate::validate_ir_authorized;
use crate::model::validate::Dialect;

// The policy-free half of the load gate (ownership + checksum helpers + the
// `IrLoadError` taxonomy) lives in the leaf; re-export it so the engine root and
// this module name `enforce_ir_ownership`, `IrLoadError`, `UNKNOWN_OWNER`, the
// checksum helpers, etc. unchanged.
pub use zero_migrate_ir::load::*;

/// Load + GATE an IR envelope document (the fail-closed chain). Returns the
/// validated, ownership-checked [`MigrationIr`] with its `owner_app` STAMPED to
/// `deploying_app` (a spoofed/absent value in the artifact is discarded) — ready
/// for `IrAuthor::lower`.
///
/// The steps run in the security-critical order: deserialize, then `ir_version`,
/// then `validate_ir` (for `target_dialect`), then finite timeout budgets, then
/// ownership, then the checksum-hint compare. Every step is fail-closed; lowering
/// NEVER sees an artifact that failed any gate.
///
/// `registry` is the project's table→owner map; `target_dialect` is threaded from
/// the deploy backend selection (`deploy_migrate.rs` / `--engine`).
///
/// # Errors
/// [`IrLoadError`] for a malformed document, an unknown future `ir_version`, a
/// structural-validation failure, a zero (indefinite) timeout override, an
/// ownership violation, or a checksum-hint mismatch.
pub fn load_ir_document(
    bytes: &str,
    deploying_app: &str,
    target_dialect: Dialect,
    registry: &BTreeMap<String, String>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<MigrationIr, IrLoadError> {
    load_ir_document_authorized(
        bytes,
        deploying_app,
        target_dialect,
        registry,
        schema_scope,
        None,
    )
}

/// [`load_ir_document`] threaded with the charter that answers vendor authority.
///
/// A caller holding the composed policy passes it here so the structural gate reads a
/// privileged primitive's grant off the charter rather than deriving it from
/// `schema_scope`, which answers schema confinement and nothing else. The two entries
/// on [`crate::render::lower::IrAuthor`] hold that policy and call this; a caller that
/// holds none keeps the scope-derived fallback described on
/// [`VendorAuthority`](crate::model::validate::VendorAuthority).
///
/// # Errors
/// [`IrLoadError`], on the same terms as [`load_ir_document`].
pub fn load_ir_document_authorized(
    bytes: &str,
    deploying_app: &str,
    target_dialect: Dialect,
    registry: &BTreeMap<String, String>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
    authority: Option<crate::model::validate::VendorAuthority<'_>>,
) -> Result<MigrationIr, IrLoadError> {
    // 1. deserialize (closed AST + numeric domain reject malformed/lossy here).
    let mut ir: MigrationIr =
        serde_json::from_str(bytes).map_err(|e| IrLoadError::Deserialize(e.to_string()))?;

    // 2. ir_version fail-closed — BEFORE any checksum/lower.
    ir.check_ir_version()?;

    // 3. structural validation — the authoritative gate over every Expr slot, plus
    //    the schema-confinement + guard-direction gate threaded with the active
    //    [`SchemaScope`]: a Confined cross-schema op is REFUSED here, fail-closed,
    //    BEFORE lower. (The author-PK CONFORMANCE re-check is no longer
    //    threaded through a `PolicyProfile` here — that conformance is owned by the
    //    injection resolver `resolve_create_table_policy`, which the server runs
    //    over the operator's `EffectivePolicy` before this load.)
    validate_ir_authorized(&ir, target_dialect, &[], schema_scope, authority)?;

    // 3b. finite timeout budgets -- a `flags.timeout_ms` / `flags.lock_timeout_ms`
    //    of 0 is the engines' "no limit" sentinel, not a zero budget, so it
    //    disables the timeout it claims to set. Refused here so the author sees it
    //    while the artifact is still editable; the binding refusal is at apply,
    //    where the effective value is resolved and where a config-sourced zero or
    //    a hand-built `Migration` also arrives.
    enforce_ir_finite_timeouts(&ir)?;

    // 4. ownership — over the ARTIFACT's claimed owner is irrelevant; the check is
    //    against the deploying app + the project registry (fail-closed unknown).
    enforce_ir_ownership(&ir, deploying_app, registry)?;

    // 5. advisory checksum-hint compare — recompute + compare, then
    //    DROP the hint (it never folds into the authoritative checksum). Done
    //    against the artifact's claimed hint BEFORE we stamp owner_app, since the
    //    hint domain excludes owner_app anyway.
    if let Some(hint) = ir.checksum.clone() {
        // Fail closed if the hint domain is not yet fully computable for this
        // IR (a non-default flags / deps / supersedes contribution is not yet
        // foldable). Never compare a hint against a PARTIAL domain.
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

    // Server-stamp the owner (a spoofed/absent artifact value is discarded).
    // owner_app is excluded from the hint domain, so stamping it never invalidates
    // the just-verified hint.
    ir.owner_app = deploying_app.to_string();
    Ok(ir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ir::{ColType, IrColumn, Op};

    fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(t, o)| (t.to_string(), o.to_string()))
            .collect()
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
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn envelope_json(ops_json: &str, extra: &str) -> String {
        format!(r#"{{"ir_version": 1, "name": "m", "ops": {ops_json}{extra}}}"#)
    }

    // ── ir_version fail-closed on the PRODUCTION path ───────────────────────

    #[test]
    fn load_rejects_future_ir_version_before_anything_else() {
        let bytes = r#"{"ir_version": 999, "name": "m", "ops": [{"op":"dropTable","table":"t"}]}"#;
        let reg = registry(&[("t", "app_a")]);
        let err = load_ir_document(bytes, "app_a", Dialect::Postgres, &reg, None).unwrap_err();
        assert!(matches!(err, IrLoadError::Version(_)), "got: {err}");
    }

    // ── validate_ir wired as the loader's gate ──────────────────────────────
    // A hostile IR envelope driven through the REAL loader (not the validator unit
    // test) must have the structural gate FIRE on the production path.

    #[test]
    fn load_runs_validate_ir_on_the_production_path() {
        // A createTable whose Check references a column NOT on the table — rule
        // (c). The gate must reject it via validate_ir on the real load path.
        let ops = r#"[{"op":"createTable","name":"users","columns":[{"name":"first","type":"text"}],"constraints":[{"kind":{"kind":"check","expr":{"node":"unaryOp","op":"isNotNull","operand":{"node":"colRef","name":"ghost"}}}}]}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).unwrap_err();
        match err {
            IrLoadError::Validate(ae) => {
                assert_eq!(ae.code, crate::model::validate::CODE_UNSUPPORTED);
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
        let bytes = envelope_json(ops, "");
        let reg = registry(&[("users", "app_a")]);
        // PG accepts (validation OK), SQLite rejects.
        assert!(load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).is_ok());
        let err = load_ir_document(&bytes, "app_a", Dialect::Sqlite, &reg, None).unwrap_err();
        assert!(matches!(err, IrLoadError::Validate(_)), "got: {err}");
    }

    // ── deserialize gate: unknown node tag / out-of-domain scalar ───────────

    // -- finite timeout budgets on the PRODUCTION load path ------------------

    #[test]
    fn load_rejects_a_zero_timeout_override_on_either_field() {
        for field in ["timeout_ms", "lock_timeout_ms"] {
            let ops = r#"[{"op":"dropTable","table":"t"}]"#;
            let bytes = envelope_json(ops, &format!(r#","flags":{{"{field}":0}}"#));
            let reg = registry(&[("t", "app_a")]);
            let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None)
                .expect_err("a zero timeout override disables the timeout it claims to set");
            assert_eq!(
                err,
                IrLoadError::IndefiniteTimeoutFlag { field },
                "the load gate must name the zero-valued override"
            );
        }
    }

    #[test]
    fn load_accepts_a_finite_timeout_override_on_either_field() {
        for field in ["timeout_ms", "lock_timeout_ms"] {
            let ops = r#"[{"op":"dropTable","table":"t"}]"#;
            let bytes = envelope_json(ops, &format!(r#","flags":{{"{field}":1}}"#));
            let reg = registry(&[("t", "app_a")]);
            load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None)
                .expect("the smallest expressible budget is finite and loads");
        }
    }

    #[test]
    fn load_rejects_unknown_expr_node_tag_at_deserialize() {
        let ops = r#"[{"op":"delete","table":"users","where":{"node":"evilNode"}}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).unwrap_err();
        assert!(matches!(err, IrLoadError::Deserialize(_)), "got: {err}");
    }

    #[test]
    fn load_rejects_lossy_numeric_scalar_at_deserialize() {
        let ops =
            r#"[{"op":"insert","table":"users","columns":["a"],"rows":[[9007199254740992]]}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).unwrap_err();
        match err {
            IrLoadError::Deserialize(msg) => {
                assert!(msg.contains("IrValue"), "got: {msg}");
            }
            other => panic!("expected Deserialize, got: {other}"),
        }
    }

    // ── ownership fail-closed ───────────────────────────────────────────────

    #[test]
    fn load_refuses_op_on_another_apps_table() {
        let ops = r#"[{"op":"dropColumn","table":"users","column":"x"}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[("users", "app_owner")]); // owned by a DIFFERENT app
        let err =
            load_ir_document(&bytes, "app_intruder", Dialect::Postgres, &reg, None).unwrap_err();
        match err {
            IrLoadError::NotTableOwner {
                table,
                owner,
                deploying_app,
                op_index,
            } => {
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
        let ops =
            r#"[{"op":"delete","table":"never_declared","where":{"node":"literal","value":true}}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[("users", "app_a")]); // no entry for `never_declared`
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).unwrap_err();
        match err {
            IrLoadError::NotTableOwner { table, owner, .. } => {
                assert_eq!(table, "never_declared");
                assert_eq!(owner, UNKNOWN_OWNER, "unknown owner must fail closed");
            }
            other => panic!("expected NotTableOwner (unknown-owner), got: {other}"),
        }
    }

    #[test]
    fn load_refuses_bare_name_drop_index_fail_closed() {
        // fail-closed: a bare-name DropIndex (`table: None`) has no
        // ownership-checkable target, so the ownership pass `continue`d over it —
        // letting a hostile IR envelope `{op:"dropIndex", name:"<other_app_index>"}`
        // (no table hint) DROP another app's index cross-tenant. The fix refuses a
        // bare-name DropIndex at validate time (no name→owner registry resolver
        // exists), so the bypass is closed. An intruder targeting another app's
        // index by NAME is now REFUSED, not silently applied.
        let ops = r#"[{"op":"dropIndex","name":"victim_secret_idx"}]"#;
        let bytes = envelope_json(ops, "");
        // The registry knows the victim app owns tables; the intruder owns nothing.
        let reg = registry(&[("victim_secrets", "app_victim")]);
        let err =
            load_ir_document(&bytes, "app_intruder", Dialect::Postgres, &reg, None).unwrap_err();
        match err {
            IrLoadError::Validate(ae) => {
                assert_eq!(ae.code, crate::model::validate::CODE_UNSUPPORTED);
                assert_eq!(
                    ae.kind,
                    Some(crate::model::validate::UnsupportedKind::Op),
                    "a bare-name DropIndex is an UNSUPPORTED op (kind:op), fail-closed"
                );
                assert_eq!(ae.op_index, 0);
            }
            other => panic!("expected a fail-closed Validate(UNSUPPORTED op), got: {other}"),
        }
    }

    #[test]
    fn load_allows_table_hinted_drop_index_owned_by_deployer() {
        // The remedy: a DropIndex carrying its owning-table hint IS ownership-
        // checkable (the table's owner resolves through the registry), so a
        // table-hinted drop on a table the deployer owns is allowed — the fix
        // refuses ONLY the un-checkable bare-name form.
        let ops = r#"[{"op":"dropIndex","name":"mine_idx","table":"mine"}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[("mine", "app_a")]);
        load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None)
            .expect("a table-hinted DropIndex on an owned table is allowed");
    }

    #[test]
    fn load_refuses_table_hinted_drop_index_on_another_apps_table() {
        // And a table-hinted DropIndex against ANOTHER app's table is refused by the
        // ownership pass (the table hint resolves to a foreign owner).
        let ops = r#"[{"op":"dropIndex","name":"theirs_idx","table":"theirs"}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[("theirs", "app_owner")]);
        let err =
            load_ir_document(&bytes, "app_intruder", Dialect::Postgres, &reg, None).unwrap_err();
        match err {
            IrLoadError::NotTableOwner { table, owner, .. } => {
                assert_eq!(table, "theirs");
                assert_eq!(owner, "app_owner");
            }
            other => panic!("expected NotTableOwner, got: {other}"),
        }
    }

    #[test]
    fn load_allows_create_table_of_a_new_table_by_its_declarer() {
        // A createTable establishes ownership for its NEW table (the declarer);
        // a following op on that same new table is then allowed.
        let ops = r#"[{"op":"createTable","name":"fresh","columns":[{"name":"first","type":"text"}]},{"op":"addColumn","table":"fresh","column":"x","type":"int"}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[]); // `fresh` is brand new — not in the project registry
        let ir = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).unwrap();
        assert_eq!(ir.owner_app, "app_a", "owner_app must be server-stamped");
    }

    #[test]
    fn load_registers_platform_exact_create_table_for_structural_attachments() {
        // Platform resolved createTable carries exactly the author fields (no
        // confined system fields) and may use a composite PK. Ownership is
        // shape-agnostic: same-file structural attachments must resolve against
        // the table registered by the createTable pre-pass.
        let ops = r#"[
            {"op":"createTable","name":"platform_registry","schema":"zero_migrate","columns":[
                {"name":"app_id","type":"text","nullable":false},
                {"name":"route","type":"text","nullable":false},
                {"name":"target","type":"text","nullable":false}
            ],"primaryKey":["app_id","route"],"constraints":[],"indexes":[]},
            {"op":"setRls","table":"platform_registry","schema":"zero_migrate","enabled":true,"forced":true},
            {"op":"createPolicy","name":"tenant_isolation","table":"platform_registry",
                "schema":"zero_migrate","forCmd":"all",
                "using":{"node":"literal","value":true}},
            {"op":"comment","target":{"kind":"table","schema":"zero_migrate",
                "name":"platform_registry"},"comment":"Platform route registry"},
            {"op":"createIndex","table":"platform_registry","schema":"zero_migrate",
                "name":"platform_registry_target_idx",
                "columns":[{"kind":"column","name":"target"}]},
            {"op":"createFunction","name":"platform_registry_touch","schema":"zero_migrate",
                "returns":"trigger","language":"plpgsql","replace":true,
                "body":"BEGIN RETURN NEW; END;"},
            {"op":"createTrigger","name":"platform_registry_touch_trg",
                "table":"platform_registry","schema":"zero_migrate","timing":"before",
                "events":["update"],"forEach":"row",
                "action":{"kind":"executeFunction","name":"platform_registry_touch"}}
        ]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[]);
        let scope = crate::model::policy::SchemaScope::Allowlist(vec!["zero_migrate".into()]);
        let ir = load_ir_document(&bytes, "platform", Dialect::Postgres, &reg, Some(&scope))
            .expect("platform exact createTable must register ownership for same-file attachments");
        assert_eq!(ir.owner_app, "platform");
    }

    #[test]
    fn load_refuses_unknown_table_structural_attach_fail_closed() {
        let ops =
            r#"[{"op":"setRls","table":"never_declared","schema":"zero_migrate","enabled":true}]"#;
        let bytes = envelope_json(ops, "");
        let scope = crate::model::policy::SchemaScope::Allowlist(vec!["zero_migrate".into()]);
        let err = load_ir_document(
            &bytes,
            "platform",
            Dialect::Postgres,
            &registry(&[]),
            Some(&scope),
        )
        .expect_err("attach to an unowned/unknown table must fail closed");
        match err {
            IrLoadError::NotTableOwner {
                table,
                owner,
                op_index,
                ..
            } => {
                assert_eq!(table, "never_declared");
                assert_eq!(owner, UNKNOWN_OWNER);
                assert_eq!(op_index, 0);
            }
            other => panic!("expected NotTableOwner for unknown-table attach, got: {other}"),
        }
    }

    #[test]
    fn load_confined_resolved_create_table_ownership_is_unchanged() {
        let raw = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: String::new(),
            ops: vec![
                create_table("fresh"),
                Op::AddColumn {
                    table: "fresh".into(),
                    column: "x".into(),
                    ty: ColType::Int,
                    nullable: None,
                    default: None,
                    value_format: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                    schema: None,
                    existence_guard: None,
                },
            ],
            flags: crate::model::ir::IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let resolved = crate::model::table_shape::resolve_create_table_policy(
            &raw,
            &crate::test_fixtures::confined_charter(),
            "app",
        )
        .expect("confined createTable resolves system fields");
        let bytes = serde_json::to_string(&resolved).expect("resolved IR serializes");
        load_ir_document(&bytes, "app_a", Dialect::Postgres, &registry(&[]), None)
            .expect("confined resolved createTable still registers ownership");
    }

    #[test]
    fn load_allows_dml_positioned_before_its_create_table_in_same_migration() {
        // ORDER-INDEPENDENCE: the createTable ownership pre-pass
        // registers ALL createTable names BEFORE the per-op check, so an op that
        // appears POSITIONALLY BEFORE its createTable still passes ownership — the
        // table is pre-registered to the deploying app. This is who-may-touch, not
        // apply-order validity (the executor enforces apply order). It is NOT a
        // security relaxation: the table is still owned by the deploying app, and a
        // collision with ANOTHER app's table is still refused (see the test below).
        let ops = r#"[{"op":"insert","table":"fresh","columns":["id"],"rows":[[1]]},{"op":"createTable","name":"fresh","columns":[{"name":"id","type":"int"}]}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[]); // `fresh` is brand new — declared later in THIS migration
        let ir = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None)
            .expect("DML before its createTable passes ownership (order-independent pre-pass)");
        assert_eq!(ir.owner_app, "app_a");
    }

    #[test]
    fn load_refuses_dml_before_create_table_when_table_belongs_to_another_app() {
        // The order-independence above is NOT a relaxation: if the table named by
        // the pre-positioned DML is ALREADY owned by a DIFFERENT app, the per-op
        // check still refuses it (the pre-pass only inserts when ABSENT, so the
        // existing foreign owner is never overwritten). A createTable colliding
        // with that foreign table is likewise refused.
        let ops = r#"[{"op":"insert","table":"users","columns":["id"],"rows":[[1]]},{"op":"createTable","name":"users","columns":[{"name":"id","type":"int"}]}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[("users", "app_owner")]);
        let err =
            load_ir_document(&bytes, "app_intruder", Dialect::Postgres, &reg, None).unwrap_err();
        match err {
            IrLoadError::NotTableOwner {
                table,
                owner,
                op_index,
                ..
            } => {
                assert_eq!(table, "users");
                assert_eq!(
                    owner, "app_owner",
                    "foreign owner must not be overwritten by the pre-pass"
                );
                assert_eq!(
                    op_index, 0,
                    "the FIRST op (the pre-positioned DML) is the one refused"
                );
            }
            other => panic!("expected NotTableOwner, got: {other}"),
        }
    }

    #[test]
    fn load_refuses_create_table_colliding_with_another_apps_table() {
        // A createTable for a table ALREADY owned by another app does not silently
        // take ownership — the per-op check refuses it (the working registry only
        // inserts when ABSENT).
        let ops =
            r#"[{"op":"createTable","name":"users","columns":[{"name":"first","type":"text"}]}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[("users", "app_owner")]);
        let err =
            load_ir_document(&bytes, "app_intruder", Dialect::Postgres, &reg, None).unwrap_err();
        assert!(
            matches!(err, IrLoadError::NotTableOwner { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn enforce_ir_ownership_unit_create_then_use() {
        let ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: String::new(),
            ops: vec![
                create_table("fresh"),
                Op::DropColumn {
                    table: "fresh".into(),
                    column: "x".into(),
                    schema: None,
                    existence_guard: None,
                },
            ],
            flags: crate::model::ir::IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        assert!(enforce_ir_ownership(&ir, "app_a", &registry(&[])).is_ok());
    }

    // ── checksum-hint compare wired ─────────────────────────────────────────

    /// FROZEN-HEX hint golden: pin the hint-domain
    /// checksum for a FIXED IR to a hard-coded literal, then drive the loader
    /// with that literal embedded in the IR envelope bytes. This breaks the
    /// self-reference of [`load_accepts_a_correct_checksum_hint`] (which computes
    /// the "correct" hint with the very function under test): here the accepted
    /// value is an INDEPENDENT literal captured once, so a drift in EITHER
    /// `recompute_hint_domain_checksum` (the hint-domain fold — incl. how it
    /// folds `MigrationFlags::default()` when the override is all-None) OR the
    /// loader's compare is caught. The JS builder MUST emit this same hex for
    /// this IR; this frozen literal is the independent oracle.
    ///
    /// If this hex changes, the hint-domain wire format drifted — the JS `op.*`
    /// author would emit a hint the engine rejects. Not allowed without a
    /// deliberate, matched break on both sides.
    ///
    /// Re-captured when the lock-safety envelope added `lock_timeout_ms` to
    /// `MigrationFlags`: the hint domain folds `MigrationFlags::default()`, whose
    /// canonical-JSON image gained the `lock_timeout_ms: null` key, so this hex
    /// moved BY CONSTRUCTION. The JS author reuses this same Rust crate's
    /// serialization, so both sides move together — a DELIBERATE, matched break.
    #[test]
    fn load_accepts_a_frozen_checksum_hint_golden() {
        // A fixed dropTable IR with all-default flags/deps/supersedes/precond.
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        // Hard-coded literal — NOT computed by the function under test.
        // Re-captured when the IR checksum domain-separator tag was set to
        // `zero-migrate/of_ir/v1` (`model/migration.rs`): the hint-domain fold
        // hashes that tag, so this hex moved BY CONSTRUCTION. The JS author reuses
        // this same Rust crate's serialization, so both sides move together — a
        // deliberate, matched break.
        const FROZEN_HINT: &str =
            "8adb4d9360aa90f73145071a2ce0c769793beee4cc17d136af7e52098c766bb4";
        let bytes = envelope_json(ops, &format!(r#", "checksum": "{FROZEN_HINT}""#));
        let reg = registry(&[("users", "app_a")]);
        let loaded = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None)
            .expect("loader must accept the frozen-hex hint for this fixed IR");
        assert_eq!(loaded.checksum.as_deref(), Some(FROZEN_HINT));
    }

    #[test]
    fn load_accepts_a_correct_checksum_hint() {
        // Build the IR, compute the CORRECT hint domain checksum, embed it, and
        // assert the load accepts it.
        let ops_vec = vec![Op::DropTable {
            table: "users".into(),
            cascade: None,
            schema: None,
            existence_guard: None,
        }];
        let ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: String::new(),
            ops: ops_vec,
            flags: crate::model::ir::IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let correct = recompute_hint_domain_checksum(&ir);
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = envelope_json(ops, &format!(r#", "checksum": "{}""#, correct.as_str()));
        let reg = registry(&[("users", "app_a")]);
        let loaded = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).unwrap();
        // The hint is carried through (the engine recomputes; it does not strip it).
        assert_eq!(loaded.checksum.as_deref(), Some(correct.as_str()));
    }

    #[test]
    fn load_rejects_a_wrong_checksum_hint() {
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = envelope_json(ops, r#", "checksum": "deadbeefdeadbeef""#);
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).unwrap_err();
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
        // The hint is advisory and need not be present.
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = envelope_json(ops, "");
        let reg = registry(&[("users", "app_a")]);
        assert!(load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).is_ok());
    }

    // ── hint domain is not yet fully computable (deps/supersedes/flags) ──────
    // The recompute currently folds ONLY ops+preconditions (neutral flags, empty
    // deps). A hint-bearing IR that ALSO carries depends_on/supersedes or
    // non-default flags must NOT be silently compared against a PARTIAL domain
    // (that would both false-reject a spec-correct hint AND false-accept tampering
    // of the un-folded fields). The loader fails closed with a clear error.

    #[test]
    fn load_rejects_hint_bearing_ir_with_depends_on_fail_closed() {
        // A hint over an IR that carries a depends_on entry: the hint domain
        // (per ir.rs doc) includes depends_on, but the recompute cannot fold it,
        // so the loader refuses rather than compare a partial domain.
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = envelope_json(
            ops,
            r#", "depends_on": ["m_0001"], "checksum": "deadbeefdeadbeef""#,
        );
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).unwrap_err();
        assert!(
            matches!(err, IrLoadError::ChecksumHintNotComputable { .. }),
            "a hint over a depends_on-bearing IR must fail closed, got: {err}"
        );
    }

    #[test]
    fn load_rejects_hint_bearing_ir_with_supersedes_fail_closed() {
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = envelope_json(
            ops,
            r#", "supersedes": ["m_0001"], "checksum": "deadbeefdeadbeef""#,
        );
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).unwrap_err();
        assert!(
            matches!(err, IrLoadError::ChecksumHintNotComputable { .. }),
            "a hint over a supersedes-bearing IR must fail closed, got: {err}"
        );
    }

    #[test]
    fn load_rejects_hint_bearing_ir_with_non_default_flags_fail_closed() {
        // A non-default flag override (transactional:false) is outside the
        // foldable domain (neutral defaults), so a hint over it fails closed.
        let ops = r#"[{"op":"dropTable","table":"users"}]"#;
        let bytes = envelope_json(
            ops,
            r#", "flags": {"transactional": false}, "checksum": "deadbeefdeadbeef""#,
        );
        let reg = registry(&[("users", "app_a")]);
        let err = load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).unwrap_err();
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
        let bytes = envelope_json(ops, r#", "depends_on": ["m_0001"]"#);
        let reg = registry(&[("users", "app_a")]);
        assert!(
            load_ir_document(&bytes, "app_a", Dialect::Postgres, &reg, None).is_ok(),
            "a depends_on-bearing IR WITHOUT a hint must load"
        );
    }

    // ── order: ir_version is checked BEFORE validate/ownership/checksum ──────

    #[test]
    fn version_gate_precedes_ownership_and_checksum() {
        // A future-version artifact that ALSO has an ownership violation + bad hint
        // must surface the VERSION error (the first fail-closed gate), proving the
        // ordering.
        let bytes = r#"{"ir_version": 999, "name": "m", "ops": [{"op":"dropTable","table":"foreign"}], "checksum": "deadbeef"}"#;
        let reg = registry(&[("foreign", "other_app")]);
        let err = load_ir_document(bytes, "app_a", Dialect::Postgres, &reg, None).unwrap_err();
        assert!(
            matches!(err, IrLoadError::Version(_)),
            "version gate must precede others, got: {err}"
        );
    }
}
