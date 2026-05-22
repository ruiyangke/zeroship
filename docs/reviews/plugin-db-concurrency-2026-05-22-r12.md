# plugin-db concurrency / lifecycle review — 2026-05-22 r12

**Commit:** `0bf71f27` (HEAD; post-r11 cycles 14:17 + 14:47 + 15:17)
**Lens:** concurrency + lifecycle (round 12, post-capture-layer re-walk)
**Prior:** `…-r11.md` (89, +1 vs r10), `…-r10.md` (88), `…-r9.md` (88).

**Cycle commits audited this round** (per brief):

- `f6adb68b` — privatize 11 `IsolateDbContext` fields. Surface-only.
- `bf75e866` — compio-postgres `test-utils` feature. Test-utility only.
- `75d9ae5c` — `bench_row_to_json` harness. Bench-only.
- `91771aaf` — demote 27 `IsolateDbContext` accessors to `pub(crate)`.
  Surface-only.
- `4e9dbafb` — `bench_first_row_or_null` harness. Bench-only.
- `0bf71f27` — `tracing-subscriber` capture layer + 11 tests
  (`set_mig_lock`, `destructive_invariant_error`, 2 documentation
  snapshots, 6 self-tests, 1 negative case).

**Adjacent (in window, not in brief list):** `18aee490`
(finalise_backfill F1 warn-shape unification — observability-only field
rename), `251d53b4` (`row_to_json` O(N²)→O(N), pure-sync perf fix on a
non-async function).

---

## Headline

**Plateau holds at 89.** None of the cycle's six brief-listed commits
touches a synchronisation surface; the two adjacent commits
(`18aee490`, `251d53b4`) are observability-only and pure-sync
respectively. No new findings; no carries closed; no regressions.

- **2a (run_sql cancel pending_emits residue):** `exec.rs`,
  `v8_classes/transaction.rs`, `orchestrator/transaction.rs` —
  `git diff 81226451..0bf71f27` is empty. **Byte-identical to r11.**
- **2b (`exec_commit_batch` is_done window):** `migrations.rs:614-681`
  has only the `18aee490` rename inside the `finalise_backfill` warn
  (`error→audit_err`, `terminal→transition`); zero new async points.
  The cancellation window between `release_advisory_lock.await` and
  `clear_mig_lock()` at `:679` is **unchanged**.
- **Broker waker sites:** `broker.rs:319-321 / :325-327 / :359-369` —
  `git diff` empty. **Byte-identical to r11.**
- **WAL consumer:** `git diff` empty.
- **Cross-isolate / PENDING_EMITS / TX_CONN ownership:** untouched.

Score **89 / 100 (Δ = 0 vs r11)**. The plateau math from r10/r11 is
re-validated: the headline is gated by 2a + 2b; this cycle's commits
are orthogonal to both.

---

## I-R11-1 (accessor demotion) — semantic re-check

`91771aaf` demoted 27 accessors from `pub` to `pub(crate)`. Verified
via `git show` line-by-line: every change is a `pub fn` → `pub(crate)
fn` token swap. Bodies are identical:

- `next_tx_token`: `self.tx_token_counter = self.tx_token_counter.wrapping_add(1); self.tx_token_counter`
  — unchanged.
- `set_tx_token`: `debug_assert!(token == 0 || self.tx_conn.is_some(), …); self.tx_token = token;`
  — unchanged.
- `set_auto_tx_owned`: `debug_assert!(!owned || self.tx_conn.is_some(), …); self.auto_tx_owned = owned;`
  — unchanged.
- `push_pending_emit`: `self.pending_emits.get_or_insert_with(Vec::new).push(ev);`
  — unchanged.
- `drain_pending_emits`: `self.pending_emits.take().unwrap_or_default()`
  — unchanged.
- `clear_pending_emits`: `self.pending_emits = None;` — unchanged.
- `install_tx_client` / `take_tx_client` / `put_tx_client`: identical bodies.
- `try_mark_consumer_running`: `self.running_consumers.insert(app_id.to_string())`
  — unchanged.

**No state-machine semantics shifted.** The accessor demotion is a
visibility-only commit; the invariant checks (`debug_assert!` on
non-zero token requiring active tx_conn; `debug_assert!` on
auto_tx_owned true requiring tx_conn) are preserved verbatim.

Combined with `f6adb68b` (field privatization), the cycle tightens
the API surface — the *only* way external code can mutate the state
machine is through the accessors that carry the invariant checks.
This is **defense in depth, not a behavior change**. Net concurrency
impact: zero.

---

## NEW-R11-1 (capture layer + 11 tests) — concurrency footgun audit

The brief's specific concern: does the new `CaptureLayer`
infrastructure introduce a footgun (mutex held across await, shared
state across test threads, restrictive trait bounds)?

### Mutex-across-await — **no**

Two lock sites in `test_support/mod.rs`:

- **Line 154** (`on_event`): `if let Ok(mut buf) = self.events.lock() { if buf.len() < MAX_EVENTS { buf.push(test_event); } }`.
  `Layer<S>::on_event` is **sync** (the trait signature is `fn`, not
  `async fn`). No `.await` reachable from inside the guard. Push is
  O(1) amortised.
- **Line 249** (`capture` return path): `buffer.lock().expect(…).clone()`.
  Called **after** `with_default` returned — `f` has fully completed.
  No `.await` in scope.

`grep -n await crates/plugin-db/src/test_support/mod.rs` returns
nothing. **Clean.**

### Shared state across test threads — **safe by `with_default` semantics**

`tracing::subscriber::with_default` calls `set_default(&dispatcher)`
which writes to a **thread-local** dispatcher slot
(`tracing-core/src/dispatcher.rs` lines confirmed in `~/.cargo/`).
Concurrent tests on other threads see the no-op global default; their
events never reach this layer's buffer.

Each `capture()` call constructs a **fresh** `CaptureLayer::new()`
with a fresh `Arc<Mutex<Vec::new()>>`. The buffer Arc is extracted
once via `layer.buffer()` before the layer is moved into the
registry; after `with_default` returns, the local `buffer` handle
reads the cloned-then-dropped events. No cross-test contamination is
possible.

### Trait bounds — **not restrictive**

`Arc<Mutex<Vec<TestEvent>>>` is `Send + Sync` (Arc is Send+Sync iff
T: Send+Sync; Mutex<T> is Send+Sync iff T: Send; Vec<TestEvent> is
Send because TestEvent's fields are all Send). The `'static` bound is
satisfied trivially. The `Clone` derive on `CaptureLayer` is required
because `Layer::and_then` and the registry-composition combinators
clone the layer internally, and the `Arc` makes that cheap +
buffer-sharing semantics correct.

No production code consumes `CaptureLayer`. The module is `cfg(test)`
only — invisible to release builds, `--features test-helpers` builds,
and integration tests (per the in-file gate note on `lib.rs:117-128`).

### Tests themselves — sync, no shared globals

- **`set_mig_lock_shadow_replace_emits_error_with_prev_and_new`**
  (`context.rs:987-1035`): drives a **local** `IsolateDbContext::new()`
  — never touches `ISOLATE_CTX` thread-local. Pure sync. The
  `set_mig_lock` accessor it exercises is sync.
- **`set_mig_lock_first_install_emits_no_event`**
  (`context.rs:1038-1053`): same shape, negative case.
- **`destructive_invariant_error_emits_named_fields_at_error_level`**
  (`apply.rs:451-491`): calls the sync `destructive_invariant_error(&op)`
  (`apply.rs:309-325`) directly. No global state, no async, no
  `ISOLATE_CTX`. `grep -n "ISOLATE_CTX\|with_mut\|::with(" apply.rs`
  returns nothing in the test mod.
- **`f1_warn_shape_documentation_snapshot`** (`apply.rs:492-545`):
  re-emits the `tracing::warn!` macro syntax verbatim; does not call
  production code at all.
- **`i6_release_advisory_lock_warn_shape_documentation_snapshot`**
  (`migrations.rs:1000-1050`): same pattern — macro re-emit only.
- **6 self-tests** on the capture layer itself: all sync, all use
  locally-constructed strings and primitives.

**Verdict: zero concurrency footgun introduced.** The capture layer is
sync, thread-local-scoped via `with_default`, panic-resilient via
`lock().ok()`, and bounded via `MAX_EVENTS = 1024`. None of the 11
new tests grab a borrow across await, touch global state, or spawn
tasks.

---

## 2a — byte-identical at HEAD

`git diff 81226451..0bf71f27 -- crates/plugin-db/src/exec.rs crates/plugin-db/src/v8_classes/transaction.rs crates/plugin-db/src/orchestrator/transaction.rs`
**returns empty.**

- `exec.rs:43-73` (`run_sql`): take/await/put pattern unchanged.
- `v8_classes/transaction.rs:119-141` (`Transaction::drop`):
  early-out at `:121-122` still bypasses `clear_pending_emits()`
  (which is called at `:139` only on the live-owner path).
- `v8_classes/transaction.rs:224-273` (`end`): client-already-cleared
  early-out at `:240-246` still bypasses `clear_pending_emits()`.

**Status: carry-over IMPORTANT, unchanged.** Fix family unchanged
(one-liner `clear_pending_emits()` at each early-out site).

---

## 2b — finalise_backfill warn renamed, async surface unchanged

`migrations.rs:614-681` at HEAD vs r11:

```diff
-        if let Err(e) = backend
+        if let Err(audit_err) = backend
             .finalise_backfill(&client, app_id, audit_id, terminal, error_message)
             .await
         {
             tracing::warn!(
                 app_id = %app_id, name = %name, collection = %collection,
                 audit_id = audit_id,
-                terminal = ?terminal,
-                error = %e,
+                transition = ?terminal,
+                audit_err = %audit_err,
                 "finalise_backfill failed; …"
             );
         }
```

This is the `18aee490` F1 warn-shape unification — the field-name
rename (`error → audit_err`, `terminal → transition`) brings the 6th
F1 warn site into the unified shape closed by r11's 2c. **Pure
observability.** No new `.await` point; no change to the
`release_advisory_lock.await → drop(client) → clear_mig_lock()`
sequence at `:666-679`.

The cancellation window analysis from r11 carries verbatim:

- Err-but-not-cancelled at `:666`: hits `:670-675` warn arm, falls
  through to `:678 drop(client)` + `:679 clear_mig_lock()`. State
  consistent.
- Cancellation during `:668`'s await: future unwinds; `client` Drop
  closes the PG session (server-side lock auto-released);
  in-process `mig_lock` slot is NOT cleared. **Same hazard as r10/r11.**
- Between await-resolve and clear_mig_lock: zero await points. The
  rename is inside a synchronous `tracing::warn!` macro; macros don't
  introduce awaits.

**Status: carry-over IMPORTANT, unchanged.** Fix unchanged
(`MigLockGuard` RAII whose Drop calls `clear_mig_lock`).

---

## Broker waker sites — byte-identical

`git diff 81226451..0bf71f27 -- crates/plugin-db/src/broker.rs`
**returns empty.**

- `:319-321` (push overflow `inner.waker.take()` + `w.wake()`): unchanged.
- `:325-327` (push normal): unchanged.
- `:359-369` (close): unchanged.

**Status: 3 sites, MINOR-latent, carry-over.**

---

## TX_CONN ownership, PENDING_EMITS, MIG_LOCK ↔ audit_row, WAL, cross-isolate

All untouched at the **synchronisation layer**:

- **TX_CONN ownership**: the 6 mutation sites (`exec.rs:51/55`,
  `v8_classes/transaction.rs:131-135/239/252`,
  `orchestrator/transaction.rs:172`) are unchanged. The accessor
  demotion (`91771aaf`) renamed `pub fn` → `pub(crate) fn` but the
  call-site lines are byte-identical.
- **PENDING_EMITS**: `exec.rs` diff empty; `push_pending_emit /
  drain_pending_emits / clear_pending_emits` bodies unchanged.
- **MIG_LOCK ↔ audit_row**: `set_mig_lock` body unchanged (the
  tracing::error! at `:383` carries the 4-field shape the new test
  pins). `return_mig_client` body unchanged. State machine identical.
- **WAL consumer**: `git diff` empty. No touches this cycle.
- **Cross-isolate**: no new `Send`-bridging, no new thread-locals.
  `compio` single-thread-per-isolate invariant unchanged.

The `f6adb68b` field privatization closes a theoretical drift vector
(future contributor adds a `ctx.tx_token_counter += 1` outside
`next_tx_token`), but the **current** state machine was already
disciplined; no behavior change today.

---

## v8_bridge `row_to_json` — concurrency-irrelevant

`251d53b4` rewrote `column_to_json` to take `idx: usize` instead of
`name: &str` (O(N²) → O(N) via `Row::try_get(idx)`). Both functions
are **sync**, take `&Row` (immutable), allocate a fresh
`serde_json::Map`, and return. `grep -n "await\|spawn\|unsafe\|RefCell" v8_bridge.rs`
returns nothing.

**Not a concurrency surface.** Perf-only.

---

## Findings, structured

### CARRY-OVER IMPORTANT — `run_sql` cancellation pending_emits residue (2a)

```
[IMPORTANT] crates/plugin-db/src/exec.rs:51-55 +
            crates/plugin-db/src/v8_classes/transaction.rs:121-123 +
            crates/plugin-db/src/v8_classes/transaction.rs:240-246
  Status: r6/r7/r8/r9/r10/r11 carry, byte-identical at HEAD.
  Fix: clear_pending_emits() in BOTH early-out paths.
```

### CARRY-OVER IMPORTANT — `exec_commit_batch` is_done window (2b)

```
[IMPORTANT] crates/plugin-db/src/migrations.rs:614-681
  Status: r6..r11 carry. 18aee490 renamed two fields inside the
       finalise_backfill warn (observability-only). Zero new async
       points. Cancellation window semantics unchanged.
  Fix: MigLockGuard RAII (Drop calls clear_mig_lock).
```

### CARRY-OVER MINOR-latent — broker waker borrow_mut spans `w.wake()`

```
[MINOR-latent] crates/plugin-db/src/broker.rs:319-321, :325-327, :359-369
  Status: r10/r11 carry. Three sites total. Byte-identical.
  Fix: extract waker, drop inner borrow, then wake. One commit.
```

### CARRY-OVER MINOR-transient — CIC retry × pool depth

```
[MINOR-transient] backend/postgres.rs (CIC loop). Unchanged.
```

### CARRY-OVER COSMETIC — `OrchestratorLockGuard::release()` infallible Result

```
[COSMETIC] orchestrator/lock_guard.rs:144-183. Unchanged.
```

---

## Invariants that held under r12 audit

- 386f9bf5 lazy ConsumerRunningGuard construction — unchanged.
- e5315083 `mark_consumer_running` cfg-gating — unchanged (now `pub(crate)`).
- All 5 ConsumerRunningGuard lifecycle scenarios — unchanged.
- SuppressGuard ⊂ ConsumerRunningGuard lifetime nesting — unchanged.
- OrchestratorLockGuard 4-layer hardening — unchanged.
- PENDING_EMITS visibility ordering — unchanged.
- TX_CONN ownership: same 6 mutation sites — unchanged.
- `next_tx_token` monotonic discipline — unchanged + tightened by
  field privatization (counter no longer mutable outside the accessor).
- `set_tx_token` / `set_auto_tx_owned` `debug_assert!` invariants — unchanged.
- Compio single-thread per isolate — unchanged.
- `Subscription::next` `.await` discipline — unchanged.
- `with_default` thread-local scoping (capture layer) — verified
  safe; no cross-thread bleed.

---

## Specific brief questions, answered

1. **Does `0bf71f27`'s `Arc<Mutex<Vec<TestEvent>>>` hold the lock
   across any await?** No. Both lock sites (`mod.rs:154`, `:249`) are
   in sync contexts. The trait `Layer::on_event` is sync. The
   post-capture read happens after `with_default` returns. `grep -n
   await crates/plugin-db/src/test_support/mod.rs` returns nothing.

2. **Are the `Send + Sync + 'static` bounds accidentally restrictive?**
   No. They're the natural bounds for `with_default`'s thread-local
   dispatcher install; `Arc<Mutex<…>>` satisfies them without
   constraining what the buffer contains beyond `TestEvent: Send`
   (which a struct of String/HashMap/Level is, trivially).

3. **Did `91771aaf` shift any semantic for `next_tx_token` /
   `set_tx_token` / `set_auto_tx_owned` / `push_pending_emit`?** No.
   Bodies are byte-identical; only `pub fn` → `pub(crate) fn` token
   swap. Verified via `git show 91771aaf -- crates/plugin-db/src/context.rs`.

4. **Does the I6 typed-error `release_advisory_lock` change the
   cancellation window characteristics?** No. The typed Result widens
   the textual width of the is_done branch (handled in r11) and adds
   a sync warn arm; the await on `release_advisory_lock` itself is
   the only cancellation point and is unchanged.

5. **Did the 3 broker waker sites change?** No. `git diff
   81226451..0bf71f27 -- crates/plugin-db/src/broker.rs` is empty.

---

## Plateau math

| Finding | Severity | Lift | Status |
| --- | --- | --- | --- |
| 2a run_sql cancel pending_emits | IMPORTANT | +2 | carry |
| 2b exec_commit_batch is_done window | IMPORTANT | +2 | carry |
| broker push wake ×2 + close ×1 | MINOR-latent | +0.5 | carry |
| OrchestratorLockGuard::release Result | COSMETIC | +0.5 | carry |

**r12 floor:** 89 (unchanged from r11).
**With 2a + 2b landed:** 89 + 4 = 93.
**With broker waker fix (all 3 sites):** 93.5.
**With cosmetic Result tighten:** 94.

The cycle's six brief-listed commits are **all orthogonal** to the
gating IMPORTANTs: 4 are bench/test-utility (no production code
touched), 2 are pure-visibility (no semantic shift). The two
adjacent commits (`18aee490`, `251d53b4`) are observability and
pure-sync perf respectively.

---

## Score: 89 / 100 (r11: 89, Δ = 0)

**Why 0:**

- No commit this cycle touched a synchronisation surface. The capture
  layer is sync, thread-local-scoped, and `cfg(test)`-gated;
  privatization + accessor demotion are surface-only; benches add no
  production code; the I35 perf fix and 18aee490 warn-rename are
  observability-only.
- 2a, 2b, broker MINOR-latent — all carry verbatim. No new findings.
- No regressions; no closures.

**Why not lower:**

- All invariants hold. The capture layer infrastructure is correctly
  scoped — `cfg(test)` only, dev-dep tracing-subscriber, no leak into
  release or `--features test-helpers` builds, no cross-thread state.
- Tests are pure sync over locally-constructed state.

**Why not higher:**

- The two IMPORTANTs persist (2a + 2b). The forcing function for r12
  was test-coverage + bench-tooling + API-surface hardening, not
  lifecycle hardening. As predicted by r10/r11 plateau math, the
  headline does not move without `MigLockGuard` RAII landing and
  `clear_pending_emits()` being added at the two early-out sites.

| Round | Score | IMPORTANTs | New findings | Closed |
| --- | --- | --- | --- | --- |
| r9  | 88 | 2 (carry) | 0 | 0 |
| r10 | 88 | 2 (carry) | 1 MINOR-latent | 0 |
| r11 | 89 | 2 (carry) | 0 | 1 (apply.rs 2c) |
| r12 | **89** | **2 (carry)** | **0** | **0** |

---

## Did anything new surface, or are we still plateau?

**Still plateau, deeply.** Three consecutive rounds (r10, r11, r12)
with the same two IMPORTANTs. r11 closed apply.rs 2c (+1 lift); r12
closes nothing. The cycle's six commits are well-chosen for their
stated purposes (test coverage uplift via capture layer, API surface
tightening, perf measurement infrastructure) but they are
**structurally incapable** of touching 2a or 2b — none of them goes
near `v8_classes/transaction.rs` early-out paths or
`migrations.rs:679`'s `clear_mig_lock()` call.

The next +2 lift requires the lifecycle-hardening cycle r10/r11/r12
have all called for:

1. `clear_pending_emits()` at `Transaction::drop:122` early-out.
   One line. +1.
2. `clear_pending_emits()` at `Transaction::end:244` early-out.
   One line. +1.
3. `MigLockGuard` RAII (Drop calls `clear_mig_lock`) at
   `migrations.rs:328-336`. +2.
4. Broker waker extract-before-wake (3 sites, one commit). +0.5.
5. `OrchestratorLockGuard::release()` drop the Result. +0.5.

Total reachable from current floor: **94**. No new findings expected
without a structural change to the audit surface (e.g., adding
fuzz/loom coverage to the cancellation paths, which lives well above
the plateau).

**Score: 89 / 100 (r11: 89 / 100, Δ = 0).** Plateau confirmed for the
third consecutive round. Gating constraint unchanged.
