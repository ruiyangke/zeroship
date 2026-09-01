//! The scoped rule model (II.2.2, II.4.2). A policy is a *list of scoped rules*,
//! not a flat map: every [`Rule`] pairs a [`Scope`] with one of four [`RuleKind`]s.
//! `Grant`/`Require` reference a registry knob by key + carry a value; `Inject`/
//! `Validate` carry their own declarative content payload.
//!
//! These types are the resolved (post-normalization) IN-MEMORY model. The
//! document loader (`crate::document`) parses the wire form, normalizes scope
//! patterns (II.2.7), and validates against the registry before producing them.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::knob::{KnobKey, KnobValue};
use crate::scope::{Pattern, Scope, SegGlob};

/// A single scoped policy rule.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rule {
    /// Which schema/table objects this rule addresses (already
    /// default-scope-met + normalized by the loader, except Global-key rules whose
    /// scope is the legality marker `All`).
    pub scope: Scope,
    /// The rule's kind + payload.
    pub kind: RuleKind,
}

/// One of the four scoped rule kinds (II.2.2).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RuleKind {
    /// Capability permission - references a `Grant`-polarity knob. Composes DOWN.
    Grant { key: KnobKey, value: KnobValue },
    /// Obligation - references a `Require`-polarity knob. Composes UP (un-droppable).
    Require { key: KnobKey, value: KnobValue },
    /// Content rule: columns/indexes/PK to add to matching `createTable` ops.
    /// Composes UP (obligation polarity) - a charter injection is un-droppable.
    Inject { spec: InjectSpec },
    /// Content rule: a declared structural predicate. Nothing evaluates one against a
    /// table - see [`ValidatePredicate`] for what the class does and does not do.
    /// Composes UP (a layer can add a predicate, never drop one).
    Validate { pred: ValidatePredicate },
}

impl RuleKind {
    /// The knob key a `Grant`/`Require` rule references, if any (`Inject`/`Validate`
    /// carry no key - they are content, not knob-valued).
    #[must_use]
    pub fn key(&self) -> Option<&KnobKey> {
        match self {
            RuleKind::Grant { key, .. } | RuleKind::Require { key, .. } => Some(key),
            RuleKind::Inject { .. } | RuleKind::Validate { .. } => None,
        }
    }
}

/// Static, declarative injection content (II.4.2). Enough to drive the future
/// resolver and the II.2.6b conformance check. Column/index *types* are carried as
/// opaque strings here (the leaf crate has no SQL-type dep); the resolver maps them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InjectSpec {
    /// Columns to add to a matching created table, in document order.
    pub columns: Vec<InjectColumn>,
    /// Indexes to add.
    pub indexes: Vec<InjectIndex>,
    /// When `Some`, pins the table's primary key to exactly these columns.
    pub primary_key: Option<Vec<String>>,
    /// How an author-declared PK interacts with a pinned PK (II.4.3).
    pub author_primary_key: AuthorPkPolicy,
    /// Root-charter-only: when true the composer enforces that the creatable scope
    /// is contained in the inject
    /// (II.2.6a). `mandatory = true` on a NON-root layer is a hard load error
    /// (`MandatoryInjectOnNonRootLayer`).
    pub mandatory: bool,
}

/// One injected column: enough to drive the resolver + the II.2.6b conformance
/// check (name/type/nullable/default/assign/collation). The `ty` is an opaque type
/// token.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InjectColumn {
    /// The column's (normalized) name.
    pub name: String,
    /// The column's SQL type as an opaque token (e.g. `timestamptz`).
    pub ty: String,
    /// Whether the column is nullable.
    pub nullable: bool,
    /// An optional default expression (opaque token), if any.
    pub default: Option<String>,
    /// Platform assignment policy for this column, when present.
    ///
    /// The generator is already parsed into a closed invocation. Consumers never
    /// re-parse the charter spelling, and the DDL default remains an independent
    /// slot rather than being derived from this value.
    pub assign: Option<Assignment>,
    /// An optional collation INTENT for the column.
    ///
    /// Unlike `ty` this is NOT an opaque token, and the difference is deliberate.
    /// `ty` is opaque because this leaf crate has no SQL-type dependency to check
    /// it against; a collation is not a type, it is a comparison rule with a fixed
    /// vocabulary, and one that lands in emitted DDL. A closed enum lets the LOADER
    /// refuse an unknown spelling at charter-load time, which is where an operator
    /// can still fix it, instead of at render time on a deploy.
    pub collation: Option<InjectCollation>,
}

/// A platform-computed column value: which generator runs, and on which write event.
///
/// This is distinct from a DDL `default`. An assignment is non-overridable policy;
/// a default is a database fallback whose caller-supplied value wins.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    /// The closed generator invocation.
    pub by: AssignmentGenerator,
    /// The event that activates the generator.
    pub on: AssignmentEvent,
}

/// The closed assignment-event vocabulary.
///
/// Restore is deliberately absent: restore is the inverse of delete and clears
/// fields assigned on [`Self::Delete`].
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentEvent {
    /// Assign while inserting a row.
    Insert,
    /// Assign on every write, including insert/update/delete paths.
    Write,
    /// Assign while soft-deleting a row.
    Delete,
}

/// A parsed invocation of one of the platform's closed assignment generators.
///
/// The charter spells this as a string (`now`, `typedId`, `increment(1)`), but the
/// string exists only at the serialization boundary. In memory, arguments are typed
/// and no consumer can reinterpret the invocation differently.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum AssignmentGenerator {
    /// Use the current timestamp.
    Now,
    /// Mint a typed id. Its per-collection prefix is resolved separately and is not
    /// charter data.
    TypedId,
    /// Use the authenticated actor, or the generator's anonymous-write result.
    Actor,
    /// Increment the existing integer by this charter-level constant.
    Increment(i64),
    /// Let the column's own DDL identity assign the value; the runtime emits nothing.
    Identity,
}

impl JsonSchema for AssignmentGenerator {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AssignmentGenerator".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // The wire spelling remains compact, but it is not an arbitrary string.
        // Keep the fixed invocations closed and constrain the one parameterized
        // invocation to a signed decimal integer. Deserialize performs the final
        // i64 range check.
        schemars::json_schema!({
            "description": "A closed platform assignment-generator invocation.",
            "oneOf": [
                { "const": "now" },
                { "const": "typedId" },
                { "const": "actor" },
                { "const": "identity" },
                {
                    "type": "string",
                    "pattern": "^increment\\(-?(0|[1-9][0-9]*)\\)$"
                }
            ]
        })
    }
}

impl fmt::Display for AssignmentGenerator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Now => f.write_str("now"),
            Self::TypedId => f.write_str("typedId"),
            Self::Actor => f.write_str("actor"),
            Self::Increment(amount) => write!(f, "increment({amount})"),
            Self::Identity => f.write_str("identity"),
        }
    }
}

impl FromStr for AssignmentGenerator {
    type Err = AssignmentGeneratorParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let invocation = raw.trim();
        match invocation {
            "now" => Ok(Self::Now),
            "typedId" => Ok(Self::TypedId),
            "actor" => Ok(Self::Actor),
            "identity" => Ok(Self::Identity),
            _ => {
                let Some(argument) = invocation
                    .strip_prefix("increment(")
                    .and_then(|rest| rest.strip_suffix(')'))
                else {
                    return Err(AssignmentGeneratorParseError::UnknownInvocation {
                        invocation: raw.to_string(),
                    });
                };
                let amount = argument.trim().parse::<i64>().map_err(|_| {
                    AssignmentGeneratorParseError::InvalidIncrementArgument {
                        argument: argument.to_string(),
                    }
                })?;
                Ok(Self::Increment(amount))
            }
        }
    }
}

impl Serialize for AssignmentGenerator {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for AssignmentGenerator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// A malformed assignment-generator invocation at the policy boundary.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum AssignmentGeneratorParseError {
    /// The generator name or invocation shape is outside the closed vocabulary.
    #[error("unknown assignment generator invocation {invocation:?}")]
    UnknownInvocation { invocation: String },
    /// `increment(...)` did not carry exactly one signed integer constant.
    #[error("increment argument must be a signed integer constant, got {argument:?}")]
    InvalidIncrementArgument { argument: String },
}

/// The CLOSED collation-intent vocabulary an `[[inject]]` column may pin.
///
/// The resolver maps this onto the engine's per-column collation facet, which each
/// dialect spells its own way. A charter is ONE document that has to be sound on
/// PostgreSQL, MySQL and SQLite at once, so it names the intent rather than a
/// collation NAME - `C` would be meaningless to two of the three.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InjectCollation {
    /// Compare and order the column by its stored bytes, so a value whose byte
    /// order is its semantic order (an id minted from a monotonic clock) sorts the
    /// way it was minted rather than the way a natural-language locale would.
    Bytewise,
}

/// One injected index: a name + the columns it covers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InjectIndex {
    /// The index's (normalized) name.
    pub name: String,
    /// The columns the index covers, in order.
    pub columns: Vec<String>,
}

/// How an author-declared primary key interacts with a pinned injected PK (II.4.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthorPkPolicy {
    /// The author may declare their own PK (when the inject rule does not pin one).
    Allow,
    /// An author-declared PK (other than the folded `id` PK) is rejected - but only
    /// when the rule pins a `primary_key`, per the scoping this type states above.
    /// An unpinned rule has no key to reject the author's in favour of, so it never
    /// rejects one; the loader refuses that combination rather than accept a
    /// restriction nothing applies.
    Forbid,
}

/// The FIXED set of structural predicates a `[[validate]]` rule can name (II.4.2). No
/// open expression language - a blunt security surface.
///
/// NONE of these is checked against a table. [`ForbiddenColumns`](Self::ForbiddenColumns)
/// is the only variant a document may still declare, and what it constrains is the
/// POLICY rather than a schema: it rejects an `[[inject]]` that contributes a name it
/// forbids. Every other variant is declared, composed and sealed and then read by
/// nothing, so the loader refuses them (`LoadError::ValidatePredicateNotEnforced`) instead of
/// sealing a control the engine does not apply. They stay declared here because the
/// seal encoding and the composer's union-up are defined over the whole set, and
/// because enforcing a predicate against a live table is a separate piece of work with
/// a seam decision owed per variant.
///
/// ONE rule governs every name below: a name-comparison is decided over the II.2.7
/// fold of BOTH sides, never over the stored bytes. WHERE the fold happens differs by
/// variant. `ColumnNamePattern`'s globs are folded by the loader and stored folded - a
/// [`NameGlob`] is match machinery, it has no author-facing text to preserve. Every
/// other name literal here (`ForbiddenColumns::names`, `TypeNullability::column`,
/// `RequireIndex::columns`) is stored VERBATIM as authored and folded at comparison
/// time by `compose::names_match`. Folding those at load instead would rewrite the
/// sealed predicate encoding, which is a policy-identity question and not a matching
/// one.
///
/// So a comparison written as `stored == catalog_name` is a bug: it puts an unfolded
/// literal against a folded catalog name and silently misses on any case or quoting
/// difference. Compare through `names_match`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ValidatePredicate {
    /// The table must carry a primary key (satisfiable by an injected PK).
    HasPrimaryKey,
    /// Column names must match every `require` glob and no `forbid` glob.
    ColumnNamePattern {
        require: Vec<NameGlob>,
        forbid: Vec<NameGlob>,
    },
    /// The one predicate with a consumer, and it is a consistency constraint on the
    /// POLICY. It rejects a contradicting inject: a document that both injects one of
    /// these names and forbids it on an overlapping scope fails to load
    /// (`LoadError::SelfContradictoryInjectValidate`), and so does a composition where
    /// the collision is across layers (`CharterInjectValidateContradiction`,
    /// `DraftValidateContradictsCharterInject`).
    ///
    /// It is NEVER checked against a table being created or altered. A migration that
    /// adds one of these columns is not refused by this rule.
    ///
    /// Stored as authored, matched folded.
    ForbiddenColumns { names: Vec<String> },
    /// A named column's type / nullability constraint.
    TypeNullability {
        column: String,
        ty: Option<String>,
        nullable: Option<bool>,
    },
    /// The table must carry an index over exactly these columns.
    RequireIndex { columns: Vec<String> },
    /// The created/renamed table's NORMALIZED name must NOT match any pattern
    /// (II.2.6c - journal-lookalike defense). Patterns are full schema-qualified
    /// scope [`Pattern`]s (`*.journal`, `public.schema_migrations`), not bare
    /// column-name globs.
    TableNameForbidden { patterns: Vec<Pattern> },
}

/// A single-identifier glob used inside validate predicates (column/table names).
/// Kept distinct from `scope::Pattern` (which is a two-segment schema.table glob):
/// a predicate glob is one folded segment. Stored as the post-normalization bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NameGlob {
    /// The folded glob text (a single `*` is a wildcard unless it was quoted).
    pub glob: SegGlob,
}
