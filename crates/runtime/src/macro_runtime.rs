//! Stable re-export surface for `runtime-macros` emit.
//!
//! The `#[v8_class]` proc-macro (and the IDL-class siblings —
//! `#[webidl_dictionary]`, `#[webidl_enumeration]`, etc.) emits paths
//! into this module — never directly into other modules of
//! `zeroship_runtime`. This gives the runtime crate freedom to
//! relocate types internally without breaking the macro's emit
//! contract.
//!
//! # Macro author rules
//!
//! Every new emit path goes here first. Adding a re-export is part
//! of the macro's PR. See `crates/runtime-macros/STABILITY.md` for
//! the formal contract.
//!
//! Internal callers of the same types (e.g. `request.rs` uses
//! `OpError` directly) MUST keep using the canonical paths
//! (`crate::state::OpError`) — this facade is for the macro alone.
//!
//! # Why a flat sub-module shape
//!
//! The emit substitutions are simple prefix swaps:
//! `::zeroship_runtime::<m>::<Item>` → `::zeroship_runtime::macro_runtime::<m>::<Item>`.
//! We deliberately mirror the original module shape in the
//! sub-module names so the substitution stays mechanical and the
//! Wave-5b snapshot diff is purely path-swap (no semantic change).
//!
//! # Stability
//!
//! Items here are part of the macro's public emit contract. Renames
//! / relocations require coordinated PRs across `runtime-macros`
//! AND `zeroship_runtime`. See design `docs/proposals/runtime-
//! macros-refactor.md` §3.9 / §3.10.

// ----- state -----
//
// `OpError` (incl. `js_value` / `type_error` ctors) is emitted from
// every error-throwing arg extractor and from the WebIDL dict/enum
// `from_v8` impls. `OpErrorKind` is emitted by the codegen exception
// builder. `OpResult::JsValue` is emitted by `#[v8_async_method]`'s
// resolver tail. `SharedState` is emitted as the slot type the
// async-method body fetches off the isolate. `IntoResolveValue` is
// the trait whose `into_resolve_value` async-method emit calls on
// the user's return value.
pub mod state {
    pub use crate::state::{IntoResolveValue, OpError, OpErrorKind, OpResult, SharedState};
}

// ----- byte_string (WebIDL ByteString) -----
//
// `ByteString` (the newtype) is emitted by the slow-path arg
// extractor for ByteString-typed params (Headers names/values,
// fetch Request method). `from_bytes` is the constructor used in
// both slow and fastcall paths. `read_byte_string` is the slow-
// path reader.
pub mod byte_string {
    pub use crate::byte_string::{ByteString, read_byte_string};
}

// ----- url_native::helpers (WebIDL USVString) -----
//
// Emitted by the USVString / Option<USVString> slow-path arg
// extractors. Mirrors the canonical sub-module path so the emit
// substitution stays mechanical: `url_native::helpers::USVString`
// → `macro_runtime::url_native::helpers::USVString`.
pub mod url_native {
    pub mod helpers {
        pub use crate::url_native::helpers::{USVString, read_usv_string_or_throw};
    }
}

// ----- clamp (WebIDL [Clamp]) -----
//
// Emitted by `clamp_reader_ctor` in `runtime-macros` for `[Clamp]`
// integer params. Both the reader fns AND the newtype constructors
// are emitted (the slow-path reads via `read_clamp_*` and wraps in
// `Clamp*`).
pub mod clamp {
    pub use crate::clamp::{
        ClampI32, ClampI64, ClampU16, ClampU32, ClampU64,
        read_clamp_i32, read_clamp_i64, read_clamp_u16, read_clamp_u32, read_clamp_u64,
    };
}

// ----- wrap (WebIDL default integer conversion) -----
//
// Emitted by `wrap_reader` in `runtime-macros` for default-converted
// integer params. Only the reader fns are emitted — the wrap newtypes
// have no public ctor (the reader returns the wrapped value directly).
pub mod wrap {
    pub use crate::wrap::{
        read_wrap_i8, read_wrap_i16, read_wrap_i32,
        read_wrap_u8, read_wrap_u16, read_wrap_u32,
    };
}

// ----- enforce_range (WebIDL [EnforceRange]) -----
//
// Emitted by the `EnforceRangeU32` / `EnforceRangeU64` slow-path arg
// extractors. The reader fns throw on overflow per WebIDL §3.13.3;
// the newtype itself is recognised by ident in `types.rs` (no path-
// emit), so we only re-export the readers.
pub mod enforce_range {
    pub use crate::enforce_range::{read_enforce_range_u32, read_enforce_range_u64};
}

// ----- convert (WebIdlConvertible blanket impl) -----
//
// Emitted by both `#[webidl_dictionary]` (for the per-dict blanket
// impl + per-member field extraction) and `#[webidl_enumeration]`
// (for the enum-side blanket impl). The blanket lets dict/enum types
// compose inside `sequence<T>`, `record<K, V>`, and other dicts.
pub mod convert {
    pub use crate::convert::WebIdlConvertible;
}

// ----- dom::exception::build -----
//
// Emitted by `gen_throw_op_error_arms` in `runtime-macros::codegen`
// — the `OpErrorKind::DomException(name)` arm builds a `DOMException`
// via this fn. The path mirrors the canonical `dom::exception::build`
// shape so the emit substitution is mechanical.
pub mod dom {
    pub mod exception {
        pub use crate::dom::exception::build;
    }
}

// ----- node_error::build_node_exception -----
//
// Emitted by `gen_throw_op_error_arms` — the `OpErrorKind::NodeError(code)`
// arm builds a Node.js-style exception (with `.code` property) via
// this fn.
pub mod node_error {
    pub use crate::node_error::build_node_exception;
}

// ----- V8ClassInstance trait (Wave 5c, design §3.5) -----
//
// Stable typed brand-check entry point per `#[v8_class]`. Replaces the
// underscored `__zs_is_<Class>` grep target with `<Class>::is_instance`
// (the inherent method) plus a sealed trait `V8ClassInstance` that
// downstream code can use as a generic bound (`fn check<T: V8ClassInstance>`).
//
// The macro emits BOTH:
//   1. `impl <Class> { pub fn is_instance(scope, v) -> bool { ... } }`
//   2. `impl V8ClassInstance for <Class> { ... }` (gated by Sealed)
//
// The inherent method is what callers usually want — `Request::is_instance(scope, v)`
// reads naturally and doesn't require importing the trait. The trait
// exists for the rare generic-over-class pattern (a brand-check
// table indexed by class type) and enforces the macro-only
// invariant via the `Sealed` super-trait below.
//
// Sealing keeps the trait macro-only: only the proc-macro can write
// `impl Sealed for X` because `__private::Sealed` lives behind a
// `__private` module name. The path is `pub` so the macro emit at
// downstream-crate sites can name it; the underscored module name is
// the convention that signals "look but don't touch."
//
// Closes F2 — third-party impls of the brand-check trait are unsound
// (they'd have to fabricate a fake brand-check); sealing prevents that.

#[doc(hidden)]
pub mod __private {
    /// Sealing super-trait. Only `runtime-macros` emits `impl Sealed
    /// for <Class>`. The `pub` is needed so the macro's emitted impl
    /// at user-crate sites can name the trait — but the `__private`
    /// module name is doc-hidden and signals that user code MUST NOT
    /// `impl Sealed for X`.
    pub trait Sealed {}
}

/// WebIDL §3.7 brand check, generic over `#[v8_class]` types.
///
/// Returns `true` iff `value` is an instance of the implementing class
/// in the current isolate (or a subclass via `#[v8_inherit]`). The
/// underlying check walks the prototype chain comparing handle identity
/// against the cached install slot.
///
/// # Usage
///
/// ```ignore
/// // Inherent — preferred for the common case:
/// if Request::is_instance(scope, value) { /* ... */ }
///
/// // Trait — for generic / table-driven dispatch:
/// fn is_one_of<T: V8ClassInstance>(
///     scope: &mut v8::PinScope,
///     v: v8::Local<v8::Value>,
/// ) -> bool {
///     T::is_instance(scope, v)
/// }
/// ```
///
/// # Sealing
///
/// `V8ClassInstance: __private::Sealed` keeps this macro-only. Third-
/// party impls would be unsound — they'd have to fabricate a brand
/// check without owning the install slot.
///
/// # Stability
///
/// The trait, its method signature, and the sealing pattern are all
/// part of `runtime-macros`'s STABILITY contract. See
/// `crates/runtime-macros/STABILITY.md`.
pub trait V8ClassInstance: __private::Sealed {
    /// Brand-check: returns `true` iff `value` is an instance of this
    /// class (or a subclass via `#[v8_inherit]`) in the current isolate.
    /// Returns `false` for non-Object values and for isolates where
    /// the class hasn't been installed.
    fn is_instance(scope: &mut ::v8::PinScope, value: ::v8::Local<::v8::Value>) -> bool;
}
