use super::FieldPath;
use core::fmt;

pub const MAX_ROW_LIMIT: i64 = super::compile::MAX_QUERY_LIMIT;
pub const MAX_ROW_OFFSET: i64 = super::compile::MAX_QUERY_OFFSET;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowLimit(i64);

impl RowLimit {
    pub fn new(rows: i64) -> Result<Self, ReadError> {
        if (1..=MAX_ROW_LIMIT).contains(&rows) {
            Ok(Self(rows))
        } else {
            Err(ReadError::LimitOutOfRange { requested: rows })
        }
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl Default for RowLimit {
    fn default() -> Self {
        Self(MAX_ROW_LIMIT)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowOffset(i64);

impl RowOffset {
    pub fn new(rows: i64) -> Result<Self, ReadError> {
        if (0..=MAX_ROW_OFFSET).contains(&rows) {
            Ok(Self(rows))
        } else {
            Err(ReadError::OffsetOutOfRange { requested: rows })
        }
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NullOrder {
    First,
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderKey {
    pub path: FieldPath,
    pub direction: Direction,
    pub nulls: NullOrder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    LimitOutOfRange { requested: i64 },
    OffsetOutOfRange { requested: i64 },
    PredicateTooComplex,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitOutOfRange { requested } => {
                write!(f, "row limit {requested} is outside the supported range")
            }
            Self::OffsetOutOfRange { requested } => {
                write!(f, "row offset {requested} is outside the supported range")
            }
            Self::PredicateTooComplex => {
                f.write_str("read predicate exceeds its complexity budget")
            }
        }
    }
}

impl std::error::Error for ReadError {}
