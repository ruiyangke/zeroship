//! SQLite SQL spelling. The future `zero-migrate-sqlite`.
//!
//! The trigger spelling at the bottom of this file arrived here in step 3 of
//! `docs/proposals/pluggable-backends.md`, from `render::lower`. It is a DIRECTORY
//! MOVE and nothing else: same functions, same bytes emitted, one const renamed. It
//! was the last SQLite render path living in the 17k-line core lowerer, and its own
//! doc had already said so — `backends/sqlite.rs`'s `render_trigger_op` carried a
//! note calling the delegation "a POINTER to work that `lower.rs`'s own step-3 pass
//! has to finish, not a boundary that is done". This is that pass. MySQL's trigger
//! spelling was the worked example of where it lands.

use crate::model::expr::{CastTarget, ExtractField, ScalarFn};
use crate::model::ir::{
    ForEach, IrScalar, Op, RaiseLevel, TableRef, TriggerAction, TriggerEvent, TriggerStmt,
};
use crate::render::dml::{self, DmlError};
use crate::render::lower::IrLowerError;
use crate::render::renderer::{Capability, DialectSupports, DmlRenderer};
use crate::render::step::BindValue;
use crate::schema::query::SqlDialect;

/// This module's own vendor identity — the ONE dialect literal it is allowed to
/// name. See `backends/mod.rs`.
///
/// # It absorbed the trigger renderer's const on the way in
///
/// `render_sqlite_trigger_op` and the two helpers below it named this vendor
/// nineteen times between them while they lived in `render::lower`: thirteen
/// capability and inline-render arguments that were already right, and six identifier
/// quotes that were NOT. Those six called the PostgreSQL-pinned
/// `dml::quote_bare_ident`, so every identifier in a rendered SQLite trigger was
/// spelled by `PostgresDmlRenderer::quote_ident` — correct only because both vendors
/// spell an identifier `"x"`, and a hard blocker on extracting a `zero-migrate-sqlite`
/// crate that does not need `zero-migrate-postgres` at RUNTIME. A crate-extraction
/// spike proved the reach was live rather than theoretical: it rendered a
/// `createTrigger` from inside the extracted crate and got PostgreSQL's marker back
/// in the SQLite trigger SQL.
///
/// Routing those six through `quote_bare_ident_for_dialect` was the fix; folding the
/// other thirteen into a single const is what made it stay fixed. That const was
/// `SQLITE_TRIGGER_DIALECT`, a `lower.rs`-local stand-in for the rule this file
/// already obeyed, and its whole purpose was to make the eventual move of those three
/// functions a RELOCATION rather than an edit. It worked: the move renamed one
/// identifier and touched nothing else, and this const is the thing it was renamed to.
///
/// Pinned by `tests/dialect_matrix/sqlite_trigger_quoting_reaches_postgres.rs`, whose
/// count went 6 → 0 when the fix landed and whose subject-anchor followed the three
/// functions here.
const DIALECT: SqlDialect = SqlDialect::Sqlite;

#[derive(Debug)]
pub(super) struct SqliteDmlRenderer;

pub(super) static RENDERER: SqliteDmlRenderer = SqliteDmlRenderer;

impl DmlRenderer for SqliteDmlRenderer {
    fn quote_ident(&self, ident: &str) -> String {
        super::ansi_double_quote_ident(ident)
    }

    fn qualify_table(&self, _project_schema: &str, table: &str) -> Result<String, DmlError> {
        dml::quote_bare_ident_for_dialect("table", table, DIALECT)
    }

    fn cast_target(&self, target: CastTarget) -> &'static str {
        match target {
            CastTarget::Text => "text",
            CastTarget::Int => "integer",
            CastTarget::Real => "real",
            CastTarget::Boolean => "integer",
            CastTarget::Bytes => "blob",
            CastTarget::Uuid => "text",
        }
    }

    fn placeholder(&self, n: usize) -> String {
        dml::sqlite_placeholder(n)
    }

    fn inline_string_literal(&self, s: &str) -> String {
        dml::sql_string_literal(s)
    }

    fn inline_decimal_literal(&self, d: &str) -> String {
        // SQLite stores an exact decimal losslessly only as TEXT.
        dml::sql_string_literal(d)
    }

    fn inline_bytes_literal(&self, bytes: &[u8]) -> String {
        format!("X'{}'", hex::encode(bytes))
    }

    /// rusqlite binds a byte vector natively, so SQLite needs NO decoder around
    /// the placeholder and NO base64 detour — the bytes stay bytes end to end.
    fn bind_bytes(&self, bytes: &[u8], push: &mut dyn FnMut(BindValue) -> String) -> String {
        push(BindValue::Bytes(bytes.to_vec()))
    }

    fn render_in_list(
        &self,
        expr: &str,
        elems: &[IrScalar],
        negated: bool,
        joiner: &str,
    ) -> Result<String, DmlError> {
        let rendered = elems
            .iter()
            .map(|elem| dml::render_in_list_elem_portable(elem, self))
            .collect::<Result<Vec<_>, _>>()?;
        let op = if negated { "NOT IN" } else { "IN" };
        Ok(format!("({expr} {op} ({}))", rendered.join(joiner)))
    }

    fn render_regex_match(&self, _expr: &str, _pattern: &str) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "regex is not supported on SQLite (no stock REGEXP); use dialect({...}) to port"
                .to_string(),
        ))
    }

    fn render_extract(&self, field: ExtractField, expr: &str) -> String {
        let fmt = match field {
            ExtractField::Year => "%Y",
            ExtractField::Month => "%m",
            ExtractField::Day => "%d",
            ExtractField::Hour => "%H",
            ExtractField::Minute => "%M",
            ExtractField::Dow => "%w",
        };
        format!("CAST(strftime('{fmt}', {expr}) AS INTEGER)")
    }

    fn render_concat(&self, l: &str, r: &str) -> String {
        format!("({l} || {r})")
    }

    fn render_distinct_from(&self, l: &str, r: &str) -> String {
        format!("({l} IS DISTINCT FROM {r})")
    }

    fn render_scalar_fn_override(&self, f: ScalarFn, args: &[String]) -> Option<String> {
        // SQLite exposes floor()/ceil() only when it was built with the optional
        // math extension. Lower both operations to core SQL so the portable DSL
        // behaves the same on every supported SQLite build. The builder enforces
        // one argument; a malformed hand-authored arity falls back to the generic
        // spelling, which validation rejects before render.
        match f {
            ScalarFn::Floor if args.len() == 1 => {
                let arg = &args[0];
                Some(format!(
                    "(CASE WHEN {arg} >= 9223372036854775808.0 OR {arg} <= -9223372036854775808.0 THEN {arg} ELSE CAST({arg} AS INTEGER) - (CAST({arg} AS INTEGER) > {arg}) END)"
                ))
            }
            ScalarFn::Ceil if args.len() == 1 => {
                let arg = &args[0];
                Some(format!(
                    "(CASE WHEN {arg} >= 9223372036854775808.0 OR {arg} <= -9223372036854775808.0 THEN {arg} ELSE CAST({arg} AS INTEGER) + (CAST({arg} AS INTEGER) < {arg}) END)"
                ))
            }
            _ => None,
        }
    }

    fn render_is_true(&self, operand: &str) -> String {
        // SQLite has no native boolean type (values are 0/1) and rejects the
        // `IS TRUE` / `IS FALSE` predicates at apply.
        format!("({operand} = 1)")
    }

    fn render_is_false(&self, operand: &str) -> String {
        format!("({operand} = 0)")
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
        let d = if dc == '\'' {
            "''''".to_string()
        } else {
            format!("'{dc}'")
        };
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

    fn uuid_v4(&self) -> String {
        // SQLite has no native UUID generator. Build each canonical group from
        // random bytes, pin the version nibble to `4`, and choose the variant
        // nibble from `8..b` (binary `10xx`). The outer lower() canonicalizes
        // hex(randomblob(...)), whose native spelling is uppercase.
        "(lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || \
         substr(hex(randomblob(2)), 2, 3) || '-' || \
         substr('89ab', ((instr('0123456789ABCDEF', \
         substr(hex(randomblob(1)), 1, 1)) - 1) % 4) + 1, 1) || \
         substr(hex(randomblob(2)), 2, 3) || '-' || hex(randomblob(6))))"
            .to_string()
    }

    fn uuid_v7(&self) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "uuidV7 database generation is unsupported on SQLite".to_string(),
        ))
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
        if replace && !DIALECT.supports(Capability::CreateOrReplaceView) {
            vec![format!("DROP VIEW IF EXISTS {qname}")]
        } else {
            Vec::new()
        }
    }

    fn view_object_name(&self, name: &str, _eff_schema: &str) -> Result<String, IrLowerError> {
        Ok(dml::quote_bare_ident_for_dialect("view", name, DIALECT)?)
    }

    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError> {
        let mut sql = {
            if let Some(schema) = table.schema.as_deref() {
                if !schema.eq_ignore_ascii_case(eff_schema) {
                    return Err(IrLowerError::LowerCrossSchema(schema.to_string()));
                }
            }
            dml::quote_bare_ident_for_dialect("table", &table.name, DIALECT)?
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

    /// STEP 3, RESOLVED. The 315 lines this used to reach across the crate for now
    /// sit at the bottom of this file, and the delegation is a local call.
    ///
    /// The note that stood here said the SQLite trigger SPELLING still lived in
    /// `render::lower::render_sqlite_trigger_op`, inside the 17k-line core lowerer,
    /// and that this delegation was "a POINTER to work that `lower.rs`'s own step-3
    /// pass has to finish, not a boundary that is done". Nothing about the emitted
    /// SQL changed when it moved — that is what made it a move.
    ///
    /// PostgreSQL is STILL in the position SQLite just left, via `render::vendor`,
    /// and that one is not the same shape: `render::vendor` is PostgreSQL by
    /// CONSTRUCTION rather than by gate (it carries no dialect literal at all), so
    /// every dialect-match census scores it zero. Do not read this resolved note as
    /// covering it.
    fn render_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<crate::render::vendor::VendorStatement>, IrLowerError> {
        Ok(vec![render_sqlite_trigger_op(op, eff_schema)?])
    }
}

fn render_sqlite_trigger_op(
    op: &Op,
    eff_schema: &str,
) -> Result<crate::render::vendor::VendorStatement, IrLowerError> {
    match op {
        Op::CreateTrigger {
            name,
            table,
            timing,
            events,
            for_each,
            when,
            action,
            ..
        } => {
            if events.is_empty() {
                return Err(IrLowerError::Vendor(
                    crate::render::vendor::VendorError::EmptyList {
                        what: "trigger events",
                    },
                ));
            }
            if events.iter().any(|e| matches!(e, TriggerEvent::Truncate))
                && !DIALECT.supports(Capability::TriggerTruncateEvent)
            {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "triggerEventTruncate",
                    dialect: DIALECT,
                });
            }
            if matches!(for_each, ForEach::Statement)
                && !DIALECT.supports(Capability::TriggerStatementForEach)
            {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "forEachStatement",
                    dialect: DIALECT,
                });
            }
            let TriggerAction::Body { statements } = action else {
                if !DIALECT.supports(Capability::TriggerExecuteFunction) {
                    return Err(IrLowerError::TriggerUnsupported {
                        kind: "executeFunction",
                        dialect: DIALECT,
                    });
                }
                return Err(IrLowerError::UnsupportedOp(
                    "SQLite trigger action routed past capability check",
                ));
            };
            if !DIALECT.supports(Capability::TriggerBody) {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "triggerBody",
                    dialect: DIALECT,
                });
            }
            if statements.is_empty() {
                return Err(IrLowerError::Vendor(
                    crate::render::vendor::VendorError::EmptyList {
                        what: "trigger body statements",
                    },
                ));
            }

            let qname = crate::render::dml::quote_bare_ident_for_dialect("trigger", name, DIALECT)?;
            let qtable = crate::render::dml::quote_bare_ident_for_dialect("table", table, DIALECT)?;
            let events_sql = events
                .iter()
                .map(|e| e.as_sql())
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut up = format!(
                "CREATE TRIGGER {qname} {} {events_sql} ON {qtable}",
                timing.as_sql()
            );
            up.push_str(" FOR EACH ROW");
            if let Some(pred) = when {
                up.push_str(&format!(
                    " WHEN ({})",
                    crate::render::dml::render_predicate_sqlite(pred)?
                ));
            }
            let body: Result<Vec<_>, _> = statements
                .iter()
                .map(|stmt| render_sqlite_trigger_stmt(stmt, eff_schema))
                .collect();
            up.push_str(" BEGIN ");
            up.push_str(
                &body?
                    .into_iter()
                    .map(|s| format!("{s};"))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            up.push_str(" END;");
            Ok(crate::render::vendor::VendorStatement {
                name: format!("create_trigger_{name}_{table}"),
                up,
                down: Some(format!("DROP TRIGGER IF EXISTS {qname}")),
            })
        }
        Op::DropTrigger {
            name,
            table,
            if_exists,
            ..
        } => {
            let qname = crate::render::dml::quote_bare_ident_for_dialect("trigger", name, DIALECT)?;
            let mut up = String::from("DROP TRIGGER ");
            if if_exists.unwrap_or(false) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&qname);
            Ok(crate::render::vendor::VendorStatement {
                name: format!("drop_trigger_{name}_{table}"),
                up,
                down: None,
            })
        }
        _ => Err(IrLowerError::UnsupportedOp(
            "non-trigger op routed to trigger renderer",
        )),
    }
}

fn sqlite_trigger_table_ref(
    table: &str,
    schema: Option<&str>,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
    if let Some(schema) = schema {
        if !schema.eq_ignore_ascii_case(eff_schema) {
            return Err(IrLowerError::LowerCrossSchema(schema.to_string()));
        }
    }
    Ok(crate::render::dml::quote_bare_ident_for_dialect(
        "table", table, DIALECT,
    )?)
}

fn render_sqlite_trigger_stmt(
    stmt: &TriggerStmt,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
    match stmt {
        TriggerStmt::Insert {
            table,
            columns,
            rows,
            schema,
        } => {
            if columns.is_empty() {
                return Err(IrLowerError::DmlAssemble(
                    crate::render::dml::DmlError::MalformedInsert {
                        table: table.clone(),
                        reason: "no columns".to_string(),
                    },
                ));
            }
            if rows.is_empty() {
                return Err(IrLowerError::DmlAssemble(
                    crate::render::dml::DmlError::MalformedInsert {
                        table: table.clone(),
                        reason: "no rows".to_string(),
                    },
                ));
            }
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let qcols: Result<Vec<_>, _> = columns
                .iter()
                .map(|c| crate::render::dml::quote_bare_ident_for_dialect("column", c, DIALECT))
                .collect();
            let qcols = qcols?;
            let mut groups = Vec::with_capacity(rows.len());
            for (ri, row) in rows.iter().enumerate() {
                if row.len() != columns.len() {
                    return Err(IrLowerError::DmlAssemble(
                        crate::render::dml::DmlError::MalformedInsert {
                            table: table.clone(),
                            reason: format!(
                                "row {ri} has {} value(s) but {} column(s) were named",
                                row.len(),
                                columns.len()
                            ),
                        },
                    ));
                }
                let vals: Result<Vec<_>, _> = row
                    .iter()
                    .map(|v| crate::render::dml::render_value_inline(v, DIALECT))
                    .collect();
                groups.push(format!("({})", vals?.join(", ")));
            }
            Ok(format!(
                "INSERT INTO {qtable} ({}) VALUES {}",
                qcols.join(", "),
                groups.join(", ")
            ))
        }
        TriggerStmt::Update {
            table,
            set,
            r#where,
            schema,
        } => {
            if set.is_empty() {
                return Err(IrLowerError::DmlAssemble(
                    crate::render::dml::DmlError::EmptySet {
                        op: "update",
                        table: table.clone(),
                    },
                ));
            }
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let mut assigns = Vec::with_capacity(set.len());
            for (col, rhs) in set {
                assigns.push(format!(
                    "{} = {}",
                    crate::render::dml::quote_bare_ident_for_dialect("column", col, DIALECT)?,
                    crate::render::dml::render_value_inline(rhs, DIALECT)?
                ));
            }
            let mut sql = format!("UPDATE {qtable} SET {}", assigns.join(", "));
            if let Some(pred) = r#where {
                sql.push_str(&format!(
                    " WHERE {}",
                    crate::render::dml::render_expr_inline(pred, DIALECT)?
                ));
            }
            Ok(sql)
        }
        TriggerStmt::Delete {
            table,
            r#where,
            limit,
            schema,
        } => {
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let pred = crate::render::dml::render_expr_inline(r#where, DIALECT)?;
            Ok(match limit {
                None => format!("DELETE FROM {qtable} WHERE {pred}"),
                // Trigger rendering has no live-catalog snapshot for the body
                // target. Refuse a limited delete instead of guessing at hidden
                // rowid; the one-shot DML path can use a proven PK/UNIQUE key.
                Some(_) => {
                    return Err(IrLowerError::DmlAssemble(
                        crate::render::dml::DmlError::SqliteLimitedDeleteNeedsUniqueIdentity {
                            table: table.clone(),
                        },
                    ));
                }
            })
        }
        TriggerStmt::Select { expr } => Ok(format!(
            "SELECT {}",
            crate::render::dml::render_expr_inline(expr, DIALECT)?
        )),
        TriggerStmt::Raise {
            level: RaiseLevel::Ignore,
            ..
        } => Ok("SELECT RAISE(IGNORE)".to_string()),
        TriggerStmt::Raise { level, message, .. } => Ok(format!(
            "SELECT RAISE({},{})",
            level.as_sqlite_sql(),
            crate::render::dml::sql_string_literal(message)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::expr::Expr;
    use crate::model::ir::SafeU64;

    /// Relocated from `render::lower`'s test module with the renderer it covers.
    /// A unit test for a private helper cannot outlive its module, and leaving it
    /// behind would have meant either widening the helper's visibility or dropping
    /// the only coverage of this refusal.
    #[test]
    fn sqlite_trigger_limited_delete_is_rejected_without_live_identity_facts() {
        let stmt = TriggerStmt::Delete {
            table: "events".to_string(),
            r#where: Expr::UnaryOp {
                op: crate::model::expr::UnaryOp::IsNull,
                operand: Box::new(Expr::col("code")),
            },
            limit: Some(SafeU64::new(1).unwrap()),
            schema: None,
        };
        let err = render_sqlite_trigger_stmt(&stmt, "app")
            .expect_err("trigger body rendering cannot guess at hidden rowid");
        assert!(matches!(
            err,
            IrLowerError::DmlAssemble(
                crate::render::dml::DmlError::SqliteLimitedDeleteNeedsUniqueIdentity {
                    ref table
                }
            ) if table == "events"
        ));
    }
}
