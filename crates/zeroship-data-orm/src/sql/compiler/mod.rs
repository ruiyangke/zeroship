//! SQL compiler contracts and native execution output.

mod query;
mod postgres;
mod shared;
mod sqlite;
mod writer;
pub use query::{CompiledQuery, ParameterType};
pub use postgres::PostgresCompiler;
pub use sqlite::SqliteCompiler;
pub use shared::{Requirements, SqlCompiler, SqlSupport};
pub(crate) use writer::{ParameterSlot, SqlWriter};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    BindLimitExceeded { limit: usize },
    InvalidStatement(String),
    Unsupported(&'static str),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidStatement(message) => f.write_str(message),
            Self::Unsupported(feature) => write!(f, "SQL compiler does not support {feature}"),
            Self::BindLimitExceeded { limit } => {
                write!(f, "statement exceeds the backend bind limit of {limit}")
            }
        }
    }
}

impl std::error::Error for CompileError {}

impl From<super::IdentError> for CompileError {
    fn from(error: super::IdentError) -> Self {
        Self::InvalidStatement(error.to_string())
    }
}
