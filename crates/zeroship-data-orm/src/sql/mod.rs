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
pub mod identity;
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

pub use ident::{Ident, IdentError, IdentRole, MAX_IDENT_BYTES};
pub use literal::{
    Finite, Finite32, Literal, LiteralError, LiteralSet, MAX_MEMBERSHIP_LIST_LEN, MAX_VECTOR_DIMS,
    QueryVector,
};
pub use path::{FieldPath, JsonKey, MAX_JSON_KEY_BYTES, MAX_PATH_SEGMENTS, PathError};
pub use plan::{
    DbPlan, Direction, MAX_ROW_LIMIT, MAX_ROW_OFFSET, NullOrder, OrderKey, PlanError, RowLimit,
    RowOffset, Select, SelectBuilder,
};
pub use predicate::{
    AggregateFunc, AggregateRef, CompareOp, EscapeChar, MAX_PREDICATE_DEPTH, MembershipOp, Operand,
    PatternOp, Predicate, PredicateError, RangeBounds, TextPattern,
};
pub use projection::{
    Exposure, ProjectedField, Projection, ProjectionError, ProjectionKind, ProjectionSource,
    SearchScalarKind,
};
pub use search::{
    GeoPoint, MAX_RADIUS_METRES, RadiusMetres, Search, SearchBuilder, SearchCriterion, SearchError,
    VectorMetric,
};
pub use write::{
    Arithmetic, ArithmeticOp, Assignment, BindBudget, ColumnAssignment, ColumnValue, Delete,
    DeleteBuilder, Insert, InsertBuilder, MAX_INSERT_ROWS, Returning, Update, UpdateBuilder,
    WriteError, WriteValue,
};

/// Catalog facts used by runtime protection and decoding.
pub mod catalog;
/// Runtime SQL compilation from validated collection operations.
pub mod compile;
pub mod compiler;
pub mod statement;
pub mod registration;
pub mod descriptors;
pub mod lifecycle;
pub mod mask_codec;
pub mod schema_error;
pub use zeroship_core::schema_name::SchemaName;

/// SDK filter decoding into typed predicates.
pub mod filter;

pub mod internal;

pub mod sqlite_values;

pub mod sqlite_search;

mod array_update;
pub mod codecs;
pub mod json;
pub mod temporal;
pub mod update;
