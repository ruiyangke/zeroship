//! The type that makes SQL text unrepresentable.
//!
//! # The problem this replaces
//!
//! `crates/zeroship-schema/src/query.rs` builds SQL by string concatenation.
//! A caller-supplied name reaches a `format!` and correctness rests on a
//! validator having been called first, on a path that is not the one doing the
//! formatting. The validators are good ones - `validate_collection`
//! (`query.rs:626-664`) and `validate_field_name` (`query.rs:793-849`) both
//! refuse NUL bytes, over-long names, non-ASCII, and a table of reserved
//! shapes - but they are *functions someone remembered to call*, and the type
//! that reaches the renderer afterwards is `&str`, which is also the type of
//! everything they refused.
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
//! satisfied by `zeroship_data_plann` and prove nothing.
//!
//! ```
//! use zeroship_data_plan::{Ident, IdentRole};
//! let id = Ident::parse_as("users", IdentRole::Collection).expect("valid");
//! assert_eq!(id.as_str(), "users");
//! assert!(Ident::parse_as("users\"; DROP TABLE users; --", IdentRole::Collection).is_err());
//! ```
//!
//! ```compile_fail
//! // The private field means the newtype cannot be forged.
//! let forged = zeroship_data_plan::Ident("users\"; DROP TABLE users; --".to_string());
//! ```
//!
//! ```compile_fail
//! // ... and it cannot be opened up either.
//! let id = zeroship_data_plan::Ident::parse_as("users", zeroship_data_plan::IdentRole::Collection).unwrap();
//! let raw: String = id.0;
//! ```
//!
//! ```compile_fail
//! // There is no blanket conversion from text.
//! let id: zeroship_data_plan::Ident = "users".to_string().into();
//! ```
//!
//! # Why the role is a parameter and not decoration
//!
//! The fences genuinely differ, and SC-3 is emphatic that they are **a pair,
//! not a single guardian**: a *table* name is fenced by `validate_collection`'s
//! prefix list, a *column* name by a second list in `RESERVED_NAMES` (`_`,
//! `__zs_`, `__zeroship_`, `sqlite_`, the `_masked` sibling suffix, and the six
//! classification names).
//!
//! **`validate_collection` is FORKED, and the two forks reserve different
//! prefixes.** The data plane's
//! (`zeroship-schema/src/query.rs`) fences `__zeroship`; the migration
//! authoring path's (`zeroship-migrate-core/src/schema/query.rs`) fences
//! `__zero_migrate` and does NOT fence `__zeroship`. A creator's `createTable`
//! passes through the authoring fork, so a `__zeroship`-prefixed table name is
//! not refused there. Do not restate either fork's list as "the" reserved set:
//! an earlier version of this comment did, citing line numbers that no longer
//! exist, and that is a claim that reads as protection.
//!
//! Moving one without the other is the dangerous half of the move, because the survivor
//! makes the namespace look defended. Both are re-stated below, together, each
//! with its own arm in `tests/ident_refusals.rs`.
//!
//! Three role-specific decisions are deliberate departures from the shapes in
//! `query.rs`, and each is called out on the table that carries it:
//!
//! * [`IdentRole::Collection`] fences `sqlite_`, which `validate_collection`
//!   does **not**. See [`COLLECTION_RESERVATIONS`].
//! * [`IdentRole::Alias`] permits a single leading `_`, which
//!   `validate_field_name` does not. See [`ALIAS_RESERVATIONS`].
//! * No role fences the seven platform system-field names. That reservation
//!   fires only at schema-declaration time
//!   (`validate_field_name_for_declaration`, `query.rs:867-878`), and this
//!   crate plans no DDL - `db.users.find({ id: "..." })` is the canonical query
//!   shape and must keep working.

use core::fmt;

/// The `PostgreSQL` identifier length limit (`NAMEDATALEN - 1`), in bytes.
///
/// `PostgreSQL` does not error on a longer identifier; it silently truncates and
/// emits a NOTICE, so two distinct names can collapse onto one. Refusing here
/// is right for names a creator *chose* - they can shorten them, and the
/// refusal says so. It would be wrong for names the platform *derives*, which
/// is why `zeroship_schema::ident::cap_ident_name` caps rather than refuses;
/// this crate never derives a name.
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
            Self::Collection => COLLECTION_RESERVATIONS,
            Self::Column => COLUMN_RESERVATIONS,
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

/// The shape of a reservation. Mirrors `query.rs`'s `ReservedName`
/// (`query.rs:694-701`) so the fences can be compared row by row when the port
/// moves them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reservation {
    /// Refuse a name spelled exactly this.
    Exact(&'static str),
    /// Refuse any name starting with this, case-insensitively.
    Prefix(&'static str),
    /// Refuse any name ending with this.
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
                name.len() >= p.len() && name.as_bytes()[..p.len()].eq_ignore_ascii_case(p.as_bytes())
            }
            Self::Suffix(s) => name.len() >= s.len() && name.as_bytes()[name.len() - s.len()..].eq_ignore_ascii_case(s.as_bytes()),
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
/// `__zeroship` is refused here even though the platform's own system schema
/// (`__zeroship_admin`) is spelled that way, and refusing it is the point: this
/// crate builds plans the *worker* executes, and the worker executes creator
/// code. Per the platform invariant, state a separate service writes and the
/// worker only reads must not be nameable from a worker-built plan.
const NAMESPACE_RESERVATIONS: &[Reservation] = &[
    Reservation::Prefix("pg_"),
    Reservation::Exact("information_schema"),
    Reservation::Prefix("__zeroship"),
    Reservation::Prefix("__zs_"),
    Reservation::Prefix("sqlite_"),
];

/// Table-name fences. `pg_` and `__zeroship` are `validate_collection`'s two
/// reserved prefixes verbatim (`query.rs:645-653`).
///
/// `sqlite_` is **added**, and its absence from `validate_collection` looks
/// like a straightforward inversion rather than a decision: `SQLite` reserves the
/// `sqlite_` prefix for *table* names specifically, yet in `query.rs` it is
/// fenced only on columns (`RESERVED_NAMES`, `query.rs:738-766`) - the one
/// place `SQLite` does not reserve it. The dev tier is `SQLite`, so the fence
/// belongs on both roles here.
const COLLECTION_RESERVATIONS: &[Reservation] = &[
    Reservation::Prefix("pg_"),
    Reservation::Prefix("__zeroship"),
    Reservation::Prefix("__zs_"),
    Reservation::Prefix("sqlite_"),
];

/// Column-name fences, in `RESERVED_NAMES` order (`query.rs:738-766`) so the
/// error a given name produces is the same one it produces today.
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
    Reservation::Prefix("sqlite_"),
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
    /// `validate_field_name`, whose message interpolates the raw name
    /// (`query.rs:814`) and so can carry control characters or a broken
    /// escape into whatever reads the error.
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
/// every masked-sibling projection, where the same text is both a column and an
/// alias.
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
        if let Some(bad) = raw.chars().find(|c| !(c.is_ascii_alphanumeric() || *c == '_')) {
            return Err(IdentError::IllegalCharacter {
                role,
                character: bad,
            });
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

    /// The platform's masked-sibling column for `parent`.
    ///
    /// This exists because the `_masked` suffix is **refused** for a column
    /// (`COLUMN_RESERVATIONS`), which is what stops a creator declaring
    /// `ssn_masked` and shadowing the sibling the platform emits. The platform
    /// still has to name it, so the name is DERIVED at exactly one site rather
    /// than parsed at every call site that needs it - a second site would be a
    /// second place the fence could be argued around.
    ///
    /// `pub(crate)` on purpose: the only legitimate consumer is
    /// `ProjectedField::masked`.
    ///
    /// # Errors
    ///
    /// [`IdentError::TooLong`] when the derived name overflows
    /// [`MAX_IDENT_BYTES`]. This crate deliberately does **not** reproduce the
    /// hash-and-truncate cap that `zeroship_schema::ident::cap_ident_name`
    /// applies to derived names: a second implementation of that cap is how the
    /// runtime and the migration engine once disagreed about what an index was
    /// called, and guessing wrong here would mean projecting a column that does
    /// not exist. Refusing says so.
    pub(crate) fn masked_sibling_of(parent: &Self) -> Result<Self, IdentError> {
        let derived = format!("{}{MASKED_SUFFIX}", parent.0);
        if derived.len() > MAX_IDENT_BYTES {
            return Err(IdentError::TooLong {
                role: IdentRole::Column,
                len: derived.len(),
            });
        }
        Ok(Self(derived))
    }
}

/// The suffix the platform's sibling columns carry. Reserved against creator
/// declaration by [`COLUMN_RESERVATIONS`].
pub const MASKED_SUFFIX: &str = "_masked";

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
        assert!(Reservation::Prefix("pg_").matches("PG_CLASS"), "prefix fence must be case-insensitive");
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

    /// Every role must actually consult a table, or a role added later gets a
    /// silent free pass. `Constraint`/`Index` share the deliberately empty one.
    #[test]
    fn every_role_resolves_a_reservation_table() {
        let roles = [
            IdentRole::Namespace,
            IdentRole::Collection,
            IdentRole::Column,
            IdentRole::Alias,
            IdentRole::Constraint,
            IdentRole::Index,
        ];
        let with_fences = roles
            .iter()
            .filter(|r| !r.reservations().is_empty())
            .count();
        assert_eq!(
            with_fences, 4,
            "four of the six roles carry a creator-facing fence; \
             Constraint and Index name only platform-derived identifiers"
        );
        assert_eq!(roles.len(), 6, "a new role must be given a fence table deliberately");
    }
}
