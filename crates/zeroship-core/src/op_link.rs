//! The gateway -> auth ("OP") HTTP connection-lifetime contract.
//!
//! The auth service serves the OP endpoints the gateway brokers to
//! (`POST /oauth2/token`, `POST /oauth2/revoke`). Its ntex server closes an
//! idle keep-alive connection after [`AUTH_KEEP_ALIVE_SECS`]. The gateway pools
//! one `cyper::Client` per worker thread and reuses the connections in it; if
//! the pool hands out a connection the server has already closed, the request
//! fails with a transport error. On a non-idempotent `POST /oauth2/token`
//! that failure is also counted against the gateway's circuit breaker.
//!
//! Both sides take the server window from [`AUTH_KEEP_ALIVE_SECS`], and the
//! client derives its reuse bound from that same window through
//! [`op_client_idle_timeout`]. The client bound is always a fraction of the
//! server window, so the two cannot drift apart.

use std::time::Duration;

/// How long the auth HTTP server keeps an idle keep-alive connection open, in
/// whole seconds (ntex configures its keep-alive in whole seconds).
pub const AUTH_KEEP_ALIVE_SECS: u16 = 5;

/// [`AUTH_KEEP_ALIVE_SECS`] as a [`Duration`] for the client-side derivation.
pub const AUTH_KEEP_ALIVE: Duration = Duration::from_secs(AUTH_KEEP_ALIVE_SECS as u64);

/// How long the gateway may reuse a pooled OP client before rebuilding it.
///
/// Strictly shorter than `keep_alive`: halving it leaves headroom for timer and
/// scheduler jitter, so a pooled connection is retired before the server closes
/// it.
#[must_use]
pub fn op_client_idle_timeout(keep_alive: Duration) -> Duration {
    keep_alive / 2
}

#[cfg(test)]
mod tests {
    use super::{op_client_idle_timeout, AUTH_KEEP_ALIVE};

    #[test]
    fn client_reuse_bound_is_positive_and_strictly_below_the_server_window() {
        // The invariant the whole module exists for: the client must retire a
        // pooled connection BEFORE the server closes it. A zero bound would
        // rebuild on every call (no pooling) and a bound at or above the
        // server window would let the stale-connection race back in.
        let bound = op_client_idle_timeout(AUTH_KEEP_ALIVE);
        assert!(bound > std::time::Duration::ZERO, "a zero bound disables pooling");
        assert!(
            bound < AUTH_KEEP_ALIVE,
            "the client bound must be strictly shorter than the server window"
        );
    }
}
