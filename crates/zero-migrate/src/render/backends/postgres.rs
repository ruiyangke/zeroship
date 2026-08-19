//! PostgreSQL SQL spelling. The future `zero-migrate-pg`.

use crate::model::expr::CastTarget;
use crate::model::ir::TableRef;
use crate::model::ir::{Op, TriggerAction};
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
    dml::quote_ident_checked(ident)
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
            CastTarget::Int => "integer",
            CastTarget::Real => "real",
            CastTarget::Boolean => "boolean",
            CastTarget::Bytes => "bytea",
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
            dml::quote_bare_ident("view", name)?
        ))
    }

    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError> {
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
