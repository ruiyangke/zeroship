//! Drift detection (design §5 "Drift", scenarios 35 + 36) — **read-only**.
//!
//! Drift is any divergence between what the journal says happened and either
//! (a) the migration set the operator now ships, or (b) the live database
//! schema. This module **surfaces** drift; it NEVER emits DDL and NEVER mutates
//! anything (design §5: *"surface, don't auto-fix"*). The whole module runs as
//! the admin/read connection over `information_schema`/`pg_catalog`, never as
//! the privileged `migrator` role, and binds every identifier so an injected
//! schema/table name cannot break the introspection queries.
//!
//! Two independent axes:
//!
//! - **B1 — checksum / tamper / orphan drift** ([`check_checksum_drift`]):
//!   compares the journal's recorded checksum for each NET-applied version
//!   against the checksum of the same version in the supplied set. A mismatch
//!   means the migration SQL was edited after it applied, or the journal row was
//!   tampered (design §1.5 / scenario 36). A net-applied version with NO matching
//!   migration in the supplied set is an **orphan** ([`OrphanJournal`]) — the
//!   bundle is missing a migration the database already has. This is the exact
//!   comparison the executor's apply flow does as its abort-on-drift pre-check
//!   (design §2.3 step 3); [`apply`](crate::apply()) calls this function and aborts
//!   if it returns any [`ChecksumDrift`], so the report and the gate share one
//!   implementation.
//!
//! - **B2 — structural introspection** ([`snapshot_schema`] + [`diff_snapshots`]):
//!   introspect the LIVE project schema into a deterministic [`SchemaSnapshot`]
//!   and `diff` it against an **expected** snapshot the CALLER supplies. The
//!   expected snapshot is owned by the control-plane / authoring layer (it holds
//!   the declared/union schema, design §0); this module does NOT rebuild a schema
//!   model by replaying DDL — that is the authoring layer's job. `diff_snapshots`
//!   is a pure function returning a [`StructuralDrift`] report; it never returns
//!   DDL.

use std::collections::BTreeMap;

use compio_postgres::Client;

use crate::apply::executor::BackendError;
use crate::apply::journal::{self, AppliedEntry, JournalError, Phase};
use crate::conn::ExecutorConfig;
use crate::model::migration::Migration;
use crate::model::snapshot::{
    canonical_index_sort_order, index_elements_canonically_eq, index_predicates_canonically_eq,
    normalize_sequence_max_value, normalize_sequence_min_value, ColumnSnapshot,
    ConstraintSnapshot, ExtensionSnapshot, IndexElementSnapshot, IndexSnapshot,
    NamedTypeSnapshot, RoleSnapshot, SchemaObjectSnapshot, SchemaSnapshot,
    SequenceDataTypeSnapshot, SequenceSnapshot, TableSnapshot, ViewSnapshot,
};
use crate::model::ir::{IndexSortOrder, SafeI64, SafeU64, SequenceOwnedBy};

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
/// a downgrade). Surfaced, not silently ignored (executor M1).
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

impl From<compio_postgres::Error> for DriftError {
    fn from(error: compio_postgres::Error) -> Self {
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
/// set (design §2.3 step 3 / §5).
///
/// For each net-applied version (the latest event is `completed`, per
/// [`journal::applied`]):
///
/// - the supplied set has a migration with that version whose checksum differs
///   ⇒ [`ChecksumDrift`] (the migration SQL was mutated after apply, or the
///   journal row was tampered — scenario 36);
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
pub async fn check_checksum_drift(
    conn: &Client,
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
/// with their own `applied()` read, so the repeatable-exemption / kind-mismatch /
/// tamper / orphan rules can never diverge across dialects (design §2.7: the
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
                // v3 Plan E (re-critic) — DRIFT EXEMPTION anchored on the JOURNALED
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
                //  - journaled `repeatable` AND supplied `repeatable=true` ⇒ EXEMPT
                //    (the repeatable phase handles its re-apply);
                //  - journaled once-only (apply/baseline/squash) but supplied
                //    `repeatable=true` ⇒ KIND MISMATCH = TAMPER (the flip-flag attack:
                //    turning an applied once-only into a repeatable to slip a mutated
                //    `up` past the once-only abort) ⇒ ChecksumDrift / abort;
                //  - journaled `repeatable` but supplied `repeatable=false` ⇒ reverse
                //    re-classification (also a kind mismatch) ⇒ ChecksumDrift / abort;
                //  - journaled once-only AND supplied once-only ⇒ the ordinary
                //    once-only tamper guard (changed checksum still aborts).
                let journaled_repeatable =
                    entry.kind.is_some_and(crate::apply::journal::JournaledKind::is_repeatable);
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
/// `ALTER COLUMN … TYPE`, `DROP NOT NULL`, an index losing UNIQUE, a rewritten
/// CHECK predicate). This is the tamper blind spot #1 closes.
///
/// Names only — never DDL. The caller decides what (if anything) to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlteredObject {
    /// The table the object belongs to (e.g. `users`).
    pub table: String,
    /// The object, qualified within the table: a column as `column id`, an index
    /// as `index users_email_idx`, a constraint as `constraint users_age_chk`.
    pub object: String,
    /// The attribute that diverged: `data_type`, `nullable`, `unique`, `columns`,
    /// `access_method`, `expression`, `kind`, or `definition`.
    pub field: String,
    /// The expected snapshot's value for `field`.
    pub expected: String,
    /// The live DB's value for `field`.
    pub actual: String,
}

/// A structural-drift report (the pure [`diff_snapshots`] output).
///
/// Names only — never DDL. The caller (control plane) decides what, if anything,
/// to do; this module's job ends at *surfacing* (design §5).
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
    /// out-of-band `ALTER` (type/nullability/uniqueness/CHECK-body change). The
    /// missing/unexpected name buckets cannot see these because the name still
    /// matches; this bucket is the attribute-aware tamper surface (#1).
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
/// (design §5 structural drift).
///
/// **Read-only.** Hits `information_schema` / `pg_catalog` only; emits no DDL,
/// mutates nothing. Run as the admin/read connection (NOT the `migrator` role).
///
/// **Injection-safe.** `project_schema` is passed as a **bind parameter** to
/// every catalog query — never interpolated into SQL text — so a schema name
/// containing a quote, a semicolon, or any SQL metacharacter selects zero rows
/// rather than altering the query.
///
/// Determinism: the result map is a `BTreeMap` and every column/index/constraint
/// vector is sorted by name, so the snapshot is stable across catalog scan order.
///
/// # Preconditions
/// The caller MUST pass an **admin/read** connection — this function takes
/// whatever [`Client`] it is handed and never elevates to the `migrator` role.
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

fn split_column_catalog_comment(comment: Option<String>) -> (Option<String>, Option<String>) {
    match comment {
        Some(comment) if is_internal_column_comment_sentinel(&comment) => (None, Some(comment)),
        other => (other, None),
    }
}

fn is_internal_column_comment_sentinel(comment: &str) -> bool {
    comment.starts_with("__zsmask:") || comment.starts_with("zsenc:")
}

pub async fn snapshot_schema(
    conn: &Client,
    project_schema: &str,
) -> Result<SchemaSnapshot, DriftError> {
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    let mut views: BTreeMap<String, ViewSnapshot> = BTreeMap::new();
    let mut named_types: BTreeMap<String, NamedTypeSnapshot> = BTreeMap::new();

    // Base tables in the schema. `table_schema` is BOUND ($1), never interpolated.
    let table_rows = conn
        .query(
            "SELECT c.relname AS table_name, obj_description(c.oid, 'pg_class') AS comment \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') \
             ORDER BY c.relname",
            &[&project_schema],
        )
        .await?;
    for r in &table_rows {
        let name: String = r.get("table_name");
        tables.insert(name, TableSnapshot {
            columns: Vec::new(),
            indexes: Vec::new(),
            constraints: Vec::new(),
            runtime_options: Default::default(),
            comment: r.try_get("comment").ok().flatten(),
            // PG recovers CHECK / generated / partial-index references from the
            // structured buckets (pg_get_constraintdef / pg_get_expr); no raw text.
            stored_create_sql: None,
        });
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
            &[&project_schema],
        )
        .await?;
    for r in &view_rows {
        let name: String = r.get("view_name");
        let relkind: i8 = r.get("relkind");
        let materialized = matches!(u8::try_from(relkind).ok().map(char::from), Some('m'));
        let definition: Option<String> = r.try_get("definition").ok().flatten();
        views.insert(name, ViewSnapshot {
            materialized,
            columns: None,
            definition,
            comment: r.try_get("comment").ok().flatten(),
        });
    }

    let type_rows = conn
        .query(
            "SELECT t.typname AS type_name, t.typtype, obj_description(t.oid, 'pg_type') AS comment \
             FROM pg_type t \
             JOIN pg_namespace n ON n.oid = t.typnamespace \
             WHERE n.nspname = $1 AND t.typtype IN ('e', 'd') \
             ORDER BY t.typname",
            &[&project_schema],
        )
        .await?;
    for r in &type_rows {
        let typtype: i8 = r.get("typtype");
        let kind = match u8::try_from(typtype).ok().map(char::from) {
            Some('e') => "enum",
            Some('d') => "domain",
            _ => continue,
        };
        named_types.insert(
            r.get("type_name"),
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
            "SELECT c.table_name, c.column_name, c.data_type, c.is_nullable, \
                    c.character_maximum_length, \
                    format_type(a.atttypid, a.atttypmod) AS format_type, \
                    col_description(rel.oid, a.attnum) AS comment \
             FROM information_schema.columns c \
             JOIN pg_namespace n ON n.nspname = c.table_schema \
             JOIN pg_class rel ON rel.relname = c.table_name AND rel.relnamespace = n.oid \
             JOIN pg_attribute a ON a.attrelid = rel.oid AND a.attname = c.column_name \
             WHERE c.table_schema = $1 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY c.table_name, c.column_name",
            &[&project_schema],
        )
        .await?;
    for r in &col_rows {
        let table: String = r.get("table_name");
        if let Some(t) = tables.get_mut(&table) {
            let nullable: String = r.get("is_nullable");
            let data_type: String = r.get("data_type");
            // For a `USER-DEFINED` (extension) type, recover the precise spelling
            // from `format_type` and canonicalise it to the engine's DDL form so
            // it round-trips against the desired snapshot.
            let data_type = if data_type.eq_ignore_ascii_case("USER-DEFINED") {
                canonical_extension_type(&r.get::<_, String>("format_type"))
            } else if data_type.eq_ignore_ascii_case("character") {
                match r.try_get::<_, Option<i32>>("character_maximum_length").ok().flatten() {
                    Some(len) if len > 0 => format!("character({len})"),
                    _ => data_type,
                }
            } else {
                data_type
            };
            let (comment, comment_sentinel) =
                split_column_catalog_comment(r.try_get("comment").ok().flatten());
            t.columns.push(ColumnSnapshot {
                name: r.get("column_name"),
                data_type,
                nullable: nullable.eq_ignore_ascii_case("YES"),
                // Defaults and inline encryption sentinels are emission-only.
                // COMMENT-based runtime sentinels are classified into
                // `comment_sentinel` so they do not drift against user-authored
                // catalog comments.
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
                    ) AS columns \
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
            &[&project_schema],
        )
        .await?;
    for r in &idx_rows {
        let table: String = r.get("table_name");
        if let Some(t) = tables.get_mut(&table) {
            // `array_agg` over an empty/all-expression key set is SQL NULL → an
            // empty column list (a wholly-expression index has no plain columns).
            let columns: Vec<String> = r.try_get("columns").unwrap_or_default();
            let element_tokens: Vec<String> = r.try_get("elements").unwrap_or_default();
            let elements = if element_tokens.is_empty() {
                columns.iter().cloned().map(IndexElementSnapshot::column).collect()
            } else {
                element_tokens
                    .into_iter()
                    .filter_map(|token| {
                        token
                            .strip_prefix("col_desc:")
                            .map(|name| {
                                IndexElementSnapshot::column_ordered(name, IndexSortOrder::Desc)
                            })
                            .or_else(|| token.strip_prefix("col:").map(IndexElementSnapshot::column))
                            .or_else(|| token.strip_prefix("expr:").map(IndexElementSnapshot::expr))
                    })
                    .collect()
            };
            t.indexes.push(IndexSnapshot {
                name: r.get("index_name"),
                unique: r.get("indisunique"),
                elements,
                columns,
                access_method: r.get("access_method"),
                predicate: r.try_get("index_pred").ok().flatten(),
                // Emission-only; never recovered from the catalog.
                opclass: None,
                comment: r.try_get("comment").ok().flatten(),
            });
        }
    }

    // Constraints via pg_catalog (schema BOUND $1 on the namespace name). We read
    // byte-comparable constraint bodies from `pg_get_constraintdef(oid)` so a
    // same-name CHECK whose predicate was rewritten out-of-band is surfaced by the
    // attribute diff (#1). EXCLUDE bodies are not byte-comparable with the authored
    // IR render, so they are presence/kind-only. `contype` is mapped to the same
    // human label information_schema reports (`PRIMARY KEY` / `FOREIGN KEY` /
    // `UNIQUE` / `CHECK` / `EXCLUDE`) so `kind` is unchanged.
    let constraint_rows = conn
        .query(
            "SELECT c.relname AS table_name, con.conname AS constraint_name, \
                    con.contype AS contype, pg_get_constraintdef(con.oid) AS definition, \
                    obj_description(con.oid, 'pg_constraint') AS comment \
             FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = con.connamespace \
             WHERE n.nspname = $1 AND con.contype IN ('p', 'f', 'u', 'c', 'x') \
             ORDER BY c.relname, con.conname",
            &[&project_schema],
        )
        .await?;
    for r in &constraint_rows {
        let table: String = r.get("table_name");
        if let Some(t) = tables.get_mut(&table) {
            let contype: i8 = r.get("contype");
            let kind = match u8::try_from(contype).ok().map(char::from) {
                Some('p') => "PRIMARY KEY",
                Some('f') => "FOREIGN KEY",
                Some('u') => "UNIQUE",
                Some('c') => "CHECK",
                Some('x') => "EXCLUDE",
                _ => "UNKNOWN",
            };
            let definition = if constraint_definition_is_comparable(kind) {
                r.get("definition")
            } else {
                String::new()
            };
            t.constraints.push(ConstraintSnapshot {
                name: r.get("constraint_name"),
                kind: kind.to_string(),
                definition,
                comment: r.try_get("comment").ok().flatten(),
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
            &[&project_schema],
        )
        .await?;
    let mut sequences = std::collections::BTreeMap::new();
    for r in &seq_rows {
        let as_type = SequenceDataTypeSnapshot::from_pg_type_name(&r.get::<_, String>("data_type"));
        let increment = SafeI64::new(r.get("increment_by")).map_err(DriftError::Snapshot)?;
        let min_value = normalize_sequence_min_value(as_type, increment, r.get("min_value"))
            .map_err(DriftError::Snapshot)?;
        let max_value = normalize_sequence_max_value(as_type, increment, r.get("max_value"))
            .map_err(DriftError::Snapshot)?;
        let cache_raw: i64 = r.get("cache_size");
        let cache = u64::try_from(cache_raw)
            .map_err(|_| DriftError::Snapshot(format!("sequence cache size {cache_raw} is negative")))
            .and_then(|n| SafeU64::new(n).map_err(DriftError::Snapshot))?;
        let owned_table: Option<String> = r.try_get("owned_table").ok().flatten();
        let owned_column: Option<String> = r.try_get("owned_column").ok().flatten();
        let owned_by = match (owned_table, owned_column) {
            (Some(table), Some(column)) => Some(SequenceOwnedBy { table, column }),
            _ => None,
        };
        sequences.insert(
            r.get("sequence_name"),
            SequenceSnapshot {
                as_type,
                increment,
                min_value,
                max_value,
                start: SafeI64::new(r.get("start_value")).map_err(DriftError::Snapshot)?,
                cache,
                cycle: r.get("cycle"),
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
            &[&project_schema],
        )
        .await?;
    let mut schemas = BTreeMap::new();
    for r in &schema_rows {
        schemas.insert(
            r.get("schema_name"),
            SchemaObjectSnapshot {
                owner: Some(r.get("owner")),
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
            r.get("extension_name"),
            ExtensionSnapshot {
                schema: Some(r.get("schema_name")),
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
            r.get("rolname"),
            RoleSnapshot {
                login: r.get("rolcanlogin"),
                superuser: r.get("rolsuper"),
                create_db: r.get("rolcreatedb"),
                create_role: r.get("rolcreaterole"),
                bypass_rls: r.get("rolbypassrls"),
                inherit: r.get("rolinherit"),
                replication: r.get("rolreplication"),
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
    })
}

/// Diff an **expected** snapshot against the **actual** (live) snapshot — a PURE
/// function, no I/O, no DDL (design §5: surface, don't auto-fix).
///
/// The expected snapshot is **supplied by the caller** — the control-plane /
/// authoring layer owns the declared/union schema (design §0) and is the only
/// component that knows the intended shape. This function does NOT rebuild that
/// model by replaying the migration DDL; that is deliberately the authoring
/// layer's responsibility, and this seam keeps the two concerns separate.
///
/// Returns:
/// - `missing_objects` — present in `expected`, absent in `actual` (a declared
///   table/column/index/constraint the DB never got).
/// - `unexpected_objects` — present in `actual`, absent in `expected` (an
///   out-of-band object created outside the journal — scenario 35).
///
/// Object names are qualified for legibility: a table as `"users"`, a column as
/// `"users.email"`, an index as `"users index orders_email_idx"`, a constraint
/// as `"users constraint users_pkey"`. Output vectors are sorted + deterministic.
///
/// Same-name objects present on BOTH sides are compared ATTRIBUTE-BY-ATTRIBUTE
/// (#1): columns by `data_type` + `nullable`, indexes by `unique` + `columns`,
/// constraints by `kind` plus byte-comparable `definition` bodies. Any divergence
/// becomes an [`AlteredObject`] — closing the out-of-band-`ALTER` blind spot that
/// pure name diffing left open.
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
            &exp_t.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            &act_t.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            &mut missing,
            &mut unexpected,
        );
        diff_named(
            name,
            "index ",
            &exp_t.indexes.iter().map(|i| i.name.clone()).collect::<Vec<_>>(),
            &act_t.indexes.iter().map(|i| i.name.clone()).collect::<Vec<_>>(),
            &mut missing,
            &mut unexpected,
        );
        diff_named(
            name,
            "constraint ",
            &exp_t.constraints.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            &act_t.constraints.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
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
    value.map_or_else(String::new, |owned| format!("{}.{}", owned.table, owned.column))
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

    push("as", expected.as_type.to_string(), actual.as_type.to_string());
    push("increment", expected.increment.to_string(), actual.increment.to_string());
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
    push("start", expected.start.to_string(), actual.start.to_string());
    push("cache", expected.cache.to_string(), actual.cache.to_string());
    push("cycle", expected.cycle.to_string(), actual.cycle.to_string());
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
                IndexElementSnapshot::Column { name, order } => {
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

    push("table", "comment", exp_t.comment.as_deref().unwrap_or(""), act_t.comment.as_deref().unwrap_or(""));

    // Columns: data_type + nullable + catalog comment.
    let act_cols: BTreeMap<&str, &ColumnSnapshot> =
        act_t.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    for ec in &exp_t.columns {
        if let Some(ac) = act_cols.get(ec.name.as_str()) {
            let obj = format!("column {}", ec.name);
            push(&obj, "data_type", &ec.data_type, &ac.data_type);
            push(
                &obj,
                "nullable",
                &ec.nullable.to_string(),
                &ac.nullable.to_string(),
            );
            push(
                &obj,
                "comment",
                ec.comment.as_deref().unwrap_or(""),
                ac.comment.as_deref().unwrap_or(""),
            );
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
            push(&obj, "unique", &ei.unique.to_string(), &ai.unique.to_string());
            push(&obj, "columns", &ei.columns.join(","), &ai.columns.join(","));
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
            if !index_predicates_canonically_eq(ei.predicate.as_deref(), ai.predicate.as_deref())
            {
                push(
                    &obj,
                    "predicate",
                    ei.predicate.as_deref().unwrap_or(""),
                    ai.predicate.as_deref().unwrap_or(""),
                );
            }
            push(
                &obj,
                "comment",
                ei.comment.as_deref().unwrap_or(""),
                ai.comment.as_deref().unwrap_or(""),
            );
        }
    }

    // Constraints: kind + byte-comparable definition bodies. EXCLUDE definitions
    // are intentionally presence/kind-only: PG canonicalizes them differently from
    // the authored IR render, and this engine cannot normalize them to a proven
    // comparable form. The existence guard still fails closed for same-name
    // unprovable constraints; structural drift must not false-positive after a
    // clean apply + re-introspection.
    let act_con: BTreeMap<&str, &ConstraintSnapshot> =
        act_t.constraints.iter().map(|c| (c.name.as_str(), c)).collect();
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

fn constraint_definition_is_comparable(kind: &str) -> bool {
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
/// [`diff_snapshots`]; it carries reports only, never DDL or a remediation plan
/// (design §5).
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
