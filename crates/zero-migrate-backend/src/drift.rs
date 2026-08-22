//! The drift-report vocabulary every backend's drift query returns.
//!
//! Two independent drift surfaces share this module, matching the two questions a
//! deploy asks of a live database:
//!
//! - **checksum / tamper / orphan drift** — does the journal still agree with the
//!   migration set it claims to have applied? ([`ChecksumDrift`],
//!   [`OrphanJournal`], [`ChecksumDriftReport`]);
//! - **structural drift** — does the live catalog still match the shape the
//!   applied migrations describe? ([`AlteredObject`], [`StructuralDrift`]).
//!
//! [`DriftReport`] is the aggregate the caller assembles from both, and
//! [`DriftError`] is the shared refusal.
//!
//! Names only, never DDL. These types SURFACE a divergence; deciding what to do
//! about one is the control plane's job. The comparison ALGORITHMS that produce
//! them (`compare_applied_to_set`, `diff_snapshots`, the per-vendor catalog
//! normalizations) stay in the engine — only the shape a backend hands back lives
//! here.

use crate::executor::BackendError;
use crate::journal::JournalError;

/// A net-applied version whose journal checksum no longer matches the supplied
/// set's checksum for that version — tamper / edited-after-applied (scenario 36).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumDrift {
    /// The drifting migration's version (`mig_…`).
    pub version: String,
    /// The checksum recorded in the journal (the latest `completed` event).
    pub recorded: String,
    /// The checksum of the migration now in the supplied set.
    pub expected: String,
}

/// A net-applied version with NO corresponding migration in the supplied set —
/// the journal knows of a migration the shipped bundle does not (a dropped slice,
/// a downgrade). Surfaced, not silently ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanJournal {
    /// The orphaned version recorded as net-applied in the journal.
    pub version: String,
    /// The journal's recorded checksum for it.
    pub recorded: String,
}

/// Error from a drift query.
#[derive(Debug, thiserror::Error)]
pub enum DriftError {
    /// A database/driver error whose concrete backend type remains
    /// downcastable through [`BackendError`].
    #[error("drift db error: {0}")]
    Db(#[from] BackendError),
    /// A journal read failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// A catalog value could not be represented in the structural snapshot.
    #[error("drift snapshot error: {0}")]
    Snapshot(String),
    /// A **dialect-neutral** backend error whose message is already the intended
    /// operator-facing text. Structured driver failures belong in
    /// [`DriftError::Db`].
    #[error("drift backend error: {0}")]
    Backend(String),
}

// Like the journal's, this conversion lived in the PostgreSQL `drift_sql` module —
// the only place that had needed it. Both types are foreign to that module now, so
// the impl is an orphan there and belongs beside the error it constructs.
impl From<crate::driver::DbError> for DriftError {
    fn from(error: crate::driver::DbError) -> Self {
        Self::Db(error.into())
    }
}

/// The result of `check_checksum_drift`: the per-version checksum mismatches
/// plus the journal versions absent from the supplied set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChecksumDriftReport {
    /// Versions whose journal checksum disagrees with the supplied set.
    pub checksum_drift: Vec<ChecksumDrift>,
    /// Net-applied versions with no migration in the supplied set.
    pub orphan_journal: Vec<OrphanJournal>,
}

impl ChecksumDriftReport {
    /// True if neither tamper nor orphan drift was found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.checksum_drift.is_empty() && self.orphan_journal.is_empty()
    }
}

/// One same-name object whose ATTRIBUTES diverge across the two snapshots.
///
/// A column / index / constraint present on BOTH sides but with a changed
/// attribute — an out-of-band `ALTER` that name-only diffing would miss (e.g.
/// `ALTER COLUMN … TYPE`, `DROP NOT NULL`, an identity/default generator flip,
/// an index losing UNIQUE, a rewritten format CHECK, or an FK repoint/action
/// change). This is the tamper blind spot #1 closes.
///
/// Names only — never DDL. The caller decides what (if anything) to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlteredObject {
    /// The table the object belongs to (e.g. `users`).
    pub table: String,
    /// The object, qualified within the table: a column as `column id`, an index
    /// as `index users_email_idx`, a constraint as `constraint users_age_chk`.
    pub object: String,
    /// The attribute that diverged: `data_type`, `nullable`, `identity`,
    /// `default`, `format`, `unique`, `columns`, `access_method`, `expression`,
    /// `kind`, or `definition`.
    pub field: String,
    /// The expected snapshot's value for `field`.
    pub expected: String,
    /// The live DB's value for `field`.
    pub actual: String,
}

/// A structural-drift report (the pure `diff_snapshots` output).
///
/// Names only — never DDL. The caller (control plane) decides what, if anything,
/// to do; this module's job ends at *surfacing*.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructuralDrift {
    /// Objects the EXPECTED snapshot has that the LIVE DB does not — e.g. a table
    /// that should exist but is missing, or a column declared but absent.
    pub missing_objects: Vec<String>,
    /// Objects the LIVE DB has that the EXPECTED snapshot does not — out-of-band
    /// creation (scenario 35): a table/column/index/constraint created by hand,
    /// outside the migration journal.
    pub unexpected_objects: Vec<String>,
    /// Same-name objects (present on BOTH sides) whose ATTRIBUTES diverge — an
    /// out-of-band `ALTER` (type/nullability/identity/default/format/reference/
    /// uniqueness change). The missing/unexpected name buckets cannot see these
    /// because the name still matches; this bucket is the attribute-aware tamper
    /// surface (#1).
    pub altered_objects: Vec<AlteredObject>,
}

impl StructuralDrift {
    /// True if the live schema matches the expected snapshot exactly.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.missing_objects.is_empty()
            && self.unexpected_objects.is_empty()
            && self.altered_objects.is_empty()
    }
}

// ---------------------------------------------------------------------------
// DriftReport — the aggregate surface (B1 + B2)
// ---------------------------------------------------------------------------

/// The full drift surface for a project: checksum/tamper drift, orphan journal
/// entries, and (when a structural diff is run) missing / unexpected objects.
///
/// Assembled by the caller from `check_checksum_drift` and
/// `diff_snapshots`; it carries reports only, never DDL or a remediation plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriftReport {
    /// Net-applied versions whose journal checksum disagrees with the set.
    pub checksum_drift: Vec<ChecksumDrift>,
    /// Expected objects absent from the live DB (from a structural diff).
    pub missing_objects: Vec<String>,
    /// Live objects absent from the expected snapshot (out-of-band creation).
    pub unexpected_objects: Vec<String>,
    /// Same-name objects whose attributes diverge (out-of-band `ALTER` — #1).
    pub altered_objects: Vec<AlteredObject>,
    /// Net-applied versions with no migration in the supplied set.
    pub orphan_journal: Vec<OrphanJournal>,
}

impl DriftReport {
    /// Assemble from a checksum-drift report and a structural diff.
    #[must_use]
    pub fn new(checksum: ChecksumDriftReport, structural: StructuralDrift) -> Self {
        Self {
            checksum_drift: checksum.checksum_drift,
            missing_objects: structural.missing_objects,
            unexpected_objects: structural.unexpected_objects,
            altered_objects: structural.altered_objects,
            orphan_journal: checksum.orphan_journal,
        }
    }

    /// True if no drift of any kind was found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.checksum_drift.is_empty()
            && self.missing_objects.is_empty()
            && self.unexpected_objects.is_empty()
            && self.altered_objects.is_empty()
            && self.orphan_journal.is_empty()
    }
}
