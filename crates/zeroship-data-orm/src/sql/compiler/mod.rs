//! SQL compiler contracts and native execution output.

mod postgres;
mod query;
mod shared;
mod sqlite;
mod writer;
pub use postgres::PostgresCompiler;
pub use query::{CompiledQuery, ParameterType};
pub use shared::{IdentityPlan, IdentityReadPlan, Requirements, SqlCompiler, SqlSupport};
pub use sqlite::SqliteCompiler;
pub(crate) use writer::{ParameterSlot, SqlWriter};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindBudget {
    dialect: &'static str,
    max: usize,
}

impl BindBudget {
    pub const POSTGRES: Self = Self {
        dialect: "postgres",
        max: u16::MAX as usize,
    };

    pub const SQLITE: Self = Self {
        dialect: "sqlite",
        max: 32_766,
    };

    #[must_use]
    pub const fn max(self) -> usize {
        self.max
    }

    #[must_use]
    pub const fn dialect_name(self) -> &'static str {
        self.dialect
    }
}

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
