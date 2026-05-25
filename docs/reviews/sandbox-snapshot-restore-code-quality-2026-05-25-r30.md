# Sandbox snapshot-restore code-quality review — 2026-05-25 r30

**Reviewer**: code-quality r30 (cron-pilot)
**HEAD**: `0ee106d2`. **Prior**: r29 (`ce062846`).
**Scope since r29**: 3 in-crate commits — R29-C1 + r29-A2 class-fix (`62b083e1`), R28-API2 sweep (`9ac5b850`), R29-P1 GC parallelize (`81b6e689`). Plus 2 reviewer-artifact + 2 controller-pin housekeeping commits (no in-crate source).

## Summary

- **6 findings**: 0 critical, 0 important, 6 minor. **R29-P1 + R28-API2 sweep LAND clean; R29-C1 class-fix LANDS clean.**
- **R29-C1 class-fix CLOSED at `62b083e1`** — `spawn_delayed_release` deleted entirely; two production callers (`stop_inner:1393`, `CreateGuard::drop:2358`) now both go through `release_vm_index_after(...).await` inline. The typed-Task escape hatch (`spawn_delayed_release_in_worker`) carries a 17-line rustdoc explaining when `.detach()` is sound vs not. The R29-C1 regression test `release_vm_index_after_survives_short_lived_runtime` (`:4882-4932`) **exercises the helper directly via `detach_isolated`**, not the full `snap-teardown-<tail>` → `stop_preserving_state` → `stop_inner` call chain — adequate for pinning the helper invariant; see R30-M2 for the test-scope tradeoff.
- **R28-API2 CLOSED at `9ac5b850`** — 4 test-pub items cfg-gated, 1 (`Database::set_role_dsns_for_test`) deleted. Cfg-gate discipline verified: **all four sites apply `#[cfg(any(test, feature = "test-support"))]` uniformly to the item AND each impl block, no split**. Pre-launch "no back-compat" rule honoured for the dead helper.
- **R29-P1 LANDS at `81b6e689`** — `GcStopper` trait mirrors `sweep::IdleSnapshotter` correctly modulo the deliberate omission of `Send + Sync` bounds (justified — local `&dyn` use, not `Arc<dyn>`). `gc_stop_chunked` design parallels `sweep::snapshot_rows_chunked`. **One latent panic-propagation hole in the production wire surfaced — see R30-M1.**
- **No new bare `unwrap()` / `expect()` in production this cycle.** Verified: `nomad_ch.rs` adds zero bare unwraps (the helper rustdoc + body use `unwrap_or_else(|p| p.into_inner())`); `registry.rs:951-988` `AppStateGcStopper` adds no unwraps; the `release_vm_index_after`/`spawn_delayed_release_in_worker` bodies use the established poison-recovery pattern.

## Carry table

| Finding | r29 state | r30 state |
| --- | --- | --- |
| **R29-C1** spawn_delayed_release on detach_isolated → vm_index leak | OPEN CRITICAL (concurrency-r29) | **CLOSED at `62b083e1`** (helper deleted; both prod callers use `release_vm_index_after.await`) |
| **R28-API2** test-scaffolding pub items reach prod rlib | OPEN IMPORTANT (api-surface-r28) | **CLOSED at `9ac5b850`** (4 gated + 1 deleted; cfg-gate applied uniformly) |
| **R29-M1** `ClockResyncOutcome` classifier string-matches underlying error | OPEN | OPEN, unchanged (no touch this cycle) |
| **R29-M2** dead `Ok` arm in half-dead-agent rollback path | OPEN | OPEN, unchanged |
| **R29-M3** R28-M1 carry — `Any { .. }` panic-payload sites | OPEN, 14 sites | OPEN, **count unchanged** (no new sites this cycle; `spawn_delayed_release_in_worker:432` adds a typed `Box<dyn Any + Send>` in return type but doesn't `format!` with `{:?}` so doesn't add to the carry) |
| **R28-M2 / R29-M5** silent `unwrap_or(0)` on `SystemTime::now()` | OPEN, 2 sites | OPEN, unchanged |
| **R29-M4** test `format!` block-argument shape | OPEN | OPEN, unchanged |
| **R29-M6** heredoc-comment foot-gun shape | OPEN | OPEN, unchanged (no touch this cycle) |
| **R27-M1/M3/M4/M5** carry minor cosmetic finds | OPEN | OPEN, unchanged |

## MINOR (new this round)

### [R30-M1] `AppStateGcStopper::stop_one` poisoned-`get()` panic propagates out of `gc_stop_chunked` and kills the snap-idle-gc thread — `catch_unwind` covers only `expired()`

**File**: `crates/sandbox/src/registry.rs:955-988` (`AppStateGcStopper::stop_one`) + `:1023-1046` (the calling loop).

```rust
impl GcStopper for AppStateGcStopper {
    fn stop_one<'a>(
        &'a self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + 'a>> {
        let state = self.state.clone();
        Box::pin(async move {
            if let Some(info) = state.sandboxes.get(&id) {       // <— `read().unwrap()` inside; can panic
                tracing::info!(...);
            }
            if let Err(e) = state.backend.stop(id).await { ... }
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                state.sandboxes.remove(&id);                      // <— wrapped
            }));
            Ok(())
        })
    }
}
```

**Why a smell**: `state.sandboxes.get(&id)` is `crate::registry::SandboxRegistry::get` at `:240-245`, which calls `self.by_sandbox.read().unwrap()` — bare `.unwrap()`, panics on lock poison. The `remove` call at the tail IS wrapped in `catch_unwind`; the `get` call at the head is NOT. Asymmetric.

Pre-R29-P1 (the deleted serial loop at `registry.rs:854-878` baseline) had this same asymmetry, so the smell is not new — but the consequence sharpened. In the serial-loop world a `get()` panic killed the snap-idle-gc thread mid-iteration; the next chunk of `to_kill` was never processed but `backend.stop(prior_id)` had at least completed serially. In the R29-P1 world the panic propagates up through `futures::future::join_all().await`, which **drops all (cap − 1) sibling in-flight `stop_one` futures mid-poll**. Each sibling holds `state.backend.stop(id)` in progress — already past `state.write().remove(&sandbox_id)` (line 1232 of `nomad_ch.rs::stop_inner`) but mid `wait_for_agent_silent` or pre-`release_vm_index_after`. The Nomad job has been purged but the vm_index is never returned to the allocator. **Each cancelled sibling leaks one vm_index until the next controller-boot orphan prune.**

Severity bump from r29 baseline: serial loop = 1 stop completes + thread dies; R29-P1 = up to `cap=8` stops abort mid-`stop_inner` + thread dies + up to 7 vm_indices leaked.

This is **NOT a R29-P1 regression by intent** — the panic only fires on poisoned `RwLock`, which only happens if a writer (`insert`/`remove`/`set_preview_*`) previously panicked while holding the lock. Today no writer panics inside the critical section; the smell is potential, not active. But:

1. **Easy fix**: wrap the entire `stop_one` body in `catch_unwind(AssertUnwindSafe(...))` so a poisoned `get()` becomes a logged warning + `Ok(())` instead of propagating. Symmetric with the `remove` wrap.
2. **Defense-in-depth**: also wrap `gc_stop_chunked(&to_kill, &stopper, ...).await` itself in `catch_unwind` at the loop site (`:1045`), mirroring the `expired()` wrap at `:1028`. A panic-safe `stop_one` makes this redundant but the symmetry with the rest of `start_idle_gc` is the operator-facing property: every chunk-of-work the snap-idle-gc loop touches has a panic-firewall.

The rustdoc on `start_idle_gc` at `:600-606` explicitly cites the prior version's mistake — *"A previous version held a single `compio::runtime::spawn(...).detach()` — any panic in the loop (RwLock poisoning, malformed UUID, weird backend error) killed the task forever, with no log saying GC died."* — and claims the wrap closes it. R29-P1 added a new unwrapped call site that re-opens the same hole.

**Severity**: MINOR. The poison path doesn't fire today. The asymmetry with the `expired()` wrap (catch_unwind) and the explicit rustdoc claim ("we now wrap each iteration") makes this a structural inconsistency worth closing.

---

### [R30-M2] R29-C1 regression test exercises the helper, not the production call chain

**File**: `crates/sandbox/src/backend/nomad_ch.rs:4862-4932` (`release_vm_index_after_survives_short_lived_runtime`).

```rust
let pool_for_fut = Arc::clone(&pool);
crate::detach::detach_isolated(
    "test-r29-c1",
    move || async move {
        VmIndexAllocator::release_vm_index_after(    // <— called directly
            pool_for_fut, allocated,
            Duration::from_millis(100), "test-r29-c1", Uuid::nil(),
        )
        .await;
    },
);
```

**Why a smell**: the production R29-C1 bug shape was `detach_isolated("snap-teardown-<tail>", ...) → teardown_source_for_snapshot → stop_preserving_state → stop_inner → spawn_delayed_release (now release_vm_index_after.await)`. The test exercises step 1 (`detach_isolated`) + step 5 (`release_vm_index_after`) directly, skipping steps 2-4. The R28-DISCIPLINE rule ("verify the test fixture actually reproduces production") asks whether a future refactor that breaks the production chain would still let this test pass.

Cases where this test passes but production R29-C1 returns:
- `stop_inner` refactor that bypasses `release_vm_index_after` and inlines a `compio::runtime::spawn(...).detach()` directly for the release.
- `stop_preserving_state` refactor that interposes a fire-and-forget detach between itself and `stop_inner`.
- A new caller path (e.g. cleanup admin endpoint) that calls `release_vm_index_after` but accidentally wraps it with `compio::runtime::spawn(...).detach()`.

The corresponding R28-C1 test (`create_guard_drop_releases_vm_index_under_isolated_runtime`, `:4670-4728`) DOES go through the full `CreateGuard::drop` → `detach_isolated("create-rollbk", ...)` chain — exercising the production wire at step 1 and the helper at step 2. The asymmetry between the two regression tests is the smell: R28-C1 tests the production caller; R29-C1 tests the helper.

**Why the author may have chosen this scope**: the full R29-C1 chain requires a working `NomadCHBackend` + mock Nomad HTTP responses for `stop_inner`'s `/shutdown`, `stop_nomad_job`, and `wait_for_job_gone` HTTP calls. The unit-test scaffolding doesn't exist; building it would be a 100+ line fixture. The helper-only test is 50 lines and pins the load-bearing invariant (timer must survive runtime drop). The tradeoff is documented at `:4868-4877` ("we don't need the full stack; what we're pinning is the helper's behaviour").

**Fix shape if a future cycle wants to close this**: add a second R29-C1 regression test that wires `NomadCHBackend::stop_preserving_state` against a mock Nomad agent (the integration-test fixture under `crates/sandbox/tests/sandbox_*` already has the scaffolding pattern). Or move the assertion up to an integration test that exercises the admin-snapshot endpoint end-to-end.

**Severity**: MINOR. The helper-level test catches the load-bearing invariant; the production-chain refactor that would defeat it is unlikely (the helper is named and rustdoc'd specifically for this case). The smell is "the R28-C1 test sets the bar higher than the R29-C1 test does."

---

### [R30-M3] `#[allow(dead_code)]` on `spawn_delayed_release_in_worker` is redundant — `pub fn` doesn't fire `dead_code` lint

**File**: `crates/sandbox/src/backend/nomad_ch.rs:420-443`.

```rust
/// No current call site in this crate uses this helper; it
/// exists as the type-safe escape hatch for any future
/// background-task path that needs fire-and-forget delayed
/// release on a long-lived runtime without blocking the caller.
#[allow(dead_code)] // typed escape hatch — see rustdoc
pub fn spawn_delayed_release_in_worker(...) -> compio::runtime::Task<...> { ... }
```

`#[allow(dead_code)]` on a `pub` fn in a library crate is a no-op — the lint already treats `pub` as a "may be used by an external crate" signal and doesn't warn. The function IS called by two unit tests (`release_vm_index_after_honors_configured_delay:4783` and `spawn_delayed_release_in_worker_returns_joinable_task:4836`), so even if rustc were stricter, the lint wouldn't fire.

If the intent is "warn ME if this becomes unreachable" — the right tool is `#[cfg(test)]` (which would also gate it from the production rlib, free `test-support` parity) OR a custom CI lint. `#[allow(dead_code)]` is the wrong knob.

A future caller wiring the typed escape hatch will not be warned by removing the annotation; conversely, a contributor who deletes the last test caller won't be warned because `pub` shields them. The annotation reads as a hint to readers ("yes, I know there's no production caller") which is already covered by the 4-line rustdoc directly above. Drop the attribute; let the rustdoc carry the message.

**Severity**: MINOR. Cosmetic. The annotation is harmless; the smell is the implication that it does something it doesn't.

---

### [R30-M4] `GcStopper` trait omits `Send + Sync` bounds where `IdleSnapshotter` carries them — asymmetric but intentional; rustdoc should call this out

**Files**: `crates/sandbox/src/registry.rs:939-944` vs `crates/sandbox/src/sweep.rs:481-486`.

```rust
// sweep.rs
pub trait IdleSnapshotter: Send + Sync {
    fn snapshot_one<'a>(...) -> Pin<Box<dyn Future<Output = Result<(), String>> + 'a>>;
}

// registry.rs
trait GcStopper {                       // <— no Send + Sync
    fn stop_one<'a>(...) -> Pin<Box<dyn Future<Output = Result<(), String>> + 'a>>;
}
```

The asymmetry is real. `IdleSnapshotter` is wrapped as `Arc<dyn IdleSnapshotter>` and passed to `spawn_idle_eviction_sweep` (`:768-770`), which requires `Send + Sync`. `GcStopper` is used only as `&dyn GcStopper` inside `gc_stop_chunked` (`:1001`), so the bounds aren't structurally necessary.

But the rustdoc on `GcStopper` at `:933-938` says *"Trait shape mirrors `sweep::IdleSnapshotter` so the chunked-concurrency helper has a single uniform interface…"* — claiming parity. A reader who notices the missing `Send + Sync` and assumes it's an oversight would add them defensively, possibly breaking the test fixture (`SleepingGcStopper`) which captures `&AtomicUsize` (Send + Sync) and is fine, but the cost is non-zero on the next refactor.

Fix-shape: one of —
1. Add `Send + Sync` for true parity (zero behavioural change today; defends a future `Arc<dyn GcStopper>` use case).
2. Document the asymmetry in the rustdoc: *"Unlike `IdleSnapshotter`, this trait is not `Send + Sync` — the snap-idle-gc loop holds the stopper as `&dyn` only, never `Arc<dyn>`. If a future caller needs to share across tasks, add the bounds then."*

**Severity**: MINOR. The current shape is correct; the rustdoc's "mirrors sweep::IdleSnapshotter" claim is one bound shy of accurate.

---

### [R30-M5] `gc_stop_chunked` ignores per-future results; `IdleSnapshotter::snapshot_one` failures get logged per-row but `GcStopper::stop_one` returns are silently dropped

**File**: `crates/sandbox/src/registry.rs:1001-1006`.

```rust
async fn gc_stop_chunked(ids: &[Uuid], stopper: &dyn GcStopper, cap: usize) {
    let cap = cap.max(1);
    for chunk in ids.chunks(cap) {
        let _ = futures::future::join_all(chunk.iter().map(|id| stopper.stop_one(*id))).await;
    }
}
```

`futures::future::join_all` returns `Vec<Result<(), String>>` — discarded via `let _ = ...`. The rationale comment at `:998-1000` says *"One stop failing does NOT abort the chunk — the `GcStopper::stop_one` impl already wraps every error path as a log line + `Ok(())`."* — accurate for `AppStateGcStopper`, since it logs the `backend.stop` error and returns `Ok(())`.

Compare to `sweep::snapshot_rows_chunked` at `:747-755`:

```rust
let results = futures::future::join_all(...).await;
for ((row, _), result) in parsed.iter().zip(results) {
    if let Err(e) = result {
        tracing::warn!(sandbox_id = %row.sandbox_id, error = %e, "...");
    }
}
```

The sweep helper logs per-row failures from the result vec; the GC helper discards them. The difference is that `IdleSnapshotter::snapshot_one` can return `Err` (the production impl returns errors from `lookup_source_vm_ops`, missing wiring, etc.), while `AppStateGcStopper::stop_one` returns `Ok(())` unconditionally (errors are logged inside).

This **couples** `gc_stop_chunked`'s correctness to `AppStateGcStopper`'s invariant ("always returns Ok"). A future second `GcStopper` impl that returns `Err` would have its errors silently discarded. The `let _ = ...` is precisely the foot-gun.

Two fix-shapes:
1. Match `sweep::snapshot_rows_chunked`'s pattern: collect the results and log any `Err`.
2. Change `GcStopper::stop_one`'s return type to `()` instead of `Result<(), String>` — the trait API codifies "log-and-swallow inside" as the contract.

Option 2 is structurally cleaner (the type system enforces the invariant the rustdoc currently asserts).

**Severity**: MINOR. The discarded `Result` is unreachable today by the one impl's contract. The smell is that the helper's API surface promises a fan-out over `Result<(), String>` but the calling code can't observe failures — either change the contract or honour it.

---

### [R30-M6] `GC_STOP_CONCURRENCY = 8` is a hard-coded const where `sweep::DEFAULT_PER_WORKER_CONCURRENCY = 2` is env-overridable

**Files**: `crates/sandbox/src/registry.rs:931` vs `crates/sandbox/src/sweep.rs:64`.

```rust
// registry.rs
const GC_STOP_CONCURRENCY: usize = 8;
```

vs the sweep crate which exposes its concurrency cap as an env-overridable config (`SANDBOX_SNAPSHOT_PER_WORKER_CONCURRENCY`, default 2, parsed at `sweep.rs:1158`-ish). The R29-P1 commit message at `81b6e689` cites `cap=8 keeps peak parallel work bounded against Nomad (each stop drives a /shutdown request — too many at once would just trade one starvation for another)` — the value is operator-tunable in principle.

A future operator hitting Nomad-API contention (e.g. controller-host fleet doubled, but Nomad addr unchanged) would want to lower the cap; one hitting GC backlog growth (the original R29-P1 shape worsens) would want to raise it. Today neither is possible without a code change.

Fix-shape: lift to `SANDBOX_GC_STOP_CONCURRENCY` env var with default 8, parsed at `start_idle_gc` boot and threaded down to `gc_stop_chunked`. Or — cheaper — keep the const but bump it to a `pub` symbol in a `crate::sweep`-adjacent module so it's grep-discoverable.

The asymmetry with `DEFAULT_PER_WORKER_CONCURRENCY` (env-overridable, public const, rustdoc explains the budget) makes the GC cap look like a one-off rather than a deliberate counterpart.

**Severity**: MINOR. The hard-coded value is correct for today's deployment. The smell is the operator-knob asymmetry with the sibling sweep helper.

---

## Cleanliness verification

### Production `unwrap()` / `expect()` since r29 baseline

- `nomad_ch.rs`: `release_vm_index_after` body (`:383-404`) — `unwrap_or_else(|p| p.into_inner())` on the allocator mutex (consistent with the file's other 4 sites). Zero new bare unwraps.
- `nomad_ch.rs::spawn_delayed_release_in_worker` (`:425-443`) — zero unwraps (delegates to `release_vm_index_after`).
- `registry.rs::AppStateGcStopper::stop_one` (`:955-988`) — zero unwraps in the helper body. The transitive `state.sandboxes.get(&id)` IS a lock-unwrap call (see R30-M1), but pre-existing.
- `db.rs::from_test_config` — zero unwraps (already used `.map_err` correctly pre-cfg-gate).
- `restore_handler.rs::StubRestoreBackend` impls — unwraps under `Mutex` poison-recovery pattern (consistent).

Production unwrap discipline holds for this round's new code.

### Cfg-gate uniformity (R28-API2 sweep)

Audited each of the four gated files for split attribute application:

| File | Item attr | `impl Type {}` | `impl Trait for Type {}` |
|---|---|---|---|
| `restore_handler.rs:1305` | `#[cfg(any(test, feature = "test-support"))]` | `:1330` — same | `:1355` — same |
| `snapshot_handler.rs:694` | `#[cfg(any(test, feature = "test-support"))]` | `:703` — same | `:715` — same |
| `sweep.rs:495` | `#[cfg(any(test, feature = "test-support"))]` | (no inherent impl) | `:501` — same |
| `db.rs:522` | `#[cfg(any(test, feature = "test-support"))]` | (free fn, not in trait impl) | (no trait impl) |

All four uniform. No item-gated-but-impl-not (or vice-versa) split.

### Cargo.toml self-dev-dep

`crates/sandbox/Cargo.toml:77,79-84` — already present from R27-API2 (`c2e07b2f`), unchanged this cycle. The 9ac5b850 commit message confirms `Cargo.toml unchanged`. Verified by file inspection.

### R29-C1 fix — production call-site audit

```
$ grep -n 'spawn_delayed_release\b\|release_vm_index_after\b' crates/sandbox/src/backend/nomad_ch.rs
…
383:    pub async fn release_vm_index_after(            <— helper def
425:    pub fn spawn_delayed_release_in_worker(         <— typed escape hatch
440:        compio::runtime::spawn(Self::release_vm_index_after(    <— escape hatch body
1393:            VmIndexAllocator::release_vm_index_after(           <— stop_inner caller
2358:                        VmIndexAllocator::release_vm_index_after(   <— CreateGuard::drop caller
```

Two production callers, both inline-await. Pre-r29 fire-and-forget shape gone. Class-fix complete.

The R29-C1 regression test does not exercise the `stop_inner` chain end-to-end (see R30-M2), but the helper-level invariant is pinned: any caller awaiting the helper inside a short-lived runtime IS guaranteed to observe the release before runtime drop.

### `compio::runtime::Task` return type — contract verification

`spawn_delayed_release_in_worker:431-433` returns `compio::runtime::Task<Result<(), Box<dyn std::any::Any + Send>>>` — the explicit `Result` wrapper acknowledges compio's `spawn` panic-catch behaviour. The companion test `spawn_delayed_release_in_worker_returns_joinable_task:4831-4860` includes a **compile-time shadowing assertion**:

```rust
let task: compio::runtime::Task<
    Result<(), Box<dyn std::any::Any + Send>>,
> = task;
```

This is the right idiom — if a future refactor changes the return type, the shadowing assertion fails to compile and the rustdoc can't silently drift. Good discipline.

### R29-P1 fan-out math — wall-time check

`gc_stop_chunked_is_actually_concurrent` at `:849-918`:
- `n = 10`, `cap = 8`, `per_stop = 2 s`.
- Serial floor: 20 s. Parallel ideal: 4 s (one full chunk of 8 + a partial chunk of 2).
- Wall-time assertion: `< 5 s` (25 % margin above ideal; 75 % margin below serial floor).
- Semantic assertion: `max_in_flight == cap` (defends against partial-overlap regressions where `join_all` polls only one future at a time).

The commit message reports measured local wall `elapsed=4.000218719s max_in_flight=8` — matches ideal, fan-out confirmed real. Test design is robust.

## Bottom line

r30 lands clean on R29-C1 (the critical concurrency fix), R28-API2 (the api-surface sweep), and R29-P1 (the GC-backpressure fix).

- **Zero critical findings.** Zero important findings.
- **R30-M1** is the most actionable new smell — R29-P1's new `stop_one` call site re-opens the panic-firewall gap that `start_idle_gc`'s rustdoc explicitly claims is closed. Easy fix: wrap `stop_one` body in `catch_unwind` (or also wrap the `gc_stop_chunked` call site). Pairs with the existing `expired()` catch_unwind for symmetry.
- **R30-M2** is a test-scope tradeoff: the R29-C1 regression test exercises the helper, not the full `stop_preserving_state → stop_inner` chain. Asymmetric with R28-C1's test which DOES exercise the full `CreateGuard::drop` chain. Worth closing if/when integration-test scaffolding for `NomadCHBackend::stop_preserving_state` exists.
- **R30-M3** is `#[allow(dead_code)]` on a `pub` fn — redundant. Cosmetic.
- **R30-M4** is the `Send + Sync` asymmetry between `GcStopper` and `IdleSnapshotter` — correct as designed, mis-described in rustdoc.
- **R30-M5** is the `let _ =` on `futures::future::join_all` results — couples helper correctness to one impl's contract. Either log per-result (sweep parity) or change return type to `()`.
- **R30-M6** is the operator-knob asymmetry — `GC_STOP_CONCURRENCY` is a hard-coded const where `DEFAULT_PER_WORKER_CONCURRENCY` is env-overridable.

**Code-quality lens reads HEAD `0ee106d2` as production-ready.** Three of the four open r29 carries (R28-API2, R29-C1, R29-I1 closed earlier) are now closed; the remaining minor carries (R29-M1 through R29-M6) are unchanged from r29 with no new pressure to close. The R29-P1 trait extraction is structurally aligned with the `sweep::IdleSnapshotter` precedent and the chunked-fan-out helper is parametrically tested. The R29-C1 class-fix correctly identifies the helper itself (not just one caller) as the load-bearing surface and deletes the foot-gun.

The R29-P1 + R29-C1 + R28-API2 trio represents a strong cycle: a stress-driven regression (vm_index exhaustion under N=12 GC tick), root cause identified as caller-side serial loop interacting with the just-landed R29-C1 inline-await, trait-extraction parallelisation as the structural fix, and full test coverage at both wall-time and semantic levels. The pattern is becoming a template the codebase reuses (sweep, GC) — the `Send + Sync` trait-bound asymmetry (R30-M4) is the only place the parallel doesn't quite hold.

Next round's likely surface: the R29-M1 typed-wrapper string-match still-OPEN finding; R30-M1 catch_unwind symmetry if a contributor has a quiet 10 minutes; the integration-test scaffolding for `NomadCHBackend` that would close R30-M2.
