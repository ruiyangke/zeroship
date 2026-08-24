//! PostgreSQL value-format spelling and catalog normalization.

use zero_migrate_backend::dml::sql_string_literal;
use zero_migrate_backend::snapshot::{ColumnCollationSnapshot, IdDefaultSnapshot};
use zero_migrate_backend::value_format::{
    CatalogSqlContext, LiteralCastKind, ValueFormatColumnMetadata, ValueFormatRenderer,
};
use zero_migrate_ir::dialect::{DialectId, POSTGRES};
use zero_migrate_ir::expr::{BinaryOp, CastTarget, Expr, ScalarFn, SynthFn};
use zero_migrate_ir::ir::{IrScalar, ValueFormat};

const DIALECT: DialectId = POSTGRES;

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
        | Expr::RegexMatch { .. } => Some(PgDefaultType::Boolean),
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
            Expr::InList { expr, .. } | Expr::RegexMatch { expr, .. } => visit(expr),
            Expr::Dialectal { legs } => {
                for leg in legs.values_mut() {
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

fn literal_cast_kind(compact: &str) -> Option<LiteralCastKind> {
    if compact == "uuid" {
        return Some(LiteralCastKind::Uuid);
    }
    if matches!(compact, "text" | "charactervarying" | "varchar") {
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

fn is_catalog_cast_target(compact: &str) -> bool {
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

#[derive(Debug)]
pub(super) struct PostgresValueFormatRenderer;

pub(super) static RENDERER: PostgresValueFormatRenderer = PostgresValueFormatRenderer;

impl ValueFormatRenderer for PostgresValueFormatRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn normalize_authored_default_expr(&self, expr: &Expr) -> Option<Expr> {
        Some(normalize_redundant_pg_default_casts(expr))
    }

    fn normalize_text_literal_snapshot(&self, snapshot: IdDefaultSnapshot) -> IdDefaultSnapshot {
        snapshot
    }

    fn normalize_uuid_literal_snapshot(&self, snapshot: IdDefaultSnapshot) -> IdDefaultSnapshot {
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

    fn catalog_default_is_unquoted_literal(&self, _expression_default: Option<bool>) -> bool {
        false
    }

    fn catalog_default_marker_is_authoritative(&self) -> bool {
        false
    }

    fn authored_storage_uses_rendered_literal(&self) -> bool {
        true
    }

    fn literal_cast_kind(&self, compact_target: &str) -> Option<LiteralCastKind> {
        literal_cast_kind(compact_target)
    }

    fn is_catalog_cast_target(&self, compact_target: &str) -> bool {
        is_catalog_cast_target(compact_target)
    }

    fn canonical_catalog_cast_target(&self, compact_target: &str) -> String {
        compact_target.to_string()
    }

    fn canonical_unattributed_catalog_cast_target(&self, _compact_target: &str) -> Option<String> {
        None
    }

    fn catalog_literal_hex_carrier<'a>(&self, _tokens: &'a [String]) -> Option<&'a str> {
        None
    }

    fn is_catalog_string_introducer(&self, _word: &str, _followed_by_quote: bool) -> bool {
        false
    }

    fn normalize_catalog_tokens(&self, context: CatalogSqlContext, tokens: &mut Vec<String>) {
        if matches!(
            context,
            CatalogSqlContext::Expression | CatalogSqlContext::Check
        ) {
            strip_pg_catalog_qualifiers(tokens);
        }
        if context == CatalogSqlContext::Check {
            // PostgreSQL annotates regex literals as `::text`; that catalog-only
            // cast is not present in the authored CHECK contract.
            let mut cursor = 0_usize;
            while cursor + 1 < tokens.len() {
                if tokens[cursor] == "::" && tokens[cursor + 1] == "text" {
                    tokens.drain(cursor..=cursor + 1);
                } else {
                    cursor += 1;
                }
            }
        }
    }

    fn normalizes_trim_both_from(&self) -> bool {
        true
    }

    fn canonical_catalog_function_name<'a>(&self, name: &'a str) -> &'a str {
        match name {
            "btrim" => "trim",
            _ => name,
        }
    }

    fn canonical_unattributed_catalog_function_name<'a>(&self, name: &'a str) -> Option<&'a str> {
        (name == "btrim").then_some("trim")
    }

    fn uuid_generator_candidates(&self, rendered: &str) -> Vec<String> {
        vec![rendered.to_string(), format!("pg_catalog.{rendered}")]
    }

    fn recovery_candidates(
        &self,
        _literals: &[String],
        _type_id_alphabet: &str,
        _ulid_alphabet: &str,
    ) -> Vec<ValueFormat> {
        Vec::new()
    }

    fn uuid_column_metadata(&self, _quoted: &str) -> Option<ValueFormatColumnMetadata> {
        None
    }

    fn ulid_column_metadata(
        &self,
        quoted: &str,
        regex: &str,
        len: usize,
    ) -> ValueFormatColumnMetadata {
        let regex = sql_string_literal(regex);
        ValueFormatColumnMetadata {
            ddl_type: "text COLLATE \"C\"".to_string(),
            collation: Some(ColumnCollationSnapshot {
                schema: Some("pg_catalog".to_string()),
                name: "C".to_string(),
            }),
            inline_check: format!(
                "CHECK ({quoted} IS NULL OR (octet_length({quoted}) = {len} AND \
                 ({quoted} COLLATE \"C\") ~ {regex}))"
            ),
        }
    }

    fn type_id_column_metadata(
        &self,
        quoted: &str,
        _stored_prefix: &str,
        _suffix_start: usize,
        total_len: usize,
        _suffix_len: usize,
        _alphabet: &str,
        regex: &str,
    ) -> ValueFormatColumnMetadata {
        let regex = sql_string_literal(regex);
        ValueFormatColumnMetadata {
            ddl_type: "text COLLATE \"C\"".to_string(),
            collation: Some(ColumnCollationSnapshot {
                schema: Some("pg_catalog".to_string()),
                name: "C".to_string(),
            }),
            inline_check: format!(
                "CHECK ({quoted} IS NULL OR (octet_length({quoted}) = {total_len} AND \
                 ({quoted} COLLATE \"C\") ~ {regex}))"
            ),
        }
    }

    fn bytewise_column_metadata(
        &self,
        rendered_type: &str,
    ) -> (String, Option<ColumnCollationSnapshot>) {
        (
            format!("{rendered_type} COLLATE \"C\""),
            Some(ColumnCollationSnapshot {
                schema: Some("pg_catalog".to_string()),
                name: "C".to_string(),
            }),
        )
    }
}
