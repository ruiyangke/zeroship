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
use crate::model::ir::IndexSortOrder;
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot, IndexSnapshot, SchemaSnapshot,
    TableSnapshot, ViewSnapshot,
};

use super::actor::{MigrationActor, SqliteActorError};
use super::authorizer::Mode;

/// One member column of a composite foreign key, as `PRAGMA foreign_key_list`
/// reports it: `(seq, from_column, to_column)` — `seq` orders the columns within
/// the FK, `from`/`to` are the local and referenced column names.
type ForeignKeyColumn = (i64, String, String);

/// Foreign keys grouped by their `PRAGMA foreign_key_list` `id` (a composite FK
/// spans several rows sharing one `id`). The value is the referenced table name
/// plus that FK's ordered member columns. Named to keep
/// [`introspect_foreign_keys`] free of the `clippy::type_complexity` trip the
/// inline `BTreeMap<i64, (String, Vec<(i64, String, String)>)>` caused (no
/// behaviour change).
type ForeignKeysById = BTreeMap<i64, (String, Vec<ForeignKeyColumn>)>;

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
        introspect_foreign_keys(actor, &table, &mut tables).await?;
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
        let pk_ord: i64 = r
            .get(5)
            .and_then(Clone::clone)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
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
            default: None,
            generated: None,
            identity: None,
            encryption_sentinel: None,
            ddl_type_override: None,
            inline_checks: Vec::new(),
            comment_sentinel: recover_inline_sentinel(stored_create_sql, &name),
            case_sensitive: recover_case_sensitive(stored_create_sql, &name),
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

fn find_char_outside_quotes(s: &str, needle: char, from: usize) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if i < from {
            continue;
        }
        match ch {
            '\'' if !in_double => {
                if in_single && matches!(chars.peek(), Some((_, '\''))) {
                    chars.next();
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single => {
                if in_double && matches!(chars.peek(), Some((_, '"'))) {
                    chars.next();
                } else {
                    in_double = !in_double;
                }
            }
            _ if ch == needle && !in_single && !in_double => return Some(i),
            _ => {}
        }
    }
    None
}

fn find_matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0_usize;
    let mut chars = s.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if i < open {
            continue;
        }
        match ch {
            '\'' if !in_double => {
                if in_single && matches!(chars.peek(), Some((_, '\''))) {
                    chars.next();
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single => {
                if in_double && matches!(chars.peek(), Some((_, '"'))) {
                    chars.next();
                } else {
                    in_double = !in_double;
                }
            }
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && !in_double => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0_usize;
    let mut depth = 0_usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        match ch {
            '\'' if !in_double => {
                if in_single && matches!(chars.peek(), Some((_, '\''))) {
                    chars.next();
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single => {
                if in_double && matches!(chars.peek(), Some((_, '"'))) {
                    chars.next();
                } else {
                    in_double = !in_double;
                }
            }
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && !in_double => {
                depth = depth.saturating_sub(1);
            }
            ',' if depth == 0 && !in_single && !in_double => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Foreign keys via `PRAGMA foreign_key_list(<t>)` → one `FOREIGN KEY`
/// [`ConstraintSnapshot`] per declared FK. SQLite does not name FKs (they have no
/// constraint name), so we synthesise a deterministic name from the (id, referenced
/// table, local columns) — stable across reads of the same schema.
///
/// `foreign_key_list` columns: id, seq, table, from, to, on_update, on_delete,
/// match. Multiple rows with the same `id` are the columns of one composite FK.
async fn introspect_foreign_keys(
    actor: &MigrationActor,
    table: &str,
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
        let entry = by_id
            .entry(id)
            .or_insert_with(|| (ref_table.clone(), Vec::new()));
        entry.1.push((seq, from, to));
    }

    let Some(t) = tables.get_mut(table) else {
        return Ok(());
    };
    for (id, (ref_table, mut cols)) in by_id {
        cols.sort_by_key(|(seq, _, _)| *seq);
        let from_cols: Vec<String> = cols.iter().map(|(_, f, _)| f.clone()).collect();
        let to_cols: Vec<String> = cols.iter().map(|(_, _, to)| to.clone()).collect();
        // Deterministic synthetic name (SQLite FKs are unnamed): table + columns + id.
        let name = format!("fk_{table}_{}_{id}", from_cols.join("_"));
        t.constraints.push(ConstraintSnapshot {
            name,
            kind: "FOREIGN KEY".to_string(),
            definition: format!(
                "FOREIGN KEY ({}) REFERENCES {}({})",
                from_cols.join(", "),
                ref_table,
                to_cols.join(", ")
            ),
            comment: None,
        });
    }
    Ok(())
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
    let clause = sqlite_column_clause(create_sql, column)?;
    if clause.to_ascii_lowercase().contains("collate nocase") {
        Some(false)
    } else {
        None
    }
}

fn sqlite_column_clause<'a>(create_sql: &'a str, column: &str) -> Option<&'a str> {
    // The emitter quotes identifiers with double-quotes: `"<col>_masked" TEXT NOT
    // NULL /* zero-migrate:mask:… */`. The masked SIBLING column carries the `zero-migrate:mask:`
    // sentinel; an encrypted column carries `zero-migrate:enc:` on the column itself. We scan
    // for the column token, then the next `/* … */` up to the next comma/`)` at
    // depth 0.
    let needle_quoted = format!("\"{column}\"");
    let start = create_sql.find(&needle_quoted).or_else(|| {
        // Bare (unquoted) fallback — match the column as a whole word.
        find_bare_ident(create_sql, column)
    })?;
    let rest = &create_sql[start + needle_quoted_len(create_sql, start, column)..];

    // Find the column-clause boundary: the next top-level comma or the closing `)`.
    // We do not need perfect SQL parsing — just a bound so a sentinel from a LATER
    // column is not mis-attributed. CRITICAL: skip `/* … */` regions so a comma
    // INSIDE the sentinel comment (e.g. `kind=last4,classification=pii`) is NOT
    // mistaken for a top-level column separator (which would truncate the clause
    // before the `*/` and lose the sentinel).
    let mut depth: i32 = 0;
    let mut clause_end = rest.len();
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip a block comment wholesale.
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    clause_end = i;
                    break;
                }
                depth -= 1;
            }
            b',' if depth == 0 => {
                clause_end = i;
                break;
            }
            _ => {}
        }
        i += 1;
    }
    Some(&rest[..clause_end])
}

/// Length of the quoted or bare needle we matched at `start`, so the slice past it
/// is correct for either spelling.
fn needle_quoted_len(create_sql: &str, start: usize, column: &str) -> usize {
    let quoted = format!("\"{column}\"");
    if create_sql[start..].starts_with(&quoted) {
        quoted.len()
    } else {
        column.len()
    }
}

/// Find a bare (unquoted) identifier as a whole word (preceded + followed by a
/// non-identifier char), so we don't match a substring of a longer name.
fn find_bare_ident(haystack: &str, ident: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(ident) {
        let at = from + rel;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        let after = at + ident.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return Some(at);
        }
        from = at + ident.len();
    }
    None
}

const fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
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
    fn bare_ident_is_whole_word() {
        // `user` must not match inside `username`.
        assert_eq!(
            find_bare_ident("CREATE TABLE t (username TEXT)", "user"),
            None
        );
        assert!(find_bare_ident("CREATE TABLE t (user TEXT)", "user").is_some());
    }
}
