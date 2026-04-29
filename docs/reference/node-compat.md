# Node.js Compatibility Layer for zeroship V8 Runtime

## Overview

Implement `node:*` module resolution at the V8 runtime level so that npm packages (LangChain, OpenAI SDK, zod, etc.) that import Node.js APIs work transparently in the zeroship runtime. No Vite plugin involvement — the runtime handles it natively, just like workerd and Deno.

## Problem

npm packages import Node.js APIs:
```typescript
import { createHash } from "node:crypto";
import { AsyncLocalStorage } from "node:async_hooks";
import { Buffer } from "node:buffer";
```

The zeroship V8 runtime doesn't resolve `node:*` specifiers. `import("node:crypto")` fails with "Not supported" because V8's module resolve callback doesn't know about these modules.

## How workerd Does It

Studied from `refs/workerd/src/`. Three layers:

### Layer 1: C++ Native Modules

```cpp
// workerd/api/node/node.h
#define NODEJS_MODULES(V)
  V(CryptoImpl,       "node-internal:crypto")
  V(BufferUtil,       "node-internal:buffer")
  V(AsyncHooksModule, "node-internal:async_hooks")
  V(UtilModule,       "node-internal:util")
  V(TimersUtil,       "node-internal:timers")
  // ...

// Registered as:
registry.addBuiltinModule<CryptoImpl>("node-internal:crypto", Type::INTERNAL);
```

Real C++ implementations of crypto operations (Hash, HMAC, AES), buffer manipulation, async context tracking. Compiled into the workerd binary.

### Layer 2: TypeScript Bridge Modules

```typescript
// workerd/src/node/crypto.ts
import { createHash, createHmac } from "node-internal:crypto";  // → C++
export { createHash, createHmac };
export const randomUUID = crypto.randomUUID.bind(crypto);
export const subtle = crypto.subtle;
```

Regular TypeScript files that import from C++ layer and re-export with Node.js-compatible interface. Bundled into `NODE_BUNDLE` at build time.

### Layer 3: V8 Module Resolve Callback

```cpp
// workerd/jsg/modules.c++:42-116
// V8 calls this for every `import` statement
resolveCallback(specifier, referrer) {
    if (isNodeJsCompatEnabled()) {
        if (spec = checkNodeSpecifier(specifier)) {
            // normalize "crypto" → "node:crypto"
        }
    }
    if (spec.startsWith("node:")) {
        return registry.resolve(spec);  // returns pre-registered module
    }
}
```

V8's resolve callback intercepts all import statements. For `node:*` specifiers, it looks up the pre-registered module in the registry. No filesystem, no bundler, no Vite — handled entirely at the runtime level.

## Design for zeroship

Same three-layer architecture, adapted for our Rust + V8 stack:

### Layer 1: Web APIs (already exist)

Our V8 runtime already has:
- `crypto.randomUUID()`, `crypto.getRandomValues()`, `crypto.subtle.digest()` — via WebCrypto
- `ReadableStream`, `WritableStream`, `TransformStream` — via Streams API
- `TextEncoder`, `TextDecoder` — via Encoding API
- `fetch()` — via compio HTTP
- `setTimeout`, `setInterval`, `queueMicrotask` — via event loop
- `URL`, `URLSearchParams` — via URL polyfill

### Layer 2: JS Polyfill Modules

Pure JavaScript modules that bridge Web APIs to the Node.js interface. Embedded in the Rust binary via `include_str!`.

```
crates/runtime/src/node/
  crypto.js       — createHash (pure JS SHA-256), randomBytes, randomUUID
  buffer.js       — Buffer class wrapping Uint8Array
  path.js         — join, resolve, extname, basename, dirname
  async_hooks.js  — AsyncLocalStorage (closure-based)
  util.js         — inspect, format, promisify, deprecate
  events.js       — EventEmitter
  stream.js       — Readable, Writable, Duplex, PassThrough (minimal)
  stream_web.js   — re-export ReadableStream/WritableStream
  timers.js       — setTimeout/setInterval wrappers
  timers_promises.js — setTimeout as Promise
  assert.js       — assert, equal, strictEqual, deepEqual
  process.js      — env, version, platform, nextTick, stdout, stderr
  os.js           — platform, arch, tmpdir, EOL
  url.js          — URL, URLSearchParams, parse, format
  http.js         — METHODS, STATUS_CODES (stub)
  fs.js           — throws "not available" on actual use (stub)
```

Each file is a self-contained ES module using only Web APIs available in V8.

### Layer 3: Module Resolve Callback (Rust)

In `crates/runtime/src/modules.rs`, extend the existing module resolve callback:

```rust
// When V8 encounters: import { createHash } from "node:crypto"
fn resolve_callback(context, specifier, referrer) -> Option<Module> {
    let spec = specifier.to_string();

    // Check if it's a node: specifier
    if let Some(node_module) = resolve_node_module(&spec) {
        return Some(node_module);
    }

    // Existing resolution logic for user modules...
}

fn resolve_node_module(specifier: &str) -> Option<Module> {
    // Normalize: "crypto" → "node:crypto"
    let normalized = if specifier.starts_with("node:") {
        specifier.to_string()
    } else if NODE_MODULES.contains_key(specifier) {
        format!("node:{specifier}")
    } else {
        return None;
    };

    // Look up pre-compiled module
    NODE_MODULE_CACHE.with(|cache| {
        cache.borrow().get(&normalized).cloned()
    })
}
```

### Module Registration (at init)

During `setup_globals()` or `load_polyfills_and_modules()`, pre-compile all node: polyfills:

```rust
// In init.rs, after setup_globals():
let node_modules = [
    ("node:crypto",          include_str!("node/crypto.js")),
    ("node:buffer",          include_str!("node/buffer.js")),
    ("node:path",            include_str!("node/path.js")),
    ("node:async_hooks",     include_str!("node/async_hooks.js")),
    ("node:util",            include_str!("node/util.js")),
    ("node:events",          include_str!("node/events.js")),
    ("node:stream",          include_str!("node/stream.js")),
    ("node:stream/web",      include_str!("node/stream_web.js")),
    ("node:timers",          include_str!("node/timers.js")),
    ("node:timers/promises", include_str!("node/timers_promises.js")),
    ("node:assert",          include_str!("node/assert.js")),
    ("node:assert/strict",   include_str!("node/assert.js")),
    ("node:process",         include_str!("node/process.js")),
    ("node:os",              include_str!("node/os.js")),
    ("node:url",             include_str!("node/url.js")),
    ("node:http",            include_str!("node/http.js")),
    ("node:https",           include_str!("node/http.js")),
    ("node:fs",              include_str!("node/fs.js")),
    ("node:fs/promises",     include_str!("node/fs.js")),
];

for (specifier, source) in node_modules {
    compile_and_register_module(scope, specifier, source);
}
```

### Dynamic Import Support

Also need `set_host_import_module_dynamically_callback` for `import()` expressions:

```rust
isolate.set_host_import_module_dynamically_callback(dynamic_import_callback);

fn dynamic_import_callback(context, referrer, specifier, ...) -> Option<Promise> {
    // Same logic: check node:* → return pre-registered module
    // For user modules: resolve against referrer path
}
```

## Polyfill Implementation Notes

### node:crypto — createHash

The main challenge. WebCrypto's `digest()` is async, but `createHash().digest()` is sync.

**Solution**: Pure JS SHA-256 implementation (~60 lines). Only SHA-256 is needed for LangChain (cache keys, content hashing). Alternatively, use `@noble/hashes` (audited, 0-dep, sync).

```javascript
// node/crypto.js
export function createHash(algorithm) {
    if (algorithm !== "sha256") throw new Error("Only sha256 supported");
    let chunks = [];
    return {
        update(data) { chunks.push(data); return this; },
        digest(encoding) {
            const hex = sha256(chunks.join(""));
            if (encoding === "hex") return hex;
            if (encoding === "base64") return hexToBase64(hex);
            return hexToBytes(hex);
        },
    };
}

export const randomUUID = () => crypto.randomUUID();
export function randomBytes(size) {
    const buf = new Uint8Array(size);
    crypto.getRandomValues(buf);
    return buf;
}
```

### node:buffer — Buffer

Extend `Uint8Array` with Node.js Buffer methods:

```javascript
class Buffer extends Uint8Array {
    static from(input, encoding) { ... }
    static alloc(size, fill) { ... }
    static concat(list) { ... }
    toString(encoding) { ... }  // hex, base64, utf8
}
globalThis.Buffer = Buffer;
```

### node:async_hooks — AsyncLocalStorage

Closure-based implementation (no actual async tracking):

```javascript
class AsyncLocalStorage {
    #store = undefined;
    getStore() { return this.#store; }
    run(store, fn, ...args) {
        const prev = this.#store;
        this.#store = store;
        try { return fn(...args); }
        finally { this.#store = prev; }
    }
}
```

This works for LangChain's context propagation. For full async tracking (across await boundaries), would need V8 PromiseHook integration — not needed now.

## What Changes

### Rust (crates/runtime/)

```
crates/runtime/src/
  modules.rs    — (modify) add node:* resolution in resolve callback
  init.rs       — (modify) register node modules during init
  node/         — (create) directory for polyfill JS files
    crypto.js
    buffer.js
    path.js
    async_hooks.js
    util.js
    events.js
    stream.js
    stream_web.js
    timers.js
    timers_promises.js
    assert.js
    process.js
    os.js
    url.js
    http.js
    fs.js
```

### Vite Plugin (sdks/vite-plugin/)

- Remove `node-compat.ts` — no longer needed
- Remove `fetchModule` override in environment.ts — runtime handles it
- Remove `getBuiltins` empty override in dev-server.ts — not needed

### No New Crate Dependencies

All polyfills are pure JS using Web APIs already available in the runtime.

## Testing

1. **Unit test per module**: `import { createHash } from "node:crypto"` → verify output
2. **LangChain integration**: The original goal — `ChatOpenAI` + `createReactAgent` should work
3. **Verify no regressions**: Existing benchmarks and E2E tests pass

## Implementation Order

1. Create `crates/runtime/src/node/` with polyfill JS files
2. Modify `modules.rs` — add node:* resolution in resolve callback + dynamic import callback
3. Modify `init.rs` — register node modules at init
4. Test with simple `import "node:crypto"` script
5. Test with LangChain demo
6. Remove Vite-side node-compat code

## Scope Exclusions

- **Full Node.js compat** — only what LangChain + OpenAI SDK + zod need
- **node:fs actual filesystem** — stubs only (throws on use)
- **node:net / node:tls / node:child_process** — not needed for LangChain
- **AsyncLocalStorage across await** — simple closure-based impl is sufficient
- **SHA-1, MD5, SHA-512** — only SHA-256 for now (add later if needed)
