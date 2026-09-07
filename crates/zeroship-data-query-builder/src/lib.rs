//! `DbPlan`: a runtime database plan in which SQL text is not a representable
//! value.
//!
//! # What this crate is for
//!
//! The runtime's query builder today is string concatenation. A caller-supplied
//! name reaches a `format!`, and correctness rests on a validator having been
//! called somewhere upstream. The validators are good; the *type* that arrives
//! at the renderer is `&str`, which is also the type of everything they
//! refused.
//!
//! This crate replaces that boundary with values that cannot be wrong:
//!
//! * an identifier is an [`Ident`], whose only constructor validates against a
//!   role-specific fence, and which cannot be built from arbitrary text - see
//!   [`ident`] for the compile-fail proofs;
//! * a value is a [`Literal`], which is always bound as a parameter and never
//!   written into the statement;
//! * **nullness is a node**, not a literal and not an operator, so the class of
//!   defect where a null silently became an empty string is unrepresentable
//!   rather than merely repaired;
//! * a plan renders **deterministically**, so two callers who expressed the
//!   same query differently share one prepared statement;
//! * a predicate is **bounded in depth** ([`MAX_PREDICATE_DEPTH`]) as well as in
//!   width. Depth is the bound that matters: width is heap work, height is stack
//!   frames, and Rust has no recursion limit, so an over-deep tree aborts the
//!   worker rather than failing a request. `Predicate` is the only recursive
//!   type here, and the bound is checked at construction, ahead of all four of
//!   its recursive surfaces - `canonical`, the lowering, `mentions_aggregate`,
//!   and the derived `Ord`/`Hash` that `canonical`'s own sort calls.
//!
//! There is no `raw_sql` and no way to add one without changing the shape of
//! every node: nothing in the public surface accepts a `String` in a position
//! that reaches the statement text. `tests/no_sql_text_escape_hatch.rs` keeps
//! it that way.
//!
//! # A leaf, and deliberately so
//!
//! No `v8`, no runtime, no `zeroship-plugin-db`, no `zeroship-schema`, and in
//! fact no dependencies at all. That is what lets the whole grammar be built
//! and tested without an isolate, a database or a network, and it is why the
//! crate exists separately rather than inside the plugin.
//!
//! The absence of `serde` in particular is load-bearing: SC-3's decision 3 says
//! `DbPlan` must not derive `Serialize`, because the moment a plan crosses a
//! process boundary it is a wire format needing versioning and a migration
//! story. Here the derive does not compile.
//!
//! # Scope
//!
//! SC-3 names six plan families - read, relation, write, search, unmask and
//! effects - and this crate builds **three**, read, write and search, plus the
//! shared core underneath all six. That split is the document's own:
//! per-family node spelling is deliberately left to each family's port, because
//! a family that gets its own shape wrong costs that family a revision, whereas
//! a shared expression node fixed wrongly by whoever ports first costs every
//! family after it.
//!
//! The search family is vector and spatial, **and only those two** - SC-3 is
//! explicit that full-text search was deleted and that the IR must not carry a
//! family for a feature that no longer exists. See [`search`], which records
//! the sweep that checked it rather than taking the document's word, and which
//! argues the two decisions the family forces: how a plan expresses a
//! capability only one backend has, and why the ranking key is structural
//! rather than a caller-supplied sort.
//!
//! The write family adds two shapes the read family had no use for and takes
//! care not to add a third. [`write::WriteValue::Null`] makes a written NULL a
//! node rather than a literal, and [`write::Arithmetic`] makes `col = col + 1`
//! expressible - which it must be, because `query.rs:3907` emits
//! `"version" = "version" + 1` on every dispatched update. Both are closed
//! shapes confined to the SET/VALUES position; neither is a general expression
//! node, because a general one would fix the shared grammar for search, unmask
//! and effects too. See [`mod@write`] for the full argument, including what it
//! costs to leave the `jsonb` update operators out.
//!
//! Nothing here emits DDL. The runtime executes none, so `CREATE TABLE`,
//! indexes, constraints and `ALTER` belong to the migration and schema side and
//! are out of scope by construction, not by convention.

pub mod ident;
pub mod literal;
pub mod path;
pub mod plan;
pub mod predicate;
pub mod projection;
pub mod render;
pub mod search;
pub mod write;

pub use ident::{Ident, IdentError, IdentRole, MAX_IDENT_BYTES};
pub use literal::{
    Finite, Finite32, Literal, LiteralError, LiteralSet, QueryVector, MAX_MEMBERSHIP_LIST_LEN,
    MAX_VECTOR_DIMS,
};
pub use path::{FieldPath, JsonKey, PathError, MAX_JSON_KEY_BYTES, MAX_PATH_SEGMENTS};
pub use plan::{
    DbPlan, Direction, NullOrder, OrderKey, PlanError, RowLimit, RowOffset, Select, SelectBuilder,
    MAX_ROW_LIMIT, MAX_ROW_OFFSET,
};
pub use predicate::{
    AggregateFunc, AggregateRef, CompareOp, EscapeChar, MembershipOp, Operand, PatternOp,
    Predicate, PredicateError, RangeBounds, TextPattern, MAX_PREDICATE_DEPTH,
};
pub use projection::{
    Exposure, ProjectedField, Projection, ProjectionError, ProjectionKind, ProjectionSource,
    SearchScalarKind,
};
pub use search::{
    GeoPoint, RadiusMetres, Search, SearchBuilder, SearchCriterion, SearchError, VectorMetric,
    MAX_RADIUS_METRES,
};
pub use render::RenderedSql;
pub use write::{
    Arithmetic, ArithmeticOp, Assignment, BindBudget, ColumnAssignment, ColumnValue, Delete,
    DeleteBuilder, Insert, InsertBuilder, Returning, Update, UpdateBuilder, WriteError, WriteValue,
    MAX_INSERT_ROWS,
};
