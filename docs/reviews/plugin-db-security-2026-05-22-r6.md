# plugin-db Security Review — Round 6 (2026-05-22)

Target: `crates/plugin-db/` at `/home/ruiyang/Projects/appbase`,
HEAD includes r5 closure work + four post-r5 commits:

- `e399eeea` — `ConsumerRunningGuard` (Drop-guard for
  `unmark_consumer_running` on panic-unwind).
- `cbbc9059` — 5-site `coded_sql` dedup onto
  `crate::error::{prefix_message, coded_sql}`.
- `f1f06900` — integration-test signature fix for the r5
  cross-app scope plumbing.
- `f7d0961c` — `error.rs` module preamble updated to reflect the
  post-[I28] state.

Fresh re-audit; r5 findings were re-walked from scratch against
the current source.

---

## 1. Findings

### 1.1 Cross-app scope closures (r5 NEW CRITICAL) — verified intact

**Verdict: closed and durable across all sibling v8_methods.**

I enumerated every `#[v8_method]` site in `crates/plugin-db/src/` and
mapped each to its app-id source. The full list:

| File:line | Method | App-id source |
| --- | --- | --- |
| `v8_classes/replication.rs:54` | `setup(opts)` | `resolve_setup_app_id(&self.app_id, _)` → stamped |
| `v8_classes/replication.rs:76` | `watchdog(opts)` | `resolve_watchdog_app_id(&self.app_id, _)` → stamped (NEW, c0590506) |
| `v8_classes/replication.rs:97` | `dropAbandoned(opts)` | `resolve_drop_abandoned_app_id(&self.app_id, _)` → stamped (NEW, c0590506) |
| `v8_classes/db.rs:114` | `collection(name)` | `&self.app_id` → stamped |
| `v8_classes/db.rs:143` | `registerModel(...)` | `&self.app_id` → stamped |
| `v8_classes/db.rs:179` | `beginTransaction(opts)` | `self.app_id.clone()` → stamped |
| `v8_classes/db.rs:211` | `openSubscription(coll)` | `&self.app_id` → stamped |
| `v8_classes/db.rs:241` | `startReplicationConsumer(opts)` | `resolve_consumer_app_id(&self.app_id, ...)` → stamped (309ed52f) |
| `v8_classes/migrations.rs:73` | `start(spec)` | `get_app_id_pub(&state)` → isolate env var `APP_ID` (same source as `self.app_id`) |
| `v8_classes/migrations.rs:83` | `status(spec)` | `self.app_id.clone()` → stamped |
| `v8_classes/migrations.rs:94` | `cancel(spec)` | `self.app_id.clone()` → stamped |
| `v8_classes/migrations.rs:105` | `reset(spec)` | `self.app_id.clone()` → stamped |
| `v8_classes/collection.rs:68-331` | 14 CRUD methods | `&self.app_id` (set by `mint_collection` from parent Db's stamped id) |
| `v8_classes/transaction.rs:164` | `collection(name)` | `&self.app_id` (set by `mint_transaction`) |
| `v8_classes/subscription.rs:132` | `close()` | none needed (closes broker handle owned by the wrapper) |
| `v8_classes/migration.rs:244` | `commitBatch(...)` | `&owner.app_id` (set by `migration_start_with_spec` from `get_app_id_pub`) |

Every method that touches app-scoped state either:
1. Routes its app-id through a `resolve_*_app_id(&self.app_id, _opts)`
   helper that returns the stamped value verbatim and is pinned by unit
   tests against `opts.appId` override shapes (string / number / null /
   array / object / unicode), OR
2. Reads `self.app_id` directly with no parsing of `opts`.

The only deviation is `Migrations::start` and `migration_start_with_spec`,
which read `get_app_id_pub(&state)` (i.e. `state.env_vars["APP_ID"]`)
rather than `self.app_id`. Both sources are stamped at isolate creation
from the same APP_ID env var and never mutated — they're equivalent.
The `migrations.rs` v8_class's `status / cancel / reset` still go through
`self.app_id.clone()` so the divergence is purely in the `.start()`
path, which is unrelated to a cross-app bypass (the isolate env var
cannot be set from JS).

**No new finding.** The r5 NEW CRITICAL is closed across the entire
v8_class surface, and the regression-trip helpers (`resolve_*_app_id` +
their `*_ignores_*_override` unit tests in
`v8_classes/replication.rs:206-340` and `v8_classes/db.rs:411+`) make
the next CRITICAL of this shape impossible to reintroduce without
deleting tests.

---

### 1.2 Dedup'd `coded_sql` / `prefix_message` — variant-introduction risk

**Verdict: structurally correct today; one latent fragility worth a
debt note, not a security issue.**

`crate::error::prefix_message` (`error.rs:327-345`):

```rust
pub(crate) fn prefix_message(err: &mut DbError, prefix: &str) {
    match err {
        DbError::UniqueViolation { message }
        | DbError::FkViolation { message }
        | DbError::NotNullViolation { message }
        | DbError::CheckViolation { message }
        | DbError::Serialization { message }
        | DbError::LockContention { message }
        | DbError::Transient { message }
        | DbError::Internal { message } => {
            *message = format!("{prefix}{message}");
        }
        _ => {}
    }
}
```

`DbError` is `#[non_exhaustive]` (`error.rs:55`). The `_ => {}` wildcard
arm therefore covers BOTH:

- The four structured variants (`ValidationFailed`, `Configuration`,
  `Coded`, `SchemaRefused`) — deliberately bypassed because their
  wire body is a contract the SDK parses (pinned by
  `prefix_message_leaves_structured_variants_alone`, `error.rs:737-806`).
- ANY future variant added to `DbError` — silently bypassed.

The CALL paths are:
1. `coded_sql(context, e: compio_postgres::Error)` —
   `error.rs:357-361`. Calls `prefix_message` on `DbError::from_pg(&e)`.
   `from_pg` (`error.rs:151-192`) returns ONLY the 8 prefix-eligible
   variants today (UniqueViolation, FkViolation, NotNullViolation,
   CheckViolation, Serialization, LockContention, Transient, Internal).
   So today no `coded_sql` caller can hit a structured variant.
2. Direct `prefix_message(&mut err, "…")` in
   `replication.rs:202, 215` etc. — passes a `DbError::from_pg(&e)`.
   Same guarantee.

**Hypothetical**: if a future committer adds a new SQLSTATE
classification to `from_pg` that returns, say, a `Coded { code:
"deadlock_chain", ... }` variant, the new variant's message would
silently bypass the `"audit: …: "` / `"diff: …: "` / `"auth/bootstrap:
…: "` prefix. Operator-facing context is lost; the variant + code
still reach the SDK intact (since `to_op_error` covers every variant
explicitly via the exhaustive match at `error.rs:198-247`).

**Why this is not a security finding**: the prefix is a
human-readable phrase. The SDK contract is `err.code` (still correct),
and the message body (still correct, just unprefixed). No authz
bypass, no tenant leak, no panic.

**Verification**:
```bash
grep -n "match err\|match &mut err" /home/ruiyang/Projects/appbase/crates/plugin-db/src/error.rs
grep -n "DbError::from_pg" /home/ruiyang/Projects/appbase/crates/plugin-db/src/error.rs
```

The duplicated walker in `migrations.rs::coded_db` (`migrations.rs:82-102`)
has the same shape and same fragility. Dedup commit `cbbc9059`
collapsed the 5 `coded_sql` sites but not the `coded_db` one. Same
analysis: not a security risk, but a debt note.

```
[INFO] error.rs:327, migrations.rs:82 — `_ => {}` wildcard arm + `#[non_exhaustive]` DbError
  Why: a new variant added to `DbError` would silently bypass the operator-facing
       "<module>: <ctx>: " prefix. Variant + code still reach the SDK; only the
       message-body context is lost. No authz / leak / DoS impact.
  Fix: (optional) replace `_ => {}` with an explicit match arm for each structured
       variant (`ValidationFailed | Configuration | Coded | SchemaRefused`),
       so a new DbError variant is a compile-time forcing function — the
       reviewer is forced to choose prefix-or-not for the new variant.
  Verification: rg "_ => \\{\\}" crates/plugin-db/src/error.rs crates/plugin-db/src/migrations.rs
```

---

### 1.3 `ConsumerRunningGuard` Drop weaponisation (e399eeea)

**Verdict: safe under current call-graph; no panic-during-panic
weapon.**

Drop impl (`replication_ops.rs:267-273`):

```rust
impl Drop for ConsumerRunningGuard {
    fn drop(&mut self) {
        crate::context::with_mut(|c| {
            c.unmark_consumer_running(&self.app_id)
        });
    }
}
```

The concerns to rule out:

(a) **Panic during panic** via `RefCell::borrow_mut` double-borrow:
`context::with_mut` (`context.rs:458-460`) goes through
`ISOLATE_CTX.with(|c| f(&mut c.borrow_mut()))`. A double `borrow_mut`
panics. If the Drop fires during a panic-unwind that originated INSIDE
an active `context::with_mut(...)` closure, the second `borrow_mut`
aborts the process (double-panic). I walked every code path the
spawned task can take after entering the `_guard` scope:
- `run_supervised(consumer).await` is the only awaited body.
- `run_supervised` (`wal_consumer.rs:714-761`) does NOT touch
  `crate::context`; it just calls `consumer.run().await` in a retry
  loop.
- `consumer.run()` (`wal_consumer.rs:366-399`) calls
  `repl::connect_replication`, `IDENTIFY_SYSTEM`,
  `start_logical_replication`, then `SuppressGuard::activate` + the
  decode loop. None of those go through `crate::context`.
- `SuppressGuard` (`wal_consumer.rs:139-159`) uses its own
  `SUPPRESSED_APPS` thread-local; orthogonal to `ISOLATE_CTX`.
- The decode loop's broker `publish` (`broker.rs:604+`) uses a
  separate thread-local `BROKER`; orthogonal to `ISOLATE_CTX`.

So no awaited future inside the spawned task can be holding an
`ISOLATE_CTX.borrow_mut` when a panic unwinds through `_guard`. Drop's
own `with_mut` reborrows cleanly.

(b) **Drop body itself panicking**: `unmark_consumer_running`
(`context.rs:420-422`) is `HashSet::remove`. Returns `Option<_>`,
cannot panic for a properly-initialised HashSet (and `IsolateDbContext`
uses `HashSet::new()` at line 411 — never poisoned).

(c) **Drop running after the isolate's thread-local was destroyed**:
the consumer task is `compio::runtime::spawn(...).detach()`-ed onto
the worker's compio runtime, which runs on the same thread as the
isolate. The thread-local is initialised lazily on first access (the
`thread_local!` macro). If the thread is being torn down at drop
time, the `ISOLATE_CTX.with(...)` call would panic with "cannot access
a Thread Local Storage value during or after destruction". But the
`detach()`'d task cannot outlive its compio runtime; the runtime
shuts down before thread-locals are destroyed. So Drop reaches a live
TLS slot.

**No finding.** The Drop guard is sound. The commit message correctly
identifies the bug it closes (post-await unmark unreachable on panic).

---

### 1.4 SQL injection sweep — `format!`-built SQL

I enumerated every `format!(r#"..."#, ...)`-style SQL builder in the
crate. Three categories:

**Category A — interpolates a typed const (compile-time safe)**

`auth/bootstrap.rs:122, 195, 215, 228, 799, 809`, `auth/keys.rs:75, 105,
134`, `auth/session.rs:127, 172`. All interpolate `ADMIN_SCHEMA`,
`PLATFORM_ROLE`, `APP_ROLE_TEMPLATE` — three `const &str`s defined in
`auth/mod.rs:77-89`. No runtime input.

**Category B — interpolates `quote_ident(...)`-routed input**

`query.rs:156` (build_create_schema), `:195` (build_create_table_with_fks),
`:288` (build_add_foreign_key), `:1044, 1083, 1390, 1416, 1641, 1689`
(CRUD SELECT/UPDATE/DELETE builders), `migrations.rs:563, 571-574`
(per-row UPDATE in `apply_updates`). `quote_ident` (`query.rs:146-148`)
double-quotes and escapes any embedded `"`. Combined with the
upstream `validate_collection` + `validate_schema` + `validate_field_name`
calls (`query.rs:61-122`), no shell-character can survive both layers
to escape the quoted identifier.

The one row-UPDATE path in `migrations.rs:555-565` passes raw
JSON-object keys (`col` in `for (col, val) in set_obj`) through
`quote_ident` without calling `validate_field_name` first. The
SQL-injection vector is closed by `quote_ident` (a malicious `";`
would just produce `""";"` and Postgres would reject it as a missing
column). But a 64-byte-plus column name would be silently truncated by
Postgres, potentially aliasing two distinct logical fields to the same
column. This is WITHIN-tenant only (the surrounding `quote_ident(app_id)
+ quote_ident(collection)` ties the UPDATE to the calling tenant's own
table), so the impact is "tenant can confuse their own migration",
not cross-tenant. Carried over from r5; not new in r6.

**Category C — interpolates `replication.rs::publication_name(...)` /
`slot_name(...)`**

`replication.rs:191, 211, 287, 312, 318` etc. Both helpers funnel
through `sanitise_app_id` (`replication.rs:82-101`) which only accepts
`[A-Za-z0-9_]` and lowercases. No injection vector. The
`slot_name_like_prefix` helper (`replication.rs:113-127`, new in
c0590506) is the only function that produces a `LIKE`-suitable string,
and it's always passed via a `$1` bind, NEVER interpolated.

**Audit.rs interpolation surface** (r5 finding 1.x carried over):
18 raw `"{app_id}"` interpolations in `audit.rs:188, 202, 235, 247,
254, 269, 287, 332, 482, 531, 562, 616, 643, 663, 686, 716, 750, 780`.
Every site is gated by `validate_app_id(app_id)?` at function entry
(verified by reading `audit::ensure_audit_table_exists:182` and the
other public-facing audit fns). `validate_app_id` enforces
`[A-Za-z0-9_-]` but has NO length cap. Same r5 finding; status
unchanged. Within-tenant only (tenant supplies their own schema name),
so the truncation risk is a within-tenant collision possibility
(operational), not cross-tenant injection.

```
[MINOR] crates/plugin-db/src/audit.rs:805-822 — `validate_app_id` has no length cap
  Why: a 64-byte-plus app_id passes the char-set gate but Postgres silently truncates
       schema/table names to 63 bytes (NAMEDATALEN). Two app_ids sharing a 63-byte
       prefix would collide on the same `__zeroship_migrations` table after truncation.
       Today typed_id-derived app_ids are far shorter than 63 bytes, so the risk is
       theoretical — but defence-in-depth would add the cap.
  Fix: add `if name.len() > 63 { return Err(DbError::validation("invalid_app_id", …)); }`
       after the emptiness check; mirror the cap already present in
       `query.rs::validate_collection:72-76`.
  Verification: rg "fn validate_app_id" crates/plugin-db/src/audit.rs -A 18
```

(Carried over from r5; unchanged in r6.)

**No new SQL injection finding in r6.**

---

### 1.5 App-id isolation re-verification

Re-checked the following invariants:

- **Per-isolate APP_ID env var**: each V8 isolate is built by the
  worker with its app's `APP_ID` in `state.env_vars`. `get_app_id_pub`
  (`v8_bridge.rs:74-86`) reads it; `mint_db` stamps `self.app_id`
  from the same source. The two are constructor-time equivalent and
  never mutated.
- **Per-isolate `IsolateDbContext`**: `ISOLATE_CTX` (`context.rs:439-444`)
  is a `thread_local!`. Each worker thread serves one isolate at a
  time (V8 enter/exit between apps); when the worker swaps isolates,
  the thread-local context is RESET (the worker calls
  `context::with_mut(|c| *c = IsolateDbContext::new())` between app
  swaps — verified by grepping `lib.rs` for `*c = IsolateDbContext`).
  So cross-tenant state leakage via the thread-local is impossible.
- **`running_consumers` set keyed by raw app_id**: tracked as
  `HashSet<String>`. The mark + unmark sites both use the resolved
  (stamped) `app_id` from the v8_class. Within-app re-entry hits the
  set correctly. Cross-app collision impossible because the value
  IS the app_id.
- **`SUPPRESSED_APPS` set keyed by raw app_id**: same pattern;
  `SuppressGuard::activate(&self.app_id)` uses the consumer's own
  app_id. Cross-app collision impossible.

No drift since r5.

---

### 1.6 Reserved-prefix length validation (byte-eq + cap order)

**Verdict: correct ordering; sound under multibyte/ASCII edge cases.
Same conclusion as r5's 1.9.**

`validate_collection` (`query.rs:61-99`) executes in order:

1. `name.is_empty()` → reject (line 62).
2. `name.contains('\0')` → reject (line 67).
3. `name.len() > 63` → reject (line 72).
4. `bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"pg_")` →
   reject (line 80).
5. `bytes.len() >= 10 && bytes[..10].eq_ignore_ascii_case(b"__zeroship")`
   → reject (line 85).
6. `name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')` →
   reject (line 90).

The byte-eq fastpath uses `bytes[..N]` with a guarding `bytes.len() >= N`
check. The slice indexing cannot panic. Length cap at step 3 is
defensive — even a malicious 1 GB string is rejected before any further
processing.

`eq_ignore_ascii_case` on a byte slice only folds 7-bit ASCII; multi-byte
UTF-8 leading bytes (`[0xC2..=0xF4]`) never match the ASCII pattern.
The final allowlist at step 6 then bounds the input to
`[A-Za-z0-9_]` anyway.

**No finding.** Carried over from r5.

---

### 1.7 [I43] Advisory-lock DoS — status check

**Verdict: still open. Unchanged since r4/r5.**

`OrchestratorLockGuard::acquire` (`lock_guard.rs:117-130`) calls
`backend.acquire_advisory_lock(...)` which uses the blocking
`pg_advisory_lock(hashtext($1), hashtext($2))` (`backend/postgres.rs:118`).

The lock key is `zs_reg:<app_id>` namespaced per-app, so:
- **Cross-tenant impact**: NONE. Tenant A's blocking
  `pg_advisory_lock("zs_reg:app_a", "register_model")` does not block
  tenant B's `pg_advisory_lock("zs_reg:app_b", "register_model")`.
- **Within-tenant impact**: a runaway `registerModel` loop from one
  tenant can monopolise their OWN advisory lock holder. The
  serialisation per app is in-fact the intended behaviour (proposal
  A2 line 202). The DoS surface is: any pool client tied up waiting
  for the lock cannot serve other queries for that tenant. With pool
  size N and a single hot tenant, N-1 clients remain available — not
  a complete-pool-exhaustion vector.

`[I43]` tracks the cost-saving improvement (switch to
`pg_try_advisory_lock` with explicit `lock_not_available` rejection so
the SDK retries instead of the pool worker blocking). Not a security
delta.

```
[INFO] crates/plugin-db/src/orchestrator/lock_guard.rs:123 — blocking pg_advisory_lock (carryover [I43])
  Why: a within-app DoS surface (one tenant ties up one pool slot per concurrent
       registerModel). Cross-tenant impact is null due to per-app lock-key namespacing.
  Fix: tracked in [I43] backlog; switch to pg_try_advisory_lock and surface the
       contention as `lock_not_available` so the SDK retries instead of the pool
       worker blocking.
  Verification: rg "acquire_advisory_lock|pg_advisory_lock" crates/plugin-db/src/
```

---

### 1.8 WAL slot DoS within-app — re-audit

**Verdict: bounded by idempotency; no new attack surface.**

`ensure_publication_and_slot` (`replication.rs:167-...`) is idempotent
per-app:
- Slot name is `__zs_slot_<sanitised_app_id>` — deterministic per
  app; repeated `setup()` re-uses the same slot.
- Publication name is `__zs_pub_<sanitised_app_id>` — deterministic.

A tenant cannot create N slots from one isolate. The
`startReplicationConsumer` dispatch additionally short-circuits via
`is_consumer_running(&app_id)` so a second call is a no-op.

The cross-tenant scoping fix in c0590506 (`watchdog_query` +
`drop_abandoned_slots` now bind `slot_name LIKE $1` against
`slot_name_like_prefix(app_id)`) means tenant A cannot enumerate or
drop tenant B's slots. The within-app `dropAbandoned` can still drop
tenant A's OWN slots; that's the intended behaviour.

**No finding.** Carried over from r5 (1.7 was "no finding" there too).

---

### 1.9 Unrelated carry-over status

The following r5 findings remain open and unchanged:

| Finding | r5 severity | r6 status |
| --- | --- | --- |
| `audit.rs::validate_app_id` no length cap (18 raw interp sites) | IMPORTANT | Unchanged. See 1.4. |
| `backend/postgres.rs` raw `app_id` / `spec.name` interpolation | IMPORTANT | Unchanged. |
| `replication.rs::ensure_publication_and_slot` builders with `quote_ident`-shaped fast paths | IMPORTANT | Unchanged. (Within-tenant only.) |
| `init_session` P0001 RAISE substring promotion fragility | MINOR | Unchanged. |
| `replication::slot_status` is `pub` without v8 caller | MINOR | Unchanged. (`replication.rs:588` still `pub`.) |
| `validate_schema` / `audit::validate_app_id` accept leading digits + no 63-byte cap | MINOR | Unchanged. |
| `diff::count_violating_not_null` `pub` + unvalidated | MINOR | Unchanged. |
| `auto_tx.rs::exec_auto_begin` per-query connect, no per-app cap | MINOR | Unchanged. |
| `init_session` `search_path` includes `public` | MINOR | Unchanged. |
| WAL consumer cross-tenant visibility Rust-enforced (deferred to P8c) | MINOR | Unchanged. |

The four r5→r6 commits don't introduce new surface in any of these
areas, and don't close any of them.

---

## 2. New / regression check from r5→r6 commits

| Commit | Touches | Security delta |
| --- | --- | --- |
| `e399eeea` | `replication_ops.rs:264-284` | Adds `ConsumerRunningGuard` Drop-impl so `unmark_consumer_running` fires on panic-unwind. Closes the "stuck-running" surface where a panic in `run_supervised` would mark the app permanently running on this thread (idempotent shortcircuit, no consumer task — denial of replication for THE SAME tenant on THE SAME thread until restart). Within-tenant operational fix; not a cross-tenant fix. Drop body uses `context::with_mut(|c| c.unmark_consumer_running(...))` which is safe under current call-graph (see 1.3). **Net positive**, no new surface. |
| `cbbc9059` | 5 files: `audit.rs`, `auth/{bootstrap,keys,session}.rs`, `diff.rs` | Collapses 5 duplicated `coded_sql` walkers onto `crate::error::{prefix_message, coded_sql}`. The wildcard-arm bypass on the `#[non_exhaustive]` enum is now centralised (still present, but in ONE place — easier to audit). Behaviour is preserved verbatim; 3 contract tests pinned in `error::tests` (`prefix_message_preserves_variant_and_code` covers 8 prefix-eligible variants × wire code; `prefix_message_leaves_structured_variants_alone` covers 4 structured variants). **Net positive on maintainability**, no new surface. See 1.2 for the latent fragility note. |
| `f1f06900` | `tests/integration.rs:2906, 2947, 2956` | Threads `app_id` through the three integration-test callers of `watchdog_query` / `drop_abandoned_slots` that the c0590506 cross-app fix had broken. Compile-time fix; no runtime surface. **Net positive** (lets the integration tests actually run again, restoring cross-tenant scoping E2E coverage). |
| `f7d0961c` | `error.rs:1-23` (preamble only) | Documents that the post-[I28] hold-outs are now just (a) the SchemaRefused envelope and (b) two ASCII hex helpers in `auth/session.rs`. No code surface change. **Net positive on operator clarity**, no new surface. |

No regressions introduced. The `ConsumerRunningGuard` is the only
runtime-surface change and its analysis (1.3) shows it's safe under
current call-graph.

---

## 3. Summary table

| Finding | Severity | Status |
| --- | --- | --- |
| Wildcard-arm prefix bypass on `#[non_exhaustive]` DbError future variants | INFO | New observation; not a security finding (variant + `.code` still reach SDK; only operator-facing context phrase is lost). |
| `audit.rs::validate_app_id` no length cap | MINOR | Carryover (r5). |
| Migration-row UPDATE column-name length truncation | INFO | Carryover (r5 implicit). Within-tenant only. |
| `[I43]` blocking `pg_advisory_lock` | INFO/IMPORTANT | Carryover (r3-r5). Within-app DoS surface. |
| **r5 NEW CRITICAL cross-app scope** | — | Closed by c0590506 + e399eeea; verified durable across all `#[v8_method]` sites in 1.1. |
| **r5→r6 four commits introduce no regressions** | — | Verified in section 2. |

---

## 4. Score

**78 / 100** (up from 74 in r5).

Justification:

- **+10** for closing the r5 NEW CRITICAL (`watchdog` + `dropAbandoned`
  cross-app exposure) via c0590506. Both methods now bind
  `slot_name LIKE $1` against `slot_name_like_prefix(app_id)` and the
  v8_class threads `self.app_id` through `resolve_watchdog_app_id` /
  `resolve_drop_abandoned_app_id`. 8 new unit tests pin the
  override-rejection invariant; 2 unit tests pin the `LIKE`-prefix
  value. The whole r5 CRITICAL is closed.
- **+1** for the Drop-guard (`e399eeea`) — the consumer registry now
  recovers from a panic-unwind in `run_supervised`. Within-tenant
  operational improvement.
- **+1** for the `coded_sql` dedup (`cbbc9059`) — one shared
  variant-walker is easier to audit than 5 duplicates. The
  contract tests now live in `error::tests` with full 8-variant
  coverage (vs. 4-variant in the prior replication-local test).
- **-1** for the latent wildcard-arm fragility (1.2) — pre-existing,
  now visible because the dedup makes it a single-site concern. Not
  a security gap today but a tripwire for future contributors.
- **-7** retained from r5 baseline for the IMPORTANT / MINOR carryovers
  unchanged in r6 (`audit.rs` no-length-cap, `[I43]` blocking lock,
  `init_session` substring promotion, etc.).

The r5→r6 delta is straightforwardly positive: the highest-severity
open finding (NEW CRITICAL) is closed and re-verified across the
entire v8_class surface, the panic-recovery surface is bounded, and
the error-classification layer is consolidated. No new surface
introduced. The score moves up by 4 points from r5's 74.

What would push to 85+:

1. Add the 63-byte length cap to `audit::validate_app_id` and
   `query::validate_schema` — closes the carryover MINOR + the
   theoretical-but-real truncation collision.
2. Replace `error.rs` and `migrations.rs` `_ => {}` wildcard arms with
   explicit per-variant arms so a future DbError variant is a
   compile-time forcing function for the next reviewer.
3. Demote `replication::slot_status` to `pub(crate)` (carryover MINOR).
4. Resolve `[I43]` (switch to `pg_try_advisory_lock`).
5. Convert `init_session` substring-promotion to SQLSTATE + DETAIL
   discriminator (carryover MINOR).

What would push past 90:

6. Replace the four convention-deviation injection sites in
   `audit.rs` (and the `replication::ensure_publication_and_slot` raw
   `{pub_name}` interp) with `quote_ident` so every DDL builder uses
   one consistent quoting primitive.
7. Per-app semaphore in `crate::context` capping in-flight auto-tx
   connection acquires.
8. Per-app Postgres role ownership of the WAL slot (P8c —
   proposal-tracked).
