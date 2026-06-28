# zeroship Feature Map

zeroship is a platform where anyone can create, launch, and monetize software without
writing code: creators describe an app in natural language, AI builds it, and the
platform runs everything (hosting, database, auth, payments, scaling). This document is
**the** comprehensive, navigable map of every platform feature, assembled from 19 area
catalogs.

## How to read this map

- The **Index** lists all 19 areas with feature counts and a one-line summary; click a
  row to jump to that section.
- Each **area section** has a summary plus a table of its features. Columns: Feature ·
  Status · Surface · Code · Docs · Example · Notes. Code/Docs/Example are repo-relative
  paths (or `—` when absent).
- The **Cross-cutting status rollup** at the bottom totals statuses across the whole
  platform and calls out the notable stub/dead/planned gaps and the consolidated
  documentation gaps.

## Status legend

| Symbol | Status | Meaning |
| --- | --- | --- |
| 🟢 | **shipped** | Implemented, wired, and exercised end-to-end. |
| 🟡 | **partial** | Core works, but a documented sub-capability or wiring is incomplete. |
| 🟠 | **stub** | Surface/scaffold exists; returns not-implemented or is non-effectful. |
| 🔵 | **planned** | Documented/intended; no working code on disk. |
| ⚫ | **dead** | Code/field exists but is unreachable, retired, or superseded. |

---

## Index

| # | Area | Features | Summary |
| --- | --- | --- | --- |
| 1 | [Runtime / WinterCG](#1-runtime--wintercg) | 45 | V8 embedder on compio/io_uring with a full native WinterCG surface plus platform extensions. |
| 2 | [Node.js compatibility](#2-nodejs-compatibility) | 31 | Seven runtime-native synthetic node: modules plus a Vite-layer unenv@2 build tier. |
| 3 | [env.db / @zeroship/db](#3-envdb--zeroshipdb) | 45 | Typed schema-driven database layer: CRUD, search, masking, encryption, transactions, reactivity. |
| 4 | [env.kv / @zeroship/kv](#4-envkv--zeroshipkv) | 31 | Strongly-consistent atomic KV over redb (dev) / Redis (prod) with a typed SDK. |
| 5 | [env.storage / @zeroship/storage](#5-envstorage--zeroshipstorage) | 24 | Object-store CRUD over LocalFs; S3 backend declared but unimplemented. |
| 6 | [Auth (env.auth + IdP + SDK)](#6-auth-envauth--idp--sdk) | 47 | Full OIDC/OAuth identity platform: login UI, federation, 2FA, sessions, GDPR, relay, SDK. |
| 7 | [RPC / server functions](#7-rpc--server-functions) | 47 | Typed server-function stack: discovery, transport, transformers, retries, fail-closed auth. |
| 8 | [Deploy contract / bootstrap](#8-deploy-contract--bootstrap) | 19 | The `default = { fetch?, rpc? }` contract and the framework-internal bootstrap. |
| 9 | [Gateway](#9-gateway) | 41 | Edge layer: manifest dispatch, multi-arm auth, rate/concurrency limits, CHWBL routing. |
| 10 | [Control plane](#10-control-plane-cratescontrol) | 38 | Creator/admin API: app CRUD, deploy ingest, env/secrets, route feeds, billing, admin. |
| 11 | [Authorization (Cedar)](#11-authorization-cedar) | 32 | Cedar policy engine + control-plane AuthzGuard; P10/P11/P12 are documented but unbuilt. |
| 12 | [Billing / Stripe Connect](#12-billing--stripe-connect) | 16 | Stripe Connect ledger end-to-end; usage metering is ingest-only and env.meter is absent. |
| 13 | [Bundle / .zship / Blob Store](#13-bundle--zship--blob-store) | 36 | The .zship artifact, Manifest types, BlobStore trait, LocalDiskBlobStore; no S3. |
| 14 | [Vite plugin / build pipeline](#14-vite-plugin--build-pipeline) | 35 | Single zeroship() plugin: use-server discovery, dev runtime bridge, .zship emit. |
| 15 | [Sandbox / AI build env](#15-sandbox--ai-build-env) | 56 | Isolated microVM build envs: 3 backends, snapshot/restore, preview proxy, HA, admin API. |
| 16 | [Worker](#16-worker) | 26 | V8 execution tier: dispatch, per-thread isolate LRU, version/env reconcile. |
| 17 | [Drivers + core infra](#17-drivers--core-infra) | 51 | compio-postgres, compio-redis, and zeroship-core shared types/crypto/auth/config. |
| 18 | [CLI + developer experience](#18-cli--developer-experience) | 33 | The zeroship binary, create-zeroship-app, and the vite-plugin dev/build loop. |
| 19 | [@zeroship/ui design system](#19-zeroshipui-design-system) | 80 | Governed React design system on Base UI: primitives, layouts, blocks, sections. |
| 20 | [Other SDK packages](#20-other-sdk-packages) | 8 | The remaining published `@zeroship/*` packages: React db-bindings, eslint-config, ambient types, the `zeroship` stub. |

---

## 1. Runtime / WinterCG

V8 embedder (no tokio; compio/io_uring) running creator code in per-thread, per-app
isolates. Provides a comprehensive native WinterCG surface — fetch, Request/Response/Headers,
Streams, WebCrypto, WebSocket, URL, encoding, Blob/File, AbortController, EventSource — plus
platform extensions (env namespaces, waitUntil, RPC dispatch, subscription WS framing,
capability enforcement). All JS polyfills are replaced with native Rust v8_class wrappers.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| V8 Isolate / Event Loop (compio) | 🟢 | internal | `crates/runtime/src/core/runtime.rs` | `docs/architecture/runtime.md` | `crates/runtime/tests/call_fetch_handler.rs` | Slow handler blocks thread; deploys drop in-flight; single Rc<RefCell> SharedState. |
| WinterCG fetch handler dispatch | 🟢 | internal | `crates/runtime/src/core/runtime.rs` | `docs/architecture/runtime.md` | `crates/runtime/tests/call_fetch_handler.rs` | 3 entry points: fetch, rpc fast-path, fetchFast. |
| globalThis.fetch (outbound) | 🟢 | `globalThis.fetch` | `crates/runtime/src/web/fetch/mod.rs` | — | `crates/runtime/tests/fetch_native.rs` | Streaming request body deferred; 10 MB max body; query/mutation cannot fetch. |
| Request / Response classes | 🟢 | `globalThis.Request` / `Response` | `crates/runtime/src/web/fetch/request.rs`, `response.rs` | — | `crates/runtime/tests/fetch_request.rs` | Body streams pumped by Rust forwarder; WPT-tested. |
| Headers class | 🟢 | `globalThis.Headers` | `crates/runtime/src/web/headers.rs` | — | `crates/runtime/tests/headers.rs` | WPT suite in wpt_headers.rs. |
| WHATWG Streams | 🟢 | `globalThis.ReadableStream` / `WritableStream` / `TransformStream` | `crates/runtime/src/web/streams/` | — | `crates/runtime/tests/streams_native.rs` | BYOB, byte streams, tee, pipe, async iter; cap 65,536/isolate. |
| CompressionStream / DecompressionStream | 🟢 | `globalThis.CompressionStream` / `DecompressionStream` | `crates/runtime/src/web/streams/compression.rs` | — | `crates/runtime/tests/compression_streams.rs` | gzip/deflate/deflate-raw/brotli. |
| WebCrypto (SubtleCrypto) | 🟢 | `globalThis.crypto.subtle.*` | `crates/runtime/src/web/crypto/` | — | `crates/runtime/tests/crypto_native.rs` | AES/RSA/ECDSA/ECDH/Ed25519/X25519/HMAC/HKDF/PBKDF2/SHA/JWK. |
| crypto.getRandomValues / randomUUID | 🟢 | `globalThis.crypto.getRandomValues()` / `randomUUID()` | `crates/runtime/src/web/crypto/crypto_class.rs` | — | `crates/runtime/tests/crypto.rs` | Part of the Crypto global. |
| WebSocket client (outbound) | 🟢 | `globalThis.WebSocket` | `crates/runtime/src/web/websocket/mod.rs` | `docs/reference/websocket-design.md` | `crates/runtime/tests/websocket_e2e.rs` | SSRF checks; no permessage-deflate. |
| WebSocketPair (server upgrade) | 🟢 | `globalThis.WebSocketPair` | `crates/runtime/src/web/websocket/pair.rs` | `docs/reference/websocket-design.md` | `crates/runtime/tests/websocket_e2e.rs` | `new Response(null, { status: 101, webSocket })`. |
| WS Subscription transport | 🟢 | internal (bootstrap); via @zeroship/rpc subscriptions | `crates/runtime/src/core/init.rs` | `docs/reference/websocket-design.md` | `crates/runtime/tests/websocket_e2e.rs` | User-space JS; hello/data/end/error/ping/pong. |
| URL / URLSearchParams | 🟢 | `globalThis.URL` / `URLSearchParams` | `crates/runtime/src/web/url/` | — | `crates/runtime/tests/url_native.rs` | ada-url backed; live-sync params. |
| TextEncoder / TextDecoder | 🟢 | `globalThis.TextEncoder` / `TextDecoder` | `crates/runtime/src/web/encoding/mod.rs` | — | `crates/runtime/tests/wpt_text_encoding.rs` | All WHATWG encodings (encoding_rs). |
| TextEncoderStream / TextDecoderStream | 🟢 | `globalThis.TextEncoderStream` / `TextDecoderStream` | `crates/runtime/src/web/encoding/streams.rs` | — | `crates/runtime/tests/text_encoding_streams.rs` | Loaded after native streams. |
| AbortController / AbortSignal | 🟢 | `globalThis.AbortController` / `AbortSignal` | `crates/runtime/src/web/dom/abort_controller.rs`, `abort_signal.rs` | — | `crates/runtime/tests/abort.rs` | RPC eviction fan-out in rpc/abort.rs. |
| EventTarget / Event / CustomEvent | 🟢 | `globalThis.EventTarget` / `Event` / `CustomEvent` | `crates/runtime/src/web/dom/event_target.rs`, `event.rs`, `custom_event.rs` | — | `crates/runtime/tests/event_target.rs` | MessageEvent/CloseEvent also native. |
| DOMException | 🟢 | `globalThis.DOMException` | `crates/runtime/src/web/dom/exception.rs` | — | `crates/runtime/tests/dom_exception.rs` | Replaces former JS polyfill. |
| FormData | 🟢 | `globalThis.FormData` | `crates/runtime/src/web/dom/form_data.rs` | — | `crates/runtime/tests/form_data.rs` | Live iterators; Blob→File at append. |
| Blob / File | 🟢 | `globalThis.Blob` / `File` | `crates/runtime/src/web/blob/` | — | `crates/runtime/tests/blob_native.rs` | Blob.stream() → ReadableStream. |
| structuredClone | 🟢 | `globalThis.structuredClone(value)` | `crates/runtime/src/web/structured_clone.rs` | — | — | V8 ValueSerializer; transfer deferred. |
| atob / btoa | 🟢 | `globalThis.atob()` / `btoa()` | `crates/runtime/src/web/base64.rs` | — | `crates/runtime/tests/base64.rs` | WHATWG §8.6. |
| EventSource (SSE client) | 🟢 | `globalThis.EventSource` | `crates/runtime/src/web/eventsource.rs` | — | `crates/runtime/tests/eventsource.rs` | Delegates IO to globalThis.fetch. |
| Timers (setTimeout/Interval/clear*) | 🟢 | `globalThis.setTimeout` / `setInterval` / `clear*` | `crates/runtime/src/core/init.rs` | — | `crates/runtime/tests/web_apis.rs` | Per-isolate admission control; setImmediate is a JS shim. |
| queueMicrotask | 🟢 | `globalThis.queueMicrotask(fn)` | `crates/runtime/src/core/init.rs` | — | — | Via Promise.resolve().then(fn). |
| performance.now() | 🟢 | `globalThis.performance.now()` | `crates/runtime/src/core/init.rs` | — | — | Per-isolate epoch (no cross-isolate timing). |
| console.* | 🟢 | `globalThis.console.*` | `crates/runtime/src/core/init.rs` | — | `crates/runtime/tests/console.rs` | Per-request capture; 4096 B/line, 1000 lines. |
| Intl / ICU | 🟢 | `globalThis.Intl.*` | `crates/runtime/src/core/init.rs` | — | — | ICU data loaded at init_v8(). |
| node:async_hooks (AsyncLocalStorage) | 🟢 | `import { AsyncLocalStorage } from 'node:async_hooks'` | `crates/runtime/src/node/async_hooks/als.rs` | `docs/reference/node-compat.md` | `crates/runtime/tests/async_local_storage.rs` | V8 ContinuationPreservedEmbedderData. |
| node:crypto | 🟢 | `import { createHash, ... } from 'node:crypto'` | `crates/runtime/src/node/crypto/` | `docs/reference/node-compat.md` | `crates/runtime/tests/crypto_node.rs` | getCiphers/getCurves stubs; async KDFs deferred. |
| node:buffer (Buffer global) | 🟢 | `import { Buffer } from 'node:buffer'` / global | `crates/runtime/src/node/buffer/mod.rs` | `docs/reference/node-compat.md` | `crates/runtime/tests/node_buffer.rs` | transcode / MAX_STRING_LENGTH deferred. |
| node:zlib | 🟢 | `import { gzipSync, ... } from 'node:zlib'` | `crates/runtime/src/node/zlib/mod.rs` | `docs/reference/node-compat.md` | `crates/runtime/tests/codec.rs` | Stream constructors are throwing stubs. |
| node:os | 🟢 | `import { platform, ... } from 'node:os'` | `crates/runtime/src/node/os/mod.rs` | `docs/reference/node-compat.md` | — | networkInterfaces/get/setPriority are stubs. |
| node:path | 🟢 | `import { join, ... } from 'node:path'` | `crates/runtime/src/node/path/mod.rs` | `docs/reference/node-compat.md` | — | Backed by static path.js. |
| node:util | 🟡 | `import { format, promisify, ... } from 'node:util'` | `crates/runtime/src/node/util/mod.rs` | `docs/reference/node-compat.md` | — | inspect quirks, styleText, parseArgs short flags deferred. |
| Dynamic import (await import()) | 🟢 | `globalThis` (import() expr) | `crates/runtime/src/core/dynamic_import.rs` | — | `crates/runtime/tests/dynamic_import.rs` | Bundle is the closed world; no fetch-on-demand. |
| CPU limit enforcement | 🟢 | internal (RuntimeBuilder::cpu_limit) | `crates/runtime/src/core/cpu_timer.rs` | `docs/reference/runtime-limits.md` | — | Linux only; pump CPU budget also enforced. |
| Wall timeout enforcement | 🟢 | internal (RuntimeBuilder::wall_timeout) | `crates/runtime/src/core/serve.rs` | `docs/reference/runtime-limits.md` | — | Default unset. |
| V8 Heap limit | 🟢 | internal (RuntimeBuilder::heap_limit_mb) | `crates/runtime/src/core/runtime.rs` | `docs/reference/runtime-limits.md` | `crates/runtime/tests/heap_limits.rs` | Default 128 MB; terminates after 5 hits. |
| Idle GC | 🟢 | internal (RuntimeBuilder::idle_gc_after_ms) | `crates/runtime/src/core/runtime.rs` | `docs/reference/runtime-limits.md` | `crates/runtime/tests/idle_gc.rs` | Default 30s; 0 disables. |
| SSRF protection (fetch + WS) | 🟢 | internal | `crates/runtime/src/transport/ssrf.rs` | — | — | String + DNS-level blocklist. |
| RPC capability enforcement | 🟢 | internal (__zsEnterKind/__zsExitKind) | `crates/runtime/src/rpc/capability.rs` | — | `crates/runtime/tests/capability.rs` | query can't write, mutation can't fetch. |
| waitUntil / getRequest / getRequestContext | 🟢 | `import { waitUntil, getRequest, ... } from 'zeroship'` | `crates/runtime/src/core/init.rs` | `docs/reference/zeroship-standard.md` | — | getRequest() null on RPC fast-path. |
| env / runQuery / currentUser etc. | 🟢 | `import { env, runQuery, currentUser, ... } from 'zeroship'` | `crates/runtime/src/core/init.rs` | `docs/reference/zeroship-standard.md` | — | runQuery/runMutation push capability frame. |
| Native plugin system (env.* namespaces) | 🟢 | `env.db.*` / `env.kv.*` / `env.storage.*` / `env.auth.*` | `crates/runtime/src/core/plugin.rs` | `docs/reference/plugin-system.md` | — | env.meter/assets documented as planned, not registered. |
| AI SDK Data Stream Protocol (SSE) | 🟢 | internal (sseFromAsyncGen) | `crates/runtime/src/core/init.rs` | — | `crates/runtime/tests/ai_sdk_stream.rs` | 0:text 2:object e:error d:done. |
| WebIDL conversion layer | 🟢 | internal | `crates/runtime/src/webidl/` | — | — | USVString/ByteString/Clamp/EnforceRange/WebIdlDict. |
| v8_class proc macro | 🟢 | internal (crates/runtime-macros) | `crates/runtime-macros/` | `docs/reference/plugin-system.md` | — | Backs every native class. |
| structuredClone transfer | 🔵 | `globalThis.structuredClone(value, { transfer })` | `crates/runtime/src/web/structured_clone.rs` | — | — | Deferred; always deep-clones. |
| Streaming fetch request body | 🔵 | `globalThis.fetch(url, { body: stream })` | `crates/runtime/src/web/fetch/mod.rs` | — | — | snapshot_request returns error. |
| node:zlib stream constructors | 🟠 | `import { createGzip } from 'node:zlib'` | `crates/runtime/src/node/zlib/mod.rs` | — | — | Throwing stubs → gzipSync/CompressionStream. |
| node:os networkInterfaces / get/setPriority | 🟠 | `import { networkInterfaces } from 'node:os'` | `crates/runtime/src/node/os/mod.rs` | — | — | Throwing stubs; no sandbox OS introspection. |
| env.meter.* (billing metering primitive) | 🔵 | `env.meter.*` (not registered) | — | `docs/reference/billing-metering.md` | — | Planned/platform-internal per AGENTS.md. |
| env.assets.* (runtime-emitted assets) | 🔵 | `env.assets.*` (not registered) | — | — | — | Planned/platform-internal only. |

---

## 2. Node.js compatibility

A narrow Node.js compat layer in two tiers: (1) seven runtime-native synthetic ESM modules
registered directly in V8 (`crates/runtime/src/core/native_modules.rs`), and (2) a
Vite-plugin build-time layer (`sdks/vite-plugin/src/node-compat.ts`) combining custom
polyfills with unenv@2 aliases. A partial mismatch: `node:zlib`/`node:os` are runtime-native
but absent from the plugin's `RUNTIME_NATIVE_MODULES`, so in dev they fall through to unenv.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| node:async_hooks — AsyncLocalStorage | 🟢 | `import { AsyncLocalStorage } from "node:async_hooks"` | `crates/runtime/src/node/async_hooks/als.rs` | `docs/reference/node-compat.md` | `crates/runtime/tests/async_local_storage.rs` | run/getStore/enterWith/disable; rest are throw-stubs. |
| node:async_hooks — stub exports | 🟠 | `import { AsyncResource, createHook, ... }` | `crates/runtime/src/node/async_hooks/mod.rs` | `docs/reference/node-compat.md` | — | Throw ERR_METHOD_NOT_IMPLEMENTED. |
| node:buffer — Buffer class | 🟢 | `import { Buffer } from "node:buffer"` / global | `crates/runtime/src/node/buffer/mod.rs` | `docs/reference/node-compat.md` | `crates/runtime/tests/node_buffer.rs` | transcode/MAX_STRING_LENGTH deferred; same identity as global. |
| node:buffer — module helpers | 🟢 | `import { kMaxLength, isUtf8, isAscii }` | `crates/runtime/src/node/buffer/mod.rs` | — | `crates/runtime/tests/node_buffer.rs` | MAX_STRING_LENGTH absent. |
| node:crypto — Hash / createHash | 🟢 | `import { createHash } from "node:crypto"` | `crates/runtime/src/node/crypto/hash.rs` | — | `crates/runtime/tests/crypto_node.rs` | aws-lc-rs; throws after digest. |
| node:crypto — Hmac / createHmac | 🟢 | `import { createHmac } from "node:crypto"` | `crates/runtime/src/node/crypto/hmac.rs` | — | `crates/runtime/tests/crypto_node.rs` | Hmac.copy not implemented. |
| node:crypto — random functions | 🟢 | `import { randomBytes, randomUUID }` | `crates/runtime/src/node/crypto/random.rs` | — | `crates/runtime/tests/crypto_node.rs` | aws-lc-rs SystemRandom. |
| node:crypto — KDFs (pbkdf2/hkdf/scrypt) | 🟢 | `import { pbkdf2Sync, hkdfSync, scryptSync }` | `crates/runtime/src/node/crypto/kdf.rs` | — | `crates/runtime/tests/crypto_node.rs` | Async variants run sync on V8 thread. |
| node:crypto — KeyObject + factories | 🟢 | `import { createSecretKey, KeyObject }` | `crates/runtime/src/node/crypto/key_object.rs` | — | `crates/runtime/tests/crypto_node.rs` | Interops with WebCrypto CryptoKey. |
| node:crypto — Sign/Verify + one-shot | 🟢 | `import { createSign, sign, publicEncrypt }` | `crates/runtime/src/node/crypto/sign_verify.rs` | — | `crates/runtime/tests/crypto_node.rs` | ECDSA DER output; RSA-OAEP. |
| node:crypto — Cipher / Decipher | 🟢 | `import { createCipheriv, createDecipheriv }` | `crates/runtime/src/node/crypto/cipher.rs` | — | `crates/runtime/tests/crypto_node.rs` | CCM deferred; getCiphers() stale-empty. |
| node:crypto — key generation | 🟢 | `import { generateKeyPairSync, generateKeySync }` | `crates/runtime/src/node/crypto/keygen.rs` | — | `crates/runtime/tests/crypto_node.rs` | Async counterparts may lack callback wiring. |
| node:crypto — misc | 🟡 | `import { timingSafeEqual, getHashes, getCurves }` | `crates/runtime/src/node/crypto/misc.rs` | — | `crates/runtime/tests/crypto_node.rs` | getCiphers empty; setFips throws; secureHeapUsed stub. |
| node:crypto — WebCrypto bridge | 🟢 | `import { webcrypto, subtle }` | `crates/runtime/src/node/crypto/module.rs` | — | — | crypto.webcrypto === globalThis.crypto. |
| node:zlib — sync one-shot codecs | 🟢 | `import { gzipSync, brotliCompressSync }` | `crates/runtime/src/node/zlib/mod.rs` | `docs/reference/node-compat.md` | `crates/runtime/tests/node_zlib.rs` | flate2 + brotli; level option. |
| node:zlib — async callback codecs | 🟡 | `import { gzip, brotliCompress }` | `crates/runtime/src/node/zlib/mod.rs` | — | `crates/runtime/tests/node_zlib.rs` | Runs sync on V8 thread; large input blocks loop. |
| node:zlib — stream constructors | 🟠 | `import { createGzip } from "node:zlib"` | `crates/runtime/src/node/zlib/mod.rs` | — | — | Throw ERR_METHOD_NOT_IMPLEMENTED. |
| node:zlib — constants | 🟡 | `import { constants } from "node:zlib"` | `crates/runtime/src/node/zlib/mod.rs` | — | — | ~30 of ~80 constants. |
| node:zlib — build↔runtime mismatch | 🟡 | internal | `sdks/vite-plugin/src/node-compat.ts` | — | — | Absent from RUNTIME_NATIVE_MODULES → unenv in dev. |
| node:os | 🟡 | `import { platform, arch, cpus } from "node:os"` | `crates/runtime/src/node/os/mod.rs` | `docs/reference/node-compat.md` | `crates/runtime/tests/node_os.rs` | Sandbox constants; networkInterfaces empty; build mismatch. |
| node:path (POSIX) | 🟢 | `import { join, resolve, dirname }` | `crates/runtime/src/node/path/mod.rs` | `docs/reference/node-compat.md` | `crates/runtime/tests/node_path.rs` | win32 is throwing Proxy; posix aliases module. |
| node:util — format/inspect | 🟡 | `import { format, inspect } from "node:util"` | `crates/runtime/src/node/util/mod.rs` | `docs/reference/node-compat.md` | `crates/runtime/tests/node_util.rs` | inspect.custom, styleText, MIMEType deferred. |
| node:util — promisify/callbackify/deprecate | 🟢 | `import { promisify, callbackify }` | `crates/runtime/src/node/util/mod.rs` | — | `crates/runtime/tests/node_util.rs` | promisify.custom supported. |
| node:util — types predicates | 🟢 | `import { types } from "node:util"` | `crates/runtime/src/node/util/mod.rs` | — | `crates/runtime/tests/node_util.rs` | isProxy always false. |
| node:util — isDeepStrictEqual | 🟢 | `import { isDeepStrictEqual }` | `crates/runtime/src/node/util/mod.rs` | — | `crates/runtime/tests/node_util.rs` | Map order not enforced. |
| node:util — parseArgs | 🟡 | `import { parseArgs }` | `crates/runtime/src/node/util/mod.rs` | — | — | Long-form only; short flags/strict deferred. |
| node:util — TextEncoder/Decoder re-export | 🟢 | `import { TextEncoder, TextDecoder }` | `crates/runtime/src/node/util/mod.rs` | — | — | Identity with globals. |
| globalThis.process polyfill | 🟡 | bare `process` / `import process from "node:process"` | `crates/runtime/src/core/init.rs` | — | — | Runtime sets global; Vite wraps it; not a real EventEmitter. |
| node:timers/promises — Vite polyfill | 🟡 | `import { setTimeout, setInterval } from "node:timers/promises"` | `sdks/vite-plugin/src/node-compat.ts` | — | — | Custom (unenv setInterval isn't async gen); build-only. |
| node:module — Vite polyfill | 🟠 | `import { createRequire } from "node:module"` | `sdks/vite-plugin/src/node-compat.ts` | — | — | noop-Proxy; documented as "not a feature". |
| unenv@2 fallback for other node: modules | 🟡 | any unhandled `node:*` (build only) | `sdks/vite-plugin/src/node-compat.ts` | `docs/reference/node-compat.md` | — | events/stream/http/fs/etc.; quality varies. |
| Bare global injection (Buffer/process/...) | 🟢 | internal (build transform) | `sdks/vite-plugin/src/node-compat.ts` | — | — | @rollup/plugin-inject; SSR env only. |
| __zeroshipNodeBuiltin dev bridge | 🟢 | internal | `crates/runtime/src/core/native_modules.rs` | — | — | Lets Vite ModuleRunner source native node: exports. |
| Bare-without-prefix specifiers ('crypto') | 🟡 | `import { createHash } from 'crypto'` | `sdks/vite-plugin/src/node-compat.ts` | — | — | Works in Vite; bare 'crypto' fails at runtime (prod). |

---

## 3. env.db / @zeroship/db

The platform's structured database layer. Creators declare schema through committed
op.* migrations; the generated runtime descriptor installs collection wrappers on
`env.db` at boot, and all CRUD/search/transaction/migration/reactive operations go
through that typed surface — no raw SQL. The Rust plugin (`crates/plugin-db`)
provides the native V8 surface; the TS SDK (`@zeroship/db`) wraps it. Both Postgres
(prod) and SQLite (dev/test) are supported.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| Schema DSL — t.* type builders | 🟢 | `import { t } from '@zeroship/db'` | `sdks/db/src/types.ts` | `docs/reference/db.md` | `sdks/db/tests/types.test.ts` | t.encrypted/vector/geoPoint/id; no t.date(). |
| Schema refinements (.required/.unique/.index/...) | 🟢 | chained on t.*() | `sdks/db/src/types.ts` | `docs/reference/db.md` | `sdks/db/tests/types.test.ts` | .fts(lang) ignored at query time on SQLite. |
| Per-collection options (schema() builder) | 🟢 | `import { schema } from '@zeroship/db'` | `sdks/db/src/types.ts` | `docs/reference/db.md` | `sdks/db/tests/named-indexes.test.ts` | softDelete/withVersioning are hints; cols always created. |
| registerModel DDL pipeline | 🟢 | internal (via bootstrap) | `crates/plugin-db/src/register_model/` | `docs/reference/db.md` | `crates/plugin-db/tests/integration.rs` | PG advisory-lock + CONCURRENTLY; SQLite skips mask ops. |
| Per-app Postgres schema isolation | 🟢 | internal | `crates/plugin-db/src/register_model/bootstrap.rs` | `docs/reference/db.md` | — | app_id runtime-injected; SQLite is one file/app. |
| System fields (id, created_at, ..., deleted_at) | 🟢 | internal (on every Row<S>) | `crates/plugin-db/src/query.rs`, `crud/system_fields_pass.rs` | `docs/reference/db.md` | `sdks/db/tests/p7-pr1-system-field-builders.test.ts` | 7 columns; names reserved at deploy. |
| Typed-id prefix system | 🟢 | `t.id('prefix')` / Id<S> | `crates/plugin-db/src/crud/system_fields_pass.rs`, `sdks/db/src/types.ts` | `docs/reference/db.md` | `sdks/db/tests/p7-id-prefix.test.ts` | UUIDv7 base62, sortable. |
| Collection.insert / insertMany | 🟢 | `Collection.insert(doc)` / `insertMany(docs)` | `crates/plugin-db/src/crud/mod.rs` | `docs/reference/db.md` | `sdks/db/tests/query.test.ts` | Result outside tx; bare Row inside tx. |
| Collection.find / get | 🟢 | `Collection.find(filter, opts?)` / `get(...)` | `crates/plugin-db/src/crud/mod.rs` | `docs/reference/db.md` | `sdks/db/tests/query.test.ts` | Auto-filters deleted_at; masked → MaskedValue. |
| Query builder (lazy thenable) | 🟢 | `@zeroship/db` Query class | `sdks/db/src/query.ts` | `docs/reference/db.md` | `sdks/db/tests/query.test.ts` | sort/limit/skip/select/after/paginate/with/first/unique. |
| Collection.exists | 🟢 | `Collection.exists(filter?)` | `sdks/db/src/collection/crud.ts` | `docs/reference/db.md` | — | find(...).limit(1) SDK-side. |
| Collection.count | 🟢 | `Collection.count(filter, opts?)` | `crates/plugin-db/src/crud/mod.rs`, `query.rs` | `docs/reference/db.md` | — | Auto-filters soft-deleted. |
| Collection.distinct | 🟢 | `Collection.distinct(field, filter?, opts?)` | `crates/plugin-db/src/crud/mod.rs`, `query.rs` | `docs/reference/db.md` | — | opts.field required. |
| Collection.update / updateMany | 🟢 | `Collection.update(filter, patch)` / `updateMany(...)` | `crates/plugin-db/src/crud/mod.rs` | `docs/reference/db.md` | `sdks/db/tests/p7-pr4-update-version.test.ts` | CAS version; $inc/$push/$set operators. |
| Collection.delete / deleteMany (soft) | 🟢 | `Collection.delete(idOrFilter)` / `deleteMany(...)` | `crates/plugin-db/src/crud/mod.rs` | `docs/reference/db.md` | `sdks/db/tests/p7-pr5-soft-delete.test.ts` | Sets deleted_at; emits Update CDC. |
| Collection.purge / purgeMany (hard) | 🟢 | `Collection.purge(idOrFilter)` / `purgeMany(...)` | `crates/plugin-db/src/crud/mod.rs` | `docs/reference/db.md` | `sdks/db/tests/p7-pr5-soft-delete.test.ts` | GDPR erase; bypasses soft-delete filter. |
| Collection.restore / restoreMany | 🟢 | `Collection.restore(idOrFilter)` / `restoreMany(...)` | `crates/plugin-db/src/crud/mod.rs` | `docs/reference/db.md` | `sdks/db/tests/p7-pr5-soft-delete.test.ts` | Clears deleted_at, bumps version. |
| Collection.upsert | 🟢 | `Collection.upsert(doc, { conflictFields })` | `crates/plugin-db/src/crud/mod.rs` | `docs/reference/db.md` | — | conflictFields required. |
| Collection.aggregate | 🟢 | `Collection.aggregate(pipeline, opts?)` | `crates/plugin-db/src/crud/mod.rs`, `query.rs` | `docs/reference/db.md` | — | $match/$group/$having/$sort/$limit; $first sort-order future. |
| Filter operators | 🟢 | Filter<S> on read/write | `crates/plugin-db/src/query.rs` | `docs/reference/db.md` | `sdks/db/tests/query-and-or-semantics.test.ts` | All values parameterized; $search → FTS. |
| Optimistic concurrency (version + withRetry) | 🟢 | `Collection.update({id,version},...)` / `withRetry` | `crates/plugin-db/src/crud/system_fields_pass.rs`, `sdks/db/src/with-retry.ts` | `docs/reference/db.md` | `sdks/db/tests/optimistic-lock-in-tx.test.ts` | withRetry max:3, no backoff default. |
| Relations — with: { fk: true } | 🟢 | `find(filter, { with })` / `Query.with(spec)` | `sdks/db/src/collection/relations.ts` | `docs/reference/db.md` | `sdks/db/tests/relations.test.ts` | v1 single-level; no nested with. |
| Foreign keys (t.ref) + cross-app rejection | 🟢 | `t.ref('collection', opts?)` | `sdks/db/src/types.ts`, `crates/plugin-db/src/cross_app_fk.rs` | `docs/reference/db.md` | `sdks/db/tests/b2-ref-validation.test.ts` | Default restrict/deferrable; cross-app blocked. |
| Named multi-column indexes | 🟢 | `schema({...}).index('name', [...])` | `sdks/db/src/types.ts`, `crates/plugin-db/src/register_model/apply.rs` | `docs/reference/db.md` | `sdks/db/tests/named-indexes.test.ts` | Field-list change is currently a no-op. |
| Native transactions + nested savepoints | 🟢 | `env.db.transaction(async tx => {...})` | `crates/plugin-db/src/transaction/mod.rs`, `v8_classes/db.rs` | `docs/reference/db.md` | `sdks/db/tests/p9-pr3-native-transaction.test.ts` | PG isolation; SQLite ignores level. |
| Vector search (t.vector + search({vector})) | 🟢 | `Collection.search({ vector, k, ... })` | `crates/plugin-db/src/crud/mod.rs`, `backend/sqlite/vector.rs`, `backend/postgres.rs` | `docs/reference/db.md` | — | pgvector / sqlite-vec; innerProduct PG-only. |
| Full-text search (.fts() + search({text})) | 🟢 | `Collection.search({ text, ... })` | `crates/plugin-db/src/crud/mod.rs`, `backend/sqlite/fts.rs`, `backend/postgres.rs` | `docs/reference/db.md` | — | _rank not comparable across backends. |
| Geo / spatial search (t.geoPoint + near()) | 🟢 | `Collection.near({ field, point, radius, ... })` | `crates/plugin-db/src/crud/mod.rs`, `backend/sqlite/spatial.rs`, `backend/postgres.rs` | `docs/reference/db.md` | — | PostGIS; SQLite haversine flat scan; polygon PG-only. |
| Column-level encryption (t.encrypted) | 🟢 | `t.encrypted({wraps})` | `crates/plugin-db/src/encryption/`, `crud/encryption_pass.rs` | `docs/reference/db.md` | `sdks/db/tests/p5-encrypted-builder-and-filter-fence.test.ts` | AES-256-GCM; fenced from filters. |
| Field masking (.mask() + MaskedValue) | 🟢 | `.mask({ kind, classification })` | `crates/plugin-db/src/crud/mask_pass.rs`, `v8_classes/masked_value.rs` | `docs/reference/db.md` | `sdks/db/tests/p55-pr1-mask-builder-and-masked-value.test.ts` | 8 kinds × 6 classifications; __zsmask__ sentinel. |
| MaskedValue.unmask() / bulkUnmask() | 🟢 | `MaskedValue.unmask(opts)` / `Collection.bulkUnmask(...)` | `crates/plugin-db/src/crud/unmask.rs`, `sdks/db/src/collection/masking.ts` | `docs/reference/db.md` | `sdks/db/tests/p55-pr7-per-query-unmask.test.ts` | Atomic; every call audited. |
| defineMaskPolicy() | 🟢 | `import { defineMaskPolicy } from '@zeroship/db'` | `sdks/db/src/policy.ts`, `crates/plugin-db/src/crud/mask_policy.rs` | `docs/reference/db.md` | `sdks/db/tests/p55-pr5-define-mask-policy.test.ts` | Keyed by app_id; replace not merge. |
| Mask drift detection cron | 🟡 | internal (no JS surface) | `crates/plugin-db/src/crud/mask_drift.rs` | `docs/reference/db.md` | — | Test-only cfg; cron scheduling not wired. |
| Mask/encryption backfill pipeline | 🟡 | internal (DDL apply) | `crates/plugin-db/src/crud/mask_backfill.rs` | — | — | PG only; SQLite returns backend_unsupported. |
| Data backfill migrations (@zeroship/migrations) | 🟢 | `import { defineMigration, migrations }` | `sdks/migrations/src/`, `crates/plugin-db/src/migrations.rs` | `docs/reference/db.md` | `sdks/migrations/tests/run.test.ts` | PG-only (advisory lock); dead-letter queue. |
| Migration sweeper (orphan reaper) | 🟡 | internal (no JS surface) | `crates/plugin-db/src/migration_sweeper.rs` | — | — | No platform cron hook yet. |
| In-process CDC broker (openSubscription) | 🟢 | `collection.openSubscription()` / `subscribe(name)` | `crates/plugin-db/src/broker.rs`, `v8_classes/subscription.rs`, `sdks/db/src/subscribe.ts` | — | `sdks/db/tests/subscribe-close.test.ts` | In-process only; coarse-grained; 1024-event queue. |
| Live queries — db.live(queryFn) | 🟢 | `db.live(queryFn, opts?)` | `sdks/db/src/live.ts` | `docs/reference/db.md` | `sdks/db/tests/live.test.ts` | v1 coarse-grained; LIVE_IN_TRANSACTION error. |
| WAL replication consumer | 🟡 | `__platform.startReplicationConsumer()` | `crates/plugin-db/src/wal_consumer.rs`, `replication.rs` | — | — | pgoutput decoder ships; no reconnection/auto-spawn. |
| Replication slot/publication lifecycle | 🟡 | internal (__platform.replication.*) | `crates/plugin-db/src/replication.rs`, `replication_ops.rs` | — | — | PG-only; no leader election. |
| DDL audit log (__zeroship_migrations) | 🟢 | internal (SQL-readable) | `crates/plugin-db/src/audit.rs`, `register_model/apply.rs` | — | — | Per-app schema. |
| Unmask audit log (__zeroship_audit_unmask) | 🟢 | internal (SQL-readable) | `crates/plugin-db/src/crud/unmask.rs` | `docs/reference/db.md` | — | Granted + denied audited. |
| Mask drift audit log (__zeroship_audit_mask_drift) | 🟡 | internal (SQL-readable) | `crates/plugin-db/src/crud/mask_drift.rs` | `docs/reference/db.md` | — | Drift cron not scheduled. |
| App namespace drop (drop_namespace) | 🟢 | internal (control-plane) | `crates/plugin-db/src/drop_namespace.rs` | — | — | DROP SCHEMA CASCADE; PG-only. |
| Dual-backend support (PG + SQLite) | 🟢 | internal (DbPlugin::new(url)) | `crates/plugin-db/src/lib.rs` | `docs/reference/sqlite-divergences.md` | `crates/plugin-db/tests/sqlite_integration.rs` | URL-driven; SQLite dev/test only. |
| Per-app auth schema (PG roles, sessions) | 🟢 | internal (bootstrap) | `crates/plugin-db/src/auth/` | — | — | PG-only; SQLite has shim. |
| DataLoader (batched get by id) | 🟢 | internal (Collection.get) | `sdks/db/src/loader.ts` | — | `sdks/db/tests/loader.test.ts` | Per-collection, per-tx-depth. |
| Input validation | 🟢 | automatic on insert/update | `sdks/db/src/validate.ts` | `docs/reference/db.md` | `sdks/db/tests/validate.test.ts` | Runs in JS before native call. |
| env.db generated type augmentation | 🟢 | `generated/zeroship/env.db.ts` in tsconfig include | `crates/zeroship-migrate-js/src/gen_types.rs` | `docs/reference/db.md` | `crates/zeroship-migrate-js/tests/gen_types_dts_golden.rs` | Folded migration set is canonical; `@zeroship/db/env` is retired. |
| Schema strictness (strict/lenient/off) | 🟢 | `schema({...}).strictness(...)` | `crates/plugin-db/src/register_model/validate.rs` | `docs/reference/db.md` | — | Default strict; refuses destructive. |
| Per-query unmask hint (find opts.unmask) | 🟢 | `Collection.find(filter, { unmask, actor, ... })` | `crates/plugin-db/src/crud/mod.rs` | `docs/reference/db.md` | `sdks/db/tests/p55-pr7-per-query-unmask.test.ts` | id must be in select if projecting. |
| Collection.unmaskField / bulkUnmask | 🟢 | `collection.unmaskField(rowPk, column, opts?)` | `crates/plugin-db/src/v8_classes/collection.rs`, `crud/unmask.rs` | `docs/reference/db.md` | — | Collection name un-spoofable; audited. |
| Unindexed query runtime warnings | 🟢 | automatic (dev) | `sdks/db/src/collection/index-warnings.ts` | `docs/reference/db.md` | `sdks/db/tests/named-indexes.test.ts` | Suppressed in production. |
| Encrypted field filter fence | 🟢 | automatic when schema has t.encrypted | `sdks/db/src/collection/encryption-fence.ts` | — | `sdks/db/tests/filter-encryption-types.test.ts` | Deterministic mode allows equality. |

---

## 4. env.kv / @zeroship/kv

A strongly-consistent, atomic key-value store for ephemeral hot-path state (rate-limit
counters, leases, cache-aside, session scratch). A pluggable Rust backend (redb for
single-process/dev, Redis/Dragonfly for fleets) exposed as a v8_class on `env.kv`, plus a
typed TS SDK (`@zeroship/kv`) adding JSON serialization, Result wrapping, and conveniences.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| env.kv.get | 🟢 | `env.kv.get(key)` | `crates/plugin-kv/src/v8_class.rs`, `dispatch.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/e2e_runtime.rs` | Raw string at native layer; SDK JSON.parses. |
| env.kv.set | 🟢 | `env.kv.set(key, value, {ttlMs?})` | `crates/plugin-kv/src/v8_class.rs`, `dispatch.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/e2e_runtime.rs` | Value must be string; TTL≤100yr. |
| env.kv.delete | 🟢 | `env.kv.delete(key)` | `crates/plugin-kv/src/v8_class.rs`, `dispatch.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/e2e_runtime.rs` | {deleted:bool}; no error on miss. |
| env.kv.incr | 🟢 | `env.kv.incr(key, {by?, ttlMs?})` | `crates/plugin-kv/src/v8_class.rs`, `backend/redis.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/e2e_runtime.rs` | Fixed-window TTL; Lua EVAL on Redis. |
| env.kv.setIfAbsent | 🟢 | `env.kv.setIfAbsent(key, value, {ttlMs?})` | `crates/plugin-kv/src/v8_class.rs`, `dispatch.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/e2e_runtime.rs` | Redis SET NX; redb MVCC. |
| env.kv.expire | 🟢 | `env.kv.expire(key, ttlMs)` | `crates/plugin-kv/src/v8_class.rs`, `dispatch.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/e2e_runtime.rs` | PEXPIRE; updated:false on miss. |
| env.kv.ttl | 🟢 | `env.kv.ttl(key)` | `crates/plugin-kv/src/v8_class.rs`, `backend/mod.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/e2e_runtime.rs` | 3-state TtlState. |
| env.kv.persist | 🟢 | `env.kv.persist(key)` | `crates/plugin-kv/src/v8_class.rs`, `dispatch.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/e2e_runtime.rs` | Redis PERSIST. |
| env.kv.list | 🟢 | `env.kv.list(prefix?, {cursor?, limit?})` | `crates/plugin-kv/src/v8_class.rs`, `backend/redb.rs`, `backend/redis.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/e2e_runtime.rs` | Literal prefix; opaque cursor; max 10000. |
| redb backend | 🟢 | internal (ZEROSHIP_KV_URL unset) | `crates/plugin-kv/src/backend/redb.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/e2e_runtime.rs` | Single-writer MVCC; Durability::Immediate. |
| Redis/Dragonfly backend (single-node) | 🟢 | internal (redis:// URL) | `crates/plugin-kv/src/backend/redis.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/redis_backend.rs` | Per-thread pool; Lua incr. |
| Redis/Dragonfly backend (cluster) | 🟢 | internal (?cluster=true) | `crates/plugin-kv/src/backend/redis.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/redis_backend.rs` | Hash-tag slot; single-shard ceiling per app. |
| Per-app key namespacing / isolation | 🟢 | internal (Backend::scope) | `crates/plugin-kv/src/backend/mod.rs` | — | `crates/plugin-kv/tests/redis_backend.rs` | {app_id}:key; braces rejected in user keys. |
| Input validation and limits | 🟢 | internal | `crates/plugin-kv/src/limits.rs` | — | `crates/plugin-kv/tests/e2e_runtime.rs` | key 512B, value 256KiB, list clamp 10000. |
| Typed error classification | 🟢 | error.code on rejected Promises | `crates/plugin-kv/src/error.rs` | `docs/reference/kv.md` | `crates/plugin-kv/tests/e2e_runtime.rs` | Validation → TypeError; runtime → coded. |
| KvPlugin / NativePlugin registration | 🟢 | internal | `crates/plugin-kv/src/lib.rs`, `v8_class.rs` | — | `crates/plugin-kv/tests/e2e_runtime.rs` | One Kv instance/isolate. |
| @zeroship/kv kv.get<T> | 🟢 | `kv.get<T>(key)` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | ok(null) on miss. |
| @zeroship/kv kv.getString | 🟢 | `kv.getString(key)` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | get<string> convenience. |
| @zeroship/kv kv.set<T> | 🟢 | `kv.set<T>(key, value, {ttlMs?})` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | JSON-encodes. |
| @zeroship/kv kv.delete | 🟢 | `kv.delete(key)` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | Result<{deleted}>. |
| @zeroship/kv kv.incr | 🟢 | `kv.incr(key, {by?, ttlMs?})` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | bigint→Number normalize (precision loss>2^53). |
| @zeroship/kv kv.setIfAbsent<T> | 🟢 | `kv.setIfAbsent<T>(key, value, {ttlMs?})` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | Result<{stored}>. |
| @zeroship/kv kv.expire | 🟢 | `kv.expire(key, ttlMs)` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | Result<{updated}>. |
| @zeroship/kv kv.ttl | 🟢 | `kv.ttl(key)` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | 3-state. |
| @zeroship/kv kv.persist | 🟢 | `kv.persist(key)` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | Result<{updated}>. |
| @zeroship/kv kv.list | 🟢 | `kv.list(prefix?, {cursor?, limit?})` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | Pagination loop pattern. |
| @zeroship/kv kv.has | 🟢 | `kv.has(key)` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | Pure JS over get(). |
| @zeroship/kv kv.getOrSet | 🟢 | `kv.getOrSet<T>(key, {ttlMs?}, factory)` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | NOT atomic (stampede possible). |
| @zeroship/kv kv.namespace | 🟢 | `kv.namespace(prefix)` | `sdks/kv/src/index.ts` | `docs/reference/kv.md` | `sdks/kv/tests/kv.test.ts` | String-concat sugar; composes. |
| createKv factory / NativeKv injection | 🟢 | `import { createKv } from "@zeroship/kv"` | `sdks/kv/src/index.ts` | — | `sdks/kv/tests/kv.test.ts` | Mock injection for tests. |
| Backend unavailability / graceful rejection | 🟢 | error.code === 'kv_connection' | `crates/plugin-kv/src/backend/redis.rs`, `error.rs` | — | `crates/plugin-kv/tests/e2e_runtime.rs` | Rejects rather than hangs; retry hint. |
| Redis cluster hash-tag slot targeting | 🟢 | internal | `crates/plugin-kv/src/backend/mod.rs`, `backend/redis.rs` | — | `crates/plugin-kv/tests/redis_backend.rs` | Single-shard ceiling per whale app. |
| Redis list SCAN glob escaping | 🟢 | internal | `crates/plugin-kv/src/limits.rs` | — | `crates/plugin-kv/src/limits.rs` | Escapes glob metachars. |
| Worker / CLI plugin wiring | 🟢 | `zeroship serve` / worker | `crates/cli/src/main.rs`, `crates/worker/src/main.rs` | `docs/reference/kv.md` | — | URL→Redis, else redb; absent → env.kv absent. |

---

## 5. env.storage / @zeroship/storage

An object-store CRUD primitive (`env.storage.*`) wrapped by `@zeroship/storage` as a typed
`Bucket` class. The Rust kernel is fully implemented against a LocalFs backend keyed under
`<root>/<app_id>/<bucket>/<key>`; an S3/R2 backend is declared in a feature flag with **no
implementation**. The SDK ships in the app template, but there is **no reference doc page**.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| env.storage.put | 🟢 | `env.storage.put(bucket, key, bytesBase64, contentType?)` | `crates/plugin-storage/src/callbacks.rs`, `backend/local.rs` | — | `crates/worker/src/handler.rs` | content_type silently ignored; 2x-RAM OOM vector (ST-2). |
| env.storage.get | 🟢 | `env.storage.get(bucket, key)` | `crates/plugin-storage/src/callbacks.rs`, `backend/local.rs` | — | `crates/worker/src/handler.rs` | contentType always null; triple-buffers (ST-3). |
| env.storage.delete | 🟢 | `env.storage.delete(bucket, key)` | `crates/plugin-storage/src/callbacks.rs`, `backend/local.rs` | — | — | Returns false on NotFound. |
| env.storage.list | 🟢 | `env.storage.list(bucket, prefix?)` | `crates/plugin-storage/src/callbacks.rs`, `backend/local.rs` | — | `sdks/create-zeroship-app/template/src/index.ts` | Blocking read_dir; no pagination; follows symlinks (ST-6). |
| Multi-tenancy isolation via app_id | 🟢 | internal | `crates/plugin-storage/src/callbacks.rs`, `backend/local.rs` | — | — | Falls back to "default" if APP_ID absent (ST-5). |
| Path-traversal rejection | 🟡 | internal | `crates/plugin-storage/src/backend/mod.rs` | — | `crates/plugin-storage/src/backend/mod.rs` | No NUL/backslash/dotfile reject (ST-1); list bypasses validator (ST-4). |
| LocalFs backend | 🟢 | internal | `crates/plugin-storage/src/backend/local.rs` | — | — | compio AsyncWriteAt/ReadAt; list walk is blocking std::fs. |
| S3/R2/MinIO/Spaces/B2 backend (s3 flag) | 🟠 | internal | `crates/plugin-storage/Cargo.toml` | — | — | Feature flag exists; zero source files; gates nothing. |
| Pluggable Backend trait | 🟢 | internal | `crates/plugin-storage/src/backend/mod.rs` | `docs/reference/plugin-system.md` | — | async_trait(?Send); ObjectMeta/ListEntry. |
| StoragePlugin constructor variants | 🟢 | internal | `crates/plugin-storage/src/lib.rs` | — | — | ::new is a back-compat alias (removal candidate). |
| StoragePlugin registration | 🟢 | `zeroship serve` / worker --storage-root | `crates/cli/src/main.rs`, `crates/worker/src/main.rs`, `worker/src/cache.rs` | — | — | Degrades gracefully if root empty. |
| @zeroship/storage — Bucket class | 🟢 | `import { Bucket, bucket } from '@zeroship/storage'` | `sdks/storage/src/index.ts` | — | `sdks/create-zeroship-app/template/src/index.ts` | Result envelopes; no tests dir present. |
| @zeroship/storage — bucket() factory | 🟢 | `import { bucket } from '@zeroship/storage'` | `sdks/storage/src/index.ts` | — | `sdks/create-zeroship-app/template/src/index.ts` | Canonical template entry. |
| @zeroship/storage — Bucket.put() | 🟢 | `bucket.put(key, body, opts?)` | `sdks/storage/src/index.ts` | — | `sdks/create-zeroship-app/template/src/index.ts` | contentType discarded by LocalFs. |
| @zeroship/storage — Bucket.get() | 🟢 | `bucket.get(key)` | `sdks/storage/src/index.ts` | — | — | contentType always null in practice. |
| @zeroship/storage — Bucket.getText() | 🟢 | `bucket.getText(key)` | `sdks/storage/src/index.ts` | — | — | Thin UTF-8 wrapper. |
| @zeroship/storage — Bucket.delete() | 🟢 | `bucket.delete(key)` | `sdks/storage/src/index.ts` | — | — | deleted:false if not found. |
| @zeroship/storage — Bucket.list() | 🟢 | `bucket.list(prefix?)` | `sdks/storage/src/index.ts` | — | `sdks/create-zeroship-app/template/src/index.ts` | modifiedAt → Date; inherits unbounded walk. |
| Presigned URL / direct upload URL | 🔵 | `env.storage.presignUrl` (planned) | — | `docs/proposals/feature-roadmap.md` | — | Requires S3 backend. |
| Image resize / transform on upload | 🔵 | @zeroship/storage (planned) | — | `docs/proposals/feature-roadmap.md` | — | No code. |
| content_type sidecar metadata | 🟠 | put contentType / get contentType | `crates/plugin-storage/src/backend/local.rs` | — | — | Accepted, dropped; always null on get. |
| Per-app storage quota enforcement | 🔵 | internal | — | — | — | Open finding ST-2 (HIGH); no code. |

---

## 6. Auth (env.auth + IdP + SDK)

A full OpenID Connect 1.0 / OAuth 2.1 identity platform. `crates/auth` is the login UI and
identity service (ntex, Postgres, Hydra admin client); ory/hydra is the OIDC kernel sidecar;
the gateway is the OIDC RP for every hosted app; `env.auth` (AuthPlugin) is the per-request V8
primitive; `@zeroship/auth` ships a server helper, headless browser client, and React adapter.
Covers password/Google/GitHub, magic-link, TOTP 2FA, sessions, GDPR erasure, relay email,
audit, and a dev-tier parity implementation.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| Password signup | 🟢 | GET/POST /signup | `crates/auth/src/ui/signup.rs`, `identity/password.rs` | `docs/reference/auth.md` | `sdks/auth/tests/identity.test.ts` | Argon2id OWASP 2026; per-IP rate-limit. |
| Password login | 🟢 | GET/POST /login | `crates/auth/src/ui/login.rs`, `identity/password.rs` | `docs/reference/auth.md` | `sdks/auth/tests/identity.test.ts` | Enumeration-resistant; leaky token buckets. |
| Password hashing (Argon2id) | 🟢 | internal | `crates/auth/src/identity/password.rs` | — | — | spawn_blocking for CPU-bound ops. |
| Google OIDC federation | 🟢 | GET /oauth/google/start, /callback | `crates/auth/src/identity/oauth/google.rs`, `ui/oauth_google.rs` | `docs/reference/auth.md` | — | S256 PKCE + nonce + at_hash; hd claim. |
| GitHub OAuth federation | 🟢 | GET /oauth/github/start, /callback | `crates/auth/src/identity/oauth/github.rs`, `ui/oauth_github.rs` | `docs/reference/auth.md` | — | OAuth 2.0 (no ID token); rejects noreply. |
| Account linking / federation linker | 🟢 | GET/POST /link | `crates/auth/src/identity/linker.rs`, `ui/link.rs` | `docs/reference/auth.md` | `crates/auth/src/identity/linker.rs` | 10-min HMAC PendingLink token. |
| OAuth identity unlink | 🟢 | POST /me/unlink/{provider} | `crates/auth/src/ui/me.rs`, `store/identities.rs` | `docs/reference/auth.md` | — | Must-retain-one-credential; CSRF. |
| Magic-link login | 🟢 | POST /magic/start, /await, /verify... | `crates/auth/src/identity/magic_link.rs`, `ui/magic.rs` | `docs/reference/auth.md` | — | 32B CSPRNG; 15-min TTL; cross-device polling. |
| Email verification | 🟢 | GET /verify, POST /verify/redeem | `crates/auth/src/identity/verification.rs`, `ui/verify.rs` | `docs/reference/auth.md` | — | 24h single-use; one-active-per-user. |
| Password reset | 🟢 | GET/POST /forgot, /reset | `crates/auth/src/identity/password_reset.rs`, `ui/forgot.rs`, `ui/reset.rs` | `docs/reference/auth.md` | — | 60-min token; target bound at issue. |
| TOTP 2FA | 🟢 | POST /me/2fa/enroll, /confirm, /disable; /login/2fa | `crates/auth/src/identity/totp.rs`, `ui/totp.rs`, `sessions/totp_challenge.rs` | `docs/reference/auth.md` | `crates/auth/src/identity/totp.rs` | RFC 6238; AES-256-GCM secret bound to user_id. |
| TOTP backup codes | 🟢 | internal | `crates/auth/src/identity/totp.rs` | — | `crates/auth/src/identity/totp.rs` | 10 single-use Argon2id-hashed codes. |
| IdP session management | 🟢 | GET /me/sessions, POST /me/sessions/{id}/revoke | `crates/auth/src/sessions/login.rs`, `store/sessions.rs`, `ui/sessions.rs` | `docs/reference/auth.md` | — | 12h hard / 30m sliding; __Host- cookie. |
| GDPR deletion request / erasure | 🟢 | POST /me/delete, /me/delete/cancel; cron | `crates/auth/src/ui/account_deletion.rs`, `cron/account_reaper.rs` | `docs/reference/auth.md` | `crates/auth/src/cron/account_reaper.rs` | 30-day grace; branches on Stripe history. |
| OIDC consent flow | 🟢 | GET /consent, POST /consent/accept, /deny | `crates/auth/src/ui/consent.rs`, `hydra_client/consent.rs` | `docs/reference/auth.md` | — | Mints relay alias at consent. |
| Device Authorization Grant (RFC 8628) | 🟢 | GET/POST /device | `crates/auth/src/ui/device.rs`, `hydra_client/device.rs` | `docs/reference/auth.md` | — | Requires IdP session; CSRF. |
| RP-initiated logout | 🟢 | GET/POST /logout | `crates/auth/src/ui/logout.rs`, `hydra_client/logout.rs` | `docs/reference/auth.md` | — | Hydra urls.logout. |
| OIDC backchannel logout (BCL 1.0) | 🟢 | POST /oidc/backchannel-logout (gateway) | `crates/gateway/src/backchannel_logout.rs` | `docs/reference/auth.md` | — | jti replay prevention; per-app + global. |
| User profile page (/me) | 🟡 | GET /me | `crates/auth/src/ui/me.rs` | `docs/reference/auth.md` | — | Link-a-new-provider deferred ("coming soon"). |
| JWK rotation cron | 🟢 | internal | `crates/auth/src/cron/jwk_rotation.rs` | — | `crates/runtime/tests/` (jwk_rotation_test) | Daily; EdDSA + RS256. |
| Audit retention cron | 🟢 | internal | `crates/auth/src/cron/audit_retention.rs` | — | `crates/auth/src/cron/audit_retention.rs` | security 365d / PII 90d / debug 30d. |
| Token sweep cron | 🟢 | internal | `crates/auth/src/cron/token_sweep.rs` | — | `crates/auth/src/cron/token_sweep.rs` | Expired magic_links/verifications/etc. |
| Account reaper cron | 🟢 | internal | `crates/auth/src/cron/account_reaper.rs` | — | `crates/auth/src/cron/account_reaper.rs` | Per-user txns; financial-history retention. |
| Audit log (structured events) | 🟢 | internal | `crates/auth/src/audit.rs`, `store/audit.rs` | — | — | emit() swallows PG fail; emit_strict() propagates. |
| Rate limiting (leaky token bucket) | 🟢 | internal | `crates/auth/src/ratelimit.rs`, `store/ratelimit.rs` | — | — | Atomic PG upsert; Bucket::* constants. |
| Mailer abstraction (stdout/SMTP/Resend) | 🟢 | internal | `crates/auth/src/mailer/mod.rs`, `smtp.rs`, `resend.rs`, `stdout.rs` | — | — | Suppression check at trait level. |
| Email suppression list | 🟢 | POST /webhooks/postmark, /ses-sns | `crates/auth/src/store/suppressions.rs`, `ui/webhooks.rs`, `mailer/bounce.rs`, `mailer/sns.rs` | — | — | Basic auth (Postmark); RSA-SHA1 (SES-SNS). |
| Relay email forwarding | 🟢 | POST /webhooks/relay-inbound | `crates/auth/src/store/relay.rs`, `mailer/forward.rs`, `mailer/inbound.rs`, `ui/webhooks.rs` | — | `crates/auth/src/mailer/forward.rs` | Real inbox never in headers; loop cap N=3; one-way v1. |
| Relay alias minting at consent | 🟢 | internal | `crates/auth/src/store/relay.rs` | — | `crates/auth/src/store/relay.rs` | 62-bit base36; re-grant reuses alias. |
| env.auth.getUser() | 🟢 | `env.auth.getUser()` | `crates/runtime/src/auth.rs` | `docs/reference/auth.md` | `crates/runtime/tests/auth_plugin.rs` | Per-request keyed; WS fallback. |
| env.auth.requireUser() | 🟢 | `env.auth.requireUser()` | `crates/runtime/src/auth.rs` | `docs/reference/auth.md` | `crates/runtime/tests/auth_plugin.rs` | Throws → 401; RPC fail-closed (SEC-5). |
| @zeroship/auth server helper | 🟢 | `@zeroship/auth` (server) | `sdks/auth/src/server.ts` | `docs/reference/auth.md` | `sdks/auth/tests/server.test.ts` | getUser/requireUser/isLoggedIn; no signOut. |
| @zeroship/auth headless browser client | 🟢 | `@zeroship/auth/client` | `sdks/auth/src/client.ts`, `internal/` | `docs/reference/auth-dev-tier.md` | `sdks/auth/tests/client.test.ts` | BFF; PKCE S256; no token in browser. |
| @zeroship/auth React adapter | 🟢 | `@zeroship/auth/react` | `sdks/auth/src/react.tsx` | — | `sdks/auth/tests/react.test.tsx` | AuthProvider/useAuth/AuthModal (cross-origin iframe). |
| Session scope step-up / requestScopes | 🟢 | `client.requestScopes(scopes)` | `sdks/auth/src/client.ts` | — | — | prompt:'consent'; no browser token. |
| Auth state change events | 🟢 | `client.onAuthStateChange(cb)` | `sdks/auth/src/client.ts` | — | `sdks/auth/tests/client.test.ts` | RECOVERING event during 503 backoff. |
| Dev-tier auth provider | 🟢 | internal (ZEROSHIP_DEV=1) | `crates/runtime/src/core/dev_auth.rs` | `docs/reference/auth-dev-tier.md` | `sdks/auth/tests/dev-tier.test.ts` | Distinct cookie; contract parity. |
| Pairwise subject identifier (pws_) | 🟢 | internal (User.id) | `crates/gateway/src/identities.rs`, `auth_token.rs` | `docs/reference/auth.md` | — | Per-app opaque; global usr_ never exposed. |
| CSRF protection (double-submit) | 🟢 | internal | `crates/auth/src/csrf.rs` | — | — | Constant-time; __Host- prefix in prod. |
| Hydra admin client | 🟢 | internal | `crates/auth/src/hydra_client/` | — | — | cyper (no tokio). |
| Security headers middleware | 🟢 | internal | `crates/auth/src/headers.rs` | — | — | Per-route frame-ancestors for immersive iframe. |
| Bootstrap (JWK seeding + client reg) | 🟢 | internal (boot) | `crates/auth/src/bootstrap/` | — | — | Advisory lock; backchannel_logout_uri per client. |
| Startup validation | 🟢 | internal | `crates/auth/src/startup_validation.rs` | — | — | Refuses start on misconfig. |
| Account eligibility check | 🟢 | internal | `crates/auth/src/identity/eligibility.rs` | — | — | Guards login/device for disabled/pending. |
| Link-from-/me (add identity) | 🟠 | GET /me (placeholder) | `crates/auth/src/ui/me.rs` | — | — | Only "coming soon" template text. |
| Email address change | 🔵 | none | `crates/auth/src/` | — | — | No handler/store/doc found. |
| Refresh token management | 🟢 | internal (gateway) | `crates/gateway/src/session_token.rs` | `docs/reference/auth.md` | `sdks/auth/tests/refresh.test.ts` | reuse-detected never swept; BFF holds token. |

---

## 7. RPC / server functions

A typed, transport-safe server-function stack. Server modules declare procedures with
`"use server"` + wrappers from `@zeroship/rpc/server`; the Vite plugin discovers them, emits
client stubs, and builds manifest resources. Clients use generated stubs or `createRpcClient`.
Transport covers query (GET/POST), mutation/action (POST), stream (SSE/AI-SDK), and WebSocket
subscriptions. The gateway enforces fail-closed auth (default `user` for all `rpc:`).

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| query procedure wrapper | 🟢 | `query(handler, config?)` | `sdks/rpc/src/server.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/server.test.ts` | GET stub; query() can't fetch. |
| mutation procedure wrapper | 🟢 | `mutation(handler, config?)` | `sdks/rpc/src/server.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/server.test.ts` | Generic procedure defaults to mutation. |
| action procedure wrapper | 🟢 | `action(handler, config?)` | `sdks/rpc/src/server.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/server.test.ts` | Manifest folds action → omitted kind. |
| stream procedure wrapper | 🟢 | `stream(handler, config?)` | `sdks/rpc/src/server.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/stream.test.ts` | AI-SDK Data Stream Protocol. |
| subscription procedure wrapper | 🟡 | `subscription(handler, config?)` | `sdks/rpc/src/server.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/subscription.test.ts` | Transport works; public client proxy UNIMPLEMENTED; use stream(). |
| generic procedure wrapper | 🟢 | `procedure(handler, config)` | `sdks/rpc/src/server.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/server.test.ts` | Needs config.kind. |
| streamResponse procedure wrapper | 🟢 | `streamResponse(handler, config?)` | `sdks/rpc/src/server.ts` | — | — | Returns raw Response. |
| 'use server' file-level discovery | 🟢 | internal (Vite transform) | `sdks/vite-plugin/src/transform.ts` | `docs/reference/rpc.md` | `sdks/vite-plugin/test/` | Directive is the only opt-in now. |
| 'use server' function-level discovery | 🟢 | internal (Vite transform) | `sdks/vite-plugin/src/transform.ts` | `docs/reference/rpc.md` | `sdks/vite-plugin/test/` | Per-binding server reference. |
| reference-graph walk (Phase-2) | 🟢 | internal | `sdks/vite-plugin/src/server-graph.ts` | — | `sdks/vite-plugin/test/` | Static dispatch table; falls back to namespace-walk. |
| Vite-generated client stubs | 🟢 | internal (client env) | `sdks/vite-plugin/src/transform.ts` | `docs/reference/rpc.md` | `sdks/vite-plugin/test/` | __zsRpc.query/mutation/stream; subscription fails. |
| synthetic server entry (virtual) | 🟢 | `virtual:zeroship/_server-entry` | `sdks/vite-plugin/src/rpc-registry.ts` | `docs/reference/zeroship-standard.md` | `sdks/vite-plugin/test/` | namespace-walk default; Phase-2 dict optional. |
| lazy procedure loading | 🟢 | `query(handler, { lazy: true })` | `sdks/vite-plugin/src/transform.ts`, `rpc-registry.ts` | — | — | Literal boolean only. |
| procedure wire ID (id config) | 🟢 | `query(handler, { id: 'todos.list' })` | `sdks/vite-plugin/src/manifest.ts`, `sdks/rpc/src/server.ts` | `docs/reference/rpc.md` | — | Prod rejects bare-name; collision check. |
| bootstrap dispatcher (__zsDispatch) | 🟢 | internal | `sdks/bootstrap/src/dispatcher.ts` | — | `sdks/bootstrap/tests/` | Single dispatch source; dev+prod. |
| fetch handler (createFetchHandler) | 🟢 | internal | `sdks/bootstrap/src/fetch-handler.ts` | — | `sdks/bootstrap/tests/` | GET base64url / POST JSON; SuperJSON envelope. |
| Zod input validation | 🟢 | `query(handler, { input })` | `sdks/bootstrap/src/dispatcher.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/make-procedure.test.ts` | Any .parse()-able. |
| Zod output validation (dev-only) | 🟡 | `query(handler, { output })` | `sdks/bootstrap/src/dispatcher.ts` | `docs/reference/rpc.md` | — | __zsValidateOutput never set true today. |
| B3 capability frame enforcement | 🟢 | internal (__zsEnterKind/__zsExitKind) | `crates/runtime/src/core/init.rs`, `sdks/bootstrap/src/dispatcher.ts` | — | — | Falls back to no-op without natives. |
| runQuery / runMutation composition | 🟢 | `runQuery(fn, args)` / `runMutation(...)` | `crates/runtime/src/core/init.rs` | — | — | Only in action/stream/subscription. |
| per-request context getters | 🟢 | `currentUser()` / `currentRequestId()` / ... | `crates/runtime/src/core/init.rs` | — | — | Via `zeroship` synthetic module. |
| transport — query (GET/POST fallback) | 🟢 | `rpc.query(id)` | `sdks/rpc/src/transport.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/transport.test.ts` | 6 KB URL threshold. |
| transport — mutation/action (POST) | 🟢 | `rpc.mutation(id)` / `rpc.action(id)` | `sdks/rpc/src/transport.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/transport.test.ts` | Idempotency-Key UUIDv7. |
| transport — stream (SSE/AI-SDK) | 🟢 | `rpc.stream(id)` | `sdks/rpc/src/transport.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/stream.test.ts` | Demand-driven iteration. |
| transport — WebSocket subscription | 🟢 | `client(opts).proc.subscribe(input, opts)` | `sdks/rpc/src/transport.ts` | — | `sdks/rpc/test/subscription.test.ts` | zs.v1 subprotocol; auto-reconnect. |
| streamUrl helper | 🟢 | `rpc.stream('id').streamUrl(input)` | `sdks/rpc/src/transport.ts`, `runtime.ts` | `docs/reference/rpc.md` | — | For ai-sdk useChat. |
| retry policy (unary) | 🟢 | `createRpcClient({ retry })` | `sdks/rpc/src/transport.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/transport.test.ts` | Writes retry only with idempotency key. |
| call timeout | 🟢 | `createRpcClient({ timeout })` | `sdks/rpc/src/transport.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/transport.test.ts` | Composes with caller signal. |
| idempotency-key support | 🟢 | `mutation(handler, { idempotent: true })` | `sdks/rpc/src/idempotency.ts`, `transport.ts`, `crates/gateway/src/router/dispatch.rs` | `docs/reference/rpc.md` | `sdks/rpc/test/idempotency.test.ts` | TTL 24h default, 7d max; fails closed. |
| auth resolver (client-side) | 🟢 | `createRpcClient({ auth })` | `sdks/rpc/src/client.ts`, `runtime.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/auth.test.ts` | Null result → anon call. |
| gateway auth fail-closed default (SEC-5) | 🟢 | internal | `crates/gateway/src/compiled.rs` | `docs/reference/rpc.md` | `crates/gateway/src/compiled.rs` | rpc: defaults to user; anon needs publiclyAccessible. |
| RpcError typed error class | 🟢 | `import { RpcError, ErrorCode }` | `sdks/rpc/src/error.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/error.test.ts` | 14 gRPC-style codes. |
| JSON transformer (default) | 🟢 | `createRpcClient({ transformer: 'json' })` | `sdks/rpc/src/encoding.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/transport.test.ts` | Unwraps {json, meta}. |
| superjson transformer | 🟢 | `createRpcClient({ transformer: 'superjson' })` | `sdks/rpc/src/encoding.ts` | `docs/reference/rpc.md` | — | Optional peer dep; must match server. |
| query auto-batching | 🟢 | `client({ batch: true })` | `sdks/rpc/src/batch.ts` | — | `sdks/rpc/test/batch.test.ts` | Mutations/streams never batched. |
| proxy-style typed client | 🟢 | `client<App>(opts).proc.query(input)` | `sdks/rpc/src/client.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/client.test.ts` | Dotted ids expand to nested objects. |
| factory-style typed client | 🟢 | `createRpcClient<Contract>(opts).query('id')` | `sdks/rpc/src/runtime.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/runtime.test.ts` | configureRpcClient installs defaults. |
| InferRpcContract type helper | 🟢 | `InferRpcContract<typeof server>` | `sdks/rpc/src/types.ts` | `docs/reference/rpc.md` | `sdks/rpc/test/typecheck.ts` | Type-check time only. |
| server-reference brand | 🟢 | internal | `sdks/rpc/src/make-procedure.ts` | — | — | Symbol.for('zeroship/server-reference'). |
| module-level $config | 🟢 | `export const $config = {...}` | `sdks/vite-plugin/src/transform.ts`, `manifest.ts` | — | — | Lower precedence than fn.config. |
| defineApp resource tree + RPC defaults | 🟢 | `defineApp({ resources, rpc })` | `sdks/server/src/define-app.ts`, `types.ts`, `sdks/vite-plugin/src/manifest.ts` | `docs/reference/rpc.md` | — | override array required to shadow. |
| per-procedure rate limiting | 🟢 | `query(handler, { rateLimit })` | `sdks/server/src/types.ts`, `crates/gateway/src/router/dispatch.rs` | `docs/reference/rpc.md` | — | ip/user/session/app; min-wins. |
| per-procedure timeout | 🟡 | `query(handler, { timeout })` | `sdks/server/src/types.ts`, `crates/gateway/src/compiled.rs` | `docs/reference/rpc.md` | — | timeout_ms hardcoded None; not enforced. |
| per-procedure max input bytes | 🟢 | `mutation(handler, { maxInputBytes })` | `sdks/server/src/types.ts`, `crates/gateway/src/router/dispatch.rs` | `docs/reference/rpc.md` | — | Gateway enforces. |
| per-procedure middleware list | 🟠 | `query(handler, { middleware })` | `sdks/server/src/types.ts`, `sdks/vite-plugin/src/manifest.ts` | `docs/reference/rpc.md` | — | Carried in manifest; runtime chain not wired. |
| dev HMR registry (__registerModule) | 🟢 | internal | `sdks/vite-plugin/src/dev-bootstrap/rpc-registry.ts`, `transform.ts` | — | — | No-op in production. |
| configureRpcClient global defaults | 🟢 | `configureRpcClient(opts)` | `sdks/rpc/src/runtime.ts` | `docs/reference/rpc.md` | — | Returns restore fn. |
| onError / onAuthExpired global hooks | 🟢 | `createRpcClient({ onError, onAuthExpired })` | `sdks/rpc/src/transport.ts`, `client.ts` | `docs/reference/rpc.md` | — | onAuthExpired once per expiry. |
| @zeroship/rpc-react adapter | 🟡 | `ZeroshipProvider` / `useStream` / `rpcInvalidate` | `sdks/rpc-react/dist/` | — | — | Dist-only; TanStack hook integration not done. |
| @zeroship/rpc-client proxy package | 🟡 | `@zeroship/rpc-client` | `sdks/rpc-client/dist/` | — | — | Dist-only; HookRegistry slots unfilled. |
| defineRpcProcedures type helper | 🟢 | `defineRpcProcedures<Contract>()(procedures)` | `sdks/rpc/src/runtime.ts` | — | — | Registry-backed clients. |
| newUuidV7 (public utility) | 🟢 | `import { newUuidV7 } from '@zeroship/rpc/client'` | `sdks/rpc/src/idempotency.ts` | — | — | RFC 9562. |

---

## 8. Deploy contract / bootstrap

How a zeroship app module is loaded and dispatched. It centers on a standard default-export
shape (`{ fetch?, rpc? }`), which the runtime bootstrap
(`crates/runtime/src/core/init.rs` + `sdks/bootstrap/`) wraps around every user module before
V8 evaluates it. The bootstrap package is framework-internal: the runtime crate `include_str!`s
its compiled dist files and the Vite plugin imports it for dev. User code must not import it.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| default-export contract | 🟢 | internal (user module namespace) | `crates/runtime/src/core/init.rs` | `docs/reference/zeroship-standard.md` | `examples/raw-rpc.js` | fetchFast recognized but undocumented. |
| normalizeUserModule | 🟢 | `@zeroship/bootstrap` API | `sdks/bootstrap/src/normalize.ts` | — | `sdks/bootstrap/tests/install-schema.test.ts` | Named exports win over default.rpc; registry wins. |
| installSchema / schema auto-discovery | 🟢 | `@zeroship/bootstrap/install-schema` | `sdks/bootstrap/src/install-schema.ts` | — | `sdks/bootstrap/tests/install-schema.test.ts` | registerModel in topo order; __zsDbPlatform deleted after. |
| normalizeSchema + expandUnionToFlatColumns | 🟢 | `@zeroship/bootstrap/install-schema` | `sdks/bootstrap/src/install-schema.ts` | — | `sdks/bootstrap/tests/install-schema.test.ts` | Refuses reserved system names. |
| validateRefTargets | 🟢 | `@zeroship/bootstrap/install-schema` | `sdks/bootstrap/src/install-schema.ts` | — | `sdks/bootstrap/tests/install-schema.test.ts` | Cross-app refs blocked at install. |
| __zsDispatch (embedded RPC dispatcher) | 🟢 | internal | `sdks/bootstrap/src/dispatcher.ts` | `docs/reference/zeroship-standard.md` | `examples/raw-rpc.js` | Idempotent IIFE; dev+prod identical JS. |
| runtime-entry (prod TLA orchestrator) | 🟢 | internal (include_str!) | `sdks/bootstrap/src/runtime-entry.ts`, `crates/runtime/src/core/init.rs` | `docs/reference/zeroship-standard.md` | `examples/raw-rpc.js` | Runs after dispatcher; skips if env.db absent. |
| dev-entry (dev coordinator) | 🟢 | internal (@zeroship/bootstrap dev) | `sdks/bootstrap/src/dev-entry.ts` | — | — | Not in barrel; lazy schema install. |
| createFetchHandler (WinterCG wrapper) | 🟢 | internal | `sdks/bootstrap/src/fetch-handler.ts` | — | `sdks/bootstrap/tests/fetch-handler.test.ts` | 5xx sanitized in prod; SuperJSON optional. |
| dev-tier auth provider | 🟢 | internal (@zeroship/bootstrap/dev-auth) | `sdks/bootstrap/src/dev-auth.ts` | `docs/reference/auth-dev-tier.md` | `sdks/bootstrap/tests/dev-auth.test.ts` | Absent from .zship; byte-compatible with Rust. |
| WS subscription dispatch | 🟢 | `default.subscribe` (kernel-called) | `crates/runtime/src/core/init.rs` | — | `examples/raw-streaming.js` | hello/data/ping/pong; close 4400/4408. |
| fetchFast extension | 🟢 | internal (user namespace) | `crates/runtime/src/core/init.rs` | — | — | Signature undocumented; no example. |
| zeroship facade module | 🟢 | `import { env } from 'zeroship'` | `crates/runtime/src/core/init.rs` | — | `examples/http-handler.js` | env/waitUntil/getRequest/current*/runQuery. |
| raw-JS deploy (no-tooling) | 🟢 | `zeroship serve <file>.js` | `crates/runtime/src/core/init.rs` | `docs/reference/zeroship-standard.md` | `examples/raw-rpc.js` | Dict or function-shape rpc; fn.config drives frames. |
| mask policy flush at boot | 🟢 | internal | `sdks/bootstrap/src/runtime-entry.ts`, `dev-entry.ts` | — | — | Single-shot at cold start. |
| __zsDbPlatform resolver + capability boundary | 🟢 | internal (V8 Private symbol) | `crates/runtime/src/core/init.rs` | — | — | Resolver deleted after use (P9 §8). |
| legacy fallback fetch / user.index() | 🟡 | internal (fallbackFetch) | `crates/runtime/src/core/init.rs` | — | — | Undocumented "legacy"; unary RPC fallthrough → 404. |
| manifest.exports.schema (deprecated field) | ⚫ | internal (manifest wire) | `crates/bundle/src/manifest.rs` | — | — | No longer read/written; kept for archive upgrade. |
| bootstrap build ordering (pnpm before cargo) | 🟢 | internal (build toolchain) | `sdks/bootstrap/scripts/post-build.mjs` | `sdks/bootstrap/README.md` | — | post-build strips export marker for splice. |

---

## 9. Gateway

The platform's edge layer for the App Runtime. It handles manifest dispatch, multi-arm
JWT/session/DPoP authentication, CORS, rate/concurrency limiting, idempotent RPC dedup, static
asset serving with tiered cache, OIDC RP for hosted apps, browser-facing BFF auth endpoints,
back-channel logout, CHWBL worker routing, and the auth.zeroship.ai reverse proxy. All features
are internally accessed; end-users hit it indirectly via HTTP.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| Manifest-based resource-tree dispatch | 🟢 | internal | `crates/gateway/src/compiled.rs`, `router/dispatch.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/src/compiled.rs` | Rule-walker removed; resources is the only path. |
| Subdomain routing (Host-header) | 🟢 | internal | `crates/gateway/src/router/dispatch.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/src/router/dispatch.rs` | Path-based takes priority over subdomain. |
| Path canonicalization + traversal rejection (SEC-2) | 🟢 | internal | `crates/gateway/src/compiled.rs`, `router/dispatch.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/src/compiled.rs` | Canonical form used for auth match + forward. |
| Compiled policy inheritance | 🟢 | internal | `crates/gateway/src/compiled.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/src/compiled.rs` | timeout_ms declared but None. |
| RPC fail-closed default (SEC-5) | 🟢 | internal | `crates/gateway/src/compiled.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/src/compiled.rs` | rpc: defaults to User. |
| Multi-arm per-request auth (cookie/Bearer/DPoP) | 🟢 | internal | `crates/gateway/src/router/auth.rs` | `docs/reference/auth.md` | `crates/gateway/tests/dpop_e2e.rs` | DPoP cnf.jkt binding deferred; fail-closed. |
| Pairwise subject projection (pws_) | 🟢 | internal | `crates/gateway/src/router/auth.rs` | `docs/reference/auth.md` | `crates/gateway/tests/identities_relay_test.rs` | 503 if no sector_identifier. |
| Per-app family-marker revocation + cache (R1d) | 🟢 | internal | `crates/gateway/src/router/auth.rs` | `docs/reference/auth.md` | `crates/gateway/tests/auth_token_anchors_test.rs` | Fail-closed on cache-miss + DB error. |
| BFF browser-auth endpoints | 🟢 | /__zeroship/auth/authorize, /popup-callback, /signout | `crates/gateway/src/browser_auth.rs` | `docs/reference/auth.md` | `crates/gateway/tests/browser_auth_test.rs` | Same-origin only; strict CSP relay. |
| BFF session token endpoint | 🟢 | POST/GET /__zeroship/auth/session | `crates/gateway/src/auth_token.rs` | `docs/reference/auth.md` | `crates/gateway/tests/auth_token_anchors_test.rs` | No JWT in body; reload-storm coalescing. |
| Stateless signed session cookie | 🟢 | internal | `crates/gateway/src/session_token.rs`, `signing.rs` | `docs/reference/auth.md` | `crates/gateway/tests/auth_token_anchors_test.rs` | Ed25519; verified locally, no DB. |
| OIDC RP authorize→callback→mint | 🟢 | GET /__zeroship/auth/callback | `crates/gateway/src/oidc_rp.rs`, `router/dispatch.rs` | `docs/reference/auth.md` | `crates/gateway/tests/oidc_rp_e2e.rs` | HMAC-signed stash cookie. |
| DPoP proof verification + jti replay (RFC 9449) | 🟢 | internal | `crates/gateway/src/router/auth.rs`, `lib.rs` | `docs/reference/auth.md` | `crates/gateway/tests/dpop_e2e.rs` | 120s window; per-app client_id binding. |
| OIDC back-channel logout webhook | 🟢 | POST /oidc/backchannel-logout | `crates/gateway/src/backchannel_logout.rs` | — | `crates/gateway/tests/backchannel_logout_test.rs` | DB released before outbound Hydra revoke. |
| auth.zeroship.ai reverse proxy | 🟢 | internal | `crates/gateway/src/router/dispatch.rs` | — | `crates/gateway/tests/oidc_rp_e2e.rs` | XFF re-authored (SEC-3); exact host match. |
| CSRF origin guard (cookie mutations) | 🟢 | internal | `crates/gateway/src/router/auth.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/tests/oidc_rp_e2e.rs` | Mismatch drops cookie; Bearer/DPoP exempt. |
| Route-level OAuth scope enforcement | 🟢 | internal | `crates/gateway/src/router/auth.rs`, `compiled.rs` | `docs/reference/auth.md` | `crates/gateway/src/sync.rs` | Only User/Admin routes; anon never 403s. |
| CORS preflight + header injection | 🟢 | internal | `crates/gateway/src/router/cors.rs`, `router/dispatch.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/src/router/cors.rs` | Wildcard requires no credentials. |
| Global per-app rate limiting | 🟢 | internal | `crates/gateway/src/enforce.rs` | — | `crates/gateway/src/enforce.rs` | Boot-time bucket; separate from per-resource. |
| Per-resource rate limiting (4 scopes) | 🟢 | internal | `crates/gateway/src/enforce.rs`, `router/dispatch.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/src/enforce.rs` | ip/user/session/app; Retry-After. |
| Per-app concurrency limiting (RAII) | 🟢 | internal | `crates/gateway/src/enforce.rs` | — | `crates/gateway/src/enforce.rs` | Subscriptions hold one slot for life. |
| Idempotency dedup for RPC mutations | 🟢 | internal | `crates/gateway/src/idempotency.rs`, `router/dispatch.rs` | `docs/architecture/gateway-routing.md` | — | In-memory default; Redis store not wired in main. |
| Max-input-bytes request body cap | 🟢 | internal | `crates/gateway/src/router/dispatch.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/src/compiled.rs` | 413 on overflow. |
| ProcedureKind method gate | 🟢 | internal | `crates/gateway/src/router/dispatch.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/src/compiled.rs` | 405/426; Action None-kind path. |
| CHWBL hash-ring worker routing | 🟢 | internal | `crates/gateway/src/proxy.rs` | `docs/architecture/gateway-routing.md` | — | 150 vnodes; Unix socket support. |
| Worker dispatch proxy + connection pooling | 🟢 | internal | `crates/gateway/src/proxy.rs` | `docs/architecture/distributed.md` | — | Reserved headers scrubbed. |
| ZeroShip-User HMAC signing | 🟢 | internal | `crates/gateway/src/proxy.rs`, `oidc_rp.rs` | `docs/architecture/distributed.md` | — | Empty worker_key disables (dev). |
| App response header sanitization (SEC-9) | 🟢 | internal | `crates/gateway/src/proxy.rs` | — | `crates/gateway/src/proxy.rs` | Strips cookie Domain; caps count/size. |
| Route cache with 5s polling | 🟢 | internal | `crates/gateway/src/sync.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/src/sync.rs` | Push-pull; configurable interval. |
| Static asset serving (tiered cache) | 🟢 | internal | `crates/gateway/src/router/static_serve.rs`, `blob_cache.rs`, `router/variants.rs`, `conditional.rs` | `docs/architecture/gateway-routing.md` | — | mem→disk→BlobStore; br/gzip; 304/206. |
| Redirect and rewrite actions | 🟢 | internal | `crates/gateway/src/compiled.rs`, `router/dispatch.rs` | `docs/architecture/gateway-routing.md` | `crates/gateway/src/compiled.rs` | Recursive rewrites unsupported. |
| WS subscription affinity routing | 🟡 | internal | `crates/gateway/src/router/dispatch.rs` | `docs/architecture/gateway-routing.md` | — | Affinity runs; WS proxy returns 501 (use zeroship serve). |
| API key validation (legacy) | 🟡 | internal | `crates/gateway/src/auth.rs` | — | — | check_api_key not called from main dispatch. |
| Hydra client with circuit breaker | 🟢 | internal | `crates/gateway/src/hydra_client.rs`, `oidc_rp.rs` | — | `crates/gateway/tests/hydra_breaker_test.rs` | 4xx doesn't trip; only transport/timeouts. |
| Per-app anchor store (reload-recovery) | 🟢 | internal | `crates/gateway/src/anchors.rs` | `docs/reference/auth.md` | `crates/gateway/tests/auth_token_anchors_test.rs` | 30-day; single-flight per anchor. |
| Gateway sessions store (audit/revocation) | 🟢 | internal | `crates/gateway/src/sessions.rs` | `docs/reference/auth.md` | `crates/gateway/tests/sessions_test.rs` | Not read on hot path (R1b). |
| Per-app identities store (pws_ + relay) | 🟢 | internal | `crates/gateway/src/identities.rs` | `docs/reference/auth.md` | `crates/gateway/tests/identities_relay_test.rs` | Relay lookup fail → empty email. |
| Row-level-security tenant isolation | 🟢 | internal | `crates/gateway/src/rls.rs` | — | `crates/gateway/tests/sessions_test.rs` | SET LOCAL GUC; unset fails closed. |
| Per-thread PostgreSQL pool | 🟢 | internal | `crates/gateway/src/db.rs`, `lib.rs` | — | `crates/gateway/tests/db_pool_smoke.rs` | !Send; None in dev no-DB. |
| Trust-proxy client IP derivation | 🟢 | internal | `crates/gateway/src/router/dispatch.rs` | — | — | Default false; behind trusted L7 only. |
| DPoP jti replay cache (tiered) | 🟢 | internal | `crates/gateway/src/lib.rs` (TieredJtiCache) | — | `crates/gateway/tests/dpop_e2e.rs` | 120s window. |
| Insecure-dev mode (HTTP cookies) | 🟢 | internal (--insecure-dev) | `crates/gateway/src/lib.rs`, `main.rs` | — | — | Prod must be false (__Host- needs Secure). |
| x-wall-time-ms response header | 🟢 | internal | `crates/gateway/src/router/dispatch.rs` | — | — | Informational. |
| Relay email alias (email-claim swap §7) | 🟢 | internal | `crates/gateway/src/router/auth.rs`, `identities.rs` | `docs/reference/auth.md` | `crates/gateway/tests/identities_relay_test.rs` | All 3 auth arms; fail-closed empty. |
| Ed25519 signing key load + rotation overlap | 🟢 | internal (--signing-key-file) | `crates/gateway/src/signing.rs`, `session_token.rs` | — | — | --prev-signing-key-file overlap; perm check. |

---

## 10. Control plane (crates/control)

The creator/admin API server: app CRUD, deploy ingest, env/secrets, route and version feeds,
billing/Stripe Connect, platform admin, Cedar authz, PAT issuance, OAuth client management,
audit, and crons. It is a pure REST resource server (no OIDC RP of its own after R5); every
caller authenticates via a PAT (Ed25519/JWT) or an OAuth access token introspected via Hydra.
`@zeroship/control` wraps the HTTP surface for platform-owned code.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| App CRUD — create/get/list/delete | 🟢 | POST/GET/DELETE /api/apps | `crates/control/src/api.rs` | `docs/reference/control.md` | `crates/control/tests/registry_schema_test.rs` | list scoped to ownership (C1 fix). |
| Deploy ingest (.zship) | 🟢 | POST /api/apps/{id}/deploy | `crates/control/src/api.rs`, `deploy.rs` | `docs/architecture/control-plane.md` | `crates/control/tests/deploy_test.rs` | Scope validation before provisioning. |
| Route feed for gateway | 🟢 | GET /internal/routes | `crates/control/src/internal.rs`, `registry.rs` | `docs/architecture/control-plane.md` | — | Passthrough fallback on null manifest. |
| Version feed for workers | 🟢 | GET /internal/versions, /internal/apps/{id} | `crates/control/src/internal.rs`, `registry.rs` | `docs/architecture/control-plane.md` | — | env_version is lazy refetch trigger. |
| Env feed for workers | 🟢 | GET /internal/apps/{id}/env | `crates/control/src/internal.rs`, `env_store.rs` | `docs/architecture/control-plane.md` | `crates/control/tests/env_store.rs` | 404 on deleted app. |
| Usage ingest from workers | 🟢 | POST /internal/usage | `crates/control/src/internal.rs`, `registry.rs` | `docs/reference/billing-metering.md` | — | Upsert; positive deltas only. |
| Usage read for creators | 🟢 | GET /api/apps/{id}/usage | `crates/control/src/api.rs`, `registry.rs` | `docs/reference/billing-metering.md` | — | JSON by resource. |
| Plan management | 🟢 | PUT /api/apps/{id}/plan | `crates/control/src/api.rs`, `registry.rs` | `docs/reference/control.md` | — | Limits hardcoded in runtime_limits_for_plan. |
| App logs fan-out | 🟢 | GET /api/apps/{id}/logs | `crates/control/src/api.rs` | `docs/reference/control.md` | `crates/control/tests/app_logs_http_test.rs` | 2s/worker; 502 only if all fail. |
| Env vars CRUD | 🟢 | GET/PUT/DELETE /api/apps/{id}/env/vars | `crates/control/src/env_handlers.rs`, `env_store.rs` | `docs/reference/control.md` | `crates/control/tests/env_store.rs` | env_version bump; audited. |
| Secrets CRUD (encrypted) | 🟢 | GET/PUT/DELETE /api/apps/{id}/env/secrets | `crates/control/src/env_handlers.rs`, `env_store.rs` | `docs/reference/control.md` | `crates/control/tests/env_store.rs` | AES-256-GCM; AAD binds app+key. |
| Secret key rotation (re-encrypt) | 🟢 | internal (EnvStore::rotate_app) | `crates/control/src/env_store.rs` | — | — | No HTTP endpoint to trigger yet. |
| Secret process.env exposure list | 🟢 | GET/PUT /api/apps/{id}/env/expose | `crates/control/src/env_handlers.rs`, `env_store.rs` | `docs/reference/control.md` | — | Atomic; audited. |
| Audit log read (per-app) | 🟢 | GET /api/apps/{id}/audit | `crates/control/src/env_handlers.rs`, `audit.rs` | `docs/reference/control.md` | — | Append-only with tamper trigger. |
| Cedar-backed authorization (AuthzGuard) | 🟢 | internal | `crates/control/src/authz_guard.rs` | — | `crates/control/tests/authz_guard_oauth_test.rs` | PAT first, then Hydra introspection. |
| Personal Access Token (PAT) issuance | 🟢 | POST/GET /api/me/tokens, DELETE /{id} | `crates/control/src/token_handlers.rs` | — | `crates/control/tests/token_handlers_test.rs` | Grant-subset; no PAT-chains; 365d max. |
| Platform admin role management | 🟢 | POST/DELETE/GET /api/admin/users/{id}/role | `crates/control/src/admin_handlers.rs` | — | `crates/control/tests/admin_handlers_test.rs` | Invalidates EntityCache. |
| Platform Cedar policy CRUD | 🟢 | GET/PUT/DELETE /api/admin/platform-policies | `crates/control/src/admin_handlers.rs` | — | `crates/control/tests/admin_handlers_test.rs` | Validated before write; audit diff. |
| App audit-lock | 🟢 | POST /api/admin/apps/{id}/audit-lock | `crates/control/src/admin_handlers.rs` | — | — | Flag stored; not yet enforced in sweep. |
| App suspension | 🟡 | POST /api/admin/apps/{id}/suspend | `crates/control/src/admin_handlers.rs` | — | — | Flag written; gateway does not act on it. |
| First-party OAuth client registration | 🟢 | POST/GET/DELETE /api/admin/oauth-clients | `crates/control/src/oauth_handlers.rs` | — | `crates/control/tests/oauth_handlers_test.rs` | Hydra-rollback on DB failure. |
| Per-app OAuth client provisioning (auto) | 🟢 | internal (create_app + deploy) | `crates/control/src/app_oauth_client.rs` | — | `crates/control/tests/app_oauth_client_test.rs` | Idempotent; non-destructive URI merge. |
| Custom-domain OAuth redirect URI sync | 🟡 | internal (sync_app_redirect_uris) | `crates/control/src/app_oauth_client.rs` | — | — | Implemented/tested; no production caller. |
| App-declared OAuth scope registry | 🟢 | internal (deploy) | `crates/control/src/app_oauth_client.rs`, `api.rs` | — | `crates/control/tests/app_oauth_client_test.rs` | Hard-fails deploy on vocab collision. |
| OAuth grant listing (user-facing) | 🟢 | GET /api/me/oauth-grants | `crates/control/src/oauth_grants_handlers.rs` | — | `crates/control/tests/oauth_grants_handlers_test.rs` | Joined with client metadata. |
| OAuth grant revocation (user-facing) | 🟢 | DELETE /api/me/oauth-grants/{client_id} | `crates/control/src/oauth_grants_handlers.rs` | — | `crates/control/tests/oauth_grants_handlers_test.rs` | Owned connection; family marker; Hydra delete. |
| Stripe Connect onboarding | 🟡 | POST /api/creators/{id}/stripe/onboard | `crates/control/src/stripe_handlers.rs` | `docs/reference/billing-metering.md` | — | Placeholder URL; real account_links TODO. |
| Stripe Connect account link / unlink | 🟢 | POST /callback, DELETE /api/creators/{id}/stripe | `crates/control/src/stripe_handlers.rs`, `stripe_store.rs` | `docs/reference/billing-metering.md` | `crates/control/tests/stripe_store.rs` | Cascades payouts. |
| Stripe webhook ingest (invoice.paid) | 🟢 | POST /internal/webhooks/stripe | `crates/control/src/stripe_handlers.rs` | `docs/reference/billing-metering.md` | `crates/control/tests/stripe_webhook_test.rs` | HMAC; idempotent on event_id. |
| Creator earnings view | 🟢 | GET /api/creators/{id}/earnings | `crates/control/src/stripe_handlers.rs`, `stripe_store.rs` | `docs/reference/billing-metering.md` | — | Cents; 50 recent payouts. |
| Console bootstrap seed | 🟢 | --bootstrap-console / BOOTSTRAP_CONSOLE=1 | `crates/control/src/bootstrap_console.rs` | — | `crates/control/tests/bootstrap_console_test.rs` | Deterministic id; system=true. |
| Builder OAuth client bootstrap | 🟡 | --bootstrap-builder-client | `crates/control/src/bootstrap_builder.rs` | — | `crates/control/tests/bootstrap_builder_test.rs` | Retired in R5; code retained (effectively dead). |
| Orphaned-app reaper (cron) | 🟢 | internal | `crates/control/src/cron/orphaned_app_reaper.rs` | — | `crates/control/tests/orphaned_app_reaper_test.rs` | system=true never reaped. |
| Audit retention sweep (cron) | 🟢 | internal | `crates/control/src/cron/audit_retention.rs` | — | `crates/control/tests/audit_retention_test.rs` | GUC flag cleared on exit. |
| Rate limiting (per-IP token bucket) | 🟢 | internal | `crates/control/src/rate_limit.rs`, `http_util.rs` | — | — | In-memory per-process; DB-backed for multi-node. |
| Metering aggregation helpers | 🟠 | internal | `crates/control/src/metering.rs` | — | — | Single-line stub comment; no impl. |
| Control health check | 🟢 | GET /internal/health | `crates/control/src/internal.rs` | — | — | Liveness only. |
| TypeScript control client (@zeroship/control) | 🟢 | `@zeroship/control` npm | `sdks/control/src/index.ts` | `docs/reference/control.md` | — | Auth namespace removed (R5); missing admin/PAT wrappers. |
| Config validation (--check-config) | 🟢 | --check-config [--format] | `crates/control/src/main.rs` | — | — | Text/JSON; prod startup guards. |

---

## 11. Authorization (Cedar)

A Cedar-backed policy engine (`crates/authz`) wired into the control plane via an ntex
extractor (AuthzGuard). P9 shipped the platform RBAC half (engine, static platform+creator
policies, PAT issuance with TOKEN⊂USER enforcement, OAuth scope mapping, consent gate, admin
policy CRUD, audit). P10 (toggle-matrix UI), P11 (orgs, analyzer, incident lock), and P12
(end-user authz in worker via plugin-authz + @zeroship/permissions) are documented with **zero
code on disk**.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| Cedar engine (Authorizer) | 🟢 | internal | `crates/authz/src/engine.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/engine_test.rs` | Schema field always None; DB policy rows not merged at boot. |
| Wrapper Policy / Statement / Effect types | 🟢 | internal | `crates/authz/src/policy.rs`, `statement.rs`, `effect.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/scaffold_test.rs` | serde round-trip tested. |
| Action enum (closed vocabulary) | 🟢 | internal | `crates/authz/src/action.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/scaffold_test.rs` | 17 variants; PlatformPoliciesWrite extra. |
| Resource enum (App, Org, Any) | 🟢 | internal | `crates/authz/src/resource.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/scaffold_test.rs` | Org for P11; no org CRUD yet. |
| Condition library (IpRange/TimeWindow/Mfa) | 🟡 | internal | `crates/authz/src/condition.rs`, `lower.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/engine_test.rs` | IpRange/TimeWindow work; MFA conditions not enforced; TimeWindow UTC-only. |
| Cedar source lowering (lower()) | 🟢 | internal | `crates/authz/src/lower.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/injection_test.rs` | Injection-hardened. |
| Static platform + creator Cedar policies | 🟢 | internal | `policies/platform/`, `policies/creator/`, `crates/authz/src/engine.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/platform_policies_test.rs` | 10 policies; build.rs parses at compile. |
| Entity assembly + LRU cache | 🟢 | internal | `crates/authz/src/entities.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/two_call_test.rs` | 30s TTL; default role 'none' (C1 fix). |
| enforce() — two-call TOKEN⊂USER | 🟢 | internal | `crates/authz/src/eval.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/two_call_test.rs` | Both must allow; 100% audited (no sampling). |
| is_authorized_anywhere() | 🟢 | internal | `crates/authz/src/eval.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/anywhere_uuid_regression_test.rs` | Consent gate + PAT mint. |
| AuthzGuard ntex extractor | 🟢 | internal | `crates/control/src/authz_guard.rs` | `docs/proposals/authorization.md` | `crates/control/src/token_handlers.rs` | Bearer-only (R5); MFA context always false. |
| PAT issuance (POST /me/tokens) | 🟢 | HTTP endpoint | `crates/control/src/token_handlers.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/two_call_test.rs` | No PAT-chains; MFA rejected at mint. |
| PAT listing (GET /me/tokens) | 🟢 | HTTP endpoint | `crates/control/src/token_handlers.rs` | — | — | No pagination/per-token audit view. |
| PAT revocation (DELETE /me/tokens/{id}) | 🟢 | HTTP endpoint | `crates/control/src/token_handlers.rs` | — | — | Owner-only; EntityCache invalidate. |
| OAuth scope vocabulary (Scope enum) | 🟢 | internal | `crates/authz/src/scope.rs` | `docs/proposals/authorization.md` | `crates/authz/src/scope.rs` | 16-scope 1:1 with Action; no PlatformPoliciesWrite scope. |
| OAuth consent UI with authz gate | 🟢 | internal | `crates/auth/src/ui/consent.rs` | `docs/proposals/authorization.md` | — | Identity scopes bypass gate. |
| Platform RBAC role management | 🟢 | HTTP endpoint | `crates/control/src/admin_handlers.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/two_call_test.rs` | admin grants; admin+support read. |
| App audit-lock (set_app_audit_lock) | 🟢 | HTTP endpoint | `crates/control/src/admin_handlers.rs`, `policies/platform/audit_locked.cedar` | `docs/proposals/authorization.md` | `crates/authz/tests/two_call_test.rs` | Cedar forbid blocks writes. |
| App suspension (set_app_suspension) | 🟢 | HTTP endpoint | `crates/control/src/admin_handlers.rs`, `policies/platform/suspended_apps.cedar` | — | `crates/authz/tests/two_call_test.rs` | Reads allowed; writes blocked. |
| Operator platform policy CRUD | 🟡 | HTTP endpoint | `crates/control/src/admin_handlers.rs` | `docs/proposals/authorization.md` | — | Persisted but NOT merged into running engine. |
| policy_hash (SHA-256 canonical) | 🟢 | internal | `crates/authz/src/engine.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/engine_test.rs` | Key-sorted; drift detection. |
| Authorization audit log | 🟢 | internal | `crates/authz/src/eval.rs` | `docs/proposals/authorization.md` | `crates/authz/tests/two_call_test.rs` | Fire-and-forget; 100% audited. |
| Build-time Cedar lint (build.rs) | 🟢 | internal | `crates/authz/build.rs` | `docs/proposals/authorization.md` | — | Panics on invalid Cedar. |
| DEFAULT_PLATFORM_ROLE ('none') | 🟢 | internal | `crates/authz/src/entities.rs` | — | `crates/authz/tests/platform_policies_test.rs` | Zero-privilege default (C1 IDOR fix). |
| PAT EdDSA JWT signing (PatIssuer) | 🟢 | internal | `crates/control/src/token_handlers.rs` | `docs/proposals/authorization.md` | `crates/control/src/token_handlers.rs` | Unix perm check; JWK thumbprint kid. |
| OAuth Hydra introspection in AuthzGuard | 🟢 | internal | `crates/control/src/authz_guard.rs` | `docs/proposals/authorization.md` | — | Audience check; unknown scope → 401. |
| OAuth Device Authorization Grant UI | 🟢 | HTTP endpoint | `crates/auth/src/ui/device.rs` | `docs/proposals/authorization.md` | — | CSRF; emits device_grant_accepted. |
| OAuth client registration (admin) | 🟢 | HTTP endpoint | `crates/control/src/oauth_handlers.rs` | — | — | Scope validation; Hydra+DB transactional. |
| User OAuth grant listing/revocation | 🟢 | HTTP endpoint | `crates/control/src/oauth_grants_handlers.rs` | — | — | Deletes Hydra consent sessions. |
| P10: Toggle-matrix UI for PAT policies | 🔵 | internal | — | `docs/proposals/authorization.md` | — | No dashboard route; no cedar-wasm. |
| P11: Orgs + analyzer + incident lock | 🔵 | internal | — | `docs/proposals/authorization.md` | — | No org CRUD/table/analyzer/lock policy. |
| P12: End-user authz in worker (env.authz) | 🔵 | `env.authz.*` / `@zeroship/permissions` | — | `docs/proposals/authorization.md` | — | No plugin-authz crate; no SDK; no manifest field. |
| P12: Creator policies.cedar in .zship | 🔵 | vite-plugin / manifest | — | `docs/proposals/authorization.md` | — | No authz manifest field; no authz.ts. |
| CLI 'zeroship policy edit' | 🔵 | CLI cmd | — | `docs/proposals/authorization.md` | — | No policy subcommand. |

---

## 12. Billing / Stripe Connect

Stripe Connect direct-charge flows for creator monetisation, plus a raw-infrastructure
usage-metering pipeline. The Stripe-Connect side is substantially built (account linkage,
webhook-ingest ledger, earnings dashboard, all end-to-end). The usage-metering side exists
only as schema + an ingest endpoint: the worker never sends data, `env.meter.*` is absent from
the runtime, and no billing decisions are driven by the counters. Platform fee enforcement is
purely trust-based. **Note:** a *complete* metering→pricing→spending-limit engine exists in the
**dead/excluded `crates/platform`** (tokio/axum, can't run — see the rollup) — so this is
"built then abandoned", not "never built"; reviving it is a from-scratch compio re-implementation.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| Stripe Connect onboarding URL | 🟠 | POST /api/creators/{id}/stripe/onboard | `crates/control/src/stripe_handlers.rs` | `docs/reference/billing-metering.md` | — | Hardcoded placeholder URL; real account_links TODO. |
| Stripe Connect account link (callback) | 🟢 | POST /api/creators/{id}/stripe/callback | `crates/control/src/stripe_handlers.rs` | `docs/reference/billing-metering.md` | `crates/control/tests/stripe_store.rs` | Validates acct_ shape; idempotent. |
| Stripe Connect account unlink | 🟢 | DELETE /api/creators/{id}/stripe | `crates/control/src/stripe_handlers.rs` | `docs/reference/billing-metering.md` | `crates/control/tests/stripe_store.rs` | Soft-delete; FK RESTRICT preserves payouts. |
| Creator account link-history ledger | 🟢 | internal (StripeStore::account_history) | `crates/control/src/stripe_store.rs` | — | `crates/control/tests/stripe_store.rs` | No HTTP endpoint exposes it. |
| Stripe webhook ingest (invoice.paid) | 🟢 | POST /internal/webhooks/stripe | `crates/control/src/stripe_handlers.rs` | `docs/reference/billing-metering.md` | `crates/control/tests/stripe_webhook_test.rs` | Fee read from payload, not re-computed. |
| Webhook signature verification (Rust) | 🟢 | internal | `crates/control/src/stripe_handlers.rs` | `docs/reference/billing-metering.md` | `crates/control/src/stripe_handlers.rs` | Constant-time; cross-validated with TS. |
| Payout ledger (write) | 🟢 | internal (StripeStore::record_payout) | `crates/control/src/stripe_store.rs` | — | `crates/control/tests/stripe_store.rs` | DB CHECK net = gross - fee; cents. |
| Earnings dashboard (read) | 🟢 | GET /api/creators/{id}/earnings | `crates/control/src/stripe_handlers.rs` | `docs/reference/billing-metering.md` | — | Totals + 50 recent; no pagination. |
| @zeroship/payments — checkout / startOnboarding | 🟢 | `createPaymentsClient({ baseUrl, creatorId, auth }).checkout/startOnboarding` | `sdks/payments/src/connect.ts` | `docs/reference/billing-metering.md` | `sdks/payments/tests/connect.test.ts` | Thin client over control (`POST …/connect/checkout`, `…/stripe/onboard`); NO fee param — fee is server-authoritative (ISS-29). |
| @zeroship/payments — verifyWebhook | 🟢 | `verifyWebhook(rawBody, sig, secret, opts)` | `sdks/payments/src/webhook.ts` | `docs/reference/billing-metering.md` | `sdks/payments/tests/webhook.test.ts` | WebCrypto HMAC; runs in V8/browser/Node. |
| App plan management | 🟡 | PUT /api/apps/{id}/plan | `crates/control/src/api.rs` | — | — | plan_id is a label; no billing logic acts on it. |
| Usage counter ingest (worker → control) | 🟡 | POST /internal/usage | `crates/control/src/internal.rs` | `docs/reference/billing-metering.md` | — | No worker ever calls it; ingest-only. |
| Usage counter read (dashboard) | 🟡 | GET /api/apps/{id}/usage | `crates/control/src/api.rs` | `docs/reference/billing-metering.md` | — | Returns empty maps in real deploys. |
| Usage history / snapshots | 🟠 | internal (DB schema only) | `db/changelog/changesets/0004_control.sql` | — | — | Table exists; no code reads/writes it. |
| env.meter.* native primitive | 🔵 | `env.meter.*` (planned) | `crates/control/src/metering.rs` | — | — | Single-line stub; not registered in runtime. |
| Platform fee enforcement | 🟡 | internal (reads application_fee_amount) | `crates/control/src/stripe_handlers.rs` | — | — | 15% set by SDK; not re-verified server-side. |

---

## 13. Bundle / .zship / Blob Store

The bundle crate defines the `.zship` deploy artifact — a zstd-compressed tar containing
`manifest.json` and content-addressed blobs. It provides the Manifest type (per-app dispatch
table), all resource/routing/policy types, the `BlobStore` trait and `LocalDiskBlobStore`,
streaming ingest, size/count limits, and a legacy synchronous `BundleStore`/`LocalFs` VFS.
Gateway, control, and worker all read from a shared `LocalDiskBlobStore`; **no S3/remote backend**.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| .zship archive format (tar.zst) | 🟢 | internal | `crates/bundle/src/unpack.rs` | `docs/reference/zship.md` | `crates/bundle/tests/manifest_test.rs` | Version 1 only; built by vite-plugin. |
| Manifest struct (dispatch table) | 🟢 | internal (wire) | `crates/bundle/src/manifest.rs` | `docs/reference/zship.md` | `crates/bundle/tests/manifest_test.rs` | deploy_hash stamped by control on ingest. |
| Manifest::validate() | 🟢 | internal | `crates/bundle/src/manifest.rs` | `docs/reference/zship.md` | `crates/bundle/tests/manifest_test.rs` | Scope vocab-collision delegated to control. |
| Manifest::passthrough() | 🟢 | internal | `crates/bundle/src/manifest.rs` | — | `crates/core/src/types.rs` | Epoch built_at sentinel. |
| WorkerCode (entry + modules map) | 🟢 | internal (manifest.worker) | `crates/bundle/src/manifest.rs` | `docs/reference/zship.md` | `crates/bundle/tests/manifest_test.rs` | None for SSG-only. |
| ResourceEntry (unified map entry) | 🟢 | internal (manifest.resources) | `crates/bundle/src/rule.rs` | `docs/reference/zship.md` | `crates/bundle/src/rule.rs` | Compiled to EffectivePolicy at load. |
| Resource inheritance chain | 🟢 | internal | `crates/bundle/src/manifest.rs` | — | `crates/bundle/src/manifest.rs` | Depth 16; scopes/middleware union. |
| AuthLevel (anon/user/admin) | 🟢 | internal (ResourceEntry.auth) | `crates/bundle/src/rule.rs` | `docs/reference/zship.md` | `crates/bundle/src/rule.rs` | anon requires publicly_accessible. |
| ProcedureKind (query/mutation/...) | 🟢 | internal (rpc: entries) | `crates/bundle/src/rule.rs` | — | `crates/bundle/src/rule.rs` | None default = Action. |
| Match variants (Exact/Prefix/Glob/Any) | 🟢 | internal (Rule.match) | `crates/bundle/src/rule.rs` | — | `crates/bundle/src/rule.rs` | Segment-aware prefix. |
| Action variants (Static/Worker/Redirect/Rewrite) | 🟢 | internal (Rule.action) | `crates/bundle/src/rule.rs` | — | `crates/bundle/src/rule.rs` | Rewrite hop limit in gateway. |
| CacheCtl (cache policy) | 🟢 | internal | `crates/bundle/src/rule.rs` | — | — | Wire only; enforcement in gateway. |
| RateLimit (per-resource) | 🟢 | internal (ResourceEntry.rate_limit) | `crates/bundle/src/rule.rs` | — | `crates/bundle/src/rule.rs` | 4 scopes; rpm→rps ceil. |
| Cors (per-rule CORS) | 🟢 | internal | `crates/bundle/src/rule.rs` | — | — | Exact origins only (v1). |
| RedirectAction / StaticAction | 🟢 | internal | `crates/bundle/src/rule.rs` | — | — | At most one of redirect/rewrite/static. |
| AuthConfig + ScopeDef (declared scopes) | 🟢 | internal (manifest.auth.scopes) | `crates/bundle/src/manifest.rs` | — | `crates/bundle/tests/manifest_test.rs` | Format-validated; collision check in control. |
| required_scopes on ResourceEntry | 🟢 | internal | `crates/bundle/src/rule.rs` | — | `crates/bundle/src/rule.rs` | Union along chain; OIDC scopes reserved. |
| AssetEntry + AssetVariant | 🟢 | internal (manifest.assets) | `crates/bundle/src/asset.rs` | — | — | br/gzip variants only. |
| runtime_assets + asset_version | 🟡 | internal (manifest.runtime_assets) | `crates/bundle/src/manifest.rs` | — | — | Wire+gateway ship; env.assets.* not registered. |
| sourcemaps map | 🟢 | internal (manifest.sourcemaps) | `crates/bundle/src/manifest.rs` | — | `crates/bundle/src/unpack.rs` | Sourcemaps are first-class blobs. |
| schemas map (JSONSchema refs) | 🟢 | internal (manifest.schemas) | `crates/bundle/src/manifest.rs` | — | — | sha256: ref format validated. |
| aliases map (wire-id stability) | 🟢 | internal (manifest.aliases) | `crates/bundle/src/manifest.rs` | — | — | Gateway ignores; vite-plugin not yet writing it. |
| transformer field | 🟢 | internal (manifest.transformer) | `crates/bundle/src/manifest.rs` | `docs/reference/zship.md` | — | json (default) / superjson. |
| ManifestMetadata (compiler + built_at) | 🟢 | internal (manifest.metadata) | `crates/bundle/src/manifest.rs` | — | — | built_at required by ingest. |
| ManifestExports + HandlerEntry | ⚫ | internal (manifest.exports) | `crates/bundle/src/manifest.rs` | `docs/reference/zship.md` | `crates/bundle/tests/manifest_test.rs` | Deprecated Stage 5c; runtime no longer reads. |
| deploy_hash computation | 🟢 | internal (ingest) | `crates/bundle/src/unpack.rs` | `docs/reference/zship.md` | `crates/bundle/src/unpack.rs` | Canonical key-sorted, deploy_hash omitted. |
| ingest() — streaming ingestion | 🟢 | internal (control deploy) | `crates/bundle/src/unpack.rs` | `docs/architecture/blob-store.md` | `crates/control/tests/deploy_http_test.rs` | O(64 KiB) peak; asserts all hashes present. |
| Ingest limits (size/count caps) | 🟢 | internal (limits.rs) | `crates/bundle/src/limits.rs` | `docs/reference/zship.md` | — | 256MB/256MB/1MB/16MB/10000; zip-bomb defense. |
| BlobStore trait | 🟢 | internal | `crates/bundle/src/blob.rs` | `docs/architecture/blob-store.md` | `crates/bundle/tests/blob_test.rs` | async_trait(?Send); put_blob_stream idempotent. |
| LocalDiskBlobStore | 🟢 | internal (all 3 services) | `crates/bundle/src/blob.rs` | `docs/architecture/blob-store.md` | `crates/bundle/tests/blob_test.rs` | Only backend; disk loss irrecoverable. |
| S3BlobStore / CachedBlobStore | 🔵 | internal | `crates/bundle/src/blob.rs` | `docs/architecture/blob-store.md` | — | Comment-referenced only; no impl. |
| sha256_hex() / validate_hash_format() | 🟢 | internal | `crates/bundle/src/blob.rs` | — | `crates/bundle/tests/blob_test.rs` | pub re-exported. |
| BundleStore trait + LocalFs (legacy VFS) | 🟡 | internal (plugin-storage) | `crates/bundle/src/store.rs` | — | `crates/bundle/tests/store_test.rs` | Predates BlobStore; blocking std::fs; no remote. |
| CLI deploy command | 🟢 | `zeroship deploy <path>.zship --app=<id>` | `crates/cli/src/main.rs` | — | — | curl wrapper; token 3-priority chain. |
| middleware list on ResourceEntry | 🟡 | internal | `crates/bundle/src/rule.rs` | — | — | Wire+compile ship; dispatch target not implemented. |
| idempotent / idempotency_ttl_hours | 🟢 | internal | `crates/bundle/src/rule.rs` | — | `crates/gateway/src/idempotency.rs` | TTL [1,168]; gateway implements dedup. |
| max_input_bytes on ResourceEntry | 🟢 | internal | `crates/bundle/src/rule.rs` | — | — | Gateway enforces. |
| csrf_origins on ResourceEntry | 🟢 | internal | `crates/bundle/src/rule.rs` | — | — | Intersection merge along chain. |

---

## 14. Vite plugin / build pipeline

`@zeroship/vite-plugin` is the single entry point integrating user apps with the platform at
dev and build time. It handles server-procedure discovery via `"use server"` transforms,
synthetic SSR-entry generation, Node.js compat shims (unenv@2), a Vite Environment API bridge
to the real V8 runtime in dev, and `.zship` archive emission. There are no separate CSR/SSR/SSG
plugins — the `mode` option selects the build posture.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| Plugin entry / factory (zeroship()) | 🟢 | `import { zeroship } from '@zeroship/vite-plugin'` | `sdks/vite-plugin/src/index.ts` | `docs/reference/vite-plugin.md` | `examples/csr-todo/vite.config.ts` | Returns 7 composed plugins. |
| 'use server' file-level detection | 🟢 | internal (transform) | `sdks/vite-plugin/src/transform.ts` | `docs/reference/vite-plugin.md` | `examples/csr-todo/src/server.ts` | Acorn + Oxc directive shapes. |
| 'use server' function-level detection | 🟢 | internal (transform/graph) | `sdks/vite-plugin/src/transform.ts` | `docs/reference/vite-plugin.md` | — | Fn/Expr/Arrow; only that fn becomes RPC. |
| RPC wrapper-call recognition | 🟢 | internal (transform) | `sdks/vite-plugin/src/transform.ts` | `docs/reference/vite-plugin.md` | — | `export const x = query(...)`; subscription stubs → UNIMPLEMENTED. |
| Client stub emission | 🟢 | internal (client env) | `sdks/vite-plugin/src/transform.ts` | `docs/reference/vite-plugin.md` | `examples/csr-todo/src/api.ts` | __zsRpc factories; file overwritten. |
| SSR/server-env transform (proc metadata) | 🟢 | internal (server env) | `sdks/vite-plugin/src/transform.ts` | — | — | __zsAttachProcedureMeta + __registerModule. |
| WireId resolution | 🟢 | internal | `sdks/vite-plugin/src/transform.ts` | `docs/reference/vite-plugin.md` | — | Prod rejects bare name; collision fails build. |
| Lazy procedure loading | 🟢 | internal | `sdks/vite-plugin/src/transform.ts` | `docs/reference/vite-plugin.md` | — | Literal boolean; warns+eager fallback. |
| Synthetic server entry | 🟢 | `virtual:zeroship/_server-entry` | `sdks/vite-plugin/src/rpc-registry.ts` | `docs/reference/vite-plugin.md` | — | namespace-walk always active in prod. |
| Phase-2 static-binding entry | 🟠 | internal (getBindings param) | `sdks/vite-plugin/src/rpc-registry.ts` | — | — | Implemented/tested; never passed from build.ts. |
| Server-graph reference walk | 🟡 | internal | `sdks/vite-plugin/src/server-graph.ts` | — | `sdks/vite-plugin/test/server-graph.test.ts` | Tested in isolation; not wired into active build. |
| Strict-mode gate for server bindings | 🟡 | internal | `sdks/vite-plugin/src/server-graph.ts` | — | `sdks/vite-plugin/test/server-graph-strict.test.ts` | rpc.strict accepted but no effect (graph not called). |
| Manifest extras computation | 🟢 | internal (closeBundle) | `sdks/vite-plugin/src/manifest.ts` | `docs/reference/vite-plugin.md` | `sdks/vite-plugin/test/manifest-resources.test.ts` | snake_case rename; secure-by-default. |
| defineApp resources from config.ts | 🟢 | internal | `sdks/vite-plugin/src/manifest.ts` | — | `sdks/vite-plugin/test/manifest-resources.test.ts` | new Function() eval; computed exprs error. |
| Production build pipeline (client + SSR) | 🟢 | internal (buildPlugin) | `sdks/vite-plugin/src/build.ts` | `docs/reference/vite-plugin.md` | `examples/csr-todo/vite.config.ts` | SSR target webworker; strips 'use server'. |
| Static / SSG build mode | 🟢 | `zeroship({ mode: 'static' })` | `sdks/vite-plugin/src/build.ts` | `docs/reference/vite-plugin.md` | `examples/ssg-docs/vite.config.ts` | Stub input injected then deleted. |
| .zship archive emitter | 🟢 | internal (emitZship) | `sdks/vite-plugin/src/zship.ts` | `docs/reference/zship.md` | `sdks/vite-plugin/test/zship.test.ts` | brotli default; validates hash refs. |
| SSR vs SPA catch-all detection | 🟢 | internal (probeUserDefaultExport) | `sdks/vite-plugin/src/build.ts` | — | — | Conservative default true (SSR). |
| virtual:zeroship/client-manifest | 🟢 | `import manifest from 'virtual:zeroship/client-manifest'` | `sdks/vite-plugin/src/build.ts` | — | `examples/ssr-blog/vite.config.ts` | Reads dist/.vite/manifest.json. |
| Node.js compat shims | 🟢 | internal (SSR/zeroship env) | `sdks/vite-plugin/src/node-compat.ts` | `docs/reference/node-compat.md` | — | Native modules external; custom polyfills. |
| Virtual zeroship module resolution | 🟢 | `import { env } from 'zeroship'` | `sdks/vite-plugin/src/zeroship-module.ts` | — | — | Mirrors ZEROSHIP_MODULE_JS. |
| @zeroship/bootstrap resolver | 🟢 | internal (resolveId) | `sdks/vite-plugin/src/zeroship-module.ts` | — | — | Resolves to framework-installed copy. |
| Vite Environment API integration | 🟢 | internal (environments.zeroship) | `sdks/vite-plugin/src/environment.ts` | `docs/reference/vite-environment-api.md` | — | Intercepts node:* fetchModule. |
| Dev server bridge (spawn + proxy) | 🟢 | internal (configureServer) | `sdks/vite-plugin/src/dev-server.ts` | `docs/reference/vite-environment-api.md` | — | Crash-restart; ZEROSHIP_BIN override. |
| HTTP module-fetch endpoint | 🟢 | POST /__zeroship_fetch | `sdks/vite-plugin/src/dev-server.ts` | `docs/reference/vite-environment-api.md` | — | fetchModule/getBuiltins only; 64 KB cap. |
| Poll-based HMR | 🟢 | GET /__zeroship_hmr_check | `sdks/vite-plugin/src/dev-server.ts` | `docs/reference/vite-environment-api.md` | — | V8 polls every 500ms. |
| Dev-bootstrap ModuleRunner | 🟢 | internal (V8 entry) | `sdks/vite-plugin/src/dev-bootstrap/index.ts` | `docs/reference/vite-environment-api.md` | — | eval-based evaluator; preserves TypeBuilder identity. |
| Dev RPC registry | 🟢 | internal (__registerModule/__lookup) | `sdks/vite-plugin/src/dev-bootstrap/rpc-registry.ts` | — | `sdks/vite-plugin/test/dev-bootstrap-rpc-registry.test.ts` | Module-scoped ownership; HMR prune. |
| Dev-tier auth provider config | 🟢 | `zeroship({ devAuth: ... })` | `sdks/vite-plugin/src/dev-auth-config.ts` | `docs/reference/auth-dev-tier.md` | — | Fresh secret per start; dev-only by construction. |
| Dev SQLite database fallback | 🟢 | internal | `sdks/vite-plugin/src/dev-db.ts` | `docs/reference/vite-environment-api.md` | — | shell > .env > sqlite:.zeroship/dev.sqlite. |
| Server-entry auto-detection | 🟢 | internal (findServerEntry) | `sdks/vite-plugin/src/build.ts` | — | — | Fixed candidate list; serverEntry override. |
| client-manifest TypeScript type shim | 🟢 | `@zeroship/vite-plugin/types` | `sdks/vite-plugin/src/client-manifest.d.ts` | — | `examples/ssr-blog/vite.config.ts` | Mirrors Vite Manifest shape. |
| CSR build support | 🟢 | `zeroship()` (default full) | `sdks/vite-plugin/src/build.ts` | `docs/reference/vite-plugin.md` | `examples/csr-todo/vite.config.ts` | SPA fallback catch-all when no default.fetch. |
| SSR build support | 🟢 | `zeroship()` + build.manifest:true | `sdks/vite-plugin/src/build.ts` | — | `examples/ssr-blog/vite.config.ts` | User must set manifest:true. |
| SSG build support | 🟢 | `zeroship({ mode: 'static' })` | `sdks/vite-plugin/src/build.ts` | — | `examples/ssg-docs/vite.config.ts` | HTML pre-render is user's responsibility. |
| rpcEndpoint option | ⚫ | `zeroship({ rpcEndpoint })` | `sdks/vite-plugin/src/transform.ts` | `docs/reference/vite-plugin.md` | — | _rpcEndpoint unused; stubs use fixed path. |
| Stale dist-only files | ⚫ | none | `sdks/vite-plugin/dist/resolve-schema.js` | — | — | Orphaned dist; source deleted, dist not cleaned. |

---

## 15. Sandbox / AI build env

Isolated execution environments for AI-generated creator apps. Each sandbox is a microVM (or
container, dev) running the zeroship-sandbox-agent as PID 1; the controller orchestrates
lifecycle over an HTTP API with Ed25519-signed requests. Three backends: Docker (dev),
Kubernetes + libkrun (fleet), Nomad + Cloud Hypervisor (production). The codebase is
production-shaped for nomad-ch with snapshot/restore, preview proxy, share tokens, HA pg-backed
state, and an operator admin API; the cold-boot orchestrator is the only remaining stub.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| Docker backend | 🟢 | internal (SANDBOX_BACKEND=docker) | `crates/sandbox/src/backend/docker.rs` | `docs/architecture/builder.md` | `crates/sandbox/examples/lifecycle_e2e.rs` | restore_from_sealed Err; no restart-restore. |
| Kubernetes + libkrun backend | 🟢 | internal (SANDBOX_BACKEND=k8s) | `crates/sandbox/src/backend/k8s.rs` | `docs/runbooks/sandbox-agent.md` | `crates/sandbox-agent/examples/e2e_k3s.rs` | Shells out to kubectl; restore Err. |
| Nomad + Cloud Hypervisor backend | 🟢 | internal (SANDBOX_BACKEND=nomad-ch) | `crates/sandbox/src/backend/nomad_ch.rs` | `docs/runbooks/sandbox-nomad-ch.md` | `crates/sandbox/examples/lifecycle_e2e.rs` | Production; vm_index free-list; node-pin. |
| Sandbox create | 🟢 | POST /sandboxes | `crates/sandbox/src/handlers.rs` | `docs/architecture/builder.md` | `crates/sandbox/tests/sandbox_pg_e2e.rs` | Re-attach if alive; retry on stale-agent. |
| Sandbox list | 🟢 | GET /sandboxes?user_id= | `crates/sandbox/src/handlers.rs` | — | — | Requires user_id (no cross-tenant). |
| Sandbox get | 🟢 | GET /sandboxes/{id}?user_id= | `crates/sandbox/src/handlers.rs` | — | — | 404 for missing or wrong-owner. |
| Sandbox stop/delete | 🟢 | DELETE /sandboxes/{id}?user_id= | `crates/sandbox/src/handlers.rs` | — | `crates/sandbox/tests/sandbox_pg_e2e.rs` | CAS-fenced against HA takeover. |
| In-sandbox command exec | 🟢 | POST /sandboxes/{id}/exec | `crates/sandbox/src/handlers.rs` | — | `crates/sandbox/tests/sandbox_preview_e2e.rs` | 600s cap; env-cleared child. |
| Workspace file tree | 🟢 | GET /sandboxes/{id}/file-tree | `crates/sandbox/src/handlers.rs` | — | — | truncated flag at cap. |
| Workspace file CRUD | 🟢 | GET/PUT/DELETE /sandboxes/{id}/files/{path} | `crates/sandbox/src/handlers.rs` | — | `crates/sandbox/tests/sandbox_preview_e2e.rs` | openat2 RESOLVE_BENEATH/NO_SYMLINKS. |
| Preview HTTP proxy | 🟢 | ANY /sandboxes/{id}/preview/{port}/{path} | `crates/sandbox/src/preview.rs` | `docs/architecture/builder.md` | `crates/sandbox/tests/sandbox_preview_e2e.rs` | Bearer or share-token; port allow-list; 100 MiB. |
| Preview WebSocket proxy (controller) | 🟢 | TCP SANDBOX_PREVIEW_WS_PORT | `crates/sandbox/src/preview_ws.rs` | — | `crates/sandbox/tests/sandbox_preview_ws_e2e.rs` | Dedicated port; TCP splice. |
| Preview share token mint/list/revoke | 🟢 | POST/GET/DELETE /sandboxes/{id}/preview/{port}/share | `crates/sandbox/src/preview_share_handlers.rs` | — | `crates/sandbox/tests/sandbox_preview_share_e2e.rs` | HMAC; 100/day; per-token revoke 501. |
| Share token cookie conversion (?t=) | 🟢 | GET .../preview/{port}/{path}?t= | `crates/sandbox/src/preview.rs` | — | `crates/sandbox/tests/sandbox_preview_share_e2e.rs` | __Host- cookie; Sec-Fetch gate. |
| sandbox-agent: PID-1 HTTP server | 🟢 | internal (HTTP :7777 in VM) | `crates/sandbox-agent/src/main.rs` | `docs/runbooks/sandbox-agent.md` | `crates/sandbox-agent/examples/e2e_k3s.rs` | Protocol v1; per-route body caps. |
| sandbox-agent: Ed25519 verification | 🟢 | internal (X-Sbx-* headers) | `crates/sandbox-agent/src/auth.rs` | — | — | 5s skew; 30s nonce LRU; canonical v1/v1.1. |
| sandbox-agent: command exec (/exec) | 🟢 | internal (POST /exec) | `crates/sandbox-agent/src/exec.rs` | — | `crates/sandbox-agent/examples/e2e_k3s.rs` | nobody:nogroup; SIGKILL group on timeout. |
| sandbox-agent: proxy HTTP (/proxy) | 🟢 | internal (ANY /proxy/{port}/{path}) | `crates/sandbox-agent/src/proxy.rs` | — | `crates/sandbox/tests/sandbox_preview_e2e.rs` | Port allow-list; Set-Cookie Domain strip. |
| sandbox-agent: proxy WebSocket (:7778) | 🟢 | internal (TCP :7778) | `crates/sandbox-agent/src/proxy_ws.rs` | — | `crates/sandbox/tests/sandbox_preview_ws_e2e.rs` | TCP splice; proxy.ws-v1. |
| sandbox-agent: clock resync | 🟢 | internal (POST /_clock_resync) | `crates/sandbox-agent/src/handlers.rs` | — | — | Post-restore; sandbox_id binding. |
| sandbox-agent: PID-1 zombie reaper | 🟢 | internal | `crates/sandbox-agent/src/reap.rs` | — | — | Idempotent. |
| sandbox-agent: security audit log | 🟢 | internal (tracing target 'audit') | `crates/sandbox-agent/src/audit.rs` | — | — | Stable event ids. |
| Sealed-record persistence | 🟢 | internal (SANDBOX_PERSIST_AUTH=1) | `crates/sandbox/src/persist.rs` | — | `crates/sandbox/tests/sandbox_persist_e2e.rs` | XChaCha20-Poly1305; nomad-ch full rehydrate. |
| Snapshot | 🟢 | POST /admin/sandboxes/{id}/snapshot | `crates/sandbox/src/snapshot_handler.rs` | — | `crates/sandbox/tests/sandbox_pg_e2e.rs` | CAS state machine; nomad-ch only. |
| Snapshot AEAD encryption | 🟢 | internal (AeadSnapshotStore) | `crates/sandbox/src/snapshot_aead.rs` | — | — | ChaCha20-Poly1305; HKDF per-snapshot DEK. |
| Snapshot store: local disk (L1) | 🟢 | internal | `crates/sandbox/src/snapshot_store.rs` | — | — | Canonical SHA-256 over 3 files. |
| Snapshot store: GCS L2 tiered | 🟢 | internal (SANDBOX_SNAPSHOT_USE_GCS) | `crates/sandbox/src/snapshot_store_gcs.rs` | — | — | Fire-and-forget L2; no retry yet. |
| Wake / restore | 🟢 | POST /admin/sandboxes/{id}/wake | `crates/sandbox/src/restore_handler.rs` | — | `crates/sandbox/tests/sandbox_pg_e2e.rs` | CAS; sync/async modes; checksum verify. |
| Async wake polling | 🟢 | GET /admin/sandboxes/{id}/wake/{wake_id} | `crates/sandbox/src/admin_handlers.rs` | — | — | wake_jobs table; GC sweep. |
| Cold boot | 🟠 | POST /admin/sandboxes/{id}/cold-boot | `crates/sandbox/src/admin_handlers.rs` | — | — | Always 501 feature_disabled. |
| Idle eviction sweep | 🟢 | internal | `crates/sandbox/src/sweep.rs` | — | — | 30-min default; bounded concurrency. |
| Transient-state takeover sweep | 🟢 | internal | `crates/sandbox/src/sweep.rs` | — | — | 30s scan; defense-in-depth. |
| wake_jobs GC sweep | 🟢 | internal | `crates/sandbox/src/sweep.rs` | — | — | 60s; 300s retention. |
| sandbox_events partition provisioner | 🟢 | internal | `crates/sandbox/src/sweep.rs` | — | — | Monthly partitions; DDL role. |
| HA heartbeat and lease takeover | 🟢 | internal | `crates/sandbox/src/db.rs` | — | `crates/sandbox/tests/sandbox_pg_e2e.rs` | 5s heartbeat; CAS generation guard. |
| Startup restore from pg + sealed | 🟢 | internal | `crates/sandbox/src/restore.rs` | — | `crates/sandbox/tests/sandbox_persist_e2e.rs` | nomad-ch full; fingerprint probe. |
| Admin: list all sandboxes | 🟢 | GET /admin/sandboxes | `crates/sandbox/src/admin_handlers.rs` | — | `crates/sandbox/tests/sandbox_admin_e2e.rs` | Cross-tenant; pagination. |
| Admin: sandbox detail | 🟢 | GET /admin/sandboxes/{id} | `crates/sandbox/src/admin_handlers.rs` | — | `crates/sandbox/tests/sandbox_admin_e2e.rs` | pg row + agent /version. |
| Admin: per-user sandbox list | 🟢 | GET /admin/users/{user_id}/sandboxes | `crates/sandbox/src/admin_handlers.rs` | — | — | Includes historical rows. |
| Admin: per-user share list | 🟢 | GET /admin/users/{user_id}/shares | `crates/sandbox/src/admin_handlers.rs` | — | — | Share audit metadata. |
| Admin: host fleet status | 🟢 | GET /admin/hosts | `crates/sandbox/src/admin_handlers.rs` | — | — | host_id/status/heartbeat_lag. |
| Admin: GDPR data export | 🟢 | GET /admin/users/{user_id}/export | `crates/sandbox/src/admin_handlers.rs` | — | — | Full bearer; REPEATABLE READ. |
| Admin: GDPR cascade delete | 🟢 | DELETE /admin/users/{user_id} | `crates/sandbox/src/admin_handlers.rs` | — | — | Full bearer; gdpr pool. |
| Admin: read-only (RO) bearer role | 🟢 | internal (AdminRole) | `crates/sandbox/src/admin_handlers.rs` | — | `crates/sandbox/tests/sandbox_admin_e2e.rs` | HTTP gate; DB role deferred. |
| Prometheus metrics endpoint | 🟢 | GET /metrics | `crates/sandbox/src/admin_handlers.rs` | — | — | Process-global atomics. |
| Sandbox in-memory registry | 🟢 | internal | `crates/sandbox/src/registry.rs` | — | — | (user,project)→sbx; 60s secret grace. |
| Pg-backed sandbox state | 🟢 | internal (SANDBOX_DATABASE_URL) | `crates/sandbox/src/db.rs` | — | `crates/sandbox/tests/sandbox_pg_e2e.rs` | 3 pg roles; per-thread Pool cache. |
| detach_isolated task isolation | 🟢 | internal | `crates/sandbox/src/detach.rs` | — | — | Dedicated OS thread + compio runtime. |
| GCP cluster provisioning scripts | 🟢 | bash scripts | `crates/sandbox/scripts/provision-gcp-cluster.sh` | `docs/runbooks/sandbox-nomad-ch.md` | — | provision/bootstrap/teardown. |
| Nomad ch Go task driver | 🟢 | internal (driver 'ch') | `nomad-driver-ch/` | `docs/runbooks/sandbox-nomad-ch.md` | — | RecoverTask + TaskStats; Exec=false. |
| sandbox-base image / bake-rootfs | 🟢 | bash/Dockerfile | `crates/sandbox/docker/Dockerfile.sandbox-base` | — | — | seccomp-io-uring profile. |
| typed_id sbx/usr/prj validation | 🟢 | internal | `crates/sandbox/src/handlers.rs` | — | `crates/sandbox/tests/sandbox_typed_id_e2e.rs` | pg CHECK; path-traversal boundary. |
| Lifecycle e2e example | 🟢 | cargo --example lifecycle_e2e | `crates/sandbox/examples/lifecycle_e2e.rs` | — | — | — |
| Stress e2e example | 🟢 | cargo --example stress_e2e | `crates/sandbox/examples/stress_e2e.rs` | — | — | — |
| Per-token share revoke | 🟠 | DELETE .../share/{token_id} | `crates/sandbox/src/preview_share_handlers.rs` | — | — | 501; bulk revoke works. |
| sandbox_admin_ro pg role | 🔵 | internal | `crates/sandbox/src/db.rs` | — | — | HTTP gate ships; DB role deferred. |
| Per-operator JWT admin auth | 🔵 | internal | `crates/sandbox/src/admin_handlers.rs` | — | — | Replaces shared-bearer; deferred. |
| GCS L2 upload retry / metric | 🔵 | internal | `crates/sandbox/src/snapshot_store_gcs.rs` | — | — | Fire-and-forget warn only. |
| LRU eviction + refcount pinning (L1) | 🔵 | internal | `crates/sandbox/src/snapshot_store.rs` | — | — | Planned. |
| Docker/K8s restart-restore | 🟡 | internal | `crates/sandbox/src/backend/mod.rs` | — | — | nomad-ch only; Docker/K8s return Err. |

---

## 16. Worker

The V8 execution tier of the app runtime. It receives HTTP dispatch envelopes from the gateway,
runs creator app code in per-thread V8 isolates managed by an LRU cache, and continuously
reconciles local isolate state with the control plane's version and env feeds. Each ntex thread
owns its own isolate cache; a single process-wide poller fetches the version map and env
snapshots are shared across threads via a process-wide RwLock.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| HTTP dispatch endpoint | 🟢 | POST /dispatch/{app_id} | `crates/worker/src/handler.rs` | `docs/architecture/distributed.md` | `tests/e2e_platform.sh` | 4 MiB body cap; Bearer worker_key. |
| Streaming (SSE/ReadableStream) forwarding | 🟢 | internal | `crates/worker/src/handler.rs` | — | — | Waker-based mpsc; no busy-poll. |
| WebSocket upgrade via dispatch endpoint | ⚫ | HTTP endpoint (unreachable) | `crates/worker/src/handler.rs` | — | — | Returns 500; gateway uses separate WS path. |
| Per-thread V8 isolate LRU cache | 🟢 | internal | `crates/worker/src/cache.rs` | `AGENTS.md` | `tests/e2e_platform.sh` | Thread-local; default 200; abort fan-out on evict. |
| On-demand app loading (cold start) | 🟢 | internal (cache miss) | `crates/worker/src/handler.rs` | `docs/architecture/distributed.md` | `tests/e2e_platform.sh` | env committed before isolate. |
| Process-wide version poller | 🟢 | internal | `crates/worker/src/sync.rs` | `docs/architecture/distributed.md` | — | Single task; GCs SharedEnvs. |
| Per-thread reconcile loop | 🟢 | internal | `crates/worker/src/sync.rs` | `docs/architecture/distributed.md` | `tests/e2e_platform.sh` | env refresh + isolate swap; startup jitter. |
| Env-only isolate rotation (SEC-7) | 🟢 | internal | `crates/worker/src/sync.rs` | — | `crates/worker/src/sync.rs` | env_version bump → isolate swap. |
| Process-wide env snapshot cache | 🟢 | internal | `crates/worker/src/sync.rs` | — | — | Fail-closed 503 (ENV_UNAVAILABLE). |
| App kernel namespace wiring | 🟢 | env.db/kv/storage/auth | `crates/worker/src/cache.rs` | `AGENTS.md` | `crates/worker/src/handler.rs` | KV=Redis multi-node; namespaces degrade independently. |
| Worker gateway authentication | 🟢 | HTTP (gateway-facing) | `crates/worker/src/handler.rs` | — | — | Bearer + ZeroShip-User HMAC; ≥32B key. |
| ZeroShip-User header forwarding | 🟢 | internal (env.auth.*) | `crates/worker/src/handler.rs` | `docs/reference/auth.md` | — | Requires x-request-id; missing → no-user. |
| Wall-clock timeout with cancellation | 🟢 | internal | `crates/worker/src/handler.rs` | `docs/reference/runtime-limits.md` | — | Default 30s; CancelFlag + pump notify. |
| Per-app runtime limits enforcement | 🟢 | internal | `crates/worker/src/cache.rs` | `docs/reference/runtime-limits.md` | — | Limits change triggers reload. |
| In-flight request abort on LRU eviction | 🟢 | internal | `crates/worker/src/cache.rs` | — | — | Synchronous abort before drop. |
| App console log capture + /logs endpoint | 🟢 | GET /logs/{app_id} | `crates/worker/src/logs.rs` | — | `crates/worker/src/handler.rs` | Ring buffer 1000 lines; no persistence. |
| Prometheus metrics endpoint | 🟢 | GET /metrics | `crates/worker/src/metrics.rs` | — | — | 13 counters; no auth. |
| Health check endpoint | 🟢 | GET /health | `crates/worker/src/main.rs` | — | `tests/e2e_platform.sh` | Liveness only. |
| Config validation dry-run | 🟢 | --check-config [--format] | `crates/worker/src/main.rs` | — | `tests/config_check_e2e.sh` | Non-secret summary. |
| Secret reference resolution | 🟢 | CONTROL_KEY / WORKER_KEY / ... | `crates/worker/src/main.rs` | — | `tests/config_check_e2e.sh` | urn:zeroship:env/file; [secrets] overlay. |
| Unix domain socket listener | 🟢 | --socket / WORKER_SOCKET | `crates/worker/src/main.rs` | — | — | Stale socket removed at startup. |
| Graceful shutdown with drain timeout | 🟢 | --shutdown-timeout | `crates/worker/src/main.rs` | — | — | 0 = wait forever. |
| mimalloc global allocator | 🟢 | internal | `crates/worker/src/main.rs` | — | — | #[global_allocator]. |
| Deleted-app cleanup | 🟢 | internal | `crates/worker/src/sync.rs` | — | — | reconcile evict + version-poller env GC. |
| LoadedMeta tracking | 🟢 | internal | `crates/worker/src/cache.rs` | — | — | deploy hash + env version per isolate. |
| Per-thread cyper HTTP client | 🟢 | internal | `crates/worker/src/sync.rs` | — | — | SendWrapper; 5s control timeout. |
| SSG-only deploy handling | 🟢 | internal | `crates/worker/src/sync.rs` | — | — | Evicts/skips missing worker hash. |

---

## 17. Drivers + core infra

Three crates form the shared infrastructure layer. `compio-postgres` is a full async PostgreSQL
client (ported from tokio-postgres) on compio/io_uring — zero tokio. `compio-redis` is a
Redis/Dragonfly client with single-node and cluster modes. `zeroship-core` is the shared type
library every crate imports for wire types, cryptography, auth utilities, config loading,
observability, and OIDC/OAuth protocol primitives.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| PG client — connect + Client/Connection split | 🟢 | internal | `crates/compio-postgres/src/connect.rs`, `client.rs`, `connection.rs` | — | `crates/compio-postgres/tests/integration.rs` | tokio-postgres port; NoTls only. |
| PG client — query/execute/query_* variants | 🟢 | internal | `crates/compio-postgres/src/client.rs`, `query.rs` | — | `crates/compio-postgres/tests/integration.rs` | query_text_params for JSON builders. |
| PG client — prepared statements | 🟢 | internal | `crates/compio-postgres/src/prepare.rs`, `statement.rs` | — | `crates/compio-postgres/tests/integration.rs` | CachedTypeInfo per connection. |
| PG client — transactions + savepoints | 🟢 | internal | `crates/compio-postgres/src/transaction.rs`, `transaction_builder.rs` | — | `crates/compio-postgres/tests/integration.rs` | Drop fires ROLLBACK; dirty-flag barrier. |
| PG client — simple_query / batch_execute | 🟢 | internal | `crates/compio-postgres/src/simple_query.rs` | — | `crates/compio-postgres/tests/integration.rs` | Text protocol; pool dirty barrier. |
| PG client — COPY IN / COPY OUT | 🟢 | internal | `crates/compio-postgres/src/copy_in.rs`, `copy_out.rs`, `binary_copy.rs` | — | `crates/compio-postgres/tests/integration.rs` | Binary copy helper port. |
| PG client — portals (bind-execute) | 🟢 | internal | `crates/compio-postgres/src/portal.rs`, `bind.rs` | — | `crates/compio-postgres/tests/integration.rs` | Cursor-like partial fetch. |
| PG client — async LISTEN/NOTIFY | 🟢 | internal | `crates/compio-postgres/src/connection.rs`, `lib.rs` | — | `crates/compio-postgres/tests/integration.rs` | AsyncMessage Notification/Notice. |
| PG client — cancel token | 🟢 | internal | `crates/compio-postgres/src/cancel_token.rs`, `cancel_query.rs` | — | — | Deprecated wrappers delegate. |
| PG client — TLS negotiation | 🟡 | internal | `crates/compio-postgres/src/connect_tls.rs`, `tls.rs`, `config.rs` | — | — | Full code exists; only NoTls exported. |
| PG client — pipelining | 🟢 | internal | `crates/compio-postgres/src/connection.rs`, `client.rs` | — | — | Unbounded channel FIFO. |
| PG client — logical replication (pgoutput) | 🟢 | internal | `crates/compio-postgres/src/replication.rs` | — | — | Own CopyBoth framer; single-host. |
| PG connection pool | 🟢 | internal | `crates/compio-postgres/src/pool.rs` | — | `crates/compio-postgres/tests/integration.rs` | !Send; FIFO-fair; dirty barrier on checkout. |
| PG test-utils feature | 🟢 | internal (feature=test-utils) | `crates/compio-postgres/src/test_utils.rs` | — | — | Excluded from prod builds. |
| PG config — connection string parser | 🟢 | internal | `crates/compio-postgres/src/config.rs` | — | — | DSN parser; Unix socket support. |
| Redis single-node client | 🟢 | internal (plugin-kv) | `crates/compio-redis/src/client.rs` | — | `crates/compio-redis/tests/integration.rs` | Literal IP only; no TLS; one cmd in flight. |
| Redis reply size cap (64 MB) | 🟢 | internal | `crates/compio-redis/src/client.rs` | — | `crates/compio-redis/src/client.rs` | OOM guard; array-bomb rejected. |
| Redis dirty-barrier for pool reuse | 🟢 | internal | `crates/compio-redis/src/client.rs`, `pool.rs` | — | `crates/compio-redis/src/pool.rs` | Cross-tenant desync prevention. |
| Redis cluster client | 🟢 | internal (plugin-kv) | `crates/compio-redis/src/cluster.rs` | — | `crates/compio-redis/tests/cluster.rs` | CLUSTER SLOTS; MOVED/ASK; SSRF allowlist. |
| Redis connection pool | 🟢 | internal | `crates/compio-redis/src/pool.rs` | — | `crates/compio-redis/src/pool.rs` | LIFO; test-on-borrow; no wait queue. |
| Redis — no TLS | 🔵 | internal | `crates/compio-redis/src/lib.rs` | — | — | rediss:// not implemented; MITM possible. |
| typed_id — UUIDv7 base62 IDs | 🟢 | internal | `crates/core/src/typed_id.rs` | — | `crates/core/src/typed_id.rs` | usr/app/ses/wak/oac; parse_with_prefix. |
| Wire types — AppRecord/RouteEntry/... | 🟢 | internal | `crates/core/src/types.rs` | `docs/architecture/gateway-routing.md` | — | env_version monotonic counter. |
| Wire types — UsageReport/AppUsage/ControlEvent | 🟢 | internal | `crates/core/src/types.rs` | — | — | CommonError enum. |
| AppRuntimeLimits | 🟢 | internal | `crates/core/src/types.rs` | `docs/reference/runtime-limits.md` | — | Defaults None. |
| Crypto — AES-256-GCM secrets at rest | 🟢 | internal (plugin-db EnvStore) | `crates/core/src/crypto.rs` | — | `crates/core/src/crypto.rs` | Versioned AAD; per-app HKDF not yet done. |
| Auth utils — constant-time key compare | 🟢 | internal | `crates/core/src/auth/mod.rs` | — | `crates/core/src/auth/mod.rs` | Constant iteration count. |
| Auth utils — HMAC-SHA256 sign/verify | 🟢 | internal | `crates/core/src/auth/mod.rs` | — | `crates/core/src/auth/mod.rs` | RFC 4231 KAT. |
| Auth utils — ZeroShip-User header sign/verify | 🟢 | internal | `crates/core/src/auth/mod.rs` | `docs/reference/auth.md` | `crates/core/src/auth/mod.rs` | 60s age, 5s skew, per-request bind. |
| Auth utils — API key hash / validate | 🟢 | internal | `crates/core/src/auth/mod.rs` | — | `crates/core/src/auth/mod.rs` | SHA-256; constant-time. |
| Auth utils — pairwise subject derivation | 🟢 | internal | `crates/core/src/auth/mod.rs` | `docs/reference/auth.md` | `crates/core/src/auth/mod.rs` | pws_; dedicated salt; UUID normalize. |
| Auth utils — is_pairwise_subject check | 🟢 | internal | `crates/core/src/auth/mod.rs` | — | `crates/core/src/auth/mod.rs` | Shape check only, not a forgery gate. |
| Auth utils — trusted OAuth client resolution | 🟢 | internal | `crates/core/src/auth/trusted_clients.rs` | — | — | Empty default (fail-closed). |
| Config — FileConfig TOML overlay | 🟢 | internal (bootstrap_or_exit) | `crates/core/src/config/file.rs`, `source.rs`, `bootstrap.rs` | — | — | deny_unknown_fields; XDG discovery. |
| Config — secret reference system | 🟡 | internal | `crates/core/src/config/secrets.rs` | — | `crates/core/src/config/secrets.rs` | env/file resolve; vault/awssm parse-but-unresolvable. |
| Config — secret strength validation | 🟢 | internal | `crates/core/src/config/secrets.rs` | — | `crates/core/src/config/secrets.rs` | ≥32B; dev sentinels; --dev-insecure bypass. |
| Config — loopback URL check | 🟢 | internal | `crates/core/src/config/secrets.rs` | — | `crates/core/src/config/secrets.rs` | Literal-only; no DNS. |
| Config — bootstrap_or_exit | 🟢 | internal | `crates/core/src/config/bootstrap.rs` | — | — | CheckConfigReport for --check-config. |
| Observability — tracing subscriber init | 🟢 | ZEROSHIP_LOG_FORMAT / --log-format | `crates/core/src/observability.rs` | — | `crates/core/src/observability.rs` | 5 formats; LogTracer bridge; idempotent. |
| OIDC — JWKS cache + ID token verifier | 🟢 | internal (gateway/control RP) | `crates/core/src/oidc_verify.rs` | `docs/reference/auth.md` | `crates/core/tests/oidc_verify_test.rs` | 5-min TTL; stale-on-error; RS/ES algos. |
| OIDC — BCL logout_token verifier | 🟢 | internal (auth BCL) | `crates/core/src/logout_token.rs` | — | — | events claim; nonce-absent. |
| DPoP — RFC 9449 proof verifier | 🟢 | internal (auth Bearer+DPoP) | `crates/core/src/dpop.rs` | — | — | RFC 7638 thumbprint; HMAC/none blocked. |
| PKCE — RFC 7636 verifier + S256 | 🟢 | internal | `crates/core/src/pkce.rs` | — | — | 43-char verifier. |
| Hydra introspector + LRU cache | 🟢 | internal (control authz) | `crates/core/src/hydra.rs` | — | `crates/core/tests/hydra_introspect.rs` | SHA-256(token) cache key; 5-min TTL. |
| Wrapper revocation — per-app family marker | 🟢 | internal (gateway/auth signout) | `crates/core/src/wrapper_revocation.rs` | `docs/reference/auth.md` | — | Per-app scope; 24h retention. |
| SuperJSON — wire-compatible encode/decode | 🟢 | internal (RPC) | `crates/core/src/superjson.rs` | `docs/reference/rpc.md` | `crates/core/tests/superjson_test.rs` | npm superjson@2 wire; 17 fixtures. |
| Preview port allowlist / denylist | 🟢 | internal (sandbox) | `crates/core/src/preview_ports.rs` | — | `crates/core/src/preview_ports.rs` | HARDCODED_DENY + DEFAULT_DENY; shared. |

---

## 18. CLI + developer experience

The `zeroship` binary (serve/deploy/login/logout/whoami/secret/var), the `create-zeroship-app`
scaffolder, and the `@zeroship/vite-plugin` dev/build loop. Dev tier is zero-config: SQLite
replaces Postgres, redb replaces Redis, LocalFs replaces S3, and an in-process dev-auth provider
replaces the Hydra/gateway stack. Production builds produce a `.zship` archive consumed by
`zeroship deploy`.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| zeroship serve | 🟢 | `zeroship serve <file> [--port] [...]` | `crates/cli/src/main.rs` | `docs/runbooks/local-dev.md` | `examples/http-handler.js` | .js path only; registers db/storage/auth/kv. |
| zeroship deploy | 🟢 | `zeroship deploy <path.zship> --app=<id>` | `crates/cli/src/main.rs` | `docs/runbooks/local-dev.md` | — | curl POST; prints deploy_hash. |
| zeroship login (Device Grant) | 🟢 | `zeroship login [--auth-url]` | `crates/cli/src/auth.rs` | — | `crates/cli/tests/login_test.rs` | RFC 8628; token.json mode 0600. |
| zeroship logout | 🟢 | `zeroship logout` | `crates/cli/src/auth.rs` | — | `crates/cli/tests/login_test.rs` | /oauth2/revoke; deletes creds. |
| zeroship whoami | 🟢 | `zeroship whoami` | `crates/cli/src/auth.rs` | — | `crates/cli/tests/login_test.rs` | /userinfo; transparent refresh. |
| zeroship secret set/list/rm | 🟢 | `zeroship secret set KEY=value \| list \| rm KEY` | `crates/cli/src/secrets.rs` | — | — | ls/del aliases; splits on first '='. |
| zeroship var set/list/rm | 🟢 | `zeroship var set KEY=value \| list \| rm KEY` | `crates/cli/src/secrets.rs` | — | — | /vars endpoint. |
| Bearer token resolution | 🟢 | internal | `crates/cli/src/main.rs` | `docs/runbooks/local-dev.md` | `crates/cli/src/main.rs` | flag > env > saved creds. |
| create-zeroship-app scaffolder | 🟢 | `npm create zeroship-app <name>` | `sdks/create-zeroship-app/bin/create.js` | — | `sdks/create-zeroship-app/template/` | _gitignore → .gitignore; private registry. |
| create-zeroship-app template | 🟢 | @zeroship/vite-plugin + rpc/server | `sdks/create-zeroship-app/template/src/index.ts` | — | `sdks/create-zeroship-app/template/` | Notes CRUD + storage + kv + React. |
| Vite plugin — zeroship() factory | 🟢 | `import { zeroship } from '@zeroship/vite-plugin'` | `sdks/vite-plugin/src/index.ts` | `docs/reference/vite-plugin.md` | `sdks/create-zeroship-app/template/vite.config.ts` | mode full/static; rpc.strict not effectful. |
| Vite plugin — dev server spawn + proxy | 🟢 | internal | `sdks/vite-plugin/src/dev-server.ts` | `docs/reference/vite-plugin.md` | `examples/db-todos` | Spawns zeroship serve; crash-restart. |
| Vite plugin — Vite Environment API | 🟢 | internal | `sdks/vite-plugin/src/environment.ts` | `docs/reference/vite-environment-api.md` | — | /__zeroship_fetch + /__zeroship_hmr_check. |
| Vite plugin — dev HMR poll | 🟢 | internal (V8 polls) | `sdks/vite-plugin/src/dev-bootstrap/hmr.ts` | — | — | 500ms; transitive importer walk. |
| Vite plugin — dev-auth provider | 🟢 | `ZeroshipOptions.devAuth` | `sdks/vite-plugin/src/dev-auth-config.ts` | `docs/reference/auth-dev-tier.md` | — | Fresh secret per lifetime; dev-only. |
| Vite plugin — dev-db (SQLite zero-config) | 🟢 | internal | `sdks/vite-plugin/src/dev-db.ts` | `docs/reference/auth-dev-tier.md` | — | shell > .env > sqlite default. |
| Vite plugin — .dotenv parsing | 🟢 | internal | `sdks/vite-plugin/src/dev-server.ts` | — | — | Bespoke; shell wins over .env. |
| Vite plugin — server-entry auto-detection | 🟢 | internal | `sdks/vite-plugin/src/build.ts` | `docs/reference/vite-plugin.md` | — | Fixed candidate list. |
| Vite plugin — 'use server' (file-level) | 🟢 | @zeroship/vite-plugin transform | `sdks/vite-plugin/src/transform.ts` | `docs/reference/vite-plugin.md` | `sdks/create-zeroship-app/template/src/index.ts` | Directive is the only opt-in. |
| Vite plugin — 'use server' (function-level) | 🟢 | @zeroship/vite-plugin transform | `sdks/vite-plugin/src/transform.ts` | — | — | Only graph consumes today. |
| Vite plugin — wire-id + collision detection | 🟢 | internal | `sdks/vite-plugin/src/manifest.ts` | `docs/reference/rpc.md` | — | Prod rejects bare name. |
| Vite plugin — lazy procedure support | 🟢 | @zeroship/vite-plugin | `sdks/vite-plugin/src/transform.ts` | `docs/reference/vite-plugin.md` | — | Literal boolean only. |
| Vite plugin — server-graph walk (strict) | 🟡 | `ZeroshipOptions.rpc.strict` | `sdks/vite-plugin/src/server-graph.ts` | `docs/reference/vite-plugin.md` | `sdks/vite-plugin/test/server-graph.test.ts` | Not wired into build.ts. |
| Vite plugin — node compat shims | 🟢 | internal | `sdks/vite-plugin/src/node-compat.ts` | `docs/reference/node-compat.md` | — | unenv@2 + custom; @rollup/plugin-inject. |
| Vite plugin — production build (.zship) | 🟢 | internal (closeBundle) | `sdks/vite-plugin/src/build.ts` | `docs/reference/zship.md` | `examples/db-todos` | rolldown SSR; strips 'use server'. |
| Vite plugin — .zship packing + precompress | 🟢 | internal (emitZship) | `sdks/vite-plugin/src/zship.ts` | `docs/reference/zship.md` | — | brotli default; canonical JSON. |
| Vite plugin — static mode | 🟢 | `zeroship({ mode: 'static' })` | `sdks/vite-plugin/src/build.ts` | `docs/reference/vite-plugin.md` | — | Stub input then deleted. |
| Vite plugin — client-manifest virtual module | 🟢 | `virtual:zeroship/client-manifest` | `sdks/vite-plugin/src/build.ts` | — | — | Graceful {} fallback. |
| Vite plugin — dev-bootstrap | 🟢 | internal | `sdks/vite-plugin/src/dev-bootstrap/index.ts` | — | — | ModuleRunner; deps reoptimize rebuild. |
| CLI serve — dev KV backend (redb/Redis) | 🟢 | ZEROSHIP_KV_URL / ZEROSHIP_KV_PATH | `crates/cli/src/main.rs` | — | — | URL→Redis, else redb. |
| CLI serve — dev storage (LocalFs) | 🟢 | ZEROSHIP_STORAGE_ROOT | `crates/cli/src/main.rs` | — | — | Default .zeroship/storage. |
| CLI serve — heap limit configuration | 🟢 | --heap-limit-mb / ZEROSHIP_HEAP_LIMIT_MB | `crates/cli/src/main.rs` | `docs/reference/runtime-limits.md` | — | Dev default 512MB vs prod 128MB. |
| zeroship build | ⚫ | (removed) | `crates/cli/src/main.rs` | — | — | Build path is @zeroship/vite-plugin. |
| zeroship inspect | ⚫ | (removed) | `crates/cli/src/main.rs` | — | — | Removed in artifact-layout redesign. |
| Vite plugin — rpcEndpoint option | 🟠 | `ZeroshipOptions.rpcEndpoint` | `sdks/vite-plugin/src/index.ts` | `docs/reference/vite-plugin.md` | — | Accepted but non-effectful. |
| subscription procedures | 🟡 | `subscription(handler, config)` | `sdks/vite-plugin/src/transform.ts` | `docs/reference/vite-plugin.md` | — | Server discovered; client UNIMPLEMENTED. |

---

## 19. @zeroship/ui design system

A governed React design system on Base UI (headless primitives) with a single registered theme,
"crystal" (cool pastel glass). Four export tiers: interactive primitives (40+), layout
primitives (7 layouts + 2 compositions), composed application blocks (12), and marketing page
sections (7). All styling uses semantic CSS custom properties (`--zs-*`) with an oklch-only
palette. **Storybook is the sole reference doc surface** — there is no prose doc in
`docs/reference/` for this package.

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| ThemeProvider + useTheme | 🟡 | `ThemeProvider, useTheme, themes` | `sdks/ui/src/theme.tsx` | — | `sdks/ui/src/stories/story.css` | ADR specified 3 themes; only crystal built. |
| Design token foundation (styles.css) | 🟢 | `import '@zeroship/ui/styles.css'` | `sdks/ui/src/styles.css` | — | — | Theme-invariant; Tailwind contract file missing. |
| Crystal theme palette | 🟢 | internal CSS ([data-theme='crystal']) | `sdks/ui/src/styles.css` | — | — | Only registered theme. |
| Button | 🟢 | `Button` | `sdks/ui/src/components/Button/Button.tsx` | — | `sdks/ui/src/stories/Button.stories.tsx` | 4 variants, loading, asChild, a11y guard. |
| Input | 🟢 | `Input` | `sdks/ui/src/components/Input/Input.tsx` | — | `sdks/ui/src/stories/Input.stories.tsx` | outline/filled/plain; Field context. |
| Field | 🟢 | `Field, useFieldVisualSize` | `sdks/ui/src/components/Field/Field.tsx` | — | `sdks/ui/src/stories/Field.stories.tsx` | Cascades required/disabled/size. |
| Fieldset | 🟢 | `Fieldset, useFieldsetDisabledContext` | `sdks/ui/src/components/Fieldset/Fieldset.tsx` | — | `sdks/ui/src/stories/Fieldset.stories.tsx` | Propagates disabled to descendants. |
| Form | 🟢 | `Form, FormActions` | `sdks/ui/src/components/Form/Form.tsx` | — | `sdks/ui/src/stories/Form.stories.tsx` | Server-error routing; no initialValues. |
| Checkbox | 🟢 | `Checkbox` | `sdks/ui/src/components/Checkbox/Checkbox.tsx` | — | `sdks/ui/src/stories/Checkbox.stories.tsx` | Indeterminate-capable. |
| CheckboxGroup | 🟢 | `CheckboxGroup` | `sdks/ui/src/components/CheckboxGroup/CheckboxGroup.tsx` | — | `sdks/ui/src/stories/CheckboxGroup.stories.tsx` | Controlled/uncontrolled multi-value. |
| Switch | 🟢 | `Switch` | `sdks/ui/src/components/Switch/Switch.tsx` | — | `sdks/ui/src/stories/Switch.stories.tsx` | Field context. |
| Radio / RadioGroup | 🟢 | `Radio, RadioGroup` | `sdks/ui/src/components/Radio/Radio.tsx` | — | `sdks/ui/src/stories/Radio.stories.tsx` | Horizontal/vertical. |
| Toggle / ToggleGroup | 🟢 | `Toggle, ToggleGroup` | `sdks/ui/src/components/Toggle/Toggle.tsx` | — | `sdks/ui/src/stories/Toggle.stories.tsx` | Single/multiple exclusive. |
| Select | 🟢 | `Select` | `sdks/ui/src/components/Select/Select.tsx` | — | `sdks/ui/src/stories/Select.stories.tsx` | Single/multiple; hidden input. |
| Combobox | 🟢 | `Combobox` | `sdks/ui/src/components/Combobox/Combobox.tsx` | — | `sdks/ui/src/stories/Combobox.stories.tsx` | Typeahead; chips in multiple. |
| Autocomplete | 🟢 | `Autocomplete` | `sdks/ui/src/components/Autocomplete/Autocomplete.tsx` | — | `sdks/ui/src/stories/Autocomplete.stories.tsx` | Free-text value kept as-is. |
| NumberField | 🟢 | `NumberField` | `sdks/ui/src/components/NumberField/NumberField.tsx` | — | `sdks/ui/src/stories/NumberField.stories.tsx` | Stepper; min/max/step. |
| Slider | 🟢 | `Slider` | `sdks/ui/src/components/Slider/Slider.tsx` | — | `sdks/ui/src/stories/Slider.stories.tsx` | Auto single/range; value badge. |
| OtpField | 🟢 | `OtpField` | `sdks/ui/src/components/OtpField/OtpField.tsx` | — | `sdks/ui/src/stories/OtpField.stories.tsx` | Paste-split; keyboard nav. |
| Card | 🟢 | `Card` | `sdks/ui/src/components/Card/Card.tsx` | — | `sdks/ui/src/stories/Card.stories.tsx` | header/media/footer anatomy. |
| Dialog | 🟢 | `Dialog, createDialogHandle` | `sdks/ui/src/components/Dialog/Dialog.tsx` | — | `sdks/ui/src/stories/Dialog.stories.tsx` | Glass backdrop; imperative handle. |
| AlertDialog | 🟢 | `AlertDialog` | `sdks/ui/src/components/AlertDialog/AlertDialog.tsx` | — | `sdks/ui/src/stories/AlertDialog.stories.tsx` | Destructive-confirm variant. |
| Drawer | 🟢 | `Drawer` | `sdks/ui/src/components/Drawer/Drawer.tsx` | — | `sdks/ui/src/stories/Drawer.stories.tsx` | Slide-in from 4 edges. |
| Popover | 🟢 | `Popover, createPopoverHandle` | `sdks/ui/src/components/Popover/Popover.tsx` | — | `sdks/ui/src/stories/Popover.stories.tsx` | Anchored floating panel. |
| Tooltip | 🟢 | `Tooltip, createTooltipHandle` | `sdks/ui/src/components/Tooltip/Tooltip.tsx` | — | `sdks/ui/src/stories/Tooltip.stories.tsx` | Hover/focus; decorative-safe. |
| PreviewCard | 🟢 | `PreviewCard, createPreviewCardHandle` | `sdks/ui/src/components/PreviewCard/PreviewCard.tsx` | — | `sdks/ui/src/stories/PreviewCard.stories.tsx` | Hover rich preview. |
| Menu | 🟢 | `Menu, createMenuHandle` | `sdks/ui/src/components/Menu/Menu.tsx` | — | `sdks/ui/src/stories/Menu.stories.tsx` | Items/groups/submenus/radio. |
| ContextMenu | 🟢 | `ContextMenu` | `sdks/ui/src/components/ContextMenu/ContextMenu.tsx` | — | `sdks/ui/src/stories/ContextMenu.stories.tsx` | Right-click; cursor position. |
| Menubar | 🟢 | `Menubar` | `sdks/ui/src/components/Menubar/Menubar.tsx` | — | `sdks/ui/src/stories/Menubar.stories.tsx` | App-style menu bar. |
| Toolbar | 🟢 | `Toolbar, ToolbarComponent` | `sdks/ui/src/components/Toolbar/Toolbar.tsx` | — | `sdks/ui/src/stories/Toolbar.stories.tsx` | Roving tabindex. |
| NavigationMenu | 🟢 | `NavigationMenu` | `sdks/ui/src/components/NavigationMenu/NavigationMenu.tsx` | — | `sdks/ui/src/stories/NavigationMenu.stories.tsx` | Mega-menu popouts. |
| Tabs | 🟢 | `Tabs` | `sdks/ui/src/components/Tabs/Tabs.tsx` | — | `sdks/ui/src/stories/Tabs.stories.tsx` | underline/chip/pill; indicator. |
| Accordion | 🟢 | `Accordion` | `sdks/ui/src/components/Accordion/Accordion.tsx` | — | `sdks/ui/src/stories/Accordion.stories.tsx` | single/multiple; animated. |
| Collapsible | 🟢 | `Collapsible` | `sdks/ui/src/components/Collapsible/Collapsible.tsx` | — | `sdks/ui/src/stories/Collapsible.stories.tsx` | Single-section disclosure. |
| Toast + useToast | 🟢 | `Toast, useToast` | `sdks/ui/src/components/Toast/Toast.tsx` | — | `sdks/ui/src/stories/Toast.stories.tsx` | Imperative; success/error/warn/info. |
| ScrollArea | 🟢 | `ScrollArea` | `sdks/ui/src/components/ScrollArea/ScrollArea.tsx` | — | `sdks/ui/src/stories/ScrollArea.stories.tsx` | Auto-hide scrollbars. |
| Avatar | 🟢 | `Avatar` | `sdks/ui/src/components/Avatar/Avatar.tsx` | — | `sdks/ui/src/stories/Avatar.stories.tsx` | Image/initials/icon fallback. |
| Badge | 🟢 | `Badge` | `sdks/ui/src/components/Badge/Badge.tsx` | — | `sdks/ui/src/stories/Badge.stories.tsx` | 5 intents; static. |
| Tag | 🟢 | `Tag` | `sdks/ui/src/components/Tag/Tag.tsx` | — | `sdks/ui/src/stories/Tag.stories.tsx` | Removable chip. |
| Icon | 🟢 | `Icon` | `sdks/ui/src/components/Icon/Icon.tsx` | — | `sdks/ui/src/stories/Icon.stories.tsx` | Governed lucide wrapper. |
| Separator | 🟢 | `Separator` | `sdks/ui/src/components/Separator/Separator.tsx` | — | `sdks/ui/src/stories/Separator.stories.tsx` | solid/dashed/dotted. |
| Breadcrumbs | 🟢 | `Breadcrumbs` | `sdks/ui/src/components/Breadcrumbs/Breadcrumbs.tsx` | — | `sdks/ui/src/stories/Breadcrumbs.stories.tsx` | aria-current on last. |
| Meter | 🟢 | `Meter, meterStatus` | `sdks/ui/src/components/Meter/Meter.tsx` | — | `sdks/ui/src/stories/Meter.stories.tsx` | Semantic <meter>. |
| Progress | 🟢 | `Progress` | `sdks/ui/src/components/Progress/Progress.tsx` | — | `sdks/ui/src/stories/Progress.stories.tsx` | Indeterminate/determinate. |
| Skeleton | 🟢 | `Skeleton` | `sdks/ui/src/components/Skeleton/Skeleton.tsx` | — | `sdks/ui/src/stories/Skeleton.stories.tsx` | text/rounded/circular shimmer. |
| Spinner | 🟢 | `Spinner` | `sdks/ui/src/components/Spinner/Spinner.tsx` | — | `sdks/ui/src/stories/Spinner.stories.tsx` | sr-only label; reduced-motion. |
| Stack layout primitive | 🟢 | `Stack` | `sdks/ui/src/layouts/Stack/Stack.tsx` | — | `sdks/ui/src/stories/Stack.stories.tsx` | 1D flex; governed gap. |
| Grid layout primitive | 🟢 | `Grid` | `sdks/ui/src/layouts/Grid/Grid.tsx` | — | `sdks/ui/src/stories/Grid.stories.tsx` | minColWidth or columns. |
| Cluster layout primitive | 🟢 | `Cluster` | `sdks/ui/src/layouts/Cluster/Cluster.tsx` | — | `sdks/ui/src/stories/Cluster.stories.tsx` | Wrapping inline row. |
| Container layout primitive | 🟢 | `Container` | `sdks/ui/src/layouts/Container/Container.tsx` | — | `sdks/ui/src/stories/Container.stories.tsx` | The single width authority. |
| Split layout primitive | 🟢 | `Split` | `sdks/ui/src/layouts/Split/Split.tsx` | — | `sdks/ui/src/stories/Split.stories.tsx` | Fixed Side + fluid Main. |
| Center layout primitive | 🟢 | `Center` | `sdks/ui/src/layouts/Center/Center.tsx` | — | `sdks/ui/src/stories/Center.stories.tsx` | Intrinsic centering. |
| AppShell layout composition | 🟢 | `AppShell, useAppShellSidebar` | `sdks/ui/src/layouts/AppShell/AppShell.tsx` | — | `sdks/ui/src/stories/AppShell.stories.tsx` | Header/body/footer; skip-link. |
| PageHeader layout composition | 🟢 | `PageHeader` | `sdks/ui/src/layouts/PageHeader/PageHeader.tsx` | — | `sdks/ui/src/stories/PageHeader.stories.tsx` | breadcrumbs/title/actions. |
| EmptyState block | 🟢 | `EmptyState` | `sdks/ui/src/blocks/EmptyState/EmptyState.tsx` | — | `sdks/ui/src/stories/EmptyState.stories.tsx` | Centered empty-collection. |
| ErrorState block | 🟢 | `ErrorState` | `sdks/ui/src/blocks/ErrorState/ErrorState.tsx` | — | `sdks/ui/src/stories/ErrorState.stories.tsx` | Intent colors; retry actions. |
| StatCard block | 🟢 | `StatCard` | `sdks/ui/src/blocks/StatCard/StatCard.tsx` | — | `sdks/ui/src/stories/StatCard.stories.tsx` | value + delta arrow. |
| Banner block | 🟢 | `Banner` | `sdks/ui/src/blocks/Banner/Banner.tsx` | — | `sdks/ui/src/stories/Banner.stories.tsx` | 5 intents; live-region option. |
| DescriptionList block | 🟢 | `DescriptionList` | `sdks/ui/src/blocks/DescriptionList/DescriptionList.tsx` | — | `sdks/ui/src/stories/DescriptionList.stories.tsx` | DL/DT/DD semantics. |
| DataTable block | 🟢 | `DataTable` | `sdks/ui/src/blocks/DataTable/DataTable.tsx` | — | `sdks/ui/src/stories/DataTable.stories.tsx` | @tanstack/react-table; sort/filter/paginate/select. |
| Pagination block | 🟢 | `Pagination, buildPageItems` | `sdks/ui/src/blocks/Pagination/Pagination.tsx` | — | `sdks/ui/src/stories/Pagination.stories.tsx` | prev/next + numbered. |
| FilterBar block | 🟢 | `FilterBar` | `sdks/ui/src/blocks/FilterBar/FilterBar.tsx` | — | `sdks/ui/src/stories/FilterBar.stories.tsx` | Search + active-filter chips. |
| ListView block | 🟢 | `ListView` | `sdks/ui/src/blocks/ListView/ListView.tsx` | — | `sdks/ui/src/stories/ListView.stories.tsx` | Stacked rows; href/onClick a11y. |
| FormSection block | 🟢 | `FormSection` | `sdks/ui/src/blocks/FormSection/FormSection.tsx` | — | `sdks/ui/src/stories/FormSection.stories.tsx` | stacked/aside; footer actions. |
| AuthForm block | 🟢 | `AuthForm` | `sdks/ui/src/blocks/AuthForm/AuthForm.tsx` | — | `sdks/ui/src/stories/AuthForm.stories.tsx` | signIn/signUp; no bundled auth. |
| Stepper block | 🟢 | `Stepper` | `sdks/ui/src/blocks/Stepper/Stepper.tsx` | — | `sdks/ui/src/stories/Stepper.stories.tsx` | WCAG 1.4.1 (not color-only). |
| Hero section | 🟢 | `Hero, SectionTone` | `sdks/ui/src/sections/Hero/Hero.tsx` | — | `sdks/ui/src/stories/Hero.stories.tsx` | eyebrow/title/media; Container-wrapped. |
| PricingTable section | 🟢 | `PricingTable` | `sdks/ui/src/sections/PricingTable/PricingTable.tsx` | — | `sdks/ui/src/stories/PricingTable.stories.tsx` | Featured tier ring + badge. |
| FeatureGrid section | 🟢 | `FeatureGrid` | `sdks/ui/src/sections/FeatureGrid/FeatureGrid.tsx` | — | `sdks/ui/src/stories/FeatureGrid.stories.tsx` | 2/3/4 cols. |
| Cta section | 🟢 | `Cta` | `sdks/ui/src/sections/Cta/Cta.tsx` | — | `sdks/ui/src/stories/Cta.stories.tsx` | inline/stacked; accent tone. |
| StatsBand section | 🟢 | `StatsBand` | `sdks/ui/src/sections/StatsBand/StatsBand.tsx` | — | `sdks/ui/src/stories/StatsBand.stories.tsx` | Billboard stats. |
| Faq section | 🟢 | `Faq` | `sdks/ui/src/sections/Faq/Faq.tsx` | — | `sdks/ui/src/stories/Faq.stories.tsx` | Built on Accordion. |
| Footer section | 🟢 | `Footer` | `sdks/ui/src/sections/Footer/Footer.tsx` | — | `sdks/ui/src/stories/Footer.stories.tsx` | Multi-column link groups. |
| SectionTone system | 🟢 | `SectionTone` (type) | `sdks/ui/src/sections/_tone.ts` | — | — | data-section-band + data-tone. |
| Storybook documentation + a11y gate | 🟢 | http://127.0.0.1:6006 | `sdks/ui/.storybook/` | `sdks/ui/README.md` | `sdks/ui/src/stories/` | Sole API reference; MCP server. |
| Tailwind v4 contract file | 🔵 | `@zeroship/ui/tailwind.css` (declared) | `sdks/ui/package.json` | `docs/decisions/2026-05-26-design-system.md` | — | Export declared; file does not exist. |
| Atelier / Studio / Dusk themes | 🔵 | (would be themes/ThemeName) | `sdks/ui/src/styles.css` | `docs/decisions/2026-05-26-design-system.md` | — | ADR-specified; never implemented. |

---

## 20. Other SDK packages

The remaining published `@zeroship/*` packages not covered above (added 2026-06-11 per the
completeness critic).

| Feature | Status | Surface | Code | Docs | Example | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| `@zeroship/react` — React bindings for db reactivity | 🟢 | `useQuery` / `useSuspenseQuery` / `QueryClientProvider` / `createDefaultClient` | `sdks/react/src/` | none | `sdks/react/test/{useQuery,useSuspenseQuery}.test.tsx` | Bridges `@zeroship/db` reactive-broker queries onto the React render cycle (P8b stage 4). Distinct from `@zeroship/rpc-react`. 9 unit tests. |
| `@zeroship/eslint-config` — flat ESLint config + custom rule | 🟢 | `recommended` preset + the `no-unindexed-query` rule (D1) | `sdks/eslint-config/src/{index,rules/no-unindexed-query}.ts` | none | `sdks/eslint-config/tests/no-unindexed-query.test.ts` | Lints unindexed `env.db` queries at author time (the build-time peer of the runtime warning in §3). |
| `@zeroship/types` — ambient TS declarations | 🟢 | published `.d.ts` (auth/db/globals/shared/zeroship) | `sdks/types/*.d.ts` | none | — | Ambient types for the `zeroship` module + SDK globals. |
| `zeroship` — published stub package | ⚫ | the bare `zeroship` import (placeholder) | `sdks/zeroship-stub/` (name `zeroship`, `0.0.0-stub`) | none | — | Placeholder reserving the `zeroship` npm name; the real module is runtime-injected. |

---

## Cross-cutting status rollup

Totals across **20** areas (≈745 catalogued features; the §20 packages + the corrected
`crates/platform` dead-crate entry were added 2026-06-11):

| Status | Count | Share |
| --- | --- | --- |
| 🟢 shipped | 634 | 86.5% |
| 🟡 partial | 52 | 7.1% |
| 🟠 stub | 18 | 2.5% |
| 🔵 planned | 22 | 3.0% |
| ⚫ dead | 7 | 1.0% |
| **Total** | **733** | 100% |

### Notable STUB / DEAD / PLANNED features

**Metering & billing is the largest cluster of incomplete work:**
- 🔵 `env.meter.*` native primitive — listed as planned in AGENTS.md; `crates/control/src/metering.rs` is a one-line stub; never registered in the runtime.
- 🟠 Metering aggregation helpers (`crates/control/src/metering.rs`) — single-line stub comment, no impl.
- 🟡 Worker → control usage pipeline — `POST /internal/usage` is implemented but **no worker ever calls it**; usage read returns empty maps in real deploys.
- 🟠 Usage history snapshots (`app_usage_history`) — table exists; no code reads or writes it.
- 🟡 Platform fee enforcement — the 15% fee is set by the SDK and read from the Stripe payload, never re-computed server-side.
- 🟠 Stripe Connect onboarding URL — returns a hardcoded placeholder; the real `account_links` POST is a TODO.

**Object storage has no production backend:**
- 🟠 S3/R2/MinIO/Spaces/B2 backend — the `s3` Cargo feature flag exists with **zero source files**; gates nothing.
- 🔵 `S3BlobStore` / `CachedBlobStore` — comment-referenced only; all app bundles sit on one node's disk with no replication.
- 🟠 `content_type` sidecar metadata — accepted on put, silently dropped, always null on get.
- 🔵 Presigned URLs, image transform, per-app storage quota — no code (quota is open finding ST-2).

**Authorization P10–P12 are documented but unbuilt:**
- 🔵 P10 toggle-matrix PAT UI, 🔵 P11 orgs + Cedar analyzer + incident lock, 🔵 P12 end-user `env.authz` (no `plugin-authz` crate, no `@zeroship/permissions`, no manifest `authz` field), 🔵 CLI `zeroship policy edit`.
- 🟡 Operator platform-policy CRUD — rows persist but are **not merged into the running Cedar engine** (no hot-reload).
- 🟡 MFA Cedar conditions (`RequireMfa`/`MfaWithin`) — lowered correctly but rejected at PAT mint and never populated from session state; `TimeWindow` is UTC-only.

**Runtime / Node gaps:**
- 🔵 `env.assets.*` native primitive, 🔵 `structuredClone` transfer, 🔵 streaming fetch request body.
- 🟠 `node:zlib` stream constructors, 🟠 `node:os` networkInterfaces/get/setPriority — throwing stubs.
- 🟡 `node:zlib` / `node:os` build↔runtime mismatch (absent from the Vite plugin `RUNTIME_NATIVE_MODULES`, fall through to unenv in dev).
- 🟡 `node:crypto` `getCiphers()` returns empty despite Cipher being shipped (stale stub).

**Subscriptions are server-only:**
- 🟡 `subscription()` procedures — transport works and is tested, but the public RPC client proxy is **UNIMPLEMENTED** (Vite stub fails at runtime); use `stream()` for shipped live feeds.

**Other notable dead / partial:**
- ⚫ `crates/platform` (`zeroship-platform`) — **EXISTS but is DEAD/excluded**: ~5,800 LOC / 35 files (core/plan/enforcement/metering/billing/control/server) **excluded from the workspace** (`Cargo.toml` `exclude`), built on tokio + axum + sqlx (violating the zero-tokio invariant), depended on by nothing, last touched 2026-05-06. It holds a **complete-but-unrunnable metering + billing + spending-limit implementation** (`core/meter_store.rs` MeterStore trait w/ InMemory/Sqlite/Redis/Postgres/Mmap backends, `core/billing.rs`, `metering/` flush+rollover) — superseded by the live `control`+`gateway`+`worker`, which have only the 1-line `metering.rs` stub (see §12). "Not built" is wrong; "built, then abandoned, can't run" is the truth — and the reason re-implementing metering/billing in the compio stack is from-scratch work, not a port.
- ⚫ `zeroship build` / `zeroship inspect` CLI commands — removed in the artifact-layout redesign (build path is `@zeroship/vite-plugin`).
- ⚫ `manifest.exports.schema` / `ManifestExports.handlers` — deprecated Stage 5c; kept on the wire for archive upgrade only.
- ⚫ Worker WebSocket-upgrade-via-/dispatch — returns 500 (gateway uses a separate WS path).
- ⚫ `rpcEndpoint` Vite option — accepted but non-effectful; stubs use the fixed `/__zeroship/v1/<id>` path.
- ⚫ Vite plugin stale dist-only files (`resolve-schema.js`, `ts-to-json-schema.js`, `zsapp.js`) — orphaned.
- 🟡 App suspension (control) — flag is written but the gateway does not act on it; a suspended app keeps serving.
- 🟡 Gateway WS subscription proxy (multi-node) — returns 501; affinity selection runs but proxy not wired (use `zeroship serve` single-tenant).
- 🟠 Sandbox cold-boot endpoint (501) and per-token share revoke (501).
- 🟡 Per-procedure `timeout` — manifested but `timeout_ms` hardcoded `None` in the gateway; not enforced.
- 🟠 Per-procedure `middleware` list — carried in the manifest but the runtime middleware chain is not wired.
- 🟡 compio-postgres TLS — full negotiation code exists but only `NoTls` is exported.
- 🔵 compio-redis TLS — `rediss://` not implemented (MITM possible on plaintext link).
- 🔵 `@zeroship/ui` Tailwind v4 contract file and Atelier/Studio/Dusk themes — declared in the ADR / package.json but never built (only "crystal" ships).

### Consolidated documentation gaps

The single largest cross-cutting gap is **reference documentation**. Many shipped, load-bearing
surfaces have no `docs/reference/` page. The actionable list, grouped by area:

**Runtime / Node (no reference doc for any native web API):**
- The entire native WinterCG surface: `fetch`, Request/Response/Headers, WHATWG Streams,
  Compression/Decompression streams, WebCrypto, URL/URLSearchParams, TextEncoder/Decoder
  (+ stream variants), AbortController/Signal, EventTarget/Event/CustomEvent/DOMException,
  FormData, Blob/File, structuredClone, atob/btoa, EventSource, timers, `performance.now()`,
  console, Intl. (Streams/WebCrypto reference docs are referenced in source comments but
  absent from `docs/reference/`.)
- Platform extensions: SSRF protection, RPC capability gates, AI-SDK SSE encoder.
- node modules `node:crypto`, `node:buffer`, `node:zlib`, `node:os`, `node:path`, `node:util`
  have only the `node-compat.md` stub; the Vite polyfills (`node:process`/`module`/`timers/promises`),
  the `__zeroshipNodeBuiltin` dev bridge, and the `getCiphers()` / `node:zlib`–`node:os`
  build↔runtime divergences are undocumented.

**env.db:** CDC broker / `openSubscription`, WAL replication consumer, replication slot lifecycle,
mask-drift cron, mask/encryption backfill DDL ops, `__zeroship_migrations` audit table, migration
sweeper, `drop_namespace`, per-app PG auth bootstrap, DataLoader, encrypted-field filter fence,
`init_pool_async` lazy-init contract. (`sqlite-divergences.md` omits auth/session bootstrap and
the SQLite mask-migration limitation.)

**env.kv:** the `{app_id}:` scope/hash-tag wire format, the validation constants (key 512 B, value
256 KiB, TTL 100 yr, list limits), the full error-code set + sync TypeError paths, `createKv`
test injection, `kv_connection` retry hint, SCAN glob escaping, the cluster single-shard ceiling,
`incr` bigint behavior, and the redb-vs-Redis lazy-TTL divergence.

**env.storage:** there is **no reference doc page at all** (contrast db/kv/auth). Also undocumented:
`ZEROSHIP_STORAGE_ROOT`, the multi-node shared-volume requirement, `content_type` discard behavior,
path-validation rules, and the missing SDK test suite.

**Auth:** the four crons (JWK rotation, audit retention, token sweep, account reaper), rate
limiting, the mailer abstraction + drivers, email suppression, relay email forwarding, CSRF
double-submit, the Hydra admin client, security-headers middleware, bootstrap/JWK seeding, startup
validation, account eligibility, the `@zeroship/auth` React adapter, `requestScopes`,
`onAuthStateChange`, TOTP backup codes, and the pairwise subject identifier.

**RPC:** `streamResponse`, lazy loading, query auto-batching, the `__SERVER_REFERENCE` brand,
module-level `$config`, the WebSocket subscription wire protocol, the B3 capability frame,
`runQuery`/`runMutation`, the per-request context getters, `__zsDispatch`, `createFetchHandler`,
the dev HMR registry, `defineRpcProcedures`, `newUuidV7`, the `@zeroship/rpc-react` /
`@zeroship/rpc-client` packages, and the `onError`/`onAuthExpired` hooks.

**Deploy / bootstrap:** `fetchFast` signature/semantics, the `zeroship` module exports
(`waitUntil`/`getRequest`/`current*`/`runQuery`), the dev-entry API, `createFetchHandler` wire
protocol, the WS subscription frame protocol + close codes, mask-policy flush, the `__zsDbPlatform`
capability boundary (P9 §8), the `user.index()` fallback, and the `@zeroship/bootstrap` package.

**Gateway:** back-channel logout endpoint, the auth.zeroship.ai reverse-proxy split, app-response
header sanitization (SEC-9), trust-proxy IP derivation, the Hydra circuit breaker, the per-thread
PG pool, RLS GUC tenant isolation, the `x-wall-time-ms` header, global rate limiting + concurrency
limiting config, insecure-dev semantics, Ed25519 key rotation, and the Redis idempotency-store
backend contract.

**Control plane:** per-app OAuth client lifecycle, PAT issuance/policy model, the platform admin
surface (roles, Cedar policies, suspension, audit-lock), OAuth grant management, the two crons,
console bootstrap, secret key rotation, app suspension, rate-limit config, trust-proxy, and the
Cedar AuthzGuard dual-bearer path.

**Authorization:** PAT list/revoke endpoints, app suspension + audit-lock policies, OAuth client
registration, user OAuth grant endpoints, the `DEFAULT_PLATFORM_ROLE='none'` invariant (C1 IDOR
fix), operator platform-policy CRUD (+ its unimplemented hot-reload), and the MFA / TimeWindow
condition limitations — all documented only in proposal/code comments.

**Billing:** the account-history endpoint + soft-delete semantics, `plan_id` semantics, usage
history snapshots, the planned `env.meter.*`, platform-fee policy, and the worker-side usage
producer. (`stripe_handlers.rs` references a `stripe-integration-todo.md` that does not exist.)

**Bundle / .zship:** `ManifestExports.handlers` post-deprecation, AuthConfig/ScopeDef wire format,
`required_scopes`, resource inheritance + override markers, the rule/cache/rate-limit/CORS types,
the `middleware` list, the `runtime_assets` mutation protocol, the `aliases` map, `ManifestMetadata`,
the ingest limits (the zship.md "Limits" section does not exist), the legacy `BundleStore`/`LocalFs`
VFS, and the CLI `deploy` command.

**Vite plugin / CLI:** the Phase-2 static-binding entry, the server-graph walk + strict gate,
`probeUserDefaultExport`, `findServerEntry`, the dev RPC registry, the dev-bootstrap ModuleRunner,
the `client-manifest` type shim, SSR/SSG build modes, the `ZEROSHIP_BIN` override, auto-derived URL
resources, the custom node polyfills, function-level `"use server"`, the `.dotenv` parser, and the
`zeroship login`/`logout`/`whoami`/`secret`/`var` commands and `create-zeroship-app`.

**Sandbox:** the entire HTTP API (sandbox + preview + share-token), the agent Ed25519 protocol +
`/version` capability negotiation, sealed-record + snapshot artifact formats, the admin API,
the HA lease/heartbeat/takeover protocol, the wake state machine, the `sandbox_events` partition
schema, the Nomad `ch` driver TaskConfig + VM layout, agent body caps, the mint rate limiter, and
the `detach_isolated` pattern.

**Worker:** streaming forwarding, gateway auth + ZeroShip-User HMAC, the env snapshot cache, abort
on eviction, the `/logs` and `/metrics` and `/health` endpoints, `--check-config`, the
`urn:zeroship:` secret URN scheme, the Unix socket, shutdown drain, `needs_reload()`/`LoadedMeta`,
the SEC-7 env rotation, the per-thread cyper client, and SSG-only handling.

**Drivers + core:** `compio-postgres` and `compio-redis` have **no reference doc** (architecture,
wire format, pool config, TLS gaps); `typed_id`, the wire types, the AES-256-GCM at-rest crypto
(and the per-app HKDF gap), the auth utils (ZeroShip-User header, pairwise, HMAC), the
`urn:zeroship:` secret system (vault/awssm unresolvable), observability log formats, the DPoP /
BCL verifiers, the Hydra introspector, SuperJSON (Rust side), the preview port allowlist, and
wrapper revocation.

**@zeroship/ui:** every component, layout, block, and section is documented **only in Storybook**;
there is no `docs/reference/` page for the package, the crystal token contract, the design-token
foundation, the `SectionTone` system, the Toast imperative API, or the Storybook MCP server.
