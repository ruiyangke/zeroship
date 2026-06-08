# compio-postgres driver review

A port of tokio-postgres to the compio/io_uring runtime, with a bespoke connection pool, replication client, and buffered-stream I/O layer.

The port is high-fidelity where it counts: the wire codec, framing, auth/TLS handshake, connection-string parsing, error/type/row surface, and the Client/Connection split are faithful — in several places (channel-binding `Require` enforcement, fail-closed `target_session_attrs`, removal of two upstream `.unwrap()` panics, the transaction dirty-flag checkout barrier) the port is measurably *more* correct than its ancestor. The author also correctly diagnosed the central io_uring hazard — that dropping an in-flight completion-model read can silently consume-then-discard kernel bytes — and refused to naively port tokio's `select!`-over-socket loop. The cost of that decision, however, is the headline risk: serializing the connection driver discarded upstream's *simultaneous* read+write multiplexing, which produces a COPY-IN deadlock and silent non-delivery of LISTEN/NOTIFY on idle connections. The single most serious defect is independent of the I/O model: the bespoke pool tracks capacity with a hand-maintained counter and **no RAII guard across its `await` points**, so any cancellation of `get()` — including the pool's own `connection_timeout` — permanently leaks capacity and eventually bricks the pool. That bug, POOL-1, should block any production use until fixed.

## Remediation status — ALL 11 fixed (2026-06-05, branch `fix/compio-postgres-driver-review`, not pushed)

Every verified finding was fixed under strict TDD (RED test proving the bug → minimal GREEN fix → refactor), each with a faithful regression test and its own commit. The criticals/high were independently re-proven RED (revert the fix, watch the test fail) by the reviewer, not just the implementing agent. Full suite after all fixes: **25 lib + 29 integration tests green** against a live Postgres 16 (docker compose, port 5440); all 7 dependent crates build clean; no new clippy errors.

| ID | Sev | Status | Commit | Regression test (RED-proven) |
| --- | --- | --- | --- | --- |
| POOL-1 | critical | ✅ Fixed | `e240c1d8` | `get_cancellation_during_connect_does_not_leak_permits` — pre-fix `total_count()` inflates to `max_size` and bricks |
| COPY-1 | critical | ✅ Fixed | `dacac0f4` | `copy_in_error_does_not_deadlock` — pre-fix deadlocks 15.2s, post-fix errors in 0.38s |
| IO-1 | high | ✅ Fixed | `dacac0f4` | (same root cause / fix as COPY-1 — concurrent read during COPY-IN) |
| IO-2 | high | ✅ Fixed | `dacac0f4` | `notify_delivered_on_idle_listener` — pre-fix times out (never delivered) |
| REPL-1 | high | ✅ Fixed (driver) | `2777d564` | `lsn_tracker_reports_flush_below_received` — pre-fix `(100,100,100)` vs `(100,50,50)` |
| POOL-2 | medium | ✅ Fixed | `099e20aa` | `freed_connection_goes_to_front_waiter_not_a_barging_fresh_caller` + reclaim-on-drop test |
| IO-3 | medium | ✅ Fixed | `dacac0f4` | `concurrent_queries_are_pipelined` (correctness; timing is non-discriminating — see note) |
| BINCOPY-1 | medium | ✅ Fixed | `8fd8c94d` | `header_accepts_oid_flag` + unknown-critical/low-bit/magic tests |
| IO-4 | low | ✅ Fixed | `dacac0f4` | dead EOF conjunct removed in the multiplexed loop |
| REPL-2 | low | ✅ Fixed | `6281b1f5` | `identify_row_truncated_is_error_not_panic` — pre-fix panics |
| REPL-3 | info | ✅ Fixed | `d5bcaade` | `start_replication_error_response_surfaces_dberror` — pre-fix loses the SQLSTATE |

Precondition commit `ce34c28b` flipped `[lib] test = false` → `true`, un-hiding **15 in-src unit tests** (13 replication, 2 config) that were compiled nowhere and never ran.

### How the run-loop cluster (COPY-1/IO-1/IO-2/IO-3/IO-4) was fixed
The serialized loop was replaced — **only on the splittable plain (NoTls) path, which is 100% of pool traffic** since `Pool::connect_one` uses `NoTls` — with a multiplexed design: a **dedicated, detached read task owns the read half and loops `read_backend` to completion** (so the cancel-unsafe read is never dropped — the never-drop invariant is now *structural*), forwarding frames over a cap-1 channel; the **main loop owns the write half and `select`s only over cancel-safe channel ops + a fully-awaited flush**. This delivers concurrent read+write: idle NOTIFYs are read, COPY-IN ErrorResponses are read mid-stream (no deadlock), and later requests are written while earlier responses arrive. FIFO framing and the `pending_responses` back-pressure ordering are preserved (the read branch is gated off while a stashed batch waits). The TLS variant keeps the original serialized loop as a documented fallback (compio-tls streams can't be split).

### Notes / follow-ups
- **REPL-1 is the *driver-level* half of the fix.** The driver now correctly separates the received vs. flushed LSN so a caller *can* report `flush < received`. Fully realizing the durability guarantee additionally requires the **consumer** (`crates/plugin-db/src/wal_consumer.rs`) to call `advance_lsn` only after a durable hand-off — a policy decision left out of scope here (different crate). A code comment on `advance_lsn` flags this.
- **IO-3**: the pg_sleep timing test cannot discriminate the fix because Postgres executes a single connection's queries strictly in order, so single-connection pipelining yields no latency win; the test was kept as a multi-request correctness regression instead.
- **Out of scope (pre-existing, not introduced here):** two `clippy::approx_constant` errors in the original `numeric_types` integration test (`integration.rs` ~551-552).

## OSS comparison

### Fidelity vs tokio-postgres

**Preserved.** The Client/Connection split is reproduced exactly: `Connection::run(self)` is spawned as a standalone task that solely owns the socket, while `Client` communicates over an mpsc request channel and `Responses` drains an mpsc reply channel — the same architecture tokio-postgres uses, minus the executor-feature gating (correctly stripped, since the compio port has no `runtime` feature). Pipelining is structurally preserved (requests are enqueued on an unbounded channel and the driver pages replies back FIFO), and FIFO framing integrity is intact: `pending_responses` is fully drained before the next frame is read, so a stashed first batch can never be overtaken by a same-request second batch, and a dropped `RowStream` cannot desync the pipeline. The prepared-statement cache, typeinfo cache, and the `query`/`prepare` paths are near-verbatim, with the prepare-cycle detection and recheck-before-set logic carried over correctly. Error mapping (`DbError`, `SqlState`, `Error::db`) and the `GenericClient` trait surface are byte-identical.

**Necessarily changed.** tokio-postgres builds its connection on `Framed<MaybeTlsStream, PostgresCodec>` driven by `poll_message_inner`, which runs `poll_read` *and* `poll_write` on every wake (connection.rs:301-318). compio is a completion-model runtime: a `Framed`/`select!`-over-socket design would, on the losing branch of a `select!`, drop an in-flight read whose kernel completion may already have consumed bytes into a now-discarded buffer — silent data loss. The port therefore replaces `Framed` with a bespoke `BufStream` (owned-buffer fill/retry I/O) plus free-function `read_backend`/`write_frontend`, and **serializes** the run-loop into phases so a `read_backend().await` is never dropped mid-flight.

Is that sound? The *memory/cancel-safety* decision is correct and is the right call — verified against the compio cancellation semantics, dropping a raced read here genuinely loses bytes, and the author avoided that trap. The codec rewrite (185 vs 98 LOC) faithfully reproduces `PostgresCodec::decode` (async-message split at idx 0, Normal batch terminated by `READY_FOR_QUERY`, correct length cap), and the added 64 MB length cap + bounded chunk reads are welcome DoS hardening with no protocol-visibility change. What is *not* sound is the collateral loss of read/write *concurrency*: phase-serialization is a stronger constraint than cancel-safety requires, and it is the direct root cause of IO-1/COPY-1 (COPY-IN deadlock), IO-2 (idle notify stall), and IO-3 (pipelining write-serialization). These are genuine divergences from the upstream contract, not forced adaptations — a fix can preserve cancel-safety (carry a single in-progress `read_backend` future across loop iterations) while restoring write-before-read concurrency.

### The bespoke pool vs deadpool-postgres / bb8

tokio-postgres ships no pool, so this is bespoke and is correctly judged against deadpool/bb8 conventions. **What it meets:** the per-entry detached-Connection-task model is a sound adaptation of the Client/Connection split to a poolable form; liveness detection via `Client::is_closed()` (== request-channel `sender.is_closed()`) is a valid signal because the task owns the receiver. The checkout barrier runs the dirty/validation probe as a *Client-side* `simple_query("")`, which is cancel-safe by the same argument as the main loop (a dropped probe future only drops the depth-1 `Responses` receiver; the connection task pages the reply to completion and discards it). The `is_dirty` barrier also correctly honors tokio-postgres's Transaction-drop-fires-ROLLBACK contract at the pool layer, and `Waiter::Drop` tombstones its slot to avoid waker leaks. The single-threaded `Cell`/`RefCell` model is appropriate given compio's `!Send` streams.

**What it misses** — the two conventions deadpool/bb8 are built around:

- **RAII permit accounting (the big one).** bb8/deadpool back capacity with an ordered semaphore whose permit is released in `Drop`, panic- and cancellation-safe. This pool uses a hand-maintained `total` counter with manual `±1` and *no Drop guard* across the three `await` points in `get_inner`. That is POOL-1 — a critical capacity leak.
- **Fair FIFO hand-off.** bb8/deadpool hand a freed connection directly to the front waiter via an ordered queue. This pool pushes the freed entry into a shared idle vec and only *advisorily* wakes a waiter, so a fresh caller can barge the slot ahead of a parked waiter. That is POOL-2 — FIFO unfairness / possible starvation.

### Bespoke replication & buf_stream posture

Replication has no tokio-postgres counterpart (upstream ships no `START_REPLICATION`/`CopyBothResponse` code), so it is judged against the PostgreSQL walsender protocol directly. The pgoutput decoder is exemplary — every field read is routed through `read_u8/u16/u32/u64` helpers that return `UnexpectedEof`, more defensive than anything upstream ships, and `next()` bounds-checks frame bodies before indexing. The defects are localized: one hand-rolled `IDENTIFY_SYSTEM` `DataRow` parser indexes raw (REPL-2), the `START_REPLICATION` error path discards the `DbError` (REPL-3), and the standby-status LSN arithmetic conflates received/flush/apply into one value (REPL-1, severity contested — see Findings). The `buf_stream` posture is otherwise a strength: the owned-buffer model, 64 MB cap, and `validate_length` guard are correct and faithful.

## Findings

### Critical

#### POOL-1 — Cancellation of `get()` permanently leaks the `total` permit (and destroys a live connection at the barrier awaits), exhausting the pool
**Area:** Connection pool (bespoke) · **File:** `crates/compio-postgres/src/pool.rs:360, 411, 434, 453-455`
**Reference:** bespoke — violates the production-pool RAII invariant (every early-return/cancellation between "permit taken" and "conn installed" must release the permit in `Drop`); compio `timeout` drops the losing future per `refs/compio/compio-runtime/src/time.rs:83-88`.

`Pool::get` wraps `get_inner` in `compio::time::timeout`, which is a `select!` that *drops* the inner future on elapse. Inside `get_inner`, `total` is the de-facto permit but is hand-maintained with manual `±1` and no `Drop` guard. Three `await` points sit between a `total`/entry commitment and its release: (a) the on-demand connect path sets `total += 1` (line 454) then awaits `connect_one(...)` (line 455) — the compensating `-1` lives only in the `Err`/`Ok` match arms, so a drop while parked at the `.await` runs neither arm and inflates `total` permanently; (b)/(c) the dirty barrier (line 411) and alive-bypass validation (line 434) pop an `entry` out of `idle` (line 380) and `.await` a `simple_query("")` *before* the entry is wrapped in a `PooledClient` or `active` is incremented — a drop here drops the bare `entry` (killing a live backend) while `return_client`, the only decrement path, never runs. There is no reconciliation: `total`/`active` are never recomputed from `idle.len()+active`. Crucially, path (a) needs **zero external cancellation** — if `connect_one` hangs near the 30 s `connection_timeout` (the network-partition case this pool exists to survive), the pool's own outer timeout fires while parked at line 455 and self-inflicts the leak.

**Impact:** Monotonic, permanent erosion of capacity under routine operational cancellation (HTTP client disconnect, handler `select!`, caller timeout, slow connect). After `max_size` leak events `total >= max_size` forever, the create gate at line 453 never fires again, every `get()` parks on `Waiter` and times out, and `total_count()` reports phantom-full while real PG backends leak at the barrier paths.

**Fix:** Make the permit RAII. Introduce a guard holding `&Pool` that increments `total` on construction and decrements on `Drop` unless explicitly `disarm()`ed once the connection is installed into the returned `PooledClient`. Wrap the line-454 reservation and the popped `entry` (lines 380-447) so any cancellation drop releases `total` and routes the still-usable entry back to idle (or counts the eviction). Add a regression test that `timeout`s/cancels a `get()` parked at `connect_one` *and* at the barrier `simple_query`, then asserts `total_count()` returns to baseline and a subsequent `get()` succeeds — the existing `pool_exhaustion` test does not cover this (it uses `min_idle:0` and only drops at the Drop-guarded `Waiter` await).

**Verify vote:** 2/2 upheld · **Confidence:** high

#### COPY-1 — COPY FROM STDIN loop never reads the socket, deadlocking the connection
**Area:** Transactions / cancel / COPY / replication · **File:** `crates/compio-postgres/src/connection.rs:352-381`
**Reference:** `refs/rust-postgres/tokio-postgres/src/connection.rs:305-309` (poll_message_inner runs `poll_read` every wake, concurrent with poll_write's CopyIn branch at 243-260).

The `RequestMessages::CopyIn` arm of `handle_request` is a send-only blocking loop: `receiver.next().await → write_frontend → flush().await`, with **no socket read** until the receiver yields `None`. A block comment claims it "poll[s] the socket non-blockingly between each frame by checking whether any parseable message is already in the read buffer" — that code does not exist. Upstream multiplexes: `poll_write`'s CopyIn branch writes one frame then stashes the receiver and returns, and `poll_read` runs in the same poll, so a mid-COPY `ErrorResponse` is delivered immediately. In compio, `flush().await` parks the *sole* connection task on the kernel send; during a large COPY the server can fill its recv buffer, stop draining, and (on a constraint violation / disk-full) try to send an `ErrorResponse` whose flush blocks because the client never reads — classic symmetric COPY deadlock with no timeout in the loop. One verifier found it is worse than scoped: because `copy_in()` awaits `BindComplete`/`CopyInResponse` *before* returning the sink while the connection task is already parked on the empty CopyIn receiver, the deadlock can fire at *every* COPY initiation, not only under backpressure.

**Impact:** A wedged connection task that is never returned to the pool (permanent capacity loss), plus a behavioral divergence even short of deadlock — constraint-violation errors surface only after the entire input is streamed.

**Fix:** Restore read/write multiplexing for the CopyIn branch under compio's cancel-safety contract: carry a single in-progress `read_backend` future across iterations (do **not** naively `select!`-drop the read), only awaiting `receiver.next()` when no read is pending, and deliver any `ErrorResponse` via `deliver_batch`. At minimum, implement the promised non-blocking peek of `stream.buf()` between frames, bound the flush with a write timeout, and delete the false comment.

**Verify vote:** 2/2 upheld (one verifier downgraded to medium, see note) · **Confidence:** high
**Severity note:** Both verifiers confirmed the deadlock mechanism is real and reproduces under compio's true semantics. One held it critical; the other corrected to **medium** on reachability grounds — `copy_in`/`BinaryCopyInWriter` have zero callers outside the driver crate (plugin-db is structured CRUD, not bulk COPY), so the bug is currently unreachable through any platform path. It is a confirmed correctness defect plus a provably false comment in shipped public API; treat it as **fix-before-anyone-builds-on-`copy_in`** rather than a live production fire. (IO-1 below is the same root defect viewed from the I/O-model area.)

### High

#### IO-2 — Idle connection never reads the socket; LISTEN/NOTIFY is silently never delivered to a pure-listener connection
**Area:** I/O model & cancellation safety · **File:** `crates/compio-postgres/src/connection.rs:251-270, 144-148`; `lib.rs:7`
**Reference:** `refs/rust-postgres/tokio-postgres/src/connection.rs:95-136` (poll_read intercepts async `NotificationResponse` regardless of outstanding requests).

When no response is in flight, Step E awaits **only** a new client request (`self.receiver.next().await`); it never reads the socket. The code's own comment admits unsolicited messages "remain in the kernel socket buffer." Upstream's `poll_read` returns `NotificationResponse` as an `AsyncMessage` on every readable poll, independent of any outstanding request. The crate advertises this feature — `lib.rs:7` ("async notifications preserved") and the public `Connection::notifications()` API exist specifically for LISTEN/NOTIFY. A connection used purely as a subscriber (the canonical pattern: `LISTEN chan` once, then await) never sends another request, so Step E blocks forever and the notification sits unread — no error, no log, just missing events. The non-delivery is total for a pure listener; connections that interleave queries instead see unbounded delivery latency (tied to the next query).

**Impact:** Silent functional regression of a documented, public-API feature — directly contradicting the "async notifications preserved" claim.

**Fix:** In Step E, when the async channel is registered, race the request receiver against a socket read using a **cancel-safe** primitive. Note: the finding's suggested `CancelToken` has the same "cancellation is not reliable" semantics and would not recover a discarded buffer; a faithful fix should instead gate an owned read on compio's non-consuming readability primitive (`poll_readable`/`PollFd`), or own the idle read in a resumable state machine.

**Verify vote:** 2/2 upheld · **Confidence:** high
**Severity note:** One verifier kept this **high** (fully-broken pure-subscriber path against a documented public API); the other corrected to **medium** after confirming zero first-party callers of `.notifications()` today (internal/pre-launch), so it bites only a future consumer or an external LISTEN/NOTIFY user. Both agree the divergence is real and not a forced adaptation.

#### IO-1 — COPY IN streams writes with no concurrent socket read (I/O-model view of COPY-1)
**Area:** I/O model & cancellation safety · **File:** `crates/compio-postgres/src/connection.rs:352-381`
**Reference:** `refs/rust-postgres/tokio-postgres/src/connection.rs:243-260` (poll_write CopyIn) run together with poll_read via `poll_message_inner:301-318`.

This is the same defect as COPY-1, raised independently in the I/O-model area: the CopyIn arm writes frames in a tight loop and never reads the socket until the receiver is exhausted, while a block comment falsely claims a non-blocking inter-frame read. The two sub-claims — (1) the arm has zero read/peek/try_recv calls; (2) upstream interleaves read+write in the same poll — were verified true by both verifiers.

**Impact / Fix:** As COPY-1.

**Verify vote:** 2/2 upheld · **Confidence:** high
**Severity note:** Both verifiers corrected this entry to **low**, on a more detailed reading of PostgreSQL's server behavior than COPY-1's verifiers applied: on a *simple-query* COPY error, the backend (`postgres.c:4998-5007`) explicitly *accepts-but-ignores* leftover `CopyData`/`CopyDone`/`CopyFail` rather than blocking, so the server keeps draining, the client's `flush()` unblocks, and the `ErrorResponse` is surfaced *later* (after the client flushes its buffered frames) via `CopyInSink::finish` returning `Err` — a **timeliness/latency divergence plus a lying comment**, not a permanent hang, for the common error cases. A true symmetric deadlock additionally requires the server's send-toward-client buffer to be full at error time (e.g. a NOTICE flood the client never reads, or a never-finishing client), which the cited triggers do not by themselves cause. The minimum actionable fix (delete/correct the false comment; optionally model COPY-IN as upstream does for early error surfacing) stands regardless. The unresolved disagreement between the COPY-1 and IO-1 verifier pairs is whether the worst case is a hang (critical/medium) or delayed error delivery (low); both pairs agree there is a real, fixable defect and a provably false comment. **Recommended posture: fix the comment and the latency divergence now; treat the deadlock as a real but conditional worst case to close when COPY-IN is hardened.**

#### REPL-1 — StandbyStatusUpdate reports received (not durably-flushed) LSN as `flush_lsn`
**Area:** Transactions / cancel / COPY / replication · **File:** `crates/compio-postgres/src/replication.rs:618-625, 638-646`
**Reference:** bespoke — ground truth `PostgreSQL walsender.c:2496-2502` (`flushPtr` drives `LogicalConfirmReceivedLocation`, advancing the slot's `confirmed_flush` and freeing WAL).

`advance_lsn` advances **both** `last_processed_lsn` and `last_received_lsn` from one value, and `send_standby_status_update` reports `max(processed, received)` into all three LSN slots — including `flush_lsn` (line 757). Per PG, `flush_lsn` is a durability promise that advances `confirmed_flush` and lets the server recycle WAL and advance catalog xmin. The consumer (`wal_consumer.rs:452,475`) calls `advance_lsn(wal_end)` on every non-commit `XLogData` and on keepalives — marking data merely seen on the wire / dispatched to in-memory broker events as flushed. Because the two pointers are folded and the reporter takes the max, `flush` can never lag `received`, so even a careful caller cannot truthfully report "received X but durably flushed Y < X."

**Impact:** Disputed — depends on whether a durable downstream sink exists. *If* a consumer persists dispatched changes and crashes after a StandbyStatusUpdate, the slot resumes from `confirmed_flush_lsn` past the lost changes → silently skipped change events.

**Fix:** Separate the pointers: `advance_lsn` advances only `last_processed_lsn`; track `last_received_lsn` from `wal_end` in `next()`. Report `write_lsn = last_received_lsn` but `flush_lsn = apply_lsn = last_processed_lsn`. Then advance only after a change is durably handed off (e.g. on Commit after broker ack), not on every inter-commit `wal_end`.

**Verify vote:** 1/2 upheld · **Confidence:** medium
**Severity note (contested):** This finding **did not survive cleanly** and is retained only as a precision/correctness caveat, not an actionable data-loss bug. The mechanical observation (flush conflated with received) is confirmed by both verifiers. However, the *only* consumer of the replication API is `plugin-db/wal_consumer.rs`, whose sole sink is the **ephemeral, at-most-once, thread-local broker** (`broker.rs`) — there is no durable change-event store, no on-disk log, no LSN checkpoint anywhere in the tree. One verifier downgraded to **low** (the data-loss impact assumes a durability contract the change-stream explicitly does not offer); the second **refuted** it outright to **info**, noting that source rows are durably committed by the writer's own transaction independent of the slot, that lost in-flight notifications are already handled by the broker's `Resync` overflow path, and that the suggested fix would report the identical LSN since no caller has a durable checkpoint to gate on. **Net: documentation/precision nit at current architecture; revisit to High only if a durable at-least-once CDC sink is ever added.**

### Medium

#### IO-3 — Pipelined requests are write-serialized: a second concurrent query is not sent until the first's response starts arriving
**Area:** I/O model & cancellation safety · **File:** `crates/compio-postgres/src/connection.rs:224-249`; `lib.rs:48-55`
**Reference:** `refs/rust-postgres/tokio-postgres/src/connection.rs:195-263` (poll_write drains the request channel and writes ALL queued requests back-to-back every poll, independent of poll_read).

While any response is outstanding, Step D **blocks** in `read_backend().await`; queued requests are drained (via `try_recv`) only *after* a server message arrives. With the documented pipelining pattern `join!(execute(A), execute(B))`, both futures push their `Request` onto the unbounded channel on first poll; the driver writes A in Step E, enters Step D, and blocks reading A's response — B is not written until A's first response frame arrives. Upstream's `poll_write` loops `start_send`-ing every queued request in one poll before any read, so A and B overlap on the wire. The crate copies the upstream pipelining doc verbatim (`lib.rs:48-55`), so this is a divergence from advertised behavior. FIFO ordering and correctness are preserved (A always completes and unblocks B; no deadlock).

**Impact:** The advertised pipelining optimization is largely defeated — B's send is delayed by ~one full A round-trip, collapsing batched-independent-query throughput toward the sequential case.

**Fix:** Before blocking on read in Step D, first drain all already-queued requests with a `try_recv` loop and write them, *then* read. Only the read-racing restriction is forced by cancel-safety; the write-before-read ordering is not, so a non-blocking drain-and-write placed before `read_backend().await` is a sound, faithful fix.

**Verify vote:** 1/1 upheld · **Confidence:** high
**Coverage note:** Untested — `concurrent_connections` uses 5 separate connections sequentially and `large_result_set` collects via `try_collect`, so no test exercises true single-connection pipelining.

#### POOL-2 — FIFO unfairness: fresh callers barge the idle slot ahead of parked waiters
**Area:** Connection pool (bespoke) · **File:** `crates/compio-postgres/src/pool.rs:380, 502, 505, 737`
**Reference:** bespoke — violates the production-pool fair-FIFO invariant (waiters served in arrival order; no waiter starved by later arrivals barging the idle slot — bb8/deadpool back this with an ordered semaphore/notify queue).

`return_client` pushes the freed entry into the shared idle vec and only then *advisorily* `wake_one_waiter()`. But `get_inner` step 1 unconditionally pops idle as its first action, with no "defer to existing waiters" gate. Sequence: pool full, waiter W parked; caller C returns a connection (push to idle + wake W); before W's task is polled, fresh caller N calls `get()`, pops the idle entry first, and takes it; W re-polls, finds idle empty and `total==max_size`, and re-parks at the tail. The hand-off is advisory, not direct — unlike bb8/deadpool, which hand the freed connection to the front waiter.

**Impact:** Tail-latency unfairness and possible indefinite starvation of a parked acquirer under sustained fresh-caller load, undermining the `connection_timeout` fairness guarantee. (Guaranteed starvation is scheduler-dependent — the compio run-queue ordering could not be pinned down — so the worst case is "possible," not proven; tail-latency unfairness is definite.)

**Fix:** Hand the returned/created connection directly to the front live waiter — stash the entry in a per-waiter handoff slot (or a reserved-for-waiter queue) and wake that waiter, which takes the reserved entry without re-entering the general pop path; push to idle only when no waiter is pending. Alternatively, gate `get_inner` step 1 so a fresh caller with non-empty waiters yields to the queue. Add a test that parks N waiters, returns N connections interleaved with fresh `get()` calls, and asserts arrival-order service.

**Verify vote:** 1/1 upheld · **Confidence:** high

#### BINCOPY-1 — Binary COPY header critical-flag mask is too broad (`0xFFFF_0000`), wrongly rejecting OID headers and making `has_oids` dead code
**Area:** Transactions / cancel / COPY / replication · **File:** `crates/compio-postgres/src/binary_copy.rs:165-174`
**Reference:** `refs/rust-postgres/tokio-postgres/src/binary_copy.rs:163-164` (parses `has_oids`, no flag rejection); ground truth `PostgreSQL copyfromparse.c:206-214`.

The port added header validation that cites the PG spec but uses the wrong mask: `if (flags as u32) & 0xFFFF_0000 != 0 { return …"critical flags set" }`, then `let has_oids = (flags & (1 << 16)) != 0`. PG's own reader handles bit 16 (the OID flag) separately, then *clears* it before checking the rest: the critical-flag space is bits 17..31 (`0xFFFE_0000`), not 16..31. With `0xFFFF_0000`, any header that sets bit 16 is rejected as "critical flags set" before reaching the `has_oids` line, so `has_oids` is permanently false and the `len += 1` path at lines 191-192 is dead. Upstream parses `has_oids` correctly and accepts OID-carrying input.

**Impact:** A binary COPY stream carrying the OID flag (pre-PG12 dumps, or any non-PG producer setting bit 16) is rejected with a misleading error instead of being parsed; the ported `has_oids` feature is silently disabled. Narrow blast radius (PG12+ never emits OIDs), but a real logic bug and fidelity regression vs both upstream and the cited spec.

**Fix:** Match PG: check `(flags & (1<<16)) != 0` for the OID case first, then validate critical flags with `(flags as u32) & 0xFFFE_0000 != 0`. Keep the `has_oids` assignment so the `len += 1` path stays live; or, if OID support is intentionally dropped, error explicitly on bit 16 and delete the dead `has_oids` code. (Note: modern PG's own reader also rejects bit 16 with a "WITH OIDS" error; the fidelity gap is specifically vs tokio-postgres, which accepts it.)

**Verify vote:** 1/1 upheld · **Confidence:** medium

### Low

#### IO-4 — Dead EOF-recovery condition in Step D (`self.responses.is_empty()` is always false there)
**Area:** I/O model & cancellation safety · **File:** `crates/compio-postgres/src/connection.rs:233-242`
**Reference:** `refs/rust-postgres/tokio-postgres/src/connection.rs:101` (poll_response `None` → `Error::closed()`: EOF mid-request is an error — matches).

Step D is entered only under `if !self.responses.is_empty()`. Its EOF handler is `if is_eof(&e) && self.responses.is_empty() { return Ok(()); }` — but `read_backend` does not touch `self.responses`, so the condition is necessarily false inside this branch and the `return Ok(())` is unreachable. The reachable behavior (return `Err(e)` on EOF mid-request) is correct and matches upstream's `Error::closed()`. The terminating-branch copy in Step C is live and correct because responses can drain across iterations there.

**Impact:** No behavioral bug. A dead, copy-pasted condition that misleads readers into thinking a clean mid-request shutdown is possible. Pure code-smell.

**Fix:** In Step D, drop the `&& self.responses.is_empty()` and just `return Err(e)` on any read error; keep the `is_eof` handling in Step C where it is reachable.

**Verify vote:** 1/1 upheld · **Confidence:** high

#### REPL-2 — IDENTIFY_SYSTEM DataRow parsed with unchecked indexing; malformed row panics the replication task
**Area:** Transactions / cancel / COPY / replication · **File:** `crates/compio-postgres/src/replication.rs:282-307`
**Reference:** bespoke — contrast with the bounds-checked `read_u8/u16/u32/u64` discipline in the same file (951-995) and the `body.len()` checks in `next()` (512, 548).

The DataRow field loop indexes the buffer with no length validation: `u16::from_be_bytes([buf[0], buf[1]])` with no `buf.len() >= 2` check, `i32::from_be_bytes([buf[idx..idx+4]])` unchecked, and `&buf[idx..end]` unchecked. Every other server-data parser in the file is defensive. A truncated or unexpectedly-shaped `IDENTIFY_SYSTEM` DataRow triggers an index-out-of-bounds panic. (Note: upstream's own DataRow consumer, `DataRowRanges::next`, is bounds-checked and returns `UnexpectedEof` — so this is *less* safe than upstream, not equivalent.)

**Impact:** A panic in the replication consumer task. The data is server-trusted post-auth, so practically low risk, but the task often runs detached (background) where a panic can be silent — a latent robustness gap inconsistent with the file's otherwise careful style.

**Fix:** Bounds-check before each index (mirror the `read_uN` helpers or the `next()` `body.len()` guards): verify `buf.len() >= 2` before the field count, `idx+4 <= buf.len()` before each length, and `end <= buf.len()` before the UTF-8 slice; on failure return `Error::parse` rather than panicking.

**Verify vote:** 1/1 upheld · **Confidence:** medium

### Info

#### REPL-3 — START_REPLICATION ErrorResponse is discarded; only a byte count is surfaced, not the DbError
**Area:** Transactions / cancel / COPY / replication · **File:** `crates/compio-postgres/src/replication.rs:380-390`
**Reference:** bespoke — contrast with `identify_system`, which maps `ErrorResponse → Error::db(body)` (replication.rs:319).

The `ERROR_RESPONSE_TAG` arm reassembles the full error frame into `body`, then returns `Error::io(…"START_REPLICATION ErrorResponse: {} bytes", body.len())` — reporting a *length*, not the message. The same file's `identify_system` correctly does `Message::ErrorResponse(body) => return Err(Error::db(body))`, and `DbError::parse` is available. Every upstream `ErrorResponse` site maps to `Error::db`.

**Impact:** When START_REPLICATION fails (missing slot/publication, insufficient privilege, wrong `proto_version` — all common setup errors), the caller gets "START_REPLICATION ErrorResponse: 87 bytes" with no diagnostic. Pure debuggability regression; no correctness or security impact.

**Fix:** Parse the `ErrorResponse` into a `DbError` (reuse the `identify_system` path / `DbError::parse`) and return `Error::db`, so the server's message/code/severity propagate.

**Verify vote:** 1/1 upheld · **Confidence:** high

## Per-area fidelity verdicts

| Area | Verdict | Confirmed / Refuted | Notes |
| --- | --- | --- | --- |
| I/O model & cancellation safety | Cancel-safety **correct & deliberate**; read/write **concurrency lost** | 4 / 0 | Codec/framing/64MB-cap/EOF faithful; serialization rewrite → IO-1/2/3 |
| Connection pool (bespoke) | Architecture sound; **accounting fails RAII** | 2 / 0 | Detached-task model + cancel-safe barrier are strengths; POOL-1 critical, POOL-2 unfairness |
| Query / prepare / pipelining / statements | **High fidelity, zero defects** | 0 / 1 | Near-verbatim; bespoke `query_text_params` + prepare cycle-detection all correct |
| Transactions / cancel / COPY / replication | Mostly high-fidelity; defects where io-model touched logic | 5 / 0 | Cancel-key/savepoint byte-correct; dirty-flag a real improvement; COPY/replication/bincopy bugs |
| Connect / config / TLS / auth | **High fidelity + 2 security improvements** | 0 / 0 | Conn-string/SCRAM/MD5/SSL byte-identical; channel-binding `Require` + fail-closed `target_session_attrs` |
| API surface / client / errors / types / row | **Faithful + improvements** | 0 / 2 | Response-routing core byte-identical; removes 2 upstream `.unwrap()` panics; no injection in `query_text_params` |

## Prioritized recommendations

1. **Fix POOL-1 before any production use (Critical).** Convert pool capacity accounting to RAII: a permit guard that decrements `total` on `Drop` unless disarmed after the connection is installed, covering both the on-demand-connect reservation and the popped-entry barrier paths. Add the cancellation regression test described above. This is the one defect that bricks the pool under normal operation and self-triggers via the pool's own timeout.
2. **Harden COPY-IN before exposing `copy_in`/`BinaryCopyInWriter` (COPY-1/IO-1).** At minimum, delete the false "non-blocking read between frames" comment and bound the flush with a write timeout *now* — the comment actively misleads and the latency divergence is real. Before any first-party caller adopts COPY FROM STDIN, restore read/write multiplexing by carrying a single in-progress `read_backend` future across loop iterations so a mid-COPY `ErrorResponse` is surfaced promptly and the worst-case deadlock is closed.
3. **Restore idle-read for LISTEN/NOTIFY (IO-2).** Gate an owned, cancel-safe socket read in Step E (via `poll_readable`/`PollFd`, not a `select!`-dropped read or `CancelToken`) so the documented `notifications()` API actually delivers. Either fix it or retract the "async notifications preserved" claim and the public API; do not ship a silently-dead documented feature.
4. **Restore pipelining write-before-read (IO-3).** Drain the request channel and write all queued requests before the blocking read in Step D. Low-risk, faithful to upstream, and recovers the advertised pipelining benefit. Add a true single-connection `join!`-pipelining test.
5. **Fix the FIFO hand-off (POOL-2).** Hand a freed connection directly to the front waiter (reserved-slot handoff) rather than racing it into the shared idle vec; add an arrival-order fairness test.
6. **Correct the binary-COPY flag mask (BINCOPY-1).** Use `0xFFFE_0000` and parse bit 16 first, keeping (or deliberately deleting) the `has_oids` path.
7. **Replication robustness/diagnostics (REPL-2, REPL-3).** Bounds-check the `IDENTIFY_SYSTEM` DataRow parse; parse the START_REPLICATION `ErrorResponse` into a `DbError`. Low effort, removes a panic and a debuggability black hole.
8. **Resolve the REPL-1 LSN-conflation question by decision, not code, for now.** It is a precision/doc nit at the current ephemeral-broker architecture. Separate write/flush/apply LSNs (and gate `advance_lsn` on durable hand-off) only if/when a durable at-least-once CDC sink is added — at which point this becomes High. Until then, fix the misleading "conservative" doc comment.
9. **Code-smell cleanup (IO-4).** Drop the dead `&& self.responses.is_empty()` in Step D.

## Coverage & method

**Method.** Each area was reviewed by diffing the compio port against its tokio-postgres ancestor file-by-file (`refs/rust-postgres/tokio-postgres/src/*`), and, for bespoke surfaces with no upstream (the pool, replication, `buf_stream`), against the relevant external ground truth — PostgreSQL server source (`walsender.c`, `copyfromparse.c`, `postgres.c`, `pqcomm.c`), the compio runtime/driver source (`refs/compio/*` for cancellation/timeout semantics), and deadpool/bb8 conventions for the pool. Every raw finding then passed an **adversarial verify gate**: independent verifier(s) re-read the cited lines, re-derived the upstream/contract reference, and voted `confirmed` / `severity-overstated` / `refuted`. Of 14 raw findings, **11 were upheld** (this report) and **3 refuted** and dropped (one each in query/prepare and two in API/client/errors). Severity-overstated votes are reflected as per-finding "Severity note" caveats rather than silently re-leveled, so the disagreements (notably COPY-1 vs IO-1, and REPL-1) are visible to the reader.

**Exclusions.** `error/sqlstate.rs` (generated from the PG catalog) and `test_utils.rs` (test-only harness) were excluded from line-level review as out of scope.

**Under-covered / needs live-PG.** Three of the most consequential findings sit on paths the existing test suite does not exercise, and verification was static (source-level) rather than runtime:
- **COPY FROM STDIN** has no integration test at all; the deadlock-vs-delayed-error disagreement between the COPY-1 and IO-1 verifier pairs is precisely the kind of question a live test against a real PostgreSQL backend (large COPY + mid-stream constraint violation, with and without a saturated server→client buffer) would settle definitively. This is the top live-PG testing gap.
- **LISTEN/NOTIFY** (IO-2) has zero tests, unlike upstream's `notifications()` test; a live pure-subscriber test would confirm total non-delivery.
- **Single-connection pipelining** (IO-3) is untested — `concurrent_connections` uses separate connections and `large_result_set` collects serially; a `join!`-on-one-connection latency test is needed.
- **Pool cancellation/fairness** (POOL-1, POOL-2): the existing `pool_exhaustion` test only drops at the Drop-guarded `Waiter` await, so neither the leak paths nor the barge-ahead ordering is covered; both need the regression tests specified in their fixes.
- **REPL-1's** real-world severity is gated entirely on downstream architecture (presence/absence of a durable CDC sink), which is a design question, not a test.

The connect/config/TLS/auth and API/client/types areas are well-covered and yielded zero upheld findings; the pool acquire path and the COPY/notify/pipelining I/O paths are the areas where confidence is bounded by the absence of live-PG runtime tests.