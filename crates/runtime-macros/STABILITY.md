# `runtime-macros` emit stability contract

This document is the formal contract for symbols and paths that the
`#[v8_class]` proc macro (and its IDL siblings — `#[derive(WebIdlDict)]`,
`#[derive(WebIdlEnum)]`) emit into user crates. Renames and relocations
require coordinated PRs across `runtime-macros` AND `zeroship_runtime`
AND any `::zeroship_runtime::macro_runtime::*` facade additions.

The macro is bespoke — there is no third-party consumer. Internal
callers (the runtime crate, plugin crates, worker, CLI) MUST coordinate
breaking changes via this file plus `runtime-macros/TODO.md`.

---

## Per-class emitted symbols (`#[v8_class] impl <ClassTy>`)

| Symbol | Visibility | Stability | Notes |
|---|---|---|---|
| `<ClassTy>::install(scope) -> Local<FunctionTemplate>` | `pub` (inherent) | stable | Idempotent per isolate via the install slot. The runtime calls this from `setup_globals`. |
| `<ClassTy>::is_instance(scope, v) -> bool` | `pub` (inherent) | stable since Wave 5c | WebIDL §3.7 brand check. Preferred entry point for new consumer code. |
| `<ClassTy>` impl `V8ClassInstance` | trait impl | stable since Wave 5c | Generic-over-class bound. Sealed; only the macro can satisfy it. |
| `<ClassTy>` impl `__private::Sealed` | trait impl | private | Sealing supertrait of `V8ClassInstance`. User code MUST NOT impl this. |
| ~~`__zs_is_<ClassTy>` (free fn)~~ | ~~`pub` `#[doc(hidden)]`~~ | **removed in Wave 8** | Pre-Wave-5c grep target. Removed per the deprecation policy below. Any remaining call site MUST migrate to `<ClassTy>::is_instance`. |
| `__InstallSlot_<ClassTy>` (newtype) | `pub` `#[doc(hidden)]` | private; do not grep | Per-isolate slot wrapper for the cached `Global<FunctionTemplate>`. The hand-rolled `EventTarget` mirror in `crates/runtime/src/web/dom/event_target.rs` is the only known external consumer; new consumers MUST go through `<ClassTy>::install`. |
| `__BrandSlot_<ClassTy>` (newtype) | `pub` `#[doc(hidden)]` | private; do not grep | Per-isolate slot for the cached prototype handle (used by `__brand_check_<ClassTy>`). Internal to the macro; never call. |
| `__brand_check_<ClassTy>` | `pub(crate)` | private | Inner brand-check walker. Operates on `Local<Object>` (no value-shape gate). Internal helper for `<ClassTy>::is_instance`. |

### Brand-check API choice

```rust
// Preferred (Wave 5c+):
if Request::is_instance(scope, value) { /* ... */ }

// Generic / table-driven (Wave 5c+):
fn is_one_of<T: V8ClassInstance>(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> bool {
    T::is_instance(scope, v)
}

// Removed in Wave 8 (commit a8b50f8 — see git log for the exact
// hash on master after merge):
//   if __zs_is_Request(scope, value) { /* ... */ }   // ❌ no longer emitted
```

### Wave 8 removal of `__zs_is_<ClassTy>` — completed

Per the design's deprecation policy (`docs/proposals/runtime-macros-
refactor.md` §3.10):

1. **Wave 5c** — emit `<ClassTy>::is_instance` + the `V8ClassInstance`
   trait alongside `__zs_is_<ClassTy>`. Deprecation notice on the
   underscored symbol's rustdoc. All known production callers
   (`request.rs`, `als.rs`) migrated.
2. **Waves 6 / 7** — production code stayed migrated; only
   `v8_brand_pub_smoke.rs` still exercised the legacy symbol.
3. **Wave 8 (this wave)** — `__zs_is_<ClassTy>` deleted from emit
   (`crates/runtime-macros/src/v8_class/emit/public_is.rs`).
   `<ClassTy>::is_instance` now owns the `Local<Value>::try_into`
   gate that used to live in the shim. The single remaining test
   caller in `v8_brand_pub_smoke.rs` migrated to
   `<Class>::is_instance(scope, v)`. Project-wide grep for `__zs_is_`
   now returns ZERO matches outside this STABILITY.md and the
   macro's internal documentation.

---

## Per-derive emitted symbols (`#[derive(WebIdlDict)]` / `#[derive(WebIdlEnum)]`)

For `#[derive(WebIdlDict)] struct <DictTy>`:

| Symbol | Visibility | Stability |
|---|---|---|
| `<DictTy>::from_v8(scope, value) -> Result<Self, OpError>` | `pub` (inherent) | stable |
| `<DictTy>` impl `WebIdlConvertible` | trait impl | stable |

For `#[derive(WebIdlEnum)] enum <EnumTy>`:

| Symbol | Visibility | Stability |
|---|---|---|
| `<EnumTy>::from_str(s: &str) -> Option<Self>` | `pub` (inherent) | stable |
| `<EnumTy>::as_str(&self) -> &'static str` | `pub` (inherent) | stable |
| `<EnumTy>` impl `WebIdlConvertible` | trait impl | stable |

Both derives implicitly require the user type to satisfy `Default`
when used in dict-as-member position; that's enforced by the generated
code referencing `<T as Default>::default()`.

---

## Macro-runtime facade

Every emit path goes through `::zeroship_runtime::macro_runtime::*`
since Wave 5b. **The macro never references `::zeroship_runtime::<m>::<Item>`
directly.** When the macro starts emitting a new type:

1. Add a re-export under `crates/runtime/src/macro_runtime.rs`
   (matching the canonical sub-module shape — see the existing
   sub-modules for the convention).
2. Update the macro emit to use `::zeroship_runtime::macro_runtime::<m>::<Item>`.
3. Land both in the same PR; the runtime + macro crates ship lock-step.

The full list of currently-emitted facade paths (Wave 5b):

| Facade path | Backing canonical path |
|---|---|
| `macro_runtime::state::OpError` | `crate::state::OpError` |
| `macro_runtime::state::OpErrorKind` | `crate::state::OpErrorKind` |
| `macro_runtime::state::OpResult` | `crate::state::OpResult` |
| `macro_runtime::state::SharedState` | `crate::state::SharedState` (= `Rc<RefCell<RuntimeState>>`) |
| `macro_runtime::state::IntoResolveValue` | `crate::state::IntoResolveValue` |
| `macro_runtime::byte_string::ByteString` | `crate::byte_string::ByteString` |
| `macro_runtime::byte_string::read_byte_string` | `crate::byte_string::read_byte_string` |
| `macro_runtime::url_native::helpers::USVString` | `crate::url_native::helpers::USVString` |
| `macro_runtime::url_native::helpers::read_usv_string_or_throw` | `crate::url_native::helpers::read_usv_string_or_throw` |
| `macro_runtime::clamp::Clamp{U16,U32,I32,U64,I64}` | `crate::clamp::*` |
| `macro_runtime::clamp::read_clamp_{u16,u32,i32,u64,i64}` | `crate::clamp::*` |
| `macro_runtime::wrap::read_wrap_{u8,u16,u32,i8,i16,i32}` | `crate::wrap::*` |
| `macro_runtime::enforce_range::read_enforce_range_{u32,u64}` | `crate::enforce_range::*` |
| `macro_runtime::convert::WebIdlConvertible` | `crate::convert::WebIdlConvertible` |
| `macro_runtime::dom::exception::build` | `crate::dom::exception::build` |
| `macro_runtime::node_error::build_node_exception` | `crate::node_error::build_node_exception` |
| `macro_runtime::V8ClassInstance` | (no canonical — defined in macro_runtime) |
| `macro_runtime::__private::Sealed` | (no canonical — defined in macro_runtime) |

See `docs/proposals/runtime-macros-refactor.md` §3.9 for the
rationale (decouple macro emit from internal type relocations).

---

## Internal callers vs. macro emit

There's a crucial separation:

- **Internal callers** in `zeroship_runtime` (e.g. `request.rs`,
  `headers.rs`) MUST keep using the canonical paths
  (`crate::state::OpError`, `crate::byte_string::ByteString`, …).
- **Macro emit** at user-crate sites MUST go through the facade
  (`::zeroship_runtime::macro_runtime::*`).

The facade is a one-way contract: the macro reads it; the runtime
populates it. The runtime's own modules don't read from the facade
because that would create a cycle (the facade re-exports them).

---

## Versioning

The macro is versioned alongside `zeroship_runtime` (lock-step). Both
crates live in the same workspace and ship from the same commit.
External consumers don't exist.

When breaking-changing an emitted symbol, the workflow is:

1. Open the change in this file (`STABILITY.md`).
2. Migrate every internal caller in the same PR.
3. Update the snapshot fixtures (`crates/runtime-macros/src/v8_class/snapshots/`).
4. If the change adds a new emitted path, add the corresponding
   re-export in `crates/runtime/src/macro_runtime.rs`.

For the formal change-classifier (path-swap auto-accept vs structural
emit manual review), see `docs/proposals/runtime-macros-refactor.md`
§5.1.2.
