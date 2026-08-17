//! Dialect-specific physical metadata for validated textual value formats and
//! portable logical UUID storage.
//!
//! A [`ValueFormat`] is logical schema metadata carried separately from the
//! physical [`ColType`](crate::model::ir::ColType). This module is the one seam
//! that turns that metadata into the exact column collation and inline format
//! `CHECK` consumed by the shared DDL emitters.

use crate::model::expr::{BinaryOp, CastTarget, Expr, ScalarFn, SynthFn};
use crate::model::ir::{validate_type_id_prefix, IrDefault, IrScalar, SequenceRef, ValueFormat};
use crate::model::snapshot::{
    canonical_id_default_expression, ColumnCollationSnapshot, IdDefaultSnapshot,
};
use crate::render::dml::{
    mysql_grammar_string_literal, quote_ident_for_dialect, sql_string_literal,
};
use crate::schema::query::SqlDialect;

const TYPE_ID_SUFFIX_LEN: usize = 26;
const TYPE_ID_ALPHABET: &str = "0123456789abcdefghjkmnpqrstvwxyz";
const ULID_LEN: usize = 26;
const ULID_ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const UUID_TEXT_LEN: usize = 36;

#[derive(Clone, Copy, PartialEq, Eq)]
enum PgDefaultType {
    Text,
    Integer,
    Real,
    Boolean,
    Bytes,
    Uuid,
}

fn pg_cast_target_type(target: CastTarget) -> PgDefaultType {
    match target {
        CastTarget::Text => PgDefaultType::Text,
        CastTarget::Int => PgDefaultType::Integer,
        CastTarget::Real => PgDefaultType::Real,
        CastTarget::Boolean => PgDefaultType::Boolean,
        CastTarget::Bytes => PgDefaultType::Bytes,
        CastTarget::Uuid => PgDefaultType::Uuid,
    }
}

fn pg_default_expr_type(expr: &Expr) -> Option<PgDefaultType> {
    fn common<'a>(expressions: impl IntoIterator<Item = &'a Expr>) -> Option<PgDefaultType> {
        let mut inferred = None;
        for expression in expressions {
            if matches!(
                expression,
                Expr::Literal {
                    value: IrScalar::Null
                }
            ) {
                continue;
            }
            let candidate = pg_default_expr_type(expression)?;
            match inferred {
                None => inferred = Some(candidate),
                Some(previous) if previous == candidate => {}
                Some(_) => return None,
            }
        }
        inferred
    }

    match expr {
        Expr::Literal { value } => match value {
            IrScalar::Null => None,
            IrScalar::Bool(_) => Some(PgDefaultType::Boolean),
            IrScalar::Int(value) | IrScalar::Int64(value) if i32::try_from(*value).is_ok() => {
                Some(PgDefaultType::Integer)
            }
            IrScalar::Str(_) => Some(PgDefaultType::Text),
            IrScalar::Bytes(_) => Some(PgDefaultType::Bytes),
            IrScalar::Int(_) | IrScalar::Int64(_) | IrScalar::Decimal(_) => None,
        },
        Expr::BinOp { op, lhs, rhs } => match op {
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge
            | BinaryOp::And
            | BinaryOp::Or => Some(PgDefaultType::Boolean),
            BinaryOp::Concat => Some(PgDefaultType::Text),
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
                let left = pg_default_expr_type(lhs)?;
                (pg_default_expr_type(rhs) == Some(left)
                    && matches!(left, PgDefaultType::Integer | PgDefaultType::Real))
                .then_some(left)
            }
        },
        Expr::UnaryOp { .. }
        | Expr::Between { .. }
        | Expr::Like { .. }
        | Expr::DistinctFrom { .. }
        | Expr::InList { .. }
        | Expr::PgRegexMatch { .. } => Some(PgDefaultType::Boolean),
        Expr::Case { branches, r#else } => common(
            branches
                .iter()
                .map(|branch| &branch.then)
                .chain(r#else.iter().map(Box::as_ref)),
        ),
        Expr::FnCall { r#fn, args } => match r#fn {
            ScalarFn::Lower
            | ScalarFn::Upper
            | ScalarFn::Trim
            | ScalarFn::Substr
            | ScalarFn::Replace
            | ScalarFn::CurrentSetting => Some(PgDefaultType::Text),
            // CURRENT_USER is special SQL syntax rather than an ordinary text
            // function call. PostgreSQL retains an explicit cast around it in
            // pg_get_expr, so it must not enter redundant-cast elimination.
            ScalarFn::CurrentUser => None,
            ScalarFn::Length => Some(PgDefaultType::Integer),
            ScalarFn::Abs => args.first().and_then(pg_default_expr_type),
            ScalarFn::Mod => common(args).filter(|kind| *kind == PgDefaultType::Integer),
            ScalarFn::Coalesce | ScalarFn::Nullif => common(args),
            // PostgreSQL resolves these through numeric/double-precision
            // overloads whose return type is not necessarily CastTarget::Real.
            ScalarFn::Round | ScalarFn::Floor | ScalarFn::Ceil => None,
        },
        Expr::FnSynth { r#fn, .. } => match r#fn {
            SynthFn::ConcatWs | SynthFn::SplitPart => Some(PgDefaultType::Text),
            SynthFn::Now => None,
        },
        Expr::UuidV4 | Expr::UuidV7 => Some(PgDefaultType::Uuid),
        Expr::Cast { target, .. } => Some(pg_cast_target_type(*target)),
        Expr::PgColumnSize { .. } => Some(PgDefaultType::Integer),
        Expr::Agg { .. }
        | Expr::Extract { .. }
        | Expr::PgExtract { .. }
        | Expr::PgInterval { .. }
        | Expr::Dialectal { .. }
        | Expr::ColRef { .. } => None,
    }
}

fn normalize_redundant_pg_default_casts(expr: &Expr) -> Expr {
    fn visit(expr: &mut Expr) {
        match expr {
            Expr::BinOp { lhs, rhs, .. } => {
                visit(lhs);
                visit(rhs);
            }
            Expr::UnaryOp { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::PgColumnSize { expr: operand }
            | Expr::Extract { from: operand, .. }
            | Expr::PgExtract { from: operand, .. } => visit(operand),
            Expr::Case { branches, r#else } => {
                for branch in branches {
                    visit(&mut branch.when);
                    visit(&mut branch.then);
                }
                if let Some(r#else) = r#else {
                    visit(r#else);
                }
            }
            Expr::FnCall { args, .. } | Expr::FnSynth { args, .. } => {
                for argument in args {
                    visit(argument);
                }
            }
            Expr::Between { operand, low, high } => {
                visit(operand);
                visit(low);
                visit(high);
            }
            Expr::Like { operand, pattern } => {
                visit(operand);
                visit(pattern);
            }
            Expr::DistinctFrom { left, right } => {
                visit(left);
                visit(right);
            }
            Expr::Agg { arg, delimiter, .. } => {
                if let Some(arg) = arg {
                    visit(arg);
                }
                if let Some(delimiter) = delimiter {
                    visit(delimiter);
                }
            }
            Expr::InList { expr, .. } | Expr::PgRegexMatch { expr, .. } => visit(expr),
            Expr::Dialectal {
                default,
                pg,
                sqlite,
                mysql,
            } => {
                for leg in [default, pg, sqlite, mysql].into_iter().flatten() {
                    visit(leg);
                }
            }
            Expr::ColRef { .. }
            | Expr::Literal { .. }
            | Expr::UuidV4
            | Expr::UuidV7
            | Expr::PgInterval { .. } => {}
        }

        let replacement = match expr {
            Expr::Cast { operand, target }
                if pg_default_expr_type(operand) == Some(pg_cast_target_type(*target)) =>
            {
                Some((**operand).clone())
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            *expr = replacement;
        }
    }

    let mut normalized = expr.clone();
    visit(&mut normalized);
    normalized
}

/// Project a structured authored default into the narrow ID-default drift key.
pub(crate) fn authored_id_default(
    default: Option<&IrDefault>,
    rendered: Option<&str>,
    dialect: SqlDialect,
    default_schema: Option<&str>,
) -> IdDefaultSnapshot {
    match default {
        None => IdDefaultSnapshot::Absent,
        Some(IrDefault::Literal {
            value: IrScalar::Null,
        }) => IdDefaultSnapshot::Absent,
        Some(IrDefault::Literal { value }) => {
            IdDefaultSnapshot::Literal(authored_literal_fingerprint(value))
        }
        Some(IrDefault::Expr {
            expr: Expr::Literal {
                value: IrScalar::Null,
            },
        }) => IdDefaultSnapshot::Absent,
        Some(IrDefault::Expr {
            expr: Expr::Literal { value },
        }) => IdDefaultSnapshot::Literal(
            rendered
                .and_then(|rendered| sql_literal_fingerprint_in_dialect(rendered, dialect))
                .unwrap_or_else(|| authored_literal_fingerprint(value)),
        ),
        Some(IrDefault::Expr { expr: Expr::UuidV4 }) => IdDefaultSnapshot::UuidV4,
        Some(IrDefault::Expr { expr: Expr::UuidV7 }) => IdDefaultSnapshot::UuidV7,
        Some(IrDefault::Nextval { sequence }) => {
            let sequence = SequenceRef {
                name: sequence.name.clone(),
                schema: sequence
                    .schema
                    .clone()
                    .or_else(|| default_schema.map(str::to_string)),
            };
            IdDefaultSnapshot::Nextval(crate::render::declarative::nextval_default_expr(&sequence))
        }
        Some(_) => {
            let normalized_rendered = match default {
                Some(IrDefault::Expr { expr }) if dialect == SqlDialect::Postgres => {
                    crate::render::dml::render_expr_inline(
                        &normalize_redundant_pg_default_casts(expr),
                        dialect,
                    )
                    .ok()
                }
                _ => None,
            };
            let rendered = normalized_rendered.as_deref().or(rendered);
            rendered
                .and_then(|rendered| sql_literal_fingerprint_in_dialect(rendered, dialect))
                .map_or_else(
                    || {
                        IdDefaultSnapshot::Expression(catalog_expression_fingerprint_in_dialect(
                            rendered.unwrap_or_default(),
                            dialect,
                        ))
                    },
                    id_default_from_literal_fingerprint,
                )
        }
    }
}

/// UUID columns accept several textual spellings, while PostgreSQL stores and
/// deparses one canonical lowercase/hyphenated representation. Preserve that
/// semantic normalization only on the UUID-typed default surface; TypeID/ULID
/// text literals remain byte-exact.
pub(crate) fn authored_uuid_id_default(
    default: Option<&IrDefault>,
    rendered: Option<&str>,
    dialect: SqlDialect,
    default_schema: Option<&str>,
) -> IdDefaultSnapshot {
    let snapshot = authored_storage_literal_snapshot(
        authored_id_default(default, rendered, dialect, default_schema),
        rendered,
        dialect,
    );
    let snapshot = if dialect == SqlDialect::Mysql {
        mysql_text_literal_snapshot(snapshot)
    } else {
        snapshot
    };
    uuid_literal_snapshot(snapshot, dialect == SqlDialect::Postgres)
}

/// TypeID/ULID columns persist character storage. Project authored scalar
/// literals through the actual rendered literal so a decimal carried through
/// the descriptor bridge as a quoted string compares to that stored text on all
/// dialects. MySQL additionally reports a non-expression `COLUMN_DEFAULT` in
/// its coerced character form, without SQL quotes.
pub(crate) fn authored_text_id_default(
    default: Option<&IrDefault>,
    rendered: Option<&str>,
    dialect: SqlDialect,
    default_schema: Option<&str>,
) -> IdDefaultSnapshot {
    let snapshot = authored_storage_literal_snapshot(
        authored_id_default(default, rendered, dialect, default_schema),
        rendered,
        dialect,
    );
    if dialect == SqlDialect::Mysql {
        mysql_text_literal_snapshot(snapshot)
    } else {
        snapshot
    }
}

fn authored_literal_fingerprint(value: &IrScalar) -> String {
    match value {
        IrScalar::Null => "null".to_string(),
        IrScalar::Bool(value) => value.to_string(),
        IrScalar::Int(value) | IrScalar::Int64(value) => value.to_string(),
        IrScalar::Decimal(value) => value.strip_prefix('+').unwrap_or(value).to_string(),
        IrScalar::Str(value) => {
            serde_json::to_string(value).expect("string serialization is infallible")
        }
        // Binary defaults are not a portable ID surface, but keep a stable,
        // collision-free key if a hand-built IR reaches this narrow path.
        IrScalar::Bytes(_) => {
            serde_json::to_string(value).expect("IR scalar serialization is infallible")
        }
    }
}

/// Project a live catalog default into the same semantic key as
/// [`authored_id_default`]. `mysql_expression_default` is the authoritative
/// `EXTRA`/`DEFAULT_GENERATED` distinction: MySQL's `COLUMN_DEFAULT` strips SQL
/// quotes from literals, so the text alone cannot distinguish a literal such as
/// `"uuid()"` from an expression.
pub(crate) fn catalog_id_default(
    default: Option<&str>,
    dialect: SqlDialect,
    mysql_expression_default: Option<bool>,
) -> IdDefaultSnapshot {
    let Some(default) = default else {
        return IdDefaultSnapshot::Absent;
    };

    if dialect == SqlDialect::Mysql && mysql_expression_default == Some(false) {
        return IdDefaultSnapshot::Literal(
            serde_json::to_string(default).expect("string serialization is infallible"),
        );
    }
    if default_matches_uuid(default, dialect, false) {
        return IdDefaultSnapshot::UuidV4;
    }
    if default_matches_uuid(default, dialect, true) {
        return IdDefaultSnapshot::UuidV7;
    }
    if let Some(literal) = sql_literal_fingerprint_in_dialect(default, dialect) {
        return id_default_from_literal_fingerprint(literal);
    }
    IdDefaultSnapshot::Expression(catalog_expression_fingerprint_in_dialect(default, dialect))
}

pub(crate) fn catalog_uuid_id_default(
    default: Option<&str>,
    dialect: SqlDialect,
    mysql_expression_default: Option<bool>,
) -> IdDefaultSnapshot {
    let snapshot = catalog_id_default(default, dialect, mysql_expression_default);
    let snapshot = if dialect == SqlDialect::Mysql {
        mysql_text_literal_snapshot(snapshot)
    } else {
        snapshot
    };
    uuid_literal_snapshot(snapshot, dialect == SqlDialect::Postgres)
}

pub(crate) fn catalog_text_id_default(
    default: Option<&str>,
    dialect: SqlDialect,
    mysql_expression_default: Option<bool>,
) -> IdDefaultSnapshot {
    let snapshot = catalog_id_default(default, dialect, mysql_expression_default);
    if dialect == SqlDialect::Mysql {
        mysql_text_literal_snapshot(snapshot)
    } else {
        snapshot
    }
}

/// Compare a catalog default whose dialect-specific expression/literal marker
/// was not retained against one expected semantic key. This is used for typed
/// references: their local format CHECK is intentionally absent, but the
/// authored side still declares that their default is an ID-default surface.
pub(crate) fn catalog_id_default_for_expected(
    expected: &IdDefaultSnapshot,
    default: Option<&str>,
    dialect: Option<SqlDialect>,
    mysql_expression_default: Option<bool>,
) -> IdDefaultSnapshot {
    let Some(default) = default else {
        return IdDefaultSnapshot::Absent;
    };
    if matches!(expected, IdDefaultSnapshot::UuidLiteral(_)) {
        let Some(dialect) = dialect else {
            return uuid_literal_snapshot(
                IdDefaultSnapshot::Literal(
                    serde_json::to_string(default).expect("string serialization is infallible"),
                ),
                false,
            );
        };
        return catalog_uuid_id_default(Some(default), dialect, mysql_expression_default);
    }
    if dialect == Some(SqlDialect::Mysql) {
        if let Some(expression_default) = mysql_expression_default {
            return catalog_id_default(Some(default), SqlDialect::Mysql, Some(expression_default));
        }
    }
    if matches!(expected, IdDefaultSnapshot::Literal(_)) {
        if let Some(literal) = dialect.map_or_else(
            || sql_literal_fingerprint(default),
            |dialect| sql_literal_fingerprint_in_dialect(default, dialect),
        ) {
            return IdDefaultSnapshot::Literal(literal);
        }
        // MySQL information_schema returns literal text without SQL quotes.
        return IdDefaultSnapshot::Literal(
            serde_json::to_string(default).expect("string serialization is infallible"),
        );
    }
    if let Some(dialect) = dialect {
        let recovered = catalog_id_default(Some(default), dialect, None);
        if !matches!(recovered, IdDefaultSnapshot::Expression(_)) {
            return recovered;
        }
    }
    IdDefaultSnapshot::Expression(dialect.map_or_else(
        || catalog_expression_fingerprint(default),
        |dialect| catalog_expression_fingerprint_in_dialect(default, dialect),
    ))
}

fn default_matches_uuid(default: &str, dialect: SqlDialect, v7: bool) -> bool {
    let rendered = if v7 {
        crate::render::renderer::renderer(dialect).uuid_v7().ok()
    } else {
        Some(crate::render::renderer::renderer(dialect).uuid_v4())
    };
    rendered.is_some_and(|rendered| {
        let actual = catalog_expression_fingerprint_in_dialect(default, dialect);
        actual == catalog_expression_fingerprint_in_dialect(&rendered, dialect)
            || (dialect == SqlDialect::Postgres
                && actual
                    == catalog_expression_fingerprint_in_dialect(
                        &format!("pg_catalog.{rendered}"),
                        dialect,
                    ))
    })
}

fn id_default_from_literal_fingerprint(literal: String) -> IdDefaultSnapshot {
    if literal == "null" {
        IdDefaultSnapshot::Absent
    } else {
        IdDefaultSnapshot::Literal(literal)
    }
}

fn uuid_literal_snapshot(
    snapshot: IdDefaultSnapshot,
    canonicalize_postgres_spelling: bool,
) -> IdDefaultSnapshot {
    if !canonicalize_postgres_spelling {
        return snapshot;
    }
    let IdDefaultSnapshot::Literal(fingerprint) = snapshot else {
        return snapshot;
    };
    let canonical = serde_json::from_str::<String>(&fingerprint)
        .ok()
        .and_then(|value| uuid::Uuid::parse_str(&value).ok())
        .map(|value| {
            serde_json::to_string(&value.to_string()).expect("UUID serialization is infallible")
        })
        .unwrap_or(fingerprint);
    IdDefaultSnapshot::UuidLiteral(canonical)
}

fn authored_storage_literal_snapshot(
    snapshot: IdDefaultSnapshot,
    rendered: Option<&str>,
    dialect: SqlDialect,
) -> IdDefaultSnapshot {
    if dialect == SqlDialect::Mysql || !matches!(snapshot, IdDefaultSnapshot::Literal(_)) {
        return snapshot;
    }
    rendered
        .and_then(|rendered| sql_literal_fingerprint_in_dialect(rendered, dialect))
        .map_or(snapshot, id_default_from_literal_fingerprint)
}

fn mysql_text_literal_snapshot(snapshot: IdDefaultSnapshot) -> IdDefaultSnapshot {
    let IdDefaultSnapshot::Literal(fingerprint) = snapshot else {
        return snapshot;
    };
    let stored = if let Ok(value) = serde_json::from_str::<String>(&fingerprint) {
        value
    } else if fingerprint == "true" {
        "1".to_string()
    } else if fingerprint == "false" {
        "0".to_string()
    } else if crate::model::ir::is_decimal_string(&fingerprint) {
        fingerprint
            .strip_prefix('+')
            .unwrap_or(&fingerprint)
            .to_string()
    } else {
        return IdDefaultSnapshot::Literal(fingerprint);
    };
    IdDefaultSnapshot::Literal(
        serde_json::to_string(&stored).expect("string serialization is infallible"),
    )
}

#[derive(Clone, Copy)]
enum LiteralCastKind {
    Text,
    SignedInteger { bits: u8 },
    UnsignedInteger { bits: u8 },
    ExactNumeric,
    Real,
    Boolean,
    Uuid,
}

fn canonical_decimal_sql_literal(value: &str) -> Option<String> {
    if !crate::model::ir::is_decimal_string(value) {
        return None;
    }
    let (negative, body) = if let Some(body) = value.strip_prefix('-') {
        (true, body)
    } else {
        (false, value.strip_prefix('+').unwrap_or(value))
    };
    let (integer, fraction) = body
        .split_once('.')
        .map_or((body, None), |(integer, fraction)| {
            (integer, Some(fraction))
        });
    let integer = integer.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    let nonzero = integer != "0"
        || fraction.is_some_and(|fraction| fraction.bytes().any(|digit| digit != b'0'));
    let sign = if negative && nonzero { "-" } else { "" };
    Some(match fraction {
        Some("") | None => format!("{sign}{integer}"),
        Some(fraction) => format!("{sign}{integer}.{fraction}"),
    })
}

fn sql_literal_fingerprint(expression: &str) -> Option<String> {
    sql_literal_fingerprint_with_dialect(expression, None)
}

fn sql_literal_fingerprint_in_dialect(expression: &str, dialect: SqlDialect) -> Option<String> {
    sql_literal_fingerprint_with_dialect(expression, Some(dialect))
}

fn sql_literal_fingerprint_with_dialect(
    expression: &str,
    dialect: Option<SqlDialect>,
) -> Option<String> {
    fn top_level_token(tokens: &[String], needle: &str, from_end: bool) -> Option<usize> {
        let mut depth = 0_i32;
        let mut found = None;
        for (index, token) in tokens.iter().enumerate() {
            match token.as_str() {
                "(" => depth += 1,
                ")" => depth -= 1,
                _ if depth == 0 && token == needle => {
                    if !from_end {
                        return Some(index);
                    }
                    found = Some(index);
                }
                _ => {}
            }
        }
        found
    }

    fn cast_kind(tokens: &[String], dialect: Option<SqlDialect>) -> Option<LiteralCastKind> {
        let compact = tokens.join("");
        let compact = if dialect.is_none() || dialect == Some(SqlDialect::Mysql) {
            compact
                .split_once("charset")
                .map_or(compact.as_str(), |(base, _)| base)
        } else {
            compact.as_str()
        };
        if compact == "uuid" {
            return Some(LiteralCastKind::Uuid);
        }
        if matches!(compact, "text" | "charactervarying" | "varchar")
            || ((dialect.is_none() || dialect == Some(SqlDialect::Mysql))
                && matches!(compact, "character" | "char"))
        {
            return Some(LiteralCastKind::Text);
        }
        let numeric_kind = match compact {
            "smallint" | "int2" => Some(LiteralCastKind::SignedInteger { bits: 16 }),
            "integer" | "int" | "int4" => Some(LiteralCastKind::SignedInteger { bits: 32 }),
            "bigint" | "int8" | "signed" => Some(LiteralCastKind::SignedInteger { bits: 64 }),
            "unsigned" => Some(LiteralCastKind::UnsignedInteger { bits: 64 }),
            "numeric" | "decimal" => Some(LiteralCastKind::ExactNumeric),
            "real" | "double" | "doubleprecision" => Some(LiteralCastKind::Real),
            _ => None,
        };
        if numeric_kind.is_some() {
            return numeric_kind;
        }
        matches!(compact, "boolean" | "bool").then_some(LiteralCastKind::Boolean)
    }

    fn apply_cast(input: String, target: &[String], dialect: Option<SqlDialect>) -> Option<String> {
        // A typed NULL remains the absence-equivalent SQL NULL even for cast
        // targets outside the portable scalar surface (for example BYTEA).
        if input == "null" {
            return Some(input);
        }

        let kind = cast_kind(target, dialect)?;
        let string = serde_json::from_str::<String>(&input).ok();
        let number = canonical_decimal_sql_literal(&input);
        match kind {
            LiteralCastKind::Text | LiteralCastKind::Uuid => {
                if let Some(string) = string {
                    serde_json::to_string(&string).ok()
                } else {
                    number.and_then(|number| serde_json::to_string(&number).ok())
                }
            }
            LiteralCastKind::SignedInteger { bits } => number
                .or_else(|| string.and_then(|value| canonical_decimal_sql_literal(&value)))
                .filter(|value| !value.contains('.'))
                .and_then(|value| {
                    let parsed = value.parse::<i128>().ok()?;
                    let minimum = -(1_i128 << (bits - 1));
                    let maximum = (1_i128 << (bits - 1)) - 1;
                    (parsed >= minimum && parsed <= maximum).then_some(value)
                }),
            LiteralCastKind::UnsignedInteger { bits } => number
                .or_else(|| string.and_then(|value| canonical_decimal_sql_literal(&value)))
                .filter(|value| !value.contains('.') && !value.starts_with('-'))
                .and_then(|value| {
                    let parsed = value.parse::<u128>().ok()?;
                    let maximum = (1_u128 << bits) - 1;
                    (parsed <= maximum).then_some(value)
                }),
            LiteralCastKind::ExactNumeric => {
                number.or_else(|| string.and_then(|value| canonical_decimal_sql_literal(&value)))
            }
            LiteralCastKind::Real => None,
            LiteralCastKind::Boolean => {
                if matches!(input.as_str(), "true" | "false") {
                    Some(input)
                } else {
                    string
                        .filter(|value| {
                            value.eq_ignore_ascii_case("true")
                                || value.eq_ignore_ascii_case("false")
                        })
                        .map(|value| value.to_ascii_lowercase())
                }
            }
        }
    }

    fn decode_quoted_string(token: &str) -> Option<String> {
        let bytes = token.as_bytes();
        if bytes.first() != Some(&b'\'') || bytes.last() != Some(&b'\'') {
            return None;
        }
        let mut decoded = String::new();
        let mut cursor = 1_usize;
        while cursor + 1 < bytes.len() {
            if bytes[cursor] == b'\'' {
                if bytes.get(cursor + 1) != Some(&b'\'') {
                    return None;
                }
                decoded.push('\'');
                cursor += 2;
            } else {
                let start = cursor;
                while cursor + 1 < bytes.len() && bytes[cursor] != b'\'' {
                    cursor += 1;
                }
                decoded.push_str(&token[start..cursor]);
            }
        }
        Some(decoded)
    }

    fn leaf(tokens: &[String], dialect: Option<SqlDialect>) -> Option<String> {
        if tokens.len() == 1 {
            if let Some(decoded) = decode_quoted_string(&tokens[0]) {
                return serde_json::to_string(&decoded).ok();
            }
        }

        // MySQL's renderer uses a charset-qualified hex string so its meaning
        // cannot depend on NO_BACKSLASH_ESCAPES. Treat that exact text carrier
        // as a string, while leaving an ordinary binary X'..' literal outside
        // the ID-literal comparison surface.
        if (dialect.is_none() || dialect == Some(SqlDialect::Mysql))
            && tokens.len() == 3
            && tokens[0].starts_with('_')
            && tokens[1] == "x"
        {
            let encoded = decode_quoted_string(&tokens[2])?;
            let decoded = String::from_utf8(hex::decode(encoded).ok()?).ok()?;
            return serde_json::to_string(&decoded).ok();
        }

        let joined = tokens.join("");
        let compact = canonical_id_default_expression(&joined);
        let compact = compact.strip_prefix('+').unwrap_or(&compact);
        if matches!(compact, "null" | "true" | "false") {
            return Some(compact.to_string());
        }
        canonical_decimal_sql_literal(compact)
    }

    fn parse(tokens: &[String], dialect: Option<SqlDialect>) -> Option<String> {
        let tokens = strip_outer_token_parens(tokens);

        if tokens.first().map(String::as_str) == Some("cast")
            && tokens.get(1).map(String::as_str) == Some("(")
            && tokens.last().map(String::as_str) == Some(")")
        {
            let body = &tokens[2..tokens.len() - 1];
            let separator = top_level_token(body, "as", false)?;
            if separator == 0 || separator + 1 == body.len() {
                return None;
            }
            let input = parse(&body[..separator], dialect)?;
            return apply_cast(input, &body[separator + 1..], dialect);
        }

        if let Some(separator) = top_level_token(tokens, "::", true) {
            if separator == 0 || separator + 1 == tokens.len() {
                return None;
            }
            let input = parse(&tokens[..separator], dialect)?;
            return apply_cast(input, &tokens[separator + 1..], dialect);
        }

        leaf(tokens, dialect)
    }

    parse(
        &catalog_sql_tokens_with_dialect(None, expression, dialect),
        dialect,
    )
}

/// The physical column details implied by one logical value format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValueFormatColumnMetadata {
    /// Exact dialect DDL type, including the format's bytewise collation.
    pub ddl_type: String,
    /// Exact non-default catalog collation identity, when the dialect exposes
    /// one independently from its DDL type spelling.
    pub collation: Option<ColumnCollationSnapshot>,
    /// Null-tolerant canonical spelling check, including its `CHECK` wrapper.
    pub inline_check: String,
}

/// Engine-owned format contract recovered from one catalog CHECK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveredFormatCheck {
    /// The portable textual UUID spelling CHECK used on MySQL/SQLite.
    Uuid,
    /// A TypeID or ULID CHECK, including the exact TypeID prefix.
    Value(ValueFormat),
}

/// Recover an engine-owned UUID/TypeID/ULID CHECK from catalog SQL.
///
/// A candidate format is first inferred from its anchored grammar literal, then
/// the complete clause is compared against a freshly rendered authoritative
/// contract after removing catalog-only syntax (redundant parentheses,
/// whitespace, PostgreSQL's `::text`, identifier quote choices, and MySQL
/// charset introducers). A partially edited CHECK therefore does not masquerade
/// as a valid format contract.
pub(crate) fn recover_format_check(
    column: &str,
    check_sql: &str,
    dialect: SqlDialect,
) -> Option<RecoveredFormatCheck> {
    if let Ok(Some(uuid)) = uuid_column_metadata(column, dialect) {
        if canonical_check_sql(column, check_sql) == canonical_check_sql(column, &uuid.inline_check)
        {
            return Some(RecoveredFormatCheck::Uuid);
        }
    }

    let literals = sql_string_literals(check_sql);
    let mut candidates = Vec::new();
    for literal in &literals {
        let candidate = if literal == &ulid_regex() {
            Some(ValueFormat::Ulid)
        } else {
            type_id_format_from_regex(literal)
        };
        if let Some(candidate) = candidate {
            candidates.push(candidate);
        }
    }
    if dialect == SqlDialect::Sqlite {
        let lower_guard = format!("*[^{TYPE_ID_ALPHABET}]*");
        let upper_guard = format!("*[^{ULID_ALPHABET}]*");
        if literals.iter().any(|literal| literal == &upper_guard) {
            candidates.push(ValueFormat::Ulid);
        }
        if literals.iter().any(|literal| literal == &lower_guard) {
            let stored_prefix = literals.iter().find(|literal| {
                literal.ends_with('_') && !literal.starts_with('*') && literal.as_str() != "[0-7]"
            });
            let prefix = stored_prefix.map_or_else(String::new, |stored| {
                stored.strip_suffix('_').unwrap_or(stored).to_string()
            });
            if validate_type_id_prefix(&prefix).is_ok() {
                candidates.push(ValueFormat::TypeId { prefix });
            }
        }
    }

    let mut unique_candidates = Vec::new();
    for candidate in candidates {
        if !unique_candidates.contains(&candidate) {
            unique_candidates.push(candidate);
        }
    }
    for candidate in unique_candidates {
        let expected = column_metadata(column, &candidate, dialect).ok()?;
        if canonical_check_sql(column, check_sql)
            == canonical_check_sql(column, &expected.inline_check)
        {
            return Some(RecoveredFormatCheck::Value(candidate));
        }
    }
    None
}

fn ulid_regex() -> String {
    format!("^[0-7][{ULID_ALPHABET}]{{{}}}$", ULID_LEN - 1)
}

fn type_id_format_from_regex(regex: &str) -> Option<ValueFormat> {
    let suffix = format!("[0-7][{TYPE_ID_ALPHABET}]{{{}}}$", TYPE_ID_SUFFIX_LEN - 1);
    let stored_prefix = regex.strip_prefix('^')?.strip_suffix(&suffix)?;
    let prefix = if stored_prefix.is_empty() {
        String::new()
    } else {
        stored_prefix.strip_suffix('_')?.to_string()
    };
    validate_type_id_prefix(&prefix).ok()?;
    Some(ValueFormat::TypeId { prefix })
}

fn sql_string_literals(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut literals = Vec::new();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        if bytes[cursor] != b'\'' {
            cursor += 1;
            continue;
        }
        cursor += 1;
        let mut literal = String::new();
        while cursor < bytes.len() {
            if bytes[cursor] == b'\'' {
                if bytes.get(cursor + 1) == Some(&b'\'') {
                    literal.push('\'');
                    cursor += 2;
                    continue;
                }
                cursor += 1;
                literals.push(literal);
                break;
            }
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor] != b'\'' {
                cursor += 1;
            }
            literal.push_str(&sql[start..cursor]);
        }
    }
    literals
}

fn catalog_sql_tokens(column: Option<&str>, sql: &str) -> Vec<String> {
    catalog_sql_tokens_with_dialect(column, sql, None)
}

fn catalog_sql_tokens_with_dialect(
    column: Option<&str>,
    sql: &str,
    dialect: Option<SqlDialect>,
) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte.is_ascii_whitespace() {
            cursor += 1;
            continue;
        }
        if byte == b'\'' {
            let mut literal = String::from("'");
            cursor += 1;
            while cursor < bytes.len() {
                literal.push(char::from(bytes[cursor]));
                if bytes[cursor] == b'\'' {
                    cursor += 1;
                    if bytes.get(cursor) == Some(&b'\'') {
                        literal.push('\'');
                        cursor += 1;
                        continue;
                    }
                    break;
                }
                cursor += 1;
            }
            out.push(literal);
            continue;
        }
        if matches!(byte, b'"' | b'`' | b'[') {
            let close = if byte == b'[' { b']' } else { byte };
            cursor += 1;
            let mut identifier = String::new();
            while cursor < bytes.len() {
                if bytes[cursor] == close {
                    if bytes.get(cursor + 1) == Some(&close) {
                        identifier.push(char::from(close));
                        cursor += 2;
                        continue;
                    }
                    cursor += 1;
                    break;
                }
                let start = cursor;
                while cursor < bytes.len() && bytes[cursor] != close {
                    cursor += 1;
                }
                identifier.push_str(&sql[start..cursor]);
            }
            if column.is_some_and(|column| identifier.eq_ignore_ascii_case(column)) {
                out.push("@column".to_string());
            } else {
                out.push(format!("ident:{identifier}"));
            }
            continue;
        }
        if byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$') {
            let start = cursor;
            cursor += 1;
            while bytes.get(cursor).is_some_and(|candidate| {
                candidate.is_ascii_alphanumeric() || matches!(candidate, b'_' | b'$')
            }) {
                cursor += 1;
            }
            let word = &sql[start..cursor];
            if (dialect.is_none() || dialect == Some(SqlDialect::Mysql))
                && word.starts_with('_')
                && bytes.get(cursor) == Some(&b'\'')
            {
                // MySQL catalog charset introducer; the following literal is
                // retained by the next iteration.
                continue;
            }
            if column.is_some_and(|column| word.eq_ignore_ascii_case(column)) {
                out.push("@column".to_string());
            } else {
                out.push(word.to_ascii_lowercase());
            }
            continue;
        }
        if byte.is_ascii_digit() {
            let start = cursor;
            cursor += 1;
            while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor += 1;
            }
            out.push(sql[start..cursor].to_string());
            continue;
        }
        let two = bytes
            .get(cursor..cursor + 2)
            .and_then(|pair| std::str::from_utf8(pair).ok());
        if matches!(two, Some("::" | "<>" | "!=" | "<=" | ">=" | "<<" | ">>")) {
            out.push(two.expect("matched two-byte operator").to_string());
            cursor += 2;
        } else {
            out.push(char::from(byte.to_ascii_lowercase()).to_string());
            cursor += 1;
        }
    }
    out
}

fn strip_outer_token_parens(mut tokens: &[String]) -> &[String] {
    loop {
        if tokens.first().map(String::as_str) != Some("(")
            || tokens.last().map(String::as_str) != Some(")")
        {
            return tokens;
        }
        let mut depth = 0_i32;
        let mut encloses_all = true;
        for (index, token) in tokens.iter().enumerate() {
            match token.as_str() {
                "(" => depth += 1,
                ")" => depth -= 1,
                _ => {}
            }
            if depth == 0 && index + 1 != tokens.len() {
                encloses_all = false;
                break;
            }
            if depth < 0 {
                return tokens;
            }
        }
        if !encloses_all || depth != 0 {
            return tokens;
        }
        tokens = &tokens[1..tokens.len() - 1];
    }
}

fn split_top_level<'a>(tokens: &'a [String], separator: &str) -> Vec<&'a [String]> {
    let mut depth = 0_i32;
    let mut start = 0_usize;
    let mut parts = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        match token.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            _ if depth == 0 && token == separator => {
                parts.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if !parts.is_empty() {
        parts.push(&tokens[start..]);
    }
    parts
}

fn serialize_tokens(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|token| format!("{}:{token}", token.len()))
        .collect::<Vec<_>>()
        .join("|")
}

#[derive(Debug)]
enum BooleanFingerprint {
    Or(Vec<Self>),
    And(Vec<Self>),
    Atom(String),
}

impl BooleanFingerprint {
    fn parse(tokens: &[String]) -> Self {
        let tokens = strip_outer_token_parens(tokens);
        let or_parts = split_top_level(tokens, "or");
        if !or_parts.is_empty() {
            let mut nodes = Vec::new();
            for part in or_parts {
                match Self::parse(part) {
                    Self::Or(inner) => nodes.extend(inner),
                    node => nodes.push(node),
                }
            }
            return Self::Or(nodes);
        }
        let and_parts = split_top_level(tokens, "and");
        if !and_parts.is_empty() {
            let mut nodes = Vec::new();
            for part in and_parts {
                match Self::parse(part) {
                    Self::And(inner) => nodes.extend(inner),
                    node => nodes.push(node),
                }
            }
            return Self::And(nodes);
        }
        Self::Atom(serialize_tokens(tokens))
    }

    fn serialize(&self) -> String {
        match self {
            Self::Or(nodes) => format!(
                "or({})",
                nodes
                    .iter()
                    .map(Self::serialize)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Self::And(nodes) => format!(
                "and({})",
                nodes
                    .iter()
                    .map(Self::serialize)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Self::Atom(atom) => format!("atom({atom})"),
        }
    }
}

fn canonical_check_sql(column: &str, sql: &str) -> String {
    let mut tokens = catalog_sql_tokens(Some(column), sql);
    strip_pg_catalog_qualifiers(&mut tokens);
    if tokens.first().is_some_and(|token| token == "check") {
        tokens.remove(0);
    }
    // PostgreSQL annotates regex literals as `::text`; that catalog-only cast
    // is not present in the authored CHECK contract.
    let mut cursor = 0_usize;
    while cursor + 1 < tokens.len() {
        if tokens[cursor] == "::" && tokens[cursor + 1] == "text" {
            tokens.drain(cursor..=cursor + 1);
        } else {
            cursor += 1;
        }
    }
    BooleanFingerprint::parse(&tokens).serialize()
}

/// PostgreSQL's deparser qualifies pinned built-ins when a same-spelling object
/// earlier on `search_path` would otherwise change name resolution. The OID is
/// still the same pg_catalog object, so that qualification is catalog decoration
/// rather than format/default semantics. User-schema qualifiers are retained.
fn strip_pg_catalog_qualifiers(tokens: &mut Vec<String>) {
    let mut cursor = 0_usize;
    while cursor + 1 < tokens.len() {
        if matches!(tokens[cursor].as_str(), "pg_catalog" | "ident:pg_catalog")
            && tokens[cursor + 1] == "."
        {
            tokens.drain(cursor..=cursor + 1);
        } else {
            cursor += 1;
        }
    }

    // When a same-spelling operator is visible earlier on search_path,
    // pg_get_constraintdef/pg_get_expr renders the pinned built-in as
    // `OPERATOR(pg_catalog.~)`. After removing the catalog qualifier, discard
    // only the OPERATOR wrapper and retain every punctuation token inside it;
    // multi-byte operators therefore keep the same tokenizer shape as authored
    // infix SQL.
    let mut cursor = 0_usize;
    while cursor + 3 < tokens.len() {
        if tokens[cursor] != "operator" || tokens[cursor + 1] != "(" {
            cursor += 1;
            continue;
        }
        let mut depth = 0_i32;
        let mut close = None;
        for (index, token) in tokens.iter().enumerate().skip(cursor + 1) {
            match token.as_str() {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            break;
        };
        tokens.remove(close);
        tokens.drain(cursor..cursor + 2);
    }
}

/// Catalog-stable fingerprint for the closed expression-default subset. It
/// parses function arguments and bitwise precedence, so MySQL's redundant
/// grouping parentheses normalize away without erasing semantically meaningful
/// grouping (or parentheses inside string literals).
pub(crate) fn catalog_expression_fingerprint(sql: &str) -> String {
    catalog_expression_fingerprint_with_dialect(sql, None)
}

pub(crate) fn catalog_expression_fingerprint_in_dialect(sql: &str, dialect: SqlDialect) -> String {
    catalog_expression_fingerprint_with_dialect(sql, Some(dialect))
}

fn catalog_expression_fingerprint_with_dialect(sql: &str, dialect: Option<SqlDialect>) -> String {
    fn top_level_token(tokens: &[String], needle: &str, from_end: bool) -> Option<usize> {
        let mut depth = 0_i32;
        let mut found = None;
        for (index, token) in tokens.iter().enumerate() {
            match token.as_str() {
                "(" => depth += 1,
                ")" => depth -= 1,
                _ if depth == 0 && token == needle => {
                    if !from_end {
                        return Some(index);
                    }
                    found = Some(index);
                }
                _ => {}
            }
        }
        found
    }

    fn cast_parts(
        tokens: &[String],
        dialect: Option<SqlDialect>,
    ) -> Option<(&[String], &[String])> {
        let tokens = strip_outer_token_parens(tokens);
        if tokens.first().map(String::as_str) == Some("cast")
            && tokens.get(1).map(String::as_str) == Some("(")
            && tokens.last().map(String::as_str) == Some(")")
        {
            let body = &tokens[2..tokens.len() - 1];
            let separator = top_level_token(body, "as", false)?;
            if separator > 0 && separator + 1 < body.len() {
                return Some((&body[..separator], &body[separator + 1..]));
            }
        }
        if let Some(separator) = top_level_token(tokens, "::", true) {
            let operand = &tokens[..separator];
            let target = &tokens[separator + 1..];
            let operand_is_primary = operand.len() == 1
                || call_parts(operand).is_some()
                || (operand.first().map(String::as_str) == Some("(")
                    && operand.last().map(String::as_str) == Some(")")
                    && strip_outer_token_parens(operand).len() < operand.len());
            if separator > 0
                && !target.is_empty()
                && operand_is_primary
                && is_cast_target(target, dialect)
            {
                return Some((operand, target));
            }
        }
        None
    }

    fn is_cast_target(tokens: &[String], dialect: Option<SqlDialect>) -> bool {
        let compact = tokens
            .iter()
            .filter(|token| !matches!(token.as_str(), "(" | ")" | ","))
            .map(String::as_str)
            .collect::<String>();
        let compact = if dialect.is_none() || dialect == Some(SqlDialect::Mysql) {
            compact
                .split_once("charset")
                .map_or(compact.as_str(), |(base, _)| base)
        } else {
            compact.as_str()
        };
        matches!(
            compact,
            "text"
                | "character"
                | "charactervarying"
                | "varchar"
                | "char"
                | "smallint"
                | "integer"
                | "bigint"
                | "int"
                | "int2"
                | "int4"
                | "int8"
                | "numeric"
                | "decimal"
                | "real"
                | "double"
                | "doubleprecision"
                | "boolean"
                | "bool"
                | "bytea"
                | "blob"
                | "binary"
                | "uuid"
        ) || [
            "character",
            "charactervarying",
            "varchar",
            "char",
            "numeric",
            "decimal",
        ]
        .iter()
        .any(|prefix| {
            compact.starts_with(prefix)
                && compact[prefix.len()..]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
        })
    }

    fn cast_target(tokens: &[String], dialect: Option<SqlDialect>) -> String {
        let compact = tokens
            .iter()
            .filter(|token| !matches!(token.as_str(), "(" | ")"))
            .map(String::as_str)
            .collect::<String>();
        if dialect.is_none() || dialect == Some(SqlDialect::Mysql) {
            compact
                .split_once("charset")
                .map_or(compact.as_str(), |(base, _)| base)
                .to_string()
        } else {
            compact
        }
    }

    fn call_parts(tokens: &[String]) -> Option<(&str, &[String])> {
        let tokens = strip_outer_token_parens(tokens);
        if tokens.len() < 3
            || tokens.get(1).map(String::as_str) != Some("(")
            || tokens.last().map(String::as_str) != Some(")")
        {
            return None;
        }
        let mut depth = 0_i32;
        for (index, token) in tokens.iter().enumerate().skip(1) {
            match token.as_str() {
                "(" => depth += 1,
                ")" => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                return (index + 1 == tokens.len())
                    .then_some((tokens[0].as_str(), &tokens[2..tokens.len() - 1]));
            }
        }
        None
    }

    fn normalize_embedded_literals(tokens: &[String], dialect: Option<SqlDialect>) -> Vec<String> {
        let mut normalized = Vec::with_capacity(tokens.len());
        let mut cursor = 0_usize;
        while cursor < tokens.len() {
            let mut best = None;
            for end in cursor + 1..=tokens.len() {
                if let Some(literal) =
                    sql_literal_fingerprint_with_dialect(&tokens[cursor..end].join(" "), dialect)
                {
                    best = Some((end, literal));
                }
            }
            if let Some((end, literal)) = best {
                normalized.push(format!("literal:{literal}"));
                cursor = end;
            } else {
                normalized.push(tokens[cursor].clone());
                cursor += 1;
            }
        }
        normalized
    }

    fn remove_implicit_case_else_null(tokens: &mut Vec<String>, dialect: Option<SqlDialect>) {
        let mut cursor = 0_usize;
        while cursor + 2 < tokens.len() {
            if tokens[cursor] == "else" {
                let implicit_end = (cursor + 2..tokens.len()).find(|end| {
                    tokens[*end] == "end"
                        && sql_literal_fingerprint_with_dialect(
                            &tokens[cursor + 1..*end].join(" "),
                            dialect,
                        )
                        .as_deref()
                            == Some("null")
                });
                if let Some(end) = implicit_end {
                    tokens.drain(cursor..end);
                    continue;
                }
            }
            cursor += 1;
        }
    }

    fn normalize_unary_numeric_literals(tokens: &mut Vec<String>) {
        let mut cursor = 0_usize;
        while cursor + 1 < tokens.len() {
            let sign = tokens[cursor].as_str();
            let unary_context = cursor == 0
                || matches!(
                    tokens[cursor - 1].as_str(),
                    "(" | ","
                        | "+"
                        | "-"
                        | "*"
                        | "/"
                        | "%"
                        | "="
                        | "<>"
                        | "!="
                        | "<"
                        | ">"
                        | "<="
                        | ">="
                        | "&"
                        | "|"
                        | "then"
                        | "else"
                        | "when"
                        | "from"
                        | "as"
                );
            let numeric = tokens[cursor + 1]
                .strip_prefix("literal:")
                .and_then(canonical_decimal_sql_literal);
            if matches!(sign, "+" | "-") && unary_context {
                if let Some(number) = numeric {
                    let signed = if sign == "-" {
                        canonical_decimal_sql_literal(&format!("-{number}"))
                            .expect("a sign plus a decimal remains a decimal")
                    } else {
                        number.to_string()
                    };
                    tokens.splice(cursor..=cursor + 1, [format!("literal:{signed}")]);
                    continue;
                }
            }
            cursor += 1;
        }
    }

    fn expression(tokens: &[String], dialect: Option<SqlDialect>) -> String {
        let tokens = strip_outer_token_parens(tokens);
        // PostgreSQL annotates otherwise-untyped scalar constants while resolving
        // function overloads (`'X'::text`, `'-1'::integer`, ...). Reuse the
        // typed-literal normalizer recursively so those catalog casts compare to
        // the authored scalar value, while nonliteral/value-changing casts remain.
        if let Some(literal) = sql_literal_fingerprint_with_dialect(&tokens.join(" "), dialect) {
            return format!("literal:{literal}");
        }
        if let Some(sign @ ("+" | "-")) = tokens.first().map(String::as_str) {
            if let Some(number) =
                sql_literal_fingerprint_with_dialect(&tokens[1..].join(" "), dialect)
                    .and_then(|number| canonical_decimal_sql_literal(&number))
            {
                return if sign == "-" {
                    format!(
                        "literal:{}",
                        canonical_decimal_sql_literal(&format!("-{number}"))
                            .expect("a sign plus a decimal remains a decimal")
                    )
                } else {
                    format!("literal:{number}")
                };
            }
        }
        for operator in ["|", "&"] {
            let parts = split_top_level(tokens, operator);
            if !parts.is_empty() {
                return format!(
                    "{operator}({})",
                    parts
                        .iter()
                        .map(|part| expression(part, dialect))
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
        }

        if let Some((operand, target)) = cast_parts(tokens, dialect) {
            let target = cast_target(target, dialect);
            return format!("cast:{target}({})", expression(operand, dialect));
        }

        if let Some((name, body)) = call_parts(tokens) {
            if (dialect.is_none() || dialect == Some(SqlDialect::Postgres))
                && name == "trim"
                && body.first().map(String::as_str) == Some("both")
                && body.get(1).map(String::as_str) == Some("from")
            {
                return format!("call:trim({})", expression(&body[2..], dialect));
            }
            let args = split_top_level(body, ",");
            let args = if args.is_empty() && body.is_empty() {
                Vec::new()
            } else if args.is_empty() {
                vec![expression(body, dialect)]
            } else {
                args.into_iter()
                    .map(|argument| expression(argument, dialect))
                    .collect()
            };
            let name = match (dialect, name) {
                (Some(SqlDialect::Mysql), "now" | "current_timestamp") => "current_timestamp",
                (Some(SqlDialect::Mysql), "ceil" | "ceiling") => "ceil",
                (Some(SqlDialect::Postgres) | None, "btrim") => "trim",
                _ => name,
            };
            return format!("call:{name}({})", args.join(","));
        }
        // PostgreSQL materializes an omitted searched-CASE ELSE arm as a typed
        // `ELSE NULL::<resolved type>`. SQL defines omission as exactly ELSE
        // NULL, so erase that deparser-only arm before general leaf rewriting.
        let mut tokens = tokens.to_vec();
        remove_implicit_case_else_null(&mut tokens, dialect);
        let mut tokens = normalize_embedded_literals(&tokens, dialect);
        normalize_unary_numeric_literals(&mut tokens);
        serialize_tokens(&tokens)
    }

    let mut tokens = catalog_sql_tokens_with_dialect(None, sql, dialect);
    if dialect.is_none() || dialect == Some(SqlDialect::Postgres) {
        strip_pg_catalog_qualifiers(&mut tokens);
    }
    expression(&tokens, dialect)
}

/// Lower a logical UUID column to the portable textual contract used on MySQL
/// and SQLite. PostgreSQL's native `uuid` type enforces the representation, so
/// it needs neither an override nor a duplicate `CHECK`.
pub(crate) fn uuid_column_metadata(
    column: &str,
    dialect: SqlDialect,
) -> Result<Option<ValueFormatColumnMetadata>, String> {
    let quoted = quote_ident_for_dialect("UUID column", column, dialect)
        .map_err(|error| error.to_string())?;
    let metadata = match dialect {
        SqlDialect::Postgres => return Ok(None),
        SqlDialect::Mysql => {
            let regex = mysql_grammar_string_literal(
                "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
            );
            ValueFormatColumnMetadata {
                ddl_type: "VARCHAR(36) CHARACTER SET ascii COLLATE ascii_bin".to_string(),
                collation: None,
                inline_check: format!(
                    "CHECK ({quoted} IS NULL OR (CHAR_LENGTH({quoted}) = {UUID_TEXT_LEN} AND \
                     REGEXP_LIKE({quoted}, {regex}, 'c')))"
                ),
            }
        }
        SqlDialect::Sqlite => ValueFormatColumnMetadata {
            ddl_type: "TEXT COLLATE BINARY".to_string(),
            // BINARY is SQLite's canonical default and is represented by None.
            collation: None,
            inline_check: format!(
                "CHECK ({quoted} IS NULL OR (typeof({quoted}) = 'text' AND \
                 length({quoted}) = {UUID_TEXT_LEN} AND \
                 length(CAST({quoted} AS BLOB)) = {UUID_TEXT_LEN} AND \
                 substr({quoted}, 9, 1) = '-' AND substr({quoted}, 14, 1) = '-' AND \
                 substr({quoted}, 19, 1) = '-' AND substr({quoted}, 24, 1) = '-' AND \
                 length({quoted}) - length(replace({quoted}, '-', '')) = 4 AND \
                 replace({quoted}, '-', '') NOT GLOB '*[^0-9a-f]*'))"
            ),
        },
    };
    Ok(Some(metadata))
}

/// Lower one logical value format to its dialect-specific text representation.
///
/// Prefixes are validated here as well as in the policy validator because some
/// internal tests and trusted callers exercise lowering directly. Malformed
/// hand-built IR must fail closed at either entry point.
pub(crate) fn column_metadata(
    column: &str,
    value_format: &ValueFormat,
    dialect: SqlDialect,
) -> Result<ValueFormatColumnMetadata, String> {
    match value_format {
        ValueFormat::TypeId { prefix } => type_id_column_metadata(column, prefix, dialect),
        ValueFormat::Ulid => ulid_column_metadata(column, dialect),
    }
}

fn ulid_column_metadata(
    column: &str,
    dialect: SqlDialect,
) -> Result<ValueFormatColumnMetadata, String> {
    let quoted = quote_ident_for_dialect("ULID column", column, dialect)
        .map_err(|error| error.to_string())?;
    let regex = ulid_regex();

    let (ddl_type, inline_check) = match dialect {
        SqlDialect::Postgres => {
            let regex = sql_string_literal(&regex);
            (
                "text COLLATE \"C\"".to_string(),
                format!(
                    "CHECK ({quoted} IS NULL OR (octet_length({quoted}) = {ULID_LEN} AND \
                     ({quoted} COLLATE \"C\") ~ {regex}))"
                ),
            )
        }
        SqlDialect::Mysql => {
            let regex = mysql_grammar_string_literal(&regex);
            (
                "VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin".to_string(),
                format!(
                    "CHECK ({quoted} IS NULL OR (CHAR_LENGTH({quoted}) = {ULID_LEN} AND \
                     REGEXP_LIKE({quoted}, {regex}, 'c')))"
                ),
            )
        }
        SqlDialect::Sqlite => (
            "TEXT COLLATE BINARY".to_string(),
            format!(
                "CHECK ({quoted} IS NULL OR (typeof({quoted}) = 'text' AND \
                 length({quoted}) = {ULID_LEN} AND \
                 length(CAST({quoted} AS BLOB)) = {ULID_LEN} AND \
                 substr({quoted}, 1, 1) GLOB '[0-7]' AND \
                 substr({quoted}, 1, {ULID_LEN}) NOT GLOB \
                 '*[^{ULID_ALPHABET}]*'))"
            ),
        ),
    };

    Ok(ValueFormatColumnMetadata {
        ddl_type,
        collation: bytewise_catalog_collation(dialect),
        inline_check,
    })
}

fn type_id_column_metadata(
    column: &str,
    prefix: &str,
    dialect: SqlDialect,
) -> Result<ValueFormatColumnMetadata, String> {
    validate_type_id_prefix(prefix)?;

    let quoted = quote_ident_for_dialect("TypeID column", column, dialect)
        .map_err(|error| error.to_string())?;
    let stored_prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}_")
    };
    let suffix_start = stored_prefix.len() + 1; // SQL strings are one-indexed.
    let total_len = stored_prefix.len() + TYPE_ID_SUFFIX_LEN;
    let regex = format!(
        "^{stored_prefix}[0-7][{TYPE_ID_ALPHABET}]{{{}}}$",
        TYPE_ID_SUFFIX_LEN - 1
    );

    let (ddl_type, inline_check) = match dialect {
        SqlDialect::Postgres => {
            let regex = sql_string_literal(&regex);
            (
                "text COLLATE \"C\"".to_string(),
                format!(
                    "CHECK ({quoted} IS NULL OR (octet_length({quoted}) = {total_len} AND \
                     ({quoted} COLLATE \"C\") ~ {regex}))"
                ),
            )
        }
        SqlDialect::Mysql => {
            let regex = mysql_grammar_string_literal(&regex);
            (
                "VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin".to_string(),
                format!(
                    "CHECK ({quoted} IS NULL OR (CHAR_LENGTH({quoted}) = {total_len} AND \
                     REGEXP_LIKE({quoted}, {regex}, 'c')))"
                ),
            )
        }
        SqlDialect::Sqlite => {
            let prefix_predicate = if stored_prefix.is_empty() {
                String::new()
            } else {
                format!(
                    " AND substr({quoted}, 1, {}) = {} COLLATE BINARY",
                    stored_prefix.len(),
                    sql_string_literal(&stored_prefix)
                )
            };
            (
                "TEXT COLLATE BINARY".to_string(),
                format!(
                    "CHECK ({quoted} IS NULL OR (typeof({quoted}) = 'text' AND \
                     length({quoted}) = {total_len} AND \
                     length(CAST({quoted} AS BLOB)) = {total_len}{prefix_predicate} AND \
                     substr({quoted}, {suffix_start}, 1) GLOB '[0-7]' AND \
                     substr({quoted}, {suffix_start}, {TYPE_ID_SUFFIX_LEN}) NOT GLOB \
                     '*[^{TYPE_ID_ALPHABET}]*'))"
                ),
            )
        }
    };

    Ok(ValueFormatColumnMetadata {
        ddl_type,
        collation: bytewise_catalog_collation(dialect),
        inline_check,
    })
}

fn bytewise_catalog_collation(dialect: SqlDialect) -> Option<ColumnCollationSnapshot> {
    matches!(dialect, SqlDialect::Postgres).then(|| ColumnCollationSnapshot {
        schema: Some("pg_catalog".to_string()),
        name: "C".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        authored_id_default, authored_text_id_default, authored_uuid_id_default,
        catalog_expression_fingerprint, catalog_expression_fingerprint_in_dialect,
        catalog_id_default, catalog_id_default_for_expected, catalog_text_id_default,
        catalog_uuid_id_default, column_metadata, recover_format_check, RecoveredFormatCheck,
    };
    use crate::model::expr::{CastTarget, Expr, ScalarFn};
    use crate::model::ir::{IrDefault, IrScalar, SequenceRef, ValueFormat};
    use crate::model::snapshot::IdDefaultSnapshot;
    use crate::schema::query::SqlDialect;

    const LOWER: &str = "0123456789abcdefghjkmnpqrstvwxyz";
    const UPPER: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    /// The measurement behind `apply::drift::index_expression_bodies_are_comparable`'s
    /// verdict that the reduce-both-sides technique does not rescue an index body:
    /// everything the CATALOG injects is already normalised away by this fingerprint,
    /// so what remains between an offline render and a live read is the identifier
    /// QUOTING - and, after a column rename, the identifier itself, which no
    /// normaliser can reconcile.
    #[test]
    fn expression_fingerprint_already_absorbs_what_the_catalog_injects() {
        for (authored, catalog) in [
            (r#"("note" <> 'a')"#, "(note <> 'a'::text)"),
            (r#"("note" || 'x')"#, "(note || 'x'::text)"),
            (r#"("qty" + 1)"#, "(qty + 1)"),
        ] {
            let authored_key = catalog_expression_fingerprint_in_dialect(
                &authored.replace('"', ""),
                SqlDialect::Postgres,
            );
            let catalog_key =
                catalog_expression_fingerprint_in_dialect(catalog, SqlDialect::Postgres);
            assert_eq!(
                authored_key, catalog_key,
                "the injected cast and the added parentheses must already normalise away; \
                 {authored} against {catalog}"
            );
        }
        assert_ne!(
            catalog_expression_fingerprint_in_dialect(r#"("qty" + 1)"#, SqlDialect::Postgres),
            catalog_expression_fingerprint_in_dialect("(qty + 1)", SqlDialect::Postgres),
            "identifier QUOTING is the one thing left between the two sides, so a body \
             comparison would need a rule for it before the rename problem even comes up"
        );
    }

    #[test]
    fn postgres_catalog_parentheses_and_text_cast_recover_exact_format() {
        let check = format!(
            "CHECK (((public_id IS NULL) OR ((pg_catalog.octet_length(public_id) = 34) AND \
             ((public_id COLLATE \"C\") ~ '^account_[0-7][{LOWER}]{{25}}$'::text))))"
        );
        assert_eq!(
            recover_format_check("public_id", &check, SqlDialect::Postgres),
            Some(RecoveredFormatCheck::Value(ValueFormat::TypeId {
                prefix: "account".to_string(),
            }))
        );
        let qualified_operator = check.replacen(" ~ ", " OPERATOR(pg_catalog.~) ", 1);
        assert_eq!(
            recover_format_check("public_id", &qualified_operator, SqlDialect::Postgres),
            Some(RecoveredFormatCheck::Value(ValueFormat::TypeId {
                prefix: "account".to_string(),
            })),
            "a search-path-qualified built-in regex operator is catalog decoration"
        );
    }

    #[test]
    fn mysql_catalog_charset_introducers_recover_ulid() {
        let check = format!(
            "((`event_id` is null) or ((char_length(`event_id`) = 26) and \
             regexp_like(`event_id`,_latin1'^[0-7][{UPPER}]{{25}}$',_ascii'c')))"
        );
        assert_eq!(
            recover_format_check("event_id", &check, SqlDialect::Mysql),
            Some(RecoveredFormatCheck::Value(ValueFormat::Ulid))
        );
    }

    #[test]
    fn any_contract_edit_is_not_recovered_as_the_original_format() {
        let check = format!(
            "CHECK (\"id\" IS NULL OR (octet_length(\"id\") = 99 AND \
             (\"id\" COLLATE \"C\") ~ '^account_[0-7][{LOWER}]{{25}}$'))"
        );
        assert_eq!(
            recover_format_check("id", &check, SqlDialect::Postgres),
            None
        );
    }

    #[test]
    fn sqlite_glob_contract_recovers_type_id_prefix() {
        let check = format!(
            "CHECK (\"id\" IS NULL OR (typeof(\"id\") = 'text' AND length(\"id\") = 34 \
             AND length(CAST(\"id\" AS BLOB)) = 34 AND substr(\"id\", 1, 8) = \
             'account_' COLLATE BINARY AND substr(\"id\", 9, 1) GLOB '[0-7]' AND \
             substr(\"id\", 9, 26) NOT GLOB '*[^{LOWER}]*'))"
        );
        assert_eq!(
            recover_format_check("id", &check, SqlDialect::Sqlite),
            Some(RecoveredFormatCheck::Value(ValueFormat::TypeId {
                prefix: "account".to_string(),
            }))
        );
    }

    #[test]
    fn authored_nextval_uses_the_project_schema_when_the_reference_is_unqualified() {
        let default = IrDefault::Nextval {
            sequence: SequenceRef {
                name: "event_ids".to_string(),
                schema: None,
            },
        };
        assert_eq!(
            authored_id_default(Some(&default), None, SqlDialect::Postgres, Some("app")),
            IdDefaultSnapshot::Nextval(crate::render::declarative::nextval_default_expr(
                &SequenceRef {
                    name: "event_ids".to_string(),
                    schema: Some("app".to_string()),
                }
            ))
        );
    }

    #[test]
    fn catalog_literal_normalization_matches_authored_id_literals() {
        let uuid = "00000000-0000-4000-8000-000000000000";
        let authored_uuid = authored_id_default(
            Some(&IrDefault::Literal {
                value: IrScalar::Str(uuid.to_string()),
            }),
            None,
            SqlDialect::Postgres,
            Some("app"),
        );
        assert_eq!(
            authored_uuid,
            catalog_id_default(Some(&format!("'{uuid}'::uuid")), SqlDialect::Postgres, None,)
        );

        let authored_int64 = authored_id_default(
            Some(&IrDefault::Literal {
                value: IrScalar::Int64(i64::MAX),
            }),
            None,
            SqlDialect::Postgres,
            Some("app"),
        );
        assert_eq!(
            authored_int64,
            catalog_id_default(
                Some("'9223372036854775807'::bigint"),
                SqlDialect::Postgres,
                None,
            )
        );
        assert_eq!(
            authored_uuid,
            catalog_id_default(Some(uuid), SqlDialect::Mysql, Some(false))
        );
        assert_ne!(
            catalog_id_default(Some("uuid()"), SqlDialect::Mysql, Some(false)),
            catalog_id_default(Some("uuid()"), SqlDialect::Mysql, Some(true)),
            "MySQL's DEFAULT_GENERATED marker must distinguish a string literal from a call"
        );

        let expression_literal = IrDefault::Expr {
            expr: crate::model::expr::Expr::Literal {
                value: IrScalar::Str(uuid.to_string()),
            },
        };
        assert_eq!(
            authored_id_default(
                Some(&expression_literal),
                Some(&format!("'{uuid}'")),
                SqlDialect::Postgres,
                Some("app")
            ),
            authored_uuid,
            "an expression-wrapped scalar literal has the same semantic default key"
        );

        for null_default in [
            IrDefault::Literal {
                value: IrScalar::Null,
            },
            IrDefault::Expr {
                expr: crate::model::expr::Expr::Literal {
                    value: IrScalar::Null,
                },
            },
        ] {
            assert_eq!(
                authored_id_default(
                    Some(&null_default),
                    Some("NULL"),
                    SqlDialect::Postgres,
                    Some("app")
                ),
                IdDefaultSnapshot::Absent,
                "DEFAULT NULL is semantically the same as omitting an ID default"
            );
        }
        assert_eq!(
            catalog_id_default(Some("NULL::text"), SqlDialect::Postgres, None),
            IdDefaultSnapshot::Absent
        );
        assert_eq!(
            catalog_id_default(Some("(NULL)"), SqlDialect::Sqlite, None),
            IdDefaultSnapshot::Absent
        );
        assert_eq!(
            catalog_id_default(None, SqlDialect::Mysql, Some(false)),
            IdDefaultSnapshot::Absent
        );
    }

    #[test]
    fn only_postgres_canonicalizes_native_uuid_literal_spelling() {
        let upper = "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA";
        let lower = upper.to_ascii_lowercase();
        let default = IrDefault::Literal {
            value: IrScalar::Str(upper.to_string()),
        };
        let postgres_expected =
            authored_uuid_id_default(Some(&default), None, SqlDialect::Postgres, Some("app"));
        assert_eq!(
            postgres_expected,
            catalog_uuid_id_default(
                Some(&format!("'{lower}'::uuid")),
                SqlDialect::Postgres,
                None,
            )
        );

        for dialect in [SqlDialect::Mysql, SqlDialect::Sqlite] {
            let catalog = if dialect == SqlDialect::Mysql {
                lower.clone()
            } else {
                format!("'{lower}'")
            };
            assert_ne!(
                authored_uuid_id_default(Some(&default), None, dialect, Some("app")),
                catalog_uuid_id_default(
                    Some(&catalog),
                    dialect,
                    (dialect == SqlDialect::Mysql).then_some(false),
                ),
                "{dialect:?} UUID text storage must preserve a case-changing drift"
            );
        }
    }

    #[test]
    fn text_id_scalar_defaults_match_their_rendered_catalog_storage_value() {
        let decimal = IrDefault::Literal {
            value: IrScalar::Decimal("12345678901234567890123456".to_string()),
        };
        for (dialect, rendered, catalog, expression_marker) in [
            (
                SqlDialect::Postgres,
                "'12345678901234567890123456'",
                "'12345678901234567890123456'::text",
                None,
            ),
            (
                SqlDialect::Sqlite,
                "'12345678901234567890123456'",
                "'12345678901234567890123456'",
                None,
            ),
            (
                SqlDialect::Mysql,
                "12345678901234567890123456",
                "12345678901234567890123456",
                Some(false),
            ),
        ] {
            assert_eq!(
                authored_text_id_default(Some(&decimal), Some(rendered), dialect, Some("app")),
                catalog_id_default(Some(catalog), dialect, expression_marker),
                "{dialect:?} must compare the text value actually stored for a decimal ID default"
            );
        }

        column_metadata(
            "type_key",
            &ValueFormat::TypeId {
                prefix: String::new(),
            },
            SqlDialect::Mysql,
        )
        .expect("an empty-prefix TypeID contract is valid");
        let expression_decimal = IrDefault::Expr {
            expr: Expr::Literal {
                value: IrScalar::Decimal("12345678901234567890123456".to_string()),
            },
        };
        assert_eq!(
            authored_text_id_default(
                Some(&expression_decimal),
                Some("12345678901234567890123456"),
                SqlDialect::Mysql,
                Some("app")
            ),
            catalog_id_default(
                Some("12345678901234567890123456"),
                SqlDialect::Mysql,
                Some(false)
            ),
            "a MySQL expression-wrapped literal is emitted and stored as a scalar TypeID default"
        );

        let cast_a = "CAST(12345678901234567890123456 AS CHAR)";
        let cast_b = "CAST(12345678901234567890123457 AS CHAR)";
        assert_ne!(
            catalog_id_default(Some(cast_a), SqlDialect::Mysql, Some(true)),
            catalog_id_default(Some(cast_b), SqlDialect::Mysql, Some(true)),
            "adjacent arbitrary-precision decimal CAST defaults must not collide"
        );

        let numeric_cast = IrDefault::Expr {
            expr: Expr::Cast {
                operand: Box::new(Expr::Literal {
                    value: IrScalar::Decimal("00042".to_string()),
                }),
                target: CastTarget::Int,
            },
        };
        let rendered_numeric_cast = crate::render::dml::render_expr_inline(
            match &numeric_cast {
                IrDefault::Expr { expr } => expr,
                _ => unreachable!("fixture is an expression default"),
            },
            SqlDialect::Mysql,
        )
        .expect("numeric cast renders");
        assert_eq!(
            authored_text_id_default(
                Some(&numeric_cast),
                Some(&rendered_numeric_cast),
                SqlDialect::Mysql,
                Some("app")
            ),
            catalog_text_id_default(Some("cast(42 as signed)"), SqlDialect::Mysql, Some(true)),
            "MySQL coerces a numeric expression default through TypeID character storage"
        );

        let uuid = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let uuid_cast = IrDefault::Expr {
            expr: Expr::Cast {
                operand: Box::new(Expr::Literal {
                    value: IrScalar::Str(uuid.to_string()),
                }),
                target: CastTarget::Uuid,
            },
        };
        let rendered_uuid_cast = crate::render::dml::render_expr_inline(
            match &uuid_cast {
                IrDefault::Expr { expr } => expr,
                _ => unreachable!("fixture is an expression default"),
            },
            SqlDialect::Mysql,
        )
        .expect("UUID cast renders");
        assert_eq!(
            authored_uuid_id_default(
                Some(&uuid_cast),
                Some(&rendered_uuid_cast),
                SqlDialect::Mysql,
                Some("app")
            ),
            catalog_uuid_id_default(
                Some(&format!("cast(_latin1'{uuid}' as char(36) charset latin1)")),
                SqlDialect::Mysql,
                Some(true)
            ),
            "MySQL's resolved charset must not turn a UUID literal CAST into an expression"
        );
    }

    #[test]
    fn explicit_literal_cast_defaults_match_catalog_forms_on_every_dialect() {
        let type_id = "account_01arz3ndektsv4rrffq69g5fav";
        let default = IrDefault::Expr {
            expr: Expr::Cast {
                operand: Box::new(Expr::Literal {
                    value: IrScalar::Str(type_id.to_string()),
                }),
                target: CastTarget::Text,
            },
        };
        let expected = IdDefaultSnapshot::Literal(
            serde_json::to_string(type_id).expect("string serialization"),
        );

        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
            let rendered = crate::render::dml::render_expr_inline(
                match &default {
                    IrDefault::Expr { expr } => expr,
                    _ => unreachable!("fixture is an expression default"),
                },
                dialect,
            )
            .expect("render literal cast");
            assert_eq!(
                authored_id_default(Some(&default), Some(&rendered), dialect, Some("app")),
                expected,
                "authored {dialect:?} literal cast"
            );

            let catalog = if dialect == SqlDialect::Postgres {
                format!("'{type_id}'::text")
            } else {
                rendered
            };
            assert_eq!(
                catalog_id_default(
                    Some(&catalog),
                    dialect,
                    (dialect == SqlDialect::Mysql).then_some(true),
                ),
                expected,
                "catalog {dialect:?} literal cast"
            );
        }
    }

    #[test]
    fn literal_cast_normalization_preserves_value_semantics_and_null() {
        assert_eq!(
            catalog_id_default(Some("CAST('42' AS text)"), SqlDialect::Postgres, None,),
            IdDefaultSnapshot::Literal("\"42\"".to_string())
        );
        assert_eq!(
            catalog_id_default(Some("CAST('42' AS integer)"), SqlDialect::Postgres, None,),
            IdDefaultSnapshot::Literal("42".to_string())
        );
        assert_eq!(
            catalog_id_default(
                Some("(CAST('42' AS integer))::text"),
                SqlDialect::Postgres,
                None,
            ),
            IdDefaultSnapshot::Literal("\"42\"".to_string()),
            "nested casts must be applied from the inside out"
        );

        let null_cast = IrDefault::Expr {
            expr: Expr::Cast {
                operand: Box::new(Expr::Literal {
                    value: IrScalar::Null,
                }),
                target: CastTarget::Bytes,
            },
        };
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
            let rendered = crate::render::dml::render_expr_inline(
                match &null_cast {
                    IrDefault::Expr { expr } => expr,
                    _ => unreachable!("fixture is an expression default"),
                },
                dialect,
            )
            .expect("render NULL cast");
            assert_eq!(
                authored_id_default(Some(&null_cast), Some(&rendered), dialect, Some("app")),
                IdDefaultSnapshot::Absent,
                "authored typed NULL is absence-equivalent on {dialect:?}"
            );
            assert_eq!(
                catalog_id_default(
                    Some(&rendered),
                    dialect,
                    (dialect == SqlDialect::Mysql).then_some(true),
                ),
                IdDefaultSnapshot::Absent,
                "catalog typed NULL is absence-equivalent on {dialect:?}"
            );
        }
    }

    #[test]
    fn expression_fingerprint_preserves_semantic_bitwise_grouping() {
        assert_ne!(
            catalog_expression_fingerprint("(ord(random_bytes(1)) & 15) | 64"),
            catalog_expression_fingerprint("ord(random_bytes(1)) & (15 | 64)")
        );
        assert_eq!(
            catalog_expression_fingerprint("((ord(random_bytes(1)) & 15) | 64)"),
            catalog_expression_fingerprint("(ord(random_bytes(1)) & 15) | 64")
        );
        assert_eq!(
            catalog_expression_fingerprint("lower('X')"),
            catalog_expression_fingerprint("lower('X'::text)"),
            "PostgreSQL's implicit text argument cast is catalog decoration"
        );
        assert_eq!(
            catalog_expression_fingerprint("lower('X')"),
            catalog_expression_fingerprint("pg_catalog.lower('X'::text)"),
            "explicit pg_catalog qualification is deparser decoration"
        );
        assert_eq!(
            catalog_expression_fingerprint("lower('X')"),
            catalog_expression_fingerprint("pg_catalog.lower(('X')::text)"),
            "parenthesized typed call arguments retain call structure"
        );
        assert_eq!(
            catalog_expression_fingerprint("lower(_utf8mb4 X'58')"),
            catalog_expression_fingerprint("lower(_utf8mb4'X')"),
            "MySQL charset-qualified hex and quoted string carriers are equivalent"
        );
        assert_eq!(
            catalog_expression_fingerprint(
                "CASE WHEN true THEN 'account_00' ELSE 'account_01' END"
            ),
            catalog_expression_fingerprint(
                "CASE WHEN true THEN 'account_00'::text ELSE 'account_01'::text END"
            ),
            "typed literals must normalize inside CASE branches"
        );
        assert_eq!(
            catalog_expression_fingerprint("CASE WHEN true THEN 'account_00' END"),
            catalog_expression_fingerprint(
                "CASE WHEN true THEN 'account_00'::text ELSE NULL::text END"
            ),
            "PostgreSQL's implicit searched-CASE ELSE NULL is deparser decoration"
        );
        assert_eq!(
            catalog_expression_fingerprint("'a' || 'b'"),
            catalog_expression_fingerprint("'a'::text || 'b'::text"),
            "typed literals must normalize inside concatenation"
        );
        assert_eq!(
            catalog_expression_fingerprint("CAST(('a' = 'a') AS text)"),
            catalog_expression_fingerprint("(('a'::text = 'a'::text))::text"),
            "typed literals must normalize inside a value-changing outer cast"
        );
        assert_eq!(
            catalog_expression_fingerprint("trim(' X ')"),
            catalog_expression_fingerprint("TRIM(BOTH FROM ' X '::text)"),
            "PostgreSQL's SQL-standard TRIM deparse must match the authored scalar call"
        );
        assert_eq!(
            catalog_expression_fingerprint(
                "substr('account_01arz3ndektsv4rrffq69g5fav', abs(-1), 34)"
            ),
            catalog_expression_fingerprint(
                "substr(_latin1'account_01arz3ndektsv4rrffq69g5fav',abs(-(1)),34)"
            ),
            "MySQL's parenthesized unary numeric literal is catalog decoration"
        );
        assert_eq!(
            catalog_expression_fingerprint("CAST(lower('ACCOUNT_00') AS char)"),
            catalog_expression_fingerprint(
                "cast(lower(_utf8mb4'ACCOUNT_00') as char charset utf8mb4)"
            ),
            "MySQL's resolved character set on CAST AS CHAR is catalog decoration"
        );
        assert_eq!(
            catalog_expression_fingerprint("CAST(lower('A') AS char(36))"),
            catalog_expression_fingerprint("cast(lower(_latin1'A') as char(36) charset latin1)"),
            "resolved charset normalization retains an authored CAST length"
        );
    }

    #[test]
    fn expression_fingerprint_scopes_catalog_aliases_to_their_dialect() {
        for (authored, catalog) in [
            ("CURRENT_TIMESTAMP(6)", "now(6)"),
            ("ceil(1.25)", "ceiling(1.25)"),
        ] {
            assert_eq!(
                catalog_expression_fingerprint_in_dialect(authored, SqlDialect::Mysql),
                catalog_expression_fingerprint_in_dialect(catalog, SqlDialect::Mysql),
                "MySQL's information_schema function alias must stay clean"
            );
            assert_ne!(
                catalog_expression_fingerprint_in_dialect(authored, SqlDialect::Sqlite),
                catalog_expression_fingerprint_in_dialect(catalog, SqlDialect::Sqlite),
                "MySQL-only aliases must remain distinct SQLite expressions"
            );
        }

        assert_eq!(
            catalog_expression_fingerprint_in_dialect("trim(' x ')", SqlDialect::Postgres),
            catalog_expression_fingerprint_in_dialect("btrim(' x ')", SqlDialect::Postgres),
        );
        assert_ne!(
            catalog_expression_fingerprint_in_dialect("trim(' x ')", SqlDialect::Sqlite),
            catalog_expression_fingerprint_in_dialect("btrim(' x ')", SqlDialect::Sqlite),
            "PostgreSQL's btrim deparse alias must not hide a SQLite generator change"
        );
        assert_ne!(
            catalog_expression_fingerprint_in_dialect("uuid()", SqlDialect::Mysql),
            catalog_expression_fingerprint_in_dialect("pg_catalog.uuid()", SqlDialect::Mysql),
            "PostgreSQL catalog qualification is not decoration on MySQL"
        );
    }

    #[test]
    fn postgres_cast_deparsing_preserves_semantics_without_phantom_drift() {
        for (expr, catalog) in [
            (
                Expr::Cast {
                    operand: Box::new(Expr::FnCall {
                        r#fn: ScalarFn::Lower,
                        args: vec![Expr::Literal {
                            value: IrScalar::Str("ABC".to_string()),
                        }],
                    }),
                    target: CastTarget::Text,
                },
                "pg_catalog.lower('ABC'::text)",
            ),
            (
                Expr::Cast {
                    operand: Box::new(Expr::FnCall {
                        r#fn: ScalarFn::Abs,
                        args: vec![Expr::Literal {
                            value: IrScalar::Int(-1),
                        }],
                    }),
                    target: CastTarget::Int,
                },
                "pg_catalog.abs('-1'::integer)",
            ),
        ] {
            let default = IrDefault::Expr { expr };
            let rendered = match &default {
                IrDefault::Expr { expr } => {
                    crate::render::dml::render_expr_inline(expr, SqlDialect::Postgres)
                        .expect("render structured default")
                }
                _ => unreachable!("fixture is an expression default"),
            };
            assert_eq!(
                authored_id_default(
                    Some(&default),
                    Some(&rendered),
                    SqlDialect::Postgres,
                    Some("app")
                ),
                catalog_id_default(Some(catalog), SqlDialect::Postgres, None),
                "a redundant authored cast must match PostgreSQL's deparsed form"
            );
        }

        for (authored, catalog) in [
            (
                "CAST(gen_random_uuid() AS text)",
                "(pg_catalog.gen_random_uuid())::text",
            ),
            (
                "CAST(octet_length('abc') AS bigint)",
                "(pg_catalog.octet_length('abc'::text))::bigint",
            ),
            (
                "CAST(octet_length('abc') AS text)",
                "(pg_catalog.octet_length('abc'::text))::text",
            ),
        ] {
            assert_eq!(
                catalog_expression_fingerprint(authored),
                catalog_expression_fingerprint(catalog),
                "PostgreSQL cast deparsing must normalize {authored:?} and {catalog:?}"
            );
        }
        assert_ne!(
            catalog_expression_fingerprint("CAST(octet_length('abc') AS text)"),
            catalog_expression_fingerprint("octet_length('abc')"),
            "a value-changing integer-to-text cast must remain semantic"
        );
        assert_ne!(
            catalog_expression_fingerprint_in_dialect(
                "CAST(CAST(1.9 AS integer) AS text)",
                SqlDialect::Postgres,
            ),
            catalog_expression_fingerprint_in_dialect(
                "CAST(CAST(1.9 AS real) AS text)",
                SqlDialect::Postgres,
            ),
            "value-changing numeric cast targets must remain part of the drift key"
        );
        assert_ne!(
            catalog_expression_fingerprint_in_dialect("CAST('abc' AS text)", SqlDialect::Postgres,),
            catalog_expression_fingerprint_in_dialect(
                "CAST('abc' AS character(2))",
                SqlDialect::Postgres,
            ),
            "value-changing text typmods must remain part of the drift key"
        );

        let current_user = IrDefault::Expr {
            expr: Expr::Cast {
                operand: Box::new(Expr::FnCall {
                    r#fn: ScalarFn::CurrentUser,
                    args: Vec::new(),
                }),
                target: CastTarget::Text,
            },
        };
        let rendered = crate::render::dml::render_expr_inline(
            match &current_user {
                IrDefault::Expr { expr } => expr,
                _ => unreachable!("fixture is an expression default"),
            },
            SqlDialect::Postgres,
        )
        .expect("CURRENT_USER cast renders");
        assert_eq!(
            authored_id_default(
                Some(&current_user),
                Some(&rendered),
                SqlDialect::Postgres,
                Some("app")
            ),
            catalog_id_default(Some("(CURRENT_USER)::text"), SqlDialect::Postgres, None),
            "PostgreSQL retains an explicit cast around special CURRENT_USER syntax"
        );

        let leading_zero_decimal = IrDefault::Expr {
            expr: Expr::Cast {
                operand: Box::new(Expr::Literal {
                    value: IrScalar::Decimal("001.00".to_string()),
                }),
                target: CastTarget::Text,
            },
        };
        let rendered = crate::render::dml::render_expr_inline(
            match &leading_zero_decimal {
                IrDefault::Expr { expr } => expr,
                _ => unreachable!("fixture is an expression default"),
            },
            SqlDialect::Postgres,
        )
        .expect("decimal text cast renders");
        assert_eq!(
            authored_id_default(
                Some(&leading_zero_decimal),
                Some(&rendered),
                SqlDialect::Postgres,
                Some("app")
            ),
            catalog_id_default(Some("(1.00)::text"), SqlDialect::Postgres, None),
            "numeric parser canonicalization must not drift a leading-zero decimal literal"
        );

        let negative_zero = IrDefault::Expr {
            expr: Expr::Cast {
                operand: Box::new(Expr::Literal {
                    value: IrScalar::Decimal("-0.00".to_string()),
                }),
                target: CastTarget::Text,
            },
        };
        let rendered = crate::render::dml::render_expr_inline(
            match &negative_zero {
                IrDefault::Expr { expr } => expr,
                _ => unreachable!("fixture is an expression default"),
            },
            SqlDialect::Postgres,
        )
        .expect("negative-zero text cast renders");
        assert_eq!(
            authored_id_default(
                Some(&negative_zero),
                Some(&rendered),
                SqlDialect::Postgres,
                Some("app")
            ),
            catalog_id_default(Some("(0.00)::text"), SqlDialect::Postgres, None),
            "PostgreSQL canonicalizes negative numeric zero before a text cast"
        );
    }

    #[test]
    fn qualified_postgres_generators_and_dialect_specific_fallbacks_are_exact() {
        assert_eq!(
            catalog_id_default(
                Some("pg_catalog.gen_random_uuid()"),
                SqlDialect::Postgres,
                None,
            ),
            IdDefaultSnapshot::UuidV4
        );
        assert_eq!(
            catalog_id_default(Some("pg_catalog.uuidv7()"), SqlDialect::Postgres, None,),
            IdDefaultSnapshot::UuidV7
        );
        for dialect in [SqlDialect::Mysql, SqlDialect::Sqlite] {
            assert_ne!(
                catalog_id_default_for_expected(
                    &IdDefaultSnapshot::UuidV4,
                    Some("gen_random_uuid()"),
                    Some(dialect),
                    None,
                ),
                IdDefaultSnapshot::UuidV4,
                "a foreign-dialect generator must not satisfy a typed-reference default"
            );
        }
    }

    #[test]
    fn moving_sqlite_format_parentheses_changes_the_contract() {
        let expected = column_metadata(
            "id",
            &ValueFormat::TypeId {
                prefix: "account".to_string(),
            },
            SqlDialect::Sqlite,
        )
        .expect("TypeID metadata")
        .inline_check;
        let altered = expected.replace(
            "substr(\"id\", 9, 26) NOT GLOB '*[^0123456789abcdefghjkmnpqrstvwxyz]*'",
            "substr(\"id\", 9, 26 NOT GLOB '*[^0123456789abcdefghjkmnpqrstvwxyz]*')",
        );
        assert_ne!(
            altered, expected,
            "fixture must move a semantic parenthesis"
        );
        assert_eq!(
            recover_format_check("id", &altered, SqlDialect::Sqlite),
            None
        );
    }
}
