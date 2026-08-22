//! PostgreSQL live-catalog introspection for drift, and the PG journal read that
//! feeds the checksum comparison.
//!
//! The VENDOR half of [`apply::drift`](crate::apply::drift), which keeps the
//! dialect-blind half: the snapshot/report types, the pure `diff_snapshots`, and the
//! `compare_applied_to_set` every backend shares. Everything here names
//! `pg_catalog` / `information_schema`, parses what those return, or exists only to
//! serve something that does — the same division `mysql::drift_sql` and
//! `sqlite::drift_sql` already sit on, so all three introspectors hand the neutral
//! differ the same `SchemaSnapshot` shape.
//!
//! The helpers below are PostgreSQL catalog parsers, and `backend::postgres` is
//! their only caller. They live beside that caller rather than in core precisely
//! so the module boundary — not a build flag — is what keeps them off the neutral
//! path.

use std::collections::BTreeMap;

use super::journal_sql;
use crate::apply::drift::{
    compare_applied_to_set, parse_nextval_sequence_ref, ChecksumDriftReport, DriftError,
};
use crate::conn::ExecutorConfig;
use crate::driver::SqlSession;
use crate::model::ir::{
    IdentityCol, IndexSortOrder, IndexStorageParams, PartitionBoundValue, PartitionBounds,
    PartitionSpec, PolicyCmd, SafeI64, SafeU64, SequenceOwnedBy, SequenceRef, TriggerEvent,
    TriggerTiming,
};
use crate::model::migration::Migration;
use crate::model::snapshot::{
    normalize_sequence_max_value, normalize_sequence_min_value, ColumnCollationSnapshot,
    ColumnSnapshot, ConstraintSnapshot, ExtensionSnapshot, FunctionIdentity, FunctionKey,
    GeneratedKindSnapshot, IdDefaultSnapshot, IndexElementSnapshot, IndexSnapshot,
    NamedTypeSnapshot, PartitionSnapshot, PolicyIdentity, PolicyKey, RoleSnapshot,
    SchemaObjectSnapshot, SchemaSnapshot, SequenceDataTypeSnapshot, SequenceSnapshot,
    TableSnapshot, TriggerIdentity, TriggerKey, VendorObjectIdentities, ViewSnapshot,
};
use crate::render::value_format::{
    catalog_expression_fingerprint_in_dialect, catalog_id_default, catalog_uuid_id_default,
    recover_format_check, RecoveredFormatCheck,
};
use zero_migrate_ir::dialect::DialectId;

impl From<crate::driver::DbError> for DriftError {
    fn from(error: crate::driver::DbError) -> Self {
        Self::Db(error.into())
    }
}

/// Compare the journal's NET-applied checksums against the supplied migration
/// set.
///
/// For each net-applied version (the latest event is `completed`, per
/// [`journal_sql::applied`]):
///
/// - the supplied set has a migration with that version whose checksum differs
/// ⇒ [`ChecksumDrift`](crate::ChecksumDrift) (the migration SQL was mutated after apply, or the
/// journal row was tampered — scenario 36);
/// - the supplied set has NO migration with that version ⇒ [`OrphanJournal`](crate::OrphanJournal).
///
/// The recorded checksum used is the one [`journal_sql::applied`] returns, which is
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
pub async fn check_checksum_drift<D: SqlSession>(
    dialect: &DialectId,
    conn: &D,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<ChecksumDriftReport, DriftError> {
    let applied = journal_sql::applied(conn, cfg, dialect).await?;
    Ok(compare_applied_to_set(&applied, migrations))
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
    // (the engine's descriptor-to-column spelling). Match on
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

pub async fn snapshot_schema<D: SqlSession>(
    dialect: &DialectId,
    conn: &D,
    schema: &str,
) -> Result<SchemaSnapshot, DriftError> {
    snapshot_schema_for(conn, schema, dialect).await
}

/// The savepoint the view-body probe rolls back to. One name, reused per view,
/// because each probe is released before the next is taken.
const VIEW_BODY_PROBE_SAVEPOINT: &str = "zm_view_body_probe";

/// The temp view the probe re-prints through. Lives inside a savepoint that is
/// always rolled back, so it never outlives one iteration.
const VIEW_BODY_PROBE_VIEW: &str = "zm_view_body_probe";

/// Put a COMPARABLE view body on both sides of a drift check, using the server as
/// the only normaliser.
///
/// **WHY THIS EXISTS AS A SEPARATE STEP.** [`diff_snapshots`](crate::diff_snapshots) is pure and has no
/// connection, and the two snapshots it compares do not carry the same
/// representation of a view body. A folded snapshot carries
/// [`ViewSnapshot::authored_query`] - the typed `SelectAst` an author wrote - and
/// leaves [`ViewSnapshot::definition`] `None`. An introspected snapshot carries
/// `definition` from `pg_get_viewdef` and leaves `authored_query` `None`, because a
/// catalog cannot yield a typed body. There is NO field populated on both sides, so
/// before this existed the differ compared a view on `materialized` and `comment`
/// alone and a `CREATE OR REPLACE VIEW` run out of band reported the schema clean.
///
/// **WHY THERE IS NO TEXT NORMALISER HERE, AND WHY THERE MUST NOT BE.** PostgreSQL
/// does not keep a view body as written. It parses it, discards the text, and
/// `pg_get_viewdef` RE-PRINTS it from the parse tree. Measured on PostgreSQL 18.4,
/// authoring `SELECT "id" FROM "s"."src" WHERE "amount" > 10` reads back as
///
/// ```text
///   SELECT id
///     FROM s.src
///    WHERE amount > 10::numeric;
/// ```
///
/// - quoting dropped, whitespace reflowed, a trailing semicolon added, and a
/// `::numeric` cast inserted that nobody wrote. That cast comes from type analysis
/// against the catalog, not from parsing, so no offline pass - not even the
/// PostgreSQL parser this workspace already links - can predict it. A hand-written
/// normaliser would have to erase casts, quoting and whitespace, and one aggressive
/// enough to do that is aggressive enough to erase the body change it exists to
/// find.
///
/// So this does not normalise. It renders the authored body, hands it to the SERVER
/// as a temporary view, and reads BOTH bodies back through `pg_get_viewdef` **in one
/// statement**. Both sides then carry the identical deterministic re-print of the
/// same server, and the differ compares them with `==`. Measured: an authored body
/// and a live view built from it come back byte-identical, while a
/// `CREATE OR REPLACE VIEW` that changes the predicate comes back different.
///
/// Reading both in ONE statement is load-bearing rather than tidy.
/// `pg_get_viewdef` schema-qualifies a relation only when it is outside the current
/// `search_path`, so two reads taken under different `search_path` settings can
/// disagree on qualification alone. One statement cannot.
///
/// **WHAT IT WRITES.** A `CREATE TEMP VIEW` inside a savepoint that is always rolled
/// back, so the probe leaves nothing behind in any schema and is invisible to other
/// sessions - but it IS a write, and a session that cannot write (a read-only
/// transaction, a hot standby) cannot run it. That is why every failure below
/// DECLINES for the view it was probing rather than propagating: a body that could
/// not be re-printed leaves `definition` `None` on the expected side, the differ
/// skips it, and the result is exactly the pre-existing behaviour. Manufacturing
/// drift for every view on a replica would be a louder defect than the blind spot.
///
/// **WHAT IT DOES NOT COVER.** Only views carrying an `authored_query` and present
/// on both sides are probed. An adopted view - one introspected rather than authored
/// - has no typed body anywhere in the history, so there is nothing to compare it
/// against and it stays uncompared.
pub async fn resolve_view_bodies<D: SqlSession>(
    dialect: &DialectId,
    conn: &D,
    schema: &str,
    expected: &mut SchemaSnapshot,
    actual: &mut SchemaSnapshot,
) -> Result<(), DriftError> {
    // Nothing to probe: skip the transaction dance entirely rather than open and
    // roll back a transaction on every drift check of a view-free schema.
    if !expected
        .views
        .iter()
        .any(|(name, view)| view.authored_query.is_some() && actual.views.contains_key(name))
    {
        return Ok(());
    }

    // A savepoint needs a transaction block. `SAVEPOINT` outside one raises 25P01,
    // which is a plain recoverable error and NOT an abort, so trying it is a sound
    // way to ask a question the SQL surface has no function for. Whichever answer
    // comes back, the probe runs inside a transaction it can undo.
    let nested = conn
        .batch(&format!("SAVEPOINT {VIEW_BODY_PROBE_SAVEPOINT}"))
        .await
        .is_ok();
    if !nested {
        conn.batch("BEGIN").await?;
    }

    let outcome = resolve_view_bodies_in_transaction(conn, schema, expected, actual, dialect).await;

    let unwind = if nested {
        conn.batch(&format!(
            "ROLLBACK TO SAVEPOINT {VIEW_BODY_PROBE_SAVEPOINT}; \
             RELEASE SAVEPOINT {VIEW_BODY_PROBE_SAVEPOINT}"
        ))
        .await
    } else {
        conn.batch("ROLLBACK").await
    };

    // The probe's own result wins: an unwind failure on top of a real error would
    // otherwise hide it.
    outcome?;
    unwind?;
    Ok(())
}

/// The body of [`resolve_view_bodies`], running with a transaction already open so
/// every per-view failure has a savepoint to fall back to.
async fn resolve_view_bodies_in_transaction<D: SqlSession>(
    conn: &D,
    schema: &str,
    expected: &mut SchemaSnapshot,
    actual: &mut SchemaSnapshot,
    dialect: &DialectId,
) -> Result<(), DriftError> {
    let names: Vec<String> = expected
        .views
        .iter()
        .filter(|(name, view)| view.authored_query.is_some() && actual.views.contains_key(*name))
        .map(|(name, _)| name.clone())
        .collect();

    for name in names {
        let Some(exp_view) = expected.views.get(&name) else {
            continue;
        };
        let Some(query) = exp_view.authored_query.as_ref() else {
            continue;
        };
        let view_schema = exp_view.authored_schema.as_deref().unwrap_or(schema);
        // The authored body rendered the same way `createView` rendered it when the
        // migration ran. Anything else would be comparing the differ's idea of the
        // body against the engine's.
        let Ok(body) = crate::render::lower::render_view_query(query, view_schema, dialect, None)
        else {
            continue;
        };

        // Per-view savepoint: a body the server refuses (a dropped dependency, a
        // permission it lacks) must not abort the whole probe, and rolling back to
        // this point is also how the temp view is disposed of - no DROP needed,
        // because temp object creation is transactional.
        conn.batch(&format!("SAVEPOINT {VIEW_BODY_PROBE_SAVEPOINT}_one"))
            .await?;
        let probed = probe_one_view_body(conn, view_schema, &name, &body, dialect).await;
        conn.batch(&format!(
            "ROLLBACK TO SAVEPOINT {VIEW_BODY_PROBE_SAVEPOINT}_one; \
             RELEASE SAVEPOINT {VIEW_BODY_PROBE_SAVEPOINT}_one"
        ))
        .await?;

        // `Ok(None)` is a DECLINE - the server would not accept the probe view, so
        // both sides keep what they had and the differ's `both Some` guard skips
        // this view. `Err` is a HARD failure and propagates.
        let Some((expected_body, actual_body)) = probed? else {
            continue;
        };
        if let Some(view) = expected.views.get_mut(&name) {
            view.comparable_body = Some(expected_body);
        }
        if let Some(view) = actual.views.get_mut(&name) {
            view.comparable_body = Some(actual_body);
        }
    }
    Ok(())
}

/// Re-print one authored body and its live counterpart through the same server, in
/// the same statement.
///
/// The two failure kinds are deliberately NOT the same thing:
///
///   * `Ok(None)` - the server refused the probe view. That is the read-only
///     session, the hot standby, and a body whose dependencies no longer resolve.
///     The view declines and drift says nothing about its body, which is the
///     behaviour that existed before any of this.
///   * `Err(..)` - the catalog would not answer for a view the caller has ALREADY
///     established is present in the introspected snapshot. A missing row is an
///     error rather than an absent body, because silently reading it as "no body"
///     is how a comparison stops running without anyone noticing.
/// Spell the `<schema>.<view>` argument of the `pg_get_viewdef` probe below.
///
/// PostgreSQL, named rather than assumed: `pg_get_viewdef` is a PG catalog
/// function and this whole probe lives in the PostgreSQL backend. It used to call
/// the crate's raw escape primitive, which produced correct bytes for no stated
/// dialect.
fn pg_view_ident(ident: &str, dialect: &DialectId) -> String {
    crate::render::dml::escape_quote_ident_for_dialect(ident, dialect)
}

async fn probe_one_view_body<D: SqlSession>(
    conn: &D,
    view_schema: &str,
    name: &str,
    body: &str,
    dialect: &DialectId,
) -> Result<Option<(String, String)>, DriftError> {
    if conn
        .batch(&format!(
            "CREATE TEMP VIEW {VIEW_BODY_PROBE_VIEW} AS {body}"
        ))
        .await
        .is_err()
    {
        return Ok(None);
    }

    // BOTH bodies in ONE statement. `pg_get_viewdef` qualifies a relation only when
    // it falls outside the current `search_path`, so two reads taken separately can
    // differ on qualification alone and manufacture drift on an untouched view.
    //
    // `$1::text::regclass` rather than `$1::regclass`: the latter makes the driver
    // infer a `regclass` parameter and refuse to serialize a string into it.
    let row = conn
        .query_one(
            &format!(
                "SELECT pg_get_viewdef('pg_temp.{VIEW_BODY_PROBE_VIEW}'::regclass, true) \
                        AS expected_body, \
                        pg_get_viewdef($1::text::regclass, true) AS actual_body"
            ),
            &[format!(
                "{}.{}",
                pg_view_ident(view_schema, dialect),
                pg_view_ident(name, dialect)
            )
            .into()],
        )
        .await?;

    let expected_body: Option<String> = row
        .try_get("expected_body")
        .map_err(|error| DriftError::Snapshot(format!("re-print authored view {name}: {error}")))?;
    let actual_body: Option<String> = row
        .try_get("actual_body")
        .map_err(|error| DriftError::Snapshot(format!("re-print live view {name}: {error}")))?;
    match (expected_body, actual_body) {
        (Some(expected_body), Some(actual_body)) => Ok(Some((expected_body, actual_body))),
        // A NULL from `pg_get_viewdef` means the relation resolved to something that
        // is not a view. The caller read this name out of the introspected view map,
        // so that is a contradiction worth raising, not a body to skip.
        _ => Err(DriftError::Snapshot(format!(
            "view {view_schema}.{name} is in the introspected snapshot but the catalog \
             returned no body for it"
        ))),
    }
}

/// The functions, policies and triggers PostgreSQL actually holds in `schema`,
/// reduced to the identity drift is allowed to compare.
///
/// THREE CATALOG READS - `pg_proc`, `pg_policy`, `pg_trigger` - each binding the
/// schema as `$1` like the rest of this module. Before this existed the three maps
/// on [`SchemaSnapshot`] were filled ONLY by the offline fold, so an out-of-band
/// `DROP POLICY` left a table with row-level security enabled and nothing enforcing
/// it while drift reported the schema clean.
///
/// **WHAT IS READ AND WHAT IS REFUSED.** Every textual definition PostgreSQL owns
/// is skipped, because the server normalises all of it and a text compare would
/// report drift forever. Measured on PostgreSQL 18.4:
///
/// | authored                            | catalog                            |
/// |-------------------------------------|------------------------------------|
/// | `f(x int)`                          | `f(integer)`                       |
/// | `USING (owner = current_user)`       | `((owner = CURRENT_USER))`         |
/// | `WHEN (NEW.v > 0)`                  | `WHEN ((new.v > 0))`               |
///
/// So a function is its schema, name and canonicalised argument vector; a policy is
/// its table, command, roles and permissive flag; a trigger is its table, timing
/// and event set. Bodies, `USING`/`WITH CHECK` predicates and `WHEN` clauses are
/// not collected at all - not collected rather than collected-and-ignored, so no
/// later change can start comparing them by accident.
///
/// **THREE EXCLUSIONS, each a false-drift source rather than a nicety.**
///  * `prokind IN ('f','p')` drops aggregates and window functions, which the IR
///    cannot author and which would therefore always read as unexpected.
///  * The `pg_depend` `deptype = 'e'` filter drops functions an EXTENSION owns.
///    Measured on PostgreSQL 18.4: `CREATE EXTENSION pgcrypto WITH SCHEMA s` puts
///    37 functions in `s`, and the filter removes all 37. Without it every one of
///    them would be reported as an unexpected function on the next drift run.
///  * `tgisinternal` drops the `RI_ConstraintTrigger_*` pair PostgreSQL creates for
///    every foreign key, and `relispartition = false` drops the copy it clones onto
///    each child of a partitioned table. Measured: a single `REFERENCES` produced
///    four internal triggers, and one trigger on a partitioned parent produced one
///    visible clone per child.
async fn snapshot_vendor_objects_pg<D: SqlSession>(
    conn: &D,
    schema: &str,
) -> Result<VendorObjectIdentities, DriftError> {
    let mut out = VendorObjectIdentities::default();

    let function_rows = conn
        .query(
            "SELECT p.proname AS name, \
                    COALESCE( \
                      (SELECT array_agg(pg_catalog.format_type(a.t, NULL) ORDER BY a.ord) \
                         FROM unnest(p.proargtypes) WITH ORDINALITY AS a(t, ord)), \
                      ARRAY[]::text[] \
                    ) AS arg_types, \
                    p.prosrc AS body, \
                    (p.prosqlbody IS NOT NULL) AS has_sql_body \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = $1 AND p.prokind IN ('f', 'p') \
               AND NOT EXISTS ( \
                     SELECT 1 FROM pg_catalog.pg_depend d \
                      WHERE d.classid = 'pg_catalog.pg_proc'::regclass \
                        AND d.objid = p.oid AND d.deptype = 'e') \
             ORDER BY p.proname",
            &[schema.into()],
        )
        .await?;
    for r in &function_rows {
        // `proargtypes` holds the IN/INOUT/VARIADIC types only - OUT arguments are
        // in `proallargtypes` and are excluded here for the same reason
        // `FunctionKey::from_create` filters them: they do not identify an overload.
        let arg_types: Vec<String> = r.try_get("arg_types").unwrap_or_default();
        // `prosrc` is the body AS WRITTEN for every language this DSL admits, which
        // is why it may be compared where a deparsed policy predicate may not - see
        // `comparable_function_body`. `has_sql_body` separates the one shape that
        // has no `prosrc` to read.
        let body: String = r.try_get("body").unwrap_or_default();
        let has_sql_body: bool = r.try_get("has_sql_body").unwrap_or(false);
        out.functions.insert(
            FunctionKey {
                schema: schema.to_string(),
                name: r.try_get("name")?,
                arg_types,
            }
            .canonicalized(),
            FunctionIdentity::from_catalog(&body, has_sql_body),
        );
    }

    let policy_rows = conn
        .query(
            "SELECT c.relname AS table_name, p.polname AS policy_name, \
                    p.polcmd::text AS cmd, p.polpermissive AS permissive, \
                    COALESCE( \
                      array_agg(r.rolname ORDER BY r.rolname) \
                        FILTER (WHERE r.rolname IS NOT NULL), \
                      ARRAY[]::text[] \
                    ) AS roles \
             FROM pg_catalog.pg_policy p \
             JOIN pg_catalog.pg_class c ON c.oid = p.polrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN LATERAL unnest(p.polroles) AS pr(roleoid) ON true \
             LEFT JOIN pg_catalog.pg_roles r ON r.oid = pr.roleoid \
             WHERE n.nspname = $1 \
             GROUP BY c.relname, p.polname, p.polcmd, p.polpermissive \
             ORDER BY c.relname, p.polname",
            &[schema.into()],
        )
        .await?;
    for r in &policy_rows {
        let table: String = r.try_get("table_name")?;
        let name: String = r.try_get("policy_name")?;
        let cmd: String = r.try_get("cmd")?;
        // A policy applying to PUBLIC stores role OID 0, which joins to no pg_roles
        // row and so drops out of the aggregate as an EMPTY list - the same shape an
        // authored `to: None` folds to.
        let to: Vec<String> = r.try_get("roles").unwrap_or_default();
        out.policies.insert(
            PolicyKey {
                schema: schema.to_string(),
                table,
                name: name.clone(),
            },
            PolicyIdentity {
                for_cmd: policy_cmd_from_polcmd(&cmd, &name)?,
                to,
                permissive: r.try_get("permissive").unwrap_or(true),
            },
        );
    }

    let trigger_rows = conn
        .query(
            "SELECT c.relname AS table_name, t.tgname AS trigger_name, \
                    t.tgtype::int AS tgtype \
             FROM pg_catalog.pg_trigger t \
             JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND NOT t.tgisinternal AND c.relispartition = false \
             ORDER BY c.relname, t.tgname",
            &[schema.into()],
        )
        .await?;
    for r in &trigger_rows {
        let tgtype: i32 = r.try_get("tgtype")?;
        out.triggers.insert(
            TriggerKey {
                schema: schema.to_string(),
                table: r.try_get("table_name")?,
                name: r.try_get("trigger_name")?,
            },
            TriggerIdentity {
                timing: trigger_timing_from_tgtype(tgtype),
                events: trigger_events_from_tgtype(tgtype),
            },
        );
    }

    Ok(out)
}

/// `pg_policy.polcmd` as the authored lexicon.
///
/// An unknown code is an ERROR, not a default. PostgreSQL defines exactly these
/// five; guessing `ALL` for a sixth would silently claim a policy is broader than
/// it is, which is the wrong direction for a security facet.
fn policy_cmd_from_polcmd(cmd: &str, policy: &str) -> Result<PolicyCmd, DriftError> {
    match cmd {
        "*" => Ok(PolicyCmd::All),
        "r" => Ok(PolicyCmd::Select),
        "a" => Ok(PolicyCmd::Insert),
        "w" => Ok(PolicyCmd::Update),
        "d" => Ok(PolicyCmd::Delete),
        other => Err(DriftError::Snapshot(format!(
            "unknown pg_policy.polcmd `{other}` on policy `{policy}`"
        ))),
    }
}

/// `pg_trigger.tgtype` bit 6 (`INSTEAD OF`) then bit 1 (`BEFORE`), else `AFTER`.
/// The bit values are PostgreSQL's `TRIGGER_TYPE_*` constants.
fn trigger_timing_from_tgtype(tgtype: i32) -> TriggerTiming {
    if tgtype & 0x40 != 0 {
        TriggerTiming::InsteadOf
    } else if tgtype & 0x02 != 0 {
        TriggerTiming::Before
    } else {
        TriggerTiming::After
    }
}

/// The `tgtype` event bits, in the one canonical order both sides normalise to.
fn trigger_events_from_tgtype(tgtype: i32) -> Vec<TriggerEvent> {
    let mut events = Vec::new();
    if tgtype & 0x04 != 0 {
        events.push(TriggerEvent::Insert);
    }
    if tgtype & 0x10 != 0 {
        events.push(TriggerEvent::Update);
    }
    if tgtype & 0x08 != 0 {
        events.push(TriggerEvent::Delete);
    }
    if tgtype & 0x20 != 0 {
        events.push(TriggerEvent::Truncate);
    }
    TriggerIdentity::sorted_events(events)
}

pub(crate) async fn snapshot_schema_for<D: SqlSession>(
    conn: &D,
    schema: &str,
    dialect: &DialectId,
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
            &[schema.into()],
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

    let mut table_rls: std::collections::BTreeMap<String, bool> = Default::default();
    // Base tables in the schema. `table_schema` is BOUND ($1), never interpolated.
    // Child partitions are modeled separately in `SchemaSnapshot::partitions`;
    // their columns/indexes/constraints are inherited/propagated catalog effects.
    let table_rows = conn
        .query(
            "SELECT c.relname AS table_name, obj_description(c.oid, 'pg_class') AS comment, \
             c.relrowsecurity AS rls_enabled \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') AND c.relispartition = false \
             ORDER BY c.relname",
            &[schema.into()],
        )
        .await?;
    for r in &table_rows {
        let name: String = r.try_get("table_name")?;
        // RLS is read from the pg_class row this loop already has - no extra query.
        // Recorded even when false so the map is complete for a schema, which is what
        // lets an out-of-band DISABLE be seen as a change rather than as an absence.
        table_rls.insert(name.clone(), r.try_get("rls_enabled").unwrap_or(false));
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
            &[schema.into()],
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
            &[schema.into()],
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
                // Introspection reads back rendered SQL, never the typed body an
                // author wrote, so a view discovered here carries no inverse and a
                // drop of it stays irreversible.
                authored_query: None,
                authored_schema: None,
                comparable_body: None,
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
            &[schema.into()],
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
            &[schema.into()],
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
            //
            // The same char is ALSO the structural drift key. It used to be read
            // only for the gate above, so a column that stopped being generated out
            // of band changed nothing this differ looked at - see
            // `comparable_generated_column`.
            let attgenerated: String = r.try_get("generated_kind")?;
            let is_generated = !attgenerated.is_empty();
            let generated_kind = Some(match attgenerated.as_str() {
                "s" => GeneratedKindSnapshot::Stored,
                "v" => GeneratedKindSnapshot::Virtual,
                // Any future `attgenerated` code is still GENERATED, and reporting it
                // as an ordinary column would be a false drift line rather than a
                // missing one. `Virtual` is the conservative read: it says "computed"
                // without claiming a storage contract this build does not model.
                "" => GeneratedKindSnapshot::NotGenerated,
                _ => GeneratedKindSnapshot::Virtual,
            });
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
                dialect,
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
                generated_kind,
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
                    ic.reloptions AS reloptions, \
                    (x.indexprs IS NOT NULL OR x.indpred IS NOT NULL) AS has_expr_site, \
                    ( \
                      SELECT coalesce(array_agg(DISTINCT att.attname ORDER BY att.attname), '{}') \
                      FROM pg_depend d \
                      JOIN pg_attribute att \
                        ON att.attrelid = d.refobjid AND att.attnum = d.refobjsubid \
                      WHERE d.classid = 'pg_class'::regclass \
                        AND d.objid = x.indexrelid \
                        AND d.refclassid = 'pg_class'::regclass \
                        AND d.refobjid = x.indrelid \
                        AND d.refobjsubid > 0 \
                    ) AS referenced_columns \
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
            &[schema.into()],
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
            let has_expr_site: bool = r.try_get("has_expr_site").unwrap_or(false);
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
                // The referenced-column set behind `indexprs` / `indpred`, recovered
                // STRUCTURALLY from `pg_depend` rather than by re-parsing the deparsed
                // SQL: PostgreSQL records one dependency row per attribute an index
                // reads, so `refobjsubid > 0` yields the names directly and a string
                // literal that merely spells a column contributes nothing. Measured on
                // PostgreSQL 18.4: `(id) WHERE (a > 0)` yields `{a,id}` and follows
                // `RENAME COLUMN a TO b` to `{b,id}`, while `(note) WHERE (note <> 'a')`
                // yields `{note}` alone.
                //
                // `Some` exactly when the index HAS an expression site, matching what
                // `create_index_snapshot` records offline, so the two producers agree on
                // when the field is absent. The catalog set is a SUPERSET of the offline
                // one - it also names the key and INCLUDE attributes, which the offline
                // producer leaves to the exact-name lists - which is why every consumer
                // unions it with those lists rather than reading it alone.
                expr_cascade_columns: has_expr_site
                    .then(|| r.try_get("referenced_columns").unwrap_or_default()),
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
            &[schema.into()],
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
                    recover_format_check(column_name, &catalog_definition, dialect)
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
                                dialect,
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
                                dialect,
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
            &[schema.into()],
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
            &[schema.into()],
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

    let vendor_objects = snapshot_vendor_objects_pg(conn, schema).await?;

    Ok(SchemaSnapshot {
        tables,
        table_rls,
        views,
        named_types,
        sequences,
        roles,
        schemas,
        extensions,
        // The AUTHORED definitions stay empty on a catalog read. They are rollback
        // history - `LiveSchema::from_catalog_snapshot` hands them to the lowering
        // seam, which reads the recorded body back to build a `down` - and no
        // catalog can return the pre-normalisation body an author wrote.
        functions: BTreeMap::new(),
        policies: BTreeMap::new(),
        triggers: BTreeMap::new(),
        // `Some` even when every map inside is empty: this side HAS looked, and an
        // empty result is the positive claim that the schema holds none.
        vendor_objects: Some(vendor_objects),
        partitions,
    })
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
    dialect: &DialectId,
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
            catalog_expression_fingerprint_in_dialect(expression, dialect)
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
            catalog_expression_fingerprint_in_dialect(expression, dialect)
        )));
    }
    Some(if data_type.eq_ignore_ascii_case("uuid") {
        catalog_uuid_id_default(Some(expression), dialect, None)
    } else {
        catalog_id_default(Some(expression), dialect, None)
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

/// Whether to store a live constraint's catalog text on the introspected snapshot.
///
/// Wider than [`constraint_definition_is_comparable`]: a `CHECK` body is retained
/// even though it is never compared, because it is read for diagnostics rather than
/// for equality. `EXCLUDE` stays empty, matching what the offline renderer emits for
/// it, so the two sides agree on the field being absent rather than unread.
fn constraint_definition_is_retained(kind: &str) -> bool {
    kind != "EXCLUDE"
}
