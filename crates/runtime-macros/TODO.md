# runtime-macros TODO

Status of macro extensions tracked here. Closed items keep their commit hash
so we can grep back through the rationale.

## Done

- **`#[v8_async_method]`** — async class methods compile to a sync V8
  callback that allocates a `v8::PromiseResolver`, spawns the user's
  body via `state.spawned_ops`, and returns the Promise. The pump
  resolves on `OpResult::JsValue`.
  - Compile-time guards: `&mut self` rejected (borrow-across-`.await`
    unsound under V8 re-entry); non-`async` fn rejected.
  - Supported return types: `()`, `bool`, `u32`, `i32`, `f64`,
    `String`, `Vec<u8>`, `v8::Global<v8::Value>`, plus `Result<T,
    OpError>` over any of the above.
  - Lands: commit `0a26d45b` (codegen + ResolveValue variants),
    `17b2e880` (16 smoke tests), `caa4f450` (compile-fail doctests).
  - Borrow safety: the future captures a `Global<v8::Object>` of the
    wrapper; as long as the future hasn't dropped, V8 cannot finalise
    the wrapper, so the boxed `Self` behind the recovered `*mut Self`
    stays valid across every poll. Detail in
    `gen_async_method_callback`'s doc comment.

## Open

- **Same-name getter+setter pairing** — defining `#[v8_getter] value
  (&self)` and `#[v8_setter] value(&mut self, v)` at once is illegal in
  Rust (duplicate method names) AND the install code calls
  `set_accessor_property` separately for each. Fix needs a
  `#[v8_name = "value"]` rename plus pairing in install codegen. Body's
  `body`/`bodyUsed` are read-only so not blocking fetch.

- **`#[reject_shared]` on Vec<u8> setter args** — currently only
  honoured on regular method args. Setters take exactly one arg
  positionally, so the same logic should apply — but the
  CompressionStream chunks path uses methods, not setters, so this
  isn't blocking.

- **Generic return type detection** — `gen_call_return` currently
  handles a fixed list of scalar types (bool/u32/i32/f64/String). New
  primitives (e.g. `u64` for byte-counter getters) require a code edit
  in `lib.rs::gen_scalar_set`. A trait-based dispatch (similar to
  `IntoResolveValue`) would let users opt in by implementing the
  trait, but the existing list covers every fetch / streams /
  WebSocket / WebCrypto consumer.
