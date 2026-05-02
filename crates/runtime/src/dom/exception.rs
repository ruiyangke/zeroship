//! Native `DOMException` per WebIDL §3.14
//! (https://webidl.spec.whatwg.org/#idl-DOMException).
//!
//! Replaces the JS polyfill that lived in `embed/fetch.js`. The polyfill
//! had the right shape for `new DOMException("msg", "AbortError")` but
//! lost spec details that user code (and WPT) cares about:
//!
//!   - Real prototype chain: `(new DOMException()) instanceof Error`
//!     must be true. The polyfill set `Error.prototype` directly, but
//!     `DOMException.prototype.constructor` should still be
//!     `DOMException`, the `name` property should appear on the instance
//!     (not just the prototype), and a `Symbol.toStringTag` of
//!     `"DOMException"` should be exposed.
//!   - Spec-mapped legacy `code` numeric: the WebIDL legacy table
//!     (https://webidl.spec.whatwg.org/#idl-DOMException-error-names)
//!     maps known names to small integers; everything else is `0`. The
//!     polyfill only handled `"AbortError" → 20`; we now cover all 22.
//!   - Static legacy `INDEX_SIZE_ERR = 1` etc. constants on the
//!     constructor (and on the prototype, per WebIDL §3.7.5).
//!
//! ## Storage layout (single-source slot rule)
//!
//! All slots live in Rust on the boxed `DOMException` in internal
//! field 0:
//!
//!   - `name: String`  — the WebIDL `name` attribute.
//!   - `message: String` — the WebIDL `message` attribute.
//!   - `code: u16` — derived from `name` via the legacy mapping
//!     table; computed once at construction.
//!
//! Per WebIDL DOMException is a `[Serializable, LegacyArrayClass]`
//! interface inheriting from `Error`. We achieve the prototype-chain
//! link via `__InstallSlot_DOMException` codegen + an
//! `Object.setPrototypeOf(DOMException.prototype, Error.prototype)`
//! step in `install_global` (post-`get_function`, before global
//! registration). This is the documented Deno/Cloudflare workaround
//! since rusty_v8 doesn't expose `set_intrinsic_data_property` for
//! `Error.prototype` directly.

use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter, v8_name, v8_to_string_tag};

use crate::state::OpError;

// ---------------------------------------------------------------------------
// Legacy code mapping — WebIDL §3.14
// (https://webidl.spec.whatwg.org/#idl-DOMException-error-names)
// ---------------------------------------------------------------------------

/// Map a DOMException `name` to its legacy `code` numeric. Names that
/// don't appear in the spec table return `0`.
pub(crate) fn legacy_code_for_name(name: &str) -> u16 {
    match name {
        "IndexSizeError" => 1,
        "HierarchyRequestError" => 3,
        "WrongDocumentError" => 4,
        "InvalidCharacterError" => 5,
        "NoModificationAllowedError" => 7,
        "NotFoundError" => 8,
        "NotSupportedError" => 9,
        "InUseAttributeError" => 10,
        "InvalidStateError" => 11,
        "SyntaxError" => 12,
        "InvalidModificationError" => 13,
        "NamespaceError" => 14,
        "InvalidAccessError" => 15,
        "TypeMismatchError" => 17,
        "SecurityError" => 18,
        "NetworkError" => 19,
        "AbortError" => 20,
        "URLMismatchError" => 21,
        "QuotaExceededError" => 22,
        "TimeoutError" => 23,
        "InvalidNodeTypeError" => 24,
        "DataCloneError" => 25,
        _ => 0,
    }
}

/// Static legacy code constants installed on both the constructor and
/// the prototype per WebIDL §3.7.5. These are observable via
/// `DOMException.INDEX_SIZE_ERR === 1` and similar.
pub(crate) const LEGACY_CODE_CONSTANTS: &[(&str, u16)] = &[
    ("INDEX_SIZE_ERR", 1),
    ("DOMSTRING_SIZE_ERR", 2),
    ("HIERARCHY_REQUEST_ERR", 3),
    ("WRONG_DOCUMENT_ERR", 4),
    ("INVALID_CHARACTER_ERR", 5),
    ("NO_DATA_ALLOWED_ERR", 6),
    ("NO_MODIFICATION_ALLOWED_ERR", 7),
    ("NOT_FOUND_ERR", 8),
    ("NOT_SUPPORTED_ERR", 9),
    ("INUSE_ATTRIBUTE_ERR", 10),
    ("INVALID_STATE_ERR", 11),
    ("SYNTAX_ERR", 12),
    ("INVALID_MODIFICATION_ERR", 13),
    ("NAMESPACE_ERR", 14),
    ("INVALID_ACCESS_ERR", 15),
    ("VALIDATION_ERR", 16),
    ("TYPE_MISMATCH_ERR", 17),
    ("SECURITY_ERR", 18),
    ("NETWORK_ERR", 19),
    ("ABORT_ERR", 20),
    ("URL_MISMATCH_ERR", 21),
    ("QUOTA_EXCEEDED_ERR", 22),
    ("TIMEOUT_ERR", 23),
    ("INVALID_NODE_TYPE_ERR", 24),
    ("DATA_CLONE_ERR", 25),
];

// ---------------------------------------------------------------------------
// DOMException struct
// ---------------------------------------------------------------------------

/// Backing state for a JS-constructed DOMException. Stored in
/// internal field 0 as `Box<DOMException>`. All slots are immutable
/// post-construction (DOMException attributes are read-only per
/// WebIDL).
pub struct DOMException {
    pub name: String,
    pub message: String,
    pub code: u16,
}

impl Default for DOMException {
    fn default() -> Self {
        DOMException {
            name: "Error".to_string(),
            message: String::new(),
            code: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// DOMException IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[v8_to_string_tag = "DOMException"]
impl DOMException {
    /// `new DOMException(message?: DOMString, name?: DOMString)` —
    /// per WebIDL §3.14. Both args default to ""/"Error" respectively
    /// when absent or undefined.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        message: v8::Local<v8::Value>,
        name: v8::Local<v8::Value>,
    ) -> Result<DOMException, OpError> {
        // Per WebIDL DOMString conversion: undefined → "" (the default
        // value); everything else goes through ToString.
        let msg_str = if message.is_undefined() {
            String::new()
        } else {
            let Some(s) = message.to_string(scope) else {
                // V8 has a pending exception (e.g. throwing toString).
                // Yield with a placeholder; the pending exception
                // propagates after the callback returns.
                return Ok(DOMException::default());
            };
            s.to_rust_string_lossy(scope)
        };
        let name_str = if name.is_undefined() {
            "Error".to_string()
        } else {
            let Some(s) = name.to_string(scope) else {
                return Ok(DOMException::default());
            };
            s.to_rust_string_lossy(scope)
        };
        let code = legacy_code_for_name(&name_str);
        Ok(DOMException {
            name: name_str,
            message: msg_str,
            code,
        })
    }

    /// `domException.name` getter — WebIDL §3.14.
    #[v8_getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    /// `domException.message` getter — WebIDL §3.14.
    #[v8_getter]
    fn message(&self) -> String {
        self.message.clone()
    }

    /// `domException.code` getter — WebIDL §3.14 legacy table.
    #[v8_getter]
    fn code(&self) -> u32 {
        self.code as u32
    }
}

// ---------------------------------------------------------------------------
// install_global — wire DOMException onto globalThis with Error
// prototype chain + legacy code constants.
// ---------------------------------------------------------------------------

/// Install `DOMException` on `globalThis`, chain its prototype to
/// `Error.prototype` (so `(new DOMException()) instanceof Error` is
/// true), and decorate the constructor + prototype with legacy code
/// constants per WebIDL §3.7.5.
pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let tmpl = DOMException::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();

    // Chain DOMException.prototype.[[Prototype]] to Error.prototype.
    // Direct `set_prototype` on the prototype object is the same path
    // the IteratorPrototype handler uses, just against a different
    // intrinsic. We resolve `Error.prototype` from the active context's
    // global Error (which V8 has installed by the time setup_globals
    // runs).
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    let error_key = v8::String::new(scope, "Error").unwrap();
    if let Some(error_v) = global.get(scope, error_key.into()) {
        if let Ok(error_fn) = v8::Local::<v8::Function>::try_from(error_v) {
            if let Some(error_proto_v) = error_fn.get(scope, proto_key.into()) {
                proto.set_prototype(scope, error_proto_v);
            }
        }
    }

    // Per WebIDL §3.7.5: legacy code constants live on BOTH the
    // constructor function and the prototype. Plain set (writable +
    // enumerable + configurable) — spec-strict descriptors can be
    // tightened later.
    for (name, val) in LEGACY_CODE_CONSTANTS {
        let key = v8::String::new(scope, name).unwrap();
        let v = v8::Integer::new_from_unsigned(scope, *val as u32);
        class_fn.set(scope, key.into(), v.into());
        proto.set(scope, key.into(), v.into());
    }

    let global_key = v8::String::new(scope, "DOMException").unwrap();
    global.set(scope, global_key.into(), class_fn.into());
}
