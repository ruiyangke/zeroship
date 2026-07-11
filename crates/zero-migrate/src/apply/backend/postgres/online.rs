//! Postgres online schema-change capability.

use crate::apply::backend::capability::{BackfillSpec, OnlineSchemaChange};
use crate::model::migration::{Checksum, Migration, MigrationFlags, MigrationId};
use crate::render::expand_contract::{
    qualified, quote_ident, ExpandContractAuthor, ExpandContractError, OnlineIntent,
};
use crate::render::step::PlanStep;

/// Re-author the abort DDL for an outstanding online-rename obligation as plain,
/// phase-less [`PlanStep::Ddl`] steps.
pub fn build_abort_steps(
    project_schema: &str,
    pc: &crate::apply::journal::PendingContract,
) -> Result<Vec<PlanStep>, ExpandContractError> {
    let author = ExpandContractAuthor::new(project_schema.to_string(), "deploy-recovery-abort");
    let plan = author.author(&OnlineIntent::RenameColumn {
        table: pc.table.clone(),
        from: pc.from_col.clone(),
        to: pc.to_col.clone(),
        ty: pc.ty.clone(),
    })?;
    let c1 = plan
        .contract
        .first()
        .ok_or_else(|| ExpandContractError::Invalid("re-authored plan has no C1 step".into()))?;
    let tbl_q = qualified(project_schema, &pc.table);
    let to_q = quote_ident(&pc.to_col);
    let drop_shadow = format!("ALTER TABLE {tbl_q} DROP COLUMN IF EXISTS {to_q}");
    Ok(vec![
        PlanStep::Ddl(abort_plain_ddl(
            &format!("recover_abort_drop_trigger_{}", pc.table),
            c1.up.clone(),
            false,
        )),
        PlanStep::Ddl(abort_plain_ddl(
            &format!("recover_abort_drop_shadow_{}", pc.table),
            drop_shadow,
            true,
        )),
    ])
}

fn abort_plain_ddl(name: &str, up: String, destructive: bool) -> Migration {
    use crate::model::migration::ChecksumInput;
    let flags = MigrationFlags {
        destructive,
        requires_approval: destructive,
        ..MigrationFlags::default()
    };
    let mut m = Migration {
        version: MigrationId::generate(),
        name: name.to_string(),
        up,
        down: None,
        checksum: Checksum::of(&ChecksumInput {
            up: "",
            down: None,
            flags: &flags,
            owner_app: "deploy-recovery-abort",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags,
        owner_app: "deploy-recovery-abort".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    };
    m.checksum = Checksum::of(&ChecksumInput::from_migration(&m));
    m
}

/// Run the Postgres EXPAND sequence verbatim.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_expand_pg(
    conn: &compio_postgres::Client,
    expand: &[Migration],
    backfill: &BackfillSpec,
    approval: crate::approval::Approval,
    scope: &crate::approval::ApprovalScope,
    trigger_version: &MigrationId,
    cfg: &crate::conn::ExecutorConfig,
    applied_by: &str,
    lock_mode: crate::apply::executor::LockMode,
) -> Result<crate::apply::executor::ApplyOutcome, crate::engine::OnlineError> {
    use crate::apply::executor::{self, ApplyOutcome};
    if approval != crate::approval::Approval::Approved {
        return Err(crate::engine::OnlineError::Approval);
    }
    let scope_version = expand
        .first()
        .map_or_else(|| trigger_version.as_str(), |e1| e1.version.as_str());
    if !scope.admits(scope_version) {
        return Err(crate::engine::OnlineError::ApprovalNotScoped {
            version: scope_version.to_string(),
        });
    }
    let Some((e3, head)) = expand.split_last() else {
        return Ok(ApplyOutcome {
            applied: Vec::new(),
            skipped: Vec::new(),
            recovered: Vec::new(),
        });
    };
    let mut outcome =
        executor::apply_with_lock(conn, cfg, head, approval, applied_by, lock_mode).await?;
    crate::fault::trip(crate::fault::points::EXPAND_BETWEEN_E2_AND_BACKFILL)?;
    crate::apply::backend::postgres::backfill::run_backfill(
        conn, cfg, backfill, approval, applied_by,
    )
    .await?;
    let e3_outcome = executor::apply_with_lock(
        conn,
        cfg,
        std::slice::from_ref(e3),
        approval,
        applied_by,
        lock_mode,
    )
    .await?;
    outcome.applied.extend(e3_outcome.applied);
    outcome.recovered.extend(e3_outcome.recovered);
    Ok(outcome)
}

/// The Postgres [`OnlineSchemaChange`] implementation.
#[derive(Debug)]
pub struct PgOnline<'a> {
    conn: &'a compio_postgres::Client,
}

impl<'a> PgOnline<'a> {
    /// Wrap a migrator connection as the Postgres online-schema-change capability.
    #[must_use]
    pub fn new(conn: &'a compio_postgres::Client) -> Self {
        Self { conn }
    }
}

impl OnlineSchemaChange for PgOnline<'_> {
    fn run_online<'a>(
        &'a self,
        _intent: &'a OnlineIntent,
        expand: &'a [Migration],
        backfill: &'a BackfillSpec,
        approval: crate::approval::Approval,
        scope: &'a crate::approval::ApprovalScope,
        trigger_version: &'a MigrationId,
        cfg: &'a crate::conn::ExecutorConfig,
        applied_by: &'a str,
        lock_mode: crate::apply::executor::LockMode,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::apply::executor::ApplyOutcome, crate::engine::OnlineError>,
                > + 'a,
        >,
    > {
        Box::pin(run_expand_pg(
            self.conn,
            expand,
            backfill,
            approval,
            scope,
            trigger_version,
            cfg,
            applied_by,
            lock_mode,
        ))
    }
}
