//! Single-threaded connection pool (Rc-based, fits compio's model).

/// A single-threaded connection pool.
pub struct Pool;

/// A borrowed connection that returns to the pool on drop.
pub struct PooledConn;
