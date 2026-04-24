//! Minimal connection pool. Simpler than compio-postgres's — Redis
//! connections are cheap and stateless (no transactions to preserve,
//! no prepared statements), so a basic "idle stack + on-demand new
//! conn" strategy is plenty.
//!
//! Lifecycle:
//! - `acquire()` pops from the idle stack; if empty and under max_size,
//!   opens a fresh connection.
//! - Returning via drop of a `PooledConn` guard pushes back onto idle.
//! - On command error the guard is marked poisoned and the connection
//!   is dropped, not returned to the pool.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::client::Client;
use crate::error::{Error, Result};

#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub max_size: usize,
    /// Warm-up connections opened eagerly in `connect()`. Zero means
    /// open on first acquire.
    pub min_idle: usize,
    /// Max time a connection can stay idle before being dropped.
    pub idle_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_size: 16,
            min_idle: 1,
            idle_timeout: Duration::from_secs(600),
        }
    }
}

struct Inner {
    url: String,
    config: PoolConfig,
    /// Idle connections, LIFO for hottest-first reuse.
    idle: Vec<(Client, Instant)>,
    /// Currently-in-use count.
    busy: usize,
}

/// Pool handle — cheap to clone.
#[derive(Clone)]
pub struct Pool {
    inner: Rc<RefCell<Inner>>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.borrow();
        f.debug_struct("Pool")
            .field("url", &"redis://…(redacted)")
            .field("max_size", &inner.config.max_size)
            .field("idle", &inner.idle.len())
            .field("busy", &inner.busy)
            .finish()
    }
}

impl Pool {
    pub async fn connect(url: &str, max_size: usize) -> Result<Self> {
        Self::connect_with(url, PoolConfig { max_size, ..PoolConfig::default() }).await
    }

    pub async fn connect_with(url: &str, config: PoolConfig) -> Result<Self> {
        // Warm up at least one connection so the URL is validated and
        // auth/select ran at least once before any user code runs.
        let mut idle = Vec::with_capacity(config.min_idle.max(1));
        let warm = config.min_idle.max(1);
        for _ in 0..warm {
            let c = Client::connect(url).await?;
            idle.push((c, Instant::now()));
        }
        Ok(Pool {
            inner: Rc::new(RefCell::new(Inner {
                url: url.to_string(),
                config,
                idle,
                busy: 0,
            })),
        })
    }

    /// Acquire a connection, opening a new one if idle is empty and
    /// capacity permits.
    pub async fn acquire(&self) -> Result<PooledConn> {
        // Fast path: take from idle stack, dropping timed-out entries.
        let now = Instant::now();
        let (client, opened_new) = {
            let mut inner = self.inner.borrow_mut();
            // Drop stale idle conns.
            let timeout = inner.config.idle_timeout;
            while let Some((_, ts)) = inner.idle.last() {
                if now.duration_since(*ts) > timeout {
                    inner.idle.pop();
                } else {
                    break;
                }
            }
            if let Some((c, _)) = inner.idle.pop() {
                inner.busy += 1;
                (Some(c), false)
            } else if inner.busy < inner.config.max_size {
                inner.busy += 1;
                (None, true)
            } else {
                return Err(Error::Pool(format!(
                    "pool exhausted — max_size={}, busy={}",
                    inner.config.max_size, inner.busy
                )));
            }
        };

        let client = match client {
            Some(c) => c,
            None => {
                let url = self.inner.borrow().url.clone();
                match Client::connect(&url).await {
                    Ok(c) => c,
                    Err(e) => {
                        // Back out the busy counter on failure.
                        self.inner.borrow_mut().busy -= 1;
                        return Err(e);
                    }
                }
            }
        };
        let _ = opened_new; // silence unused
        Ok(PooledConn {
            pool: self.inner.clone(),
            client: Some(client),
            poisoned: false,
        })
    }
}

/// Guard that auto-returns the connection on drop.
pub struct PooledConn {
    pool: Rc<RefCell<Inner>>,
    client: Option<Client>,
    poisoned: bool,
}

impl PooledConn {
    /// Mark the connection as dead — don't return to pool on drop.
    pub fn poison(&mut self) { self.poisoned = true; }

    pub fn as_mut(&mut self) -> &mut Client {
        self.client.as_mut().expect("client taken")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        let mut inner = self.pool.borrow_mut();
        inner.busy = inner.busy.saturating_sub(1);
        if let Some(c) = self.client.take() {
            if !self.poisoned {
                inner.idle.push((c, Instant::now()));
            }
        }
    }
}

impl std::ops::Deref for PooledConn {
    type Target = Client;
    fn deref(&self) -> &Client { self.client.as_ref().expect("client taken") }
}

impl std::ops::DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut Client { self.client.as_mut().expect("client taken") }
}
