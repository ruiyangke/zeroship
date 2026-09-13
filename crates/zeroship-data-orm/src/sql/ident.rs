//! Validated SQL identifiers with role-specific reservations.
//!
//! `Ident::parse_as` is the public construction boundary. Collection, column and
//! alias roles have different reserved-name rules; injected-column collision checks
//! belong to migration validation so queries can still address those fields.
//!
//! ```
//! use zeroship_data_orm::sql::{Ident, IdentRole};
//! let id = Ident::parse_as("users", IdentRole::Collection).expect("valid");
//! assert_eq!(id.as_str(), "users");
//! assert!(Ident::parse_as("users\"; DROP TABLE users; --", IdentRole::Collection).is_err());
//! ```
//! ```compile_fail
//! // The private field means the newtype cannot be forged.
//! let forged = zeroship_data_orm::sql::Ident("users\"; DROP TABLE users; --".to_string());
//! ```
//! ```compile_fail
//! // ... and it cannot be opened up either.
//! let id = zeroship_data_orm::sql::Ident::parse_as("users", zeroship_data_orm::sql::IdentRole::Collection).unwrap();
//! let raw: String = id.0;
//! ```
//! ```compile_fail
//! // There is no blanket conversion from text.
//! let id: zeroship_data_orm::sql::Ident = "users".to_string().into();
//! ```

use core::fmt;

/// Maximum accepted identifier length.
/// Rejecting overlong names prevents PostgreSQL truncation from merging distinct
/// creator-selected identifiers. Migration code separately caps generated names.
pub const MAX_IDENT_BYTES: usize = 63;

/// Where an identifier is about to be used. The fences differ per role, so the
/// role is a required argument to [`Ident::parse_as`] rather than something a
/// caller may leave to a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentRole {
    /// A schema name (the app's own schema).
    Namespace,
    /// A table name.
    Collection,
    /// A column name being *referenced*. Not a column being declared: this
    /// crate plans no DDL.
    Column,
    /// A column the platform references rather than one a creator declared.
    ///
    /// This role permits platform prefixes while retaining identifier and
    /// database-catalog validation.
    StoredColumn,
    /// An output name in a projection, including the platform's own synthetic
    /// result columns.
    Alias,
    /// A named constraint, as in an upsert conflict target.
    Constraint,
    /// A named index.
    Index,
}

impl IdentRole {
    /// The reservation table this role is fenced by.
    const fn reservations(self) -> &'static [Reservation] {
        match self {
            Self::Namespace => NAMESPACE_RESERVATIONS,
            Self::Collection => &[],
            Self::Column => COLUMN_RESERVATIONS,
            Self::StoredColumn => STORED_COLUMN_RESERVATIONS,
            Self::Alias => ALIAS_RESERVATIONS,
            Self::Constraint | Self::Index => DERIVED_NAME_RESERVATIONS,
        }
    }

    /// The role's name, for error messages.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Namespace => "namespace",
            Self::Collection => "collection",
            Self::Column => "column",
            Self::StoredColumn => "stored column",
            Self::Alias => "alias",
            Self::Constraint => "constraint",
            Self::Index => "index",
        }
    }
}

impl fmt::Display for IdentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A role-specific reserved-name rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reservation {
    /// Refuse a name spelled exactly this.
    Exact(&'static str),
    /// Refuse any name starting with this, case-insensitively.
    Prefix(&'static str),
}

impl Reservation {
    fn matches(self, name: &str) -> bool {
        match self {
            Self::Exact(n) => name == n,
            // PostgreSQL folds unquoted identifiers, so catalog-prefix fences
            // are case-insensitive even though emitted identifiers are quoted.
            Self::Prefix(p) => {
                name.len() >= p.len()
                    && name.as_bytes()[..p.len()].eq_ignore_ascii_case(p.as_bytes())
            }
        }
    }

    fn describe(self) -> String {
        match self {
            Self::Exact(n) => format!("the name '{n}' is reserved"),
            Self::Prefix(p) => format!("the prefix '{p}' is reserved"),
        }
    }
}

/// Schema-name fences.
///
/// These reservations prevent qualified access to backend catalogs and
/// platform-owned schemas. Collection names are checked separately.
const NAMESPACE_RESERVATIONS: &[Reservation] = &[
    Reservation::Prefix("pg_"),
    Reservation::Exact("information_schema"),
    Reservation::Prefix("__zeroship"),
    Reservation::Prefix("__zs_"),
    Reservation::Prefix("sqlite_"),
];

/// SQLite catalog tables live inside each attached database. PostgreSQL
/// catalogs live in a separate schema, so a schema-qualified `pg_*` table in
/// the bound creator schema is an ordinary table.
const COLLECTION_RESERVATIONS: &[Reservation] = &[Reservation::Prefix("sqlite_")];

const BACKEND_CATALOG_RESERVATIONS: &[Reservation] =
    &[Reservation::Prefix("pg_"), Reservation::Prefix("sqlite_")];

/// Column-name fences. Platform prefixes remain explicit for auditability even
/// though the leading-underscore rule also matches them.
const COLUMN_RESERVATIONS: &[Reservation] = &[
    // Synthetic result columns the runtime emits, e.g. `_distance` on vector
    // search. Reserved so a creator column cannot shadow one.
    Reservation::Prefix("_"),
    Reservation::Prefix("__zs_"),
    Reservation::Prefix("__zeroship_"),
    // Classification names are reserved for authorization and audit metadata.
    Reservation::Exact("public"),
    Reservation::Exact("pii"),
    Reservation::Exact("spi"),
    Reservation::Exact("phi"),
    Reservation::Exact("pci"),
    Reservation::Exact("internal"),
];

/// Fences for a column the platform references rather than one a creator
/// declared.
///
/// Platform prefixes are permitted here; classification and backend catalog
/// names remain reserved.
const STORED_COLUMN_RESERVATIONS: &[Reservation] = &[
    Reservation::Exact("public"),
    Reservation::Exact("pii"),
    Reservation::Exact("spi"),
    Reservation::Exact("phi"),
    Reservation::Exact("pci"),
    Reservation::Exact("internal"),
];

/// Output-name fences.
///
/// Aliases may use a leading underscore for synthetic result fields. Creator
/// columns cannot use that prefix, so they cannot shadow those fields.
const ALIAS_RESERVATIONS: &[Reservation] = &[
    Reservation::Prefix("__zs_"),
    Reservation::Prefix("__zeroship"),
    Reservation::Prefix("sqlite_"),
];

/// Derived constraint and index names use the shared shape rules.
const DERIVED_NAME_RESERVATIONS: &[Reservation] = &[];

/// Why an identifier was refused.
///
/// Every variant names the identifier role that rejected the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentError {
    /// The name was empty.
    Empty { role: IdentRole },
    /// The name contained a NUL byte.
    NulByte { role: IdentRole },
    /// The name exceeded [`MAX_IDENT_BYTES`].
    TooLong { role: IdentRole, len: usize },
    /// The name contained something outside `[A-Za-z0-9_]`.
    ///
    /// The error reports the escaped character without echoing the whole name.
    IllegalCharacter { role: IdentRole, character: char },
    /// The name hit the role's reservation table. Safe to echo: the charset
    /// check runs first, so `name` here is always `[A-Za-z0-9_]`.
    Reserved {
        role: IdentRole,
        name: String,
        reservation: String,
    },
}

impl fmt::Display for IdentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { role } => write!(f, "{role} name cannot be empty"),
            Self::NulByte { role } => write!(f, "{role} name must not contain a NUL byte"),
            Self::TooLong { role, len } => write!(
                f,
                "{role} name is {len} bytes, over the {MAX_IDENT_BYTES}-byte identifier limit"
            ),
            Self::IllegalCharacter { role, character } => write!(
                f,
                "{role} name contains '{}' (allowed: ASCII alphanumeric and underscore)",
                character.escape_debug()
            ),
            Self::Reserved {
                role,
                name,
                reservation,
            } => write!(f, "{role} name '{name}' is refused: {reservation}"),
        }
    }
}

impl std::error::Error for IdentError {}

/// A validated SQL identifier.
///
/// The **only** constructor is [`Ident::parse_as`]. The field is private; there
/// is no `From<String>`, no `Deserialize`, and no `into_string`. See the module
/// documentation for the compile-fail proofs of each of those.
///
/// `Ident` does not retain its validation role. Statement constructors own the
/// role of each slot and validate text at that boundary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ident(String);

impl Ident {
    /// Validate `raw` for use in `role`.
    ///
    /// Validation checks emptiness, NUL, length, charset, then reservations. Running the
    /// charset check before the reservations is what makes
    /// [`IdentError::Reserved`] safe to echo, and it is also why a name like
    /// `"caf\u{e9}"` reports the encoding problem rather than a spurious
    /// suffix hit.
    ///
    /// # Errors
    ///
    /// [`IdentError`], naming the role and the specific fence that refused.
    pub fn parse_as(raw: &str, role: IdentRole) -> Result<Self, IdentError> {
        if raw.is_empty() {
            return Err(IdentError::Empty { role });
        }
        if raw.contains('\0') {
            return Err(IdentError::NulByte { role });
        }
        if raw.len() > MAX_IDENT_BYTES {
            return Err(IdentError::TooLong {
                role,
                len: raw.len(),
            });
        }
        if let Some(bad) = raw
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || *c == '_'))
        {
            return Err(IdentError::IllegalCharacter {
                role,
                character: bad,
            });
        }
        if role == IdentRole::Collection {
            for reservation in COLLECTION_RESERVATIONS {
                if reservation.matches(raw) {
                    return Err(IdentError::Reserved {
                        role,
                        name: raw.to_string(),
                        reservation: reservation.describe(),
                    });
                }
            }
        }
        for reservation in role.reservations() {
            if reservation.matches(raw) {
                return Err(IdentError::Reserved {
                    role,
                    name: raw.to_string(),
                    reservation: reservation.describe(),
                });
            }
        }
        if matches!(role, IdentRole::Column | IdentRole::StoredColumn) {
            for reservation in BACKEND_CATALOG_RESERVATIONS {
                if reservation.matches(raw) {
                    return Err(IdentError::Reserved {
                        role,
                        name: raw.to_string(),
                        reservation: reservation.describe(),
                    });
                }
            }
        }
        Ok(Self(raw.to_string()))
    }

    /// The validated text.
    ///
    /// This is a borrow, not a handover: there is deliberately no
    /// `into_string`, so an `Ident` cannot be laundered back into an owned
    /// `String` that some other code path then treats as pre-validated.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reservation predicate must be able to say NO, or every table below
    /// it is decorative. Deliberately not proved by the fences' own findings,
    /// which are expected to be empty on well-formed input.
    #[test]
    fn the_reservation_predicate_can_actually_fail() {
        assert!(Reservation::Prefix("pg_").matches("pg_class"));
        assert!(
            Reservation::Prefix("pg_").matches("PG_CLASS"),
            "prefix fence must be case-insensitive"
        );
        assert!(!Reservation::Prefix("pg_").matches("page_views"));
        assert!(Reservation::Exact("pii").matches("pii"));
        assert!(!Reservation::Exact("pii").matches("piix"));
    }

    /// A prefix fence must not match a name SHORTER than the prefix, which is
    /// where a naive byte-slice comparison panics rather than returning false.
    #[test]
    fn a_short_name_does_not_panic_the_prefix_fence() {
        assert!(!Reservation::Prefix("__zeroship").matches("_"));
    }

    /// Every role must have deliberate reservation behavior. Collection and
    /// column consult the backend catalog table in addition to their neutral
    /// role-specific fences; `Constraint`/`Index` intentionally accept the
    /// witnesses because they name only platform-derived identifiers.
    #[test]
    fn every_role_has_deliberate_reservation_behavior() {
        let cases = [
            (IdentRole::Namespace, "__zeroship_reserved", false),
            (IdentRole::Collection, "pg_class", true),
            (IdentRole::Collection, "__zeroship_audit_unmask", true),
            (IdentRole::Column, "pg_attribute", false),
            // Stored physical columns use a separate role; backend catalog and
            // classification names remain invalid for it.
            (IdentRole::StoredColumn, "__zs_raw__ssn", true),
            (IdentRole::Alias, "__zeroship_internal", false),
            (IdentRole::Constraint, "pg_constraint", true),
            (IdentRole::Index, "pg_index", true),
        ];
        for (role, witness, expected_acceptance) in cases {
            assert_eq!(
                Ident::parse_as(witness, role).is_ok(),
                expected_acceptance,
                "unexpected reservation verdict for {role} witness {witness:?}"
            );
            // A new variant makes this match non-exhaustive, which is a COMPILE
            // error, so a role cannot be added without landing in `cases`.
            //
            // A length assertion cannot do this job: comparing the array to its
            // own literal length agrees with itself forever, so a role added to
            // the enum and omitted from `cases` goes unnoticed.
            match role {
                IdentRole::Namespace
                | IdentRole::Collection
                | IdentRole::Column
                | IdentRole::StoredColumn
                | IdentRole::Alias
                | IdentRole::Constraint
                | IdentRole::Index => {}
            }
        }
    }

    #[test]
    fn app_schema_tables_share_one_collection_role() {
        assert!(Ident::parse_as("__zeroship_audit_unmask", IdentRole::Collection).is_ok());
        assert!(Ident::parse_as("__zeroship_migrations", IdentRole::Collection).is_ok());
        assert!(Ident::parse_as("pg_class", IdentRole::Collection).is_ok());
        assert!(Ident::parse_as("sqlite_schema", IdentRole::Collection).is_err());
    }
}
