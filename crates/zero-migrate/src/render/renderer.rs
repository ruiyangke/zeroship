use crate::schema::query::SqlDialect;

use crate::model::expr::CastTarget;
use crate::model::ir::{
    ForEach, Op, RaiseLevel, TableRef, TriggerAction, TriggerEvent, TriggerStmt, TriggerTiming,
};
use crate::render::dml::{self, DmlError};
use crate::render::lower::IrLowerError;

/// The dialect feature predicates the migration lowerer asks.
///
/// PROMOTED to public vocabulary in `zero_migrate_ir::backend` — unchanged in
/// spirit and unchanged in membership (the same 25 predicates, the same
/// spellings). It is re-exported here so the ~250 in-crate `Capability::…` uses
/// keep naming it through `render::renderer`.
pub use zero_migrate_ir::backend::Capability;

/// Ask a dialect a capability QUESTION.
///
/// The answer no longer lives in an exhaustive `match` on the vendor: it is read
/// off the vendor's [`BackendDescriptor`](zero_migrate_ir::backend::BackendDescriptor),
/// which is the whole point of promoting the matrix. A fourth backend answers by
/// declaring a descriptor in its own crate, not by editing an arm here.
pub(crate) trait DialectSupports {
    fn supports(self, cap: Capability) -> bool;
}

impl DialectSupports for SqlDialect {
    fn supports(self, cap: Capability) -> bool {
        self.descriptor().capabilities.contains(cap)
    }
}

/// Dialect-specific DML/view/trigger rendering.
///
/// No method has a default body: adding a dialect requires an explicit impl for
/// every render decision. The single exhaustive dispatch match lives in
/// [`renderer`], so a third [`SqlDialect`] variant breaks there at compile time
/// until its renderer is implemented and wired.
pub(crate) trait DmlRenderer {
    fn quote_ident(&self, ident: &str) -> String;
    fn qualify_table(&self, project_schema: &str, table: &str) -> Result<String, DmlError>;
    fn cast_target(&self, target: CastTarget) -> &'static str;
    fn render_concat_ws(&self, rendered: &[String]) -> String;
    fn render_split_part(&self, col_sql: &str, delim: &str, n: i64) -> Result<String, DmlError>;
    fn synth_now(&self) -> String;
    fn uuid_v4(&self) -> String;
    fn uuid_v7(&self) -> Result<String, DmlError>;
    fn validate_view_materialized(&self, materialized: bool) -> Result<(), IrLowerError>;
    fn view_create_prefix(&self, materialized: bool, replace: bool)
        -> Result<String, IrLowerError>;
    fn view_replace_prelude(&self, qname: &str, replace: bool) -> Vec<String>;
    fn view_object_name(&self, name: &str, eff_schema: &str) -> Result<String, IrLowerError>;
    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError>;
    fn render_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<crate::render::vendor::VendorStatement>, IrLowerError>;
}

struct PostgresDmlRenderer;
struct SqliteDmlRenderer;
struct MysqlDmlRenderer;

static POSTGRES_DML_RENDERER: PostgresDmlRenderer = PostgresDmlRenderer;
static SQLITE_DML_RENDERER: SqliteDmlRenderer = SqliteDmlRenderer;
static MYSQL_DML_RENDERER: MysqlDmlRenderer = MysqlDmlRenderer;

pub(crate) fn renderer(dialect: SqlDialect) -> &'static dyn DmlRenderer {
    match dialect {
        SqlDialect::Postgres => &POSTGRES_DML_RENDERER,
        SqlDialect::Sqlite => &SQLITE_DML_RENDERER,
        SqlDialect::Mysql => &MYSQL_DML_RENDERER,
    }
}

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
        if materialized && !SqlDialect::Postgres.supports(Capability::MaterializedView) {
            return Err(IrLowerError::ViewUnsupported {
                kind: "materializedView",
                dialect: SqlDialect::Postgres,
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
        } else if replace && SqlDialect::Postgres.supports(Capability::CreateOrReplaceView) {
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
            if !SqlDialect::Postgres.supports(Capability::TriggerBody) {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "triggerBody",
                    dialect: SqlDialect::Postgres,
                });
            }
        }
        let stmts = match crate::render::vendor::render_vendor_op(op, eff_schema) {
            Ok(stmts) => stmts,
            Err(crate::render::vendor::VendorError::UnsupportedTriggerAction { kind }) => {
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
    fn quote_ident(&self, ident: &str) -> String {
        dml::escape_quote_ident(ident)
    }

    fn qualify_table(&self, _project_schema: &str, table: &str) -> Result<String, DmlError> {
        dml::quote_bare_ident("table", table)
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

    fn validate_view_materialized(&self, materialized: bool) -> Result<(), IrLowerError> {
        if materialized && !SqlDialect::Sqlite.supports(Capability::MaterializedView) {
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
        if replace && !SqlDialect::Sqlite.supports(Capability::CreateOrReplaceView) {
            vec![format!("DROP VIEW IF EXISTS {qname}")]
        } else {
            Vec::new()
        }
    }

    fn view_object_name(&self, name: &str, _eff_schema: &str) -> Result<String, IrLowerError> {
        Ok(dml::quote_bare_ident("view", name)?)
    }

    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError> {
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
    ) -> Result<Vec<crate::render::vendor::VendorStatement>, IrLowerError> {
        Ok(vec![crate::render::lower::render_sqlite_trigger_op(
            op, eff_schema,
        )?])
    }
}

impl DmlRenderer for MysqlDmlRenderer {
    fn quote_ident(&self, ident: &str) -> String {
        crate::schema::query::mysql_quote_ident(ident)
    }

    fn qualify_table(&self, project_schema: &str, table: &str) -> Result<String, DmlError> {
        let t = dml::quote_bare_ident_for_dialect("table", table, SqlDialect::Mysql)?;
        Ok(format!(
            "{}.{}",
            dml::quote_ident_checked_for_dialect(project_schema, SqlDialect::Mysql).map_err(
                |e| DmlError::InvalidIdentifier {
                    what: "schema",
                    value: e.value,
                },
            )?,
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

    fn render_concat_ws(&self, rendered: &[String]) -> String {
        format!("concat_ws({})", rendered.join(", "))
    }

    fn render_split_part(&self, col_sql: &str, delim: &str, n: i64) -> Result<String, DmlError> {
        let d = dml::inline_string_literal(delim, SqlDialect::Mysql);
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

    fn validate_view_materialized(&self, materialized: bool) -> Result<(), IrLowerError> {
        if materialized && !SqlDialect::Mysql.supports(Capability::MaterializedView) {
            return Err(IrLowerError::ViewUnsupported {
                kind: "materializedView",
                dialect: SqlDialect::Mysql,
            });
        }
        Ok(())
    }

    fn view_create_prefix(
        &self,
        materialized: bool,
        replace: bool,
    ) -> Result<String, IrLowerError> {
        self.validate_view_materialized(materialized)?;
        let mut create = String::from("CREATE ");
        if replace && SqlDialect::Mysql.supports(Capability::CreateOrReplaceView) {
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
            dml::quote_ident_checked_for_dialect(eff_schema, SqlDialect::Mysql).map_err(|e| {
                DmlError::InvalidIdentifier {
                    what: "schema",
                    value: e.value,
                }
            })?,
            dml::quote_bare_ident_for_dialect("view", name, SqlDialect::Mysql)?
        ))
    }

    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError> {
        let mut sql = {
            let schema = table.schema.as_deref().unwrap_or(eff_schema);
            format!(
                "{}.{}",
                dml::quote_ident_checked_for_dialect(schema, SqlDialect::Mysql).map_err(|e| {
                    DmlError::InvalidIdentifier {
                        what: "schema",
                        value: e.value,
                    }
                },)?,
                dml::quote_bare_ident_for_dialect("table", &table.name, SqlDialect::Mysql)?
            )
        };
        if let Some(alias) = table.alias.as_deref() {
            sql.push_str(" AS ");
            sql.push_str(&dml::quote_bare_ident_for_dialect(
                "table alias",
                alias,
                SqlDialect::Mysql,
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
        dml::quote_ident_checked_for_dialect(eff_schema, SqlDialect::Mysql).map_err(|e| {
            DmlError::InvalidIdentifier {
                what: "schema",
                value: e.value,
            }
        },)?,
        dml::quote_bare_ident_for_dialect("trigger", name, SqlDialect::Mysql)?
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
        dml::quote_ident_checked_for_dialect(schema, SqlDialect::Mysql).map_err(|e| {
            DmlError::InvalidIdentifier {
                what: "schema",
                value: e.value,
            }
        },)?,
        dml::quote_bare_ident_for_dialect("table", table, SqlDialect::Mysql)?
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
            dialect: SqlDialect::Mysql,
        });
    }
    if matches!(events[0], TriggerEvent::Truncate) {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "triggerEventTruncate",
            dialect: SqlDialect::Mysql,
        });
    }
    if matches!(timing, TriggerTiming::InsteadOf) {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "triggerTimingInsteadOf",
            dialect: SqlDialect::Mysql,
        });
    }
    if matches!(for_each, ForEach::Statement) {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "forEachStatement",
            dialect: SqlDialect::Mysql,
        });
    }
    if when.is_some() {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "triggerWhen",
            dialect: SqlDialect::Mysql,
        });
    }
    let TriggerAction::Body { statements } = action else {
        return Err(IrLowerError::TriggerUnsupported {
            kind: "executeFunction",
            dialect: SqlDialect::Mysql,
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
                .map(|c| dml::quote_bare_ident_for_dialect("column", c, SqlDialect::Mysql))
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
                    .map(|value| crate::render::dml::render_value_inline(value, SqlDialect::Mysql))
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
                    dml::quote_bare_ident_for_dialect("column", col, SqlDialect::Mysql)?,
                    crate::render::dml::render_value_inline(rhs, SqlDialect::Mysql)?
                ));
            }
            let mut sql = format!("UPDATE {qtable} SET {}", assigns.join(", "));
            if let Some(pred) = r#where {
                sql.push_str(&format!(
                    " WHERE {}",
                    crate::render::dml::render_expr_inline(pred, SqlDialect::Mysql)?
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
            let pred = crate::render::dml::render_expr_inline(r#where, SqlDialect::Mysql)?;
            Ok(match limit {
                None => format!("DELETE FROM {qtable} WHERE {pred}"),
                Some(n) => format!("DELETE FROM {qtable} WHERE {pred} LIMIT {}", n.get()),
            })
        }
        TriggerStmt::Select { expr } => Ok(format!(
            "SELECT {}",
            crate::render::dml::render_expr_inline(expr, SqlDialect::Mysql)?
        )),
        TriggerStmt::Raise {
            level: RaiseLevel::Ignore,
            ..
        } => Err(IrLowerError::TriggerUnsupported {
            kind: "raiseIgnore",
            dialect: SqlDialect::Mysql,
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
                crate::render::dml::inline_string_literal(message, SqlDialect::Mysql)
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_returns_expected_dml_renderer() {
        assert_eq!(renderer(SqlDialect::Postgres).synth_now(), "now()");
        assert_eq!(
            renderer(SqlDialect::Sqlite).synth_now(),
            "CURRENT_TIMESTAMP"
        );
        assert_eq!(
            renderer(SqlDialect::Mysql).synth_now(),
            "CURRENT_TIMESTAMP(6)"
        );
    }

    #[test]
    fn dialect_capability_matrix_is_explicit() {
        // This matrix pins the feature surface for every shipping dialect, so a
        // descriptor that flips an answer fails HERE rather than in whichever
        // render path happened to read it.
        //
        // The exhaustiveness check at the bottom used to compare this table
        // against a hand-written `ALL_CAPABILITIES` list, which had drifted:
        // `MaterializedEnumType`, `MaterializedDomainType` and
        // `SchemaWideIndexNames` were in the enum and in the dispatch matrix but
        // in neither the pinned table nor the "all" list, so three predicates
        // times three dialects were unpinned and the completeness assertion
        // could not notice. It now compares against `Capability::ALL`, the one
        // vocabulary list, which cannot drift from the enum without the set's
        // own tests failing.
        let expected = [
            (
                SqlDialect::Postgres,
                [
                    (Capability::NonPkIdentity, true),
                    (Capability::VirtualGeneratedColumn, false),
                    (Capability::CrossSchemaDdl, true),
                    (Capability::TableLevelForeignKey, true),
                    (Capability::TableLevelUnique, true),
                    (Capability::NonBtreeIndexMethod, true),
                    (Capability::PartialIndexPredicate, true),
                    (Capability::NativeAlterColumn, true),
                    (Capability::AlterTableAddConstraint, true),
                    (Capability::AlterTableDropConstraint, true),
                    (Capability::AlterTableValidateConstraint, true),
                    (Capability::InsertOnConflictClause, true),
                    (Capability::PostgresVendorPrimitives, true),
                    (Capability::MaterializedView, true),
                    (Capability::CreateOrReplaceView, true),
                    (Capability::TriggerTruncateEvent, true),
                    (Capability::TriggerStatementForEach, true),
                    (Capability::TriggerExecuteFunction, true),
                    (Capability::TriggerBody, false),
                    (Capability::MaterializedEnumType, true),
                    (Capability::MaterializedDomainType, true),
                    (Capability::Sequence, true),
                    (Capability::ExclusionConstraint, true),
                    (Capability::CommentOn, true),
                    (Capability::SchemaWideIndexNames, true),
                ],
            ),
            (
                SqlDialect::Sqlite,
                [
                    (Capability::NonPkIdentity, false),
                    (Capability::VirtualGeneratedColumn, true),
                    (Capability::CrossSchemaDdl, false),
                    (Capability::TableLevelForeignKey, true),
                    (Capability::TableLevelUnique, false),
                    (Capability::NonBtreeIndexMethod, false),
                    (Capability::PartialIndexPredicate, true),
                    (Capability::NativeAlterColumn, false),
                    (Capability::AlterTableAddConstraint, false),
                    (Capability::AlterTableDropConstraint, false),
                    (Capability::AlterTableValidateConstraint, false),
                    (Capability::InsertOnConflictClause, true),
                    (Capability::PostgresVendorPrimitives, false),
                    (Capability::MaterializedView, false),
                    (Capability::CreateOrReplaceView, false),
                    (Capability::TriggerTruncateEvent, false),
                    (Capability::TriggerStatementForEach, false),
                    (Capability::TriggerExecuteFunction, false),
                    (Capability::TriggerBody, true),
                    (Capability::MaterializedEnumType, false),
                    (Capability::MaterializedDomainType, false),
                    (Capability::Sequence, false),
                    (Capability::ExclusionConstraint, false),
                    (Capability::CommentOn, false),
                    (Capability::SchemaWideIndexNames, true),
                ],
            ),
            (
                SqlDialect::Mysql,
                [
                    (Capability::NonPkIdentity, false),
                    (Capability::VirtualGeneratedColumn, true),
                    (Capability::CrossSchemaDdl, true),
                    (Capability::TableLevelForeignKey, true),
                    (Capability::TableLevelUnique, true),
                    (Capability::NonBtreeIndexMethod, false),
                    (Capability::PartialIndexPredicate, false),
                    (Capability::NativeAlterColumn, true),
                    (Capability::AlterTableAddConstraint, true),
                    (Capability::AlterTableDropConstraint, true),
                    (Capability::AlterTableValidateConstraint, false),
                    (Capability::InsertOnConflictClause, true),
                    (Capability::PostgresVendorPrimitives, false),
                    (Capability::MaterializedView, false),
                    (Capability::CreateOrReplaceView, true),
                    (Capability::TriggerTruncateEvent, false),
                    (Capability::TriggerStatementForEach, false),
                    (Capability::TriggerExecuteFunction, false),
                    (Capability::TriggerBody, true),
                    (Capability::MaterializedEnumType, false),
                    (Capability::MaterializedDomainType, false),
                    (Capability::Sequence, false),
                    (Capability::ExclusionConstraint, false),
                    (Capability::CommentOn, false),
                    (Capability::SchemaWideIndexNames, false),
                ],
            ),
        ];

        for (dialect, capabilities) in expected {
            for (cap, supported) in capabilities {
                assert_eq!(
                    dialect.supports(cap),
                    supported,
                    "{dialect:?} support for {cap:?}"
                );
                assert_eq!(
                    dialect.descriptor().capabilities.contains(cap),
                    supported,
                    "{dialect:?} descriptor answer for {cap:?}"
                );
            }
        }

        // Exhaustiveness against the ONE vocabulary list, not a second
        // hand-written copy of it.
        for (dialect, capabilities) in expected {
            let pinned: Vec<Capability> = capabilities.iter().map(|(cap, _)| *cap).collect();
            for cap in Capability::ALL {
                assert!(
                    pinned.contains(cap),
                    "{dialect:?}: {cap:?} is in the vocabulary but unpinned by this matrix"
                );
            }
            assert_eq!(
                pinned.len(),
                Capability::ALL.len(),
                "{dialect:?}: the pinned matrix must be exactly the vocabulary"
            );
        }
    }

    /// Every shipping descriptor must answer the whole vocabulary, and the
    /// answers must be a real per-dialect matrix rather than one shared row.
    #[test]
    fn every_shipping_descriptor_answers_the_whole_vocabulary() {
        let dialects = [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql];
        for cap in Capability::ALL {
            let answers: Vec<bool> = dialects.iter().map(|d| d.supports(*cap)).collect();
            assert_eq!(answers.len(), 3, "{cap:?} must be answered by all three");
        }
        // The three shipping capability sets are pairwise distinct; a bug that
        // pointed every descriptor at one set would otherwise pass silently.
        assert_ne!(
            SqlDialect::Postgres.descriptor().capabilities,
            SqlDialect::Sqlite.descriptor().capabilities
        );
        assert_ne!(
            SqlDialect::Postgres.descriptor().capabilities,
            SqlDialect::Mysql.descriptor().capabilities
        );
        assert_ne!(
            SqlDialect::Sqlite.descriptor().capabilities,
            SqlDialect::Mysql.descriptor().capabilities
        );
    }
}
