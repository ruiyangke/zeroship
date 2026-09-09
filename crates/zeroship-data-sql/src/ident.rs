//! The type that makes SQL text unrepresentable.
//!
//! # The problem this replaces
//!
//! `crates/zeroship-data-sql/src/compile.rs` builds SQL by string concatenation.
//! A caller-supplied name reaches a `format!` and correctness rests on a
//! validator having been called first, on a path that is not the one doing the
//! formatting. The validators are good ones - `validate_collection` and
//! `validate_field_name` both refuse NUL bytes, over-long names, non-ASCII, and
//! a table of reserved shapes - but they are *functions someone remembered to
//! call*, and the type that reaches the renderer afterwards is `&str`, which is
//! also the type of everything they refused.
//!
//! [`Ident`] closes that gap by construction. It has exactly one constructor,
//! [`Ident::parse_as`]. Its field is private, so it cannot be built by struct
//! literal; there is no `From<String>`, no `FromStr`, no `Deserialize` (this
//! crate has no `serde` dependency at all - see `Cargo.toml`), and no
//! `into_string`. A renderer that holds an `Ident` holds something that passed
//! the fence, and nothing else in this crate accepts a bare string in a
//! position where SQL text would land.
//!
//! The control for the three `compile_fail` blocks below. A `compile_fail`
//! doctest passes on ANY compile error, including a typo in a path, so without
//! a companion block that does compile over the same names they would all be
//! satisfied by `zeroship_data_sqln` and prove nothing.
//!
//! ```
//! use zeroship_data_sql::{Ident, IdentRole};
//! let id = Ident::parse_as("users", IdentRole::Collection).expect("valid");
//! assert_eq!(id.as_str(), "users");
//! assert!(Ident::parse_as("users\"; DROP TABLE users; --", IdentRole::Collection).is_err());
//! ```
//!
//! ```compile_fail
//! // The private field means the newtype cannot be forged.
//! let forged = zeroship_data_sql::Ident("users\"; DROP TABLE users; --".to_string());
//! ```
//!
//! ```compile_fail
//! // ... and it cannot be opened up either.
//! let id = zeroship_data_sql::Ident::parse_as("users", zeroship_data_sql::IdentRole::Collection).unwrap();
//! let raw: String = id.0;
//! ```
//!
//! ```compile_fail
//! // There is no blanket conversion from text.
//! let id: zeroship_data_sql::Ident = "users".to_string().into();
//! ```
//!
//! # Why the role is a parameter and not decoration
//!
//! The fences genuinely differ, and SC-3 is emphatic that they are **a pair,
//! not a single guardian**: a *table* name is fenced by `validate_collection`'s
//! prefix list, a *column* name by a second list in `RESERVED_NAMES` (`_`,
//! `__zs_`, `__zeroship_`, the `_masked` sibling suffix, and the six
//! classification names).
//!
//! **`validate_collection` is FORKED.** The data plane and migration engine
//! once reserved different platform prefixes. They now execute matching
//! `PLATFORM_RESERVED_COLLECTION_PREFIXES` slices, and the data-plane suite
//! compares every copy. This crate is the third copy because its zero-dependency
//! boundary forbids importing either validator.
//!
//! Normal declarative migration loading now calls the migration engine's
//! `validate_collection`, so a creator `createTable` is fenced before lowering.
//! Code paths that do not load migration IR must still invoke their own copy;
//! matching slices do nothing by themselves.
//!
//! Two role-specific decisions are deliberate departures from the shapes in
//! `query.rs`, and each is called out on the table that carries it:
//!
//! * [`IdentRole::Alias`] permits a single leading `_`, which
//!   `validate_field_name` does not. See [`ALIAS_RESERVATIONS`].
//! * No role fences the seven platform system-field names. That reservation
//!   fires only at schema-declaration time
//!   (`validate_field_name_for_declaration`), and this crate plans no DDL -
//!   `db.users.find({ id: "..." })` is the canonical query shape and must keep
//!   working.

use core::fmt;

/// The `PostgreSQL` identifier length limit (`NAMEDATALEN - 1`), in bytes.
///
/// `PostgreSQL` does not error on a longer identifier; it silently truncates and
/// emits a NOTICE, so two distinct names can collapse onto one. Refusing here
/// is right for names a creator *chose* - they can shorten them, and the
/// refusal says so. It would be wrong for names the platform *derives*, which
/// is why `zeroship_data_sql::derived_ident::cap_ident_name` caps rather than refuses;
/// this crate never derives a name.
pub const MAX_IDENT_BYTES: usize = 63;

/// Platform-owned collection prefixes mirrored by both query validators.
///
/// Public only so the data-plane suite can enforce exact parity without adding
/// a production dependency to this zero-dependency crate.
///
/// `"__zero_migrate"` is deliberately absent. The engine's journal tables are
/// `__zeroship_schema_*`, and the one live object carrying that token is the
/// SQLite rebuild table, built as `{table}__zero_migrate_rebuild` - a SUFFIX,
/// which a prefix list cannot cover in either direction. Adding it back would
/// fence an empty namespace and still miss the collision it looks like it
/// addresses.
#[doc(hidden)]
pub const PLATFORM_RESERVED_COLLECTION_PREFIXES: &[&str] = &["__zeroship"];

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
    /// A column the PLATFORM references, not one a creator declared: the
    /// physical side of a [`crate::ProjectionSource::Stored`].
    ///
    /// It exists for the same reason [`Self::Alias`] does. The platform's stored
    /// forms are spelled with the very prefixes [`COLUMN_RESERVATIONS`] refuses,
    /// because refusing them is what stops a creator declaring one; the platform
    /// still has to name them. Splitting the role is how that is expressed
    /// without weakening the creator-facing fence.
    ///
    /// This replaced a `pub(crate)` constructor that built the name by
    /// `format!` and skipped `parse_as` entirely, so a stored name went through
    /// no charset check and no catalog fence at all. A role is strictly
    /// stronger: it still refuses `pg_` and `sqlite_`, the classification names,
    /// quote injection and NUL.
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

/// The shape of a reservation. Mirrors `query.rs`'s `ReservedName` so the
/// fences can be compared row by row when the port moves them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reservation {
    /// Refuse a name spelled exactly this.
    Exact(&'static str),
    /// Refuse any name starting with this, case-insensitively.
    Prefix(&'static str),
    /// Refuse any name ending with this, case-sensitively.
    Suffix(&'static str),
}

impl Reservation {
    fn matches(self, name: &str) -> bool {
        match self {
            Self::Exact(n) => name == n,
            // Case-insensitive, as both forks of `validate_collection` are:
            // PostgreSQL folds unquoted identifiers to lower case, so `PG_Foo`
            // and `pg_foo` are the same catalog name and a case-sensitive fence
            // would miss one of them.
            Self::Prefix(p) => {
                name.len() >= p.len()
                    && name.as_bytes()[..p.len()].eq_ignore_ascii_case(p.as_bytes())
            }
            Self::Suffix(s) => name.ends_with(s),
        }
    }

    fn describe(self) -> String {
        match self {
            Self::Exact(n) => format!("the name '{n}' is reserved"),
            Self::Prefix(p) => format!("the prefix '{p}' is reserved"),
            Self::Suffix(s) => format!("the suffix '{s}' is reserved"),
        }
    }
}

/// Schema-name fences.
///
/// `__zeroship` is refused here, and it guards two distinct things. The first is
/// live: `__zeroship_` is the prefix of tables that exist in every app schema
/// today - the migration journal, the unmask audit table, the workflow journal -
/// and a creator-named collection colliding with one of them is a corruption
/// rather than a name clash. The second is a reservation: this crate builds
/// plans the *worker* executes, and the worker executes creator code, so per the
/// platform invariant, state a separate service writes and the worker only reads
/// must not be nameable from a worker-built plan. No such platform-owned schema
/// exists at the time of writing; the fence holds the namespace open for one and
/// protects the live tables meanwhile. Do not narrow it on the grounds that the
/// schema is absent.
const NAMESPACE_RESERVATIONS: &[Reservation] = &[
    Reservation::Prefix("pg_"),
    Reservation::Exact("information_schema"),
    Reservation::Prefix("__zeroship"),
    Reservation::Prefix("__zs_"),
    Reservation::Prefix("sqlite_"),
];

/// Catalog prefixes owned by the backends the runtime can address.
///
/// These are separate from the neutral platform tables. SQLite refuses
/// `sqlite_*` table names outright, while PostgreSQL does not reliably refuse
/// every `pg_*` object. Both prefixes apply to columns because the migration
/// engine fences the union from every registered backend against later
/// retargeting. The behavioral parity suite derives the real shipping set and
/// fails when this zero-dependency runtime copy drifts.
const BACKEND_CATALOG_RESERVATIONS: &[Reservation] =
    &[Reservation::Prefix("pg_"), Reservation::Prefix("sqlite_")];

/// Column-name fences, in `RESERVED_NAMES` order so the error a given name
/// produces is the same one it produces today.
///
/// `Prefix("_")` subsumes `__zs_` and `__zeroship_`; both are kept anyway,
/// because the port SC-3 describes is a *move* of this table and a move that
/// silently drops rows is exactly the failure the "pair, not a single
/// guardian" note warns about.
const COLUMN_RESERVATIONS: &[Reservation] = &[
    // Synthetic result columns the runtime emits, e.g. `_distance` on vector
    // search. Reserved so a creator column cannot shadow one.
    Reservation::Prefix("_"),
    Reservation::Prefix("__zs_"),
    Reservation::Prefix("__zeroship_"),
    // Masked-column sibling suffix (the Path B sibling-column strategy).
    Reservation::Suffix("_masked"),
    // The six default classifications, reserved at column level so a creator
    // schema cannot collide with the taxonomy authorization and audit use.
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
/// This is [`COLUMN_RESERVATIONS`] with the four platform-shape rows removed -
/// `Prefix("_")`, `Prefix("__zs_")`, `Prefix("__zeroship_")` and
/// `Suffix("_masked")` - because those rows exist to stop a CREATOR naming a
/// platform column, and this role is the platform doing exactly that. The
/// classification names stay: nothing the platform stores is called `pii`, and
/// keeping them costs nothing while preserving the taxonomy fence in both roles.
///
/// The backend catalog fences (`pg_`, `sqlite_`) apply to this role too, wired
/// beside [`IdentRole::Column`] in `parse_as`.
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
/// An alias is **allowed** a single leading `_`, which a column is not, and the
/// asymmetry is the reason the role exists. The platform's own synthetic result
/// columns are spelled that way - `_distance` on a vector search - and they are
/// emitted as aliases, never declared as columns. The `_` fence on
/// [`COLUMN_RESERVATIONS`] is what stops a creator column shadowing one; the
/// alias side is the platform's to spell.
///
/// The `_masked` suffix is likewise allowed here, because an internal-exposure
/// projection may legitimately surface a sibling under its own name.
const ALIAS_RESERVATIONS: &[Reservation] = &[
    Reservation::Prefix("__zs_"),
    Reservation::Prefix("__zeroship"),
    Reservation::Prefix("sqlite_"),
];

/// Fences for names the platform derives (constraints, indexes). No creator
/// reservation applies - a creator does not choose these - so only the shared
/// shape rules (charset, length, no NUL) run.
const DERIVED_NAME_RESERVATIONS: &[Reservation] = &[];

/// Why an identifier was refused.
///
/// Every variant names the role, because the same text is legal in one position
/// and refused in another and an error that does not say which is being tested
/// sends the reader to the wrong fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentError {
    /// The name was empty.
    Empty { role: IdentRole },
    /// The name contained a NUL byte. Kept as its own variant rather than
    /// folded into [`IdentError::IllegalCharacter`]: a NUL is how a truncating
    /// C-string consumer is attacked, not a typo.
    NulByte { role: IdentRole },
    /// The name exceeded [`MAX_IDENT_BYTES`].
    TooLong { role: IdentRole, len: usize },
    /// The name contained something outside `[A-Za-z0-9_]`.
    ///
    /// The offending character is reported escaped, and the *name* is not
    /// echoed at all. This is a deliberate departure from
    /// `validate_field_name`, whose message interpolates the raw name and so
    /// can carry control characters or a broken escape into whatever reads the
    /// error.
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
/// `Ident` does **not** remember which role validated it. That follows SC-3's
/// own sketch, and it is a real limitation rather than an oversight: nothing
/// stops a value parsed as an alias being stored in a field that wants a
/// collection. What prevents it in practice is that the plan structs name their
/// slots, so the miscarriage has to be written deliberately. Carrying the role
/// in the type was considered and rejected because it forces a double parse at
/// every stored projection, where the same text is both a column and an alias.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ident(String);

impl Ident {
    /// Validate `raw` for use in `role`.
    ///
    /// Order matters and is the same order `query.rs` uses: emptiness, NUL,
    /// length, charset, and only then the reservation table. Running the
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
            for reservation in BACKEND_CATALOG_RESERVATIONS {
                if reservation.matches(raw) {
                    return Err(IdentError::Reserved {
                        role,
                        name: raw.to_string(),
                        reservation: reservation.describe(),
                    });
                }
            }
            for prefix in PLATFORM_RESERVED_COLLECTION_PREFIXES {
                let reservation = Reservation::Prefix(prefix);
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
        assert!(Reservation::Suffix("_masked").matches("ssn_masked"));
        assert!(!Reservation::Suffix("_masked").matches("masked_ssn"));
        assert!(Reservation::Exact("pii").matches("pii"));
        assert!(!Reservation::Exact("pii").matches("piix"));
    }

    /// A prefix fence must not match a name SHORTER than the prefix, which is
    /// where a naive byte-slice comparison panics rather than returning false.
    #[test]
    fn a_short_name_does_not_panic_the_prefix_fence() {
        assert!(!Reservation::Prefix("__zeroship").matches("_"));
        assert!(!Reservation::Suffix("_masked").matches("s"));
    }

    /// Every role must have deliberate reservation behavior. Collection and
    /// column consult the backend catalog table in addition to their neutral
    /// role-specific fences; `Constraint`/`Index` intentionally accept the
    /// witnesses because they name only platform-derived identifiers.
    #[test]
    fn every_role_has_deliberate_reservation_behavior() {
        let cases = [
            (IdentRole::Namespace, "__zeroship_reserved", false),
            (IdentRole::Collection, "pg_class", false),
            (IdentRole::Column, "pg_attribute", false),
            // Accepted, and that IS the deliberate behaviour: this role exists
            // so the platform can name its own stored columns. The refusal half
            // is `the_stored_prefix_is_reserved_against_creators_and_nameable_
            // by_the_platform` in `tests/ident_refusals.rs`, which pins that
            // `pg_`, `sqlite_` and the classification names still fail here.
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
        println!("ruled on {} roles", cases.len());
    }
}
