use zeroship_schema::query::SqlDialect;

use crate::dml::{self, DmlError};
use crate::expr::CastTarget;
use crate::ir::{Op, TableRef};
use crate::ir_author::IrLowerError;

/// Dialect-specific DML/view/trigger rendering.
///
/// No method has a default body: adding a dialect requires an explicit impl for
/// every render decision. The single exhaustive dispatch match lives in
/// [`renderer`], so a third [`SqlDialect`] variant breaks there at compile time
/// until its renderer is implemented and wired.
pub(crate) trait DmlRenderer {
    fn qualify_table(&self, project_schema: &str, table: &str) -> Result<String, DmlError>;
    fn cast_target(&self, target: CastTarget) -> &'static str;
    fn render_concat_ws(&self, rendered: &[String]) -> String;
    fn render_split_part(&self, col_sql: &str, delim: &str, n: i64) -> Result<String, DmlError>;
    fn synth_now(&self) -> String;
    fn synth_uuid(&self) -> String;
    fn validate_view_materialized(&self, materialized: bool) -> Result<(), IrLowerError>;
    fn view_create_prefix(&self, materialized: bool, replace: bool)
        -> Result<String, IrLowerError>;
    fn view_replace_prelude(&self, qname: &str, replace: bool) -> Vec<String>;
    fn view_object_name(&self, name: &str, eff_schema: &str) -> Result<String, IrLowerError>;
    fn render_table_ref(&self, table: &TableRef, eff_schema: &str)
        -> Result<String, IrLowerError>;
    fn render_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<crate::vendor::VendorStatement>, IrLowerError>;
}

struct PostgresDmlRenderer;
struct SqliteDmlRenderer;

static POSTGRES_DML_RENDERER: PostgresDmlRenderer = PostgresDmlRenderer;
static SQLITE_DML_RENDERER: SqliteDmlRenderer = SqliteDmlRenderer;

pub(crate) fn renderer(dialect: SqlDialect) -> &'static dyn DmlRenderer {
    match dialect {
        SqlDialect::Postgres => &POSTGRES_DML_RENDERER,
        SqlDialect::Sqlite => &SQLITE_DML_RENDERER,
    }
}

fn quote_engine_ident_as_dml(what: &'static str, ident: &str) -> Result<String, IrLowerError> {
    dml::quote_ident_checked(ident)
        .map_err(|e| DmlError::InvalidIdentifier { what, value: e.value })
        .map_err(IrLowerError::DmlAssemble)
}

impl DmlRenderer for PostgresDmlRenderer {
    fn qualify_table(&self, project_schema: &str, table: &str) -> Result<String, DmlError> {
        let t = dml::quote_bare_ident("table", table)?;
        Ok(format!(
            "{}.{}",
            dml::quote_ident_checked(project_schema).map_err(|e| DmlError::InvalidIdentifier {
                what: "schema",
                value: e.value,
            })?,
            t
        ))
    }

    fn cast_target(&self, target: CastTarget) -> &'static str {
        match target {
            CastTarget::Text => "text",
            CastTarget::Integer => "integer",
            CastTarget::Real => "real",
            CastTarget::Boolean => "boolean",
            CastTarget::Blob => "bytea",
            CastTarget::Uuid => "uuid",
        }
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

    fn synth_uuid(&self) -> String {
        "gen_random_uuid()".to_string()
    }

    fn validate_view_materialized(&self, _materialized: bool) -> Result<(), IrLowerError> {
        Ok(())
    }

    fn view_create_prefix(
        &self,
        materialized: bool,
        replace: bool,
    ) -> Result<String, IrLowerError> {
        let mut create = String::from("CREATE ");
        if materialized {
            create.push_str("MATERIALIZED VIEW ");
        } else if replace {
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
            dml::quote_bare_ident("view", name)?
        ))
    }

    fn render_table_ref(
        &self,
        table: &TableRef,
        eff_schema: &str,
    ) -> Result<String, IrLowerError> {
        let mut sql = {
            let schema = table.schema.as_deref().unwrap_or(eff_schema);
            format!(
                "{}.{}",
                quote_engine_ident_as_dml("schema", schema)?,
                dml::quote_bare_ident("table", &table.name)?
            )
        };
        if let Some(alias) = table.alias.as_deref() {
            sql.push_str(" AS ");
            sql.push_str(&dml::quote_bare_ident("table alias", alias)?);
        }
        Ok(sql)
    }

    fn render_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<crate::vendor::VendorStatement>, IrLowerError> {
        let stmts = match crate::vendor::render_vendor_op(op, eff_schema) {
            Ok(stmts) => stmts,
            Err(crate::vendor::VendorError::UnsupportedTriggerAction { kind }) => {
                return Err(IrLowerError::TriggerUnsupported {
                    kind,
                    dialect: SqlDialect::Postgres,
                });
            }
            Err(e) => return Err(IrLowerError::Vendor(e)),
        };
        Ok(stmts)
    }
}

impl DmlRenderer for SqliteDmlRenderer {
    fn qualify_table(&self, _project_schema: &str, table: &str) -> Result<String, DmlError> {
        dml::quote_bare_ident("table", table)
    }

    fn cast_target(&self, target: CastTarget) -> &'static str {
        match target {
            CastTarget::Text => "text",
            CastTarget::Integer => "integer",
            CastTarget::Real => "real",
            CastTarget::Boolean => "integer",
            CastTarget::Blob => "blob",
            CastTarget::Uuid => "text",
        }
    }

    fn render_concat_ws(&self, rendered: &[String]) -> String {
        // rendered[0] is the delimiter; rendered[1..] are the values.
        // NULL-skipping join: coalesce each value with '' joined by the
        // delimiter would re-introduce empty fields; the pinned SQLite shape
        // for concat_ws is a fold that skips NULLs. We use the standard
        // equivalent: trim away the delimiter that a leading NULL would leave.
        // For the bounded value count we emit the explicit
        // `substr(<acc>, len(delim)+1)` head-trim of a `||`-fold where each
        // value contributes `delim || value` only when not NULL.
        let delim = &rendered[0];
        let values = &rendered[1..];
        // acc = '' ; for each v: acc = acc || (case when v is null then '' else delim||v end)
        // then strip the single leading delim.
        let mut fold = String::from("''");
        for v in values {
            fold = format!(
                "({fold} || (CASE WHEN ({v}) IS NULL THEN '' ELSE ({delim}) || ({v}) END))"
            );
        }
        // Strip the leading delimiter (length of the delim literal). Using
        // substr with instr-free fixed-prefix removal is only correct when the
        // delim is a fixed literal; concatWs's delim is a Literal by the op
        // shape, so this holds. We strip `length(delim)` leading chars.
        format!("substr({fold}, length({delim}) + 1)")
    }

    fn render_split_part(&self, col_sql: &str, delim: &str, n: i64) -> Result<String, DmlError> {
        // ENVELOPE (SQLite only): single-ASCII-byte delim, 1 <= n <= MAX_N.
        let bytes = delim.as_bytes();
        if bytes.len() != 1 || bytes[0] >= 0x80 {
            return Err(DmlError::UnrenderableExpr(format!(
                "c.fn.splitPart delimiter must be a single ASCII character (one byte, \
                 code point < 0x80) to lower portably on SQLite; got {delim:?}"
            )));
        }
        if n > dml::SPLIT_PART_MAX_N {
            return Err(DmlError::UnrenderableExpr(format!(
                "c.fn.splitPart part index n must be in 1..={} \
                 (the proven inline-unroll bound) to lower portably on SQLite; got {n}",
                dml::SPLIT_PART_MAX_N
            )));
        }
        let dc = char::from(bytes[0]);
        // Single-ASCII delimiter as an inline SQL string literal (`'` -> `''`).
        let d = if dc == '\'' { "''''".to_string() } else { format!("'{dc}'") };
        // cur0 = (col || 'd') - the sentinel-terminated string.
        let mut cur = format!("({col_sql} || {d})");
        // cur_i = substr(cur_i-1, instr(cur_i-1, 'd') + 1), i = 1..n-1.
        for _ in 1..n {
            cur = format!("substr({cur}, instr({cur}, {d}) + 1)");
        }
        // result = substr(cur_n-1, 1, instr(cur_n-1, 'd') - 1).
        Ok(format!("substr({cur}, 1, instr({cur}, {d}) - 1)"))
    }

    fn synth_now(&self) -> String {
        "CURRENT_TIMESTAMP".to_string()
    }

    fn synth_uuid(&self) -> String {
        "lower(hex(randomblob(16)))".to_string()
    }

    fn validate_view_materialized(&self, materialized: bool) -> Result<(), IrLowerError> {
        if materialized {
            return Err(IrLowerError::ViewUnsupported {
                kind: "materializedView",
                dialect: SqlDialect::Sqlite,
            });
        }
        Ok(())
    }

    fn view_create_prefix(
        &self,
        _materialized: bool,
        _replace: bool,
    ) -> Result<String, IrLowerError> {
        let mut create = String::from("CREATE ");
        create.push_str("VIEW ");
        Ok(create)
    }

    fn view_replace_prelude(&self, qname: &str, replace: bool) -> Vec<String> {
        if replace {
            vec![format!("DROP VIEW IF EXISTS {qname}")]
        } else {
            Vec::new()
        }
    }

    fn view_object_name(&self, name: &str, _eff_schema: &str) -> Result<String, IrLowerError> {
        Ok(dml::quote_bare_ident("view", name)?)
    }

    fn render_table_ref(
        &self,
        table: &TableRef,
        eff_schema: &str,
    ) -> Result<String, IrLowerError> {
        let mut sql = {
            if let Some(schema) = table.schema.as_deref() {
                if !schema.eq_ignore_ascii_case(eff_schema) {
                    return Err(IrLowerError::LowerCrossSchema(schema.to_string()));
                }
            }
            dml::quote_bare_ident("table", &table.name)?
        };
        if let Some(alias) = table.alias.as_deref() {
            sql.push_str(" AS ");
            sql.push_str(&dml::quote_bare_ident("table alias", alias)?);
        }
        Ok(sql)
    }

    fn render_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<crate::vendor::VendorStatement>, IrLowerError> {
        Ok(vec![crate::ir_author::render_sqlite_trigger_op(op, eff_schema)?])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_returns_expected_dml_renderer() {
        assert_eq!(renderer(SqlDialect::Postgres).synth_now(), "now()");
        assert_eq!(renderer(SqlDialect::Sqlite).synth_now(), "CURRENT_TIMESTAMP");
    }
}
