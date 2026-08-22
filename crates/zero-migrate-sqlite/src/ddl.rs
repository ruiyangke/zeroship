//! SQLite DDL emission, moved verbatim from the engine.

use zero_migrate_backend::ddl::{
    default_clause, fk_local_columns, fk_policy_tail, fk_referenced_columns, fk_target_table,
    generated_clause, inline_checks_clause, inline_pk_for_column, render_index_order_suffix,
    should_render_table_pk, CreateTableRequest, DdlEmitter,
};
use zero_migrate_backend::schema::SchemaRenderer;
use zero_migrate_backend::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot, IndexSnapshot,
};
use zero_migrate_ir::dialect::{DialectId, SQLITE};

/// This module's own vendor identity.
const DIALECT: DialectId = SQLITE;

fn sqlite_ident(ident: &str) -> String {
    crate::schema::RENDERER.quote_ident(ident)
}

fn primary_key_clause(c: &ColumnSnapshot, inline_pk: bool) -> &'static str {
    if crate::schema::sqlite_auto_increment_identity_pk(c, inline_pk) {
        " PRIMARY KEY AUTOINCREMENT"
    } else if inline_pk {
        " PRIMARY KEY"
    } else {
        ""
    }
}

fn null_clause(c: &ColumnSnapshot, inline_pk: bool) -> &'static str {
    if c.nullable || crate::schema::sqlite_auto_increment_identity_pk(c, inline_pk) {
        ""
    } else {
        " NOT NULL"
    }
}

pub(super) fn emitter(project_schema: &str) -> Box<dyn DdlEmitter> {
    Box::new(SqliteEmitter {
        project_schema: project_schema.to_string(),
    })
}

fn render_index_elements_sqlite(idx: &IndexSnapshot) -> String {
    let elements = if idx.elements.is_empty() {
        idx.columns
            .iter()
            .map(|c| IndexElementSnapshot::column(c.clone()))
            .collect::<Vec<_>>()
    } else {
        idx.elements.clone()
    };
    elements
        .iter()
        .map(|element| match element {
            // opclass/collation are PG-only (refused at validate before lower), so
            // the SQLite element render intentionally ignores them.
            IndexElementSnapshot::Column { name, order, .. } => {
                format!(
                    "{}{}",
                    sqlite_ident(name),
                    render_index_order_suffix(*order)
                )
            }
            IndexElementSnapshot::Expr(expr) => format!("({expr})"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn fk_ddl_local_cols(cols: &[String]) -> String {
    cols.iter()
        .map(|c| sqlite_ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}

fn fk_ddl_referenced_cols(cols: &[String]) -> String {
    cols.iter()
        .map(|c| {
            if c == "id" {
                "id".to_string()
            } else {
                sqlite_ident(c)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Confined-`SQLite` DDL emitter — unqualified (`main` = the app file), inline
/// `/* … */` sentinels, plain b-tree indexes (no `USING` / `WITH`). Byte-identical
/// to the former `SQLite` arm of each render method.
struct SqliteEmitter {
    project_schema: String,
}

impl SqliteEmitter {
    fn qualified(&self, object: &str) -> String {
        format!(
            "{}.{}",
            sqlite_ident(&self.project_schema),
            sqlite_ident(object)
        )
    }
}

impl DdlEmitter for SqliteEmitter {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn fk_clause(&self, fk: &ConstraintSnapshot) -> String {
        let cols = fk_local_columns(&fk.definition);
        let target = fk_target_table(&fk.definition).unwrap_or_default();
        let ref_cols = fk_referenced_columns(&fk.definition);
        let policy = fk_policy_tail(&fk.definition);
        format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}){}",
            sqlite_ident(&fk.name),
            fk_ddl_local_cols(&cols),
            self.qualified(&target),
            fk_ddl_referenced_cols(&ref_cols),
            policy,
        )
    }

    fn indexes_inlined_by_create(&self, req: &CreateTableRequest<'_>) -> Vec<String> {
        req.injected_indexes.to_vec()
    }

    fn create_table(&self, req: &CreateTableRequest<'_>) -> Vec<String> {
        let (table, t) = (req.table, req.snapshot);
        let mut parts: Vec<String> = Vec::new();
        for c in &t.columns {
            let inline_pk = inline_pk_for_column(table, t, &c.name);
            let ty = crate::schema::RENDERER.column_type(c, inline_pk);
            let pk = primary_key_clause(c, inline_pk);
            let null = null_clause(c, inline_pk);
            let generated = generated_clause(c.generated.as_ref());
            let default = default_clause(c.default.as_deref());
            let checks = inline_checks_clause(c);
            let enc = c
                .encryption_sentinel
                .as_deref()
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            let sqlite_inline_sentinel = if c.encryption_sentinel.is_none() {
                c.comment_sentinel
                    .as_deref()
                    .map(|s| format!(" /* {s} */"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            parts.push(format!(
                "{} {}{}{}{}{}{}{}{}",
                sqlite_ident(&c.name),
                ty,
                enc,
                sqlite_inline_sentinel,
                generated,
                pk,
                null,
                default,
                checks,
            ));
        }
        // Note the missing `req.inline_fks` loop, which is not an omission: SQLite
        // inlines every FK off the snapshot below, because it has no late
        // `ADD CONSTRAINT` to defer one to.
        for c in &t.constraints {
            if should_render_table_pk(table, t, c)
                || c.kind == "CHECK"
                || c.kind == "UNIQUE"
                || c.kind == "FOREIGN KEY"
            {
                parts.push(format!(
                    "CONSTRAINT {} {}",
                    sqlite_ident(&c.name),
                    c.definition
                ));
            }
        }
        let mut statements = vec![format!(
            "CREATE TABLE {} ({})",
            sqlite_ident(table),
            parts.join(", ")
        )];
        // The policy-injected indexes ride INSIDE the create payload here, which is
        // why the differ's follow-on index loop skips them on this dialect. A caller
        // with no inject to give (PostgreSQL's or MySQL's, neither of which reaches
        // this impl) contributes none.
        for idx in t
            .indexes
            .iter()
            .filter(|idx| req.injected_indexes.contains(&idx.name))
        {
            let (up, _) = self.create_index(table, idx);
            statements.push(up);
        }
        statements
    }

    fn add_column(&self, table: &str, c: &ColumnSnapshot) -> (Vec<String>, Option<String>) {
        let inline_pk = false;
        let null = null_clause(c, inline_pk);
        let generated = generated_clause(c.generated.as_ref());
        let default = default_clause(c.default.as_deref());
        let checks = inline_checks_clause(c);
        // inline `/* zero-migrate:enc:… */` for an encrypted column added
        // after the table exists.
        let enc = c
            .encryption_sentinel
            .as_deref()
            .map(|s| format!(" {s}"))
            .unwrap_or_default();
        // on SQLite the table is `main` (the app file): emit an UNqualified
        // `ALTER TABLE <t> ADD COLUMN …`. A schema-qualified `"schema"."t"` would
        // resolve to no table ("no such table").
        let table_ref = sqlite_ident(table);
        // on SQLite the mask sentinel rides INLINE in the column clause
        // (there is NO `COMMENT ON COLUMN` in SQLite — it is a syntax error). SQLite
        // preserves the inline `/* … */` comment through `ADD COLUMN` in
        // `sqlite_master.sql` (verified), so the drift recovery
        // (`recover_inline_sentinel`) round-trips it from the stored CREATE text
        // exactly like a create-time sentinel.
        //
        // `comment_sentinel` holds the BARE body (`zero-migrate:mask:…` / `zero-migrate:enc:…`, no
        // `/* */`); the SQLite inline form needs the `/* */` wrapper. The ENCRYPTED
        // column case is already covered by `enc` above (`encryption_sentinel` is the
        // pre-wrapped `/* zero-migrate:enc:… */` form), so only the MASKED-SIBLING case
        // (`comment_sentinel` set, `encryption_sentinel` unset) rides here — wrapped.
        let sqlite_inline_sentinel = if c.encryption_sentinel.is_none() {
            c.comment_sentinel
                .as_deref()
                .map(|s| format!(" /* {s} */"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let ty = crate::schema::RENDERER.column_type(c, inline_pk);
        let up = format!(
            "ALTER TABLE {} ADD COLUMN {} {}{}{}{}{}{}{}",
            table_ref,
            sqlite_ident(&c.name),
            ty,
            enc,
            sqlite_inline_sentinel,
            generated,
            null,
            default,
            checks,
        );
        let down = format!(
            "ALTER TABLE {} DROP COLUMN {}",
            table_ref,
            sqlite_ident(&c.name)
        );
        // SQLite ADD COLUMN is a SINGLE statement (the sentinel rides inline); the
        // structural list therefore has exactly one element.
        (vec![up], Some(down))
    }

    fn create_index(&self, table: &str, idx: &IndexSnapshot) -> (String, String) {
        let unique = if idx.unique { "UNIQUE " } else { "" };
        let col_list = render_index_elements_sqlite(idx);
        // SQLite indexes are UNqualified (`main` = the app file), and
        // SQLite has no `USING <method>` / `WITH (lists=…)` (those PG access-method
        // clauses are emitted only on the PG arm; a SQLite B-tree is the only kind
        // the additive index path emits). The schema qualifier is on neither the
        // index name nor the table.
        (
            format!(
                "CREATE {unique}INDEX IF NOT EXISTS {} ON {} ({col_list}){}",
                sqlite_ident(&idx.name),
                sqlite_ident(table),
                idx.predicate
                    .as_deref()
                    .map(|p| format!(" WHERE {p}"))
                    .unwrap_or_default(),
            ),
            format!("DROP INDEX IF EXISTS {}", sqlite_ident(&idx.name)),
        )
    }

    fn drop_table_up(&self, table: &str) -> String {
        format!("DROP TABLE {}", sqlite_ident(table))
    }

    fn rename_table(&self, table: &str, to: &str) -> (String, String) {
        // SQLite has native `ALTER TABLE <old> RENAME TO <new>` (a `main`-scoped
        // metadata rewrite). Both names are UNqualified `main` names — a
        // schema-qualified ref would resolve to no table. `down` is the inverse.
        (
            format!(
                "ALTER TABLE {} RENAME TO {}",
                sqlite_ident(table),
                sqlite_ident(to)
            ),
            format!(
                "ALTER TABLE {} RENAME TO {}",
                sqlite_ident(to),
                sqlite_ident(table)
            ),
        )
    }

    fn drop_column_up(&self, table: &str, col: &str) -> String {
        // SQLite ≥ 3.35 has native `ALTER TABLE … DROP COLUMN`; emit it
        // UNqualified (`main` = the app file). A schema-qualified `"schema"."t"` would
        // resolve to no table.
        format!(
            "ALTER TABLE {} DROP COLUMN {}",
            sqlite_ident(table),
            sqlite_ident(col)
        )
    }

    fn drop_index_up(&self, _table: Option<&str>, idx_name: &str) -> String {
        // on SQLite an index lives UNqualified in `main` (the app file).
        // A schema-qualified `DROP INDEX "schema"."ix"` does NOT error on SQLite — it
        // SILENTLY no-ops (the qualified name never resolves), reporting success while
        // the index survives: silent drift, the dangerous failure mode. Emit the
        // unqualified `DROP INDEX <name>` so the index is ACTUALLY dropped.
        format!("DROP INDEX {}", sqlite_ident(idx_name))
    }
}
