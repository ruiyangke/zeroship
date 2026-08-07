//! Live-connection accounting and an awaitable drain.
//!
//! A [`Connection`](crate::Connection) owns its socket and is normally driven
//! by a detached task. Dropping the [`Client`](crate::Client) half only *asks*
//! that task to shut down: it still has to write `Terminate`, drain the wire,
//! and let the socket drop. That shutdown is asynchronous, so it only completes
//! if the runtime is still being polled.
//!
//! If the runtime is torn down first, the socket is orphaned: an io_uring
//! submission co-owns the descriptor and is never reclaimed, so the descriptor
//! stays open - and the server-side backend stays live - for the rest of the
//! process. Callers that tear a runtime down deliberately (a test, a graceful
//! shutdown path) therefore need a way to wait for the shutdown to land rather
//! than guessing at a sleep.
//!
//! [`live_connections`] is that observation point and [`drain_connections`] is
//! the wait. The counter is per-thread because a `Connection` never leaves the
//! thread whose runtime drives it.

use std::cell::Cell;
use std::time::{Duration, Instant};

thread_local! {
    /// Connections constructed on this thread that have not been dropped yet.
    static LIVE: Cell<usize> = const { Cell::new(0) };
}

/// Increments the live count on construction and decrements it on drop.
///
/// Held as a field of `Connection`, so the count falls exactly when the
/// connection - and with it the socket - is released.
pub(crate) struct LiveConnectionGuard;

impl LiveConnectionGuard {
    pub(crate) fn new() -> Self {
        LIVE.with(|c| c.set(c.get() + 1));
        Self
    }
}

impl Drop for LiveConnectionGuard {
    fn drop(&mut self) {
        LIVE.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// Number of connections this thread has opened and not yet released.
#[must_use]
pub fn live_connections() -> usize {
    LIVE.with(Cell::get)
}

/// Wait until every connection opened on this thread has been released, or
/// until `timeout` elapses. Returns `true` if the drain completed.
///
/// Call this after dropping the last `Client`/`Pool` handle and before letting
/// the runtime go, so the connections' sockets are closed while there is still
/// a runtime to close them. A handle that is still alive keeps its connection
/// counted, so an outstanding one makes this wait out its whole timeout.
pub async fn drain_connections(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if live_connections() == 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        compio::time::sleep(Duration::from_millis(1)).await;
    }
}
