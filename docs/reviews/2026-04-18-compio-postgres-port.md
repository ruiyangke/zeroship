# compio-postgres Port — Code Review Findings

Consolidated review from 3 independent code-critic agents (I/O foundation, Client API, advanced features).

## Ship-blockers (CRITICAL — multi-critic agreement)

### C1. `try_send` silently drops response batches
**File:** `connection.rs:349`
**Flagged by:** all 3 critics
```rust
let _ = response.sender.try_send(messages);  // drop on Full
```
Channels are bounded to 1. Under pipelining, COPY OUT, long RowStreams, or slow consumers, the second batch for the same request is dropped. If the dropped batch contains `ReadyForQuery`, the `Responses::poll_next` caller blocks forever (deadlock).
**Fix:** restore tokio-postgres's `pending_responses` side-channel. When `try_send` returns `Full`, push the batch back into a connection-owned queue and re-poll the sender on the next iteration before reading more bytes.

### C2. `read_backend` is NOT cancel-safe under compio
**File:** `connection.rs:180-194`, `buf_stream.rs:75-86`
**Flagged by:** I/O critic
compio's io_uring model uses owned buffers — `self.inner.read(buf).await` reads into an owned `Vec<u8>` and extends `self.read_buf` *after* the await returns. When `select_biased!` picks the recv branch, the read future is dropped; compio's `Submit::drop` calls `cancel(key)` which is documented as "not reliable — the underlying operation may continue." The kernel may have already completed the read; those bytes are discarded with the Vec. Under load with both sides ready, frames get lost mid-stream.
**Fix:** pin the read future across loop iterations; only poll `recv_fut` when the read future is genuinely parked. Or move to an explicit state-machine polling pattern that doesn't drop outstanding io_uring ops.

### C3. `__private_api_rollback` is fire-and-forget
**File:** `client.rs:642-654`, `transaction.rs:38-47`, `pool.rs:448-464`
**Flagged by:** advanced critic
When `Transaction` drops without commit, ROLLBACK is enqueued but not awaited, and the returned `Responses` is discarded. The pool's `return_client` trusts `is_closed() == false` → pushes the entry back as idle → next caller inherits a connection with an in-flight ROLLBACK. If the ROLLBACK itself fails (network drop, server error), the next caller's first query lands behind the orphaned ROLLBACK in FIFO order. The old pool's `status() != b'I'` check caught this — the port lost it.
**Fix:** either (a) make `__private_api_rollback` synchronous (await ROLLBACK completion) by exposing an async version and awaiting at Transaction Drop — requires an async-drop-workaround pattern, or (b) mark the Client "dirty" (needs verification) when drop-rollback happens and have `return_client` issue a validation barrier before reuse.

### C4. Shutdown never calls `socket.shutdown()`
**File:** `connection.rs:198-206`
**Flagged by:** I/O critic
After Client drops and Terminate is flushed, the run loop returns and the stream is just dropped. TLS session shutdown is skipped — rustls `allow_unclean_close = false` treats this as truncation attack. No clean TCP FIN/ACK.
**Fix:** after final flush, `self.stream.get_mut().shutdown().await?` before returning, handling `BrokenPipe`/`NotConnected` gracefully.

### C5. Recursive type resolution has no cycle detection
**File:** `prepare.rs:145-196`
**Flagged by:** Client critic
Domain-over-domain-over-itself, or composite referencing own row type, recurses forever → stack overflow. `client.type_(oid)` returns `None` until `set_type` is called after all children resolve, so any OID currently under resolution isn't visible to `get_type_rec`.
**Fix:** thread an `in_flight: HashSet<Oid>` through the recursive helpers; return `CycleDetected` error when the set already contains the current oid.

## High-severity (cross-critic agreement or single-critic with high confidence)

### H1. SCRAM channel-binding downgrade surface
**File:** `connect_raw.rs:278-302`
**Flagged by:** I/O critic
When server offers SCRAM-SHA-256-PLUS, if the TLS backend returns `None` from `tls_server_end_point` (missing cert hash support), code silently falls back to plain SCRAM. Only `channel_binding=require` in config prevents this; default is `prefer`, which is vulnerable.
**Fix:** when server advertises `-PLUS` and binding is missing, error unless `channel_binding=disable`.

### H2. ParameterStatus misrouted during handshake
**File:** `connect_raw.rs:84-89`, `read_info` at 351-388
**Flagged by:** I/O critic
`Handshake::next()` routes ALL async messages (Notice/Notification/ParameterStatus) into `delayed`. Tokio-postgres only defers NoticeResponse. When `read_backend` classifies ParameterStatus as Async (arriving at buffer head), it bypasses the `read_info` accumulator. `client.parameter("server_version")` called right after connect may return None because the connection task hasn't replayed the delayed queue yet.
**Fix:** in `Handshake::next`, only push NoticeResponse into `delayed`; return ParameterStatus and NotificationResponse so `read_info`/`authenticate` see them in-place.

### H3. BufStream `fill()` alloc churn under slow peers
**File:** `buf_stream.rs:68-88`
**Flagged by:** I/O critic
While-loop allocates `vec![0u8; capacity]` every iteration with `capacity = max(8KB, min_bytes - have)`. For a 64MB frame arriving in 16KB chunks, that's 4096 iterations × 65MB allocations = ~260 GB heap churn. Real DoS vector against slow-lorising servers.
**Fix:** allocate once at 8KB and loop on that; only grow if needed size exceeds current capacity. Or use `BytesMut::reserve` + read-into-uninit.

### H4. CopyIn ErrorResponse kills the connection instead of routing to request
**File:** `connection.rs:263-301`
**Flagged by:** I/O critic
`Action::Backend(Err(e))` inside the COPY select returns the error up, killing the whole connection. Should distinguish ErrorResponse (route to the request's Response sender so caller sees `Err(Error::db(...))`) from socket-broken (kill connection).
**Fix:** propagate the parsed backend message back; if ErrorResponse, dispatch via the response channel; if I/O error, kill.

### H5. No write backpressure — deadlock window
**File:** `connection.rs:144-220`
**Flagged by:** I/O critic
`handle_request` writes + flushes synchronously. Under slow-server write backpressure, `flush().await` blocks the whole loop including the read half. Server can't drain its send because we aren't reading; our write buffer fills. Both sides wedge.
**Fix:** interleave read/write progress. Don't accept new requests if the write half isn't draining. At minimum, run flush concurrently with reads in the select.

### H6. Statement/Portal/__private_api_rollback Drop panics on NUL names
**Files:** `statement.rs:23-29`, `portal.rs:19-25`, `client.rs:645-647`
**Flagged by:** Client critic
`frontend::close(...).unwrap()` panics if name contains NUL. Statement names are `s{NEXT_ID}`, so internally safe — but `__private_api_rollback(Some(savepoint_name))` takes user-supplied names. If a downstream crate passes arbitrary strings as savepoint names, this is a DoS from user input.
**Fix:** replace `unwrap()` with `if let Err(_) = ... { return; }` in Drop; validate or sanitize savepoint names at the boundary.

### H7. total-counter waiter-starvation race in pool
**File:** `pool.rs:421-443`
**Flagged by:** advanced critic
Waiters only woken on `return_client`, not after a successful connect. Multiple tasks each reserving-then-connecting can starve the waiter queue.
**Fix:** wake a waiter after successful reservation in addition to return.

### H8. copy_in 4KB coalescing boundary off-by-one
**File:** `copy_in.rs:160-176`
**Flagged by:** advanced critic
With `this.buf` at exactly 4096 bytes and no further writes, data sits indefinitely until `poll_flush`. Threshold check `> 4096` should be `>=` or document the boundary.
**Fix:** change to `>=` and clarify in comments.

### H9. binary_copy.rs ignores reserved "critical" header bits
**File:** `binary_copy.rs:184`
**Flagged by:** advanced critic
Spec: "if any bit of the higher-order 16 bits is set, the reader must abort." Code only checks bit 16 (`has_oids`), ignores 17-31.
**Fix:** `if flags & 0xFFFF_0000 != 0 { return error }`.

### H10. Concurrent prepare leaks server-side prepared statements
**File:** `prepare.rs:205-277` (typeinfo_* slots)
**Flagged by:** Client critic
Two concurrent `query_raw` calls both hit a cold typeinfo slot; both `prepare_rec`, both PREPARE on server, only one wins the cache; the loser's server-side statement lives until disconnect with no DEALLOCATE.
**Fix:** double-checked locking on typeinfo statement slots.

## Medium-severity (selected)

- **M1. `query_text_params` NUL-byte and type-drift hazards** — `query.rs:79-154`. NUL in param = server rejects with `invalid byte sequence`; PG version drift can change inferred return types silently.
- **M2. Handshake::next corrupts iterator on parse error** — `connect_raw.rs:73-97`. `?` leaves `pending` half-consumed; subsequent calls see corrupt state. Fix: clear `pending` on error.
- **M3. codec.rs intra-batch length validation missing** — `codec.rs:128`. `validate_length` only runs on batch-head; nested messages with inflated lengths slip through until 64MB cap eventually fires via incremental fill.
- **M4. `idle_timeout` scan is O(n²)** — `pool.rs:505-509`. Not material at max_size=8; problematic if pool sizes grow.
- **M5. Waiter tombstone accumulation** — `pool.rs:728-744`. Cancelled waiters linger; `pending_count()` is misleading. Consider periodic compaction.
- **M6. ReadUncommitted silently promoted to ReadCommitted** — `transaction_builder.rs:74`. PG accepts syntax, silently upgrades. User tuning for throughput gets no signal.
- **M7. `inc_created` not called in housekeeper success path** — `pool.rs:296`. Metrics drift.
- **M8. `read_info` builds returned `parameters` that's incomplete if messages come through Async path** — see H2.
- **M9. `simple_query("")` validation goes through different protocol than old Conn::execute("")**. Behaviorally OK; documentation claim "matches legacy path exactly" is inaccurate.
- **M10. CopyDone-before-ReadyForQuery in batch**: copy_out stream terminates on CopyDone, stranding RFQ in Responses.cur — harmless given Responses drop, but contributes to the try_send drop hazard (C1).

## Low (informational only; not fixing)

- Unbounded type cache (inherited from tokio-postgres)
- RowStream::get debug-log branch does redundant encode
- Row::ranges allocated eagerly per row (hot-path micro-opt)
- generic_client.rs async-trait boxing (measurable at 300k req/s)
- eprintln! for housekeeper errors may leak config
- i32 vs u32 for process_id (inherited bug)
- Drop-unwrap on copy_fail("") (arithmetically can't fail, but worth a comment)

## Summary

**14 CRITICAL/HIGH issues worth fixing before commit.** The port is structurally correct and integration tests pass (23/23), but the tests are single-threaded and don't exercise the race-sensitive paths that critics flagged. The `try_send` drop (C1), `read_backend` cancel-safety (C2), fire-and-forget rollback (C3), missing shutdown (C4), and cycle detection (C5) are all production hazards under realistic load.

**Scoring:** I/O foundation 72/100, Client API 78/100, pool 70/100, transaction 65/100, copy_in 78/100, copy_out 88/100, binary_copy 80/100. Average: ~76/100. Not ready to ship without C1-C5 fixed.
