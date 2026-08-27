//! Per-thread cyper HTTP client.
//!
//! Bridges the spec-side `web::fetch` algorithm chain to the wire layer.
//! The thread-local instance is shared across every `fetch()` call made on
//! the same THREAD - which is many apps, not one. A worker thread hosts a
//! whole LRU of isolates (`crates/worker/src/cache.rs`), so one app's warm
//! pooled connection is an entry another app can be served from. That is a
//! real property of this client and the reason a per-app fetch policy has no
//! slot here today; `fetch` is not gated, so nothing depends on it yet.
//!
//! **Must be thread-local**, NOT a process-wide static. cyper's connector
//! wraps its I/O in `SendWrapper` (panics if dereferenced from a thread
//! other than the one that created it). With V8 isolates per worker
//! thread each driving `fetch()` calls, a process-wide `OnceLock<Client>`
//! would cache pooled connections on whichever thread ran first, then
//! panic the moment a different thread's isolate dispatched. Per-thread
//! costs a small handful of idle connections per host — cheap and
//! correct.
//!
//! THE SSRF RESOLVER IS INSTALLED IN BOTH MODES. A process that stated the
//! dev relaxation gets `SsrfResolver::dev_loopback()`, which lets loopback
//! through so the Vite plugin's `ModuleRunner` can reach `localhost:5173` and
//! strips every other blocked range exactly as production does.
//!
//! Until 2026-08-27 the dev arm built the client with `builder.build()` - no
//! custom resolver at all - so a hostname that resolved into link-local or
//! RFC1918 space connected. That is the same SSRF hole `validate_url` guards
//! one layer up, and it was reachable by any process holding `ZEROSHIP_DEV=1`,
//! which at the time included a production worker that had inherited it.

use crate::transport::ssrf::{dev_mode_enabled, SsrfResolver};

/// Which SSRF resolver a given mode installs.
///
/// The RETURN TYPE is the guarantee. It is `SsrfResolver`, not
/// `Option<SsrfResolver>`, so "this mode gets no resolver" - which is what the
/// dev arm did until 2026-08-27 - cannot be written here without changing the
/// signature. A test can pin which resolver each mode selects; only the type
/// can rule out selecting none.
const fn resolver_for(dev_loopback: bool) -> SsrfResolver {
    if dev_loopback {
        SsrfResolver::dev_loopback()
    } else {
        SsrfResolver::strict()
    }
}

/// Per-thread cyper client. Cloning is cheap (just bumps an Rc).
///
/// The mode is read ONCE per thread, when that thread first builds its client.
/// A `set_dev_mode` call after a thread has a client does not re-resolve it,
/// which is why the embedding process states the mode before it serves (see
/// `cmd_serve` in `crates/zeroship-cli/src/main.rs`).
pub fn shared_cyper_client() -> cyper::Client {
    thread_local! {
        static CLIENT: cyper::Client = cyper::Client::builder()
            .custom_resolver(resolver_for(dev_mode_enabled()))
            .build();
    }
    CLIENT.with(|c| c.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both modes install a resolver, and the dev one is the LOOPBACK-ONLY
    /// resolver rather than an absent filter.
    ///
    /// Reads the mode as an argument, never from the process-wide cell: this
    /// module's client is a `thread_local!` that resolves the mode once per
    /// thread, so a test that wrote the cell would decide what every later
    /// `fetch` on that thread resolves through.
    #[test]
    fn both_modes_install_a_resolver_and_dev_is_loopback_only() {
        assert!(resolver_for(true).admits_loopback());
        assert!(!resolver_for(false).admits_loopback());
    }
}
