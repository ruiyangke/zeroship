//! Host-authoring lower: turn a pure-JS IR envelope into the engine's ordered
//! [`AppliedPlan`](zero_migrate::AppliedPlan), **folding the single authoritative
//! `Checksum::of_ir` in Rust** (never in JS).
//!
//! ## Why the addon lowers (not the facade)
//! The pure-JS host recorder can only produce the dialect-neutral IR op envelope. The
//! ops→SQL LOWER (`IrAuthor::load_and_lower`) is a Rust-only engine step (it routes
//! every op through the shared snapshot-builder + DDL emitter, `render/lower.rs`). So
//! the facade hands the envelope + provenance here; this module runs the SAME
//! fail-closed LOAD GATE + LOWER the IR envelope deploy path runs
//! (`IrAuthor::new(schema, app, dialect).load_and_lower(bytes, app, &registry, &live,
//! None)`), and the resulting `Migration.checksum` is `Checksum::of_ir` folded by
//! Rust over the canonical op list + the server-stamped `owner_app`. The JS side emits
//! ops; Rust owns the checksum — exactly the invariant the pure-JS recorder
//! (`packages/zero-migrate/src/internal/recorder.ts`) preserves.
//!
//! ## Live catalog input
//! Host apply and plan-aware status read the selected database catalog first and
//! lower against those live tables, columns, and indexes. This makes existing-table
//! changes use authoritative column types and unique-index facts. The DB-free plan
//! helper intentionally uses an empty live schema and remains a structural preview.

use std::collections::BTreeMap;

use zero_migrate::apply::journal::{AppliedEntry, Phase};
use zero_migrate::model::ir::{MigrationIr, Op};
use zero_migrate::model::migration::Migration;
use zero_migrate::model::table_shape::confined_no_inject_policy;
use zero_migrate::ops::status::PlanStatusManifest;
#[cfg(any(feature = "napi", test))]
use zero_migrate::ops::status::{AppliedPlanStatus, ReconciledPlanState};
use zero_migrate::{
    effective_policy_from_ceiling_toml, fold_ops_onto, resolve_create_table_policy, FoldError,
    GuardConfig, IrAuthor, LiveSchema, LoweredArtifact, SqlDialect,
};

/// Map the wire dialect spelling to the render [`SqlDialect`]. Unknown → `Err`.
fn parse_sql_dialect(s: &str) -> Result<SqlDialect, String> {
    match s {
        "postgres" => Ok(SqlDialect::Postgres),
        "sqlite" => Ok(SqlDialect::Sqlite),
        "mysql" => Ok(SqlDialect::Mysql),
        other => Err(format!(
            "unknown dialect {other:?} (expected postgres|sqlite|mysql)"
        )),
    }
}

/// Require every authored prefix plan to be fully, exactly applied before its IR
/// may contribute logical column contracts to the current migration. The status
/// fold is authoritative for net rollbacks, inflight/partial work, checksum drift,
/// dependencies, and terminal online-contract resolutions.
#[cfg(any(feature = "napi", test))]
pub(crate) fn require_applied_prefix(
    manifests: &[PlanStatusManifest],
    prior_count: usize,
    status: &AppliedPlanStatus,
) -> Result<(), String> {
    let prefix = manifests.get(..prior_count).ok_or_else(|| {
        format!(
            "authored migration prefix has {prior_count} entries but lowering returned only {} plans",
            manifests.len()
        )
    })?;
    let current = manifests.get(prior_count).ok_or_else(|| {
        "authored migration set did not include a current migration plan".to_string()
    })?;
    let current_state = status
        .plans
        .iter()
        .find(|plan| plan.version == current.version)
        .map(|plan| plan.state)
        .ok_or_else(|| {
            format!(
                "current migration {:?} ({}) is absent from journal reconciliation",
                current.name,
                current.version.as_str()
            )
        })?;
    if current_state != ReconciledPlanState::Applied {
        if let Some(unexpected) = status.unexpected_journal.first() {
            return Err(format!(
                "authored migration prefix is incomplete: net-applied journal step {} was not supplied",
                unexpected.version
            ));
        }
    }
    for manifest in prefix {
        let reconciled = status
            .plans
            .iter()
            .find(|plan| plan.version == manifest.version)
            .ok_or_else(|| {
                format!(
                    "authored prior migration {:?} ({}) is absent from journal reconciliation",
                    manifest.name,
                    manifest.version.as_str()
                )
            })?;
        if reconciled.state != ReconciledPlanState::Applied {
            return Err(format!(
                "authored prior migration {:?} ({}) is not fully applied (state: {})",
                manifest.name,
                manifest.version.as_str(),
                reconciled.state.as_str()
            ));
        }
    }
    Ok(())
}

/// Run the fail-closed IR envelope LOAD GATE + LOWER over an envelope, returning the
/// complete guarded artifact. Its [`AppliedPlan`](zero_migrate::AppliedPlan) retains
/// every ordered `Ddl`, `Dml`, `Backfill`, and `OnlineRename` step.
///
/// - `envelope_json` — the pure-JS IR envelope bytes (`{ ir_version, name,
///   ops }`). The envelope MUST NOT carry `owner_app` (a provenance field the
///   builder can't be trusted to set); it is stamped from `owner_app` here.
/// - `owner_app` — the deploying app id (`app_…`); the ownership check + the
///   `owner_app` stamped onto every emitted `Migration` + folded into its checksum.
/// - `project_schema` — the confined project schema the lower pins ops to.
/// - `dialect` — `"postgres" | "sqlite" | "mysql"`.
/// - `registry_json` — the project's `{ "table": "owner_app", … }` map (drives the
///   ownership check); an empty object `{}` on a fresh single-app project.
/// - `policy_ceiling_toml` — the **policy input**: the host's `RootCeiling` document
///   (TOML) that drives table-shape injection. The engine constructs NO default
///   ceiling: `None` injects nothing (the author-owned shape passes through); `Some`
///   composes the ceiling (against an empty draft) into the `EffectivePolicy` whose
///   `injects_for(object)` drives column/index/PK injection. The monorepo caller
///   passes zeroship's confined ceiling here (Phase 3).
///
/// # Errors
/// A JSON `Err(message)` on: an unknown dialect, a malformed registry, a malformed
/// policy ceiling document, the load gate refusing the artifact (malformed / future
/// `ir_version` / structural reject / ownership violation / checksum-hint mismatch),
/// or a lower failure — never a panic.
pub fn lower_envelope_to_plan(
    envelope_json: &str,
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    policy_ceiling_toml: Option<&str>,
) -> Result<LoweredArtifact, String> {
    lower_envelope_to_plan_with_live(
        envelope_json,
        owner_app,
        project_schema,
        dialect,
        registry_json,
        policy_ceiling_toml,
        &LiveSchema::default(),
    )
}

/// Lower an IR envelope against a catalog-derived live schema. Host apply and
/// status use this form so live column types, column references, index ownership,
/// and unique-index approval gates are authoritative at the apply seam.
pub fn lower_envelope_to_plan_with_live(
    envelope_json: &str,
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    policy_ceiling_toml: Option<&str>,
    live: &LiveSchema,
) -> Result<LoweredArtifact, String> {
    lower_envelope_to_plan_with_live_and_resolved_ir(
        envelope_json,
        owner_app,
        project_schema,
        dialect,
        registry_json,
        policy_ceiling_toml,
        live,
    )
    .map(|(artifact, _resolved)| artifact)
}

#[allow(clippy::too_many_arguments)]
fn lower_envelope_to_plan_with_live_and_resolved_ir(
    envelope_json: &str,
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    policy_ceiling_toml: Option<&str>,
    live: &LiveSchema,
) -> Result<(LoweredArtifact, MigrationIr), String> {
    let dialect = parse_sql_dialect(dialect)?;
    let registry: BTreeMap<String, String> = serde_json::from_str(registry_json)
        .map_err(|e| format!("registry_json is not a string→string map: {e}"))?;

    // **System-shape fold (mirrors the pure-JS recorder's fold).** The
    // pure-JS host recorder drains ONLY the author-declared columns — the
    // platform-managed system fields (`id`/`created_at`/`updated_at`/`version`/…)
    // + the `["id"]` PRIMARY KEY are injected by `resolve_create_table_policy` off the
    // host-supplied POLICY CEILING (the `EffectivePolicy`'s `injects_for`), NOT by the
    // JS DSL. The engine hardcodes no ceiling: the monorepo passes zeroship's confined
    // ceiling. The native IR envelope on disk is post-fold (the recorder folds before
    // writing); the host path folds here so the addon lowers the SAME resolved shape —
    // otherwise the confined table-shape guard rejects a createTable missing its system
    // columns (TABLE_SHAPE_POLICY). This is a pure structural resolve; the JS side never
    // sees the system fields (they are platform-owned).
    //
    // Injection and guard share this one composed `EffectivePolicy`; the load gate
    // no longer takes a separate `PolicyProfile` (retired in Cut 3).
    let effective = match policy_ceiling_toml {
        Some(toml) => effective_policy_from_ceiling_toml(toml)?,
        // No ceiling supplied ⇒ inject nothing (author-owned shape). The engine
        // constructs no default inject ceiling of its own, but still supplies the
        // scoped confined namespace grants the guard requires.
        None => confined_no_inject_policy(project_schema)?,
    };
    let raw_ir: MigrationIr = serde_json::from_str(envelope_json)
        .map_err(|e| format!("envelope is not a MigrationIr document: {e}"))?;
    let resolved = resolve_create_table_policy(&raw_ir, &effective)
        .map_err(|e| format!("table-shape resolve failed: {e}"))?;
    let resolved_bytes = serde_json::to_string(&resolved)
        .map_err(|e| format!("resolved IR failed to serialize: {e}"))?;

    let author = IrAuthor::new(project_schema, owner_app, dialect);

    // Use the GUARDED lower — the SAME entry the IR envelope deploy path uses
    // (`load_and_lower_guarded` in `render/lower.rs`). This matters for JOURNAL
    // IDENTITY: `load_and_lower_guarded` assembles an `AppliedPlan` and stamps the
    // dialect-neutral `authoritative_ir_checksum` (the `Checksum::of_ir` ANCHOR)
    // onto EVERY DDL step's `Migration.checksum`. The non-guarded `load_and_lower`
    // → `lower()` skips `assemble_plan`, so its steps carry per-step checksums
    // instead of the shared anchor — which diverges from the reference journal
    // (the DB-backed oracle caught exactly this). Guarded here ⇒ the host journal's
    // checksum column is byte-identical to the reference path's.
    let guard_cfg = GuardConfig::from_policy(effective, dialect);
    let artifact = author
        .load_and_lower_guarded(&resolved_bytes, owner_app, &registry, live, &guard_cfg)
        .map_err(|e| e.to_string())?;
    Ok((artifact, resolved))
}

/// Lower an ordered migration set against one catalog snapshot.
///
/// # Errors
/// The first envelope that fails the guarded load/lower gate is returned.
#[allow(clippy::too_many_arguments)]
pub fn lower_ordered_envelopes_to_plans(
    envelope_json: &[String],
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    policy_ceiling_toml: Option<&str>,
    snapshot: zero_migrate::model::snapshot::SchemaSnapshot,
    journal_entries: &[AppliedEntry],
    resolved_contracts: &[zero_migrate::apply::journal::ResolvedPendingContract],
) -> Result<Vec<LoweredArtifact>, String> {
    lower_ordered_envelopes_to_plans_inner(
        envelope_json,
        owner_app,
        project_schema,
        dialect,
        registry_json,
        policy_ceiling_toml,
        snapshot,
        journal_entries,
        resolved_contracts,
        false,
    )
}

/// Apply-strict peer of [`lower_ordered_envelopes_to_plans`].
///
/// Status may reconstruct an historical PostgreSQL online rename from related
/// journal evidence. Apply must additionally prove that every reconstructed
/// rename has the complete exact resumable or terminal evidence before accepting
/// its plan. Ordered apply lowering uses this entrypoint so adding authored prefix
/// envelopes cannot weaken the existing single-envelope replay gate.
#[allow(clippy::too_many_arguments)]
pub fn lower_ordered_envelopes_to_plans_for_apply(
    envelope_json: &[String],
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    policy_ceiling_toml: Option<&str>,
    snapshot: zero_migrate::model::snapshot::SchemaSnapshot,
    journal_entries: &[AppliedEntry],
    resolved_contracts: &[zero_migrate::apply::journal::ResolvedPendingContract],
) -> Result<Vec<LoweredArtifact>, String> {
    lower_ordered_envelopes_to_plans_inner(
        envelope_json,
        owner_app,
        project_schema,
        dialect,
        registry_json,
        policy_ceiling_toml,
        snapshot,
        journal_entries,
        resolved_contracts,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn lower_ordered_envelopes_to_plans_inner(
    envelope_json: &[String],
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    policy_ceiling_toml: Option<&str>,
    snapshot: zero_migrate::model::snapshot::SchemaSnapshot,
    journal_entries: &[AppliedEntry],
    resolved_contracts: &[zero_migrate::apply::journal::ResolvedPendingContract],
    strict_historical_apply: bool,
) -> Result<Vec<LoweredArtifact>, String> {
    let dialect = parse_sql_dialect(dialect)?;
    let mut registry: BTreeMap<String, String> = serde_json::from_str(registry_json)
        .map_err(|e| format!("registry_json is not a string→string map: {e}"))?;
    let base_snapshot = snapshot;
    let mut live = live_schema_with_ownership(base_snapshot.clone(), owner_app, &registry);
    let mut pending_ops = Vec::new();
    let mut artifacts = Vec::with_capacity(envelope_json.len());

    for envelope in envelope_json {
        let raw: MigrationIr = serde_json::from_str(envelope)
            .map_err(|error| format!("envelope is not a MigrationIr document: {error}"))?;
        let historical_contracts: Vec<_> = resolved_contracts
            .iter()
            .filter(|terminal| {
                terminal.contract.owner_app.as_deref() == Some(owner_app)
                    && ops_contain_contract_rename(&raw.ops, dialect, &terminal.contract)
            })
            .collect();
        let effective_registry = serde_json::to_string(&registry)
            .map_err(|e| format!("effective ownership registry failed to serialize: {e}"))?;
        let initial = lower_envelope_to_plan_with_live_and_resolved_ir(
            envelope,
            owner_app,
            project_schema,
            dialect_name(dialect),
            &effective_registry,
            policy_ceiling_toml,
            &live,
        );
        let (artifact, resolved) = match initial {
            Ok(lowered) => lowered,
            Err(original_error)
                if dialect == SqlDialect::Postgres
                    && (is_historical_rename_lower_error(&original_error)
                        || !historical_contracts.is_empty()) =>
            {
                // A rename that has already expanded legitimately sees both old
                // and new columns; after contract completion it sees only the new
                // column. Reconstruct the pre-rename catalog view solely for
                // status lowering, then require journal evidence for the derived
                // plan before accepting it. A genuinely fresh collision or absent
                // source therefore still returns the original fail-closed error.
                let mut historical_live = live.clone();
                if !normalize_historical_renames(
                    &mut historical_live,
                    &raw.ops,
                    dialect,
                    project_schema,
                    owner_app,
                    resolved_contracts,
                )? {
                    return Err(original_error);
                }
                let mut historical_registry = registry.clone();
                for terminal in historical_contracts {
                    historical_registry
                        .insert(terminal.contract.table.clone(), owner_app.to_string());
                }
                let historical_registry_json = serde_json::to_string(&historical_registry)
                    .map_err(|error| {
                        format!("historical ownership registry failed to serialize: {error}")
                    })?;
                let lowered = lower_envelope_to_plan_with_live_and_resolved_ir(
                    envelope,
                    owner_app,
                    project_schema,
                    dialect_name(dialect),
                    &historical_registry_json,
                    policy_ceiling_toml,
                    &historical_live,
                )?;
                if strict_historical_apply {
                    validate_historical_apply_evidence(
                        &lowered.0,
                        &live,
                        journal_entries,
                        resolved_contracts,
                    )
                    .map_err(|evidence_error| {
                        format!("{original_error}; historical replay refused: {evidence_error}")
                    })?;
                }
                if plan_has_no_journal_evidence(&lowered.0, journal_entries)? {
                    return Err(original_error);
                }
                lowered
            }
            Err(error) => return Err(error),
        };

        let projection_ops = ops_without_completed_journal_evidence(&artifact, journal_entries)?;
        if !projection_ops.is_empty() {
            for (op, inflight) in projection_ops {
                let mut candidate = pending_ops.clone();
                candidate.push(op.clone());
                match fold_ops_onto(&base_snapshot, &candidate, dialect, project_schema) {
                    Ok(_) => pending_ops = candidate,
                    Err(error)
                        if inflight
                            && inflight_projection_already_reflected(
                                &base_snapshot,
                                &op,
                                &error,
                            ) =>
                    {
                        // MySQL records `started` before running non-transactional
                        // DDL. The catalog can therefore show either side of the
                        // operation after a crash. When its postcondition is already
                        // visible, keep that live shape; status will still reconcile
                        // the matching journal step as inflight.
                    }
                    Err(error) => {
                        return Err(format!(
                            "failed to project pending schema after envelope {:?}: {error}",
                            resolved.name
                        ));
                    }
                }
                advance_ownership_registry(
                    &mut registry,
                    std::slice::from_ref(&op),
                    dialect,
                    owner_app,
                );
            }
            let projected = fold_ops_onto(&base_snapshot, &pending_ops, dialect, project_schema)
                .map_err(|error| {
                    format!(
                        "failed to rebuild projected schema after envelope {:?}: {error}",
                        resolved.name
                    )
                })?;
            let logical_columns = live.logical_columns.clone();
            live = live_schema_with_ownership(projected, owner_app, &registry);
            live.logical_columns = logical_columns;
        }
        live.advance_logical_columns(&resolved, dialect, project_schema, None)
            .map_err(|error| {
                format!(
                    "failed to advance logical project schema after envelope {:?}: {error}",
                    resolved.name
                )
            })?;
        artifacts.push(artifact);
    }

    Ok(artifacts)
}

/// Prove that a historical catalog reconstruction is safe to use for apply.
///
/// Status may render a partial plan from any related journal evidence. Apply is
/// stricter: every reconstructed online rename must have its complete, exact
/// expand chain in the journal and must be either the currently outstanding
/// transition or a terminally applied resolution.
pub fn validate_historical_apply_evidence(
    artifact: &LoweredArtifact,
    live: &LiveSchema,
    journal_entries: &[AppliedEntry],
    resolved: &[zero_migrate::apply::journal::ResolvedPendingContract],
) -> Result<(), String> {
    let mut saw_historical_rename = false;
    for step in &artifact.plan.steps {
        let zero_migrate::PlanStep::OnlineRename(zero_migrate::RenameStep::PgExpandContract(
            rename,
        )) = step
        else {
            continue;
        };
        let migration_is_exact = |migration: &Migration| {
            journal_entries.iter().any(|entry| {
                entry.phase == zero_migrate::Phase::Completed
                    && entry.version == migration.version.as_str()
                    && entry.checksum == migration.checksum.as_str()
            })
        };
        let expand_head_is_exact = rename.expand.iter().take(2).all(migration_is_exact);
        let expand_is_exact = rename.expand.iter().all(migration_is_exact);
        let plan_matches = |plan_version: &str| {
            rename
                .plan_version
                .as_ref()
                .is_none_or(|expected| expected.as_str() == plan_version)
        };
        let terminal = resolved.iter().find(|terminal| {
            terminal.contract.pending_version == rename.trigger_version.as_str()
                && plan_matches(&terminal.contract.plan_version)
        });
        let (source_exists, destination_exists) = live
            .table_snapshots
            .get(match &rename.intent {
                zero_migrate::OnlineIntent::RenameColumn { table, .. } => table,
            })
            .map_or((false, false), |table| match &rename.intent {
                zero_migrate::OnlineIntent::RenameColumn { from, to, .. } => (
                    table.columns.iter().any(|column| column.name == *from),
                    table.columns.iter().any(|column| column.name == *to),
                ),
            });

        if terminal.is_some() {
            if !expand_is_exact {
                return Err(format!(
                    "historical rename {} has a terminal resolution without its exact expand journal chain",
                    rename.trigger_version.as_str()
                ));
            }
            saw_historical_rename = true;
            continue;
        }

        // An ordinary source-only rename in a mixed migration is still fresh. It
        // needs no historical evidence merely because a different rename in the
        // same envelope required reconstruction.
        if source_exists && !destination_exists {
            continue;
        }

        if source_exists && destination_exists && expand_head_is_exact {
            saw_historical_rename = true;
            continue;
        }

        {
            return Err(format!(
                "historical rename {} lacks exact resumable or terminal journal evidence",
                rename.trigger_version.as_str()
            ));
        }
    }
    if !saw_historical_rename {
        return Err("historical apply fallback derived no PostgreSQL online rename".to_string());
    }
    Ok(())
}

fn is_historical_rename_lower_error(error: &str) -> bool {
    error.contains("needs the live")
        || (error.contains("renameColumn") && error.contains("already exists on the live table"))
}

fn ops_contain_contract_rename(
    ops: &[Op],
    dialect: SqlDialect,
    contract: &zero_migrate::apply::journal::PendingContract,
) -> bool {
    ops.iter().any(|op| match op {
        Op::RenameColumn {
            table, from, to, ..
        } => table == &contract.table && from == &contract.from_col && to == &contract.to_col,
        Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            let selected = match dialect {
                SqlDialect::Postgres => pg.as_deref().or(default.as_deref()),
                SqlDialect::Sqlite => sqlite.as_deref().or(default.as_deref()),
                SqlDialect::Mysql => mysql.as_deref().or(default.as_deref()),
            };
            selected
                .is_some_and(|selected| ops_contain_contract_rename(selected, dialect, contract))
        }
        _ => false,
    })
}

/// Rebuild the pre-rename table view needed to derive stable historical plan
/// identities during status. This never authorizes apply: its caller accepts the
/// result only when the derived plan already has journal evidence.
fn normalize_historical_renames(
    live: &mut LiveSchema,
    ops: &[Op],
    dialect: SqlDialect,
    project_schema: &str,
    owner_app: &str,
    resolved_contracts: &[zero_migrate::apply::journal::ResolvedPendingContract],
) -> Result<bool, String> {
    let mut changed = false;
    // Undo authored renames from the final catalog back to the migration's
    // starting shape. Reverse order is required for chains such as a -> b -> c.
    for op in ops.iter().rev() {
        match op {
            Op::RenameColumn {
                table,
                from,
                to,
                ty,
                ..
            } => {
                let source = live.table_snapshots.get(table).and_then(|snapshot| {
                    snapshot
                        .columns
                        .iter()
                        .find(|column| column.name == *from)
                        .cloned()
                });
                let destination = live.table_snapshots.get(table).and_then(|snapshot| {
                    snapshot
                        .columns
                        .iter()
                        .find(|column| column.name == *to)
                        .cloned()
                });
                if let Some(destination) = destination {
                    let snapshot = live.table_snapshots.get_mut(table).ok_or_else(|| {
                        "historical rename table disappeared during reconstruction".to_string()
                    })?;
                    snapshot.columns.retain(|column| column.name != *to);
                    if source.is_none() {
                        let mut historical = destination;
                        historical.name.clone_from(from);
                        snapshot.columns.push(historical);
                    }
                    snapshot
                        .columns
                        .sort_by(|left, right| left.name.cmp(&right.name));
                    changed = true;
                    continue;
                }
                if source.is_some() {
                    continue;
                }
                let terminal = resolved_contracts.iter().find(|terminal| {
                    terminal.contract.table == *table
                        && terminal.contract.from_col == *from
                        && terminal.contract.to_col == *to
                        && terminal.contract.owner_app.as_deref() == Some(owner_app)
                });
                if let Some(terminal) = terminal {
                    let historical = synthetic_rename_source_column(
                        live.table_snapshots.get(table),
                        table,
                        from,
                        ty,
                        &terminal.contract.ty,
                        dialect,
                        project_schema,
                    )?;
                    let snapshot = live
                        .table_snapshots
                        .entry(table.clone())
                        .or_insert_with(empty_table_snapshot);
                    snapshot.columns.push(historical);
                    snapshot
                        .columns
                        .sort_by(|left, right| left.name.cmp(&right.name));
                    live.tables.insert(table.clone());
                    live.table_ownership
                        .insert(table.clone(), owner_app.to_string());
                    changed = true;
                }
            }
            Op::Dialectal {
                default,
                pg,
                sqlite,
                mysql,
            } => {
                let selected = match dialect {
                    SqlDialect::Postgres => pg.as_deref().or(default.as_deref()),
                    SqlDialect::Sqlite => sqlite.as_deref().or(default.as_deref()),
                    SqlDialect::Mysql => mysql.as_deref().or(default.as_deref()),
                };
                if let Some(selected) = selected {
                    changed |= normalize_historical_renames(
                        live,
                        selected,
                        dialect,
                        project_schema,
                        owner_app,
                        resolved_contracts,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(changed)
}

fn synthetic_rename_source_column(
    existing_table: Option<&zero_migrate::model::snapshot::TableSnapshot>,
    table: &str,
    from: &str,
    ty: &zero_migrate::model::ir::ColType,
    durable_ddl_type: &str,
    dialect: SqlDialect,
    project_schema: &str,
) -> Result<zero_migrate::model::snapshot::ColumnSnapshot, String> {
    if dialect == SqlDialect::Postgres {
        if let Some((data_type, _authored_ddl_type)) =
            zero_migrate::render::lower::postgres_named_type_metadata(ty, project_schema)
                .map_err(|error| format!("failed to reconstruct historical named type: {error}"))?
        {
            return Ok(zero_migrate::model::snapshot::ColumnSnapshot {
                name: from.to_string(),
                data_type,
                ddl_type_override: Some(durable_ddl_type.to_string()),
                nullable: true,
                ..Default::default()
            });
        }
    }

    let mut base = zero_migrate::model::snapshot::SchemaSnapshot::default();
    base.tables.insert(
        table.to_string(),
        existing_table.cloned().unwrap_or_else(empty_table_snapshot),
    );
    let add = Op::AddColumn {
        table: table.to_string(),
        column: from.to_string(),
        ty: ty.clone(),
        nullable: None,
        default: None,
        value_format: None,
        vector_metric: None,
        case_sensitive: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: None,
    };
    let projected = fold_ops_onto(&base, &[add], dialect, project_schema)
        .map_err(|error| format!("failed to reconstruct historical rename type: {error}"))?;
    let mut column = projected
        .tables
        .get(table)
        .and_then(|snapshot| snapshot.columns.iter().find(|column| column.name == from))
        .cloned()
        .ok_or_else(|| "historical rename reconstruction produced no source column".to_string())?;
    column.ddl_type_override = Some(durable_ddl_type.to_string());
    Ok(column)
}

fn empty_table_snapshot() -> zero_migrate::model::snapshot::TableSnapshot {
    zero_migrate::model::snapshot::TableSnapshot {
        columns: Vec::new(),
        indexes: Vec::new(),
        constraints: Vec::new(),
        runtime_options: Default::default(),
        partition_by: None,
        comment: None,
        stored_create_sql: None,
    }
}

fn dialect_name(dialect: SqlDialect) -> &'static str {
    match dialect {
        SqlDialect::Postgres => "postgres",
        SqlDialect::Sqlite => "sqlite",
        SqlDialect::Mysql => "mysql",
    }
}

fn live_schema_with_ownership(
    snapshot: zero_migrate::model::snapshot::SchemaSnapshot,
    owner_app: &str,
    registry: &BTreeMap<String, String>,
) -> LiveSchema {
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, owner_app);
    live.table_ownership = live
        .tables
        .iter()
        .filter_map(|table| {
            registry
                .get(table)
                .map(|owner| (table.clone(), owner.clone()))
        })
        .collect();
    live
}

fn plan_has_no_journal_evidence(
    artifact: &LoweredArtifact,
    journal_entries: &[AppliedEntry],
) -> Result<bool, String> {
    let manifest = PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
        .map_err(|e| e.to_string())?;
    Ok(manifest.steps.iter().all(|step| {
        !journal_entries
            .iter()
            .any(|entry| entry.version == step.version.as_str())
    }))
}

fn ops_without_completed_journal_evidence(
    artifact: &LoweredArtifact,
    journal_entries: &[AppliedEntry],
) -> Result<Vec<(Op, bool)>, String> {
    let mut pending = Vec::new();
    for span in &artifact.op_spans {
        let mut completed = false;
        let mut inflight = false;
        for range in std::iter::once(&span.step_range).chain(&span.additional_step_ranges) {
            let steps = artifact.plan.steps.get(range.clone()).ok_or_else(|| {
                format!(
                    "lowered operation has invalid plan-step range {}..{} for {} steps",
                    range.start,
                    range.end,
                    artifact.plan.steps.len()
                )
            })?;
            completed |= steps
                .iter()
                .any(|step| step_has_journal_phase(step, journal_entries, Phase::Completed));
            inflight |= steps
                .iter()
                .any(|step| step_has_journal_phase(step, journal_entries, Phase::Started));
        }
        if !completed {
            pending.push((span.op.clone(), inflight));
        }
    }
    Ok(pending)
}

fn step_has_journal_phase(
    step: &zero_migrate::PlanStep,
    journal_entries: &[AppliedEntry],
    phase: Phase,
) -> bool {
    let has_version = |version: &str| {
        journal_entries
            .iter()
            .any(|entry| entry.version == version && entry.phase == phase)
    };
    match step {
        zero_migrate::PlanStep::Ddl(migration) => has_version(migration.version.as_str()),
        zero_migrate::PlanStep::Dml { version, .. }
        | zero_migrate::PlanStep::Backfill { version, .. } => has_version(version.as_str()),
        zero_migrate::PlanStep::OnlineRename(zero_migrate::RenameStep::PgExpandContract(
            rename,
        )) => rename
            .expand
            .iter()
            .chain(&rename.contract)
            .any(|migration| has_version(migration.version.as_str())),
        zero_migrate::PlanStep::OnlineRename(zero_migrate::RenameStep::SqliteRebuild(rename)) => {
            has_version(rename.migration.version.as_str())
        }
    }
}

fn inflight_projection_already_reflected(
    snapshot: &zero_migrate::model::snapshot::SchemaSnapshot,
    op: &Op,
    error: &FoldError,
) -> bool {
    let table_has_column = |table: &str, column: &str| {
        snapshot
            .tables
            .get(table)
            .is_some_and(|snapshot| snapshot.columns.iter().any(|item| item.name == column))
    };
    let table_has_constraint = |table: &str, name: &str| {
        snapshot
            .tables
            .get(table)
            .is_some_and(|snapshot| snapshot.constraints.iter().any(|item| item.name == name))
    };
    let has_index = |table: Option<&str>, name: &str| match table {
        Some(table) => snapshot
            .tables
            .get(table)
            .is_some_and(|snapshot| snapshot.indexes.iter().any(|item| item.name == name)),
        None => snapshot
            .tables
            .values()
            .any(|snapshot| snapshot.indexes.iter().any(|item| item.name == name)),
    };

    match (op, error) {
        (Op::CreateTable { name, .. }, FoldError::DuplicateTable(actual)) => {
            name == actual && snapshot.tables.contains_key(name)
        }
        (Op::CreatePartition { name, .. }, FoldError::DuplicateTable(actual)) => {
            name == actual
                && (snapshot.partitions.contains_key(name) || snapshot.tables.contains_key(name))
        }
        (Op::AttachPartition { parent, name, .. }, FoldError::DuplicateTable(actual)) => {
            name == actual
                && snapshot
                    .partitions
                    .get(name)
                    .is_some_and(|partition| partition.of == *parent)
        }
        (Op::DropTable { table, .. }, FoldError::MissingTable(actual))
        | (Op::DropPartition { name: table, .. }, FoldError::MissingTable(actual)) => {
            table == actual
                && !snapshot.tables.contains_key(table)
                && !snapshot.partitions.contains_key(table)
        }
        (Op::DetachPartition { name, .. }, FoldError::MissingTable(actual)) => {
            name == actual
                && !snapshot.partitions.contains_key(name)
                && snapshot.tables.contains_key(name)
        }
        (Op::RenameTable { table, to, .. }, FoldError::MissingTable(actual)) => {
            table == actual
                && !snapshot.tables.contains_key(table)
                && snapshot.tables.contains_key(to)
        }
        (Op::RenameTable { table, to, .. }, FoldError::DuplicateTable(actual)) => {
            to == actual && !snapshot.tables.contains_key(table) && snapshot.tables.contains_key(to)
        }
        (
            Op::AddColumn { table, column, .. },
            FoldError::DuplicateColumn {
                table: actual_table,
                column: actual_column,
            },
        ) => table == actual_table && column == actual_column && table_has_column(table, column),
        (
            Op::DropColumn { table, column, .. },
            FoldError::MissingColumn {
                table: actual_table,
                column: actual_column,
            },
        ) => {
            table == actual_table
                && column == actual_column
                && snapshot.tables.contains_key(table)
                && !table_has_column(table, column)
        }
        (
            Op::RenameColumn {
                table, from, to, ..
            },
            FoldError::MissingColumn {
                table: actual_table,
                column: actual_column,
            },
        ) => {
            table == actual_table
                && from == actual_column
                && !table_has_column(table, from)
                && table_has_column(table, to)
        }
        (
            Op::RenameColumn {
                table, from, to, ..
            },
            FoldError::RenameCollision {
                table: actual_table,
                to: actual_to,
            },
        ) => {
            table == actual_table
                && to == actual_to
                && !table_has_column(table, from)
                && table_has_column(table, to)
        }
        (
            Op::AddConstraint { table, .. },
            FoldError::DuplicateConstraint {
                table: actual_table,
                name: actual_name,
            },
        ) => table == actual_table && table_has_constraint(table, actual_name),
        (
            Op::DropConstraint { table, name, .. },
            FoldError::MissingConstraint {
                table: actual_table,
                name: actual_name,
            },
        ) => {
            table == actual_table
                && name == actual_name
                && snapshot.tables.contains_key(table)
                && !table_has_constraint(table, name)
        }
        (Op::CreateIndex { table, .. }, FoldError::DuplicateIndex(actual)) => {
            has_index(Some(table), actual)
        }
        (Op::DropIndex { table, name, .. }, FoldError::MissingIndex(actual)) => {
            name == actual && !has_index(table.as_deref(), name)
        }
        (Op::CreateView { name, .. }, FoldError::DuplicateView(actual)) => {
            name == actual && snapshot.views.contains_key(name)
        }
        (Op::DropView { name, .. }, FoldError::MissingView(actual)) => {
            name == actual && !snapshot.views.contains_key(name)
        }
        (Op::CreateSequence { name, .. }, FoldError::DuplicateSequence(actual)) => {
            name == actual && snapshot.sequences.contains_key(name)
        }
        (Op::DropSequence { name, .. }, FoldError::MissingSequence(actual)) => {
            name == actual && !snapshot.sequences.contains_key(name)
        }
        _ => false,
    }
}

fn advance_ownership_registry(
    registry: &mut BTreeMap<String, String>,
    ops: &[Op],
    dialect: SqlDialect,
    owner_app: &str,
) {
    for op in ops {
        match op {
            Op::CreateTable { name, .. } => {
                registry.insert(name.clone(), owner_app.to_string());
            }
            Op::DropTable { table, .. } => {
                registry.remove(table);
            }
            Op::RenameTable { table, to, .. } => {
                let owner = registry
                    .remove(table)
                    .unwrap_or_else(|| owner_app.to_string());
                registry.insert(to.clone(), owner);
            }
            Op::CreatePartition { name, of, .. } => {
                let owner = registry
                    .get(of)
                    .cloned()
                    .unwrap_or_else(|| owner_app.to_string());
                registry.insert(name.clone(), owner);
            }
            Op::AttachPartition { parent, name, .. } => {
                let owner = registry
                    .get(parent)
                    .cloned()
                    .unwrap_or_else(|| owner_app.to_string());
                registry.entry(name.clone()).or_insert(owner);
            }
            Op::DropPartition { name, .. } => {
                registry.remove(name);
            }
            Op::Dialectal {
                default,
                pg,
                sqlite,
                mysql,
            } => {
                let selected = match dialect {
                    SqlDialect::Postgres => pg.as_deref().or(default.as_deref()),
                    SqlDialect::Sqlite => sqlite.as_deref().or(default.as_deref()),
                    SqlDialect::Mysql => mysql.as_deref().or(default.as_deref()),
                };
                if let Some(selected) = selected {
                    advance_ownership_registry(registry, selected, dialect, owner_app);
                }
            }
            _ => {}
        }
    }
}

/// Compatibility projection of [`lower_envelope_to_plan`] for callers that still
/// consume a flat `Vec<Migration>`. This view intentionally cannot represent DML or
/// backfill steps; execution callers must use the complete plan instead.
///
/// # Errors
/// Same as [`lower_envelope_to_plan`].
pub fn lower_envelope_to_migrations(
    envelope_json: &str,
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    policy_ceiling_toml: Option<&str>,
) -> Result<Vec<Migration>, String> {
    lower_envelope_to_plan(
        envelope_json,
        owner_app,
        project_schema,
        dialect,
        registry_json,
        policy_ceiling_toml,
    )
    .map(|artifact| artifact.migrations())
}

/// [`lower_envelope_to_migrations`] returning a JSON `Vec<Migration>` string (the
/// compatibility shape for migration-only consumers. Plan execution must not use
/// this projection because it cannot carry data steps.
///
/// # Errors
/// Same as [`lower_envelope_to_migrations`]; the error is returned as an `Err`
/// string so the napi bridge can reject the promise with it.
pub fn lower_envelope_to_migrations_json(
    envelope_json: &str,
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    policy_ceiling_toml: Option<&str>,
) -> Result<String, String> {
    let migrations = lower_envelope_to_migrations(
        envelope_json,
        owner_app,
        project_schema,
        dialect,
        registry_json,
        policy_ceiling_toml,
    )?;
    serde_json::to_string(&migrations)
        .map_err(|e| format!("failed to serialize lowered migrations: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zero_migrate::{BindValue, PlanStep};

    // A minimal create-first envelope: one `createTable` op. Mirrors what the pure-JS
    // host recorder emits (ir_version stamped from the addon's `irVersion()` floor).
    fn create_widgets_envelope(ir_version: u32) -> String {
        serde_json::json!({
            "ir_version": ir_version,
            "name": "create_widgets",
            "ops": [
                {
                    "op": "createTable",
                    "table": "widgets",
                    "columns": [
                        { "name": "id", "type": { "kind": "int" }, "nullable": false },
                        { "name": "label", "type": { "kind": "text" }, "nullable": true }
                    ]
                }
            ]
        })
        .to_string()
    }

    // The compact four-op payload is pinned byte-for-byte by the SDK recorder test
    // `complete DML surface records identically and preserves authored order`.
    // Keeping the same shape here makes this the DB-free recorder-output -> host-plan
    // half of that regression without adding a test-only bridge API.
    fn data_steps_envelope(ir_version: u32, inserted_id: i64) -> String {
        serde_json::json!({
            "ir_version": ir_version,
            "name": "seed_and_rewrite_widgets",
            "ops": [
                {
                    "op": "insert",
                    "table": "widgets",
                    "columns": ["id", "label"],
                    "rows": [[inserted_id, "new"]]
                },
                {
                    "op": "update",
                    "table": "widgets",
                    "set": {
                        "label": "updated"
                    },
                    "where": {
                        "node": "binOp",
                        "op": "eq",
                        "lhs": { "node": "colRef", "name": "id" },
                        "rhs": { "node": "literal", "value": 1 }
                    }
                },
                {
                    "op": "delete",
                    "table": "widgets",
                    "where": {
                        "node": "binOp",
                        "op": "eq",
                        "lhs": { "node": "colRef", "name": "id" },
                        "rhs": { "node": "literal", "value": 2 }
                    },
                    "limit": 1
                },
                {
                    "op": "backfill",
                    "table": "widgets",
                    "cursorColumns": ["id"],
                    "cursorStability": { "mode": "guardUpdates" },
                    "batchSize": 1000,
                    "set": {
                        "label": "filled"
                    },
                    "filter": {
                        "node": "unaryOp",
                        "op": "isNull",
                        "operand": { "node": "colRef", "name": "label" }
                    },
                    "name": "backfill_widgets"
                }
            ]
        })
        .to_string()
    }

    fn ordered_status_envelopes(ir_version: u32) -> Vec<String> {
        vec![
            serde_json::json!({
                "ir_version": ir_version,
                "name": "create_status_widgets",
                "ops": [{
                    "op": "createTable",
                    "name": "status_widgets",
                    "columns": [{ "name": "payload", "type": "json" }],
                    "primaryKey": null,
                    "constraints": [],
                    "indexes": []
                }]
            })
            .to_string(),
            serde_json::json!({
                "ir_version": ir_version,
                "name": "default_status_widgets_payload",
                "ops": [{
                    "op": "setColumnDefault",
                    "table": "status_widgets",
                    "column": "payload",
                    "value": { "container": "object" }
                }]
            })
            .to_string(),
        ]
    }

    // The generic confined test ceiling the monorepo will supply in Phase 3.
    const CEILING: &str = zero_migrate::ZEROSHIP_CONFINED_CEILING_TOML;

    #[test]
    fn historical_chained_renames_reconstruct_the_original_column() {
        let ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
            "name": "rename_chain",
            "ops": [
                { "op": "renameColumn", "table": "items", "from": "a", "to": "b", "type": "text" },
                { "op": "renameColumn", "table": "items", "from": "b", "to": "c", "type": "text" }
            ]
        }))
        .expect("rename chain parses");
        let snapshot = zero_migrate::model::snapshot::SchemaSnapshot {
            tables: BTreeMap::from([(
                "items".to_string(),
                zero_migrate::model::snapshot::TableSnapshot {
                    columns: vec![zero_migrate::model::snapshot::ColumnSnapshot {
                        name: "c".to_string(),
                        data_type: "text".to_string(),
                        nullable: true,
                        ..Default::default()
                    }],
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    runtime_options: Default::default(),
                    partition_by: None,
                    comment: None,
                    stored_create_sql: None,
                },
            )]),
            ..Default::default()
        };
        let mut live = LiveSchema::from_catalog_snapshot(snapshot, "app_test");

        assert!(normalize_historical_renames(
            &mut live,
            &ir.ops,
            SqlDialect::Postgres,
            "app_test",
            "app_test",
            &[],
        )
        .expect("historical chain normalizes"));
        let names: Vec<&str> = live.table_snapshots["items"]
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(names, vec!["a"]);
    }

    fn rename_live(columns: &[&str]) -> LiveSchema {
        let snapshot = zero_migrate::model::snapshot::SchemaSnapshot {
            tables: BTreeMap::from([(
                "items".to_string(),
                zero_migrate::model::snapshot::TableSnapshot {
                    columns: columns
                        .iter()
                        .map(|name| zero_migrate::model::snapshot::ColumnSnapshot {
                            name: (*name).to_string(),
                            data_type: "text".to_string(),
                            nullable: true,
                            ..Default::default()
                        })
                        .collect(),
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    runtime_options: Default::default(),
                    partition_by: None,
                    comment: None,
                    stored_create_sql: None,
                },
            )]),
            ..Default::default()
        };
        LiveSchema::from_catalog_snapshot(snapshot, "app_test")
    }

    fn named_type_rename_live(data_type: &str, ddl_type: &str) -> LiveSchema {
        let snapshot = zero_migrate::model::snapshot::SchemaSnapshot {
            tables: BTreeMap::from([(
                "items".to_string(),
                zero_migrate::model::snapshot::TableSnapshot {
                    columns: vec![zero_migrate::model::snapshot::ColumnSnapshot {
                        name: "state".to_string(),
                        data_type: data_type.to_string(),
                        ddl_type_override: Some(ddl_type.to_string()),
                        nullable: true,
                        ..Default::default()
                    }],
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    runtime_options: Default::default(),
                    partition_by: None,
                    comment: None,
                    stored_create_sql: None,
                },
            )]),
            ..Default::default()
        };
        LiveSchema::from_catalog_snapshot(snapshot, "app_test")
    }

    fn named_type_rename_envelope(name: &str, ty: serde_json::Value) -> String {
        serde_json::json!({
            "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
            "name": name,
            "ops": [{
                "op": "renameColumn",
                "table": "items",
                "from": "state",
                "to": "status",
                "type": ty
            }]
        })
        .to_string()
    }

    #[test]
    fn fresh_postgres_named_type_renames_use_qualified_type_spelling() {
        let cases = [
            (
                "rename_enum_state",
                serde_json::json!({ "enum": { "name": "item_state" } }),
                "app_test.item_state",
                "\"app_test\".\"item_state\"",
            ),
            (
                "rename_domain_state",
                serde_json::json!({
                    "domain": { "name": "state_code", "schema": "shared_types" }
                }),
                "shared_types.state_code",
                "\"shared_types\".\"state_code\"",
            ),
        ];

        for (name, ty, catalog_type, ddl_type) in cases {
            let envelope = named_type_rename_envelope(name, ty);
            let artifact = lower_envelope_to_plan_with_live(
                &envelope,
                "app_test",
                "app_test",
                "postgres",
                r#"{"items":"app_test"}"#,
                None,
                &named_type_rename_live(catalog_type, ddl_type),
            )
            .expect("named type rename lowers from its live source type");
            let rename = artifact
                .plan
                .steps
                .iter()
                .find_map(|step| match step {
                    PlanStep::OnlineRename(zero_migrate::RenameStep::PgExpandContract(rename)) => {
                        Some(rename)
                    }
                    _ => None,
                })
                .expect("plan contains the named type rename");
            let zero_migrate::OnlineIntent::RenameColumn { ty, .. } = &rename.intent;
            assert_eq!(ty, ddl_type);
            assert!(rename.expand[0].up.ends_with(ddl_type));
        }
    }

    #[test]
    fn fresh_postgres_named_type_renames_compare_mixed_case_catalog_identity() {
        let cases = [
            (
                "rename_quoted_enum_state",
                serde_json::json!({ "enum": { "name": "MoodState" } }),
                "AppSpace.MoodState",
                "\"AppSpace\".\"MoodState\"",
            ),
            (
                "rename_quoted_domain_state",
                serde_json::json!({
                    "domain": { "name": "StateCode", "schema": "SharedTypes" }
                }),
                "SharedTypes.StateCode",
                "\"SharedTypes\".\"StateCode\"",
            ),
        ];

        for (name, ty, catalog_type, ddl_type) in cases {
            let artifact = lower_envelope_to_plan_with_live(
                &named_type_rename_envelope(name, ty),
                "app_test",
                "AppSpace",
                "postgres",
                r#"{"items":"app_test"}"#,
                None,
                &named_type_rename_live(catalog_type, ddl_type),
            )
            .expect("quoted named type identity lowers without comparing DDL quotes");
            let rename = artifact
                .plan
                .steps
                .iter()
                .find_map(|step| match step {
                    PlanStep::OnlineRename(zero_migrate::RenameStep::PgExpandContract(rename)) => {
                        Some(rename)
                    }
                    _ => None,
                })
                .expect("plan contains the mixed-case named type rename");
            let zero_migrate::OnlineIntent::RenameColumn { ty, .. } = &rename.intent;
            assert_eq!(ty, ddl_type);
        }
    }

    #[test]
    fn terminal_named_type_renames_reconstruct_after_the_table_disappears() {
        let cases = [
            (
                "replay_enum_state",
                serde_json::json!({ "enum": { "name": "item_state" } }),
                "app_test.item_state",
                "\"app_test\".\"item_state\"",
            ),
            (
                "replay_domain_state",
                serde_json::json!({
                    "domain": { "name": "state_code", "schema": "shared_types" }
                }),
                "shared_types.state_code",
                "\"shared_types\".\"state_code\"",
            ),
        ];

        for (name, ty, catalog_type, ddl_type) in cases {
            let envelope = named_type_rename_envelope(name, ty);
            let original = lower_envelope_to_plan_with_live(
                &envelope,
                "app_test",
                "app_test",
                "postgres",
                r#"{"items":"app_test"}"#,
                None,
                &named_type_rename_live(catalog_type, ddl_type),
            )
            .expect("named type rename lowers from the original table");
            let (entries, terminal) = completed_rename_lifecycle(&original);

            let replay = lower_ordered_envelopes_to_plans(
                std::slice::from_ref(&envelope),
                "app_test",
                "app_test",
                "postgres",
                "{}",
                None,
                zero_migrate::model::snapshot::SchemaSnapshot::default(),
                &entries,
                std::slice::from_ref(&terminal),
            )
            .expect("terminal named type rename reconstructs without its table");

            assert_eq!(replay.len(), 1);
            assert_eq!(replay[0].plan.version, original.plan.version);
        }
    }

    fn decimal_rename_envelope() -> String {
        serde_json::json!({
            "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
            "name": "rename_decimal_amount",
            "ops": [{
                "op": "renameColumn",
                "table": "items",
                "from": "amount",
                "to": "total",
                "type": { "decimal": { "precision": 20, "scale": 4 } }
            }]
        })
        .to_string()
    }

    fn decimal_rename_live(ddl_type: &str) -> LiveSchema {
        let snapshot = zero_migrate::model::snapshot::SchemaSnapshot {
            tables: BTreeMap::from([(
                "items".to_string(),
                zero_migrate::model::snapshot::TableSnapshot {
                    columns: vec![zero_migrate::model::snapshot::ColumnSnapshot {
                        name: "amount".to_string(),
                        data_type: "numeric".to_string(),
                        ddl_type_override: Some(ddl_type.to_string()),
                        nullable: true,
                        ..Default::default()
                    }],
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    runtime_options: Default::default(),
                    partition_by: None,
                    comment: None,
                    stored_create_sql: None,
                },
            )]),
            ..Default::default()
        };
        LiveSchema::from_catalog_snapshot(snapshot, "app_test")
    }

    #[test]
    fn fresh_decimal_rename_rejects_live_modifier_drift() {
        let error = lower_envelope_to_plan_with_live(
            &decimal_rename_envelope(),
            "app_test",
            "app_test",
            "postgres",
            r#"{"items":"app_test"}"#,
            None,
            &decimal_rename_live("numeric(20,2)"),
        )
        .expect_err("a decimal rename must not normalize authored modifiers to the live type");

        assert!(error.contains("numeric(20, 4)"), "{error}");
        assert!(error.contains("numeric(20,2)"), "{error}");
    }

    #[test]
    fn fresh_decimal_rename_accepts_equivalent_postgres_alias_spelling() {
        let artifact = lower_envelope_to_plan_with_live(
            &decimal_rename_envelope(),
            "app_test",
            "app_test",
            "postgres",
            r#"{"items":"app_test"}"#,
            None,
            &decimal_rename_live("decimal(20,4)"),
        )
        .expect("decimal and numeric are equivalent PostgreSQL spellings");
        let rename = artifact
            .plan
            .steps
            .iter()
            .find_map(|step| match step {
                PlanStep::OnlineRename(zero_migrate::RenameStep::PgExpandContract(rename)) => {
                    Some(rename)
                }
                _ => None,
            })
            .expect("plan contains decimal rename");
        let zero_migrate::OnlineIntent::RenameColumn { ty, .. } = &rename.intent;
        assert_eq!(ty, "decimal(20,4)");
    }

    fn completed_rename_lifecycle(
        artifact: &LoweredArtifact,
    ) -> (
        Vec<AppliedEntry>,
        zero_migrate::apply::journal::ResolvedPendingContract,
    ) {
        use zero_migrate::apply::journal::{
            JournaledKind, PendingContract, ResolvedPendingContract,
        };

        let rename = artifact
            .plan
            .steps
            .iter()
            .find_map(|step| match step {
                PlanStep::OnlineRename(zero_migrate::RenameStep::PgExpandContract(rename)) => {
                    Some(rename)
                }
                _ => None,
            })
            .expect("plan contains an online rename");
        let entries = rename
            .expand
            .iter()
            .map(|migration| AppliedEntry {
                version: migration.version.as_str().to_string(),
                checksum: migration.checksum.as_str().to_string(),
                phase: zero_migrate::Phase::Completed,
                kind: Some(JournaledKind::Apply),
            })
            .collect();
        let terminal = ResolvedPendingContract {
            contract: PendingContract {
                owner_app: Some("app_test".to_string()),
                table: match &rename.intent {
                    zero_migrate::OnlineIntent::RenameColumn { table, .. } => table.clone(),
                },
                from_col: match &rename.intent {
                    zero_migrate::OnlineIntent::RenameColumn { from, .. } => from.clone(),
                },
                to_col: match &rename.intent {
                    zero_migrate::OnlineIntent::RenameColumn { to, .. } => to.clone(),
                },
                ty: match &rename.intent {
                    zero_migrate::OnlineIntent::RenameColumn { ty, .. } => ty.clone(),
                },
                pending_version: rename.trigger_version.as_str().to_string(),
                plan_version: rename
                    .plan_version
                    .as_ref()
                    .expect("IR rename has a plan version")
                    .as_str()
                    .to_string(),
                contract_versions: rename
                    .contract
                    .iter()
                    .map(|migration| migration.version.as_str().to_string())
                    .collect(),
            },
            resolution: zero_migrate::Resolution::Applied,
        };
        (entries, terminal)
    }

    #[test]
    fn terminal_decimal_rename_reconstructs_after_its_table_disappears() {
        let envelope = serde_json::json!({
            "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
            "name": "rename_items_amount",
            "ops": [{
                "op": "renameColumn",
                "table": "items",
                "from": "amount",
                "to": "total",
                "type": { "decimal": { "precision": 20, "scale": 4 } }
            }]
        })
        .to_string();
        let initial_snapshot = zero_migrate::model::snapshot::SchemaSnapshot {
            tables: BTreeMap::from([(
                "items".to_string(),
                zero_migrate::model::snapshot::TableSnapshot {
                    columns: vec![zero_migrate::model::snapshot::ColumnSnapshot {
                        name: "amount".to_string(),
                        data_type: "numeric".to_string(),
                        ddl_type_override: Some("numeric(20,4)".to_string()),
                        nullable: true,
                        ..Default::default()
                    }],
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    runtime_options: Default::default(),
                    partition_by: None,
                    comment: None,
                    stored_create_sql: None,
                },
            )]),
            ..Default::default()
        };
        let initial_live = LiveSchema::from_catalog_snapshot(initial_snapshot, "app_test");
        let original = lower_envelope_to_plan_with_live(
            &envelope,
            "app_test",
            "app_test",
            "postgres",
            r#"{"items":"app_test"}"#,
            None,
            &initial_live,
        )
        .expect("decimal rename lowers from the original table");
        let (entries, terminal) = completed_rename_lifecycle(&original);

        let replay = lower_ordered_envelopes_to_plans(
            std::slice::from_ref(&envelope),
            "app_test",
            "app_test",
            "postgres",
            "{}",
            None,
            zero_migrate::model::snapshot::SchemaSnapshot::default(),
            &entries,
            std::slice::from_ref(&terminal),
        )
        .expect("terminal history reconstructs a dropped table without losing its type");

        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].plan.version, original.plan.version);
    }

    #[test]
    fn separately_resolved_rename_chain_replays_from_the_final_catalog() {
        let first = serde_json::json!({
            "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
            "name": "rename_items_a_to_b",
            "ops": [{
                "op": "renameColumn",
                "table": "items",
                "from": "a",
                "to": "b",
                "type": "text"
            }]
        })
        .to_string();
        let second = serde_json::json!({
            "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
            "name": "rename_items_b_to_c",
            "ops": [{
                "op": "renameColumn",
                "table": "items",
                "from": "b",
                "to": "c",
                "type": "text"
            }]
        })
        .to_string();
        let first_original = lower_envelope_to_plan_with_live(
            &first,
            "app_test",
            "app_test",
            "postgres",
            r#"{"items":"app_test"}"#,
            None,
            &rename_live(&["a"]),
        )
        .expect("first rename lowers");
        let second_original = lower_envelope_to_plan_with_live(
            &second,
            "app_test",
            "app_test",
            "postgres",
            r#"{"items":"app_test"}"#,
            None,
            &rename_live(&["b"]),
        )
        .expect("second rename lowers after the first resolves");
        let (mut entries, first_terminal) = completed_rename_lifecycle(&first_original);
        let (second_entries, second_terminal) = completed_rename_lifecycle(&second_original);
        entries.extend(second_entries);
        let terminals = vec![first_terminal, second_terminal];
        let final_snapshot = zero_migrate::model::snapshot::SchemaSnapshot {
            tables: rename_live(&["c"]).table_snapshots,
            ..Default::default()
        };

        let replay = lower_ordered_envelopes_to_plans(
            &[first, second],
            "app_test",
            "app_test",
            "postgres",
            r#"{"items":"app_test"}"#,
            None,
            final_snapshot,
            &entries,
            &terminals,
        )
        .expect("resolved rename files replay from the final c-only catalog");

        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].plan.version, first_original.plan.version);
        assert_eq!(replay[1].plan.version, second_original.plan.version);
    }

    #[test]
    fn historical_apply_requires_exact_lifecycle_evidence() {
        use zero_migrate::apply::journal::{
            JournaledKind, PendingContract, ResolvedPendingContract,
        };

        let envelope = serde_json::json!({
            "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
            "name": "rename_items_label",
            "ops": [{
                "op": "renameColumn",
                "table": "items",
                "from": "a",
                "to": "b",
                "type": "text"
            }]
        })
        .to_string();
        let artifact = lower_envelope_to_plan_with_live(
            &envelope,
            "app_test",
            "app_test",
            "postgres",
            r#"{"items":"app_test"}"#,
            None,
            &rename_live(&["a"]),
        )
        .expect("rename lowers from its source shape");
        let rename = artifact
            .plan
            .steps
            .iter()
            .find_map(|step| match step {
                PlanStep::OnlineRename(zero_migrate::RenameStep::PgExpandContract(rename)) => {
                    Some(rename)
                }
                _ => None,
            })
            .expect("plan contains an online rename");
        let entries: Vec<AppliedEntry> = rename
            .expand
            .iter()
            .map(|migration| AppliedEntry {
                version: migration.version.as_str().to_string(),
                checksum: migration.checksum.as_str().to_string(),
                phase: zero_migrate::Phase::Completed,
                kind: Some(JournaledKind::Apply),
            })
            .collect();
        let terminal = ResolvedPendingContract {
            contract: PendingContract {
                owner_app: Some("app_test".to_string()),
                table: "items".to_string(),
                from_col: "a".to_string(),
                to_col: "b".to_string(),
                ty: "text".to_string(),
                pending_version: rename.trigger_version.as_str().to_string(),
                plan_version: rename
                    .plan_version
                    .as_ref()
                    .expect("IR rename has a plan version")
                    .as_str()
                    .to_string(),
                contract_versions: rename
                    .contract
                    .iter()
                    .map(|migration| migration.version.as_str().to_string())
                    .collect(),
            },
            resolution: zero_migrate::Resolution::Applied,
        };

        assert!(
            validate_historical_apply_evidence(&artifact, &rename_live(&["b"]), &entries, &[],)
                .is_err()
        );
        validate_historical_apply_evidence(
            &artifact,
            &rename_live(&["b"]),
            &entries,
            std::slice::from_ref(&terminal),
        )
        .expect("destination-only replay has exact applied evidence");

        assert!(validate_historical_apply_evidence(
            &artifact,
            &rename_live(&["b"]),
            &entries[..2],
            std::slice::from_ref(&terminal),
        )
        .is_err());
        validate_historical_apply_evidence(
            &artifact,
            &rename_live(&[]),
            &entries,
            std::slice::from_ref(&terminal),
        )
        .expect("later migrations may remove both historical column names");

        let head_only = &entries[..2];
        validate_historical_apply_evidence(&artifact, &rename_live(&["a", "b"]), head_only, &[])
            .expect("coexisting columns with an exact expand head are resumable");
    }

    #[test]
    fn unknown_dialect_is_an_err_not_a_panic() {
        let env = create_widgets_envelope(zero_migrate::model::ir::CURRENT_IR_VERSION);
        let r = lower_envelope_to_migrations(&env, "app_x", "app_x", "oracle", "{}", Some(CEILING));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("unknown dialect"));
    }

    #[test]
    fn malformed_registry_is_an_err() {
        let env = create_widgets_envelope(zero_migrate::model::ir::CURRENT_IR_VERSION);
        let r = lower_envelope_to_migrations(
            &env,
            "app_x",
            "app_x",
            "postgres",
            "[1,2,3]",
            Some(CEILING),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("registry_json"));
    }

    #[test]
    fn malformed_policy_ceiling_is_an_err() {
        let env = create_widgets_envelope(zero_migrate::model::ir::CURRENT_IR_VERSION);
        let r = lower_envelope_to_migrations(
            &env,
            "app_x",
            "app_x",
            "postgres",
            "{}",
            Some("this is not = valid toml ["),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("policy"));
    }

    #[test]
    fn create_first_envelope_lowers_to_a_migration_with_a_folded_checksum() {
        // The op shape may or may not match the exact IR schema (the round-trip op
        // vocabulary is authoritative), so this test asserts the GATE behavior: a
        // syntactically-valid envelope either lowers to a non-empty `Vec<Migration>`
        // whose sole migration carries a NON-empty `up` SQL + a folded checksum, or
        // fails closed with a message (never a panic). Both are acceptable proofs the
        // lower path is wired; the DB-backed oracle asserts journal identity.
        let env = create_widgets_envelope(zero_migrate::model::ir::CURRENT_IR_VERSION);
        match lower_envelope_to_migrations(
            &env,
            "app_widgets",
            "app_widgets",
            "postgres",
            "{}",
            Some(CEILING),
        ) {
            Ok(migs) => {
                assert!(
                    !migs.is_empty(),
                    "a create-first envelope lowers to ≥1 migration"
                );
                let m = &migs[0];
                assert!(
                    !m.up.is_empty(),
                    "the lowered migration has non-empty up SQL"
                );
                assert_eq!(
                    m.owner_app, "app_widgets",
                    "owner_app is stamped from the arg"
                );
            }
            Err(msg) => {
                // Fail-closed is acceptable here (op-schema mismatch) — the load gate
                // never panics. The DB oracle uses a build-verified envelope.
                assert!(!msg.is_empty());
            }
        }
    }

    #[test]
    fn complete_dml_surface_retains_order_identity_checksum_and_approval_classification() {
        let lower = |inserted_id| {
            let env = data_steps_envelope(zero_migrate::model::ir::CURRENT_IR_VERSION, inserted_id);
            lower_envelope_to_plan(
                &env,
                "app_widgets",
                "app_widgets",
                "postgres",
                r#"{"widgets":"app_widgets"}"#,
                None,
            )
            .expect("the complete data surface lowers to a plan")
        };
        let first = lower(1);
        let identical = lower(1);
        let edited = lower(9);

        let step_identity = |step: &PlanStep| match step {
            PlanStep::Dml {
                version, checksum, ..
            }
            | PlanStep::Backfill {
                version, checksum, ..
            } => (version.as_str().to_string(), checksum.as_str().to_string()),
            other => panic!("expected a data step, got {other:?}"),
        };
        let identities = |artifact: &zero_migrate::LoweredArtifact| {
            artifact
                .plan
                .steps
                .iter()
                .map(&step_identity)
                .collect::<Vec<_>>()
        };

        assert_eq!(first.plan.steps.len(), 4);
        assert!(matches!(
            &first.plan.steps[0],
            PlanStep::Dml { name, binds, .. }
                if name == "insert widgets"
                    && binds == &[BindValue::Int(1), BindValue::Text("new".into())]
        ));
        assert!(matches!(
            &first.plan.steps[1],
            PlanStep::Dml { name, .. } if name == "update widgets"
        ));
        assert!(matches!(
            &first.plan.steps[2],
            PlanStep::Dml { name, .. } if name == "delete widgets"
        ));
        assert!(matches!(
            &first.plan.steps[3],
            PlanStep::Backfill { spec, .. }
                if spec.name == "backfill_widgets"
                    && spec.cursor_columns == ["id"]
                    && spec.batch_size == 1000
        ));

        // Insert/update are ordinary writes. Delete loses data, and backfill is a
        // resumable rewrite, so both latter steps need explicit per-version approval.
        assert!(!first.plan.steps[0].is_destructive());
        assert_eq!(first.plan.steps[0].approval_scope_version(), None);
        assert!(!first.plan.steps[1].is_destructive());
        assert_eq!(first.plan.steps[1].approval_scope_version(), None);
        for step in &first.plan.steps[2..] {
            assert!(step.is_destructive());
            let (version, _) = step_identity(step);
            assert_eq!(step.approval_scope_version(), Some(version.as_str()));
        }

        let first_identities = identities(&first);
        let edited_identities = identities(&edited);
        assert_eq!(first_identities, identities(&identical));
        assert_eq!(first.plan.version, identical.plan.version);
        assert_eq!(first.plan.checksum, identical.plan.checksum);
        assert_eq!(
            first_identities
                .iter()
                .map(|(version, _)| version)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            4,
            "each authored ordinal needs its own journal identity"
        );
        assert_eq!(
            first.plan.version, edited.plan.version,
            "the migration name, not its data, is the plan identity"
        );
        assert_eq!(
            first_identities
                .iter()
                .map(|(version, _)| version)
                .collect::<Vec<_>>(),
            edited_identities
                .iter()
                .map(|(version, _)| version)
                .collect::<Vec<_>>(),
            "editing a bound value must retain every stable ordered step id"
        );
        assert_ne!(first.plan.checksum, edited.plan.checksum);
        for (step, (_, edited_checksum)) in edited.plan.steps.iter().zip(&edited_identities) {
            assert_eq!(edited_checksum, edited.plan.checksum.as_str());
            assert_eq!(step_identity(step).1, edited.plan.checksum.as_str());
        }
        for (_, checksum) in &first_identities {
            assert_eq!(checksum, first.plan.checksum.as_str());
        }

        assert_eq!(first.touched_tables, vec!["widgets"]);
        assert!(
            !first.plan.rollbackable,
            "data steps do not synthesize down SQL"
        );

        // This is the compatibility view that used to feed apply. Pin its lossiness
        // so a future execution path cannot accidentally switch back to it.
        assert!(
            first.migrations().is_empty(),
            "the migration-only projection cannot represent data steps"
        );
    }

    #[test]
    fn ordered_status_lowering_projects_create_before_live_dependent_default() {
        let envelopes = ordered_status_envelopes(zero_migrate::model::ir::CURRENT_IR_VERSION);

        let plans = lower_ordered_envelopes_to_plans(
            &envelopes,
            "app_status_ordered",
            "app_status_ordered",
            "postgres",
            "{}",
            None,
            zero_migrate::model::snapshot::SchemaSnapshot::default(),
            &[],
            &[],
        )
        .expect("the follow-up lowers against the created table's projected JSON type");

        assert_eq!(plans.len(), 2);
        assert!(plans.iter().all(|artifact| !artifact.plan.steps.is_empty()));
    }

    #[test]
    fn ordered_lowering_carries_logical_id_contracts_across_artifacts() {
        let owner = "app_cross_artifact_ids";
        let declaration = serde_json::json!({
            "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
            "name": "declare_cross_artifact_ids",
            "ops": [{
                "op": "createTable",
                "name": "cross_artifact_ids",
                "columns": [
                    { "name": "cursor", "type": "int", "nullable": false },
                    { "name": "uuid_id", "type": "uuid" },
                    {
                        "name": "type_id",
                        "type": "text",
                        "valueFormat": { "typeId": { "prefix": "order" } }
                    },
                    { "name": "ulid_id", "type": "text", "valueFormat": "ulid" }
                ],
                "primaryKey": ["cursor"],
                "constraints": [],
                "indexes": []
            }]
        })
        .to_string();
        let backfill = serde_json::json!({
            "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
            "name": "backfill_cross_artifact_ids",
            "ops": [{
                "op": "backfill",
                "table": "cross_artifact_ids",
                "cursorColumns": ["cursor"],
                "cursorStability": { "mode": "guardUpdates" },
                "batchSize": 10,
                "set": {
                    "uuid_id": { "perRow": "uuidV7" },
                    "type_id": { "perRow": { "typeId": { "prefix": "order" } } },
                    "ulid_id": { "perRow": "ulid" }
                },
                "name": "fill_cross_artifact_ids"
            }]
        })
        .to_string();

        let plans = lower_ordered_envelopes_to_plans(
            &[declaration.clone(), backfill.clone()],
            owner,
            owner,
            "postgres",
            "{}",
            None,
            zero_migrate::model::snapshot::SchemaSnapshot::default(),
            &[],
            &[],
        )
        .expect("the backfill resolves all logical families from the prior artifact");
        assert_eq!(plans.len(), 2);
        assert!(matches!(
            plans[1].plan.steps.as_slice(),
            [zero_migrate::PlanStep::Backfill { .. }]
        ));

        let declaration_artifact =
            lower_envelope_to_plan(&declaration, owner, owner, "postgres", "{}", None)
                .expect("the declaration plan lowers independently");
        let declaration_manifest =
            PlanStatusManifest::from_applied_plan(&declaration_artifact.plan, &[])
                .expect("the declaration manifest projects");
        let completed_declaration = declaration_manifest
            .steps
            .iter()
            .map(|step| AppliedEntry {
                version: step.version.as_str().to_string(),
                checksum: step.checksum.as_str().to_string(),
                phase: Phase::Completed,
                kind: None,
            })
            .collect::<Vec<_>>();
        let declaration_ir: MigrationIr =
            serde_json::from_str(&declaration).expect("the declaration IR parses");
        let live_snapshot =
            zero_migrate::fold_ops(&declaration_ir.ops, SqlDialect::Postgres, owner)
                .expect("the applied declaration is reflected in the catalog");
        let applied_prefix_plans = lower_ordered_envelopes_to_plans(
            &[declaration, backfill.clone()],
            owner,
            owner,
            "postgres",
            &format!(r#"{{"cross_artifact_ids":"{owner}"}}"#),
            None,
            live_snapshot,
            &completed_declaration,
            &[],
        )
        .expect("an applied declaration still advances logical metadata for the backfill");
        assert!(matches!(
            applied_prefix_plans[1].plan.steps.as_slice(),
            [zero_migrate::PlanStep::Backfill { .. }]
        ));

        let error = lower_envelope_to_plan(
            &backfill,
            owner,
            owner,
            "postgres",
            &format!(r#"{{"cross_artifact_ids":"{owner}"}}"#),
            None,
        )
        .expect_err("a backfill-only lower without the declaration map must fail closed");
        assert!(
            error.contains("no logical column declaration"),
            "got: {error}"
        );
    }

    #[test]
    fn ordered_status_lowering_projects_an_inflight_create_that_has_not_landed() {
        use zero_migrate::apply::journal::Phase;

        let owner = "app_status_inflight_absent";
        let envelopes = ordered_status_envelopes(zero_migrate::model::ir::CURRENT_IR_VERSION);
        let create = lower_envelope_to_plan(&envelopes[0], owner, owner, "mysql", "{}", None)
            .expect("create plan lowers");
        let manifest = PlanStatusManifest::from_applied_plan(&create.plan, &create.depends_on)
            .expect("create manifest projects");
        let started = AppliedEntry {
            version: manifest.steps[0].version.as_str().to_string(),
            checksum: manifest.steps[0].checksum.as_str().to_string(),
            phase: Phase::Started,
            kind: None,
        };
        let journal_entries = vec![started];

        let plans = lower_ordered_envelopes_to_plans(
            &envelopes,
            owner,
            owner,
            "mysql",
            "{}",
            None,
            zero_migrate::model::snapshot::SchemaSnapshot::default(),
            &journal_entries,
            &[],
        )
        .expect("the dependent envelope lowers against the projected inflight create");

        assert_eq!(plans.len(), 2);
        assert_inflight_then_pending(&plans, &journal_entries);
    }

    #[test]
    fn ordered_status_lowering_uses_live_shape_when_an_inflight_create_landed() {
        use zero_migrate::apply::journal::Phase;

        let owner = "app_status_inflight_landed";
        let envelopes = ordered_status_envelopes(zero_migrate::model::ir::CURRENT_IR_VERSION);
        let create_ir: MigrationIr =
            serde_json::from_str(&envelopes[0]).expect("create envelope parses");
        let live_snapshot = zero_migrate::fold_ops(&create_ir.ops, SqlDialect::Mysql, owner)
            .expect("the inflight create is reflected in the catalog");
        let create = lower_envelope_to_plan(&envelopes[0], owner, owner, "mysql", "{}", None)
            .expect("create plan lowers");
        let manifest = PlanStatusManifest::from_applied_plan(&create.plan, &create.depends_on)
            .expect("create manifest projects");
        let started = AppliedEntry {
            version: manifest.steps[0].version.as_str().to_string(),
            checksum: manifest.steps[0].checksum.as_str().to_string(),
            phase: Phase::Started,
            kind: None,
        };
        let journal_entries = vec![started];

        let plans = lower_ordered_envelopes_to_plans(
            &envelopes,
            owner,
            owner,
            "mysql",
            "{}",
            None,
            live_snapshot,
            &journal_entries,
            &[],
        )
        .expect("the dependent envelope lowers from the already-landed live shape");

        assert_eq!(plans.len(), 2);
        assert_inflight_then_pending(&plans, &journal_entries);
    }

    fn assert_inflight_then_pending(plans: &[LoweredArtifact], journal_entries: &[AppliedEntry]) {
        use zero_migrate::ops::status::{
            reconcile_applied_plans, PlanStatusStepState, ReconciledPlanState,
        };

        let manifests = plans
            .iter()
            .map(|artifact| {
                PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
                    .expect("status manifest projects")
            })
            .collect::<Vec<_>>();
        let status = reconcile_applied_plans(&manifests, journal_entries, &[])
            .expect("ordered plans reconcile");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Partial);
        assert_eq!(
            status.plans[0].steps[0].state,
            PlanStatusStepState::Inflight
        );
        assert_eq!(status.plans[1].state, ReconciledPlanState::Pending);
    }

    #[test]
    fn ordered_status_lowering_does_not_replay_an_applied_prefix() {
        use zero_migrate::apply::journal::{JournaledKind, Phase};

        let envelopes = ordered_status_envelopes(zero_migrate::model::ir::CURRENT_IR_VERSION);
        let create_ir: MigrationIr =
            serde_json::from_str(&envelopes[0]).expect("create envelope parses");
        let live_snapshot =
            zero_migrate::fold_ops(&create_ir.ops, SqlDialect::Postgres, "app_status_ordered")
                .expect("applied create is reflected in the catalog");
        let applied_create = lower_envelope_to_plan(
            &envelopes[0],
            "app_status_ordered",
            "app_status_ordered",
            "postgres",
            "{}",
            None,
        )
        .expect("create plan lowers");
        let manifest =
            PlanStatusManifest::from_applied_plan(&applied_create.plan, &applied_create.depends_on)
                .expect("create manifest projects");
        let journal_entries = manifest
            .steps
            .iter()
            .map(|step| AppliedEntry {
                version: step.version.as_str().to_string(),
                checksum: step.checksum.as_str().to_string(),
                phase: Phase::Completed,
                kind: Some(if step.repeatable {
                    JournaledKind::Repeatable
                } else {
                    JournaledKind::Apply
                }),
            })
            .collect::<Vec<_>>();

        let plans = lower_ordered_envelopes_to_plans(
            &envelopes,
            "app_status_ordered",
            "app_status_ordered",
            "postgres",
            r#"{"status_widgets":"app_status_ordered"}"#,
            None,
            live_snapshot,
            &journal_entries,
            &[],
        )
        .expect("the applied create is not replayed onto its existing table");

        assert_eq!(plans.len(), 2);
    }

    #[test]
    fn ordered_status_lowering_projects_pending_tail_of_partially_applied_envelope() {
        use zero_migrate::apply::journal::{JournaledKind, Phase};

        let owner = "app_status_partial";
        let mixed = serde_json::json!({
            "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
            "name": "create_then_extend_status_widgets",
            "ops": [
                {
                    "op": "createTable",
                    "name": "status_widgets",
                    "columns": [{ "name": "label", "type": "text" }],
                    "primaryKey": null,
                    "constraints": [],
                    "indexes": []
                },
                {
                    "op": "addColumn",
                    "table": "status_widgets",
                    "column": "rank",
                    "type": "int"
                },
                {
                    "op": "addColumn",
                    "table": "status_widgets",
                    "column": "payload",
                    "type": "json"
                }
            ]
        })
        .to_string();
        let follow_up = serde_json::json!({
            "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
            "name": "default_status_widgets_payload",
            "ops": [{
                "op": "setColumnDefault",
                "table": "status_widgets",
                "column": "payload",
                "value": { "container": "object" }
            }]
        })
        .to_string();

        let mixed_ir: MigrationIr = serde_json::from_str(&mixed).expect("mixed envelope parses");
        let live_snapshot = zero_migrate::fold_ops(&mixed_ir.ops[..2], SqlDialect::Postgres, owner)
            .expect("the applied create/add prefix is reflected in the catalog");
        let initial = lower_envelope_to_plan(&mixed, owner, owner, "postgres", "{}", None)
            .expect("mixed plan lowers from its original catalog state");
        let manifest = PlanStatusManifest::from_applied_plan(&initial.plan, &initial.depends_on)
            .expect("mixed manifest projects");
        assert_eq!(
            manifest.steps.len(),
            3,
            "the test needs a two-step applied prefix and one pending tail"
        );
        let journal_entries = manifest.steps[..2]
            .iter()
            .map(|applied| AppliedEntry {
                version: applied.version.as_str().to_string(),
                checksum: applied.checksum.as_str().to_string(),
                phase: Phase::Completed,
                kind: Some(if applied.repeatable {
                    JournaledKind::Repeatable
                } else {
                    JournaledKind::Apply
                }),
            })
            .collect::<Vec<_>>();

        let plans = lower_ordered_envelopes_to_plans(
            &[mixed, follow_up],
            owner,
            owner,
            "postgres",
            r#"{"status_widgets":"app_status_partial"}"#,
            None,
            live_snapshot,
            &journal_entries,
            &[],
        )
        .expect("the follow-up lowers against the mixed envelope's pending addColumn tail");

        assert_eq!(plans.len(), 2);
    }

    fn prefix_gate_manifests() -> Vec<PlanStatusManifest> {
        [
            ("declare_prefix", "prefix_table"),
            ("current_change", "current_table"),
        ]
        .into_iter()
        .map(|(name, table)| {
            let envelope = serde_json::json!({
                "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
                "name": name,
                "ops": [{
                    "op": "createTable",
                    "name": table,
                    "columns": [{ "name": "id", "type": "int" }],
                    "primaryKey": null,
                    "constraints": [],
                    "indexes": []
                }]
            })
            .to_string();
            let artifact = lower_envelope_to_plan(
                &envelope,
                "app_prefix_gate",
                "app_prefix_gate",
                "postgres",
                "{}",
                None,
            )
            .expect("test envelope lowers");
            PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
                .expect("test manifest projects")
        })
        .collect()
    }

    fn prefix_gate_entries(manifest: &PlanStatusManifest, phase: Phase) -> Vec<AppliedEntry> {
        manifest
            .steps
            .iter()
            .map(|step| AppliedEntry {
                version: step.version.as_str().to_string(),
                checksum: step.checksum.as_str().to_string(),
                phase,
                kind: None,
            })
            .collect()
    }

    #[test]
    fn authored_prefix_requires_exact_net_applied_plans() {
        use zero_migrate::ops::status::reconcile_applied_plans;

        let manifests = prefix_gate_manifests();
        let completed = prefix_gate_entries(&manifests[0], Phase::Completed);
        let applied =
            reconcile_applied_plans(&manifests, &completed, &[]).expect("prefix reconciles");
        require_applied_prefix(&manifests, 1, &applied)
            .expect("an exact completed prefix is trusted");

        let pending =
            reconcile_applied_plans(&manifests, &[], &[]).expect("missing prefix reconciles");
        let error = require_applied_prefix(&manifests, 1, &pending)
            .expect_err("a missing prefix must not seed declarations");
        assert!(error.contains("not fully applied"), "got: {error}");

        let inflight_entries = prefix_gate_entries(&manifests[0], Phase::Started);
        let inflight = reconcile_applied_plans(&manifests, &inflight_entries, &[])
            .expect("inflight prefix reconciles");
        let error = require_applied_prefix(&manifests, 1, &inflight)
            .expect_err("an inflight prefix must not seed declarations");
        assert!(error.contains("not fully applied"), "got: {error}");

        let mut drifted_entries = completed;
        drifted_entries[0].checksum = "0".repeat(64);
        let drifted = reconcile_applied_plans(&manifests, &drifted_entries, &[])
            .expect("drifted prefix reconciles");
        let error = require_applied_prefix(&manifests, 1, &drifted)
            .expect_err("a drifted prefix must not seed declarations");
        assert!(error.contains("not fully applied"), "got: {error}");
    }

    #[test]
    fn incomplete_authored_history_only_allows_an_applied_current_replay() {
        use zero_migrate::model::migration::MigrationId;
        use zero_migrate::ops::status::reconcile_applied_plans;

        let manifests = prefix_gate_manifests();
        let mut prior_only = prefix_gate_entries(&manifests[0], Phase::Completed);
        prior_only.push(AppliedEntry {
            version: MigrationId::derive("omitted_artifact", b"step")
                .as_str()
                .to_string(),
            checksum: "1".repeat(64),
            phase: Phase::Completed,
            kind: None,
        });
        let pending_current = reconcile_applied_plans(&manifests, &prior_only, &[])
            .expect("incomplete history reconciles");
        let error = require_applied_prefix(&manifests, 1, &pending_current)
            .expect_err("a pending current cannot apply from incomplete history");
        assert!(error.contains("prefix is incomplete"), "got: {error}");

        let mut replay_entries = prior_only;
        replay_entries.extend(prefix_gate_entries(&manifests[1], Phase::Completed));
        let applied_current = reconcile_applied_plans(&manifests, &replay_entries, &[])
            .expect("applied replay reconciles");
        require_applied_prefix(&manifests, 1, &applied_current)
            .expect("an applied current remains a safe no-op during a directory rerun");
    }
}
