# Native Node.js `node:crypto` shipped

**Status:** Shipped 2026-05-05 — synthetic module + Stages A/B/C kernel (Hash/HMAC/KDF/random/KeyObject/Sign/Verify/Cipher/keygen). Surface beyond Stage C tracked in the proposal.
**Long-form design:** [`docs/proposals/node-crypto-native.md`](../proposals/node-crypto-native.md)
**Implementation:** [`crates/runtime/src/web/crypto/`](../../crates/runtime/src/web/crypto/) (shared kernel) + `node:crypto` synthetic module registered via `crates/runtime/src/core/native_modules.rs` (`"node:crypto"` → `node::crypto::synthetic_module`).

## Context

`node:crypto` was a JS shim at `sdks/vite-plugin/src/node-compat.ts`
plus ad-hoc `__cryptoHashSync` / `__cryptoHmacSync` V8 callbacks.
The shim collected chunks into a JS array and decoded via
`TextDecoder` on each `update()` — silently corrupting non-text
binary data. Errors were plain `Error` instances rather than
spec-correct `error.code === "ERR_CRYPTO_*"` / `ERR_OSSL_*`, so
packages branching on those codes silently misbehaved. Without
native `node:crypto`, ~80% of remaining npm packages were broken:
`jsonwebtoken`, `bcrypt`, `scrypt-js`, `crypto-js`, `node-forge`,
every Postgres/MySQL/Redis driver (SCRAM HMAC), `axios`/`got`/`undici`
(AWS sigv4 + OAuth1), `firebase-admin`/`googleapis`/`aws-sdk`.

## Decision

- Adopt Node's API signatures verbatim; entry-shim layer translates JS args to the shared kernel.
- Reuse the WebCrypto kernel: `node:crypto`'s `webcrypto` / `subtle` / `getRandomValues` re-export the native WebCrypto classes; `KeyObject` shares the underlying `KeyMaterial` enum + key-store bridges (D-N4).
- Single provider: aws-lc-rs / aws-lc-sys (already in workspace).
- Bridge: `KeyObject.from(cryptoKey)` and `crypto.subtle.importKey('jwk', keyObject.export(...))` so JOSE and `jsonwebtoken` interop.
- Spec-correct Node error codes via `OpError::node(...)`. Each `ERR_*` code carries the right class (Error / TypeError / RangeError) per `node_errors.h` and `lib/internal/errors.js`.
- Synthetic module shape (not bundle code) so the loader at `sdks/vite-plugin` rewires straight to the native module specifier.
- Stage A (digest/HMAC/KDF), Stage B (hand tests + random), Stage C (KeyObject + Sign/Verify + Cipher + keygen) landed in sequence.

## Consequences

- Streaming hash/HMAC works correctly for binary chunks (zero-copy per-chunk `digest::Context::update`).
- JS shim at `sdks/vite-plugin/src/node-compat.ts:67-127` (60 LOC) and `__cryptoHashSync`/`__cryptoHmacSync` callbacks deletable.
- Post-ship layout: `crypto_ops/` renamed to `base/crypto/` (`dcdaf38`), `node:crypto` lifted out of `web/` into `node/` (`c63701e`, `2376b9d`).
- Stages beyond C (X.509 certificates, Diffie-Hellman groups, scrypt details, post-quantum) remain tracked in the proposal as continuing work.

## See also

- Implementing commits: `5aa0fac` crypto kernel — DigestContext + HmacContext + KDF (Stage A); `1e17cf4` crypto_node skeleton (Stage B); `0c42058` 36 Stage B smoke tests; `8fb4de3` add scrypt (raw FFI); `fb347d4` Stage C — KeyObject, Sign/Verify, Cipher, keygen; `6f37da0` register `node:async_hooks` + `node:crypto` as native synthetic modules; `abec749` Merge native node:async_hooks + node:crypto synthetic modules (#175); `1591466` node-compat: rewire `node:crypto` shim to native (delete dead callbacks); `2f69ac6` Merge RPC v2 phases 1+2 + native node:* modules + cleanups.
- Related ADRs: [webcrypto-native](./2026-05-02-webcrypto-native.md).
