//! Native `RpcError` — RPC v2 phase 1 (Wave B).
//!
//! Mirrors the [`crate::web::dom::exception::DOMException`] pattern but
//! built on the modern `#[v8_inherit_intrinsic = "Error"]` attribute
//! (DOMException predates the attribute and uses a manual prototype
//! walk in `install_global`). The class is exposed on `globalThis` as
//! `RpcError`; instances satisfy `instanceof Error` AND `instanceof
//! RpcError`, carry a `Symbol.toStringTag` of `"RpcError"`, and decorate
//! the constructor with the `ZsErrorCode` string constants
//! (`RpcError.UNAUTHENTICATED === "UNAUTHENTICATED"` etc.) per the
//! design doc.
//!
//! ## Storage layout
//!
//! All fields live in Rust on the boxed `RpcError` in internal field 0:
//!
//!   - `code: ZsErrorCode` — closed enum of canonical error codes.
//!   - `message: String`  — human-readable description.
//!   - `details_json: Option<String>` — raw JSON text of the structured
//!     payload (Zod issues, validation errors, etc.). We cache the
//!     `JSON.stringify`-output verbatim so the JS-side round-trip
//!     preserves source-key insertion order. [`RpcError::details_as_value`]
//!     hydrates the text into a `serde_json::Value` on demand.
//!   - `retryable: bool`   — set by the constructor either explicitly
//!     (via `opts.retryable`) or derived from the code's
//!     [`ZsErrorCode::default_retryable`] table.
//!   - `expose_message: bool` — whether the gateway may forward this
//!     error's `message` to clients.
//!
//! ## Wire mapping
//!
//! [`ZsErrorCode::as_wire_str`] returns the canonical UPPER_SNAKE form
//! (`Unauthenticated → "UNAUTHENTICATED"`). The WebIDL enum derive
//! uses `#[webidl_name = "..."]` per variant so JS-side construction
//! (`new RpcError("UNAUTHENTICATED", ...)`) speaks the same wire form.
//! [`ZsErrorCode::http_status`] maps each variant to its canonical HTTP
//! status (matches the gRPC → HTTP mapping table).

use std::cell::RefCell;

#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_getter, v8_inherit_intrinsic, v8_name, v8_to_string_tag,
    WebIdlDict, WebIdlEnum,
};

use crate::convert::WebIdlConvertible;
use crate::state::OpError;

// ---------------------------------------------------------------------------
// ZsErrorCode — closed enum (gRPC-flavoured wire codes)
// ---------------------------------------------------------------------------

/// The canonical RPC error code set. Wire form is UPPER_SNAKE_CASE
/// (`"UNAUTHENTICATED"` etc.); the WebIDL enum derive maps each
/// variant to that wire string via `#[webidl_name = ...]` per variant.
///
/// `Default = Unknown` matches the proposal: deserializing an empty or
/// missing `code` field on the wire produces `Unknown` rather than
/// rejecting — clients can carry forward foreign codes without losing
/// the error envelope.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, WebIdlEnum)]
pub enum ZsErrorCode {
    #[default]
    #[webidl_name = "UNKNOWN"]
    Unknown,
    #[webidl_name = "UNAUTHENTICATED"]
    Unauthenticated,
    #[webidl_name = "PERMISSION_DENIED"]
    PermissionDenied,
    #[webidl_name = "NOT_FOUND"]
    NotFound,
    #[webidl_name = "INVALID_ARGUMENT"]
    InvalidArgument,
    #[webidl_name = "FAILED_PRECONDITION"]
    FailedPrecondition,
    #[webidl_name = "ALREADY_EXISTS"]
    AlreadyExists,
    #[webidl_name = "RESOURCE_EXHAUSTED"]
    ResourceExhausted,
    #[webidl_name = "ABORTED"]
    Aborted,
    #[webidl_name = "INTERNAL"]
    Internal,
    #[webidl_name = "UNAVAILABLE"]
    Unavailable,
    #[webidl_name = "TIMEOUT"]
    Timeout,
    #[webidl_name = "CANCELLED"]
    Cancelled,
    #[webidl_name = "OUT_OF_RANGE"]
    OutOfRange,
    #[webidl_name = "UNIMPLEMENTED"]
    Unimplemented,
}

impl ZsErrorCode {
    /// Variant → canonical UPPER_SNAKE wire string. Round-trips with
    /// [`from_wire_str`].
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::NotFound => "NOT_FOUND",
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::FailedPrecondition => "FAILED_PRECONDITION",
            Self::AlreadyExists => "ALREADY_EXISTS",
            Self::ResourceExhausted => "RESOURCE_EXHAUSTED",
            Self::Aborted => "ABORTED",
            Self::Internal => "INTERNAL",
            Self::Unavailable => "UNAVAILABLE",
            Self::Timeout => "TIMEOUT",
            Self::Cancelled => "CANCELLED",
            Self::OutOfRange => "OUT_OF_RANGE",
            Self::Unimplemented => "UNIMPLEMENTED",
        }
    }

    /// Wire string → variant. None on unknown name.
    pub fn from_wire_str(s: &str) -> Option<Self> {
        // Delegate to the WebIdlEnum-derived `from_str` — it walks the
        // exact same `#[webidl_name = ...]` table.
        Self::from_str(s)
    }

    /// Canonical HTTP status for this code. Matches the gRPC → HTTP
    /// mapping table (Google API design guide); `Aborted`/`Cancelled`
    /// map to 499 (nginx's "client closed request"), the only non-RFC
    /// code in common use that fits the semantics.
    pub fn http_status(self) -> u16 {
        match self {
            Self::Unauthenticated => 401,
            Self::PermissionDenied => 403,
            Self::NotFound => 404,
            Self::InvalidArgument | Self::FailedPrecondition | Self::OutOfRange => 400,
            Self::AlreadyExists => 409,
            Self::ResourceExhausted => 429,
            Self::Aborted | Self::Cancelled => 499,
            Self::Timeout => 504,
            Self::Unavailable => 503,
            Self::Internal | Self::Unimplemented | Self::Unknown => 500,
        }
    }

    /// Whether this code is retryable by default. Override on a
    /// per-instance basis via `opts.retryable` at construction.
    pub fn default_retryable(self) -> bool {
        matches!(
            self,
            Self::ResourceExhausted | Self::Timeout | Self::Unavailable
        )
    }
}

// ---------------------------------------------------------------------------
// RpcErrorInit — the constructor's options dict (WebIDL §3.10)
// ---------------------------------------------------------------------------

/// Options passed to `new RpcError(code, message, opts?)`.
///
/// `details` and `cause` ride as `v8::Local` passthroughs: `details` is
/// `JSON.stringify`d at construction so the round-trip preserves
/// insertion order (`serde_json::Value`'s default `BTreeMap` backing
/// would re-sort keys); `cause` is stashed verbatim and re-attached as
/// an own property on the instance by the post-init hook (matches
/// ECMAScript §20.5.6.1.1 — `{ [[Writable]]: true, [[Enumerable]]:
/// false, [[Configurable]]: true }`).
#[derive(Default, Debug, WebIdlDict)]
pub struct RpcErrorInit<'s> {
    pub details: Option<v8::Local<'s, v8::Value>>,
    pub cause: Option<v8::Local<'s, v8::Value>>,
    pub retryable: Option<bool>,
    #[webidl_name = "exposeMessage"]
    pub expose_message: Option<bool>,
}

// ---------------------------------------------------------------------------
// RpcError struct (V8 instance state)
// ---------------------------------------------------------------------------

/// Backing state for a JS-constructed RpcError. Stored in the V8
/// wrapper's internal field 0 as `Box<RpcError>`. All non-pending
/// slots are immutable post-construction.
///
/// `details_json` carries the raw `JSON.stringify`-output of the
/// original JS value, preserving the source object's key insertion
/// order across the getter round-trip (a `serde_json::Value` round-
/// trip with the default backing would re-sort string keys via
/// `BTreeMap`). [`details_as_value`] hydrates the stored text into a
/// `serde_json::Value` on demand for Rust callers that prefer the
/// typed shape; the wire format is the JSON text either way.
///
/// `pending_cause` stashes the constructor's `opts.cause` value until
/// the post-init hook (`attach_cause`) attaches it as an own property
/// on `args.this()`. Constructor body can't see `__this` (the macro
/// only passes user args) so the hand-off goes through this slot.
/// Always cleared (`take`) by the post-init hook — observable as
/// `None` thereafter.
#[derive(Default, Debug)]
pub struct RpcError {
    pub code: ZsErrorCode,
    pub message: String,
    pub details_json: Option<String>,
    pub retryable: bool,
    pub expose_message: bool,
    pub pending_cause: RefCell<Option<v8::Global<v8::Value>>>,
}

impl RpcError {
    /// Hydrate the stored `details_json` text into a typed
    /// `serde_json::Value`. Returns `None` when no details are set or
    /// the stored text fails to parse (in practice the text is always
    /// valid JSON because it came from `JSON.stringify` or
    /// `serde_json::to_string`).
    pub fn details_as_value(&self) -> Option<serde_json::Value> {
        self.details_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
    }
}

// ---------------------------------------------------------------------------
// RpcError IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[v8_to_string_tag = "RpcError"]
#[v8_inherit_intrinsic = "Error"]
impl RpcError {
    /// `new RpcError(code: ZsErrorCode, message: DOMString, opts?:
    /// RpcErrorInit)`. Per the proposal: missing `code` throws
    /// TypeError (WebIDL enum unknown-value rule); missing `message`
    /// defaults to `""`; missing `opts` is treated as the empty dict
    /// (all defaults).
    ///
    /// All three args take `v8::Local<v8::Value>` rather than the
    /// derived types directly because the `#[v8_class]` macro's arg
    /// classifier (`KnownType::from_ty`) only routes primitives /
    /// well-known WebIDL newtypes through `WebIdlConvertible`; user
    /// enum / dict types fall through to its `StringDefault` arm,
    /// which would produce a type-mismatched extraction. Manual
    /// `from_v8` calls in the body bypass the classifier; pattern is
    /// the same as `CustomEvent::new`.
    #[v8_constructor(post_init = "attach_cause")]
    fn new(
        scope: &mut v8::PinScope,
        code: v8::Local<v8::Value>,
        message: v8::Local<v8::Value>,
        opts: v8::Local<v8::Value>,
    ) -> Result<RpcError, OpError> {
        let code = ZsErrorCode::from_v8(scope, code)?;
        let message = if message.is_undefined() {
            String::new()
        } else {
            <String as WebIdlConvertible>::from_v8(scope, message)?
        };
        let opts = RpcErrorInit::from_v8(scope, opts)?;
        let details_json = match opts.details {
            None => None,
            Some(v) => {
                // Preserve source-key order by snapshotting via
                // `JSON.stringify` once at construction and caching the
                // raw text. The getter feeds the same text back through
                // `JSON.parse` so the JS-visible shape round-trips
                // verbatim. Failure (cycle, non-serializable) surfaces
                // as a TypeError — matches WebIDL `any?` boundary
                // conversion when the caller passes an invalid value.
                if v.is_undefined() {
                    None
                } else {
                    let s = v8::json::stringify(scope, v).ok_or_else(|| {
                        OpError::type_error(
                            "RpcError details: not JSON-serializable",
                        )
                    })?;
                    Some(s.to_rust_string_lossy(scope))
                }
            }
        };
        // Stash `cause` for the post-init hook. `undefined` ⇒ no own
        // property emitted (matches `new Error("x")` shape: `'cause' in
        // err === false`); any other value (incl. `null`) is preserved
        // verbatim. The hand-off is necessary because the macro doesn't
        // expose `args.this()` to the constructor body — the hook
        // re-reads via `with_state` after field-0 install.
        let pending_cause = match opts.cause {
            Some(v) if !v.is_undefined() => RefCell::new(Some(v8::Global::new(scope, v))),
            _ => RefCell::new(None),
        };
        let retryable = opts
            .retryable
            .unwrap_or_else(|| code.default_retryable());
        Ok(RpcError {
            code,
            message,
            details_json,
            retryable,
            expose_message: opts.expose_message.unwrap_or(false),
            pending_cause,
        })
    }

    /// `rpcError.code` — wire string (UPPER_SNAKE).
    #[v8_getter]
    fn code(&self) -> String {
        self.code.as_wire_str().to_string()
    }

    /// `rpcError.name` — always `"RpcError"`. Required for the
    /// JS-conventional Error shape (`e.name + ": " + e.message`).
    #[v8_getter]
    fn name(&self) -> String {
        "RpcError".to_string()
    }

    /// `rpcError.message` — human-readable description.
    #[v8_getter]
    fn message(&self) -> String {
        self.message.clone()
    }

    /// `rpcError.details` — round-trips the constructor's details arg
    /// back to JS via `JSON.parse` of the cached stringified form.
    /// Returns `undefined` (the JS-conventional "absent" value) when
    /// no details were attached, matching the
    /// `details_default_undefined` test contract.
    #[v8_getter]
    fn details<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let Some(s) = self.details_json.as_deref() else {
            return v8::undefined(scope).into();
        };
        let Some(json_str) = v8::String::new(scope, s) else {
            return v8::null(scope).into();
        };
        v8::json::parse(scope, json_str).unwrap_or_else(|| v8::null(scope).into())
    }

    /// `rpcError.retryable` — flag for clients/SDKs that drive
    /// automatic retry behaviour.
    #[v8_getter]
    fn retryable(&self) -> bool {
        self.retryable
    }

    /// `rpcError.exposeMessage` — gateway-level flag controlling whether
    /// the message string forwards to clients (false ⇒ message is
    /// stripped or replaced with a generic placeholder).
    #[v8_getter]
    #[v8_name = "exposeMessage"]
    fn expose_message(&self) -> bool {
        self.expose_message
    }

    /// `rpcError.status` — convenience accessor for the canonical HTTP
    /// status, computed from [`ZsErrorCode::http_status`]. Stable
    /// per-code mapping; never depends on instance state.
    #[v8_getter]
    fn status(&self) -> u32 {
        self.code.http_status() as u32
    }

    /// Post-init hook: re-attach the stashed `cause` as a non-enumerable
    /// own data property on the instance, matching the ECMAScript
    /// `Error` shape (§20.5.6.1.1 step 3:
    /// `CreateNonEnumerableDataPropertyOrThrow(O, "cause", cause)` —
    /// writable + configurable + non-enumerable). Skipped (no property
    /// emitted) when `cause` was absent or explicitly `undefined` — so
    /// `'cause' in new RpcError(...)` is false in those cases, exactly
    /// like a plain `new Error("x")`.
    pub(crate) fn attach_cause(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Result<(), OpError> {
        let cause_g = with_state(scope, this, |s| s.pending_cause.borrow_mut().take())
            .ok_or_else(|| OpError::error("attach_cause: with_state returned None"))?;
        let Some(cause_g) = cause_g else { return Ok(()); };
        let cause = v8::Local::new(scope, &cause_g);
        let key = v8::String::new(scope, "cause")
            .ok_or_else(|| OpError::error("attach_cause: String alloc failed"))?;
        // DONT_ENUM only — keeps writable + configurable, matching the
        // ECMA-spec descriptor for the implicit cause own property.
        this.define_own_property(scope, key.into(), cause, v8::PropertyAttribute::DONT_ENUM);
        Ok(())
    }
}

/// Recover the boxed `RpcError` from a JS wrapper's internal field 0.
/// Returns `None` for non-RpcError objects or torn-down state. SAFETY:
/// the External points at a `Box<RpcError>` whose lifetime is owned by
/// V8's weak finalizer; aliasing is fine because the hook only borrows
/// non-mutably or through the inner `RefCell`s.
pub fn with_state<R>(
    scope: &mut v8::PinScope,
    this: v8::Local<v8::Object>,
    f: impl FnOnce(&RpcError) -> R,
) -> Option<R> {
    let raw = this.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw).ok()?;
    let ptr = ext.value() as *const RpcError;
    if ptr.is_null() {
        return None;
    }
    Some(f(unsafe { &*ptr }))
}

// ---------------------------------------------------------------------------
// Construction helpers — Rust-side RpcError minting
// ---------------------------------------------------------------------------

/// Optional fields for [`build`] / [`throw`].
#[derive(Default, Debug, Clone)]
pub struct RpcErrorBuildOptions {
    pub details: Option<serde_json::Value>,
    pub retryable: Option<bool>,
    pub expose_message: Option<bool>,
}

/// Build a native RpcError JS object with the given code + message +
/// options. Returns the wrapper as a `v8::Local<v8::Object>` so callers
/// can `scope.throw_exception(obj.into())` or attach the value as a
/// JS-visible reason. The returned object has the full RpcError
/// prototype chain (including `instanceof Error`) and a real boxed
/// state in internal field 0.
///
/// Use this from Rust paths that need to construct an RpcError with a
/// spec-correct shape — e.g. the gateway's error envelope reconstitution,
/// or the dispatch layer's normalised throw path.
pub fn build<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    code: ZsErrorCode,
    message: &str,
    options: RpcErrorBuildOptions,
) -> v8::Local<'s, v8::Object> {
    let tmpl = RpcError::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .expect("RpcError instance allocation failed");

    let retryable = options
        .retryable
        .unwrap_or_else(|| code.default_retryable());
    let details_json = options
        .details
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".to_string()));
    // Rust-side builds bypass V8's [[Construct]], so the post-init
    // hook never fires — leave `pending_cause` empty. Callers that
    // need a JS-visible cause can attach it on the returned object.
    let state = RpcError {
        code,
        message: message.to_string(),
        details_json,
        retryable,
        expose_message: options.expose_message.unwrap_or(false),
        pending_cause: RefCell::new(None),
    };
    let boxed: Box<RpcError> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // Wire prototype to RpcError.prototype.
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    obj.set_prototype(scope, proto_v);

    // Finalizer: reclaim the Box on GC or isolate teardown. Mirrors
    // the DOMException::build pattern.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut RpcError));
        }),
    );
    std::mem::forget(weak);

    obj
}

/// Throw a native RpcError. Sets the pending V8 exception on `scope`;
/// callers should immediately return from their callback.
pub fn throw(scope: &mut v8::PinScope, code: ZsErrorCode, message: &str) {
    let obj = build(scope, code, message, RpcErrorBuildOptions::default());
    scope.throw_exception(obj.into());
}

// ---------------------------------------------------------------------------
// install_global — wire RpcError onto globalThis with Error.prototype
// chain (handled by `#[v8_inherit_intrinsic = "Error"]`) plus
// ZsErrorCode wire-string constants on the constructor.
// ---------------------------------------------------------------------------

/// Install `RpcError` on `globalThis`. Mirrors
/// [`crate::web::dom::exception::install_global`] but uses
/// `#[v8_inherit_intrinsic = "Error"]` to chain
/// `RpcError.prototype.[[Prototype]]` to `Error.prototype` (the macro
/// emits the prototype-chain JS at install time; we don't need the
/// manual `Object.setPrototypeOf` workaround DOMException requires).
///
/// Decorates the constructor function with the `ZsErrorCode` wire-name
/// constants (`RpcError.UNAUTHENTICATED === "UNAUTHENTICATED"` etc.)
/// per the proposal so user code can construct via
/// `new RpcError(RpcError.UNAUTHENTICATED, ...)` instead of repeating
/// the bare string literal.
pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let tmpl = RpcError::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();

    // Decorate the constructor with ZsErrorCode wire-string constants.
    // Each constant is its own wire-string value: `RpcError.NOT_FOUND`
    // === "NOT_FOUND". Plain `set` (writable + enumerable + configurable)
    // — spec-strict descriptors can be tightened later if userland needs
    // them locked down.
    for code in ALL_CODES {
        let wire = code.as_wire_str();
        let key = v8::String::new(scope, wire).unwrap();
        let value = v8::String::new(scope, wire).unwrap();
        class_fn.set(scope, key.into(), value.into());
    }

    let global_key = v8::String::new(scope, "RpcError").unwrap();
    global.set(scope, global_key.into(), class_fn.into());
}

/// Full set of `ZsErrorCode` variants — used by [`install_global`] to
/// emit constants on the constructor. Kept in sync with the enum
/// declaration (compiler exhaustiveness on `as_wire_str` catches any
/// drift; we'd just need to extend this slice when adding a variant).
const ALL_CODES: &[ZsErrorCode] = &[
    ZsErrorCode::Unknown,
    ZsErrorCode::Unauthenticated,
    ZsErrorCode::PermissionDenied,
    ZsErrorCode::NotFound,
    ZsErrorCode::InvalidArgument,
    ZsErrorCode::FailedPrecondition,
    ZsErrorCode::AlreadyExists,
    ZsErrorCode::ResourceExhausted,
    ZsErrorCode::Aborted,
    ZsErrorCode::Internal,
    ZsErrorCode::Unavailable,
    ZsErrorCode::Timeout,
    ZsErrorCode::Cancelled,
    ZsErrorCode::OutOfRange,
    ZsErrorCode::Unimplemented,
];
