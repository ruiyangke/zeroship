//! Single-threaded connection pool (Rc-based, fits compio's model).
//!
//! Each compio worker gets its own pool. Connections are borrowed via
//! `PooledConn` and returned on drop if still healthy.

use std::cell::{Cell, RefCell};
use std::ops::{Deref, DerefMut};

use crate::conn::Conn;
use crate::{Error, Result, Row, ToSql};

/// A single-threaded connection pool.
pub struct Pool {
    url: String,
    conns: RefCell<Vec<Conn>>,
    max_size: usize,
    total: Cell<usize>,
}

impl Pool {
    /// Create a new pool. Eagerly opens one connection to verify the URL.
    pub async fn connect(url: &str, max_size: usize) -> Result<Self> {
        let conn = Conn::connect(url).await?;
        let pool = Self {
            url: url.to_string(),
            conns: RefCell::new(vec![conn]),
            max_size,
            total: Cell::new(1),
        };
        Ok(pool)
    }

    /// Acquire a connection from the pool.
    pub async fn get(&self) -> Result<PooledConn<'_>> {
        // Try to pop an idle connection
        let conn = self.conns.borrow_mut().pop();
        if let Some(mut conn) = conn {
            // If the connection needs a rollback from a dropped transaction, send it
            if conn.needs_rollback {
                let _ = conn.execute("ROLLBACK", &[]).await;
                conn.needs_rollback = false;
            }
            return Ok(PooledConn {
                conn: Some(conn),
                pool: self,
            });
        }

        // No idle connections — create a new one if under limit
        if self.total.get() < self.max_size {
            let conn = Conn::connect(&self.url).await?;
            self.total.set(self.total.get() + 1);
            return Ok(PooledConn {
                conn: Some(conn),
                pool: self,
            });
        }

        Err(Error::Pool(format!(
            "pool exhausted (max_size={})",
            self.max_size
        )))
    }

    /// Convenience: acquire a connection, run a query, return the connection.
    pub async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>> {
        let mut conn = self.get().await?;
        conn.query(sql, params).await
    }

    /// Convenience: query with text-format string parameters.
    pub async fn query_text_params(&self, sql: &str, params: &[&str]) -> Result<Vec<Row>> {
        let mut conn = self.get().await?;
        conn.query_text_params(sql, params).await
    }

    /// Convenience: acquire a connection, execute a statement, return the connection.
    pub async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
        let mut conn = self.get().await?;
        conn.execute(sql, params).await
    }

    /// Return a connection to the pool (called by PooledConn::drop).
    fn return_conn(&self, conn: Conn) {
        // Only return healthy connections (idle state)
        if conn.status() == b'I' {
            self.conns.borrow_mut().push(conn);
        } else {
            // Unhealthy or in-transaction — drop it, decrement total
            self.total.set(self.total.get().saturating_sub(1));
        }
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("url", &"***")
            .field("idle", &self.conns.borrow().len())
            .field("total", &self.total.get())
            .field("max_size", &self.max_size)
            .finish()
    }
}

/// A borrowed connection that returns to the pool on drop.
pub struct PooledConn<'a> {
    conn: Option<Conn>,
    pool: &'a Pool,
}

impl Deref for PooledConn<'_> {
    type Target = Conn;
    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().unwrap()
    }
}

impl DerefMut for PooledConn<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.conn.as_mut().unwrap()
    }
}

impl Drop for PooledConn<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.return_conn(conn);
        }
    }
}

impl std::fmt::Debug for PooledConn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledConn").finish()
    }
}
