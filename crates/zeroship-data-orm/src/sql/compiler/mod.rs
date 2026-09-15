//! SQL compiler contracts and native execution output.

#[cfg(test)]
mod array_tests;
#[cfg(test)]
mod comparison_tests;
#[cfg(test)]
mod coordination_tests;
#[cfg(test)]
mod lock_tests;
mod postgres;
mod query;
mod shared;
mod sqlite;
#[cfg(test)]
mod timestamp_tests;
mod writer;
pub use postgres::PostgresCompiler;
pub use query::{CompiledQuery, ParameterType};
pub(crate) use shared::compiler_requirements;
pub use shared::{IdentityPlan, IdentityReadPlan, Requirements, SqlCompiler, SqlSupport};
pub use sqlite::SqliteCompiler;
pub(crate) use writer::{ParameterSlot, SqlWriter};

pub(crate) const POSTGRES_BIND_LIMIT: usize = u16::MAX as usize;
pub(crate) const SQLITE_BIND_LIMIT: usize = 32_766;
pub(crate) const SQLITE_SPATIAL_IDENTITY_ALIAS: &str = "__zs_spatial_identity";

pub(crate) fn enforce_support(
    implemented: SqlSupport,
    required: &Requirements,
    effective: &SqlSupport,
) -> Result<(), CompileError> {
    shared::check(implemented, required, effective)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    BindLimitExceeded { limit: usize },
    InvalidStatement(String),
    Unsupported(&'static str),
    /// A value or offset carries a sub-millisecond part the registered backend
    /// cannot store. It is refused rather than floored: a caller that reads an
    /// instant back and compares it for equality must get the value it wrote.
    TimestampPrecisionUnsupported,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidStatement(message) => f.write_str(message),
            Self::Unsupported(feature) => write!(f, "SQL compiler does not support {feature}"),
            Self::BindLimitExceeded { limit } => {
                write!(f, "statement exceeds the backend bind limit of {limit}")
            }
            Self::TimestampPrecisionUnsupported => f.write_str(
                "this database stores whole milliseconds; a sub-millisecond timestamp is refused",
            ),
        }
    }
}

impl std::error::Error for CompileError {}

impl From<super::IdentError> for CompileError {
    fn from(error: super::IdentError) -> Self {
        Self::InvalidStatement(error.to_string())
    }
}
