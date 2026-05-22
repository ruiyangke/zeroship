# plugin-db Security Review — Round 7 (2026-05-22)

Target: `crates/plugin-db/` at `/home/ruiyang/Projects/appbase`.

Prior rounds: r1-r5; r6 (cycle 06:00) scored 78/100. Five commits
landed since r6:

| Commit | Subject |
| --- | --- |
| `70921112` | atomic `try_mark_consumer_running` + `try_claim` race fix |
| `aa639715` | `WalConsumer::new` typed `Result<_, DbError>` |
| `a272d1af` | P0001 DETAIL token classification (bootstrap.rs CREATE FUNCTION + `classify_p0001_detail`) |
| `386f9bf5` | lift `ConsumerRunningGuard` to module scope + 4 lifecycle tests, fix `then_some -> then` latent bug |
| `e5315083` | dead-code/docstring cleanups: dead `format!` in `init_session`, gate `mark_consumer_running` to test/test-helpers, doc fixups |

This round is a fresh re-audit against HEAD (`7df5e2a2`).

---

## 1. Findings

### 1.1 P0001 DETAIL classification — DETAIL is tenant-influenceable? (NO)

**Verdict: no finding. The DETAIL tokens are static literals in the
SECURITY DEFINER CREATE FUNCTION body; a tenant cannot influence them.**

The CREATE FUNCTION body in
`crates/plugin-db/src/auth/bootstrap.rs:521-559` sets DETAIL via:

```sql
RAISE EXCEPTION 'session-init signature expired'
  USING ERRCODE = 'P0001',
        DETAIL = 'session_signature_expired';   -- static literal
```

All 5 DETAIL values are **static string literals** in the DDL — no
interpolation, no parameter substitution, no concatenation. Verified
by grep of every DETAIL site:

```
crates/plugin-db/src/auth/bootstrap.rs:525:   DETAIL = 'session_signature_expired';
crates/plugin-db/src/auth/bootstrap.rs:531:   DETAIL = 'session_invalid_actor_kind';
crates/plugin-db/src/auth/bootstrap.rs:536:   DETAIL = 'session_nonce_too_short';
crates/plugin-db/src/auth/bootstrap.rs:550:   DETAIL = 'session_nonce_replay';
crates/plugin-db/src/auth/bootstrap.rs:558:   DETAIL = 'session_invalid_signature';
```

Where tenant input DOES appear in a RAISE message (lines 529, 736,
772 — `'invalid actor_kind: %', p_actor_kind` etc.) it lands in the
MESSAGE via PL/pgSQL `%` parameter substitution (not string
concatenation; not eligible for secondary SQL injection). It never
appears in DETAIL. The discriminator the Rust side reads is
**unspoofable** from the tenant's side.

Two additional defence layers worth noting:

1. The Rust classifier (`auth/session.rs:174-202`) gates on
   `db_err.code() != &SqlState::RAISE_EXCEPTION` first
   (SQLSTATE = `P0001` numeric constant, not a string), then matches
   `db_err.detail()?` against a closed set of 5 literals via `match`
   with `_ => None` fall-through. A future PG version that adds a new
   DETAIL semantics, or a non-P0001 error that happens to carry one
   of these tokens in DETAIL, would simply fall through to
   `coded_sql` — not be misclassified.
2. The `init_session` SQL function is `SECURITY DEFINER`. A
   tenant-controlled connection cannot replace the function body or
   inject into the DDL — only the owner role
   (`PLATFORM_ROLE = __zeroship_admin`) can drop/recreate it.

**No finding.** This is a textbook example of using SQLSTATE+DETAIL
as a typed channel. It strictly improves on the r5/r6 substring
classification (which was robust but locale/formatter-fragile).

```
[INFO] crates/plugin-db/src/auth/bootstrap.rs:521-559 — DETAIL classification
  Why: DETAIL tokens are static literals in the SECURITY DEFINER CREATE FUNCTION
       body; tenant cannot influence the discriminator. Plus the Rust classifier
       gates on the SQLSTATE constant (P0001) before consulting DETAIL, with a
       closed-set match + `_ => None` fall-through.
  Fix: none (security-improving change vs. the r5/r6 substring path).
  Verification:
       Grep -n "DETAIL" crates/plugin-db/src/auth/bootstrap.rs
       (5 hits — all on static literals)
       Grep -n "DETAIL\|RAISE EXCEPTION" crates/plugin-db/src/auth/bootstrap.rs
       Read crates/plugin-db/src/auth/session.rs:174-202
```

---

### 1.2 `try_mark_consumer_running` — cross-app isolation

**Verdict: no finding. Per-app keying makes cross-app collision
impossible by construction.**

`IsolateDbContext::try_mark_consumer_running` (`context.rs:433-435`)
is a one-liner:

```rust
pub fn try_mark_consumer_running(&mut self, app_id: &str) -> bool {
    self.running_consumers.insert(app_id.to_string())
}
```

`running_consumers: HashSet<String>` is keyed by raw app_id. The
`HashSet::insert(...)` returns `true` if newly inserted (caller won
the race) and `false` if already present (caller lost).

Cross-app isolation: app A's `app_id` and app B's `app_id` are
distinct strings, so app A's mark cannot collide with app B's. The
underlying `HashSet` keys them independently. Same primitive
guarantee as the prior `mark_consumer_running`; the atomicity
upgrade is purely about race-with-self (concurrent
`startReplicationConsumer()` from the same isolate).

The `ConsumerRunningGuard::try_claim` site (`replication_ops.rs:356-359`)
correctly threads the resolved (stamped) `self.app_id` through to
the HashSet — verified by:

```bash
Grep "ConsumerRunningGuard::try_claim\|try_mark_consumer_running" crates/plugin-db/src/
```

Two-call sites: (a) `replication_ops.rs:289` (production spawn,
uses `app_for_task = self.app_id.clone()`); (b) `replication_ops.rs:357`
(inside `try_claim`).

There is no path where a tenant-supplied opts.appId can reach
`try_mark_consumer_running` — the stamped `self.app_id` is used end
to end. (r5 NEW CRITICAL closure pinned by `resolve_consumer_app_id`
override-rejection unit test, still in place at
`v8_classes/replication.rs:206-340`.)

**No finding.**

---

### 1.3 `ConsumerRunningGuard` lazy `then` vs eager `then_some` — bug-window?

**Verdict: no finding remaining. The latent bug was caught and fixed
by `386f9bf5` before any production deploy used the racy variant.**

The 70921112 patch initially used `won.then_some(Self { app_id })` —
which eagerly constructs `Self { app_id }` even on the lost-race
path, immediately drops the temporary, which fires `Drop` and
unmarks the WINNER's claim (`replication_ops.rs:362-366`). Both
racing tasks then exit with no consumer running.

`386f9bf5` replaced `then_some` with `then(|| Self { app_id })`
(`replication_ops.rs:358`), and added a lifecycle test
`consumer_running_guard_try_claim_loses_when_already_marked`
(`replication_ops.rs:413-424`) that fails on the regression.

Window of exposure: 70921112 (`05:27`) → 386f9bf5 (`05:53`). No
production deploys in the interval; the bug was a self-DoS
(supervisor task exits and the running marker is wrong, but
nothing else is corrupted — the next `startReplicationConsumer`
call would re-claim cleanly because `unmark_consumer_running` had
fired). Not a cross-tenant nor a privilege-escalation bug.

The fix is structurally correct: `then(|| ...)` evaluates the
closure only on `true`. The docstring at
`replication_ops.rs:350-355` explicitly documents the load-bearing
laziness so a future contributor doesn't revert it.

**No finding.** The regression test is in place; security delta
**positive** (a self-DoS race window of ~25 minutes is closed and
pinned).

---

### 1.4 `WalConsumer::new` typed `Result<_, DbError>` — leak of `db_url`?

**Verdict: no finding. The `not_provisioned` message is a static
literal; no path interpolates the URL.**

The new constructor (`wal_consumer.rs:346-368`) returns:

- `DbError::ValidationFailed { code: "invalid_app_id", … }` for
  sanitise failures (message: `"replication: app_id contains
  invalid character ..."`, allocating `format!` but does NOT touch
  `db_url`). Verified at `replication.rs:91-97`.
- `DbError::Configuration { code: "not_provisioned", message:
  "wal consumer: db_url not configured (replication requires a
  connected runtime context)" }`. The message is a **static
  string literal**; `db_url` itself does not appear. Verified at
  `wal_consumer.rs:347-353`.

The dispatch path (`replication_ops.rs:245-253`) calls
`e.to_op_error()` which routes both variants to `OpError::coded`
verbatim. The tenant SDK sees `e.code = "invalid_app_id"` or
`e.code = "not_provisioned"` with a static message body.

I also re-walked the broader `db_url` propagation surface to look
for any path where the URL string could leak through an error
message:

- `lib.rs:355-368` (`init_pool_async`): on `Pool::connect(&url, 8)`
  failure, the formatter is `format!("db: failed to connect: {e}")`
  + a source-chain walk. `compio_postgres::Error::Display`
  (`crates/compio-postgres/src/error/mod.rs:394-403`) emits stable
  short strings ("invalid connection string", "error connecting to
  server", "authentication error" etc.) without embedding the URL.
  The source chain may include a `url::ParseError`, which describes
  structural problems ("invalid host", "invalid port") and does
  **not** embed the input string. **Not a leak today.**
- `wal_consumer.rs:391-394` (`WalConsumer::run`): an `url.parse::
  <compio_postgres::Config>()` failure is wrapped as
  `ConsumerError::Connect(e.to_string())`. This error flows only
  to `tracing::warn!`/`tracing::error!` inside `run_supervised`
  (`wal_consumer.rs:763-771`) — it is **operator-facing logs**,
  not tenant-facing. The detached `spawn` means it can't reach the
  JS promise. No tenant leak.
- `orchestrator/register_model/mod.rs:119-121`: same shape as
  `init_pool_async` — only flows from the underlying String error,
  which doesn't carry the URL.

**No finding.** The dedicated `Configuration { code:
"not_provisioned" }` path is leak-free and lets the SDK branch
cleanly between developer-error (`invalid_app_id`) and
operator-error (`not_provisioned`).

---

### 1.5 `mark_consumer_running` cfg-gated — production reachability

**Verdict: no finding. The function is unreachable from production
code; the `#[cfg(any(test, feature = "test-helpers"))]` gate is
correctly placed and there are no production callers.**

`context.rs:423-426`:

```rust
#[cfg(any(test, feature = "test-helpers"))]
pub fn mark_consumer_running(&mut self, app_id: &str) {
    self.running_consumers.insert(app_id.to_string());
}
```

All callers verified:

```bash
Grep "mark_consumer_running" crates/plugin-db/
```

Production call-sites: **zero**. The only non-doc references are
inside `#[cfg(test)] mod tests` blocks at `context.rs:889-948`.
Production replication path (`replication_ops.rs:289 →
ConsumerRunningGuard::try_claim → try_mark_consumer_running`) uses
the atomic variant exclusively.

I confirmed the test module boundaries with:
```bash
grep -n "^mod tests\|^#\[cfg(test)\]" crates/plugin-db/src/context.rs
# 480:#[cfg(test)]
# 481:mod tests {
```

All `mark_consumer_running` callers in `context.rs` are at lines
893, 902, 905, 913, 914, 926, 935, 936, 937 — all within the
`mod tests` block starting at 481.

`test-helpers` is an opt-in cargo feature exposed for integration
test crates that need the same helper without `#[cfg(test)]`
visibility. Production builds (default features) will not compile
in the function — verified at `context.rs:423` (`#[cfg(any(test,
feature = "test-helpers"))]`).

**No finding.** The footgun-removal is exactly what api-surface
r6 MAJOR-R6-1 requested; production attack surface narrowed by
one method.

---

### 1.6 SQL injection sweep — re-walked

**Verdict: no new finding. The only SQL-shape changes in the
r6→r7 delta are constant-string additions (DETAIL clauses).**

Diff-walked added SQL surface:

```bash
git diff 9caf4f2e..HEAD -- crates/plugin-db/ | grep -E "^\+" | \
  grep -iE "format!|SQL|EXECUTE|RAISE|publication_name|slot_name"
```

The only matches are: (a) the 5 new DETAIL clauses (static
literals — analysed in 1.1); (b) `slot_name(app_id)?` and
`publication_name(app_id)?` in the typed `WalConsumer::new`
(`wal_consumer.rs:359-360`) — both already routed through
`sanitise_app_id` which gates `[A-Za-z0-9_]` only; (c) `format!`
in `coded_sql` (test fixtures only).

The 18-site `audit.rs` raw `"{app_id}"` interpolation surface
(carryover IMPORTANT from r5) is unchanged. Same conclusion:
within-tenant only, gated by `validate_app_id` which has no
length cap. Still tracked.

The migration row-UPDATE column-name length truncation
(`migrations.rs:563-565`) is unchanged; within-tenant only.

```
[MINOR] crates/plugin-db/src/audit.rs:805-822 — `validate_app_id` has no length cap
  Why: 64-byte-plus app_id passes the char-set gate but Postgres silently truncates
       schema/table names to 63 bytes (NAMEDATALEN). Two app_ids sharing a 63-byte
       prefix would collide on the same `__zeroship_migrations` table.
       Today typed_id-derived app_ids are well under 63 bytes, so the risk is
       defence-in-depth.
  Fix: add `if name.len() > 63 { return Err(DbError::validation("invalid_app_id", ...)); }`
       after the emptiness check; mirror `query.rs::validate_collection:72-76`.
  Verification: Grep "fn validate_app_id" crates/plugin-db/src/audit.rs -A 18
```

(Carryover from r5/r6; unchanged.)

```
[MINOR] crates/plugin-db/src/query.rs:127-142 — `validate_schema` no 63-byte cap, accepts leading digits
  Why: same truncation/collision class as audit::validate_app_id.
  Fix: add the byte-length cap; consider a leading-digit guard (mirror PG's identifier rules).
  Verification: Read crates/plugin-db/src/query.rs offset 125 limit 25
```

(Carryover from r5/r6; unchanged.)

---

### 1.7 App-id isolation re-verification (post r6→r7 commits)

**Verdict: still intact across all `#[v8_method]` sites; no new
drift.**

I re-walked every `#[v8_method]` definition in
`crates/plugin-db/src/v8_classes/` and re-confirmed each method's
app-id source. The table is unchanged from r6 §1.1; the
post-r6 commits do not add any new `#[v8_method]` and do not
change the app-id sourcing at any existing site. Specifically:

- `try_mark_consumer_running` is called from
  `ConsumerRunningGuard::try_claim`, which receives `app_for_task`
  from `replication_ops.rs:287` — that's the `app_id` resolved by
  `resolve_consumer_app_id(&self.app_id, ...)` at
  `v8_classes/replication.rs:241`, i.e. the stamped value (pinned
  by the override-rejection unit tests at
  `v8_classes/replication.rs:206-340`).
- The new test module at `replication_ops.rs:368-425` uses
  hardcoded test app_ids (`"guard_t1"`, etc.) and is gated by
  `#[cfg(test)]` — not a runtime surface.

**No new finding.** Cross-app scope closure (r5 NEW CRITICAL via
c0590506) remains durable.

---

### 1.8 [I43] blocking `pg_advisory_lock` — within-app DoS

**Verdict: unchanged. Carryover from r3-r6; no r7 delta.**

`orchestrator/lock_guard.rs:117-130` still calls
`backend.acquire_advisory_lock(...)` which uses the blocking
`pg_advisory_lock(hashtext($1), hashtext($2))`. Per-app
namespacing (key = `"zs_reg:<app_id>"`) means cross-tenant
impact is null. Within-app: one runaway `registerModel` loop can
tie up one pool client. With pool size N this leaves N-1 clients
free; not a full pool-exhaustion vector.

[I43] tracks the `pg_try_advisory_lock` migration in the deferred
backlog. Not a security delta.

```
[INFO] crates/plugin-db/src/orchestrator/lock_guard.rs:117 — blocking pg_advisory_lock (carryover [I43])
  Why: within-app DoS surface (one pool client tied up per concurrent registerModel).
       Cross-tenant impact null due to per-app lock-key namespacing.
  Fix: tracked in [I43] backlog; pg_try_advisory_lock + lock_not_available retry.
  Verification: Grep "acquire_advisory_lock|pg_advisory_lock" crates/plugin-db/src/
```

---

### 1.9 Reserved-prefix validation — byte-eq + length cap

**Verdict: still correct. No drift since r5/r6.**

`validate_collection` (`query.rs:61-99`) executes in order: empty
→ null-byte → 63-byte cap → `pg_` prefix → `__zeroship` prefix →
final charset allowlist. The byte-slice `eq_ignore_ascii_case`
fastpath is guarded by `bytes.len() >= N` so slice indexing
cannot panic. ASCII-only fold means multibyte UTF-8 leading
bytes can't synthesize a `p` `g` `_` sequence.

`validate_field_name` (`query.rs:106-123`) likewise enforces the
63-byte cap.

**No finding.** Carryover from r5/r6.

---

### 1.10 Carryover status summary

| Finding | r5 sev | r6 sev | r7 sev | Status |
| --- | --- | --- | --- | --- |
| `audit.rs::validate_app_id` no 63-byte length cap (18 raw interp sites) | IMPORTANT | IMPORTANT | IMPORTANT | Unchanged. |
| `query::validate_schema` no 63-byte cap, leading-digit ok | MINOR | MINOR | MINOR | Unchanged. |
| `backend/postgres.rs` raw `app_id`/`spec.name` interp | IMPORTANT | IMPORTANT | IMPORTANT | Unchanged. |
| `replication::ensure_publication_and_slot` `quote_ident` fast-paths | IMPORTANT | IMPORTANT | IMPORTANT | Unchanged. Within-tenant only. |
| `init_session` P0001 substring promotion fragility | MINOR | MINOR | **CLOSED** | a272d1af + e5315083 replaced substring with DETAIL token. |
| `replication::slot_status` `pub` without v8 caller | MINOR | MINOR | MINOR | Unchanged (`replication.rs` still `pub`). |
| `diff::count_violating_not_null` `pub` + unvalidated | MINOR | MINOR | MINOR | Unchanged. |
| `auto_tx.rs::exec_auto_begin` per-query connect, no per-app cap | MINOR | MINOR | MINOR | Unchanged. |
| `init_session` `search_path` includes `public` | MINOR | MINOR | MINOR | Unchanged. |
| Migration row-UPDATE column-name truncation | INFO | INFO | INFO | Unchanged. Within-tenant only. |
| `[I43]` blocking `pg_advisory_lock` | INFO | INFO | INFO | Unchanged. Within-app DoS only. |
| WAL consumer cross-tenant visibility Rust-enforced (deferred P8c) | MINOR | MINOR | MINOR | Unchanged. |
| **r5 NEW CRITICAL cross-app scope (`watchdog` / `dropAbandoned`)** | NEW | CLOSED | CLOSED | Durable across all `#[v8_method]` sites (re-verified). |
| **Concurrency r7 NEW MINOR `startReplicationConsumer` race** | — | — | **CLOSED** | 70921112 + 386f9bf5 atomic `try_claim` + lifecycle tests. |
| **r6 latent `then_some` lost-race unmarks-winner bug** | — | — | **CLOSED** | 386f9bf5 lazy `then(\|\| ...)` + regression test. |
| Wildcard-arm prefix bypass on `#[non_exhaustive]` DbError future variants | — | INFO | INFO | Unchanged. Not a security finding (variant + `.code` still reach SDK). |

---

## 2. New / regression check from r6→r7 commits

| Commit | Touches | Security delta |
| --- | --- | --- |
| `70921112` | `context.rs:418-425`, `replication_ops.rs:274-305` | Adds `try_mark_consumer_running` (atomic check-and-set returning `bool`). Spawned task now uses `try_claim` so two concurrent dispatches that race past the outer `is_consumer_running` gate cannot both spawn; the loser bails without provisioning or marking. Closes a self-DoS where two rapid-succession dispatches both spawn and the second hits SQLSTATE 55006 in a retry loop. Within-tenant operational fix; no cross-tenant impact. **Net positive.** |
| `aa639715` | `wal_consumer.rs:233-368`, `replication_ops.rs:229-247`, removes `ConsumerError::NotProvisioned` | `WalConsumer::new` now returns `Result<_, DbError>` directly; dispatch boundary no longer re-stamps a generic `Configuration { code: "not_provisioned" }`. SDK now sees `.code = "invalid_app_id"` vs `.code = "not_provisioned"` as distinct error classes. Configuration message is a **static literal** — no `db_url` interpolation, no leak (verified 1.4). **Net positive (separates developer-error from operator-error in the SDK contract).** |
| `a272d1af` | `auth/bootstrap.rs:521-559` (CREATE FUNCTION body), `auth/session.rs:174-249` | All 5 RAISE EXCEPTION sites in `init_session` now set `DETAIL = '<stable_token>'` where token is a **static literal**. Rust side uses `classify_p0001_detail` to discriminate via `e.as_db_error().detail()` rather than `msg.contains("...")` substrings. Locale- and formatter-independent classification; SDK contract is more robust. DETAIL is NOT tenant-influenceable (analysed 1.1). **Net positive (closes r6 MINOR carryover on substring-fragility).** |
| `386f9bf5` | `orchestrator/lock_guard.rs` (structural test), `replication_ops.rs:332-425` (module-scope guard + 4 lifecycle tests) | Lifts `ConsumerRunningGuard` to module scope, adds 4 lifecycle tests (mark, drop, panic-drop, lost-claim). **Caught + fixed a latent bug** — `then_some(Self { app_id })` evaluated eagerly, dropping a temporary `Self` on the lost-race path which fired `Drop` and unmarked the winner's claim. Both racing tasks would have exited with no consumer running (self-DoS, 25-min exposure window pre-deploy). Replaced with lazy `then(|| ...)`. The 70921112 atomic semantics are now actually atomic. **Net positive.** |
| `e5315083` | `auth/session.rs:227-249` (dead `format!` removal), `context.rs:413-426` (gate `mark_consumer_running` to test/test-helpers), `replication_ops.rs` (doc + unused import cleanup) | Removes a dead `format!("{e}")` + source-chain walk in `init_session::map_err` (was discarded via `let _ = msg`). Production no longer pays the allocation cost; classifier reads `detail()` borrow-only. Gates `mark_consumer_running` behind `#[cfg(any(test, feature = "test-helpers"))]` so production builds don't expose the non-atomic footgun (verified 1.5 — zero production callers). **Net positive on attack surface (one production method removed) and on operator clarity.** |

No regressions introduced.

---

## 3. Summary table

| Finding | Severity | Status |
| --- | --- | --- |
| P0001 DETAIL classification — tenant-influenceable? | INFO | No finding (DETAIL is static literal in DDL; SQLSTATE constant gates first). |
| `try_mark_consumer_running` cross-app isolation | INFO | No finding (per-app key in HashSet). |
| `ConsumerRunningGuard` `then_some` latent bug | — | Closed by 386f9bf5 (lazy `then` + regression test). |
| `WalConsumer::new` typed Result — `db_url` leak? | INFO | No finding (static literal message; URL never interpolated). |
| `mark_consumer_running` gated to test/test-helpers | INFO | No finding (zero production callers). |
| SQL injection in r6→r7 delta | — | No new builder; DETAIL literals are constants. |
| App-id isolation post r6→r7 | — | Durable across all `#[v8_method]` sites. |
| `[I43]` blocking `pg_advisory_lock` | INFO/IMPORTANT | Carryover. Within-app DoS only. |
| Reserved-prefix validation | — | Carryover; structurally correct. |
| `audit.rs::validate_app_id` no length cap | MINOR | Carryover from r5/r6. |
| `query::validate_schema` no length cap | MINOR | Carryover from r5/r6. |
| `init_session` P0001 substring fragility | — | **Closed by a272d1af** (now DETAIL-token based). |
| Concurrency r7 NEW MINOR `startReplicationConsumer` race | — | **Closed by 70921112 + 386f9bf5.** |

---

## 4. Score

**82 / 100** (up from 78 in r6).

Delta from r6:

- **+1** for `aa639715` (typed `WalConsumer::new`) — splits
  developer-error (`invalid_app_id`) from operator-error
  (`not_provisioned`) at the SDK boundary. Improves observability;
  no new leak surface (message is a static literal — analysed 1.4).
- **+2** for `a272d1af` (P0001 DETAIL classification) — closes
  the r5/r6 carryover MINOR on substring-promotion fragility.
  DETAIL tokens are static literals in the SECURITY DEFINER
  CREATE FUNCTION body, **unspoofable** by tenant input. The Rust
  classifier gates on SQLSTATE constant first, then matches a
  closed set with `_ => None` fall-through. Locale- and
  formatter-independent.
- **+1** for `70921112` (atomic `try_claim`) — closes a
  within-tenant self-DoS (two rapid-succession dispatches both
  spawn → SQLSTATE 55006 retry loop). Per-app keying makes
  cross-tenant impact impossible.
- **+1** for `386f9bf5` — catches AND fixes the `then_some` lost-race
  latent bug AT TEST TIME via a lifecycle regression test, before
  any production deploy could see it. The regression-trip is
  pinned (`consumer_running_guard_try_claim_loses_when_already_marked`
  at `replication_ops.rs:413-424`).
- **-1** for the `e5315083` change being purely net-positive but
  not closing a security finding (api-surface improvement only).
- Net: +4 (from 78 to 82).

What would push to 88+:

1. Add the 63-byte length cap to `audit::validate_app_id` AND
   `query::validate_schema` — closes both r5/r6 carryover MINORs
   and the theoretical-but-real truncation collision.
2. Replace `error.rs`/`migrations.rs` `_ => {}` wildcard arms with
   explicit per-variant arms so a future `DbError` variant is a
   compile-time forcing function (carryover INFO from r6 1.2).
3. Demote `replication::slot_status` to `pub(crate)` (carryover
   MINOR).
4. Resolve `[I43]` (switch to `pg_try_advisory_lock` with
   `lock_not_available` rejection).

What would push past 92:

5. Replace the 4 convention-deviation injection sites in
   `audit.rs` and `replication::ensure_publication_and_slot` raw
   `{pub_name}` interp with `quote_ident` so every DDL builder
   uses one consistent quoting primitive.
6. Per-app semaphore in `crate::context` capping in-flight
   auto-tx connection acquires (within-app DoS bound).
7. Per-app Postgres role ownership of the WAL slot (P8c —
   proposal-tracked).
8. Add a `cfg(not(test))` `compile_error!` on any module-level
   `pub fn mark_consumer_running` rename to ensure the
   non-atomic variant cannot accidentally re-enter production
   through a contributor's test-helpers feature flag.

The r6→r7 delta is straightforwardly positive: one carryover MINOR
closed (`init_session` substring fragility), one within-tenant DoS
race closed and pinned (`startReplicationConsumer` atomic
try-claim), and one production footgun removed
(`mark_consumer_running` gated behind cfg). No new attack surface
introduced; the typed `WalConsumer::new` and DETAIL classification
are explicit security-improving refactors.
