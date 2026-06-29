//! Native `Request` class per WHATWG Fetch §5.4
//! (https://fetch.spec.whatwg.org/#request-class).
//!
//! `Box<RequestState>` lives in V8 internal field 0; the unit struct
//! `Request` is the JS-class identity that drives the install slot,
//! brand check, callback names, `Symbol.toStringTag`, and
//! `constructor.name` via `#[v8_state_marker(Request)] impl
//! RequestState`.
//!
//! What stays hand-rolled (per design §7.2.1):
//!   - `state_ptr` (private) — `Body` trait + `mod.rs::snapshot_request`
//!     need raw `*mut RequestState` access without the macro brand check.
//!   - `install_global` wrapper — calls `Request::install`, wires body
//!     trait methods (`install_body_methods::<Request>`).
//!   - `build_kernel_request` — kernel fast-path Request builder.
//!   - Helpers for headers / signal / body / method validation.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use crate::fetch_body::body::{Body, BodyImpl};
use crate::fetch_body::consumers::{install_body_methods, BodyMarker};
use crate::fetch_body::extract::extract_body;
use super::enums::{
    ReferrerPolicy, RequestCache, RequestCredentials, RequestDestination, RequestMode,
    RequestRedirect,
};
// The macro attributes are consumed by `#[v8_class]` expansion (which
// strips them before rustc sees them), so the imports show as unused.
// The `#[v8_state_marker]` is also consumed by the impl-block-level
// strip in `mod.rs::strip_marker_attrs`. Allow the unused-imports lint
// since proc-macro attribute usage isn't visible to the compiler's
// usage tracker.
#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_getter, v8_method, v8_name, v8_state_marker, WebIdlDict,
};

/// Synthetic base URL used when `new Request(input)` receives a
/// relative URL or an empty string. Server-side runtimes (workerd,
/// Deno workers) follow the same convention since there is no
/// document.baseURI / Window.location to source the spec's "API base
/// URL" from. Matches workerd's default.
const DEFAULT_BASE_URL: &str = "http://localhost/";

// ---------------------------------------------------------------------------
// RequestState — boxed state stored in V8 internal field 0
// ---------------------------------------------------------------------------

/// The boxed Rust state for a Request wrapper. Mutable fields live in
/// `RefCell` so getter callbacks can hand out read-only views without
/// cloning, while constructor / future setter paths can mutate.
///
/// Typed enum fields (`mode`, `credentials`, `cache`, `redirect`,
/// `referrer_policy`, `destination`) live in `Cell` since the enums are
/// `Copy`. Per Fetch §5.4 these are validated at construction time —
/// the `WebIdlConvertible::from_v8` paths reject unknown values with a
/// TypeError, so by the time a value lands in the cell it's a valid
/// spec variant.
#[allow(missing_debug_implementations)]
pub struct RequestState {
    pub body: RefCell<BodyImpl>,
    pub method: RefCell<String>,
    pub url: RefCell<String>,
    /// Headers Global so re-reads of `request.headers` return the SAME
    /// JS object per Fetch §5.4 `[SameObject]`. The JS-visible getter
    /// materialises a `Local<Object>` from this Global on every call —
    /// V8 Globals are persistent handles to the same Object, so identity
    /// is preserved without any extra Private-symbol caching layer.
    ///
    /// JS-constructed Requests populate this field eagerly inside the
    /// constructor; the kernel-side fast-path builder defers V8 work and
    /// stashes the raw header list in `raw_headers` instead — the
    /// `headers` getter materialises and caches into THIS field on
    /// first access. Same-object semantics still hold because every
    /// subsequent read sees the cached Global.
    pub headers: RefCell<Option<v8::Global<v8::Object>>>,
    /// Pending raw header list set by `build_kernel_request` when the
    /// caller wants to avoid the V8 Headers wrapper allocation on
    /// procedures that never read `request.headers`. Consumed (cleared)
    /// by the first `headers` getter call (or `Body::content_type`),
    /// which materialises the wrapper and stores the Global in
    /// `headers`. Held as `Arc` so the kernel can share the same backing
    /// `Vec` it already allocated for RPC dispatch without an extra
    /// O(N) clone.
    pub raw_headers: RefCell<Option<Arc<Vec<(String, String)>>>>,
    /// AbortSignal Global. Always present per spec — `request.signal`
    /// returns a fresh signal even when the user didn't pass one. We
    /// lazily mint on first access if none was provided.
    pub signal: RefCell<Option<v8::Global<v8::Object>>>,
    /// Typed enum slots — see `enums.rs`. Validated at construction
    /// time (TypeError on unknown JS values per WebIDL §3.13.7).
    pub destination: Cell<RequestDestination>,
    pub referrer: RefCell<String>,
    pub referrer_policy: Cell<ReferrerPolicy>,
    pub mode: Cell<RequestMode>,
    pub credentials: Cell<RequestCredentials>,
    pub cache: Cell<RequestCache>,
    pub redirect: Cell<RequestRedirect>,
    pub integrity: RefCell<String>,
    pub keepalive: Cell<bool>,
    pub is_reload_navigation: Cell<bool>,
    pub is_history_navigation: Cell<bool>,
    pub duplex: RefCell<String>,
    pub priority: RefCell<String>,
}

impl Default for RequestState {
    fn default() -> Self {
        RequestState {
            body: RefCell::new(BodyImpl::null()),
            method: RefCell::new("GET".to_string()),
            url: RefCell::new(String::new()),
            headers: RefCell::new(None),
            raw_headers: RefCell::new(None),
            signal: RefCell::new(None),
            destination: Cell::new(RequestDestination::Empty),
            referrer: RefCell::new("about:client".to_string()),
            referrer_policy: Cell::new(ReferrerPolicy::Empty),
            // Constructor default per Fetch §5.4 step 14: requests
            // built from JS land on "cors". The enum's own
            // `Default::default()` is `NoCors` (the storage default
            // for new requests created without a JS constructor — only
            // reachable via the kernel-side `build_kernel_request`,
            // which doesn't observe `req.mode` via JS). The
            // constructor body sets this explicitly via the dict's
            // missing-path default; we initialise to `Cors` here so
            // the kernel-side path matches the JS-observable shape.
            mode: Cell::new(RequestMode::Cors),
            credentials: Cell::new(RequestCredentials::SameOrigin),
            cache: Cell::new(RequestCache::Default),
            redirect: Cell::new(RequestRedirect::Follow),
            integrity: RefCell::new(String::new()),
            keepalive: Cell::new(false),
            is_reload_navigation: Cell::new(false),
            is_history_navigation: Cell::new(false),
            duplex: RefCell::new("half".to_string()),
            priority: RefCell::new("auto".to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// RequestInit — `[Dictionary]` per Fetch §5.4
// ---------------------------------------------------------------------------

/// `RequestInit` per Fetch §5.4. The typed-enum members migrate from
/// the v1 `RefCell<String>` storage; the v8::Value passthroughs
/// (`headers`, `body`, `signal`) stay OUTSIDE the dict because they're
/// union types the constructor body dispatches on by V8 shape (the
/// HeadersInit union for `headers`, the BodyInit union for `body`,
/// AbortSignal-or-null for `signal`).
///
/// Critically, the dict's blanket `Option<T: WebIdlConvertible>` impl
/// collapses missing / undefined / null into `None` — but Fetch §5.4
/// distinguishes "missing init.body" (inherit from input Request) from
/// "explicit null" (use null body, do not inherit). The constructor
/// body reads `body` / `headers` / `signal` raw from the init object
/// to preserve that distinction.
///
/// # Behaviour change vs v1
///
/// Unknown enum values (`mode: "bogus"`) now throw TypeError instead
/// of being silently stored as a string. Spec-correct per WebIDL
/// §3.13.7 step 4.
#[derive(Default, Debug, WebIdlDict)]
pub(crate) struct RequestInit {
    pub method: Option<String>,
    pub mode: Option<RequestMode>,
    pub credentials: Option<RequestCredentials>,
    pub cache: Option<RequestCache>,
    pub redirect: Option<RequestRedirect>,
    pub referrer: Option<String>,
    #[webidl_name = "referrerPolicy"]
    pub referrer_policy: Option<ReferrerPolicy>,
    pub integrity: Option<String>,
    pub keepalive: Option<bool>,
    pub duplex: Option<String>,
    pub priority: Option<String>,
}

// ---------------------------------------------------------------------------
// Body trait impl
// ---------------------------------------------------------------------------

/// Marker (unit) struct — JS-class identity. The `#[v8_class]
/// #[v8_state_marker(Request)] impl RequestState` block below binds
/// `Request` to the install slot / brand check / callback names while
/// the boxed payload is `Box<RequestState>`.
pub struct Request;

impl BodyMarker for Request {
    const CLASS_LABEL: &'static str = "Request";
}

impl Body for Request {
    fn body_state<'a>(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Option<&'a BodyImpl> {
        let raw = state_ptr(scope, this)?;
        // SAFETY: pointer stable for the lifetime of the wrapper. We
        // hand out a `&BodyImpl` whose lifetime is constrained by the
        // caller (typically the duration of a single V8 callback).
        let state: &RequestState = unsafe { &*raw };
        // Borrow the RefCell read-only and project to the inner. The
        // returned reference outlives the borrow — sound only because
        // BodyImpl fields live in their own allocations (Globals,
        // Option, u64). The caller must NOT call any mutator on the
        // RefCell during use; consumers don't.
        let cell = state.body.borrow();
        let ptr: *const BodyImpl = &*cell;
        drop(cell);
        Some(unsafe { &*ptr })
    }

    fn content_type(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Option<String> {
        let raw = state_ptr(scope, this)?;
        let state: &RequestState = unsafe { &*raw };
        // Prefer the V8 Headers wrapper when already materialised — it
        // honours per-spec Set-Cookie joining and any user mutations.
        if let Some(h_global) = state.headers.borrow().as_ref().cloned() {
            return read_content_type(scope, h_global);
        }
        // Fall back to the kernel-supplied raw header list (kernel fast-
        // path Request that never had `request.headers` read by user JS).
        // Avoids materialising the Headers wrapper just to read CT.
        if let Some(raw_headers) = state.raw_headers.borrow().as_ref() {
            return read_content_type_from_raw(raw_headers.as_slice());
        }
        None
    }
}

/// Find Content-Type by case-insensitive name match in a raw header
/// list. Mirrors `read_content_type` for the lazy path where the V8
/// Headers wrapper has not been built yet — saves a V8 round trip and
/// a string allocation in the common no-CT case.
fn read_content_type_from_raw(pairs: &[(String, String)]) -> Option<String> {
    for (n, v) in pairs {
        if n.eq_ignore_ascii_case("content-type") {
            return Some(v.clone());
        }
    }
    None
}

fn read_content_type(
    scope: &mut v8::PinScope,
    headers_global: v8::Global<v8::Object>,
) -> Option<String> {
    let headers_obj = v8::Local::new(scope, headers_global);
    let key = v8::String::new(scope, "get").unwrap();
    let fn_v = headers_obj.get(scope, key.into())?;
    let fn_l: v8::Local<v8::Function> = fn_v.try_into().ok()?;
    let arg = v8::String::new(scope, "Content-Type").unwrap();
    let result = fn_l.call(scope, headers_obj.into(), &[arg.into()])?;
    if result.is_null_or_undefined() {
        return None;
    }
    Some(result.to_rust_string_lossy(scope))
}

/// Read the boxed `*mut RequestState` from V8 internal field 0.
///
/// Stays private — only the `Body` trait impl above and the in-module
/// constructor (which inspects an INPUT Request) need raw-pointer
/// access. Per design `docs/proposals/macro-v8-state.md` §7.2.1 the
/// macro doesn't auto-emit a `state_ptr` accessor: each consumer that
/// needs raw access defines its own (avoids a public unsafe API
/// surface). Outside this module the access is via the macro's brand-
/// checked methods/getters or `mod.rs::snapshot_request` (which
/// duplicates the External-deref shape inline since it's a
/// crate-private helper).
fn state_ptr(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> Option<*mut RequestState> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *mut RequestState;
    if ptr.is_null() {
        return None;
    }
    Some(ptr)
}

// ---------------------------------------------------------------------------
// V8 class — constructor, getters, clone (macro-emitted)
// ---------------------------------------------------------------------------

#[v8_class]
#[v8_state_marker(Request)]
impl RequestState {
    /// Fetch §5.4 steps 1–43. Returns by value; the macro boxes the
    /// state, installs the External in internal field 0, and registers
    /// the GC finalizer. Result mapping: `Ok(state)` → boxed wrapper;
    /// `Err(OpError)` → typed JS exception per the macro's standard
    /// OpError dispatch. Must-new is automatic — `Request("...")` (no
    /// `new`) throws `Failed to construct 'Request': Please use the
    /// 'new' operator, ...` per WebIDL §3.7.1.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        input_v: v8::Local<v8::Value>,
        init_v: v8::Local<v8::Value>,
    ) -> Result<RequestState, crate::state::OpError> {
        if input_v.is_undefined() {
            return Err(crate::state::OpError::type_error(
                "Request constructor requires an input argument",
            ));
        }

        let state = RequestState::default();
        // PENDING_CT migration (design §7.2.4). The hand-roll used a
        // thread-local HashMap keyed by the stack address of `state` to
        // defer Content-Type until headers were built. Under the macro
        // path the constructor returns `RequestState` by value and the
        // macro's `__instance` slot may live at a different address —
        // address-keyed map would miss. Since the whole state-build →
        // headers-build → CT-apply flow lives in THIS function, just
        // keep the pending CT in a local. No struct field, no thread-
        // local: cleaner than v1's two suggested options.
        let mut pending_ct: Option<String> = None;

        // Step 6: parse `input` — string or Request. Use the typed
        // `Request::is_instance` brand check.
        let input_is_request =
            input_v.is_object() && Request::is_instance(scope, input_v);

        let initial_url: String = if input_is_request {
            let req_obj: v8::Local<v8::Object> = match input_v.try_into() {
                Ok(o) => o,
                Err(_) => {
                    return Err(crate::state::OpError::type_error(
                        "Request input is not a Request",
                    ));
                }
            };
            let raw = match state_ptr(scope, req_obj) {
                Some(p) => p,
                None => {
                    return Err(crate::state::OpError::type_error(
                        "Request input is not a Request",
                    ));
                }
            };
            // SAFETY: `Request::is_instance` confirmed the prototype-chain
            // brand; internal-field-0 holds a `Box<RequestState>`.
            let other: &RequestState = unsafe { &*raw };
            // Copy over scalar fields from the input Request. Typed-enum
            // slots are `Cell::set` since the enums are `Copy`.
            *state.method.borrow_mut() = other.method.borrow().clone();
            *state.referrer.borrow_mut() = other.referrer.borrow().clone();
            state.referrer_policy.set(other.referrer_policy.get());
            state.mode.set(other.mode.get());
            state.credentials.set(other.credentials.get());
            state.cache.set(other.cache.get());
            state.redirect.set(other.redirect.get());
            *state.integrity.borrow_mut() = other.integrity.borrow().clone();
            state.keepalive.set(other.keepalive.get());
            *state.priority.borrow_mut() = other.priority.borrow().clone();
            // Body / Headers will be (potentially) overridden by init.
            // The disturbed-input-Request check (Fetch §5.4 step 36 "If
            // input is a Request and inputBody is non-null and inputBody
            // is a body whose stream is disturbed, throw a TypeError")
            // moves AFTER we know whether init.body provides an override.
            // If init.body is set, we use that and don't inherit the
            // input's body — so a disturbed input is fine.
            other.url.borrow().clone()
        } else {
            // String input. Per Fetch §5.4 step 6: parse input against entry
            // settings object's API base URL. In a server-side runtime we
            // don't have a document or a worker location; we follow the
            // workerd convention of using `http://localhost/` as the
            // synthetic API base URL so that:
            //   - Empty string and relative URLs resolve (per spec they
            //     resolve against the base URL, not fail).
            //   - Absolute URLs short-circuit and use their own scheme.
            // ada-url tries absolute-parse first; if that fails it falls
            // back to base-relative parsing. We prefer absolute parse
            // explicitly to keep the resulting href closer to user input
            // when possible.
            let url_str = input_v.to_rust_string_lossy(scope);
            let parsed = ada_url::Url::parse(&url_str, None)
                .or_else(|_| ada_url::Url::parse(&url_str, Some(DEFAULT_BASE_URL)));
            match parsed {
                Ok(u) => u.href().to_string(),
                Err(_) => {
                    return Err(crate::state::OpError::type_error(format!(
                        "Failed to parse URL: {url_str}"
                    )));
                }
            }
        };

        *state.url.borrow_mut() = initial_url;

        // Step 12+: parse `init` via the WebIdlDict reader. This is the
        // single point where unknown enum values throw TypeError per
        // WebIDL §3.13.7 (e.g. `mode: "bogus"`). The dict carries only
        // the typed members; the v8::Value passthroughs `headers` /
        // `body` / `signal` are read separately below to preserve the
        // explicit-null vs missing distinction (the dict's
        // `Option<v8::Local<...>>` blanket collapses both to `None`).
        let init_dict = RequestInit::from_v8(scope, init_v)?;
        // Whether init is an actual JS Object (vs null/undefined). The
        // duplex-on-stream-body check (Fetch §5.4 step 36) cares about
        // "init['duplex'] does not exist" — when init itself is missing
        // there's no dict to consult, so we suppress the check.
        let init_obj: Option<v8::Local<v8::Object>> = if init_v.is_null_or_undefined() {
            None
        } else {
            v8::Local::<v8::Object>::try_from(init_v).ok()
        };

        // Method — normalize per §4.3 (uppercase standard methods,
        // forbidden CONNECT/TRACE/TRACK, RFC 9110 token validation).
        if let Some(raw_method) = init_dict.method.as_deref() {
            let normalized = match normalize_method(raw_method) {
                Ok(m) => m,
                Err(e) => return Err(crate::state::OpError::type_error(e)),
            };
            *state.method.borrow_mut() = normalized;
        }

        // Apply scalar / typed-enum init overrides. Each `Some` value
        // wins over the inherited / default; `None` (missing key) leaves
        // the slot at whatever was set above (input-Request copy or
        // `RequestState::default()`).
        if let Some(referrer) = init_dict.referrer.clone() {
            *state.referrer.borrow_mut() = referrer;
        }
        if let Some(rp) = init_dict.referrer_policy {
            state.referrer_policy.set(rp);
        }
        if let Some(mode) = init_dict.mode {
            state.mode.set(mode);
        }
        if let Some(creds) = init_dict.credentials {
            state.credentials.set(creds);
        }
        if let Some(cache) = init_dict.cache {
            state.cache.set(cache);
        }
        if let Some(redirect) = init_dict.redirect {
            state.redirect.set(redirect);
        }
        if let Some(integrity) = init_dict.integrity.clone() {
            *state.integrity.borrow_mut() = integrity;
        }
        if let Some(duplex) = init_dict.duplex.clone() {
            *state.duplex.borrow_mut() = duplex;
        }
        if let Some(priority) = init_dict.priority.clone() {
            *state.priority.borrow_mut() = priority;
        }
        if let Some(keepalive) = init_dict.keepalive {
            state.keepalive.set(keepalive);
        }
        // Per Fetch (Chrome/Deno/etc.): `duplex: "full"` is not yet
        // supported — implementations throw TypeError. We match that
        // behaviour. WPT request-init-stream.any.js explicitly
        // requires this for any body shape (null, string, Uint8Array,
        // ReadableStream) when duplex is "full".
        if state.duplex.borrow().as_str() == "full" {
            return Err(crate::state::OpError::type_error(
                "Request init.duplex = 'full' is not supported",
            ));
        }

        // Body extraction. Step 35–36. Read raw from init_obj so that
        // explicit `body: null` is distinguished from missing (the
        // former skips input-Request inheritance per spec).
        let body_v: Option<v8::Local<v8::Value>> =
            init_obj.and_then(|o| get_raw_init(scope, o, "body"));

        // If init.body is missing AND input was a Request, inherit the
        // input's body. Per Fetch §5.4 step 36 + step 42 ("clone a body"):
        // the new request's body is a CLONE of the input's body — a fresh
        // body whose stream is independent of the input's.
        //
        // We delay both the actual cloning AND the input-disturb marker
        // until AFTER all input validation succeeds (per WPT
        // request-disturbed.any.js "Request construction failure should
        // not set bodyUsed"). For now record only that we should perform
        // an inherit clone; carry forward the disturbed-check error.
        enum InheritMode {
            None,
            BytesSource(std::rc::Rc<Vec<u8>>),
            StreamSource,
        }
        let inherit_mode: InheritMode = if body_v.is_none() && input_is_request {
            let req_obj: v8::Local<v8::Object> = input_v.try_into().unwrap();
            let raw = state_ptr(scope, req_obj).unwrap();
            // SAFETY: input was brand-checked above.
            let other: &RequestState = unsafe { &*raw };
            let other_stream_g = other.body.borrow().stream.borrow().clone();
            let other_source = other.body.borrow().source.clone();

            if let Some(stream_g) = other_stream_g.as_ref() {
                let stream = v8::Local::new(scope, stream_g.clone());
                if crate::fetch_body::consumers::stream_disturbed_or_used(scope, req_obj, stream) {
                    return Err(crate::state::OpError::type_error(
                        "Cannot construct Request from a disturbed Request",
                    ));
                }
            }
            match other_source {
                Some(crate::fetch_body::body::BodySource::Bytes(rc))
                | Some(crate::fetch_body::body::BodySource::Blob(rc, _))
                | Some(crate::fetch_body::body::BodySource::UrlSearchParams(rc))
                | Some(crate::fetch_body::body::BodySource::FormData(rc, _)) => {
                    InheritMode::BytesSource(rc)
                }
                Some(crate::fetch_body::body::BodySource::Stream) => InheritMode::StreamSource,
                None => InheritMode::None,
            }
        } else {
            InheritMode::None
        };

        // For the GET/HEAD/forbidden-method check below to fire BEFORE we
        // disturb input, the inherited body value here is ONLY the
        // disturbing-not-yet-applied marker. The actual stream construction
        // happens after validation succeeds.
        //
        // We synthesise a non-null sentinel (a fresh empty stream is
        // overkill, so we use null but track the mode separately). The
        // GET/HEAD validation needs to know SOMETHING is there → use a
        // null + mode-driven inherit applied later.
        let has_inherited_body = !matches!(inherit_mode, InheritMode::None);

        let body_input: Option<v8::Local<v8::Value>> = body_v;

        // GET/HEAD body-presence check (Fetch §5.4 step 35.5). Fires for
        // BOTH explicit init.body AND inherited body. This must run BEFORE
        // any body extraction / disturb-marker — per WPT
        // request-disturbed.any.js "Request construction failure should
        // not set bodyUsed".
        let has_explicit_body = body_input.is_some_and(|b| !b.is_null_or_undefined());
        if has_explicit_body || has_inherited_body {
            let method = state.method.borrow().clone();
            if method == "GET" || method == "HEAD" {
                return Err(crate::state::OpError::type_error(
                    "Request with GET/HEAD method cannot have body",
                ));
            }
        }

        // Process explicit init.body if any.
        if let Some(b) = body_input {
            if !b.is_null_or_undefined() {
                // Per Fetch §5.4 step 36: when body is a ReadableStream,
                // init["duplex"] must exist (since the body is half-duplex
                // by default — full-duplex is opt-in). The spec's exact
                // wording: "If body is a ReadableStream and init["duplex"]
                // does not exist, throw a TypeError."
                //
                // We match Chrome / Deno here: only validate when body is
                // a ReadableStream. URLSearchParams / Blob / etc. don't
                // need duplex.
                if init_obj.is_some() {
                    let body_is_stream = if let Ok(obj) = v8::Local::<v8::Object>::try_from(b) {
                        is_readable_stream_global_instance(scope, obj)
                    } else {
                        false
                    };
                    if body_is_stream && init_dict.duplex.is_none() {
                        return Err(crate::state::OpError::type_error(
                            "Request with ReadableStream body requires init.duplex = 'half'",
                        ));
                    }
                }
                let keepalive = state.keepalive.get();
                match extract_body(scope, b, keepalive) {
                    Ok(extracted) => {
                        *state.body.borrow_mut() = extracted.body;
                        if let Some(ct) = extracted.content_type {
                            // Stash for later: we'll set on Headers after
                            // the headers init step, but only if the user
                            // didn't already set one. PENDING_CT lives as
                            // a local variable rather than a thread-local
                            // (see top-of-fn comment).
                            pending_ct = Some(ct);
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        // Apply inherited body from input Request (if init.body wasn't
        // provided). At this point all validation has succeeded, so
        // disturbing the input is safe. Per Fetch §5.4 step 42 ("If
        // initBody is null and inputBody is non-null, set finalBody to the
        // result of cloning inputBody.") — clone via body-source rebuild
        // (preserves input's stream identity for byte sources) or tee
        // (for true Stream sources).
        if !has_explicit_body {
            if let InheritMode::BytesSource(rc) = &inherit_mode {
                // FIX B: defer stream construction. The body getter
                // builds a ReadableStream lazily from the source.
                let length = Some(rc.len() as u64);
                *state.body.borrow_mut() = crate::fetch_body::body::BodyImpl {
                    stream: std::cell::RefCell::new(None),
                    source: Some(crate::fetch_body::body::BodySource::Bytes(rc.clone())),
                    length,
                };
            } else if let InheritMode::StreamSource = &inherit_mode {
                // Tee the input's stream; replace input's stream with
                // branch[0] (it remains in input's body slot but is now
                // tee'd-locked); use branch[1] as the new request's body.
                let req_obj: v8::Local<v8::Object> = input_v.try_into().unwrap();
                let raw = state_ptr(scope, req_obj).unwrap();
                // SAFETY: input was brand-checked above.
                let other: &RequestState = unsafe { &*raw };
                let other_stream_g = other.body.borrow().stream.borrow().clone();
                if let Some(stream_g) = other_stream_g {
                    let stream_local = v8::Local::new(scope, stream_g);
                    if let Some((branch_a, branch_b)) = tee_stream(scope, stream_local) {
                        *other.body.borrow().stream.borrow_mut() = Some(v8::Global::new(scope, branch_a));
                        *state.body.borrow_mut() = crate::fetch_body::body::BodyImpl {
                            stream: std::cell::RefCell::new(Some(v8::Global::new(scope, branch_b))),
                            source: Some(crate::fetch_body::body::BodySource::Stream),
                            length: None,
                        };
                    }
                }
            }
        }

        // Per Fetch §5.4 step 42 + WPT request-disturbed.any.js: if input
        // is a Request with a non-null body, the input is marked body-used
        // regardless of whether init.body overrode the body. The test
        // "Input request used for creating new request became disturbed
        // even if body is not used" confirms this: even when init.body is
        // provided, the input request becomes disturbed.
        //
        // Fire only on construction success (we only reach here past all
        // validation throws — per WPT "Request construction failure should
        // not set bodyUsed").
        if input_is_request {
            let req_obj: v8::Local<v8::Object> = input_v.try_into().unwrap();
            let raw = state_ptr(scope, req_obj).unwrap();
            // SAFETY: input was brand-checked above.
            let other: &RequestState = unsafe { &*raw };
            // Body is non-null when the stream is materialized OR a
            // rewindable source is present (FIX B lazy-stream path).
            let body_present = {
                let body = other.body.borrow();
                body.stream.borrow().is_some() || body.source.is_some()
            };
            if body_present {
                crate::fetch_body::consumers::set_body_used_marker(scope, req_obj);
            }
        }

        // Headers — build / inherit. Per Fetch §5.4 step 32:
        //   1. Let headers be a copy of this's headers.
        //   2. If init["headers"] exists, then [...] fill from init.
        let init_headers_v: Option<v8::Local<v8::Value>> =
            init_obj.and_then(|o| get_raw_init(scope, o, "headers"));
        let headers_obj =
            match build_request_headers(scope, init_headers_v, input_is_request, input_v) {
                Ok(h) => h,
                Err(msg) => return Err(crate::state::OpError::type_error(msg)),
            };

        // Apply pending Content-Type (set on the body but only if the user
        // didn't already provide one on init.headers). PENDING_CT now lives
        // as the local `pending_ct` variable above; apply it inline.
        if let Some(ct) = pending_ct.take() {
            apply_content_type_if_absent(scope, headers_obj, &ct);
        }

        *state.headers.borrow_mut() = Some(v8::Global::new(scope, headers_obj));

        // Signal: chain `init.signal` if provided. Always mint a fresh
        // signal so `request.signal` is non-null per Fetch §5.4.
        let init_signal_v: Option<v8::Local<v8::Value>> =
            init_obj.and_then(|o| get_raw_init(scope, o, "signal"));
        let signal_obj = build_request_signal(scope, init_signal_v);
        *state.signal.borrow_mut() = Some(v8::Global::new(scope, signal_obj));

        // The macro's emitted callback boxes `state`, installs the box
        // pointer in V8 internal field 0, registers the GC finalizer,
        // and returns the wrapper to JS. No manual `Box::new` here.
        Ok(state)
    }

    // ---------------------------------------------------------------------
    // Simple string getters (15)
    // ---------------------------------------------------------------------

    #[v8_getter]
    fn method(&self) -> String {
        self.method.borrow().clone()
    }

    #[v8_getter]
    fn url(&self) -> String {
        self.url.borrow().clone()
    }

    #[v8_getter]
    fn destination(&self) -> String {
        self.destination.get().as_str().to_string()
    }

    #[v8_getter]
    fn referrer(&self) -> String {
        self.referrer.borrow().clone()
    }

    #[v8_getter]
    #[v8_name = "referrerPolicy"]
    fn referrer_policy(&self) -> String {
        self.referrer_policy.get().as_str().to_string()
    }

    #[v8_getter]
    fn mode(&self) -> String {
        self.mode.get().as_str().to_string()
    }

    #[v8_getter]
    fn credentials(&self) -> String {
        self.credentials.get().as_str().to_string()
    }

    #[v8_getter]
    fn cache(&self) -> String {
        self.cache.get().as_str().to_string()
    }

    #[v8_getter]
    fn redirect(&self) -> String {
        self.redirect.get().as_str().to_string()
    }

    #[v8_getter]
    fn integrity(&self) -> String {
        self.integrity.borrow().clone()
    }

    #[v8_getter]
    fn keepalive(&self) -> bool {
        self.keepalive.get()
    }

    #[v8_getter]
    #[v8_name = "isReloadNavigation"]
    fn is_reload_navigation(&self) -> bool {
        self.is_reload_navigation.get()
    }

    #[v8_getter]
    #[v8_name = "isHistoryNavigation"]
    fn is_history_navigation(&self) -> bool {
        self.is_history_navigation.get()
    }

    #[v8_getter]
    fn duplex(&self) -> String {
        self.duplex.borrow().clone()
    }

    #[v8_getter]
    fn priority(&self) -> String {
        self.priority.borrow().clone()
    }

    // ---------------------------------------------------------------------
    // headers / signal — Global-projection getters
    // ---------------------------------------------------------------------
    //
    // WebIDL §3.7.5 `[SameObject]` requires `req.headers === req.headers`.
    // We satisfy it without the macro's `#[v8_getter(same_object)]` cache:
    // `state.headers` is a `Global<Object>` cache — populated eagerly
    // by the JS constructor and lazily by this getter on the first
    // `request.headers` read of a kernel-fast-path Request (where
    // `raw_headers` carries the unmaterialised list). V8 Globals are
    // persistent handles, so `Local::new(scope, g)` returns the same
    // Object identity on every call. The macro's Private-symbol cache
    // would be a redundant indirection on a stable identity — measured
    // ~6% slower on the httpGet kernel-fast-path bench.

    #[v8_getter]
    fn headers<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Object> {
        if let Some(g) = self.headers.borrow().as_ref() {
            return v8::Local::new(scope, g);
        }
        // Lazy mint from the kernel-supplied raw header list. The
        // kernel fast-path Request builder defers `build_kernel_headers`
        // here — procedures that never read `request.headers` skip the
        // V8 Headers wrapper allocation entirely. Per `[SameObject]` in
        // Fetch §5.4 we cache the resulting Global so subsequent reads
        // return the identical wrapper.
        let raw = self.raw_headers.borrow_mut().take();
        let headers_obj = match raw {
            Some(arc) => crate::headers::build_kernel_headers(scope, arc.as_slice())
                .unwrap_or_else(|| empty_headers(scope)),
            // Defensive: no raw list and no cache (e.g. RequestState
            // built via Default::default() for testing). Mint empty.
            None => empty_headers(scope),
        };
        let g = v8::Global::new(scope, headers_obj);
        *self.headers.borrow_mut() = Some(g);
        // Re-borrow to materialise the Local — borrow_mut already dropped.
        let stash = self.headers.borrow();
        v8::Local::new(scope, stash.as_ref().unwrap())
    }

    #[v8_getter]
    fn signal<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Object> {
        // Fast path: signal already minted.
        if let Some(g) = self.signal.borrow().as_ref() {
            return v8::Local::new(scope, g);
        }
        // Lazy-mint per Fetch §5.4: `request.signal` MUST always return a
        // non-null AbortSignal, even when the kernel-side fast-path Request
        // builder didn't supply one (most server-side requests don't have
        // an upstream cancellation signal — minting on demand is observably
        // identical to constructor-minting).
        let signal_obj = build_request_signal(scope, None);
        let signal_g = v8::Global::new(scope, signal_obj);
        *self.signal.borrow_mut() = Some(signal_g);
        let stash = self.signal.borrow();
        v8::Local::new(scope, stash.as_ref().unwrap())
    }

    // ---------------------------------------------------------------------
    // clone()
    // ---------------------------------------------------------------------

    /// Fetch §5.4 `clone()`. Builds a fresh Request via the public
    /// constructor (URL parser + signal mint) but tees the body's
    /// stream (or rebuilds from its rewindable source) so the
    /// original remains usable. The synthetic `wrapper: Local<Object>`
    /// param is bound by the macro to `args.this()` (per
    /// `helpers::is_wrapper_local`) — needed for the body-used Private
    /// symbol check in `stream_disturbed_or_used`.
    #[v8_method]
    fn clone<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        wrapper: v8::Local<v8::Object>,
    ) -> Result<v8::Local<'s, v8::Object>, crate::state::OpError> {
        // Disturbed body → TypeError. Use both the locked-stream check
        // AND the wrapper's body-used marker (which fires when a consumer
        // started but the stream auto-released its lock).
        let stream_global_opt = self.body.borrow().stream.borrow().clone();
        if let Some(stream_g) = &stream_global_opt {
            let stream = v8::Local::new(scope, stream_g.clone());
            if crate::fetch_body::consumers::stream_disturbed_or_used(scope, wrapper, stream) {
                return Err(crate::state::OpError::type_error(
                    "Cannot clone a disturbed Request",
                ));
            }
        }

        // Build the clone WITHOUT going through `new Request(this, ...)`.
        // The constructor's "transfer body" step (per Fetch §5.4 step 36)
        // would disturb the original — we don't want that for clone(),
        // since the spec's `clone()` algorithm preserves the original's
        // body usability. So we build a fresh Request instance, copy
        // scalar fields from `this`, and tee or rebuild the body.
        let global = scope.get_current_context().global(scope);
        let req_class_key = v8::String::new(scope, "Request").unwrap();
        let req_class_v = global.get(scope, req_class_key.into()).unwrap();
        let req_class_fn: v8::Local<v8::Function> = req_class_v.try_into().unwrap();

        let body_is_stream = matches!(
            self.body.borrow().source,
            Some(crate::fetch_body::body::BodySource::Stream)
        ) && self.body.borrow().stream.borrow().is_some();

        // Tee the stream so original + clone share both halves and remain
        // independently consumable.
        let (left_branch, right_branch) = if body_is_stream {
            let stream_g = self.body.borrow().stream.borrow().clone().unwrap();
            let stream = v8::Local::new(scope, stream_g);
            match tee_stream(scope, stream) {
                Some(pair) => (Some(pair.0), Some(pair.1)),
                None => {
                    return Err(crate::state::OpError::type_error(
                        "Failed to tee Request body",
                    ));
                }
            }
        } else {
            (None, None)
        };

        // Build init that passes the URL via the constructor's URL parser
        // and the cloned body / headers. We include `duplex: "half"`
        // unconditionally — the constructor's duplex check fires for any
        // ReadableStream body, and we may pass a tee'd stream below.
        let init = v8::Object::new(scope);
        {
            let key = v8::String::new(scope, "method").unwrap();
            let v = v8::String::new(scope, &self.method.borrow()).unwrap();
            init.set(scope, key.into(), v.into());
        }
        {
            let key = v8::String::new(scope, "duplex").unwrap();
            let v = v8::String::new(scope, "half").unwrap();
            init.set(scope, key.into(), v.into());
        }
        // If the V8 Headers wrapper hasn't been materialised yet (lazy
        // kernel path), build one from raw_headers so the clone gets a
        // copy of the kernel-supplied list.
        if self.headers.borrow().is_none() {
            if let Some(arc) = self.raw_headers.borrow_mut().take() {
                if let Some(h_obj) =
                    crate::headers::build_kernel_headers(scope, arc.as_slice())
                {
                    *self.headers.borrow_mut() =
                        Some(v8::Global::new(scope, h_obj));
                }
            }
        }
        if let Some(h_g) = self.headers.borrow().clone() {
            let h_local = v8::Local::new(scope, h_g);
            let key = v8::String::new(scope, "headers").unwrap();
            init.set(scope, key.into(), h_local.into());
        }
        if let Some(rb) = right_branch {
            let key = v8::String::new(scope, "body").unwrap();
            init.set(scope, key.into(), rb.into());
            if let Some(lb) = left_branch {
                *self.body.borrow().stream.borrow_mut() = Some(v8::Global::new(scope, lb));
            }
        } else if let Some(src) = self.body.borrow().source.clone() {
            match src {
                crate::fetch_body::body::BodySource::Bytes(rc)
                | crate::fetch_body::body::BodySource::Blob(rc, _)
                | crate::fetch_body::body::BodySource::UrlSearchParams(rc)
                | crate::fetch_body::body::BodySource::FormData(rc, _) => {
                    let new_stream = crate::fetch_body::extract::build_byte_stream(scope, rc);
                    let stream_local = v8::Local::new(scope, new_stream);
                    let key = v8::String::new(scope, "body").unwrap();
                    init.set(scope, key.into(), stream_local.into());
                }
                crate::fetch_body::body::BodySource::Stream => {}
            }
        }

        // Pass URL as a string input (NOT `this` — that would trigger the
        // constructor's input-Request copy path which disturbs the input).
        let url_str = v8::String::new(scope, &self.url.borrow()).unwrap();
        let args2 = [url_str.into(), init.into()];
        match req_class_fn.new_instance(scope, &args2) {
            Some(o) => Ok(o),
            None => {
                // Inner constructor already set a pending exception on
                // scope; surfacing our own would mask the cause. The
                // original hand-roll let the pending exception propagate
                // (returning early without rv.set). The macro path can't
                // express that cleanly, so we return Err with a generic
                // message — V8 picks up the most-recent throw, which
                // happens to be the original constructor's. WPT
                // request-clone tests don't distinguish error identity.
                Err(crate::state::OpError::type_error(
                    "Failed to clone Request",
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Class wiring — install_global wrapper around macro emission
// ---------------------------------------------------------------------------

/// Per-isolate cache of the Request prototype Object for the kernel
/// fast-path Request builder.
///
/// Replaces the v1 `RequestTemplateSlot.prototype` field. The class
/// FunctionTemplate now lives in the macro-emitted
/// `__InstallSlot_Request` slot; the prototype Object is not in that
/// slot, so we cache it separately here.
///
/// Populated lazily on the first `build_kernel_request` call — once
/// the prototype is materialised, the slot lookup is one Rc-clone-shaped
/// hop. Per design `docs/proposals/macro-v8-state.md` §7.2.1.
pub struct RequestPrototypeSlot(pub v8::Global<v8::Object>);

/// Install Request on `globalThis`. Wraps the macro-emitted
/// `Request::install` with:
///   1. Installing `Request` as a global function via
///      `tmpl.get_function(scope)`.
///   2. Installing the six body-trait methods (text/json/arrayBuffer/
///      bytes/blob/formData) on the prototype via
///      `install_body_methods::<Request>` — these are NOT routed through
///      the macro because the Body trait is a Rust-side dispatch the
///      macro has no awareness of.
pub fn install_global<'s>(scope: &mut v8::PinScope<'s, '_>, global: v8::Local<v8::Object>) {
    // The macro emits `Request::install(scope) -> Local<FunctionTemplate>`
    // which sets up the `__InstallSlot_Request` slot on first call and
    // returns the cached template on subsequent calls. Idempotent.
    let class_tmpl = Request::install(scope);
    let class_fn = class_tmpl.get_function(scope).unwrap();

    // Reach the prototype to install Body trait methods on top of the
    // macro-emitted prototype.
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    // The Body trait dispatch is shared with Response — text/json/
    // arrayBuffer/bytes/blob/formData and the body/bodyUsed accessors.
    // All read internal field 0 via the trait's `body_state(scope, this)`
    // projection, which casts to `*mut RequestState`. Same cast type
    // as the macro's emitted callbacks, so zero conflict.
    install_body_methods::<Request>(scope, proto);

    let key = v8::String::new(scope, "Request").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

// ---------------------------------------------------------------------------
// Kernel fast-path Request builder
// ---------------------------------------------------------------------------

/// Build a Request directly from raw HTTP wire data — bypasses the
/// WebIDL §5.4 constructor entirely. Used by the kernel's fetch
/// dispatch (`runtime.rs::call_fetch_handler`) instead of the JS
/// helper `HTTP_CREATE_REQUEST_JS`.
///
/// What we skip relative to the spec constructor:
///   - `globalThis.Request` lookup (template comes from the macro's
///     `__InstallSlot_Request`; prototype from `RequestPrototypeSlot`,
///     populated on first call).
///   - URL re-parse (the upstream gateway already gave us a clean URL).
///     We trust it verbatim; the Request's `url` getter reads it back.
///   - WebIDL union dispatch on `init` (we know exactly what we have).
///   - Method normalization (already done by the HTTP parser).
///   - Body extraction (`extract_body` walks every accepted body type
///     for the public surface — Blob / FormData / URLSearchParams /
///     ReadableStream — none of which apply for raw HTTP wire data).
///   - AbortSignal minting (the kernel's fetch dispatch doesn't need
///     a signal on the request; we lazy-mint on first `request.signal`
///     read by storing `None`. Per Fetch §5.4 a fresh signal MUST be
///     returned, so the `signal` getter falls back to building one on
///     demand).
///   - JSON-marshaled headers (the kernel hands us the list directly).
///
/// What we keep:
///   - The same wrapper shape (internal field 0 = Box<RequestState>),
///     so all downstream code (getters, `inspect_response`, `Body`
///     trait dispatch) stays unchanged.
///   - The same finalizer wiring (drops the Box on V8 GC of the
///     wrapper).
pub fn build_kernel_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Option<v8::Local<'s, v8::Object>> {
    // 1. Resolve the FunctionTemplate from the macro's install slot.
    //    Set on the first call to `Request::install` in this isolate
    //    (i.e. setup_globals, before any user JS runs).
    let req_tmpl_g = scope.get_slot::<__InstallSlot_Request>()?.0.clone();
    let req_tmpl = v8::Local::new(scope, req_tmpl_g);
    let inst_tmpl = req_tmpl.instance_template(scope);
    let this_obj = inst_tmpl.new_instance(scope)?;

    // 2. Resolve the prototype Object — cached lazily on first call.
    //    Per design `docs/proposals/macro-v8-state.md` §7.2.1: 3-V8-
    //    call walk the first time, slot-read steady state.
    let req_proto_g = match scope.get_slot::<RequestPrototypeSlot>() {
        Some(s) => s.0.clone(),
        None => {
            let func = req_tmpl.get_function(scope)?;
            let key = v8::String::new(scope, "prototype")?;
            let proto: v8::Local<v8::Object> = func.get(scope, key.into())?.try_into().ok()?;
            let g = v8::Global::new(scope, proto);
            scope.set_slot(RequestPrototypeSlot(g.clone()));
            g
        }
    };
    let req_proto = v8::Local::new(scope, req_proto_g);
    this_obj.set_prototype(scope, req_proto.into());

    // 3. Defer Headers wrapper construction. We stash the raw header
    //    list in `raw_headers` and let the `headers` getter materialise
    //    a native Headers wrapper on first access. Procedures that
    //    never read `request.headers` skip the V8 allocation + Vec
    //    clone entirely. Per Fetch §5.4 `[SameObject]` the getter
    //    caches the materialised Global so identity is preserved across
    //    reads.
    let raw_headers = Arc::new(headers.to_vec());

    // 4. Build the body. For wire HTTP: GET/HEAD have no body; for
    // other methods, preserve the raw request bytes. We use the
    // BodySource::Bytes path so consumer methods (`text` / `json` /
    // etc.) can short-circuit without materializing a stream.
    let body_impl = if body.is_empty() || method == "GET" || method == "HEAD" {
        crate::fetch_body::body::BodyImpl::null()
    } else {
        let bytes = std::rc::Rc::new(body.to_vec());
        let length = Some(bytes.len() as u64);
        crate::fetch_body::body::BodyImpl {
            stream: std::cell::RefCell::new(None),
            source: Some(crate::fetch_body::body::BodySource::Bytes(bytes)),
            length,
        }
    };

    // 5. Build the RequestState. Method/URL go in verbatim; all other
    // fields keep their spec defaults from RequestState::default().
    let state = RequestState {
        body: RefCell::new(body_impl),
        method: RefCell::new(method.to_string()),
        url: RefCell::new(url.to_string()),
        headers: RefCell::new(None),
        raw_headers: RefCell::new(Some(raw_headers)),
        signal: RefCell::new(None),
        ..RequestState::default()
    };

    // 6. Box, install in internal field 0, register finalizer. The
    //    finalizer's drop type is `RequestState` to match the box
    //    payload type — same shape as the macro's emitted finalizer
    //    in `gen_box_and_install_finalizer`.
    let boxed = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    this_obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        this_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut RequestState));
        }),
    );
    std::mem::forget(weak);

    Some(this_obj)
}

// ---------------------------------------------------------------------------
// Helpers — JS-side dispatch utilities (not routed through the macro)
// ---------------------------------------------------------------------------

/// True iff `obj instanceof globalThis.ReadableStream`. Used for the
/// duplex-validation step (Fetch §5.4 step 36) — needs the same
/// discriminator as fetch_body::extract::is_readable_stream_instance.
fn is_readable_stream_global_instance(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> bool {
    let global = scope.get_current_context().global(scope);
    let key = match v8::String::new(scope, "ReadableStream") {
        Some(k) => k,
        None => return false,
    };
    let Some(class_v) = global.get(scope, key.into()) else {
        return false;
    };
    let Ok(class_obj) = v8::Local::<v8::Object>::try_from(class_v) else {
        return false;
    };
    obj.instance_of(scope, class_obj).unwrap_or(false)
}

/// Read a single property from the init object. Returns `None` for
/// missing keys AND undefined values (matching the WebIDL §3.10
/// "missing dictionary member" path), but `Some(v)` for explicit null.
/// The constructor body uses this distinction for the `body` /
/// `headers` / `signal` raw passthroughs — explicit null suppresses
/// the input-Request inheritance per Fetch §5.4 step 36.
fn get_raw_init<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    init: v8::Local<v8::Object>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let key = v8::String::new(scope, name)?;
    let v = init.get(scope, key.into())?;
    if v.is_undefined() {
        return None;
    }
    Some(v)
}

/// Per Fetch §4.3 "method" + §5.4 step 25:
///   1. If method is one of `CONNECT`, `TRACE`, `TRACK` (case-
///      insensitive), throw TypeError.
///   2. If method is one of the standard methods (DELETE, GET, HEAD,
///      OPTIONS, POST, PUT) case-insensitively, return the upper-case
///      form.
///   3. Otherwise, return method as-is (case-preserved per spec).
fn normalize_method(method: &str) -> Result<String, String> {
    let upper = method.to_ascii_uppercase();
    match upper.as_str() {
        "CONNECT" | "TRACE" | "TRACK" => {
            Err(format!("'{method}' HTTP method is forbidden"))
        }
        "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT" => Ok(upper),
        _ => {
            // Validate as a token (RFC 9110 §5.6.2).
            if method.is_empty() || !method.bytes().all(is_method_token) {
                return Err(format!("'{method}' is not a valid HTTP method"));
            }
            Ok(method.to_string())
        }
    }
}

fn is_method_token(b: u8) -> bool {
    matches!(
        b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
            | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'
    )
}

// ---------------------------------------------------------------------------
// Body / Content-Type plumbing
// ---------------------------------------------------------------------------

/// Apply a body-derived Content-Type to the request's headers, but only
/// if no Content-Type was already set (e.g. via `init.headers`). Spec
/// per Fetch §5.4 step 36.6.
///
/// Replaces the v1 thread-local PENDING_CT scheme. The macro path keeps
/// the constructor body monolithic, so we pass the pending CT through
/// the local stack rather than a thread-local table keyed by state
/// address.
fn apply_content_type_if_absent(
    scope: &mut v8::PinScope,
    headers_obj: v8::Local<v8::Object>,
    ct: &str,
) {
    // Only set Content-Type if not already present.
    let has_key = v8::String::new(scope, "has").unwrap();
    let has_v = match headers_obj.get(scope, has_key.into()) {
        Some(v) => v,
        None => return,
    };
    let has_fn: v8::Local<v8::Function> = match has_v.try_into() {
        Ok(f) => f,
        Err(_) => return,
    };
    let arg = v8::String::new(scope, "Content-Type").unwrap();
    let exists = match has_fn.call(scope, headers_obj.into(), &[arg.into()]) {
        Some(v) => v.boolean_value(scope),
        None => false,
    };
    if exists {
        return;
    }
    // headers.set("Content-Type", ct)
    let set_key = v8::String::new(scope, "set").unwrap();
    let set_v = match headers_obj.get(scope, set_key.into()) {
        Some(v) => v,
        None => return,
    };
    let set_fn: v8::Local<v8::Function> = match set_v.try_into() {
        Ok(f) => f,
        Err(_) => return,
    };
    let n = v8::String::new(scope, "Content-Type").unwrap();
    let v = v8::String::new(scope, ct).unwrap();
    let _ = set_fn.call(scope, headers_obj.into(), &[n.into(), v.into()]);
}

// ---------------------------------------------------------------------------
// Headers + Signal builders
// ---------------------------------------------------------------------------

fn build_request_headers<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    init_headers: Option<v8::Local<v8::Value>>,
    input_is_request: bool,
    input_v: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Object>, String> {
    let global = scope.get_current_context().global(scope);
    let headers_class_key = v8::String::new(scope, "Headers").unwrap();
    let class_v = global
        .get(scope, headers_class_key.into())
        .ok_or_else(|| "Headers class missing".to_string())?;
    let class_fn: v8::Local<v8::Function> = class_v
        .try_into()
        .map_err(|_| "Headers is not a function".to_string())?;

    // Determine the init for the new Headers:
    //   - If init.headers is present, use that.
    //   - Else if input is a Request, copy headers from there.
    //   - Else empty.
    let headers_init: v8::Local<v8::Value> = if let Some(v) = init_headers {
        v
    } else if input_is_request {
        let req_obj: v8::Local<v8::Object> = input_v.try_into().unwrap();
        let raw = state_ptr(scope, req_obj).ok_or_else(|| "Request input invalid".to_string())?;
        // SAFETY: caller already brand-checked input_v.
        let other: &RequestState = unsafe { &*raw };
        // If the input's V8 Headers wrapper is already materialised, copy
        // it directly. Otherwise mint one from the kernel-supplied raw
        // header list and cache it on the input so future reads stay
        // identity-stable per [SameObject].
        if other.headers.borrow().is_none() {
            if let Some(arc) = other.raw_headers.borrow_mut().take() {
                if let Some(h_obj) =
                    crate::headers::build_kernel_headers(scope, arc.as_slice())
                {
                    *other.headers.borrow_mut() =
                        Some(v8::Global::new(scope, h_obj));
                }
            }
        }
        match other.headers.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()).into(),
            None => v8::undefined(scope).into(),
        }
    } else {
        v8::undefined(scope).into()
    };

    let args = [headers_init];
    let h = class_fn
        .new_instance(scope, &args)
        .ok_or_else(|| "Headers constructor failed".to_string())?;
    Ok(h)
}

fn empty_headers<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Object> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "Headers").unwrap();
    let class_v = global.get(scope, key.into()).expect("Headers missing");
    let class_fn: v8::Local<v8::Function> = class_v.try_into().unwrap();
    class_fn
        .new_instance(scope, &[])
        .expect("new Headers failed")
}

fn build_request_signal<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    init_signal: Option<v8::Local<v8::Value>>,
) -> v8::Local<'s, v8::Object> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "AbortSignal").unwrap();
    let class_v = global.get(scope, key.into()).expect("AbortSignal missing");
    let class_obj: v8::Local<v8::Object> = class_v.try_into().unwrap();

    // If init.signal is provided, run AbortSignal.any([init.signal])
    // so the request's signal aborts when init.signal does. If no
    // init.signal, just `new AbortController().signal`.
    if let Some(sig_v) = init_signal {
        if !sig_v.is_null_or_undefined() {
            // AbortSignal.any([sig_v]) — returns a fresh signal.
            let any_key = v8::String::new(scope, "any").unwrap();
            if let Some(any_fn_v) = class_obj.get(scope, any_key.into()) {
                if let Ok(any_fn) = v8::Local::<v8::Function>::try_from(any_fn_v) {
                    let arr = v8::Array::new(scope, 1);
                    arr.set_index(scope, 0, sig_v);
                    let args = [arr.into()];
                    if let Some(result) = any_fn.call(scope, class_obj.into(), &args) {
                        if let Ok(o) = v8::Local::<v8::Object>::try_from(result) {
                            return o;
                        }
                    }
                }
            }
        }
    }

    // Default: fresh AbortController().signal.
    let ac_key = v8::String::new(scope, "AbortController").unwrap();
    let ac_v = global.get(scope, ac_key.into()).expect("AbortController missing");
    let ac_fn: v8::Local<v8::Function> = ac_v.try_into().unwrap();
    let ac = ac_fn
        .new_instance(scope, &[])
        .expect("new AbortController failed");
    let sig_key = v8::String::new(scope, "signal").unwrap();
    let sig_v = ac.get(scope, sig_key.into()).unwrap();
    sig_v.try_into().unwrap()
}

fn tee_stream<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<'s, v8::Object>,
) -> Option<(v8::Local<'s, v8::Object>, v8::Local<'s, v8::Object>)> {
    let key = v8::String::new(scope, "tee")?;
    let fn_v = stream.get(scope, key.into())?;
    let fn_l: v8::Local<v8::Function> = fn_v.try_into().ok()?;
    let result = fn_l.call(scope, stream.into(), &[])?;
    let arr: v8::Local<v8::Array> = result.try_into().ok()?;
    let a = arr.get_index(scope, 0)?;
    let b = arr.get_index(scope, 1)?;
    Some((a.try_into().ok()?, b.try_into().ok()?))
}
