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
//! In dev mode (`ZEROSHIP_DEV=1`) the SSRF resolver is bypassed so
//! the Vite plugin's ModuleRunner can reach localhost:5173 etc.

use crate::transport::ssrf::{dev_mode_enabled, SsrfResolver};

/// Per-thread cyper client. Cloning is cheap (just bumps an Rc).
pub fn shared_cyper_client() -> cyper::Client {
    thread_local! {
        static CLIENT: cyper::Client = {
            let builder = cyper::Client::builder();
            if dev_mode_enabled() {
                builder.build()
            } else {
                builder.custom_resolver(SsrfResolver).build()
            }
        };
    }
    CLIENT.with(|c| c.clone())
}
