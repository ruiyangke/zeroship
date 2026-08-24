//! MySQL DDL emission, moved verbatim from the engine.

use std::collections::BTreeSet;

use zero_migrate_backend::ddl::{
    constraint_supports_fk_columns, fk_local_columns, fk_policy_tail, fk_referenced_columns,
    fk_target_table, generated_clause, index_supports_fk_columns, inline_checks_clause,
    inline_pk_for_column, render_index_order_suffix, should_render_table_pk, CreateTableRequest,
    DdlEmitter, GENERATED_PREFIX,
};
use zero_migrate_backend::schema::SchemaRenderer;
use zero_migrate_backend::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, GeneratedColumnSnapshot, IndexElementSnapshot,
    IndexSnapshot, TableSnapshot,
};
use zero_migrate_backend::spelling::ansi_double_quote_ident;
use zero_migrate_ir::dialect::DialectId;

// This module's vendor identity, read from the crate's ONE declaration of it.
use crate::DIALECT;

fn mysql_quote_ident(ident: &str) -> String {
    crate::schema::RENDERER.quote_ident(ident)
}

fn mysql_qualified(schema: &str, object: &str) -> String {
    format!(
        "{}.{}",
        mysql_quote_ident(schema),
        mysql_quote_ident(object)
    )
}

pub(super) fn emitter(project_schema: &str) -> Box<dyn DdlEmitter> {
    Box::new(MysqlEmitter {
        project_schema: project_schema.to_string(),
    })
}

fn mysql_generated_clause(generated: Option<&GeneratedColumnSnapshot>) -> String {
    generated_clause(generated)
}

fn mysql_identity_clause(c: &ColumnSnapshot) -> &'static str {
    if matches!(c.identity, Some(identity) if !identity.always) {
        " AUTO_INCREMENT"
    } else {
        ""
    }
}

fn primary_key_clause(inline_pk: bool) -> &'static str {
    if inline_pk {
        " PRIMARY KEY"
    } else {
        ""
    }
}

fn null_clause(c: &ColumnSnapshot) -> &'static str {
    if c.nullable || matches!(c.identity, Some(identity) if !identity.always) {
        ""
    } else {
        " NOT NULL"
    }
}

fn render_index_elements_mysql(idx: &IndexSnapshot) -> String {
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
            // the MySQL element render intentionally ignores them.
            IndexElementSnapshot::Column { name, order, .. } => {
                format!(
                    "{}{}",
                    mysql_quote_ident(name),
                    render_index_order_suffix(*order)
                )
            }
            IndexElementSnapshot::Expr(expr) => format!("({expr})"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn mysql_inline_fk_supporting_indexes<'a>(
    table: &'a TableSnapshot,
    inline_fks: &[&ConstraintSnapshot],
) -> Vec<&'a IndexSnapshot> {
    let mut names = BTreeSet::new();
    let mut indexes = Vec::new();
    for fk in inline_fks {
        let columns = fk_local_columns(&fk.definition);
        if table
            .constraints
            .iter()
            .any(|constraint| constraint_supports_fk_columns(constraint, &columns))
        {
            continue;
        }
        if let Some(index) = table
            .indexes
            .iter()
            .find(|index| index_supports_fk_columns(index, &columns))
        {
            if names.insert(index.name.as_str()) {
                indexes.push(index);
            }
        }
    }
    indexes
}

fn mysql_default_clause(default: Option<&str>) -> String {
    match default {
        Some("'{}'::jsonb") => " DEFAULT (JSON_OBJECT())".to_string(),
        Some("'[]'::jsonb") => " DEFAULT (JSON_ARRAY())".to_string(),
        Some(d) => {
            if let Some(expr) = d.strip_prefix(GENERATED_PREFIX) {
                format!(" GENERATED ALWAYS AS ({expr}) STORED")
            } else {
                // JSON container defaults are translated by the exact arms
                // above. Never rewrite an arbitrary rendered default: a text
                // value is allowed to contain the bytes `::jsonb` verbatim.
                format!(" DEFAULT {d}")
            }
        }
        None => String::new(),
    }
}

fn mysql_enum_type_from_check(definition: &str, col: &str) -> Option<String> {
    let prefix = format!("CHECK ({} IN (", ansi_double_quote_ident(col));
    let suffix = "))";
    let inner = definition.strip_prefix(&prefix)?.strip_suffix(suffix)?;
    Some(format!("ENUM({inner})"))
}

fn mysql_fk_policy_tail(definition: &str) -> String {
    let tail = fk_policy_tail(definition);
    tail.replace(" DEFERRABLE INITIALLY DEFERRED", "")
}

/// Re-spell a constraint `definition` body from its PostgreSQL quoting into MySQL's.
///
/// Constraint bodies are built once in PostgreSQL spelling so the desired snapshot
/// round-trips byte-for-byte against `pg_get_constraintdef` (see
/// `constraintdef_cols`); MySQL needs the same body with backtick-quoted
/// identifiers.
///
/// The rewrite tracks three states, because the escaping rules differ in each and
/// conflating them turns an identifier into SQL structure:
///
/// - Outside any quote, `"` opens an identifier and `'` opens a string.
/// - Inside a `"`-quoted identifier, `'` is an ordinary character, a doubled `""`
///   is one literal `"`, and a backtick MUST be doubled, since it is the delimiter
///   in the target spelling.
/// - Inside a `'`-quoted string, `"` is an ordinary character and `''` is one
///   literal quote.
///
/// Treating `'` as a string delimiter while inside an identifier is what breaks: a
/// column named `it's` swallows the identifier's closing `"`, and a column carrying
/// a backtick emits it undoubled, so `` a`), KEY `k2` (`id `` closes the identifier
/// early and contributes an index the author never declared.
fn mysql_requote_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_string = false;
    let mut in_ident = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if in_ident => {
                if chars.peek() == Some(&'"') {
                    // `""` inside a PostgreSQL identifier is one literal `"`, which
                    // needs no escaping between backticks.
                    chars.next();
                    out.push('"');
                } else {
                    in_ident = false;
                    out.push('`');
                }
            }
            '"' if !in_string => {
                in_ident = true;
                out.push('`');
            }
            '`' if in_ident => out.push_str("``"),
            '\'' if in_ident => out.push('\''),
            '\'' if !in_ident => {
                out.push(c);
                if in_string && chars.peek() == Some(&'\'') {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                } else {
                    in_string = !in_string;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

struct MysqlEmitter {
    project_schema: String,
}

impl MysqlEmitter {
    fn qualified(&self, object: &str) -> String {
        mysql_qualified(&self.project_schema, object)
    }

    /// MySQL's inline / stand-alone FK clause. (Was
    /// `DeclarativeAuthor::mysql_fk_clause`, moved VERBATIM.) The policy tail drops
    /// `DEFERRABLE INITIALLY DEFERRED`, which MySQL does not accept, via
    /// [`mysql_fk_policy_tail`].
    fn fk_clause(&self, fk: &ConstraintSnapshot) -> String {
        let columns = fk_local_columns(&fk.definition)
            .iter()
            .map(|column| mysql_quote_ident(column))
            .collect::<Vec<_>>()
            .join(", ");
        let target = fk_target_table(&fk.definition).unwrap_or_default();
        let referenced_columns = fk_referenced_columns(&fk.definition)
            .iter()
            .map(|column| mysql_quote_ident(column))
            .collect::<Vec<_>>()
            .join(", ");
        let policy = mysql_fk_policy_tail(&fk.definition);
        format!(
            "CONSTRAINT {} FOREIGN KEY ({columns}) REFERENCES {} ({referenced_columns}){policy}",
            mysql_quote_ident(&fk.name),
            self.qualified(&target),
        )
    }
}

impl DdlEmitter for MysqlEmitter {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn fk_clause(&self, fk: &ConstraintSnapshot) -> String {
        MysqlEmitter::fk_clause(self, fk)
    }

    fn alter_table_ref(&self, table: &str) -> String {
        self.qualified(table)
    }

    fn drop_foreign_key_up(&self, table: &str, name: &str) -> Option<String> {
        Some(format!(
            "ALTER TABLE {} DROP FOREIGN KEY {}",
            self.qualified(table),
            mysql_quote_ident(name),
        ))
    }

    fn alter_column_refs(&self, table: &str, column: &str) -> (String, String) {
        (self.qualified(table), mysql_quote_ident(column))
    }

    fn indexes_inlined_by_create(&self, req: &CreateTableRequest<'_>) -> Vec<String> {
        mysql_inline_fk_supporting_indexes(req.snapshot, req.inline_fks)
            .into_iter()
            .map(|index| index.name.clone())
            .collect()
    }

    fn create_table(&self, req: &CreateTableRequest<'_>) -> Vec<String> {
        let (table, t, inline_fks) = (req.table, req.snapshot, req.inline_fks);
        let mut parts: Vec<String> = Vec::new();
        let mut consumed_enum_checks = BTreeSet::new();
        for (column_index, c) in t.columns.iter().enumerate() {
            let inline_pk = inline_pk_for_column(&crate::fold::POLICY, table, t, &c.name);
            let enum_check_name = req
                .enum_check_names
                .get(column_index)
                .expect("core supplies one enum CHECK name per snapshot column")
                .clone();
            let enum_type = t
                .constraints
                .iter()
                .find(|chk| chk.kind == "CHECK" && chk.name == enum_check_name)
                .and_then(|chk| mysql_enum_type_from_check(&chk.definition, &c.name));
            if enum_type.is_some() {
                consumed_enum_checks.insert(enum_check_name);
            }
            // The CHECK fold replaces the base type with a native enum, then hands
            // that override back to the same vendor renderer so its collation rule
            // remains the one spelling authority.
            let ty = if let Some(enum_type) = enum_type {
                let mut enum_column = c.clone();
                enum_column.ddl_type_override = Some(enum_type);
                crate::schema::RENDERER.column_type(&enum_column, inline_pk)
            } else {
                crate::schema::RENDERER.column_type(c, inline_pk)
            };
            let pk = primary_key_clause(inline_pk);
            let null = null_clause(c);
            let identity = mysql_identity_clause(c);
            let generated = mysql_generated_clause(c.generated.as_ref());
            let default = mysql_default_clause(c.default.as_deref());
            let checks = inline_checks_clause(c);
            parts.push(format!(
                "{} {}{}{}{}{}{}{}",
                mysql_quote_ident(&c.name),
                ty,
                identity,
                generated,
                pk,
                null,
                default,
                checks,
            ));
        }
        // InnoDB creates an implicit child index when an inline FK has no index
        // in the same CREATE TABLE statement. The planned index must therefore be
        // inline too; emitting it only as a later CREATE INDEX would conceal the
        // implicit object from the preview and can leave a redundant index. These
        // exact snapshots are skipped from the follow-on index-unit loop below.
        for idx in mysql_inline_fk_supporting_indexes(t, inline_fks) {
            let unique = if idx.unique { "UNIQUE " } else { "" };
            parts.push(format!(
                "{unique}KEY {} ({})",
                mysql_quote_ident(&idx.name),
                render_index_elements_mysql(idx)
            ));
        }
        for fk in inline_fks {
            parts.push(self.fk_clause(fk));
        }
        for c in &t.constraints {
            if consumed_enum_checks.contains(&c.name) {
                continue;
            }
            if should_render_table_pk(&crate::fold::POLICY, table, t, c) || c.kind == "CHECK" || c.kind == "UNIQUE" {
                parts.push(format!(
                    "CONSTRAINT {} {}",
                    mysql_quote_ident(&c.name),
                    mysql_requote_sql(&c.definition)
                ));
            }
        }
        vec![format!(
            "CREATE TABLE {} ({})",
            self.qualified(table),
            parts.join(", ")
        )]
    }

    fn add_column(&self, table: &str, c: &ColumnSnapshot) -> (Vec<String>, Option<String>) {
        let inline_pk = false;
        let null = null_clause(c);
        let generated = mysql_generated_clause(c.generated.as_ref());
        let default = mysql_default_clause(c.default.as_deref());
        let identity = mysql_identity_clause(c);
        let checks = inline_checks_clause(c);
        let table_ref = self.qualified(table);
        let up = format!(
            "ALTER TABLE {} ADD COLUMN {} {}{}{}{}{}{}",
            table_ref,
            mysql_quote_ident(&c.name),
            crate::schema::RENDERER.column_type(c, inline_pk),
            identity,
            generated,
            null,
            default,
            checks,
        );
        let down = format!(
            "ALTER TABLE {} DROP COLUMN {}",
            table_ref,
            mysql_quote_ident(&c.name)
        );
        (vec![up], Some(down))
    }

    fn create_index(&self, table: &str, idx: &IndexSnapshot) -> (String, String) {
        let unique = if idx.unique { "UNIQUE " } else { "" };
        let col_list = render_index_elements_mysql(idx);
        (
            format!(
                "CREATE {unique}INDEX {} ON {} ({col_list}){}",
                mysql_quote_ident(&idx.name),
                self.qualified(table),
                idx.predicate
                    .as_deref()
                    .map(|p| format!(" WHERE {p}"))
                    .unwrap_or_default(),
            ),
            format!(
                "DROP INDEX {} ON {}",
                mysql_quote_ident(&idx.name),
                self.qualified(table)
            ),
        )
    }

    fn drop_table_up(&self, table: &str) -> String {
        format!("DROP TABLE {}", self.qualified(table))
    }

    fn rename_table(&self, table: &str, to: &str) -> (String, String) {
        (
            format!(
                "RENAME TABLE {} TO {}",
                self.qualified(table),
                self.qualified(to)
            ),
            format!(
                "RENAME TABLE {} TO {}",
                self.qualified(to),
                self.qualified(table)
            ),
        )
    }

    fn drop_column_up(&self, table: &str, col: &str) -> String {
        format!(
            "ALTER TABLE {} DROP COLUMN {}",
            self.qualified(table),
            mysql_quote_ident(col)
        )
    }

    fn drop_index_up(&self, table: Option<&str>, idx_name: &str) -> String {
        match table {
            Some(table) => format!(
                "DROP INDEX {} ON {}",
                mysql_quote_ident(idx_name),
                self.qualified(table)
            ),
            None => format!("DROP INDEX {}", mysql_quote_ident(idx_name)),
        }
    }

    fn create_partition(
        &self,
        _name: &str,
        _of: &str,
        _bounds: &zero_migrate_ir::ir::PartitionBounds,
    ) -> Option<(String, String)> {
        // This backend's current engine projection collapses authored
        // partitions instead of emitting native partition-relation DDL.
        None
    }

    fn attach_partition(
        &self,
        _parent: &str,
        _name: &str,
        _bounds: &zero_migrate_ir::ir::PartitionBounds,
    ) -> Option<(String, String)> {
        None
    }

    fn detach_partition(&self, _parent: &str, _name: &str, _concurrently: bool) -> Option<String> {
        None
    }

    fn drop_partition(&self, _name: &str, _cascade: bool) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::mysql_default_clause;

    #[test]
    fn mysql_text_default_preserves_jsonb_like_substring() {
        assert_eq!(
            mysql_default_clause(Some("'foo::jsonb'")),
            " DEFAULT 'foo::jsonb'"
        );
        assert_eq!(
            mysql_default_clause(Some("'{}'::jsonb")),
            " DEFAULT (JSON_OBJECT())"
        );
        assert_eq!(
            mysql_default_clause(Some("'[]'::jsonb")),
            " DEFAULT (JSON_ARRAY())"
        );
        assert_eq!(
            mysql_default_clause(Some("(JSON_OBJECT())")),
            " DEFAULT (JSON_OBJECT())"
        );
        assert_eq!(
            mysql_default_clause(Some("(JSON_ARRAY())")),
            " DEFAULT (JSON_ARRAY())"
        );
    }
}
