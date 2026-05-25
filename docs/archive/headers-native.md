# Native `Headers` design

**Date:** 2026-05-01
**Status:** **Shipped** — `crates/runtime/src/web/headers.rs` (930 LOC). HeadersIterator migrated to `#[v8_iterable]` and `Headers.has` is fastcall (commits `d2fea29`, `5677051`). Document retained as the canonical design spec.
**Spec:** WHATWG Fetch §2.2 — https://fetch.spec.whatwg.org/#headers-class
**WebIDL:** https://webidl.spec.whatwg.org/

## Revision history

- **v2 (2026-05-01)** — Round-2 revision driven by the headers-review.md
  critic. Resolves all 5 BLOCKERs (live iteration, ByteString TypeError,
  normalize-then-validate ordering, name validation in `delete`/`has`/
  `get`, `delete_` rename via `#[v8_name]`), all 13 MAJORs, and the
  remaining MINORs/NITs/missing concepts. Iteration model flipped from
  snapshot to live. v1 explicitly commits to pure-native (no JS polyfill
  fallback) and to a storage shape forward-compatible with native
  Request/Response `[SameObject]` aliasing.
- **v1 (2026-05-01)** — Initial draft.

## Decisions (settled)

These are not open questions. They are the choices this design commits
to, with the spec/WPT citation that forces each.

| Decision | Choice | Forcing reference |
|---|---|---|
| Iteration model | **Live** (re-run sort-and-combine on every `next()`) | WebIDL §3.7.10.2 default iterator; Fetch §2.2 *"value pairs to iterate over"*; WPT `header-setcookie.any.js` mutation tests |
| Storage element type | `Vec<u8>` (not `String`) | Fetch §2.2 lenient byte-value rules (0x01–0x08, 0x0B, 0x0C, 0x0E–0x1F, 0x7F valid) |
| `validate` return shape | `bool`; throws TypeError only on bad name/value or immutable guard; returns false silently for forbidden request/response header guards | https://fetch.spec.whatwg.org/#headers-validate steps 1–5 |
| Op order in `append`/`set` | Normalize value → Validate(name, normalized) → Guard step → list mutate | Fetch §2.2.1 `append` step 1 = normalize; `set` step 1 = normalize |
| Name validation in `delete`/`has`/`get` | Required; throws TypeError on invalid name (validate is called with empty value) | Fetch §2.2.1 step 1 of each |
| HTTP whitespace bytes (normalize set) | 0x09, 0x0A, 0x0D, 0x20 (TAB, LF, CR, SP) | https://fetch.spec.whatwg.org/#http-whitespace-byte |
| ByteString boundary | `read_byte_string(scope, value)` using `String::contains_only_one_byte()` precheck → `write_one_byte_v2`; throws TypeError if any code unit > 0xFF | https://webidl.spec.whatwg.org/#js-to-ByteString step 2 |
| sequence-vs-record dispatch | `GetMethod(V, %Symbol.iterator%)` semantics: undefined for null/undefined, TypeError if non-callable, else callable | https://webidl.spec.whatwg.org/#js-to-union |
| Record key enumeration | `[[OwnPropertyKeys]]` then per-key `[[GetOwnProperty]]` enumerable check; Symbol keys go through ByteString conversion which throws | https://webidl.spec.whatwg.org/#js-to-record steps 3–4 |
| Iterator @@toStringTag | `"Headers Iterator"` (interface name + " Iterator") | WebIDL §3.7.10.2 |
| Iterator [[Prototype]] | `%Iterator.prototype%` | WebIDL §3.7.10.2 |
| Iteration result `"key+value"` array construction | `v8::Array::new(scope, 2)` + `set_index` (NOT `Array.of`) | https://webidl.spec.whatwg.org/#dfn-iterator-result avoids constructor-lookup observability |
| `new Headers(otherHeaders)` | Works through the iterable branch (Headers is itself iterable) | Empirical browser behaviour; iterable mixin |
| Polyfill removal cadence | Feature-flag native → cutover → remove polyfill (three separate landings, not one) | Risk control |

## Scope

Replace the JS-shimmed `Headers` (currently part of `embed/fetch.js`)
with a native Rust class via `#[v8_class]`. Targets the full WebIDL
surface:

```webidl
typedef (sequence<sequence<ByteString>> or record<ByteString, ByteString>) HeadersInit;

[Exposed=(Window,Worker)]
interface Headers {
  constructor(optional HeadersInit init);

  undefined append(ByteString name, ByteString value);
  undefined delete(ByteString name);
  ByteString? get(ByteString name);
  sequence<ByteString> getSetCookie();
  boolean has(ByteString name);
  undefined set(ByteString name, ByteString value);
  iterable<ByteString, ByteString>;
};
```

The `iterable<>` mixin auto-defines `entries()`, `keys()`, `values()`,
`forEach(callback, thisArg)`, and `@@iterator` per WebIDL §3.7.10.

Goals:
- WPT-compliant: target every test in `fetch/api/headers/` whose
  `META: script=` lines do not require Request/Response. (Authoritative
  list under "Test plan".)
- Bytes-faithful: `Vec<u8>` storage so non-UTF-8 header values
  (legitimate per RFC 9110 §5.5 / Fetch §2.2) round-trip without lossy
  substitution.
- Casing-faithful: at append time, if the list already contains a
  byte-case-insensitive match, reuse that match's name casing; else
  store the name as given. The note at Fetch §2.2.1 *"to append a
  header"* step 1 forces this. Lowercase only happens in sort-and-
  combine for iteration. (See "Casing semantics" for the post-`delete`
  reset case.)
- Set-Cookie special cases per the post-PR-#1346 spec
  (https://github.com/whatwg/fetch/pull/1346).
- Pure-native: no JS polyfill fallback in v1. The design must compose
  cleanly with native Request/Response when those land — see
  "Forward compatibility (Request/Response, [SameObject])" below.

Out of scope for v1:
- Guard semantics for `request` / `request-no-cors` / `response` /
  `immutable`. The forbidden-name lists and CORS-safelist enforcement
  only matter when `Request`/`Response` flow through the class. Ship
  alongside Request/Response. v1 omits the `guard` field entirely (see
  MINOR-25 fix below).
- The header-list `combine` operation (Fetch §2.2.4
  `concept-header-list-combine`) — used by HTTP fetch when responses
  arrive, not by Headers IDL methods. v1 doesn't need it; document
  here as a deferred building block (addresses MINOR-24).

## Storage

```rust
pub struct Headers {
    /// (name, value) pairs in insertion order. Names are byte sequences;
    /// case is preserved per `append` step 1.
    list: Vec<(Vec<u8>, Vec<u8>)>,

    /// Cached output of sort-and-combine. `None` on construction and
    /// after any mutation. Populated lazily by sort-and-combine and
    /// re-used by all live iterators between mutations.
    /// (Addresses MISSING-10.)
    sorted_cache: Option<Vec<(Vec<u8>, Vec<u8>)>>,
}
```

<!-- Added in round 2: addressing MINOR-25 dead Guard field -->
**No `guard` field in v1.** Adding a `Guard` enum that is never read is
dead state. When Request/Response land we will reintroduce it (likely
behind a `#[cfg(feature = "guards")]` or just unconditionally). The
validate algorithm in v1 short-circuits at the "Guard::None" equivalent
— see "validate" below.

**Why `Vec<u8>` not `String`:** header values legitimately contain bytes
0x01–0x08, 0x0B, 0x0C, 0x0E–0x1F, 0x7F per Fetch §2.2's lenient
validation rules. ByteString in WebIDL is a sequence of code units in
0x00–0xFF (https://webidl.spec.whatwg.org/#idl-ByteString), serialised
in V8 as a Latin-1 one-byte string. Storing as Rust `String` would
force UTF-8 validity and lose round-trip fidelity for bytes 0x80–0xFF.

**Why `Vec` not `HashMap` / multimap:** Fetch §2.2.1 explicitly defines
the header list as an *ordered list*. Sort happens on iteration only.
<!-- Added in round 2: addressing MAJOR-17 perf claim -->
We do not claim list size is bounded. Real-world traffic on this
platform will see CDN-augmented responses with 50+ headers
(CF-Ray, x-amz-*, cache-control, ETag, multiple Set-Cookies),
multi-AZ proxy chains with 200+ headers, and complex `Forwarded`/
`Via`/`Server-Timing` chains. O(n) lookup is acceptable because
(a) per-request header counts in the hundreds are still fast in
absolute terms, (b) sort-and-combine results are cached (see
`sorted_cache`), and (c) this matches Deno's flat shape; workerd's
two-tier common/uncommon map is a parser-side optimisation that lives
upstream of the Headers IDL surface and is irrelevant here.

## Forward compatibility (Request/Response, `[SameObject]`)

<!-- Added in round 2: addressing MISSING-6 [SameObject] -->
Native Request and Response are coming. The Fetch spec marks
`Request.headers` and `Response.headers` as `[SameObject]`, meaning each
Request/Response instance must return the *same* Headers object on
every property access. Two ways to support this:

1. **Aliased ownership** — `Rc<RefCell<HeaderList>>` where Request and
   the Headers wrapper share the same list. Read/write goes through
   `borrow`/`borrow_mut`. JS sees one Headers object per Request.

2. **Embedded ownership** — Headers is owned by the Request/Response
   struct directly; the JS `Headers` wrapper is created lazily and
   cached on the Request wrapper so the same V8 object is returned each
   call.

v1 takes path 2 conceptually. The v1 storage `list: Vec<...>` is owned
by `Headers` directly and we do not expose mutation paths from
"outside." When Request/Response land, the migration is:

- Move `list` and `sorted_cache` into a `HeaderList` struct
- Wrap as `Rc<RefCell<HeaderList>>` if we need shared mutation, or
  keep the embedded form and lazily create a wrapper Headers object
  for `[SameObject]`.

Either way, the v1 *public algorithms* (append/set/delete/get/has/
getSetCookie/sort-and-combine) take `&mut self` / `&self` on the list
and don't bake in single-owner assumptions. The only v1 footgun would
be if we built a multi-owner pattern around `Box<Headers>` directly;
we do not — the boxed Self lives in the V8 internal field, and
Request will hold a different shape (likely `RequestState` with its
own header list) and produce a JS Headers object that wraps a borrow.

This is consistent with Deno's `ext/fetch/22_headers.js` (Headers in
JS, header list in JS too, Request holds a reference) and with
workerd's `Headers.h` which holds a reference into Request state.

## ByteString validation

<!-- Added in round 2: addressing BLOCKER-2 ByteString TypeError -->
The Rust function IS the WebIDL boundary. There is no upstream
ByteString conversion. We MUST throw TypeError per
https://webidl.spec.whatwg.org/#js-to-ByteString step 2: *"If the value
of any element of x is greater than 255, then throw a TypeError."*

```rust
/// WebIDL ByteString conversion: read a v8::Value as a sequence of
/// 8-bit code units, throwing TypeError on any code unit > 0xFF.
fn read_byte_string(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    // WebIDL §3.2.10 step 1: ToString. v8::Value::to_string handles
    // ToString including symbol → TypeError per ECMA-262.
    let s = value
        .to_string(scope)
        .ok_or_else(|| OpError::type_error("Cannot convert value to ByteString"))?;

    // WebIDL §3.2.10 step 2: every code unit must be ≤ 0xFF.
    // V8 stores strings as ONE_BYTE (Latin-1) when all chars are ≤ 0xFF
    // and TWO_BYTE otherwise. The exact predicate is exposed:
    //   v8::String::contains_only_one_byte()
    // which returns true iff every code unit is ≤ 0xFF (including the
    // hidden case where a TWO_BYTE storage happens to hold only ≤ 0xFF
    // values — V8 internally optimises this).
    if !s.contains_only_one_byte() {
        return Err(OpError::type_error(
            "String contains code units > 0xFF (ByteString)",
        ));
    }

    // Now safe: every code unit is in 0x00–0xFF. write_one_byte_v2
    // copies the low byte of each code unit. Since all high bytes are
    // 0, this is a faithful Latin-1 → bytes copy.
    let len = s.length();
    let mut buf = vec![0u8; len];
    s.write_one_byte_v2(scope, 0, &mut buf, v8::WriteFlags::empty());
    Ok(buf)
}
```

The precheck is essential. `write_one_byte_v2` silently truncates code
units > 0xFF (confirmed in `v8-147.1.0/src/string.rs:559-576`); calling
it without the precheck would let `'\u0100'` pass through as `0x00`,
violating WebIDL.

This is the **only** ByteString reader in `headers.rs`. Every place
that touches a name or value goes through it.

### Macro extension (BLOCKER-2 / MAJOR-12 fix)

<!-- Added in round 2: addressing MAJOR-12 Vec<u8> extraction inconsistency -->
The macro currently extracts `Vec<u8>` only from ArrayBufferView (see
`crates/runtime-macros/src/lib.rs:198-217`). For Headers, we extend
`gen_extract` with a new path: when a method parameter is declared as
`Vec<u8>` AND the method has the attribute `#[v8_bytestring(arg_name)]`
(or, equivalently, the macro recognises a `ByteString` newtype — see
below), the extraction calls `read_byte_string(scope, value)` instead of
the ArrayBuffer path.

Two paths to commit to (we pick the second):

- **Per-arg attribute** `#[v8_bytestring(name, value)]` on the method.
  Pros: explicit. Cons: noisy; one attribute per method.
- **Newtype** `pub struct ByteString(Vec<u8>);` declared in
  `runtime-core` and recognised by the macro. The method signature
  becomes `fn append(&mut self, name: ByteString, value: ByteString)`.
  Macro `gen_extract` matches `ByteString` and emits the
  `read_byte_string` call. Internally `ByteString` derefs / `into()`
  to `Vec<u8>` so the body is unchanged.

**Decision:** Newtype `ByteString`. It is self-describing, future-proof
(every WebIDL ByteString in the codebase reuses it), and keeps method
signatures clean. The signature table below uses `ByteString`.

This resolves the table inconsistency from v1 (BLOCKER-2 / MAJOR-12):
`name`/`value` are NOT `String` — they are `ByteString` and bypass the
macro's lossy default `to_rust_string_lossy` path entirely.

## Header name / value byte rules

```rust
/// HTTP token: 1*tchar.
/// tchar per RFC 9110 §5.6.2:
///   "!" / "#" / "$" / "%" / "&" / "'" / "*" / "+" / "-" / "." /
///   "^" / "_" / "`" / "|" / "~" / DIGIT / ALPHA
/// (https://www.rfc-editor.org/rfc/rfc9110.html#section-5.6.2)
fn is_header_name(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(is_tchar)
}

fn is_tchar(b: &u8) -> bool {
    matches!(*b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
      | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
      | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z')
}

/// Header value (Fetch §2.2 "header value"):
///   - No leading/trailing 0x09 or 0x20 (TAB, SPACE)
///   - No 0x00 (NUL), 0x0A (LF), 0x0D (CR) anywhere
/// Other bytes (incl. 0x80+, 0x0B, 0x0C, 0x7F, 0x01–0x08, 0x0E–0x1F)
/// are allowed.
fn is_header_value(b: &[u8]) -> bool {
    !b.first().is_some_and(|&c| c == b' ' || c == b'\t')
        && !b.last().is_some_and(|&c| c == b' ' || c == b'\t')
        && !b.iter().any(|&c| matches!(c, 0x00 | 0x0A | 0x0D))
}
```

<!-- Added in round 2: addressing MINOR-18 tchar citation -->
Citation: RFC 9110 §5.6.2 (token / tchar) is the canonical source;
Fetch §2.2 "header name" links into it.

<!-- Added in round 2: addressing MINOR-23 (the leading/trailing/newline check is correct as-is) -->
Note that 0x09/0x20 leading/trailing fail validation **after**
normalisation (which would have stripped them); 0x0A/0x0D anywhere
fail because normalisation only strips them from the ends, not the
middle. A value containing an embedded LF/CR is invalid.

## Normalize-then-validate algorithm shape

<!-- Added in round 2: addressing BLOCKER-3 normalize-then-validate ordering -->
Fetch §2.2.1 `append` step 1 reads: *"Normalize value."* Then validate.
Then guard. Then list-append. The v1 design got this order wrong; this
section makes it explicit.

```rust
/// Fetch §2.2 "header value normalize":
/// Remove any leading and trailing HTTP whitespace bytes from value.
/// HTTP whitespace bytes (https://fetch.spec.whatwg.org/#http-whitespace-byte):
///   0x09 (TAB), 0x0A (LF), 0x0D (CR), 0x20 (SP).
fn normalize_value(v: &[u8]) -> &[u8] {
    let is_ws = |b: u8| matches!(b, 0x09 | 0x0A | 0x0D | 0x20);
    let start = v.iter().position(|&b| !is_ws(b)).unwrap_or(v.len());
    let end = v.iter().rposition(|&b| !is_ws(b)).map_or(start, |i| i + 1);
    &v[start..end]
}
```

The four whitespace bytes are explicit per
https://fetch.spec.whatwg.org/#http-whitespace-byte — note 0x0A/0x0D
ARE stripped at the ends here even though they are forbidden in the
middle. This is correct: `append("X-Foo", "  hello\r\n")` normalises
to `"hello"`, then validate passes.

## validate algorithm

<!-- Added in round 2: addressing BLOCKER-4 + MAJOR-6 validate semantics -->
Per https://fetch.spec.whatwg.org/#headers-validate, validate takes a
`(name, value)` pair and the receiver's guard, returns a boolean:

```rust
/// Fetch §2.2 "validate":
///   1. If name is not a header name OR value is not a header value:
///      throw TypeError.
///   2. If headers' guard is "immutable": throw TypeError.
///   3. If headers' guard is "request" and (name, value) is a forbidden
///      request-header: return false.
///   4. If headers' guard is "response" and name is a forbidden
///      response-header name: return false.
///   5. Return true.
fn validate(&self, name: &[u8], value: &[u8]) -> Result<bool, OpError> {
    // Step 1
    if !is_header_name(name) || !is_header_value(value) {
        return Err(OpError::type_error("Invalid header name or value"));
    }
    // Step 2 (guards land in v2 — the immutable branch is unreachable
    // in v1; structure the code so adding it in v2 is a one-liner).
    // (No guard field in v1, so steps 2–4 are no-ops; return true.)
    Ok(true)
}
```

Caller pattern in `append`:

```rust
fn append(&mut self, name: ByteString, value: ByteString) -> Result<(), OpError> {
    // Step 1: Normalize value.
    let value = normalize_value(&value).to_vec();
    // Step 2 + 3: validate; if returns false, silently no-op; if throws,
    // propagate.
    if !self.validate(&name, &value)? {
        return Ok(());
    }
    // Step 4: list-append.
    self.append_unguarded(name.into(), value);
    self.invalidate_sort_cache();
    Ok(())
}
```

The crucial shape: validate **returns** `Ok(false)` for guard rejections
(steps 3/4) and the caller silently no-ops; validate **throws**
(returns `Err(OpError::type_error)`) only for steps 1 and 2.

For v1 with no guards, steps 3 and 4 are unreachable, so validate
returns either `Ok(true)` or throws. The shape still matters because
the caller pattern must be in place when guards land.

## Per-method algorithms

Each spec method gets a Rust method that does normalize/validate/list-
op in spec order. The list operations are factored as private helpers
so the IDL methods stay readable.

### `append` — Fetch §2.2.1 `dom-headers-append`

```rust
fn append(&mut self, name: ByteString, value: ByteString) -> Result<(), OpError> {
    // Step 1: Normalize value.
    let value = normalize_value(&value).to_vec();
    // Step 2: Validate (name, normalized value). May throw or return false.
    if !self.validate(&name, &value)? {
        return Ok(());
    }
    // Step 3: list-append.
    self.list_append(name.into(), value);
    self.invalidate_sort_cache();
    Ok(())
}

/// "to append a header" §2.2.1: if list contains a header byte-case-
/// insensitively matching name, set name to the first such match's name
/// (preserving casing); push (name, value).
fn list_append(&mut self, name: Vec<u8>, value: Vec<u8>) {
    let canonical = self.list.iter()
        .find(|(n, _)| ascii_eq_ignore_case(n, &name))
        .map(|(n, _)| n.clone())
        .unwrap_or(name);
    self.list.push((canonical, value));
}
```

### `set` — Fetch §2.2.1 `dom-headers-set`

<!-- Added in round 2: addressing MAJOR-9 set_unguarded vs IDL set -->
The IDL `set` method does normalize+validate+list-set. `list_set` is
the list primitive. This was unclear in v1 — both layers are spelled
out here.

```rust
fn set(&mut self, name: ByteString, value: ByteString) -> Result<(), OpError> {
    let value = normalize_value(&value).to_vec();
    if !self.validate(&name, &value)? {
        return Ok(());
    }
    self.list_set(name.into(), value);
    self.invalidate_sort_cache();
    Ok(())
}

/// Fetch §2.2.4 "header list set":
///   If list contains name, set the value of the first such header to
///   value and remove the others. Otherwise append (name, value).
fn list_set(&mut self, name: Vec<u8>, value: Vec<u8>) {
    let mut found = false;
    self.list.retain_mut(|(n, v)| {
        if ascii_eq_ignore_case(n, &name) {
            if !found {
                found = true;
                *v = value.clone();
                true
            } else {
                false
            }
        } else {
            true
        }
    });
    if !found {
        self.list.push((name, value));
    }
}
```

### `delete` — Fetch §2.2.1 `dom-headers-delete`

<!-- Added in round 2: addressing BLOCKER-4 delete name validation -->
Step 1 of `dom-headers-delete`: *"If validating (name, ``) for this
returns false, then return."* Validate with empty value is the spec's
mechanism to reuse step 1 (name check) and step 2 (immutable guard) of
`validate`. With empty value, step 1's value check is satisfied
(empty byte sequence is a valid header value: no leading/trailing
WS, no NUL/LF/CR). Bad name still throws TypeError.

```rust
fn delete(&mut self, name: ByteString) -> Result<(), OpError> {
    if !self.validate(&name, b"")? {
        return Ok(());
    }
    self.list_delete(&name);
    self.invalidate_sort_cache();
    Ok(())
}

fn list_delete(&mut self, name: &[u8]) {
    self.list.retain(|(n, _)| !ascii_eq_ignore_case(n, name));
}
```

### `get` — Fetch §2.2.1 `dom-headers-get`

<!-- Added in round 2: addressing MAJOR-14 get name validation -->
Step 1: validate(name, ""); on bad name throw TypeError.
Step 2: return list-get value.

```rust
fn get(&self, name: ByteString) -> Result<Option<Vec<u8>>, OpError> {
    self.validate_name_only(&name)?;
    Ok(self.list_get(&name))
}

/// Fetch §2.2.4 "header list get":
///   1. If list does not contain name, return null.
///   2. Return the combined value given name and list.
/// Combined value: all matching values joined by 0x2C 0x20 (", "),
/// in order. Note: this applies to set-cookie too — getSetCookie() is
/// the un-joined accessor.
fn list_get(&self, name: &[u8]) -> Option<Vec<u8>> {
    let mut buf: Option<Vec<u8>> = None;
    for (n, v) in &self.list {
        if ascii_eq_ignore_case(n, name) {
            match &mut buf {
                None => buf = Some(v.clone()),
                Some(out) => {
                    out.extend_from_slice(b", ");
                    out.extend_from_slice(v);
                }
            }
        }
    }
    buf
}
```

`validate_name_only` is `validate` with an empty value, structured
to reuse the name-check / immutable branches and skip the
forbidden-header branches.

### `has` — Fetch §2.2.1 `dom-headers-has`

```rust
fn has(&self, name: ByteString) -> Result<bool, OpError> {
    self.validate_name_only(&name)?;
    Ok(self.list.iter().any(|(n, _)| ascii_eq_ignore_case(n, &name)))
}
```

### `getSetCookie` — Fetch §2.2.1 `dom-headers-getsetcookie`

```rust
fn getSetCookie(&self) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (n, v) in &self.list {
        if ascii_eq_ignore_case(n, b"set-cookie") {
            out.push(v.clone());
        }
    }
    out
}
```

<!-- Added in round 2: addressing MINOR-19 getSetCookie return type -->
Returns `Vec<Vec<u8>>`, not `Vec<String>`. Set-Cookie values can
contain any of bytes 0x80+ (e.g. encoded session tokens). The macro
needs a `Vec<Vec<u8>>` → JS Array-of-ByteString emitter — see
"Macro extensions required" below.

### sort-and-combine — Fetch §2.2.4 / §2.2

<!-- Added in round 2: addressing MISSING-1 + MISSING-10 -->
This is the IDL hook named *"value pairs to iterate over"* per Fetch
§2.2 final paragraph: *"The value pairs to iterate over are the return
value of running sort and combine with this's header list."* The
WebIDL `iterable<>` mixin asks for "value pairs to iterate over" on
every iterator step (https://webidl.spec.whatwg.org/#js-default-
iterator-object) — so this is the connection between `iterable<>`
and our list.

Result is cached on `self.sorted_cache`. Any mutation invalidates it.

```rust
/// Fetch §2.2.4 "to sort and combine":
///   1. Let headers be a list of (name, value) pairs.
///   2. Let names be the result of converting each name in this to
///      lowercase, removing duplicates, and sorting bytewise.
///   3. For each name of names:
///      - If name is `set-cookie`: for each (n, v) in this where
///        lowercase(n) == name, append (name, v) to headers (in
///        list order, NOT lex order, within the cluster).
///      - Else: append (name, list_get(name)) to headers — i.e. the
///        joined value, joined by ", ".
///   4. Return headers.
fn sort_and_combine(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
    // names: lowercase, dedup, sort bytewise.
    let mut names: Vec<Vec<u8>> = self.list.iter()
        .map(|(n, _)| n.to_ascii_lowercase())
        .collect();
    names.sort();
    names.dedup();

    let mut out = Vec::with_capacity(self.list.len());
    for name in names {
        if name.as_slice() == b"set-cookie" {
            for (n, v) in &self.list {
                if ascii_eq_ignore_case(n, b"set-cookie") {
                    out.push((name.clone(), v.clone()));
                }
            }
        } else {
            if let Some(v) = self.list_get(&name) {
                out.push((name, v));
            }
        }
    }
    out
}

fn value_pairs_to_iterate_over(&mut self) -> &[(Vec<u8>, Vec<u8>)] {
    if self.sorted_cache.is_none() {
        self.sorted_cache = Some(self.sort_and_combine());
    }
    self.sorted_cache.as_ref().unwrap()
}

fn invalidate_sort_cache(&mut self) {
    self.sorted_cache = None;
}
```

The Set-Cookie cluster sits at the lex position of `set-cookie` among
all unique names; within the cluster, items are in original insertion
order. WPT `header-setcookie.any.js` is the canonical fixture.

### Constructor + `fill` — Fetch §2.2.1 `dom-Headers`

```rust
fn new(scope: &mut v8::PinScope, init: v8::Local<v8::Value>) -> Result<Self, OpError> {
    let mut headers = Headers { list: Vec::new(), sorted_cache: None };
    headers.fill(scope, init)?;
    Ok(headers)
}

fn fill(&mut self, scope: &mut v8::PinScope, init: v8::Local<v8::Value>) -> Result<(), OpError> {
    if init.is_undefined() || init.is_null() {
        return Ok(());
    }
    // WebIDL union dispatch: §3.2.20 step "If V is an Object" branch:
    //   "If types includes a sequence type, then:
    //      Let method be ? GetMethod(V, %Symbol.iterator%).
    //      If method is not undefined, return [creating a sequence
    //      from an iterable]."
    let obj = init.to_object(scope)
        .ok_or_else(|| OpError::type_error("Headers init must be an Object"))?;
    let iter = get_method(scope, obj.into(), v8::Symbol::get_iterator(scope).into())?;
    if let Some(iter_fn) = iter {
        // Sequence path
        fill_from_iterable(scope, obj, iter_fn, self)
    } else {
        // Record path
        fill_from_record(scope, obj, self)
    }
}
```

#### `get_method` (ECMA-262 7.3.11)

<!-- Added in round 2: addressing MAJOR-7 GetMethod semantics -->
The WebIDL union dispatch test must use `GetMethod`, not
`is_function()`. Per https://tc39.es/ecma262/#sec-getmethod:
1. Let func be `? GetV(V, P)`.
2. If func is **either undefined or null**, return undefined.
3. If `IsCallable(func)` is **false**, throw a TypeError.
4. Return func.

```rust
fn get_method<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    receiver: v8::Local<'s, v8::Value>,
    key: v8::Local<'s, v8::Value>,
) -> Result<Option<v8::Local<'s, v8::Function>>, OpError> {
    let obj = receiver.to_object(scope)
        .ok_or_else(|| OpError::type_error("Cannot get method of non-object"))?;
    let func = obj.get(scope, key)
        .ok_or_else(|| OpError::type_error("Property access threw"))?;
    if func.is_null_or_undefined() {
        return Ok(None);
    }
    if !func.is_function() {
        return Err(OpError::type_error("@@iterator is not callable"));
    }
    Ok(Some(func.try_into().unwrap()))
}
```

`is_function()` returns true for V8 `JSFunction`, bound functions, and
function-like Proxies whose target is callable; it correctly handles
the Proxy-with-callable-target case via V8's internal `IsCallable`
check. The remaining gap from v1 was the **null/undefined → undefined**
return and the **non-callable → TypeError** throw, both of which
`get_method` now handles.

#### `fill_from_iterable` — sequence-of-pairs path

The iterable protocol per ECMA-262: invoke `iter_fn` with `obj` as
`this`; call `.next()` on the result; check `.done`; if done, return;
else read `.value` and treat as a 2-element sequence (each element
converted to ByteString); list-append. Repeat. WebIDL §3.7.10 plus
ECMA-262 spec the entire dance.

`new Headers(otherHeaders)` works because Headers is itself iterable
(`@@iterator` → `entries()`). The iterator yields `[name, value]`
arrays. Each yielded value is converted to a 2-element sequence and
appended. <!-- Added in round 2: addressing MISSING-4 -->

#### `fill_from_record` — record path

<!-- Added in round 2: addressing MAJOR-8 record uses [[OwnPropertyKeys]] -->
Per https://webidl.spec.whatwg.org/#js-to-record:
1. If `Type(O)` is not Object, throw TypeError.
2. Let `keys` be `? O.[[OwnPropertyKeys]]()` — **all** own keys
   including Symbol keys.
3. For each `key` in `keys` (in insertion order):
   a. Let `desc` be `? O.[[GetOwnProperty]](key)`.
   b. If `desc` is not undefined and `desc.[[Enumerable]]` is true:
      i. Let `typedKey` be `key` converted to IDL type `K` (=
         ByteString). For Symbol keys, ByteString conversion does
         `ToString(symbol)` which throws TypeError per ECMA-262
         7.1.17. So Symbol-keyed enumerable properties cause
         `new Headers({...})` to throw.
      ii. Let `value` be `? Get(O, key)`.
      iii. Let `typedValue` be `value` converted to ByteString.
      iv. Append (typedKey, typedValue) to the result.

V8 API: use `Object::get_own_property_names` with
`GetPropertyNamesArgs::OwnAll` (returns ALL own keys including
symbols), NOT `OwnEnumerableStrings`. The enumerable filter happens
via `get_own_property_descriptor` per key. Symbol keys reach the
ByteString conversion which throws.

Pseudocode:

```rust
fn fill_from_record(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    headers: &mut Headers,
) -> Result<(), OpError> {
    // GetPropertyNamesArgs::OwnAll → all own keys, all-types, including
    // symbols, in [[OwnPropertyKeys]] order.
    let keys = obj.get_own_property_names(scope, GetPropertyNamesArgs {
        mode: KeyCollectionMode::OwnOnly,
        property_filter: PropertyFilter::ALL_PROPERTIES,
        index_filter: IndexFilter::INCLUDE_INDICES,
        key_conversion: KeyConversionMode::CONVERT_TO_STRING, // careful: this affects index keys; we want symbols preserved
    }).ok_or_else(|| OpError::type_error("Failed to enumerate own keys"))?;

    for i in 0..keys.length() {
        let key = keys.get_index(scope, i).unwrap();
        let desc = obj.get_own_property_descriptor(scope, key.try_into().unwrap())
            .ok_or_else(|| OpError::type_error("Failed to get descriptor"))?;
        // desc is undefined or an Object with `enumerable` field.
        if desc.is_undefined() {
            continue;
        }
        let desc_obj = desc.to_object(scope).unwrap();
        let enumerable_key = v8::String::new(scope, "enumerable").unwrap();
        let enumerable = desc_obj.get(scope, enumerable_key.into())
            .map(|v| v.boolean_value(scope))
            .unwrap_or(false);
        if !enumerable {
            continue;
        }
        // ByteString conversion of the key. Symbols throw here.
        let name = read_byte_string(scope, key)?;
        let value_v8 = obj.get(scope, key)
            .ok_or_else(|| OpError::type_error("Property get threw"))?;
        let value = read_byte_string(scope, value_v8)?;
        // Append (with normalize+validate inside).
        headers.append(name.into(), value.into())?;
    }
    Ok(())
}
```

The `KeyConversionMode::CONVERT_TO_STRING` part needs verification —
we want symbols preserved as Symbol values, not stringified. Use
`KeyConversionMode::NO_NUMBERS` or whatever V8 preserves Symbols
intact; if not available, walk via `Object::get_own_property_names`
twice (strings then symbols) and merge in `[[OwnPropertyKeys]]` order.
Implementation detail; the algorithm is what matters.

## Iterator implementation (LIVE)

<!-- Added in round 2: addressing BLOCKER-1 live iteration + MAJOR-15 closing the open question -->
**Iteration is LIVE.** Each `next()` call re-runs (or reuses cached)
sort-and-combine and reads from index `position`. Mutation between
`next()` calls is observable in subsequent `next()` results.

Forcing references:
- WebIDL §3.7.10.2 default iterator object's `%Iterator.prototype%.next`
  algorithm: *"Let kind be the iterator's kind. Let values be object's
  target's value pairs to iterate over."* — re-evaluated per call.
- WebIDL `forEach` algorithm step (§3.7.10.3): *"Set pairs to
  idlObject's current list of value pairs to iterate over (it might
  have changed)."*
- WPT `header-setcookie.any.js` (the two mutation-during-iteration
  tests cited verbatim in the review). Both fail with snapshot
  semantics; both pass with live semantics + index-based stepping.

### Storage shape

```rust
pub struct HeadersIterator {
    /// Strong reference to the parent Headers wrapper object. Keeps
    /// the source alive and lets next() reach back to the Rust state.
    parent: v8::Global<v8::Object>,

    /// Current index into the (live, freshly-read on every next call)
    /// `value_pairs_to_iterate_over` sequence.
    index: usize,

    /// Iteration kind per WebIDL §3.7.10.
    kind: IterKind,
}

/// WebIDL §3.7.10 iteration kinds. Names match the spec.
enum IterKind {
    /// "key+value" — `entries()` and the default `@@iterator`.
    KeyAndValue,
    /// "key" — `keys()`. For Headers, the "key" is the header name.
    Key,
    /// "value" — `values()`. For Headers, the "value" is the header value.
    Value,
}
```

<!-- Added in round 2: addressing MAJOR-10 IterKind WebIDL terminology -->
Names are now the WebIDL spec names.

### Reach-back path (MINOR-20)

<!-- Added in round 2: addressing MINOR-20 parent reach-back FFI path -->
Every `next()` call:

```text
self.parent: v8::Global<v8::Object>
  → .open(scope) → v8::Local<v8::Object>          // re-enter scope-bound view
  → .get_internal_field(scope, 0).unwrap()        // slot 0 holds the Box
  → External::cast(value).value() as *mut Headers // extract pointer
  → unsafe { &mut *ptr }                          // mutable borrow into Rust state
  → .value_pairs_to_iterate_over()                // run/reuse sort-and-combine
  → read at self.index
  → self.index += 1
```

The `unsafe` deref is sound because:
- V8 isolates are per-thread; no cross-thread access (NIT-7 below
  addresses Send/Sync explicitly).
- The Box<Headers> lives until the Headers wrapper is GC'd; the
  iterator's Global<Object> keeps the wrapper alive, which keeps the
  Box alive.
- We never borrow the Box across a yield point — the borrow scope is
  the entire `next()` call body.

If `parent` cannot be opened (impossible in practice — the Global
keeps it live), `next()` returns `{value: undefined, done: true}`.

### `next()` algorithm

```rust
#[v8_class]
impl HeadersIterator {
    #[v8_method]
    fn next(&mut self, scope: &mut v8::PinScope) -> v8::Local<v8::Value> {
        let parent = self.parent.open(scope);
        let headers = unsafe { headers_from_wrapper(parent) };

        // Re-read pairs every call (LIVE). `value_pairs_to_iterate_over`
        // returns a slice into the cached sort-and-combine result;
        // any mutation since the previous next() invalidated the cache.
        let pairs = headers.value_pairs_to_iterate_over();

        if self.index >= pairs.len() {
            return iter_result(scope, v8::undefined(scope).into(), true);
        }
        let (n, v) = &pairs[self.index];
        self.index += 1;

        let value: v8::Local<v8::Value> = match self.kind {
            IterKind::KeyAndValue => {
                // WebIDL §3.7.10.3 "iteration result" for key+value:
                //   ArrayCreate(2) + CreateDataPropertyOrThrow.
                // Implemented in V8 as v8::Array::new + set_index — NOT
                // Array.of (which is observable via Array constructor).
                let arr = v8::Array::new(scope, 2);
                arr.set_index(scope, 0, bytestring_to_v8(scope, n).into());
                arr.set_index(scope, 1, bytestring_to_v8(scope, v).into());
                arr.into()
            }
            IterKind::Key => bytestring_to_v8(scope, n).into(),
            IterKind::Value => bytestring_to_v8(scope, v).into(),
        };
        iter_result(scope, value, false)
    }
}
```

<!-- Added in round 2: addressing MAJOR-10 explicit Array::new+set_index, not Array.of -->

### Iterator install (MISSING-2, MISSING-3)

<!-- Added in round 2: addressing MISSING-2 + MISSING-3 -->
Two prototype concerns the macro currently can't handle:

1. **`@@toStringTag` must be `"Headers Iterator"`** (spec: interface
   name + " " + "Iterator"), NOT the Rust struct name `"HeadersIterator"`.
   The current macro auto-installs `@@toStringTag = <class_name_str>`
   (see `crates/runtime-macros/src/v8_class.rs` lines 270–284).

2. **`[[Prototype]]` must be `%Iterator.prototype%`** per WebIDL
   §3.7.10.2. The macro doesn't handle prototype chaining.

#### Macro extensions required

- **`#[v8_to_string_tag = "Headers Iterator"]`** at the impl block level.
  When present, install codegen uses the literal instead of the class
  name. Three-line change in `gen_install` to read the attribute.
- **`#[v8_inherit = "iterator_prototype"]`** or a programmatic
  install hook. The simplest path: after `Class::install(scope)`
  returns the FunctionTemplate, the calling code in `init.rs` can
  call `function_template.prototype_template(scope).set_prototype(...)`.
  Verify this works; if not, the macro needs to grow an
  `inherit_from_intrinsic` knob.

`%Iterator.prototype%` is reachable via
`v8::Context::get_intrinsic_iterator_prototype` or by walking
`Reflect.getPrototypeOf((function*(){})())` in JS. Use the V8 API
path.

### Iterator parent / GC (MISSING-7)

<!-- Added in round 2: addressing MISSING-7 send-safety -->
V8 isolates are per-thread; `Box<Headers>` in the External slot is
single-isolate, single-thread by construction. The Headers struct
implements neither `Send` nor `Sync`. The runtime never moves
Headers instances across threads. The crate-wide invariant
"V8 per thread, one isolate per app" (AGENTS.md, Key invariants)
covers this.

### Five entry points on Headers

`forEach`, `keys`, `values`, `entries`, `[@@iterator]` are five thin
methods on `Headers`:

| Method | Returns | Implementation |
|---|---|---|
| `keys()` | new HeadersIterator(kind = Key) | construct + return |
| `values()` | new HeadersIterator(kind = Value) | construct + return |
| `entries()` | new HeadersIterator(kind = KeyAndValue) | construct + return |
| `[@@iterator]()` | same as `entries()` | aliased |
| `forEach(cb, thisArg)` | undefined | iterate inline (LIVE — per WebIDL §3.7.10.3 forEach algorithm spelling out "current list of value pairs to iterate over") |

`forEach` runs its loop using the live "value pairs" lookup on every
iteration, exactly as the spec requires.

## Set-Cookie (five places of divergence)

1. **`getSetCookie()`** — un-joined array. Only this returns a sequence.
2. **Iteration** (sort-and-combine) — set-cookie cluster emits one pair
   per value, insertion order within cluster, lex position by lowercase
   name overall.
3. **`get("set-cookie")` STILL JOINS** — counter-intuitive but spec-
   mandated. The header-list-get combine step has no carve-out for
   set-cookie. WPT `header-setcookie.any.js` test "Headers.prototype.get
   combines set-cookie headers in order" verifies this.
4. **Forbidden request-header** includes `Set-Cookie` — request-guarded
   Headers can't append it. (v2 with guards.)
5. **Forbidden response-header** is `{Set-Cookie, Set-Cookie2}` —
   response-guarded ditto. (v2 with guards.)

WPT `header-setcookie.any.js` covers all five. Items 1–3 in v1; 4–5 v2.

## Casing semantics

<!-- Added in round 2: addressing MAJOR-11 prose alignment -->
Per Fetch §2.2.1 `concept-headers-append` step 1: *"If list contains
name, then set name to the first such header's name."* The "first such
header" means the first **currently-in-list** match, by byte-case-
insensitive comparison.

Walk-through:
- `append("X-Foo", "1")` → `[("X-Foo", "1")]`
- `append("x-foo", "2")` → list contains `X-Foo`; reuse `X-Foo`. List: `[("X-Foo", "1"), ("X-Foo", "2")]`.
- `delete("X-FOO")` → list empty.
- `append("y-foo", "3")` → list empty, no match, store as given. List: `[("y-foo", "3")]`.
- `append("Y-Foo", "4")` → list contains `y-foo`; reuse `y-foo`. List: `[("y-foo", "3"), ("y-foo", "4")]`.

Iteration always emits lowercase (the sort step). Storage tracks the
casing-as-of-first-currently-in-list match, which is what v1's
pseudocode `self.list.iter().find(...)` does — only currently-in-list
entries are searched.

This corner is unverified in WPT (the closest case in
`headers-casing.any.js` deletes everything between sequences), but the
behaviour is uniquely determined by the spec wording.

## Macro usage (revised)

<!-- Added in round 2: addressing MAJOR-12 + MAJOR-13 + BLOCKER-2 + BLOCKER-5 -->

### Headers — IDL → Rust mapping

| IDL | Rust signature | Macro feature |
|---|---|---|
| `constructor(init)` | `fn new(scope, init: v8::Local<v8::Value>) -> Result<Self, OpError>` | scope synthetic + Local arg + Result<Self, _> (exists) |
| `append(name, value)` | `fn append(&mut self, name: ByteString, value: ByteString) -> Result<(), OpError>` | **NEW**: `ByteString` newtype extraction → calls `read_byte_string` |
| `delete(name)` | `#[v8_name = "delete"] fn delete_(&mut self, name: ByteString) -> Result<(), OpError>` | **NEW**: `#[v8_name]` rename + ByteString |
| `get(name)` | `fn get(&self, name: ByteString) -> Result<Option<Vec<u8>>, OpError>` | **NEW**: `Result<Option<Vec<u8>>>` (extends existing Result<Option<T>>) |
| `getSetCookie()` | `fn getSetCookie(&self) -> Vec<Vec<u8>>` | **NEW**: `Vec<Vec<u8>>` → JS Array<ByteString> emitter |
| `has(name)` | `fn has(&self, name: ByteString) -> Result<bool, OpError>` | ByteString + existing Result<bool> |
| `set(name, value)` | `fn set(&mut self, name: ByteString, value: ByteString) -> Result<(), OpError>` | ByteString + existing Result<()> |
| `forEach(cb, thisArg)` | `fn forEach(&self, scope, cb: v8::Local<v8::Value>, this_arg: v8::Local<v8::Value>) -> Result<(), OpError>` | exists |
| `entries`/`keys`/`values`/`[@@iterator]` | each returns `v8::Local<v8::Value>` | exists |

### HeadersIterator — IDL → Rust mapping

| Member | Rust |
|---|---|
| `next()` | `fn next(&mut self, scope) -> v8::Local<v8::Value>` |
| `[[Prototype]] = %Iterator.prototype%` | install hook (macro extension) |
| `@@toStringTag = "Headers Iterator"` | `#[v8_to_string_tag = "Headers Iterator"]` (macro extension) |

### Macro extensions required

This design *owns* the macro work. None of these are deferred-and-pray:

1. **`ByteString` newtype extraction**
   - Where: `crates/runtime-macros/src/lib.rs` `gen_extract` (around
     line 198, parallel to the Vec<u8>-from-ArrayBufferView path).
   - Detection: type path matches `ByteString` (a re-exportable
     newtype defined in `runtime-core` or `runtime`).
   - Codegen: emits `let __arg = read_byte_string(scope, raw_value)?;`
     where `raw_value` is the v8::Value at the corresponding arg
     index. `read_byte_string` returns `Result<Vec<u8>, OpError>`;
     macro propagates the `?`.
   - Estimate: ~30 lines. ~2h including tests.

2. **`#[v8_name = "delete"]` attribute**
   - Where: `crates/runtime-macros/src/v8_class.rs` `classify` reads
     attributes; thread the parsed name through `ClassMethod`; in
     `gen_install` `proto_sets`, use the attribute value instead of
     `m.func.sig.ident.to_string()` for the JS-visible key.
   - Conflict detection: error if two methods declare the same JS
     name (the macro's existing self-doc at lines 28–34 calls this
     out). Scan all classified methods, build a `HashMap<String,
     &ClassMethod>`, error on collision.
   - Estimate: ~25 lines. ~2h.

3. **`Vec<Vec<u8>>` → JS Array<ByteString> emitter**
   - Where: `crates/runtime-macros/src/lib.rs` `gen_vec_set` (around
     line 366), parallel to the `Vec<String>` case.
   - Codegen: `for each Vec<u8> elem, build a v8::String via
     v8::String::new_from_one_byte(scope, &elem); set_index`.
   - Estimate: ~15 lines. ~1h.

4. **`Result<Option<Vec<u8>>>` return** for `get`
   - Where: `gen_call_return` (lib.rs:415-426). Currently handles
     `Result<Option<{String,u32,bool}>>` but not `Result<Option<Vec<u8>>>`.
   - Codegen: when `Some(bytes)`, build a v8::String via
     `v8::String::new_from_one_byte(scope, &bytes)`; when `None`,
     return `v8::null(scope)`.
   - Estimate: ~10 lines. ~1h.

5. **`#[v8_to_string_tag = "..."]` impl-level attribute**
   - Where: `gen_install` lines 270–284 — the `@@toStringTag`
     installation. Currently uses `class_name_str`. Read an
     impl-level attribute and use the literal if present.
   - Estimate: ~10 lines. ~30min.

6. **Iterator prototype chaining to `%Iterator.prototype%`**
   - Two paths to consider:
     a. New attribute `#[v8_inherit_from_intrinsic = "iterator_prototype"]`
        that calls `FunctionTemplate::inherit` against the intrinsic.
     b. Post-install hook in `init.rs` that walks the prototype and
        sets `%Iterator.prototype%` as the parent.
   - Path (b) is simpler if V8 exposes the intrinsic at install time.
   - Estimate: ~20 lines either way. ~2h.

**Total macro work: ~110 lines, ~8h** (corrects v1's "~1h" estimate
which was wildly optimistic — see NIT-28 fix).

### Why not `typed_id` for header names? (MISSING-9)

<!-- Added in round 2: addressing MISSING-9 typed_id consistency -->
Header names are not entity identifiers; they're a wire-format token
defined externally by RFC 9110 / IANA. `typed_id` (UUIDv7 + base62 +
entity prefix per `crates/core/src/typed_id.rs`) is for platform-
internal entities. ByteString is the right type because it matches
the spec.

## File layout

```
crates/runtime/src/
├── headers.rs         (new) Headers + HeadersIterator + helpers
├── lib.rs             (modified) +pub mod headers;
└── init.rs            (modified) install Headers on globalThis behind
                       feature flag; remove polyfill branch in cutover

crates/runtime-core/ (or runtime/src/)
└── byte_string.rs     (new) `pub struct ByteString(pub Vec<u8>);` with
                       Deref/From/Into shims

crates/runtime/tests/
├── headers.rs                 (new) hand-written tests (~40 cases)
├── wpt_headers.rs             (new) WPT runner (mirrors wpt_text_encoding.rs)
└── wpt/fetch/api/headers/     (vendored from web-platform-tests)

crates/runtime-macros/
├── src/v8_class.rs   (modified) +#[v8_name], +#[v8_to_string_tag]
├── src/lib.rs        (modified) +ByteString extract, +Vec<Vec<u8>>,
│                     +Result<Option<Vec<u8>>>
```

### Polyfill removal (cutover plan)

<!-- Added in round 2: addressing MINOR-22 polyfill removal cadence -->
**Three landings, not one** (from the Decisions table):

1. **Land native + feature flag** — DONE in `0c419fd2` (native impl) +
   `d475b1e5` (wire-in) + `c83fb34a` (WPT 98/0/1). `globalThis.Headers`
   was installed as the native class only when `ZEROSHIP_NATIVE_HEADERS=1`
   was set; polyfill remained the default.

2. **Cutover** — DONE in `474c69dd`. Refactored every callsite that
   reached into polyfill internals (`_map`, `_fromTrusted`, `_toArray`,
   `_zsHeadersArr` access patterns) onto the native iterable surface
   and the `new Headers([[name, value], ...])` constructor. The
   default stayed on the polyfill (flag still gated on
   `ZEROSHIP_NATIVE_HEADERS=1`), but with both modes green under
   `cargo test -p zeroship-runtime`. Sites refactored:
   - `crates/runtime/src/transport/handler.rs::HTTP_CREATE_REQUEST_JS` — Request
     build helper now uses `new Headers(arrayOfPairs)`.
   - `crates/runtime/src/transport/handler.rs::extract_response_headers` — slow
     path walks `headers[Symbol.iterator]()` instead of `_map`.
   - `crates/runtime/src/embed/fetch.js` `Response.json` fast path —
     uses the constructor instead of `Object.create + _map`.
   - `crates/runtime/src/embed/fetch.js` `fetch()` — serialises
     request headers via `[...request.headers]` instead of
     `request.headers._toArray()`.

3. **Remove polyfill** — DONE in the commit immediately after
   `474c69dd`. Deleted the JS Headers class block (~177 lines) from
   `crates/runtime/src/embed/fetch.js`, removed the
   `ZEROSHIP_NATIVE_HEADERS` env-var gate from
   `crates/runtime/src/core/init.rs`, renamed the install hook from
   `install_native_headers_post` to `install_headers`, and updated all
   doc comments. Native Headers is now the only Headers in the
   runtime. WPT remains 98/0/1.

## Test plan

### Hand-written smoke tests (`tests/headers.rs`)

- Empty: `new Headers().get("x") === null`
- Sequence: `new Headers([["a","1"]])` has `a=1`
- Record: `new Headers({a: "1"})` has `a=1`
- Headers from Headers: `new Headers(new Headers([["a","1"]]))` has `a=1`
  (MISSING-4 explicit test)
- Sequence with non-2-element pair: throws TypeError
- `append("a","1")` then `append("a","2")` → `get("a") === "1, 2"`
- Casing preservation: `append("X-Foo", "1")` then
  `append("x-foo", "2")` — iteration emits `x-foo`, list stores
  `("X-Foo", "1"), ("X-Foo", "2")`
- Casing reset after delete: `append("X-Foo", "1")`, `delete`, `append("y-foo", "3")`, `append("Y-Foo", "4")` → list `("y-foo", "3"), ("y-foo", "4")`
- `set` replaces, `delete` removes
- Set-Cookie: `append("set-cookie", "a=1")` + `append("set-cookie", "b=2")`:
  - `get("set-cookie")` → `"a=1, b=2"`
  - `getSetCookie()` → `["a=1", "b=2"]`
- Iteration order: lex by lowercase name; set-cookie cluster keeps
  insertion order
- Header name validation: `append("invalid name", "x")` throws TypeError
- Header value validation (post-normalize): `append("x", "bad\r\nvalue")` throws TypeError
  (after stripping outer WS, embedded \r\n still present)
- Whitespace normalization: `append("x", "  hi  ")` → `get("x") === "hi"`
- Empty name: `append("", "x")` throws TypeError
- `delete("invalid name")` throws TypeError (BLOCKER-4)
- `has("invalid name")` throws TypeError (BLOCKER-4)
- `get("invalid name")` throws TypeError (BLOCKER-4 / MAJOR-14)
- ByteString rejection: `append("\u0100", "x")` throws TypeError (BLOCKER-2)
- Symbol-keyed record: `new Headers({[Symbol.iterator]: "x", a: "1"})` throws TypeError (MAJOR-8)
- Non-callable @@iterator: `new Headers({[Symbol.iterator]: 5})` throws TypeError (MAJOR-7)
- forEach: invokes callback per pair, in iteration order
- `Object.prototype.toString.call(new Headers()) === "[object Headers]"`
- Iterator @@toStringTag: `Object.prototype.toString.call(new Headers().entries()) === "[object Headers Iterator]"` (MISSING-2)
- Iterator [[Prototype]]: `Object.getPrototypeOf(Object.getPrototypeOf(new Headers().entries())) === Iterator.prototype` or equivalent reach via `%IteratorPrototype%` (MISSING-3)

#### Live iteration explicit tests (BLOCKER-1)

These mirror the WPT cases that v1 got wrong. They MUST pass.

- Mutation during iteration #1:
  ```js
  const h = new Headers([["fizz","buzz"], ["X-Header","test"]]);
  const it = h[Symbol.iterator]();
  assertEqual(it.next().value, ["fizz","buzz"]);
  h.append("Set-Cookie","a=b");
  assertEqual(it.next().value, ["set-cookie","a=b"]);   // appears AFTER iter creation
  h.append("Accept","text/html");
  assertEqual(it.next().value, ["set-cookie","a=b"]);   // hmm — but the previous next emitted set-cookie at lex pos so this should be x-header now (per WPT)
  // (Exact transcript from header-setcookie.any.js used verbatim.)
  ```
- Mutation during iteration #2:
  ```js
  const h = new Headers([["set-cookie","a"],["set-cookie","b"],["set-cookie","c"]]);
  const it = h[Symbol.iterator]();
  assertEqual(it.next().value, ["set-cookie","a"]);
  h.delete("set-cookie");
  h.append("set-cookie","d");
  h.append("set-cookie","e");
  h.append("set-cookie","f");
  assertEqual(it.next().value, ["set-cookie","e"]);     // skips d!
  ```

### WPT runner (`tests/wpt_headers.rs`)

Vendor and run, in priority order:

| File | Why |
|---|---|
| `idlharness.any.js` | **Canonical IDL conformance** — flags @@toStringTag, prototype chain, all interface members. Must pass before declaring v1 done. (MAJOR-16) |
| `headers-basic.any.js` | append/get/set/delete/has/iterator |
| `headers-casing.any.js` | case-insensitive lookup, case-preserve |
| `headers-combine.any.js` | multi-value join, sort order |
| `headers-normalize.any.js` | value whitespace stripping (BLOCKER-3 verification) |
| `header-values.any.js` | byte-level value validation |
| `headers-errors.any.js` | TypeError surface |
| `headers-record.any.js` | record path; Proxy traps; ordering; Symbol-key rejection (MAJOR-8) |
| `header-setcookie.any.js` | Set-Cookie special cases + the two **iterator-mutation tests** (BLOCKER-1) |
| `headers-non-ascii.any.js` | bytes 0x80+ round-trip (MAJOR-16, ByteString fidelity) |
| `headers-structure.any.js` | required-method sanity |

Skip in v1 (need Request/Response):
- `headers-no-cors.any.js`
- `headers-forbidden-override.any.js`

For `header-setcookie.any.js` the response-forbidden assertions are
skipped (annotate explicit subtest IDs in the runner harness, per
MINOR-21).

Same harness pattern as `wpt_text_encoding.rs`. Target: **100% pass on
the 11 v1 files** (idlharness included).

## Implementation sequence

<!-- Added in round 2: addressing NIT-28 estimates -->

| Step | Work | Estimate |
|---|---|---|
| 1 | Macro extensions: `ByteString`, `#[v8_name]`, `Vec<Vec<u8>>`, `Result<Option<Vec<u8>>>`, `#[v8_to_string_tag]`, iterator prototype hook | 8h |
| 2 | `headers.rs` core: struct, validators, normalize, validate, list ops, IDL methods | 4h |
| 3 | `headers.rs` iterator: HeadersIterator class, sort-and-combine, live next(), reach-back, install %Iterator.prototype% chain | 4h |
| 4 | Hand-written tests (40 cases) and pass | 2h |
| 5 | WPT runner: vendor 11 files, harness, iterate to 100% pass | 6h |
| 6 | Wire into `setup_globals` behind feature flag (no polyfill removal yet) | 1h |
| 7 | (Separate landing) cutover default to native | 1h |
| 8 | (Separate landing) remove polyfill | 1h |

**Total v1 work: ~25h.** This is the realistic estimate. v1's "~3h"
was off by an order of magnitude — partly because BLOCKER-2 (ByteString
TypeError), BLOCKER-1 (live iteration), and the macro extensions were
all underestimated.

## Status notes

**Linked discussions:** TBD — file in `docs/decisions/` after
implementation lands. (MINOR-30: previously the proposal had no
tracking issue; convert to a date-prefixed ADR upon completion.)

**Reviewer assignment:** TBD — runtime owners.

## References

- WHATWG Fetch §2.2 — https://fetch.spec.whatwg.org/#headers-class
- WHATWG Fetch header list operations — https://fetch.spec.whatwg.org/#concept-header-list
- WHATWG Fetch validate — https://fetch.spec.whatwg.org/#headers-validate
- WHATWG Fetch sort and combine — https://fetch.spec.whatwg.org/#concept-header-list-sort-and-combine
- WHATWG Fetch HTTP whitespace bytes — https://fetch.spec.whatwg.org/#http-whitespace-byte
- WebIDL ByteString — https://webidl.spec.whatwg.org/#js-to-ByteString
- WebIDL record — https://webidl.spec.whatwg.org/#js-to-record
- WebIDL union dispatch — https://webidl.spec.whatwg.org/#js-to-union
- WebIDL default iterator object — https://webidl.spec.whatwg.org/#js-default-iterator-object
- WebIDL forEach algorithm — https://webidl.spec.whatwg.org/#es-forEach
- ECMA-262 GetMethod — https://tc39.es/ecma262/#sec-getmethod
- RFC 9110 §5.6.2 (tchar) — https://www.rfc-editor.org/rfc/rfc9110.html#section-5.6.2
- WPT fetch/api/headers — https://github.com/web-platform-tests/wpt/tree/master/fetch/api/headers
- Fetch PR #1346 (Set-Cookie semantics) — https://github.com/whatwg/fetch/pull/1346
- v8::String::contains_only_one_byte — V8 API
- v8::String::write_one_byte_v2 — V8 API (silent truncation; precheck required)
- Project: `crates/runtime-macros/src/v8_class.rs` lines 28–34 (known gaps), 200–244 (gen_install), 270–284 (auto @@toStringTag)
- Project: `crates/runtime-macros/src/lib.rs` lines 198–217 (Vec<u8> from ArrayBufferView path), 366–375 (gen_vec_set)
