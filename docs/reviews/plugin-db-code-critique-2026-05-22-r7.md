# plugin-db code critique — 2026-05-22 R7

**Score trajectory: 78 (R1) → 85 (R2) → 87 (R3) → 88 (R4) → 89 (R5) → 90 (R6) → 93 (R7)**

Scope: `crates/plugin-db/` at HEAD (post-`e5315083`).
Lens: Rust correctness and idioms only. Architecture / security / perf live
in sibling reviews.

Re-audited fresh against the brief's eight dimensions. The headline this
round is **three R5/R6 MAJORs closed in one cycle**, each by a small
well-targeted commit:

- **MAJOR-R6-1** (`ConsumerRunningGuard` mark-before-spawn race) —
  closed by `34d209b5` (move mark inside guard ctor) + `70921112`
  (atomic try-claim using `HashSet::insert`'s return value as the
  check-and-set primitive). The lost-race branch uses `then(|| ...)`
  rather than `then_some(...)` because `then_some` evaluates eagerly:
  building an ephemeral `Self` on the lost path would fire its Drop
  and unmark the winner. That trap was caught by tests added in
  `386f9bf5` (`consumer_running_guard_try_claim_loses_when_already_marked`).
- **MAJOR-R5-1** (`init_session` substring matching) — closed by
  `a272d1af`. The SECURITY DEFINER `init_session` body now tags every
  `RAISE EXCEPTION` with a stable `USING DETAIL = 'session_*'` token,
  and `auth/session.rs:174` reads `db_err.detail()` via the structured
  `compio_postgres::Error::as_db_error()` path. Locale- /
  formatter-independent classification at last.
- **MAJOR-R5-4** (`WalConsumer::new` typed-code loss) — closed by
  `aa639715`. The constructor returns `Result<_, DbError>` with two
  failure variants (`Configuration { code: "not_provisioned" }` for a
  missing `db_url`; `ValidationFailed { code: "invalid_app_id" }` for
  sanitise failures). `replication_ops.rs:247` no longer re-stamps a
  blanket `"not_provisioned"` code over both classes.

Plus `386f9bf5` adds the byte-offset structural test
(`release_flips_flag_after_unlock_await_structural`) that pins the
`[I42]` invariant in source-layout terms — any future contributor who
flips `self.released = true` BEFORE the unlock-SQL `.await` trips the
test at compile-time, without needing a live PG fixture.

Net progress this cycle: **+3**.

Carry-overs that remain open: MAJOR-R5-5 remnant (`release()` flag
flips regardless of unlock outcome), MIN-R6-1 (`replication.rs`
`coded_sql`-wrapper asymmetry), MIN-R5-4 (broker `w.wake()` while
RefMut held), MIN-R5-1 (`iso_timestamp_after` non-saturating add),
MIN-R5-2 / R5-3 / R5-5, INFO-R5-1 / R5-2. **Two new MINOR findings**
surface this round: MIN-R7-1 (substring-match SQLSTATE in
`replication.rs:213,257` — same fragility class as the just-closed
`init_session` issue, two more sites) and MIN-R7-2 (`release()`
returns `Result` but is currently infallible).

Findings tagged `[CRITICAL] / [MAJOR] / [MINOR] / [INFO]` per the brief,
with `file:line` evidence and verification commands.

---

## Verified recent commits

### `70921112` — atomic `try_mark_consumer_running` closes the mark race

**Verified.** `context.rs:428-435`:

```rust
/// Atomically check-and-mark: returns `true` if the caller won the
/// mark (was not previously running), `false` if another caller
/// already marked this app. Used by the spawned consumer task to
/// close the race between dispatch's idempotent gate and the
/// task's first poll (concurrency r7 NEW MINOR).
pub fn try_mark_consumer_running(&mut self, app_id: &str) -> bool {
    self.running_consumers.insert(app_id.to_string())
}
```

`HashSet::insert` returns `true` iff the value was newly inserted —
this IS the atomic primitive on a single-threaded `HashSet`. Combined
with `ConsumerRunningGuard::try_claim` at `replication_ops.rs:356-359`:

```rust
fn try_claim(app_id: String) -> Option<Self> {
    let won = crate::context::with_mut(|c| c.try_mark_consumer_running(&app_id));
    won.then(|| Self { app_id })
}
```

…the mark-before-spawn race is closed AND the lost-race path doesn't
fabricate a Self (and therefore can't fire Drop on the winner's
mark). The lazy `then(|| ...)` is load-bearing — see the doc comment
at `replication_ops.rs:349-355` for the trap that `then_some` would
re-introduce.

### `386f9bf5` — structural [I42] test + ConsumerRunningGuard lifecycle tests

**Verified.** Four new unit tests in `replication_ops.rs:378-424`:

- `consumer_running_guard_new_marks_app` — Drop-guard invariant 1.
- `consumer_running_guard_drop_unmarks_app` — Drop-guard invariant 2.
- `consumer_running_guard_drop_unmarks_on_panic_unwind` — panic safety.
- `consumer_running_guard_try_claim_loses_when_already_marked` —
  the exact bug the `then_some` trap would have caused. The test
  asserts `g1` still holds the mark after `g2` is `None`. If a future
  contributor switches back to `then_some`, the loser's ephemeral
  Self drops, unmarks the winner's claim, and this test fails fast.

Plus the structural test at `lock_guard.rs:349-382`
(`release_flips_flag_after_unlock_await_structural`) that `include_str!`s
the source file and asserts `self.released = true;` appears at a
byte-offset AFTER `.query_text_params(`. Mirror of the mint_subscription
pattern.

### `a272d1af` — P0001 DETAIL classification in `init_session`

**Verified.** `auth/session.rs:174-202`:

```rust
fn classify_p0001_detail(
    e: &compio_postgres::Error,
) -> Option<(&'static str, &'static str)> {
    let db_err = e.as_db_error()?;
    if db_err.code() != &compio_postgres::error::SqlState::RAISE_EXCEPTION {
        return None;
    }
    match db_err.detail()? {
        "session_signature_expired" => Some((...)),
        "session_nonce_replay"      => Some((...)),
        ...
    }
}
```

And the SECURITY DEFINER body in `auth/bootstrap.rs:523-558` now tags
each RAISE with `USING ERRCODE = 'P0001', DETAIL = 'session_*'`. Locale
drift, formatter changes, message rewording can no longer break
classification.

Verification: `Grep -n "msg.contains" crates/plugin-db/src/auth/`
returns zero — the substring-match anti-pattern is gone from `auth/`.
Integration tests at `tests/integration.rs:3577-3660` continue to
assert `code == "session_signature_expired"` / `"session_nonce_replay"`
end-to-end.

### `aa639715` — `WalConsumer::new` typed Result

**Verified.** `wal_consumer.rs:346-368`:

```rust
pub fn new(app_id: &str, db_url: &str) -> Result<Self, DbError> {
    if db_url.is_empty() {
        return Err(DbError::Configuration {
            code: "not_provisioned",
            message: "wal consumer: db_url not configured ...".to_string(),
        });
    }
    let slot_name = crate::replication::slot_name(app_id)?;
    let publication_name = crate::replication::publication_name(app_id)?;
    Ok(Self { ... })
}
```

`replication_ops.rs:245-254` propagates the typed error verbatim:

```rust
let consumer = match crate::wal_consumer::WalConsumer::new(&app_id, &url) {
    Ok(c) => c.with_start_lsn(setup.confirmed_flush_lsn.clone()),
    Err(e) => {
        return OpResult::JsValue {
            resolver,
            value: ResolveValue::RejectError(e.to_op_error()),
            request_id,
        };
    }
};
```

Two unit tests pin the wire shape: `wal_consumer_new_invalid_app_id_returns_typed_error`
(asserts `code == "invalid_app_id"`) and
`wal_consumer_new_missing_db_url_returns_configuration` (asserts
`code == "not_provisioned"`). The SDK can finally branch on `err.code`
between "operator misconfigured" and "developer passed a bad app id".

### `4b2e7046` — ConsumerRunningGuard comment rewrite

**Verified.** `replication_ops.rs:262-298` — the comment block now
describes the current shape (guard's lifetime bound to the future;
try_claim atomic vs. the race window) plus a 3-line history block
citing each commit's specific failure mode (e399eeea / 34d209b5 /
70921112). The MAJOR-R6-1 lying comment is gone.

### `e5315083` — dead-code + docstring cleanups

**Verified.** `mark_consumer_running` is now gated behind
`#[cfg(any(test, feature = "test-helpers"))]` (`context.rs:423-426`)
with a doc comment that points production callers at the atomic
`try_mark_consumer_running`. This kills the api-surface r6 MAJOR
(API footgun: two near-identical mark methods one of which silently
loses races).

---

## New findings (R7)

### [MINOR] MIN-R7-1 — `replication.rs` substring-matches SQLSTATE codes

**Files:** `crates/plugin-db/src/replication.rs:213`, `:257`

```rust
// L211-218:
if let Err(e) = pool.execute(&pub_sql, &[]).await {
    let msg = format!("{e:#}");
    if !msg.contains("42710") {
        let mut err = DbError::from_pg(&e);
        prefix_message(&mut err, "replication: CREATE PUBLICATION: ");
        return Err(err);
    }
}

// L250-274:
.map_err(|e| {
    let msg = format!("{e:#}");
    if msg.contains("55000") || msg.to_lowercase().contains("wal_level") {
        DbError::Configuration { code: "wal_level_not_logical", ... }
    } else {
        let mut err = DbError::from_pg(&e);
        prefix_message(...);
        err
    }
})
```

**Why it's a problem:**

This is exactly the same fragility class the `a272d1af` commit just
closed in `auth/session.rs`: matching error classification on the
formatted message body rather than on the structured SQLSTATE.

- A future `compio_postgres::Error` formatter that omits the `42710:`
  / `55000:` prefix (or changes the rendering) flips both branches.
- `to_lowercase()` allocates per invocation; the SQLSTATE check is O(1).
- `42710` (`duplicate_object`) and `55000` (`object_not_in_prerequisite_state`)
  are both standard SQLSTATE constants exposed via
  `compio_postgres::error::SqlState` — same as how `RAISE_EXCEPTION`
  is referenced in `auth/session.rs:178`.

**Why it's MINOR not MAJOR:**

- The Configuration branch at L258 is a UX upgrade (operator gets a
  clearer message), not a correctness gate — falling back to the
  generic DbError on a missed match still reports the error, just
  less helpfully.
- The `42710` swallow at L213 is a benign no-op short-circuit; a
  miss would cause the function to error with the underlying PG
  message, but the next call's idempotent probe at L195 would still
  succeed.

Both sites' failure mode is "error message gets less helpful," not
"wrong code reaches the SDK." Genuine MINOR.

**Fix:**

Mirror the `auth/session.rs` pattern:

```rust
fn is_duplicate_object(e: &compio_postgres::Error) -> bool {
    e.as_db_error()
        .map(|d| d.code() == &compio_postgres::error::SqlState::DUPLICATE_OBJECT)
        .unwrap_or(false)
}

if let Err(e) = pool.execute(&pub_sql, &[]).await {
    if !is_duplicate_object(&e) {
        return Err(coded_sql("replication: CREATE PUBLICATION", e));
    }
}
```

Same for the `55000` branch (use `SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE`
— the `wal_level` heuristic is independent and can stay as a
defense-in-depth string check, but only AFTER the SQLSTATE primary
check).

**Verification:**
- `Grep -n 'contains("42710"\|contains("55000"' crates/plugin-db/src/`
  → 2 hits.
- `Grep -n 'SqlState::' crates/plugin-db/src/auth/session.rs` → 1
  hit (the pattern we should mirror).

---

### [MINOR] MIN-R7-2 — `OrchestratorLockGuard::release` returns `Result` but is currently infallible

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:144-183`

```rust
pub(crate) async fn release(mut self) -> Result<Option<PooledClient<'p>>, DbError> {
    if self.released {
        return Ok(self.client.take());
    }
    if let Some(client) = self.client.as_ref() {
        ...
        if let Err(e) = client
            .query_text_params(unlock_sql, &[self.key.as_str(), self.tag])
            .await
        {
            tracing::warn!(...);   // logs but doesn't propagate
        }
    }
    self.released = true;
    Ok(self.client.take())          // always Ok
}
```

The function signature declares fallibility but the body never produces
`Err`: unlock-SQL errors are logged-and-swallowed (ffb1e101 intent),
the `client.take()` is infallible, the `released` short-circuit returns
`Ok(None)`. Every call site uses `let _ = lock_guard.release().await`
or `lock_guard.release().await?` — both of which would work without
the `Result` wrap.

**Why it's a problem:**

- Misleading API surface — readers wonder which paths can Err.
- `?` propagation at call sites adds noise that never fires.
- Inhibits a future "taint the released flag on unlock failure"
  refactor (MAJOR-R5-5 remnant): if `release()` actually started
  returning Err on unlock failure, every call site would need
  updating, but they'd already be calling `?` "correctly" without
  thinking about it.

**Why MINOR:**

Forward-compat hold: if a future change DOES introduce a fallible
path (e.g. taint-on-failure semantics from MAJOR-R5-5 remnant), the
signature is already correct. So this is "the API is more general
than the current impl" — not a bug, but a smell.

**Fix:** Two options:

1. **Keep the signature, document the asymmetry.** Add a doc note that
   the current impl never returns Err (intentional best-effort
   semantics), but the signature reserves the right.
2. **Tighten to infallible.** Change to `async fn release(mut self)
   -> Option<PooledClient<'p>>`. If MAJOR-R5-5 remnant is later
   addressed by surfacing unlock failures, restore the `Result`.

Option 1 is the cheaper move and aligns with the (currently best-effort)
shape. Mention it in the doc-comment near the unlock-SQL `if let Err(e)`
arm.

**Verification:**
- `Grep -n "pub(crate) async fn release" crates/plugin-db/src/orchestrator/lock_guard.rs`
- Trace every `Err(` in the function body → none reachable.

---

## R6 carry-over status

| R6 item | Status | Detail |
|---|---|---|
| MAJOR-R6-1 (mark-before-spawn race) | **CLOSED** by `34d209b5` + `70921112`. Mark moved inside the guard ctor; ctor is atomic via `try_mark_consumer_running` (HashSet::insert-as-CAS). The `then_some` trap variant is pinned by `consumer_running_guard_try_claim_loses_when_already_marked` (R7 commit `386f9bf5`). |
| MIN-R6-1 (`coded_sql` wrapper asymmetry) | **unchanged** | `replication.rs` still has 8 inline `let mut err = DbError::from_pg(&e); prefix_message(...); err` sites (`replication.rs:201-605`). The five other modules wrap via a thin `coded_sql` shim. Audit.rs's comment at L54-57 falsely claims `replication` is one of them. Compound with MIN-R7-1 — a `coded_sql` wrapper in `replication.rs` would land alongside the SQLSTATE-structural sweep. |
| MIN-R6-2 (`first_row_or_internal` redundant `'a`) | **unchanged** | `error.rs:378-385` still spells out the lifetime. Trivial. |

## R5 carry-over status

| R5 item | Status | Detail |
|---|---|---|
| MAJOR-R5-1 (`init_session` substring matching) | **CLOSED** by `a272d1af`. P0001 + DETAIL token classification via `as_db_error()`. |
| MAJOR-R5-2 (consumer-running mark race) | **fully CLOSED** (was R6 partial). Combined with MAJOR-R6-1 close. |
| MAJOR-R5-3 (five `coded_sql` duplicates) | **closed** at R6 by `cbbc9059`. |
| MAJOR-R5-4 (`WalConsumer::new` typed-code loss) | **CLOSED** by `aa639715`. Two-variant Result with stable `.code`s. |
| MAJOR-R5-5 (silent unlock-SQL swallowing) | **partial — log added at R6, flag still flips** | `lock_guard.rs:168-181`: `tracing::warn!` is good operator-observability; the `released = true` flip at L181 still runs after a swallowed Err. Compound with MIN-R7-2 — both are about the same "released" state-machine being undertyped. |
| MIN-R5-1 (`iso_timestamp_after` non-saturating add) | **unchanged** | `auth/session.rs:331`: `now + ttl_secs.saturating_mul(1000)`. The `saturating_mul` covers ttl; the `+` itself isn't saturating. One-character fix. |
| MIN-R5-2 (`mint_db` Option vs Result asymmetry) | **unchanged** | `v8_classes/db.rs:367` still returns `Option<v8::Local<...>>`. |
| MIN-R5-3 (six near-identical `mint_*` bodies) | **unchanged** | 6 ~40-line bodies in `v8_classes/`. |
| MIN-R5-4 (broker waker-while-borrowed) | **unchanged** | `broker.rs:319-321,325-327,366-368` still call `w.wake()` while `inner: RefMut` is alive. Defensive concern — if any waker re-enters the Subscription's `pop`/`push`/`close` it panics on the RefCell. Real-world wakers (compio's task scheduler) don't re-enter, but the layout is fragile. Drop the `inner` borrow before calling `w.wake()`. |
| MIN-R5-5 (`run_sql` cancellation race) | **unchanged** | `exec.rs:49-56` still `take → await → put` without an RAII guard. |
| INFO-R5-1 (`into_held` dead code) | **unchanged** | `lock_guard.rs:196-209` still `#[allow(dead_code)]`. Doc comment justifies it. |
| INFO-R5-2 (test compio runtime mint) | **unchanged** | `lock_guard.rs:268` still `Runtime::new().unwrap()`. |
| R4 M1 (`v8_bridge.rs:217` try_into unwrap) | **unchanged** | Standard `try_into().unwrap()` post-`is_array` check. Safe in practice. |

---

## Audit summary — eight dimensions

| Dimension | Finding | Severity |
|---|---|---|
| 1. RefCell-across-await | No new patterns. `Grep` for `\.borrow(_mut)?\(\).*\.await` returns zero. `ConsumerRunningGuard::Drop` calls `with_mut` from a sync Drop. `SuppressGuard::Drop` calls `unsuppress_app` (also `with_mut`); no awaits. Per-app suppression set is a `thread_local! RefCell<HashSet<String>>`. The broker waker-while-borrowed pattern (MIN-R5-4) is the only remaining open concern. | MINOR (carried) |
| 2. Unsafe | Same 9 finalizer-pattern blocks in `v8_classes/`. Zero new unsafe this round. | OK |
| 3. Panic risks | Production `.unwrap()` count steady: 17 `v8::String::new(...).unwrap()` (V8 OOM-only), 2 `try_into().unwrap()` (length-checked), 1 `into_held::expect` (documented invariant). Recent commits add **zero** new panic sites. The `then_some`→`then(|| ...)` swap in `try_claim` removed a potential leak/double-drop (not a panic but a related foot-shoot). | OK |
| 4. Error handling typing | `Grep` for `Result<[^,>]+,\s*String\s*>` returns 15 hits: 11 doc-comment references to the post-[I28] state, 2 production hold-outs (`lib.rs::init_pool_async` — embedder-facing top-level entry; `orchestrator::register_model::validate::validate` — envelope-as-Err wire contract), 2 internal parsers (`auth/session.rs::hex_decode`/`hex_nibble`). The post-`aa639715` sweep is now saturated — every fallible function that crosses the V8 boundary returns `Result<_, DbError>`. **Major progress maintained from R6.** | OK |
| 5. Lifetimes | `ConsumerRunningGuard` carries no lifetime parameter — owns a `String`. `OrchestratorLockGuard<'p>` lifetime is well-justified (pool-borrow rooting). `try_claim` returns `Option<Self>` without needing an explicit lifetime — clean. The `'a` in `first_row_or_internal` (R6 MIN-R6-2) is still redundant but trivial. | OK |
| 6. Idiomatic patterns | The `then(|| ...)` vs `then_some(...)` swap (`70921112` commit) is the textbook example of "lazy construction is load-bearing" — the inline doc comment at `replication_ops.rs:349-355` names the trap and the test at L413-424 pins it. **Highest-quality idiom signal this round.** RAII discipline is otherwise strengthening; no new clippy::pedantic-class issues. | OK |
| 7. Resource lifecycle | `ConsumerRunningGuard` lifecycle is now closed end-to-end: try_claim atomic, Drop unmarks on all exit paths (graceful, panic, pre-poll drop). `OrchestratorLockGuard` — release-on-success, hand-off-via-into_held, Drop-as-fallback all documented + tested. The MAJOR-R5-5 remnant (release-flag-flips-regardless) is the only open concern. | OK (was MAJOR at R6) |
| 8. Type ascription | No new turbofish abuse. `try_claim`'s `won.then(|| Self { app_id })` reads cleanly. `try_mark_consumer_running` returns `bool` — explicit, no ascription needed. Helper signatures (`classify_p0001_detail` returning `Option<(&'static str, &'static str)>`) are concise. | OK |

---

## Score breakdown

| Dimension | R1 | R2 | R3 | R4 | R5 | R6 | R7 | Change |
|---|---|---|---|---|---|---|---|---|
| Correctness | 72 | 80 | 84 | 84 | 84 | 85 | 90 | Three MAJORs closed (R5-1 substring matching, R5-4 typed code loss, R6-1 mark race). The `then_some`→`then(\|\| ...)` swap closes a latent bug class. New tests pin both the race and the trap. Net +5. |
| Performance | 84 | 84 | 84 | 86 | 88 | 88 | 89 | `e5315083`'s deadcode cleanup + `aa639715`'s removal of an Error.source()-walking format!() in `init_session`'s error path (perf r7 N7-M0) — both cold but real. Net +1. |
| Security | 88 | 88 | 90 | 90 | 90 | 90 | 91 | The DETAIL-token classification is a small security-defense win — locale or formatter drift can no longer make the SDK mis-classify a session refusal as "internal". Tiny gain. |
| API design | 76 | 82 | 84 | 86 | 87 | 88 | 90 | `WalConsumer::new`'s typed Result + the two-variant code surface — the SDK can finally branch on `invalid_app_id` vs `not_provisioned`. `mark_consumer_running` gated behind `cfg(test, feature)` removed an API footgun (two methods, one race-free) on the production surface. Net +2. |
| Rust idioms | 80 | 84 | 86 | 87 | 87 | 89 | 92 | The `then(\|\| ...)` swap + atomic `try_mark_consumer_running` + the `Option<Self>::try_claim` shape is textbook-quality RAII. Combined with the structural [I42] test, the lifecycle discipline is now visible in the source layout. Net +3. |
| **Overall** | **78** | **85** | **87** | **88** | **89** | **90** | **93** | Net +3. Three MAJORs closed in one cycle is the dominant signal; the new MINORs (MIN-R7-1 SQLSTATE-substring in replication.rs, MIN-R7-2 release()'s over-broad signature) are both forward-compat smells, not bugs. |

---

## Ceiling-blockers for 95+

In priority order:

1. **MAJOR-R5-5 remnant** — `lock_guard.release()` flips `released = true`
   even when the unlock SQL erred. The warn-log (`ffb1e101`) is good
   observability but doesn't propagate. Two paths:
   - Surface the unlock error via `Result` and have callers decide;
   - Add a `released_state: { Pending, Confirmed, Failed }` field and
     have Drop fire its catastrophic-path log on `Failed` so the leak
     stays visible after release().
   Either resolves MIN-R7-2 in the same commit.
2. **MIN-R5-4** — broker `w.wake()` while RefMut held. Three sites in
   `broker.rs:319-321,325-327,366-368`. Trivial restructure: extract
   the waker, drop the borrow, then `wake()`. The change is mechanical
   and would close the most-cited "looks scary" pattern in the file.
3. **MIN-R7-1 + MIN-R6-1 combined sweep** — `replication.rs`'s 8
   inline `prefix_message` sites → consolidate to `coded_sql` calls,
   and while doing so, swap the two `msg.contains("42710")` /
   `msg.contains("55000")` sites to `SqlState`-structural checks.
   One commit, two findings closed.
4. **MIN-R5-1** — `iso_timestamp_after`'s `now + saturating_mul(...)`
   → `now.saturating_add(...)`. One-character fix.
5. **MIN-R5-3** — six near-identical `mint_*` bodies in `v8_classes/`.
   Either extract a macro or accept the duplication; closes the last
   non-trivial DRY violation in the crate.

---

## Verification commands

```sh
# RefCell-across-await
Grep -n '\.borrow(_mut)?\(\)' crates/plugin-db/src
Grep -n '\.borrow(_mut)?\(\)[\s\S]{0,40}\.await' --multiline crates/plugin-db/src

# Unsafe count
Grep -n 'unsafe' crates/plugin-db/src

# Panic surfaces
Grep -n '\.unwrap\(\)' crates/plugin-db/src    # 191 (almost all in #[cfg(test)] blocks)
Grep -n '\.expect\(' crates/plugin-db/src      # 38 (RuntimeState assertions + test helpers)

# Result<_, String> sites
Grep -nE 'Result<[^,>]+,\s*String\s*>' crates/plugin-db/src

# Drop impls
Grep -n '^impl Drop for' crates/plugin-db/src  # 7 sites — all reviewed

# then_some trap check (the latent bug class)
Grep -n '\.then_some(' crates/plugin-db/src    # 0 (post-70921112; trap eliminated)
Grep -n '\.then(\|\|' crates/plugin-db/src    # 1 (the deliberate try_claim site)

# SQLSTATE substring-match anti-pattern
Grep -nE 'contains\("[0-9P]{5}"\)' crates/plugin-db/src

# coded_sql wrapper asymmetry
Grep -n '^fn coded_sql' crates/plugin-db/src   # 5 wrappers + 1 in error.rs
Grep -n 'crate::error::coded_sql' crates/plugin-db/src

# WalConsumer::new typed signature
Grep -n 'pub fn new' crates/plugin-db/src/wal_consumer.rs
```

---

## Comparison vs R6

| | R6 (90) | R7 (93) | Delta |
|---|---|---|---|
| CRITICAL findings | 0 | 0 | — |
| MAJOR findings open | 3 (R5-1, R5-4, R6-1) | 1 (R5-5 remnant) | **-2** |
| MAJOR findings closed | 3 (R5-2, R5-3, R5-5 partial) | 3 (R5-1, R5-4, R6-1) | +3 |
| MINOR findings (open) | 7 | 8 | +1 (MIN-R7-1, R7-2 new; MIN-R6-1 still open) |
| Lines of duplicated `coded_sql` boilerplate | 8 inline + 5 wrappers | unchanged | — |
| Structural / contract tests | 1 (`mint_subscription_does_not_leak_broker_entry_*`) | 6 (+ `release_flips_flag_after_unlock_await`, 4 ConsumerRunningGuard lifecycle, `wal_consumer_new_*_returns_typed_error` ×2) | +5 |

The R7 cycle is the cleanest single round in the trajectory: three
MAJORs closed, six new structural / lifecycle tests added, the latent
`then_some`-eager-construction bug caught and pinned by a test, and
zero net regression. The crate is now solidly above the "production
Rust" threshold (90) the brief sets.
