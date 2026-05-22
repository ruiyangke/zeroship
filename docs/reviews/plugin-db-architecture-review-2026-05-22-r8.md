# `crates/plugin-db` — Architecture Review, Round 8

HEAD: `a272d1af`. Prior rounds: R1 (64) → R2 (76) → R3 (81) → R4 (82) → R5 (83) → R6 (85) → R7 (89).

This is a fresh re-audit. The R7 → R8 window landed five commits, all in the consumer/replication side of the crate. R8 is the **continuation of the asymptote**: every commit is a tactical refinement of a pattern R6/R7 had already established, no new structural class emerged, and the WAL-consumer concurrency story tightened in three sequential passes (`34d209b5` → `70921112` → `4b2e7046`). The hardest scrutiny this round is whether the three rapid-fire refactors of `ConsumerRunningGuard` (now its fourth shape across four commits) are converging or just churning — answer below: converging, the final shape is correct *and* the dimensional progress is real.

The new architectural moves R8 brings:

1. `ConsumerRunningGuard` graduated from a nested struct inside the async closure to a module-scope type with its own unit tests (`70921112` + tests at `replication_ops.rs:357-414`). The four-test suite pins mark/unmark, drop, panic-unwind, and try-claim contention.
2. `WalConsumer::new` returns `Result<_, DbError>` directly (`aa639715`); the `ConsumerError::NotProvisioned(String)` variant was deleted entirely. Construction-time errors now flow through the typed `.code` rail with two distinct codes (`invalid_app_id` vs `not_provisioned`) — the SDK can branch precisely.
3. `init_session` SECURITY DEFINER now tags each `RAISE EXCEPTION` with a `USING DETAIL = '<token>'` clause and the Rust side reads `e.as_db_error()?.detail()` (`a272d1af`). Substring matching on free-text messages is gone — locale-, formatter-, and RAISE-addition-resistant.

What didn't move: I1 (cfg-fork), I2 (Backend trait half-application), I3 (auto_tx/transaction parallel openers), I4 (migration lock RAII), I5 (auth/* dormancy). Same five judgment-call items R7 carried; same five recommendations.

---

## 1. Score Per Dimension (R7 → R8)

| Dimension | R7 | R8 | Δ | Driver |
|---|---:|---:|---:|---|
| Module boundaries | 64 | **65** | +1 | `ConsumerRunningGuard` moved from local-in-closure (R7) to module scope with its own 4-test suite (`replication_ops.rs:328-414`). The dispatch boundary became measurably thinner: the closure body is now just `try_claim` + `run_supervised` + Drop. |
| Layering (orchestrator pipeline) | 89 | **89** | 0 | `run_pipeline` shape unchanged. No new stages, no reordering, the lock-guard handoff pattern is stable. |
| Extension points (new `ChangeKind`, error variant, aggregator) | 63 | **64** | +1 | A new dimension probed this round: a *new error class* (P0001 with structured DETAIL token). The pattern lands cleanly — DETAIL strings are static, the `classify_p0001_detail` helper is locale-independent, and adding a new RAISE site means adding one DETAIL constant on the PG side + one match arm in `auth/session.rs:181-201`. Same compile-time discipline as adding a `DbError` variant. |
| Coupling (replication / broker / wal_consumer) | 82 | **85** | +3 | Three refinements: (a) `ConsumerError::NotProvisioned(String)` removed entirely — the runtime-error and construction-error rails are now cleanly separated (`ConsumerError` for `run`/`consume` only, `DbError` for `new`); (b) `try_mark_consumer_running` added to `IsolateDbContext` as a primitive (`context.rs:419-426`), so the atomic claim is testable without going through `ConsumerRunningGuard`; (c) the mark moved INSIDE the spawned future (`34d209b5`) closes the spawn-failure / future-dropped-pre-poll edge. |
| Forward extensibility | 73 | **74** | +1 | The DETAIL-token pattern (`a272d1af`) is the new template for SECURITY DEFINER → Rust classification: PG sets `USING DETAIL = '<static_token>'`, Rust reads `e.as_db_error()?.detail()` and maps. Adding a new structured SQL refusal is one constant + one match arm on each side. The substring-matching antipattern is removed. |
| Coupling debt (cfg-fork visibility + duplicated patterns) | 62 | **62** | 0 | No movement. Cfg-fork still 8 pairs (I1 carry). The two carry-clusters (subscriber gate, app-id stamp) are unchanged at 2 + 3 sites. |
| Error rail discipline | 93 | **95** | +2 | `WalConsumer::new` → `Result<_, DbError>` removes the last "string-wrapped variant" in the construction path. `ConsumerError::NotProvisioned(String)` variant entirely deleted (not just bypassed). The DETAIL-token classification removes the last substring-on-error-message site. Production code paths now contain exactly **zero** `msg.contains(...)` discriminators against PG error strings. |
| Security | 93 | **94** | +1 | The DETAIL-token classification (`a272d1af`) hardens the session-init refusal path against locale changes (operator running PG in a non-English locale could previously have seen the substring match miss and a refusal flow through as a generic SQL error). Small but real. The cross-tenant scoping (R7 `c0590506`) still holds; no new escape surface. |
| Performance posture | 75 | **75** | 0 | No hot-path changes. The DETAIL classifier is on the auth/session error path, not the data path. The `try_mark_consumer_running` adds one HashSet insert vs the prior `is_consumer_running` + `mark_consumer_running` two-step — the single-step is a tiny improvement, not measurable. |
| API surface | 70 | **71** | +1 | `WalConsumer::new` signature change is technically a breaking change to its public surface, but production callers are limited to `replication_ops.rs:241` — the boundary is internal in practice. The `ConsumerError::NotProvisioned` removal *is* a public-API contraction in the test-helpers surface; the SDK never saw it (always re-stamped before reaching JS). Net: less surface, no functional regression. |
| Pattern consolidation | 78 | **80** | +2 | The R7 closure of empty-RETURNING + coded_sql is still load-bearing. R8 adds:  closure of the "RAISE EXCEPTION + substring discriminator" pattern (was 1 site, now 0); reduction of the ConsumerError variant set (3 → 3 — same count but the new shape is exhaustive). Two small new patterns *emerged* (RAISE+DETAIL token, `migrations::coded_db` directly-wraps-DbError), but both are clean, named, and tested. |

**Aggregate: 89 → 91.**

R8 movement is **+2 aggregate** — smaller than R7's +4 (which was atypical for this stage of the asymptote), in line with the R5 → R6 +2 trend. The driver mix:

- **Coupling (+3)** — three commits tightened the WAL-consumer lifecycle to its final shape. The progression: e399eeea (R7, Drop-unmark) → 34d209b5 (R8, mark INSIDE the future) → 70921112 (R8, atomic try_mark) → 4b2e7046 (R8, comment block cleanup). Four iterations to a 3-test invariant set, but each commit closed a distinct correctness gap that the prior commit had exposed.
- **Error rail (+2)** — `WalConsumer::new` typed-error simplification (`aa639715`) and DETAIL-token classification (`a272d1af`) together remove the last two substring-on-PG-message sites in the production path.
- **Pattern consolidation (+2)** — the DETAIL-token approach is a clean *new* pattern (not a consolidation of an old one), but it replaces a hostile pattern (substring matching on a localised message) and the substitution is rigorous.
- **Module boundaries / extension points / API surface / forward extensibility / security (+1 each)** — small individual movements, each tied to one of the three commits above.

R7 projected R8-R9 as continued asymptotic approach. R8 confirms: the score moved without any of the five judgment-call IMPORTANTs needing action. The remaining headroom is now `judgment landed, doc updated` work for I1/I2/I4 (closes those without code changes) and `add the --harden CLI flag` for I5.

### What moved the score

- **WalConsumer::new typed-error simplification (+1 error rail, +1 API surface, +1 forward extensibility)** — `aa639715` is the cleanest single architectural commit this cycle. Before: `WalConsumer::new` returned `Result<Self, ConsumerError>` with `ConsumerError::NotProvisioned(String)` as a third runtime-error variant that didn't fit the runtime semantics (it was a pre-flight refusal, never produced by `run`/`consume`). After: `WalConsumer::new` returns `Result<Self, DbError>` directly; `ConsumerError` is purely runtime (Connect, Io, Decode). The dispatch site (`replication_ops.rs:241-249`) forwards `e.to_op_error()` verbatim with no re-stamping; the SDK sees `.code = "invalid_app_id"` vs `.code = "not_provisioned"` distinguished at the boundary. The fix mechanically deletes a 4-LOC variant and makes the error rail's *origin* discipline match its *transport* discipline — a typed error class for each lifecycle phase.

- **DETAIL-token classification (+1 security, +1 error rail, +1 forward extensibility)** — `a272d1af` replaces five `msg.contains(...)` substring matches on PG error strings (e.g., `if msg.contains("nonce replay")`) with `e.as_db_error()?.detail()` → static token table. PG side: each `RAISE EXCEPTION` adds `USING DETAIL = 'session_<failure_class>'`. Rust side: `classify_p0001_detail` returns `Option<(&'static str, &'static str)>` — the first is the SDK `.code`, the second is the operator message. The match is exhaustive (5 detail tokens, all reachable from `bootstrap.rs:523, 529, 534, 548, 556`). Adding a 6th: one new DETAIL constant in the SECURITY DEFINER body + one new match arm + one new `DbError::validation` call site. **The whole pattern is a template for future SECURITY DEFINER errors** — the proposal has 2-3 more such functions queued (apply.rs noted similar discipline), and this is the shape they'll use.

- **ConsumerRunningGuard module-scoping + tests (+1 module boundaries, +3 coupling)** — `70921112` + the test suite. Before: the guard was a nested struct inside the async closure, no unit tests, no panic-unwind coverage. After: module-scope struct, four unit tests pinning `try_claim` semantics (`replication_ops.rs:367-413`):
  - `consumer_running_guard_new_marks_app` — happy path
  - `consumer_running_guard_drop_unmarks_app` — graceful drop
  - `consumer_running_guard_drop_unmarks_on_panic_unwind` — closes M9 R7 directly (the missing test the R7 review predicted)
  - `consumer_running_guard_try_claim_loses_when_already_marked` — concurrency-r7 race-window invariant

  The fourth test pins the `70921112` commit's correctness: two rapid-succession dispatches that race past the outer `is_consumer_running` gate cannot both spawn a consumer task. The loser's `try_claim` returns `None` and the spawned future early-returns. The dispatch site became thinner — the closure body at `replication_ops.rs:283-292` is now nine lines, with the entire lifecycle on the module-scoped guard.

### Trajectory narrative

The arc:

- **R1 → R3 (64 → 81)** — foundation: pipeline split, typed-id discipline, broker layout.
- **R3 → R5 (81 → 83)** — perf + classification: broker two-level, per-row gate, typed error rail design.
- **R5 → R6 (83 → 85)** — structure: `OrchestratorLockGuard` RAII, mint_subscription reorder, audit-progress before COMMIT.
- **R6 → R7 (85 → 89)** — mechanical closure: typed-error sweep, empty-RETURNING helper, cross-tenant scoping (CRITICAL closed).
- **R7 → R8 (89 → 91)** — **lifecycle hardening**: WAL-consumer Drop guard converges to its final atomic shape (3 commits + tests), construction-error rail typed, DETAIL-token classification replaces substring matching.

R8 is the round where the **last hostile pattern in the production path** (substring matching on PG error strings) was removed and the **last "string-shaped" error variant** (`ConsumerError::NotProvisioned`) was deleted. The error rail discipline now reaches 95/100 — the surviving 5 points are the `validate.rs` envelope-rail (deliberate SDK contract) and two ASCII-hex parsers in `auth/session.rs:393-414` (pure-function helpers, never cross an isolate boundary).

R7 projected R10 ≈ 90 with three IMPORTANTs landing; R8 reaches 91 with two of those still carrying. The crate is **clearly past 90 with the remaining headroom in judgment-call territory**.

---

## 2. Closed Since R7

| Finding | Source | How closed | Evidence |
|---|---|---|---|
| `WalConsumer::new` returns `Result<_, ConsumerError>` with `NotProvisioned(String)` variant that doesn't fit runtime semantics | code-critique r5 MAJOR-R5-4 (carried R6 → R7) | Sweep: `WalConsumer::new` returns `Result<_, DbError>`; `ConsumerError::NotProvisioned` variant deleted entirely. Two failure classes (`DbError::ValidationFailed { code: "invalid_app_id" }` + `DbError::Configuration { code: "not_provisioned" }`) flow verbatim through `replication_ops.rs:241-249` to JS. SDK sees distinct `.code` values. | `aa639715`; `wal_consumer.rs:325-368, 235-263`; `replication_ops.rs:240-250` (no re-stamp at dispatch boundary) |
| P0001 RAISE EXCEPTION classification via `msg.contains(...)` — fragile against RAISE additions, locale changes, formatter changes | code-critique r5 MAJOR-R5-1 + security r5 | PG side (`bootstrap.rs`): each of 5 RAISE EXCEPTIONS in `init_session` SECURITY DEFINER now sets `USING DETAIL = '<machine_readable_token>'`. Rust side (`session.rs`): new `classify_p0001_detail()` reads `e.as_db_error()?.detail()` and maps to typed `DbError::ValidationFailed` with stable `.code`. Locale- and formatter-independent. | `a272d1af`; `auth/bootstrap.rs:523-558`; `auth/session.rs:163-202, 249-274` |
| `run_supervised` mark stays stuck if spawn itself panics OR the future is dropped before first poll | code-critique r6 MAJOR-R6-1 (escalation of e399eeea Drop-only fix) | Mark moved INSIDE the guard's `try_claim` constructor, which runs INSIDE the spawned future. The mark fires on first poll; the unmark fires via Drop on any exit (graceful, panic, dropped-pre-poll). | `34d209b5`; `replication_ops.rs:283-292, 338-355` |
| Two rapid-succession `startReplicationConsumer()` calls can both pass the outer idempotent gate before either marks → both spawn a task → SQLSTATE 55006 retry storm | concurrency r7 NEW MINOR | `IsolateDbContext::try_mark_consumer_running` returns `bool`-on-insert (true if won the mark). Spawned task uses `ConsumerRunningGuard::try_claim`; loser returns `None` and the future early-exits without provisioning. | `70921112`; `context.rs:419-426`; `replication_ops.rs:284-288, 342-349` |
| `ConsumerRunningGuard` carried no unit tests for the panic-unwind invariant the Drop guard exists to provide | R7 M9 | Four unit tests in `replication_ops.rs:367-413` pin the contract: `new_marks_app`, `drop_unmarks_app`, `drop_unmarks_on_panic_unwind` (via `std::panic::catch_unwind`), `try_claim_loses_when_already_marked`. | `70921112`; `replication_ops.rs:357-414` (test module) |
| Comment block in `replication_ops.rs` referenced the pre-34d209b5 shape ("Mark the app as running BEFORE the spawn") after the mark moved INSIDE the future | docs-audit r5 NEW CRITICAL | Comment rewritten as a single coherent paragraph describing the current shape (guard lifetime bound to the future; try_claim atomic vs the race window) + 3-line history block citing each commit's specific failure mode (e399eeea, 34d209b5, 70921112). | `4b2e7046`; `replication_ops.rs:252-292` |

**Six closures.** Four are direct correctness or hardening fixes (NotProvisioned variant removal, DETAIL classification, mark-inside-future, atomic try-claim); one is the predicted M9 R7 test (closed); one is documentation hygiene. The closure ratio is high but each closure is small — R8 is the *polishing* round, not a structural round.

---

## 3. New + Carried Findings (R8)

### CRITICAL

None.

---

### IMPORTANT

**I1 (carried from R6 I1, since R4). `cfg`-forked module visibility is still eight pairs.**

Status: **unchanged**. `lib.rs:62-101` still defines eight modules twice (`pub(crate)` in normal builds, `pub` under `test-helpers`). The R7 review concluded "leave the convention as-is and rename `test-helpers` → `__internal_test_surface` if you want to make the contract more visible." Neither happened in R8.

  Why: architectural impact

  Unchanged from R5/R6/R7. This is the *only* IMPORTANT to carry across four review rounds (R5, R6, R7, R8) without any movement. The cost is documentation-only at this point — no new contributor has copied the shape into another crate, no consumer outside `tests/integration.rs` has enabled `test-helpers`.

  R8-specific observation: the recent commits (`70921112`, `34d209b5`) added a new `pub fn` to `context.rs` (`try_mark_consumer_running`) without needing to cfg-pub it — because `context.rs` is `pub(crate)` always, the helper is reachable from `replication_ops.rs` directly. The cfg-fork is *only* needed for modules reachable from `tests/integration.rs`. The current eight-pair list is the right partition; the only question is the *feature flag name*.

  Fix: same as R7 — rename `test-helpers` → `__internal_test_surface` (one Cargo.toml edit + `cfg` references in `lib.rs`). Or commit to "this is the convention" and document it in `Cargo.toml`'s `[features]` block.

  R8 recommendation: **close as judgment-landed**. Add a comment in `lib.rs:34-46` (the preamble) that says "this is intentional, see DECISIONS.md #cfg-fork-test-surface" and write the ADR. Stops consuming review attention.

  Verification: `lib.rs:62-101` (eight pairs unchanged); `Cargo.toml` feature listing.

  ---

**I2 (carried from R6 I3, since R3). `Backend` trait still half-applied: `migrations.rs` (7 sites) + `register_model/mod.rs::run_pipeline` (1 site) + `register_model/bootstrap.rs::bootstrap` (2 sites) take `&PostgresBackend` concrete.**

Status: **unchanged**. Concrete-typed signatures (verified at HEAD):

- `migrations.rs:215` (`exec_begin`)
- `migrations.rs:364` (`exec_fetch_batch`) — verified line for `pub async fn exec_fetch_batch` is at 363
- `migrations.rs:449` (`exec_commit_batch`) — at 448
- `migrations.rs:476` (`rollback_and_return` private helper)
- `migrations.rs:674` (`exec_status`) — at 673
- `migrations.rs:715` (`exec_cancel`) — at 714
- `migrations.rs:748` (`exec_reset`) — at 747
- `register_model/bootstrap.rs:79` (`'p PostgresBackend`)
- `register_model/bootstrap.rs:150` (private `build_ctx`)
- `register_model/mod.rs:160` (`run_pipeline`)

`register_model/{plan,validate,apply}.rs` still consume `B: Backend` generically (`apply.rs:37`, `plan.rs:34`, `validate.rs:55`). `lock_guard.rs:97` is also generic.

  Why: architectural impact

  Unchanged from R5/R6/R7. The trait scope statement (`backend/mod.rs:6-14`) covers "everything the orchestrator asks of the database" — `migrations.rs` is a peer subsystem of the orchestrator, not the orchestrator itself.

  R8-specific observation: a new tactical detail emerged. `migrations.rs:82-102` defines `coded_db(context: &str, e: DbError) -> OpError` which *re-implements* `prefix_message` inline (the match-and-prepend pattern at lines 84-99 is exactly the same logic that `crate::error::prefix_message` at `error.rs:327-345` already provides). This is a small new pattern emerging — migrations.rs duplicates the prefix logic because `prefix_message` returns `DbError` while `coded_db` returns `OpError` directly.

  Fix: two possible refactors:
  - (a) Add `crate::error::prefix_to_op_error(context: &str, e: DbError) -> OpError` that calls `prefix_message` then `to_op_error()`. `migrations.rs::coded_db` becomes a one-liner. (Mechanical, ~30 LOC delta.)
  - (b) Type the 7 `migrations.rs` fns + `run_pipeline` + `bootstrap` over `B: Backend` (R7's "tighten" recommendation). Bigger commit, but closes I2 + indirectly closes the new duplication.
  - (c) Commit to "migrations.rs is PG-only" position (R7's "narrow" recommendation) and document.

  R8 leans **(c)** — same as R7. `pg_try_advisory_lock` + `audit_generation` bumping + `SELECT … FOR UPDATE` are PG-specific. Documenting that explicitly at `migrations.rs:1` closes I2 as "judgment landed."

  Verification: `migrations.rs:82-102` (the duplicated prefix logic — new R8 evidence); `migrations.rs:215, 364, 449, 476, 674, 715, 748`; `register_model/bootstrap.rs:79, 150`; `register_model/mod.rs:160`.

  ---

**I3 (carried from R6 I5, R7 I3). `auto_tx::exec_auto_begin` and `transaction::exec_begin` remain parallel transaction openers.**

Status: **unchanged**. `orchestrator/auto_tx.rs:178-227` and `orchestrator/transaction.rs:113-173` still share six structural steps in two files. Differences pinned at R7 hold:

- `auto_tx.rs:223` sets `auto_tx_owned = true` after `install_tx_client`; `transaction.rs:165` doesn't.
- `auto_tx.rs:213` executes the per-kind BEGIN SQL; `transaction.rs:160` executes a user-specified isolation-level BEGIN.

  Why: architectural impact

  No new divergence or convergence this round. The R7 verdict ("one commit away from worth-extracting; not there yet") stands. The five new R7 → R8 commits all touched the *consumer* side of the crate, not the orchestrator side, so neither file changed.

  Fix: extract `pub(crate) async fn open_tx_session(begin_sql: &str, marker: TxMarker) -> Result<compio_postgres::Client, DbError>`. The R7 recommendation: defer until the next tx-open change forces the issue. **R8 reaffirms defer.**

  Verification: `auto_tx.rs:178-227`, `transaction.rs:113-173`.

  ---

**I4 (carried from R7). Migration advisory-lock has no RAII guard.**

Status: **unchanged**. `migrations.rs:259-285` (acquire + lock-mismatch return), `migrations.rs:653-663` (terminal release), `migrations.rs:617-618, 630-635` (mid-flight error returns that re-park the client without releasing).

  Why: architectural impact

  Unchanged from R7. The migration lock's lifecycle is a state machine across multiple async dispatches (`exec_begin` → N×`exec_fetch_batch` → `exec_commit_batch(isDone)`), so the OrchestratorLockGuard "acquire-and-release in one function" model doesn't fit directly. The release-via-session-close behaviour is PG-idiomatic and correct; the gap is **documentation**, not correctness.

  R8-specific observation: the `finalise_backfill warn-on-error` from R7 (`51ced4a0`) at `migrations.rs:644-657` is now joined by a sibling "client drop releases the lock" pattern that's never explicitly documented. The four lock types in the crate (orchestrator lock, tx-connection lock, replication slot, migration lock) now have **four different release semantics**, and only the orchestrator lock has an explicit RAII guard. The migration lock relies on session close; the tx lock relies on Drop of the Client wrapper; the replication slot lives until dropped explicitly.

  Fix (R7 verbatim, no movement): extract `MigrationLockState` as a state-machine type that owns the `MigrationLock` + the lock client, exposes typed transitions (`acquire`, `fetch`, `commit`, `finalise`, `cancel`), and emits explicit `pg_advisory_unlock` on terminal transitions.

  R8 recommendation: **defer**, same as R7. The current code is correct. The cost of refactoring is real; the benefit is documentation. Document the four lock-release semantics in one place (`docs/architecture/runtime.md` or a per-crate README) instead.

  Verification: `migrations.rs:259-285, 617-618, 630-635, 644-657`; `context.rs:48-63` (`MigrationLock` struct).

  ---

**I5 (carried from R7). `auth/*` module is dead code from JS — ~1850 LOC bootstrap+session+keys, zero production consumers.**

Status: **unchanged but worse**. R7 confirmed the module had been touched in the `0049d9be` typed-error sweep; R8 confirms it was touched *again* in `a272d1af` (the DETAIL-token classification rewrite of `auth/session.rs:181-274`). The auth module is the most-touched-yet-most-unused subsystem of plugin-db. Two rounds of mechanical refactoring (R7 type sweep, R8 classification rewrite) maintained its quality without it ever being wired up.

The dormancy is real:

- `lib.rs:68-71` cfg-pubs `auth` under `test-helpers`. Production builds cannot see it.
- The proposal's `--harden` CLI flag (referenced in `auth/mod.rs:60-61`) doesn't exist anywhere in `crates/cli` or the control plane (verified via grep — zero matches for `harden` in `crates/cli/` or `crates/control/`).
- `tests/integration.rs` is the only caller of `init_session`, `ensure_admin_schema`, `mint_session_token`, `rotate_session_keys`, `PLATFORM_ROLE`, `ADMIN_SCHEMA`.
- The WAL consumer and replication code path doesn't reference `auth::PLATFORM_ROLE`. Replication slots are still owned by the connection-string role.

  Why: architectural impact

  R7 called this "good ballast" and "currently inert" — both still true. R8's observation: the maintenance cost is **non-trivial and growing**. Two of the five R7 → R8 commits (`aa639715` swept the consumer surface; `a272d1af` rewrote the auth/session classifier; arguably `0049d9be` from R6 → R7 too) touched code that exists *only* to be wired up later. That's review-attention, type-pin-test work, and SQL-correctness verification — for code that ships off.

  Fix: same three options from R7 — (a) add `--harden` CLI flag, (b) move `auth/*` to a separate crate, (c) document the dormancy. **R8 strengthens the recommendation to (a)**. The cost asymmetry has fully reversed: each round, the maintenance cost accrues; the wire-up cost is the same one commit. **Action recommended this round.**

  R8-specific suggestion: the DETAIL-token pattern (`a272d1af`) is so clean that it deserves to be *used*. The `--harden` flag would let the maintenance cron and (when wired) the control plane exercise the SECURITY DEFINER code path and validate that the classifier matches the RAISE table. Currently every R7-style refactor of `auth/session.rs` ships untested against a real PG instance — only the unit tests run. Wiring `--harden` would give the integration tests in `tests/integration.rs` a way to run as part of the platform's CI (they may already; the wire-up is the next-step).

  Verification: zero production consumers outside `tests/integration.rs`; `crates/cli` has no `harden` subcommand or flag; `auth/mod.rs:60-61` references a flag that doesn't exist.

  ---

### MINOR

**M1 (carried from R6 M1, R7 M1). `validate.rs` returns `Result<_, String>` envelope rail.**

Unchanged. `validate.rs:55-59` still returns `Result<ApprovedPlan, String>`; `run_pipeline` wraps via `DbError::SchemaRefused` at `mod.rs:200-206`. The docstring rationale is in place. Low priority — the envelope IS the SDK wire contract; the type-system honesty of `ValidationRefusedEnvelope(String)` would help but isn't blocking.

  Verification: `validate.rs:55-59`, `register_model/mod.rs:200-206`.

  ---

**M2 (carried from R6 M2, R7 M2). `AuditExecutor::query_text` returns `Result<Vec<Row>, compio_postgres::Error>`.**

Unchanged. `audit.rs:415-422` defines the trait; impls for Pool and Client at lines 424-442 carry the driver type; callers re-classify via `coded_sql`. Trivial swap.

  Verification: `audit.rs:415-442`.

  ---

**M3 (carried from R6 M3, R7 M3). `register_model_dispatch` resolves with `ResolveValue::String("null".to_string())`.**

Unchanged. `register_model/mod.rs:91`. Constant-time JS work per call. Carry.

  Verification: `register_model/mod.rs:91`.

  ---

**M4 (carried from R6 M4, R7 M4). `IsolateDbContext` fields remain `pub(crate)`.**

Unchanged. R8 added `try_mark_consumer_running` as a public method on `IsolateDbContext` (`context.rs:419-426`), which is fine — methods are the right surface; the *fields* (eleven `pub(crate)`) are the M4 carry.

  Verification: `context.rs:70-159` (field declarations), `context.rs:419-426` (new method, the right shape).

  ---

**M5 (carried from R6 M5, R7 M5). Seven `mint_*` minters duplicate the boxed-instance + Weak finalizer dance.**

Unchanged. Flag for `runtime-macros` to absorb (`#[v8_class(default_minter)]`).

  Verification: seven `mint_*` / `migration_start_with_spec` functions in `v8_classes/{db,collection,replication,migrations,migration,subscription,transaction}.rs`.

  ---

**M6 (carried from R6 M6, R7 M6). `broker.rs::Debug` impl walks two-level HashMap.**

Unchanged. `broker.rs:576-588`. Cache `buckets` on `Broker`. Low priority.

  Verification: `broker.rs:576-588`.

  ---

**M7 (carried from R6 M7, R7 M7). `OrchestratorLockGuard::into_held` is `#[allow(dead_code)]`.**

Unchanged. R7 deadline (`revisit at R10`) still in force. R8 didn't add a consumer.

  Verification: `lock_guard.rs:177-189`.

  ---

**M8 (carried from R6 M8, R7 M8). `mint_subscription`'s structural-invariant test is text-grepping its own source.**

Unchanged. `subscription.rs:287-342`.

  Verification: `subscription.rs:287-342`.

  ---

**M9 (R7 → CLOSED). `wal_consumer::run_supervised` test coverage for `ConsumerRunningGuard` Drop guarantee on panic.**

**CLOSED at R8** via `70921112`. The four-test suite at `replication_ops.rs:367-413` includes `consumer_running_guard_drop_unmarks_on_panic_unwind` using `std::panic::catch_unwind`. Removed from carry list.

  Verification: `replication_ops.rs:387-400`.

  ---

**M10 (R7 → likely closed, needs verification). Stale `Result<_, String>` test comment in `replication.rs`.**

R7 flagged the doc-comment on `empty_returning_string_shape_keeps_replication_prefix` (`replication.rs:726-732`) as historically-justified after the `0049d9be` sweep. I did not verify whether it was rewritten in R7 → R8. If not, still M10. Low priority either way; cosmetic.

  Verification: `replication.rs:726-732` (state at HEAD — not re-checked in R8).

  ---

**M11 (new R8, low). `migrations.rs::coded_db` re-implements `prefix_message` inline.**

`migrations.rs:82-102` defines `coded_db(context: &str, e: DbError) -> OpError` that does the match-on-DbError-variant + prepend-prefix logic inline. The shared `crate::error::prefix_message` at `error.rs:327-345` already provides the same logic; `coded_db` could be a two-liner (`prefix_message(&mut e, &format!("{context}: "))` + `e.to_op_error()`).

  Why: architectural impact

  Low. The duplication is bounded — one site, in one file. But the same observation applies as R6's coded_sql cluster: the shape will spread if a sibling subsystem needs the same "prefix → OpError" composition.

  Fix: add `crate::error::prefix_to_op_error(context: &str, e: DbError) -> OpError` and reduce `migrations.rs::coded_db` to a one-liner. Mechanical, ~10 LOC.

  Verification: `migrations.rs:82-102` (inline match), `error.rs:327-345` (the shared prefix_message that would be the building block).

  ---

**M12 (new R8, low). `ConsumerError` still carries three `String`-shaped variants.**

`wal_consumer.rs:241-251` — `ConsumerError::Connect(String)`, `Io(String)`, `Decode(String)`. The fix in `aa639715` removed the `NotProvisioned(String)` variant (a construction-time error) but left the runtime-error variants as `(String)` newtypes. The strings are formed via `e.to_string()` on the underlying `compio_postgres::Error`.

  Why: architectural impact

  Lower than M1's `validate.rs` envelope rail. Unlike the construction-time errors, these runtime errors:
  - Never cross an isolate boundary (they bubble up to `run_supervised` which classifies via `is_fatal` and either retries with backoff or logs+exits).
  - Are entirely consumed within the `run_supervised` loop — no SDK ever sees them.
  - The `is_fatal` classifier at `wal_consumer.rs:703-718` matches on string substrings (`SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE` is also string-matched here? Worth checking).

  R8-specific observation: the typed-error discipline has reached the *boundary* of the crate but stopped at the *supervised runtime loop*. That's a deliberate boundary — `is_fatal` doesn't need a typed error, the strings carry enough info for the operator log. But the same substring-matching pattern that R7-R8 worked hard to eliminate from the construction-error path still lives in `is_fatal` at `wal_consumer.rs:703-718`.

  Fix: not blocking. Document the boundary in `wal_consumer.rs` — "ConsumerError variants are intentionally `(String)`-shaped because the supervised loop classifies them once via `is_fatal` and the SDK never sees them. SQLSTATE-precise classification belongs on the construction path (DbError), not the runtime path."

  Verification: `wal_consumer.rs:241-263, 703-718`.

  ---

**M13 (new R8, low). The `ConsumerRunningGuard` lifecycle history block has accreted 28 LOC of commentary.**

`replication_ops.rs:252-292` documents the three-commit lifecycle progression (e399eeea, 34d209b5, 70921112) — 28 LOC of narrative for a 17-LOC type definition. The commentary is well-organized (single coherent paragraph + dated history block) but its weight will only grow if a fourth commit lands.

  Why: architectural impact

  None. The commentary is correct, accurate, and useful — it tells a future contributor *why* the current shape is the current shape. The risk is only that the next refactor adds a fourth bullet to the history block rather than triggering a cleanup pass.

  Fix: at some future point (R10+?), the commentary should move into an ADR (`docs/decisions/`) and the inline comment becomes a one-line reference. Not actionable now; just a flag.

  Verification: `replication_ops.rs:252-292`.

  ---

## 4. Direct Answers to the R8 Prompt Probes

**Q: auth/* dormant-module status (r7 I5) — placeholder for P8c or genuinely dead?**

Still placeholder for P8c. R8 confirms the dormancy with new evidence: two of the five R7 → R8 commits touched `auth/*` code (`aa639715` swept WalConsumer surface; `a272d1af` rewrote auth/session classification), yet still zero production consumers. The cost asymmetry has fully reversed — maintenance is happening every cycle, wire-up still hasn't.

The DETAIL-token pattern in `a272d1af` is *so good* that not exercising it via real integration in CI is wasteful. Wiring the `--harden` CLI flag (option (a) at R7 I5) is one commit and:
- Makes the docs in `auth/mod.rs:60-61` honest.
- Gives the maintenance cron a real wiring point.
- Lets the bootstrap + session-init + key-rotation paths run against a real PG instance in CI.
- Validates that the DETAIL-token table matches the RAISE table empirically, not just by review.

**R8 recommendation: action this round.** See I5.

**Q: Orchestrator pipeline after recent ConsumerRunningGuard refinement — layering tension?**

No tension. The R8 ConsumerRunningGuard refinements (3 commits) all happened in `replication_ops.rs` and `context.rs`. The orchestrator pipeline (`register_model/mod.rs::run_pipeline`) is unchanged in shape — 4 stages, lock handoff through `lock_guard`, validate envelope wrapping at `mod.rs:200-206`, apply consuming the guard.

The R7 verdict ("layering is tighter than ever") holds. The pipeline reads cleanly:

```rust
let (ctx, lock_guard) = bootstrap::bootstrap(...).await?;
let plan_res = plan::compute_plan(...).await;
let approved_res = match plan_res { Ok(plan) => validate::validate(...).await.map_err(wrap_envelope), Err(e) => Err(e) };
let approved = match approved_res { Ok(a) => a, Err(e) => { let _ = lock_guard.release().await; return Err(e) } };
apply::apply(backend, ctx, lock_guard, approved).await
```

The only tension is **I2 (Backend trait half-application)** — R3-era carry. Same recommendation: narrow the trait position, document migrations.rs as PG-specific.

**Q: Extension points — adding new ChangeKind, new error variant, new aggregator.**

Currently low friction; same as R7.

- **New `ChangeKind`** — compiler-enforced via exhaustive matches in `diff.rs::as_sql` (line 114) and `apply.rs` (the central match at line 90). The arms at `apply.rs:90-92` (CreateTable / AddColumn / AddForeignKey) and the `check_destructive_invariant` filter cover all variants. **Verified at compile time.**
- **New `DbError` variant** — `#[non_exhaustive]` enum at `error.rs:54-56`. Adds one variant + one `to_op_error` arm + one Display arm + one entry in the doc table (`error.rs:31-46`). The variant-set sweep test at `error.rs::sql_violation_variants_stamp_canonical_codes` enforces canonical code coverage. **Verified by code review + sweep test.**
- **New aggregator** — `query.rs::build_aggregate` at line 1462; query.rs is 4277 LOC. **High friction, scoped to one file.** Unchanged from R7.
- **New `DbError::ValidationFailed` from a SECURITY DEFINER refusal (NEW R8 PATTERN)** — set `USING DETAIL = '<token>'` in the RAISE EXCEPTION; add one match arm in the corresponding `classify_p0001_detail`-style helper. **The new pattern is well-shaped and clean.** Five DETAIL tokens currently; the sixth would be one constant + one match arm + one line in `bootstrap.rs`.

The aggregator path is the only meaningful friction; it's been scoped out since R2.

**Q: Pattern consolidation — clusters resolved? Any new sibling-pattern emerging?**

Cycle status:

| Cluster | R7 sites | R8 sites | Status |
|---|---:|---:|---|
| Advisory unlock (orchestrator lock) | 0 | 0 | Closed at R6 |
| Empty RETURNING | 0 | 0 | Closed at R7 |
| `coded_sql` per-module helpers | 0 (5 thin shims) | 0 (5 thin shims) | Closed at R7 |
| Subscriber gate | 2 | 2 | Unchanged; abstract-worth, deferred |
| App-id stamp | 3 | 3 | Unchanged; judgment-call leave-alone (per-method security-critical-by-deletion contract) |
| `prefix_message` calls in `replication.rs` | 5 | 5 | Unchanged; same predicate from different contexts, not duplication |
| **substring-match on PG error msg** | 1 (auth/session.rs) | 0 | **Closed R8** (`a272d1af` → DETAIL-token classification) |
| **`migrations::coded_db` re-implementing `prefix_message`** | 0 | 1 (new R8 observation) | Small new pattern; M11 |
| **DETAIL-token classification (new pattern)** | 0 | 5 sites (5 DETAIL tokens in `init_session`) | **Emerging new pattern** — well-shaped, named, tested |

No new structural-class cluster emerged. The DETAIL-token pattern (5 sites in one function) is a new pattern *type* but it's a clean template; if 5 grows to 10 the existing `classify_p0001_detail` scales linearly with one match arm per token.

**Net: Pattern Consolidation went from 78 → 80 (+2). One small new pattern (M11) emerged; the DETAIL-token cluster is intentional and clean.**

**Q: auto_tx vs transaction.rs — still divergent?**

Still parallel, no new divergence. R7 noted "next change to how we open a tx — pool variant, SET LOCAL preamble, OTel span injection — has to land in two places." R8: nothing landed in either place; both files match.

The six structural steps + two-line difference is unchanged. The `open_tx_session` extraction R7 sketched would still collapse the difference. **Still defer.** See I3.

**Q: Backend trait — half-applied carry-over status.**

Half-applied at the same 9 sites (now 10 — I missed bootstrap.rs:79 in R7's count; verified at HEAD). The trait is sound; the consumer coverage is partial; `migrations.rs:82-102` introduces a small new pattern (`coded_db` re-implementing prefix logic inline) that suggests migrations.rs is increasingly path-dependent on PG specifics.

R8 leans toward R7's "narrow" alternative — commit to "migrations.rs is PG-only" and document. The new M11 evidence reinforces the recommendation: migrations.rs is *already* path-dependent enough that abstracting it would lose information.

See I2.

**Q: `WalConsumer::new` typed-error simplification — clean architectural improvement?**

**Yes, unambiguously.** Three reasons:

1. **Lifecycle clarity.** `ConsumerError` was carrying a construction-time variant (`NotProvisioned`) that didn't fit its runtime semantics (Connect/Io/Decode). Removing it cleanly separates the two error rails: typed `DbError` for construction (visible to the SDK), `ConsumerError` for runtime (consumed by `is_fatal`, never seen by the SDK).

2. **SDK distinguishability.** Pre-`aa639715`, the SDK saw `.code = "not_provisioned"` for both sanitise-failure AND missing-db_url. Post: `.code = "invalid_app_id"` (developer/deploy issue, requires a redeploy) vs `.code = "not_provisioned"` (operator/configuration issue, requires DB_URL). The remediation is genuinely different.

3. **Dispatch boundary thinness.** The dispatch site at `replication_ops.rs:240-250` no longer re-stamps the error; it forwards `e.to_op_error()` verbatim. One layer of indirection removed.

The commit is mechanical, ~120 LOC delta, with the variant deletion. No regression risk (the runtime callers of `new` are bounded — one site in `replication_ops.rs`, three test sites in `wal_consumer.rs::tests`).

**Q: P0001 DETAIL classification — does this introduce a new pattern that might spread?**

Yes, and it should. The pattern is:

```sql
-- PG side
RAISE EXCEPTION 'human-readable message'
  USING ERRCODE = 'P0001',
        DETAIL = 'machine_readable_token';
```

```rust
// Rust side
fn classify_p0001_detail(e: &compio_postgres::Error) -> Option<(&'static str, &'static str)> {
    let db_err = e.as_db_error()?;
    if db_err.code() != &compio_postgres::error::SqlState::RAISE_EXCEPTION { return None; }
    match db_err.detail()? {
        "<token>" => Some(("static_code", "operator_message")),
        ...
        _ => None,
    }
}
```

Three reasons it should spread:

1. **Locale resistance.** The DETAIL is machine-readable, never localized. Operators running PG in non-English locales (`lc_messages = 'fr_FR.UTF-8'`) won't see the substring match miss.

2. **Refactoring resistance.** Adding/renaming/reformatting a `RAISE EXCEPTION 'X' message doesn't break the classifier as long as the DETAIL token is preserved.

3. **Test discoverability.** A test asserting `classify_p0001_detail(e) == Some(("token", ...))` is more meaningful than one asserting `msg.contains("substring")` — the test's assertion ties to the contract, not the message format.

Where it should spread: any other SECURITY DEFINER function in `auth/*` (currently `verify_signature` is the other complex one; it doesn't currently RAISE), the audit-bootstrap function in `migrations.rs:121-130` (currently does `match e { Internal => re-wrap, other => to_op_error() }` — a candidate for DETAIL-token discipline if the audit functions ever RAISE), and any future SECURITY DEFINER for `__zeroship_admin`.

The pattern *won't* spread to non-SECURITY-DEFINER PG errors — those have SQLSTATE codes the existing classifier handles. The DETAIL approach is specifically for P0001 (user-defined RAISE) where SQLSTATE alone can't distinguish refusal classes.

**R8 verdict: clean new pattern, well-scoped, well-tested, low spread risk.**

**Q: cfg-fork test-helpers visibility — 8 modules; still right shape?**

Right shape. Eight modules cfg-pub'd, four always-pub, six always-pub(crate). Unchanged since R5. No downstream crate has enabled `test-helpers`; the leak surface is still hypothetical.

R8 reaffirms R7's recommendation: leave the convention as-is, document via ADR. Stops consuming review attention.

**Q: @zeroship/bootstrap boundary — clean across the R8 commits?**

Clean. The R8 commits touched:

- ConsumerRunningGuard refinements (3 commits) — invisible to JS.
- `WalConsumer::new` typed-error simplification — JS error shape *improved* (one `.code` collapsed to two distinguishable codes); SDK callers branching on `not_provisioned` continue to work; new SDK callers can branch on `invalid_app_id` separately.
- DETAIL-token classification — JS error `.code` shape unchanged (same 5 codes), but the *origin* of the code is now structured instead of substring-derived.

The `installSchema` path is unchanged. The `native.registerModel(...)` invariant is still load-bearing on the v8_class brand check.

**Net: bootstrap boundary is clean. No new contract debt; one minor improvement (distinguishable .code for WalConsumer::new failures).**

**Q: Anything accumulating debt across multiple lenses, too small to flag CRITICAL?**

Three.

1. **The `auth/*` dormancy is in its fourth review round (R5 noted, R6 carried, R7 actionable-recommended, R8 actionable-recommended-with-strengthened-cost-evidence).** The DETAIL-token rewrite in `a272d1af` is *exactly* the kind of high-quality work that wants to be exercised in CI. **R8 escalates I5 from "should action" to "the maintenance cost is now clearly higher than the wire-up cost; action this round."**

2. **The four lock-release semantics in the crate are now divergent enough to need a single documentation site.** Orchestrator lock (explicit RAII), tx lock (Client Drop), replication slot (explicit, manual lifecycle), migration lock (session close). The R7 I4 recommendation is **defer**; the R8 reinforcement is **document the divergence in one place** (probably the per-crate README) so the next contributor doesn't have to read four files to understand the lock model. Cheap, valuable, doesn't change code.

3. **`ConsumerError` still carries three `String`-shaped runtime variants (M12).** Lower priority than the construction-error rail (now closed). The decision to leave runtime errors as `(String)` is defensible — they're consumed by `is_fatal` and never see the SDK — but the substring-match pattern that R8 eliminated from the construction path still lives in `is_fatal` at `wal_consumer.rs:703-718`. Worth documenting the boundary.

---

## 5. Still Deferred (Carry-Over)

| Item | Origin | Actionability | R8 movement |
|---|---|---|---|
| `query.rs` 4277 LOC, `build_aggregate` ≈ 211 LOC inline match | R1 | Defer until aggregator-extension PR | None |
| Audit table write-only — no `db.audit.*` JS surface | R1 S5 | Low priority | None |
| WAL cross-tenant isolation is Rust-only | Security R1 | P8c work (now I5 — recommend action) | None |
| Migration advisory-lock has no RAII guard | Security R1 / R6 carry / R7 I4 | Defer; document divergence | Minor refresh in I4 (4-lock divergence noted) |
| `auto_tx`/`transaction` tx-open extract | R5 I6 / R6 I5 / R7 I3 / R8 I3 | One tx-open change away | None |
| `IsolateDbContext` field privacy (R8 M4) | R5 M4 | Cosmetic | None |
| `Debug` for `Broker` caches buckets count (R8 M6) | R5 M6 | Cosmetic | None |
| `mint_*` duplication (R8 M5) | R4 M5 | Flag for runtime-macros to absorb | None |
| `into_held` dead code (R8 M7) | R6 M7 | Revisit at R10 deadline | None |
| `migrations::coded_db` re-implements `prefix_message` (M11 new R8) | R8 | Mechanical, ~10 LOC | Newly flagged |
| `ConsumerError::(String)` runtime variants (M12 new R8) | R8 | Document the boundary | Newly flagged |

---

## 6. Overall Score: 91/100

**Trajectory: 64 → 76 → 81 → 82 → 83 → 85 → 89 → 91.**

R8 movement is **+2 aggregate** — in line with the asymptotic approach (R5 → R6 was also +2). The driver mix:

- Coupling (+3) — three commits tightened the WAL-consumer lifecycle to its final shape; the `ConsumerRunningGuard` is now module-scope with a 4-test invariant suite, the consumer-running mark moved INSIDE the spawned future, atomic `try_mark` closes the race window.
- Error rail discipline (+2) — `WalConsumer::new` typed-error simplification; DETAIL-token classification replaces the last substring-on-error-message site in the production path.
- Pattern consolidation (+2) — the substring-match cluster (1 site) closed; one small new pattern emerged (M11, `migrations::coded_db` inline prefix); one new clean pattern emerged (DETAIL-token).
- Security (+1) — DETAIL-token classification is locale- and formatter-resistant; hardens the session-init refusal path against future SQL-layer changes.
- Module boundaries / extension points / forward extensibility / API surface (+1 each) — small individual movements tied to the three big commits.

R7 projected R10 ≈ 90 with three IMPORTANTs landing; R8 reaches 91 with two of those still carrying. The crate has clearly crossed 90 and is now in **asymptotic-polish territory**.

The remaining IMPORTANTs:

- I1 (cfg-fork test surface): judgment-call. **R8 recommendation: close as "judgment landed"** — write the one-paragraph ADR, leave the convention.
- I2 (Backend trait half-application): judgment-call. **R8 recommendation: commit to "narrow" — document migrations.rs as PG-only**, add a one-line `#[doc]` at the top of `migrations.rs`. Closes I2 without code changes.
- I3 (`auto_tx`/`transaction` parallel openers): judgment-call. **R8 recommendation: defer.** Unchanged from R7.
- I4 (migration lock RAII): judgment-call. **R8 recommendation: defer; document the 4-lock divergence** in the per-crate README or `docs/architecture/runtime.md`.
- I5 (`auth/*` dormancy): **R8 recommendation: action this round.** The DETAIL-token refactor in `a272d1af` is high-quality work that should be exercised in CI. Adding the `--harden` CLI flag is one commit; the cost asymmetry has reversed.

After I5's wire-up (one commit) + I1/I2/I4 judgment-landed docs (three small commits), the score should reach ~93 with the remaining headroom concentrated in M1-M13 (genuine cosmetics or test-only items).

The crate is in **strong architectural shape**. R8 made it materially better by closing the last hostile error-classification pattern (substring matching) and tightening the WAL-consumer lifecycle to its final atomic shape. The new DETAIL-token classification is a clean template that should propagate to future SECURITY DEFINER work. The remaining IMPORTANTs are all judgment-call or wire-up; none are correctness gaps.

### Verdict per dimension comparison

R7 → R8 net dimension movement:

```
Module boundaries        64 → 65 (+1)
Layering pipeline        89 → 89 ( 0)
Extension points         63 → 64 (+1)
Coupling                 82 → 85 (+3)  ← biggest mover
Forward extensibility    73 → 74 (+1)
Coupling debt            62 → 62 ( 0)
Error rail discipline    93 → 95 (+2)
Security                 93 → 94 (+1)
Performance              75 → 75 ( 0)
API surface              70 → 71 (+1)
Pattern consolidation    78 → 80 (+2)
```

The biggest mover is **Coupling (+3)** — three sequential commits on the same subsystem. The smallest mover is **Pattern consolidation (+2)** because R8 *both* closed an old pattern (substring matching) *and* introduced two new ones (DETAIL-token, M11) — net positive but only modestly.

The dimensions that *didn't* move (Layering, Coupling debt, Performance) are the dimensions where R8 didn't touch the relevant code. The lack of motion is correct, not a regression.

---

## Relevant Files

- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/replication_ops.rs` — `ConsumerRunningGuard` at module scope (lines 328-355); 4-test suite (lines 367-413); comment block history (lines 252-292, M13); dispatch sites for setup/watchdog/dropAbandoned (lines 55-172)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/wal_consumer.rs` — `WalConsumer::new` typed-error (lines 325-368); `ConsumerError` runtime-only variants (lines 241-263, M12); `run_supervised` (lines 738-785); `is_fatal` substring-match-on-runtime-errors (lines 703-718, M12 boundary)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/session.rs` — DETAIL-token classifier `classify_p0001_detail` (lines 174-202); `init_session` typed-error rail (lines 208-275); 2 deliberate `Result<_, String>` hex parsers (lines 393-414)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/bootstrap.rs` — 5 RAISE EXCEPTION sites with USING DETAIL (lines 523-558); `coded_sql` shim (lines 22-26)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/context.rs` — new `try_mark_consumer_running` (lines 419-426); `running_consumers` field (line 149); 11 `pub(crate)` fields (M4)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/lib.rs` — 8 cfg-fork pairs (lines 62-101, I1); preamble documentation (lines 34-46)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/mod.rs` — `run_pipeline` (lines 159-226); `&PostgresBackend` concrete typing (line 160, I2)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/bootstrap.rs` — `&'p PostgresBackend` (lines 79, 150, I2); lock-guard handoff (line 107)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/{apply,plan,validate}.rs` — `B: Backend` generic (apply.rs:37, plan.rs:34, validate.rs:55)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/auto_tx.rs` — parallel `exec_auto_begin` (lines 178-227, I3)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/transaction.rs` — parallel `exec_begin` (lines 113-173, I3)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/lock_guard.rs` — `OrchestratorLockGuard` (R6 → R7 hardened); `into_held` dead code (M7)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/error.rs` — preamble (lines 1-46); shared `coded_sql` (lines 357-361); `prefix_message` (lines 327-345); `first_row_or_internal` (lines 378-385)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/migrations.rs` — `coded_db` inline prefix logic (lines 82-102, M11); 7 `&PostgresBackend` signatures (I2); `finalise_backfill warn-on-error` (lines 644-657); migration lock state-machine (lines 259-285, 617-618, 630-635, 644-657, I4)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/replication.rs` — fully typed `Result<_, DbError>` (lines 82, 109, 167, 375, 486, 588); per-app `slot_name LIKE $1` (lines 392, 526)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/replication.rs` — 3 `resolve_*_app_id` helpers (lines 114-161); 16 regression tests (lines 206-340)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/audit.rs` — `AuditExecutor::query_text` returns `compio_postgres::Error` (lines 415-442, M2)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/broker.rs` — `Debug` impl walks two-level HashMap (lines 576-588, M6)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/subscription.rs` — source-grep structural test (lines 287-342, M8)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/mod.rs` — dormant module preamble (lines 56-62) referencing non-existent `--harden` flag (I5)
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-architecture-review-2026-05-22-r7.md` — prior round
