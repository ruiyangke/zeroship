//! appbase-pg — minimal PostgreSQL driver for compio/io_uring.
//!
//! Uses `postgres-protocol` for wire format encoding/decoding and SCRAM-SHA-256
//! authentication. Provides buffered compio I/O, connection pool, and a simple
//! query API for the appbase platform.

mod conn;
mod pool;
mod stream;

use std::sync::Arc;

pub use conn::Conn;
pub use pool::{Pool, PooledConn};
pub use postgres_types::{FromSql, ToSql, Type};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors from the PostgreSQL driver.
#[derive(Debug)]
pub enum Error {
    /// PostgreSQL error response (severity, SQLSTATE code, message).
    Postgres {
        severity: String,
        code: String,
        message: String,
    },
    /// I/O error (connection refused, timeout, broken pipe).
    Io(std::io::Error),
    /// Wire protocol violation (unexpected message, malformed data).
    Protocol(String),
    /// Authentication failure (bad password, unsupported mechanism).
    Auth(String),
    /// Connection pool exhausted.
    Pool(String),
    /// TLS negotiation failure.
    Tls(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Postgres {
                severity,
                code,
                message,
            } => {
                write!(f, "pg error ({severity}/{code}): {message}")
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::Auth(msg) => write!(f, "auth error: {msg}"),
            Self::Pool(msg) => write!(f, "pool error: {msg}"),
            Self::Tls(msg) => write!(f, "TLS error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Result type for this crate.
pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Row + Column
// ---------------------------------------------------------------------------

/// A single row returned from a query.
pub struct Row {
    pub(crate) columns: Arc<Vec<Column>>,
    pub(crate) values: Vec<Option<Vec<u8>>>,
}

/// Column metadata from RowDescription.
#[derive(Debug, Clone)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// PostgreSQL type OID.
    pub oid: u32,
}

impl Row {
    /// Get a column value by name, panicking if missing or wrong type.
    pub fn get<'a, T>(&'a self, column: &str) -> T
    where
        T: FromSql<'a>,
    {
        self.try_get(column)
            .unwrap_or_else(|e| panic!("column '{column}': {e}"))
    }

    /// Get a column value by name, returning an error if missing or wrong type.
    pub fn try_get<'a, T>(&'a self, column: &str) -> Result<T>
    where
        T: FromSql<'a>,
    {
        let idx = self
            .columns
            .iter()
            .position(|c| c.name == column)
            .ok_or_else(|| Error::Protocol(format!("no column '{column}'")))?;

        let col = &self.columns[idx];
        let pg_type = Type::from_oid(col.oid).unwrap_or(Type::TEXT);

        match &self.values[idx] {
            Some(bytes) => T::from_sql(&pg_type, bytes)
                .map_err(|e| Error::Protocol(format!("column '{column}': {e}"))),
            None => T::from_sql_null(&pg_type)
                .map_err(|e| Error::Protocol(format!("column '{column}' is null: {e}"))),
        }
    }

    /// Returns the number of columns.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns true if the row has no columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Returns the column metadata.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }
}

impl std::fmt::Debug for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Row")
            .field("columns", &self.columns)
            .field("values", &format_args!("[{} values]", self.values.len()))
            .finish()
    }
}
