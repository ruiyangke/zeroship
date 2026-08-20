//! SQLite SQL spelling. The future `zero-migrate-sqlite`.

use crate::model::expr::{CastTarget, ExtractField, ScalarFn};
use crate::model::ir::{IrScalar, Op, TableRef};
use crate::render::dml::{self, DmlError};
use crate::render::lower::IrLowerError;
use crate::render::renderer::{Capability, DialectSupports, DmlRenderer};
use crate::render::step::BindValue;
use crate::schema::query::SqlDialect;

/// This module's own vendor identity — the ONE dialect literal it is allowed to
/// name. See `backends/mod.rs`.
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

    /// NOTE (step 3, not yet resolved): the SQLite trigger SPELLING still lives
    /// in `render::lower::render_sqlite_trigger_op` — roughly 250 lines inside
    /// the 16.9k-line core lowerer. This delegation is a POINTER to work that
    /// `lower.rs`'s own step-3 pass has to finish, not a boundary that is done.
    /// PostgreSQL is in the same position via `render::vendor`; only MySQL's
    /// trigger spelling actually lives in its backend module today.
    fn render_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<crate::render::vendor::VendorStatement>, IrLowerError> {
        Ok(vec![crate::render::lower::render_sqlite_trigger_op(
            op, eff_schema,
        )?])
    }
}
