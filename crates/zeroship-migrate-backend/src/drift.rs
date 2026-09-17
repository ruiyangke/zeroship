//! The drift-report vocabulary every backend's drift query returns.
//!
//! Two independent drift surfaces share this module, matching the two questions a
//! deploy asks of a live database:
//!
//! - **checksum / tamper / orphan drift** - does the journal still agree with the
//!   migration set it claims to have applied? ([`ChecksumDrift`],
//!   [`OrphanJournal`], [`ChecksumDriftReport`]);
//! - **structural drift** - does the live catalog still match the shape the
//!   applied migrations describe? ([`AlteredObject`], [`StructuralDrift`]).
//!
//! [`DriftReport`] is the aggregate the caller assembles from both, and
//! [`DriftError`] is the shared refusal.
//!
//! Names only, never DDL. These types SURFACE a divergence; deciding what to do
//! about one is the control plane's job.
//!
//! One comparison ALGORITHM lives here too: [`compare_applied_to_set`], the
//! checksum/tamper/orphan verdict. It is here rather than in the engine because
//! every backend runs it over its OWN journal read - the read is dialect-coupled,
//! the rules over it must not be - so a home above the vendors is a home the
//! vendors cannot reach. The STRUCTURAL comparisons (`diff_snapshots` and the
//! per-vendor catalog normalizations) stay in the engine: they resolve a vendor's
//! value-format renderer from a `DialectId`, which is the engine's question.

use std::collections::BTreeMap;

use crate::executor::BackendError;
use crate::journal::{AppliedEntry, JournalError, Phase};
use crate::snapshot::PartitionSnapshot;
use zeroship_migrate_ir::migration::Migration;

/// A net-applied version whose journal checksum no longer matches the supplied
/// set's checksum for that version - tamper / edited-after-applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumDrift {
    /// The drifting migration's version (`mig_...`).
    pub version: String,
    /// The checksum recorded in the journal (the latest `completed` event).
    pub recorded: String,
    /// The checksum of the migration now in the supplied set.
    pub expected: String,
}

/// A net-applied version with NO corresponding migration in the supplied set -
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

// This conversion belongs beside the error it constructs. Both types are foreign
// to the PostgreSQL `drift_sql` module, so the impl would be an orphan there.
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
/// attribute - an out-of-band `ALTER` that name-only diffing would miss (e.g.
/// `ALTER COLUMN ... TYPE`, `DROP NOT NULL`, an identity/default generator flip,
/// an index losing UNIQUE, a rewritten format CHECK, or an FK repoint/action
/// change). This is the tamper blind spot that name-only diffing leaves open.
///
/// Names only - never DDL. The caller decides what (if anything) to do.
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
/// Names only - never DDL. The caller (control plane) decides what, if anything,
/// to do; this module's job ends at *surfacing*.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructuralDrift {
    /// Objects the EXPECTED snapshot has that the LIVE DB does not - e.g. a table
    /// that should exist but is missing, or a column declared but absent.
    pub missing_objects: Vec<String>,
    /// Objects the LIVE DB has that the EXPECTED snapshot does not - out-of-band
    /// creation (scenario 35): a table/column/index/constraint created by hand,
    /// outside the migration journal.
    pub unexpected_objects: Vec<String>,
    /// Same-name objects (present on BOTH sides) whose ATTRIBUTES diverge - an
    /// out-of-band `ALTER` (type/nullability/identity/default/format/reference/
    /// uniqueness change). The missing/unexpected name buckets cannot see these
    /// because the name still matches; this bucket is the attribute-aware tamper
    /// surface.
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
// DriftReport - the aggregate surface (B1 + B2)
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
    /// Same-name objects whose attributes diverge (out-of-band `ALTER`).
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

// ---------------------------------------------------------------------------
// B1 - checksum / tamper / orphan drift
// ---------------------------------------------------------------------------

/// The **dialect-agnostic** core of [`check_checksum_drift`](crate::backend::MigrationBackend::check_checksum_drift): compare a set of
/// net-applied journal entries (already read by the dialect-coupled `applied`)
/// against the supplied migration set, producing the [`ChecksumDriftReport`].
///
/// Extracted so EVERY [`MigrationBackend`](crate::backend::MigrationBackend) impl
/// shares ONE comparison - the Postgres path and the SQLite path both call this
/// with their own `applied` read, so the repeatable-exemption / kind-mismatch /
/// tamper / orphan rules can never diverge across dialects (design: the
/// comparison is dialect-agnostic; only the journal read underneath differs).
///
/// Pure: no I/O. See [`check_checksum_drift`](crate::backend::MigrationBackend::check_checksum_drift) for the per-rule rationale.
#[must_use]
pub fn compare_applied_to_set(
    applied: &[AppliedEntry],
    migrations: &[Migration],
) -> ChecksumDriftReport {
    let by_version: BTreeMap<&str, &Migration> =
        migrations.iter().map(|m| (m.version.as_str(), m)).collect();

    let mut report = ChecksumDriftReport::default();
    for entry in applied {
        // Only NET-applied (completed) versions can drift / be orphaned; a lone
        // `started` inflight marker is a crash-recovery key, not a settled state.
        if entry.phase != Phase::Completed {
            continue;
        }
        match by_version.get(entry.version.as_str()) {
            Some(m) => {
                // DRIFT EXEMPTION anchored on the JOURNALED
                // kind, NEVER on the attacker-suppliable `m.flags.repeatable`.
                //
                // A repeatable migration's checksum changes by DESIGN (a changed
                // `CREATE OR REPLACE ...` re-runs each deploy), so a checksum mismatch
                // on a GENUINE repeatable is the re-run signal, not tamper. But the
                // ONLY trustworthy evidence that a version IS a repeatable is what the
                // journal recorded when it last applied (`kind='repeatable'`) - the
                // supplied flag is forgeable. So the exemption requires BOTH the
                // journaled kind AND the supplied flag to agree on "repeatable":
                //
                // - journaled `repeatable` AND supplied `repeatable=true` => EXEMPT
                // (the repeatable phase handles its re-apply);
                // - journaled once-only (apply/baseline/squash) but supplied
                // `repeatable=true` => KIND MISMATCH = TAMPER (the flip-flag attack:
                // turning an applied once-only into a repeatable to slip a mutated
                // `up` past the once-only abort) => ChecksumDrift / abort;
                // - journaled `repeatable` but supplied `repeatable=false` => reverse
                // re-classification (also a kind mismatch) => ChecksumDrift / abort;
                // - journaled once-only AND supplied once-only => the ordinary
                // once-only tamper guard (changed checksum still aborts).
                let journaled_repeatable = entry
                    .kind
                    .is_some_and(crate::journal::JournaledKind::is_repeatable);
                let supplied_repeatable = m.flags.repeatable;
                if journaled_repeatable && supplied_repeatable {
                    // Legit repeatable re-run signal - exempt from the tamper abort.
                    continue;
                }
                if journaled_repeatable != supplied_repeatable {
                    // Kind mismatch: the supplied repeatability disagrees with the
                    // journaled identity-class. This is tamper (the flip-flag bypass
                    // or its reverse) - abort with ChecksumDrift regardless of whether
                    // the checksums happen to match, because the RE-CLASSIFICATION
                    // itself is the attack. Reuse ChecksumDrift so `apply` aborts on
                    // the shared gate; recorded vs expected carry the two checksums.
                    report.checksum_drift.push(ChecksumDrift {
                        version: entry.version.clone(),
                        recorded: entry.checksum.clone(),
                        expected: m.checksum.as_str().to_string(),
                    });
                    continue;
                }
                // Both once-only: the ordinary tamper guard.
                if entry.checksum != m.checksum.as_str() {
                    report.checksum_drift.push(ChecksumDrift {
                        version: entry.version.clone(),
                        recorded: entry.checksum.clone(),
                        expected: m.checksum.as_str().to_string(),
                    });
                }
            }
            None => report.orphan_journal.push(OrphanJournal {
                version: entry.version.clone(),
                recorded: entry.checksum.clone(),
            }),
        }
    }
    report
}

/// Compare ONE same-name child partition declared-vs-live, as `(field, expected,
/// actual)` triples in declaration order (`of` before `bounds`).
///
/// Hoisted out of the engine's `diff_snapshots` so the structural differ and the
/// existence-guard partition probe ([`crate::existence_probe::decide`])
/// share ONE definition of "the same partition": a second, drifting copy in the
/// probe is exactly how a guard and a drift report come to disagree about the same
/// catalog.
///
/// `bounds` equality is the derived `PartitionBounds` `PartialEq`, which is already
/// the canonical comparison: `snapshot_schema` parses `pg_get_expr` back into the
/// same enum, so an integer bound round-trips (PostgreSQL prints it unquoted). It
/// does NOT canonicalize literal SPELLING across types: a timestamptz bound
/// authored as `2026-05-01T00:00:00Z` and printed by the catalog as
/// `2026-05-01 00:00:00+00` compares unequal, which the probe reports as drift
/// rather than resolving.
pub fn partition_divergences(
    expected: &PartitionSnapshot,
    actual: &PartitionSnapshot,
) -> Vec<(&'static str, String, String)> {
    let mut out = Vec::new();
    if expected.of != actual.of {
        out.push(("of", expected.of.clone(), actual.of.clone()));
    }
    if expected.bounds != actual.bounds {
        out.push((
            "bounds",
            format!("{:?}", expected.bounds),
            format!("{:?}", actual.bounds),
        ));
    }
    out
}

/// The authored view body, rendered the way the LOWERING that created the view
/// rendered it.
///
/// A view-body drift check needs a body on both sides, and only one side can be
/// read out of a catalog. The other side is a typed
/// [`ViewQuery`](zeroship_migrate_ir::ir::ViewQuery) an author wrote, and printing it is
/// the engine's lowering - not a vendor's. A backend that re-printed it itself would
/// be comparing the DIFFER's idea of the body against the ENGINE's, which is the one
/// comparison a body check must never make.
///
/// So the probe takes the printer instead of resolving one. The engine implements
/// this over its own view-query walk; the backend that drives the server-side
/// re-print consumes it and never learns how a body is spelled.
///
/// `None` means "this body could not be rendered" - the caller DECLINES that view
/// (leaves it uncompared) rather than manufacturing drift for it.
pub trait AuthoredViewBody {
    /// Render `query` as it would have been rendered when the view was created,
    /// with `eff_schema` as the effective schema for unqualified relations.
    fn render(&self, query: &zeroship_migrate_ir::ir::ViewQuery, eff_schema: &str) -> Option<String>;
}
