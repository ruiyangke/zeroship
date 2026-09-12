//! SQL compiler contracts and native execution output.

mod query;
mod writer;
pub use query::{CompiledQuery, ParameterType};
pub(crate) use writer::{ParameterSlot, SqlWriter};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    BindLimitExceeded { limit: usize },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BindLimitExceeded { limit } => {
                write!(f, "statement exceeds the backend bind limit of {limit}")
            }
        }
    }
}

impl std::error::Error for CompileError {}
