//! Neutral contract for vendor-owned value-format rendering, plus the ONE catalog
//! normalization every backend compares through.
//!
//! Logical UUID, ULID and TypeID intent belongs to the IR. The exact storage type,
//! collation, `CHECK` spelling, and catalog deparser normalization belong to the
//! backend that emits or reads them. [`ValueFormatRenderer`] names that boundary
//! without supplying a shared vendor answer.
//!
//! # Why the COMPARISON lives here and not in the engine
//!
//! `catalog_id_default`, `recover_format_check` and the fingerprint machinery under
//! them are single-sourced comparison logic, not vendor spelling: two defaults or two
//! format `CHECK`s are equivalent by ONE algorithm, and a backend that wrote its own
//! copy would be a backend whose drift verdict could disagree with the engine's.
//!
//! They used to sit in `zeroship_migrate::render::value_format`, where they resolved a
//! renderer out of the engine's registry from a `DialectId`. That is the same
//! compressed cycle `crate::dml`'s header describes: the registry sits ABOVE the
//! vendors and the comparison sits BELOW them, so no crate can hold both, and a
//! backend crate calling the engine's resolver is a vendor asking a registry to hand
//! the vendor back to itself. They take the renderers directly now - a backend passes
//! its own, and the engine, which holds a dialect identity rather than a renderer,
//! resolves once at its own door.
//!
//! # The two shapes of "whose rules", and why one of them cannot be built here
//!
//! [`CatalogRules`] is the seam. A snapshot that records which backend produced it is
//! normalized by THAT backend alone: [`VendorRules`] is that arm, and a backend crate
//! builds it from the one renderer it owns.
//!
//! A legacy snapshot with no backend provenance has to compose every registered
//! vendor's declared rules, because none of them can be ruled out. That arm is the
//! ENGINE's, it needs the registry, and it is deliberately absent from this crate:
//! composing across vendors is something only the crate that knows all of them can do,
//! and leaving the trait open is what keeps that knowledge out of a backend.

use crate::renderer::DmlRenderer;
use crate::snapshot::{
    canonical_id_default_expression, ColumnCollationSnapshot, IdDefaultSnapshot,
};
use zeroship_migrate_ir::dialect::DialectId;
use zeroship_migrate_ir::expr::Expr;
use zeroship_migrate_ir::ir::{validate_type_id_prefix, ValueFormat};

/// The physical column details implied by one logical value format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueFormatColumnMetadata {
    /// Exact backend DDL type, including the format's bytewise collation.
    pub ddl_type: String,
    /// Exact non-default catalog collation identity, when the backend exposes
    /// one independently from its DDL type spelling.
    pub collation: Option<ColumnCollationSnapshot>,
    /// Null-tolerant canonical spelling check, including its `CHECK` wrapper.
    pub inline_check: String,
}

/// The scalar meaning of a catalog cast target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralCastKind {
    Text,
    SignedInteger { bits: u8 },
    UnsignedInteger { bits: u8 },
    ExactNumeric,
    Real,
    Boolean,
    Uuid,
}

/// Where catalog SQL tokens are being normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSqlContext {
    Literal,
    Expression,
    Check,
}

/// Vendor facts used by the engine's value-format comparison and rendering logic.
///
/// Every method is required. A backend must state each spelling and normalization
/// rule in its own crate; it cannot inherit one from a shipping vendor or from the
/// contract crate.
pub trait ValueFormatRenderer: std::fmt::Debug + Sync {
    fn dialect(&self) -> DialectId;
    fn normalize_authored_default_expr(&self, expr: &Expr) -> Option<Expr>;
    fn normalize_text_literal_snapshot(&self, snapshot: IdDefaultSnapshot) -> IdDefaultSnapshot;
    fn normalize_uuid_literal_snapshot(&self, snapshot: IdDefaultSnapshot) -> IdDefaultSnapshot;
    fn catalog_default_is_unquoted_literal(&self, expression_default: Option<bool>) -> bool;
    fn catalog_default_marker_is_authoritative(&self) -> bool;
    fn authored_storage_uses_rendered_literal(&self) -> bool;

    fn literal_cast_kind(&self, compact_target: &str) -> Option<LiteralCastKind>;
    fn is_catalog_cast_target(&self, compact_target: &str) -> bool;
    fn canonical_catalog_cast_target(&self, compact_target: &str) -> String;
    fn canonical_unattributed_catalog_cast_target(&self, compact_target: &str) -> Option<String>;
    fn catalog_literal_hex_carrier<'a>(&self, tokens: &'a [String]) -> Option<&'a str>;
    fn is_catalog_string_introducer(&self, word: &str, followed_by_quote: bool) -> bool;
    fn normalize_catalog_tokens(&self, context: CatalogSqlContext, tokens: &mut Vec<String>);
    fn normalizes_trim_both_from(&self) -> bool;
    fn canonical_catalog_function_name<'a>(&self, name: &'a str) -> &'a str;
    fn canonical_unattributed_catalog_function_name<'a>(&self, name: &'a str) -> Option<&'a str>;
    fn uuid_generator_candidates(&self, rendered: &str) -> Vec<String>;
    fn recovery_candidates(
        &self,
        literals: &[String],
        type_id_alphabet: &str,
        ulid_alphabet: &str,
    ) -> Vec<ValueFormat>;

    fn uuid_column_metadata(&self, quoted: &str) -> Option<ValueFormatColumnMetadata>;
    fn ulid_column_metadata(
        &self,
        quoted: &str,
        regex: &str,
        len: usize,
    ) -> ValueFormatColumnMetadata;
    #[allow(clippy::too_many_arguments)]
    fn type_id_column_metadata(
        &self,
        quoted: &str,
        stored_prefix: &str,
        suffix_start: usize,
        total_len: usize,
        suffix_len: usize,
        alphabet: &str,
        regex: &str,
    ) -> ValueFormatColumnMetadata;
    fn bytewise_column_metadata(
        &self,
        rendered_type: &str,
    ) -> (String, Option<ColumnCollationSnapshot>);
}

/// Whose catalog-normalization rules one comparison runs under.
///
/// Every method answers a vendor FACT, and the two shapes differ only in whose.
/// [`VendorRules`] is one backend's, which is what a snapshot carrying its backend's
/// provenance gets. The other shape - every registered vendor's, composed - belongs
/// to the engine and is why this is a trait rather than a `&dyn ValueFormatRenderer`:
/// a backend crate holds exactly one renderer and cannot express it.
///
/// The methods are the subset of [`ValueFormatRenderer`] that the normalization
/// reaches WITHOUT knowing which vendor it is talking to. Everything else it needs is
/// reached through a renderer it was handed directly.
pub trait CatalogRules {
    /// The scalar meaning of a catalog cast target, if any rule recognizes it.
    fn literal_cast_kind(&self, compact_target: &str) -> Option<LiteralCastKind>;
    /// Whether `compact_target` is a cast target at all.
    fn is_catalog_cast_target(&self, compact_target: &str) -> bool;
    /// The canonical spelling of a catalog cast target.
    fn canonical_catalog_cast_target(&self, compact_target: &str) -> String;
    /// The quoted-string token carrying a hex-encoded literal, if these rules
    /// spell one that way.
    fn catalog_literal_hex_carrier<'a>(&self, tokens: &'a [String]) -> Option<&'a str>;
    /// Whether `word` introduces a following string literal rather than being one.
    fn is_catalog_string_introducer(&self, word: &str, followed_by_quote: bool) -> bool;
    /// Apply every token normalization these rules declare, in place.
    fn normalize_catalog_tokens(&self, context: CatalogSqlContext, tokens: &mut Vec<String>);
    /// Whether `trim(both from x)` is the same call as `trim(x)`.
    fn normalizes_trim_both_from(&self) -> bool;
    /// The canonical spelling of a catalog function name.
    fn canonical_catalog_function_name<'a>(&self, name: &'a str) -> &'a str;
}

/// One backend's rules - the arm a vendor crate builds for its own snapshots.
#[derive(Debug, Clone, Copy)]
pub struct VendorRules<'a>(pub &'a dyn ValueFormatRenderer);

impl CatalogRules for VendorRules<'_> {
    fn literal_cast_kind(&self, compact_target: &str) -> Option<LiteralCastKind> {
        self.0.literal_cast_kind(compact_target)
    }

    fn is_catalog_cast_target(&self, compact_target: &str) -> bool {
        self.0.is_catalog_cast_target(compact_target)
    }

    fn canonical_catalog_cast_target(&self, compact_target: &str) -> String {
        self.0.canonical_catalog_cast_target(compact_target)
    }

    fn catalog_literal_hex_carrier<'a>(&self, tokens: &'a [String]) -> Option<&'a str> {
        self.0.catalog_literal_hex_carrier(tokens)
    }

    fn is_catalog_string_introducer(&self, word: &str, followed_by_quote: bool) -> bool {
        self.0.is_catalog_string_introducer(word, followed_by_quote)
    }

    fn normalize_catalog_tokens(&self, context: CatalogSqlContext, tokens: &mut Vec<String>) {
        self.0.normalize_catalog_tokens(context, tokens);
    }

    fn normalizes_trim_both_from(&self) -> bool {
        self.0.normalizes_trim_both_from()
    }

    fn canonical_catalog_function_name<'a>(&self, name: &'a str) -> &'a str {
        self.0.canonical_catalog_function_name(name)
    }
}

const TYPE_ID_SUFFIX_LEN: usize = 26;
const TYPE_ID_ALPHABET: &str = "0123456789abcdefghjkmnpqrstvwxyz";
const ULID_LEN: usize = 26;
const ULID_ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Project a live catalog default into the same semantic key as
/// the engine's `authored_id_default`. `expression_default` is the authoritative catalog
/// expression/literal distinction when the backend exposes one: some catalogs strip SQL
/// quotes from literals, so the text alone cannot distinguish a literal such as
/// `"uuid()"` from an expression.
pub fn catalog_id_default(
    default: Option<&str>,
    value_format: &dyn ValueFormatRenderer,
    dml: &dyn DmlRenderer,
    expression_default: Option<bool>,
) -> IdDefaultSnapshot {
    let Some(default) = default else {
        return IdDefaultSnapshot::Absent;
    };

    let rules = VendorRules(value_format);
    if value_format.catalog_default_is_unquoted_literal(expression_default) {
        return IdDefaultSnapshot::Literal(
            serde_json::to_string(default).expect("string serialization is infallible"),
        );
    }
    if default_matches_uuid(default, value_format, dml, false) {
        return IdDefaultSnapshot::UuidV4;
    }
    if default_matches_uuid(default, value_format, dml, true) {
        return IdDefaultSnapshot::UuidV7;
    }
    if let Some(literal) = sql_literal_fingerprint(default, &rules) {
        return id_default_from_literal_fingerprint(literal);
    }
    IdDefaultSnapshot::Expression(catalog_expression_fingerprint(default, &rules))
}

/// [`catalog_id_default`] with the UUID surface's semantic normalization applied.
///
/// UUID columns accept several textual spellings while a catalog deparses one; the
/// backend states which, so the comparison key does not depend on which spelling was
/// stored.
pub fn catalog_uuid_id_default(
    default: Option<&str>,
    value_format: &dyn ValueFormatRenderer,
    dml: &dyn DmlRenderer,
    expression_default: Option<bool>,
) -> IdDefaultSnapshot {
    let snapshot = catalog_id_default(default, value_format, dml, expression_default);
    let snapshot = value_format.normalize_text_literal_snapshot(snapshot);
    value_format.normalize_uuid_literal_snapshot(snapshot)
}

/// [`catalog_id_default`] with the TypeID/ULID text surface's normalization applied.
pub fn catalog_text_id_default(
    default: Option<&str>,
    value_format: &dyn ValueFormatRenderer,
    dml: &dyn DmlRenderer,
    expression_default: Option<bool>,
) -> IdDefaultSnapshot {
    let snapshot = catalog_id_default(default, value_format, dml, expression_default);
    value_format.normalize_text_literal_snapshot(snapshot)
}

fn default_matches_uuid(
    default: &str,
    value_format: &dyn ValueFormatRenderer,
    dml: &dyn DmlRenderer,
    v7: bool,
) -> bool {
    let rules = VendorRules(value_format);
    let rendered = if v7 {
        dml.uuid_v7().ok()
    } else {
        Some(dml.uuid_v4())
    };
    rendered.is_some_and(|rendered| {
        let actual = catalog_expression_fingerprint(default, &rules);
        value_format
            .uuid_generator_candidates(&rendered)
            .iter()
            .any(|candidate| actual == catalog_expression_fingerprint(candidate, &rules))
    })
}

/// A literal fingerprint projected back into the ID-default key vocabulary.
pub fn id_default_from_literal_fingerprint(literal: String) -> IdDefaultSnapshot {
    if literal == "null" {
        IdDefaultSnapshot::Absent
    } else {
        IdDefaultSnapshot::Literal(literal)
    }
}

fn canonical_decimal_sql_literal(value: &str) -> Option<String> {
    if !zeroship_migrate_ir::ir::is_decimal_string(value) {
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

/// The comparison key for catalog SQL that denotes a literal, or `None` when it
/// denotes something else.
///
/// Casts are folded through `rules`' declared scalar meanings, so a catalog's typed
/// annotation of an untyped constant compares equal to the authored scalar.
pub fn sql_literal_fingerprint(expression: &str, backend: &dyn CatalogRules) -> Option<String> {
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

    fn cast_kind(tokens: &[String], backend: &dyn CatalogRules) -> Option<LiteralCastKind> {
        backend.literal_cast_kind(&tokens.join(""))
    }

    fn apply_cast(input: String, target: &[String], backend: &dyn CatalogRules) -> Option<String> {
        // A typed NULL remains the absence-equivalent SQL NULL even for cast
        // targets outside the portable scalar surface (for example BYTEA).
        if input == "null" {
            return Some(input);
        }

        let kind = cast_kind(target, backend)?;
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

    fn leaf(tokens: &[String], backend: &dyn CatalogRules) -> Option<String> {
        if tokens.len() == 1 {
            if let Some(decoded) = decode_quoted_string(&tokens[0]) {
                return serde_json::to_string(&decoded).ok();
            }
        }

        if let Some(carrier) = backend.catalog_literal_hex_carrier(tokens) {
            let encoded = decode_quoted_string(carrier)?;
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

    fn parse(tokens: &[String], backend: &dyn CatalogRules) -> Option<String> {
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
            let input = parse(&body[..separator], backend)?;
            return apply_cast(input, &body[separator + 1..], backend);
        }

        if let Some(separator) = top_level_token(tokens, "::", true) {
            if separator == 0 || separator + 1 == tokens.len() {
                return None;
            }
            let input = parse(&tokens[..separator], backend)?;
            return apply_cast(input, &tokens[separator + 1..], backend);
        }

        leaf(tokens, backend)
    }

    parse(
        &catalog_sql_tokens_with_backend(None, expression, backend, CatalogSqlContext::Literal),
        backend,
    )
}

/// Engine-owned format contract recovered from one catalog CHECK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveredFormatCheck {
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
pub fn recover_format_check(
    column: &str,
    check_sql: &str,
    value_format: &dyn ValueFormatRenderer,
    dml: &dyn DmlRenderer,
) -> Option<RecoveredFormatCheck> {
    let rules = VendorRules(value_format);
    if let Ok(Some(uuid)) = uuid_column_metadata(column, value_format, dml) {
        if canonical_check_sql(column, check_sql, &rules)
            == canonical_check_sql(column, &uuid.inline_check, &rules)
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
    candidates.extend(value_format.recovery_candidates(&literals, TYPE_ID_ALPHABET, ULID_ALPHABET));

    let mut unique_candidates = Vec::new();
    for candidate in candidates {
        if !unique_candidates.contains(&candidate) {
            unique_candidates.push(candidate);
        }
    }
    for candidate in unique_candidates {
        let expected = column_metadata(column, &candidate, value_format, dml).ok()?;
        if canonical_check_sql(column, check_sql, &rules)
            == canonical_check_sql(column, &expected.inline_check, &rules)
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

fn catalog_sql_tokens_with_backend(
    column: Option<&str>,
    sql: &str,
    backend: &dyn CatalogRules,
    context: CatalogSqlContext,
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
            let followed_by_quote = bytes.get(cursor) == Some(&b'\'');
            if backend.is_catalog_string_introducer(word, followed_by_quote) {
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
    backend.normalize_catalog_tokens(context, &mut out);
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

fn canonical_check_sql(column: &str, sql: &str, backend: &dyn CatalogRules) -> String {
    let mut tokens =
        catalog_sql_tokens_with_backend(Some(column), sql, backend, CatalogSqlContext::Check);
    if tokens.first().is_some_and(|token| token == "check") {
        tokens.remove(0);
    }
    BooleanFingerprint::parse(&tokens).serialize()
}

/// Catalog-stable fingerprint for the closed expression-default subset. It
/// parses function arguments and bitwise precedence, so MySQL's redundant
/// grouping parentheses normalize away without erasing semantically meaningful
/// grouping (or parentheses inside string literals).
pub fn catalog_expression_fingerprint(sql: &str, backend: &dyn CatalogRules) -> String {
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

    fn cast_parts<'a>(
        tokens: &'a [String],
        backend: &dyn CatalogRules,
    ) -> Option<(&'a [String], &'a [String])> {
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
                && is_cast_target(target, backend)
            {
                return Some((operand, target));
            }
        }
        None
    }

    fn is_cast_target(tokens: &[String], backend: &dyn CatalogRules) -> bool {
        let compact = tokens
            .iter()
            .filter(|token| !matches!(token.as_str(), "(" | ")" | ","))
            .map(String::as_str)
            .collect::<String>();
        backend.is_catalog_cast_target(&compact)
    }

    fn cast_target(tokens: &[String], backend: &dyn CatalogRules) -> String {
        let compact = tokens
            .iter()
            .filter(|token| !matches!(token.as_str(), "(" | ")"))
            .map(String::as_str)
            .collect::<String>();
        backend.canonical_catalog_cast_target(&compact)
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

    fn normalize_embedded_literals(tokens: &[String], backend: &dyn CatalogRules) -> Vec<String> {
        let mut normalized = Vec::with_capacity(tokens.len());
        let mut cursor = 0_usize;
        while cursor < tokens.len() {
            let mut best = None;
            for end in cursor + 1..=tokens.len() {
                if let Some(literal) =
                    sql_literal_fingerprint(&tokens[cursor..end].join(" "), backend)
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

    fn remove_implicit_case_else_null(tokens: &mut Vec<String>, backend: &dyn CatalogRules) {
        let mut cursor = 0_usize;
        while cursor + 2 < tokens.len() {
            if tokens[cursor] == "else" {
                let implicit_end = (cursor + 2..tokens.len()).find(|end| {
                    tokens[*end] == "end"
                        && sql_literal_fingerprint(&tokens[cursor + 1..*end].join(" "), backend)
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

    fn expression(tokens: &[String], backend: &dyn CatalogRules) -> String {
        let tokens = strip_outer_token_parens(tokens);
        // PostgreSQL annotates otherwise-untyped scalar constants while resolving
        // function overloads (`'X'::text`, `'-1'::integer`, ...). Reuse the
        // typed-literal normalizer recursively so those catalog casts compare to
        // the authored scalar value, while nonliteral/value-changing casts remain.
        if let Some(literal) = sql_literal_fingerprint(&tokens.join(" "), backend) {
            return format!("literal:{literal}");
        }
        if let Some(sign @ ("+" | "-")) = tokens.first().map(String::as_str) {
            if let Some(number) = sql_literal_fingerprint(&tokens[1..].join(" "), backend)
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
                        .map(|part| expression(part, backend))
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
        }

        if let Some((operand, target)) = cast_parts(tokens, backend) {
            let target = cast_target(target, backend);
            return format!("cast:{target}({})", expression(operand, backend));
        }

        if let Some((name, body)) = call_parts(tokens) {
            if backend.normalizes_trim_both_from()
                && name == "trim"
                && body.first().map(String::as_str) == Some("both")
                && body.get(1).map(String::as_str) == Some("from")
            {
                return format!("call:trim({})", expression(&body[2..], backend));
            }
            let args = split_top_level(body, ",");
            let args = if args.is_empty() && body.is_empty() {
                Vec::new()
            } else if args.is_empty() {
                vec![expression(body, backend)]
            } else {
                args.into_iter()
                    .map(|argument| expression(argument, backend))
                    .collect()
            };
            let name = backend.canonical_catalog_function_name(name);
            return format!("call:{name}({})", args.join(","));
        }
        // PostgreSQL materializes an omitted searched-CASE ELSE arm as a typed
        // `ELSE NULL::<resolved type>`. SQL defines omission as exactly ELSE
        // NULL, so erase that deparser-only arm before general leaf rewriting.
        let mut tokens = tokens.to_vec();
        remove_implicit_case_else_null(&mut tokens, backend);
        let mut tokens = normalize_embedded_literals(&tokens, backend);
        normalize_unary_numeric_literals(&mut tokens);
        serialize_tokens(&tokens)
    }

    let tokens = catalog_sql_tokens_with_backend(None, sql, backend, CatalogSqlContext::Expression);
    expression(&tokens, backend)
}

/// Lower a logical UUID column to the portable textual contract used on MySQL
/// and SQLite. PostgreSQL's native `uuid` type enforces the representation, so
/// it needs neither an override nor a duplicate `CHECK`.
pub fn uuid_column_metadata(
    column: &str,
    value_format: &dyn ValueFormatRenderer,
    dml: &dyn DmlRenderer,
) -> Result<Option<ValueFormatColumnMetadata>, String> {
    let quoted = crate::dml::quote_ident_for_backend("UUID column", column, dml)
        .map_err(|error| error.to_string())?;
    Ok(value_format.uuid_column_metadata(&quoted))
}

/// Lower one logical value format to its dialect-specific text representation.
///
/// Prefixes are validated here as well as in the policy validator because some
/// internal tests and trusted callers exercise lowering directly. Malformed
/// hand-built IR must fail closed at either entry point.
pub fn column_metadata(
    column: &str,
    format: &ValueFormat,
    value_format: &dyn ValueFormatRenderer,
    dml: &dyn DmlRenderer,
) -> Result<ValueFormatColumnMetadata, String> {
    match format {
        ValueFormat::TypeId { prefix } => {
            type_id_column_metadata(column, prefix, value_format, dml)
        }
        ValueFormat::Ulid => ulid_column_metadata(column, value_format, dml),
    }
}

fn ulid_column_metadata(
    column: &str,
    value_format: &dyn ValueFormatRenderer,
    dml: &dyn DmlRenderer,
) -> Result<ValueFormatColumnMetadata, String> {
    let quoted = crate::dml::quote_ident_for_backend("ULID column", column, dml)
        .map_err(|error| error.to_string())?;
    let regex = ulid_regex();
    Ok(value_format.ulid_column_metadata(&quoted, &regex, ULID_LEN))
}

fn type_id_column_metadata(
    column: &str,
    prefix: &str,
    value_format: &dyn ValueFormatRenderer,
    dml: &dyn DmlRenderer,
) -> Result<ValueFormatColumnMetadata, String> {
    validate_type_id_prefix(prefix)?;

    let quoted = crate::dml::quote_ident_for_backend("TypeID column", column, dml)
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
    Ok(value_format.type_id_column_metadata(
        &quoted,
        &stored_prefix,
        suffix_start,
        total_len,
        TYPE_ID_SUFFIX_LEN,
        TYPE_ID_ALPHABET,
        &regex,
    ))
}
