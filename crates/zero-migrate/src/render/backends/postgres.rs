//! PostgreSQL SQL spelling. The future `zero-migrate-pg`.

use crate::model::expr::{CastTarget, ExtractField, ScalarFn};
use crate::model::ir::TableRef;
use crate::model::ir::{IrScalar, Op, TriggerAction};
use crate::render::dml::{self, DmlError};
use crate::render::lower::IrLowerError;
use crate::render::renderer::{Capability, DialectSupports, DmlRenderer};
use crate::schema::query::SqlDialect;

/// This module's own vendor identity — the ONE dialect literal it is allowed to
/// name. See `backends/mod.rs` for why. Deleting this const (and the
/// `DIALECT`-shaped error fields it feeds) is the whole of the edit this module
/// needs when it becomes its own crate.
const DIALECT: SqlDialect = SqlDialect::Postgres;

pub(super) struct PostgresDmlRenderer;

pub(super) static RENDERER: PostgresDmlRenderer = PostgresDmlRenderer;

fn quote_engine_ident_as_dml(what: &'static str, ident: &str) -> Result<String, IrLowerError> {
    dml::quote_ident_checked_for_dialect(ident, DIALECT)
        .map_err(|e| DmlError::InvalidIdentifier {
            what,
            value: e.value,
        })
        .map_err(IrLowerError::DmlAssemble)
}

impl DmlRenderer for PostgresDmlRenderer {
    fn quote_ident(&self, ident: &str) -> String {
        dml::escape_quote_ident(ident)
    }

    fn qualify_table(&self, project_schema: &str, table: &str) -> Result<String, DmlError> {
        let t = dml::quote_bare_ident_for_dialect("table", table, DIALECT)?;
        Ok(format!(
            "{}.{}",
            dml::quote_ident_checked_for_dialect(project_schema, DIALECT).map_err(|e| {
                DmlError::InvalidIdentifier {
                    what: "schema",
                    value: e.value,
                }
            })?,
            t
        ))
    }

    fn cast_target(&self, target: CastTarget) -> &'static str {
        match target {
            CastTarget::Text => "text",
            CastTarget::Int => "integer",
            CastTarget::Real => "real",
            CastTarget::Boolean => "boolean",
            CastTarget::Bytes => "bytea",
            CastTarget::Uuid => "uuid",
        }
    }

    fn placeholder(&self, n: usize) -> String {
        format!("${n}")
    }

    fn inline_string_literal(&self, s: &str) -> String {
        dml::sql_string_literal(s)
    }

    fn inline_decimal_literal(&self, d: &str) -> String {
        d.to_string()
    }

    fn inline_bytes_literal(&self, bytes: &[u8]) -> String {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        format!("decode({}, 'base64')", dml::sql_string_literal(&encoded))
    }

    fn render_in_list(
        &self,
        expr: &str,
        elems: &[IrScalar],
        negated: bool,
        joiner: &str,
    ) -> Result<String, DmlError> {
        let rendered: Result<Vec<_>, _> = elems.iter().map(dml::render_in_list_elem_pg).collect();
        let (cmp, quantifier) = if negated { ("<>", "ALL") } else { ("=", "ANY") };
        Ok(format!(
            "({expr} {cmp} {quantifier} (ARRAY[{}]))",
            rendered?.join(joiner)
        ))
    }

    fn render_regex_match(&self, expr: &str, pattern: &str) -> Result<String, DmlError> {
        Ok(format!(
            "({expr} ~ {})",
            dml::pg_text_literal(pattern, "PG regex pattern")?
        ))
    }

    fn render_extract(&self, field: ExtractField, expr: &str) -> String {
        format!("EXTRACT({} FROM {expr})", dml::extract_field_name(field))
    }

    fn render_concat(&self, l: &str, r: &str) -> String {
        format!("({l} || {r})")
    }

    fn render_distinct_from(&self, l: &str, r: &str) -> String {
        format!("({l} IS DISTINCT FROM {r})")
    }

    fn render_scalar_fn_override(&self, _f: ScalarFn, _args: &[String]) -> Option<String> {
        // PostgreSQL spells every allow-listed scalar the way the shared table
        // does; the portable INTENT and the native name coincide here.
        None
    }

    fn render_is_true(&self, operand: &str) -> String {
        format!("({operand} IS TRUE)")
    }

    fn render_is_false(&self, operand: &str) -> String {
        format!("({operand} IS FALSE)")
    }

    fn render_concat_ws(&self, rendered: &[String]) -> String {
        format!("concat_ws({})", rendered.join(", "))
    }

    fn render_split_part(&self, col_sql: &str, delim: &str, n: i64) -> Result<String, DmlError> {
        let d = format!("'{}'", delim.replace('\'', "''"));
        Ok(format!("split_part({col_sql}, {d}, {n})"))
    }

    fn synth_now(&self) -> String {
        "now()".to_string()
    }

    fn uuid_v4(&self) -> String {
        "gen_random_uuid()".to_string()
    }

    fn uuid_v7(&self) -> Result<String, DmlError> {
        Ok("uuidv7()".to_string())
    }

    fn validate_view_materialized(&self, materialized: bool) -> Result<(), IrLowerError> {
        if materialized && !DIALECT.supports(Capability::MaterializedView) {
            return Err(IrLowerError::ViewUnsupported {
                kind: "materializedView",
                dialect: DIALECT,
            });
        }
        Ok(())
    }

    fn view_create_prefix(
        &self,
        materialized: bool,
        replace: bool,
    ) -> Result<String, IrLowerError> {
        // Postgres has no `CREATE OR REPLACE MATERIALIZED VIEW`. Rather than
        // silently dropping `replace` (which would leave a populated matview in
        // place under a "replace" request) or destructively DROP+CREATE it, fail
        // closed so the author resolves the contradiction explicitly.
        if materialized && replace {
            return Err(IrLowerError::UnsupportedOp(
                "validated createView replace+materialized reached lower",
            ));
        }
        let mut create = String::from("CREATE ");
        if materialized {
            create.push_str("MATERIALIZED VIEW ");
        } else if replace && DIALECT.supports(Capability::CreateOrReplaceView) {
            create.push_str("OR REPLACE VIEW ");
        } else {
            create.push_str("VIEW ");
        }
        Ok(create)
    }

    fn view_replace_prelude(&self, _qname: &str, _replace: bool) -> Vec<String> {
        Vec::new()
    }

    fn view_object_name(&self, name: &str, eff_schema: &str) -> Result<String, IrLowerError> {
        Ok(format!(
            "{}.{}",
            quote_engine_ident_as_dml("schema", eff_schema)?,
            dml::quote_bare_ident_for_dialect("view", name, DIALECT)?
        ))
    }

    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError> {
        let mut sql = {
            let schema = table.schema.as_deref().unwrap_or(eff_schema);
            format!(
                "{}.{}",
                quote_engine_ident_as_dml("schema", schema)?,
                dml::quote_bare_ident_for_dialect("table", &table.name, DIALECT)?
            )
        };
        if let Some(alias) = table.alias.as_deref() {
            sql.push_str(" AS ");
            sql.push_str(&dml::quote_bare_ident_for_dialect(
                "table alias",
                alias,
                DIALECT,
            )?);
        }
        Ok(sql)
    }

    fn render_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<crate::render::vendor::VendorStatement>, IrLowerError> {
        if let Op::CreateTrigger {
            action: TriggerAction::Body { .. },
            ..
        } = op
        {
            if !DIALECT.supports(Capability::TriggerBody) {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "triggerBody",
                    dialect: DIALECT,
                });
            }
        }
        let stmts = match crate::render::vendor::render_vendor_op(op, eff_schema) {
            Ok(stmts) => stmts,
            Err(crate::render::vendor::VendorError::UnsupportedTriggerAction { kind }) => {
                return Err(IrLowerError::TriggerUnsupported {
                    kind,
                    dialect: DIALECT,
                });
            }
            Err(e) => return Err(IrLowerError::Vendor(e)),
        };
        Ok(stmts)
    }
}
