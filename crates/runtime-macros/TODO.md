# runtime-macros TODO

## Brand check: every `#[v8_class]` callback should verify `this` per WebIDL §3.7

**Source**: URL review, M4/M5 (2026-05-02). The local fix lives in
`crates/runtime/src/url_native/search_params.rs::is_url_search_params`.

### Problem

The macro currently emits the following pattern for every method/getter/
setter callback (see `gen_method_callback` and `gen_setter_callback` in
`v8_class.rs`):

```rust
let __ext = match __this.get_internal_field(scope, 0)
    .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
{
    Some(e) => e,
    None => { /* throw "Illegal invocation" */ }
};
let __instance = unsafe { &mut *(__ext.value() as *mut #class_ty) };
```

The "verification" is `internal field 0 is an External" — but every
`#[v8_class]` instance with `internal_field_count = 1` has an External
there. So:

```js
URLSearchParams.prototype.entries.call(headers_instance)
//   the Headers Box is reinterpreted as a URLSearchParams Box → UB
```

WebIDL §3.7 mandates a real brand check: the receiver must be a genuine
instance of the class, verified by walking the prototype chain (or by
some other class-identity mechanism).

### Local fix shipped (URLSearchParams)

`crates/runtime/src/url_native/search_params.rs` adds:
- `UrlNativeSlot::search_params_prototype: Global<Object>` — captured
  at install time.
- `is_url_search_params(obj, scope)` — walks the prototype chain
  (max depth 32) looking for the cached `URLSearchParams.prototype`.
- `iter_factory_callback` and `for_each_callback` invoke the check
  before any Box deref.

### System-wide fix

Every `#[v8_class]` callback should perform an equivalent check before
the unsafe deref. Sketch:

1. At `install` time (in the macro-emitted code) cache the class's
   prototype in a per-class isolate slot (e.g.
   `__BrandSlot_<ClassTy>(Global<Object>)`).
2. In `gen_method_callback` / `gen_setter_callback` /
   `gen_constructor_callback`'s prologue, walk `__this`'s prototype
   chain for the cached prototype. If absent, throw "Illegal
   invocation".
3. The check is cheap (≤32 pointer comparisons; in practice 1–2 hops
   for a direct instance); the cost is dwarfed by the V8 callback
   overhead.

### Affected classes (current map of `#[v8_class]`)

- Headers, Request, Response, FormData
- AbortSignal, EventTarget, Event
- Blob, File
- ReadableStream, ReadableStreamDefaultReader, etc. (all stream
  classes)
- URL, URLSearchParams, URLSearchParamsIterator (this dispatch fixed
  URLSearchParams + URLSearchParamsIterator locally; URL still relies
  on the unsafe pattern but has no cross-class lookalikes among
  installed `#[v8_class]` types).

### Acceptance

A representative test surface (drop into the macro test crate):

```js
// Headers method called with non-Headers receiver throws
try { Headers.prototype.append.call({}, "k", "v"); }
catch (e) { /* expect TypeError "Illegal invocation" */ }

// Response method called with Request receiver throws (cross-class)
const req = new Request("https://x");
try { Response.prototype.text.call(req); }
catch (e) { /* expect TypeError */ }
```
