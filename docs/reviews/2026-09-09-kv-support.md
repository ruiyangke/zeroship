# KV storage and V8 boundary review

The crate split is implemented as `zeroship-kv` and `zeroship-kv-v8`.
The storage crate owns backends, scoping, shared guardrails, and typed errors.
The binding owns JavaScript conversion, promise scheduling, per-isolate state,
and successful-operation metering. `KvBinding` is the binding's public entry
point; hosts construct storage backends directly. Backend tests moved with the
storage implementation. The dependency boundary is enforced by
`crates/zeroship-kv/tests/architecture.rs`.

The review also found existing behavior defects that remain outside the crate
extraction:

## Redis increment replies can lose precision

`INCR_TTL_SCRIPT` in `crates/zeroship-kv/src/backend/redis.rs` returns the Lua
numeric result of `INCRBY`. Redis stores the exact integer, but the Lua result
can round before the driver decodes it. The V8 binding's BigInt conversion cannot
recover information already lost at that boundary. This contradicts the native
integer-counter contract even when the TypeScript SDK is bypassed.

A live Redis probe reproduced a mismatch between the script's returned integer
and `GET` of the same key. Reproduce against the local test Redis with:

```sh
nix shell nixpkgs#redis -c redis-cli -p 6390 EVAL \
  "redis.call('SET', KEYS[1], '9007199254740992', 'PX', 60000); local n=redis.call('INCRBY', KEYS[1], '1'); local s=redis.call('GET', KEYS[1]); redis.call('DEL', KEYS[1]); return {n,s}" \
  1 '{kv-review}:precision'
```

The correction must return the stored decimal string from within the same Lua
execution and decode it into the Rust integer. A separate `GET` after `EVAL`
would race concurrent increments. The driver currently exposes integer-only
`eval` methods, so the storage and driver reply contracts must change together.
Add coverage at the Rust backend and V8 boundaries using increments that cannot
be represented exactly as a floating-point number.

## Embedded delete treats an expired row as present

`RedbBackend::delete` in `crates/zeroship-kv/src/backend/redb.rs` returns
`removed.is_some()` without consulting the stored expiration. In contrast,
`get`, `ttl`, `expire`, and `persist` inspect expiration and treat an expired row
as missing. Source inspection therefore identifies a dev/Redis difference:
deleting an expired key can report success locally after reads already report
absence. The correction should remove the stale row but compute the return
value using its expiration, with a shared backend regression case.

## Boundaries to preserve

`Backend` remains a trusted-host interface: callers supply the app scope and
validate creator input. It is not an authorization boundary for arbitrary Rust
callers. V8 obtains that scope from the runtime and validates before dispatch.
The embedded backend performs synchronous redb work on the calling thread;
extracting the crate does not change its blocking behavior. Metering remains in
the binding, so direct Rust backend calls require their own host accounting.
