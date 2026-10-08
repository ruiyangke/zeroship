# A runaway isolate costs its creator and stays in its isolate

**Status.** PROPOSED. Nothing in this document is implemented. Preventing every runaway is not the
goal. The design serves two goals:

- **Goal 1: a runaway costs its creator.** Every piece of code that runs on a creator's isolate is
  billed as CPU, including work that never finishes: a request that spins forever, a workflow
  execution, module evaluation at startup, the pump's continuations, eviction listeners. The bill is
  measured by the platform in the process that runs the code, and it is bounded so one bug cannot run
  up an unbounded charge.
- **Goal 2: a runaway's impact stays in its isolate.** One app's runaway costs that app its isolate,
  never the whole worker: not the co-tenants sharing its thread, and not the process.

The tenant boundary (schema binding, the binding role, execution-zone placement) is not what this
changes and is not weakened here.

---

## What is true today

- **Many tenants share a process.** One isolate per (app, live deploy) serves requests. A worker
  thread enters and leaves isolates so it can serve many apps, and a worker process runs several such
  threads, each with its own isolate cache (`crates/zeroship-worker/src/cache.rs`). No app is pinned
  to a thread: a request loads its app on whichever thread receives it.
- **Entries are manual pairs.** The runtime enters and leaves an isolate with a pair of calls
  (`crates/zeroship-runtime/src/core/runtime.rs`). A panic between them leaves the isolate entered,
  and the thread keeps serving, because compio catches a panicking task at the task boundary.
- **CPU billing covers two kinds of entry.** The meter keeps in-memory counters per app and drains
  them on an interval into a worker-local WAL that survives a restart
  (`crates/zeroship-metering/src/meter.rs`, `crates/zeroship-metering/src/outbox.rs`):

  | Entry into a creator isolate | Metered today |
  |---|---|
  | Request dispatch | yes, once the call returns |
  | Pump windows: timers, microtasks, async continuations, subscriptions, WebSocket events, the cancellation sweep | yes |
  | Module evaluation at startup | no |
  | Workflow execution: startup and the synchronous dispatch | no (its pump windows are metered) |
  | Eviction abort listeners | no |
  | The cancellation sweep at quarantine | no |
  | Idle and stop-time garbage collection | no |

  So a request that never returns is never billed, a workflow execution's synchronous CPU is never
  billed, and a process crash loses every app's undrained counters.
- **A spend cap exists.** Control derives each app's spend state from period spend
  (`crates/zeroship-control/src/spend.rs`), and the gateway refuses an app in the `Block` state.
- **Limits rest on termination.** Every isolate has a heap cap whose callback requests termination
  and grants bounded unwind headroom. The CPU limit is a one-shot timer, and its watchdog thread calls
  `terminate_execution` (`crates/zeroship-runtime/src/core/cpu_timer.rs`). That watchdog exists only
  once some isolate with a CPU limit has registered a timer. The `unlimited` plan has neither a CPU
  limit nor a wall timeout. No embedder OOM handler is installed, and memory outside the V8 heap is
  uncapped.
- **A wedged thread stays in rotation.** ntex hands each connection to the next worker thread in
  turn, and only a worker's own thread can mark itself failed, because ntex-server keeps the
  availability state private. So a thread that never returns keeps receiving connections. `healthz`
  is a constant success, the gateway sends each request to one worker with no failover, and the
  compose worker service has no restart policy.

## How a runaway escapes its isolate

V8 (pinned by the `v8` crate in `Cargo.lock`) honors a termination request only at interrupt checks,
which compiled code places at function entry and at loop back-edges. Take a loop whose body builds a
constant-size array, with a length between two of TurboFan's own limits: above the size it
initializes inline, and below the iteration count under which it drops a counted loop's stack check.
TurboFan lowers that initialization into an inner counted loop with no check of its own. Its
loop-unrolling pass then drops the enclosing loop's back-edge check as well. The loop never observes
termination, and re-requesting it does not help. Ordinary library code can trip this, with no malice
needed. `--no-turboshaft-loop-unrolling` restores the check; `--max-opt=2`, `--no-turbofan` and
`--jitless` do too, at higher cost. The regression tests reproduce the escaping shape and its
controls: shorter and longer constant lengths, a non-constant length, and typed arrays are all
terminated under default flags.

The escape fails two ways:

- **Hang.** A loop that drops what it allocates spins on its thread forever and ignores the CPU
  limit. On the `unlimited` plan nothing requests termination at all, so even a plain `for(;;){}`
  wedges the thread. Everything else on that thread stops with it.
- **Out of memory.** A loop that keeps what it allocates fills the heap. Once the bounded grants run
  out, V8 ends the process, and a handler cannot return from that. **In a shared process an
  out-of-memory cannot be contained after the fact.** Termination has to land before the heap is
  exhausted, or there has to be a process boundary around the isolates likely to do it.

Host code wedges a thread with no compiler fault at all. Termination is observed only when control
returns to JavaScript, so a host callback that loops on a count the creator chooses holds the thread
until it finishes. PBKDF2 is one: its synchronous, callback and WebCrypto forms all run on the
isolate's thread (`crates/zeroship-runtime/src/node/crypto/kdf.rs`,
`crates/zeroship-runtime/src/web/crypto/derive.rs`).

## Goal 1: a runaway costs its creator

**The billing unit is thread CPU while a creator isolate is entered**, charged to that isolate's app.
It includes garbage collection the app's thread runs while entered. V8's background threads run
outside any entry and stay unattributed. Each entry records its kind:

- **Creator** entries run creator code: dispatch, startup, the pump, eviction listeners and the
  quarantine sweep.
- **Housekeeping** entries are platform-initiated: idle collection and collection at a stop.

Both kinds bill `cpu_us` until the operator rules on housekeeping (open question 8).

**The mechanism (change 1):**

- **One scoped entry.** Every creator entry goes through one scoped call. That call leaves the
  isolate and closes the thread's entry both when it returns and when it unwinds, so a caught panic
  leaves nothing entered. Two other entries run no creator code and are not creator entries: the
  one V8 makes when it creates an isolate, which the worker leaves at once, and the one made before
  disposal.
- **One isolate per thread.** No production path enters a second isolate while one is entered, and
  the scoped entry enforces this. Re-entering the same isolate deepens the entry. Entering another
  isolate while one is open fails an assertion before V8 is entered, so attribution can never pool
  two apps.
- **The entry slot.** Each thread that hosts entries publishes its open entry (app, meter, start
  time, kind) in a process-wide, lock-free slot. Three parties can charge an entry's CPU: the
  sampler, the closing owner and the crash handler. Each slice is charged exactly once, by whichever
  of them claims it with a compare-and-swap.
- **A foreign clock is read only while its owner is alive.** The sampler reads another thread's CPU
  clock, and that clock id names a TID:
  - After the thread exits, a read fails.
  - Once the kernel reuses the TID for a new thread of the same process, a read silently returns
    that thread's CPU. That would be a misattribution.

  So the owner's thread-exit hook marks its slot closing and waits for in-flight readers, and a
  reader announces itself before it checks the slot. A thread exits only after its exit hooks have
  run, so every read names a live owner. This holds after a panic and on a detached thread.

  A pass holds a slot only for its clock read, its charge and the wedged thread's demotion
  (change 5). The hang record and the abandon handle run after the pass lets go, so a slow volume
  cannot hold up a thread's exit. Reading `/proc`, using thread pidfds, or having each thread publish
  its own reading would all need the same handshake to reuse slots safely, so the direct read is
  used.
- **The sampler** runs on the watchdog thread and samples open entries on a cadence (open question
  1), so an entry that never returns is billed while it runs. The worker starts the watchdog at boot
  and refuses to boot without it, since billing now depends on it.
- **One mechanism replaces two.** The per-call-site CPU sampling around request dispatch and the
  pump's own CPU billing are retired. Request recording keeps requests, wall time and bytes. Nothing
  is counted twice, and no call site has to remember to record.

**Workflow executions** enter their isolates through the same scoped entry on the worker's
workflow-host thread. They bill the same app-keyed `cpu_us` from the process that runs the code,
including their startup and synchronous dispatch. Whether workflow CPU is priced like request CPU is
open question 7.

**Durability.** A crash loses at most the undrained counters, one outbox interval. The in-flight
slices are not lost: the OOM handler claims every open entry on every thread and writes it to a crash
file. The next boot ships those records as usage events whose id and source stay stable across
re-ships (change 2). A hang is not a crash: the sampler keeps charging the wedged entry until the
thread is abandoned.

**Bounding the bill.** Cadence billing gets a runaway's CPU to the spend engine within one outbox
interval plus the next recompute, and `Block` stops new requests at the gateway. An entry already
running is bounded separately: abandonment (change 5) ends its billing, so each runaway entry costs at
most the plan-independent entry ceiling plus the termination grace, times one core. On the free plan
the CPU limit stops the runaway once change 3 makes termination land, and the ceiling stops whatever
still escapes.

**Metering stays infrastructure.** The CPU comes from the kernel's per-thread clock in the process
running the code, and the app is the one the isolate was built for. Creator code can neither forge
nor suppress either.

## Goal 2: a runaway's impact stays in its isolate

**Oversized buffers fail inside the app.** A per-isolate external-memory budget, enforced by an
allocator that refuses before allocating, turns an oversized buffer into a `RangeError` in the app's
own code (change 4).

**Termination lands (change 3).**

- *Compiled loops.* The loop-unrolling flag lets the CPU limit and the heap cap stop the proven
  shape.
- *Host loops.* A host loop whose count the creator chooses runs in bounded chunks and checks the
  isolate's termination request between chunks. Both the CPU limit and the ceiling raise that
  request, so either one ends the call within one chunk. The check reads the runtime's own
  termination flag: inside a host callback V8's own "is terminating" answer does not turn true after
  a termination requested from another thread, so it cannot be the signal.
- *Why chunking.*
  - A cap on the count low enough to keep one call short breaks legitimate high-cost password
    hashing, and still holds the thread for each capped call.
  - Running the loop off the isolate thread moves its CPU outside the entry, so the entry does not
    bill it; and the synchronous form must block the thread anyway.
  - Chunking keeps the work on the entry's thread, billed, behind an unchanged API.
- *What cannot be split.* A primitive the host cannot split gets caps on the parameters the creator
  chooses, so that one call stays bounded. scrypt is the case today: it is one library call, and
  the creator chooses both its cost and its memory limit.

**A hang costs its thread, not the process (change 5).** When the watchdog finds an entry open past a
plan-independent ceiling, it requests termination. If that request is still unobserved after a
grace, it abandons the thread:

- **Stop routing to it.**
  - *Why it needs a patch.* A wedged thread runs no futures, so nothing on it can re-poll readiness,
    drop the handle whose drop marks it failed, or reach an arbiter stop hook. No readiness gate in
    the worker's own service can retire it; only another thread changing the availability state that
    ntex's manager reads can.
  - *Why upgrading does not help.* Every published ntex-server keeps that state private, and sets
    the failure only from the worker's own thread: 3.9.0, which the lock pins; 3.11.2, the latest
    on the ntex 3 line; and 4.2.0, the latest, which belongs to the pre-release ntex 4 line.
  - *What the design does.* It vendors a patch that lets another thread mark a worker failed, and
    offers that handle upstream. The manager then drops the worker from rotation and starts a
    replacement thread with a fresh cache.
  - *The fallback.* The alternative that needs no patch is one single-worker ntex server per thread
    on its own `SO_REUSEPORT` listener, stopped to close that listener. It trades round-robin
    hand-off for the kernel's hashing, and it is the fallback if the patch's upkeep is refused (open
    question 13).
- **Co-tenants cold-start.** The next request for any app that lived on the wedged thread loads on a
  healthy thread. The wedged thread's in-flight requests are lost, and so are requests on gateway
  keep-alive connections it owns; both end at the gateway's worker timeout.
- **The spinner is demoted, and its bill stops.** The watchdog moves the wedged thread to the idle
  scheduling class. A process may do that to its own threads without privilege. Without a CPU quota
  the thread then runs only on CPU no other thread wants. Billing of the abandoned entry ends at
  abandonment; whatever CPU the thread spends afterwards is a platform cost, bounded by the recycle
  threshold.
- **Under a CPU quota, demotion is not enough.** A container quota is shared by every thread in the
  process, and an idle-class spinner still spends it, so the healthy threads are throttled. The
  sampler keeps measuring an abandoned thread's CPU (as platform cost, not billed), and the recycle
  threshold counts that CPU: a worker whose abandoned threads keep spending quota recycles, rather
  than serving throttled. Deployments without a CPU quota rely on demotion alone.
- **Its isolates are leaked, not rebuilt.** They are thread-bound, so no other thread may drop them,
  and they stay charged to the process budget (change 9). The abandonment closes the thread's
  connection channel. ntex's worker loop stops once its channel closes, so a thread that ever returns
  winds down and disposes its isolates.
- **The process keeps serving.** It recycles only once an operator-set threshold is passed, on the
  count of abandoned threads or the bytes their isolates hold (open question 4). A recycle drains
  gracefully, and the gateway retries a connect failure on the next worker (change 10).
- **The workflow host is replaced in place, like an HTTP thread.**
  - *Why that works.* The host's claims and lease renewals are per delivery, so a second host thread
    under the same instance identity claims and renews only its own work. The worker therefore
    starts a replacement host from the resources it started the first one from.
  - *The abandoned host.* It is told to stop, so if it ever returns it drains instead of claiming.
  - *Cost to innocent executions.* Executions that shared the wedged host thread stall with it. Their
    leases lapse and they are redelivered elsewhere, each spending one delivery attempt. Whether such
    attempts are refunded is open question 5. The offending execution keeps wedging hosts only until
    the delivery budget or quarantine stops it.

**Out of memory: prevent in process, contain at a process boundary.** No handler can turn a fatal
out-of-memory into an isolate stop. So the design lets termination land before the heap is exhausted
(change 3). It puts the isolates most likely to run out of memory behind a process boundary with
trust-tier worker pools (change 7). And it attributes and quarantines the residual (change 6), so one
deployment cannot repeat it across the fleet.

**Closures.** `Atomics.wait` and `gc()` leave creator code (change 8). A process budget keeps the sum
of per-isolate allowances under the container limit, so a cgroup kill is not the enforcement mechanism
(change 9).

## What workerd and Deno do

Both were read from source at their default branches.

- **workerd** lets a fatal out-of-memory abort the process. It contains the damage with trust-tier
  cordons and by moving a suspicious worker into a process of its own, and it counts external memory
  through a V8 patch.
- **Deno** ends a worker isolate before the fatal point with a near-heap-limit callback, but it still
  aborts on a genuine out-of-memory.

Neither makes V8 interrupt an optimized loop that reaches no check without a flag or a patch, and both
rely on a process boundary for what escapes. That is this design's shape too.

## Options considered

1. **Re-arm termination.** Rejected: the proven loop reaches no check, so it never observes any
   request.
2. **A loop-optimization flag.** Adopted in change 3. The narrowest flag restores the check. It costs
   peak JavaScript speed for every tenant, behind a benchmark gate. It covers the proven shape, not
   every future elision.
3. **A V8 patch** that stops the elision leaking to the enclosing loop. This is the precise fix at full
   speed, but it means building V8 from source. It replaces option 2 if that cost is accepted.
4. **A per-isolate embedder allocator.** Adopted in change 4. It refuses before allocating, which
   becomes a catchable `RangeError`. It sees only what goes through it: host allocation needs a
   reservation rule, and growable buffers and Wasm memory commit around it. A refusal forces full
   collections, so a catch-and-retry loop is stopped by the CPU limit or the ceiling.
5. **Fold external memory into the heap cap with V8's global-limit flags.** Rejected: the limit is
   checked only after a collection, so one large buffer aborts the process. It also does not count
   shared buffers or Wasm growth.
6. **Trust-tier worker pools.** Adopted in change 7: the process boundary for the out-of-memory
   residual.
7. **Report the elision to V8 upstream.** Done in parallel; nothing relies on it.
8. **Exit the process on a hang.** Rejected: it ends every co-tenant for one app's loop. Thread
   abandonment (change 5) replaces it, and so does recycling only past a threshold.

## The changes

Each change lands with a regression test that fails on the current tree and passes after it, a
control that rules out a vacuous pass, and a mutation that breaks the test. No test reads environment
variables.

**Change 1 -- CPU metering by isolate entry.**
- *Mechanism.* Everything in Goal 1's mechanism: the scoped entry, the one-isolate assertion, the
  lock-free entry slot with exactly-once claiming, the exit handshake, the sampler, and starting the
  watchdog at boot.
- *Why it is safe.* A CPU slice is charged only by the party that wins its claim. A clock is read
  only while its owner provably cannot exit. An unwind closes the entry.
- *Where.* A new entry-slot module (`crates/zeroship-runtime/src/core/entry_slot.rs`, new), the
  runtime's entry points, and the CPU-timer watchdog.
- *Test.*
  - An entry still running is billed before it returns, a panicking entry closes, a second isolate
    is refused, and a closing thread waits for an in-flight read.
  - Controls: with no sampler pass nothing is charged before the return, and with no reader the
    thread closes at once.

**Change 2 -- a durable, bounded bill.**
- *Mechanism.* The next boot ships the crashed boot's records at join. Each slice becomes a usage
  event whose id and source both come from the record: the crashed instance's source identity and
  the record's own identity. So every re-ship carries the same pair. The period recompute keeps one
  event per id (`crates/zeroship-control/src/cron/spend_recompute.rs`), and a provider that dedups on
  source and id counts it once too.
- *Limit.* The no-double-billing claim holds within each provider's dedup window. A record older than
  the shortest window is not shipped: that is a stated loss rather than a double bill.
- *Where.* The worker's join step (`crates/zeroship-worker/src/join.rs`).
- *Test.* Shipping the same record twice yields the same id and source. Control: a different crash
  yields a different id.

**Change 3 -- termination lands.**
- *Mechanism.* `--no-turboshaft-loop-unrolling` is added to the V8 flags, gated by a runtime benchmark
  the operator accepts. Host loops with a creator-chosen count run in bounded chunks that check the
  isolate's termination request. PBKDF2 is re-expressed over the library's HMAC so it can be chunked.
  scrypt, which cannot be split, gets its cost and memory parameters capped, and its working memory is
  reserved against the isolate's external budget.
- *Where.* V8 platform init (`crates/zeroship-runtime/src/core/init.rs`) and the KDF modules.
- *Test.*
  - A warmed between-limits loop and a huge-count PBKDF2 call both end when the CPU limit fires.
  - Controls: a below-limit loop terminates with or without the flag, and a small-count PBKDF2
    matches its known-answer vector.

**Change 4 -- a per-isolate buffer budget with host reservation.**
- *Mechanism.* Each isolate gets an embedder allocator over its plan's external budget, which refuses
  before allocating. Every host-created buffer charged to an isolate goes through one helper that
  reserves against that budget first. On refusal the helper throws a `RangeError`, so a refusal
  never reaches V8's fatal path, and the helper releases the reservation when the buffer is freed.
  The macro-generated call sites use the same helper.
- *Why it is safe.* The budget handle is independent of the isolate, because a buffer can be freed
  after its isolate is disposed.
- *Stated residuals.* Resizable buffers, growable shared buffers and Wasm memory commit outside the
  allocator. The shared-buffer constructor stays reachable through shared Wasm memory, and per-buffer
  caps do not bound the total. The follow-up is to charge these commits through a V8 patch or to
  withhold the features (open question 10).
- *Where.* The runtime's isolate setup, a host-reservation helper, and its call sites.
- *Test.*
  - Over-budget fixed buffers, typed arrays and host buffers fail as a `RangeError` while the
    isolate keeps serving, and freed buffers make room again.
  - Control: the same allocations under budget succeed. The residuals are asserted as residuals.

**Change 5 -- thread abandonment for hangs, with the process serving on.**
- *Mechanism.* Goal 2's abandonment: the watchdog's ceiling and grace, the termination request, the
  vendored ntex-server handle, the replacement thread, demotion of the spinner, billing that ends at
  abandonment, the closed channel, and the workflow host replaced in place. The process recycles
  only past the operator's threshold, and the compose worker service gains a restart policy for
  that case.
- *Why it is safe.* Nothing touches the wedged thread's memory: its isolates are leaked and stay
  charged, and every action on the thread is made while its slot proves it alive.
- *Where.* The CPU-timer watchdog, the worker's thread start and its workflow-host supervisor
  (`crates/zeroship-worker/src/main.rs`, `crates/zeroship-worker/src/workflow_host.rs`), and the
  vendored ntex-server.
- *Test.*
  - A thread wedged past the ceiling leaves rotation, a replacement serves its co-tenants, the
    process keeps serving, and a wedged workflow host is replaced and its deliveries redelivered.
  - Control: an entry that finishes before the ceiling triggers none of this.

**Change 6 -- OOM handler, attribution and corroborated quarantine.**
- *Mechanism.* A per-isolate OOM handler takes no lock and allocates nothing. It records every open
  entry's slice, marks the aborting thread's entry, and ends the process. Only the marked entry
  counts toward quarantine; the other records are billing only. A process out-of-memory, a cgroup
  kill, a CHECK failure, and a heap out-of-memory with no open entry blame no app. A hang abandonment
  writes a record of its own class.
- *Corroboration.* Records ship at the next boot, attributed to the host that wrote them, because the
  crash file lives on that host and is shipped by the next boot there. Control holds a deployment
  only when independent hosts corroborate it. Hosts are counted by the host identity their join
  tokens name (change 7), not by instance ids, because a reboot is a new instance. The hold is
  audited, explained to the creator, and liftable.
- *Where.* The runtime's isolate setup and the entry-slot module, the worker's boot and join, and a
  new hold surface in Control that records through the existing audit log.
- *Test.*
  - A child that exhausts its heap leaves records that name the app and mark its entry, plus a
    co-tenant's unmarked slice. Records from one host rebooting twice hold nothing; records from two
    hosts hold the deployment.
  - Control: a CHECK failure is not classified as an app out-of-memory.

**Change 7 -- trust-tier worker pools.**
- *Mechanism.* A trust tier is a pool label within the frozen execution zone. An app's tier comes
  from its plan and its quarantine history, and the operator can override it. Control filters the
  zone's roster by tier before placement (`crates/zeroship-core/src/worker_ring.rs`), and serves an
  app only to an instance of its tier. Low-trust pools run fewer isolates per process, so an
  out-of-memory there ends fewer tenants.
- *How the tier is bound.* Exactly as the zone is: a worker never asserts its tier. The tier is a
  claim of its join token, honored only for a (zone, tier) pair the token's signer is granted, and
  the join request carries no tier (`crates/zeroship-core/src/worker_join.rs`,
  `crates/zeroship-control/src/worker_join.rs`).
- *Possession, not only the claim.* A join token is a bearer credential up to its uses and expiry,
  and compose today hands every replica its token through one shared volume. So:
  - Tier tokens are minted per host and reach only hosts of that tier, and no token store is shared
    across tiers.
  - Signers stay off machines that run creator code, as the join contract already requires.
  - A captured token admits only into its own tier. Binding tokens to a host key by attestation is
    the stronger option (open question 6).
- *Prerequisite.* The gateway routes by Control's roster rather than its URL-keyed ring
  (`crates/zeroship-gateway/src/proxy.rs`), which the worker-instances migration already names as
  owed.
- *Test.*
  - A token for a tier its signer may not mint for is refused. An admitted instance records its
    token's tier, whatever its request says. Placement excludes an instance of another tier.
  - Control: a permitted pair admits, and a matching tier is placed.

**Change 8 -- capability closures.**
- *Mechanism.* `Atomics.wait` is disabled, because a blocked wait holds the thread without spending
  CPU, so neither the CPU limit nor the sampler sees it. The `gc()` global is removed from creator
  contexts; the process-wide `--expose-gc` flag otherwise hands it to them. The one test that forced
  a collection through `gc()` moves to the host's own collection call.
- *Where.* The runtime's isolate and context setup.
- *Test.* `Atomics.wait` throws instead of blocking, and `gc` is undefined. Control: without the
  closure, the same wait blocks and times out.

**Change 9 -- process budget.**
- *Mechanism.* The worker admits isolates against one process-wide byte reservation derived from the
  container memory limit, taken atomically before the isolate is built. Each isolate is charged its
  steady allowance: its heap cap plus its external budget.
- *Unwind headroom.* Headroom is drawn only by the one entry open on a thread, so it is charged once
  per entry-hosting thread, as a fixed reserve. An isolate stopped at its cap keeps that reserve
  until it is disposed, and the thread re-reserves through the same admission before it enters
  again.
- *Who owns a reservation.* The load owns it until the isolate exists, and releases it on every path
  that produces no isolate, including a startup failure the creator can trigger. After that, the
  isolate releases it at disposal. A leaked isolate is never disposed, so it stays charged by
  construction.
- *When it does not fit.* Isolates are thread-bound, so a load that does not fit evicts only from its
  own thread. If that thread has nothing to evict, the load is deferred as a full cache defers it
  today.
- *Where.* A new budget module in the worker (`crates/zeroship-worker/src/process_budget.rs`, new) and
  the cache's load path.
- *Test.*
  - A thread's many isolates fit under the budget with one unwind reserve. An over-budget load is
    refused or evicts. A failed startup returns its reservation.
  - Control: an under-budget load admits at once.

**Change 10 -- gateway failover on connect failure.**
- *Mechanism.* When the gateway cannot connect to the worker it chose, it sends the request to the
  next worker on the ring. This is safe because the request was never delivered. A failure after
  the connection is established is not retried. This change owns the continuity of a recycle (change
  5's threshold) and a crash (change 6). It is carried into the roster routing change 7 depends on.
- *Where.* The gateway's dispatch (`crates/zeroship-gateway/src/proxy.rs`).
- *Test.* A request whose chosen worker refuses the connection is served by the next worker.
  Control: a worker that accepts and then fails is not retried.

## Threat coverage

| Way to take a thread or the process | Blast radius today | Blast radius in the end state | Billed by | Contained by |
|---|---|---|---|---|
| Request loop, constant-size array, drops what it allocates (hang) | thread, and everything routed to it | isolate | entry sampler | change 3 lands termination; change 5 abandons the thread |
| `for(;;){}` on a plan with no CPU limit | thread | isolate | entry sampler, to the ceiling | change 5's plan-independent ceiling |
| Host loop with a creator-chosen count (PBKDF2 iterations, scrypt cost) | thread; unbilled while it runs | isolate | entry sampler | change 3's chunked termination and parameter caps; change 5 as the backstop |
| Repeated wedges, one app or several | not applicable | abandoned threads' memory and idle-class CPU, up to the recycle threshold | entry sampler, to each ceiling | change 6 quarantines a repeat; the threshold recycles |
| Loop that keeps what it allocates (out of memory) | process | isolate when change 3 lands termination; otherwise the low-trust pool's process | entry sampler; the crash records carry every open slice | change 3; change 7; change 6 quarantines a repeat |
| Async continuation, timer or subscription runaway | thread | isolate | entry sampler | pump CPU share stop; change 3; change 5 |
| Startup evaluation runaway | thread | isolate | entry sampler | startup CPU limit; change 3; change 5 |
| Workflow execution runaway (including cron and queue deliveries) | workflow-host thread; uncounted CPU | the host thread, replaced in place; executions sharing it each lose a delivery attempt | entry sampler | change 3; change 5; delivery budget; change 6 |
| Eviction or quarantine listener runaway | thread | isolate | entry sampler | change 5 |
| Oversized fixed buffer or typed array | process memory, uncapped | isolate, as a `RangeError` | entry sampler | change 4 allocator |
| Host buffer creation after the budget is spent | process (abort) once the allocator exists | isolate, as a `RangeError` | entry sampler | change 4 reservation rule |
| Resizable or growable shared buffers, shared Wasm memory, Wasm memory | process memory, uncapped | process memory, stated residual | entry sampler | follow-ups in change 4; change 9; change 7 |
| Catch-and-retry on a refused allocation (forced collections) | thread CPU | isolate | entry sampler | CPU limit; change 5 on `unlimited` |
| `Atomics.wait` | thread, with no CPU spent | none | nothing to bill | change 8 |
| `gc()` thrash | thread CPU | none | entry sampler | change 8 |
| Process total past the container limit | process, cgroup kill | refused or evicted | none | change 9 |
| V8 CHECK failure, native stack overflow in a host conversion | process | process | none | change 6 classifies and blames no app |
| A compromised worker forging attributions | not applicable | not applicable | none | change 6 counts independent hosts and binds records to the writing host |
| A process presenting another tier's join token | not applicable | refused outside its tier's hosts | none | change 7 delivers tier tokens only to that tier's hosts |

Wherever a hang's end state reads "isolate", the wedged thread's in-flight requests are also lost.
So are requests on gateway keep-alive connections that thread owns; both end at the gateway's worker
timeout. The co-tenants cold-start on healthy threads.

## Landing order

- **Self-contained.** Changes 1, 3, 4, 8 and 10 depend on nothing else here; any of them can land
  first.
- **Change 6** walks change 1's slots, so it lands with or after change 1.
- **Change 2** ships change 6's records, so it lands with or after change 6.
- **Change 5** reads change 1's slots, and separately needs the vendored ntex-server patch. Change
  6's hang-class record in turn depends on change 5.
- **Change 9** depends on change 4 for the external term. Once both 5 and 9 have landed, leaked
  isolates stay charged by construction.
- **Change 7** depends on the gateway's roster routing, a prerequisite outside this set that the
  worker-instances migration already names as owed. Change 6's host-counted corroboration depends on
  change 7's per-host tokens.

None of the ten is a transitional half-state; each is the end-state piece for its goal.

## Open questions for the operator

1. **Billing cadence and durability.**
   - How short the sampler's cadence must be to bound unbilled CPU without adding watchdog overhead.
   - Whether the crash file needs a synchronous flush, or the one-interval loss on an ordinary crash
     is accepted.
2. **The spend cap.**
   - Whether cadence-billed CPU should shorten the spend recompute.
   - Whether a runaway needs a faster-tripping cap than the existing states, given each entry is
     already bounded by the ceiling.
   - Whether that ceiling should surface in the spend state at all.
3. **The entry ceiling and the termination grace.** Both are unset. Too short abandons an ordinary
   slow request; too long holds a thread and keeps its co-tenants waiting.
4. **The recycle threshold.**
   - The count of abandoned threads, the bytes their isolates and unwind reserves hold, or the CPU
     they keep spending under a quota, past which a worker recycles.
   - Whether workers run with a CPU quota at all; without one, demotion keeps an abandoned thread off
     the healthy threads' CPU and the CPU term never fires.
   - Whether repeated wedges by one app escalate its quarantine before that.
   - Whether an abandonment, or the memory it leaks, carries a fixed charge to the app.
5. **Delivery attempts lost to an abandoned workflow host.** Executions that shared the wedged host
   lose an attempt each. Should they be refunded? A refund means the service trusts the worker's
   report of an abandonment.
6. **Trust tiers.**
   - How an app's tier derives from its plan and quarantine history, and what moves it.
   - How often a tier change may cold-start an app.
   - How signers and per-host tokens are provisioned per tier, and whether to require host
     attestation that binds tokens to a host key.
7. **Workflow CPU pricing.** Whether workflow CPU is priced like request CPU, now that its startup
   and synchronous dispatch are metered.
8. **Platform housekeeping.**
   - Whether idle collection and collection at a stop bill the app's `cpu_us`, are absorbed as
     platform cost, or are priced separately.
   - The entry kind already distinguishes them.
9. **The loop-optimization flag's speed cost.** The benchmark threshold the operator accepts before
   change 3's flag ships for every tenant, and the fallback if it is refused: option 3's V8 patch, or
   changes 5 and 6 alone.
10. **External memory.**
    - The per-plan external budget's size.
    - Whether resizable and growable buffers and Wasm memory are charged through a V8 patch or
      withheld.
    - scrypt's parameter caps.
11. **Quarantine policy.**
    - How many independent hosts must corroborate, and over what window.
    - The per-organization escalation.
    - Whether a redeploy clears a hold.
    - The creator-facing message, and who authorizes a lift.
12. **The process budget's sizing rule.** Its safety margin under the container limit, and how it
    tracks change 7's per-pool isolate and thread settings.
13. **The ntex-server patch.** Whether the vendored handle is carried until an upstream release has
    it, or the per-thread `SO_REUSEPORT` servers are preferred so no patch is carried.
