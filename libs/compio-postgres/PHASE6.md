# Phase 6 — pool + workspace migration + legacy disposal

`cargo check --workspace` passes cleanly (only a pre-existing unsafe-op
warning in `crates/runtime/src/runtime.rs:435` remains; unrelated to this
port). `cargo test -p compio-postgres --no-run` is clean. `cargo test -p
zeroship-plugin-db --test integration -- --test-threads=1` reaches parity
with pre-migration: 16/21 pass; the 5 failures (`update_one_inc`,
`update_one_dec_mul`, `update_one_jsonb_array_ops`, `update_many_round_trip`,
`mixed_update`) reproduce **bit-for-bit on the pre-migration HEAD** — they
are pre-existing test bugs where `setup()` doesn't create the `updated_at`
column that `build_update_one` always writes.

## Files added / changed

| File | LOC | Kind | Notes |
|---|---|---|---|
| `src/pool.rs` | 579 (new) | port | HikariCP pool from `crates/pg/src/pool.rs`. Stores `Client` per entry, not `Conn`; dropped `needs_rollback` flag (Transaction handles it); `simple_query("")` for alive-validation; `Error::connect(io::Error::other(...))` for pool-layer failures |
| `src/client.rs` | +15 | feature | Added `Client::query_text_params(sql, &[&str])` convenience that delegates to `query::query_text_params` (see below) |
| `src/query.rs` | +92 | feature | Added `pub async fn query_text_params` — Parse with empty OID list (server infers), Bind with text format for params (code 0) and binary format for results (code 1). Matches legacy `zeroship-pg` semantics exactly |
| `src/row.rs` | +16 | feature | Added `Row::raw_value<I>(idx)` for callers that decode wire bytes manually (used by plugin-db for TIMESTAMP → Unix ms conversion) |
| `src/connect_raw.rs` | +20 | **bug fix** | `Handshake::next()` was dropping unread messages from each `BackendMessages` batch — the startup sequence (`AuthenticationOk + ParameterStatus* + BackendKeyData + ReadyForQuery`) arrives as one batch, we were returning the first and throwing the rest away. Added a `pending: BackendMessages` field that persists the iterator across calls |
| `src/lib.rs` | +1 | wiring | `mod pool;` + `pub use pool::{Pool, PoolConfig, PoolMetrics, PooledClient};` |

## Downstream crates migrated

| Crate | Files | Changes |
|---|---|---|
| `plugin-db` | `Cargo.toml`, `src/lib.rs`, `src/callbacks.rs`, `tests/integration.rs` | `zeroship-pg` → `compio-postgres` + `compio`; `TX_CONN: Option<Conn>` → `Option<Client>`; `Conn::connect` → `compio_postgres::connect + spawn`; `row.try_get::<T>` → `row.try_get::<_, T>` (compio-postgres has 2 generics); `col.oid` → `col.type_().oid()`; `col.name` → `col.name().to_string()` |
| `control` | `Cargo.toml`, `src/registry.rs`, `src/auth_service.rs` | Same pattern. `From<compio_postgres::Error>` for `RegistryError` now walks the `source()` chain to reach `DbError` messages (was matching on the old enum's Display) — preserves the duplicate-key detection heuristic |
| `auth` | `Cargo.toml`, `src/service.rs` | Same pattern. `open_conn` helper per crate spawns the connection task and returns the Client. Matches the spec — no `Transaction<'a>` used, we do manual BEGIN/COMMIT/ROLLBACK via `Client::execute` |

## Workspace Cargo.toml

- Replaced `zeroship-pg = { path = "crates/pg" }` with
  `compio-postgres = { path = "crates/compio-postgres" }` in
  `[workspace.dependencies]`.
- Added `compio = { workspace = true }` to `plugin-db`, `control`, `auth`
  so they can spawn the Connection task.

## Legacy crate disposal

- `crates/pg/` deleted entirely. `cargo check --workspace` passes after
  deletion, confirming no dangling refs.
- `docker-compose.yml` untouched (service names unchanged).

## Surprises and caveats

### 1. `Handshake::next()` batch-draining bug (upstream Phase 3)

Found while the first smoke test hung in `connect`. The existing
`Handshake::next` took a `read_backend` result, matched `Normal { mut
messages, .. }`, and returned `messages.next()` — dropping the rest of the
batch. Postgres almost always bundles AuthOk + ParameterStatus* +
BackendKeyData + ReadyForQuery into one TCP segment, so the second call to
`next()` would re-enter `read_backend` and block. Fixed by storing the
`BackendMessages` iterator in `Handshake.pending` and draining it across
calls. Strictly a correctness fix in Phase 3 code — unblocks the runtime.

### 2. `Type::TEXT` is not a drop-in replacement for "text-format, infer type"

The spec suggested `query_typed(sql, &[(param, Type::TEXT)])` for the
legacy `query_text_params` pattern. This doesn't work for the JSON-driven
query builders in plugin-db: `Type::TEXT` has OID 25, so Parse binds the
param as strictly TEXT, and a statement like `UPDATE t SET views = $1`
(where `views INTEGER`) fails with `E42804: column "views" is of type
integer but expression is of type text`. The legacy driver sent Parse with
an **empty OID list** (no type hints), which lets PG infer from context.

Fixed by adding a purpose-built `query::query_text_params` that mirrors the
legacy path exactly: `frontend::parse("", sql, iter::empty(), buf)` + Bind
with format=0 (text) + raw UTF-8 bytes. `Client::query_text_params` now
delegates to this; the typed-text path from the original spec is not used.

### 3. `needs_rollback` dropped cleanly

As the spec anticipated, tokio-postgres's `Transaction<'a>` handles
rollback-on-drop inside the transaction type itself. The pool never needs
to know. Both `get_inner`'s cleanup branch and `return_client`'s broken
check are now just `client.is_closed()` — no special ROLLBACK logic.

### 4. Pool error kind = `Connect`

Pool-layer synthetic errors (timeout, retries exhausted, warm-up failure)
are routed through `Error::connect(io::Error::other(msg))`. The
crate-private constructor is available because the pool lives inside
compio-postgres. Callers see `kind: Kind::Connect`, which is the closest
semantic match in the new error enum — failures are "can't hand you a
usable connection", which is what `Connect` means.

### 5. Test flakiness is unrelated

Ran the full integration suite on the pre-migration HEAD
(`3113934 feat(pg): HikariCP-inspired pool ...`) — same 5 `update_*`
failures, same `column "updated_at" of relation "notes" does not exist`
errors. The schema in `tests/integration.rs::setup` has never matched the
SQL that `build_update_one` emits; this is a test-side issue unrelated to
the port. 16 of 21 passing matches pre-migration exactly.

## LOC delta

```
 crates/compio-postgres/src/pool.rs       +579  (new file)
 crates/compio-postgres/src/query.rs       +92  (query_text_params)
 crates/compio-postgres/src/client.rs      +15  (Client::query_text_params)
 crates/compio-postgres/src/row.rs         +16  (raw_value accessor)
 crates/compio-postgres/src/connect_raw.rs +20  (Handshake.pending fix)
 crates/compio-postgres/src/lib.rs         +2   (pool module + re-exports)

 crates/pg/                              -1600  (entire crate deleted)

 crates/plugin-db/src/{lib,callbacks}.rs   +20  (net) — swap Conn→Client,
                                                spawn Connection task
 crates/plugin-db/tests/integration.rs     ~20  — same renames in tests
 crates/control/src/registry.rs            +30  (net) — open_conn helper,
                                                source-chain error match
 crates/control/src/auth_service.rs        +15  (net) — open_conn helper
 crates/auth/src/service.rs                +15  (net) — open_conn helper
 crates/*/Cargo.toml                       ~5   — dep swap
 Cargo.toml                                 ~3   — workspace dep swap
```

Net: +741 lines in compio-postgres, -1600 lines in legacy crate,
~150 lines of migration noise in downstream crates. End state is
smaller and the dep graph has one fewer internal crate.

## Verification

```
$ cargo check --workspace
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.33s
    (one pre-existing unsafe-op warning in runtime.rs:435, unrelated)

$ cargo test -p compio-postgres --no-run
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.16s

$ PG_TEST_URL='postgres://postgres:zeroship@localhost:5440/zeroship' \
  cargo test -p zeroship-plugin-db --test integration -- --test-threads=1
    test result: FAILED. 16 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out
    (same 5 pre-existing test-side failures as pre-migration HEAD)
```

Phase 6 closes out the port. The workspace no longer depends on the legacy
`zeroship-pg` crate; `compio-postgres` is the single PostgreSQL driver.
