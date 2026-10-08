# A runaway isolate must not take the worker process down

**Status.** PROPOSED. Nothing in this document is implemented. It settles the design for two gaps
the per-isolate heap cap leaves open:

- **(a) A runaway loop still takes down the whole worker.** The heap cap stops an isolate by
  requesting V8 termination at the next interrupt check. TurboFan compiles a loop that builds a
  constant-size array into code that reaches no interrupt check, so the termination is never
  observed. Such a loop either fills the heap and V8 aborts the process, or, if it drops what it
  allocates, never fills the heap and spins a worker thread forever. Either way one app's loop ends
  service for every co-tenant on that worker.
- **(b) Memory outside the V8 heap is not bounded.** ArrayBuffers, resizable ArrayBuffers, typed
  arrays, WebAssembly memory and host-created backing stores are counted by V8 for GC pacing but
  are not capped, so an app can grow the process past its heap cap until the host kills it with no
  attribution.

The tenant boundary (schema binding, the binding role, execution-zone placement) is not what this
changes and is not weakened here. This is about blast radius: one app's resource behavior must cost
that app its isolate, never the co-tenants sharing its worker.

---

## What is true today

**One process hosts many tenants.** One isolate per (app, live deploy) serves requests, isolates
`enter`/`exit` so one worker thread serves many apps, and one worker process runs several such
threads (`deploy/compose/docker-compose.yml` `worker` command, its `--threads` and `--max-isolates`
flags; `crates/zeroship-worker/src/cache.rs` `init_cache`, a per-thread `AppCache`). A process-wide
abort or a wedged thread ends service for every isolate it was hosting.

**Every isolate carries an operator-set heap cap.** `heap_limit_mb` is a `NonZeroU32` on every plan
(`crates/zeroship-core/src/types.rs`, `crates/zeroship-control/src/plan_catalog.rs`); the worker
stamps it on every isolate (`crates/zeroship-worker/src/cache.rs` `runtime_limits_from_app`, then
`RuntimeInner::new_with_plugins` in `crates/zeroship-runtime/src/core/runtime.rs`, which passes it
to `v8::CreateParams::heap_limits`).

**The cap terminates the isolate; it does not fail the allocation.** The near-heap-limit callback
`near_heap_limit` (`crates/zeroship-runtime/src/core/heap_cap.rs`) marks the isolate reached
(`CapReached`), requests termination through the isolate's thread-safe handle, and grants a bounded
headroom (`granted_limit`, `UNWIND_HEADROOM`, `UNWIND_GRANTS`) for the allocation in flight to
finish while the termination unwinds. The runtime then enters no more JavaScript in that isolate:
`RuntimeInner::halted` gates every entry point, and `RuntimeInner::stop_for_heap_cap` answers the
pending requests, GCs the granted headroom back, and quarantines the isolate so the worker drops it
(`set_stop_notifier` / `drop_stopped_isolate` in `crates/zeroship-worker/src/cache.rs`). The module
header of `heap_cap.rs` names the remaining gap in its own words: code that reaches no interrupt
check keeps allocating after the termination is requested, and past the bound V8 aborts the process.

**The CPU limit terminates the same way.** `crates/zeroship-runtime/src/core/cpu_timer.rs` is an
isolate-owned, one-shot POSIX timer on `CLOCK_THREAD_CPUTIME_ID`; its process-wide `CpuTimerSystem`
runs a `watchdog_loop` that calls `IsolateHandle::terminate_execution` (`terminate_registered`). So
both the heap cap and the CPU limit rest on termination landing at an interrupt check. A loop that
reaches none defeats both.

**No embedder OOM handler is installed.** `RuntimeInner::new_with_plugins` sets no OOM handler, so
V8's fatal out-of-memory path aborts with no attribution. `src/api/api.cc`
`Utils::ReportOOMFailure` invokes a per-isolate handler only when one is set through
`Isolate::SetOOMErrorHandler`, and otherwise calls `base::FatalOOM` and aborts.

**Memory outside the heap is uncapped.** `new_with_plugins` sets no `array_buffer_allocator`, so
rusty_v8 installs the default one (`isolate_create_params.rs` `finalize` ->
`new_default_allocator`). V8 does count external bytes for GC pacing, through
`JSArrayBuffer::CreateExtension` -> `AppendArrayBufferExtension` ->
`ArrayBufferSweeper::IncrementExternalMemoryCounters` (`src/objects/js-array-buffer.cc`,
`src/heap/array-buffer-sweeper.cc`), but that total enters the enforced limit only under flags that
default off (see Options). So external memory is accounted but unbounded.

## The root cause

The `v8` crate pins V8 at the version in `Cargo.lock` (`v8` 147.1.0, which is V8 14.7.173.13,
`include/v8-version.h`). V8 honors a termination request only at an interrupt check:
`src/execution/stack-guard.cc` `StackGuard::HandleInterrupts` and `HasTerminationRequest` service
`TERMINATE_EXECUTION`, and those run from the stack-guard check that compiled code emits at function
entry and at loop back-edges. `BytecodeGraphBuilder::VisitJumpLoop` places the back-edge check
(`BuildIterationBodyStackCheck`, `src/compiler/bytecode-graph-builder.cc`). Requesting termination
only sets a flag and lowers the stack-limit sentinel
(`StackGuard::update_interrupt_requests_and_stack_limits`); nothing lands until code reaches a
check.

The trigger is not "an allocating loop", and allocation is beside the point. It is a constant-size
element-initialization loop, lowered without a check of its own, whose check-removal flag leaks to
the enclosing loop:

1. `JSCreateLowering::ReduceNewArray` (`src/compiler/js-create-lowering.cc`) unrolls element
   initialization in place only for a constant length at or below `kElementLoopUnrollLimit`; above it,
   and below `JSArray::kInitialMaxFastElementArray`, it emits a `NewDoubleElements` /
   `NewSmiOrObjectElements` node.
2. `MachineLoweringReducer::REDUCE(NewArray)`
   (`src/compiler/turboshaft/machine-lowering-reducer-inl.h`) lowers that node to a
   compiler-generated `WHILE(index < length)` loop that has no `JSStackCheck`.
3. `LoopUnrollingAnalyzer::DetectUnrollableLoops` (`src/compiler/turboshaft/loop-unrolling-reducer.cc`)
   marks a loop for stack-check removal when its iteration count is statically known and
   `IsSmallerThan(kMaxIterForStackCheckRemoval)`. The inner init loop's count is the constant array
   length, so it is marked; an infinite `for(;;)` has an unknown count
   (`GetLoopIterationCount` returns the empty `IterationCount` for a non-static branch) and its own
   back-edge is never marked.
4. `MachineLoweringPhase` runs before `LoopUnrollingPhase` (`src/compiler/turboshaft/pipelines.h`),
   so the inner loop is present and marked when `LoopStackCheckElisionReducer` runs. That reducer
   (`src/compiler/turboshaft/loop-unrolling-reducer.h`) sets `skip_next_stack_check_` on a marked
   loop, and because the inner init loop carries no check of its own, the flag is still set when the
   copier reaches the next `JSStackCheck(kLoop)` -- the enclosing loop's back-edge -- and removes
   it.

So the enclosing loop loses the one check the termination needed. The general fault is that any
compiler-generated counted loop with no check of its own can leak the skip flag to the enclosing
loop; the shape proven below is a loop whose body constructs a constant-size array whose element
count is above `kElementLoopUnrollLimit` and below `kMaxIterForStackCheckRemoval`, optimized by
TurboFan. Other lowered shapes probed (substring, slice, spread, a long argument list) do reach a
check and are terminated, so the stated trigger is the constant-size-array family, while the patch in
Option 3 is what fixes the leak in general. Ordinary library code trips the proven shape; a request
handler that builds a fixed-size buffer or table each iteration is enough, with no malice.

**Derived by experiment** on the pinned V8 with a standalone bare-isolate probe that the regression
tests below reproduce in the tree. Each case is a non-retaining `for(;;)` loop with a cross-thread
termination requested shortly after start; a case that reaches no interrupt check runs to its own
break, one that does stops at once:

| Loop body | Default flags |
|---|---|
| arithmetic, no allocation | terminated |
| `new Array(K)`, K at or below `kElementLoopUnrollLimit` | terminated |
| `new Array(K)`, K between the two limits (and the same via an inlined callee) | not terminated |
| `new Array(K)`, K at or above `kMaxIterForStackCheckRemoval` | terminated |
| `new Array(n)`, n not a compile-time constant | terminated |
| `new Uint8Array(K)` / `new ArrayBuffer(K)` (external) | terminated |

The between-limits row is the only one that is not terminated, and it is terminated once
`--no-turboshaft-loop-unrolling` is set; the other rows are shown under default flags, where they
already terminate, so the flag cannot change them.

The witness that the ignoring happens at the TurboFan tier: the between-limits case is terminated
under `--max-opt=2` (Maglev as top tier), `--no-turbofan`, and `--jitless`, and a finite variant of
the same body reports `%GetOptimizationStatus` with the `kTurboFanned` bit set, which is `1 << 5` in
`src/runtime/runtime.h` `OptimizationStatus`. Re-requesting the termination repeatedly does not
change the between-limits result, because the loop reaches no check to observe any request; that is
why re-arming (the mechanism a Deno draft adds for a different case) does not address this.

## The two failure modes

The same shape fails two ways, and the design must cover both.

- **Abort.** A loop that retains its allocations fills the heap; `src/heap/heap.cc`
  `Heap::CheckHeapLimitReached` invokes the near-heap-limit callback and, still over the limit,
  calls `FatalProcessOutOfMemory`, which aborts the process.
- **Hang.** A loop that drops its allocations never fills the heap, so the out-of-memory path is
  never reached and the thread spins past the self-break bound in the experiment. Nothing stops it:
  `CpuTimer::arm` is one-shot, `healthz` (`crates/zeroship-worker/src/health.rs`) is a constant
  success, and the wall bound in the worker handler is a `compio::time::sleep` on the same wedged
  thread. On a plan with no CPU limit -- the `unlimited` plan sets `cpu_limit_ms: None` and
  `wall_timeout_ms: None` (`crates/zeroship-control/src/plan_catalog.rs`), and `arm_cpu_timer`
  (`crates/zeroship-runtime/src/core/runtime.rs`) arms only on `Some(limit)` -- nothing even requests
  termination, so a plain `for(;;){}` wedges the thread with no request outstanding. The gateway then
  keeps routing that app -- and, as its ring position spills, other apps -- to a thread that answers
  nothing.

## What workerd and Deno do

workerd and Deno were read from their source at their current default branches; the mechanism
citations are from V8 source at the pinned version.

**workerd lets a fatal out-of-memory abort the process and contains it architecturally.**
`src/workerd/jsg/setup.c++` installs `IsolateBase::oomError` per isolate and
`V8::SetFatalMemoryErrorCallback(&IsolateBase::oomError)` process-wide; both route through
`reportV8FatalError`, which calls `abort()`. Its termination helper documents that a bare terminate
is unreliable: `src/workerd/jsg/jsg.c++` `Lock::terminateExecutionNow` forces a `v8::JSON::Stringify`
to make V8 check the flag and asserts fatally if it still does not terminate. Its shipped limit
enforcer enforces nothing (`src/workerd/server/server.c++` `NullIsolateLimitEnforcer`); real
thresholds are closed-source. Containment is by trust-tier "cordons" and "Dynamic Process Isolation"
that moves a suspicious worker into its own process (Cloudflare's published security model). workerd
sets no TurboFan or loop-optimization flags; it counts external memory by a V8 patch (`patches/v8/`)
that folds external allocations into the heap counter, so its documented memory limit covers the JS
heap and WebAssembly together.

**Deno ends a worker isolate before the fatal point and still aborts on a real one.**
`cli/lib/worker.rs` installs a near-heap-limit callback that sets an `oom_triggered` flag, calls
`terminate_execution`, and returns a raised limit; `runtime/web_worker.rs` turns the resulting
failure into a worker-scoped error rather than a process abort when the flag is set, but a genuine
out-of-memory still aborts. Its own worker handle documents that `terminate_execution` returns before
the isolate stops and cannot fully close the window. Deno's re-arming of termination is a draft, not
shipped, and the case it names is promise hooks, not an optimized loop. Deno keeps the default
allocator.

The accurate lesson: an embedder can attribute and contain a fatal out-of-memory, but neither
open-source runtime makes V8 interrupt an optimized loop that reaches no check without a flag or a
patch. Prevention needs a V8-level lever; everything else is containment.

## Options considered

**Option 1: re-arm termination.** Re-request termination on a cadence. *For:* closes a narrower
case where a single termination is cleared before it lands. *Against:* does not stop the
between-limits loop at all, since that loop reaches no check to observe any request. Not a fix for
(a).

**Option 2: a loop-optimization lever so termination lands.** Set a V8 flag in `init_v8_platform`
(`crates/zeroship-runtime/src/core/init.rs`). `--no-turboshaft-loop-unrolling` is the narrowest that
restores the enclosing loop's check (it keeps TurboFan codegen and disables only the phase that
elides the check); `--max-opt=2` (cap at Maglev) and `--no-turbofan` also work and cost more.
*For:* makes the existing heap cap and CPU limit actually stop the common runaway. *Against:* a
peak-JavaScript-speed cost for every tenant in the process, to be sized by the runtime benches; it
closes the shape derived here, not a proof that no other optimization can elide a check later; and
`init_v8_platform` is shared with the migrate recorder and every test binary, so the flag is
process-global for all of them. A strong mitigation, not a complete guarantee, which is why it is
paired with containment.

**Option 3: a V8 patch.** Carry a patch that stops `LoopStackCheckElisionReducer` from leaking the
skip flag across the inner init loop (keep the enclosing loop's check). *For:* fixes the cause
precisely, at full optimization speed, and is what workerd does for its own V8 needs (`patches/v8/`).
*Against:* building V8 from source rather than consuming the prebuilt `rusty_v8` archive, which is a
toolchain and maintenance change. The right long-term fix; too large to gate the first landing.

**Option 4: a per-isolate embedder ArrayBuffer allocator with a budget (recommended for (b)).**
Install a `v8::ArrayBuffer::Allocator` built with `v8::new_rust_allocator` that tracks the isolate's
external total and returns null from `allocate` / `allocate_uninitialized` when an allocation would
pass the plan's external budget. *For:* the refusal happens before the allocation, so V8 turns it
into a catchable `RangeError` (`src/builtins/builtins-arraybuffer.cc` `ConstructBuffer` ->
`kArrayBufferAllocationFailed`), never a process abort -- proven, including a single allocation far
larger than the budget and gradual growth across many allocations, with the isolate still serving
afterwards (the bare-isolate probe the regression tests reproduce). *Against:* it bounds only the
paths that go through `allocate` (fixed ArrayBuffer and typed arrays); resizable ArrayBuffers and
Wasm memory use the page allocator (`BackingStore::TryAllocateAndPartiallyCommitMemory`), and host
buffers come in through `ArrayBuffer::new_backing_store_from_vec`
(`crates/zeroship-runtime/src/web/fetch/body/extract.rs`, `body_stream.rs`, `consumers.rs`,
`crates/zeroship-runtime/src/core/runtime.rs`, `crates/zeroship-data-v8/src/v8_values.rs`,
`crates/zeroship-storage-v8/src/callbacks.rs`). Each refusal that reaches the allocator makes V8 run
`HeapAllocator::RetryCustomAllocate`, which forces full garbage collections
(`src/heap/heap-allocator.cc` `HeapAllocator::CollectAllAvailableGarbage`) before giving up, and
the embedder cannot skip that. So a script that catches the `RangeError` and retries in a loop
forces a full collection per iteration. That loop
does reach interrupt checks (the external-allocation rows terminate), so on a plan with a CPU limit
the CPU limit stops it; on a plan without one (`unlimited`), only change 3's plan-independent
ceiling stops it, by exiting the process with an attributed hang record. A second hazard comes with
the allocator: host code that creates an `ArrayBuffer` through the isolate allocator after the budget
is spent does not get a `RangeError` -- `v8::ArrayBuffer::new` and `ArrayBuffer::new_backing_store`
reach V8's `ArrayBuffer::New` / `NewBackingStore`, which call
`FatalProcessOutOfMemory("v8::ArrayBuffer::New")` on allocation failure (`src/api/api.cc`), and the
pinned crate binds no fallible variant (`ArrayBuffer::MaybeNew` or the return-null failure mode).
Change 1's host-reservation rule closes that. `RustAllocatorVtable` is also gated out under the V8
sandbox feature, so this precludes enabling that sandbox; the shipped archive is the non-sandbox
build, so the trade is available today. The paths it misses are closed or stated separately below.

**Option 5: fold external memory into the per-isolate cap through V8's own accounting (rejected).**
Set `--enforce-global-heap-limit` (so `Heap::ReachedHeapLimit` compares `GlobalConsumedBytes` against
`max_global_memory_size`), `--external-memory-accounted-in-global-limit` (so `Heap::GlobalSizeOfObjects`
adds `external_memory()`; both default off, `src/heap/heap.cc`, `src/flags/flag-definitions.h`), and
`--maximum-global-heap-limit-factor` (a process-wide integer, default eight, that multiplies the
old-generation maximum and is re-applied on every near-heap-limit grant through
`HeapLimits::SetMaximumSizes`). *For:* V8's own counter reaches more paths than the allocator does.
*Against, and disqualifying:* the global limit is checked only after a GC
(`Heap::CheckHeapLimitReached`, from the collection epilogue), so a single large allocation succeeds
first, and the near-heap-limit callback can then only raise the limit by the bounded grant; a single
allocation larger than the factor times the grant ceiling stays over the limit and V8 calls
`FatalProcessOutOfMemory`. Measured: with the runtime's own grant policy and the free-plan cap at
factor one, one creator line that constructs a multi-gigabyte `ArrayBuffer` aborts the process, where
the current tree allocates the same buffer without aborting; raising the factor only moves the
threshold, since `JSArrayBuffer::kMaxByteLength` is near `kMaxSafeInteger` on this build. It also does
not count `SharedArrayBuffer` or Wasm growth at all (`BackingStore::PerIsolateAccountingLength`
returns zero for a shared or a Wasm backing store, `src/objects/backing-store.h`): measured, a
gigabyte of either grows with the counter unmoved and the callback never firing. So Option 5 both
introduces a new abort and misses two paths; it is not used.

**Option 6: trust-tier worker pools.** Place low-trust apps in a separate pool from high-trust apps.
*For:* the strongest containment, and it fits the existing placement model: apps carry a frozen
execution zone (`db/migrations-ts/20260914000600_app_execution_zones.ts`,
`db/migrations-ts/20260919000100_project_execution_zone.ts`) and route over the shared ring
(`crates/zeroship-core/src/worker_ring.rs` `ring_walk` / `eligible_set`,
`crates/zeroship-control/src/worker_join.rs` `instance_serves_app`), so a trust tier is another
coarse placement axis. *Against:* a larger change across placement, join-signer zones and operator
provisioning, and it reduces bin-packing. The bound for free-tier abuse; proposed as a follow-on,
the operator's call.

**Option 7: report the elision upstream to V8.** The minimal repro is the bare-isolate case the
regression tests land. Frame it as "the check-removal flag leaks from a compiler-generated
element-init loop to the enclosing loop", not "unrolling elides a check". Low cost, unknown timeline,
so it cannot be the only response; do it in parallel with Option 3.

## Recommended design (end state)

Pre-launch, no back-compat: build the end state directly. No single mechanism both prevents the
runaway and bounds every residual, so the end state is layered. The external-memory bound is the
embedder allocator (Option 4), not the global-limit flags (Option 5, which aborts and under-counts).

1. **External-memory budget by embedder allocator (Option 4), with host reservation, and a stated
   disposition for each path it cannot see.** Give each isolate a `v8::new_rust_allocator` over the
   plan's external budget in `RuntimeInner::new_with_plugins`, refusing an over-budget allocation as
   a catchable `RangeError` before it happens. This bounds fixed ArrayBuffers and typed arrays made by
   creator code, single-large and gradual, with no abort. The budget handle is its own reference-counted
   atomic, stamped with the app id when the isolate is built and holding no reference to the isolate:
   `CreateParams::array_buffer_allocator` takes a per-isolate shared pointer and every backing store
   keeps that pointer (`src/objects/backing-store.cc` `BackingStore::SetAllocatorFromIsolate`), so
   `free` can run on the array-buffer sweeper thread or after the isolate is disposed. Isolate to app is one to one, so the
   budget is attributable to the app.

   **The host-reservation rule.** Every host allocation charged to an isolate reserves against that
   isolate's budget first, on the isolate's thread, and throws a `RangeError` into the script when the
   reservation is refused, so a host allocation never reaches the allocator's refusal and never
   reaches V8's `FatalProcessOutOfMemory("v8::ArrayBuffer::New")`. Reserving first is sound because
   allocations happen only on that thread and frees only lower the count. The rule lives in one
   runtime helper that every binding uses to create an ArrayBuffer or backing store, and
   `zeroship-runtime-macros` generates calls to that helper instead of `v8::ArrayBuffer::new`. It
   covers the allocator-routed host call sites: `crates/zeroship-runtime/src/web/blob/mod.rs`
   `arrayBuffer`, `bytes`, `build_blob_stream`; `crates/zeroship-runtime/src/web/blob/file.rs`
   `arrayBuffer`, `bytes`; `crates/zeroship-runtime/src/node/buffer/native.rs` `emit_uint8array`;
   `crates/zeroship-runtime/src/node/crypto/kdf.rs` `hkdf_callback`, `hkdf_sync_callback`;
   `crates/zeroship-runtime/src/web/crypto/helpers.rs` `vec_to_arraybuffer`, `vec_to_uint8array`,
   `vec_to_uint8array_typed`; `crates/zeroship-runtime/src/web/crypto/crypto_key.rs`
   `vec_to_uint8array`; `crates/zeroship-runtime/src/web/crypto/ops.rs` `derive_key`;
   `crates/zeroship-runtime/src/web/crypto/wrap.rs` `unwrap_key`;
   `crates/zeroship-runtime/src/web/websocket/dispatch.rs` `dispatch_one`;
   `crates/zeroship-runtime/src/web/encoding/streams.rs` `encoder_transform_callback`;
   `crates/zeroship-runtime/src/web/streams/compression.rs` `enqueue_bytes`;
   `crates/zeroship-runtime/src/web/streams/byte_tee.rs` `chunk_steps`;
   `crates/zeroship-runtime/src/web/streams/readable_byte_controller.rs` `pull_steps`,
   `readable_byte_stream_controller_enqueue_cloned_chunk_to_queue`;
   `crates/zeroship-runtime/src/rpc/superjson.rs` `revive_typed_array`;
   `crates/zeroship-runtime/src/core/runtime.rs` `handle_op_result_pump`; and the code generated by
   `crates/zeroship-runtime-macros/src/codegen.rs` `gen_vec_u8_set` and
   `crates/zeroship-runtime-macros/src/v8_iterable/value_marshal.rs` `gen_to_v8`.

   **Host `from_vec` buffers get a byte bound by the same helper.** The existing stream budget
   (`crates/zeroship-runtime/src/web/streams/budget.rs`) counts live streams, not bytes, so it does not
   bound these. The helper reserves the byte length before building the backing store and builds it
   with `ArrayBuffer::new_backing_store_from_ptr`, whose deleter releases the reservation when V8
   frees the store; the release runs on whatever thread frees it, which the atomic handle allows. This
   replaces `new_backing_store_from_vec` at `crates/zeroship-runtime/src/web/fetch/body/consumers.rs`
   `consumer_array_buffer`, `consumer_bytes`, `consumer_form_data`, `settle_outer`;
   `crates/zeroship-runtime/src/web/fetch/body/body_stream.rs` `settle_with_bytes`;
   `crates/zeroship-runtime/src/web/fetch/body/extract.rs` `pull_callback`;
   `crates/zeroship-runtime/src/core/runtime.rs` `uint8_array_from_bytes`;
   `crates/zeroship-data-v8/src/v8_values.rs` `encode`; and
   `crates/zeroship-storage-v8/src/callbacks.rs` `into_v8`.

   **Paths the design does not bound, stated with their follow-ups.** Each follow-up must bound the
   isolate's total, not one buffer.
   - *Resizable ArrayBuffers.* They commit through the page allocator, there is no disable flag, and
     `RustAllocatorVtable` exposes no `MaxAllocationSize` hook, though V8 checks a resizable
     `maxByteLength` against `array_buffer_allocator()->MaxAllocationSize()`
     (`src/builtins/builtins-arraybuffer.cc` `TryAllocateBackingStore`). That hook, or a context-setup
     guard on the constructor's `maxByteLength`, caps each buffer and not the total: many buffers each
     under the cap still sum without bound, and a guard on the global constructor is bypassed through
     the intrinsic (`new Uint8Array(1).buffer.constructor`) unless `resize` and `transfer` are guarded
     too. The follow-up that bounds the total is to charge resizable and growable commits to the
     per-isolate budget (a V8 patch at the commit path), or to disable resizable and growable buffers
     for creator code.
   - *SharedArrayBuffer.* `--enable-sharedarraybuffer-per-context` with no callback keeps the global
     off creator contexts (`src/init/bootstrapper.cc` `Genesis::InitializeGlobal_sharedarraybuffer`,
     `src/execution/isolate.cc` `Isolate::IsSharedArrayBufferConstructorEnabled`), but the
     constructor stays reachable as
     `new WebAssembly.Memory({shared: true, ...}).buffer.constructor`, and no flag in the pinned V8
     refuses shared Wasm memory. A fixed `SharedArrayBuffer` made that way goes through the isolate
     allocator and is refused; a growable one commits through the page allocator and is not. Growable
     shared buffers belong to the resizable follow-up above, and shared Wasm memory to the Wasm one.
   - *WebAssembly memory.* `--wasm-max-mem-pages` caps each memory, not an isolate's total: an
     `initial` above it is refused, growth stops at it, and a declared `maximum` above it is accepted.
     Many memories, each at the cap, still sum past the budget, and Wasm memory is not counted by V8
     past its initial size. The follow-up that bounds the total is to charge Wasm memory commits to the
     per-isolate budget (a V8 patch at `src/objects/backing-store.cc` `BackingStore::AllocateWasmMemory`
     and `GrowWasmMemoryInPlace`), or to withhold WebAssembly from creator code; removing the
     `WebAssembly` global at context setup is the candidate mechanism for the latter and is not yet
     tested.

   The global-limit flags are deliberately not set, so these paths stay unbounded rather than
   introducing the single-large abort.
2. **Loop-optimization lever (Option 2).** Set `--no-turboshaft-loop-unrolling` in
   `init_v8_platform`, sized by a runtime benchmark the operator accepts, so the cap's and CPU
   limit's termination lands on the common runaway shape and turns an abort or a hang into an
   ordinary isolate stop. Pursue Option 3 (the patch) as the eventual replacement and Option 7 (the
   upstream report) in parallel.
3. **Off-thread hang detector with attributed process exit.** The CPU watchdog
   (`crates/zeroship-runtime/src/core/cpu_timer.rs`) escalates: when an isolate has been in one V8
   entry past a plan-independent ceiling, whether or not a termination was requested, it records an
   attributed crash of a distinct hang class (shared with change 4) and exits the process. The
   plan-independent ceiling is what covers the `unlimited` plan, which arms no CPU timer and so has no
   outstanding termination to wait on. Each worker thread stamps, on entering V8, a monotonic epoch
   and the time it entered; the watchdog thread reads them without the isolate lock.
4. **Per-isolate OOM handler, crash classification, and attributed quarantine.** Register
   `Isolate::set_oom_error_handler` for every isolate in `RuntimeInner::new_with_plugins` (not
   `V8::set_fatal_error_handler`, which binds the CHECK-failure path, not the out-of-memory path).
   The handler allocates nothing, classifies the crash by `OomDetails::is_heap_oom`, writes a fixed,
   pre-formatted record naming the app (from the enter/exit marker) with `write(2)` to a file
   descriptor opened at boot, and then ends the process itself with `_exit` rather than returning,
   so it does not fall through to `FATAL("API fatal error handler returned ...")` and record the same
   crash twice. The hang class from change 3 is a countable, attributed crash alongside a heap
   out-of-memory. The next boot ships the record to control at join. Control keys a deployment hold on
   app plus owning organization, requires corroboration from more than one instance before holding,
   holds on the first corroborated attribution of a counted class pending review, records an audit
   entry, shows the creator why, and offers an explicit lift path. A process out-of-memory, a cgroup
   kill (a signal with no handler), and a CHECK failure are not counted against an app.

Supporting pieces of the end state:

- **The app-attribution marker lives in the runtime, not the worker dispatch.** Set it in
  `RuntimeInner::enter_isolate` / `exit_isolate` (`crates/zeroship-runtime/src/core/runtime.rs`),
  shaped by `enter_depth`, so it names the isolate actually entered -- covering the pump, startup,
  eviction (`entered_for_eviction`) and workflow isolates, not only a request dispatch. Out-of-memory
  on a V8 background thread with no current isolate stays unattributed.
- **The crash record is per instance.** Compose and the real deployment mount shared volumes, so the
  record file is named by the instance id and written to a per-instance path that survives a
  reschedule, read once at the next boot and then cleared.

This keeps the gateway dumb, keeps metering and the privilege boundaries intact, and adds no `env.*`
surface: the external budget is an operator-set per-plan value beside the heap cap, and the
quarantine decision is control's.

## Threat coverage

| Way to take a thread or the process | Covered by | Status |
|---|---|---|
| Retaining loop, constant-size array (abort) | change 2 lands termination; change 4 attributes a residual abort | covered |
| Non-retaining loop, constant-size array (hang) | change 2 lands termination; change 3 exits with attribution on a residual | covered |
| Hang on a plan with no CPU limit (unlimited) | change 3's plan-independent ceiling | covered |
| Fixed ArrayBuffer / typed-array growth (single-large or gradual) | change 1 allocator refuses as a RangeError | covered |
| Host ArrayBuffer creation after the budget is spent | change 1 host-reservation rule throws a RangeError before the allocator refuses | covered |
| Host `from_vec` buffers | change 1 helper reserves the bytes and the backing-store deleter releases them | covered |
| Resizable ArrayBuffer and growable SharedArrayBuffer growth | unbounded in the pinned crate; follow-up charges commits to the per-isolate budget (V8 patch) or disables these buffers for creator code | open, stated |
| SharedArrayBuffer through shared Wasm memory | global hidden by `--enable-sharedarraybuffer-per-context`; constructor still reachable via `WebAssembly.Memory({shared: true})`; fixed ones refused by the allocator, growable ones in the row above | open, stated |
| WebAssembly memory growth | `--wasm-max-mem-pages` caps each memory, not the total; follow-up charges Wasm commits to the per-isolate budget (V8 patch) or withholds WebAssembly from creator code | open, stated |
| Wasm loop reaching no check (same elision in the Wasm pipeline) | probe during implementation; change 2 may cover it via `--wasm-loop-unrolling` | open, probe |
| `Atomics.wait` blocking a thread | change 5 (`set_allow_atomics_wait(false)`) | covered by change |
| `gc()` thrash | change 5 (remove `gc()` from creator contexts) | covered by change |
| Process total past the container limit (cgroup kill) | change 6 (process budget / admission) | covered by change |
| V8 CHECK abort, native stack overflow in host conversions | change 4 classifies and does not blame an app | partial, stated |
| Compromised worker forging attributions | reports bound to the reporting instance identity; corroboration required | covered by change 4 |

## Components, landing order, and the regression test that fails before each

Each change lands with a test that fails on the current tree and passes after, bound to the change by
a mutation that breaks the test. The repro for each lands in the tree as that test; the experiment
log is provenance only. Numbers stay in the log.

**Change 1 -- external-memory budget and host reservation.**
- *Touches:* a per-isolate allocator and its reference-counted budget handle built beside
  `crates/zeroship-runtime/src/core/heap_cap.rs` and installed in `RuntimeInner::new_with_plugins`
  (`crates/zeroship-runtime/src/core/runtime.rs`); the host-reservation helper in the runtime, and
  every host call site listed under the recommended design moved onto it, including the
  `from_vec` sites; `crates/zeroship-runtime-macros/src/codegen.rs` `gen_vec_u8_set` and
  `crates/zeroship-runtime-macros/src/v8_iterable/value_marshal.rs` `gen_to_v8`, so generated
  bindings call the helper; `--enable-sharedarraybuffer-per-context` and `--wasm-max-mem-pages` in
  `init_v8_platform` (`crates/zeroship-runtime/src/core/init.rs`); the external budget on
  `RuntimeLimits` / `RuntimeBuilder` and `AppRuntimeLimits` (`crates/zeroship-core/src/types.rs`),
  `plan_catalog.rs` and `runtime_limits_from_app` (`crates/zeroship-worker/src/cache.rs`); the
  control-to-worker limits projection (`crates/zeroship-worker/src/sync.rs`); and
  `docs/reference/runtime-limits.md` for the new budget and the stated residuals.
- *Tests* (`crates/zeroship-runtime/tests/integration/heap_limits.rs`), one per path, each with a
  control under the budget so no case passes over zero effect:
  - a single fixed `ArrayBuffer` larger than the budget yields a `RangeError` and the isolate still
    serves, run through `in_own_process!` so an abort fails the case rather than passing it;
  - a loop of fixed ArrayBuffers past the budget yields a `RangeError`, with a refuse, free, retry
    churn case that binds the free-side decrement;
  - a typed array over the budget yields a `RangeError`;
  - host allocation after the budget is spent, through `in_own_process!`: the script fills the
    budget and catches its own `RangeError`, then awaits `new Blob(['x']).arrayBuffer()`, which must
    reject with a `RangeError` while the child process stays alive and the isolate serves the next
    request; the control runs the same host call under the budget and resolves;
  - a host `from_vec` buffer (a request body read with `arrayBuffer()`) larger than the remaining
    budget rejects with a `RangeError`, and after it is dropped and collected the reservation is
    released, so an equal read succeeds;
  - `SharedArrayBuffer`: the global is absent in a creator context, and a fixed buffer made through
    `new WebAssembly.Memory({shared: true, ...}).buffer.constructor` larger than the budget yields a
    `RangeError`; a growable one is asserted to grow past the budget today, as the stated residual,
    until the follow-up lands and the case flips to a refusal;
  - Wasm: a `WebAssembly.Memory` whose `initial` is above the pin is refused and growth stops at the
    pin; several memories each at the pin are asserted to exceed the budget today, as the stated
    residual, until the follow-up lands;
  - a resizable `ArrayBuffer` whose max is over the budget is asserted to allocate today, as the
    stated residual, until the follow-up lands.
  Fail-before: without the allocator and helper the over-budget fixed, typed and host allocations
  succeed, so the refusal assertions fail; with the allocator but without the helper the host-call
  child aborts, so that case fails.
- *Mutations:* make the allocator's over-budget branch allocate anyway (the fixed and typed refusals
  fail); bypass the helper's reservation so the host call goes straight to `v8::ArrayBuffer::new` (the
  host-call child aborts and the case fails); drop the deleter-side release (the equal re-read is
  refused); drop `--enable-sharedarraybuffer-per-context` (the global reappears); drop
  `--wasm-max-mem-pages` (the over-pin `initial` succeeds).

**Change 2 -- loop-optimization lever.**
- *Touches:* `init_v8_platform` (`crates/zeroship-runtime/src/core/init.rs`).
- *Test* (`crates/zeroship-runtime/tests/integration/heap_limits.rs`, run through `in_own_process!`
  in `crates/zeroship-runtime/tests/support/mod.rs`): the non-retaining constant-size-array loop,
  warmed so the body is TurboFan-optimized, under a CPU limit, is terminated rather than running to
  its own break; the body sits between `kElementLoopUnrollLimit` and `kMaxIterForStackCheckRemoval`
  so it is the shape that hangs today, and the case asserts termination, not self-break, so it is not
  vacuous. A control with a below-limit length terminates with or without the flag, proving the test
  distinguishes the shape. Fail-before: without the flag the warmed loop is not terminated.
- *Mutation:* remove the flag; the warmed loop is not terminated and the test fails.

**Change 3 -- hang detector.**
- *Touches:* `crates/zeroship-runtime/src/core/cpu_timer.rs` (the watchdog escalation and the
  plan-independent ceiling) and the enter/exit epoch marker in
  `crates/zeroship-runtime/src/core/runtime.rs`.
- *Tests* (`crates/zeroship-runtime/tests/integration/`, through `in_own_process!`): a loop that
  ignores a requested termination drives the watchdog, after its bound, to exit the process with the
  attributed hang record naming the app; a second case on a plan with no CPU limit (the unlimited
  plan's shape) wedges with no termination outstanding and is still exited by the plan-independent
  ceiling; a control whose loop honors termination is stopped without a process exit, so the detector
  does not fire on the ordinary case. Fail-before: without the escalation each child runs to the
  harness timeout.
- *Mutation:* make the escalation a no-op (both children hang and the tests fail); remove the
  plan-independent ceiling so it waits on an outstanding termination (the no-CPU-limit child hangs).

**Change 4 -- OOM handler and quarantine.**
- *Touches:* the handler and the enter/exit marker in
  `crates/zeroship-runtime/src/core/runtime.rs`; the boot-time file descriptor and the next-boot
  shipping in `crates/zeroship-worker/src/main.rs` and the join path
  (`crates/zeroship-worker/src/join.rs`); the control-side ingestion, classification and the
  org-keyed hold with its lift and audit, a new deployment-hold surface in
  `crates/zeroship-control/src/` (not the workflow `archive` / `DISABLE` lifecycle, which is
  app-level calendar control, not a hold).
- *Tests:* (i) an `in_own_process!` case that drives a real isolate past its cap with the grants
  exhausted and asserts the child's crash record names the app and classifies the crash as a heap
  out-of-memory; a control that crashes for an unrelated reason is not classified as an app heap
  out-of-memory. (ii) a control test that ingests corroborated heap-out-of-memory records for one
  (app, organization) and holds the deployment, with negative controls (a single uncorroborated
  record, and an under-threshold or non-heap record, hold nothing) and a lift-path case.
  Fail-before: neither the handler nor the hold rule exists, so both assertions fail.
- *Mutations:* (i) delete the handler registration; the record is absent. (ii) make the hold ignore
  corroboration or crash kind; a negative control now holds and the test fails.

**Change 5 -- capability closures.**
- *Touches:* `Isolate::set_allow_atomics_wait(false)` in `RuntimeInner::new_with_plugins`
  (`crates/zeroship-runtime/src/core/runtime.rs`); removal of the `gc()` global at context setup,
  since `--expose-gc` in `init_v8_platform` exposes it process-wide; the in-tree tests that call
  `gc()` from script (`crates/zeroship-runtime/tests/integration/v8_async_method_smoke.rs`) move to a
  host-side forced collection.
- *Tests:* a script calling `Atomics.wait` throws rather than blocking the thread; `gc` is
  `undefined` in a creator context, while the host can still force a collection. Fail-before: today
  `Atomics.wait` blocks and `gc` is a function in a creator context.
- *Mutations:* drop the `set_allow_atomics_wait(false)` call (the wait blocks again); drop the `gc()`
  removal (the global reappears).

**Change 6 -- process budget.**
- *Touches:* an admission check in `crates/zeroship-worker/src/cache.rs` (`load_app` / `evict_lru`)
  against a process budget derived from the container memory limit, read at boot in
  `crates/zeroship-worker/src/main.rs`. Each admitted isolate is charged its heap cap, its unwind
  headroom and its external budget, so one app's process-wide external total is its budget times the
  number of isolates it holds across worker threads and live workflow isolates.
- *Test* (`crates/zeroship-worker/src/cache.rs` tests): loading isolates whose caps, unwind headroom
  and external budgets together would pass the process budget is refused or triggers eviction rather
  than admitting the sum; a case whose caps alone fit but whose external budgets do not is also
  refused, so the external term is bound; a control under the budget admits normally. Fail-before:
  today the sum is unbounded and the load always admits.
- *Mutations:* make the admission ignore the budget (the over-budget load admits); drop the external
  term from the charge (the case that only the external budgets push over admits).

Order: changes 1, 2 and 5 are self-contained runtime changes, independent of each other and of
control; any lands first. Change 3 depends on the enter/exit marker that change 4 also uses, so the
marker lands with whichever of 3 or 4 is first. Change 6 is worker-local. Change 4 is the largest,
spanning runtime, worker and control. All six are the end state; none is a transitional half-state.

## Open questions for the operator

1. **The external budget size.** The per-plan external budget is a new operator-set value beside the
   heap cap. Should it equal the heap cap, be a separate larger number, or be a single combined
   memory number the allocator derives the external share from.
2. **The speed cost of the loop lever.** `--no-turboshaft-loop-unrolling` must be benchmarked before
   it ships, since it costs peak JavaScript speed for every tenant and binds the migrate recorder and
   test binaries too. If the cost is unacceptable, the fallback is Option 3 (the patch) or relying on
   changes 3 and 4 alone until the upstream fix lands, accepting one stopped worker per offending
   deployment.
3. **The quarantine policy.** How many corroborating instances and over what window hold a
   deployment; the per-organization escalation; whether a redeploy clears a hold (and the abuse of a
   deploy-to-reset path); the creator-facing message; and who authorizes a lift.
4. **The process budget.** The admission rule or operator sizing that keeps the sum of caps plus
   headroom under the container limit, so a cgroup kill is not the enforcement mechanism.
5. **The stated residuals: resizable and growable buffers, shared Wasm memory, and Wasm memory.**
   Each is unbounded per isolate in the pinned crate (see change 1). For each, choose between
   charging its commits to the per-isolate budget through a V8 patch at the commit path, and
   withholding the feature from creator code (disabling resizable and growable buffers, or
   withholding WebAssembly, which also removes the shared-memory route to `SharedArrayBuffer`). A
   per-buffer or per-memory cap alone does not bound the total.
6. **Trust-tier pools.** Whether Option 6 is wanted as the free-tier bound, given it fits the
   execution-zone and ring model but costs bin-packing.
