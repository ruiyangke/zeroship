//! Registry of in-flight `env.storage.getStream` download sources.
//!
//! # This registry is per-THREAD, not per-isolate
//!
//! The backing store is a `thread_local!`, and a worker OS thread
//! multiplexes up to `--max-isolates` (default **200**, see
//! `crates/zeroship-worker/src/main.rs:67`) app isolates — they `enter`/`exit` around
//! each dispatch (`crates/zeroship-worker/src/handler.rs:386-396`) rather than each
//! owning a thread. So everything in here is shared by every app resident on
//! the thread, and an unkeyed `HashMap<u32, _>` would be a cross-tenant
//! channel: `readChunk(1)` from app B would hand back app A's object bytes,
//! and `cancelStream(n)` would destroy A's in-flight download. Ids are
//! allocated monotonically from 1, so they are enumerable by construction —
//! there is no secrecy to lean on.
//!
//! This is the same threat plugin-db documents as **SEC-1** on
//! `ThreadDbContext::tx_conns` (`crates/zeroship-data-v8/src/context.rs:127-140`):
//! a per-thread slot that parks one app's resource across an `await` is
//! reachable by every co-resident app unless it is keyed by the owning
//! `app_id`. We take the same remedy for the same reason.
//!
//! Where the two diverge: plugin-db can note that "V8 is single-threaded per
//! isolate, so a given app still has at most one entry", which bounds its map
//! by construction. **That reasoning does not transfer here** — an app may
//! hold many concurrent download streams, so this registry needs an explicit
//! bound of its own. See [`open`] and
//! [`crate::limits::max_live_get_streams_per_app`].
//!
//! # Why ownership cannot be bypassed
//!
//! `LIVE` is private to this module and no accessor returns it. The map is
//! nested app-major, so a stream slot is only reachable by first indexing an
//! app. Every function exported from this module takes the owning `app_id` as
//! its **first parameter**; there is no overload, no default, and no
//! `stream_id`-only entry point. A future `read_chunk_v2` that forgets to ask
//! who is calling therefore does not compile — it has nothing to pass.
//!
//! # Reclamation
//!
//! There is no runtime teardown hook to sweep this from: `NativePlugin`
//! (`crates/zeroship-runtime/src/core/plugin.rs:36-83`) has only `namespace` / `name`
//! / `register` / `build_instance`, all of which run once at isolate
//! construction, and request completion
//! (`RuntimeInner::clear_executing_request`,
//! `crates/zeroship-runtime/src/core/runtime.rs:3372-3376`) is a two-field assignment
//! that calls no plugin code.
//!
//! A request-scoped sweep would also be *wrong*, not merely absent: the
//! `@zeroship/storage` SDK wraps a handle in a `ReadableStream` whose `pull`
//! calls `readChunk` lazily (`sdks/storage/src/index.ts:203-210`), so a
//! creator returning `new Response(body)` legitimately drains the stream
//! after the fetch handler has already resolved. Tearing streams down at
//! request end would break the primary streaming-download path.
//!
//! So the bound is a cap at acquisition — the mechanism plugin-db uses for
//! `MAX_SUBSCRIPTIONS_PER_APP` (`crates/zeroship-data-orm/src/broker.rs:153`) and the
//! runtime uses for `MAX_PENDING_FETCHES` (`crates/zeroship-runtime/src/core/state.rs:284`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::backend::BoxByteStream;

/// The parked source for one live download stream.
///
/// `Rc<RefCell<Option<…>>>` so `readChunk` can take the source out for the
/// duration of an async pull and put it back, without holding a `RefCell`
/// borrow across the await.
pub(crate) type StreamSlot = Rc<RefCell<Option<BoxByteStream>>>;

/// One app's live download streams. Private: only this module constructs or
/// reads it, which is what keeps `app_id` mandatory at every entry point.
#[derive(Default)]
struct AppStreams {
    streams: HashMap<u32, StreamSlot>,
    /// Monotonic id source, **per app**. Scoping the counter as well as the
    /// map means one app's ids reveal nothing about another's traffic volume.
    next_id: u32,
}

thread_local! {
    /// App-major registry. Never exposed; see the module docs.
    static LIVE: RefCell<HashMap<String, AppStreams>> = RefCell::new(HashMap::new());
}

/// Park `source` as a new live download stream owned by `app_id`, returning
/// the id the app will quote back to [`slot`] / [`close`].
///
/// `Err` when the app already holds
/// [`crate::limits::max_live_get_streams_per_app`] live streams. Each entry
/// pins a process-wide resource — an open fd on `LocalFs`, a live HTTP
/// response body on S3 — and nothing but the app reclaims them, so the cap is
/// what stops one tenant from exhausting the thread. Refusing (rather than
/// evicting the oldest) is deliberate: evicting would surface to the victim
/// stream as a clean EOF, silently truncating an object mid-download.
pub(crate) fn open(app_id: &str, source: BoxByteStream) -> Result<u32, String> {
    let cap = crate::limits::max_live_get_streams_per_app();
    LIVE.with(|live| {
        let mut live = live.borrow_mut();
        let app = live.entry(app_id.to_string()).or_default();

        if app.streams.len() >= cap {
            return Err(format!(
                "storage: too many live download streams ({cap} max per app); \
                 drain or cancelStream an open getStream handle before opening \
                 another (override with {})",
                crate::limits::MAX_LIVE_GET_STREAMS_PER_APP_ENV
            ));
        }

        // Allocate the next free id. The probe matters only after a u32
        // wraparound, where a naive counter could collide with a still-live
        // stream. It terminates because we are below `cap`, and `cap` is
        // clamped below the u32 id space (`limits::MAX_LIVE_GET_STREAMS_CEILING`),
        // so at least one id is always free.
        let id = loop {
            app.next_id = app.next_id.wrapping_add(1).max(1);
            if !app.streams.contains_key(&app.next_id) {
                break app.next_id;
            }
        };
        app.streams.insert(id, Rc::new(RefCell::new(Some(source))));
        Ok(id)
    })
}

/// The slot for `stream_id` **if `app_id` owns it**. A stream belonging to
/// another app is indistinguishable from one that never existed, which is
/// what the callers turn into a clean EOF.
pub(crate) fn slot(app_id: &str, stream_id: u32) -> Option<StreamSlot> {
    LIVE.with(|live| live.borrow().get(app_id)?.streams.get(&stream_id).cloned())
}

/// Drop `stream_id` **if `app_id` owns it**; a no-op otherwise. Dropping the
/// slot releases the underlying fd / HTTP body.
pub(crate) fn close(app_id: &str, stream_id: u32) {
    LIVE.with(|live| {
        let mut live = live.borrow_mut();
        let Some(app) = live.get_mut(app_id) else { return };
        app.streams.remove(&stream_id);
        // Don't retain an empty per-app entry: the key is an app-supplied
        // String, and keeping one per app ever seen would be its own
        // (smaller) unbounded per-thread growth.
        if app.streams.is_empty() {
            live.remove(app_id);
        }
    });
}

/// How many live streams `app_id` currently holds. Test/assertion support.
#[cfg(test)]
pub(crate) fn live_count(app_id: &str) -> usize {
    LIVE.with(|live| live.borrow().get(app_id).map_or(0, |a| a.streams.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ChunkResult, ChunkSource};

    /// A source that never yields, so nothing self-reclaims during a test.
    struct Idle;

    #[async_trait::async_trait(?Send)]
    impl ChunkSource for Idle {
        async fn next_chunk(&mut self) -> Option<ChunkResult> {
            None
        }
    }

    fn idle() -> BoxByteStream {
        Box::new(Idle)
    }

    #[test]
    fn a_stream_is_invisible_to_another_app() {
        let id = open("app_a", idle()).unwrap();
        assert!(slot("app_a", id).is_some(), "owner must reach its own stream");
        assert!(slot("app_b", id).is_none(), "a co-resident app must not reach it");
        close("app_a", id);
    }

    #[test]
    fn close_by_a_non_owner_does_not_destroy_the_stream() {
        let id = open("app_c", idle()).unwrap();
        close("app_d", id);
        assert!(
            slot("app_c", id).is_some(),
            "a non-owner's cancelStream must not reclaim the owner's stream"
        );
        close("app_c", id);
        assert!(slot("app_c", id).is_none());
    }

    #[test]
    fn ids_are_scoped_per_app_so_both_apps_see_low_ids() {
        let a = open("app_e", idle()).unwrap();
        let b = open("app_f", idle()).unwrap();
        // Both start at 1: the counter is per-app, so ids leak no
        // information about another tenant's stream volume.
        assert_eq!((a, b), (1, 1));
        assert!(slot("app_e", b).is_some());
        assert!(slot("app_f", a).is_some());
        close("app_e", a);
        close("app_f", b);
    }

    #[test]
    fn the_cap_is_charged_per_app() {
        let cap = crate::limits::max_live_get_streams_per_app();
        let mut ids = Vec::new();
        for _ in 0..cap {
            ids.push(open("app_g", idle()).expect("under cap"));
        }
        let err = open("app_g", idle()).expect_err("over cap must be refused");
        assert!(err.contains("live download streams"), "unexpected error: {err}");

        // A co-resident app is unaffected — the cap must not be a
        // cross-tenant denial of service.
        let other = open("app_h", idle()).expect("a different app has its own budget");
        close("app_h", other);

        // Closing one frees exactly one slot.
        close("app_g", ids.pop().unwrap());
        let reopened = open("app_g", idle()).expect("a freed slot is reusable");
        assert_eq!(live_count("app_g"), cap);
        close("app_g", reopened);
        for id in ids {
            close("app_g", id);
        }
        assert_eq!(live_count("app_g"), 0);
    }

    #[test]
    fn emptying_an_app_drops_its_registry_entry() {
        let id = open("app_i", idle()).unwrap();
        close("app_i", id);
        assert_eq!(live_count("app_i"), 0);
        LIVE.with(|live| {
            assert!(
                !live.borrow().contains_key("app_i"),
                "an app with no live streams must not retain a registry entry"
            );
        });
    }
}
