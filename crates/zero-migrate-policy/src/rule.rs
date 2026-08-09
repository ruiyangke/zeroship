//! The scoped rule model (II.2.2, II.4.2). A policy is a *list of scoped rules*,
//! not a flat map: every [`Rule`] pairs a [`Scope`] with one of four [`RuleKind`]s.
//! `Grant`/`Require` reference a registry knob by key + carry a value; `Inject`/
//! `Validate` carry their own declarative content payload.
//!
//! These types are the resolved (post-normalization) IN-MEMORY model. The
//! document loader (`crate::document`) parses the wire form, normalizes scope
//! patterns (II.2.7), and validates against the registry before producing them.

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
    /// Capability permission — references a `Grant`-polarity knob. Composes DOWN.
    Grant { key: KnobKey, value: KnobValue },
    /// Obligation — references a `Require`-polarity knob. Composes UP (un-droppable).
    Require { key: KnobKey, value: KnobValue },
    /// Content rule: columns/indexes/PK to add to matching `createTable` ops.
    /// Composes UP (obligation polarity) — a charter injection is un-droppable.
    Inject { spec: InjectSpec },
    /// Content rule: a structural predicate a matching table/op must satisfy.
    Validate { pred: ValidatePredicate },
}

impl RuleKind {
    /// The knob key a `Grant`/`Require` rule references, if any (`Inject`/`Validate`
    /// carry no key — they are content, not knob-valued).
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
    /// Root-charter-only: when true the composer enforces creatable ⊑ inject
    /// (II.2.6a). `mandatory = true` on a NON-root layer is a hard load error
    /// (`MandatoryInjectOnNonRootLayer`).
    pub mandatory: bool,
}

/// One injected column: enough to drive the resolver + the II.2.6b conformance
/// check (name/type/nullable/default). The `ty` is an opaque type token.
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

/// The FIXED set of structural predicates a matching table/op must satisfy
/// post-injection (II.4.2). No open expression language — a blunt security surface.
/// All name-comparisons evaluate over NORMALIZED names (II.2.7); the loader folds
/// the literals here at parse time.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ValidatePredicate {
    /// The table must carry a primary key (satisfiable by an injected PK).
    HasPrimaryKey,
    /// Column names must match every `require` glob and no `forbid` glob.
    ColumnNamePattern {
        require: Vec<NameGlob>,
        forbid: Vec<NameGlob>,
    },
    /// These (normalized) column names must NOT appear.
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
    /// (II.2.6c — journal-lookalike defense). Patterns are full schema-qualified
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
