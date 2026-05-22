# `crates/plugin-db` — Architecture Review, Round 7

HEAD: `51ced4a0`. Prior rounds: R1 (64) → R2 (76) → R3 (81) → R4 (82) → R5 (83) → R6 (85).

This is a fresh re-audit. The R6 → R7 window landed ten commits, most of which closed R6 IMPORTANTs head-on. The architectural posture this round is the cleanest it has been across all seven rounds: every CRITICAL/IMPORTANT carried in by R6 has either fully closed or visibly shrunk, no new structural-class finding emerged, and the patterns that *did* land (Drop-guard panic safety, scoped tenancy filters, helper de-dup) all reinforce the abstractions established in R3–R6.

The ceiling for further movement is now the small set of judgment-call items: cfg-fork test surface (architectural inversion, low realised cost), `Backend` trait half-application (concrete `&PostgresBackend` in 9 sites), and the auth/* dead-code surface (P8c lever, currently unwired from JS).

---

## 1. Score Per Dimension (R6 → R7)

| Dimension | R6 | R7 | Δ | Driver |
|---|---:|---:|---:|---|
| Module boundaries | 63 | **64** | +1 | `lock_guard.rs` hardening (`#[must_use]`, unlock-SQL warn) makes the orchestrator-lock invariant compile-time visible; consumer test (`tests/integration.rs`) is the only external touch point. No new cfg-fork pair. |
| Layering (orchestrator pipeline) | 88 | **89** | +1 | `run_pipeline` unchanged in shape; the guard hardening (deferred `released = true` flip, unlock-SQL `tracing::warn`) closed the cancellation-leak window R6 implicitly assumed away. Pipeline reads identically; the safety net behind it is tighter. |
| Extension points (new `ChangeKind`, error variant, aggregator) | 63 | **63** | 0 | `ChangeKind` still 7 variants exhaustively matched in `apply.rs::run_op` (match-exhaustiveness pin lives in `check_destructive_invariant` + the unreachable `DropColumn/DropIndex` arm at `apply.rs:155-158`). `DbError` still `#[non_exhaustive]`. No movement; the substrate is sound. |
| Coupling (replication / broker / wal_consumer) | 81 | **82** | +1 | `ConsumerRunningGuard` Drop guard in `replication_ops.rs:264-273` closes the panic-leaks-the-running-marker class. The producer-side subscriber gate (R6 I4 sub-pattern, `wal_consumer.rs:554` + `exec.rs:201`) is unchanged — still 2 sites, still borderline-leave-alone. |
| Forward extensibility | 72 | **73** | +1 | `OrchestratorLockGuard` is now battle-tested across the must-use, deferred-flip, and warn-on-error hardening cycles. The template for the predicted-R7 `MigrationLockGuard` extract is mature; the migration lock at `migrations.rs:259-300, 643-647` is the natural next consumer. |
| Coupling debt (cfg-fork visibility + duplicated patterns) | 56 | **62** | +6 | Empty-RETURNING cluster (was 3 sites + 4th near-sibling at R6) fully closed via `first_row_or_internal` helper (`eda96ead`). `coded_sql` cluster (was 5 modules) collapsed to one variant-walker (`cbbc9059`). Subscriber gate (2 sites) and app-id stamp (2 sites) unchanged. Cfg-fork still 8 pairs. |
| Error rail discipline | 84 | **93** | +9 | The big move. R6's I2 (29 functions on `Result<_, String>`) closed via `0049d9be` — `replication.rs` + `auth/*` swept to `Result<_, DbError>`. Production `Result<_, String>` sites collapse to 5 (validate envelope, hex parsing, two parse_spec helpers, init_pool_async); each is a deliberate exception with a documented justification. `91830cca` removed the stale `.into_string()` boundary in replication. |
| Security | 91 | **93** | +2 | `c0590506` scoped `watchdog` and `dropAbandoned` to `self.app_id` (CRITICAL cross-tenant info-disclosure + DoS, sibling of the `309ed52f` setup-hijack). The fix mirrors the `resolve_setup_app_id` pattern with two new `resolve_watchdog_app_id` / `resolve_drop_abandoned_app_id` helpers, both unit-tested for override-shape resistance (`v8_classes/replication.rs:278-340`). Per-app `LIKE $1` filter in `replication.rs:392, 526` cited as the SQL anchor. |
| Performance posture | 75 | **75** | 0 | No hot-path movement. R5 broker two-level + per-row gate carry. |
| API surface | 70 | **70** | 0 | No new demotions or promotions. R6's 5 `mint_*` demotions hold. `auth/*` module still cfg-pub'd with no production consumer (carried below as M3/judgment-call). |
| Pattern consolidation | 62 | **78** | +16 | Two clusters fully closed this round: empty-RETURNING (4 sites + 2 string-rail twins → all use `first_row_or_internal`), and `coded_sql` (5 per-module wrappers → 1 `crate::error::coded_sql` + thin module-prefix shims). Subscriber gate (2 sites) and app-id stamp (3 sites — Db, Replication setup, Replication watchdog+drop) are the remaining clusters; both judgment-call leave-alones. |

**Aggregate: 85 → 89.**

R7 movement is +4 aggregate. The error-rail sweep (+9) and pattern consolidation (+16) carry the bulk; the security fix (+2) is high-severity-by-cost but visible. The crate's architectural posture is now **stable and near-asymptotic** — three of the four R6 IMPORTANTs closed, the fourth (cfg-fork test surface) remains a judgment-call.

### What moved the score

- **Error rail discipline (+9)** — `0049d9be` is the single largest mechanical refactor this cycle: ~24 `pub async fn` signatures across `auth/bootstrap.rs` (13), `auth/keys.rs` (3), `auth/session.rs` (1+wrappers), `replication.rs` (7) flipped from `Result<_, String>` to `Result<_, DbError>`. The string-rail flattening in `replication.rs:248` (R6's smoking gun) is gone. The SDK's `.code` discriminator now flows end-to-end from `compio-postgres::Error` → `DbError::from_pg` → `to_op_error()` → JS `err.code` for every production helper.
- **Pattern consolidation (+16)** — two clusters closed in two commits. `eda96ead` extracted `first_row_or_internal<R>(rows, op_label)` at `error.rs:378-385`. Audit.rs (2 sites), replication.rs (1 site), and the helper's own test (`first_row_or_internal_returns_internal_err_on_empty` in `error.rs:651-669`) collapse into one predicate; the empty-RETURNING contract is named once instead of duplicated across files. `cbbc9059` collapsed the per-module `coded_sql(context, e)` helpers into one shared `crate::error::coded_sql` with thin per-module prefix shims (`audit/bootstrap/keys/session/diff` × `coded_sql` → `crate::error::coded_sql(&format!("audit: {context}"), e)`).
- **Security (+2)** — `c0590506` closed a CRITICAL cross-tenant escape that survived the `309ed52f` setup-hijack fix from prior rounds. Pre-fix, `db.replication.watchdog()` issued a cluster-wide `pg_replication_slots` enumeration (info disclosure: App A learns every co-tenant's slot names) and `db.replication.dropAbandoned()` issued a cluster-wide DROP sweep (DoS: App A reaps co-tenant inactive slots, forcing co-tenant subscribers into resync). The fix adds `slot_name LIKE $1` filters bound to `slot_name(app_id) + '%'` in both query paths (`replication.rs:392, 526`) plus two new `resolve_*_app_id` helpers with 16 new regression tests for override-shape resistance.
- **Coupling (+1)** — `e399eeea` wrapped the consumer-running marker in a Drop guard (`ConsumerRunningGuard` in `replication_ops.rs:264-273`). Pre-fix, if `run_supervised` panicked before its terminal `unmark_consumer_running`, the app stayed "running" forever and `startReplicationConsumer` could never re-arm. Now the Drop fires on graceful exit OR panic-unwind.

### Trajectory narrative

The arc:

- **R1 → R3 (64 → 81)** was the foundation round: pipeline split into stages, typed-id discipline, broker layout.
- **R3 → R5 (81 → 83)** was the perf + classification round: broker two-level, per-row gate, typed error rail design.
- **R5 → R6 (83 → 85)** was the structure round: `OrchestratorLockGuard` RAII, mint_subscription reorder, mint_* demotions, audit-progress before COMMIT.
- **R6 → R7 (85 → 89)** is the **mechanical closure round**: the typed-error sweep (predicted as "one focused PR" at R6) landed; the empty-RETURNING helper (R7's predicted highest-value follow-up) landed; the cross-tenant scoping CRITICAL (which surfaced in security-r5, not previously in arch reviews) closed.

R6 projected R10 ≈ 90 if I1 (cfg-fork test_support), I2 (error-rail sweep), and I4 (empty-RETURNING helper) all landed. Two of those three landed in R7. R7 already reaches 89 — the projection was conservative.

The remaining +1-2 points to 90+ come from items either small (validate-envelope newtype, M1 from R6) or judgment-deferred (cfg-fork convention, `Backend` trait half-application, `auth/*` dead-code lifting). None are blocking.

---

## 2. Closed Since R6

| Finding | Source | How closed | Evidence |
|---|---|---|---|
| `Result<_, String>` rail across `replication.rs` + `auth/*` (~24 functions) | R6 I2 (carried R3 → R6) | Mechanical sweep: every `.map_err(\|e\| format!(...))` → `?` against `From<compio_postgres::Error> for DbError`. `replication.rs:248` `.into_string()` boundary removed. | `0049d9be`, `91830cca`; `replication.rs:82, 109, 167, 375, 486, 588`; `auth/bootstrap.rs:66` + 12 internal sites; `auth/keys.rs:66, 102, 129`; `auth/session.rs:83, 167, 232, 245` |
| Empty-RETURNING cluster (3 typed + 1 Coded near-sibling + 2 string-rail twins = 6 sites) | R6 I4 (grew R4 → R6) | Helper `first_row_or_internal<R>(rows, op_label)` extracted at `error.rs:378-385`; consumers at `audit.rs:314, 600` and `replication.rs:282`. Unit test `first_row_or_internal_returns_internal_err_on_empty` (`error.rs:651-669`) pins the contract. | `eda96ead`; `error.rs:378-385`; `audit.rs:314, 600`; `replication.rs:282`; `replication.rs:846-862` (round-trip test for the wire shape) |
| `coded_sql` helper duplicated across 5 modules | R6 I5 / dedup cluster | One shared `crate::error::coded_sql(context, e)` at `error.rs:357-361` walks the `DbError::from_pg` + `prefix_message` pipeline. Per-module shims (`audit::coded_sql`, `auth/bootstrap::coded_sql`, `auth/keys::coded_sql`, `auth/session::coded_sql`, `diff::coded_sql`) collapse to one-liner wrappers that compose the module-scoped prefix. | `cbbc9059`; `error.rs:357-361`; `audit.rs:58-60`, `auth/bootstrap.rs:24-26`, `auth/keys.rs:41-43`, `auth/session.rs:35-37`, `diff.rs:40-42` |
| `replication.watchdog()` + `dropAbandoned()` ran cluster-wide queries — cross-tenant info-disclosure + DoS (CRITICAL, sibling of `309ed52f` setup-hijack) | new R7 finding from security-r5 / cross-tenant escape | Per-app `slot_name LIKE $1` filter bound to `slot_name(app_id) + '%'` in `watchdog_query` (`replication.rs:392`) and `drop_abandoned_slots` (`replication.rs:526`). New `resolve_watchdog_app_id` + `resolve_drop_abandoned_app_id` helpers (`v8_classes/replication.rs:142-161`) ignore JS-supplied `appId` overrides. 16 new unit tests for override-shape resistance. | `c0590506`; `v8_classes/replication.rs:77-111, 142-161, 278-340`; `replication.rs:392, 526`; `replication_ops.rs:96, 137` (dispatch boundary) |
| `run_supervised` panic before terminal `unmark_consumer_running` stranded the running marker forever | new R7 correctness fix from code-critique r5 MAJOR-R5-2 | `ConsumerRunningGuard` struct + `Drop` impl in `replication_ops.rs:264-273` clears the marker on graceful exit OR panic-unwind. `mark_consumer_running` moved BEFORE the spawn so a racing concurrent call short-circuits. | `e399eeea`; `replication_ops.rs:259-285` |
| `OrchestratorLockGuard.released = true` flipped BEFORE `pg_advisory_unlock` await — cancellation could silently leak the lock | new R7 hardening from concurrency-r5 M-NEW-r5-1 | `release().await` now: (1) borrow the client `&`, (2) issue `pg_advisory_unlock`, (3) flip `released` to true, (4) `take()` the client out. Cancellation between (1) and (3) leaves `released = false` and the client present, so Drop fires its catastrophic-path log. | `bd1e7ce1`; `lock_guard.rs:124-163` |
| `pg_advisory_unlock` errors silently swallowed — operator never learns the lock might still be held | new R7 finding from code-critique r5 MAJOR-R5-5 | `release()` wraps the unlock SQL `Result` in `if let Err(e)` and emits `tracing::warn!` naming the `(key, tag)`. The session-scoped lock will still auto-release on PG session end; the log just makes the leak visible. | `ffb1e101`; `lock_guard.rs:148-159` |
| `OrchestratorLockGuard` accidentally dropped — e.g. `let _ = acquire(...).await` — silently leaked the lock with no compile-time warning | new R7 hardening | `#[must_use]` annotation with explanatory message on the struct (`lock_guard.rs:66-67`); Drop log message expanded to name the diagnosis path (async cancellation / panic / forgotten `release()`/ `into_held()`). | `808a32af`; `lock_guard.rs:66-67, 207-217` |
| `finalise_backfill` errors during the terminal-status transition silently absorbed | new R7 finding from migration-pipeline r5 R5-M7 / F1 family | `migrations.rs:644-657` now emits `tracing::warn!` on error with `(app_id, audit_id, terminal)` context. Lock release continues regardless because the row state is "as good as it can be" at this point. | `51ced4a0`; `migrations.rs:644-657` |
| `error.rs` preamble docstring drifted out of sync after the `[I28]` sweep | new R7 docs lie | Preamble at `error.rs:9-23` rewritten to reflect the post-sweep state: explicit list of the 5 surviving `Result<_, String>` exceptions (validate envelope, 2 ASCII hex helpers in `auth/session.rs`); each has a documented justification. | `f7d0961c`; `error.rs:9-23` |

**Ten closures.** Five tactical, three structural (error-rail sweep, empty-RETURNING helper, coded_sql dedup), two security (cross-tenant scoping + Drop-guard for consumer marker). The closure ratio this round is exceptional — every R6 IMPORTANT that had a single-commit shape landed; the only carried-IMPORTANT is the cfg-fork test surface, which is a convention-vs-correctness judgment call.

---

## 3. New + Carried Findings (R7)

### CRITICAL

None.

---

### IMPORTANT

**I1 (carried from R6 I1, since R4). `cfg`-forked module visibility is still eight pairs.**

`lib.rs:62-101` defines eight modules twice — `pub(crate)` in normal builds, `pub` under `test-helpers`: `audit`, `auth`, `exec`, `migrations`, `orchestrator`, `replication`, `replication_ops`, `wal_consumer`. R6 noted "eight is enough that the next contributor copies the shape without thinking." R7 status: unchanged at eight; the shape hasn't propagated beyond plugin-db (no other crate has copied the convention), and no downstream crate has enabled `test-helpers` yet. The leak is still hypothetical.

  Why: architectural impact

  The lib.rs preamble (`lib.rs:34-46`) names the inversion explicitly: "Most modules are `pub(crate)` in normal builds. Several are also consumed by external test crates under `tests/`, which are compiled as separate crate targets." That's an honest documentation of a design choice. But "documented architectural inversion" remains worse than either alternative R6 named: a `test_support` re-export module, OR a renamed `__internal_test_surface` feature flag.

  R7-specific observation: this is the only R6 IMPORTANT that didn't close. Every other one had a single-commit fix (sweep, helper, scope-filter). The cfg-fork doesn't — it requires either a `test_support` module + a coordinated path migration in `tests/integration.rs` (~30 import-site changes) or a feature rename (which is cheaper but doesn't fix the underlying inversion). Both are mechanical but neither is a 30-LOC commit.

  Fix: from R4-R6 verbatim. The judgment call narrows: at four review rounds carrying this finding, the right move is **leave the convention as-is** and document it once more clearly in `Cargo.toml`'s `[features]` block. Rename `test-helpers` → `__internal_test_surface` if you want to make the contract more visible at the consumer side; otherwise accept that this is the crate's published convention for "exposes internal modules for in-tree integration tests."

  Verification: `lib.rs:62-101` (eight pairs unchanged).

  ---

**I2 (carried from R6 I3, since R3). `Backend` trait still half-applied: `migrations.rs` (6 sites) + `register_model/mod.rs::run_pipeline` (1 site) + `register_model/bootstrap.rs::bootstrap` (1 site) take `&PostgresBackend` or `&'p PostgresBackend`.**

Status: **unchanged**. Concrete-typed signatures:

- `migrations.rs:215` (`exec_begin`)
- `migrations.rs:364` (`exec_fetch_batch`)
- `migrations.rs:449` (`exec_commit_batch`)
- `migrations.rs:476` (`rollback_and_return` private helper)
- `migrations.rs:674` (`exec_status`)
- `migrations.rs:715` (`exec_cancel`)
- `migrations.rs:748` (`exec_reset`)
- `register_model/bootstrap.rs:150` (private `build_ctx`)
- `register_model/mod.rs:160` (`run_pipeline`)

`register_model/{plan,validate,apply}.rs` all consume `B: Backend` generically; `lock_guard.rs:97` does too. The trait is generic-capable at the abstraction; the consumer pins it concrete at the entry.

  Why: architectural impact

  Unchanged from R5/R6. The trait scope statement (`backend/mod.rs:6-14`) says "everything the orchestrator asks of the database" — but `migrations.rs` is a peer subsystem of the orchestrator, not the orchestrator itself. The conventional read: `Backend` covers the **DDL** orchestrator (`register_model/*`), not the **data-backfill** orchestrator (`migrations.rs`). Today's `migrations.rs` calls 14 `Backend` methods (`ensure_app_schema`, `ensure_audit_table`, `acquire_dedicated_client`, `try_acquire_advisory_lock`, `release_advisory_lock`, `find_latest_backfill_row`, `set_backfill_running`, `insert_backfill_running`, `peek_latest_backfill_status`, `heartbeat_backfill`, `lock_audit_row_for_update`, `update_backfill_progress`, `finalise_backfill`, `cancel_backfill_row_pool`, `reset_backfill_row_*`, `next_schema_version`) — so the trait *does* cover what migrations needs. The concrete-typing is convention.

  Fix: as in R5/R6 — type the 7 `migrations.rs` fns + `run_pipeline` + `bootstrap` over `B: Backend`. The compile-time pin in `backend/mod.rs:391-433` is the safety net.

  Alternative path (judgment call, R7): commit to "migrations.rs is PG-only" and document. Rationale: `pg_try_advisory_lock` + `audit_generation` bumping + `SELECT … FOR UPDATE` are PG-specific primitives. The trait surface for those would either be PG-shaped abstractions (basically the current `Backend` methods) or a leaky "do these PG things" subset. **Pick one position and stick to it.** Either:
  - Tighten: `Backend` is the universal data-store boundary; rewrite the 9 callers to `B: Backend`. OR
  - Narrow: `Backend` is "what the DDL orchestrator needs"; the migration backfill uses PG-only primitives and documents that in `migrations.rs:1`.

  R7 leans toward "narrow" — `migrations.rs` is fundamentally about advisory locks and row-level FOR UPDATE, which are PG abstractions. The trait should shrink to what `register_model/*` actually consumes (the 16 methods listed above is the trait; if migrations specialises, those 14 migration-specific methods leave the trait and live on `PostgresBackend` impl directly).

  Verification: `migrations.rs:215, 364, 449, 476, 674, 715, 748`; `register_model/bootstrap.rs:150`; `register_model/mod.rs:160`.

  ---

**I3 (carried from R6 I5, NOT closed). `auto_tx::exec_auto_begin` and `transaction::exec_begin` remain parallel transaction openers.**

Status: **unchanged**. `orchestrator/auto_tx.rs:178-227` and `orchestrator/transaction.rs:113-173` still share the same six structural steps in two files. Both call `compio_postgres::connect` directly, both spawn the connection task, both install the tx client, both clear pending emits.

  Why: architectural impact

  No new divergence this round; no convergence either. The flag value is "tripwire for the next change to how we open a tx." Today, both paths get `DbError::Transient` mapping on connect failure; tomorrow, an OTel span injection or a `SET LOCAL` preamble has to land in two places.

  Fix: extract `pub(crate) async fn open_tx_session(begin_sql: &str, marker: TxMarker) -> Result<compio_postgres::Client, DbError>` into a new `crate::tx_session` submodule (or just on `context.rs`). `TxMarker` is the "this is auto-tx" / "this is user-driven" flag — covers the only meaningful behavioural difference (whether to flip `auto_tx_owned`).

  Alternative (R7 judgment call): leave them parallel until the next tx-open change forces the issue. Same call as R6. The lock-guard precedent supports the "extract when needed" position — `OrchestratorLockGuard` waited until three identical unlock sequences accumulated; tx-open has two. **One commit away from worth-extracting; not there yet.**

  Verification: `auto_tx.rs:178-227`, `transaction.rs:113-173`.

  ---

**I4 (new R7 / carried from R6 deferred-item list). Migration advisory-lock has no RAII guard.**

R6 flagged this as "the R7 priority": apply the `OrchestratorLockGuard` template to the migration lock. The migration lock has the same shape — session-scoped `pg_try_advisory_lock` on a dedicated client, held across a stateful run, must release on every exit path including panic. Today, `migrations.rs` open-codes the lifecycle:

- `migrations.rs:259-285` (acquire + lock-mismatch return),
- `migrations.rs:653-663` (terminal release path),
- `migrations.rs:617-618, 630-635` (mid-flight error returns that re-park the client without releasing — relies on the next call to `clear_mig_lock` to drop the client at GC time, which closes the session and releases the lock by side effect).

The release-on-error path is correct today (drop closes the session, session-scoped lock auto-releases), but the pattern is "lock releases via Drop + connection close" rather than "lock releases via explicit unlock." That works for advisory locks; it would NOT work if the lock primitive changed to something requiring an explicit release SQL.

  Why: architectural impact

  Low-immediate-risk, high-template-value. The current code is correct. But the lock-release shape diverges from `OrchestratorLockGuard`'s explicit pattern, and the next contributor reading both side-by-side has to learn two different release semantics for the same advisory-lock primitive.

  Two judgment-call observations:

  1. **The migration lock's lifecycle is genuinely different.** Where the orchestrator lock is acquired-once-released-once within a single async function, the migration lock lives across many async dispatches (`exec_begin` → `exec_fetch_batch` → `exec_commit_batch`*N → `exec_commit_batch(isDone=true)`). The RAII guard pattern fits a single function scope; the migration lock fits a state machine that the per-isolate context tracks via `mig_lock: Option<MigrationLock>`. The right shape isn't `MigrationLockGuard<'p>` — it's a state-machine type that owns the lock for the duration of one run.
  2. **The current shape works because it's PG-specific.** Connection close releases session-scoped locks; that's a PG implementation detail. The orchestrator guard codifies an explicit release for clarity. The migration lock could too, but the value is documentation, not correctness.

  Fix: extract `MigrationLockState` as a state-machine type that owns the `MigrationLock` + the lock client, exposes typed transitions (`acquire`, `fetch`, `commit`, `finalise`, `cancel`), and emits explicit `pg_advisory_unlock` on terminal transitions. Drop log on missed terminal. Same `#[must_use]` discipline as `OrchestratorLockGuard`.

  Alternative: leave as-is. The current code is correct. Document the "session close releases" behaviour at the top of `migrations.rs` for the next reader. R7 leans **defer** — the migration lock isn't broken, the orchestrator lock template is the model for when it does break.

  Verification: `migrations.rs:259-285, 617-618, 630-635, 653-663`; `context.rs:48-63` (`MigrationLock` struct definition).

  ---

**I5 (new R7). `auth/*` module is dead code from JS — 1095 LOC bootstrap + 503 session + 231 keys, zero production consumers.**

The api-surface r5 review (M1, line 188-200 of `plugin-db-api-surface-2026-05-22-r5.md`) flagged this; it's not yet been characterised as architecture-class. R7's audit confirms the dead-code surface:

- `auth::bootstrap::ensure_admin_schema(pool: &Pool) -> Result<BootstrapOutcome, DbError>` — not called from any non-`auth/*`, non-`tests/integration.rs` code in the workspace. The proposal mentions a `--harden` CLI flag (`auth/mod.rs:60-61`); no such flag exists in `crates/cli` or anywhere else.
- `auth::session::{mint_session_token, init_session, mint_and_init, mint_and_init_via_pool}` — same story. The `IsolateDbContext` doesn't carry a platform-pool slot; no callback exists to invoke session init from JS.
- `auth::keys::{rotate_session_keys, current_key_id, previous_key_id}` — same.
- `auth::mod` constants (`ADMIN_SCHEMA`, `PLATFORM_ROLE`, `APP_ROLE_TEMPLATE`, `DEFAULT_TOKEN_TTL_SECS`, `NONCE_RETENTION_SECS`) — referenced only by `auth/bootstrap.rs` itself.

  Why: architectural impact

  The module isn't broken — it's a complete, tested, documented implementation of P8c (R5-R8 of the original db proposal). The 19 references to `DbError` in `auth/bootstrap.rs` (after the R7 sweep) prove it's been kept current. But it has no production consumer; tests in `tests/integration.rs` are the only callers. This is **forward-extensibility ballast**: code that exists to be wired up later.

  Two judgment-call observations:

  1. **It's good ballast.** The hardening proposal (SECURITY DEFINER trust anchor + HMAC-signed session init) is a real future feature. Removing this code would force a rewrite when P8c gets prioritised. Keeping it means the type signatures are pinned, the SQL is reviewed, the integration tests exercise the end-to-end path.
  2. **It's currently inert.** `lib.rs:68-71` cfg-pubs `auth` under `test-helpers`. The module's docstring (`auth/mod.rs:56-62`) calls the path "opt-in per the proposal's gradual-migration guidance" — but the opt-in switch (`--harden` or `ensure_admin_schema()` from the control plane) doesn't exist. There is no path from production code to this module.

  Fix: pick one of:
  - (a) Add the `--harden` flag to the CLI; wire `ensure_admin_schema(pool)` into `zeroship-control` startup behind it. The "opt-in" language in the docs becomes true. Low-cost step.
  - (b) Move `auth/*` to a separate crate `zeroship-plugin-db-auth` (or `zeroship-platform-auth`) so the dead-code surface doesn't bloat `plugin-db`'s LOC. Keeps the module isolated until consumed.
  - (c) Leave as-is; document that this is "future P8c, currently dormant" at the top of `lib.rs`'s `auth` cfg-fork pair.

  R7 recommendation: **option (a)**. The CLI flag is one commit, makes the docs honest, and gives the maintenance cron a real wiring point. The module's existence becomes load-bearing rather than ballast.

  Verification: zero production consumers outside `tests/integration.rs`; grep for `init_session|ensure_admin_schema|mint_session_token|rotate_session_keys|--harden` across the workspace returns only the auth/* module itself + the integration test. The CLI has no `harden` subcommand or flag (grep `crates/cli` for `harden` returns no matches).

  ---

### MINOR

**M1 (carried from R6 M1). `validate.rs` returns `Result<_, String>` envelope rail.**

Unchanged. `validate.rs:59` still returns `Result<ApprovedPlan, String>`; `run_pipeline` wraps via `DbError::SchemaRefused` at `mod.rs:200-206`. The R6 docstring fix at `validate.rs:14-32` documented the rationale. The `ValidationRefusedEnvelope(String)` newtype is still an option, still not actioned. Low priority.

  Verification: `validate.rs:59`, `register_model/mod.rs:200-206`.

  ---

**M2 (carried from R6 M2). `AuditExecutor::query_text` returns `Result<Vec<Row>, compio_postgres::Error>`.**

Unchanged. `audit.rs:431-458` carries two impls (`for Pool`, `for Client`) carrying the driver type; callers re-classify via `coded_sql`. Trivial swap.

  Verification: `audit.rs:431-458`.

  ---

**M3 (carried from R6 M3). `register_model_dispatch` resolves with `ResolveValue::String("null".to_string())`.**

Unchanged. `register_model/mod.rs:91`. Constant-time JS work per call. Carry.

  Verification: `register_model/mod.rs:91`.

  ---

**M4 (carried from R6 M4). `IsolateDbContext` fields remain `pub(crate)`.**

Unchanged. Eleven fields, all `pub(crate)`. Reachable via typed accessors. Carry.

  Verification: `context.rs:70-159`.

  ---

**M5 (carried from R6 M5). Seven `mint_*` minters duplicate the boxed-instance + Weak finalizer dance.**

Unchanged. Five of the seven `pub(crate)` since R6 (`07205e54`); the duplication itself remains. Flag for `runtime-macros` to absorb (`#[v8_class(default_minter)]`).

  Verification: seven `mint_*` / `migration_start_with_spec` functions in `v8_classes/{db,collection,replication,migrations,migration,subscription,transaction}.rs`.

  ---

**M6 (carried from R6 M6). `broker.rs::Debug` impl walks two-level HashMap.**

Unchanged. `broker.rs:580`-ish. Cache `buckets` on `Broker`. Low priority.

  Verification: `broker.rs:576-588`.

  ---

**M7 (carried from R6 M7). `OrchestratorLockGuard::into_held` is `#[allow(dead_code)]`.**

Unchanged. R6 said "revisit at R10 — if still dead, delete." We're at R7; still dead. Reaffirming R10 deadline. The hardening commits (`bd1e7ce1`, `ffb1e101`, `808a32af`) tightened `release().await`'s safety net but didn't add a consumer for `into_held()`. The current pipeline never uses it because the guard itself is threaded between stages.

  Verification: `lock_guard.rs:177-189`.

  ---

**M8 (carried from R6 M8). `mint_subscription`'s structural-invariant test is text-grepping its own source.**

Unchanged. `subscription.rs:287-342`. The test pins a real invariant via source-grep `include_str!`. R6's V8-failure-shim alternative is still better but the current shape catches the regression.

  Verification: `subscription.rs:287-342`.

  ---

**M9 (new R7, low). `wal_consumer::run_supervised` test coverage doesn't pin the `ConsumerRunningGuard` Drop guarantee on panic.**

The Drop-guard fix in `replication_ops.rs:264-273` solved a real correctness gap (panic strands the running marker). The fix's robustness is tested *implicitly* via the existing `is_consumer_registered_for_tests` / `clear_consumer_registry_for_tests` harness, but no test forces a panic inside `run_supervised` and asserts the marker clears.

  Why: architectural impact

  Low. The Drop guard is a 9-line struct; the unwind behaviour is Rust-standard. Probably overkill to test. But the *contract* (panic clears the marker) is now load-bearing for `startReplicationConsumer`'s idempotency. If a future refactor breaks it, no test catches the regression.

  Fix: add `tests/integration.rs::consumer_running_marker_clears_on_panic` that spawns a closure into `run_supervised`'s position that panics on its first iteration, asserts `is_consumer_registered_for_tests(app)` is false after `compio::runtime::Runtime::block_on` returns. The harness exists; the test is ~20 LOC.

  Verification: `replication_ops.rs:264-273` (Drop guard); no matching `#[test]` for panic-recovery.

  ---

**M10 (new R7, low). `replication.rs:734-751` test references stale "returns `Result<_, String>` (not `Result<_, DbError>`)" comment after the `0049d9be` sweep.**

The doc-comment on `empty_returning_string_shape_keeps_replication_prefix` (`replication.rs:726-732`) says `ensure_publication_and_slot` returns `Result<_, String>` and that the runtime calls `.into_string()` to flow it through `?`. After `0049d9be`, the function returns `Result<_, DbError>`; the test's assertion (`DbError::Internal { message }.into_string()` keeps the `replication:` prefix) still passes, but the *justification* for it is now historical, not current.

  Why: architectural impact

  Cosmetic. The test still pins a real invariant (operator-facing prefix preserved); the comment just describes an obsolete reason.

  Fix: rewrite the doc-comment to: "After the `[I28]` sweep, `ensure_publication_and_slot` returns `Result<_, DbError>`. This test pins the operator-facing string shape that emerges from `DbError::Internal::into_string()` — log scrapers route on the `replication:` prefix; the operation tag must be present for pinpointing without a stack trace."

  Verification: `replication.rs:726-751`.

  ---

## 4. Direct Answers to the R7 Prompt Probes

**Q: auth/* dead-code surface (api-surface r5 M1) — placeholder for P8c or genuinely dead?**

Placeholder for P8c. The module is complete, tested, and current — `0049d9be` swept its error rail in this cycle, proving someone keeps it maintained. But there is no production consumer:

- `lib.rs:68-71` cfg-pubs the module under `test-helpers`.
- The proposal's `--harden` flag (referenced in `auth/mod.rs:60-61`) doesn't exist anywhere in `crates/cli` or the control plane.
- `tests/integration.rs` is the only caller of `init_session`, `ensure_admin_schema`, `mint_session_token`, `rotate_session_keys`, `PLATFORM_ROLE`, `ADMIN_SCHEMA`.
- The WAL consumer and replication code path (which P8c is supposed to harden) doesn't reference `auth::PLATFORM_ROLE` or `auth::ADMIN_SCHEMA`. Replication slots are still owned by whatever role the connection-string identifies, not by the platform role.

The honest read: this is "P8c-ready" code that ships behind an off-switch with no in-flight on-switch. It's also documented as opt-in. Three actions are sensible (in increasing order of investment): document the dormancy in lib.rs (cheap), add the `--harden` CLI flag (small, makes the module live), move to a separate crate (largest, isolates the dead-code surface from plugin-db).

See I5 for the full discussion. **R7 recommendation: add the CLI flag.** Makes the module load-bearing rather than ballast; the cost is one commit.

**Q: Orchestrator pipeline post-OrchestratorLockGuard + post-coded_sql-dedup — any new layering tension?**

No new tension; the layering is tighter than ever. `run_pipeline` reads as four stages with explicit lock handoff:

```rust
let (ctx, lock_guard) = bootstrap::bootstrap(...).await?;
let plan = plan::compute_plan(...).await?;
let approved = validate::validate(...).await.map_err(|envelope| ... wrap ...)?;
apply::apply(backend, ctx, lock_guard, approved).await  // consumes lock_guard
```

The post-R6 hardening (deferred `released = true` flip, unlock-SQL warn) doesn't change the pipeline; it makes the guard's `release()` safer under cancellation. The `coded_sql` dedup is invisible at the pipeline layer — error classification still flows through `DbError::from_pg` + `prefix_message`, just from a shared root helper instead of per-module copies.

The single layering tension is **I2 (Backend trait half-application)**, which is a R3-era carry. `bootstrap`, `run_pipeline`, and `migrations.rs` all type their `backend` parameter as `&PostgresBackend` concrete; `plan` / `validate` / `apply` are generic. Picking a position (tighten or narrow, per I2) would close it.

**Q: Adding a new ChangeKind, error variant, aggregator — where does the friction surface?**

Currently low friction.

- **New `ChangeKind`** — adds one match arm in `diff.rs::ChangeKind::as_sql`, one in `apply.rs::run_op` (the central match), one in `apply.rs::check_destructive_invariant` if the new kind is `Drop*`. The `apply.rs` match is exhaustive (no wildcard), so the compiler forces the addition. **Verified at compile time.**
- **New `DbError` variant** — adds one variant (preferably with a `code` field), one arm in `to_op_error` (`error.rs`), one arm in `Display`, one entry in the doc table at the top of `error.rs`. The enum is `#[non_exhaustive]` so external callers don't break. **Verified by code review + the variant-set sweep test at `error.rs::sql_violation_variants_stamp_canonical_codes`.**
- **New aggregator** — lands in `query.rs::build_aggregate` (~211 LOC inline match at `query.rs:1462`). This is the historical pain point (R1 noted query.rs was 4099 LOC; today it's 4277 — still growing). Adding a new aggregator means extending the match, threading it through projection / grouping / having-clause handling. No structural seam exists for this; the function is monolithic. **High friction, but scoped to one file.**

The aggregator path is the only meaningful friction; it's been scoped out since R2 (R2 explicitly: "wait until a real sqlite or planetscale prototype is in motion"). Nothing R7 should action.

**Q: Pattern consolidation — clusters this cycle, any new ones emerging?**

Cycle status:

| Cluster | R6 sites | R7 sites | Status |
|---|---:|---:|---|
| Advisory unlock | 0 | 0 | Closed at R6 (`OrchestratorLockGuard`) |
| Empty RETURNING | 3 + 1 near-sibling + 2 twins = 6 | 0 (all use `first_row_or_internal`) | **Closed R7** (`eda96ead`) |
| `coded_sql` per-module helpers | 5 | 0 (one shared, 5 thin shims) | **Closed R7** (`cbbc9059`) |
| Subscriber gate | 2 | 2 | Unchanged; abstract-worth, deferred |
| App-id stamp | 2 | 3 (`Db`, `Replication setup`, `Replication watchdog+drop`) | Grew +1; still judgment-call leave-alone |
| `prefix_message` calls in `replication.rs` | (uncounted at R6) | 5 sites | New observation; routes through one helper. Not duplication — same predicate called from different contexts. |

No new structural-class cluster emerged. The app-id stamp grew with the `c0590506` fix (added `resolve_watchdog_app_id` + `resolve_drop_abandoned_app_id`), but the *shape* is the same: each resolver is a 3-line function whose body is just `stamped.to_string()`. The "duplication" is the function signature, not the body; consolidating would lose the per-method security-critical-by-deletion contract.

**Net: Pattern Consolidation went from 62 → 78 (+16). Two large clusters closed; one small cluster grew by 1 (deliberately).**

**Q: Forward extensibility — Backend trait status.**

The trait is sound. The compile-time tests in `backend/mod.rs:367-444` pin `PostgresBackend: Backend`, the associated types stay anchored to `compio_postgres::Client` + `crate::diff::LiveSchema`, and the `'static` bound flows through the `BackendHandle = Rc<PostgresBackend>` alias.

The gap is **consumer coverage**: of the 9 entry-point functions that *could* type their backend as `B: Backend`, only 3 do (`plan`, `validate`, `apply`). The other 6 (`bootstrap`, `run_pipeline`, `migrations.rs::exec_*`) take `&PostgresBackend`. The trait is the abstraction; the callers are the concrete.

For a second backend to land (SQLite, planetscale, …), the work is:
1. Implement `Backend` for `SqliteBackend` (or whatever). ~30 methods, each a SQL translation.
2. Type the 6 concrete consumers as `B: Backend`. Mechanical.
3. Either lift `BackendHandle` to `Rc<dyn Backend>` (cost: vtable indirection on every call) or template the per-isolate context over `B` (cost: monomorphisation surface grows).

The trait shape doesn't have to change for any of this. The single forward-extensibility risk is **migration backfill**: `migrations.rs` uses PG-specific primitives (`audit_generation` bumping, `pg_try_advisory_lock`, `SELECT … FOR UPDATE`). A SQLite backend would need different primitives (file-lock + serializable transaction); the `Backend` trait's `try_acquire_advisory_lock` semantics don't map. If migrations.rs is generalised, the trait needs another abstraction layer.

**R7 read: the trait is forward-extensible for DDL orchestration. Migration backfill is PG-only and will stay that way unless a second backend explicitly demands it. See I2 for the "narrow" alternative.**

**Q: `auto_tx` vs `transaction.rs` — still divergent?**

Still parallel, no new divergence. R6 noted "next change to how we open a tx — pool variant, SET LOCAL preamble, OTel span injection — has to land in two places." R7 confirms nothing has landed in either place; both files match.

The differences are exactly:

- `auto_tx.rs:217-224` sets `auto_tx_owned = true` after `install_tx_client`; `transaction.rs:164-167` doesn't.
- `auto_tx.rs:212-215` issues `client.execute(&sql, &[])` for the per-kind BEGIN SQL; `transaction.rs:159-162` issues the same for the user-specified isolation level.

Six structural steps, two-line difference. The `open_tx_session` extraction R6 sketched would collapse the difference into a single `TxMarker` parameter. Still defer.

See I3.

**Q: cfg-fork test-helpers visibility — 8 modules; still right shape?**

Right shape. Eight modules cfg-pub'd, four always-pub, six always-pub(crate). The split has been stable since R5. No downstream crate enables `test-helpers`; the leak surface is hypothetical.

The R6 alternative ("rename `test-helpers` → `__internal_test_surface`") is still the cheap fix. The `test_support` module is the architecturally-correct fix but requires more coordination. **R7 leans toward leaving the convention as-is** and documenting it once more clearly. Carried at I1 with this judgment.

**Q: @zeroship/bootstrap boundary — clean across the R7 commits?**

Clean. The R7 commits touched:

- Orchestrator lock-guard hardening (3 commits) — invisible to JS.
- `first_row_or_internal` helper extraction — invisible to JS.
- `coded_sql` dedup — message bodies unchanged (the helper preserves the prefix shape).
- `replication.watchdog/dropAbandoned` scoping — JS API contract is *narrower* than before (cluster-wide → per-app), but the SDK's observable behaviour for the legitimate-per-app case is unchanged. Apps that previously relied on the cluster-wide enumeration would have been exploiting cross-tenant escape; that contract was never documented.
- `ConsumerRunningGuard` Drop guard — invisible to JS; only affects re-arming after panic.
- `finalise_backfill` warn-on-error — adds a tracing event; JS-visible behaviour identical.
- `Result<_, String>` sweep — JS error shape unchanged (the `.code` discriminator was the win; the message body still flows through `to_op_error()` → JS `err.message`).
- Error rail preamble fix — docs only.

The `installSchema` path in `sdks/bootstrap/src/install-schema.ts` is not on changed code paths. The `native.registerModel(...)` invariant (`.call(native, ...)` to preserve receiver) is still load-bearing on the v8_class brand check on the Db wrapper.

**Net: bootstrap boundary is clean. No new contract debt.**

**Q: Anything accumulating debt across multiple lenses, too small to flag CRITICAL?**

Three.

1. **`validate.rs`'s `Result<_, String>` envelope rail is becoming permanent.** Same observation as R5/R6. After R7's mass sweep of `replication.rs` + `auth/*`, validate is now the *only* non-utility `Result<_, String>` site in the production-call-path tree. The newtype `ValidationRefusedEnvelope(String)` would document the contract at the type level; it's an M1 minor item but its relative weight grows as everything around it gets typed.

2. **The migration lock's stateful release across multiple dispatches is unlike every other lock in the crate.** Orchestrator lock: acquire-and-release in one function. Tx lock: same. Replication slot: acquire-then-detach. Migration lock: acquire in `exec_begin`, release in `exec_commit_batch(isDone)` after N fetch/commit pairs. The lock's lifecycle is encoded in the per-isolate `MigrationLock` slot + the wrapper's GC finalizer. This works, but it's a different model. See I4 for the discussion; the recommendation is **defer** but the next contributor reading the four lock types side-by-side will ask why.

3. **The `auth/*` dormancy is in its third review round.** Each round, the module gets touched (error sweep this round) but no consumer wires it up. The cost of removing it grows each round (you're erasing more reviewed, tested code); the cost of keeping it grows linearly (LOC, build time, doc surface). The right answer (option (a) at I5, add the `--harden` CLI flag) is one commit. **R7 recommends action this round** — the cost asymmetry has reversed.

---

## 5. Still Deferred (Carry-Over)

| Item | Origin | Actionability | R7 movement |
|---|---|---|---|
| `query.rs` 4277 LOC, `build_aggregate` ≈ 211 LOC inline match | R1 | Defer until a real aggregator-extension PR forces the issue | None |
| Audit table write-only — no `db.audit.*` JS surface | R1 S5 | Low priority | None |
| WAL cross-tenant isolation is Rust-only | Security R1 | P8c work (now I5 — recommend action) | Indirect movement via `c0590506` (per-app scope filter); SECURITY DEFINER path still deferred |
| Migration advisory-lock has no RAII guard | Security R1 / R6 carry | Same template as `OrchestratorLockGuard`, exists | Carried at I4; recommendation is **defer** (current code is correct, lock-via-session-close is PG-idiomatic) |
| `auto_tx`/`transaction` tx-open extract | R5 I6 / R6 I5 / R7 I3 | One tx-open change away from worth-extracting | None |
| `IsolateDbContext` field privacy (R6 M4) | R5 M4 | Cosmetic | None |
| `Debug` for `Broker` caches buckets count (R6 M6) | R5 M6 | Cosmetic | None |
| `mint_*` duplication (R6 M5) | R4 M5 | Flag for runtime-macros to absorb | None |

---

## 6. Overall Score: 89/100

**Trajectory: 64 → 76 → 81 → 82 → 83 → 85 → 89.**

R7 movement is **+4 aggregate** — the largest single-round delta since R2 → R3. The driver mix:

- Error rail discipline: +9 (the largest mechanical refactor in any single round; 24 `Result<_, String>` signatures swept to `DbError`).
- Pattern consolidation: +16 (two clusters closed via `first_row_or_internal` and the shared `coded_sql`; subscriber gate + app-id stamp remain as judgment-call leave-alones).
- Security: +2 (cross-tenant scoping for `watchdog` + `dropAbandoned`).
- Coupling: +1 (Drop-guard for the consumer-running marker).
- Layering / module boundaries / forward extensibility: +1 each (lock-guard hardening rounds 2-4: must_use, deferred-flip, warn-on-error).

R6 projected R10 ≈ 90 if I1 (cfg-fork), I2 (error-rail sweep), I4 (empty-RETURNING helper) all landed. Two of three landed in R7 alone; the projection was conservative.

The crate's architectural posture is now **stable and near-asymptotic**. The remaining IMPORTANTs:

- I1 (cfg-fork test surface): judgment-call. Recommend **leave as convention**, optionally rename feature flag.
- I2 (Backend trait half-application): judgment-call. Recommend **narrow** — `Backend` covers DDL orchestration only; `migrations.rs` is PG-specific by design.
- I3 (`auto_tx`/`transaction` parallel openers): judgment-call. Recommend **defer until next tx-open change**.
- I4 (migration lock RAII): judgment-call. Recommend **defer** — current code is correct; the lock-via-session-close model is PG-idiomatic.
- I5 (`auth/*` dormancy): **action recommended** this round. Add the `--harden` CLI flag; makes the module live.

The single architectural blocker to crossing 90 is **picking judgment-call positions** rather than carrying them. I1 + I2 + I4 are not "things to fix" — they're "things to commit to a position on." Each round they carry, they consume review attention without yielding architectural movement. **R8 priority: decide and document the positions; close I1/I2/I4 as "judgment landed, doc updated."**

I5 (auth/*) is the single concrete actionable item: one commit adding the CLI flag closes it.

After I5 and the judgment-call closures, the remaining items (M1-M10) are all genuinely minor — cosmetic, deferred-by-design, or M9-style "add a test that we already have the harness for."

The crate is in good architectural shape. R7 made it materially better by closing two long-running structural patterns (empty-RETURNING, coded_sql) and the typed-error-rail mechanical sweep; the security fix (cross-tenant scoping) was urgent and clean.

---

## Relevant Files

- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/lib.rs` — eight cfg-fork pairs (lines 62-101, I1); test-only helpers (lines 208-333)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/lock_guard.rs` — must_use + deferred flip + warn-on-error hardening (lines 66-67, 124-163, 207-217); `into_held` dead-code (lines 177-189, M7); unit tests (lines 222-312)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/mod.rs` — `run_pipeline` (lines 159-226); `&PostgresBackend` concrete typing (line 160, I2)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/bootstrap.rs` — `&'p PostgresBackend` concrete typing (lines 78-85, 150, I2); `OrchestratorLockGuard::acquire` at line 107; release-on-err at line 137
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/apply.rs` — `B: Backend` generic (lines 37-243); `lock_guard.release().await` between passes (line 226); `check_destructive_invariant` (lines 262-271)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/plan.rs` — `B: Backend` generic (lines 34-73)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/validate.rs` — `Result<_, String>` envelope rail (line 59, M1)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/auto_tx.rs` — parallel `exec_auto_begin` (lines 178-227, I3)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/transaction.rs` — parallel `exec_begin` (lines 113-173, I3)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/error.rs` — preamble refresh after `[I28]` (lines 9-23); shared `coded_sql` helper (lines 357-361); `prefix_message` (lines 327-345); `first_row_or_internal` helper (lines 378-385); 5 `Result<_, String>` exceptions documented (lines 14-22)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/replication.rs` — fully typed `Result<_, DbError>` (lines 82, 109, 167, 375, 486, 588); per-app `slot_name LIKE $1` filter (lines 392, 526); empty-RETURNING site closed via `first_row_or_internal` (line 282); stale test docstring (lines 726-732, M10)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/replication_ops.rs` — `ConsumerRunningGuard` Drop guard (lines 264-273); dispatchers thread `app_id` from `self.app_id` (lines 96, 137, 185)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/replication.rs` — 3 `resolve_*_app_id` helpers (lines 122-161); 16 regression tests (lines 206-340)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/db.rs` — `resolve_consumer_app_id` (lines 322-333, app-id stamp cluster)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/audit.rs` — `first_row_or_internal` consumers (lines 314, 600)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/migrations.rs` — 7 `&PostgresBackend` signatures (I2); `update_backfill_progress` BEFORE COMMIT (carried from R6); `finalise_backfill` warn-on-error (lines 644-657); migration lock state-machine pattern (lines 259-285, 617-618, 630-635, 653-663, I4)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/mod.rs` — module docstring (lines 1-62); 5 constants; no production consumer (I5)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/bootstrap.rs` — `coded_sql` shim (lines 22-26); fully typed `Result<_, DbError>` (carried from `0049d9be`)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/keys.rs` — `coded_sql` shim (lines 39-43); fully typed
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/session.rs` — `coded_sql` shim (lines 33-37); 2 deliberate `Result<_, String>` hex parsers (lines 344-365); signature type-pin tests (lines 449-475)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/backend/mod.rs` — `Backend` trait (lines 68-356); compile-time `PostgresBackend: Backend` tests (lines 367-444)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/wal_consumer.rs` — subscriber gate sibling (line 554, R6 I4 sub-pattern)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/exec.rs` — subscriber gate sibling (lines 201-205, R6 I4 sub-pattern)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/subscription.rs` — defer-subscribe (carried R6 fix); source-grep structural test (lines 287-342, M8)
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-architecture-review-2026-05-22-r6.md` — prior round
