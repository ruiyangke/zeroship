//! Native `URL` per WHATWG URL §4
//! (https://url.spec.whatwg.org/#url-class).
//!
//! Backed by `ada_url::Url` (the same parser as Node.js, Chromium,
//! Bun). Setters delegate to ada-url's mutation API rather than
//! the polyfill's naive string-splitting (the polyfill's
//! `url.host = "x:y"` literally split on `:` and reassigned both
//! parts; ada-url runs the WHATWG host-parser state machine and
//! handles bracketed IPv6 hosts, IDNA, etc.).
//!
//! ## Storage
//!
//! ```text
//! pub struct URL {
//!     inner: ada_url::Url,
//!     // [SameObject] cache: lazily allocated on first .searchParams
//!     // access; same Global on all subsequent reads. Cleared on
//!     // .href setter (which fully reparses).
//!     search_params: Option<v8::Global<v8::Object>>,
//! }
//! ```

use crate::state::OpError;
use crate::url_native::helpers::read_usv_string;
use crate::url_native::search_params::URLSearchParams;

#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_getter, v8_method, v8_name, v8_setter, v8_to_string_tag,
};

// ---------------------------------------------------------------------------
// URL struct
// ---------------------------------------------------------------------------

pub struct URL {
    pub inner: ada_url::Url,
    /// `[SameObject]` cache of the bound URLSearchParams. None until
    /// first access; populated by `URL::install_search_params_global`
    /// (called from the searchParams getter).
    pub search_params: Option<v8::Global<v8::Object>>,
}

impl Default for URL {
    fn default() -> Self {
        URL {
            // Default URL is the file scheme — ada-url won't accept the
            // empty string, and we need *something* for the
            // `#[v8_class]`-generated default constructor (which is
            // never reached on the JS surface because `new URL()` calls
            // the user-defined constructor with at least the input arg).
            inner: ada_url::Url::parse("about:blank", None).unwrap(),
            search_params: None,
        }
    }
}

// ---------------------------------------------------------------------------
// URL class — IDL surface per https://url.spec.whatwg.org/#url-class
// ---------------------------------------------------------------------------

#[v8_class]
impl URL {
    /// `new URL(input, base?)` per §4.1.
    ///
    ///   1. Let parsedBase be null.
    ///   2. If base is given:
    ///      a. Let parsedBase be the result of running the URL parser on base.
    ///      b. If parsedBase is failure, then throw a TypeError.
    ///   3. Let parsedURL be the result of running the URL parser on input
    ///      with parsedBase.
    ///   4. If parsedURL is failure, then throw a TypeError.
    ///
    /// Both args are USVString — lone surrogates → U+FFFD.
    ///
    /// `input` is required per IDL but EXPLICIT `undefined`
    /// ToUSVString-converts to the string `"undefined"` (per WebIDL
    /// §3.2.21). So `URL.parse(undefined, "aaa:/b")` parses
    /// `"undefined"` against base `"aaa:/b"` and may succeed (this
    /// matters for the WPT url-statics-parse case). The `new URL()`
    /// (no-args) path also runs the same conversion: undefined →
    /// "undefined" → ada-url parse of "undefined" alone fails
    /// (no scheme, no base) → TypeError. So zero-arity still throws,
    /// just via the parse-failure path rather than an arity check.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        input: v8::Local<v8::Value>,
        base: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        let input_s = read_usv_string(scope, input)
            .ok_or_else(|| OpError::type_error("Cannot convert input to USVString"))?;

        let base_s = if base.is_undefined() {
            None
        } else {
            // The polyfill accepted `base instanceof URL` and used its
            // `href`. The ada parser only takes &str — convert.
            // ToUSVString(base) suffices (URL.href is already USV, so
            // round-trip is fine).
            Some(
                read_usv_string(scope, base)
                    .ok_or_else(|| OpError::type_error("Cannot convert base to USVString"))?,
            )
        };

        match ada_url::Url::parse(&input_s, base_s.as_deref()) {
            Ok(inner) => Ok(URL {
                inner,
                search_params: None,
            }),
            Err(_) => Err(OpError::type_error(format!("Invalid URL: {input_s}"))),
        }
    }

    /// `URL.canParse(input, base?) → boolean` per §4.1.
    /// Uses ada-url's `can_parse` directly — no allocation on success
    /// or failure.
    ///
    /// Note: this is a static method, not an instance method. The
    /// macro emits an instance method by default; we install the
    /// static separately in `install_global`.
    // Marker — actual installation is in `install_global` below.

    /// `toString()` per §4.5 — alias for `href`.
    ///
    /// `Object.prototype.toString.call(url)` uses Symbol.toStringTag
    /// instead and produces `[object URL]`. The user-callable
    /// `url.toString()` is a different beast and returns href.
    #[v8_method]
    #[v8_name = "toString"]
    fn to_string(&self) -> String {
        self.inner.href().to_string()
    }

    /// `toJSON()` per §4.5 — alias for `href`. Used by JSON.stringify
    /// when a URL is serialized.
    #[v8_method]
    #[v8_name = "toJSON"]
    fn to_json(&self) -> String {
        self.inner.href().to_string()
    }

    /// `href` getter per §4.5. Returns the serialized URL.
    #[v8_getter]
    fn href(&self) -> String {
        self.inner.href().to_string()
    }

    /// `href` setter per §4.5:
    ///   1. Let parsedURL be the result of running the URL parser on
    ///      the given value.
    ///   2. If parsedURL is failure, throw a TypeError.
    ///   3. Set this's URL to parsedURL.
    ///   4. Empty this's query object's list. (We invalidate the
    ///      [SameObject] cache; the next `.searchParams` access reuses
    ///      the same JS object but its entries re-sync from the parsed
    ///      URL.)
    #[v8_setter]
    #[v8_name = "href"]
    fn set_href(
        &mut self,
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        let s = read_usv_string(scope, value)
            .ok_or_else(|| OpError::type_error("Cannot convert href to USVString"))?;
        if self.inner.set_href(&s).is_err() {
            return Err(OpError::type_error(format!("Invalid URL: {s}")));
        }
        // Per spec the SameObject searchParams object stays valid; only
        // its underlying list gets repopulated. Bound mode re-syncs on
        // every read, so no explicit cache invalidation is needed.
        Ok(())
    }

    /// `origin` getter per §4.5 (read-only).
    #[v8_getter]
    fn origin(&self) -> String {
        self.inner.origin()
    }

    /// `protocol` getter per §4.5.
    #[v8_getter]
    fn protocol(&self) -> String {
        self.inner.protocol().to_string()
    }

    /// `protocol` setter per §4.5: silently no-op on parser failure.
    #[v8_setter]
    #[v8_name = "protocol"]
    fn set_protocol(&mut self, scope: &mut v8::PinScope, value: v8::Local<v8::Value>) {
        let Some(s) = read_usv_string(scope, value) else {
            return;
        };
        let _ = self.inner.set_protocol(&s);
    }

    /// `username` getter per §4.5.
    #[v8_getter]
    fn username(&self) -> String {
        self.inner.username().to_string()
    }

    /// `username` setter per §4.5: silently no-op for special URLs
    /// without an authority component (file:, etc. when the spec
    /// forbids credentials).
    #[v8_setter]
    #[v8_name = "username"]
    fn set_username(&mut self, scope: &mut v8::PinScope, value: v8::Local<v8::Value>) {
        let Some(s) = read_usv_string(scope, value) else {
            return;
        };
        let _ = self.inner.set_username(Some(&s));
    }

    /// `password` getter per §4.5.
    #[v8_getter]
    fn password(&self) -> String {
        self.inner.password().to_string()
    }

    /// `password` setter per §4.5.
    #[v8_setter]
    #[v8_name = "password"]
    fn set_password(&mut self, scope: &mut v8::PinScope, value: v8::Local<v8::Value>) {
        let Some(s) = read_usv_string(scope, value) else {
            return;
        };
        let _ = self.inner.set_password(Some(&s));
    }

    /// `host` getter per §4.5 — returns hostname[:port].
    #[v8_getter]
    fn host(&self) -> String {
        self.inner.host().to_string()
    }

    /// `host` setter per §4.5 — runs ada-url's host parser. Spec-
    /// correct: handles bracketed IPv6 (`[::1]:8080`), IDNA, and
    /// punctuation-bearing hosts that the polyfill's `lastIndexOf(":")`
    /// trick mishandled.
    #[v8_setter]
    #[v8_name = "host"]
    fn set_host(&mut self, scope: &mut v8::PinScope, value: v8::Local<v8::Value>) {
        let Some(s) = read_usv_string(scope, value) else {
            return;
        };
        let _ = self.inner.set_host(Some(&s));
    }

    /// `hostname` getter per §4.5 — host without port.
    #[v8_getter]
    fn hostname(&self) -> String {
        self.inner.hostname().to_string()
    }

    /// `hostname` setter per §4.5.
    #[v8_setter]
    #[v8_name = "hostname"]
    fn set_hostname(&mut self, scope: &mut v8::PinScope, value: v8::Local<v8::Value>) {
        let Some(s) = read_usv_string(scope, value) else {
            return;
        };
        let _ = self.inner.set_hostname(Some(&s));
    }

    /// `port` getter per §4.5.
    #[v8_getter]
    fn port(&self) -> String {
        self.inner.port().to_string()
    }

    /// `port` setter per §4.5.
    #[v8_setter]
    #[v8_name = "port"]
    fn set_port(&mut self, scope: &mut v8::PinScope, value: v8::Local<v8::Value>) {
        let Some(s) = read_usv_string(scope, value) else {
            return;
        };
        // Empty string clears the port (per spec, `url.port = ""`
        // removes any explicit port). ada-url's set_port(Some("")) is
        // actually an error; explicit None form clears.
        if s.is_empty() {
            let _ = self.inner.set_port(None);
        } else {
            let _ = self.inner.set_port(Some(&s));
        }
    }

    /// `pathname` getter per §4.5.
    #[v8_getter]
    fn pathname(&self) -> String {
        self.inner.pathname().to_string()
    }

    /// `pathname` setter per §4.5.
    #[v8_setter]
    #[v8_name = "pathname"]
    fn set_pathname(&mut self, scope: &mut v8::PinScope, value: v8::Local<v8::Value>) {
        let Some(s) = read_usv_string(scope, value) else {
            return;
        };
        let _ = self.inner.set_pathname(Some(&s));
    }

    /// `search` getter per §4.5.
    #[v8_getter]
    fn search(&self) -> String {
        self.inner.search().to_string()
    }

    /// `search` setter per §4.5. Empty/no-prefix-? value clears.
    /// Important: this also implicitly updates searchParams view per
    /// §4.5 step 5 ("empty this's query object's list, then if value
    /// is non-null, set the list to the result of parsing value").
    /// Bound URLSearchParams re-syncs on every read — no explicit
    /// invalidation needed.
    #[v8_setter]
    #[v8_name = "search"]
    fn set_search(&mut self, scope: &mut v8::PinScope, value: v8::Local<v8::Value>) {
        let Some(s) = read_usv_string(scope, value) else {
            return;
        };
        if s.is_empty() {
            self.inner.set_search(None);
        } else {
            // ada-url accepts the leading '?' verbatim and strips it
            // internally if present. Pass through.
            self.inner.set_search(Some(&s));
        }
    }

    /// `hash` getter per §4.5.
    #[v8_getter]
    fn hash(&self) -> String {
        self.inner.hash().to_string()
    }

    /// `hash` setter per §4.5. Empty value clears.
    #[v8_setter]
    #[v8_name = "hash"]
    fn set_hash(&mut self, scope: &mut v8::PinScope, value: v8::Local<v8::Value>) {
        let Some(s) = read_usv_string(scope, value) else {
            return;
        };
        if s.is_empty() {
            self.inner.set_hash(None);
        } else {
            self.inner.set_hash(Some(&s));
        }
    }

    // searchParams is installed by hand because it needs `args.this()`
    // to bind the URLSearchParams to its parent URL JS wrapper. See
    // `install_global` below.
}

// ---------------------------------------------------------------------------
// install_global — install URL + URL.canParse + URL.parse + searchParams
// ---------------------------------------------------------------------------

pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Function> {
    let tmpl = URL::install(scope);

    // searchParams is a getter that returns a [SameObject]
    // URLSearchParams bound to this URL. Wire as an accessor on the
    // FunctionTemplate's prototype_template so the descriptor lives
    // on every instance's prototype chain naturally.
    {
        let proto_tmpl = tmpl.prototype_template(scope);
        let getter = v8::FunctionTemplate::new(scope, search_params_getter_callback);
        let key = v8::String::new(scope, "searchParams").unwrap();
        proto_tmpl.set_accessor_property(
            key.into(),
            Some(getter.into()),
            None,
            v8::PropertyAttribute::NONE,
        );
    }

    let class_fn = tmpl.get_function(scope).unwrap();

    // URL.canParse(input, base?) → boolean. Static method on the class
    // function itself, NOT the prototype.
    {
        let f_tmpl = v8::FunctionTemplate::new(scope, can_parse_callback);
        let f = f_tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "canParse").unwrap();
        class_fn.set(scope, key.into(), f.into());
    }

    // URL.parse(input, base?) → URL | null. Static method (newer
    // WHATWG spec). Returns null instead of throwing on parse failure.
    {
        let f_tmpl = v8::FunctionTemplate::new(scope, parse_callback);
        let f = f_tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "parse").unwrap();
        class_fn.set(scope, key.into(), f.into());
    }

    let key = v8::String::new(scope, "URL").unwrap();
    global.set(scope, key.into(), class_fn.into());
    class_fn
}

/// `URL.canParse(input, base?) → boolean`. Static method.
fn can_parse_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let input = match read_usv_string(scope, args.get(0)) {
        Some(s) => s,
        None => {
            rv.set(v8::Boolean::new(scope, false).into());
            return;
        }
    };
    let base = if args.length() > 1 && !args.get(1).is_undefined() {
        match read_usv_string(scope, args.get(1)) {
            Some(s) => Some(s),
            None => {
                rv.set(v8::Boolean::new(scope, false).into());
                return;
            }
        }
    } else {
        None
    };
    rv.set(v8::Boolean::new(scope, ada_url::Url::can_parse(&input, base.as_deref())).into());
}

/// `URL.parse(input, base?) → URL | null`. Newer WHATWG static method
/// (https://url.spec.whatwg.org/#dom-url-parse). Returns null on parse
/// failure (does NOT throw, unlike `new URL(...)`).
fn parse_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let input = match read_usv_string(scope, args.get(0)) {
        Some(s) => s,
        None => {
            rv.set(v8::null(scope).into());
            return;
        }
    };
    let base = if args.length() > 1 && !args.get(1).is_undefined() {
        match read_usv_string(scope, args.get(1)) {
            Some(s) => Some(s),
            None => {
                rv.set(v8::null(scope).into());
                return;
            }
        }
    } else {
        None
    };

    // Pre-validate so we don't pay the construct-and-rollback price on
    // the hot failure path (callers like Workers' router routinely call
    // URL.parse on speculative inputs).
    if !ada_url::Url::can_parse(&input, base.as_deref()) {
        rv.set(v8::null(scope).into());
        return;
    }

    // Look up the URL class function via the per-isolate slot. Set by
    // `install_globals` — invariant: search_params_getter_callback /
    // parse_callback only fire after the slot is populated.
    let slot = match scope.get_slot::<crate::url_native::UrlNativeSlot>() {
        Some(s) => s,
        None => {
            rv.set(v8::null(scope).into());
            return;
        }
    };
    let url_fn = v8::Local::new(scope, &slot.url_class_fn);

    // Construct via `new URL(input, base?)`. Per spec the constructor
    // throws TypeError on parse failure; we already validated above so
    // this path always succeeds. Internal field 0 (the boxed URL) is
    // populated automatically by the macro-emitted constructor.
    let input_v = v8::String::new(scope, &input).unwrap();
    let argv: Vec<v8::Local<v8::Value>> = if let Some(b) = base.as_deref() {
        vec![input_v.into(), v8::String::new(scope, b).unwrap().into()]
    } else {
        vec![input_v.into()]
    };
    let new_inst = match url_fn.new_instance(scope, &argv) {
        Some(o) => o,
        None => {
            rv.set(v8::null(scope).into());
            return;
        }
    };
    rv.set(new_inst.into());
}

/// `url.searchParams` getter. Lazily instantiate a URLSearchParams JS
/// wrapper bound to `this` URL; cache the Global so subsequent reads
/// return the same object (`[SameObject]` per IDL §4.5).
fn search_params_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this_obj = args.this();
    let url: &mut URL = match this_obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
    {
        Some(e) => unsafe { &mut *(e.value() as *mut URL) },
        None => {
            let msg = v8::String::new(scope, "Illegal invocation").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    if let Some(g) = &url.search_params {
        let cached = v8::Local::new(scope, g);
        rv.set(cached.into());
        return;
    }

    // First access — build a URLSearchParams wrapper bound to `this`.
    // Construct an instance via `new URLSearchParams()` over the
    // shared class function so methods/prototype/internal-field-count
    // all match the singleton class definition. We immediately tear
    // down the constructor's `entries` and replace the Box's internal
    // field with our own bound-mode URLSearchParams.
    let slot = match scope.get_slot::<crate::url_native::UrlNativeSlot>() {
        Some(s) => s,
        None => {
            let msg = v8::String::new(scope, "URLSearchParams not installed").unwrap();
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };
    let sp_class_fn = v8::Local::new(scope, &slot.search_params_class_fn);

    let sp_obj = match sp_class_fn.new_instance(scope, &[]) {
        Some(o) => o,
        None => return, // constructor threw — exception is pending
    };

    // Replace the constructor's stand-alone URLSearchParams with one
    // bound to this URL. We modify the existing Box's contents in
    // place rather than swapping pointers — that way the macro's
    // weak-finalizer (which captured the original raw_addr) frees the
    // right thing.
    let old_ext = match sp_obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
    {
        Some(e) => e,
        None => return, // shouldn't happen — internal field count = 1
    };
    let old_raw = old_ext.value() as *mut URLSearchParams;
    // SAFETY: old_raw is the Box allocated by the URLSearchParams
    // macro-emitted constructor; we mutate its contents in place. No
    // other &mut to this Box exists.
    let sp_inst: &mut URLSearchParams = unsafe { &mut *old_raw };
    // C1: SP holds a Weak<Object> to its parent URL — not a strong
    // Global. The URL keeps a strong Global to the SP wrapper (forward
    // direction, below) for [SameObject], but the back reference is
    // weak so the cycle is breakable. When the URL is GC'd the Weak
    // fails to upgrade and the SP transparently falls back to
    // standalone semantics.
    let parent_weak = v8::Weak::new(scope, this_obj);
    *sp_inst = URLSearchParams::bound_to(parent_weak);

    // Cache the SP wrapper Global on the URL so [SameObject] holds.
    let cached = v8::Global::new(scope, sp_obj);
    url.search_params = Some(cached);

    rv.set(sp_obj.into());
}
