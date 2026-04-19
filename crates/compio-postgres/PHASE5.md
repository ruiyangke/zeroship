# Phase 5 — transactions, COPY, notifications

`cargo check -p compio-postgres` (with and without `--features tls`) passes cleanly — no errors, no warnings attributed to this crate. Workspace check is unchanged (the single pre-existing warning in `crates/runtime/src/runtime.rs:435` is from Phase 2 and unrelated to this port).

## Files ported

| File | LOC | Source file | LOC (src) | Shape |
|---|---|---|---|---|
| `src/transaction.rs` | 360 | `transaction.rs` | 348 | Near-verbatim. `tokio::io` → `compio::io` on the deprecated `cancel_query_raw` bound; everything else lifted byte-for-byte — `Transaction<'a>`, `Savepoint`, full method surface, Drop → `__private_api_rollback`, nested `transaction()`/`savepoint()` via SAVEPOINT-sp_N. `#[cfg(feature="runtime")]` guards dropped (we always have the runtime path). |
| `src/copy_in.rs` | 239 | `copy_in.rs` | 225 | Verbatim. `CopyInReceiver` (Stream), `CopyInSink<T>` (Sink), `poll_finish`, 4 KB coalescing buffer, `CopyData`/`CopyDone`/`CopyFail`+`Sync` terminal framing. |
| `src/copy_out.rs` | 63 | `copy_out.rs` | 57 | Verbatim. `copy_out(client, statement)` + `CopyOutStream: Stream<Item = Result<Bytes, Error>>`. |
| `src/connection.rs` | 446 | `connection.rs` | 356 | Updated: `RequestMessages::CopyIn(CopyInReceiver)` variant added; request-handler branch grew an interleaved `select_biased!` that drains user COPY frames while still servicing backend messages. See below. |
| `src/client.rs` | 636 | `client.rs` | 800 | `copy_in` / `copy_out` `todo!()` swapped to real calls. No other changes — `transaction` / `build_transaction` already dispatched to real `Transaction<'_>` in Phase 4 via the in-file stub, which now resolves to the real `transaction.rs` module without a client-side edit. |
| `src/lib.rs` | 194 | — | — | Removed the three Phase-5 inline stub modules (`transaction`, `copy_in`, `copy_out`). Added real `mod` declarations and re-exports of `Transaction`, `CopyInSink`, `CopyOutStream`. lib.rs is now 194 LOC (down from ~418) — stubs fully retired. |

## Connection COPY interleaving — the only real design decision

The source tokio-postgres interleaves COPY frame writes with backend reads by parking the `CopyInReceiver` in `pending_request` every time the user's mpsc is `Pending`, yielding control back to the select loop where `poll_read` handles any async or error messages. This is critical for correctness: during a multi-GB COPY, if the server sends `ErrorResponse` mid-stream, the reader must make progress or the TCP send buffer fills and the whole pipeline deadlocks.

Our compio async-fn loop doesn't naturally express "park and come back". The naïve drain `while let Some(msg) = receiver.next().await` blocks the entire connection task and reproduces the deadlock hazard. The fix is a local `select_biased!` that races `receiver.next()` against `read_backend(&mut self.stream)`:

```rust
'copy: loop {
    let action = {
        let read_fut = read_backend(&mut self.stream).fuse();
        let recv_fut = receiver.next().fuse();
        select_biased! {
            frame = recv_fut => Action::SendFrame(...) / Action::RecvDone,
            msg   = read_fut => Action::Backend(...),
        }
    };
    match action { ... }
}
```

The action-enum indirection exists because the borrow of `self.stream` inside `read_backend(...)` inside the select can't coexist with the subsequent `write_frontend(&mut self.stream, ...)` call on the same iteration. Collecting the decision as a `Copy`/`Send` enum lets us drop the read future before reborrowing the stream to write.

**Cancellation safety.** `read_backend` is cancel-safe: it only buffers bytes into `BufStream`, which is kept on `self.stream` across iterations. If the select drops the read future mid-fill, the kernel socket buffer still contains the unread bytes; the next `read_backend` call resumes from the same BufStream and refills normally.

**Backpressure.** The user's `CopyInSink` writes to `mpsc::channel(1)`. When the connection is busy flushing an in-flight frame, `poll_ready` on the user sink parks, throttling the producer. Channel depth of 1 is correct — any larger queue risks buffering GBs of copy data in memory when the server is slow.

## Notifications — kept Phase 3 shape

Phase 3 installed `Connection::notifications()` as a setter that registers an `mpsc::UnboundedSender<AsyncMessage>` on the Connection, returning the receiver. Phase 5's task spec suggested a `Client::notifications() -> Notifications<'_>` borrow-shape to match tokio-postgres. After reading the source: **tokio-postgres has no `Notifications` struct.** Its source exposes async messages through `Connection::poll_message` (the handcranked-Future API), and users that want a stream of them construct their own wrapping `Stream`. Phase 3's `Connection::notifications() -> UnboundedReceiver<AsyncMessage>` is functionally equivalent and more ergonomic with async-fn. Left unchanged.

## `target_session_attrs` probe — still deferred, documented

The probe needs to run `SHOW transaction_read_only` on a fresh `(Client, Connection)` pair before handing either to the caller. Plumbing options considered:

1. **Spawn a short-lived driver task.** Run `Connection::run` on `compio::runtime::spawn`, issue the query from a temporary `Client`, await the response, then somehow stop the driver and hand the Connection back. Compio's `spawn` returns a `Task<T>` that runs to completion — we'd need an abort primitive or a signal to break the driver's select loop early. Possible, but invasive.

2. **Inline pump helper.** Add a `Connection::poll_one_step` that executes one iteration of the select loop synchronously, then call it from a `poll_fn` that also polls the query future. Source (tokio) does exactly this pattern via `poll_unpin`. Our async-fn loop doesn't expose per-iteration stepping — we'd have to re-model the loop as an explicit state machine.

3. **Reconnect and retry.** If the first `(Client, Connection)` pair has the wrong `transaction_read_only`, close it and error out. The user sees a setup failure rather than a silent fallback, which matches the spec's "ReadWrite/ReadOnly" enforcement semantics. This is a behavioral regression versus tokio-postgres's auto-retry-to-next-host logic, and adds no benefit over just rejecting non-default settings.

Phase 5 retains Phase 4's guard: non-default `target_session_attrs` returns an explicit `Error::config(...)` message at connect time. Single-host default configs (`Any`) are unaffected — which covers every platform deployment today.

**If Phase 6's pool layer surfaces a pattern for "connect, probe, stamp, hand back", we can lift it cleanly.** Until then, the guard stays.

## LOC summary

```
 src/transaction.rs           360  (new — was 268-line inline stub)
 src/copy_in.rs               239  (new — was  50-line inline stub)
 src/copy_out.rs               63  (new — was  22-line inline stub)
 src/connection.rs            446  (was 377: +69 for CopyIn variant and interleave loop)
 src/client.rs                636  (was 634: +2 for copy_in/copy_out wiring)
 src/lib.rs                   194  (was 418: -224 for stub removal, +imports)
```

Total crate: ~8,000 LOC (all 27 source files ported end-to-end).

## Verification

```
$ cargo check -p compio-postgres
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.60s

$ cargo check -p compio-postgres --features tls
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.62s

$ cargo check --workspace --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.77s
    (one pre-existing warning in crates/runtime/src/runtime.rs:435, unrelated)
```

No errors. No warnings attributed to compio-postgres. All three checks pass.

## Phase 6 hand-off

The crate's public surface is now complete. Phase 6 scope per the plan:

1. **`src/pool.rs`** — port the HikariCP pool from `crates/pg/src/pool.rs` (max-lifetime, idle timeout, wait queue, validation — the post-3113934 improvements). Pool entries hold `(Client, Task<Result<(), Error>>)` pairs; checkout spawns the connection's run task via `compio::runtime::spawn` and returns the Client; drop/evict calls `__private_api_close` on the Client, which signals the connection task to drain and exit.

2. **Workspace migration.** Three downstream crates currently import from `zeroship_pg`:
   - `crates/plugin-db/` (query builder, schema)
   - `crates/control/` (registry, auth)
   - `crates/auth/` (when it lands)

   Each needs `Cargo.toml` dep swap (`zeroship-pg` → `compio-postgres`) and an import-path update. Surface is mostly identical (Client, Statement, Row, Transaction, ToSql, FromSql all re-exported at the same paths). The main behavioral difference is that `Client` and `Connection` are `!Send` now (compio's Rc-based TcpStream) — any downstream code that spawns queries on a non-local tokio runtime will fail to compile, forcing the migration to compio's spawn. That's the intended behavior.

3. **Delete `crates/pg/`** or leave as a deprecated shim re-exporting from `compio-postgres` for one release. Recommendation: delete. Keeping both invites drift and doubles the dep graph.

4. **Integration tests.** Port the 23 tests in `crates/pg/tests/`. Expected-to-just-work: basic CRUD, prepare/execute, transactions, COPY, LISTEN/NOTIFY. Known-to-need-work: tests that explicitly check tokio runtime behavior (cancellation, spawn-then-cancel patterns) — these need rewriting for compio's single-threaded LocalSpawn model.

5. **Benchmark.** Run the existing `zeroship-bench.rhai` scenarios against a compio-postgres-backed worker and compare to `zeroship-pg`. We expect parity or better — compio's completion-based io_uring should outperform tokio's readiness-based epoll on high-pipelining workloads, but only by a few percent on latency-bound queries.

6. **Optional — target_session_attrs probe.** If Phase 6's pool adds a checkout-time validator hook (e.g. for `SELECT 1` liveness), the same hook can run `SHOW transaction_read_only` and drop the guard in `connect.rs`.

7. **Optional — parallel failover.** Phase 3's sequential host loop could be replaced with `FuturesOrdered` to race A/AAAA results. Optional perf win; not required for correctness.
