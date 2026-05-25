# `#[v8_state_marker]` macro attribute shipped (MAC-01)

**Status:** Shipped 2026-05-04
**Long-form design:** [`docs/proposals/macro-v8-state.md`](../archive/macro-v8-state.md)
**Implementation:** [`crates/runtime-macros/src/lib.rs`](../../crates/runtime-macros/src/lib.rs) (`pub fn v8_state_marker`); preserved through the runtime-macros refactor.

## Context

Every existing `#[v8_class]` consumer paired the JS-facing class
with a **single Rust type** that IS the boxed state. Request and
Response were the anomaly: they used `Request`/`Response` as JS-only
marker units while the actual state lived in `RequestState` /
`ResponseState`. The two carried ~535 LOC of glue boilerplate
between them that the macro couldn't see.

## Decision

- Add `#[v8_state_marker(MarkerTy)]` to project V8 internal-field-0 from a separate state struct.
- The user keeps two names (marker `Request` for the JS class, state `RequestState` for the boxed data) and the `impl` block sits on the state — matching the existing convention where the impl receiver IS the boxed state.
- Macro emits the same internal-field-0 install / brand check / `with_state` pattern as today; only the type the macro associates with the JS class changes.
- Adds ~+120 LOC of codegen logic; eliminates ~-535 LOC net across Request + Response (the spec walks of ~560 LOC of WebIDL §5.4/§5.5 stays — that's the floor).

## Consequences

- Unblocked Request (MAC-01 Phase 2) and Response (MAC-01 Phase 3) migration.
- Once shipped, `#[v8_getter(same_object)]` on `Request.headers`/`signal` and `Response.headers` became actionable (the floor for the `[SameObject]` migration that runtime-macros-refactor.md Wave 1 builds on).
- Macro preserved unchanged through the subsequent runtime-macros refactor.
- Post-ship fix: `f4dbf43` runtime-macros: fix `v8_state_marker × v8_iterable` interaction (closes #174).

## See also

- Implementing commits: `1660d4f` / `d4d65fd` / `ecd494b` runtime-macros: `#[v8_state_marker(MarkerTy)]` MAC-01 Phase 1; `cfe89f8` Merge MAC-01 Phase 1; `cbcea4f` Merge MAC-01 Phase 2 (Request); `797d343` Merge MAC-01 Phase 3 (Response).
- Related ADRs: [macro-constructor-post-init](./2026-05-04-macro-constructor-post-init.md).
