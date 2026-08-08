//! Drift detection — **read-only**.
//!
//! Drift is any divergence between what the journal says happened and either
//! (a) the migration set the operator now ships, or (b) the live database
//! schema. This module **surfaces** drift; it NEVER emits DDL and NEVER mutates
//! anything. The whole module runs as
//! the admin/read connection over `information_schema`/`pg_catalog`, never as
//! the privileged `migrator` role, and binds every identifier so an injected
//! schema/table name cannot break the introspection queries.
//!
//! Two independent axes:
//!
//! - **B1 — checksum / tamper / orphan drift** ([`check_checksum_drift`]):
//! compares the journal's recorded checksum for each NET-applied version
//! against the checksum of the same version in the supplied set. A mismatch
//! means the migration SQL was edited after it applied, or the journal row was
//! tampered. A net-applied version with NO matching
//! migration in the supplied set is an **orphan** ([`OrphanJournal`]) — the
//! bundle is missing a migration the database already has. This is the exact
//! comparison the executor's apply flow does as its abort-on-drift pre-check;
//! [`apply`](crate::apply()) calls this function and aborts
//! if it returns any [`ChecksumDrift`], so the report and the gate share one
//! implementation.
//!
//! - **B2 — structural introspection** ([`snapshot_schema`] + [`diff_snapshots`]):
//! introspect the LIVE project schema into a deterministic [`SchemaSnapshot`]
//! and `diff` it against an **expected** snapshot the CALLER supplies. The
//! expected snapshot is owned by the control-plane / authoring layer (it holds
//! the declared/union schema, design); this module does NOT rebuild a schema
//! model by replaying DDL — that is the authoring layer's job. `diff_snapshots`
//! is a pure function returning a [`StructuralDrift`] report; it never returns
//! DDL.

use std::collections::BTreeMap;

#[cfg(pg_seam)]
use crate::driver::SqlSession;

use crate::apply::executor::BackendError;
use crate::apply::journal::{self, AppliedEntry, JournalError, Phase};
use crate::conn::ExecutorConfig;
use crate::model::ir::{
    IdentityCol, IndexSortOrder, IndexStorageParams, PartitionBoundValue, PartitionBounds,
    PartitionSpec, SafeI64, SafeU64, SequenceOwnedBy, SequenceRef,
};
use crate::model::migration::Migration;
use crate::model::snapshot::{
    canonical_index_sort_order, index_elements_canonically_eq, index_predicates_canonically_eq,
    normalize_sequence_max_value, normalize_sequence_min_value, ColumnCollationSnapshot,
    ColumnSnapshot, ConstraintSnapshot, ExtensionSnapshot, IdDefaultSnapshot, IndexElementSnapshot,
    IndexSnapshot, NamedTypeSnapshot, PartitionSnapshot, RoleSnapshot, SchemaObjectSnapshot,
    SchemaSnapshot, SequenceDataTypeSnapshot, SequenceSnapshot, TableSnapshot, ViewSnapshot,
};
use crate::render::value_format::{
    catalog_expression_fingerprint_in_dialect, catalog_id_default, catalog_id_default_for_expected,
    catalog_text_id_default, catalog_uuid_id_default, recover_format_check, RecoveredFormatCheck,
};
use crate::schema::query::SqlDialect;

// ---------------------------------------------------------------------------
// B1 — checksum / tamper / orphan drift
// ---------------------------------------------------------------------------

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

#[cfg(pg_seam)]
impl From<crate::driver::DbError> for DriftError {
    fn from(error: crate::driver::DbError) -> Self {
        Self::Db(error.into())
    }
}

/// The result of [`check_checksum_drift`]: the per-version checksum mismatches
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

/// Compare the journal's NET-applied checksums against the supplied migration
/// set.
///
/// For each net-applied version (the latest event is `completed`, per
/// [`journal::applied`]):
///
/// - the supplied set has a migration with that version whose checksum differs
/// ⇒ [`ChecksumDrift`] (the migration SQL was mutated after apply, or the
/// journal row was tampered — scenario 36);
/// - the supplied set has NO migration with that version ⇒ [`OrphanJournal`].
///
/// The recorded checksum used is the one [`journal::applied`] returns, which is
/// the **latest `completed` event's** checksum for the version — correct across
/// rollback↔re-apply cycles (a re-applied migration's checksum is its newest
/// incarnation, not a stale earlier one).
///
/// This is the canonical comparison; [`apply`](crate::apply()) calls it as its
/// abort-on-drift pre-check (it aborts if [`checksum_drift`](ChecksumDriftReport::checksum_drift)
/// is non-empty), so the report and the apply gate cannot diverge.
///
/// **Read-only.** No mutation, no DDL.
///
/// # Errors
/// [`DriftError::Journal`] if the journal read fails.
#[cfg(pg_seam)]
pub async fn check_checksum_drift<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<ChecksumDriftReport, DriftError> {
    let applied = journal::applied(conn, cfg).await?;
    Ok(compare_applied_to_set(&applied, migrations))
}

/// The **dialect-agnostic** core of [`check_checksum_drift`]: compare a set of
/// net-applied journal entries (already read by the dialect-coupled `applied`)
/// against the supplied migration set, producing the [`ChecksumDriftReport`].
///
/// Extracted so EVERY [`MigrationBackend`](crate::apply::backend::MigrationBackend) impl
/// shares ONE comparison — the Postgres path and the SQLite path both call this
/// with their own `applied` read, so the repeatable-exemption / kind-mismatch /
/// tamper / orphan rules can never diverge across dialects (design: the
/// comparison is dialect-agnostic; only the journal read underneath differs).
///
/// Pure: no I/O. See [`check_checksum_drift`] for the per-rule rationale.
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
                // `CREATE OR REPLACE …` re-runs each deploy), so a checksum mismatch
                // on a GENUINE repeatable is the re-run signal, not tamper. But the
                // ONLY trustworthy evidence that a version IS a repeatable is what the
                // journal recorded when it last applied (`kind='repeatable'`) — the
                // supplied flag is forgeable. So the exemption requires BOTH the
                // journaled kind AND the supplied flag to agree on "repeatable":
                //
                // - journaled `repeatable` AND supplied `repeatable=true` ⇒ EXEMPT
                // (the repeatable phase handles its re-apply);
                // - journaled once-only (apply/baseline/squash) but supplied
                // `repeatable=true` ⇒ KIND MISMATCH = TAMPER (the flip-flag attack:
                // turning an applied once-only into a repeatable to slip a mutated
                // `up` past the once-only abort) ⇒ ChecksumDrift / abort;
                // - journaled `repeatable` but supplied `repeatable=false` ⇒ reverse
                // re-classification (also a kind mismatch) ⇒ ChecksumDrift / abort;
                // - journaled once-only AND supplied once-only ⇒ the ordinary
                // once-only tamper guard (changed checksum still aborts).
                let journaled_repeatable = entry
                    .kind
                    .is_some_and(crate::apply::journal::JournaledKind::is_repeatable);
                let supplied_repeatable = m.flags.repeatable;
                if journaled_repeatable && supplied_repeatable {
                    // Legit repeatable re-run signal — exempt from the tamper abort.
                    continue;
                }
                if journaled_repeatable != supplied_repeatable {
                    // Kind mismatch: the supplied repeatability disagrees with the
                    // journaled identity-class. This is tamper (the flip-flag bypass
                    // or its reverse) — abort with ChecksumDrift regardless of whether
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

// ---------------------------------------------------------------------------
// B2 — structural introspection + pure diff
// ---------------------------------------------------------------------------

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

/// A structural-drift report (the pure [`diff_snapshots`] output).
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

/// Introspect the LIVE structure of `project_schema` into a [`SchemaSnapshot`]
/// ///
/// **Read-only.** Hits `information_schema` / `pg_catalog` only; emits no DDL,
/// mutates nothing. Run as the admin/read connection (NOT the `migrator` role).
///
/// **Injection-safe.** `project_schema` is passed as a **bind parameter** to
/// every catalog query — never interpolated into SQL text — so a schema name
/// containing a quote, a semicolon, or any SQL metacharacter selects zero rows
/// rather than altering the query.
///
/// Identity and ID-default semantics are recovered from structured catalog
/// metadata (including a `pg_depend` lookup for search-path-stable nextval
/// identity). Engine-owned TypeID/ULID CHECKs project onto their columns, and
/// foreign keys are rebuilt from ordered catalog tuples, target identity,
/// actions, match mode, deferrability, and validation state. This avoids relying
/// on PostgreSQL's search-path-sensitive FK/nextval deparser spelling.
///
/// Determinism: the result map is a `BTreeMap` and every column/index/constraint
/// vector is sorted by name, so the snapshot is stable across catalog scan order.
///
/// # Preconditions
/// The caller MUST pass an **admin/read** connection — this function takes
/// whatever [`SqlSession`] it is handed and never elevates to the `migrator` role.
/// Binding `project_schema` by `$1` prevents cross-schema leakage regardless of
/// the connection, but choosing a least-privileged read connection is the
/// caller's obligation.
///
/// # Errors
/// [`DriftError::Db`] on a catalog query failure.
/// Normalise `pg_catalog.format_type(...)` output for an extension type back to
/// the engine's canonical DDL spelling, so a live `USER-DEFINED` column compares
/// equal to the desired snapshot the author built.
///
/// The engine emits (and the desired snapshot stores) `geography(POINT, 4326)`
/// for a geoPoint and `vector(N)` for a vector. Postgres canonicalises the stored
/// type, so `format_type` reports `geography(Point,4326)` and `vector(N)`. This
/// maps PG's canonical form back to the engine's spelling for the two extension
/// types the engine emits; any other `format_type` output is returned verbatim
/// (the closest faithful spelling we have for an unknown extension type).
fn canonical_extension_type(format_type: &str) -> String {
    let trimmed = format_type.trim();
    let lower = trimmed.to_ascii_lowercase();
    if is_citext_extension_type(trimmed) {
        return "text".to_string();
    }
    // PostGIS geography point: `geography(Point,4326)` → `geography(POINT, 4326)`
    // (the engine's `field_to_column` / `dsl_to_pg_data_type` spelling). Match on
    // the lowercased form so we are robust to PG capitalisation changes, and
    // re-emit the exact engine spelling rather than echoing PG's.
    if lower == "geography(point,4326)" {
        return "geography(POINT, 4326)".to_string();
    }
    // pgvector: `vector(N)` already matches the engine's spelling byte-for-byte.
    // Return `format_type`'s output verbatim for vector (and any other extension
    // type) — it is the precise live spelling.
    trimmed.to_string()
}

fn is_citext_extension_type(format_type: &str) -> bool {
    format_type
        .trim()
        .split('.')
        .next_back()
        .map(|tail| tail.trim_matches('"').eq_ignore_ascii_case("citext"))
        .unwrap_or(false)
}

fn split_column_catalog_comment(comment: Option<String>) -> (Option<String>, Option<String>) {
    match comment {
        Some(comment) if is_internal_column_comment_sentinel(&comment) => (None, Some(comment)),
        other => (other, None),
    }
}

fn is_internal_column_comment_sentinel(comment: &str) -> bool {
    comment.starts_with("zero-migrate:mask:") || comment.starts_with("zero-migrate:enc:")
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let trimmed = s.trim_start();
    trimmed
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &trimmed[prefix.len()..])
}

fn take_parenthesized(s: &str) -> Option<(&str, &str)> {
    let trimmed = s.trim_start();
    if !trimmed.starts_with('(') {
        return None;
    }

    let mut chars = trimmed.char_indices().peekable();
    let (_, first) = chars.next()?;
    debug_assert_eq!(first, '(');

    let mut depth = 1_u32;
    let mut in_quote = false;
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '\'' if in_quote => {
                if matches!(chars.peek(), Some((_, '\''))) {
                    chars.next();
                } else {
                    in_quote = false;
                }
            }
            '\'' => in_quote = true,
            '(' if !in_quote => depth += 1,
            ')' if !in_quote => {
                depth -= 1;
                if depth == 0 {
                    return Some((&trimmed[1..idx], &trimmed[idx + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

fn split_pg_value_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0_usize;
    let mut in_quote = false;
    let mut chars = s.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '\'' if in_quote => {
                if matches!(chars.peek(), Some((_, '\''))) {
                    chars.next();
                } else {
                    in_quote = false;
                }
            }
            '\'' => in_quote = true,
            ',' if !in_quote => {
                let value = s[start..idx].trim();
                if !value.is_empty() {
                    out.push(value.to_string());
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    let value = s[start..].trim();
    if !value.is_empty() {
        out.push(value.to_string());
    }
    out
}

fn parse_pg_quoted_string(s: &str) -> Option<(String, &str)> {
    let trimmed = s.trim_start();
    if !trimmed.starts_with('\'') {
        return None;
    }

    let mut value = String::new();
    let mut chars = trimmed.char_indices().peekable();
    chars.next();
    while let Some((idx, ch)) = chars.next() {
        if ch == '\'' {
            if matches!(chars.peek(), Some((_, '\''))) {
                chars.next();
                value.push('\'');
            } else {
                return Some((value, &trimmed[idx + 1..]));
            }
        } else {
            value.push(ch);
        }
    }
    None
}

fn parse_partition_bound_value_pg(raw: &str) -> Result<PartitionBoundValue, String> {
    let value = raw.trim();
    if value.eq_ignore_ascii_case("MINVALUE") {
        return Ok(PartitionBoundValue::MinValue);
    }
    if value.eq_ignore_ascii_case("MAXVALUE") {
        return Ok(PartitionBoundValue::MaxValue);
    }
    if let Some((quoted, trailing)) = parse_pg_quoted_string(value) {
        let trailing = trailing.trim();
        if trailing.is_empty() || trailing.starts_with("::") {
            return Ok(PartitionBoundValue::String { value: quoted });
        }
        return Err(format!("unsupported partition bound literal `{raw}`"));
    }
    if let Ok(n) = value.parse::<i64>() {
        return Ok(PartitionBoundValue::Int {
            value: SafeI64::new(n)?,
        });
    }
    Ok(PartitionBoundValue::String {
        value: value.to_string(),
    })
}

fn parse_partition_bound_values_pg(raw: &str) -> Result<Vec<PartitionBoundValue>, String> {
    split_pg_value_list(raw)
        .iter()
        .map(|value| parse_partition_bound_value_pg(value))
        .collect()
}

fn parse_partition_bounds_pg(raw: &str) -> Result<PartitionBounds, String> {
    let s = raw.trim();
    if s.eq_ignore_ascii_case("DEFAULT") {
        return Ok(PartitionBounds::Default);
    }

    let rest = strip_prefix_ci(s, "FOR VALUES")
        .ok_or_else(|| format!("unsupported partition bounds `{raw}`"))?;
    if let Some(after_from) = strip_prefix_ci(rest, "FROM") {
        let (from_raw, after_from_values) = take_parenthesized(after_from)
            .ok_or_else(|| format!("invalid RANGE partition lower bound `{raw}`"))?;
        let after_to = strip_prefix_ci(after_from_values, "TO")
            .ok_or_else(|| format!("missing RANGE partition upper bound `{raw}`"))?;
        let (to_raw, trailing) = take_parenthesized(after_to)
            .ok_or_else(|| format!("invalid RANGE partition upper bound `{raw}`"))?;
        if !trailing.trim().is_empty() {
            return Err(format!("unsupported trailing RANGE partition text `{raw}`"));
        }
        return Ok(PartitionBounds::Range {
            from: parse_partition_bound_values_pg(from_raw)?,
            to: parse_partition_bound_values_pg(to_raw)?,
        });
    }

    if let Some(after_in) = strip_prefix_ci(rest, "IN") {
        let (values_raw, trailing) = take_parenthesized(after_in)
            .ok_or_else(|| format!("invalid LIST partition bounds `{raw}`"))?;
        if !trailing.trim().is_empty() {
            return Err(format!("unsupported trailing LIST partition text `{raw}`"));
        }
        return Ok(PartitionBounds::List {
            values: parse_partition_bound_values_pg(values_raw)?,
        });
    }

    if let Some(after_with) = strip_prefix_ci(rest, "WITH") {
        let (params_raw, trailing) = take_parenthesized(after_with)
            .ok_or_else(|| format!("invalid HASH partition bounds `{raw}`"))?;
        if !trailing.trim().is_empty() {
            return Err(format!("unsupported trailing HASH partition text `{raw}`"));
        }
        let mut modulus = None;
        let mut remainder = None;
        for token in split_pg_value_list(params_raw) {
            let normalized = token.replace('=', " ");
            let parts = normalized.split_whitespace().collect::<Vec<_>>();
            if parts.len() != 2 {
                return Err(format!("invalid HASH partition parameter `{token}`"));
            }
            if parts[0].eq_ignore_ascii_case("MODULUS") {
                modulus = Some(
                    parts[1]
                        .parse::<u32>()
                        .map_err(|_| format!("invalid HASH partition modulus `{token}`"))?,
                );
            } else if parts[0].eq_ignore_ascii_case("REMAINDER") {
                remainder = Some(
                    parts[1]
                        .parse::<u32>()
                        .map_err(|_| format!("invalid HASH partition remainder `{token}`"))?,
                );
            }
        }
        return Ok(PartitionBounds::Hash {
            modulus: modulus.ok_or_else(|| format!("missing HASH partition modulus `{raw}`"))?,
            remainder: remainder
                .ok_or_else(|| format!("missing HASH partition remainder `{raw}`"))?,
        });
    }

    Err(format!("unsupported partition bounds `{raw}`"))
}

fn parse_index_storage_params_pg(
    reloptions: Option<Vec<String>>,
) -> Result<Option<IndexStorageParams>, DriftError> {
    let mut params = IndexStorageParams::default();
    for option in reloptions.unwrap_or_default() {
        let Some((key, value)) = option.split_once('=') else {
            continue;
        };
        if key.eq_ignore_ascii_case("pages_per_range") {
            params.pages_per_range = Some(value.parse::<u32>().map_err(|_| {
                DriftError::Snapshot(format!(
                    "invalid index pages_per_range reloption `{option}`"
                ))
            })?);
        } else if key.eq_ignore_ascii_case("fillfactor") {
            params.fillfactor = Some(value.parse::<u32>().map_err(|_| {
                DriftError::Snapshot(format!("invalid index fillfactor reloption `{option}`"))
            })?);
        }
    }
    Ok((!params.is_empty()).then_some(params))
}

#[cfg(pg_seam)]
pub async fn snapshot_schema<D: SqlSession>(
    conn: &D,
    project_schema: &str,
) -> Result<SchemaSnapshot, DriftError> {
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    let mut partitions: BTreeMap<String, PartitionSnapshot> = BTreeMap::new();
    let mut views: BTreeMap<String, ViewSnapshot> = BTreeMap::new();
    let mut named_types: BTreeMap<String, NamedTypeSnapshot> = BTreeMap::new();
    // Retain non-pg_catalog function/operator/collation provenance, including
    // same-spelling objects selected through search_path, until CHECK recovery
    // below may promote a text column onto the ID-default comparison surface.
    let mut default_has_user_semantic_dependency: BTreeMap<(String, String), bool> =
        BTreeMap::new();

    let partition_rows = conn
        .query(
            "SELECT child.relname AS partition_name, parent.relname AS parent_name, \
                    pg_get_expr(child.relpartbound, child.oid, true) AS bounds \
             FROM pg_class child \
             JOIN pg_namespace n ON n.oid = child.relnamespace \
             JOIN pg_inherits inh ON inh.inhrelid = child.oid \
             JOIN pg_class parent ON parent.oid = inh.inhparent \
             JOIN pg_namespace pn ON pn.oid = parent.relnamespace \
             WHERE n.nspname = $1 AND pn.nspname = $1 AND child.relispartition = true \
               AND child.relkind IN ('r', 'p') \
             ORDER BY child.relname",
            &[project_schema.into()],
        )
        .await?;
    for r in &partition_rows {
        let bounds_text: String = r.try_get("bounds")?;
        partitions.insert(
            r.try_get("partition_name")?,
            PartitionSnapshot {
                of: r.try_get("parent_name")?,
                bounds: parse_partition_bounds_pg(&bounds_text).map_err(DriftError::Snapshot)?,
            },
        );
    }

    // Base tables in the schema. `table_schema` is BOUND ($1), never interpolated.
    // Child partitions are modeled separately in `SchemaSnapshot::partitions`;
    // their columns/indexes/constraints are inherited/propagated catalog effects.
    let table_rows = conn
        .query(
            "SELECT c.relname AS table_name, obj_description(c.oid, 'pg_class') AS comment \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') AND c.relispartition = false \
             ORDER BY c.relname",
            &[project_schema.into()],
        )
        .await?;
    for r in &table_rows {
        let name: String = r.try_get("table_name")?;
        tables.insert(
            name,
            TableSnapshot {
                columns: Vec::new(),
                indexes: Vec::new(),
                constraints: Vec::new(),
                runtime_options: Default::default(),
                partition_by: None,
                comment: r.try_get("comment").ok().flatten(),
                // PG recovers CHECK / generated / partial-index references from the
                // structured buckets (pg_get_constraintdef / pg_get_expr); no raw text.
                stored_create_sql: None,
            },
        );
    }

    let partitioned_table_rows = conn
        .query(
            "SELECT c.relname AS table_name, p.partstrat, \
                    COALESCE( \
                      array_agg(a.attname ORDER BY k.ord) FILTER (WHERE a.attname IS NOT NULL), \
                      ARRAY[]::text[] \
                    ) AS columns \
             FROM pg_partitioned_table p \
             JOIN pg_class c ON c.oid = p.partrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN unnest(p.partattrs) WITH ORDINALITY AS k(attnum, ord) ON true \
             LEFT JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum \
             WHERE n.nspname = $1 \
             GROUP BY c.relname, p.partstrat \
             ORDER BY c.relname",
            &[project_schema.into()],
        )
        .await?;
    for r in &partitioned_table_rows {
        let table: String = r.try_get("table_name")?;
        let Some(t) = tables.get_mut(&table) else {
            continue;
        };
        let columns: Vec<String> = r.try_get("columns").unwrap_or_default();
        let partstrat: i8 = r.try_get("partstrat")?;
        t.partition_by = match u8::try_from(partstrat).ok().map(char::from) {
            Some('r') => Some(PartitionSpec::Range {
                columns,
                collapse: false,
            }),
            Some('l') => Some(PartitionSpec::List {
                columns,
                collapse: false,
            }),
            Some('h') => Some(PartitionSpec::Hash {
                columns,
                collapse: false,
            }),
            _ => None,
        };
    }

    // Plain and materialized views in the schema. Definitions are carried as
    // diagnostic metadata but excluded from equality for now; materialized-ness is
    // structural because SQLite cannot represent it and PG uses a distinct object
    // class.
    let view_rows = conn
        .query(
            "SELECT c.relname AS view_name, c.relkind, pg_get_viewdef(c.oid, true) AS definition, \
                    obj_description(c.oid, 'pg_class') AS comment \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('v', 'm') \
             ORDER BY c.relname",
            &[project_schema.into()],
        )
        .await?;
    for r in &view_rows {
        let name: String = r.try_get("view_name")?;
        let relkind: i8 = r.try_get("relkind")?;
        let materialized = matches!(u8::try_from(relkind).ok().map(char::from), Some('m'));
        let definition: Option<String> = r.try_get("definition").ok().flatten();
        views.insert(
            name,
            ViewSnapshot {
                materialized,
                columns: None,
                definition,
                comment: r.try_get("comment").ok().flatten(),
            },
        );
    }

    let type_rows = conn
        .query(
            "SELECT t.typname AS type_name, t.typtype, obj_description(t.oid, 'pg_type') AS comment \
             FROM pg_type t \
             JOIN pg_namespace n ON n.oid = t.typnamespace \
             WHERE n.nspname = $1 AND t.typtype IN ('e', 'd') \
             ORDER BY t.typname",
            &[project_schema.into()],
        )
        .await?;
    for r in &type_rows {
        let typtype: i8 = r.try_get("typtype")?;
        let kind = match u8::try_from(typtype).ok().map(char::from) {
            Some('e') => "enum",
            Some('d') => "domain",
            _ => continue,
        };
        named_types.insert(
            r.try_get("type_name")?,
            NamedTypeSnapshot {
                kind: kind.to_string(),
                comment: r.try_get("comment").ok().flatten(),
            },
        );
    }

    // Columns (one query for the whole schema; bucket by table).
    //
    // `information_schema.columns.data_type` reports `USER-DEFINED` for any
    // extension / composite type (pgvector's `vector(N)`, PostGIS's
    // `geography(POINT, 4326)`), which loses the precise spelling the desired
    // snapshot carries — so those columns would phantom-drift forever. We also
    // pull `pg_catalog.format_type(atttypid, atttypmod)` (the canonical PG
    // spelling, e.g. `vector(384)` / `geography(Point,4326)`) and, for a
    // `USER-DEFINED` column, normalise it back to the engine's DDL spelling
    // (see [`canonical_extension_type`]). T13.
    let col_rows = conn
        .query(
            "SELECT c.table_name, c.column_name, c.data_type, \
                    c.udt_schema, c.udt_name, c.domain_schema, c.domain_name, \
                    column_type.typtype::text AS type_kind, c.is_nullable, \
                    c.identity_generation, a.attgenerated::text AS generated_kind, \
                    c.character_maximum_length, c.collation_schema, c.collation_name, \
                    format_type(a.atttypid, a.atttypmod) AS format_type, \
                    pg_get_expr(ad.adbin, ad.adrelid) AS column_default, \
                    default_sequence.schema_name AS default_sequence_schema, \
                    default_sequence.sequence_name AS default_sequence_name, \
                    EXISTS ( \
                      SELECT 1 \
                      FROM pg_depend dep \
                      WHERE dep.classid = 'pg_attrdef'::regclass \
                        AND dep.objid = ad.oid \
                        AND ( \
                          (dep.refclassid = 'pg_proc'::regclass AND EXISTS ( \
                            SELECT 1 FROM pg_proc semantic_object \
                            JOIN pg_namespace semantic_ns \
                              ON semantic_ns.oid = semantic_object.pronamespace \
                            WHERE semantic_object.oid = dep.refobjid \
                              AND semantic_ns.nspname <> 'pg_catalog' \
                          )) OR \
                          (dep.refclassid = 'pg_operator'::regclass AND EXISTS ( \
                            SELECT 1 FROM pg_operator semantic_object \
                            JOIN pg_namespace semantic_ns \
                              ON semantic_ns.oid = semantic_object.oprnamespace \
                            WHERE semantic_object.oid = dep.refobjid \
                              AND semantic_ns.nspname <> 'pg_catalog' \
                          )) OR \
                          (dep.refclassid = 'pg_collation'::regclass AND EXISTS ( \
                            SELECT 1 FROM pg_collation semantic_object \
                            JOIN pg_namespace semantic_ns \
                              ON semantic_ns.oid = semantic_object.collnamespace \
                            WHERE semantic_object.oid = dep.refobjid \
                              AND semantic_ns.nspname <> 'pg_catalog' \
                          )) \
                        ) \
                    ) AS default_has_user_semantic_dependency, \
                    col_description(rel.oid, a.attnum) AS comment \
             FROM information_schema.columns c \
             JOIN pg_namespace n ON n.nspname = c.table_schema \
             JOIN pg_class rel ON rel.relname = c.table_name AND rel.relnamespace = n.oid \
             JOIN pg_attribute a ON a.attrelid = rel.oid AND a.attname = c.column_name \
             JOIN pg_type column_type ON column_type.oid = a.atttypid \
             LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
             LEFT JOIN LATERAL ( \
               SELECT sn.nspname AS schema_name, seq.relname AS sequence_name \
               FROM pg_depend dep \
               JOIN pg_class seq ON seq.oid = dep.refobjid AND seq.relkind = 'S' \
               JOIN pg_namespace sn ON sn.oid = seq.relnamespace \
               WHERE dep.classid = 'pg_attrdef'::regclass \
                 AND dep.objid = ad.oid \
                 AND dep.refclassid = 'pg_class'::regclass \
               ORDER BY sn.nspname, seq.relname \
               LIMIT 1 \
             ) default_sequence ON true \
             WHERE c.table_schema = $1 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY c.table_name, c.column_name",
            &[project_schema.into()],
        )
        .await?;
    for r in &col_rows {
        let table: String = r.try_get("table_name")?;
        if let Some(t) = tables.get_mut(&table) {
            let column_name: String = r.try_get("column_name")?;
            let nullable: String = r.try_get("is_nullable")?;
            let data_type: String = r.try_get("data_type")?;
            let udt_schema: String = r.try_get("udt_schema")?;
            let udt_name: String = r.try_get("udt_name")?;
            let domain_schema: Option<String> = r.try_get("domain_schema")?;
            let domain_name: Option<String> = r.try_get("domain_name")?;
            let type_kind: String = r.try_get("type_kind")?;
            let format_type: String = r.try_get("format_type")?;
            let identity = match r
                .try_get::<_, Option<String>>("identity_generation")
                .ok()
                .flatten()
                .as_deref()
            {
                Some(generation) if generation.eq_ignore_ascii_case("ALWAYS") => {
                    Some(IdentityCol { always: true })
                }
                Some(generation) if generation.eq_ignore_ascii_case("BY DEFAULT") => {
                    Some(IdentityCol { always: false })
                }
                _ => None,
            };
            let is_citext = data_type.eq_ignore_ascii_case("USER-DEFINED")
                && (is_citext_extension_type(&format_type)
                    || udt_name.eq_ignore_ascii_case("citext"));
            // For a `USER-DEFINED` (extension) type, recover the precise spelling
            // from `format_type` and canonicalise it to the engine's DDL form so
            // it round-trips against the desired snapshot.
            let data_type = if type_kind == "e" {
                // `format_type` is an exact DDL spelling, not a comparison key:
                // quoted/mixed-case names appear as `"Schema"."Type"`. Keep
                // the unquoted information_schema identity for structural
                // comparison and retain `format_type` separately below.
                format!("{udt_schema}.{udt_name}")
            } else if type_kind == "d" {
                // For a domain, information_schema exposes the domain identity
                // through domain_schema/domain_name while udt_* names its base
                // type. The actual column type is the domain, so compare that
                // catalog identity rather than the underlying type.
                format!(
                    "{}.{}",
                    domain_schema.as_deref().unwrap_or(&udt_schema),
                    domain_name.as_deref().unwrap_or(&udt_name)
                )
            } else if data_type.eq_ignore_ascii_case("USER-DEFINED") {
                canonical_extension_type(&format_type)
            } else if data_type.eq_ignore_ascii_case("ARRAY")
                && (udt_name == "_text" || format_type.eq_ignore_ascii_case("text[]"))
            {
                "text[]".to_string()
            } else if let Some(len) = r
                .try_get::<_, Option<i32>>("character_maximum_length")
                .ok()
                .flatten()
                .filter(|len| *len > 0)
            {
                // Recompose a length-qualified type's LENGTH into `data_type`.
                // `information_schema` reports the bare base name in `data_type` and
                // splits the modifier out into `character_maximum_length`, while the
                // desired snapshot spells the length INLINE (`character varying(255)`
                // for `t.string()`, `character(10)` for `t.char()`). Without this the
                // two sides can never compare equal, so every length-qualified column
                // false-drifts -- and `t.string()` defaults to `length: 255`, which
                // makes `character varying(255)` the DEFAULT string type. Keyed on the
                // catalog datum rather than on a list of type names: PostgreSQL
                // populates `character_maximum_length` for exactly the four types that
                // take a length (`character`, `character varying`, `bit`,
                // `bit varying`) and leaves it NULL everywhere else, so a per-name arm
                // would leave the same gap open for the next type. Widens the previous
                // `character`-only arm; `character(N)` keeps its exact spelling.
                //
                // Does NOT cover the modifiers information_schema reports through
                // OTHER catalog columns: `numeric(p, s)` precision/scale and
                // `time`/`timestamp`/`interval` precision stay BARE on BOTH sides on
                // purpose -- the desired side deliberately routes decimal precision to
                // `ddl_type_override` and keeps `numeric` as the comparison key (see
                // `render::lower::author_type_override`), so recomposing those here
                // would CREATE the drift this removes. Arrays and domains never reach
                // this arm (the `ARRAY`/`type_kind` arms above claim them first, and
                // PostgreSQL reports a NULL `character_maximum_length` for an array of
                // a bounded type anyway).
                format!("{data_type}({len})")
            } else {
                data_type
            };
            let (comment, comment_sentinel) =
                split_column_catalog_comment(r.try_get("comment").ok().flatten());
            // Generated expressions also live in pg_attrdef, but they are not
            // column defaults. Reading adbin without this gate would project a
            // clean generated UUID/TypeID expression onto the ID-default drift
            // surface even though information_schema.column_default is NULL.
            let is_generated = !r.try_get::<_, String>("generated_kind")?.is_empty();
            let raw_default: Option<String> = if is_generated {
                None
            } else {
                r.try_get("column_default").ok().flatten()
            };
            let has_user_semantic_dependency: bool =
                !is_generated && r.try_get::<_, bool>("default_has_user_semantic_dependency")?;
            default_has_user_semantic_dependency.insert(
                (table.clone(), column_name.clone()),
                has_user_semantic_dependency,
            );
            // `pg_get_expr`'s regclass spelling is search_path-sensitive: the
            // same nextval may deparse as either `'seq'::regclass` or
            // `'schema.seq'::regclass`. Confirm that the whole expression is the
            // narrow nextval form, then take its sequence identity from pg_depend
            // so drift comparison is stable and schema-exact.
            let parsed_nextval = recover_nextval_default(raw_default.clone());
            let structured_nextval = parsed_nextval.as_ref().and_then(|_| {
                let schema: Option<String> = r.try_get("default_sequence_schema").ok().flatten();
                let name: Option<String> = r.try_get("default_sequence_name").ok().flatten();
                name.map(|name| {
                    crate::render::declarative::nextval_default_expr(&SequenceRef { name, schema })
                })
            });
            let default = structured_nextval.or(parsed_nextval).or(raw_default);
            let id_default = recover_pg_id_default(
                &data_type,
                identity,
                default.as_deref(),
                false,
                has_user_semantic_dependency,
            );
            t.columns.push(ColumnSnapshot {
                name: column_name,
                data_type,
                nullable: nullable.eq_ignore_ascii_case("YES"),
                // Preserve PostgreSQL's canonical, modifier-aware DDL spelling
                // for operations such as online rename that must reproduce the
                // exact live type rather than information_schema's base family.
                ddl_type_override: Some(format_type),
                // Raw defaults and inline encryption sentinels are emission-only.
                // `id_default` below carries the narrow semantic comparison key.
                // COMMENT-based runtime sentinels are classified into
                // `comment_sentinel` so they do not drift against user-authored
                // catalog comments.
                identity,
                case_sensitive: if is_citext { Some(false) } else { None },
                collation: r
                    .try_get::<_, Option<String>>("collation_name")
                    .ok()
                    .flatten()
                    .map(|name| ColumnCollationSnapshot {
                        schema: r
                            .try_get::<_, Option<String>>("collation_schema")
                            .ok()
                            .flatten(),
                        name,
                    }),
                default,
                id_default,
                comment,
                comment_sentinel,
                ..Default::default()
            });
        }
    }

    // Indexes via pg_catalog (schema BOUND as a text comparison on the namespace
    // name). `indisunique` distinguishes unique indexes. The KEY columns (the
    // leading `indnkeyatts` entries of `indkey`) are recovered IN ORDER by
    // `unnest(indkey) WITH ORDINALITY` joined to `pg_attribute`, so a composite
    // / custom-named index carries its real column list (recovering columns from
    // the index NAME is unsound — 1a). Expression keys (`attnum = 0`) are kept
    // in an ordered `elements` list via `pg_get_indexdef(index, ord)`, and any
    // INCLUDE columns (ordinal beyond `indnkeyatts`) are excluded.
    //
    // The ACCESS METHOD is recovered from `pg_am.amname` (`am.amname`) so a
    // GIN/GiST/ivfflat/hnsw index round-trips as that kind and a method FLIP is
    // surfaced (#index-method-drift). The partial predicate text is recovered via
    // `pg_get_expr(indpred, indrelid)`; expression keys are ordered elements, not
    // folded into the predicate string.
    //
    // INVALID indexes are intentionally absent from the structural snapshot. They
    // are not usable implementations of a declared index, and treating them as
    // present would let guarded two-phase `CREATE INDEX CONCURRENTLY` falsely
    // `SatisfiedNoop` instead of entering recovery and rebuilding.
    let idx_rows = conn
        .query(
            "SELECT c.relname AS table_name, ic.relname AS index_name, x.indisunique, \
                    am.amname AS access_method, obj_description(ic.oid, 'pg_class') AS comment, \
                    pg_get_expr(x.indpred, x.indrelid) AS index_pred, \
                    ( \
                      SELECT array_agg( \
                        CASE \
                          WHEN k.attnum = 0 THEN 'expr:' || pg_get_indexdef(x.indexrelid, k.ord::int, true) \
                          WHEN ((x.indoption[(k.ord - 1)::int])::int & 1) = 1 THEN 'col_desc:' || att.attname \
                          ELSE 'col:' || att.attname \
                        END \
                        ORDER BY k.ord \
                      ) \
                      FROM unnest(x.indkey) WITH ORDINALITY AS k(attnum, ord) \
                      LEFT JOIN pg_attribute att \
                        ON att.attrelid = x.indrelid AND att.attnum = k.attnum \
                      WHERE k.ord <= x.indnkeyatts \
                    ) AS elements, \
                    ( \
                      SELECT array_agg(att.attname ORDER BY k.ord) \
                      FROM unnest(x.indkey) WITH ORDINALITY AS k(attnum, ord) \
                      JOIN pg_attribute att \
                        ON att.attrelid = x.indrelid AND att.attnum = k.attnum \
                      WHERE k.ord <= x.indnkeyatts AND k.attnum <> 0 \
                    ) AS columns, \
                    ( \
                      SELECT array_agg(att.attname ORDER BY k.ord) \
                      FROM unnest(x.indkey) WITH ORDINALITY AS k(attnum, ord) \
                      JOIN pg_attribute att \
                        ON att.attrelid = x.indrelid AND att.attnum = k.attnum \
                      WHERE k.ord > x.indnkeyatts AND k.attnum <> 0 \
                    ) AS include, \
                    ic.reloptions AS reloptions \
             FROM pg_index x \
             JOIN pg_class c ON c.oid = x.indrelid \
             JOIN pg_class ic ON ic.oid = x.indexrelid \
             JOIN pg_am am ON am.oid = ic.relam \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND x.indisvalid = true \
               AND NOT EXISTS ( \
                 SELECT 1 FROM pg_constraint con \
                 WHERE con.conindid = x.indexrelid AND con.contype = 'x' \
               ) \
             ORDER BY c.relname, ic.relname",
            &[project_schema.into()],
        )
        .await?;
    for r in &idx_rows {
        let table: String = r.try_get("table_name")?;
        if let Some(t) = tables.get_mut(&table) {
            // `array_agg` over an empty/all-expression key set is SQL NULL → an
            // empty column list (a wholly-expression index has no plain columns).
            let columns: Vec<String> = r.try_get("columns").unwrap_or_default();
            let include: Vec<String> = r.try_get("include").unwrap_or_default();
            let reloptions: Option<Vec<String>> = r.try_get("reloptions").ok().flatten();
            let element_tokens: Vec<String> = r.try_get("elements").unwrap_or_default();
            let elements = if element_tokens.is_empty() {
                columns
                    .iter()
                    .cloned()
                    .map(IndexElementSnapshot::column)
                    .collect()
            } else {
                element_tokens
                    .into_iter()
                    .filter_map(|token| {
                        token
                            .strip_prefix("col_desc:")
                            .map(|name| {
                                IndexElementSnapshot::column_ordered(name, IndexSortOrder::Desc)
                            })
                            .or_else(|| {
                                token.strip_prefix("col:").map(IndexElementSnapshot::column)
                            })
                            .or_else(|| token.strip_prefix("expr:").map(IndexElementSnapshot::expr))
                    })
                    .collect()
            };
            t.indexes.push(IndexSnapshot {
                name: r.try_get("index_name")?,
                unique: r.try_get("indisunique")?,
                elements,
                columns,
                access_method: r.try_get("access_method")?,
                predicate: r.try_get("index_pred").ok().flatten(),
                include,
                with: parse_index_storage_params_pg(reloptions)?,
                only: false,
                // Emission-only; never recovered from the catalog.
                opclass: None,
                nulls_not_distinct: false,
                comment: r.try_get("comment").ok().flatten(),
            });
        }
    }

    // Constraints via pg_catalog (schema BOUND $1 on the child table's namespace).
    // FK definitions are rebuilt from the structured catalog fields rather than
    // `pg_get_constraintdef`: the latter omits the referenced schema whenever the
    // target happens to be visible on `search_path`, which would make the same FK
    // snapshot differently from one session to the next. `conkey`/`confkey` are
    // expanded with ordinality so composite column order and arity remain exact.
    //
    // CHECK definitions still come from `pg_get_constraintdef`. Engine-owned
    // TypeID/ULID checks are recognized and projected onto their column's semantic
    // `value_format` facet instead of also appearing as a generic constraint. An
    // altered check intentionally fails recognition and remains a constraint, so
    // the column loses its expected format and the altered body is never accepted
    // as equivalent. EXCLUDE bodies remain presence/kind-only because PostgreSQL
    // canonicalizes them differently from the authored IR render.
    let constraint_rows = conn
        .query(
            "SELECT c.relname AS table_name, con.conname AS constraint_name, \
                    con.contype::text AS contype, \
                    pg_get_constraintdef(con.oid) AS definition, \
                    obj_description(con.oid, 'pg_constraint') AS comment, \
                    ARRAY( \
                      SELECT a.attname \
                      FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) \
                      JOIN pg_attribute a \
                        ON a.attrelid = con.conrelid AND a.attnum = k.attnum \
                      ORDER BY k.ord \
                    ) AS local_columns, \
                    rn.nspname AS referenced_schema, rc.relname AS referenced_table, \
                    CASE WHEN con.contype = 'f' THEN ARRAY( \
                      SELECT a.attname \
                      FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord) \
                      JOIN pg_attribute a \
                        ON a.attrelid = con.confrelid AND a.attnum = k.attnum \
                      ORDER BY k.ord \
                    ) END AS referenced_columns, \
                    ARRAY( \
                      SELECT a.attname \
                      FROM jsonb_array_elements_text( \
                        CASE \
                          WHEN jsonb_typeof(to_jsonb(con)->'confdelsetcols') = 'array' \
                          THEN to_jsonb(con)->'confdelsetcols' \
                          ELSE '[]'::jsonb \
                        END \
                      ) WITH ORDINALITY AS k(attnum, ord) \
                      JOIN pg_attribute a \
                        ON a.attrelid = con.conrelid \
                       AND a.attnum = k.attnum::smallint \
                      ORDER BY k.ord \
                    ) AS delete_set_columns, \
                    con.confupdtype::text AS on_update, \
                    con.confdeltype::text AS on_delete, \
                    con.confmatchtype::text AS match_type, \
                    con.condeferrable, con.condeferred, con.convalidated, \
                    EXISTS ( \
                      SELECT 1 \
                      FROM pg_depend dep \
                      WHERE dep.classid = 'pg_constraint'::regclass \
                        AND dep.objid = con.oid \
                        AND ( \
                          (dep.refclassid = 'pg_proc'::regclass AND EXISTS ( \
                            SELECT 1 FROM pg_proc semantic_object \
                            JOIN pg_namespace semantic_ns \
                              ON semantic_ns.oid = semantic_object.pronamespace \
                            WHERE semantic_object.oid = dep.refobjid \
                              AND semantic_ns.nspname <> 'pg_catalog' \
                          )) OR \
                          (dep.refclassid = 'pg_operator'::regclass AND EXISTS ( \
                            SELECT 1 FROM pg_operator semantic_object \
                            JOIN pg_namespace semantic_ns \
                              ON semantic_ns.oid = semantic_object.oprnamespace \
                            WHERE semantic_object.oid = dep.refobjid \
                              AND semantic_ns.nspname <> 'pg_catalog' \
                          )) OR \
                          (dep.refclassid = 'pg_collation'::regclass AND EXISTS ( \
                            SELECT 1 FROM pg_collation semantic_object \
                            JOIN pg_namespace semantic_ns \
                              ON semantic_ns.oid = semantic_object.collnamespace \
                            WHERE semantic_object.oid = dep.refobjid \
                              AND semantic_ns.nspname <> 'pg_catalog' \
                          )) \
                        ) \
                    ) AS has_user_semantic_dependency \
             FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_class rc ON rc.oid = con.confrelid \
             LEFT JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
             WHERE n.nspname = $1 AND con.contype IN ('p', 'f', 'u', 'c', 'x') \
             ORDER BY c.relname, con.conname",
            &[project_schema.into()],
        )
        .await?;
    for r in &constraint_rows {
        let table: String = r.try_get("table_name")?;
        if let Some(t) = tables.get_mut(&table) {
            let contype: String = r.try_get("contype")?;
            let kind = match contype.as_str() {
                "p" => "PRIMARY KEY",
                "f" => "FOREIGN KEY",
                "u" => "UNIQUE",
                "c" => "CHECK",
                "x" => "EXCLUDE",
                _ => "UNKNOWN",
            };
            let catalog_definition: String = r.try_get("definition")?;
            let local_columns: Vec<String> = r.try_get("local_columns").unwrap_or_default();
            let convalidated: bool = r.try_get("convalidated")?;
            let has_user_semantic_dependency: bool = r.try_get("has_user_semantic_dependency")?;

            if kind == "CHECK"
                && convalidated
                && !has_user_semantic_dependency
                && local_columns.len() == 1
            {
                let column_name = &local_columns[0];
                if let Some(RecoveredFormatCheck::Value(value_format)) =
                    recover_format_check(column_name, &catalog_definition, SqlDialect::Postgres)
                {
                    if let Some(column) = t
                        .columns
                        .iter_mut()
                        .find(|column| column.name == *column_name)
                    {
                        // A second engine-shaped check on the same column is not
                        // silently consumed: it remains a generic unexpected
                        // constraint below. That makes an out-of-band duplicate or
                        // conflicting format contract visible.
                        if column.value_format.is_none() {
                            column.value_format = Some(value_format);
                            column.id_default = recover_pg_id_default(
                                &column.data_type,
                                column.identity,
                                column.default.as_deref(),
                                true,
                                default_has_user_semantic_dependency
                                    .get(&(table.clone(), column.name.clone()))
                                    .copied()
                                    .unwrap_or(false),
                            );
                            continue;
                        }
                    }
                }
            }

            if kind == "FOREIGN KEY" {
                // Typed references intentionally omit a child format CHECK and
                // inherit format safety through this FK. Promote its
                // default-bearing local columns onto the live ID-default surface
                // while retaining pg_depend provenance; ordinary FK columns remain
                // ignored when the authored snapshot has no ID-default contract.
                for column_name in &local_columns {
                    if let Some(column) = t
                        .columns
                        .iter_mut()
                        .find(|column| column.name == *column_name)
                    {
                        if column.id_default.is_none() && column.default.is_some() {
                            column.id_default = recover_pg_id_default(
                                &column.data_type,
                                column.identity,
                                column.default.as_deref(),
                                true,
                                default_has_user_semantic_dependency
                                    .get(&(table.clone(), column.name.clone()))
                                    .copied()
                                    .unwrap_or(false),
                            );
                        }
                    }
                }
            }

            let definition = if kind == "FOREIGN KEY" {
                pg_foreign_key_definition(
                    &local_columns,
                    &r.try_get::<_, String>("referenced_schema")?,
                    &r.try_get::<_, String>("referenced_table")?,
                    &r.try_get::<_, Vec<String>>("referenced_columns")?,
                    &r.try_get::<_, String>("on_update")?,
                    &r.try_get::<_, String>("on_delete")?,
                    &r.try_get::<_, Vec<String>>("delete_set_columns")?,
                    &r.try_get::<_, String>("match_type")?,
                    r.try_get("condeferrable")?,
                    r.try_get("condeferred")?,
                    convalidated,
                )?
            } else if constraint_definition_is_retained(kind) {
                catalog_definition
            } else {
                String::new()
            };
            t.constraints.push(ConstraintSnapshot {
                name: r.try_get("constraint_name")?,
                kind: kind.to_string(),
                definition,
                comment: r.try_get("comment").ok().flatten(),
                // `conkey` IS PostgreSQL's own cascade predicate: `DROP COLUMN`
                // removes every constraint whose `conkey` contains the dropped
                // attribute. A whole-row CHECK has a NULL `conkey`, which the ARRAY
                // subselect above already resolves to an empty list - exactly the
                // "references no column, never cascades" case.
                cascade_columns: Some(local_columns),
            });
        }
    }

    let seq_rows = conn
        .query(
            "SELECT c.relname AS sequence_name, format_type(s.seqtypid, NULL::integer) AS data_type, \
                    s.seqstart AS start_value, s.seqincrement AS increment_by, \
                    s.seqmin AS min_value, s.seqmax AS max_value, s.seqcache AS cache_size, \
                    s.seqcycle AS cycle, oc.relname AS owned_table, oa.attname AS owned_column, \
                    obj_description(c.oid, 'pg_class') AS comment \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_sequence s ON s.seqrelid = c.oid \
             LEFT JOIN pg_depend od \
               ON od.classid = 'pg_class'::regclass \
              AND od.objid = c.oid \
              AND od.deptype = 'a' \
             LEFT JOIN pg_class oc ON oc.oid = od.refobjid \
             LEFT JOIN pg_namespace onsp ON onsp.oid = oc.relnamespace \
             LEFT JOIN pg_attribute oa ON oa.attrelid = od.refobjid AND oa.attnum = od.refobjsubid \
             WHERE n.nspname = $1 AND c.relkind = 'S' \
               AND (onsp.nspname IS NULL OR onsp.nspname = $1) \
               AND NOT EXISTS ( \
                 SELECT 1 FROM pg_depend d \
                 WHERE d.classid = 'pg_class'::regclass \
                   AND d.objid = c.oid \
                   AND d.deptype = 'i' \
               ) \
             ORDER BY c.relname",
            &[project_schema.into()],
        )
        .await?;
    let mut sequences = std::collections::BTreeMap::new();
    for r in &seq_rows {
        let as_type =
            SequenceDataTypeSnapshot::from_pg_type_name(&r.try_get::<_, String>("data_type")?);
        let increment = SafeI64::new(r.try_get("increment_by")?).map_err(DriftError::Snapshot)?;
        let min_value = normalize_sequence_min_value(as_type, increment, r.try_get("min_value")?)
            .map_err(DriftError::Snapshot)?;
        let max_value = normalize_sequence_max_value(as_type, increment, r.try_get("max_value")?)
            .map_err(DriftError::Snapshot)?;
        let cache_raw: i64 = r.try_get("cache_size")?;
        let cache = u64::try_from(cache_raw)
            .map_err(|_| {
                DriftError::Snapshot(format!("sequence cache size {cache_raw} is negative"))
            })
            .and_then(|n| SafeU64::new(n).map_err(DriftError::Snapshot))?;
        let owned_table: Option<String> = r.try_get("owned_table").ok().flatten();
        let owned_column: Option<String> = r.try_get("owned_column").ok().flatten();
        let owned_by = match (owned_table, owned_column) {
            (Some(table), Some(column)) => Some(SequenceOwnedBy { table, column }),
            _ => None,
        };
        sequences.insert(
            r.try_get("sequence_name")?,
            SequenceSnapshot {
                as_type,
                increment,
                min_value,
                max_value,
                start: SafeI64::new(r.try_get("start_value")?).map_err(DriftError::Snapshot)?,
                cache,
                cycle: r.try_get("cycle")?,
                owned_by,
                comment: r.try_get("comment").ok().flatten(),
            },
        );
    }

    let schema_rows = conn
        .query(
            "SELECT n.nspname AS schema_name, owner.rolname AS owner \
             FROM pg_namespace n \
             JOIN pg_roles owner ON owner.oid = n.nspowner \
             WHERE n.nspname = $1",
            &[project_schema.into()],
        )
        .await?;
    let mut schemas = BTreeMap::new();
    for r in &schema_rows {
        schemas.insert(
            r.try_get("schema_name")?,
            SchemaObjectSnapshot {
                owner: Some(r.try_get("owner")?),
            },
        );
    }

    let extension_rows = conn
        .query(
            "SELECT e.extname AS extension_name, n.nspname AS schema_name \
             FROM pg_extension e \
             JOIN pg_namespace n ON n.oid = e.extnamespace \
             ORDER BY e.extname",
            &[],
        )
        .await?;
    let mut extensions = BTreeMap::new();
    for r in &extension_rows {
        extensions.insert(
            r.try_get("extension_name")?,
            ExtensionSnapshot {
                schema: Some(r.try_get("schema_name")?),
            },
        );
    }

    let role_rows = conn
        .query(
            "SELECT r.rolname, r.rolcanlogin, r.rolsuper, r.rolcreatedb, \
                    r.rolcreaterole, r.rolbypassrls, r.rolinherit, r.rolreplication, \
                    COALESCE( \
                      array_agg(parent.rolname ORDER BY parent.rolname) \
                        FILTER (WHERE parent.rolname IS NOT NULL), \
                      ARRAY[]::text[] \
                    ) AS member_of \
             FROM pg_roles r \
             LEFT JOIN pg_auth_members m ON m.member = r.oid \
             LEFT JOIN pg_roles parent ON parent.oid = m.roleid \
             GROUP BY r.rolname, r.rolcanlogin, r.rolsuper, r.rolcreatedb, \
                      r.rolcreaterole, r.rolbypassrls, r.rolinherit, r.rolreplication \
             ORDER BY r.rolname",
            &[],
        )
        .await?;
    let mut roles = BTreeMap::new();
    for r in &role_rows {
        roles.insert(
            r.try_get("rolname")?,
            RoleSnapshot {
                login: r.try_get("rolcanlogin")?,
                superuser: r.try_get("rolsuper")?,
                create_db: r.try_get("rolcreatedb")?,
                create_role: r.try_get("rolcreaterole")?,
                bypass_rls: r.try_get("rolbypassrls")?,
                inherit: r.try_get("rolinherit")?,
                replication: r.try_get("rolreplication")?,
                member_of: r.try_get("member_of").unwrap_or_default(),
            },
        );
    }

    Ok(SchemaSnapshot {
        tables,
        views,
        named_types,
        sequences,
        roles,
        schemas,
        extensions,
        partitions,
    })
}

/// Compare ONE same-name child partition declared-vs-live, as `(field, expected,
/// actual)` triples in declaration order (`of` before `bounds`).
///
/// Hoisted out of [`diff_snapshots`] so the structural differ and the
/// existence-guard partition probe ([`crate::render::existence_probe::decide`])
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
pub(crate) fn partition_divergences(
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

/// Diff an **expected** snapshot against the **actual** (live) snapshot — a PURE
/// function, no I/O, no DDL.
///
/// The expected snapshot is **supplied by the caller** — the control-plane /
/// authoring layer owns the declared/union schema and is the only
/// component that knows the intended shape. This function does NOT rebuild that
/// model by replaying the migration DDL; that is deliberately the authoring
/// layer's responsibility, and this seam keeps the two concerns separate.
///
/// Returns:
/// - `missing_objects` — present in `expected`, absent in `actual` (a declared
/// table/column/index/constraint the DB never got).
/// - `unexpected_objects` — present in `actual`, absent in `expected` (an
/// out-of-band object created outside the journal — scenario 35).
///
/// Object names are qualified for legibility: a table as `"users"`, a column as
/// `"users.email"`, an index as `"users index orders_email_idx"`, a constraint
/// as `"users constraint users_pkey"`. Output vectors are sorted + deterministic.
///
/// Same-name objects present on BOTH sides are compared ATTRIBUTE-BY-ATTRIBUTE
/// (#1): columns include physical type/nullability, identity/auto-increment,
/// semantic ID defaults, and enforced TypeID/ULID format; indexes include unique,
/// ordered keys, method, predicate, INCLUDE columns, and storage parameters;
/// constraints include kind plus a comparable definition. Foreign-key definitions
/// are canonical structured identities (target schema/table, ordered local and
/// referenced tuples, actions, match behavior, and deferrability), while ordinary
/// CHECK/PK/UNIQUE bodies use the catalog-author comparison spelling. Any
/// divergence becomes an [`AlteredObject`] — closing the out-of-band-`ALTER`
/// blind spot that pure name diffing left open.
#[must_use]
pub fn diff_snapshots(expected: &SchemaSnapshot, actual: &SchemaSnapshot) -> StructuralDrift {
    let mut missing = Vec::new();
    let mut unexpected = Vec::new();
    let mut altered = Vec::new();

    // Tables present in expected but not actual → missing (whole table + its
    // children fold into the single table name; the table is the unit of
    // missing-ness). Tables in actual but not expected → unexpected.
    for name in expected.tables.keys() {
        if !actual.tables.contains_key(name) {
            missing.push(name.clone());
        }
    }
    for name in actual.tables.keys() {
        if !expected.tables.contains_key(name) {
            unexpected.push(name.clone());
        }
    }
    for name in expected.partitions.keys() {
        if !actual.partitions.contains_key(name) {
            missing.push(format!("partition {name}"));
        }
    }
    for name in actual.partitions.keys() {
        if !expected.partitions.contains_key(name) {
            unexpected.push(format!("partition {name}"));
        }
    }
    for (name, exp_partition) in &expected.partitions {
        let Some(act_partition) = actual.partitions.get(name) else {
            continue;
        };
        for (field, expected, actual) in partition_divergences(exp_partition, act_partition) {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("partition {name}"),
                field: field.to_string(),
                expected,
                actual,
            });
        }
    }
    for name in expected.views.keys() {
        if !actual.views.contains_key(name) {
            missing.push(format!("view {name}"));
        }
    }
    for name in actual.views.keys() {
        if !expected.views.contains_key(name) {
            unexpected.push(format!("view {name}"));
        }
    }
    for (name, exp_ty) in &expected.named_types {
        if !actual.named_types.contains_key(name) {
            missing.push(format!("{} {name}", exp_ty.kind));
        }
    }
    for (name, act_ty) in &actual.named_types {
        if !expected.named_types.contains_key(name) {
            unexpected.push(format!("{} {name}", act_ty.kind));
        }
    }
    for (name, exp_ty) in &expected.named_types {
        let Some(act_ty) = actual.named_types.get(name) else {
            continue;
        };
        if exp_ty.kind != act_ty.kind {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("type {name}"),
                field: "kind".to_string(),
                expected: exp_ty.kind.clone(),
                actual: act_ty.kind.clone(),
            });
        }
        if exp_ty.comment != act_ty.comment {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("type {name}"),
                field: "comment".to_string(),
                expected: exp_ty.comment.clone().unwrap_or_default(),
                actual: act_ty.comment.clone().unwrap_or_default(),
            });
        }
    }
    for name in expected.sequences.keys() {
        if !actual.sequences.contains_key(name) {
            missing.push(format!("sequence {name}"));
        }
    }
    for name in actual.sequences.keys() {
        if !expected.sequences.contains_key(name) {
            unexpected.push(format!("sequence {name}"));
        }
    }
    for (name, exp_seq) in &expected.sequences {
        let Some(act_seq) = actual.sequences.get(name) else {
            continue;
        };
        diff_sequence_attrs(name, exp_seq, act_seq, &mut altered);
    }
    for name in expected.roles.keys() {
        if !actual.roles.contains_key(name) {
            missing.push(format!("role {name}"));
        }
    }
    for (name, exp_role) in &expected.roles {
        let Some(act_role) = actual.roles.get(name) else {
            continue;
        };
        diff_role_attrs(name, exp_role, act_role, &mut altered);
    }
    for name in expected.schemas.keys() {
        if !actual.schemas.contains_key(name) {
            missing.push(format!("schema {name}"));
        }
    }
    for (name, exp_schema) in &expected.schemas {
        let Some(act_schema) = actual.schemas.get(name) else {
            continue;
        };
        diff_schema_attrs(name, exp_schema, act_schema, &mut altered);
    }
    for name in expected.extensions.keys() {
        if !actual.extensions.contains_key(name) {
            missing.push(format!("extension {name}"));
        }
    }
    for (name, exp_extension) in &expected.extensions {
        let Some(act_extension) = actual.extensions.get(name) else {
            continue;
        };
        diff_extension_attrs(name, exp_extension, act_extension, &mut altered);
    }
    for (name, exp_v) in &expected.views {
        let Some(act_v) = actual.views.get(name) else {
            continue;
        };
        if exp_v.materialized != act_v.materialized {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("view {name}"),
                field: "materialized".to_string(),
                expected: exp_v.materialized.to_string(),
                actual: act_v.materialized.to_string(),
            });
        }
        if exp_v.comment != act_v.comment {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("view {name}"),
                field: "comment".to_string(),
                expected: exp_v.comment.clone().unwrap_or_default(),
                actual: act_v.comment.clone().unwrap_or_default(),
            });
        }
    }

    // For tables present on BOTH sides, diff their columns / indexes / constraints
    // by name (added/removed → missing/unexpected) AND, for same-name children,
    // by attribute (→ altered).
    for (name, exp_t) in &expected.tables {
        let Some(act_t) = actual.tables.get(name) else {
            continue;
        };
        diff_named(
            name,
            "",
            &exp_t
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
            &act_t
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
            &mut missing,
            &mut unexpected,
        );
        diff_named(
            name,
            "index ",
            &exp_t
                .indexes
                .iter()
                .map(|i| i.name.clone())
                .collect::<Vec<_>>(),
            &act_t
                .indexes
                .iter()
                .map(|i| i.name.clone())
                .collect::<Vec<_>>(),
            &mut missing,
            &mut unexpected,
        );
        diff_named(
            name,
            "constraint ",
            &exp_t
                .constraints
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
            &act_t
                .constraints
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
            &mut missing,
            &mut unexpected,
        );

        // Attribute diff for same-name objects on both sides.
        diff_attrs(name, exp_t, act_t, &mut altered);
    }

    missing.sort_unstable();
    unexpected.sort_unstable();
    altered.sort_unstable_by(|a, b| {
        (&a.table, &a.object, &a.field).cmp(&(&b.table, &b.object, &b.field))
    });
    StructuralDrift {
        missing_objects: missing,
        unexpected_objects: unexpected,
        altered_objects: altered,
    }
}

fn format_sequence_bound(value: Option<SafeI64>) -> String {
    value.map_or_else(|| "default".to_string(), |n| n.to_string())
}

fn format_sequence_owned_by(value: Option<&SequenceOwnedBy>) -> String {
    value.map_or_else(String::new, |owned| {
        format!("{}.{}", owned.table, owned.column)
    })
}

fn parse_single_quoted_sql_string(input: &str) -> Option<String> {
    let mut chars = input.chars();
    if chars.next()? != '\'' {
        return None;
    }
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c == '\'' {
            match chars.next() {
                Some('\'') => out.push('\''),
                None => return Some(out),
                Some(_) => return None,
            }
        } else {
            out.push(c);
        }
    }
    None
}

fn parse_nextval_sequence_ref(expr: &str) -> Option<SequenceRef> {
    let expression = expr.trim();
    // pg_get_expr qualifies the built-in when a same-signature function earlier
    // on search_path would otherwise capture the deparsed spelling. The OID is
    // still proven through pg_depend below, so pg_catalog qualification is
    // catalog decoration rather than generator identity.
    let call = expression
        .strip_prefix("nextval(")
        .or_else(|| expression.strip_prefix("pg_catalog.nextval("))?;
    let inner = call.strip_suffix(')')?.trim();
    let literal = inner.strip_suffix("::regclass")?.trim();
    let regclass = parse_single_quoted_sql_string(literal)?;
    let (schema, name) = match regclass.split_once('.') {
        Some((schema, name)) if !schema.is_empty() && !name.is_empty() => {
            (Some(schema.to_string()), name.to_string())
        }
        None if !regclass.is_empty() => (None, regclass),
        _ => return None,
    };
    Some(SequenceRef { name, schema })
}

fn recover_nextval_default(expr: Option<String>) -> Option<String> {
    let sequence = parse_nextval_sequence_ref(expr.as_deref()?)?;
    Some(crate::render::declarative::nextval_default_expr(&sequence))
}

fn recover_pg_id_default(
    data_type: &str,
    identity: Option<IdentityCol>,
    expression: Option<&str>,
    force_id_surface: bool,
    has_user_semantic_dependency: bool,
) -> Option<IdDefaultSnapshot> {
    let nextval = expression.and_then(|expr| recover_nextval_default(Some(expr.to_string())));
    if !force_id_surface
        && identity.is_none()
        && !data_type.eq_ignore_ascii_case("uuid")
        && nextval.is_none()
        && !has_user_semantic_dependency
    {
        return None;
    }

    let Some(expression) = expression else {
        return Some(IdDefaultSnapshot::Absent);
    };
    if let Some(nextval) = nextval {
        // The sequence dependency proves the regclass target, but not which
        // same-spelling nextval(regclass) function the parser resolved. Only a
        // definition without a user semantic dependency may be the built-in
        // generator contract.
        if !has_user_semantic_dependency {
            return Some(IdDefaultSnapshot::Nextval(nextval));
        }
        return Some(IdDefaultSnapshot::Expression(format!(
            "user-defined:{}",
            catalog_expression_fingerprint_in_dialect(expression, SqlDialect::Postgres)
        )));
    }

    // Every function/operator admitted by the authored closed default AST
    // resolves to a pg_catalog primitive on PostgreSQL. A dependency owned by
    // another schema therefore proves that an out-of-band user object (including
    // a search_path shadow with identical deparsed spelling) participates in
    // this ID default. Keep that provenance in the semantic key for UUID
    // generators and arbitrary closed expressions alike.
    if has_user_semantic_dependency {
        return Some(IdDefaultSnapshot::Expression(format!(
            "user-defined:{}",
            catalog_expression_fingerprint_in_dialect(expression, SqlDialect::Postgres)
        )));
    }
    Some(if data_type.eq_ignore_ascii_case("uuid") {
        catalog_uuid_id_default(Some(expression), SqlDialect::Postgres, None)
    } else {
        catalog_id_default(Some(expression), SqlDialect::Postgres, None)
    })
}

fn pg_foreign_key_action(code: &str, field: &str) -> Result<Option<&'static str>, DriftError> {
    match code {
        // NO ACTION is PostgreSQL's catalog default and pg_get_constraintdef
        // omits it. Keep the same canonical spelling as authored snapshots.
        "a" => Ok(None),
        "r" => Ok(Some("RESTRICT")),
        "c" => Ok(Some("CASCADE")),
        "n" => Ok(Some("SET NULL")),
        "d" => Ok(Some("SET DEFAULT")),
        other => Err(DriftError::Snapshot(format!(
            "unknown PostgreSQL foreign-key {field} action code `{other}`"
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
fn pg_foreign_key_definition(
    local_columns: &[String],
    referenced_schema: &str,
    referenced_table: &str,
    referenced_columns: &[String],
    on_update: &str,
    on_delete: &str,
    delete_set_columns: &[String],
    match_type: &str,
    deferrable: bool,
    initially_deferred: bool,
    validated: bool,
) -> Result<String, DriftError> {
    use std::fmt::Write as _;

    let mut definition = format!(
        "FOREIGN KEY ({}) REFERENCES {}.{}({})",
        crate::render::declarative::constraintdef_cols(local_columns),
        crate::render::declarative::quote_ident_if_needed(referenced_schema),
        crate::render::declarative::quote_ident_if_needed(referenced_table),
        crate::render::declarative::constraintdef_cols(referenced_columns),
    );

    match match_type {
        "s" => {}
        "f" => definition.push_str(" MATCH FULL"),
        "p" => definition.push_str(" MATCH PARTIAL"),
        other => {
            return Err(DriftError::Snapshot(format!(
                "unknown PostgreSQL foreign-key match type code `{other}`"
            )));
        }
    }

    // PostgreSQL canonicalizes policy clauses in this order, independently of
    // their order in the authored DDL.
    if let Some(action) = pg_foreign_key_action(on_update, "ON UPDATE")? {
        let _ = write!(definition, " ON UPDATE {action}");
    }
    if let Some(action) = pg_foreign_key_action(on_delete, "ON DELETE")? {
        let _ = write!(definition, " ON DELETE {action}");
        if !delete_set_columns.is_empty() {
            if !matches!(action, "SET NULL" | "SET DEFAULT") {
                return Err(DriftError::Snapshot(format!(
                    "PostgreSQL foreign key reports ON DELETE column subset for action {action}"
                )));
            }
            let mut seen = std::collections::BTreeSet::new();
            if delete_set_columns.iter().any(|column| {
                !local_columns.iter().any(|local| local == column) || !seen.insert(column)
            }) {
                return Err(DriftError::Snapshot(
                    "PostgreSQL foreign key reports invalid ON DELETE column subset".to_string(),
                ));
            }
            let _ = write!(
                definition,
                " ({})",
                crate::render::declarative::constraintdef_cols(delete_set_columns)
            );
        }
    } else if !delete_set_columns.is_empty() {
        return Err(DriftError::Snapshot(
            "PostgreSQL foreign key reports ON DELETE column subset without an action".to_string(),
        ));
    }
    if deferrable {
        definition.push_str(" DEFERRABLE");
        if initially_deferred {
            definition.push_str(" INITIALLY DEFERRED");
        }
    }
    if !validated {
        definition.push_str(" NOT VALID");
    }
    Ok(definition)
}

fn comparable_nextval_default(expr: Option<&str>) -> Option<String> {
    let sequence = parse_nextval_sequence_ref(expr?)?;
    Some(crate::render::declarative::nextval_default_expr(&sequence))
}

fn diff_sequence_attrs(
    name: &str,
    expected: &SequenceSnapshot,
    actual: &SequenceSnapshot,
    altered: &mut Vec<AlteredObject>,
) {
    let mut push = |field: &str, expected: String, actual: String| {
        if expected != actual {
            altered.push(AlteredObject {
                table: name.to_string(),
                object: format!("sequence {name}"),
                field: field.to_string(),
                expected,
                actual,
            });
        }
    };

    push(
        "as",
        expected.as_type.to_string(),
        actual.as_type.to_string(),
    );
    push(
        "increment",
        expected.increment.to_string(),
        actual.increment.to_string(),
    );
    push(
        "min_value",
        format_sequence_bound(expected.min_value),
        format_sequence_bound(actual.min_value),
    );
    push(
        "max_value",
        format_sequence_bound(expected.max_value),
        format_sequence_bound(actual.max_value),
    );
    push(
        "start",
        expected.start.to_string(),
        actual.start.to_string(),
    );
    push(
        "cache",
        expected.cache.to_string(),
        actual.cache.to_string(),
    );
    push(
        "cycle",
        expected.cycle.to_string(),
        actual.cycle.to_string(),
    );
    push(
        "owned_by",
        format_sequence_owned_by(expected.owned_by.as_ref()),
        format_sequence_owned_by(actual.owned_by.as_ref()),
    );
    push(
        "comment",
        expected.comment.clone().unwrap_or_default(),
        actual.comment.clone().unwrap_or_default(),
    );
}

fn push_vendor_attr(
    altered: &mut Vec<AlteredObject>,
    name: &str,
    object: String,
    field: &str,
    expected: String,
    actual: String,
) {
    if expected != actual {
        altered.push(AlteredObject {
            table: name.to_string(),
            object,
            field: field.to_string(),
            expected,
            actual,
        });
    }
}

fn diff_role_attrs(
    name: &str,
    expected: &RoleSnapshot,
    actual: &RoleSnapshot,
    altered: &mut Vec<AlteredObject>,
) {
    let object = format!("role {name}");
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "login",
        expected.login.to_string(),
        actual.login.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "superuser",
        expected.superuser.to_string(),
        actual.superuser.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "create_db",
        expected.create_db.to_string(),
        actual.create_db.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "create_role",
        expected.create_role.to_string(),
        actual.create_role.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "bypass_rls",
        expected.bypass_rls.to_string(),
        actual.bypass_rls.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "inherit",
        expected.inherit.to_string(),
        actual.inherit.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object.clone(),
        "replication",
        expected.replication.to_string(),
        actual.replication.to_string(),
    );
    push_vendor_attr(
        altered,
        name,
        object,
        "member_of",
        expected.member_of.join(","),
        actual.member_of.join(","),
    );
}

fn diff_schema_attrs(
    name: &str,
    expected: &SchemaObjectSnapshot,
    actual: &SchemaObjectSnapshot,
    altered: &mut Vec<AlteredObject>,
) {
    if let Some(owner) = expected.owner.as_ref() {
        push_vendor_attr(
            altered,
            name,
            format!("schema {name}"),
            "owner",
            owner.clone(),
            actual.owner.clone().unwrap_or_default(),
        );
    }
}

fn diff_extension_attrs(
    name: &str,
    expected: &ExtensionSnapshot,
    actual: &ExtensionSnapshot,
    altered: &mut Vec<AlteredObject>,
) {
    if let Some(schema) = expected.schema.as_ref() {
        push_vendor_attr(
            altered,
            name,
            format!("extension {name}"),
            "schema",
            schema.clone(),
            actual.schema.clone().unwrap_or_default(),
        );
    }
}

fn format_case_sensitive(case_sensitive: Option<bool>) -> &'static str {
    match case_sensitive {
        Some(false) => "false",
        _ => "",
    }
}

fn format_collation(collation: Option<&ColumnCollationSnapshot>) -> String {
    collation.map_or_else(String::new, ColumnCollationSnapshot::display_name)
}

fn format_identity(column: &ColumnSnapshot) -> &'static str {
    match (column.sqlite_rowid, column.identity) {
        (true, Some(identity)) if !identity.always => "sqlite autoincrement",
        (true, None) => "sqlite rowid",
        (_, Some(identity)) if identity.always => "always",
        (_, Some(_)) => "by default / auto increment",
        _ => "",
    }
}

fn format_value_format(value_format: Option<&crate::model::ir::ValueFormat>) -> String {
    match value_format {
        None => String::new(),
        Some(crate::model::ir::ValueFormat::TypeId { prefix }) => {
            format!("typeId({prefix})")
        }
        Some(crate::model::ir::ValueFormat::Ulid) => "ulid".to_string(),
    }
}

fn format_id_default(default: Option<&crate::model::snapshot::IdDefaultSnapshot>) -> String {
    use crate::model::snapshot::IdDefaultSnapshot;
    match default {
        None => String::new(),
        Some(IdDefaultSnapshot::Absent) => "absent".to_string(),
        Some(IdDefaultSnapshot::UuidV4) => "uuidV4".to_string(),
        Some(IdDefaultSnapshot::UuidV7) => "uuidV7".to_string(),
        Some(IdDefaultSnapshot::Nextval(sequence)) => sequence.clone(),
        Some(IdDefaultSnapshot::Literal(value)) => value.clone(),
        Some(IdDefaultSnapshot::UuidLiteral(value)) => value.clone(),
        Some(IdDefaultSnapshot::Expression(expression)) => expression.clone(),
    }
}

fn introspected_table_dialect(table: &TableSnapshot) -> Option<SqlDialect> {
    if table.stored_create_sql.is_some() {
        return Some(SqlDialect::Sqlite);
    }
    if table
        .columns
        .iter()
        .any(|column| column.mysql_text_storage.is_some())
    {
        return Some(SqlDialect::Mysql);
    }
    if table
        .columns
        .iter()
        .any(|column| column.ddl_type_override.is_some())
    {
        return Some(SqlDialect::Postgres);
    }
    None
}

fn column_data_types_eq(expected: &ColumnSnapshot, actual: &ColumnSnapshot) -> bool {
    if expected.data_type == actual.data_type {
        return true;
    }
    if !(expected.sqlite_rowid && actual.sqlite_rowid) {
        return false;
    }

    // SQLite's rowid alias requires the physical declaration `INTEGER PRIMARY
    // KEY`, even when the portable authored integer width was bigint/smallint.
    // Preserve exact type drift everywhere else; this equivalence is confined to
    // two columns already proven to be the same rowid-alias contract.
    let integer_family = |data_type: &str| {
        matches!(
            data_type.trim().to_ascii_lowercase().as_str(),
            "smallint" | "integer" | "bigint" | "int" | "int2" | "int4" | "int8" | "boolean"
        )
    };
    integer_family(&expected.data_type) && integer_family(&actual.data_type)
}

/// Compare the attributes of same-name children (columns/indexes/constraints
/// present on BOTH sides of one table), pushing an [`AlteredObject`] per diverging
/// field. Added/removed children are NOT this function's concern (they go to the
/// missing/unexpected buckets via [`diff_named`]); only matched names are compared.
fn diff_attrs(
    table: &str,
    exp_t: &TableSnapshot,
    act_t: &TableSnapshot,
    altered: &mut Vec<AlteredObject>,
) {
    let mut push = |object: &str, field: &str, expected: &str, actual: &str| {
        if expected != actual {
            altered.push(AlteredObject {
                table: table.to_string(),
                object: object.to_string(),
                field: field.to_string(),
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
    };
    let format_index_elements = |elements: &[IndexElementSnapshot]| {
        elements
            .iter()
            .map(|element| match element {
                IndexElementSnapshot::Column { name, order, .. } => {
                    match canonical_index_sort_order(*order) {
                        Some(IndexSortOrder::Desc) => format!("col:{name} desc"),
                        Some(IndexSortOrder::Asc) | None => format!("col:{name}"),
                    }
                }
                IndexElementSnapshot::Expr(expr) => format!("expr:{expr}"),
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let format_index_storage_params = |params: Option<&IndexStorageParams>| {
        let Some(params) = params else {
            return String::new();
        };
        let mut entries = Vec::new();
        if let Some(pages_per_range) = params.pages_per_range {
            entries.push(format!("pages_per_range={pages_per_range}"));
        }
        if let Some(fillfactor) = params.fillfactor {
            entries.push(format!("fillfactor={fillfactor}"));
        }
        entries.join(",")
    };

    push(
        "table",
        "comment",
        exp_t.comment.as_deref().unwrap_or(""),
        act_t.comment.as_deref().unwrap_or(""),
    );

    // Columns: physical type/nullability, identity/default generation, enforced
    // value format, recoverable text collation, and catalog comment.
    let act_cols: BTreeMap<&str, &ColumnSnapshot> =
        act_t.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    let actual_dialect = introspected_table_dialect(act_t);
    for ec in &exp_t.columns {
        if let Some(ac) = act_cols.get(ec.name.as_str()) {
            let obj = format!("column {}", ec.name);
            if !column_data_types_eq(ec, ac) {
                push(&obj, "data_type", &ec.data_type, &ac.data_type);
            }
            push(
                &obj,
                "nullable",
                &ec.nullable.to_string(),
                &ac.nullable.to_string(),
            );
            push(&obj, "identity", format_identity(ec), format_identity(ac));
            push(
                &obj,
                "format",
                &format_value_format(ec.value_format.as_ref()),
                &format_value_format(ac.value_format.as_ref()),
            );
            push(
                &obj,
                "case_sensitive",
                format_case_sensitive(ec.case_sensitive),
                format_case_sensitive(ac.case_sensitive),
            );
            push(
                &obj,
                "collation",
                &format_collation(ec.collation.as_ref()),
                &format_collation(ac.collation.as_ref()),
            );
            push(
                &obj,
                "comment",
                ec.comment.as_deref().unwrap_or(""),
                ac.comment.as_deref().unwrap_or(""),
            );
            if let Some(expected_default) = ec.id_default.as_ref() {
                let recover_against_expected = || {
                    if actual_dialect == Some(SqlDialect::Mysql) && ac.mysql_text_storage.is_some()
                    {
                        return catalog_text_id_default(
                            ac.default.as_deref(),
                            SqlDialect::Mysql,
                            ac.mysql_default_generated,
                        );
                    }
                    catalog_id_default_for_expected(
                        expected_default,
                        ac.default.as_deref(),
                        actual_dialect,
                        ac.mysql_default_generated,
                    )
                };
                let actual_default =
                    if matches!(expected_default, IdDefaultSnapshot::UuidLiteral(_)) {
                        // A typed UUID reference may intentionally omit its child
                        // format CHECK, so the live side cannot always infer UUID
                        // literal semantics independently. The expected UUID arm is
                        // authoritative for canonicalizing its retained raw default.
                        recover_against_expected()
                    } else {
                        ac.id_default
                            .clone()
                            .unwrap_or_else(recover_against_expected)
                    };
                push(
                    &obj,
                    "default",
                    &format_id_default(Some(expected_default)),
                    &format_id_default(Some(&actual_default)),
                );
            } else {
                // Backward-compatible parsed-nextval seam for callers that
                // construct snapshots directly without the newer semantic key.
                let expected_nextval = comparable_nextval_default(ec.default.as_deref());
                let actual_nextval = comparable_nextval_default(ac.default.as_deref());
                if expected_nextval.is_some() || actual_nextval.is_some() {
                    push(
                        &obj,
                        "default",
                        expected_nextval.as_deref().unwrap_or(""),
                        actual_nextval.as_deref().unwrap_or(""),
                    );
                }
            }
        }
    }

    // Indexes: unique + elements + predicate + comments. A same-name index whose covered columns changed
    // out-of-band (REINDEX over a different column set, or a name reused for a
    // different shape) is surfaced by the `columns` compare — the name-only diff
    // cannot see it (1a).
    let act_idx: BTreeMap<&str, &IndexSnapshot> =
        act_t.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
    for ei in &exp_t.indexes {
        if let Some(ai) = act_idx.get(ei.name.as_str()) {
            let obj = format!("index {}", ei.name);
            push(
                &obj,
                "unique",
                &ei.unique.to_string(),
                &ai.unique.to_string(),
            );
            push(
                &obj,
                "columns",
                &ei.columns.join(","),
                &ai.columns.join(","),
            );
            if !index_elements_canonically_eq(&ei.elements, &ai.elements) {
                push(
                    &obj,
                    "elements",
                    &format_index_elements(&ei.elements),
                    &format_index_elements(&ai.elements),
                );
            }
            // Access-method drift (#index-method-drift): a same-name index whose
            // `pg_am` kind changed out-of-band — e.g. someone dropped the ANN
            // ivfflat and re-created a plain btree under the same name, or vice
            // versa. Name + columns can match while the method silently differs,
            // so this compare is the only thing that catches a btree→ivfflat
            // flip. An expected snapshot built without a method (`""`) opts out of
            // the compare (it never asserts a method it didn't intend to model).
            if !ei.access_method.is_empty() {
                push(&obj, "access_method", &ei.access_method, &ai.access_method);
            }
            if !index_predicates_canonically_eq(ei.predicate.as_deref(), ai.predicate.as_deref()) {
                push(
                    &obj,
                    "predicate",
                    ei.predicate.as_deref().unwrap_or(""),
                    ai.predicate.as_deref().unwrap_or(""),
                );
            }
            push(
                &obj,
                "include",
                &ei.include.join(","),
                &ai.include.join(","),
            );
            push(
                &obj,
                "with",
                &format_index_storage_params(ei.with.as_ref()),
                &format_index_storage_params(ai.with.as_ref()),
            );
            push(&obj, "only", &ei.only.to_string(), &ai.only.to_string());
            push(
                &obj,
                "comment",
                ei.comment.as_deref().unwrap_or(""),
                ai.comment.as_deref().unwrap_or(""),
            );
        }
    }

    // Constraints: kind + comparable canonical definitions. PostgreSQL FKs are
    // structured catalog reconstructions; the other comparable kinds retain the
    // authored/catalog body spelling. EXCLUDE definitions are intentionally
    // presence/kind-only: PG canonicalizes them differently from the authored IR
    // render, and this engine cannot normalize them to a proven comparable form.
    // The existence guard still fails closed for same-name unprovable constraints;
    // structural drift must not false-positive after a clean apply +
    // re-introspection.
    let act_con: BTreeMap<&str, &ConstraintSnapshot> = act_t
        .constraints
        .iter()
        .map(|c| (c.name.as_str(), c))
        .collect();
    for ec in &exp_t.constraints {
        if let Some(ac) = act_con.get(ec.name.as_str()) {
            let obj = format!("constraint {}", ec.name);
            push(&obj, "kind", &ec.kind, &ac.kind);
            if constraint_definition_is_comparable(&ec.kind)
                && constraint_definition_is_comparable(&ac.kind)
            {
                push(&obj, "definition", &ec.definition, &ac.definition);
            }
            push(
                &obj,
                "comment",
                ec.comment.as_deref().unwrap_or(""),
                ac.comment.as_deref().unwrap_or(""),
            );
        }
    }
}

/// Whether a constraint's `definition` text is meaningful to compare across an
/// offline-rendered snapshot and a live catalog read.
///
/// `EXCLUDE` and `CHECK` are excluded because PostgreSQL does not store either
/// body as written: `pg_get_constraintdef` deparses from the parsed tree, so it
/// re-quotes only the identifiers that need it, injects the casts that parse
/// analysis inferred, expands `IN` to `= ANY (ARRAY[...])`, and lowercases
/// keywords. Verified on PostgreSQL 18.4: `CHECK (quantity > 0)` reads back as
/// `CHECK ((quantity > 0))`, `CHECK (code = 'x')` as `CHECK ((code = 'x'::text))`,
/// and `CHECK (TRUE)` as `CHECK (true)`. An offline renderer quotes every column
/// unconditionally and knows no column types, so it cannot reproduce any of that
/// without PostgreSQL's own parse analysis. Comparing the text therefore reports
/// drift on every CHECK constraint that exists, on every comparison.
///
/// This makes the differ agree with the intent already recorded at
/// `render::declarative::field_check_constraints`, which states that CHECK bodies
/// are not re-diffed and that presence plus enforcement are what round-trip.
///
/// What this gives up: a CHECK whose body is altered out of band while keeping
/// its name and kind is not reported. That is a real loss, not a technicality -
/// swapping `CHECK (quantity > 0)` for `CHECK (quantity > -2147483648)` leaves the
/// invariant vacuous and this differ silent. Recovering it needs the treatment
/// foreign keys already get: parse the catalog text back to the closed AST and
/// compare structurally, rather than comparing spellings.
///
/// Kept separate from [`constraint_definition_is_retained`] on purpose. Not
/// comparing a body is not a reason to stop recording it: the guard's fail-closed
/// refusal reports the live definition so an operator can see what is actually
/// installed, and collapsing that to `<present>` would remove the only text in the
/// message that says anything specific.
fn constraint_definition_is_comparable(kind: &str) -> bool {
    !matches!(kind, "EXCLUDE" | "CHECK")
}

/// Whether to store a live constraint's catalog text on the introspected snapshot.
///
/// Wider than [`constraint_definition_is_comparable`]: a `CHECK` body is retained
/// even though it is never compared, because it is read for diagnostics rather than
/// for equality. `EXCLUDE` stays empty, matching what the offline renderer emits for
/// it, so the two sides agree on the field being absent rather than unread.
fn constraint_definition_is_retained(kind: &str) -> bool {
    kind != "EXCLUDE"
}

/// Diff two name lists belonging to one table, pushing qualified names into the
/// missing / unexpected accumulators. `kind_prefix` is `""` for columns (so the
/// name reads `table.col`), `"index "` / `"constraint "` otherwise.
fn diff_named(
    table: &str,
    kind_prefix: &str,
    expected: &[String],
    actual: &[String],
    missing: &mut Vec<String>,
    unexpected: &mut Vec<String>,
) {
    use std::collections::BTreeSet;
    let exp: BTreeSet<&str> = expected.iter().map(String::as_str).collect();
    let act: BTreeSet<&str> = actual.iter().map(String::as_str).collect();
    let label = |child: &str| {
        if kind_prefix.is_empty() {
            format!("{table}.{child}")
        } else {
            format!("{table} {kind_prefix}{child}")
        }
    };
    for child in expected {
        if !act.contains(child.as_str()) {
            missing.push(label(child));
        }
    }
    for child in actual {
        if !exp.contains(child.as_str()) {
            unexpected.push(label(child));
        }
    }
}

// ---------------------------------------------------------------------------
// DriftReport — the aggregate surface (B1 + B2)
// ---------------------------------------------------------------------------

/// The full drift surface for a project: checksum/tamper drift, orphan journal
/// entries, and (when a structural diff is run) missing / unexpected objects.
///
/// Assembled by the caller from [`check_checksum_drift`] and
/// [`diff_snapshots`]; it carries reports only, never DDL or a remediation plan.
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

#[cfg(test)]
mod constraint_definition_tests {
    use super::{diff_snapshots, ConstraintSnapshot};
    use crate::model::snapshot::{SchemaSnapshot, TableSnapshot};
    use crate::TableRuntimeOptions;

    /// These cover the differ directly rather than through a live database. The
    /// PostgreSQL round-trip oracle that found the CHECK mismatch is behind
    /// `skip_if_no_pg!`, so on a checkout with no database configured it reports a
    /// pass without running, and nothing else would notice this contract changing.
    fn snapshot_with(constraints: Vec<ConstraintSnapshot>) -> SchemaSnapshot {
        let mut snapshot = SchemaSnapshot::default();
        snapshot.tables.insert(
            "orders".to_string(),
            TableSnapshot {
                columns: Vec::new(),
                indexes: Vec::new(),
                constraints,
                runtime_options: TableRuntimeOptions::default(),
                partition_by: None,
                comment: None,
                stored_create_sql: None,
            },
        );
        snapshot
    }

    fn constraint(name: &str, kind: &str, definition: &str) -> ConstraintSnapshot {
        ConstraintSnapshot {
            name: name.to_string(),
            kind: kind.to_string(),
            definition: definition.to_string(),
            comment: None,
            cascade_columns: None,
        }
    }

    #[test]
    fn check_bodies_that_differ_only_in_spelling_are_not_drift() {
        // The exact pair that PostgreSQL 18.4 produces: the offline renderer quotes
        // every column, `pg_get_constraintdef` deparses without the quotes.
        let expected = snapshot_with(vec![constraint(
            "orders_quantity_check",
            "CHECK",
            "CHECK ((\"quantity\" > 0))",
        )]);
        let actual = snapshot_with(vec![constraint(
            "orders_quantity_check",
            "CHECK",
            "CHECK ((quantity > 0))",
        )]);

        assert!(
            diff_snapshots(&expected, &actual).is_clean(),
            "a CHECK body that differs only in deparse spelling must not report drift"
        );
    }

    #[test]
    fn a_renamed_check_constraint_is_still_drift() {
        // Skipping the body comparison must not make CHECK constraints invisible:
        // without this, the test above is satisfied by a differ that ignores them.
        let expected = snapshot_with(vec![constraint(
            "orders_quantity_check",
            "CHECK",
            "CHECK ((quantity > 0))",
        )]);
        let actual = snapshot_with(vec![constraint(
            "orders_quantity_chk",
            "CHECK",
            "CHECK ((quantity > 0))",
        )]);

        let drift = diff_snapshots(&expected, &actual);
        assert!(
            !drift.is_clean(),
            "a CHECK constraint present under a different name must report drift: {drift:#?}"
        );
    }

    #[test]
    fn a_check_constraint_that_changed_kind_is_still_drift() {
        let expected = snapshot_with(vec![constraint(
            "orders_quantity_check",
            "CHECK",
            "CHECK ((quantity > 0))",
        )]);
        let actual = snapshot_with(vec![constraint(
            "orders_quantity_check",
            "UNIQUE",
            "UNIQUE (quantity)",
        )]);

        let drift = diff_snapshots(&expected, &actual);
        assert!(
            !drift.is_clean(),
            "a constraint whose kind changed must report drift: {drift:#?}"
        );
    }

    #[test]
    fn a_unique_body_change_is_still_drift() {
        // The exclusion is scoped to CHECK and EXCLUDE. Every other kind still
        // compares its body, so widening the exclusion by accident shows up here.
        let expected = snapshot_with(vec![constraint(
            "orders_code_key",
            "UNIQUE",
            "UNIQUE (code)",
        )]);
        let actual = snapshot_with(vec![constraint(
            "orders_code_key",
            "UNIQUE",
            "UNIQUE (code, tenant)",
        )]);

        let drift = diff_snapshots(&expected, &actual);
        assert!(
            !drift.is_clean(),
            "a UNIQUE body change must still report drift: {drift:#?}"
        );
    }
}
