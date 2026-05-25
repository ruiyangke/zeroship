# `#[v8_constructor(post_init = "fn")]` shipped (MAC-02)

**Status:** Shipped 2026-05-04
**Long-form design:** [`docs/proposals/macro-constructor-post-init.md`](../archive/macro-constructor-post-init.md)
**Implementation:** [`crates/runtime-macros/src/v8_class/parse/marker_attr.rs`](../../crates/runtime-macros/src/v8_class/parse/marker_attr.rs) (codegen lives in `crates/runtime-macros/`).

## Context

`#[v8_constructor]` expected the user body to return a `Self` that
the macro then boxed and installed into V8 internal-field-0. Four
WHATWG stream classes (ReadableStreamDefaultReader,
ReadableStreamBYOBReader, WritableStreamDefaultWriter,
TransformStream) plus parts of native Request/Response need to make
V8 API calls **keyed off the fully-installed wrapper object** at
construction time — call helpers that themselves brand-check or
read-back via `with_state`. The macro gave no place to do that
wiring after the box install.

## Decision

- Add an opt-in `post_init = "fn"` attribute on `#[v8_constructor]`.
- The hook runs **after** the box is installed in internal-field-0 and **before** the constructor returns to JS.
- Signature is `(scope, this)`-only (v1's `&Self` reversal resolved in favor of the spec-aligned shape; the wrapper is already installed, so `with_state` works inside the hook).
- Uses stable V8-crate APIs only (`Object::SetInternalField`, `Weak::with_guaranteed_finalizer`, `FunctionTemplate::new`, `[[Construct]]` callback contract). No post-v147 features.

## Consequences

- Unblocked the four stream-class migrations: ReadableStreamDefaultReader (`d9cd7f1`), ReadableStreamBYOBReader (`135848a`), WritableStreamDefaultWriter (`c119711`), TransformStream (`47792eb` #200).
- Used downstream by RPC v2 cleanup: `b8c7642` (RpcError `cause` passthrough via post_init hook).
- Trybuild snapshot kept current for compiler-error UX (`4eed058` for rustc 1.94).

## See also

- Implementing commits: `11e8daa` / `b602094` runtime-macros: `#[v8_constructor(post_init = "fn")]` (MAC-02).
- Consumer migrations: `d9cd7f1`, `135848a`, `c119711`, `47792eb`.
- Related ADRs: [macro-v8-state](./2026-05-04-macro-v8-state.md), [streams-native](./2026-05-02-streams-native.md).
