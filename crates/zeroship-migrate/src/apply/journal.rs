//! The migration journal — `schema_migrations` (design §2.2).
//!
//! Append-only + tamper-evident. The journal of record,
//! `<meta>.schema_migrations`, is the SINGLE events table: one row per migration
//! **event** — an `applied` (forward) event or a `rolled_back` event,
//! discriminated by the `event_kind` column. It carries (version, name,
//! checksum, actor, timestamp, exec time) for every event, plus the
//! applied-only fields (kind, phase, outcome) which are NULL on a `rolled_back`
//! row. It is guarded by an **immutability trigger** that rejects UPDATE and
//! DELETE outright — the billing-ledger pattern (`db/changelog/changesets/
//! 0048_credit_ledger.sql`): a correction is a *new* row, never an edit.
//!
//! **The total event order is the native auto-increment PK** (`event_seq
//! BIGINT GENERATED ALWAYS AS IDENTITY`). There is NO standalone sequence
//! object and NO separate rolled-back table: a single IDENTITY column assigns a
//! strictly-increasing `event_seq` that never ties (even across two events in
//! one transaction), and the net state of a version is its **latest event** on
//! that scale. `applied()` reads net state off it.
//!
//! Non-transactional migrations (`CREATE INDEX CONCURRENTLY`, …) cannot wrap
//! their DDL + journal write in one transaction, so they use a **two-phase**
//! protocol around a *separate* mutable side-table,
//! `<meta>.schema_migrations_inflight`: write a `started` marker → run the DDL
//! → insert the immutable `completed` row → drop the marker. A crash leaves a
//! lone `started` marker, which the executor's recovery path detects on the
//! next apply. The inflight table is deliberately NOT immutable (the marker
//! must be deletable on completion); only the journal of record is.
//!
//! The journal lives in a per-project **meta schema** distinct from the project
//! schema, so a creator migration confined to its own schema cannot touch its
//! own history. Bootstrap ([`ensure_journal`]) is idempotent.

#[cfg(feature = "native-pg")]
use crate::apply::backend::postgres::PgSession;

use crate::apply::executor::BackendError;
use crate::conn::ExecutorConfig;

/// A journal phase (design §2.2).
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
/// supplies at apply time. The tamper guard (v3 Plan E re-critic) decides the
/// repeatable drift exemption on THIS journaled value — never on the
/// attacker-suppliable `flags.repeatable` — so a once-only migration cannot be
/// reclassified into a repeatable (or vice-versa) by flipping the flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournaledKind {
    /// An ordinary once-only migration whose `up` ran (`kind='apply'`).
    Apply,
    /// An adoption baseline: the `up` was recorded NOT run (`kind='baseline'`).
    Baseline,
    /// A squash supersession (`kind='squash'`).
    Squash,
    /// A repeatable migration's re-apply (`kind='repeatable'`, v3 Plan E). The
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

    /// True if this journaled kind is a REPEATABLE re-apply — the only kind whose
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
/// obligation (§2.0.3). An obligation is born `pending` when a `PgExpandContract`
/// EXPAND completes (its C1/C2 contract is deferred to a later deploy); it is
/// discharged by appending a `resolved` row (history is append-only — a discharge
/// is NEVER a DELETE, exactly like the journal of record). The NET state of an
/// obligation is the latest event for its `pending_version` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    /// The obligation is outstanding: the EXPAND ran, the contract (drop trigger
    /// and drop old column) has not yet been applied. Any new op touching the
    /// rename's table is fail-closed refused (§2.0.3 item 2).
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

/// How a `resolved` pending-contract obligation was discharged (§2.0.3 item 3 /
/// the `resolve-pending` CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The deferred contract (C1 drop trigger + C2 drop old column) was applied
    /// under [`Approval::Approved`](crate::approval::Approval::Approved) — the rename
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

/// An OUTSTANDING cross-deploy pending-contract obligation (§2.0.3), as read back
/// by [`outstanding_pending_contracts`]. Its net state is `pending` (no later
/// `resolved` row discharges it). The read-back is the source of truth for the
/// apply-time interlock and the `status` orphan/blocked surfacing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingContract {
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
    /// "expand" id the §2.0.2 partition keys on), deterministic per rename
    /// (§2.0.1). The engine interlock's idempotent-skip + self-EXPAND exemption +
    /// the `resolve-pending` lookup key on this. It is a deep sub-step id that the
    /// plan-level supplied set never exposes — so orphan/blocked do NOT key on it.
    pub pending_version: String,
    /// The rename's PLAN-GROUP version (the `PgExpandContract` plan's E1-anchored
    /// id, `render::lower::plan_step_version`). This is the STABLE identity the SUPPLIED
    /// migration set carries (a re-lowered IR's `lower_plan().version`) and an
    /// author's `depends_on` references, so `status`'s orphan (§2.0.3 item 3) and
    /// blocked (§2.0.4) surfacing keys on THIS — not the deep E2 `pending_version`
    /// no plan-level set ever exposes (PR9a HIGH).
    pub plan_version: String,
    /// The C1/C2 contract migration versions (so resolve can journal them and the
    /// status/orphan surfacing can name them).
    pub contract_versions: Vec<String>,
}

/// The fields of a `pending` pending-contract obligation row, bundled so the
/// writer takes one descriptor (keeping the arg count down).
#[derive(Debug, Clone, Copy)]
pub struct PendingContractRecord<'a> {
    /// The table the rename targets (bare name).
    pub table: &'a str,
    /// The existing column name (`from`).
    pub from_col: &'a str,
    /// The new column name (`to`).
    pub to_col: &'a str,
    /// The Postgres type of the column.
    pub ty: &'a str,
    /// The apply-time obligation key — the E2 trigger version.
    pub pending_version: &'a str,
    /// The rename's plan-group version (E1-anchored) — the stable identity the
    /// supplied set / `depends_on` key on for orphan/blocked (PR9a HIGH).
    pub plan_version: &'a str,
    /// The C1/C2 contract version ids, comma-separated-free (serialized as a JSON
    /// array on write; carried here as a slice).
    pub contract_versions: &'a [String],
    /// The actor recorded as opening the obligation.
    pub by: &'a str,
}

/// The deploy-scoped recovery SCOPE threaded into the EXPAND obligation write so
/// the obligation row and its recovery marker are committed in ONE transaction
/// (PR9e). When present, [`record_pending_contract_with_recovery`] appends a
/// `state='in_progress'` row to `schema_deploy_recovery` in the SAME `BEGIN … COMMIT`
/// as the `pending` obligation row — so every outstanding obligation ALWAYS has a
/// marker (closing the PR9d-rev finding-1 obligation-vs-marker crash window: the two
/// rows commit atomically or not at all).
///
/// The marker is born `in_progress`, meaning "the deploy that opened this EXPAND has
/// not yet durably reached a terminal outcome." The deploy's success arm later
/// promotes it to `committed` (the legit-pending go-live signal; never recovered);
/// a same-deploy / crash abort closes it `aborted` / `reconciled`. The
/// crash-recovery leg recovers ONLY net-`in_progress` markers — so a phase-1
/// promotion FAILURE leaves the marker in the *recoverable* (fail-safe) state, never
/// the *protected* state (PR9e: the inversion that closes the MED false-abort).
#[derive(Debug, Clone, Copy)]
pub struct DeployRecoveryScope<'a> {
    /// The per-deploy id (UUIDv7) the marker is keyed on — generated once per deploy
    /// by the control loop and threaded into the engine apply path so the marker is
    /// engine-stamped in the obligation's transaction.
    pub deploy_id: &'a str,
}

/// One journal entry (completed) or inflight marker (started), as read back for
/// the drift check + pending computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEntry {
    /// The migration version (`mig_…`).
    pub version: String,
    /// The recorded checksum (hex SHA-256). Empty for an inflight `started`
    /// marker only if the marker predates a checksum (we always write it).
    pub checksum: String,
    /// The phase the entry is in.
    pub phase: Phase,
    /// The journaled `kind` of the LATEST `completed` event for this version
    /// (`None` for a lone `started` inflight marker, which has no completed kind).
    /// The drift/tamper guard anchors the repeatable exemption on THIS value, not
    /// on the supplied `flags.repeatable` (v3 Plan E re-critic).
    pub kind: Option<JournaledKind>,
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
    /// A `completed` journal row carried an unrecognized `kind` value — a
    /// corrupted / tampered row (the CHECK constraint forbids it on write, so
    /// seeing one means out-of-band mutation).
    #[error("unrecognized journal kind '{0}'")]
    BadKind(String),
    /// A journal row carried an unrecognized `event_kind` value — a corrupted /
    /// tampered row (the CHECK constraint forbids it on write, so seeing one
    /// means out-of-band mutation).
    #[error("unrecognized journal event_kind '{0}'")]
    BadEventKind(String),
    /// An engine-supplied identifier (the meta schema or a derived trigger name)
    /// was not quotable (empty or NUL-bearing) at a render seam — fail-closed
    /// rather than interpolate it. Maps [`crate::render::dml::IdentQuoteError`].
    #[error("journal render: {0}")]
    IdentQuote(#[from] crate::render::dml::IdentQuoteError),
}

#[cfg(feature = "native-pg")]
impl From<crate::apply::backend::postgres::seam::SeamError> for JournalError {
    fn from(error: crate::apply::backend::postgres::seam::SeamError) -> Self {
        Self::Db(error.into())
    }
}

/// Quote a SQL identifier by doubling embedded quotes and wrapping in
/// double-quotes, so a schema name is never interpolated as raw SQL. Routes
/// through the ONE crate-shared engine seam ([`crate::render::dml::quote_ident_checked`])
/// — byte-identical to (and uniformly self-defending with)
/// `author`/`backfill`/`role`/`dml`: fail-closed on an empty / NUL identifier.
fn quote_ident(ident: &str) -> Result<String, JournalError> {
    Ok(crate::render::dml::quote_ident_checked(ident)?)
}

/// #150 test seam (see `dml::tests::all_engine_seams_render_uniformly`).
#[cfg(test)]
pub(crate) fn quote_ident_for_test(ident: &str) -> Result<String, JournalError> {
    quote_ident(ident)
}

/// Bootstrap (idempotently) the meta schema + journal table + inflight
/// side-table + immutability trigger (design §2.2).
///
/// Safe to call on every apply: `CREATE SCHEMA/TABLE IF NOT EXISTS`, `CREATE OR
/// REPLACE FUNCTION`, and a `pg_trigger`-guarded `CREATE TRIGGER` make a
/// re-bootstrap a no-op.
///
/// # Errors
/// [`JournalError::Db`] on any DDL failure.
#[cfg(feature = "native-pg")]
pub async fn ensure_journal<D: PgSession>(conn: &D, cfg: &ExecutorConfig) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let trg_fn = quote_ident(&format!("{}_schema_migrations_immutable", cfg.pg.meta_schema))?;
    let meta_lit = cfg.pg.meta_schema.replace('\'', "''");

    // 1. Meta schema.
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {meta}"))
        .await?;

    // 2. The append-only journal of record — the SINGLE consolidated events table
    //    (design §2.2 columns). ONE row per migration EVENT: an `applied` (forward)
    //    event or a `rolled_back` event, discriminated by `event_kind`.
    //
    //    **Native total order.** `event_seq BIGINT GENERATED ALWAYS AS IDENTITY` is
    //    the PK and the total order: the DB assigns a strictly-increasing value on
    //    every INSERT (the INSERTs never supply it). It never ties — `now()` can be
    //    equal across two events in one transaction, but the IDENTITY is monotonic
    //    — so the latest event per version decides net state. There is NO standalone
    //    sequence object (the old `CREATE SEQUENCE … schema_migrations_event_seq` +
    //    `DEFAULT nextval(...)` are gone; the column is its OWN identity).
    //
    //    Rollback is append-only too (Plan 5): an `applied` row is NEVER deleted on
    //    rollback — a `rolled_back` event is appended to THIS SAME table. A
    //    rolled-back migration becomes pending again and may be RE-APPLIED, which
    //    appends a NEW `applied` row for the same version. So `version` is NOT a
    //    primary key here (multiple events per version are legal, across
    //    rollback↔re-apply cycles); `event_seq` is the PK and the total order. The
    //    immutability trigger forbids UPDATE/DELETE, so the log stays append-only.
    //
    //    `event_kind` ∈ {`applied`,`rolled_back`} is the event DIRECTION. Distinct
    //    from `kind` ∈ {`apply`,`baseline`,`squash`,`repeatable`}, the migration
    //    TYPE of an `applied` event (Plan 9): an ordinary `apply` (the `up` actually
    //    ran), a `baseline` (the schema already existed; the `up` recorded NOT run —
    //    adoption path), a `squash` (a supersession; the squash's `up` recorded NOT
    //    run because `[v1..vN]` were already applied — see [`record_baseline`] /
    //    [`crate::ops::squash`]), or a `repeatable` (v3 Plan E — a re-applied
    //    repeatable's `up` ran, but the version's IDENTITY is a repeatable). The
    //    `repeatable` kind is LOAD-BEARING for the tamper guard: the drift exemption
    //    anchors on the JOURNALED kind, not the attacker-suppliable
    //    `flags.repeatable`, so flipping an applied once-only to `repeatable=true` is
    //    a kind mismatch ⇒ tamper. The applied-only columns (`kind`/`phase`/
    //    `outcome`) are NULL on a `rolled_back` row; a CHECK documents the
    //    per-`event_kind` shape (`applied` ⇒ all three NOT NULL; `rolled_back` ⇒ all
    //    three NULL). `by`/`at` unify the old `applied_by`/`rolled_back_by` and
    //    `applied_at`/`rolled_back_at`.
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations (
            event_seq   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            event_kind  TEXT NOT NULL CHECK (event_kind IN ('applied','rolled_back')),
            version     TEXT NOT NULL,
            name        TEXT NOT NULL,
            checksum    TEXT NOT NULL,
            \"at\"        TIMESTAMPTZ NOT NULL DEFAULT now(),
            \"by\"        TEXT NOT NULL,
            exec_ms     BIGINT,
            phase       TEXT CHECK (phase IS NULL OR phase IN ('started','completed')),
            outcome     TEXT,
            kind        TEXT CHECK (kind IS NULL OR kind IN ('apply','baseline','squash','repeatable')),
            CONSTRAINT schema_migrations_event_shape CHECK (
                (event_kind = 'applied'
                     AND kind IS NOT NULL AND phase IS NOT NULL AND outcome IS NOT NULL)
                OR
                (event_kind = 'rolled_back'
                     AND kind IS NULL AND phase IS NULL AND outcome IS NULL)
            )
        )"
    ))
    .await?;

    // 2a-bis. The append-only SUPERSESSION log (Plan 9 squash). One row per
    //     (squash_version → superseded_version) edge, written by the ADMIN when a
    //     squash migration `S` is journaled (whether via `apply` running its `up`
    //     on a fresh DB, or via [`crate::ops::squash`] recording it baseline-style on a
    //     DB that already ran `[v1..vN]`). The pending computation joins this
    //     against net-applied squashes to decide that a superseded version is
    //     SATISFIED. Append-only + immutable (trigger below): a squash's
    //     supersession edges are part of history and never edited/deleted. Edges
    //     are recorded LAST (after the `completed` row), so a net-applied squash
    //     always has its full edge set; a partial edge set never exists because the
    //     squash's `completed` row + its edges are written in one transaction by the
    //     caller. (No FK to `schema_migrations` — that table allows multiple
    //     `completed` rows per version, so there is no single PK to reference; the
    //     squash_version is validated by the caller before journaling.)
    //     (No shared sequence: supersedes is a relation [set-membership of edges],
    //     not part of the event total order, so it gets its OWN native IDENTITY PK.)
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations_supersedes (
            id                 BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            squash_version     TEXT NOT NULL,
            superseded_version TEXT NOT NULL,
            recorded_at        TIMESTAMPTZ NOT NULL DEFAULT now()
        )"
    ))
    .await?;

    // 2a-ter. The append-only CROSS-DEPLOY PENDING-CONTRACT obligation log
    //     (§2.0.3 / §2.0.4). A `PgExpandContract` online rename applies its EXPAND
    //     (E1..E3 + backfill) in deploy N and DEFERS its contract (C1 drop trigger
    //     + C2 drop old column) to a later deploy (§2.0.2). The deferred contract
    //     is a DURABLE OBLIGATION — not a transient return value — so a later deploy
    //     (or a restarted process) can READ IT BACK and fail closed on any op that
    //     touches the rename's table while the contract is still outstanding.
    //
    //     **Append-only + immutable (trigger below), exactly like
    //     `schema_migrations`.** An obligation is born `pending`; it is discharged
    //     by APPENDING a `resolved` row (never a DELETE/UPDATE). The NET state per
    //     `pending_version` is the latest `event_seq` row (DISTINCT-ON-latest, the
    //     same pattern `applied()` uses). A `pending` row with no later `resolved`
    //     row is OUTSTANDING.
    //
    //     **Unforgeable by the migrator (deny-by-absence).** It lives in the meta
    //     schema, which the migrator role has NO grant on (`role.rs` REVOKE +
    //     search_path exclusion). All writes are by the ADMIN role on the executor
    //     path, mirroring the journal-forgery defense for `schema_migrations`: a
    //     creator migration's `up` (running as the migrator) can neither plant a
    //     bogus `resolved` row (suppressing the interlock) nor a bogus `pending`
    //     row (wedging an unrelated table).
    //
    //     `state` ∈ {`pending`,`resolved`}; `resolution` ∈ {`applied`,`aborted`}
    //     and is NULL iff `state='pending'` (the per-state shape CHECK). `table` is
    //     the rename's bare target table (the interlock's match key);
    //     `pending_version` is the APPLY-TIME obligation key (the E2 trigger id) —
    //     deterministic per rename (§2.0.1), used by the engine interlock's
    //     idempotent-skip + self-EXPAND exemption + the `resolve-pending` lookup;
    //     `plan_version` is the rename's PLAN-GROUP version (the PgExpandContract
    //     plan's E1-anchored id, `render::lower::plan_step_version`) — the STABLE
    //     identity the SUPPLIED migration set carries (a re-lowered IR's
    //     `lower_plan().version`) and an author's `depends_on` references, so
    //     `status`'s orphan/blocked surfacing keys on THIS, not the deep E2
    //     sub-version that no plan-level set ever exposes (PR9a HIGH);
    //     `contract_versions` is the JSON array of C1/C2 ids.
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_pending_contracts (
            event_seq         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            state             TEXT NOT NULL CHECK (state IN ('pending','resolved')),
            \"table\"           TEXT NOT NULL,
            from_col          TEXT NOT NULL,
            to_col            TEXT NOT NULL,
            ty                TEXT NOT NULL,
            pending_version   TEXT NOT NULL,
            plan_version      TEXT NOT NULL,
            contract_versions TEXT NOT NULL,
            resolution        TEXT CHECK (resolution IS NULL OR resolution IN ('applied','aborted')),
            \"by\"              TEXT NOT NULL,
            \"at\"              TIMESTAMPTZ NOT NULL DEFAULT now(),
            CONSTRAINT schema_pending_contracts_state_shape CHECK (
                (state = 'pending'  AND resolution IS NULL)
                OR
                (state = 'resolved' AND resolution IS NOT NULL)
            )
        )"
    ))
    .await?;

    // 2a-quater. The append-only DEPLOY-SCOPED RECOVERY marker log (PR9d MED / PR9e).
    //     A multi-file APPROVED bundle applies per-step with NO whole-bundle
    //     transaction (`executor.rs`: BEGIN;up;journal;COMMIT per migration). So an
    //     in-scope online-rename EXPAND in file A commits durably (E1/E2/E3 +
    //     dual-write trigger + the `schema_pending_contracts` obligation) BEFORE a
    //     LATER file B in the SAME deploy can fail at apply for a runtime reason the
    //     read-only pre-validation cannot predict. Pre-PR9d that left file A's table
    //     half-renamed behind the creator's 4xx, owing a pending contract.
    //
    //     This log makes the WHOLE deploy a RECOVERABLE unit. Each same-deploy EXPAND
    //     writes an `in_progress` row keyed on a per-deploy `deploy_id` (a UUIDv7
    //     generated once at the start of the IR loop) + the EXPAND's `pending_version`
    //     — and that row is committed in the SAME transaction as the obligation row
    //     (PR9e — [`record_pending_contract_with_recovery`]), so every outstanding
    //     obligation ALWAYS has a marker. If a later same-deploy file fails, the
    //     control loop drives the SHARED abort (`build_abort_steps`) over exactly THIS
    //     deploy's net-`in_progress` rows under the still-held project lock, discharging
    //     each obligation `aborted` and marking the recovery row `reconciled` — so the
    //     refused bundle leaves NO half-renamed table. On a process CRASH between the
    //     EXPAND+marker commit and the in-process abort, the `in_progress` row SURVIVES;
    //     the NEXT same-app deploy (serialized by the project lock) reconciles it FIRST,
    //     before applying the new bundle. On deploy SUCCESS the loop promotes its rows to
    //     `committed` (the EXPANDs legitimately remain pending as the §2.0.2 cross-deploy
    //     partition — NOT a half-state).
    //
    //     **Strictly same-deploy-scoped.** Both legs operate only over rows whose
    //     `deploy_id` is THIS deploy's (in-process) or a net-`in_progress` prior-deploy
    //     row whose obligation is still `outstanding` (crash leg).
    //
    //     **The crash-vs-legit discriminator (PR9e — the inversion).** The marker is
    //     born `in_progress` (with the obligation, atomically), and `in_progress` itself
    //     IS the "this deploy has not durably reached a terminal outcome" signal. The
    //     success arm PROMOTES it to `committed` in one atomic batch; the crash leg's
    //     resume query (`outstanding_deploy_recoveries`) recovers ONLY net-`in_progress`
    //     markers. A net-`committed` marker is a fully-reached go-live ⇒ EXCLUDED, never
    //     aborted. Crucially, if the `committed` promotion FAILS (DB unreachable the
    //     instant the go-live reaches its success arm) the marker stays net-`in_progress`
    //     — the *recoverable* (fail-safe) state — so the next deploy AUTO-ABORTS the
    //     half-rename rather than silently mistaking a no-longer-protected marker for a
    //     committed one. A *pending* contract has not cut over reads/writes to the shadow
    //     column (the dual-write trigger keeps both in sync, and the drop-old-column
    //     contract has not run), so aborting it loses no data. This is the inverse of the
    //     pre-PR9e `open`+later-stamp design, whose stamp-failure direction *protected*
    //     the marker and let a later deploy silently revert a live contract it could not
    //     distinguish from a crash.
    //
    //     **Append-only + immutable + admin-only** (same posture as
    //     `schema_pending_contracts`): `state` transitions
    //     in_progress→committed/aborted/reconciled by APPENDING a row (never
    //     UPDATE/DELETE); the net state per (deploy_id, pending_version) is the latest
    //     `event_seq` row. The migrator role has NO grant on the meta schema, so a
    //     creator migration can neither forge nor suppress a recovery marker.
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_deploy_recovery (
            event_seq        BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            deploy_id        TEXT NOT NULL,
            pending_version  TEXT NOT NULL,
            state            TEXT NOT NULL CHECK (state IN ('in_progress','committed','aborted','reconciled')),
            \"by\"             TEXT NOT NULL,
            \"at\"             TIMESTAMPTZ NOT NULL DEFAULT now()
        )"
    ))
    .await?;

    // 2b. The MUTABLE inflight side-table for two-phase non-txn markers. NOT
    //     guarded by the immutability trigger — the marker is deleted on
    //     completion / recovery.
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_migrations_inflight (
            version     TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            checksum    TEXT NOT NULL,
            started_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            applied_by  TEXT NOT NULL
        )"
    ))
    .await?;

    // 3. Immutability trigger function (billing-ledger pattern,
    //    0048_credit_ledger). Reject UPDATE + DELETE outright. Shared by both
    //    append-only tables (the consolidated schema_migrations events table +
    //    …_supersedes).
    conn.batch_execute(&format!(
        "CREATE OR REPLACE FUNCTION {trg_fn}() RETURNS trigger AS $fn$
         BEGIN
             RAISE EXCEPTION 'migration journal is append-only (no UPDATE/DELETE)';
         END;
         $fn$ LANGUAGE plpgsql"
    ))
    .await?;

    // 4. Attach the immutability triggers idempotently to BOTH append-only
    //    tables — the consolidated events table + …_supersedes (PG 16 has no
    //    CREATE TRIGGER IF NOT EXISTS; guard on pg_trigger). Fewer triggers overall
    //    now that the two event tables are one.
    //
    //    TWO triggers per table, both calling the same RAISE function:
    //      - `BEFORE UPDATE OR DELETE ... FOR EACH ROW` — blocks row mutation.
    //      - `BEFORE TRUNCATE ... FOR EACH STATEMENT` — blocks TRUNCATE, which
    //        row-level triggers DO NOT fire on. Without the statement-level
    //        TRUNCATE trigger, `TRUNCATE {meta}.schema_migrations` would silently
    //        wipe the append-only journal. (Defense-in-depth: TRUNCATE is only
    //        reachable on the trusted-admin path — the migrator role has no grant
    //        on the meta schema — but the journal must be immutable by
    //        construction, not by least-privilege alone.)
    //
    //    Trigger names are SHORT and table-local (`zs_immutable_trg`,
    //    `zs_immutable_truncate_trg`) — a trigger name only needs to be unique
    //    per table, not per schema, so it need NOT embed the meta_schema. This is
    //    deliberate: the meta_schema under the per-app deploy model is
    //    `"<app_id>_migrations"` (a hyphenated UUID, ~37 chars). Embedding it in
    //    the trigger name overflows PostgreSQL's 63-byte NAMEDATALEN limit — the
    //    name is silently truncated, which (a) makes distinct row vs TRUNCATE
    //    names collide and (b) makes the full-name pg_trigger existence guard
    //    never match the truncated catalog name (re-bootstrap churn). Short
    //    fixed names sidestep all of that. They are still quoted as identifiers
    //    (`trg_q`) for uniformity, and the existence check compares the raw name
    //    as a string literal (`trg_lit`).
    for tbl in [
        "schema_migrations",
        "schema_migrations_supersedes",
        "schema_pending_contracts",
        "schema_deploy_recovery",
    ] {
        for (trg, level, events) in [
            ("zs_immutable_trg", "FOR EACH ROW", "UPDATE OR DELETE"),
            (
                "zs_immutable_truncate_trg",
                "FOR EACH STATEMENT",
                "TRUNCATE",
            ),
        ] {
            let trg_lit = trg.replace('\'', "''");
            let trg_q = quote_ident(trg)?;
            conn.batch_execute(&format!(
                "DO $do$ BEGIN
                    IF NOT EXISTS (
                        SELECT 1 FROM pg_trigger t
                        JOIN pg_class c ON c.oid = t.tgrelid
                        JOIN pg_namespace n ON n.oid = c.relnamespace
                        WHERE t.tgname = '{trg_lit}'
                          AND c.relname = '{tbl}'
                          AND n.nspname = '{meta_lit}'
                    ) THEN
                        EXECUTE 'CREATE TRIGGER {trg_q}
                                 BEFORE {events} ON {meta}.{tbl}
                                 {level} EXECUTE FUNCTION {trg_fn}()';
                    END IF;
                END $do$"
            ))
            .await?;
        }
    }

    Ok(())
}

/// Read the **net applied state** of the journal for the drift check + pending
/// computation, ordered by version (`UUIDv7` apply order).
///
/// The journal is append-only, including rollback (Plan 5): an `applied` row is
/// never deleted; rollback **appends** a `rolled_back` event to the SAME
/// `schema_migrations` events table, and a re-apply appends a fresh `applied`
/// row. So a version can carry several events over rollback↔re-apply cycles. The
/// NET state of a version is decided by its **latest event** on the native
/// monotonic `event_seq` (IDENTITY PK) scale:
///
/// - latest event is `applied` ⇒ the version is **applied** (returned as a
///   [`Phase::Completed`] entry carrying that latest applied row's checksum, so
///   the drift check compares against the current incarnation);
/// - latest event is `rolled_back` ⇒ the version is **pending again** (NOT
///   returned as completed; it re-enters `pending = set − completed` and can be
///   re-applied);
/// - no completed row at all but a lone `started` inflight marker ⇒ returned as a
///   [`Phase::Started`] entry (the non-txn crash-recovery key), exactly as before.
///
/// # Errors
/// [`JournalError::Db`] on query failure; [`JournalError::BadPhase`] if a stored
/// phase value is unrecognized.
#[cfg(feature = "native-pg")]
pub async fn applied<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<AppliedEntry>, JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    // Single-table net state: take the LATEST event per version (DISTINCT ON over
    // the consolidated events table, by `event_seq DESC`) and keep only the
    // versions whose latest event is `applied` (net-applied). Then UNION the lone
    // `started` inflight markers for versions that are NOT net-applied. No 3-way
    // UNION of separate tables — `event_kind` is now a column on the one table.
    // `mig_kind` carries the journaled `kind` column of the latest event (NULL for
    // a `rolled_back` event, which has no kind) and rides through to the net-applied
    // entry so the drift/tamper guard can read it. The net-applied entry is
    // surfaced as `phase='completed'` (the executor's pending/drift machinery keys
    // on Phase, not the raw stored phase).
    let rows = conn
        .query(
            &format!(
                "WITH latest AS (
                     SELECT DISTINCT ON (version) version, checksum, event_kind, kind AS mig_kind
                       FROM {meta}.schema_migrations
                      ORDER BY version, event_seq DESC
                 ),
                 net_applied AS (
                     SELECT version, checksum, mig_kind
                       FROM latest WHERE event_kind = '{applied}'
                 ),
                 union_all AS (
                     SELECT version, checksum, mig_kind, 'completed' AS phase
                       FROM net_applied
                     UNION ALL
                     SELECT version, checksum, NULL AS mig_kind, 'started' AS phase
                       FROM {meta}.schema_migrations_inflight i
                      WHERE NOT EXISTS (
                          SELECT 1 FROM net_applied n WHERE n.version = i.version
                      )
                 )
                 SELECT version, checksum, mig_kind, phase FROM union_all
                 ORDER BY version COLLATE \"C\"",
                applied = EventKind::Applied.as_str()
            ),
            &[],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let version: String = row.get("version");
        let checksum: String = row.get("checksum");
        let phase_s: String = row.get("phase");
        let phase = Phase::parse(&phase_s).ok_or(JournalError::BadPhase(phase_s))?;
        // The journaled kind of the latest completed event. A `started` marker
        // carries NULL; a completed row whose kind is unrecognized is a tampered /
        // corrupt journal row — surface it rather than silently treating it as a
        // benign apply.
        let kind = match row.try_get::<_, Option<String>>("mig_kind") {
            Ok(Some(s)) => Some(JournaledKind::parse(&s).ok_or(JournalError::BadKind(s))?),
            _ => None,
        };
        out.push(AppliedEntry {
            version,
            checksum,
            phase,
            kind,
        });
    }
    Ok(out)
}

/// A net-rolled-back version: one whose **latest** event (on the native
/// `event_seq` IDENTITY scale) is a `rolled_back` event. Such a version is pending
/// again and re-appliable; the status API surfaces it distinctly from net-applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolledBackEntry {
    /// The version (`mig_…`).
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

/// The kind of a [`HistoryEvent`] — a forward apply or a rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKind {
    /// An `applied` (forward) event (`event_kind='applied'`).
    Completed,
    /// A `rolled_back` event (`event_kind='rolled_back'`).
    RolledBack,
}

/// One event in the FULL append-only audit log (completed + rolled_back),
/// returned by [`history`] in `event_seq` order.
///
/// Unlike [`applied`] (which computes NET state and hides rolled-back history),
/// this is the raw audit trail: it shows EVERY event, including a version's
/// rollback and any subsequent re-apply, in the order they happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEvent {
    /// The shared monotonic sequence number (the total event order).
    pub event_seq: i64,
    /// The migration version (`mig_…`).
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

/// Read the versions whose **latest** event is a `rolled_back` event (net
/// rolled-back), ordered by version.
///
/// Mirrors [`applied`]'s DISTINCT-ON-latest-event logic, but keeps the versions
/// whose winning event is `rolled_back` (not `completed`). Carries the rollback
/// event's detail (name, checksum, actor, exec_ms, timestamp) for the status API.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
#[cfg(feature = "native-pg")]
pub async fn net_rolled_back<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<RolledBackEntry>, JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "WITH latest AS (
                     SELECT DISTINCT ON (version)
                            version, name, checksum, \"by\" AS actor, exec_ms,
                            \"at\" AS at, event_kind
                       FROM {meta}.schema_migrations
                      ORDER BY version, event_seq DESC
                 )
                 SELECT version, name, checksum, actor,
                        exec_ms, to_char(at, 'YYYY-MM-DD\"T\"HH24:MI:SS.USOF') AS at
                   FROM latest
                  WHERE event_kind = '{rolled_back}'
                  ORDER BY version COLLATE \"C\"",
                rolled_back = EventKind::RolledBack.as_str()
            ),
            &[],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(RolledBackEntry {
            version: row.get("version"),
            name: row.get("name"),
            checksum: row.get("checksum"),
            rolled_back_by: row.get("actor"),
            exec_ms: row.get("exec_ms"),
            at: row.get("at"),
        });
    }
    Ok(out)
}

/// Read the FULL append-only event log (every `applied` + every `rolled_back`
/// event) in `event_seq` order — the audit trail (design §2.2, scenario 46). One
/// table, ordered by the native IDENTITY PK.
///
/// This is NOT net state: a version that was applied, rolled back, and re-applied
/// appears as three events here. Read-only.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
#[cfg(feature = "native-pg")]
pub async fn history<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<HistoryEvent>, JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "SELECT event_seq, version, name,
                        to_char(\"at\", 'YYYY-MM-DD\"T\"HH24:MI:SS.USOF') AS at,
                        exec_ms, \"by\" AS actor, checksum, event_kind
                   FROM {meta}.schema_migrations
                 ORDER BY event_seq"
            ),
            &[],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let kind_s: String = row.get("event_kind");
        // Route the read-back string through the typed `EventKind` contract: an
        // unparseable value is a corrupted / tampered row, surfaced as a typed
        // error rather than silently mis-classified.
        let kind = match EventKind::parse(&kind_s).ok_or(JournalError::BadEventKind(kind_s))? {
            EventKind::Applied => HistoryKind::Completed,
            EventKind::RolledBack => HistoryKind::RolledBack,
        };
        out.push(HistoryEvent {
            event_seq: row.get("event_seq"),
            version: row.get("version"),
            name: row.get("name"),
            kind,
            at: row.get("at"),
            exec_ms: row.get("exec_ms"),
            applied_by: row.get("actor"),
            checksum: row.get("checksum"),
        });
    }
    Ok(out)
}

/// Append a `rolled_back` event for a version (Plan 5 rollback).
///
/// Run by the **ADMIN** (the migrator has no grant on the meta schema — Plan 3
/// C1): the executor brackets a rollback's `down` SQL under the migrator role,
/// then `RESET ROLE`s back to admin before this journal append, exactly mirroring
/// the up path. The append is immutable (UPDATE/DELETE forbidden by trigger); a
/// later re-apply appends a fresh `applied` row, and `applied()` reads the latest
/// event per version off the native `event_seq` IDENTITY. A `rolled_back` row
/// carries NULL for the applied-only columns (`kind`/`phase`/`outcome`), per the
/// `event_kind` shape CHECK.
///
/// # Errors
/// [`JournalError::Db`] on insert failure.
#[cfg(feature = "native-pg")]
pub async fn record_rolled_back<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
    name: &str,
    checksum: &str,
    rolled_back_by: &str,
    exec_ms: i64,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let n = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations
                     (event_kind, version, name, checksum, \"by\", exec_ms)
                 VALUES ('{rolled_back}', $1, $2, $3, $4, $5)",
                rolled_back = EventKind::RolledBack.as_str()
            ),
            &[
                version.into(),
                name.into(),
                checksum.into(),
                rolled_back_by.into(),
                exec_ms.into(),
            ],
        )
        .await?;
    debug_assert_eq!(n, 1, "record_rolled_back must insert exactly one event row");
    Ok(())
}

/// Write the `started` inflight marker for a non-transactional migration
/// (phase 1 of the two-phase protocol). On a crash this lone marker is what the
/// recovery path keys on.
///
/// # Errors
/// [`JournalError::Db`] on insert failure.
#[cfg(feature = "native-pg")]
pub async fn record_started<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
    name: &str,
    checksum: &str,
    applied_by: &str,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    conn.execute(
        &format!(
            "INSERT INTO {meta}.schema_migrations_inflight
                 (version, name, checksum, applied_by)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (version) DO NOTHING"
        ),
        &[version.into(), name.into(), checksum.into(), applied_by.into()],
    )
    .await?;
    Ok(())
}

/// The fields of a non-transactional `completed` journal event, bundled so
/// [`record_completed`] takes one descriptor (keeping the arg count in check).
#[derive(Debug, Clone, Copy)]
pub struct CompletedRecord<'a> {
    /// The migration version (`mig_…`).
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
    /// supersession edges are honored by [`superseded_versions`] (#4 restricts to
    /// `kind = 'squash'`).
    pub kind: &'a str,
}

/// Finalize a non-transactional migration (phase 2): insert the immutable
/// `completed` journal row, then clear the inflight marker.
///
/// # Errors
/// [`JournalError::Db`] on failure.
#[cfg(feature = "native-pg")]
pub async fn record_completed<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    rec: CompletedRecord<'_>,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    // Plain INSERT (consistent with the transactional path, M3). `event_seq` is a
    // surrogate identity PK, so this appends a fresh `completed` event — including
    // a re-apply after a rollback (Plan 5), where a prior `completed` + a later
    // `rolled_back` already exist for this version and `applied()` made it pending
    // again. Append-only: never an UPDATE.
    let n = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations
                     (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind)
                 VALUES ('{applied}', $1, $2, $3, $4, $5, 'completed', 'success', $6)",
                applied = EventKind::Applied.as_str()
            ),
            &[
                rec.version.into(),
                rec.name.into(),
                rec.checksum.into(),
                rec.applied_by.into(),
                rec.exec_ms.into(),
                rec.kind.into(),
            ],
        )
        .await?;
    debug_assert_eq!(n, 1, "record_completed must insert exactly one journal row");
    conn.execute(
        &format!("DELETE FROM {meta}.schema_migrations_inflight WHERE version = $1"),
        &[rec.version.into()],
    )
    .await?;
    Ok(())
}

/// Read the OUTSTANDING cross-deploy pending-contract obligations (§2.0.3).
///
/// The net state per `pending_version` is its **latest event** on the
/// `event_seq` IDENTITY scale (the same DISTINCT-ON-latest pattern [`applied`]
/// uses). An obligation is OUTSTANDING iff its latest row is `state='pending'`
/// (no later `resolved` row discharges it). This is the source of truth for the
/// apply-time interlock read-back and the `status` orphan/blocked surfacing.
///
/// Admin-run (the whole helper family is — the migrator has no meta-schema
/// grant), so a creator migration's `up` can neither read nor forge this set.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
#[cfg(feature = "native-pg")]
pub async fn outstanding_pending_contracts<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<PendingContract>, JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "WITH latest AS (
                     SELECT DISTINCT ON (pending_version)
                            pending_version, plan_version, state, \"table\", from_col,
                            to_col, ty, contract_versions
                       FROM {meta}.schema_pending_contracts
                      ORDER BY pending_version, event_seq DESC
                 )
                 SELECT pending_version, plan_version, \"table\", from_col, to_col, ty,
                        contract_versions
                   FROM latest
                  WHERE state = '{pending}'
                  ORDER BY pending_version",
                pending = PendingState::Pending.as_str()
            ),
            &[],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let cv_json: String = row.get("contract_versions");
        // The contract_versions column is a JSON array of version-id strings. A
        // parse failure is a corrupted/out-of-band-mutated row — fail closed by
        // surfacing it as a Db-class error rather than silently dropping the
        // obligation (which would un-gate the table).
        let contract_versions: Vec<String> = serde_json::from_str(&cv_json).map_err(|e| {
            JournalError::Backend(format!(
                "pending-contract row has un-parseable contract_versions JSON \
                 (corrupted / out-of-band mutation): {e}"
            ))
        })?;
        out.push(PendingContract {
            table: row.get("table"),
            from_col: row.get("from_col"),
            to_col: row.get("to_col"),
            ty: row.get("ty"),
            pending_version: row.get("pending_version"),
            plan_version: row.get("plan_version"),
            contract_versions,
        });
    }
    Ok(out)
}

/// Open a cross-deploy pending-contract obligation: INSERT a `state='pending'`
/// row (§2.0.3 item 1). Called once per `PgExpandContract` whose EXPAND completed.
///
/// **Idempotent by membership-guard (Deliverable 5).** The caller checks the
/// outstanding set under the held project lock and only records if this
/// `pending_version` is NOT already outstanding, so an idempotent re-run of deploy
/// N (EXPAND net-applied-skipped) does NOT append a duplicate `pending` row — yet
/// the obligation stays outstanding. The append-only history is preserved (a
/// re-open after a resolve would legitimately append a new `pending` row, but the
/// rename id is deterministic so that never happens in practice).
///
/// Open a cross-deploy pending-contract obligation AND, when a
/// [`DeployRecoveryScope`] is supplied, its deploy-scoped recovery marker — in ONE
/// transaction (PR9e).
///
/// This is the engine-stamped write that closes the obligation-vs-marker crash
/// window (PR9d-rev finding 1): the `pending` obligation row and the `in_progress`
/// recovery marker are bracketed in a single `BEGIN … COMMIT`, so either BOTH commit
/// or NEITHER does. Every outstanding obligation therefore ALWAYS has a marker — the
/// auto crash-recovery leg's JOIN can never miss one.
///
/// With `scope = None` this is exactly the routine (`.sql` / non-deploy / resolve /
/// abort) write: a single autocommit obligation INSERT with no marker. The deploy
/// path passes `Some(DeployRecoveryScope { deploy_id })`.
///
/// Both INSERTs are admin-side (same `Client`, same role), under the held project
/// lock and the immutability trigger.
///
/// # Errors
/// [`JournalError::Db`] on insert/commit failure. When a scope is present and any
/// statement fails, the whole transaction is rolled back so there is never an
/// obligation without its marker (nor a marker without its obligation).
#[cfg(feature = "native-pg")]
pub async fn record_pending_contract_with_recovery<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    rec: PendingContractRecord<'_>,
    scope: Option<DeployRecoveryScope<'_>>,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let cv_json = serde_json::to_string(rec.contract_versions).map_err(|e| {
        JournalError::Backend(format!("failed to serialize contract_versions JSON: {e}"))
    })?;
    let obligation_sql = format!(
        "INSERT INTO {meta}.schema_pending_contracts
             (state, \"table\", from_col, to_col, ty, pending_version,
              plan_version, contract_versions, \"by\")
         VALUES ('{pending}', $1, $2, $3, $4, $5, $6, $7, $8)",
        pending = PendingState::Pending.as_str()
    );
    let obligation_params: [crate::apply::backend::postgres::seam::SeamBind; 8] = [
        rec.table.into(),
        rec.from_col.into(),
        rec.to_col.into(),
        rec.ty.into(),
        rec.pending_version.into(),
        rec.plan_version.into(),
        (&cv_json).into(),
        rec.by.into(),
    ];

    // No recovery scope (routine / resolve / abort path) — a single autocommit
    // obligation INSERT, byte-identical to the pre-PR9e behavior.
    let Some(scope) = scope else {
        let n = conn.execute(&obligation_sql, &obligation_params).await?;
        debug_assert_eq!(n, 1, "record_pending_contract must insert exactly one row");
        return Ok(());
    };

    // Deploy path — bracket the obligation row AND its `in_progress` recovery marker
    // in ONE transaction so they commit atomically (PR9e: no obligation-without-marker
    // window). Roll the whole thing back on any failure.
    conn.batch_execute("BEGIN").await?;
    let result = async {
        let n = conn.execute(&obligation_sql, &obligation_params).await?;
        debug_assert_eq!(n, 1, "record_pending_contract must insert exactly one row");
        let m = conn
            .execute(
                &format!(
                    "INSERT INTO {meta}.schema_deploy_recovery
                         (deploy_id, pending_version, state, \"by\")
                     VALUES ($1, $2, 'in_progress', $3)"
                ),
                &[scope.deploy_id.into(), rec.pending_version.into(), rec.by.into()],
            )
            .await?;
        debug_assert_eq!(m, 1, "the in_progress recovery marker must insert exactly one row");
        Ok::<(), JournalError>(())
    }
    .await;
    if let Err(e) = result {
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(
                error = %rb,
                "zeroship-migrate: ROLLBACK failed after a \
                 record_pending_contract_with_recovery error (PR9e)"
            );
        }
        return Err(e);
    }
    conn.batch_execute("COMMIT").await?;
    Ok(())
}

/// Discharge a cross-deploy pending-contract obligation: APPEND a
/// `state='resolved'` row (§2.0.3 item 1 / the `resolve-pending` CLI). History is
/// append-only — the `pending` row is never edited or deleted.
///
/// The obligation's identity fields (`table`/`from_col`/`to_col`/`ty`/
/// `contract_versions`) are carried forward onto the `resolved` row from the
/// supplied descriptor so the row is self-describing for forensics (the shape
/// CHECK only requires `resolution IS NOT NULL`).
///
/// Admin-run; the migrator cannot reach this table.
///
/// # Errors
/// [`JournalError::Db`] on insert failure.
#[cfg(feature = "native-pg")]
pub async fn resolve_pending_contract<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    pc: &PendingContract,
    resolution: Resolution,
    by: &str,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let cv_json = serde_json::to_string(&pc.contract_versions).map_err(|e| {
        JournalError::Backend(format!("failed to serialize contract_versions JSON: {e}"))
    })?;
    let n = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_pending_contracts
                     (state, \"table\", from_col, to_col, ty, pending_version,
                      plan_version, contract_versions, resolution, \"by\")
                 VALUES ('{resolved}', $1, $2, $3, $4, $5, $6, $7, $8, $9)",
                resolved = PendingState::Resolved.as_str()
            ),
            &[
                (&pc.table).into(),
                (&pc.from_col).into(),
                (&pc.to_col).into(),
                (&pc.ty).into(),
                (&pc.pending_version).into(),
                (&pc.plan_version).into(),
                (&cv_json).into(),
                resolution.as_str().into(),
                by.into(),
            ],
        )
        .await?;
    debug_assert_eq!(n, 1, "resolve_pending_contract must insert exactly one row");
    Ok(())
}

// -- deploy-scoped recovery markers (PR9d MED) ------------------------------

/// An OUTSTANDING deploy-scoped recovery obligation: a same-deploy EXPAND whose
/// recovery row is net-`in_progress` (never promoted to `committed`, never closed
/// `reconciled`). Carried back to the control loop so it can drive the same-deploy
/// abort over exactly these `pending_version`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployRecovery {
    /// The per-deploy id (UUIDv7) the EXPAND was opened under.
    pub deploy_id: String,
    /// The obligation key (the EXPAND's E2 trigger version) this row marks for
    /// recovery — the join key into `schema_pending_contracts`.
    pub pending_version: String,
}

/// Promote a deploy-scoped recovery marker to `committed` (PR9e): APPEND a
/// `committed` row keyed on `(deploy_id, pending_version)`.
///
/// This is the crash-vs-legit-pending DISCRIMINATOR. A net-`committed` marker means
/// the deploy that opened the EXPAND reached its success arm — i.e. the online-rename
/// went go-live and the obligation is LEGITIMATELY pending (the §2.0.2 cross-deploy
/// partition), NOT a crashed half-state. A net-`in_progress` marker means the deploy
/// has not durably reached its terminal outcome (a later same-deploy file failed and
/// the process died before the in-process abort, OR the `committed` promotion itself
/// failed) — recoverable. The crash-recovery leg ([`outstanding_deploy_recoveries`])
/// only treats net-`in_progress` markers as recoverable, so it never aborts a
/// `committed` go-live. And because a *failure* to promote leaves the marker in the
/// `in_progress` (recoverable / fail-safe) state — never a protected state it would
/// later be unable to distinguish — there is no window in which a committed go-live
/// is mistaken for recoverable-but-shouldn't-be (PR9e: the inversion that closes the
/// MED false-abort).
///
/// Admin-written, append-only, under the immutability trigger.
///
/// # Errors
/// [`JournalError::Db`] on insert failure.
#[cfg(feature = "native-pg")]
pub async fn mark_deploy_recovery_committed<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    deploy_id: &str,
    pending_version: &str,
    by: &str,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let n = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_deploy_recovery
                     (deploy_id, pending_version, state, \"by\")
                 VALUES ($1, $2, 'committed', $3)"
            ),
            &[deploy_id.into(), pending_version.into(), by.into()],
        )
        .await?;
    debug_assert_eq!(
        n, 1,
        "mark_deploy_recovery_committed must insert exactly one row"
    );
    Ok(())
}

/// Promote a WHOLE deploy's recovery markers to `committed` in ONE transaction
/// (PR9e): a single `BEGIN … COMMIT` brackets every per-obligation `committed`
/// INSERT, so the success-arm promotion is ATOMIC across a multi-EXPAND go-live — it
/// either promotes ALL of this deploy's markers to net-`committed` or none of them
/// (no partial window where one go-live obligation is protected and a sibling stays
/// net-`in_progress`). A genuine commit/connection failure rolls the whole batch
/// back, leaving every marker net-`in_progress` — the *recoverable* (fail-safe)
/// state: the next deploy AUTO-ABORTS the half-rename (safe because a pending
/// contract has not cut over to the shadow column, so no data is lost). This is the
/// PR9e inversion — a promotion failure degrades to "safely re-runnable crash
/// recovery", never "silent revert of a live contract a later deploy mistakes for
/// committed".
///
/// `pending_versions` is this deploy's full obligation key set. An empty slice is a
/// no-op (no transaction opened).
///
/// Admin-written, append-only, under the immutability trigger.
///
/// # Errors
/// [`JournalError::Db`] on any insert/commit failure (the partial batch is rolled
/// back so no marker is left half-promoted).
#[cfg(feature = "native-pg")]
pub async fn mark_deploy_recovery_committed_batch<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    deploy_id: &str,
    pending_versions: &[String],
    by: &str,
) -> Result<(), JournalError> {
    if pending_versions.is_empty() {
        return Ok(());
    }
    conn.batch_execute("BEGIN").await?;
    let result = async {
        for pv in pending_versions {
            mark_deploy_recovery_committed(conn, cfg, deploy_id, pv, by).await?;
        }
        Ok::<(), JournalError>(())
    }
    .await;
    if let Err(e) = result {
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(
                error = %rb,
                "zeroship-migrate: ROLLBACK failed after a \
                 mark_deploy_recovery_committed_batch error (PR9e)"
            );
        }
        return Err(e);
    }
    conn.batch_execute("COMMIT").await?;
    Ok(())
}

/// Mark a deploy-scoped recovery obligation `reconciled` (PR9d MED): APPEND a
/// `reconciled` row (append-only — the `in_progress` row is never edited). Called
/// after a same-deploy abort or a crash-recovery abort to CLOSE the marker (the
/// EXPAND was rolled back). Admin-written.
///
/// # Errors
/// [`JournalError::Db`] on insert failure.
#[cfg(feature = "native-pg")]
pub async fn mark_deploy_recovery_reconciled<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    deploy_id: &str,
    pending_version: &str,
    by: &str,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let n = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_deploy_recovery
                     (deploy_id, pending_version, state, \"by\")
                 VALUES ($1, $2, 'reconciled', $3)"
            ),
            &[deploy_id.into(), pending_version.into(), by.into()],
        )
        .await?;
    debug_assert_eq!(
        n, 1,
        "mark_deploy_recovery_reconciled must insert exactly one row"
    );
    Ok(())
}

/// Read the deploy-recovery markers that are net-`in_progress` (latest event per
/// `(deploy_id, pending_version)` is `in_progress`) AND whose obligation is STILL
/// outstanding in `schema_pending_contracts` (PR9d MED / PR9e — the crash-recovery
/// leg's resume input).
///
/// The outstanding-obligation join is what makes resume idempotent + correct: a
/// recovery row whose obligation was already discharged (aborted by an earlier
/// resume attempt, or applied by a go-live) is NOT re-driven. Ordered by
/// `(deploy_id, pending_version)` for determinism.
///
/// **The crash-vs-legit discriminator (PR9e — the inversion).** The
/// `r.state = 'in_progress'` predicate is load-bearing: the marker is BORN
/// `in_progress` atomically with the obligation
/// ([`record_pending_contract_with_recovery`]), and a deploy that reaches its SUCCESS
/// arm PROMOTES it to `committed` ([`mark_deploy_recovery_committed_batch`]). A
/// net-`committed` marker is therefore EXCLUDED here — a fully-reached go-live is
/// never aborted. Conversely a net-`in_progress` marker means the deploy did NOT
/// durably reach a terminal outcome: either it genuinely crashed, OR the `committed`
/// promotion itself failed. BOTH are correctly recoverable — aborting the half-rename
/// is SAFE because a *pending* contract has not cut over reads/writes to the shadow
/// column (the dual-write trigger keeps both in sync; the drop-old-column contract
/// has not run), so no data is lost. This is the PR9e inversion: a promotion failure
/// leaves the marker in the *recoverable* state, never a *protected* state a later
/// deploy would silently revert (the pre-PR9e MED false-abort vector).
///
/// # Errors
/// [`JournalError::Db`] on query failure.
#[cfg(feature = "native-pg")]
pub async fn outstanding_deploy_recoveries<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<DeployRecovery>, JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "WITH latest_recovery AS (
                     SELECT DISTINCT ON (deploy_id, pending_version)
                            deploy_id, pending_version, state
                       FROM {meta}.schema_deploy_recovery
                      ORDER BY deploy_id, pending_version, event_seq DESC
                 ),
                 latest_obligation AS (
                     SELECT DISTINCT ON (pending_version) pending_version, state
                       FROM {meta}.schema_pending_contracts
                      ORDER BY pending_version, event_seq DESC
                 )
                 SELECT r.deploy_id, r.pending_version
                   FROM latest_recovery r
                   JOIN latest_obligation o ON o.pending_version = r.pending_version
                  WHERE r.state = 'in_progress' AND o.state = '{pending}'
                  ORDER BY r.deploy_id, r.pending_version",
                pending = PendingState::Pending.as_str()
            ),
            &[],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| DeployRecovery {
            deploy_id: row.get("deploy_id"),
            pending_version: row.get("pending_version"),
        })
        .collect())
}

/// Count the number of versions the journal currently records as **net-applied**
/// (latest event per version is `completed`) — the first-entry test for
/// [`crate::apply::baseline`].
///
/// Baseline is a FIRST-entry operation: you cannot baseline a DB the engine
/// already manages. This counts net-applied versions exactly as [`applied`]
/// computes them (latest event per version is `completed`), so a version that was
/// applied then rolled back does NOT count (it is pending again). A non-zero count
/// means the engine already manages real history ⇒ baseline must refuse.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
#[cfg(feature = "native-pg")]
pub async fn applied_count<D: PgSession>(conn: &D, cfg: &ExecutorConfig) -> Result<i64, JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let row = conn
        .query_one(
            &format!(
                "WITH latest AS (
                     SELECT DISTINCT ON (version) version, event_kind
                       FROM {meta}.schema_migrations
                      ORDER BY version, event_seq DESC
                 )
                 SELECT count(*)::bigint AS n FROM latest WHERE event_kind = '{applied}'",
                applied = EventKind::Applied.as_str()
            ),
            &[],
        )
        .await?;
    Ok(row.get("n"))
}

/// Read the set of versions **superseded by a net-applied squash** (Plan 9).
///
/// A version `v_i` is satisfied-by-supersession when some squash `S` with an edge
/// `S → v_i` in `schema_migrations_supersedes` is itself **net-applied** (its
/// latest event in the consolidated `schema_migrations` table has
/// `event_kind='applied'`) AND `S`'s recorded `kind` is `'squash'`. The executor
/// unions this with the net-applied set
/// to compute `pending`, so a superseded `v_i` is never (re-)run.
///
/// Only edges of a NET-APPLIED squash count: if `S` was rolled back, its
/// supersession no longer holds and the superseded versions become pending again
/// (consistent with `S` itself being pending again).
///
/// #4 — the `kind = 'squash'` restriction is load-bearing: without it, any
/// net-applied version whose `version` collided with a corrupted/forged edge's
/// `squash_version` could over-supersede (suppress a real migration). Only a
/// genuine recorded squash may supersede.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
#[cfg(feature = "native-pg")]
pub async fn superseded_versions<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<String>, JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "WITH latest AS (
                     SELECT DISTINCT ON (version) version, event_kind, kind AS mig_kind
                       FROM {meta}.schema_migrations
                      ORDER BY version, event_seq DESC
                 ),
                 net_applied_squashes AS (
                     -- #4: only a GENUINE recorded squash can supersede. Without the
                     -- mig_kind = 'squash' filter, any net-applied version whose
                     -- `version` collided with a (corrupted/forged) edge's
                     -- `squash_version` would over-supersede — suppressing a real
                     -- migration (the C1 forgery class).
                     SELECT version FROM latest
                      WHERE event_kind = '{applied}' AND mig_kind = 'squash'
                 )
                 SELECT DISTINCT s.superseded_version AS v
                   FROM {meta}.schema_migrations_supersedes s
                   JOIN net_applied_squashes n ON n.version = s.squash_version
                  ORDER BY 1",
                applied = EventKind::Applied.as_str()
            ),
            &[],
        )
        .await?;
    Ok(rows.iter().map(|r| r.get::<_, String>("v")).collect())
}

/// Read the **latest `completed` checksum per version** from the journal of
/// record (v3 Plan E — repeatables).
///
/// A repeatable migration ([`MigrationFlags::repeatable`](crate::model::migration::MigrationFlags::repeatable))
/// has a STABLE identity (its `version`/name never changes across edits) and is
/// re-applied whenever its definition checksum changes. Each re-apply appends a
/// fresh `completed` event for the same version (append-only), so a repeatable
/// accrues several `completed` rows over its lifetime. To decide whether to
/// re-run, the executor compares the migration's current checksum against the
/// **most recent** `completed` event's checksum for that identity.
///
/// Returns a map `version → latest completed checksum`, taking the latest by the
/// native monotonic `event_seq` IDENTITY (which never ties, even within one
/// transaction). Versions with no `applied` row are absent from the map (never
/// applied).
///
/// Unlike [`applied`], this filters to `event_kind='applied'` (the forward-event
/// rows) and is INDIFFERENT to rollback: a repeatable carries `down: None` and is
/// never rolled back, so its latest event is always its newest `applied` one.
/// (`rolled_back` rows carry `kind=NULL`, so they are excluded by the
/// `kind='repeatable'` filter regardless — but the explicit `event_kind='applied'`
/// keeps the intent legible.) The drift/pending machinery still uses [`applied`]
/// for versioned migrations; this is the repeatable-specific lens.
///
/// **Kind-aware (v3 Plan E re-critic #2).** Only events whose journaled
/// `kind='repeatable'` are consulted — the re-run oracle must never read a
/// once-only `kind='apply'` (or baseline/squash) row's checksum as a repeatable's
/// "prior" value. Combined with the [`applied`]-driven kind-mismatch abort in the
/// drift check, this keeps the repeatable re-run path strictly about genuine
/// repeatable history: a version that was applied once-only (and would only reach
/// this lookup via the tamper flip, which the drift check already aborts) has no
/// `repeatable`-kind event, so it is absent from the map — never silently re-run.
///
/// # Errors
/// [`JournalError::Db`] on query failure.
#[cfg(feature = "native-pg")]
pub async fn latest_completed_checksums<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<std::collections::HashMap<String, String>, JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "SELECT DISTINCT ON (version) version, checksum
                   FROM {meta}.schema_migrations
                  WHERE event_kind = '{applied}' AND kind = 'repeatable'
                  ORDER BY version, event_seq DESC",
                applied = EventKind::Applied.as_str()
            ),
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<_, String>("version"), r.get::<_, String>("checksum")))
        .collect())
}

/// The fields of a baseline/squash `completed` event recorded WITHOUT running its
/// `up` (Plan 9), bundled so [`record_baseline`] takes one descriptor.
#[derive(Debug, Clone, Copy)]
pub struct BaselineRecord<'a> {
    /// The migration version (`mig_…`).
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

/// Journal a baseline/squash `completed` event WITHOUT running its `up` (Plan 9).
///
/// The event carries an explicit `kind` (`'baseline'` or `'squash'`) and an
/// optional supersession edge set. Run by the ADMIN (the migrator has no
/// meta-schema grant), exactly like [`record_completed`]. `exec_ms` is recorded as
/// 0 (no SQL ran).
///
/// This is the journal-without-running primitive shared by [`crate::apply::baseline`]
/// (the adoption path: the schema already physically exists, so the `up` is
/// recorded not run) and [`crate::ops::squash`]'s existing-DB path (a supersession: the
/// effect of `[v1..vN]` is already present, so the squash's `up` is recorded not
/// run). #3 fix: the `completed` row + every supersession edge are inserted in ONE
/// transaction THIS function brackets (`BEGIN … COMMIT`, ROLLBACK on any error), so
/// a net-applied squash always carries its full edge set (no partial-edge window) —
/// a crash between the row and the edges can no longer leave `S` net-applied with
/// partial/empty edges (the advisory lock the callers hold gives mutual exclusion,
/// NOT atomicity). Append-only: never an UPDATE/DELETE.
///
/// # Errors
/// [`JournalError::Db`] on insert failure (the partial work is rolled back).
#[cfg(feature = "native-pg")]
pub async fn record_baseline<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    rec: BaselineRecord<'_>,
) -> Result<(), JournalError> {
    conn.batch_execute("BEGIN").await?;
    let result = record_baseline_inner(conn, cfg, rec).await;
    if let Err(e) = result {
        // Roll back the partial row/edges; surface the original error.
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %rec.version, "zeroship-migrate: ROLLBACK failed after a record_baseline error (#3)");
        }
        return Err(e);
    }
    conn.batch_execute("COMMIT").await?;
    Ok(())
}

/// The row + edge INSERTs of [`record_baseline`], run INSIDE its `BEGIN … COMMIT`
/// (#3). Split out so the caller can ROLLBACK on the first failure, making the
/// completed row and its full edge set atomic.
#[cfg(feature = "native-pg")]
async fn record_baseline_inner<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    rec: BaselineRecord<'_>,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let n = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations
                     (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind)
                 VALUES ('{applied}', $1, $2, $3, $4, 0, 'completed', 'success', $5)",
                applied = EventKind::Applied.as_str()
            ),
            &[
                rec.version.into(),
                rec.name.into(),
                rec.checksum.into(),
                rec.applied_by.into(),
                rec.kind.into(),
            ],
        )
        .await?;
    debug_assert_eq!(n, 1, "record_baseline must insert exactly one journal row");
    for sup in rec.supersedes {
        conn.execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations_supersedes
                     (squash_version, superseded_version)
                 VALUES ($1, $2)"
            ),
            &[rec.version.into(), sup.into()],
        )
        .await?;
    }
    Ok(())
}

/// Drop a stale inflight marker (recovery path cleanup) without recording a
/// completed row — used when recovery decides the partial work was undone.
///
/// # Errors
/// [`JournalError::Db`] on failure.
#[cfg(feature = "native-pg")]
pub async fn clear_inflight<D: PgSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    conn.execute(
        &format!("DELETE FROM {meta}.schema_migrations_inflight WHERE version = $1"),
        &[version.into()],
    )
    .await?;
    Ok(())
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
    /// dedicated `BadEventKind` arm — NOT `BadPhase` (the pre-fix misuse) — so a
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
    /// literals from this single typed source (§2.0.3). A drift here would
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
