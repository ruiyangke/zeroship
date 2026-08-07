# Worker Crate - Correctness + Stability Review (2026-08-06)

Scope: `crates/worker/src/**`, read-only static audit, cross-checked into
`crates/runtime`, `crates/metering`, and `crates/bundle` where the worker's
behaviour depends on them.

Focus: isolate lifecycle (LRU eviction, enter/exit, use-after-evict),
cancellation mid-dispatch, bundle cache coherence across redeploy,
exactly-once emission of the five platform counters, and reachable panics.

Findings carry a concrete failure scenario and the assertion a regression test
would make. Except where a triage note says otherwise, they are NOT independently
verified - triage before acting.

## Triage notes

**Finding 1 (CRITICAL, cross-isolate V8 handle during eviction): MECHANISM
CONFIRMED by reading the code.** Every link holds:

- `crates/runtime/src/rpc/abort.rs:62-65` declares `REGISTRY` inside
  `thread_local!`, so one map is shared by every isolate on the worker thread.
- `abort.rs:68-71` keys it `RegistryKey { app_id, request_id }`. There is no
  isolate identity in the key.
- `crates/worker/src/cache.rs:456,479-484` (`load_pinned_workflow_app`,
  `max_pinned_isolates_per_app`, `pinned_count_for_app`) confirm that several
  isolates for the SAME app coexist on one thread, which `AGENTS.md` also states
  as an invariant: one isolate per (app, live deploy) plus pinned workflow
  isolates per app.
- `cache.rs:695-697` evicts by calling `entry.runtime.with_scope(...)` into
  `abort::entered_for_eviction(scope, oldest_id)`, and `abort.rs:146-160` selects
  every entry matching `k.app_id == app_id`, then `abort.rs:172` opens each stored
  `v8::Global<v8::Object>` with `v8::Local::new(scope, ...)` using the EVICTING
  isolate's scope.

So evicting one isolate of an app opens V8 globals owned by that app's OTHER
isolates under the wrong isolate. That is a cross-isolate handle use, which is
undefined behaviour in V8 rather than a recoverable error.

Not yet reproduced by a test - the mechanism is confirmed, the reachability under
real traffic is not. A reproducer needs an app with both a live and a pinned
workflow isolate on one thread, an in-flight RPC registered in the live one, and
eviction of the pinned one.

**Finding 15 (HIGH, live deploy to no deploy leaves stale code cached): CONFIRMED
and FIXED.** `needs_reload` compared with `is_some_and`, so a remote hash of `None`
read as unchanged and an undeployed app kept serving cached code while its limits,
env version and net policy held steady. A regression test was written first and
failed on the pre-fix code. Fixed by comparing the two hashes directly.

**Finding 2 (CRITICAL, unbounded streaming bridge): MECHANISM CONFIRMED, and this
one needs no unusual configuration to reach.** The runtime accounts stream bytes
against two caps - per-stream `DEFAULT_STREAM_BUFFER_CAP` = 4 MiB
(`crates/runtime/src/core/channel.rs:89`) and process-wide
`DEFAULT_STREAM_GLOBAL_CAP` = 512 MiB (`channel.rs:99`).

`StreamReader::pop` (`channel.rs:288-296`) removes a chunk and **decrements both
counters**: `inner.buffered_bytes -= n` and
`STREAM_GLOBAL_BUFFERED.fetch_sub(n, ...)`.

The worker's drain loop (`crates/worker/src/handler.rs:1168-1173`) pops in a tight
`while let Some(chunk) = reader.pop()` and forwards each chunk with
`tx.send(...)`, where `tx` comes from `ntex::channel::mpsc::channel()`
(`handler.rs:1129`). That constructor is documented by ntex itself as creating
"a unbounded in-memory channel with buffered storage", backed by a `VecDeque`
with no capacity, and its `send` is synchronous - it can never report fullness,
so there is no backpressure signal to propagate.

Net effect: bytes are laundered out of both caps into a queue bounded by nothing.
A client that reads more slowly than the app produces grows resident memory
without limit and without appearing in either accounting figure. Unlike finding 1
this needs no pinned-isolate configuration - an ordinary slow consumer on a
streaming endpoint is enough.

Reachability still unproven by test. A reproducer produces chunks continuously
while never polling the response body, and asserts queued bytes stay bounded.

---

1. **CRITICAL — eviction can use a V8 handle in the wrong isolate.**  
   Evidence: [cache.rs:695](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:695), [cache.rs:755](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:755), [abort.rs:146](/home/ruiyang/Projects/appbase/crates/runtime/src/rpc/abort.rs:146), [abort.rs:170](/home/ruiyang/Projects/appbase/crates/runtime/src/rpc/abort.rs:170).  
   Failure: a live isolate has a pending RPC, while a pinned isolate for the same app is evicted. The thread-local abort registry selects every controller by `app_id`, then opens the live isolate’s `Global` using the pinned isolate’s scope. Rusty V8 can assert/panic or crash; if unwinding is caught, eviction exits before balancing enter depth or removing the cache entry.  
   Regression: create live and pinned runtimes for one app, register a pending RPC in the live runtime, evict the pinned runtime, and assert no panic, only victim-owned controllers are touched, and both enter depths remain balanced.

2. **CRITICAL — the HTTP streaming bridge is unbounded.**  
   Evidence: [handler.rs:1129](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:1129), [handler.rs:1168](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:1168), [channel.rs:84](/home/ruiyang/Projects/appbase/crates/runtime/src/core/channel.rs:84).  
   Failure: an app emits indefinitely while the client reads slowly. The drain pops bytes out of the runtime’s capped buffer and synchronously sends them into ntex’s unbounded `VecDeque`, removing them from both the 4 MiB per-stream and 512 MiB global accounting. RSS grows until the worker process OOMs.  
   Regression: never poll the response body while continuously producing chunks; assert queued bytes remain bounded and the producer observes backpressure.

3. **HIGH — eviction runs attacker-controlled abort listeners without any CPU or wall guard.**  
   Evidence: [cache.rs:695](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:695), [cache.rs:755](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:755).  
   Failure: an RPC installs `signal.addEventListener("abort", () => { while (true) {} })`. LRU eviction invokes `abort()` synchronously after the request CPU timer is disarmed, permanently wedging that worker thread.  
   Regression: evict such a request under a watchdog; assert eviction terminates within an infrastructure-owned limit and the thread serves another request.

4. **HIGH — production dispatches never acquire the isolate leases used by eviction.**  
   Evidence: [handler.rs:230](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:230), [handler.rs:531](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:531), [cache.rs:658](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:658), [cache.rs:721](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:721).  
   Failure: A has a never-settling request; loading B at capacity evicts A because it appears unleased. The handler’s `Runtime` clone keeps A alive outside the cache. Repeating this bypasses both normal and pinned isolate budgets and grows orphan isolates indefinitely.  
   Regression: with capacity one, hold A pending and load B; assert A is protected until completion or forcibly terminated, and total live `RuntimeInner` instances never exceed the configured bound.

5. **HIGH — replacement and authoritative eviction do not tear down active old runtimes.**  
   Evidence: [cache.rs:443](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:443), [cache.rs:568](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:568), [sync.rs:383](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:383).  
   Failure: an old request captures a secret or raw-TCP grant and waits. Env/network policy is revoked or the app is deleted; replacement/removal drops only the cache handle. The pending clone retains old code, sockets, policy, and pump and can perform the side effect after revocation.  
   Regression: retain a pending old runtime with a socket, rotate to denied policy or delete the app, then release it; assert cancellation, zero sockets, no side effect, and destruction of the old runtime.

6. **HIGH — client disconnect drops the handler without cancelling or metering V8 work.**  
   Evidence: [handler.rs:120](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:120), [handler.rs:326](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:326), [channel.rs:31](/home/ruiyang/Projects/appbase/crates/runtime/src/core/channel.rs:31).  
   Failure: ntex drops the service future when the peer disappears. Neither `recv_with_timeout`, `ResultReceiver`, nor `CancelFlag` has drop cancellation, so a never-settling promise, timers, operations, auth state, and logs survive indefinitely; none of the five platform counters are recorded.  
   Regression: disconnect a real socket after `Pending`; assert cancellation is observed, pending/request-owned state clears, and exactly one five-counter record is emitted.

7. **HIGH — timeout cleanup allows post-timeout JavaScript continuations and mutations.**  
   Evidence: [handler.rs:139](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:139), [runtime.rs:2432](/home/ruiyang/Projects/appbase/crates/runtime/src/core/runtime.rs:2432), [runtime.rs:3210](/home/ruiyang/Projects/appbase/crates/runtime/src/core/runtime.rs:3210).  
   Failure: `await slowRead(); await env.kv.set(...)` times out during the read. The pending request is removed, but the native resolver survives. When the read completes, its promise is resolved with no cancellation flag and the continuation performs the write after the 504.  
   Regression: delay a native read past the wall limit, receive 504, then release it; assert no subsequent native operation or side effect runs.

8. **HIGH — CPU-limit errors leave request-owned timers/state in the cached isolate.**  
   Evidence: [runtime.rs:2447](/home/ruiyang/Projects/appbase/crates/runtime/src/core/runtime.rs:2447), [runtime.rs:2807](/home/ruiyang/Projects/appbase/crates/runtime/src/core/runtime.rs:2807), [runtime.rs:3048](/home/ruiyang/Projects/appbase/crates/runtime/src/core/runtime.rs:3048).  
   Failure: app code schedules a sibling timer, awaits, then exceeds CPU while resuming. Termination paths remove/send the pending error but do not consistently discard request maps or call `drop_timers_owned_by`; the sibling callback later executes in the reusable cached isolate.  
   Regression: exceed CPU after scheduling a timer; after the error, assert all request maps/timers are gone and the callback never executes.

9. **HIGH — idle streams never observe disconnect or writer destruction.**  
   Evidence: [handler.rs:1171](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:1171), [handler.rs:1193](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:1193), [channel.rs:172](/home/ruiyang/Projects/appbase/crates/runtime/src/core/channel.rs:172).  
   Failure: disconnect is checked only during `tx.send`. An open stream emitting no chunks therefore retains its detached drain forever and bills `stream_wall_us` every ten seconds. Evicting its runtime has the same result because dropping `StreamWriter` does not mark the channel done.  
   Regression: disconnect from and separately evict an idle stream; assert the drain exits, state is freed, and stream billing stops.

10. **HIGH — concurrent cold loads can permanently remove the winning runtime’s env.**  
    Evidence: [handler.rs:220](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:220), [handler.rs:3040](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:3040), [handler.rs:3054](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:3054).  
    Failure: two cache-miss requests load independently. A succeeds and caches the runtime; B later fails descriptor/runtime initialization and blindly removes the shared env. Later requests see the runtime, skip loading, and return 503 indefinitely because reconciliation neither sees an env key nor detects stale loaded metadata.  
    Regression: race one successful and one failing cold load; a third request must remain 200 with the successful env intact.

11. **HIGH — stale load work can overwrite a newer deploy.**  
    Evidence: [handler.rs:2996](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:2996), [handler.rs:3044](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:3044), [sync.rs:175](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:175), [sync.rs:342](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:342).  
    Failure: a v2 cold load stalls fetching a blob. v3 is published and loaded, then v2 resumes and unconditionally replaces the cache and metadata. A security-fixed deploy is rolled back until another reconciliation.  
    Regression: barrier v2, load v3, release v2; assert runtime behavior and `LoadedMeta` remain v3.

12. **HIGH — shared env updates are not monotonic.**  
    Evidence: [sync.rs:248](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:248), [sync.rs:440](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:440).  
    Failure: one worker fetches env v2 slowly; another stores v3. The delayed v2 writer blindly replaces v3, temporarily resurrecting revoked plaintext credentials and exposing them to new isolate loads.  
    Regression: store v3, then complete a delayed v2 insertion; assert both cached version and payload stay at v3.

13. **HIGH — pinned workflow isolates have no process/thread-wide bound and pinned-only apps evade deletion GC.**  
    Evidence: [cache.rs:20](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:20), [cache.rs:483](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:483), [cache.rs:601](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:601).  
    Failure: one pinned deploy for each of N apps grows `workflow_isolates` to N regardless of `max_isolates`; only the per-app count is checked. `all_app_ids()` excludes pinned entries, so pinned-only deleted apps are never reconciled away.  
    Regression: load pinned runtimes for many apps and delete pinned-only apps; assert a total bound and complete reclamation.

14. **HIGH — pinned runtimes never receive same-deploy security-policy changes.**  
    Evidence: [handler.rs:519](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:519), [cache.rs:324](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:324), [cache.rs:601](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:601).  
    Failure: the pinned cache key contains only app and deploy hash. On a cache hit, current runtime limits, env version, and network policy are not fetched or compared, while normal reconciliation ignores pinned entries. A raw-TCP allow changed to deny remains allowed for subsequent workflow replays.  
    Regression: rotate net policy without changing deploy hash; the next replay must use a rebuilt runtime and reject the connection.

15. **HIGH — transition from a live deploy to no deploy leaves stale code cached.**  
    Evidence: [sync.rs:220](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:220), [sync.rs:271](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:271).  
    Failure: loaded hash `h1` compared with `deploy_hash=None` yields `hash_changed=false`. If limits/env/policy are unchanged, reconciliation never enters the branch that notices missing worker code, so the undeployed app remains executable indefinitely.  
    Regression: reconcile loaded `h1` against a same-policy record with no deploy/manifest; assert immediate eviction.

16. **HIGH — `cpu_us` omits all CPU consumed after the first `await`.**  
    Evidence: [handler.rs:281](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:281), [handler.rs:304](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:304), [runtime.rs:2945](/home/ruiyang/Projects/appbase/crates/runtime/src/core/runtime.rs:2945), [runtime.rs:2278](/home/ruiyang/Projects/appbase/crates/runtime/src/core/runtime.rs:2278).  
    Failure: the worker samples only synchronous entry. The runtime accumulates pump CPU, but discards it when building the settled result. Moving a 200 ms computation behind a zero-delay timer produces near-zero billed CPU.  
    Regression: burn deterministic CPU after an await in HTTP and workflow dispatch; assert the single request record includes the asynchronous CPU.

17. **HIGH — graceful shutdown loses recently recorded billing counters.**  
    Evidence: [main.rs:624](/home/ruiyang/Projects/appbase/crates/worker/src/main.rs:624), [main.rs:722](/home/ruiyang/Projects/appbase/crates/worker/src/main.rs:722), [outbox.rs:498](/home/ruiyang/Projects/appbase/crates/metering/src/outbox.rs:498).  
    Failure: the detached outbox sleeps before draining `Meter`; after server drain, main returns without a final drain, WAL append, or task join. All completed dispatch counters since the last interval disappear on normal SIGTERM.  
    Regression: record a request with a long outbox interval, gracefully stop before the tick, restart, and assert all five counters are durably present exactly once.

18. **MEDIUM — wall timeout excludes synchronous execution and grants the full limit again after it.**  
    Evidence: [handler.rs:244](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:244), [handler.rs:327](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:327).  
    Failure: a 40 ms synchronous turn followed by a 40 ms await succeeds under a 50 ms wall limit because the 50 ms timer starts only after the first turn; a fully synchronous 80 ms response is never wall-checked.  
    Regression: configure 50 ms wall/high CPU, spend 40 ms before and after an await, and assert timeout near 50 ms.

19. **MEDIUM — generated error response bytes are always metered as zero.**  
    Evidence: [handler.rs:323](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:323), [handler.rs:353](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:353), [handler.rs:357](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:357), [handler.rs:1223](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:1223).  
    Failure: rejection, timeout, and unsupported-upgrade arms call `record(0)` and then return non-empty JSON, undercounting `egress_bytes` on every such dispatch.  
    Regression: force each error, read its body, and assert `egress_bytes == body.len()` with `requests == 1`.

20. **MEDIUM — the cached-runtime/missing-env error emits none of the five counters.**  
    Evidence: [handler.rs:238](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:238), [handler.rs:258](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:258).  
    Failure: after `DISPATCH_TOTAL` increments and wall timing starts, missing env returns 503 before any recorder exists. The cold-load race above makes this reachable; every retry is entirely absent from billing.  
    Regression: dispatch with a cached runtime but missing env; assert one request/ingress/wall/cpu/egress record for the 503.

21. **MEDIUM — reconciliation destroys LRU recency.**  
    Evidence: [cache.rs:307](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:307), [cache.rs:320](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:320), [sync.rs:270](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:270).  
    Failure: `get_limits()` calls `get_runtime()`, which updates `last_used`. Every reconcile marks every app recently used in arbitrary `HashMap` order, so the next load can evict a hot app while retaining an idle one.  
    Regression: seed known A/B recency, perform reconcile-style metadata reads, force eviction, and assert the genuinely idle app remains the victim.

22. **MEDIUM — the all-app control snapshot is unbounded and deep-copied per worker thread.**  
    Evidence: [sync.rs:175](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:175), [sync.rs:505](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:505), [sync.rs:543](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:543).  
    Failure: `/internal/versions` is fully buffered without a byte cap, parsed with every manifest, then deeply cloned by every ntex worker each cycle. A large legitimate catalog can temporarily require the body, parsed map, and N full manifest copies and exhaust memory.  
    Regression: reject an oversized response and assert workers reconcile through a shared immutable snapshot rather than deep clones.

23. **MEDIUM — auxiliary app caches bypass the isolate bound.**  
    Evidence: [sync.rs:97](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:97), [sync.rs:142](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:142), [logs.rs:12](/home/ruiyang/Projects/appbase/crates/worker/src/logs.rs:12), [logs.rs:27](/home/ruiyang/Projects/appbase/crates/worker/src/logs.rs:27).  
    Failure: `SharedEnvs` retains every active app despite LRU eviction; `SharedLogs` has only a per-app line count and never removes deleted/evicted apps. Sequential traffic and app churn grow plaintext env/log memory independently of `max_isolates`.  
    Regression: churn beyond the isolate cap and delete logged apps; assert global app/byte budgets and deletion cleanup.

24. **MEDIUM — `poll_interval=0` panics every reconcile task and creates a tight control loop.**  
    Evidence: [main.rs:79](/home/ruiyang/Projects/appbase/crates/worker/src/main.rs:79), [sync.rs:131](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:131), [sync.rs:168](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:168).  
    Failure: zero is accepted; each per-thread loop evaluates modulo zero and panics, while the process-wide poller sleeps zero and continuously calls control. The server stays healthy-looking while deploy/env/policy reconciliation is dead.  
    Regression: CLI/config parsing must reject zero; every accepted interval must avoid panic and tight polling.

25. **MEDIUM — documented infinite shutdown timeout actually means immediate forced stop.**  
    Evidence: [main.rs:250](/home/ruiyang/Projects/appbase/crates/worker/src/main.rs:250), [main.rs:660](/home/ruiyang/Projects/appbase/crates/worker/src/main.rs:660), [main.rs:709](/home/ruiyang/Projects/appbase/crates/worker/src/main.rs:709).  
    Failure: worker docs define zero as “wait forever,” but ntex interprets `Seconds(0)` as no graceful wait. SIGTERM immediately drops pending dispatches, triggering the cancellation and lost-metering defects above.  
    Regression: start a pending request with timeout zero, signal shutdown, and assert the server waits for completion rather than force-dropping it.

## Checked and found sound

- Cache and loaded metadata are thread-local, matching V8/`Rc` thread affinity: [cache.rs:55](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:55).

- Rust use-after-free does not occur on cache removal: in-flight `Runtime` clones retain `RuntimeInner`, and the pump itself holds only a weak back-reference. The defect is unbounded orphan lifetime, reported above.

- New runtimes fully initialize before cache mutation; failed initialization preserves the last-good isolate, and the pump starts only after capacity is secured: [cache.rs:413](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:413), [cache.rs:427](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:427), [cache.rs:440](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:440).

- Ordinary HTTP and workflow synchronous entry/exit pairs contain no `await`; JS throws are converted into outcomes rather than unwinding Rust: [handler.rs:283](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:283), [handler.rs:576](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:576).

- LRU selection correctly excludes an explicitly leased runtime, defers when all candidates are leased, prefers socketless victims, and closes native sockets before removal. Existing tests cover those mechanics; production simply never takes the lease: [cache.rs:658](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:658), [cache.rs:677](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:677).

- Explicit wall timeout does set `CancelFlag` and wake the pump; its remaining native-resolver gap is finding 7: [handler.rs:139](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:139).

- Workflow heartbeat cleanup is RAII and runs on normal return, error, timeout, or dropped handler: [handler.rs:648](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:648), [handler.rs:659](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:659).

- Partial or corrupt production bundles are not installed: blob reads validate hash format and SHA-256, and bundle/env/descriptor/runtime initialization completes before cache mutation: [blob.rs:218](/home/ruiyang/Projects/appbase/crates/bundle/src/blob.rs:218), [s3_blob.rs:393](/home/ruiyang/Projects/appbase/crates/bundle/src/s3_blob.rs:393), [sync.rs:293](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:293).

- Dispatch bodies are capped before V8 entry, and the runtime’s direct stream buffers have per-stream and global caps. Finding 2 is specifically the unbounded downstream ntex bridge: [handler.rs:214](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:214).

- Buffered successful responses record the five counters exactly once with the actual body size. Streams record unary request/CPU/ingress once and only egress/wall deltas afterward, so ordinary stream completion does not double-count requests: [handler.rs:309](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:309), [handler.rs:1083](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:1083).

- No request/control-payload-reachable `unwrap` was found in worker code. Malformed UUIDs, frames, bodies, statuses, headers, UTF-8, manifests, descriptors, and bundle source return errors or safe fallbacks. Cache unwraps depend on per-thread initialization performed before route registration.

- No standard `RwLock` guard is held across an `await`; metrics use process-safe atomics; logs correctly enforce 1,000 retained lines per individual app. `lib.rs` contains only module exports.

This was a static, read-only audit; no files were modified.
