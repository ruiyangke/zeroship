//! Runtime query grammar, collection SQL compilation, and catalog contracts.
//!
//! The typed plan grammar validates identifiers and bounds predicates. The SDK
//! filter decoder feeds that grammar on the ORM execution path. The compiler
//! applies descriptor-based projections and value conversions for each dialect.
//! SQL is a compiler output; the operation grammar has no raw SQL input node.
//!
//! Catalog types and sentinel codecs describe what migration-created tables
//! store. DDL, schema differencing, and migration execution belong exclusively
//! to the migration engine. This module performs no database I/O. Plans are
//! local values and do not derive serialization traits.

pub mod ident;
pub mod joins;
pub use joins::{Join, JoinKind, MAX_READ_SOURCES};
pub mod literal;
pub mod path;
pub mod plan;
pub mod predicate;
pub mod projection;
pub mod render;
pub mod search;
pub mod write;

pub use compiler::BindBudget;
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
pub use write::{
    Arithmetic, ArithmeticOp, Assignment, ColumnAssignment, ColumnValue, Delete, DeleteBuilder,
    Insert, InsertBuilder, Returning, Update, UpdateBuilder, WriteError, WriteValue,
    MAX_INSERT_ROWS,
};

/// Catalog facts used by runtime protection and decoding.
pub mod catalog;
/// Runtime SQL compilation from validated collection operations.
pub mod compile;
pub mod compiler;
pub mod descriptors;
pub mod lifecycle;
pub mod mask_codec;
pub mod registration;
pub mod schema_error;
pub mod statement;
pub use zeroship_core::schema_name::SchemaName;

/// SDK filter decoding into typed predicates.
pub mod filter;

pub mod sqlite_values;

mod array_update;
pub mod codecs;
pub mod json;
pub mod temporal;
pub mod update;
