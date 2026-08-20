//! MySQL SQL spelling. The future `zero-migrate-mysql`.
//!
//! This is the only backend module whose trigger spelling actually lives here;
//! PostgreSQL's is still in `render::vendor` and SQLite's in `render::lower`.

use crate::model::expr::{CastTarget, ExtractField, ScalarFn};
use crate::model::ir::{
    ForEach, IrScalar, Op, RaiseLevel, TableRef, TriggerAction, TriggerEvent, TriggerStmt,
    TriggerTiming,
};
use crate::render::dml::{self, DmlError};
use crate::render::lower::IrLowerError;
use crate::render::renderer::{Capability, DialectSupports, DmlRenderer};
use crate::render::step::BindValue;
use crate::schema::query::SqlDialect;

/// This module's own vendor identity — the ONE dialect literal it is allowed to
/// name. See `backends/mod.rs`.
///
/// Every `dml::*_for_dialect(.., DIALECT)` call below is core's "validate, then
/// ask the vendor how to spell it" seam: `dml` owns whether an identifier is
/// LEGAL (semantics), this module owns how it is WRITTEN (spelling), and the
/// round trip goes back out through the `DmlRenderer` trait object. The const is
/// what keeps that from being a hard-coded vendor name inside a vendor module.
const DIALECT: SqlDialect = SqlDialect::Mysql;

#[derive(Debug)]
pub(super) struct MysqlDmlRenderer;

pub(super) static RENDERER: MysqlDmlRenderer = MysqlDmlRenderer;

impl DmlRenderer for MysqlDmlRenderer {
    /// THE single physical home of MySQL's backtick identifier spelling: double
    /// every embedded backtick, then wrap the result in backticks.
    ///
    /// It lives HERE, in the vendor's own module, and no longer in core. It used
    /// to be `schema::query::mysql_quote_ident` — `pub`, in the schema kernel —
    /// with this method reaching INTO core to get its own spelling: the exact
    /// mirror image of the ANSI arrangement, where `ansi_double_quote_ident` is
    /// `pub(in crate::render::backends)` so that core CANNOT reach it un-named.
    ///
    /// NOTHING WAS EMITTED WRONGLY BEFORE THE MOVE, and that is the point. Every
    /// call site named MySQL in the callee's name, so no vendor was unnamed, and
    /// `backend_modules_name_one_dialect` passed because the reach was by function
    /// name rather than by a dialect-enum literal. What it blocked was step 4: the
    /// future `zero-migrate-mysql` would have needed core at RUNTIME to spell its
    /// own identifier — the core-to-backend cycle the backend split exists to
    /// break, and the same shape as the extraction spike's finding that `-sqlite`
    /// needed `-postgres` to quote a trigger name.
    ///
    /// (This doc may not spell the dialect-enum literal, even in prose.
    /// `backend_modules_name_one_dialect`'s second half collects EVERY line in this
    /// file mentioning that path and demands the list be exactly the `DIALECT`
    /// const, so a comment is a carrier like any other line. An earlier draft of
    /// this paragraph named it and turned that test red, which is the rule working
    /// as intended.)
    ///
    /// MEASURED on the 1231-test `--lib` binary by neutering each candidate with a
    /// single appended token:
    ///
    /// | tree | neutered | red |
    /// |------|----------|-----|
    /// | before | this method | 21 |
    /// | before | `schema::query::mysql_quote_ident` | 30 |
    /// | after | this method | 54 |
    ///
    /// The two before-sets NEST rather than being disjoint — the inverse of the
    /// ANSI case, and exactly what "the backend delegates into core" means
    /// operationally: NOTHING reddened by neutering this method was missed by
    /// neutering core. The 9 in the difference (`render::lower::tests` ×5,
    /// `schema::query::hostile_identifier_quoting` ×3, and
    /// `policy_keyword_and_quoted_identifiers_are_quoted_in_injected_sql`) are the
    /// tests whose MySQL identifier bytes this backend had NO say in.
    ///
    /// AND THE 54 IS NOT A TYPO FOR THE 30 THAT WAS PREDICTED. Routing the two
    /// SECOND homes found during the change — `apply::backend::mysql::journal_sql`
    /// and `::backfill_sql`, each of which carried its own copy of the spelling and
    /// so could not be reached by the core neuter at all — added 24
    /// `apply::backend::mysql` tests on top of the 30. Nothing was lost at any
    /// step: the 54 is a strict superset of the 30, and the binary held at 1232
    /// tests throughout. The prediction was wrong because it was formed from the
    /// two sets measured FIRST, before those homes were known to exist.
    ///
    /// Like the two ANSI impls, this spells the bytes DIRECTLY rather than through
    /// the `*_for_dialect` seam its sibling methods use: it IS this dialect's
    /// `quote_ident`, so routing through the dispatch would recurse.
    fn quote_ident(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
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
            },)?,
            t
        ))
    }

    fn cast_target(&self, target: CastTarget) -> &'static str {
        match target {
            CastTarget::Text => "char",
            CastTarget::Int => "signed",
            CastTarget::Real => "double",
            CastTarget::Boolean => "unsigned",
            CastTarget::Bytes => "binary",
            CastTarget::Uuid => "char(36)",
        }
    }

    fn placeholder(&self, _n: usize) -> String {
        "?".to_string()
    }

    fn inline_string_literal(&self, s: &str) -> String {
        // A UTF-8 hex literal, so `NO_BACKSLASH_ESCAPES` (present or absent)
        // cannot change either the value or the statement shape.
        format!("_utf8mb4 X'{}'", hex::encode(s.as_bytes()))
    }

    fn inline_decimal_literal(&self, d: &str) -> String {
        d.to_string()
    }

    fn inline_bytes_literal(&self, bytes: &[u8]) -> String {
        // MySQL requires expression defaults for BLOB columns. Parentheses
        // keep the same literal valid in defaults and ordinary expressions.
        format!("(X'{}')", hex::encode(bytes))
    }

    /// mysql2 carries a raw binary bind as text and would corrupt it, so the
    /// value goes over as canonical base64 and the server decodes it. The apply
    /// backend enforces the other half of this contract: a raw binary bind that
    /// reaches the MySQL session without a `FROM_BASE64` wrapper is refused.
    fn bind_bytes(&self, bytes: &[u8], push: &mut dyn FnMut(BindValue) -> String) -> String {
        let placeholder = push(BindValue::Text(super::base64_standard(bytes)));
        format!("FROM_BASE64({placeholder})")
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

    fn render_regex_match(&self, expr: &str, pattern: &str) -> Result<String, DmlError> {
        Ok(format!(
            "({expr} REGEXP {})",
            dml::in_list_text_literal(pattern, "regex pattern", self)?
        ))
    }

    fn render_extract(&self, field: ExtractField, expr: &str) -> String {
        match field {
            ExtractField::Dow => format!("(DAYOFWEEK({expr}) - 1)"),
            _ => format!(
                "EXTRACT({} FROM {expr})",
                dml::extract_field_name(field).to_ascii_uppercase()
            ),
        }
    }

    fn render_concat(&self, l: &str, r: &str) -> String {
        // MySQL's `||` is *logical OR* absent the non-default `PIPES_AS_CONCAT`
        // sql_mode, so a `Concat` rendered as `a || b` would silently corrupt to
        // a boolean.
        format!("CONCAT({l}, {r})")
    }

    fn render_distinct_from(&self, l: &str, r: &str) -> String {
        // MySQL has NO `IS DISTINCT FROM`. `<=>` is its NULL-safe equality
        // operator, so its negation is exactly the predicate.
        format!("(NOT ({l} <=> {r}))")
    }

    fn render_scalar_fn_override(&self, f: ScalarFn, args: &[String]) -> Option<String> {
        match f {
            // The portable `length()` intent is CHARACTER length (PG + SQLite
            // `length(text)`). MySQL's `LENGTH()` is *byte* length — wrong for
            // any multibyte string — so MySQL must use `CHAR_LENGTH()`.
            ScalarFn::Length => Some(format!("char_length({})", args.join(", "))),
            _ => None,
        }
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
        let d = self.inline_string_literal(delim);
        Ok(format!(
            "substring_index(substring_index({col_sql}, {d}, {n}), {d}, -1)"
        ))
    }

    fn synth_now(&self) -> String {
        "CURRENT_TIMESTAMP(6)".to_string()
    }

    fn uuid_v4(&self) -> String {
        // UUID() is UUIDv1 on MySQL and must never implement the UUIDv4
        // contract. Consume 16 random bytes across the canonical groups and
        // mask the relevant octets: version = 0100xxxx, variant = 10xxxxxx.
        // Parentheses make this valid in MySQL's expression-default grammar.
        "(lower(concat(hex(random_bytes(4)), '-', hex(random_bytes(2)), '-', \
         hex((ord(random_bytes(1)) & 15) | 64), hex(random_bytes(1)), '-', \
         hex((ord(random_bytes(1)) & 63) | 128), hex(random_bytes(1)), '-', \
         hex(random_bytes(6)))))"
            .to_string()
    }

    fn uuid_v7(&self) -> Result<String, DmlError> {
        Err(DmlError::UnrenderableExpr(
            "uuidV7 database generation is unsupported on MySQL".to_string(),
        ))
    }

    fn view_create_prefix(
        &self,
        materialized: bool,
        replace: bool,
    ) -> Result<String, IrLowerError> {
        // Kept as a SELF-check, not restored as a trait method. Core refuses a
        // materialized view before it ever resolves a renderer, but it refuses the
        // OP's `materialized`; the `down` of a `dropView` re-creates from the
        // recorded view's own `materialized`, which core never validated. A
        // backend asking its own `DIALECT` a capability question is legal here in
        // a way that core asking it through a registry was not.
        if materialized && !DIALECT.supports(Capability::MaterializedView) {
            return Err(IrLowerError::ViewUnsupported {
                kind: "materializedView",
                dialect: DIALECT,
            });
        }
        let mut create = String::from("CREATE ");
        if replace && DIALECT.supports(Capability::CreateOrReplaceView) {
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
            dml::quote_ident_checked_for_dialect(eff_schema, DIALECT).map_err(|e| {
                DmlError::InvalidIdentifier {
                    what: "schema",
                    value: e.value,
                }
            })?,
            dml::quote_bare_ident_for_dialect("view", name, DIALECT)?
        ))
    }

    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError> {
        let mut sql = {
            let schema = table.schema.as_deref().unwrap_or(eff_schema);
            format!(
                "{}.{}",
                dml::quote_ident_checked_for_dialect(schema, DIALECT).map_err(|e| {
                    DmlError::InvalidIdentifier {
                        what: "schema",
                        value: e.value,
                    }
                },)?,
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
            } => Ok(vec![render_mysql_trigger_create(
                name,
                table,
                *timing,
                events,
                *for_each,
                when.as_ref(),
                action,
                eff_schema,
            )?]),
            Op::DropTrigger {
                name,
                table,
                if_exists,
                ..
            } => {
                let qname = mysql_trigger_name(name, eff_schema)?;
                let mut up = String::from("DROP TRIGGER ");
                if if_exists.unwrap_or(false) {
                    up.push_str("IF EXISTS ");
                }
                up.push_str(&qname);
                Ok(vec![crate::render::vendor::VendorStatement {
                    name: format!("drop_trigger_{name}_{table}"),
                    up,
                    down: None,
                }])
            }
            _ => Err(IrLowerError::UnsupportedOp(
                "non-trigger op routed to trigger renderer",
            )),
        }
    }
}

fn mysql_trigger_name(name: &str, eff_schema: &str) -> Result<String, IrLowerError> {
    Ok(format!(
        "{}.{}",
        dml::quote_ident_checked_for_dialect(eff_schema, DIALECT).map_err(|e| {
            DmlError::InvalidIdentifier {
                what: "schema",
                value: e.value,
            }
        },)?,
        dml::quote_bare_ident_for_dialect("trigger", name, DIALECT)?
    ))
}

fn mysql_trigger_table_ref(
    table: &str,
    schema: Option<&str>,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
    let schema = schema.unwrap_or(eff_schema);
    Ok(format!(
        "{}.{}",
        dml::quote_ident_checked_for_dialect(schema, DIALECT).map_err(|e| {
            DmlError::InvalidIdentifier {
                what: "schema",
                value: e.value,
            }
        },)?,
        dml::quote_bare_ident_for_dialect("table", table, DIALECT)?
    ))
}

#[allow(clippy::too_many_arguments)]
fn render_mysql_trigger_create(
    name: &str,
    table: &str,
    timing: TriggerTiming,
    events: &[TriggerEvent],
    for_each: ForEach,
    when: Option<&crate::model::expr::Expr>,
    action: &TriggerAction,
    eff_schema: &str,
) -> Result<crate::render::vendor::VendorStatement, IrLowerError> {
    if events.is_empty() {
        return Err(IrLowerError::Vendor(
            crate::render::vendor::VendorError::EmptyList {
                what: "trigger events",
            },
        ));
    }
    if events.len() != 1 {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "triggerMultipleEvents",
            dialect: DIALECT,
        });
    }
    if matches!(events[0], TriggerEvent::Truncate) {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "triggerEventTruncate",
            dialect: DIALECT,
        });
    }
    if matches!(timing, TriggerTiming::InsteadOf) {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "triggerTimingInsteadOf",
            dialect: DIALECT,
        });
    }
    if matches!(for_each, ForEach::Statement) {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "forEachStatement",
            dialect: DIALECT,
        });
    }
    if when.is_some() {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "triggerWhen",
            dialect: DIALECT,
        });
    }
    let TriggerAction::Body { statements } = action else {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "executeFunction",
            dialect: DIALECT,
        });
    };
    if statements.is_empty() {
        return Err(IrLowerError::Vendor(
            crate::render::vendor::VendorError::EmptyList {
                what: "trigger body statements",
            },
        ));
    }

    let qname = mysql_trigger_name(name, eff_schema)?;
    let qtable = mysql_trigger_table_ref(table, None, eff_schema)?;
    let body: Result<Vec<_>, _> = statements
        .iter()
        .map(|stmt| render_mysql_trigger_stmt(stmt, eff_schema))
        .collect();
    let mut up = format!(
        "CREATE TRIGGER {qname} {} {} ON {qtable} FOR EACH ROW BEGIN ",
        timing.as_sql(),
        events[0].as_sql()
    );
    up.push_str(
        &body?
            .into_iter()
            .map(|s| format!("{s};"))
            .collect::<Vec<_>>()
            .join(" "),
    );
    up.push_str(" END");
    Ok(crate::render::vendor::VendorStatement {
        name: format!("create_trigger_{name}_{table}"),
        up,
        down: Some(format!("DROP TRIGGER IF EXISTS {qname}")),
    })
}

fn render_mysql_trigger_stmt(stmt: &TriggerStmt, eff_schema: &str) -> Result<String, IrLowerError> {
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
            let qtable = mysql_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let qcols: Result<Vec<_>, _> = columns
                .iter()
                .map(|c| dml::quote_bare_ident_for_dialect("column", c, DIALECT))
                .collect();
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
                    .map(|value| crate::render::dml::render_value_inline(value, DIALECT))
                    .collect();
                groups.push(format!("({})", vals?.join(", ")));
            }
            Ok(format!(
                "INSERT INTO {qtable} ({}) VALUES {}",
                qcols?.join(", "),
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
            let qtable = mysql_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let mut assigns = Vec::with_capacity(set.len());
            for (col, rhs) in set {
                assigns.push(format!(
                    "{} = {}",
                    dml::quote_bare_ident_for_dialect("column", col, DIALECT)?,
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
            let qtable = mysql_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let pred = crate::render::dml::render_expr_inline(r#where, DIALECT)?;
            Ok(match limit {
                None => format!("DELETE FROM {qtable} WHERE {pred}"),
                Some(n) => format!("DELETE FROM {qtable} WHERE {pred} LIMIT {}", n.get()),
            })
        }
        // MySQL forbids a trigger body that RETURNS A RESULT SET, and it says so when
        // the `CREATE TRIGGER` is executed, not when the trigger fires: MySQL 8.4.11
        // answers `[0A000] Not allowed to return a result set from a trigger`. Before
        // this arm existed, the plan cleared validate, the guard and lower, and died
        // there - the one outcome class `dialect_conformance_live.rs` exists to catch,
        // and the row that measured it is `createTrigger/bodySimple` on MySQL.
        //
        // Refused rather than rewritten: `SELECT <expr>` has no result-set-free MySQL
        // spelling, because `SELECT ... INTO` needs a declared target and this closed
        // trigger-body IR has no way to name one. The refusal sits HERE, beside
        // `RAISE IGNORE`, and NOT in `dialect-support.toml`, because MySQL body
        // triggers work - an `INSERT` / `UPDATE` / `DELETE` body applies and fires.
        // Declaring the whole `bodySimple` cell unsupported would refuse all of them
        // to reject this one statement. Pinned, with both over-refusal controls, by
        // `tests/refusals/mysql_trigger_body_cannot_return_a_result_set.rs`.
        TriggerStmt::Select { .. } => Err(IrLowerError::TriggerUnsupported {
            kind: "selectStatement",
            dialect: DIALECT,
        }),
        TriggerStmt::Raise {
            level: RaiseLevel::Ignore,
            ..
        } => Err(IrLowerError::TriggerUnsupported {
            kind: "raiseIgnore",
            dialect: DIALECT,
        }),
        TriggerStmt::Raise {
            level: _,
            message,
            errcode,
        } => {
            let errcode = errcode.as_deref().unwrap_or("45000");
            Ok(format!(
                "SIGNAL SQLSTATE {} SET MESSAGE_TEXT = {}",
                crate::render::dml::mysql_grammar_string_literal(errcode),
                RENDERER.inline_string_literal(message)
            ))
        }
    }
}
