//! MySQL's `CHARACTER SET` / `COLLATE` spelling.
//!
//! # Why this is its own module and where it came from
//!
//! These spelling functions lived in `zero_migrate::render::declarative`, the
//! engine's declarative differ. They now sit beside `MysqlSchemaRenderer`, so the
//! vendor renderer has no edge back into core.
//!
//! They are MySQL SPELLING by the boundary rule stated in
//! `zero_migrate::render::backends`: `utf8mb4_0900_as_cs` versus
//! `utf8mb4_0900_ai_ci` is how this vendor WRITES case sensitivity, not a decision
//! the engine makes about it. `MysqlSchemaRenderer::column_type` pins every rendered
//! character spelling here, including native `ENUM(...)`.

/// The `CHARACTER SET` / `COLLATE` clause a MySQL character column pins, derived from
/// the portable `caseSensitive` intent.
///
/// ONE spelling of the engine's collation choice, so the `VARCHAR`/`CHAR`/`TEXT`
/// family and `ENUM` cannot drift apart. `None` is the canonical
/// snapshot spelling for the default case-SENSITIVE intent - see
/// `apply::backend::mysql::drift_sql::case_sensitive_from_collation`, which is the
/// inverse of this function and never emits `Some(true)`.
pub fn mysql_collation_clause(case_sensitive: Option<bool>) -> &'static str {
    if matches!(case_sensitive, Some(false)) {
        "CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci"
    } else {
        "CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs"
    }
}

/// Pin an explicit collation onto ANY rendered MySQL character-type spelling.
///
/// Both snapshot-native and SDK-token carriers reach the MySQL schema renderer and
/// route their resulting character spelling through this function, so they cannot
/// pin different collations or drift from [`mysql_collation_clause`].
///
/// The predicate is [`mysql_spelling_takes_collation`], on the RENDERED text rather
/// than on a type token, because that is all a spelling-level pin has. A non-character
/// spelling is returned untouched - `JSON COLLATE ...` is not merely redundant, MySQL
/// refuses to parse it, so a pin that guessed wrong would fail the `CREATE TABLE`
/// rather than mis-order a string.
///
/// Idempotent: a spelling that already carries a `COLLATE` is returned untouched, so
/// re-rendering a column cannot stack two clauses.
pub fn mysql_pin_collation(rendered: &str, case_sensitive: Option<bool>) -> String {
    let trimmed = rendered.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !mysql_spelling_takes_collation(trimmed) || lower.contains(" collate ") {
        return trimmed.to_string();
    }
    format!("{trimmed} {}", mysql_collation_clause(case_sensitive))
}

/// Whether a RENDERED MySQL type spelling is a CHARACTER type - one whose comparison,
/// sorting and uniqueness all run under a collation.
///
/// The spelling-level sibling of [`mysql_type_takes_collation`], including
/// `ENUM(...)`. MySQL stores an enum as an index into its
/// member list but compares and LOOKS UP members as strings, so an uncollated `ENUM`
/// silently accepts `'ACTIVE'` for a declared `'active'` - see
/// `mysql_pin_enum_collation`, which measured it. `SET(...)` is the same shape and
/// is named here for the same reason, though nothing in the engine emits one today.
///
/// Deliberately NOT here: `JSON` (MySQL refuses a collation on it outright), the BLOB
/// family, the spatial family, and every numeric and temporal type.
pub fn mysql_spelling_takes_collation(rendered: &str) -> bool {
    let u = rendered.trim().to_ascii_uppercase();
    mysql_type_takes_collation(rendered) || u.starts_with("ENUM(") || u.starts_with("SET(")
}

/// Whether a MySQL column type spelling is a character type that carries a
/// collation (the `VARCHAR`/`CHAR`/`TEXT` family). Numeric, temporal, JSON and BLOB
/// types do not take a general string collation here.
///
/// `ENUM` is a character type and DOES pin a collation, but it is not listed here and
/// never can be: this predicate is fed `mysql_base_column_type`, which only ever
/// sees the PostgreSQL-mapped `field_data_type` spelling, and an enum column arrives
/// there as `text`. The renderer's spelling-level pass pins `ENUM` directly.
pub fn mysql_type_takes_collation(base: &str) -> bool {
    let u = base.trim().to_ascii_uppercase();
    u.starts_with("VARCHAR")
        || u.starts_with("CHAR(")
        || u == "CHAR"
        || u.starts_with("TEXT")
        || u.starts_with("TINYTEXT")
        || u.starts_with("MEDIUMTEXT")
        || u.starts_with("LONGTEXT")
}
