//! Native URL + URLSearchParams per WHATWG URL Living Standard
//! (https://url.spec.whatwg.org/).
//!
//! Replaces `crates/runtime/src/embed/url.js` (127 LOC of JS polyfill on
//! top of the `__urlParse` callback) with two `#[v8_class]` types backed
//! by the same `ada-url::Url` parser.
//!
//! The polyfill had three main spec gaps that motivate this rewrite:
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
//! See also `docs/architecture/runtime.md` and the WHATWG spec.

pub mod helpers;
pub mod search_params;
pub mod url;

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
    pub search_params_prototype: v8::Global<v8::Object>,
}

/// Install `URL` and `URLSearchParams` on `globalThis`. Called from
/// `init.rs::load_polyfills_and_modules` after the polyfills load
/// (URL_JS gets removed once this is wired in).
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

    let slot = UrlNativeSlot {
        url_class_fn: v8::Global::new(scope, url_class_fn),
        search_params_class_fn: v8::Global::new(scope, sp_class_fn),
        search_params_prototype: v8::Global::new(scope, sp_proto),
    };
    scope.set_slot::<UrlNativeSlot>(slot);
}
