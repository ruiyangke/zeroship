//! The migration journal's dialect-neutral vocabulary - `schema_migrations`.
//!
//! This module names the journal's shared CONCEPTS and nothing else: the wire
//! enums ([`Phase`], [`EventKind`], [`JournaledKind`], [`PendingState`],
//! [`Resolution`]), the row/record shapes ([`AppliedEntry`], [`HistoryEvent`],
//! [`CompletedRecord`], [`PendingContract`], [`BaselineRecord`], ...) and the
//! shared [`JournalError`]. It emits NO SQL and names NO vendor. Each backend
//! writes its own journal in its own dialect:
//!
//! - PostgreSQL - `zeroship_migrate_postgres::backend::journal_sql`
//! - MySQL - `zeroship_migrate_mysql::backend::journal_sql`
//! - SQLite - `zeroship_migrate_sqlite::backend::journal_sql`
//!
//! The wire strings the enums below carry are the CONTRACT between those three
//! implementations: every backend's `CHECK` constraints, INSERTs and net-state
//! readers interpolate the same literals from this single typed source, so the
//! `tests` at the foot of this file pin them byte-exactly.
//!
//! # The shape all three backends implement
//!
//! Append-only + tamper-evident. The journal of record,
//! `<meta>.schema_migrations`, is the SINGLE events table: one row per migration
//! **event** - an `applied` (forward) event or a `rolled_back` event,
//! discriminated by the `event_kind` column. It carries (version, name,
//! checksum, actor, timestamp, exec time) for every event, plus the
//! applied-only fields (kind, phase, outcome) which are NULL on a `rolled_back`
//! row. It is guarded by an **immutability trigger** that rejects UPDATE and
//! DELETE outright - the billing-ledger pattern (`db/changelog/changesets/
//! 0048_credit_ledger.sql`): a correction is a *new* row, never an edit.
//!
//! The total event order is each engine's NATIVE auto-increment key
//! ([`AppliedEntry::event_seq`]). There is no standalone sequence object and no
//! separate rolled-back table: the column assigns a strictly-increasing
//! `event_seq` that never ties (even across two events in one transaction), and
//! the net state of a version is its **latest event** on that scale.
//!
//! Non-transactional migrations (`CREATE INDEX CONCURRENTLY`, ...) cannot wrap
//! their DDL + journal write in one transaction, so they use a **two-phase**
//! protocol around a *separate* mutable side-table,
//! `<meta>.schema_migrations_inflight`: write a `started` marker -> run the DDL
//! -> insert the immutable `completed` row -> drop the marker. A crash leaves a
//! lone `started` marker, which the executor's recovery path detects on the
//! next apply. The inflight table is deliberately NOT immutable (the marker
//! must be deletable on completion); only the journal of record is.
//!
//! The journal lives in a per-project **meta namespace** distinct from the
//! project namespace, so a creator migration confined to its own namespace
//! cannot touch its own history. Each backend's bootstrap is idempotent.

use crate::executor::BackendError;

/// A journal phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// A non-transactional migration begun but not yet confirmed complete
    /// (a marker in the inflight side-table). A lone `Started` on re-run
    /// signals the recovery path.
    Started,
    /// A migration fully applied + recorded in the immutable journal.
    Completed,
}

impl Phase {
    /// The wire string stored in the `phase` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
        }
    }

    /// Parse a phase from its wire string.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "started" => Some(Self::Started),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

/// How a `completed` event was RECORDED (the journaled `kind` column).
///
/// This is the migration's recorded IDENTITY-class, not anything the caller
/// supplies at apply time. The tamper guard decides the
/// repeatable drift exemption on THIS journaled value - never on the
/// attacker-suppliable `flags.repeatable` - so a once-only migration cannot be
/// reclassified into a repeatable (or vice-versa) by flipping the flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournaledKind {
    /// An ordinary once-only migration whose `up` ran (`kind='apply'`).
    Apply,
    /// An adoption baseline: the `up` was recorded NOT run (`kind='baseline'`).
    Baseline,
    /// A squash supersession (`kind='squash'`).
    Squash,
    /// A repeatable migration's re-apply (`kind='repeatable'`). The
    /// only kind whose changed checksum is a legitimate re-run signal.
    Repeatable,
}

impl JournaledKind {
    /// The wire string stored in the `kind` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Baseline => "baseline",
            Self::Squash => "squash",
            Self::Repeatable => "repeatable",
        }
    }

    /// Parse a kind from its wire string.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "apply" => Some(Self::Apply),
            "baseline" => Some(Self::Baseline),
            "squash" => Some(Self::Squash),
            "repeatable" => Some(Self::Repeatable),
            _ => None,
        }
    }

    /// True if this journaled kind is a REPEATABLE re-apply - the only kind whose
    /// changed checksum is a legitimate re-run rather than tamper.
    #[must_use]
    pub const fn is_repeatable(self) -> bool {
        matches!(self, Self::Repeatable)
    }
}

/// The discriminator on the consolidated `schema_migrations` events table: an
/// `applied` (forward) event or a `rolled_back` event. Distinct from the
/// applied-only [`JournaledKind`] (`apply`/`baseline`/`squash`/`repeatable`),
/// which describes the migration TYPE of an `applied` event; `event_kind` is the
/// direction (applied vs rolled-back). A `rolled_back` row carries NULL for the
/// applied-only columns (`kind`/`phase`/`outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A forward apply event (the migration's `up` ran, or was recorded-not-run
    /// for a baseline/squash). Carries `kind`/`phase`/`outcome`.
    Applied,
    /// A rollback event (the migration's `down` ran). The applied-only columns
    /// are NULL.
    RolledBack,
}

impl EventKind {
    /// The wire string stored in the `event_kind` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::RolledBack => "rolled_back",
        }
    }

    /// Parse an event_kind from its wire string.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "applied" => Some(Self::Applied),
            "rolled_back" => Some(Self::RolledBack),
            _ => None,
        }
    }
}

/// The lifecycle state of a cross-deploy online-rename pending-contract
/// obligation. An obligation is born `pending` when an `ExpandContract`
/// EXPAND completes (its C1/C2 contract is deferred to a later deploy); it is
/// discharged by appending a `resolved` row (history is append-only - a discharge
/// is NEVER a DELETE, exactly like the journal of record). The NET state of an
/// obligation is the latest event for its `pending_version` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    /// The obligation is outstanding: the EXPAND ran, the contract (drop trigger
    /// and drop old column) has not yet been applied. Any new op touching the
    /// rename's table is fail-closed refused.
    Pending,
    /// The obligation is discharged: the contract was applied (`--apply`) or the
    /// shadow column + trigger were dropped (`--abort`). Carries a [`Resolution`].
    Resolved,
}

impl PendingState {
    /// The wire string stored in the `state` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolved => "resolved",
        }
    }

    /// Parse a state from its wire string.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "resolved" => Some(Self::Resolved),
            _ => None,
        }
    }
}

/// How a `resolved` pending-contract obligation was discharged (via
/// the `resolve-pending` CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The deferred contract (C1 drop trigger + C2 drop old column) was applied
    /// under [`Approval::Approved`](crate::approval::Approval::Approved) - the rename
    /// completed.
    Applied,
    /// The pending contract was aborted: the shadow (`to`) column + the dual-write
    /// trigger were dropped, returning the table to its pre-rename shape. The
    /// destructive checkpoint is approval-gated, journaled, and append-only.
    Aborted,
}

impl Resolution {
    /// The wire string stored in the `resolution` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Aborted => "aborted",
        }
    }

    /// Parse a resolution from its wire string.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "applied" => Some(Self::Applied),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

/// An OUTSTANDING cross-deploy pending-contract obligation, as read back
/// by `outstanding_pending_contracts`. Its net state is `pending` (no later
/// `resolved` row discharges it). The read-back is the source of truth for the
/// apply-time interlock and the `status` orphan/blocked surfacing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingContract {
    /// App that authored the rename. `None` is reserved for legacy rows created
    /// before pending obligations recorded their author explicitly.
    pub owner_app: Option<String>,
    /// The table the rename targets (bare name; from the rename intent).
    pub table: String,
    /// The existing column being renamed away from.
    pub from_col: String,
    /// The new (shadow) column being renamed to.
    pub to_col: String,
    /// The Postgres type of the column (carried so an `--abort` can reconstruct
    /// the drop, and an `--apply` can re-author C1/C2 if needed).
    pub ty: String,
    /// The APPLY-TIME obligation key: the E2 trigger migration version (the
    /// "expand" id the partition keys on), deterministic per rename
    /// The engine interlock's idempotent-skip + self-EXPAND exemption +
    /// the `resolve-pending` lookup key on this. It is a deep sub-step id that the
    /// plan-level supplied set never exposes - so orphan/blocked do NOT key on it.
    pub pending_version: String,
    /// The rename's PLAN-GROUP version (the `ExpandContract` plan's own group id -
    /// the lowering's plan identity, or E1's deterministic id when the plan carries
    /// none). This is the STABLE identity the SUPPLIED
    /// migration set carries (a re-lowered IR's `lower_plan.version`) and an
    /// author's `depends_on` references, so `status`'s orphan and
    /// blocked surfacing keys on THIS - not the deep E2 `pending_version`
    /// no plan-level set ever exposes.
    pub plan_version: String,
    /// The C1/C2 contract migration versions (so resolve can journal them and the
    /// status/orphan surfacing can name them).
    pub contract_versions: Vec<String>,
}

/// A terminal pending-contract event and the obligation facts it closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPendingContract {
    /// Immutable obligation descriptor copied onto the resolved row.
    pub contract: PendingContract,
    /// Whether the destination or source column was retained.
    pub resolution: Resolution,
}

/// Live invariants required before either pending-contract resolution action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingContractShape {
    /// Both recorded columns exist with the authored base type. When the
    /// authored type includes an explicit modifier, the live modifier matches.
    pub columns_compatible: bool,
    /// Every current row has identical source and destination values.
    pub values_synchronized: bool,
    /// The expected dual-write trigger and function are present and enabled.
    pub trigger_ready: bool,
}

/// The fields of a `pending` pending-contract obligation row, bundled so the
/// writer takes one descriptor (keeping the arg count down).
#[derive(Debug, Clone, Copy)]
pub struct PendingContractRecord<'a> {
    /// App that authored the rename.
    pub owner_app: &'a str,
    /// The table the rename targets (bare name).
    pub table: &'a str,
    /// The existing column name (`from`).
    pub from_col: &'a str,
    /// The new column name (`to`).
    pub to_col: &'a str,
    /// The Postgres type of the column.
    pub ty: &'a str,
    /// The apply-time obligation key - the E2 trigger version.
    pub pending_version: &'a str,
    /// The rename's plan-group version (E1-anchored) - the stable identity the
    /// supplied set / `depends_on` key on for orphan/blocked.
    pub plan_version: &'a str,
    /// The C1/C2 contract version ids, comma-separated-free (serialized as a JSON
    /// array on write; carried here as a slice).
    pub contract_versions: &'a [String],
    /// The actor recorded as opening the obligation.
    pub by: &'a str,
}

/// The deploy-scoped recovery SCOPE threaded into the EXPAND obligation write so
/// the obligation row and its recovery marker are committed in ONE transaction
/// When present, `record_pending_contract_with_recovery` appends a
/// `state='in_progress'` row to `schema_deploy_recovery` in the SAME `BEGIN ... COMMIT`
/// as the `pending` obligation row - so every outstanding obligation ALWAYS has a
/// marker (closing the obligation-vs-marker crash window: the two
/// rows commit atomically or not at all).
///
/// The marker is born `in_progress`, meaning "the deploy that opened this EXPAND has
/// not yet durably reached a terminal outcome." The deploy's success arm later
/// promotes it to `committed` (the legit-pending go-live signal; never recovered);
/// a same-deploy / crash abort closes it `aborted` / `reconciled`. The
/// crash-recovery leg recovers ONLY net-`in_progress` markers - so a phase-1
/// promotion FAILURE leaves the marker in the *recoverable* (fail-safe) state, never
/// the *protected* state (the inversion that closes the false-abort).
#[derive(Debug, Clone, Copy)]
pub struct DeployRecoveryScope<'a> {
    /// The per-deploy id (UUIDv7) the marker is keyed on - generated once per deploy
    /// by the control loop and threaded into the engine apply path so the marker is
    /// engine-stamped in the obligation's transaction.
    pub deploy_id: &'a str,
}

/// One journal entry (completed) or inflight marker (started), as read back for
/// the drift check + pending computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEntry {
    /// The migration version (`mig_...`).
    pub version: String,
    /// The recorded checksum (hex SHA-256). Empty for an inflight `started`
    /// marker only if the marker predates a checksum (we always write it).
    pub checksum: String,
    /// Exact reverse SQL persisted with the latest applied event. `None` marks a
    /// legacy row (or an irreversible migration) and requires compatibility
    /// reconstruction from the checksummed source before rollback.
    pub down: Option<String>,
    /// The phase the entry is in.
    pub phase: Phase,
    /// The journaled `kind` of the LATEST `completed` event for this version
    /// (`None` for a lone `started` inflight marker, which has no completed kind).
    /// The drift/tamper guard anchors the repeatable exemption on THIS value, not
    /// on the supplied `flags.repeatable`.
    pub kind: Option<JournaledKind>,
    /// The journal's monotonic sequence number for the event this entry folds to.
    ///
    /// This is the ONLY sound apply order. Version order is not: `MigrationId::
    /// derive` stamps the high 48 bits with an `0xFF` marker and fills the rest from
    /// a SHA-256, so every IR-authored step id sorts above every `UUIDv7` file id and
    /// derived ids sort among themselves in hash order.
    ///
    /// It rides on the entry rather than being re-derived from `history()` because
    /// "net applied" is a non-trivial fold (latest event per version, keep only
    /// `applied`, union the lone `started` markers) that already has exactly one
    /// implementation. A caller that needed ordering and re-folded `history()` would
    /// be writing a second one.
    pub event_seq: i64,
}

/// Error from a journal operation.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// A database/driver error whose concrete backend type remains
    /// downcastable through [`BackendError`].
    #[error("journal db error: {0}")]
    Db(#[from] BackendError),
    /// A **dialect-neutral** journal backend error whose message is already the
    /// intended operator-facing text. Structured driver failures belong in
    /// [`JournalError::Db`].
    #[error("journal backend error: {0}")]
    Backend(String),
    /// A journal row carried an unrecognized `phase` value.
    #[error("unrecognized journal phase '{0}'")]
    BadPhase(String),
    /// A `completed` journal row carried an unrecognized `kind` value - a
    /// corrupted / tampered row (the CHECK constraint forbids it on write, so
    /// seeing one means out-of-band mutation).
    #[error("unrecognized journal kind '{0}'")]
    BadKind(String),
    /// A journal row carried an unrecognized `event_kind` value - a corrupted /
    /// tampered row (the CHECK constraint forbids it on write, so seeing one
    /// means out-of-band mutation).
    #[error("unrecognized journal event_kind '{0}'")]
    BadEventKind(String),
    /// An engine-supplied identifier (the meta schema or a derived trigger name)
    /// was not quotable (empty or NUL-bearing) at a render seam - fail-closed
    /// rather than interpolate it. Maps [`crate::dml::IdentQuoteError`].
    #[error("journal render: {0}")]
    IdentQuote(#[from] crate::dml::IdentQuoteError),
}

// This conversion used to sit in the PostgreSQL `journal_sql` module, which was
// the only place that needed it. Both types are now foreign to that module, so
// the impl is an orphan there; it belongs beside the error it constructs. Every
// backend's journal reads through the same `DbError` seam, so one home is right.
impl From<crate::driver::DbError> for JournalError {
    fn from(error: crate::driver::DbError) -> Self {
        Self::Db(error.into())
    }
}

/// A net-rolled-back version: one whose **latest** event (on the native
/// `event_seq` IDENTITY scale) is a `rolled_back` event. Such a version is pending
/// again and re-appliable; the status API surfaces it distinctly from net-applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolledBackEntry {
    /// The version (`mig_...`).
    pub version: String,
    /// The migration name recorded on the rollback event.
    pub name: String,
    /// The checksum recorded on the rollback event.
    pub checksum: String,
    /// Who performed the rollback (`applied_by`-equivalent actor string).
    pub rolled_back_by: String,
    /// The rollback `down` execution time in ms (the recorded `exec_ms`).
    pub exec_ms: Option<i64>,
    /// When the rollback was recorded (RFC-3339 / ISO-8601 from `timestamptz`).
    pub at: String,
}

/// The kind of a [`HistoryEvent`] - a forward apply or a rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKind {
    /// An `applied` (forward) event (`event_kind='applied'`).
    Completed,
    /// A `rolled_back` event (`event_kind='rolled_back'`).
    RolledBack,
}

/// One event in the FULL append-only audit log (completed + rolled_back),
/// returned by `history` in `event_seq` order.
///
/// Unlike `applied` (which computes NET state and hides rolled-back history),
/// this is the raw audit trail: it shows EVERY event, including a version's
/// rollback and any subsequent re-apply, in the order they happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEvent {
    /// The shared monotonic sequence number (the total event order).
    pub event_seq: i64,
    /// The migration version (`mig_...`).
    pub version: String,
    /// The migration name recorded on the event.
    pub name: String,
    /// Apply or rollback.
    pub kind: HistoryKind,
    /// The event timestamp (RFC-3339 / ISO-8601 from `timestamptz`).
    pub at: String,
    /// Execution time in ms (the `exec_ms` recorded on the event), if any.
    pub exec_ms: Option<i64>,
    /// The actor who performed the event (`applied_by` / `rolled_back_by`).
    pub applied_by: String,
    /// The checksum recorded on the event.
    pub checksum: String,
}

/// The fields of a non-transactional `completed` journal event, bundled so
/// `record_completed` takes one descriptor (keeping the arg count in check).
#[derive(Debug, Clone, Copy)]
pub struct CompletedRecord<'a> {
    /// The migration version (`mig_...`).
    pub version: &'a str,
    /// The migration name.
    pub name: &'a str,
    /// The migration's checksum.
    pub checksum: &'a str,
    /// The actor recorded in the journal.
    pub applied_by: &'a str,
    /// Wall time the `up` took, in milliseconds.
    pub exec_ms: i64,
    /// The migration kind: `'apply'` for an ordinary migration, `'squash'` for a
    /// fresh-path squash. A fresh-path squash MUST be stamped `'squash'` so its
    /// supersession edges are honored by `superseded_versions` (#4 restricts to
    /// `kind = 'squash'`).
    pub kind: &'a str,
    /// The reverse this apply produced, stored so a later rollback REPLAYS it
    /// rather than re-deriving one with whatever engine is installed then
    /// (F654/F658). `None` for a migration with no reverse; the rollback path
    /// then reconstructs and says so.
    pub down: Option<&'a str>,
}

// -- deploy-scoped recovery markers ------------------------------

/// An OUTSTANDING deploy-scoped recovery obligation: a same-deploy EXPAND whose
/// recovery row is net-`in_progress` (never promoted to `committed`, never closed
/// `reconciled`). Carried back to the control loop so it can drive the same-deploy
/// abort over exactly these `pending_version`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployRecovery {
    /// The per-deploy id (UUIDv7) the EXPAND was opened under.
    pub deploy_id: String,
    /// The obligation key (the EXPAND's E2 trigger version) this row marks for
    /// recovery - the join key into `schema_pending_contracts`.
    pub pending_version: String,
}

/// The fields of a baseline/squash `completed` event recorded WITHOUT running its
/// `up`, bundled so `record_baseline` takes one descriptor.
#[derive(Debug, Clone, Copy)]
pub struct BaselineRecord<'a> {
    /// The migration version (`mig_...`).
    pub version: &'a str,
    /// The migration name.
    pub name: &'a str,
    /// The migration's checksum (so the drift check compares correctly later).
    pub checksum: &'a str,
    /// The actor recorded in the journal (operator / admin).
    pub applied_by: &'a str,
    /// The event `kind`: `'baseline'` (adoption) or `'squash'` (supersession).
    pub kind: &'a str,
    /// The versions this event supersedes (empty for a baseline; `[v1..vN]` for a
    /// squash recorded on an existing DB).
    pub supersedes: &'a [&'a str],
}

#[cfg(test)]
mod tests {
    use super::{EventKind, JournalError, PendingState, Resolution};

    /// The `event_kind` wire contract is byte-exact: every INSERT/SELECT in the
    /// journal (PG + SQLite) now interpolates these literals from this single
    /// typed source, so a drift here would silently desync the schema CHECK,
    /// the writers, and the net-state readers. Pin them.
    #[test]
    fn event_kind_wire_literals_are_exact() {
        assert_eq!(EventKind::Applied.as_str(), "applied");
        assert_eq!(EventKind::RolledBack.as_str(), "rolled_back");
    }

    /// `parse` is the exact inverse of `as_str` for every variant, and rejects
    /// anything else (a corrupted / tampered row), so a read-back can never
    /// silently mis-classify the event direction.
    #[test]
    fn event_kind_parse_round_trips_and_rejects_garbage() {
        for k in [EventKind::Applied, EventKind::RolledBack] {
            assert_eq!(EventKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(EventKind::parse("rolledback"), None);
        assert_eq!(EventKind::parse("Applied"), None);
        assert_eq!(EventKind::parse(""), None);
    }

    /// A read site (e.g. `history`) maps an unparseable `event_kind` to the
    /// dedicated `BadEventKind` arm - NOT `BadPhase` (the pre-fix misuse) - so a
    /// tampered row surfaces a faithful, type-distinct error.
    #[test]
    fn unparseable_event_kind_is_bad_event_kind_not_bad_phase() {
        let raw = "garbage".to_string();
        let err = EventKind::parse(&raw)
            .ok_or(JournalError::BadEventKind(raw))
            .unwrap_err();
        assert!(matches!(err, JournalError::BadEventKind(s) if s == "garbage"));
    }

    /// The cross-deploy pending-contract `state`/`resolution` wire contract is
    /// byte-exact: the `schema_pending_contracts` CHECK constraints, the writers,
    /// and the net-state reader (`outstanding_pending_contracts`) all interpolate these
    /// literals from this single typed source. A drift here would
    /// silently un-gate the interlock (a `pending` row never read back). Pin them.
    #[test]
    fn pending_contract_wire_literals_are_exact() {
        assert_eq!(PendingState::Pending.as_str(), "pending");
        assert_eq!(PendingState::Resolved.as_str(), "resolved");
        assert_eq!(Resolution::Applied.as_str(), "applied");
        assert_eq!(Resolution::Aborted.as_str(), "aborted");
    }

    /// `parse` is the exact inverse of `as_str` for both enums, and rejects
    /// anything else, so a read-back can never silently mis-classify an
    /// obligation's net state (which would un-gate or wrongly-gate the interlock).
    #[test]
    fn pending_contract_parse_round_trips_and_rejects_garbage() {
        for s in [PendingState::Pending, PendingState::Resolved] {
            assert_eq!(PendingState::parse(s.as_str()), Some(s));
        }
        for r in [Resolution::Applied, Resolution::Aborted] {
            assert_eq!(Resolution::parse(r.as_str()), Some(r));
        }
        assert_eq!(PendingState::parse("Pending"), None);
        assert_eq!(PendingState::parse(""), None);
        assert_eq!(Resolution::parse("abort"), None);
        assert_eq!(Resolution::parse(""), None);
    }
}
