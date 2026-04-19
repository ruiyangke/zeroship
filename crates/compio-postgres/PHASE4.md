# Phase 4 — query / prepare / simple_query / cancel

`cargo check -p compio-postgres` (with and without `--features tls`) passes cleanly — no errors, no warnings attributed to this crate. Workspace check is unchanged (the single pre-existing warning in `crates/runtime/src/runtime.rs:435` is unrelated).

## Files ported

| File | LOC | Source file | LOC (src) | Shape |
|---|---|---|---|---|
| `src/prepare.rs` | 277 | `prepare.rs` | 267 | Near-verbatim. Parse + Describe + Sync, parameter/column resolution, recursive `get_type` with `prepare_rec`/`get_type_rec` boxed-future helpers. Statement slot cache populated via `InnerClient::{set_typeinfo, set_typeinfo_composite, set_typeinfo_enum}` |
| `src/query.rs` | 391 | `query.rs` | 382 | Verbatim port. `query` / `query_typed` / `execute` / `execute_typed` / `query_portal` / `encode` / `encode_bind` / `extract_row_affected` / `sync` all preserved. `pin_project_lite::pin_project! { RowStream { statement, responses, rows_affected } }` with `impl Stream` poll-driven |
| `src/simple_query.rs` | 121 | `simple_query.rs` | 115 | Verbatim. `SimpleColumn` + `SimpleQueryStream` + `simple_query` + `batch_execute` |
| `src/cancel_query.rs` | 57 | `cancel_query.rs` | 52 | Verbatim. Routes through Phase 2's `connect_socket` and this phase's `cancel_query_raw` |
| `src/cancel_query_raw.rs` | 41 | `cancel_query_raw.rs` | 31 | Verbatim control flow; only the I/O Ext-trait calls change — `tokio::io::AsyncWriteExt::{write_all, flush, shutdown}` → compio owned-buffer variants. Buffer discarded after `write_all`'s `BufResult` |
| `src/client.rs` | 634 | `client.rs` | 800 | Expanded Phase 3 skeleton. Full public surface: `prepare`, `prepare_typed`, `query`, `query_one`, `query_opt`, `query_raw`, `query_scalar`, `query_one_scalar`, `query_opt_scalar`, `query_typed`, `query_typed_one`, `query_typed_opt`, `query_typed_raw`, `execute`, `execute_typed`, `execute_raw`, `simple_query`, `simple_query_raw`, `batch_execute`, `check_connection`, `cancel_token`, `transaction`, `build_transaction`. `copy_in` / `copy_out` are Phase 5 `todo!()`s |
| `src/lib.rs` | 418 | — | — | Stubs for `query`, `prepare`, `simple_query`, `cancel_query`, `cancel_query_raw` removed. Real `mod` declarations added; re-exports of `RowStream`, `SimpleColumn`, `SimpleQueryStream` lifted from module roots. Only Phase 5 stubs remain: `transaction`, `copy_in`, `copy_out` |

## Deviations from source

1. **`target_session_attrs` post-connect probe still blocked.** The source's probe interleaves `Connection::poll_unpin(cx)` with a `simple_query_raw` future via `poll_fn`. Our `Connection::run` consumes `self`, so the in-place poll of the un-started connection isn't directly expressible. `simple_query_raw` itself is real now, but the probe requires a cooperative step — either a `poll_one_step` method on Connection or a spawn/re-join dance. Both fit more naturally once Phase 5's transaction loop is in. The guard remains, error message updated: `"target_session_attrs is not yet supported; Phase 5 will implement the probe"`. Single-host default connections are unaffected.

2. **Deprecated methods re-added.** `Client::cancel_query` / `cancel_query_raw` (the two `#[deprecated]` convenience methods) are now present so the Client surface matches tokio-postgres exactly. They forward to `cancel_token().cancel_query[_raw]`.

3. **`CancelToken` has no uses of `pub(crate) fn new(...)`.** The struct fields are `pub(crate)`, matching the source. `Client::cancel_token()` constructs it directly.

4. **`query::query_portal` under `#[allow(dead_code)]`.** No Portal-driving code path in Phase 4 — `bind.rs` returns a `Portal` but the only caller is in `generic_client` + Phase 5 transaction surface. Kept the function so Phase 5's `Transaction::bind`/`query_portal` port is a drop-in.

5. **`query::sync` under `#[allow(dead_code)]`.** Used only by `Client::check_connection`, which is a user-facing method — lint fires because the internal function is crate-private while the caller doesn't itself emit diagnostics. Harmless.

## Statement cache mutators dropped `#[allow(dead_code)]`

Phase 3 placed `#[allow(dead_code)]` on every `InnerClient::typeinfo*` method because the callers hadn't landed yet. Phase 4's `prepare.rs` uses all of them, so the allows come off and the lint stays quiet.

## Verification

```
$ cargo check -p compio-postgres
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.17s

$ cargo check -p compio-postgres --features tls
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.53s

$ cargo check --workspace --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.69s
    (one pre-existing warning in crates/runtime/src/runtime.rs:435, unrelated)
```

No errors. No warnings attributed to compio-postgres.

## Phase 5 hand-off

Symbols / modules Phase 5 must introduce to close the port:

1. **`src/transaction.rs`** — replace the stub in `lib.rs`. The full `Transaction<'a>` API: `execute`, `query_*`, `prepare*`, `batch_execute`, `simple_query`, `commit`, `rollback`, nested `transaction()` (savepoint), `bind`, `query_portal`, `copy_in`, `copy_out`. Source is 345 LOC, near-verbatim candidate once copy modules exist.
2. **`src/savepoint.rs`** — nested transactions. Source has savepoint logic embedded in `Transaction::transaction()`; Phase 5 can keep it inline or split.
3. **`src/copy_in.rs`** — real `CopyInSink<T>` with `Sink` impl, `CopyInReceiver` on the `RequestMessages` enum, Parse + Bind + Describe + Execute + CopyData/CopyDone/CopyFail framing.
4. **`src/copy_out.rs`** — real `CopyOutStream` (Stream impl). Simpler than copy_in.
5. **`Client::copy_in` / `Client::copy_out`** — currently `todo!("Phase 5")`. Swap to the real impl.
6. **`RequestMessages::CopyIn(CopyInReceiver)` variant** — blocked on the `CopyInReceiver` stream type defined in `copy_in.rs`. Phase 3's connection loop already has a place-marker where the variant would be destructured.
7. **`target_session_attrs` probe** — interleave `Connection` future with `client.simple_query_raw("SHOW transaction_read_only")`. Simplest approach: add `Connection::poll_one_step(&mut self, cx) -> Poll<Result<(), Error>>` that runs a single loop iteration; call it from inside a `poll_fn` alongside the query stream. Drop the guard in `connect.rs`.
8. **Parallel failover** — Phase 3's sequential `for host in hosts { … }` could be replaced with `FuturesOrdered` to race A/AAAA results. Optional perf win; not required for correctness.
