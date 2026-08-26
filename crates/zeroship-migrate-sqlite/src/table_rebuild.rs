//! SQLite's table-rebuild decisions and stored-DDL rewrites.

use std::collections::{BTreeMap, BTreeSet};

use zeroship_migrate_backend::ddl::{fk_local_columns, fk_target_table, is_pk_index};
use zeroship_migrate_backend::error::{DeclarativeError, IrLowerError};
use zeroship_migrate_backend::schema::SchemaRenderer;
use zeroship_migrate_backend::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexSnapshot, TableSnapshot,
};
use zeroship_migrate_backend::table_rebuild::{InjectedPrimaryKey, ResolvedRename, TableRebuildPolicy};
use zeroship_migrate_ir::dialect::DialectId;
use zeroship_migrate_ir::ir::Op;

#[derive(Debug)]
pub(super) struct SqliteTableRebuildPolicy;

pub(super) static POLICY: SqliteTableRebuildPolicy = SqliteTableRebuildPolicy;

impl SqliteTableRebuildPolicy {
    fn schema_renderer(&self) -> &'static dyn SchemaRenderer {
        &crate::schema::RENDERER
    }

    /// does this existing SQLite table need the 12-step table REBUILD to
    /// reconcile `live` -> `desired`? Returns `Some(reason)` for a change SQLite has
    /// NO native `ALTER` for, `None` if every difference is natively expressible
    /// (ADD COLUMN / DROP COLUMN / ADD INDEX / DROP INDEX).
    ///
    /// The rebuild triggers: a same-name column TYPE change, a
    /// nullability change (either direction), a hinted column RENAME, a same-name
    /// index in-place redefinition (uniqueness or column-set change), a same-name FK
    /// redefinition, and an ADD/DROP of an FK constraint (SQLite has no
    /// `ALTER TABLE ADD/DROP CONSTRAINT`, so any FK-set change is a rebuild).
    ///
    /// **Fail-closed:** the FIRST trigger found returns immediately with a precise
    /// reason; the per-op emission below never runs for a rebuild-needing table.
    fn sqlite_existing_table_needs_rebuild(
        &self,
        table: &str,
        lt: &TableSnapshot,
        dt: &TableSnapshot,
        table_renames: &[&ResolvedRename],
    ) -> Option<String> {
        // (1) A hinted column RENAME - SQLite has `RENAME COLUMN`, but the engine's
        //     rename path is the PG-shaped expand-contract sequence; on SQLite we
        //     reconcile a rename via the rebuild (`to <- from` copy mapping), keeping
        //     it single-sourced + confinement-clean.
        if let Some(r) = table_renames.first() {
            return Some(format!("rename column {} → {}", r.from, r.to));
        }

        let live_cols: BTreeMap<&str, &ColumnSnapshot> =
            lt.columns.iter().map(|c| (c.name.as_str(), c)).collect();

        // (2)/(3) A same-name column with a TYPE or NULLABILITY change. The
        //     the registered SQLite canonicalizer avoids false positives on
        //     PG-vs-SQLite spelling differences (bytea<->blob, double precision<->real,
        //     timestamptz<->text); a GENUINE change maps to two distinct tokens.
        for c in &dt.columns {
            if let Some(lc) = live_cols.get(c.name.as_str()) {
                if self.schema_renderer().canonical_type(&lc.data_type)
                    != self.schema_renderer().canonical_type(&c.data_type)
                {
                    return Some(format!(
                        "alter column {} type {} → {}",
                        c.name, lc.data_type, c.data_type
                    ));
                }
                if lc.nullable != c.nullable {
                    return Some(format!(
                        "alter column {} nullability {} → {}",
                        c.name, lc.nullable, c.nullable
                    ));
                }
                if lc.case_sensitive != c.case_sensitive {
                    return Some(format!(
                        "alter column {} caseSensitive {} → {}",
                        c.name,
                        lc.case_sensitive.unwrap_or(true),
                        c.case_sensitive.unwrap_or(true)
                    ));
                }
            }
        }

        // (4) A same-name INDEX whose uniqueness or column set changed - an in-place
        //     index redefinition (SQLite has no `ALTER INDEX`; a DROP+CREATE inside a
        //     rebuild is how the new shape's index set lands).
        let live_idx: BTreeMap<&str, &IndexSnapshot> =
            lt.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
        for idx in &dt.indexes {
            if is_pk_index(&crate::fold::POLICY, table, &idx.name) {
                continue;
            }
            if let Some(li) = live_idx.get(idx.name.as_str()) {
                if li.unique != idx.unique {
                    return Some(format!(
                        "index {}.{} uniqueness change {} → {}",
                        table, idx.name, li.unique, idx.unique
                    ));
                }
                if li.columns != idx.columns {
                    return Some(format!(
                        "index {}.{} column change {:?} → {:?}",
                        table, idx.name, li.columns, idx.columns
                    ));
                }
            }
        }

        // (5) A FOREIGN KEY set change - a redefinition (same name, changed body),
        //     an ADD (desired-only FK), or a DROP (live-only FK). SQLite inlines FKs
        //     at CREATE TABLE and has no `ALTER TABLE ADD/DROP CONSTRAINT`, so ANY FK
        //     set difference is a rebuild.
        let live_fk: BTreeMap<&str, &ConstraintSnapshot> = lt
            .constraints
            .iter()
            .filter(|c| c.kind == "FOREIGN KEY")
            .map(|c| (c.name.as_str(), c))
            .collect();
        let desired_fk: BTreeMap<&str, &ConstraintSnapshot> = dt
            .constraints
            .iter()
            .filter(|c| c.kind == "FOREIGN KEY")
            .map(|c| (c.name.as_str(), c))
            .collect();
        for (name, dc) in &desired_fk {
            match live_fk.get(name) {
                None => return Some(format!("add foreign key {table}.{name}")),
                Some(lc) if lc.definition != dc.definition => {
                    return Some(format!(
                        "foreign key {table}.{name} definition change {:?} → {:?}",
                        lc.definition, dc.definition
                    ));
                }
                Some(_) => {}
            }
        }
        for name in live_fk.keys() {
            if !desired_fk.contains_key(name) {
                return Some(format!("drop foreign key {table}.{name}"));
            }
        }

        // (6) A DROP COLUMN of a CONSTRAINED column. SQLite's native
        //     `ALTER TABLE ... DROP COLUMN` ERRORS at apply when the dropped column
        //     participates in ANY index, CHECK, foreign key, generated-column
        //     expression, or partial-index predicate - so the per-op
        //     `render_drop_column` would abort the migration. We route such a drop
        //     to the 12-step rebuild (which omits the column from `copy_columns` and
        //     recreates only the surviving dependents). A column that is
        //     UNconstrained drops natively via the per-op path (no rebuild).
        //
        //     "Dropped" = a column present in LIVE, absent from DESIRED, and NOT a
        //     rename `from` (a rename is its own rebuild trigger, handled in step 1).
        let desired_names: BTreeSet<&str> = dt.columns.iter().map(|c| c.name.as_str()).collect();
        let renamed_from: BTreeSet<&str> = table_renames.iter().map(|r| r.from.as_str()).collect();
        for lc in &lt.columns {
            let col = lc.name.as_str();
            if desired_names.contains(col) || renamed_from.contains(col) {
                continue; // surviving column, or handled by the rename path
            }
            // This `col` is being dropped. Does any index / constraint / raw-DDL
            // dependent of the LIVE table reference it? If so -> rebuild.
            if let Some(dep) = self
                .schema_renderer()
                .stored_ddl()
                .expect("the SQLite renderer must provide stored-DDL analysis")
                .dropped_column_dependent(table, lc, lt)
            {
                return Some(dep);
            }
        }

        None
    }
}

fn pure_sqlite_column_rename<'a>(
    live: &TableSnapshot,
    desired: &TableSnapshot,
    renames: &[&'a ResolvedRename],
) -> Option<&'a ResolvedRename> {
    let [rename] = renames else {
        return None;
    };
    let mut renamed_live = live.clone();
    if renamed_live
        .columns
        .iter()
        .any(|column| column.name == rename.to)
    {
        return None;
    }
    let column = renamed_live
        .columns
        .iter_mut()
        .find(|column| column.name == rename.from)?;
    column.name.clone_from(&rename.to);
    (renamed_live == *desired).then_some(*rename)
}

fn retarget_sqlite_fk_definition(
    definition: &str,
    target: &str,
    backend: &dyn SchemaRenderer,
) -> Option<String> {
    let marker = "REFERENCES";
    let marker_start = definition.find(marker)?;
    let after_marker = marker_start + marker.len();
    let target_start =
        after_marker + definition[after_marker..].find(|ch: char| !ch.is_whitespace())?;
    let target_end = target_start
        + definition[target_start..]
            .find(|ch: char| ch == '(' || ch.is_whitespace())
            .unwrap_or(definition.len() - target_start);
    let mut rewritten = definition.to_string();
    rewritten.replace_range(target_start..target_end, &backend.quote_ident(target));
    Some(rewritten)
}

fn retarget_sqlite_self_references_in_schema(
    schema: &mut serde_json::Value,
    table: &str,
    target: &str,
) {
    let Some(fields) = schema.as_object_mut() else {
        return;
    };
    for definition in fields.values_mut() {
        if definition
            .get("refTarget")
            .and_then(serde_json::Value::as_str)
            == Some(table)
        {
            if let Some(definition) = definition.as_object_mut() {
                definition.insert(
                    "refTarget".to_string(),
                    serde_json::Value::String(target.to_string()),
                );
            }
        }
    }
}

fn sqlite_stored_create_for_pure_rename(
    table: &str,
    tmp_table: &str,
    snapshot: &TableSnapshot,
    backend: &dyn SchemaRenderer,
) -> Result<String, DeclarativeError> {
    let stored = snapshot.stored_create_sql.as_deref().ok_or_else(|| {
        DeclarativeError::Invalid(format!(
            "SQLite pure-rename rebuild of '{table}' has no stored CREATE TABLE SQL"
        ))
    })?;

    // Preserve the catalog-stored table body byte-for-byte, except for named
    // self-referential FKs: during the copy/drop phase they must point at the
    // replacement table, not the old table that is about to be dropped.
    let mut temporary_snapshot = snapshot.clone();
    for constraint in &mut temporary_snapshot.constraints {
        if constraint.kind == "FOREIGN KEY"
            && fk_target_table(&constraint.definition).as_deref() == Some(table)
        {
            constraint.definition = retarget_sqlite_fk_definition(
                &constraint.definition,
                tmp_table,
                backend,
            )
            .ok_or_else(|| {
                DeclarativeError::Invalid(format!(
                    "SQLite pure-rename rebuild of '{table}' could not retarget self-referential foreign key {:?}",
                    constraint.name
                ))
            })?;
        }
    }
    let stored_ddl = backend
        .stored_ddl()
        .expect("the SQLite renderer must provide stored-DDL analysis");
    let stored = stored_ddl.rewrite_stored_foreign_keys(
        table,
        stored,
        snapshot,
        &temporary_snapshot,
        backend,
    )?;
    let (open, _) = stored_ddl.create_body_bounds(&stored).ok_or_else(|| {
        DeclarativeError::Invalid(format!(
            "SQLite pure-rename rebuild of '{table}' could not parse its stored CREATE TABLE body"
        ))
    })?;
    Ok(format!(
        "CREATE TABLE {}{}",
        backend.quote_ident(table),
        &stored[open..]
    ))
}

fn sqlite_authored_primary_key_clause(
    table: &str,
    snapshot: &TableSnapshot,
    inject: &dyn InjectedPrimaryKey,
    backend: &dyn SchemaRenderer,
) -> Result<Option<String>, DeclarativeError> {
    let mut primary_keys = snapshot
        .constraints
        .iter()
        .filter(|constraint| constraint.kind == "PRIMARY KEY");
    let Some(primary_key) = primary_keys.next() else {
        return Ok(None);
    };
    if primary_keys.next().is_some() {
        return Err(DeclarativeError::Invalid(format!(
            "SQLite rebuild snapshot for '{table}' carries more than one PRIMARY KEY"
        )));
    }
    let columns = fk_local_columns(&primary_key.definition);
    if columns.is_empty() {
        return Err(DeclarativeError::Invalid(format!(
            "SQLite rebuild snapshot for '{table}' has an unreadable PRIMARY KEY definition"
        )));
    }
    if inject.primary_key() == Some(columns.as_slice()) {
        return Ok(None);
    }
    Ok(Some(format!(
        "CONSTRAINT {} {}",
        backend.quote_ident(&primary_key.name),
        primary_key.definition
    )))
}

fn append_sqlite_table_constraint(
    table: &str,
    create_sql: &str,
    constraint: &str,
    backend: &dyn SchemaRenderer,
) -> Result<String, DeclarativeError> {
    let stored_ddl = backend
        .stored_ddl()
        .expect("the SQLite renderer must provide stored-DDL analysis");
    let (open, close) = stored_ddl.create_body_bounds(create_sql).ok_or_else(|| {
        DeclarativeError::Invalid(format!(
            "SQLite rebuild of '{table}' could not parse its emitted CREATE TABLE body"
        ))
    })?;
    let body = &create_sql[open + 1..close];
    let insert_at = open + 1 + body.trim_end().len();
    let separator = if body.trim().is_empty() { "" } else { "," };
    let mut rendered = create_sql.to_string();
    rendered.insert_str(insert_at, &format!("{separator}\n  {constraint}"));
    Ok(rendered)
}

impl TableRebuildPolicy for SqliteTableRebuildPolicy {
    /// Refuse two `renameColumn` ops on ONE table in ONE envelope, on SQLite only.
    ///
    /// SQLite reconciles a rename with the 12-step table REBUILD, whose `CREATE` is
    /// rendered from [`TableSnapshot::stored_create_sql`] - the verbatim
    /// `sqlite_master.sql` text. That text is byte-faithful on purpose, so the rebuild
    /// can hand the identifier rewrite to SQLite's own `ALTER TABLE ... RENAME COLUMN`
    /// parser and let CHECKs, generated expressions, indexes and triggers follow the
    /// rename untouched. The engine therefore cannot synthesise an updated version of
    /// it for a SECOND rebuild in the same envelope without doing the lossy SQL rewrite
    /// that design avoids.
    ///
    /// MEASURED before adding this: the second rebuild kept the first rebuild's
    /// pre-rename `CREATE` while its value-copy list had moved on, and SQLite rejected
    /// the mismatch with `table people__zero_migrate_rebuild has no column named
    /// handle`. The transaction rolls back, so nothing was corrupted - but the
    /// migration could not apply and the error named an intermediate table rather than
    /// the repair.
    ///
    /// Deliberately NOT "two ops on one table": two `addColumn`s on one table lower and
    /// apply fine today, and refusing them would reject working migrations. Only the
    /// shape that was measured to fail is refused. The wider question - that several
    /// other arms also read live structure an earlier op can invalidate - is its own
    /// ticket, not this gate.
    fn refuse_repeat_column_rename_target(
        &self,
        dialect: &DialectId,
        ops: &[Op],
    ) -> Result<(), IrLowerError> {
        // Descends `Op::Dialectal` for the SAME reason the lowering below does: a
        // rename authored inside a leg is a rename SQLite runs, and it rebuilds the
        // table from the same stored CREATE text. Scanning the raw list let a wrapper
        // hide the second rename from this preflight while the rebuild still happened,
        // so the hazard survived and only the refusal that names it was lost.
        //
        // The SELECTED leg, not every leg: a rename sitting in the PostgreSQL leg is
        // never executed here and rebuilds nothing, so refusing on it would reject a
        // migration that is correct on this target. Selection uses the wire map's
        // exact `DialectId` lookup, the same operation as the neutral fold.
        //
        // One level deep is complete: a leg cannot hold a wrapper, refused by the
        // validator before lowering is reached.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for op in ops {
            let effective: &[Op] = match op {
                Op::Dialectal { legs } => legs.get(dialect).map(Vec::as_slice).unwrap_or(&[]),
                other => std::slice::from_ref(other),
            };
            for inner in effective {
                if let Op::RenameColumn { table, .. } = inner {
                    if !seen.insert(table.as_str()) {
                        return Err(IrLowerError::RepeatRenameTarget {
                            table: table.clone(),
                            dialect: dialect.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn pure_column_rename<'a>(
        &self,
        live: &TableSnapshot,
        desired: &TableSnapshot,
        renames: &[&'a ResolvedRename],
    ) -> Option<&'a ResolvedRename> {
        pure_sqlite_column_rename(live, desired, renames)
    }

    fn retarget_foreign_key_definition(&self, definition: &str, target: &str) -> Option<String> {
        retarget_sqlite_fk_definition(definition, target, self.schema_renderer())
    }

    fn retarget_self_references_in_schema(
        &self,
        schema: &mut serde_json::Value,
        table: &str,
        target: &str,
    ) {
        retarget_sqlite_self_references_in_schema(schema, table, target);
    }

    fn stored_create_for_pure_rename(
        &self,
        table: &str,
        temporary_table: &str,
        snapshot: &TableSnapshot,
    ) -> Result<String, DeclarativeError> {
        sqlite_stored_create_for_pure_rename(
            table,
            temporary_table,
            snapshot,
            self.schema_renderer(),
        )
    }

    fn authored_primary_key_clause(
        &self,
        table: &str,
        snapshot: &TableSnapshot,
        inject: &dyn InjectedPrimaryKey,
    ) -> Result<Option<String>, DeclarativeError> {
        sqlite_authored_primary_key_clause(table, snapshot, inject, self.schema_renderer())
    }

    fn append_table_constraint(
        &self,
        table: &str,
        create_sql: &str,
        constraint: &str,
    ) -> Result<String, DeclarativeError> {
        append_sqlite_table_constraint(table, create_sql, constraint, self.schema_renderer())
    }

    fn existing_table_needs_rebuild(
        &self,
        table: &str,
        live: &TableSnapshot,
        desired: &TableSnapshot,
        renames: &[&ResolvedRename],
    ) -> Option<String> {
        self.sqlite_existing_table_needs_rebuild(table, live, desired, renames)
    }
}
