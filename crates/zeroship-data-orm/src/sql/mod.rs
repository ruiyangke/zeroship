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
pub use joins::{JoinKind, MAX_READ_SOURCES};
pub mod literal;
pub mod path;
pub mod predicate;
pub mod read;

pub use compiler::BindBudget;
pub use ident::{Ident, IdentError, IdentRole, MAX_IDENT_BYTES};
pub use literal::{
    Finite, Finite32, Literal, LiteralError, LiteralSet, QueryVector, MAX_MEMBERSHIP_LIST_LEN,
    MAX_VECTOR_DIMS,
};
pub use path::{FieldPath, JsonKey, PathError, MAX_JSON_KEY_BYTES, MAX_PATH_SEGMENTS};
pub use predicate::{
    AggregateFunc, AggregateRef, CompareOp, EscapeChar, MembershipOp, Operand, PatternOp,
    Predicate, PredicateError, RangeBounds, TextPattern, MAX_PREDICATE_DEPTH,
};
pub use read::{
    Direction, NullOrder, OrderKey, ReadError, RowLimit, RowOffset, MAX_ROW_LIMIT, MAX_ROW_OFFSET,
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

pub mod codecs;
pub mod json;
pub mod temporal;
pub mod update;
