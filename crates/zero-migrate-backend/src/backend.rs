//! The vocabulary the `MigrationBackend` dialect seam speaks.
//!
//! The trait itself is still `zero_migrate::apply::backend::MigrationBackend`.
//! What lives here is the set of neutral VALUES its signatures name — the ones
//! that reach nothing above this crate. They came down first because a vendor
//! crate cannot implement a trait whose argument and return types live in the
//! engine that already depends on it, and because these five reach no engine
//! module at all: `PlaceholderStyle`, `ProjectLockHolder` and
//! `PlanPreconditionVerdict` carry only `std` types, and `JournalFuture` carries
//! only this crate's own `JournalError`.
//!
//! The engine re-exports all of them at `zero_migrate::apply::backend`, the path
//! every existing caller uses.

use std::future::Future;
use std::pin::Pin;

use crate::conn::ExecutorConfig;
use crate::journal::{self, JournalError};

/// How a backend renders a positional bind placeholder in the SQL it issues.
///
/// The lock/journal/DML SQL a backend owns is Postgres-flavoured `$N` today
/// ([`Numbered`](PlaceholderStyle::Numbered)). A MySQL backend renders the
/// anonymous `?` ([`Question`](PlaceholderStyle::Question)) — the placeholder
/// style is a **backend concern**, consulted BEFORE any SQL crosses the
/// [`SqlSession`](crate::driver::SqlSession) seam, so the generic executor never
/// bakes a dialect's placeholder into shared SQL.
///
/// Exposed via `MigrationBackend::placeholder_style` +
/// `MigrationBackend::placeholder`; each backend's own leaves render their SQL
/// with their native style directly (PG session leaves write `$1` literally),
/// and a cross-dialect helper can render positionally through this hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderStyle {
    /// Postgres: the 1-based numbered form `$1`, `$2`, … (also SQLite's `?1`
    /// numbered form is rendered elsewhere via `render::dml`).
    Numbered,
    /// MySQL: the anonymous positional `?` (order-of-appearance binding).
    Question,
}

impl PlaceholderStyle {
    /// Render the `n`-th (1-based) positional bind placeholder in this style.
    /// `$n` for [`Numbered`](Self::Numbered); `?` for [`Question`](Self::Question)
    /// (MySQL binds positionally, so the index is implicit).
    #[must_use]
    pub fn render(self, n: usize) -> String {
        match self {
            Self::Numbered => format!("${n}"),
            Self::Question => "?".to_string(),
        }
    }
}

pub type JournalFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, JournalError>> + 'a>>;

/// One session reported holding the project lock when an acquisition found it
/// taken.
///
/// Carries no duration on purpose. Neither `pg_locks` nor
/// `performance_schema.metadata_locks` records an acquisition timestamp, and every
/// timestamp the surrounding session views offer ages the holder's session or its
/// current statement, neither of which is the age of the lock -- reporting one as
/// "held for" would be a fabricated number an operator would act on.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct ProjectLockHolder {
    /// The holding session's id: a PostgreSQL backend pid, a MySQL connection id
    /// (what `KILL` takes). Either way, the handle an operator acts on.
    pub pid: i64,
    /// Who the holding session is. PostgreSQL reports its `application_name`, when
    /// it set one; MySQL has no such setting, so it reports the holder's account as
    /// `user@host`.
    pub application_name: Option<String>,
    /// The holder's activity state: PostgreSQL's `active` / `idle in transaction`,
    /// or MySQL's command and state (`Query: executing`, `Sleep`).
    pub state: Option<String>,
    /// The holder's current statement. Absent when it is running none, and on
    /// PostgreSQL also when the reading role may not see other sessions' statement
    /// text.
    pub query: Option<String>,
}

/// What a non-blocking project-lock acquisition found.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ProjectLockAcquisition {
    /// The lock is held by this session and must be released as usual.
    Acquired,
    /// A peer holds the lock. Nothing was locked, so there is nothing to release.
    Busy(Vec<ProjectLockHolder>),
}

/// One backend's answer about a single precondition asked of the PRE-PLAN
/// database, before any of the plan's steps has run.
///
/// [`Abstain`](Self::Abstain) is not "met". It says this backend has no
/// plan-level evaluator, so the plan-wide phase must say nothing at all and leave
/// the per-migration seam as the only judge - which is what keeps a backend
/// without a catalog to consult behaving exactly as it does today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanPreconditionVerdict {
    /// No plan-level evaluator on this backend. The plan-wide phase declines to
    /// form an opinion.
    Abstain,
    /// The assertion HOLDS against the pre-plan database.
    Met,
    /// The assertion is UNMET. `blockers` carries the object descriptions the two
    /// blocked-column assertions return, so the refusal can name what the
    /// database says is in the way; empty for every other variant.
    Unmet {
        /// What the catalog says blocks the operation, when the assertion can say.
        blockers: Vec<String>,
    },
}

/// Cross-deploy pending-contract obligations capability.
///
/// Backends return `Some(&dyn CrossDeployObligations)` only when they can open
/// and discharge cross-deploy obligations. If
/// `MigrationBackend::pending_contracts` returns `None`, this capability is
/// structurally absent: reads are empty and writes are no-ops/unreachable routing
/// for that backend.
pub trait CrossDeployObligations {
    /// Read the OUTSTANDING cross-deploy pending-contract obligations —
    /// the apply-time interlock read-back + the `status` orphan/blocked source.
    /// No-op iff `MigrationBackend::pending_contracts` is `None`.
    fn outstanding_pending_contracts<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::PendingContract>>;

    /// Read terminal pending-contract tombstones for status and dependency
    /// enforcement.
    fn resolved_pending_contracts<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::ResolvedPendingContract>>;

    /// Inspect the live table shape before destructive resolution SQL runs.
    fn pending_contract_shape<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        contract: &'a journal::PendingContract,
    ) -> JournalFuture<'a, journal::PendingContractShape>;

    /// Open a `pending` cross-deploy obligation AND, when a
    /// [`journal::DeployRecoveryScope`] is supplied, its `in_progress`
    /// deploy-scoped recovery marker — in ONE transaction. No-op iff
    /// `MigrationBackend::pending_contracts` is `None`.
    fn record_pending_contract_with_recovery<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        rec: journal::PendingContractRecord<'a>,
        scope: Option<journal::DeployRecoveryScope<'a>>,
    ) -> JournalFuture<'a, bool>;

    /// Discharge an obligation by APPENDING a `resolved` row (never a delete —
    /// history is append-only). No-op iff `MigrationBackend::pending_contracts`
    /// is `None`.
    fn resolve_pending_contract<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        pc: &'a journal::PendingContract,
        resolution: journal::Resolution,
        by: &'a str,
    ) -> JournalFuture<'a, ()>;

    /// Promote a WHOLE deploy's recovery markers to `committed` in ONE atomic
    /// transaction. No-op iff `MigrationBackend::pending_contracts` is
    /// `None`.
    fn mark_deploy_recovery_committed_batch<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        deploy_id: &'a str,
        pending_versions: &'a [String],
        by: &'a str,
    ) -> JournalFuture<'a, ()>;

    /// Mark a deploy-scoped recovery obligation `reconciled` (APPEND a
    /// `reconciled` row). No-op iff `MigrationBackend::pending_contracts` is
    /// `None`.
    fn mark_deploy_recovery_reconciled<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        deploy_id: &'a str,
        pending_version: &'a str,
        by: &'a str,
    ) -> JournalFuture<'a, ()>;

    /// Read the net-`in_progress` deploy-recovery markers whose obligation is
    /// still outstanding. Empty iff `MigrationBackend::pending_contracts` is
    /// `None`.
    fn outstanding_deploy_recoveries<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::DeployRecovery>>;
}
