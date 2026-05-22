# plugin-db code critique — 2026-05-22 R6

**Score trajectory: 78 (R1) → 85 (R2) → 87 (R3) → 88 (R4) → 89 (R5) → 90 (R6)**

Scope: `crates/plugin-db/` at HEAD (post-`cbbc9059`).
Lens: Rust correctness and idioms only. Architecture / security / perf live
in sibling reviews.

Re-audited fresh against the brief's eight dimensions. The headline
delta this round is the closure of three R5 MAJORs:

- **MAJOR-R5-2** (consumer-running mark race) — `e399eeea` adds the
  `ConsumerRunningGuard` Drop guard.
- **MAJOR-R5-3** (five `coded_sql` duplicates) — `cbbc9059` extracts
  `crate::error::{prefix_message, coded_sql}` and reroutes five sites +
  `replication.rs`.
- **MAJOR-R5-5** (silent unlock-SQL swallowing) — `ffb1e101` (referenced
  in the lock_guard module preamble; pre-cycle commit) replaces the
  `let _ =` with `tracing::warn!`.

Plus `eda96ead`'s `first_row_or_internal` helper that names the
empty-RETURNING predicate once across 3 sites (audit ×2, replication
×1) — a quiet but real DRY win that also pins the regression-test
contract at a single point.

Net progress this cycle: **+1**.

Two R5 MAJORs remain open (MAJOR-R5-1 `init_session` substring-matching;
MAJOR-R5-4 `WalConsumer::new` typed-code loss). Two new findings surface
this round: MAJOR-R6-1 (`ConsumerRunningGuard` doesn't cover the
mark-before-spawn window) and MINOR-R6-1 (the `coded_sql` dedup
introduced a wrapper-pattern asymmetry between 5 modules and
`replication.rs`).

Findings tagged `[CRITICAL] / [MAJOR] / [MINOR] / [INFO]` per the brief,
with `file:line` evidence and verification commands.

---

## Verified recent commits

### `e399eeea` — `ConsumerRunningGuard` Drop guard

**Verified.** `replication_ops.rs:264-284`. The struct lives inside the
async block; on graceful completion of `run_supervised(consumer).await`
the guard's Drop fires its `unmark_consumer_running` call. On panic
inside `run_supervised`, unwind also fires Drop.

What this DOES close: R5 MAJOR-R5-2 case (2) — the inner panic-mid-loop
window. The terminal `unmark_consumer_running` is no longer line-coded
after the await; it's RAII.

What this does NOT close: see MAJOR-R6-1 below — the
mark-happens-before-spawn window. `mark_consumer_running` is still
called from the outer (synchronous) context BEFORE the future is
constructed, so any failure between mark and the future's first poll
leaves the app permanently marked.

### `cbbc9059` — dedup `coded_sql` / `prefix_message` across 5 sites

**Verified.** Two shared helpers in `error.rs:327` and `error.rs:357`:

```rust
pub(crate) fn prefix_message(err: &mut DbError, prefix: &str) { ... }
pub(crate) fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    let mut err: DbError = e.into();
    prefix_message(&mut err, &format!("{context}: "));
    err
}
```

Five per-module thin wrappers (`audit.rs:58`, `auth/bootstrap.rs:24`,
`auth/keys.rs:41`, `auth/session.rs:35`, `diff.rs:40`) now delegate via:

```rust
fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    crate::error::coded_sql(&format!("audit: {context}"), e)
}
```

`replication.rs` takes a different path — drops its module-local
`prefix_message` and imports the shared `crate::error::prefix_message`,
calling it inline against the open-coded `DbError::from_pg(&e)` pattern
(L201-205, L213-216, L229-233, L267-272, L396-399, L538-541, L568-573,
L600-603).

`crate::error` ships **two contract tests** for the shared helper:
`prefix_message_preserves_variant_and_code` (error.rs:678-734) and
`prefix_message_leaves_structured_variants_alone` (error.rs:743-804).
These pin the variant-preserving and structured-variant pass-through
guarantees the SDK contract relies on — strong discipline.

Cost: see MINOR-R6-1 below — the wrapper asymmetry (5 modules wrap;
`replication.rs` doesn't).

### `f7d0961c` — `error.rs` preamble update

**Verified.** `error.rs:1-46`. The module preamble now reads accurately:

> `Result<_, String>` is now (post-[I28] sweep, commit `0049d9be`)
> confined to a small set of deliberate hold-outs:
> - The `validate` stage in `crate::orchestrator::register_model`
>   whose `Err` IS the `validation_refused` JSON envelope...
> - Two ASCII-only `hex_decode` / `hex_nibble` pure-function helpers
>   in `auth/session.rs` — internal parsers, never crosses an isolate
>   boundary.

This matches the current grep evidence (`Result<_, String>` count: 19,
of which 11 are doc comments, 5 are intentional envelope/parser
hold-outs, 3 are pure helpers — same shape as R5). The preamble is now
in sync with the code rather than describing the pre-sweep state.

### `f1f06900` — integration test signature fix

**Verified.** Trivial: three call sites in `tests/integration.rs`
(L2906, L2947, L2956) now pass `app_id` to `watchdog_query` and
`drop_abandoned_slots`. Caught a build break that the lib build alone
masked.

### `eda96ead` — `first_row_or_internal` helper

**Verified.** Three sites converted (`audit.rs:314`, `audit.rs:600`,
`replication.rs:282`); the migrations.rs:326 callsite stays on its
`Coded` rail per `eda96ead`'s commit message rationale ("the
`migrations.rs:326` site can stay on `Coded` if its caller wants that
rail"). The helper:

```rust
pub(crate) fn first_row_or_internal<'a, R>(
    rows: &'a [R],
    op: &'static str,
) -> Result<&'a R, DbError> { ... }
```

Generic over `R` so test code at `error.rs:640` and `:652` can drive it
with `Vec<i64>` / `Vec<()>` rather than building a `compio_postgres::Row`
(crate-private constructor). The `'a` lifetime is explicit but
unnecessary — Rust would elide identically; cosmetic only.

Side observation: `migrations.rs:326` (`if id == 0 { return Err(coded(...)) }`)
is now strictly defensive — `insert_backfill_running` cannot return 0
because it `?`-propagates through `first_row_or_internal`. The dead
branch is acceptable as a defense-in-depth marker but could be
documented as such.

---

## New findings (R6)

### [MAJOR] MAJOR-R6-1 — `ConsumerRunningGuard` doesn't cover the pre-poll window

**File:** `crates/plugin-db/src/replication_ops.rs:264-284`

**Symptom:**
```rust
let app_for_task = app_id.clone();
crate::context::with_mut(|c| c.mark_consumer_running(&app_id));    // (a) before spawn
compio::runtime::spawn(async move {
    let _guard = ConsumerRunningGuard {                             // (b) on first poll
        app_id: app_for_task,
    };
    crate::wal_consumer::run_supervised(consumer).await;
})
.detach();
```

The `mark_consumer_running` write at (a) happens **synchronously, before
the spawned future is constructed**. The Drop guard at (b) is
constructed **only when the future is first polled**. There is a window
between (a) and (b) where:

1. **`spawn()` itself panics.** compio's spawn-on-shutdown-runtime
   raises a panic; the future never runs; Drop never fires; the
   `running_consumers` registry entry stays.
2. **The future is dropped without being polled.** Detached tasks can
   be dropped during runtime teardown (or by some catch_unwind path).
   The closure body executes zero lines; `_guard` is never constructed;
   Drop never fires; the registry entry stays.
3. **The synchronous code path between (a) and `.detach()` panics.**
   `compio::runtime::spawn(...).detach()` is a two-step. If `spawn`
   returns a `JoinHandle` and `.detach()` panics (vanishingly unlikely
   but the API is `pub fn detach(self)`), the mark stays.

The R5 finding listed both this race and the panic-mid-loop case. The
post-fix Drop guard closes the mid-loop case but **inherits the
mark-before-spawn race verbatim**. The commit message claims "the unmark
fires on graceful exit AND on panic-unwind" — true for the inner
unwind, false for the pre-poll-drop and spawn-panic windows.

**Why it's a problem:**

A permanently-marked app cannot re-attach a consumer. The idempotency
short-circuit at `replication_ops.rs:195-207` returns `alreadyRunning:
true` without spawning anything; the operator-facing UX is the
consumer is silently dead. Production-painful.

**Fix:**

Move the `mark` inside the spawned future, ordered before the guard
construction:

```rust
let app_for_task = app_id.clone();
compio::runtime::spawn(async move {
    // mark inside the future so the runtime guarantees Drop runs
    // alongside it if the task is ever dropped pre-poll.
    crate::context::with_mut(|c| c.mark_consumer_running(&app_for_task));
    let _guard = ConsumerRunningGuard {
        app_id: app_for_task,
    };
    crate::wal_consumer::run_supervised(consumer).await;
})
.detach();
```

This still has a brief window between the mark and the guard
construction (one `with_mut` call), but that's a single synchronous
statement — no await, no panic surface meaningful at the runtime level.

The alternative: invert the guard so it owns BOTH the mark and the
unmark:

```rust
struct ConsumerRunningGuard { app_id: String }
impl ConsumerRunningGuard {
    fn new(app_id: String) -> Self {
        crate::context::with_mut(|c| c.mark_consumer_running(&app_id));
        Self { app_id }
    }
}
impl Drop for ConsumerRunningGuard { ... }

compio::runtime::spawn(async move {
    let _guard = ConsumerRunningGuard::new(app_for_task);
    crate::wal_consumer::run_supervised(consumer).await;
}).detach();
```

This is the canonical RAII shape and makes the "mark + unmark are
inseparable" invariant local to the guard. Recommended.

**But wait — there's a contention risk.** The idempotency short-circuit
at L195 reads the registry; the new in-future mark means a racing
second `startReplicationConsumer()` call could (briefly) see no marker
and try to spawn. In practice the call is awaited end-to-end by the
caller, so the second call's read can't race with the first call's
spawn (they're sequential). But if two distinct callers can both
invoke `startReplicationConsumer` concurrently — and the dispatcher
allows it — then moving the mark inside the future opens a
double-spawn window. Worth thinking about; for now the existing
mark-before-spawn semantics may be intentional for double-spawn
prevention even though they cost the pre-poll-drop case.

If the answer is "we need both mark-before-spawn for race prevention
AND mark-inside-future for cleanup", the right shape is two markers:

- Outer mark + outer Drop guard (for the synchronous critical
  section) — RAII via a stack-local sentinel type.
- Inner mark = no-op (already set by outer) — guard just unmarks on
  Drop.

Or simpler: handle the spawn-failure path by wrapping the spawn call:

```rust
let app_for_task = app_id.clone();
crate::context::with_mut(|c| c.mark_consumer_running(&app_id));
let spawn_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    compio::runtime::spawn(async move {
        let _guard = ConsumerRunningGuard { app_id: app_for_task };
        crate::wal_consumer::run_supervised(consumer).await;
    })
    .detach()
}));
if spawn_result.is_err() {
    crate::context::with_mut(|c| c.unmark_consumer_running(&app_id));
    // surface a spawn-failed error...
}
```

**Verification:**
- `crates/plugin-db/src/replication_ops.rs:274-284`
- `grep -n "mark_consumer_running\|unmark_consumer_running" crates/plugin-db/src/`

---

### [MINOR] MIN-R6-1 — `coded_sql` wrapper asymmetry

**Files:**
- `crates/plugin-db/src/audit.rs:58-60` — wrapper
- `crates/plugin-db/src/auth/bootstrap.rs:24-26` — wrapper
- `crates/plugin-db/src/auth/keys.rs:41-43` — wrapper
- `crates/plugin-db/src/auth/session.rs:35-37` — wrapper
- `crates/plugin-db/src/diff.rs:40-42` — wrapper
- `crates/plugin-db/src/replication.rs:201-205`, `:213-216`,
  `:229-233`, `:267-272`, `:396-399`, `:538-541`, `:568-573`,
  `:600-603` — **no wrapper**; inlines `DbError::from_pg(&e)` +
  `prefix_message` directly

Five modules adopted a wrapper pattern; `replication.rs` did not. The
two shapes encode the same thing but with different boilerplate:

```rust
// Five-module wrapper shape — 3 lines per call site:
.map_err(|e| coded_sql("CREATE EXTENSION pgcrypto", e))?

// replication.rs shape — 4 lines per call site:
.map_err(|e| {
    let mut err = DbError::from_pg(&e);
    prefix_message(&mut err, "replication: probe pg_publication: ");
    err
})?
```

**Why it's a problem:**

- Inconsistent — six modules dedup'd; one does it differently.
- The 8 inline closures in `replication.rs` (L201-603) are 8 copies of
  the same 4-line shape — a sub-DRY violation within a file that the
  dedup commit explicitly intended to address.
- Future contributors reading the codebase have to guess which idiom
  to use.

**Fix:**

Either add a `replication.rs`-local `coded_sql` wrapper (matching the
other five), or — better — replace all 8 `prefix_message` inlines with
the shared `coded_sql`:

```rust
// At call site:
.map_err(|e| coded_sql("replication: probe pg_publication", e))?
```

The shared `coded_sql` already does `DbError::from_pg(&e)` via the
`From` impl and prepends the context phrase. Three of the 8 inline
sites in `replication.rs` are doing exactly what `coded_sql` does;
five are doing more (the wal_level check at L256-274 has a
conditional `Configuration` branch that isn't expressible through
`coded_sql`, so those can stay inline).

The 3 trivially-equivalent sites:
- L201-205 (probe pg_publication)
- L213-216 (CREATE PUBLICATION when not exists)
- L229-233 (probe pg_replication_slots)
- L396-399 (watchdog query)
- L538-541 (enumerate abandoned slots)
- L568-573 (per-slot DROP — has a 55006 swallow, but the swallow is
  on the success/error discriminator, not the variant prefix)
- L600-603 (slot_status)

Net: convert 6-7 of the 8 inline sites to the wrapper pattern; leave
the wal_level conditional alone.

**Verification:**
- `grep -nC2 "prefix_message(" crates/plugin-db/src/replication.rs`
- `grep -n "^fn coded_sql" crates/plugin-db/src/`

---

### [MINOR] MIN-R6-2 — `first_row_or_internal` redundant lifetime annotation

**File:** `crates/plugin-db/src/error.rs:378-385`

```rust
pub(crate) fn first_row_or_internal<'a, R>(
    rows: &'a [R],
    op: &'static str,
) -> Result<&'a R, DbError> {
    rows.first().ok_or_else(|| DbError::Internal {
        message: format!("{op}: returned no row"),
    })
}
```

The `'a` lifetime is explicit but matches what Rust would elide. The
elided form:

```rust
pub(crate) fn first_row_or_internal<R>(
    rows: &[R],
    op: &'static str,
) -> Result<&R, DbError>
```

…is identical in semantics (single input borrow ties to single output
borrow). Cosmetic; non-blocking. Worth fixing in the next idiom sweep.

**Verification:**
- `crates/plugin-db/src/error.rs:378`

---

## R5 carry-over status

| R5 item | Status | Detail |
|---|---|---|
| MAJOR-R5-1 (`init_session` substring matching) | **unchanged** | `auth/session.rs:188-225` still uses `msg.contains("nonce replay detected")`. The SECURITY DEFINER body owns the wire shape; locale drift / message rewording still breaks the SDK contract. **Promote to R6 carry-over MAJOR.** |
| MAJOR-R5-2 (consumer-running mark race) | **partial — closes inner case** | `e399eeea`'s Drop guard closes the panic-mid-loop case. The mark-before-spawn race is still open (see MAJOR-R6-1 above). |
| MAJOR-R5-3 (five `coded_sql` duplicates) | **closed by `cbbc9059`** | Shared helper in `crate::error`; 5 wrappers + replication inlines. Cost: see MIN-R6-1. |
| MAJOR-R5-4 (`WalConsumer::new` typed-code loss) | **unchanged + worsened comment** | `wal_consumer.rs:333-336` still flattens `DbError` to `ConsumerError::NotProvisioned(String)`; `replication_ops.rs:239-241` then stamps a hardcoded `"not_provisioned"` code. The doc comment at `wal_consumer.rs:327-332` now claims "the `DbError`'s `.code` is preserved at the V8 dispatch boundary" — that's the OPPOSITE of what `replication_ops.rs:233-248` does. The comment lies more confidently than R5. **Promote to R6 carry-over MAJOR.** |
| MAJOR-R5-5 (silent unlock-SQL swallowing) | **partial — log added, flag still flips** | `lock_guard.rs:168-179` now warns on unlock failure (good observability). But `self.released = true` at L181 still flips regardless of unlock outcome — so the released flag now means "release was attempted" rather than "lock is actually free". The Drop's catastrophic-path log still won't fire on unlock failure. Sufficient for ops observability via the warning; not a correct released-state signal for any future taint-on-Drop API. Score: minor-blemish status. |
| MIN-R5-1 (`iso_timestamp_after` non-saturating add) | **unchanged** | `auth/session.rs:294` still `now + ttl_secs.saturating_mul(1000)`. Defense-in-depth — one-character fix. |
| MIN-R5-2 (`mint_db` Option vs Result asymmetry) | **unchanged** | `v8_classes/db.rs:367` still returns `Option<v8::Local<...>>`. |
| MIN-R5-3 (six near-identical `mint_*` bodies) | **unchanged** | 6 ~40-line bodies in `v8_classes/`. |
| MIN-R5-4 (broker waker-while-borrowed) | **unchanged** | `broker.rs:319-321`, `:325-327`, `:366-368` still call `w.wake()` while `inner: RefMut` is alive. |
| MIN-R5-5 (`run_sql` cancellation race) | **unchanged** | `exec.rs:49-56` still `take → await → put` without an RAII guard. |
| INFO-R5-1 (`into_held` dead code) | **unchanged** | `lock_guard.rs:196-209` still `#[allow(dead_code)]`. |
| INFO-R5-2 (test compio runtime mint) | **unchanged** | `lock_guard.rs:268` still `Runtime::new().unwrap()`. |
| R4 M1 (`v8_bridge.rs:217` try_into unwrap) | **unchanged** | Still `try_into().unwrap()` post-`is_array` check. Safe in practice; stylistically panicky. |

---

## Audit summary — eight dimensions

| Dimension | Finding | Severity |
|---|---|---|
| 1. RefCell-across-await | `ConsumerRunningGuard::Drop` calls `with_mut` from a sync Drop — no await, no nested borrow risk. The mark-before-spawn `with_mut` at L275 is also sync. No new patterns. `broker::push/close` waker-while-borrowed (MIN-R5-4) still open. | MINOR (latent, carried) |
| 2. Unsafe | 9 finalizer-pattern blocks (`v8_classes/`). Zero new unsafe this round. | OK |
| 3. Panic risks | Production `.unwrap()` count unchanged (17 `v8::String::new(...).unwrap()`, 2 `try_into().unwrap()`, 1 `into_held::expect`). Recent commits introduce **zero** new panic sites. `compio::runtime::spawn` can itself panic during runtime shutdown — this is the unscored hole behind MAJOR-R6-1. | MINOR |
| 4. Error handling typing | `Result<_, String>` count steady at 19 (same as R5; the [I28] sweep saturation was reached at R5). The `coded_sql` / `prefix_message` dedup (`cbbc9059`) collapsed 5x17-line bodies + 1 in `replication.rs` into a shared helper. `first_row_or_internal` (`eda96ead`) closes the empty-RETURNING DRY cluster. **Major progress on typed-error idioms.** | OK (was MINOR-chronic at R5) |
| 5. Lifetimes | `first_row_or_internal<'a, R>(&'a [R], &'static str) -> Result<&'a R, DbError>` works but the `'a` is redundant (MIN-R6-2). `ConsumerRunningGuard` carries no lifetime parameter — just owns a `String`; clean. No higher-rank bounds anywhere. | OK |
| 6. Idiomatic patterns | The `coded_sql` wrapper asymmetry (MIN-R6-1) is the new blemish: 5 modules wrap a thin 3-liner; `replication.rs` inlines `prefix_message` 8 times. RAII discipline is otherwise strengthening (Drop guards in `ConsumerRunningGuard`, `OrchestratorLockGuard`, `Migration`, `Transaction`, `Subscription`). | MINOR |
| 7. Resource lifecycle | `ConsumerRunningGuard` Drop covers the inner-panic case but misses the mark-before-spawn window (MAJOR-R6-1). `OrchestratorLockGuard` now warns on unlock failure (MAJOR-R5-5 partial). `Subscription`, `Transaction`, `Migration` Drop impls unchanged. New RAII pattern from `eda96ead` (helper-based empty-RETURNING). | MAJOR (one new finding) |
| 8. Type ascription | No new turbofish abuse. `first_row_or_internal`'s generic R is well-justified (test-fixture independence). `coded_sql` shared helper signature is clean. | OK |

---

## Score breakdown

| Dimension | R1 | R2 | R3 | R4 | R5 | R6 | Change |
|---|---|---|---|---|---|---|---|
| Correctness | 72 | 80 | 84 | 84 | 84 | 85 | `e399eeea` (Drop guard) closes the inner panic case; `cbbc9059` adds 3 contract tests for `prefix_message` variant preservation; `eda96ead` pins `first_row_or_internal` contract. MAJOR-R6-1 (mark-before-spawn) offsets some gain. Net +1. |
| Performance | 84 | 84 | 84 | 86 | 88 | 88 | No measured perf delta this cycle. The dedup adds one `format!` per error path (cold). |
| Security | 88 | 88 | 90 | 90 | 90 | 90 | No regression. |
| API design | 76 | 82 | 84 | 86 | 87 | 88 | `crate::error::{coded_sql, prefix_message, first_row_or_internal}` adds three small, well-named, well-documented helpers with tests. `ConsumerRunningGuard` is a tidy local RAII pattern. The wrapper asymmetry (MIN-R6-1) is a small blemish. Net +1. |
| Rust idioms | 80 | 84 | 86 | 87 | 87 | 89 | The DRY cleanup (`cbbc9059`, `eda96ead`) is the dominant idioms signal this round — ~120 LOC removed from per-file duplicates, replaced with shared helpers + contract tests. The 8 inline `prefix_message` sites in `replication.rs` (MIN-R6-1) are the remaining DRY ceiling. Net +2. |
| **Overall** | **78** | **85** | **87** | **88** | **89** | **90** | Net +1. Crosses the 90 threshold on the back of three R5 MAJOR closures + the `first_row_or_internal` extraction; held back from higher by the `ConsumerRunningGuard` mark-race remnant, the `WalConsumer::new` typed-code loss, and the `init_session` substring matching. |

---

## Ceiling-blockers for 92+

In priority order:

1. **MAJOR-R6-1** — Move `mark_consumer_running` inside the spawned
   future (or behind a constructor-side RAII pattern). One-file fix.
   Closes the last hole in R5 MAJOR-R5-2.
2. **MAJOR-R5-1 carry-over** — `init_session` substring matching on
   RAISE messages. Switch to custom SQLSTATE + arm in `from_pg`.
   One-file fix; biggest semantic improvement for the SDK contract.
3. **MAJOR-R5-4 carry-over** — `WalConsumer::new` typed-code loss.
   Either thread `DbError` through `ConsumerError` or stop overstamping
   in the dispatch boundary. Also fix the lying comment at
   `wal_consumer.rs:327-332`.
4. **MIN-R6-1** — `replication.rs`'s 8 inline `prefix_message` sites
   → consolidate to `coded_sql` calls where the variant logic is
   trivial (6-7 sites); leave the wal_level conditional alone.
5. **MAJOR-R5-5 remnant** — `lock_guard.release()`'s `self.released =
   true` flip should be conditional on unlock success, OR the
   `released` field should be renamed `release_attempted` to reflect
   what it now means.
6. **R4 M-NEW-1 underlying leak** — Drop returns locked client to
   pool. Needs `compio_postgres` support for taint-on-Drop.
7. **R4 M-NEW-4 / MIN-R5-4** — Broker `push`/`close` wake-while-
   borrowed. 3 sites × 3 lines.
8. **R4 M-NEW-5 / MIN-R5-5** — `run_sql` cancellation race.

Items below the cut (MIN-R5-1, MIN-R5-2, MIN-R5-3, MIN-R6-2,
INFO-R5-1, INFO-R5-2) are cosmetic or pre-existing.

---

## Verification commands

```bash
# RefCell-across-await sweep
grep -rn "borrow_mut\|borrow()" crates/plugin-db/src | wc -l   # ~30 sites; none introduced this cycle

# Unsafe blocks
grep -rn "unsafe " crates/plugin-db/src | grep -v "//"          # 9 finalizer sites; unchanged

# Panic sites (production paths only)
grep -rn "\.unwrap()\|\.expect(" crates/plugin-db/src | grep -v "/tests/" | grep -v "mod tests"

# Result<_, String> remaining
grep -rEn "Result<.*,\s*String>" crates/plugin-db/src | wc -l   # 19 (steady from R5)
grep -rEn "Result<.*,\s*String>" crates/plugin-db/src | awk -F: '{print $1}' | sort | uniq -c | sort -rn

# coded_sql duplicates after dedup
grep -rn "^fn coded_sql" crates/plugin-db/src                   # 5 per-module wrappers (audit, auth/*, diff)
grep -rn "pub(crate) fn coded_sql\|pub(crate) fn prefix_message" crates/plugin-db/src/error.rs  # 2 shared helpers

# prefix_message inline sites in replication.rs (MIN-R6-1)
grep -n "prefix_message(" crates/plugin-db/src/replication.rs  # 8 sites

# first_row_or_internal call sites
grep -rn "first_row_or_internal" crates/plugin-db/src           # 3 production + 2 tests + 1 def

# Consumer-running mark race (MAJOR-R6-1)
grep -nC3 "mark_consumer_running\|unmark_consumer_running" crates/plugin-db/src/replication_ops.rs
grep -n "ConsumerRunningGuard" crates/plugin-db/src/replication_ops.rs

# init_session substring matching (R5 carry-over)
grep -nC5 "nonce replay detected\|signature expired\|invalid session-init signature" crates/plugin-db/src/auth/session.rs

# WalConsumer::new typed-code loss (R5 carry-over)
grep -nC3 "NotProvisioned" crates/plugin-db/src/wal_consumer.rs crates/plugin-db/src/replication_ops.rs

# lock_guard release flag (R5 partial)
grep -nC2 "self.released = true" crates/plugin-db/src/orchestrator/lock_guard.rs

# Broker waker-while-borrowed (R5 carry-over)
grep -nC2 "w.wake()" crates/plugin-db/src/broker.rs
```

---

## Score: **90/100** (vs R5: 89, **+1**)

Trajectory: 78 → 85 → 87 → 88 → 89 → **90**. The 90 threshold reflects
"production-quality Rust with named, tested, idiomatic patterns
throughout". Three R5 MAJORs closed; one new MAJOR (mark-race remnant)
opens the door to 91+ as soon as `ConsumerRunningGuard` is hoisted
inside the future. The R5 carry-overs (init_session substring
matching, WalConsumer typed-code loss) gate progress past 92.
