//! SQLite live-schema introspection for drift (SQLite-parity design §2.7 / §2.5.1).
//!
//! Produces the SAME dialect-agnostic [`SchemaSnapshot`](crate::drift::SchemaSnapshot)
//! the Postgres path returns, so [`check_checksum_drift`](crate::drift::check_checksum_drift)
//! and [`diff_snapshots`](crate::drift::diff_snapshots) work unchanged across both
//! dialects. The PG path reads `information_schema` + `pg_catalog`; this reads
//! `sqlite_master` + `PRAGMA table_info` / `PRAGMA index_list` / `PRAGMA index_info`
//! / `PRAGMA foreign_key_list` of the connection's `main` database (the app file).
//!
//! # Confinement (§2.5.1)
//!
//! Every read here runs under **`EngineJournal`** mode: the engine's OWN
//! introspection touches `sqlite_master` and issues `PRAGMA table_info(...)` etc.,
//! both of which the **CreatorUp** authorizer denies a creator from doing (PRAGMA
//! is denied outright in CreatorUp; §2.5.1). The introspection is read-only — it
//! emits no DDL and mutates nothing — but it MUST run in engine mode for the PRAGMA
//! reads to compile. A creator can never reach this code path (it is engine-private,
//! behind the `SqliteBackend`), so allowing these reads under engine mode does not
//! widen the creator surface.
//!
//! # What is excluded from the app-schema snapshot
//!
//! - SQLite internal tables (`sqlite_*`, incl. `sqlite_sequence` / `sqlite_stat*`).
//! - The `_mig` journal objects — they live in the ATTACHed `_mig` database, not
//!   `main`, so a `main`-scoped `sqlite_master` read never sees them anyway; we
//!   additionally scope every PRAGMA to `main`.
//!
//! # Sentinel recovery (§2.7)
//!
//! The SQLite emitter bakes the `/* __zsmask:… */` (and `/* zsenc:… */`) sentinels
//! INLINE in the `CREATE` text, which `sqlite_master.sql` preserves verbatim (SQLite
//! keeps comments in the stored schema text, unlike PG which discards them at
//! parse). [`recover_inline_sentinel`] pulls the `__zsmask:` / `zsenc:` body for a
//! given column out of that stored text into the snapshot's
//! [`comment_sentinel`](crate::drift::ColumnSnapshot::comment_sentinel), so a
//! masked/encrypted column round-trips faithfully rather than being silently
//! dropped to a plain column.

use std::collections::BTreeMap;

use crate::drift::{
    ColumnSnapshot, ConstraintSnapshot, DriftError, IndexSnapshot, SchemaSnapshot, TableSnapshot,
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

/// Introspect the LIVE structure of the connection's `main` database (the tenant
/// app file) into a [`SchemaSnapshot`], the same shape the PG path returns.
///
/// Read-only; runs under engine mode (the PRAGMA reads require it, §2.5.1). The
/// result map is a `BTreeMap` and every child vector is name-sorted, so the
/// snapshot is byte-stable regardless of catalog scan order — matching the PG
/// snapshot's determinism contract.
///
/// # Errors
/// [`DriftError::Backend`] on a `sqlite_master` / PRAGMA read failure.
pub(crate) async fn snapshot_schema(actor: &MigrationActor) -> Result<SchemaSnapshot, DriftError> {
    // Engine mode: the introspection reads sqlite_master + issues PRAGMAs, both of
    // which CreatorUp denies. Read-only (no DDL, no writes).
    actor.set_mode(Mode::EngineJournal).await.map_err(drift_err)?;

    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();

    // Base tables of `main`, with their stored CREATE text (carries inline
    // sentinels). `type='table'` excludes views/indexes/triggers; the
    // `name NOT LIKE 'sqlite_%'` and the `is_internal` guard exclude SQLite
    // internals. `main.sqlite_master` scopes the read to the app file — never `_mig`.
    // NOTE: we deliberately do NOT use `NOT LIKE 'sqlite_%'` here — the hardened
    // authorizer's function allowlist (§2.5.1) does not include `LIKE`, so the
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
    for r in &table_rows {
        let name = cell(r, 0)?;
        if is_internal(&name) {
            continue;
        }
        if let Some(Some(sql)) = r.get(1) {
            create_sql.insert(name.clone(), sql.clone());
        }
        tables.insert(
            name,
            TableSnapshot {
                columns: Vec::new(),
                indexes: Vec::new(),
                constraints: Vec::new(),
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

    // Deterministic ordering: sort every child vector by name (the PG path sorts via
    // `ORDER BY` in SQL; SQLite PRAGMAs return in declaration/ordinal order, so we
    // sort here to match the snapshot equality contract).
    for t in tables.values_mut() {
        t.columns.sort_by(|a, b| a.name.cmp(&b.name));
        t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        t.constraints.sort_by(|a, b| a.name.cmp(&b.name));
    }

    Ok(SchemaSnapshot { tables })
}

/// Columns via `PRAGMA table_info(<t>)`. Columns: cid, name, type, notnull,
/// dflt_value, pk. We map `type` → `data_type` (normalised lowercase, the SQLite
/// declared-type spelling), `notnull == 0` → nullable, and recover the inline
/// `__zsmask:` / `zsenc:` sentinel for the column from the stored CREATE text.
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
            // mask/encryption sentinel into `comment_sentinel` (§2.7) so an encrypted
            // / masked column round-trips faithfully rather than dropping silently.
            default: None,
            encryption_sentinel: None,
            comment_sentinel: recover_inline_sentinel(stored_create_sql, &name),
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
///   - record every index as an [`IndexSnapshot`] (its key columns from
///     index_info), EXCLUDING SQLite auto-indexes named `sqlite_autoindex_*` from
///     the *index* bucket (they are constraint artifacts, surfaced as constraints);
///   - synthesise a `UNIQUE` / `PRIMARY KEY` [`ConstraintSnapshot`] for `origin`
///     `u` / `pk` so a unique/PK constraint round-trips against a PG snapshot's
///     constraint bucket.
async fn introspect_indexes_and_unique(
    actor: &MigrationActor,
    table: &str,
    tables: &mut BTreeMap<String, TableSnapshot>,
) -> Result<(), DriftError> {
    let idx_rows = actor
        .query(&format!("PRAGMA main.index_list({})", lit(table)))
        .await
        .map_err(drift_err)?;

    // (name, unique, origin, columns) gathered first so we can borrow the table
    // mutably once.
    let mut gathered: Vec<(String, bool, String, Vec<String>)> = Vec::new();
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
        gathered.push((name, unique, origin.trim().to_string(), columns));
    }

    let Some(t) = tables.get_mut(table) else {
        return Ok(());
    };
    for (name, unique, origin, columns) in gathered {
        // A PRIMARY KEY auto-index (origin 'pk') is already surfaced as a constraint
        // from `table_info` (the authoritative PK source, which also covers the rowid
        // PK that has no index_list entry). Skip it here entirely so the PK is not
        // double-counted and the `sqlite_autoindex_*` name never leaks.
        if origin == "pk" {
            continue;
        }
        // A UNIQUE-constraint auto-index (origin 'u') is a CONSTRAINT, surfaced in the
        // constraint bucket (matching the PG snapshot, where UNIQUE is a
        // pg_constraint row, not a pg_index row in the index bucket). Its name is the
        // SQLite-internal `sqlite_autoindex_*`, which we must not leak as a creator
        // index.
        if origin == "u" {
            t.constraints.push(ConstraintSnapshot {
                name: name.clone(),
                kind: "UNIQUE".to_string(),
                // A faithful, re-parse-stable definition shape: the constraint kind
                // over its ordered key columns. (SQLite has no `pg_get_constraintdef`;
                // this canonical spelling is the closest faithful round-trip form.)
                definition: format!("UNIQUE ({})", columns.join(", ")),
            });
            // Do NOT also push it into the index bucket if it is a SQLite auto-index;
            // an explicit `CREATE UNIQUE INDEX` (origin 'c') is handled below.
            if name.starts_with("sqlite_autoindex_") {
                continue;
            }
        }
        // A real index (CREATE INDEX, origin 'c') — and an explicitly-named UNIQUE
        // index that is not a sqlite_autoindex — is an IndexSnapshot.
        if is_internal(&name) {
            continue;
        }
        t.indexes.push(IndexSnapshot {
            name,
            unique,
            columns,
            // SQLite indexes are b-tree (the only built-in index AM). fts5/vec0 are
            // vtables, not indexes, and surface as tables; see the divergences doc.
            access_method: "btree".to_string(),
            expression: None,
            opclass: None,
        });
    }
    Ok(())
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
        let entry = by_id.entry(id).or_insert_with(|| (ref_table.clone(), Vec::new()));
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
        });
    }
    Ok(())
}

/// Recover an inline `__zsmask:…` or `zsenc:…` sentinel for `column` from the
/// stored CREATE text (§2.7). The emitter writes the sentinel as an inline
/// `/* __zsmask:… */` comment immediately after the relevant column's type;
/// `sqlite_master.sql` preserves it verbatim. We find the column's clause and, if a
/// `/* … */` comment on that clause carries a `__zsmask:` / `zsenc:` body, return
/// that body (the sentinel without the comment delimiters). `None` if the column
/// carries no sentinel.
///
/// Conservative + injection-free: a pure string scan over engine-stored text, never
/// re-executed as SQL. It looks for the column NAME (quoted or bare) followed by a
/// `/* … */` block before the next column boundary, and extracts a recognised
/// sentinel body from that block.
fn recover_inline_sentinel(create_sql: &str, column: &str) -> Option<String> {
    // The emitter quotes identifiers with double-quotes: `"<col>_masked" TEXT NOT
    // NULL /* __zsmask:… */`. The masked SIBLING column carries the `__zsmask:`
    // sentinel; an encrypted column carries `zsenc:` on the column itself. We scan
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
    let clause = &rest[..clause_end];

    // Extract the first `/* … */` block in this clause and pull a recognised body.
    let open = clause.find("/*")?;
    let close_rel = clause[open + 2..].find("*/")?;
    let body = clause[open + 2..open + 2 + close_rel].trim();
    if body.starts_with("__zsmask:") || body.starts_with("zsenc:") {
        Some(body.to_string())
    } else {
        None
    }
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
            \"ssn_masked\" TEXT NOT NULL /* __zsmask:kind=last4,classification=pii */)";
        assert_eq!(
            recover_inline_sentinel(sql, "ssn_masked").as_deref(),
            Some("__zsmask:kind=last4,classification=pii")
        );
        // A plain column carries no sentinel.
        assert_eq!(recover_inline_sentinel(sql, "id"), None);
    }

    #[test]
    fn recover_inline_zsenc_sentinel() {
        let sql = "CREATE TABLE \"t\" (\"secret\" BYTEA /* zsenc:randomised:default:string */, \"x\" INTEGER)";
        assert_eq!(
            recover_inline_sentinel(sql, "secret").as_deref(),
            Some("zsenc:randomised:default:string")
        );
        // The later column must NOT inherit the earlier column's sentinel.
        assert_eq!(recover_inline_sentinel(sql, "x"), None);
    }

    #[test]
    fn bare_ident_is_whole_word() {
        // `user` must not match inside `username`.
        assert_eq!(find_bare_ident("CREATE TABLE t (username TEXT)", "user"), None);
        assert!(find_bare_ident("CREATE TABLE t (user TEXT)", "user").is_some());
    }
}
