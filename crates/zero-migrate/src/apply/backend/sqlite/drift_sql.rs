//! SQLite live-schema introspection for drift.
//!
//! Produces the SAME dialect-agnostic [`SchemaSnapshot`](crate::model::snapshot::SchemaSnapshot)
//! the Postgres path returns, so [`check_checksum_drift`](crate::apply::drift::check_checksum_drift)
//! and [`diff_snapshots`](crate::apply::drift::diff_snapshots) work unchanged across both
//! dialects. The PG path reads `information_schema` + `pg_catalog`; this reads
//! `sqlite_master` + `PRAGMA table_info` / `PRAGMA index_list` / `PRAGMA index_info`
//! / `PRAGMA foreign_key_list` of the connection's `main` database (the app file).
//!
//! # Confinement
//!
//! Every read here runs under **`EngineJournal`** mode: the engine's OWN
//! introspection touches `sqlite_master` and issues `PRAGMA table_info(...)` etc.,
//! both of which the **CreatorUp** authorizer denies a creator from doing (PRAGMA
//! is denied outright in CreatorUp;). The introspection is read-only — it
//! emits no DDL and mutates nothing — but it MUST run in engine mode for the PRAGMA
//! reads to compile. A creator can never reach this code path (it is engine-private,
//! behind the `SqliteBackend`), so allowing these reads under engine mode does not
//! widen the creator surface.
//!
//! # What is excluded from the app-schema snapshot
//!
//! - SQLite internal tables (`sqlite_*`, incl. `sqlite_sequence` / `sqlite_stat*`).
//! - The `_mig` journal objects — they live in the ATTACHed `_mig` database, not
//! `main`, so a `main`-scoped `sqlite_master` read never sees them anyway; we
//! additionally scope every PRAGMA to `main`.
//!
//! # Sentinel recovery
//!
//! The SQLite emitter bakes the `/* zero-migrate:mask:… */` (and `/* zero-migrate:enc:… */`) sentinels
//! INLINE in the `CREATE` text, which `sqlite_master.sql` preserves verbatim (SQLite
//! keeps comments in the stored schema text, unlike PG which discards them at
//! parse). [`recover_inline_sentinel`] pulls the `zero-migrate:mask:` / `zero-migrate:enc:` body for a
//! given column out of that stored text into the snapshot's
//! [`comment_sentinel`](crate::model::snapshot::ColumnSnapshot::comment_sentinel), so a
//! masked/encrypted column round-trips faithfully rather than being silently
//! dropped to a plain column.

use std::collections::BTreeMap;

use crate::apply::drift::DriftError;
use crate::model::ir::{IdentityCol, IndexSortOrder};
use crate::model::snapshot::{
    ColumnCollationSnapshot, ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot,
    IndexSnapshot, SchemaSnapshot, TableSnapshot, ViewSnapshot,
};
use crate::render::value_format::{
    catalog_id_default, catalog_uuid_id_default, recover_format_check, RecoveredFormatCheck,
};
use crate::schema::query::SqlDialect;

use super::actor::{MigrationActor, SqliteActorError};
use super::authorizer::Mode;

/// One member column of a composite foreign key, as `PRAGMA foreign_key_list`
/// reports it: `(seq, from_column, to_column)` — `seq` orders the columns within
/// the FK, `from`/`to` are the local and referenced column names.
type ForeignKeyColumn = (i64, String, String);

/// One foreign key reconstructed from `PRAGMA foreign_key_list`. SQLite returns
/// one row per member column, so the rows are first grouped by id and then sorted
/// by `seq`. Actions are repeated on every member row; the first row is enough.
#[derive(Debug)]
struct PragmaForeignKey {
    referenced_table: String,
    columns: Vec<ForeignKeyColumn>,
    on_update: String,
    on_delete: String,
    match_kind: String,
}

/// Foreign keys grouped by their `PRAGMA foreign_key_list` `id` (a composite FK
/// spans several rows sharing one `id`).
type ForeignKeysById = BTreeMap<i64, PragmaForeignKey>;

/// Metadata SQLite omits from `PRAGMA foreign_key_list`, recovered from the
/// table's stored `CREATE TABLE` text. The ordered column tuples also let us
/// correlate a parsed clause with its authoritative PRAGMA group without relying
/// on SQLite's implementation-defined FK id ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedForeignKey {
    name: Option<String>,
    local_columns: Vec<String>,
    referenced_table: String,
    referenced_columns: Vec<String>,
    on_update: Option<String>,
    on_delete: Option<String>,
    match_kind: Option<String>,
    deferrable: bool,
    initially_deferred: bool,
}

/// Map a SQLite actor error onto the dialect-neutral `Backend` arm of [`DriftError`].
fn drift_err(e: SqliteActorError) -> DriftError {
    DriftError::Backend(e.to_string())
}

/// True iff `name` is a SQLite-internal object we must exclude from the app-schema
/// snapshot: anything prefixed `sqlite_` (`sqlite_sequence`, `sqlite_stat1`,
/// `sqlite_autoindex_*`, …). The `_mig` journal lives in a different database
/// (ATTACHed alias), so it never appears in a `main`-scoped read.
fn is_internal(name: &str) -> bool {
    name.starts_with("sqlite_")
}

/// Single-quote a SQL string literal (engine-controlled identifiers, quoted
/// defensively so a name with an apostrophe cannot break a PRAGMA/SELECT).
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Parse the FTS5 source-column list for a `sqlite_master` table row that is an
/// FTS5 virtual table, or `None` if the row is not one. `name` must end in `__fts`
/// (the engine's vtable-name contract) AND `sql` must be a `… USING fts5(…)`
/// create — both, so a creator table coincidentally named `x__fts` that is a plain
/// (non-fts5) table is NOT misread as an FTS index.
fn parse_fts5_index(name: &str, sql: &str) -> Option<Vec<String>> {
    if !name.ends_with("__fts") {
        return None;
    }
    crate::schema::fts_sqlite::parse_fts5_columns(sql)
}

/// The parent collection of an FTS5 vtable named `<coll>__fts` (strip the
/// `__fts` suffix). Caller guarantees the `__fts` suffix (via [`parse_fts5_index`]).
fn fts_parent_collection(vtable: &str) -> String {
    vtable.strip_suffix("__fts").unwrap_or(vtable).to_string()
}

/// True iff `name` is an FTS5 SHADOW table — one of the auxiliary tables FTS5
/// auto-creates for a vtable `<v>`: `<v>_data` / `<v>_idx` / `<v>_docsize` /
/// `<v>_config` / `<v>_content`. These must be excluded from the app-schema
/// snapshot (like the `sqlite_*` internals) so they do not read as drift. A shadow
/// is recognised iff stripping a known suffix yields a name that is itself an FTS5
/// vtable present in `raw_names` (so a creator table merely ending in `_data` is
/// not excluded).
fn is_fts5_shadow_table(name: &str, raw_names: &[String]) -> bool {
    const SUFFIXES: &[&str] = &["_data", "_idx", "_docsize", "_config", "_content"];
    for suf in SUFFIXES {
        if let Some(base) = name.strip_suffix(suf) {
            if base.ends_with("__fts") && raw_names.iter().any(|n| n == base) {
                return true;
            }
        }
    }
    false
}

/// Introspect the LIVE structure of the connection's `main` database (the tenant
/// app file) into a [`SchemaSnapshot`], the same shape the PG path returns.
///
/// Read-only; runs under engine mode (the PRAGMA reads require it). The
/// result map is a `BTreeMap` and every child vector is name-sorted, so the
/// snapshot is byte-stable regardless of catalog scan order — matching the PG
/// snapshot's determinism contract.
///
/// # Errors
/// [`DriftError::Backend`] on a `sqlite_master` / PRAGMA read failure.
pub(crate) async fn snapshot_schema(actor: &MigrationActor) -> Result<SchemaSnapshot, DriftError> {
    // Engine mode: the introspection reads sqlite_master + issues PRAGMAs, both of
    // which CreatorUp denies. Read-only (no DDL, no writes).
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(drift_err)?;

    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    let mut views: BTreeMap<String, ViewSnapshot> = BTreeMap::new();

    // Base tables of `main`, with their stored CREATE text (carries inline
    // sentinels). `type='table'` excludes views/indexes/triggers; the
    // `name NOT LIKE 'sqlite_%'` and the `is_internal` guard exclude SQLite
    // internals. `main.sqlite_master` scopes the read to the app file — never `_mig`.
    // NOTE: we deliberately do NOT use `NOT LIKE 'sqlite_%'` here — the hardened
    // authorizer's function allowlist does not include `LIKE`, so the
    // engine's own introspection must avoid it. The `is_internal` Rust-side guard
    // below filters `sqlite_*` instead (same effect, no LIKE function call).
    let table_rows = actor
        .query(
            "SELECT name, sql FROM main.sqlite_master \
             WHERE type = 'table' \
             ORDER BY name",
        )
        .await
        .map_err(drift_err)?;

    // (table_name -> stored CREATE sql), for inline sentinel recovery per column.
    let mut create_sql: BTreeMap<String, String> = BTreeMap::new();
    // **FTS** — the FTS5 virtual tables found in this pass, as
    // `(parent_collection, IndexSnapshot)`. They are NOT base tables (they are the
    // FTS *index*), so they are NOT inserted into `tables` directly; instead each is
    // attached as an `IndexSnapshot` to its PARENT collection after the base-table
    // loop, matching what `desired_snapshot_for_dialect` (SQLite) produces — so a
    // re-diff of an unchanged FTS schema is ZERO-drift. The FTS5 *shadow* tables
    // (`<vtable>_data`/`_idx`/`_docsize`/`_config`/`_content`) are excluded from the
    // snapshot entirely (like the `sqlite_*` internals) so they never read as drift.
    let mut fts_indexes: Vec<(String, IndexSnapshot)> = Vec::new();
    // First pass over the raw table rows: classify each as an FTS5 vtable (parse its
    // `fts5(...)` column list), an FTS5 shadow table (skip), or a real base table.
    let raw_names: Vec<String> = table_rows
        .iter()
        .filter_map(|r| cell(r, 0).ok())
        .filter(|n| !is_internal(n))
        .collect();
    for r in &table_rows {
        let name = cell(r, 0)?;
        if is_internal(&name) {
            continue;
        }
        let stored_sql_opt = if let Some(Some(sql)) = r.get(1) {
            Some(sql.clone())
        } else {
            None
        };
        // An FTS5 virtual table: `sql` starts `CREATE VIRTUAL TABLE … USING fts5`.
        // Convert it to an FTS5 IndexSnapshot on its parent collection and DROP it
        // from the base-table set.
        if let Some(sql) = &stored_sql_opt {
            if let Some(columns) = parse_fts5_index(&name, sql) {
                let parent = fts_parent_collection(&name);
                fts_indexes.push((
                    parent,
                    IndexSnapshot {
                        name: name.clone(),
                        unique: false,
                        elements: columns
                            .iter()
                            .cloned()
                            .map(IndexElementSnapshot::column)
                            .collect(),
                        columns,
                        access_method: "fts5".to_string(),
                        predicate: None,
                        include: Vec::new(),
                        with: None,
                        only: false,
                        opclass: None,
                        nulls_not_distinct: false,
                        comment: None,
                    },
                ));
                continue;
            }
        }
        // An FTS5 SHADOW table (`<vtable>_data` etc.) — exclude from the snapshot. A
        // shadow's parent vtable is some `<x>__fts` that exists among the raw names.
        if is_fts5_shadow_table(&name, &raw_names) {
            continue;
        }
        if let Some(sql) = &stored_sql_opt {
            create_sql.insert(name.clone(), sql.clone());
        }
        tables.insert(
            name,
            TableSnapshot {
                columns: Vec::new(),
                indexes: Vec::new(),
                constraints: Vec::new(),
                runtime_options: Default::default(),
                partition_by: None,
                comment: None,
                // carry the verbatim CREATE text so the DROP-COLUMN rebuild
                // router can detect CHECK / generated / partial-index references the
                // structured PRAGMA buckets do not surface.
                stored_create_sql: stored_sql_opt,
            },
        );
    }

    let view_rows = actor
        .query(
            "SELECT name, sql FROM main.sqlite_master \
             WHERE type = 'view' \
             ORDER BY name",
        )
        .await
        .map_err(drift_err)?;
    for r in &view_rows {
        let name = cell(r, 0)?;
        if is_internal(&name) {
            continue;
        }
        let definition = if let Some(Some(sql)) = r.get(1) {
            Some(sql.clone())
        } else {
            None
        };
        views.insert(
            name,
            ViewSnapshot {
                materialized: false,
                columns: None,
                definition,
                comment: None,
            },
        );
    }

    // Per-table: columns (PRAGMA table_info), indexes (PRAGMA index_list / index_info),
    // and constraints synthesised from PRAGMA foreign_key_list + the PK / UNIQUE
    // index metadata.
    let table_names: Vec<String> = tables.keys().cloned().collect();
    for table in table_names {
        let stored = create_sql.get(&table).map_or("", String::as_str);
        introspect_columns(actor, &table, stored, &mut tables).await?;
        introspect_indexes_and_unique(actor, &table, &mut tables).await?;
        introspect_foreign_keys(actor, &table, stored, &mut tables).await?;
    }

    // **FTS** — attach each recognised FTS5 vtable as an `IndexSnapshot` on its
    // PARENT collection (now that the base tables are populated). If the parent is
    // somehow absent (an orphaned FTS index — should never happen, the engine emits
    // them together), the index is dropped silently rather than synthesising a
    // phantom table.
    for (parent, idx) in fts_indexes {
        if let Some(t) = tables.get_mut(&parent) {
            t.indexes.push(idx);
        }
    }

    // Deterministic ordering: sort every child vector by name (the PG path sorts via
    // `ORDER BY` in SQL; SQLite PRAGMAs return in declaration/ordinal order, so we
    // sort here to match the snapshot equality contract).
    for t in tables.values_mut() {
        t.columns.sort_by(|a, b| a.name.cmp(&b.name));
        t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        t.constraints.sort_by(|a, b| a.name.cmp(&b.name));
    }

    Ok(SchemaSnapshot {
        tables,
        views,
        ..Default::default()
    })
}

/// Columns via `PRAGMA table_info(<t>)`. Columns: cid, name, type, notnull,
/// dflt_value, pk. We map `type` → `data_type` (normalised lowercase, the SQLite
/// declared-type spelling), `notnull == 0` → nullable, and recover the inline
/// `zero-migrate:mask:` / `zero-migrate:enc:` sentinel for the column from the stored CREATE text.
async fn introspect_columns(
    actor: &MigrationActor,
    table: &str,
    stored_create_sql: &str,
    tables: &mut BTreeMap<String, TableSnapshot>,
) -> Result<(), DriftError> {
    let rows = actor
        .query(&format!("PRAGMA main.table_info({})", lit(table)))
        .await
        .map_err(drift_err)?;
    // A sole exact `INTEGER PRIMARY KEY` is a rowid alias unless SQLite also
    // materialized a real primary-key index. The latter distinguishes both
    // WITHOUT ROWID tables and the historical inline `PRIMARY KEY DESC` form.
    let primary_key_has_separate_index = actor
        .query(&format!("PRAGMA main.index_list({})", lit(table)))
        .await
        .map_err(drift_err)?
        .iter()
        .any(|row| {
            row.get(3)
                .and_then(Clone::clone)
                .is_some_and(|origin| origin.eq_ignore_ascii_case("pk"))
        });

    let primary_members = rows
        .iter()
        .filter_map(|row| {
            let ordinal = sqlite_integer_cell(row, 5);
            (ordinal > 0).then_some(ordinal)
        })
        .count();
    let without_rowid =
        crate::render::declarative::sqlite_create_is_without_rowid(stored_create_sql);
    let Some(t) = tables.get_mut(table) else {
        return Ok(());
    };
    // Collect PK member columns (table_info `pk` > 0), ordered by the pk ordinal, so
    // we can synthesise the PRIMARY KEY constraint here — `index_list` does NOT
    // report an auto-index for a rowid PK (a single `INTEGER PRIMARY KEY`), so the
    // index-bucket PK detection misses it; table_info is the authoritative source.
    let mut pk_members: Vec<(i64, String)> = Vec::new();
    for r in &rows {
        // table_info columns: 0=cid 1=name 2=type 3=notnull 4=dflt_value 5=pk
        let name = cell(r, 1)?;
        let raw_type = r.get(2).and_then(Clone::clone).unwrap_or_default();
        let notnull = r.get(3).and_then(Clone::clone).unwrap_or_default();
        let raw_default = r.get(4).and_then(Clone::clone);
        let pk_ord = sqlite_integer_cell(r, 5);
        if pk_ord > 0 {
            pk_members.push((pk_ord, name.clone()));
        }
        // Keep the declared type spelling below rather than reducing it to
        // affinity. Reference compatibility uses this catalog evidence to keep
        // an unmanaged INTEGER key distinct from an unmanaged BIGINT key even
        // though both have SQLite INTEGER affinity.
        //
        // A PRIMARY KEY column is NOT NULL in the engine's model — the desired
        // snapshot stamps `id TEXT PRIMARY KEY` as `nullable: false`, and Postgres
        // makes every PK column NOT NULL. But SQLite has a long-standing quirk: a
        // non-`INTEGER` PRIMARY KEY (e.g. our `id TEXT PRIMARY KEY`) is NOT
        // implicitly NOT NULL, so `PRAGMA table_info` reports `notnull=0` for it.
        // Taking that literally would falsely flag a `nullable true → false` drift on
        // the `id` column of every table on the SQLite leg. Treat a PK member as NOT
        // NULL so the introspected nullability agrees with the dialect-agnostic model
        // (and with the PG snapshot).
        let nullable = notnull.trim() != "1" && pk_ord == 0;
        let sqlite_rowid = primary_members == 1
            && pk_ord == 1
            && raw_type.trim().eq_ignore_ascii_case("INTEGER")
            && !without_rowid
            && !primary_key_has_separate_index;
        let identity = (sqlite_rowid && column_declares_autoincrement(stored_create_sql, &name))
            .then_some(IdentityCol { always: false });
        let recovered_checks = recover_column_format_checks(stored_create_sql, &name);
        if recovered_checks.mixed_uuid_and_value_format {
            return Err(DriftError::Snapshot(format!(
                "SQLite stored CREATE returned mixed UUID and TypeID/ULID format CHECKs for {table}.{name}"
            )));
        }
        let value_format = recovered_checks.value_format;
        let has_uuid_format_check = recovered_checks.uuid;
        let catalog_default = if has_uuid_format_check {
            catalog_uuid_id_default(raw_default.as_deref(), SqlDialect::Sqlite, None)
        } else {
            catalog_id_default(raw_default.as_deref(), SqlDialect::Sqlite, None)
        };
        let is_uuid_v4_default = matches!(
            catalog_default,
            crate::model::snapshot::IdDefaultSnapshot::UuidV4
        );
        // Defaults remain emission-only in `default`, but ID-bearing defaults
        // have a narrow semantic drift key. Recognize the exact engine UUIDv4
        // expression even when its CHECK was dropped so an out-of-band generator
        // addition/removal is still visible. An unknown default is compared only
        // when another catalog facet proves that the column carries an ID contract.
        let tracks_id_default = is_uuid_v4_default
            || identity.is_some()
            || sqlite_rowid
            || has_uuid_format_check
            || value_format.is_some();
        let id_default = tracks_id_default.then_some(catalog_default);
        t.columns.push(ColumnSnapshot {
            name: name.clone(),
            // Normalise the declared type to a lowercase spelling so it is a stable
            // drift attribute (SQLite type affinity is case-insensitive and free-
            // form; the engine's emitter spells these consistently).
            data_type: raw_type.trim().to_ascii_lowercase(),
            nullable,
            // Emission-only on the PG path; the same here. We DO recover the inline
            // mask/encryption sentinel into `comment_sentinel` so an encrypted
            // / masked column round-trips faithfully rather than dropping silently.
            // Retain the raw catalog spelling for expected-driven ID-default
            // comparison. Typed references intentionally have no local format
            // CHECK, so live introspection cannot independently mark their
            // default as ID-bearing; `diff_snapshots` classifies this raw value
            // against the authored semantic key instead. Ordinary defaults are
            // still excluded from column equality and remain emission-only.
            default: raw_default,
            generated: None,
            identity,
            sqlite_rowid,
            value_format,
            id_default,
            mysql_default_generated: None,
            encryption_sentinel: None,
            ddl_type_override: None,
            inline_checks: Vec::new(),
            comment_sentinel: recover_inline_sentinel(stored_create_sql, &name),
            case_sensitive: recover_case_sensitive(stored_create_sql, &name),
            collation: recover_column_collation(stored_create_sql, &name),
            mysql_text_storage: None,
            comment: None,
        });
    }
    // Synthesise the PRIMARY KEY constraint from table_info (matching the PG
    // snapshot's constraint bucket). A WITHOUT-ROWID / composite PK is also reported
    // here. `index_list` PK auto-indexes are skipped in
    // `introspect_indexes_and_unique` to avoid a duplicate.
    if !pk_members.is_empty() {
        pk_members.sort_by_key(|(ord, _)| *ord);
        let cols: Vec<String> = pk_members.into_iter().map(|(_, n)| n).collect();
        t.constraints.push(ConstraintSnapshot {
            name: format!("pk_{table}"),
            kind: "PRIMARY KEY".to_string(),
            definition: format!("PRIMARY KEY ({})", cols.join(", ")),
            comment: None,
        });
    }
    Ok(())
}

/// Indexes via `PRAGMA index_list(<t>)` + `PRAGMA index_info(<idx>)`, plus the
/// UNIQUE / PRIMARY KEY index → [`ConstraintSnapshot`] synthesis.
///
/// `index_list` columns: seq, name, unique, origin, partial. `origin` is `c`
/// (CREATE INDEX), `u` (a UNIQUE constraint's auto-index), or `pk` (the PRIMARY KEY
/// index). We:
/// - record every index as an [`IndexSnapshot`] (its key columns from
/// index_info), EXCLUDING SQLite auto-indexes named `sqlite_autoindex_*` from
/// the *index* bucket (they are constraint artifacts, surfaced as constraints);
/// - synthesise a `UNIQUE` / `PRIMARY KEY` [`ConstraintSnapshot`] for `origin`
/// `u` / `pk` so a unique/PK constraint round-trips against a PG snapshot's
/// constraint bucket.
async fn introspect_indexes_and_unique(
    actor: &MigrationActor,
    table: &str,
    tables: &mut BTreeMap<String, TableSnapshot>,
) -> Result<(), DriftError> {
    let idx_rows = actor
        .query(&format!("PRAGMA main.index_list({})", lit(table)))
        .await
        .map_err(drift_err)?;

    struct GatheredIndex {
        name: String,
        unique: bool,
        origin: String,
        columns: Vec<String>,
        elements: Vec<IndexElementSnapshot>,
        predicate: Option<String>,
    }

    let mut gathered: Vec<GatheredIndex> = Vec::new();
    for r in &idx_rows {
        // index_list columns: 0=seq 1=name 2=unique 3=origin 4=partial
        let name = cell(r, 1)?;
        let unique = r.get(2).and_then(Clone::clone).unwrap_or_default().trim() == "1";
        let origin = r.get(3).and_then(Clone::clone).unwrap_or_default();
        // Key columns of this index, in order.
        let info = actor
            .query(&format!("PRAGMA main.index_info({})", lit(&name)))
            .await
            .map_err(drift_err)?;
        let mut columns: Vec<String> = Vec::new();
        for ir in &info {
            // index_info columns: 0=seqno 1=cid 2=name (NULL for an expression key)
            if let Some(Some(col)) = ir.get(2) {
                columns.push(col.clone());
            }
        }
        let sql_rows = actor
            .query(&format!(
                "SELECT sql FROM main.sqlite_master WHERE type = 'index' AND name = {}",
                lit(&name)
            ))
            .await
            .map_err(drift_err)?;
        let create_sql = sql_rows
            .first()
            .and_then(|r| r.first())
            .and_then(Clone::clone);
        let (elements, predicate) = create_sql
            .as_deref()
            .and_then(parse_sqlite_index_shape)
            .unwrap_or_else(|| {
                (
                    columns
                        .iter()
                        .cloned()
                        .map(IndexElementSnapshot::column)
                        .collect(),
                    None,
                )
            });
        gathered.push(GatheredIndex {
            name,
            unique,
            origin: origin.trim().to_string(),
            columns,
            elements,
            predicate,
        });
    }

    let Some(t) = tables.get_mut(table) else {
        return Ok(());
    };
    for index in gathered {
        // A PRIMARY KEY auto-index (origin 'pk') is already surfaced as a constraint
        // from `table_info` (the authoritative PK source, which also covers the rowid
        // PK that has no index_list entry). Skip it here entirely so the PK is not
        // double-counted and the `sqlite_autoindex_*` name never leaks.
        if index.origin == "pk" {
            continue;
        }
        // A UNIQUE-constraint auto-index (origin 'u') is a CONSTRAINT, surfaced in the
        // constraint bucket (matching the PG snapshot, where UNIQUE is a
        // pg_constraint row, not a pg_index row in the index bucket). Its name is the
        // SQLite-internal `sqlite_autoindex_*`, which we must not leak as a creator
        // index.
        if index.origin == "u" {
            t.constraints.push(ConstraintSnapshot {
                name: index.name.clone(),
                kind: "UNIQUE".to_string(),
                // A faithful, re-parse-stable definition shape: the constraint kind
                // over its ordered key columns. (SQLite has no `pg_get_constraintdef`;
                // this canonical spelling is the closest faithful round-trip form.)
                definition: format!("UNIQUE ({})", index.columns.join(", ")),
                comment: None,
            });
            // Do NOT also push it into the index bucket if it is a SQLite auto-index;
            // an explicit `CREATE UNIQUE INDEX` (origin 'c') is handled below.
            if index.name.starts_with("sqlite_autoindex_") {
                continue;
            }
        }
        // A real index (CREATE INDEX, origin 'c') — and an explicitly-named UNIQUE
        // index that is not a sqlite_autoindex — is an IndexSnapshot.
        if is_internal(&index.name) {
            continue;
        }
        t.indexes.push(IndexSnapshot {
            name: index.name,
            unique: index.unique,
            elements: index.elements,
            columns: index.columns,
            // SQLite indexes are b-tree (the only built-in index AM). fts5/vec0 are
            // vtables, not indexes, and surface as tables; see the divergences doc.
            access_method: "btree".to_string(),
            predicate: index.predicate,
            include: Vec::new(),
            with: None,
            only: false,
            opclass: None,
            nulls_not_distinct: false,
            comment: None,
        });
    }
    Ok(())
}

fn parse_sqlite_index_shape(sql: &str) -> Option<(Vec<IndexElementSnapshot>, Option<String>)> {
    let lower = sql.to_ascii_lowercase();
    let on = lower.find(" on ")?;
    let open = find_char_outside_quotes(sql, '(', on + 4)?;
    let close = find_matching_paren(sql, open)?;
    let inner = &sql[open + 1..close];
    let elements = split_top_level_commas(inner)
        .into_iter()
        .filter_map(|part| parse_sqlite_index_element(part.trim()))
        .collect::<Vec<_>>();
    let tail = sql[close + 1..].trim();
    let predicate = tail
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("where"))
        .map(|_| tail[5..].trim().to_string())
        .filter(|s| !s.is_empty());
    Some((elements, predicate))
}

fn parse_sqlite_index_element(part: &str) -> Option<IndexElementSnapshot> {
    let part = part.trim();
    if part.is_empty() {
        return None;
    }
    let (part, order) = split_sqlite_index_sort_order(part);
    if let Some(inner) = strip_single_outer_parens(part) {
        return Some(IndexElementSnapshot::expr(inner.trim().to_string()));
    }
    let name = parse_sqlite_quoted_ident(part).unwrap_or_else(|| part.to_string());
    Some(match order {
        Some(order) => IndexElementSnapshot::column_ordered(name, order),
        None => IndexElementSnapshot::column(name),
    })
}

fn split_sqlite_index_sort_order(part: &str) -> (&str, Option<IndexSortOrder>) {
    let trimmed = part.trim_end();
    let Some(idx) = trimmed.rfind(char::is_whitespace) else {
        return (trimmed, None);
    };
    let (head, tail) = trimmed.split_at(idx);
    let dir = tail.trim();
    if dir.eq_ignore_ascii_case("desc") {
        (head.trim_end(), Some(IndexSortOrder::Desc))
    } else if dir.eq_ignore_ascii_case("asc") {
        (head.trim_end(), Some(IndexSortOrder::Asc))
    } else {
        (trimmed, None)
    }
}

fn parse_sqlite_quoted_ident(s: &str) -> Option<String> {
    let mut chars = s.chars();
    if chars.next()? != '"' {
        return None;
    }
    let mut out = String::new();
    let mut rest = chars.peekable();
    while let Some(c) = rest.next() {
        if c == '"' {
            if matches!(rest.peek(), Some('"')) {
                rest.next();
                out.push('"');
                continue;
            }
            if rest.next().is_none() {
                return Some(out);
            }
            return None;
        }
        out.push(c);
    }
    None
}

fn strip_single_outer_parens(s: &str) -> Option<&str> {
    if !s.starts_with('(') {
        return None;
    }
    let close = find_matching_paren(s, 0)?;
    if close == s.len() - 1 {
        Some(&s[1..close])
    } else {
        None
    }
}

#[derive(Clone, Copy)]
enum SqliteDdlScanState {
    Plain,
    Quoted(char),
    BracketQuoted,
    LineComment,
    BlockComment,
}

/// Visit SQLite DDL characters that are outside quoted strings/identifiers and
/// comments. Scanning always starts at the beginning so `from` may safely point
/// into text whose lexical context began earlier.
fn scan_sqlite_ddl_outside(
    s: &str,
    from: usize,
    mut visit: impl FnMut(usize, char) -> bool,
) -> Option<usize> {
    let mut state = SqliteDdlScanState::Plain;
    let mut chars = s.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        match state {
            SqliteDdlScanState::Plain => match ch {
                '\'' | '"' | '`' => state = SqliteDdlScanState::Quoted(ch),
                '[' => state = SqliteDdlScanState::BracketQuoted,
                '-' if matches!(chars.peek(), Some((_, '-'))) => {
                    chars.next();
                    state = SqliteDdlScanState::LineComment;
                }
                '/' if matches!(chars.peek(), Some((_, '*'))) => {
                    chars.next();
                    state = SqliteDdlScanState::BlockComment;
                }
                _ if i >= from && visit(i, ch) => return Some(i),
                _ => {}
            },
            SqliteDdlScanState::Quoted(delimiter) => {
                if ch == delimiter {
                    if matches!(chars.peek(), Some((_, next)) if *next == delimiter) {
                        chars.next();
                    } else {
                        state = SqliteDdlScanState::Plain;
                    }
                }
            }
            SqliteDdlScanState::BracketQuoted => {
                if ch == ']' {
                    if matches!(chars.peek(), Some((_, ']'))) {
                        chars.next();
                    } else {
                        state = SqliteDdlScanState::Plain;
                    }
                }
            }
            SqliteDdlScanState::LineComment => {
                if matches!(ch, '\n' | '\r') {
                    state = SqliteDdlScanState::Plain;
                }
            }
            SqliteDdlScanState::BlockComment => {
                if ch == '*' && matches!(chars.peek(), Some((_, '/'))) {
                    chars.next();
                    state = SqliteDdlScanState::Plain;
                }
            }
        }
    }
    None
}

fn find_char_outside_quotes(s: &str, needle: char, from: usize) -> Option<usize> {
    scan_sqlite_ddl_outside(s, from, |_, ch| ch == needle)
}

fn find_matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    scan_sqlite_ddl_outside(s, open, |_, ch| match ch {
        '(' => {
            depth += 1;
            false
        }
        ')' if depth > 0 => {
            depth -= 1;
            depth == 0
        }
        _ => false,
    })
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0_usize;
    let mut depth = 0_usize;
    let _ = scan_sqlite_ddl_outside(s, 0, |i, ch| {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
            }
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        false
    });
    out.push(&s[start..]);
    out
}

/// Foreign keys via `PRAGMA foreign_key_list(<t>)` → one `FOREIGN KEY`
/// [`ConstraintSnapshot`] per declared FK. PRAGMA is authoritative for the
/// ordered local/referenced tuples and referential actions. It does not expose a
/// constraint's declared name or deferrability, so those are correlated from the
/// verbatim `sqlite_master.sql` `CREATE TABLE` text. Unnamed/unparseable foreign
/// keys retain a deterministic synthetic-name fallback.
///
/// `foreign_key_list` columns: id, seq, table, from, to, on_update, on_delete,
/// match. Multiple rows with the same `id` are the columns of one composite FK.
async fn introspect_foreign_keys(
    actor: &MigrationActor,
    table: &str,
    stored_create_sql: &str,
    tables: &mut BTreeMap<String, TableSnapshot>,
) -> Result<(), DriftError> {
    let rows = actor
        .query(&format!("PRAGMA main.foreign_key_list({})", lit(table)))
        .await
        .map_err(drift_err)?;

    // Group by FK id (a composite FK spans several rows). Preserve column order by
    // `seq`.
    let mut by_id: ForeignKeysById = BTreeMap::new();
    for r in &rows {
        // 0=id 1=seq 2=table 3=from 4=to 5=on_update 6=on_delete 7=match
        let id: i64 = r
            .first()
            .and_then(Clone::clone)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_default();
        let seq: i64 = r
            .get(1)
            .and_then(Clone::clone)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_default();
        let ref_table = r.get(2).and_then(Clone::clone).unwrap_or_default();
        let from = r.get(3).and_then(Clone::clone).unwrap_or_default();
        let to = r.get(4).and_then(Clone::clone).unwrap_or_default();
        let on_update = r
            .get(5)
            .and_then(Clone::clone)
            .unwrap_or_else(|| "NO ACTION".to_string());
        let on_delete = r
            .get(6)
            .and_then(Clone::clone)
            .unwrap_or_else(|| "NO ACTION".to_string());
        let match_kind = r
            .get(7)
            .and_then(Clone::clone)
            .unwrap_or_else(|| "NONE".to_string());
        let entry = by_id.entry(id).or_insert_with(|| PragmaForeignKey {
            referenced_table: ref_table,
            columns: Vec::new(),
            on_update,
            on_delete,
            match_kind,
        });
        entry.columns.push((seq, from, to));
    }

    let parsed = parse_table_foreign_keys(stored_create_sql);
    let mut parsed_used = vec![false; parsed.len()];
    let Some(t) = tables.get_mut(table) else {
        return Ok(());
    };
    for (id, mut pragma_fk) in by_id {
        pragma_fk.columns.sort_by_key(|(seq, _, _)| *seq);
        let from_cols: Vec<String> = pragma_fk
            .columns
            .iter()
            .map(|(_, from, _)| from.clone())
            .collect();
        let to_cols: Vec<String> = pragma_fk
            .columns
            .iter()
            .map(|(_, _, to)| to.clone())
            .collect();

        // Prefer a full structural + action match (which disambiguates duplicate
        // tuples with different policies), then fall back to tuple-only matching.
        // SQLite reports every MATCH spelling as NONE on versions that implement
        // only MATCH SIMPLE, so MATCH is deliberately not part of correlation.
        let parsed_idx = parsed
            .iter()
            .enumerate()
            .find(|(idx, candidate)| {
                !parsed_used[*idx]
                    && parsed_fk_matches(candidate, &pragma_fk, &from_cols, &to_cols, true)
            })
            .or_else(|| {
                parsed.iter().enumerate().find(|(idx, candidate)| {
                    !parsed_used[*idx]
                        && parsed_fk_matches(candidate, &pragma_fk, &from_cols, &to_cols, false)
                })
            })
            .map(|(idx, _)| idx);
        let parsed_fk = parsed_idx.map(|idx| {
            parsed_used[idx] = true;
            &parsed[idx]
        });

        // Engine-authored SQLite tables always carry explicit names in their
        // stored table-level clauses. Keep the synthetic fallback for unmanaged
        // unnamed/column-level REFERENCES clauses.
        let name = parsed_fk
            .and_then(|foreign_key| foreign_key.name.clone())
            .unwrap_or_else(|| format!("fk_{table}_{}_{id}", from_cols.join("_")));
        t.constraints.push(ConstraintSnapshot {
            name,
            kind: "FOREIGN KEY".to_string(),
            definition: canonical_foreign_key_definition(
                &pragma_fk, &from_cols, &to_cols, parsed_fk,
            ),
            comment: None,
        });
    }
    Ok(())
}

fn parsed_fk_matches(
    parsed: &ParsedForeignKey,
    pragma_fk: &PragmaForeignKey,
    local_columns: &[String],
    referenced_columns: &[String],
    include_actions: bool,
) -> bool {
    identifiers_equal(&parsed.referenced_table, &pragma_fk.referenced_table)
        && identifier_lists_equal(&parsed.local_columns, local_columns)
        && identifier_lists_equal(&parsed.referenced_columns, referenced_columns)
        && (!include_actions
            || (fk_actions_equal(parsed.on_update.as_deref(), &pragma_fk.on_update)
                && fk_actions_equal(parsed.on_delete.as_deref(), &pragma_fk.on_delete)))
}

fn identifiers_equal(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn identifier_lists_equal(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| identifiers_equal(left, right))
}

fn fk_actions_equal(parsed: Option<&str>, pragma: &str) -> bool {
    use crate::schema::query::{normalize_fk_action_for_dialect, SqlDialect};

    normalize_fk_action_for_dialect(parsed, SqlDialect::Sqlite)
        == normalize_fk_action_for_dialect(Some(pragma), SqlDialect::Sqlite)
}

fn canonical_foreign_key_definition(
    pragma_fk: &PragmaForeignKey,
    local_columns: &[String],
    referenced_columns: &[String],
    parsed: Option<&ParsedForeignKey>,
) -> String {
    use std::fmt::Write as _;

    use crate::render::declarative::{constraintdef_cols, quote_ident_if_needed};
    use crate::schema::query::{normalize_fk_action_for_dialect, SqlDialect};

    let mut definition = format!(
        "FOREIGN KEY ({}) REFERENCES {}({})",
        constraintdef_cols(local_columns),
        quote_ident_if_needed(&pragma_fk.referenced_table),
        constraintdef_cols(referenced_columns),
    );

    // MATCH SIMPLE is SQLite's only portable/enforced null contract. SQLite's
    // PRAGMA commonly spells the default as NONE, while sqlite_master may retain
    // an explicit MATCH SIMPLE; both canonicalize to omission, matching desired
    // snapshots. Preserve unsupported non-simple spellings only to make them
    // visible as drift rather than silently claiming equivalent enforcement.
    let match_kind = parsed
        .and_then(|foreign_key| foreign_key.match_kind.as_deref())
        .unwrap_or(&pragma_fk.match_kind);
    if !matches!(
        match_kind.trim().to_ascii_uppercase().as_str(),
        "" | "NONE" | "SIMPLE"
    ) {
        let _ = write!(
            definition,
            " MATCH {}",
            match_kind.trim().to_ascii_uppercase()
        );
    }

    let on_update = normalize_fk_action_for_dialect(Some(&pragma_fk.on_update), SqlDialect::Sqlite);
    let on_delete = normalize_fk_action_for_dialect(Some(&pragma_fk.on_delete), SqlDialect::Sqlite);
    if on_update != "NO ACTION" {
        let _ = write!(definition, " ON UPDATE {on_update}");
    }
    if on_delete != "NO ACTION" {
        let _ = write!(definition, " ON DELETE {on_delete}");
    }
    if parsed.is_some_and(|foreign_key| foreign_key.deferrable) {
        definition.push_str(" DEFERRABLE");
        if parsed.is_some_and(|foreign_key| foreign_key.initially_deferred) {
            definition.push_str(" INITIALLY DEFERRED");
        }
    }
    definition
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SqliteDdlToken {
    Word(String),
    OpenParen,
    CloseParen,
    Comma,
    Dot,
}

/// Parse table-level foreign-key clauses from a stored SQLite `CREATE TABLE`.
/// This is intentionally a small DDL tokenizer rather than a regex: quoted
/// identifiers, comments, nested parentheses, and arbitrary whitespace are all
/// legal in the stored SQL. Column-level REFERENCES are left to the deterministic
/// PRAGMA fallback because they have no explicit constraint name to recover.
fn parse_table_foreign_keys(create_sql: &str) -> Vec<ParsedForeignKey> {
    let Some(open) = find_char_outside_quotes(create_sql, '(', 0) else {
        return Vec::new();
    };
    let Some(close) = find_matching_paren(create_sql, open) else {
        return Vec::new();
    };
    split_top_level_commas(&create_sql[open + 1..close])
        .into_iter()
        .filter_map(parse_table_foreign_key_clause)
        .collect()
}

fn parse_table_foreign_key_clause(clause: &str) -> Option<ParsedForeignKey> {
    let tokens = tokenize_sqlite_ddl(clause);
    let mut cursor = 0_usize;
    let name = if token_is_keyword(&tokens, cursor, "CONSTRAINT") {
        cursor += 1;
        Some(take_word(&tokens, &mut cursor)?)
    } else {
        None
    };
    if !take_keyword(&tokens, &mut cursor, "FOREIGN") || !take_keyword(&tokens, &mut cursor, "KEY")
    {
        return None;
    }
    let local_columns = take_identifier_list(&tokens, &mut cursor)?;
    if !take_keyword(&tokens, &mut cursor, "REFERENCES") {
        return None;
    }
    let mut referenced_table = take_word(&tokens, &mut cursor)?;
    while matches!(tokens.get(cursor), Some(SqliteDdlToken::Dot)) {
        cursor += 1;
        referenced_table = take_word(&tokens, &mut cursor)?;
    }
    let referenced_columns = take_identifier_list(&tokens, &mut cursor)?;

    let mut on_update = None;
    let mut on_delete = None;
    let mut match_kind = None;
    let mut deferrable = false;
    let mut initially_deferred = false;
    while cursor < tokens.len() {
        if take_keyword(&tokens, &mut cursor, "ON") {
            let target = take_word(&tokens, &mut cursor)?;
            let action = take_fk_action(&tokens, &mut cursor)?;
            if target.eq_ignore_ascii_case("UPDATE") {
                on_update = Some(action);
            } else if target.eq_ignore_ascii_case("DELETE") {
                on_delete = Some(action);
            }
            continue;
        }
        if take_keyword(&tokens, &mut cursor, "MATCH") {
            match_kind = Some(take_word(&tokens, &mut cursor)?.to_ascii_uppercase());
            continue;
        }
        if take_keyword(&tokens, &mut cursor, "NOT") {
            if take_keyword(&tokens, &mut cursor, "DEFERRABLE") {
                deferrable = false;
                initially_deferred = false;
            }
            continue;
        }
        if take_keyword(&tokens, &mut cursor, "DEFERRABLE") {
            deferrable = true;
            continue;
        }
        if take_keyword(&tokens, &mut cursor, "INITIALLY") {
            if take_keyword(&tokens, &mut cursor, "DEFERRED") {
                deferrable = true;
                initially_deferred = true;
            } else {
                let _ = take_keyword(&tokens, &mut cursor, "IMMEDIATE");
            }
            continue;
        }
        cursor += 1;
    }

    Some(ParsedForeignKey {
        name,
        local_columns,
        referenced_table,
        referenced_columns,
        on_update,
        on_delete,
        match_kind,
        deferrable,
        initially_deferred,
    })
}

fn tokenize_sqlite_ddl(sql: &str) -> Vec<SqliteDdlToken> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut cursor = 0_usize;
    while cursor < chars.len() {
        if chars[cursor].is_whitespace() {
            cursor += 1;
            continue;
        }
        if chars[cursor] == '-' && chars.get(cursor + 1) == Some(&'-') {
            cursor += 2;
            while cursor < chars.len() && chars[cursor] != '\n' {
                cursor += 1;
            }
            continue;
        }
        if chars[cursor] == '/' && chars.get(cursor + 1) == Some(&'*') {
            cursor += 2;
            while cursor + 1 < chars.len() && !(chars[cursor] == '*' && chars[cursor + 1] == '/') {
                cursor += 1;
            }
            cursor = (cursor + 2).min(chars.len());
            continue;
        }
        match chars[cursor] {
            '(' => {
                tokens.push(SqliteDdlToken::OpenParen);
                cursor += 1;
            }
            ')' => {
                tokens.push(SqliteDdlToken::CloseParen);
                cursor += 1;
            }
            ',' => {
                tokens.push(SqliteDdlToken::Comma);
                cursor += 1;
            }
            '.' => {
                tokens.push(SqliteDdlToken::Dot);
                cursor += 1;
            }
            '\'' | '"' | '`' => {
                let delimiter = chars[cursor];
                cursor += 1;
                let mut word = String::new();
                while cursor < chars.len() {
                    if chars[cursor] == delimiter {
                        if chars.get(cursor + 1) == Some(&delimiter) {
                            word.push(delimiter);
                            cursor += 2;
                            continue;
                        }
                        cursor += 1;
                        break;
                    }
                    word.push(chars[cursor]);
                    cursor += 1;
                }
                tokens.push(SqliteDdlToken::Word(word));
            }
            '[' => {
                cursor += 1;
                let mut word = String::new();
                while cursor < chars.len() {
                    if chars[cursor] == ']' {
                        if chars.get(cursor + 1) == Some(&']') {
                            word.push(']');
                            cursor += 2;
                            continue;
                        }
                        cursor += 1;
                        break;
                    }
                    word.push(chars[cursor]);
                    cursor += 1;
                }
                tokens.push(SqliteDdlToken::Word(word));
            }
            _ => {
                let start = cursor;
                while cursor < chars.len()
                    && !chars[cursor].is_whitespace()
                    && !matches!(chars[cursor], '(' | ')' | ',' | '.')
                {
                    if (chars[cursor] == '-' && chars.get(cursor + 1) == Some(&'-'))
                        || (chars[cursor] == '/' && chars.get(cursor + 1) == Some(&'*'))
                    {
                        break;
                    }
                    cursor += 1;
                }
                if start == cursor {
                    cursor += 1;
                } else {
                    tokens.push(SqliteDdlToken::Word(chars[start..cursor].iter().collect()));
                }
            }
        }
    }
    tokens
}

fn token_is_keyword(tokens: &[SqliteDdlToken], cursor: usize, keyword: &str) -> bool {
    matches!(
        tokens.get(cursor),
        Some(SqliteDdlToken::Word(word)) if word.eq_ignore_ascii_case(keyword)
    )
}

fn take_keyword(tokens: &[SqliteDdlToken], cursor: &mut usize, keyword: &str) -> bool {
    if token_is_keyword(tokens, *cursor, keyword) {
        *cursor += 1;
        true
    } else {
        false
    }
}

fn take_word(tokens: &[SqliteDdlToken], cursor: &mut usize) -> Option<String> {
    let SqliteDdlToken::Word(word) = tokens.get(*cursor)? else {
        return None;
    };
    *cursor += 1;
    Some(word.clone())
}

fn take_identifier_list(tokens: &[SqliteDdlToken], cursor: &mut usize) -> Option<Vec<String>> {
    if !matches!(tokens.get(*cursor), Some(SqliteDdlToken::OpenParen)) {
        return None;
    }
    *cursor += 1;
    let mut identifiers = Vec::new();
    loop {
        identifiers.push(take_word(tokens, cursor)?);
        match tokens.get(*cursor) {
            Some(SqliteDdlToken::Comma) => *cursor += 1,
            Some(SqliteDdlToken::CloseParen) => {
                *cursor += 1;
                break;
            }
            _ => return None,
        }
    }
    Some(identifiers)
}

fn take_fk_action(tokens: &[SqliteDdlToken], cursor: &mut usize) -> Option<String> {
    let first = take_word(tokens, cursor)?;
    if first.eq_ignore_ascii_case("SET") || first.eq_ignore_ascii_case("NO") {
        let second = take_word(tokens, cursor)?;
        Some(format!(
            "{} {}",
            first.to_ascii_uppercase(),
            second.to_ascii_uppercase()
        ))
    } else {
        Some(first.to_ascii_uppercase())
    }
}

/// Recover an inline `zero-migrate:mask:…` or `zero-migrate:enc:…` sentinel for `column` from the
/// stored CREATE text. The emitter writes the sentinel as an inline
/// `/* zero-migrate:mask:… */` comment immediately after the relevant column's type;
/// `sqlite_master.sql` preserves it verbatim. We find the column's clause and, if a
/// `/* … */` comment on that clause carries a `zero-migrate:mask:` / `zero-migrate:enc:` body, return
/// that body (the sentinel without the comment delimiters). `None` if the column
/// carries no sentinel.
///
/// Conservative + injection-free: a pure string scan over engine-stored text, never
/// re-executed as SQL. It looks for the column NAME (quoted or bare) followed by a
/// `/* … */` block before the next column boundary, and extracts a recognised
/// sentinel body from that block.
fn recover_inline_sentinel(create_sql: &str, column: &str) -> Option<String> {
    let clause = sqlite_column_clause(create_sql, column)?;

    // Extract the first `/* … */` block in this clause and pull a recognised body.
    let open = clause.find("/*")?;
    let close_rel = clause[open + 2..].find("*/")?;
    let body = clause[open + 2..open + 2 + close_rel].trim();
    if body.starts_with("zero-migrate:mask:") || body.starts_with("zero-migrate:enc:") {
        Some(body.to_string())
    } else {
        None
    }
}

fn recover_case_sensitive(create_sql: &str, column: &str) -> Option<bool> {
    if sqlite_column_collation_name(create_sql, column)
        .is_some_and(|name| name.eq_ignore_ascii_case("NOCASE"))
    {
        Some(false)
    } else {
        None
    }
}

fn recover_column_collation(create_sql: &str, column: &str) -> Option<ColumnCollationSnapshot> {
    let name = sqlite_column_collation_name(create_sql, column)?;
    // BINARY is SQLite's implicit default. NOCASE already has a portable,
    // drift-comparable representation in `case_sensitive`; keeping either here
    // as well would make engine-authored schemas drift on spelling alone.
    if name.eq_ignore_ascii_case("BINARY") || name.eq_ignore_ascii_case("NOCASE") {
        return None;
    }
    Some(ColumnCollationSnapshot {
        schema: None,
        // SQLite resolves collation names case-insensitively.
        name: name.to_ascii_uppercase(),
    })
}

pub(crate) fn sqlite_column_collation_name(create_sql: &str, column: &str) -> Option<String> {
    let clause = sqlite_column_clause(create_sql, column)?;
    let tokens = tokenize_sqlite_ddl(clause);
    let mut depth = 0_i32;
    let mut cursor = 0_usize;
    while cursor < tokens.len() {
        match &tokens[cursor] {
            SqliteDdlToken::OpenParen => depth += 1,
            SqliteDdlToken::CloseParen => depth -= 1,
            SqliteDdlToken::Word(word) if depth == 0 && word.eq_ignore_ascii_case("COLLATE") => {
                cursor += 1;
                return take_word(&tokens, &mut cursor);
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn sqlite_column_clause<'a>(create_sql: &'a str, column: &str) -> Option<&'a str> {
    // Parse only the outer CREATE body, then compare each clause's decoded first
    // identifier. A global substring search can confuse the table name with a
    // same-named column and cannot correctly skip backtick/bracket quoting.
    let (open, close) = crate::render::declarative::sqlite_create_body_bounds(create_sql)?;
    let clauses = crate::render::declarative::sqlite_table_clauses(&create_sql[open + 1..close])?;
    for clause in clauses {
        let quoted = crate::render::declarative::sqlite_first_ddl_word_is_quoted(clause);
        let mut cursor = 0_usize;
        let first = crate::render::declarative::sqlite_ddl_word(clause, &mut cursor)?;
        if !quoted
            && matches!(
                first.to_ascii_uppercase().as_str(),
                "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN"
            )
        {
            continue;
        }
        if first.eq_ignore_ascii_case(column) {
            return Some(&clause[cursor..]);
        }
    }
    None
}

#[derive(Default)]
struct RecoveredColumnFormatChecks {
    uuid: bool,
    value_format: Option<crate::model::ir::ValueFormat>,
    mixed_uuid_and_value_format: bool,
}

/// Recover only engine-owned, column-local format checks. SQLite stores both
/// inline and table-level CHECKs in the CREATE statement, so scan every
/// unquoted CHECK and let the exact shared contract matcher attribute it to the
/// requested column. The shared recovery
/// routine validates the complete CHECK against the authoritative dialect
/// contract, so unrelated user checks and partially edited format checks stay
/// out of the semantic snapshot.
fn recover_column_format_checks(create_sql: &str, column: &str) -> RecoveredColumnFormatChecks {
    let mut uuid_count = 0_usize;
    let mut value_formats = Vec::new();
    for (start, end) in keyword_spans(create_sql, "CHECK", false) {
        let Some(open) = find_char_outside_quotes(create_sql, '(', end) else {
            continue;
        };
        let Some(close) = find_matching_paren(create_sql, open) else {
            continue;
        };
        match recover_format_check(column, &create_sql[start..=close], SqlDialect::Sqlite) {
            Some(RecoveredFormatCheck::Uuid) => uuid_count += 1,
            Some(RecoveredFormatCheck::Value(format)) => value_formats.push(format),
            None => {}
        }
    }

    // Duplicate engine contracts are themselves a structural alteration. Keep
    // recovery fail-closed so a duplicate cannot masquerade as the one expected
    // format check merely because both clauses happen to be identical.
    let mixed_uuid_and_value_format = uuid_count > 0 && !value_formats.is_empty();
    RecoveredColumnFormatChecks {
        uuid: uuid_count == 1,
        value_format: (value_formats.len() == 1).then(|| value_formats.remove(0)),
        mixed_uuid_and_value_format,
    }
}

fn column_declares_autoincrement(create_sql: &str, column: &str) -> bool {
    sqlite_column_clause(create_sql, column)
        .is_some_and(|clause| !keyword_spans(clause, "AUTOINCREMENT", true).is_empty())
}

fn sqlite_integer_cell(row: &[Option<String>], index: usize) -> i64 {
    row.get(index)
        .and_then(Clone::clone)
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_default()
}

/// Locate unquoted SQL keywords while preserving byte spans into the original
/// clause. `top_level_only` is used for column facets such as AUTOINCREMENT;
/// full CREATE-statement CHECK recovery accepts either inline or table-level
/// placement. This deliberately does not use the general DDL tokenizer: quoted
/// identifiers and string literals are valid tokenizer words, but neither may
/// declare structural metadata.
fn keyword_spans(sql: &str, keyword: &str, top_level_only: bool) -> Vec<(usize, usize)> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Plain,
        Single,
        Double,
        Backtick,
        Bracket,
        LineComment,
        BlockComment,
    }

    fn word_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
    }

    let bytes = sql.as_bytes();
    let mut state = State::Plain;
    let mut depth = 0_usize;
    let mut spans = Vec::new();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let next = bytes.get(cursor + 1).copied();
        match state {
            State::Plain => match (byte, next) {
                (b'-', Some(b'-')) => {
                    state = State::LineComment;
                    cursor += 2;
                    continue;
                }
                (b'/', Some(b'*')) => {
                    state = State::BlockComment;
                    cursor += 2;
                    continue;
                }
                (b'\'', _) => state = State::Single,
                (b'"', _) => state = State::Double,
                (b'`', _) => state = State::Backtick,
                (b'[', _) => state = State::Bracket,
                (b'(', _) => depth += 1,
                (b')', _) => depth = depth.saturating_sub(1),
                _ if byte.is_ascii_alphabetic() || byte == b'_' => {
                    let start = cursor;
                    cursor += 1;
                    while bytes.get(cursor).is_some_and(|byte| word_byte(*byte)) {
                        cursor += 1;
                    }
                    if (!top_level_only || depth == 0)
                        && sql[start..cursor].eq_ignore_ascii_case(keyword)
                    {
                        spans.push((start, cursor));
                    }
                    continue;
                }
                _ => {}
            },
            State::Single if byte == b'\'' => {
                if next == Some(b'\'') {
                    cursor += 2;
                    continue;
                }
                state = State::Plain;
            }
            State::Double if byte == b'"' => {
                if next == Some(b'"') {
                    cursor += 2;
                    continue;
                }
                state = State::Plain;
            }
            State::Backtick if byte == b'`' => {
                if next == Some(b'`') {
                    cursor += 2;
                    continue;
                }
                state = State::Plain;
            }
            State::Bracket if byte == b']' => {
                if next == Some(b']') {
                    cursor += 2;
                    continue;
                }
                state = State::Plain;
            }
            State::LineComment if matches!(byte, b'\n' | b'\r') => state = State::Plain,
            State::BlockComment if byte == b'*' && next == Some(b'/') => {
                state = State::Plain;
                cursor += 2;
                continue;
            }
            _ => {}
        }
        cursor += 1;
    }
    spans
}

/// Extract a required text cell, erroring on NULL / missing.
fn cell(row: &[Option<String>], i: usize) -> Result<String, DriftError> {
    row.get(i)
        .and_then(Clone::clone)
        .ok_or_else(|| DriftError::Backend(format!("missing introspection cell at index {i}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recover_inline_mask_sentinel_from_create_text() {
        let sql = "CREATE TABLE \"app\".\"users\" (\
            \"id\" TEXT PRIMARY KEY, \
            \"ssn\" BYTEA, \
            \"ssn_masked\" TEXT /* zero-migrate:mask:kind=last4,classification=pii */)";
        assert_eq!(
            recover_inline_sentinel(sql, "ssn_masked").as_deref(),
            Some("zero-migrate:mask:kind=last4,classification=pii")
        );
        // A plain column carries no sentinel.
        assert_eq!(recover_inline_sentinel(sql, "id"), None);
    }

    #[test]
    fn recover_inline_enc_sentinel() {
        let sql = "CREATE TABLE \"t\" (\"secret\" BYTEA /* zero-migrate:enc:randomised:default:string */, \"x\" INTEGER)";
        assert_eq!(
            recover_inline_sentinel(sql, "secret").as_deref(),
            Some("zero-migrate:enc:randomised:default:string")
        );
        // The later column must NOT inherit the earlier column's sentinel.
        assert_eq!(recover_inline_sentinel(sql, "x"), None);
    }

    #[test]
    fn column_clause_matches_the_decoded_first_identifier_only() {
        let sql = "CREATE TABLE t (username TEXT, user INTEGER)";
        assert_eq!(
            sqlite_column_clause(sql, "user").map(str::trim),
            Some("INTEGER")
        );
        assert_eq!(sqlite_column_clause(sql, "use"), None);
    }

    #[test]
    fn recovers_only_the_columns_top_level_exact_collation() {
        let sql = r#"CREATE TABLE t (
            "defaulted" TEXT DEFAULT 'COLLATE fake, )' COLLATE RTRIM,
            "checked" TEXT CHECK ("checked" COLLATE NOCASE <> '') COLLATE [custom.name],
            "binary" TEXT COLLATE BINARY,
            "nocase" TEXT COLLATE NOCASE
        )"#;

        assert_eq!(
            recover_column_collation(sql, "defaulted").map(|collation| collation.name),
            Some("RTRIM".to_string())
        );
        assert_eq!(
            recover_column_collation(sql, "checked").map(|collation| collation.name),
            Some("CUSTOM.NAME".to_string())
        );
        assert_eq!(recover_column_collation(sql, "binary"), None);
        assert_eq!(recover_column_collation(sql, "nocase"), None);
        assert_eq!(recover_case_sensitive(sql, "nocase"), Some(false));
        assert_eq!(recover_case_sensitive(sql, "checked"), None);
    }

    #[test]
    fn column_collation_parser_uses_body_clauses_and_all_identifier_quotes() {
        let sql = r#"CREATE TABLE "items" (
            "items" TEXT COLLATE NOCASE,
            `tick` TEXT COLLATE RTRIM,
            [bracket] TEXT COLLATE [custom.name],
            'single' TEXT COLLATE NOCASE
        )"#;

        assert_eq!(
            sqlite_column_collation_name(sql, "items").as_deref(),
            Some("NOCASE")
        );
        assert_eq!(
            sqlite_column_collation_name(sql, "tick").as_deref(),
            Some("RTRIM")
        );
        assert_eq!(
            sqlite_column_collation_name(sql, "bracket").as_deref(),
            Some("custom.name")
        );
        assert_eq!(
            sqlite_column_collation_name(sql, "single").as_deref(),
            Some("NOCASE")
        );
    }

    #[test]
    fn parses_named_composite_fk_metadata_from_stored_create_sql() {
        let sql = r#"CREATE TABLE "child" (
            "tenant" TEXT,
            "order" INTEGER,
            CONSTRAINT "fk_child_parent" FOREIGN KEY ("tenant", "order")
                REFERENCES "Parent" ("tenant", "parent_id")
                MATCH SIMPLE ON DELETE CASCADE ON UPDATE SET NULL
                DEFERRABLE INITIALLY DEFERRED
        )"#;

        assert_eq!(
            parse_table_foreign_keys(sql),
            vec![ParsedForeignKey {
                name: Some("fk_child_parent".to_string()),
                local_columns: vec!["tenant".to_string(), "order".to_string()],
                referenced_table: "Parent".to_string(),
                referenced_columns: vec!["tenant".to_string(), "parent_id".to_string()],
                on_update: Some("SET NULL".to_string()),
                on_delete: Some("CASCADE".to_string()),
                match_kind: Some("SIMPLE".to_string()),
                deferrable: true,
                initially_deferred: true,
            }]
        );
    }

    #[test]
    fn parses_foreign_keys_around_all_sqlite_quotes_and_comments() {
        let sql = r#"CREATE /* outer comment with (, ) */ TABLE "child,(table)"
            -- outer line comment with (, )
            (
                'local,one' TEXT DEFAULT 'value, ) -- not a comment /* either */',
                "local(two)" INTEGER,
                `local,three` INTEGER,
                [local(four),x] INTEGER,
                /* A fake clause, comma, and close paren must stay hidden:
                   CONSTRAINT fake FOREIGN KEY (x) REFERENCES y(z), ) */
                CONSTRAINT /* comment before the quoted name (, ) */ 'fk''single'
                    FOREIGN /* comment between keywords */ KEY ('local,one', "local(two)")
                    REFERENCES /* comment before a qualified table */ [main].[parent,(table)]
                        ([ref,one], `ref(two)`) ON DELETE CASCADE,
                CONSTRAINT "fk""double" FOREIGN KEY (`local,three`)
                    REFERENCES "parent" ("ref,three"),
                -- comment between clauses with a fake comma, and close paren )
                CONSTRAINT `fk``tick` FOREIGN KEY ([local(four),x])
                    REFERENCES 'parent(two)' ('ref(four),x'),
                CONSTRAINT [fk]]bracket] FOREIGN KEY ("local(two)")
                    REFERENCES [parent] ([ref])
            ) /* trailing comment with (, ) */"#;

        let parsed = parse_table_foreign_keys(sql);
        assert_eq!(parsed.len(), 4);

        assert_eq!(parsed[0].name.as_deref(), Some("fk'single"));
        assert_eq!(parsed[0].local_columns, ["local,one", "local(two)"]);
        assert_eq!(parsed[0].referenced_table, "parent,(table)");
        assert_eq!(parsed[0].referenced_columns, ["ref,one", "ref(two)"]);
        assert_eq!(parsed[0].on_delete.as_deref(), Some("CASCADE"));

        assert_eq!(parsed[1].name.as_deref(), Some("fk\"double"));
        assert_eq!(parsed[1].local_columns, ["local,three"]);
        assert_eq!(parsed[1].referenced_columns, ["ref,three"]);

        assert_eq!(parsed[2].name.as_deref(), Some("fk`tick"));
        assert_eq!(parsed[2].local_columns, ["local(four),x"]);
        assert_eq!(parsed[2].referenced_table, "parent(two)");
        assert_eq!(parsed[2].referenced_columns, ["ref(four),x"]);

        assert_eq!(parsed[3].name.as_deref(), Some("fk]bracket"));
        assert_eq!(parsed[3].local_columns, ["local(two)"]);
        assert_eq!(parsed[3].referenced_columns, ["ref"]);
    }

    #[test]
    fn unterminated_comments_do_not_expose_phantom_table_structure() {
        let sql = r#"CREATE TABLE child /* (
            CONSTRAINT fake FOREIGN KEY (child_id) REFERENCES parent(id)
        )"#;

        assert!(parse_table_foreign_keys(sql).is_empty());
    }

    #[test]
    fn sqlite_fk_definition_is_ordered_and_match_simple_is_implicit() {
        use crate::render::declarative::ir_fk_constraint_snapshot_for_columns;
        use crate::schema::query::SqlDialect;

        let pragma = PragmaForeignKey {
            referenced_table: "Parent".to_string(),
            columns: Vec::new(),
            on_update: "SET NULL".to_string(),
            on_delete: "CASCADE".to_string(),
            match_kind: "NONE".to_string(),
        };
        let parsed = ParsedForeignKey {
            name: Some("fk_child_parent".to_string()),
            local_columns: vec!["tenant".to_string(), "order".to_string()],
            referenced_table: "Parent".to_string(),
            referenced_columns: vec!["tenant".to_string(), "parent_id".to_string()],
            on_update: Some("SET NULL".to_string()),
            on_delete: Some("CASCADE".to_string()),
            match_kind: Some("SIMPLE".to_string()),
            deferrable: true,
            initially_deferred: true,
        };

        let actual = canonical_foreign_key_definition(
            &pragma,
            &["tenant".to_string(), "order".to_string()],
            &["tenant".to_string(), "parent_id".to_string()],
            Some(&parsed),
        );
        let desired = ir_fk_constraint_snapshot_for_columns(
            "ignored_by_sqlite",
            Some("fk_child_parent"),
            &["tenant".to_string(), "order".to_string()],
            "Parent",
            &["tenant".to_string(), "parent_id".to_string()],
            Some("cascade"),
            Some("set null"),
            true,
            true,
            SqlDialect::Sqlite,
        );
        assert_eq!(
            actual, desired.definition,
            "desired snapshot must round-trip"
        );
        assert_eq!(
            actual,
            "FOREIGN KEY (tenant, \"order\") REFERENCES \"Parent\"(tenant, parent_id) \
             ON UPDATE SET NULL ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED"
        );
    }

    #[test]
    fn recovers_an_exact_table_level_value_format_check() {
        let check = crate::render::value_format::column_metadata(
            "id",
            &crate::model::ir::ValueFormat::TypeId {
                prefix: "account".to_string(),
            },
            SqlDialect::Sqlite,
        )
        .expect("TypeID metadata")
        .inline_check;
        let create_sql = format!("CREATE TABLE ids (id TEXT PRIMARY KEY, {check})");
        let recovered = recover_column_format_checks(&create_sql, "id");
        assert_eq!(
            recovered.value_format,
            Some(crate::model::ir::ValueFormat::TypeId {
                prefix: "account".to_string()
            })
        );
    }
}
