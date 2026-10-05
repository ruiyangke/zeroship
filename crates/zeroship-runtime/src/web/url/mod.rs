//! Native URL + URLSearchParams per WHATWG URL Living Standard
//! (https://url.spec.whatwg.org/).
//!
//! Two `#[v8_class]` types backed by the `ada-url::Url` parser, closing
//! three spec gaps:
//!
//! 1. Naive setters: `url.host = "x:y"` split on `:` rather than
//!    delegating to ada-url's mutation API. Per §4.4.2 the host setter
//!    runs the URL parser's "host parser" state, which understands
//!    bracketed IPv6 addresses (`[::1]:8080`), IDNA, etc.
//! 2. URLSearchParams was JS-only with no live-sync to URL.search. Per
//!    §6.1, a URL's `searchParams` returns the SAME object on each
//!    access (`[SameObject]`), and mutations through it must update the
//!    URL's serialized search component.
//! 3. `URL.parse(input, base?) → URL?` (the static newer-spec method)
//!    was missing entirely.
//!
//! ## Design
//!
//! - **URL** holds an `ada_url::Url` and a lazy `Global<Object>` cache
//!   of its searchParams view.
//! - **URLSearchParams** is either standalone (entries owned outright)
//!   or bound to a parent URL (parent's serialized search is the source
//!   of truth; entries cache invalidates on every parent set).
//! - Live sync goes both ways:
//!     - searchParams mutation → re-serialize → write to parent.set_search.
//!     - url.search setter writes ada-url; reads through searchParams
//!       repopulate from `parent.search()` lazily.
//! - The wire format for params storage is `Vec<(String, String)>`
//!   (insertion-ordered list, like Headers).
//!
//! See also the WHATWG URL Living Standard.

use std::cell::Cell;

pub mod helpers;
pub mod search_params;
// Flattening this into `mod.rs` (as done for `web::blob` /
// `web::fetch::body`) would ripple into `crate::url_native::url::URL`
// call sites outside this crate slice (e.g. `rpc/superjson.rs`).
#[allow(clippy::module_inception)]
pub mod url;

/// Per-isolate work counters the `url_native` tests read to assert
/// structural properties instead of measuring wall time.
///
/// A wall-clock budget measures the machine, not the code: a traversal
/// that is fast on an idle box misses its budget under load. These
/// count the *work*: how many times the ada-url parser ran, and how
/// many times a bound `URLSearchParams` re-parsed its parent's search,
/// so a test can assert the shape (parse once per call; parse once per
/// traversal, not once per step).
///
/// Each is incremented at the single call site that performs the work.
/// A fresh isolate starts at zero; the counters live in
/// [`UrlNativeSlot`], so parallel tests in one process do not share
/// them.
#[doc(hidden)]
#[derive(Default)]
pub struct UrlNativeCounters {
    url_parser_runs: Cell<u64>,
    search_params_reparses: Cell<u64>,
}

impl UrlNativeCounters {
    fn note_url_parser_run(&self) {
        self.url_parser_runs.set(self.url_parser_runs.get() + 1);
    }

    fn note_search_params_reparse(&self) {
        self.search_params_reparses
            .set(self.search_params_reparses.get() + 1);
    }
}

/// Record one run of the ada-url parser against the current isolate's
/// counter. A no-op when `install_globals` has not run on the isolate
/// (there is no slot to count against).
pub(crate) fn note_url_parser_run(scope: &v8::PinScope) {
    if let Some(slot) = scope.get_slot::<UrlNativeSlot>() {
        slot.counters.note_url_parser_run();
    }
}

/// Record one re-parse of a bound `URLSearchParams`'s parent search
/// against the current isolate's counter. A no-op without a slot.
pub(crate) fn note_search_params_reparse(scope: &v8::PinScope) {
    if let Some(slot) = scope.get_slot::<UrlNativeSlot>() {
        slot.counters.note_search_params_reparse();
    }
}

/// Number of times the ada-url parser has run in this isolate since
/// `install_globals`: `new URL`, `URL.parse` and `URL.canParse` each
/// contribute one run per call. `#[doc(hidden)]` test hook backing
/// `url_parse_runs_the_parser_once_per_call`.
#[doc(hidden)]
#[must_use]
pub fn url_parser_run_count(scope: &v8::PinScope) -> u64 {
    scope
        .get_slot::<UrlNativeSlot>()
        .map_or(0, |slot| slot.counters.url_parser_runs.get())
}

/// Number of times a bound `URLSearchParams` has re-parsed its parent
/// URL's search component in this isolate since `install_globals`.
/// `#[doc(hidden)]` test hook backing
/// `search_params_iter_is_linear_not_quadratic`.
#[doc(hidden)]
#[must_use]
pub fn search_params_reparse_count(scope: &v8::PinScope) -> u64 {
    scope
        .get_slot::<UrlNativeSlot>()
        .map_or(0, |slot| slot.counters.search_params_reparses.get())
}

/// Per-isolate slot storing the URLSearchParams class function and the
/// URL class function as Globals. URL's `searchParams` getter looks up
/// the URLSearchParams Function via this slot to construct a fresh JS
/// wrapper bound to its parent URL; URL.parse's static callback looks
/// up the URL Function so it can `new_instance` for the parse result.
///
/// Also caches `URLSearchParams.prototype` so the iterator factories
/// and forEach callback can perform a real brand check (M4/M5)
/// — `args.this().[[Prototype]]` chain must contain the cached
/// prototype for the call to be legal.
///
/// Stored per isolate (per `v8::Isolate::set_slot`); read via
/// `scope.get_slot::<UrlNativeSlot>()`. Set once during
/// `install_globals`.
pub struct UrlNativeSlot {
    pub url_class_fn: v8::Global<v8::Function>,
    pub search_params_class_fn: v8::Global<v8::Function>,
    /// `URLSearchParams.prototype` for the per-class brand check
    /// (M4/M5).
    ///
    /// Storage: `v8::Eternal<v8::Object>` rather than
    /// `v8::Global<v8::Object>`. Set-once at install time; the brand
    /// check itself is `crate::brand`'s, but this field is still
    /// populated for any future native call sites. Eternals are
    /// isolate-lifetime handles whose `get(scope)` returns a `Local`
    /// without allocating.
    pub search_params_prototype: v8::Eternal<v8::Object>,
    /// Work counters for the conformance tests. See
    /// [`UrlNativeCounters`].
    counters: UrlNativeCounters,
}

/// Install `URL` and `URLSearchParams` on `globalThis`. Called from
/// `init.rs::load_polyfills_and_modules` after the polyfills load.
///
/// Order matters: URLSearchParams installs first so its class function
/// is available when URL.searchParams's getter runs (the getter
/// fast-paths via the `UrlNativeSlot` set just below).
pub fn install_globals<'s>(scope: &mut v8::PinScope<'s, '_>, global: v8::Local<v8::Object>) {
    let sp_class_fn = search_params::install_global(scope, global);
    let url_class_fn = url::install_global(scope, global);

    // Capture URLSearchParams.prototype for brand-checking iterator
    // factories and forEach (M4/M5).
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let sp_proto_v = sp_class_fn.get(scope, proto_key.into()).unwrap();
    let sp_proto: v8::Local<v8::Object> = sp_proto_v
        .try_into()
        .expect("URLSearchParams.prototype is an Object");

    // search_params_prototype is an Eternal — populated set-once,
    // read by any future native brand-check site. See the field doc.
    let sp_proto_e: v8::Eternal<v8::Object> = v8::Eternal::empty();
    sp_proto_e.set(scope, sp_proto);
    let slot = UrlNativeSlot {
        url_class_fn: v8::Global::new(scope, url_class_fn),
        search_params_class_fn: v8::Global::new(scope, sp_class_fn),
        search_params_prototype: sp_proto_e,
        counters: UrlNativeCounters::default(),
    };
    scope.set_slot::<UrlNativeSlot>(slot);
}
