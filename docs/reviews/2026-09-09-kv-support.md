# KV storage and V8 boundary review

The crate split is implemented as `zeroship-kv` and `zeroship-kv-v8`.
The storage crate owns backends, scoping, shared guardrails, and typed errors.
The binding owns JavaScript conversion, promise scheduling, per-isolate state,
and successful-operation metering. `KvBinding` is the binding's public entry
point; hosts open a configured `KvStore` and pass it to the binding. Rust callers
and V8 use scoped `Kv` handles from that store. Backend tests moved with the
storage implementation. The dependency boundary is enforced by
`crates/zeroship-kv/tests/architecture.rs`.

The review also found behavior defects outside the crate extraction.

## Redis increment precision — resolved

The Redis deployment refactor changed `INCR_TTL_SCRIPT` to return `GET` from
within the same Lua execution. Rust decodes the decimal string directly into
an integer. The native topology contract and V8 runtime tests now use increments
whose result cannot be represented exactly as a floating-point number.

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
