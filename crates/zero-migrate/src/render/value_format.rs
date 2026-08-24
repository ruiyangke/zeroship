//! The engine's DOOR into the value-format seam — and the ONE place a dialect
//! becomes a vendor for a catalog comparison.
//!
//! A [`ValueFormat`] is logical schema metadata carried separately from the physical
//! [`ColType`](crate::model::ir::ColType). Turning that metadata into an exact column
//! collation and inline `CHECK`, and comparing what a catalog gives back against what
//! was authored, is single-sourced in
//! [`zero_migrate_backend::value_format`] so no backend can hold a private opinion
//! about whether two defaults are the same default.
//!
//! # What lives HERE
//!
//! Two things, and the split is the same one `render::dml` draws.
//!
//! The first is the compatibility doors: the comparison functions took a
//! `&DialectId` and resolved a renderer out of the registry, which is a cycle a
//! backend crate cannot participate in (the registry names every vendor, so it sits
//! above them; the comparison sits below them). They take the renderers directly
//! now, and the engine — which genuinely holds a dialect identity and not a renderer
//! — resolves once, here.
//!
//! The second is [`AllRegisteredVendors`], and it is not a door. A snapshot with no
//! backend provenance has to be normalized by every registered vendor's declared
//! rules composed together, because none of them can be ruled out. Composing across
//! vendors needs the registry, so it is the ENGINE's answer and only the engine can
//! write it: a backend crate holds exactly one renderer. It is the second
//! implementor of [`CatalogRules`], beside the contract crate's single-vendor one.
//!
//! The AUTHORED half also stays: it reads `IrDefault` and renders an inline
//! expression, so it needs the engine's IR-facing surface, and no backend calls it.

use crate::model::expr::Expr;
use crate::model::ir::{IrDefault, IrScalar, SequenceRef, ValueFormat};
use crate::model::snapshot::{ColumnCollationSnapshot, IdDefaultSnapshot};
use crate::render::backends::{renderer, value_format_renderer, value_format_renderers};
use zero_migrate_backend::registry::VendorSet;
use zero_migrate_backend::value_format as seam;
use zero_migrate_backend::value_format::{
    id_default_from_literal_fingerprint, CatalogRules, CatalogSqlContext, LiteralCastKind,
    ValueFormatColumnMetadata, VendorRules,
};
use zero_migrate_ir::dialect::DialectId;

// The recovered-format verdict. `#[cfg(test)]` for the same reason the door below
// is: its last production consumer left with the PostgreSQL execution half, and the
// comparison tests here still name it.
#[cfg(test)]
pub(crate) use zero_migrate_backend::value_format::RecoveredFormatCheck;

/// Every registered vendor's declared catalog rules, composed.
///
/// The answer for a legacy snapshot that does not record which backend produced it:
/// no vendor can be ruled out, so each rule is the union (or the first vendor that
/// claims the token) across the whole shipping set. This is the half of
/// [`CatalogRules`] that cannot live in the contract crate — it needs the registry,
/// and the registry names every vendor.
///
/// The per-method composition — `any` versus first-match versus apply-all — is a
/// COMPARISON decision, which is why it is stated here rather than pushed down with
/// the algorithm that consults it.
struct AllRegisteredVendors(VendorSet);

impl CatalogRules for AllRegisteredVendors {
    fn literal_cast_kind(&self, compact_target: &str) -> Option<LiteralCastKind> {
        value_format_renderers(self.0).find_map(|backend| backend.literal_cast_kind(compact_target))
    }

    fn is_catalog_cast_target(&self, compact_target: &str) -> bool {
        value_format_renderers(self.0).any(|backend| backend.is_catalog_cast_target(compact_target))
    }

    fn canonical_catalog_cast_target(&self, compact_target: &str) -> String {
        value_format_renderers(self.0)
            .find_map(|backend| backend.canonical_unattributed_catalog_cast_target(compact_target))
            .unwrap_or_else(|| compact_target.to_string())
    }

    fn catalog_literal_hex_carrier<'a>(&self, tokens: &'a [String]) -> Option<&'a str> {
        value_format_renderers(self.0)
            .find_map(|backend| backend.catalog_literal_hex_carrier(tokens))
    }

    fn is_catalog_string_introducer(&self, word: &str, followed_by_quote: bool) -> bool {
        value_format_renderers(self.0)
            .any(|backend| backend.is_catalog_string_introducer(word, followed_by_quote))
    }

    fn normalize_catalog_tokens(&self, context: CatalogSqlContext, tokens: &mut Vec<String>) {
        for backend in value_format_renderers(self.0) {
            backend.normalize_catalog_tokens(context, tokens);
        }
    }

    fn normalizes_trim_both_from(&self) -> bool {
        value_format_renderers(self.0).any(|backend| backend.normalizes_trim_both_from())
    }

    fn canonical_catalog_function_name<'a>(&self, name: &'a str) -> &'a str {
        value_format_renderers(self.0)
            .find_map(|backend| backend.canonical_unattributed_catalog_function_name(name))
            .unwrap_or(name)
    }
}

/// Project a structured authored default into the narrow ID-default drift key.
pub(crate) fn authored_id_default(
    vendors: VendorSet,
    default: Option<&IrDefault>,
    rendered: Option<&str>,
    dialect: &DialectId,
    default_schema: Option<&str>,
) -> IdDefaultSnapshot {
    let backend = crate::render::backends::value_format_renderer(vendors, dialect);
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
                .and_then(|rendered| sql_literal_fingerprint_in_dialect(vendors, rendered, dialect))
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
                Some(IrDefault::Expr { expr }) => backend
                    .normalize_authored_default_expr(expr)
                    .and_then(|expr| {
                        crate::render::dml::render_expr_inline(vendors, &expr, dialect).ok()
                    }),
                _ => None,
            };
            let rendered = normalized_rendered.as_deref().or(rendered);
            rendered
                .and_then(|rendered| sql_literal_fingerprint_in_dialect(vendors, rendered, dialect))
                .map_or_else(
                    || {
                        IdDefaultSnapshot::Expression(catalog_expression_fingerprint_in_dialect(
                            vendors,
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
    vendors: VendorSet,
    default: Option<&IrDefault>,
    rendered: Option<&str>,
    dialect: &DialectId,
    default_schema: Option<&str>,
) -> IdDefaultSnapshot {
    let backend = crate::render::backends::value_format_renderer(vendors, dialect);
    let snapshot = authored_storage_literal_snapshot(
        vendors,
        authored_id_default(vendors, default, rendered, dialect, default_schema),
        rendered,
        dialect,
    );
    let snapshot = backend.normalize_text_literal_snapshot(snapshot);
    backend.normalize_uuid_literal_snapshot(snapshot)
}

/// TypeID/ULID columns persist character storage. Project authored scalar
/// literals through the actual rendered literal so a decimal carried through
/// the descriptor bridge as a quoted string compares to that stored text on all
/// dialects. MySQL additionally reports a non-expression `COLUMN_DEFAULT` in
/// its coerced character form, without SQL quotes.
pub(crate) fn authored_text_id_default(
    vendors: VendorSet,
    default: Option<&IrDefault>,
    rendered: Option<&str>,
    dialect: &DialectId,
    default_schema: Option<&str>,
) -> IdDefaultSnapshot {
    let backend = crate::render::backends::value_format_renderer(vendors, dialect);
    let snapshot = authored_storage_literal_snapshot(
        vendors,
        authored_id_default(vendors, default, rendered, dialect, default_schema),
        rendered,
        dialect,
    );
    backend.normalize_text_literal_snapshot(snapshot)
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

fn authored_storage_literal_snapshot(
    vendors: VendorSet,
    snapshot: IdDefaultSnapshot,
    rendered: Option<&str>,
    dialect: &DialectId,
) -> IdDefaultSnapshot {
    let backend = crate::render::backends::value_format_renderer(vendors, dialect);
    if !backend.authored_storage_uses_rendered_literal()
        || !matches!(snapshot, IdDefaultSnapshot::Literal(_))
    {
        return snapshot;
    }
    rendered
        .and_then(|rendered| sql_literal_fingerprint_in_dialect(vendors, rendered, dialect))
        .map_or(snapshot, id_default_from_literal_fingerprint)
}

/// Project a live catalog default into the same semantic key as
/// [`authored_id_default`].
pub(crate) fn catalog_id_default(
    vendors: VendorSet,
    default: Option<&str>,
    dialect: &DialectId,
    expression_default: Option<bool>,
) -> IdDefaultSnapshot {
    seam::catalog_id_default(
        default,
        value_format_renderer(vendors, dialect),
        renderer(vendors, dialect),
        expression_default,
    )
}

/// [`catalog_id_default`] with the UUID surface's semantic normalization applied.
pub(crate) fn catalog_uuid_id_default(
    vendors: VendorSet,
    default: Option<&str>,
    dialect: &DialectId,
    expression_default: Option<bool>,
) -> IdDefaultSnapshot {
    seam::catalog_uuid_id_default(
        default,
        value_format_renderer(vendors, dialect),
        renderer(vendors, dialect),
        expression_default,
    )
}

/// [`catalog_id_default`] with the TypeID/ULID text surface's normalization applied.
pub(crate) fn catalog_text_id_default(
    vendors: VendorSet,
    default: Option<&str>,
    dialect: &DialectId,
    expression_default: Option<bool>,
) -> IdDefaultSnapshot {
    seam::catalog_text_id_default(
        default,
        value_format_renderer(vendors, dialect),
        renderer(vendors, dialect),
        expression_default,
    )
}

/// Compare a catalog default whose dialect-specific expression/literal marker
/// was not retained against one expected semantic key. This is used for typed
/// references: their local format CHECK is intentionally absent, but the
/// authored side still declares that their default is an ID-default surface.
///
/// The only entry point that takes an OPTIONAL dialect, which is why it is the
/// engine's rather than the contract crate's: an absent dialect is the
/// no-provenance case, and answering it means composing every registered vendor.
pub(crate) fn catalog_id_default_for_expected(
    vendors: VendorSet,
    expected: &IdDefaultSnapshot,
    default: Option<&str>,
    dialect: Option<&DialectId>,
    expression_default: Option<bool>,
) -> IdDefaultSnapshot {
    let Some(default) = default else {
        return IdDefaultSnapshot::Absent;
    };
    if matches!(expected, IdDefaultSnapshot::UuidLiteral(_)) {
        let Some(dialect) = dialect else {
            return IdDefaultSnapshot::Literal(
                serde_json::to_string(default).expect("string serialization is infallible"),
            );
        };
        return catalog_uuid_id_default(vendors, Some(default), dialect, expression_default);
    }
    if let Some(dialect) = dialect {
        if value_format_renderer(vendors, dialect).catalog_default_marker_is_authoritative() {
            if let Some(expression_default) = expression_default {
                return catalog_id_default(
                    vendors,
                    Some(default),
                    dialect,
                    Some(expression_default),
                );
            }
        }
    }
    if matches!(expected, IdDefaultSnapshot::Literal(_)) {
        if let Some(literal) = sql_literal_fingerprint(vendors, default, dialect) {
            return IdDefaultSnapshot::Literal(literal);
        }
        // MySQL information_schema returns literal text without SQL quotes.
        return IdDefaultSnapshot::Literal(
            serde_json::to_string(default).expect("string serialization is infallible"),
        );
    }
    if let Some(dialect) = dialect {
        let recovered = catalog_id_default(vendors, Some(default), dialect, None);
        if !matches!(recovered, IdDefaultSnapshot::Expression(_)) {
            return recovered;
        }
    } else if let Some(literal) = sql_literal_fingerprint(vendors, default, None) {
        // No dialect claims this snapshot, but a LITERAL is not a dialect's opinion:
        // every registered vendor agrees on one, which is exactly what
        // `AllRegisteredVendors` asks. Without this the arm below spells a plain `7`
        // as `Expression("literal:7")` and leaks an internal fingerprint prefix into
        // the operator's drift line, while the same input under any dialect reads
        // `Literal("7")`.
        //
        // It cannot change a verdict. This arm is reached only when `expected` is
        // neither `UuidLiteral` nor `Literal` - both return earlier - so the value
        // here is compared against a non-literal expectation either way.
        return IdDefaultSnapshot::Literal(literal);
    }
    IdDefaultSnapshot::Expression(catalog_expression_fingerprint_for(
        vendors, default, dialect,
    ))
}

/// The literal comparison key for catalog SQL, under one dialect's rules or —
/// when the snapshot has no provenance — every registered vendor's, composed.
fn sql_literal_fingerprint(
    vendors: VendorSet,
    expression: &str,
    dialect: Option<&DialectId>,
) -> Option<String> {
    match dialect {
        Some(dialect) => seam::sql_literal_fingerprint(
            expression,
            &VendorRules(value_format_renderer(vendors, dialect)),
        ),
        None => seam::sql_literal_fingerprint(expression, &AllRegisteredVendors(vendors)),
    }
}

fn sql_literal_fingerprint_in_dialect(
    vendors: VendorSet,
    expression: &str,
    dialect: &DialectId,
) -> Option<String> {
    sql_literal_fingerprint(vendors, expression, Some(dialect))
}

/// Catalog-stable fingerprint for the closed expression-default subset, composed
/// across every registered vendor when the snapshot has no provenance.
pub(crate) fn catalog_expression_fingerprint(vendors: VendorSet, sql: &str) -> String {
    seam::catalog_expression_fingerprint(sql, &AllRegisteredVendors(vendors))
}

pub(crate) fn catalog_expression_fingerprint_in_dialect(
    vendors: VendorSet,
    sql: &str,
    dialect: &DialectId,
) -> String {
    seam::catalog_expression_fingerprint(sql, &VendorRules(value_format_renderer(vendors, dialect)))
}

fn catalog_expression_fingerprint_for(
    vendors: VendorSet,
    sql: &str,
    dialect: Option<&DialectId>,
) -> String {
    dialect.map_or_else(
        || catalog_expression_fingerprint(vendors, sql),
        |dialect| catalog_expression_fingerprint_in_dialect(vendors, sql, dialect),
    )
}

/// Recover an engine-owned UUID/TypeID/ULID CHECK from catalog SQL.
///
/// `#[cfg(test)]` because its last PRODUCTION caller left with the PostgreSQL
/// execution half: all three vendors now enter
/// [`zero_migrate_backend::value_format::recover_format_check`] with their OWN
/// renderers, which is what the contract crate's copy takes, so nothing in the
/// engine holds a `DialectId` and needs it turned into a pair of renderers here. The
/// comparison tests below still drive it, and they are the reason it is gated rather
/// than deleted: they are the engine's own proof that the dialect-resolving spelling
/// and the renderer-taking one answer identically.
#[cfg(test)]
pub(crate) fn recover_format_check(
    column: &str,
    check_sql: &str,
    dialect: &DialectId,
) -> Option<RecoveredFormatCheck> {
    seam::recover_format_check(
        column,
        check_sql,
        value_format_renderer(crate::test_fixtures::VENDORS, dialect),
        renderer(crate::test_fixtures::VENDORS, dialect),
    )
}

/// Lower a logical UUID column to the portable textual contract used on MySQL
/// and SQLite. PostgreSQL's native `uuid` type enforces the representation, so
/// it needs neither an override nor a duplicate `CHECK`.
pub(crate) fn uuid_column_metadata(
    vendors: VendorSet,
    column: &str,
    dialect: &DialectId,
) -> Result<Option<ValueFormatColumnMetadata>, String> {
    seam::uuid_column_metadata(
        column,
        value_format_renderer(vendors, dialect),
        renderer(vendors, dialect),
    )
}

/// Lower one logical value format to its dialect-specific text representation.
pub(crate) fn column_metadata(
    vendors: VendorSet,
    column: &str,
    format: &ValueFormat,
    dialect: &DialectId,
) -> Result<ValueFormatColumnMetadata, String> {
    seam::column_metadata(
        column,
        format,
        value_format_renderer(vendors, dialect),
        renderer(vendors, dialect),
    )
}

/// The physical column details implied by a bare
/// [`ColumnCollation::Bytewise`](crate::model::ir::ColumnCollation::Bytewise) facet
/// on a column whose DDL type is already spelled as `rendered_type`.
///
/// This is the value formats' collation half WITHOUT their format half. A value
/// format also pins a length, an alphabet and a CHECK, because its whole subject is
/// what the column may HOLD; this facet's subject is only how the column COMPARES,
/// so it changes nothing but the collation and leaves the type spelling the caller
/// already decided.
///
///   * PostgreSQL: `COLLATE "C"`, and the same `pg_catalog."C"` catalog identity the
///     value formats record — so a live introspection of either compares equal.
///   * SQLite: `COLLATE BINARY`. That is already SQLite's default, so the snapshot
///     collation stays `None` (introspection canonicalizes BINARY to `None`, and a
///     `Some` here would make every such table drift on the first read).
///   * MySQL: `utf8mb4_0900_bin` — NO PAD and a memcmp of the encoded bytes, which
///     is what `C` and BINARY both mean. NOT the legacy `utf8mb4_bin`, which is PAD
///     SPACE and would make `'x'` and `'x '` the same key. NOT the value formats'
///     `CHARACTER SET ascii`: those earn ascii from a CHECK proving the content is
///     ascii, and this facet has no such proof — narrowing the charset would turn a
///     silent ordering bug into a loud rejected INSERT on a `created_by` holding a
///     non-ascii name.
///
/// Returns `None` when the dialect needs no override, which is never today but keeps
/// the caller from assuming one exists.
pub(crate) fn bytewise_column_metadata(
    vendors: VendorSet,
    rendered_type: &str,
    dialect: &DialectId,
) -> (String, Option<ColumnCollationSnapshot>) {
    value_format_renderer(vendors, dialect).bytewise_column_metadata(rendered_type)
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
    use crate::test_fixtures::{MYSQL, POSTGRES, SQLITE};

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
                crate::test_fixtures::VENDORS,
                &authored.replace('"', ""),
                &POSTGRES,
            );
            let catalog_key = catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                catalog,
                &POSTGRES,
            );
            assert_eq!(
                authored_key, catalog_key,
                "the injected cast and the added parentheses must already normalise away; \
                 {authored} against {catalog}"
            );
        }
        assert_ne!(
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                r#"("qty" + 1)"#,
                &POSTGRES
            ),
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                "(qty + 1)",
                &POSTGRES
            ),
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
            recover_format_check("public_id", &check, &POSTGRES),
            Some(RecoveredFormatCheck::Value(ValueFormat::TypeId {
                prefix: "account".to_string(),
            }))
        );
        let qualified_operator = check.replacen(" ~ ", " OPERATOR(pg_catalog.~) ", 1);
        assert_eq!(
            recover_format_check("public_id", &qualified_operator, &POSTGRES),
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
            recover_format_check("event_id", &check, &MYSQL),
            Some(RecoveredFormatCheck::Value(ValueFormat::Ulid))
        );
    }

    #[test]
    fn any_contract_edit_is_not_recovered_as_the_original_format() {
        let check = format!(
            "CHECK (\"id\" IS NULL OR (octet_length(\"id\") = 99 AND \
             (\"id\" COLLATE \"C\") ~ '^account_[0-7][{LOWER}]{{25}}$'))"
        );
        assert_eq!(recover_format_check("id", &check, &POSTGRES), None);
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
            recover_format_check("id", &check, &SQLITE),
            Some(RecoveredFormatCheck::Value(ValueFormat::TypeId {
                prefix: "account".to_string(),
            }))
        );
    }

    /// A CHECK recovery is normalised by ONE dialect's rules — the dialect it was
    /// read from — and not by every registered vendor's at once.
    ///
    /// `canonical_check_sql` used to take no dialect, so it ran all three vendors'
    /// `normalize_catalog_tokens` over the same token stream in sequence. Only
    /// PostgreSQL implements that hook, so a SQLite or MySQL contract was silently
    /// normalised by POSTGRESQL's catalog rules: `pg_catalog.` qualifiers and
    /// `::text` annotations were erased from a stream that can never contain them
    /// legitimately. An edit that injected either therefore normalised back onto the
    /// pristine contract and was recovered as valid — the exact failure
    /// `any_contract_edit_is_not_recovered_as_the_original_format` forbids, reached
    /// through a foreign vendor's normaliser instead of through a weakened comparison.
    #[test]
    fn a_foreign_vendors_catalog_decoration_does_not_normalise_away() {
        let pristine = column_metadata(
            crate::test_fixtures::VENDORS,
            "id",
            &ValueFormat::TypeId {
                prefix: "account".to_string(),
            },
            &SQLITE,
        )
        .expect("TypeID metadata")
        .inline_check;
        assert_eq!(
            recover_format_check("id", &pristine, &SQLITE),
            Some(RecoveredFormatCheck::Value(ValueFormat::TypeId {
                prefix: "account".to_string(),
            })),
            "the pristine SQLite contract must still recover"
        );

        for decorated in [
            pristine.replacen("typeof(", "pg_catalog.typeof(", 1),
            pristine.replacen("'text'", "'text'::text", 1),
        ] {
            assert_ne!(decorated, pristine, "fixture must actually decorate");
            assert_eq!(
                recover_format_check("id", &decorated, &SQLITE),
                None,
                "SQLite declares no catalog-token normalisation, so PostgreSQL's must \
                 not run on a SQLite CHECK: {decorated}"
            );
        }
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
            authored_id_default(
                crate::test_fixtures::VENDORS,
                Some(&default),
                None,
                &POSTGRES,
                Some("app")
            ),
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
            crate::test_fixtures::VENDORS,
            Some(&IrDefault::Literal {
                value: IrScalar::Str(uuid.to_string()),
            }),
            None,
            &POSTGRES,
            Some("app"),
        );
        assert_eq!(
            authored_uuid,
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some(&format!("'{uuid}'::uuid")),
                &POSTGRES,
                None,
            )
        );

        let authored_int64 = authored_id_default(
            crate::test_fixtures::VENDORS,
            Some(&IrDefault::Literal {
                value: IrScalar::Int64(i64::MAX),
            }),
            None,
            &POSTGRES,
            Some("app"),
        );
        assert_eq!(
            authored_int64,
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("'9223372036854775807'::bigint"),
                &POSTGRES,
                None
            )
        );
        assert_eq!(
            authored_uuid,
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some(uuid),
                &MYSQL,
                Some(false)
            )
        );
        assert_ne!(
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("uuid()"),
                &MYSQL,
                Some(false)
            ),
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("uuid()"),
                &MYSQL,
                Some(true)
            ),
            "MySQL's DEFAULT_GENERATED marker must distinguish a string literal from a call"
        );

        let expression_literal = IrDefault::Expr {
            expr: crate::model::expr::Expr::Literal {
                value: IrScalar::Str(uuid.to_string()),
            },
        };
        assert_eq!(
            authored_id_default(
                crate::test_fixtures::VENDORS,
                Some(&expression_literal),
                Some(&format!("'{uuid}'")),
                &POSTGRES,
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
                    crate::test_fixtures::VENDORS,
                    Some(&null_default),
                    Some("NULL"),
                    &POSTGRES,
                    Some("app")
                ),
                IdDefaultSnapshot::Absent,
                "DEFAULT NULL is semantically the same as omitting an ID default"
            );
        }
        assert_eq!(
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("NULL::text"),
                &POSTGRES,
                None
            ),
            IdDefaultSnapshot::Absent
        );
        assert_eq!(
            catalog_id_default(crate::test_fixtures::VENDORS, Some("(NULL)"), &SQLITE, None),
            IdDefaultSnapshot::Absent
        );
        assert_eq!(
            catalog_id_default(crate::test_fixtures::VENDORS, None, &MYSQL, Some(false)),
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
        let postgres_expected = authored_uuid_id_default(
            crate::test_fixtures::VENDORS,
            Some(&default),
            None,
            &POSTGRES,
            Some("app"),
        );
        assert_eq!(
            postgres_expected,
            catalog_uuid_id_default(
                crate::test_fixtures::VENDORS,
                Some(&format!("'{lower}'::uuid")),
                &POSTGRES,
                None,
            )
        );

        for dialect in [&MYSQL, &SQLITE] {
            let catalog = if dialect == &MYSQL {
                lower.clone()
            } else {
                format!("'{lower}'")
            };
            assert_ne!(
                authored_uuid_id_default(
                    crate::test_fixtures::VENDORS,
                    Some(&default),
                    None,
                    dialect,
                    Some("app")
                ),
                catalog_uuid_id_default(
                    crate::test_fixtures::VENDORS,
                    Some(&catalog),
                    dialect,
                    (dialect == &MYSQL).then_some(false),
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
                &POSTGRES,
                "'12345678901234567890123456'",
                "'12345678901234567890123456'::text",
                None,
            ),
            (
                &SQLITE,
                "'12345678901234567890123456'",
                "'12345678901234567890123456'",
                None,
            ),
            (
                &MYSQL,
                "12345678901234567890123456",
                "12345678901234567890123456",
                Some(false),
            ),
        ] {
            assert_eq!(
                authored_text_id_default(
                    crate::test_fixtures::VENDORS,
                    Some(&decimal),
                    Some(rendered),
                    dialect,
                    Some("app")
                ),
                catalog_id_default(
                    crate::test_fixtures::VENDORS,
                    Some(catalog),
                    dialect,
                    expression_marker
                ),
                "{dialect:?} must compare the text value actually stored for a decimal ID default"
            );
        }

        column_metadata(
            crate::test_fixtures::VENDORS,
            "type_key",
            &ValueFormat::TypeId {
                prefix: String::new(),
            },
            &MYSQL,
        )
        .expect("an empty-prefix TypeID contract is valid");
        let expression_decimal = IrDefault::Expr {
            expr: Expr::Literal {
                value: IrScalar::Decimal("12345678901234567890123456".to_string()),
            },
        };
        assert_eq!(
            authored_text_id_default(
                crate::test_fixtures::VENDORS,
                Some(&expression_decimal),
                Some("12345678901234567890123456"),
                &MYSQL,
                Some("app")
            ),
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("12345678901234567890123456"),
                &MYSQL,
                Some(false)
            ),
            "a MySQL expression-wrapped literal is emitted and stored as a scalar TypeID default"
        );

        let cast_a = "CAST(12345678901234567890123456 AS CHAR)";
        let cast_b = "CAST(12345678901234567890123457 AS CHAR)";
        assert_ne!(
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some(cast_a),
                &MYSQL,
                Some(true)
            ),
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some(cast_b),
                &MYSQL,
                Some(true)
            ),
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
            crate::test_fixtures::VENDORS,
            match &numeric_cast {
                IrDefault::Expr { expr } => expr,
                _ => unreachable!("fixture is an expression default"),
            },
            &MYSQL,
        )
        .expect("numeric cast renders");
        assert_eq!(
            authored_text_id_default(
                crate::test_fixtures::VENDORS,
                Some(&numeric_cast),
                Some(&rendered_numeric_cast),
                &MYSQL,
                Some("app")
            ),
            catalog_text_id_default(
                crate::test_fixtures::VENDORS,
                Some("cast(42 as signed)"),
                &MYSQL,
                Some(true)
            ),
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
            crate::test_fixtures::VENDORS,
            match &uuid_cast {
                IrDefault::Expr { expr } => expr,
                _ => unreachable!("fixture is an expression default"),
            },
            &MYSQL,
        )
        .expect("UUID cast renders");
        assert_eq!(
            authored_uuid_id_default(
                crate::test_fixtures::VENDORS,
                Some(&uuid_cast),
                Some(&rendered_uuid_cast),
                &MYSQL,
                Some("app")
            ),
            catalog_uuid_id_default(
                crate::test_fixtures::VENDORS,
                Some(&format!("cast(_latin1'{uuid}' as char(36) charset latin1)")),
                &MYSQL,
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

        for dialect in [&POSTGRES, &SQLITE, &MYSQL] {
            let rendered = crate::render::dml::render_expr_inline(
                crate::test_fixtures::VENDORS,
                match &default {
                    IrDefault::Expr { expr } => expr,
                    _ => unreachable!("fixture is an expression default"),
                },
                dialect,
            )
            .expect("render literal cast");
            assert_eq!(
                authored_id_default(
                    crate::test_fixtures::VENDORS,
                    Some(&default),
                    Some(&rendered),
                    dialect,
                    Some("app")
                ),
                expected,
                "authored {dialect:?} literal cast"
            );

            let catalog = if dialect == &POSTGRES {
                format!("'{type_id}'::text")
            } else {
                rendered
            };
            assert_eq!(
                catalog_id_default(
                    crate::test_fixtures::VENDORS,
                    Some(&catalog),
                    dialect,
                    (dialect == &MYSQL).then_some(true),
                ),
                expected,
                "catalog {dialect:?} literal cast"
            );
        }
    }

    #[test]
    fn literal_cast_normalization_preserves_value_semantics_and_null() {
        assert_eq!(
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("CAST('42' AS text)"),
                &POSTGRES,
                None
            ),
            IdDefaultSnapshot::Literal("\"42\"".to_string())
        );
        assert_eq!(
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("CAST('42' AS integer)"),
                &POSTGRES,
                None
            ),
            IdDefaultSnapshot::Literal("42".to_string())
        );
        assert_eq!(
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("(CAST('42' AS integer))::text"),
                &POSTGRES,
                None
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
        for dialect in [&POSTGRES, &SQLITE, &MYSQL] {
            let rendered = crate::render::dml::render_expr_inline(
                crate::test_fixtures::VENDORS,
                match &null_cast {
                    IrDefault::Expr { expr } => expr,
                    _ => unreachable!("fixture is an expression default"),
                },
                dialect,
            )
            .expect("render NULL cast");
            assert_eq!(
                authored_id_default(
                    crate::test_fixtures::VENDORS,
                    Some(&null_cast),
                    Some(&rendered),
                    dialect,
                    Some("app")
                ),
                IdDefaultSnapshot::Absent,
                "authored typed NULL is absence-equivalent on {dialect:?}"
            );
            assert_eq!(
                catalog_id_default(
                    crate::test_fixtures::VENDORS,
                    Some(&rendered),
                    dialect,
                    (dialect == &MYSQL).then_some(true),
                ),
                IdDefaultSnapshot::Absent,
                "catalog typed NULL is absence-equivalent on {dialect:?}"
            );
        }
    }

    #[test]
    fn expression_fingerprint_preserves_semantic_bitwise_grouping() {
        assert_ne!(
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "(ord(random_bytes(1)) & 15) | 64"
            ),
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "ord(random_bytes(1)) & (15 | 64)"
            )
        );
        assert_eq!(
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "((ord(random_bytes(1)) & 15) | 64)"
            ),
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "(ord(random_bytes(1)) & 15) | 64"
            )
        );
        assert_eq!(
            catalog_expression_fingerprint(crate::test_fixtures::VENDORS, "lower('X')"),
            catalog_expression_fingerprint(crate::test_fixtures::VENDORS, "lower('X'::text)"),
            "PostgreSQL's implicit text argument cast is catalog decoration"
        );
        assert_eq!(
            catalog_expression_fingerprint(crate::test_fixtures::VENDORS, "lower('X')"),
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "pg_catalog.lower('X'::text)"
            ),
            "explicit pg_catalog qualification is deparser decoration"
        );
        assert_eq!(
            catalog_expression_fingerprint(crate::test_fixtures::VENDORS, "lower('X')"),
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "pg_catalog.lower(('X')::text)"
            ),
            "parenthesized typed call arguments retain call structure"
        );
        assert_eq!(
            catalog_expression_fingerprint(crate::test_fixtures::VENDORS, "lower(_utf8mb4 X'58')"),
            catalog_expression_fingerprint(crate::test_fixtures::VENDORS, "lower(_utf8mb4'X')"),
            "MySQL charset-qualified hex and quoted string carriers are equivalent"
        );
        assert_eq!(
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "CASE WHEN true THEN 'account_00' ELSE 'account_01' END"
            ),
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "CASE WHEN true THEN 'account_00'::text ELSE 'account_01'::text END"
            ),
            "typed literals must normalize inside CASE branches"
        );
        assert_eq!(
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "CASE WHEN true THEN 'account_00' END"
            ),
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "CASE WHEN true THEN 'account_00'::text ELSE NULL::text END"
            ),
            "PostgreSQL's implicit searched-CASE ELSE NULL is deparser decoration"
        );
        assert_eq!(
            catalog_expression_fingerprint(crate::test_fixtures::VENDORS, "'a' || 'b'"),
            catalog_expression_fingerprint(crate::test_fixtures::VENDORS, "'a'::text || 'b'::text"),
            "typed literals must normalize inside concatenation"
        );
        assert_eq!(
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "CAST(('a' = 'a') AS text)"
            ),
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "(('a'::text = 'a'::text))::text"
            ),
            "typed literals must normalize inside a value-changing outer cast"
        );
        assert_eq!(
            catalog_expression_fingerprint(crate::test_fixtures::VENDORS, "trim(' X ')"),
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "TRIM(BOTH FROM ' X '::text)"
            ),
            "PostgreSQL's SQL-standard TRIM deparse must match the authored scalar call"
        );
        assert_eq!(
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "substr('account_01arz3ndektsv4rrffq69g5fav', abs(-1), 34)"
            ),
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "substr(_latin1'account_01arz3ndektsv4rrffq69g5fav',abs(-(1)),34)"
            ),
            "MySQL's parenthesized unary numeric literal is catalog decoration"
        );
        assert_eq!(
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "CAST(lower('ACCOUNT_00') AS char)"
            ),
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "cast(lower(_utf8mb4'ACCOUNT_00') as char charset utf8mb4)"
            ),
            "MySQL's resolved character set on CAST AS CHAR is catalog decoration"
        );
        assert_eq!(
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "CAST(lower('A') AS char(36))"
            ),
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "cast(lower(_latin1'A') as char(36) charset latin1)"
            ),
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
                catalog_expression_fingerprint_in_dialect(
                    crate::test_fixtures::VENDORS,
                    authored,
                    &MYSQL
                ),
                catalog_expression_fingerprint_in_dialect(
                    crate::test_fixtures::VENDORS,
                    catalog,
                    &MYSQL
                ),
                "MySQL's information_schema function alias must stay clean"
            );
            assert_ne!(
                catalog_expression_fingerprint_in_dialect(
                    crate::test_fixtures::VENDORS,
                    authored,
                    &SQLITE
                ),
                catalog_expression_fingerprint_in_dialect(
                    crate::test_fixtures::VENDORS,
                    catalog,
                    &SQLITE
                ),
                "MySQL-only aliases must remain distinct SQLite expressions"
            );
        }

        assert_eq!(
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                "trim(' x ')",
                &POSTGRES
            ),
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                "btrim(' x ')",
                &POSTGRES
            ),
        );
        assert_ne!(
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                "trim(' x ')",
                &SQLITE
            ),
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                "btrim(' x ')",
                &SQLITE
            ),
            "PostgreSQL's btrim deparse alias must not hide a SQLite generator change"
        );
        assert_ne!(
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                "uuid()",
                &MYSQL
            ),
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                "pg_catalog.uuid()",
                &MYSQL
            ),
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
                IrDefault::Expr { expr } => crate::render::dml::render_expr_inline(
                    crate::test_fixtures::VENDORS,
                    expr,
                    &POSTGRES,
                )
                .expect("render structured default"),
                _ => unreachable!("fixture is an expression default"),
            };
            assert_eq!(
                authored_id_default(
                    crate::test_fixtures::VENDORS,
                    Some(&default),
                    Some(&rendered),
                    &POSTGRES,
                    Some("app")
                ),
                catalog_id_default(
                    crate::test_fixtures::VENDORS,
                    Some(catalog),
                    &POSTGRES,
                    None
                ),
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
                catalog_expression_fingerprint(crate::test_fixtures::VENDORS, authored),
                catalog_expression_fingerprint(crate::test_fixtures::VENDORS, catalog),
                "PostgreSQL cast deparsing must normalize {authored:?} and {catalog:?}"
            );
        }
        assert_ne!(
            catalog_expression_fingerprint(
                crate::test_fixtures::VENDORS,
                "CAST(octet_length('abc') AS text)"
            ),
            catalog_expression_fingerprint(crate::test_fixtures::VENDORS, "octet_length('abc')"),
            "a value-changing integer-to-text cast must remain semantic"
        );
        assert_ne!(
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                "CAST(CAST(1.9 AS integer) AS text)",
                &POSTGRES,
            ),
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                "CAST(CAST(1.9 AS real) AS text)",
                &POSTGRES
            ),
            "value-changing numeric cast targets must remain part of the drift key"
        );
        assert_ne!(
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                "CAST('abc' AS text)",
                &POSTGRES
            ),
            catalog_expression_fingerprint_in_dialect(
                crate::test_fixtures::VENDORS,
                "CAST('abc' AS character(2))",
                &POSTGRES
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
            crate::test_fixtures::VENDORS,
            match &current_user {
                IrDefault::Expr { expr } => expr,
                _ => unreachable!("fixture is an expression default"),
            },
            &POSTGRES,
        )
        .expect("CURRENT_USER cast renders");
        assert_eq!(
            authored_id_default(
                crate::test_fixtures::VENDORS,
                Some(&current_user),
                Some(&rendered),
                &POSTGRES,
                Some("app")
            ),
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("(CURRENT_USER)::text"),
                &POSTGRES,
                None
            ),
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
            crate::test_fixtures::VENDORS,
            match &leading_zero_decimal {
                IrDefault::Expr { expr } => expr,
                _ => unreachable!("fixture is an expression default"),
            },
            &POSTGRES,
        )
        .expect("decimal text cast renders");
        assert_eq!(
            authored_id_default(
                crate::test_fixtures::VENDORS,
                Some(&leading_zero_decimal),
                Some(&rendered),
                &POSTGRES,
                Some("app")
            ),
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("(1.00)::text"),
                &POSTGRES,
                None
            ),
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
            crate::test_fixtures::VENDORS,
            match &negative_zero {
                IrDefault::Expr { expr } => expr,
                _ => unreachable!("fixture is an expression default"),
            },
            &POSTGRES,
        )
        .expect("negative-zero text cast renders");
        assert_eq!(
            authored_id_default(
                crate::test_fixtures::VENDORS,
                Some(&negative_zero),
                Some(&rendered),
                &POSTGRES,
                Some("app")
            ),
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("(0.00)::text"),
                &POSTGRES,
                None
            ),
            "PostgreSQL canonicalizes negative numeric zero before a text cast"
        );
    }

    #[test]
    fn qualified_postgres_generators_and_dialect_specific_fallbacks_are_exact() {
        assert_eq!(
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("pg_catalog.gen_random_uuid()"),
                &POSTGRES,
                None
            ),
            IdDefaultSnapshot::UuidV4
        );
        assert_eq!(
            catalog_id_default(
                crate::test_fixtures::VENDORS,
                Some("pg_catalog.uuidv7()"),
                &POSTGRES,
                None
            ),
            IdDefaultSnapshot::UuidV7
        );
        for dialect in [&MYSQL, &SQLITE] {
            assert_ne!(
                catalog_id_default_for_expected(
                    crate::test_fixtures::VENDORS,
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

    /// An unattributed snapshot still spells a plain literal as a LITERAL.
    ///
    /// `catalog_id_default_for_expected` reaches its last arm only when `expected`
    /// is neither [`IdDefaultSnapshot::UuidLiteral`] nor [`IdDefaultSnapshot::Literal`]
    /// — both are answered earlier — so the value it returns there can never make a
    /// drift verdict flip: a `Literal` and an `Expression` are equally unequal to an
    /// `Absent` expectation. What it CAN do is decide what the operator reads. With
    /// no dialect the arm used to fall through to `Expression("literal:7")`, leaking
    /// an internal fingerprint prefix into a report line, while every registered
    /// dialect answered `Literal("7")` for the same input.
    ///
    /// The dialect-free answer already existed: [`sql_literal_fingerprint`] takes an
    /// `Option<&DialectId>` and routes a `None` through `AllRegisteredVendors`. The
    /// arm simply never asked it. This pins that it does, and pins the agreement —
    /// an unattributed snapshot and a claimed one must not spell one literal two
    /// ways, or a reader comparing two drift reports sees a difference that is not
    /// in the database.
    #[test]
    fn an_unattributed_literal_is_spelled_as_a_literal_not_an_expression() {
        let unattributed = catalog_id_default_for_expected(
            crate::test_fixtures::VENDORS,
            &IdDefaultSnapshot::Absent,
            Some("7"),
            None,
            None,
        );
        assert_eq!(
            unattributed,
            IdDefaultSnapshot::Literal("7".to_string()),
            "with no dialect a plain literal must still read as a literal, not as an \
             expression fingerprint"
        );
        for dialect in [&POSTGRES, &MYSQL, &SQLITE] {
            assert_eq!(
                catalog_id_default_for_expected(
                    crate::test_fixtures::VENDORS,
                    &IdDefaultSnapshot::Absent,
                    Some("7"),
                    Some(dialect),
                    None,
                ),
                unattributed,
                "a claimed snapshot and an unattributed one must spell one literal the \
                 same way"
            );
        }
    }

    #[test]
    fn moving_sqlite_format_parentheses_changes_the_contract() {
        let expected = column_metadata(
            crate::test_fixtures::VENDORS,
            "id",
            &ValueFormat::TypeId {
                prefix: "account".to_string(),
            },
            &SQLITE,
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
        assert_eq!(recover_format_check("id", &altered, &SQLITE), None);
    }
}
