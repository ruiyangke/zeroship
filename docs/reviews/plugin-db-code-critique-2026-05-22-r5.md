# plugin-db code critique — 2026-05-22 R5

**Score trajectory: 78 (R1) → 85 (R2) → 87 (R3) → 88 (R4) → 89 (R5)**

Scope: `crates/plugin-db/` at HEAD (post-`91830cca`).
Lens: Rust correctness and idioms only. Architecture / security / perf live in sibling reviews.

Re-audited fresh against the brief's eight dimensions. The headline
delta this round is the [I28] sweep (`0049d9be` + `91830cca`) — six
files (`auth/bootstrap.rs`, `auth/keys.rs`, `auth/session.rs`,
`replication.rs`, `diff.rs`, `replication_ops.rs`) migrated from
`Result<_, String>` to typed `DbError`, with explicit type-level
regression-guard tests pinning the signatures. That, plus
`bd1e7ce1` ([I42] lock_guard release reorder), `4cbe9fa1`
(`mint_subscription` defer broker subscribe — closes R4 M-NEW-2),
`07205e54` (5 mint_* helpers demoted to `pub(crate)`), and `5ceb6daa`
(byte-eq prefix check, wal_consumer shim visibility) are all real
progress.

Net: two of R4's MAJORs are closed (M-NEW-2 by `4cbe9fa1`, M-NEW-1's
cancellation observability by `bd1e7ce1`'s [I42] reorder — but the
underlying lock-leak-on-pool-return remains). One new MAJOR surfaces
(MAJOR-R5-1, `init_session` substring-matching on RAISE messages —
fragile to locale changes), one new MAJOR carry-over (MAJOR-R5-2,
`start_replication_consumer_dispatch` permanent-stuck mark on
spawn/panic), and one DRY violation visible across the [I28] sweep
(five identical `coded_sql` helpers).

Findings tagged `[CRITICAL] / [MAJOR] / [MINOR] / [INFO]` per the brief,
with `file:line` evidence and verification commands.

---

## Verified recent commits

### `0049d9be` + `91830cca` — [I28] big sweep `Result<_, String>` → `DbError`

**Verified.** Six files migrated:

- `auth/bootstrap.rs` — `ensure_admin_schema` and its private helpers
  now `Result<_, DbError>`; one `Result<_, String>` site remaining
  (test-internal compile guard at L1065 in a doc comment).
- `auth/keys.rs` — `rotate_session_keys` / `current_key_id` /
  `previous_key_id` all typed.
- `auth/session.rs` — `mint_session_token` / `init_session` /
  `mint_and_init` / `mint_and_init_via_pool` all typed; type-level
  regression-guard test (`session_helpers_signatures_are_typed` at
  L466-499) makes a future flattening fail to compile.
- `replication.rs` — `sanitise_app_id` / `publication_name` /
  `slot_name` / `ensure_publication_and_slot` / `watchdog_query` /
  `drop_abandoned_slots` all typed. `prefix_message` helper preserves
  variant + code (L66-84).
- `diff.rs` — `coded_sql` helper added (L37-53); inspection paths now
  typed.
- `replication_ops.rs` — every dispatch returns `RejectError(e.to_op_error())`
  rather than the prior `OpResult::Failed { error: String }` rail.

Verifiable total: `Result<_, String>` site count is **45 → 19** (down
58%), grep with `grep -rEn "Result<.*,\s*String>" crates/plugin-db/src
| wc -l`. Of the 19 remaining:

- 11 are doc-comment references (incl. type-level guard test
  comments).
- 5 are intentional/documented (`validate.rs` x2 envelope contract;
  `error.rs` x2 module comments; `lib.rs::init_pool_async` x1
  plumbing).
- 3 are pure helpers (`hex_decode` / `hex_nibble` at session.rs L355,
  L369; `parse_commit_spec` / `parse_spec` at migration.rs L454, L742)
  — boundary helpers that produce throwaway error strings, low-priority.

So the live "fn returns `Result<_, String>` in production paths" count
is ~8 down from R3's ~30. The brief's "48→22" estimate is directionally
correct (a different counting methodology, but same magnitude).

The signature-pinning regression tests are the right discipline —
catching a future "flattening" refactor at compile time rather than
during code review.

### `bd1e7ce1` — [I42] `lock_guard.release()` reorder

**Verified.** The previous version flipped `self.released = true`
BEFORE the unlock SQL `.await`, so a future-drop / cancellation /
panic mid-await would skip the Drop's catastrophic-path log AND not
issue the unlock. The fix (`lock_guard.rs:126-148`) defers
`self.released = true` until AFTER the await completes. On
cancellation: `released = false` + `client = Some(_)` → Drop sees the
problem and fires the tracing::error.

This is the concurrency-r5 M-NEW-r5-1 fix called out in the comment
at L136-138. Cancellation observability is now correct.

What this does NOT fix: the underlying lock leak (R4 M-NEW-1 below)
when Drop fires and the client returns to the pool with the
session-scoped lock still alive. That's a separate, deeper issue.

### `e101fa95` — [I28] worktree commit

Inferred from message; same scope as `0049d9be` + `91830cca`.

### `5ceb6daa` — byte-eq prefix check + wal_consumer demotes

**Verified.** `query.rs:79-89` swaps the prior `name.to_ascii_lowercase()`
allocation for byte-slice `eq_ignore_ascii_case` against the literal
prefixes (`b"pg_"`, `b"__zeroship"`) — the lowercase string allocation
fired on EVERY CRUD dispatch. The bounds check (`bytes.len() >= 3` /
`bytes.len() >= 10`) is correctly placed before the slice index.

`wal_consumer.rs:131-185` demotes three `#[doc(hidden)] pub fn` shims
to `pub(crate)`. Internal callers (`local_emit_suppressed()` from
`emit_change()`) keep working; release-build pub surface shrinks.

### `07205e54` — 5 mint_* helpers demoted, validate.rs preamble fix

**Verified.** `mint_collection`, `mint_migrations`, `mint_replication`,
`mint_transaction`, `migration_start_with_spec` demoted from `pub`
to `pub(crate)`. `mint_db` and `mint_subscription` stay `pub` (used
externally — `mint_db` by the runtime plugin builder,
`mint_subscription` by the runtime's WS bridge test fixtures).

Asymmetry: `mint_db` returns `Option<v8::Local<...>>` (using `?`
short-circuits to `None`); the other five return `Result<..., OpError>`
with explicit error mapping. Non-blocking, but the inconsistency
costs a `match` at every call site (e.g. `db.rs:130` for
`mint_collection`). See M5-MIN-2 below.

### `4cbe9fa1` — `mint_subscription` defer broker subscribe

**Verified.** Closes R4 M-NEW-2. The function body (`subscription.rs:167-227`)
now performs every fallible V8 op (`new_instance`, `get_function`,
prototype lookup) BEFORE `broker::subscribe`. A `?` propagation
between the start of `mint_subscription` and the broker call now
returns without registering a broker entry — no leak.

The structural regression test
(`mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure`
at L287-342) walks the source text of `mint_subscription` and asserts
every `?` operator occurs BEFORE the literal `broker::subscribe(` —
clever; catches a future refactor that re-introduces the bug. Pairs
with the happy-path test
(`mint_subscription_happy_path_registers_exactly_one_broker_entry`
at L344-377) that drives a real V8 isolate and asserts the broker
entry IS registered when alloc succeeds.

### `37e61803` — migrations.rs update_backfill_progress before COMMIT

**Verified.** `migrations.rs:586-609` moves the progress UPDATE inside
the transaction (before the explicit COMMIT/ROLLBACK at L615). The
prior shape ran progress UPDATE after COMMIT released the row lock,
opening a race where `migrations.reset(...)` from a sibling caller
could clobber the cursor. Now both the data UPDATEs and the
progress UPDATE commit atomically; a reset between the data write
and the progress write is impossible because the row lock held by
`lock_audit_row_for_update` persists until COMMIT.

---

## New findings (R5)

### [MAJOR] MAJOR-R5-1 — `init_session` discriminates errors by message substring

**File:** `crates/plugin-db/src/auth/session.rs:178-237`

**Symptom:**
```rust
.map_err(|e| {
    // Walk the source chain — compio-postgres's top-level
    // Display is "db error"; the SQLSTATE-bearing inner
    // DbError is one source-hop away.
    let mut msg = format!("{e}");
    let mut cur: &dyn std::error::Error = &e;
    while let Some(src) = std::error::Error::source(cur) {
        msg.push_str(" | ");
        msg.push_str(&format!("{src}"));
        cur = src;
    }
    // Promote the structured RAISE messages to typed
    // ValidationFailed variants with stable `.code`s the SDK
    // can branch on.
    if msg.contains("nonce replay detected") {
        DbError::validation(
            "session_nonce_replay",
            "auth/session: nonce replay detected",
        )
    } else if msg.contains("signature expired") {
        DbError::validation(
            "session_signature_expired",
            "auth/session: signature expired",
        )
    } else if msg.contains("invalid session-init signature") {
        DbError::validation(
            "session_invalid_signature",
            "auth/session: invalid signature",
        )
    } else {
        coded_sql("init_session", e)
    }
})
```

The SDK's stable `.code` for nonce-replay, signature-expired, and
signature-invalid errors depends on three free-text substring matches
against the Postgres `RAISE EXCEPTION` body. This is fragile to:

1. **Locale changes** — Postgres `RAISE EXCEPTION` text can be
   localised via `lc_messages`. A `lc_messages = de_DE.UTF-8` cluster
   issues `'Replay-Nonce erkannt'` (hypothetical); the substring
   match silently falls through to `coded_sql("init_session", e)`,
   and the SDK loses the stable `.code` it branches on.
2. **Message-string drift** — The SECURITY DEFINER body owns the
   exact wording. A future edit ("nonce replay detected" →
   "nonce already presented") silently degrades the wire shape.
3. **Backslash-escape gotchas in source-chain walk** — `format!("{e}")`
   walks the error chain naively. If `compio_postgres::Error`'s top-
   level Display ever changes from `"db error"` to something that
   includes the SQLSTATE inline (e.g. `"db error 23505: detail..."`),
   `msg` shape changes and the substring check may match where it
   shouldn't (e.g. a generic message containing "expired" as
   substring of "expired session" → wrong code stamped).

**Why it's a problem:**

The Postgres SQLSTATE for `RAISE EXCEPTION USING ERRCODE = ...` is
the durable signal — `P0001` by default, or a custom 5-char SQLSTATE
the function picks. The current `from_pg` impl (`error.rs:147-188`)
does NOT handle `P0001` (it falls into the `Internal` catch-all),
which is why the substring fallback exists.

**Fix options:**

1. Switch the SECURITY DEFINER function to a custom SQLSTATE per
   error class (e.g. `RAISE EXCEPTION USING ERRCODE = 'ZS001'` for
   nonce-replay), then add an arm to `DbError::from_pg` that maps
   `ZS001` → `DbError::validation("session_nonce_replay", ...)`.
   This makes the discriminator structured and locale-independent.
2. If SQLSTATE customisation is out of scope, at minimum guard the
   `msg.contains(...)` arms with a check that the underlying error
   has SQLSTATE `P0001` (so a non-`P0001` error that happens to
   contain "expired" in its message body can't take the wrong arm).

**Fix code sketch:**
```rust
if let Some(sqlstate) = e.code() {
    if sqlstate.code() == "P0001" {
        if msg.contains("nonce replay detected") { ... }
        // ...
    }
}
coded_sql("init_session", e)
```

**Verification:**
- `grep -nC5 "nonce replay detected\|signature expired" crates/plugin-db/src/auth/session.rs`
- `grep -n "P0001\|UserRaised\|RAISE EXCEPTION" crates/plugin-db/src/error.rs` — confirms `from_pg` has no `P0001` arm.

### [MAJOR] MAJOR-R5-2 — `start_replication_consumer_dispatch` permanent-stuck mark

**File:** `crates/plugin-db/src/replication_ops.rs:244-252`

**Symptom:**
```rust
let app_for_task = app_id.clone();
crate::context::with_mut(|c| c.mark_consumer_running(&app_id));
compio::runtime::spawn(async move {
    crate::wal_consumer::run_supervised(consumer).await;
    crate::context::with_mut(|c| c.unmark_consumer_running(&app_for_task));
})
.detach();
```

Mark-then-spawn races: the `mark_consumer_running` write happens
**synchronously before** `compio::runtime::spawn`. Two paths leave
the app permanently marked despite no consumer being live:

1. **`spawn` itself fails.** `compio::runtime::spawn` is documented
   as panicking when the runtime is shut down. The panic propagates
   up through `state.borrow_mut().spawned_ops.push(...)`'s
   future poll, aborts the spawned-ops loop, but the
   `mark_consumer_running` row is already written. No subsequent
   call to `startReplicationConsumer` will spawn — the idempotency
   check at L181 short-circuits with `alreadyRunning: true`.
2. **`run_supervised` panics mid-loop.** The detached task panics
   before reaching the cleanup line (L250). compio's detached-task
   panic semantics depend on the runtime: most isolate panics abort
   the worker, but a defensive `catch_unwind` wrapper anywhere
   above (or a `Pin<Box<dyn Future>>` that catches a poll panic)
   would swallow it. The mark stays set; the cleanup never runs.

**Why it's a problem:**

The doc comment at L181-193 promises idempotency: "Subsequent calls
short-circuit and resolve with the cached outcome envelope plus
`alreadyRunning: true`." When the consumer is permanently dead but
permanently marked, the operator/SDK has no way to retry without
restarting the worker. Production-painful (silent dead-letter for
reactive queries on that app).

**Fix:**

Move the `mark_consumer_running` write INTO the spawned future, after
the supervisor task has started its decode loop (or wrap it in an
RAII guard):

```rust
let app_for_task = app_id.clone();
compio::runtime::spawn(async move {
    struct RunGuard(String);
    impl Drop for RunGuard {
        fn drop(&mut self) {
            crate::context::with_mut(|c| c.unmark_consumer_running(&self.0));
        }
    }
    crate::context::with_mut(|c| c.mark_consumer_running(&app_for_task));
    let _guard = RunGuard(app_for_task.clone());
    crate::wal_consumer::run_supervised(consumer).await;
    // _guard drops here on normal exit; also on panic unwind.
})
.detach();
```

Or, if compio's detached-task panic aborts the worker by default (it
likely does), at least wrap the inner await in `catch_unwind` and
explicitly call `unmark_consumer_running` on the error path. The
mark-then-spawn race remains either way; the RAII guard is the
cleanest fix.

**Verification:**
- `crates/plugin-db/src/replication_ops.rs:244-252`
- `grep -n "mark_consumer_running\|unmark_consumer_running" crates/plugin-db/src/` — confirm only one cleanup call site.
- `grep -n "fn spawn" crates/compio/src/runtime/` (or `compio::runtime` source) — confirm `spawn` panic behaviour on shut-down runtime.

### [MAJOR] MAJOR-R5-3 — Five copies of `coded_sql` after [I28] sweep

**Files:**
- `crates/plugin-db/src/audit.rs:55-71`
- `crates/plugin-db/src/auth/bootstrap.rs:21-37`
- `crates/plugin-db/src/auth/keys.rs:38-54`
- `crates/plugin-db/src/auth/session.rs:32-48`
- `crates/plugin-db/src/diff.rs:37-53`

Five near-identical helpers differ only in the literal prefix string
(`"audit: "`, `"auth/bootstrap: "`, `"auth/keys: "`, `"auth/session: "`,
`"diff: "`):

```rust
fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    let mut err: DbError = e.into();
    match &mut err {
        DbError::UniqueViolation { message }
        | DbError::FkViolation { message }
        | DbError::NotNullViolation { message }
        | DbError::CheckViolation { message }
        | DbError::Serialization { message }
        | DbError::LockContention { message }
        | DbError::Transient { message }
        | DbError::Internal { message } => {
            *message = format!("{prefix}{context}: {message}");
        }
        _ => {}
    }
    err
}
```

`replication.rs:66-84` has a sibling `prefix_message` that takes a
prefix `&str` directly — same pattern, different signature.

**Why it's a problem:**

- The [I28] sweep introduced the duplication. Five files now carry
  identical logic; adding a new typed variant (e.g. when error.rs
  gains `DbError::Coded`) requires editing six places (the five
  files plus `replication.rs`).
- Each call site couples a fixed literal prefix to the helper, so
  the helper has to be redefined per-file. The right shape is a
  single helper that takes the prefix:
  ```rust
  // In crate::error or a new crate::error::context module:
  pub(crate) fn coded_sql_with_prefix(prefix: &str, ctx: &str, e: compio_postgres::Error) -> DbError {
      let mut err: DbError = e.into();
      prefix_message(&mut err, &format!("{prefix}: {ctx}: "));
      err
  }
  ```
  and call as `coded_sql_with_prefix("auth/bootstrap", "CREATE TABLE", e)`.

**Fix:**

Extract one canonical helper into `crate::error`. Replace the five
copies + `prefix_message` with calls. Each call site shrinks from
a 17-line block + import to a single line.

**Verification:**
- `grep -nC1 "^fn coded_sql" crates/plugin-db/src/`
- `grep -nC1 "^fn prefix_message" crates/plugin-db/src/`

### [MAJOR] MAJOR-R5-4 — `WalConsumer::new` loses typed code at the dispatch boundary

**File:** `crates/plugin-db/src/wal_consumer.rs:326-344` paired with `crates/plugin-db/src/replication_ops.rs:218-234`

**Symptom:**

`WalConsumer::new` flattens `DbError` to a bare String:
```rust
let slot_name = crate::replication::slot_name(app_id)
    .map_err(|e| ConsumerError::NotProvisioned(e.to_string()))?;
let publication_name = crate::replication::publication_name(app_id)
    .map_err(|e| ConsumerError::NotProvisioned(e.to_string()))?;
```

The caller in `replication_ops.rs:218-234` rewraps as:
```rust
let consumer = match crate::wal_consumer::WalConsumer::new(&app_id, &url) {
    Ok(c) => c.with_start_lsn(setup.confirmed_flush_lsn.clone()),
    Err(e) => {
        return OpResult::JsValue {
            resolver,
            value: ResolveValue::RejectError(
                DbError::Configuration {
                    code: "not_provisioned",   // <-- LOST: was "invalid_app_id"
                    message: e.to_string(),
                }
                .to_op_error(),
            ),
            request_id,
        };
    }
};
```

`slot_name` / `publication_name` return `DbError::ValidationFailed
{ code: "invalid_app_id", ... }` for a bad app id (e.g. spaces,
empty, non-alphanumeric — see `replication.rs:107-126`). That code
is the SDK's stable discriminator. After the `e.to_string()` → bare
`ConsumerError::NotProvisioned(String)` → re-wrap as `DbError::Configuration
{ code: "not_provisioned" }`, the SDK now sees `.code = "not_provisioned"`
for an invalid-app-id error.

The doc comment at L327-332 even claims "The `DbError`'s `.code` is
preserved at the V8 dispatch boundary" — but the preservation is
the OPPOSITE of what the code does. The boundary STAMPS a hardcoded
`"not_provisioned"`, throwing away the upstream code.

**Why it's a problem:**

- The SDK that branches on `err.code === "invalid_app_id"` to refuse
  the user input cleanly now sees `"not_provisioned"` and routes
  the error through its "provisioning hasn't run yet" UX path. A
  user with a malformed app id gets "please run replicationSetup
  first" instead of "your app id is invalid".
- The doc comment is a lie that will mislead future contributors.

**Fix:**

Either:
1. Change `ConsumerError::NotProvisioned` to carry the typed
   `DbError` instead of `String`, and the caller forwards the typed
   error verbatim via `e.to_op_error()`.
2. Add a `ConsumerError::InvalidAppId(DbError)` variant; the caller
   matches and forwards the inner DbError's `.code`.

**Verification:**
- `grep -nC3 "NotProvisioned" crates/plugin-db/src/wal_consumer.rs crates/plugin-db/src/replication_ops.rs`
- `grep -n "invalid_app_id" crates/plugin-db/src/replication.rs`

### [MAJOR] MAJOR-R5-5 — `lock_guard.release()` silently swallows unlock SQL failure

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:140-148`

**Symptom:**
```rust
if let Some(client) = self.client.as_ref() {
    let unlock_sql =
        "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
    let _ = client
        .query_text_params(unlock_sql, &[self.key.as_str(), self.tag])
        .await;
}
self.released = true;
Ok(self.client.take())
```

When the unlock SQL itself fails (network hiccup, connection torn
down between Pass 1 and the unlock, server-side OOM, ...), the
`let _ =` swallows the error AND L147 still flips
`self.released = true`. The Drop path then no-ops (no log), but the
session-scoped advisory lock is still held. The client returns to
the pool — same M-NEW-1 leak as R4, but now with no observability.

The comment at L120-123 says: "best-effort: errors from the
underlying `query_text_params` are swallowed (matches the pre-
existing inline sites; the session-scoped lock will auto-release
when the backend session ends if the explicit unlock failed)". The
auto-release claim is conditional on the pool eventually closing
the connection — which `compio_postgres::Pool` does not do by TTL
default (verify against the pool's eviction policy).

**Why it's a problem:**

[I42] (`bd1e7ce1`) fixed the cancellation-mid-await case: a future
drop / panic between the await call and the state flip leaves
`released = false`, so Drop fires its catastrophic log. But the
**unlock-SQL-succeeded-the-syntax-check-but-failed-at-runtime** case
is not covered — the await completes, but with an `Err`, and L147
still flips the flag.

**Fix:**

Only flip `released = true` on successful unlock:
```rust
if let Some(client) = self.client.as_ref() {
    match client.query_text_params(unlock_sql, &[...]).await {
        Ok(_) => self.released = true,
        Err(e) => {
            tracing::error!(
                key = %self.key, tag = %self.tag, error = ?e,
                "OrchestratorLockGuard: unlock SQL failed; lock will stay held",
            );
            // self.released stays false → Drop fires its own log.
        }
    }
} else {
    self.released = true;  // no client → nothing to unlock; idempotent.
}
Ok(self.client.take())
```

The Drop log then becomes the canonical "lock leaked" signal.
Operators can grep for it; a future taint-on-Drop API can use the
`released` flag to know whether to taint the connection.

**Verification:**
- `crates/plugin-db/src/orchestrator/lock_guard.rs:140-148`
- `grep -n "pg_advisory_unlock" crates/plugin-db/src/` — confirm the unlock-SQL sites still all flow through `release()`.

### [MINOR] MIN-R5-1 — `auth/session.rs::iso_timestamp_after` non-saturating addition

**File:** `crates/plugin-db/src/auth/session.rs:300-307`

```rust
fn iso_timestamp_after(ttl_secs: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let total_ms = now + ttl_secs.saturating_mul(1000);
    format_unix_millis(total_ms)
}
```

`now + ttl_secs.saturating_mul(1000)` is a plain `i64 + i64` that
overflows on debug-build (panic) and wraps on release-build. Inputs
that produce overflow: `ttl_secs = i64::MAX / 1000` saturates the
multiply to `i64::MAX`, then `now + i64::MAX` overflows for any
positive `now`. The caller in production passes `Some(positive_int_64)`
or `None` (defaults to `DEFAULT_TOKEN_TTL_SECS` = 300 seconds), so
this isn't reachable today — but the function is `pub`-ish (private
helper of `mint_session_token`, callable from tests).

**Fix:**

`let total_ms = now.saturating_add(ttl_secs.saturating_mul(1000));`

One-character change. Defense in depth.

**Verification:**
- `crates/plugin-db/src/auth/session.rs:305`

### [MINOR] MIN-R5-2 — `mint_db` returns `Option<v8::Local>`; siblings return `Result<_, OpError>`

**File:** `crates/plugin-db/src/v8_classes/db.rs:367-409`

`mint_db` uses `?` on `new_instance(scope)?` / `get_function(scope)?` /
`get(scope, proto_key.into())?` and returns `None` on any failure.
Every other `mint_*` helper (`mint_collection`, `mint_replication`,
`mint_transaction`, `mint_subscription`, `mint_migrations`) returns
`Result<v8::Local<...>, OpError>` with `.ok_or_else(|| OpError::type_error("..."))?`
mapping. The asymmetry costs:

1. Call sites of `mint_db` (1 today, in `lib.rs`) must `match` the
   `Option` and synthesize an error message; the sibling helpers
   already carry a descriptive error message.
2. The error message identifying WHICH step failed is lost in
   `mint_db`'s `?` chain — a `None` from `new_instance` is
   indistinguishable from a `None` from prototype lookup.
3. Future contributors adding a new step have a 50/50 chance of
   picking the wrong return shape.

**Fix:**

Migrate `mint_db` to `Result<v8::Local<...>, OpError>` with the same
`.ok_or_else(|| OpError::type_error("..."))?` pattern. Update the
single caller in `lib.rs` to forward via `?` or match on `Err(e)`
and propagate the typed error.

**Verification:**
- `crates/plugin-db/src/v8_classes/db.rs:367`
- `grep -rn "mint_db(" crates/plugin-db/ crates/runtime/` — confirm caller count.

### [MINOR] MIN-R5-3 — Six near-identical `mint_*` bodies

**Files:**
- `db.rs:367-409`
- `collection.rs:353-393`
- `replication.rs:117-155`
- `transaction.rs:288-329`
- `subscription.rs:167-227` (the broker-aware variant; structural difference is the deferred subscribe)
- `migrations.rs:251-289`

Each implements the same V8 wrapper-build dance:
1. `Class::install(scope)`
2. `class_tmpl.instance_template(scope).new_instance(scope)`
3. `class_tmpl.get_function(scope)` → prototype lookup → `set_prototype`
4. `Box::new(state)` → `Box::into_raw` → `External::new` → `set_internal_field(0, ext.into())`
5. `Weak::with_guaranteed_finalizer` + `mem::forget`

A trait or macro abstraction would collapse 5 of the 6 to ~3 lines
each (the `Subscription` mint stays special-cased due to the broker
subscribe ordering invariant — and even that could be a hook the
trait method calls between steps 4 and 5).

Not a correctness bug; pure code-debt. The six bodies are currently
~40 lines each = ~240 lines of near-duplicate code in `v8_classes/`.

**Fix:**

Extract a helper:
```rust
pub(crate) fn install_wrapper_state<'s, T: 'static>(
    scope: &mut v8::PinScope<'s, '_>,
    class_tmpl: v8::Local<'s, v8::FunctionTemplate>,
    state: T,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| OpError::type_error(&format!(
            "{} instance allocation failed", std::any::type_name::<T>()
        )))?;
    // ... rest of the dance ...
}
```

The `Subscription` variant takes the same shape with a `before_finalizer:
impl FnOnce(...)` parameter (or just inlines the broker subscribe
between steps 4 and 5).

**Verification:**
- `grep -nE "^pub(\(crate\))? fn mint_" crates/plugin-db/src/v8_classes/`

### [MINOR] MIN-R5-4 — Broker `push` / `close` still wake while RefCell is borrowed

**File:** `crates/plugin-db/src/broker.rs:319-321`, `:325-327`, `:366-368`

R4 M-NEW-4 status: **still open.** Three `w.wake()` call sites still
fire while `inner: RefMut<SubscriptionInner>` is alive. Today's
compio waker just enqueues a wake-up and returns, so no production
panic — but the pattern is fragile (custom executor, test harness
with synchronous wake, `noop_waker` composition).

Same fix as R4: hoist the waker out and drop the borrow before
waking:
```rust
let waker = inner.waker.take();
drop(inner);
if let Some(w) = waker { w.wake(); }
```

Three-line change x 3 sites = 9 LOC.

### [MINOR] MIN-R5-5 — `exec.rs::run_sql` still not cancellation-safe

**File:** `crates/plugin-db/src/exec.rs:43-57`

R4 M-NEW-5 status: **still open.** The `take → await → put`
pattern at L49-56 leaks the transaction client on future cancellation
(isolate shutdown, async drop). Once the slot is None, subsequent
`run_sql` calls under the same tx see `take_tx_client() == None` and
fire the `"db: transaction connection lost"` Internal error. Same
RAII guard fix as r4.

### [INFO] INFO-R5-1 — `into_held` still dead code

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:152-165`

R4 M-NEW-3 status: **unchanged.** `#[allow(dead_code)]` still
present; zero callers in the workspace. The doc comment justifies
keeping it as "future caller scaffolding"; the `.expect()` at L164
remains a latent panic site if a test or refactor uses
`for_test_no_client` to construct a clientless guard and then calls
`into_held`. Not score-affecting; documented and gated.

### [INFO] INFO-R5-2 — Test helpers still mint compio runtimes

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:218-223`

R4 M-NEW-7 status: **unchanged.** `compio::runtime::Runtime::new().unwrap()`
in `release_idempotent_when_no_client` still produces a per-test
runtime with an `.unwrap()` panic site. Migrate to `#[compio::test]`
(if available) or a shared fixture. Low priority; test-only.

---

## R4 carry-over status

| R4 item | Status | Detail |
|---|---|---|
| M-NEW-1 (Drop leaks locked client) | **partial** | [I42] `bd1e7ce1` fixes the cancellation observability — a cancelled await now leaves `released=false`, so Drop fires its log. But on the regular `let _ = lock_guard.release()` path, an unlock-SQL failure still flips `released=true` and silently swallows the error. See MAJOR-R5-5. |
| M-NEW-2 (mint_subscription leaks broker) | **closed by `4cbe9fa1`** | broker::subscribe deferred until after all V8 `?`-able ops. Structural regression guard. |
| M-NEW-3 (into_held dead code) | **unchanged** | See INFO-R5-1. |
| M-NEW-4 (Subscription::push waker-while-borrowed) | **unchanged** | See MIN-R5-4. |
| M-NEW-5 (run_sql cancellation race) | **unchanged** | See MIN-R5-5. |
| M-NEW-6 (into_held doc comment lies) | **unchanged** | Doc still reads "future callers" — see INFO-R5-1. |
| M-NEW-7 (test helper creates per-call runtime) | **unchanged** | See INFO-R5-2. |
| I1 (release_advisory_lock void return) | **closed by architectural displacement** | Carries over from R4. |
| I2 (`Result<_, String>` site count) | **major progress** | 45 → 19. The six [I28] target files are functionally complete. See above. |
| I3 (replication empty-LSN sentinel) | **closed by `c83d6a8c`** (R4 verification). |
| M1 (`v8_bridge.rs:217` try_into unwrap) | **unchanged** | Still: `let arr: v8::Local<v8::Array> = v.try_into().unwrap();` |
| M2 (`v8_bridge.rs:171` i64 off-by-one) | not re-verified |
| M5 (migrations `finalise_backfill` swallow) | **unchanged** | Still `let _ = backend.finalise_backfill(...).await;` at `migrations.rs:639`. |

---

## Audit summary — eight dimensions

| Dimension | Finding | Severity |
|---|---|---|
| 1. RefCell-across-await | No new patterns from [I28] (sweep touched non-V8 files only). `broker.rs::push/close` waker-while-borrowed pattern (MIN-R5-4) unchanged. `lock_guard::release` correctly issues SQL via `client.as_ref()` (no RefCell, no concern). `auto_tx::auto_end` clears flag BEFORE awaiting commit — correct ordering. | MINOR (latent) |
| 2. Unsafe | 9 finalizer-pattern blocks (`v8_classes/{db,collection,migration,migrations,replication,subscription,transaction}.rs`). Zero new unsafe from [I28] / [I42] / `4cbe9fa1` / `07205e54` / `5ceb6daa`. All 9 paired with `Box::into_raw` constructor + SAFETY comments naming the keepalive Global. | OK |
| 3. Panic risks | Production `.unwrap()` count: 17 V8 `v8::String::new(scope, "...").unwrap()` (infallible in practice — empty-string alloc never fails outside OOM). 2 `try_into().unwrap()` in `v8_bridge.rs` (L217 array cast post-`is_array`; L414/L437 byte-slice → fixed-array post-length check — both safe but stylistically panicky). 1 `lock_guard::into_held::expect` (dead code, see INFO-R5-1). New code from [I28] introduces **zero** new panic sites. | MINOR |
| 4. Error handling typing | `Result<_, String>` site count: 45 → 19 (down 58%). Live "fn signatures returning bare String" count: ~8 down from ~30. The [I28] sweep was thorough and well-tested. Five `coded_sql` duplicates (MAJOR-R5-3) are the cost of doing the migration per-file rather than via a shared helper. | MINOR (chronic; major progress this round) |
| 5. Lifetimes | `OrchestratorLockGuard<'p>` propagates cleanly. [I28] added no new lifetime-bearing APIs: all six migrated files use owned types (`String`, `Vec<_>`, `DbError`) or `&str` borrows that don't escape the function. No higher-rank bounds. `pub async fn ensure_publication_and_slot(pool: &Pool, app_id: &str)` shape is canonical. | OK |
| 6. Idiomatic patterns | `replication::sanitise_app_id` / `publication_name` / `slot_name` use `?` propagation through the typed error rail — clean. `lock_guard::release` uses `if let Some(client) = self.client.as_ref()` rather than `if let Some(ref client) = self.client` — current idiom. `auto_tx::begin_to_resolve_value` / `end_to_resolve_value` (R4 finding) still exemplary. Six `mint_*` helpers (MIN-R5-3) and five `coded_sql` (MAJOR-R5-3) are idioms violations introduced by code growth. | MAJOR (DRY) |
| 7. Resource lifecycle | `OrchestratorLockGuard` (M-NEW-1 R4 partial; MAJOR-R5-5 R5). `Subscription::Drop` clean. `Transaction::Drop` token-fenced. `Migration::Drop` catch_unwind defense. New finding: `mark_consumer_running` race (MAJOR-R5-2). `mint_subscription` broker-leak fix (`4cbe9fa1`) is good RAII discipline. | MAJOR |
| 8. Type ascription | `begin_to_resolve_value` / `end_to_resolve_value` signatures still clean. `coded_sql`'s `fn(&str, compio_postgres::Error) -> DbError` signature is the right shape — it's the duplication that's the problem, not the per-fn ergonomics. No turbofish abuse, no `as` casts in production paths beyond V8 plumbing (which is unavoidable for the finalizer pattern). | OK |

---

## Score breakdown

| Dimension | R1 | R2 | R3 | R4 | R5 | Change |
|---|---|---|---|---|---|---|
| Correctness | 72 | 80 | 84 | 84 | 84 | MAJOR-R5-1 (substring matching) + MAJOR-R5-2 (consumer mark race) + MAJOR-R5-4 (NotProvisioned code loss) offset by [I42] cancellation fix + `4cbe9fa1` broker leak fix. Net flat. |
| Performance | 84 | 84 | 84 | 86 | 88 | `5ceb6daa` per-CRUD lowercase alloc removed; `37e61803` cleaner row-lock ordering removes a contention edge in `migrations.commitBatch`. |
| Security | 88 | 88 | 90 | 90 | 90 | No regression. R3+R4 fixes hold. |
| API design | 76 | 82 | 84 | 86 | 87 | `07205e54` demotes five mint_* helpers to `pub(crate)` — pub surface shrinks. `mint_db` vs siblings asymmetry (MIN-R5-2) is a minor blemish. |
| Rust idioms | 80 | 84 | 86 | 87 | 87 | [I28] typed-error sweep is exemplary discipline (type-level regression tests). The cost: five `coded_sql` duplicates (MAJOR-R5-3) and six `mint_*` near-duplicates (MIN-R5-3) — DRY ceiling has lowered the dimension's headroom. Net flat. |
| **Overall** | **78** | **85** | **87** | **88** | **89** | Net +1. The [I28] sweep is the dominant signal; closure of R4 M-NEW-2 + the new MAJORs balance out. |

---

## Ceiling-blockers for 90+

In priority order:

1. **MAJOR-R5-1** — `init_session` substring matching on RAISE messages.
   Switch to custom SQLSTATE in the SECURITY DEFINER body + arm in
   `from_pg`. One-file fix; biggest semantic improvement (durable
   `.code` for the SDK).
2. **MAJOR-R5-2** — `start_replication_consumer_dispatch` mark race.
   Move `mark_consumer_running` into the spawned future + RAII
   `RunGuard` for cleanup-on-panic. One-file fix.
3. **MAJOR-R5-5** — `lock_guard.release()` swallowed unlock-SQL error.
   Move `self.released = true` inside the `Ok(_)` arm. Two-line fix.
4. **MAJOR-R5-4** — `WalConsumer::new` typed-code loss. Either
   carry typed `DbError` through `ConsumerError`, or add an
   `InvalidAppId(DbError)` variant.
5. **MAJOR-R5-3** — Five `coded_sql` duplicates. Extract to
   `crate::error`. ~120 LOC removed, single-point-of-update for
   variant additions.
6. **R4 M-NEW-1 underlying leak** — Drop returns locked client to
   pool. Either taint-on-Drop (compio_postgres support needed) or
   close the connection explicitly inside Drop.
7. **R4 M-NEW-4 / MIN-R5-4** — Broker `push`/`close` wake-while-
   borrowed. 3 sites x 3 lines.
8. **R4 M-NEW-5 / MIN-R5-5** — `run_sql` cancellation race. RAII
   guard for the take/put round-trip.

Items below the cut (MIN-R5-1 / MIN-R5-2 / MIN-R5-3 / INFO-R5-1 /
INFO-R5-2) are cosmetic or pre-existing; not score-affecting.

---

## Verification commands

```bash
# RefCell-across-await sweep
grep -rn "borrow_mut\|borrow()" crates/plugin-db/src | wc -l   # ~30 sites; none introduced by [I28]

# Unsafe blocks
grep -rn "unsafe {" crates/plugin-db/src                       # 9 finalizer sites; zero new

# Panic sites (production paths only)
grep -rn "\.unwrap()\|\.expect(" crates/plugin-db/src | grep -v "/tests/" | grep -v "mod tests"

# Result<_, String> remaining
grep -rEn "Result<.*,\s*String>" crates/plugin-db/src | wc -l   # 19 (down from 45)
grep -rEn "Result<.*,\s*String>" crates/plugin-db/src | awk -F: '{print $1}' | sort | uniq -c | sort -rn

# coded_sql duplicates
grep -rn "^fn coded_sql" crates/plugin-db/src                  # 5 copies (audit, auth/*, diff)

# mint_* helpers
grep -rnE "^pub(\(crate\))? fn mint_" crates/plugin-db/src/v8_classes/   # 6 near-duplicate bodies

# Lock guard call sites
grep -rn "OrchestratorLockGuard\|into_held\|guard.release" crates/plugin-db/src

# I28 typed-error signatures
grep -nC3 "session_helpers_signatures_are_typed" crates/plugin-db/src/auth/session.rs

# Substring-matching error discrimination
grep -nC5 "nonce replay detected\|signature expired\|invalid session-init signature" crates/plugin-db/src

# Consumer-running mark race
grep -nC3 "mark_consumer_running\|unmark_consumer_running" crates/plugin-db/src

# Broker waker-while-borrowed
grep -nC3 "w.wake()\|.wake();" crates/plugin-db/src/broker.rs
```
