# `crates/plugin-db` — Architecture Review, Round 6

HEAD: `37e61803`. Prior rounds: R1 (64) → R2 (76) → R3 (81) → R4 (82) → R5 (83).

Four commits since R5 (chronological — read top-to-bottom for cycle order):

1. `cbd12944` — Extract `OrchestratorLockGuard` RAII. Three open-coded `pg_advisory_unlock` blocks at `bootstrap.rs`, `register_model/mod.rs`, `apply.rs` collapse into one type with `release().await` + Drop-warn fallback. Closes R5 I1 (top finding from R4).
2. `07205e54` — Demote five `mint_*` helpers (`mint_collection`, `mint_migrations`, `mint_replication`, `mint_transaction`, `migration_start_with_spec`) from `pub` → `pub(crate)`. Fix `validate.rs` preamble docstring lie about `SchemaRefused.code`.
3. `4cbe9fa1` — Reorder `mint_subscription`: V8 alloc before `broker::subscribe()`. Closes a concurrency-class leak — any `?` between subscribe and the wrapper install previously stranded a broker entry the `is_closed`-based prune could never reach.
4. `37e61803` — Move `update_backfill_progress` BEFORE `COMMIT` in `migrations.rs::exec_commit_batch`. The progress UPDATE now joins the row-lock-protected window; reset-clobber race closed.

All four are surgical correctness/architecture wins. No structural refactors landed. R5's I5 sibling-pattern cluster — the highest-volume backlog item — is **untouched**.

---

## 1. Score Per Dimension (R5 → R6)

| Dimension | R5 | R6 | Delta | Driver |
|---|---:|---:|---:|---|
| Module boundaries (Backend trait half-applied; cfg-fork surface) | 62 | **63** | +1 | `lock_guard.rs` lives in `pub(crate) mod lock_guard` (mod.rs:29); no surface widening from the new abstraction. `register_model/{bootstrap,mod,apply}` now hand back/receive an `OrchestratorLockGuard<'p>` instead of a raw `PooledClient<'p>` — the lifetime that pinned `&PostgresBackend` in `run_pipeline` is the same, but the *contract* the guard expresses is now visible at the type level. |
| Layering (orchestrator pipeline) | 85 | **88** | +3 | `run_pipeline` now reads as a clean sequencer: `bootstrap → plan → validate → apply`, with the lock contract carried by the guard. `apply()` takes `(ctx, lock_guard, approved)` separately — the pass-1/pass-2 boundary is articulated as `lock_guard.release().await` between loops. The "release on err from plan/validate" branch (`mod.rs:217`) is now one line and obviously correct. |
| Extension points | 63 | **63** | 0 | No new `ChangeKind` / aggregator / error variant work. Match-exhaustiveness from R5's `3ef6a170` still pins additions to `ChangeKind` at compile time inside `apply.rs::run_op`. `DbError` is still `#[non_exhaustive]` so external additions of new SQLSTATE classes are fine. |
| Coupling (replication / broker / wal_consumer) | 80 | **81** | +1 | `mint_subscription`'s reorder removes a coupling failure mode between V8 alloc and the broker. The producer paths (wal_consumer + exec) still share the same `is_app_suppressed + has_subscribers` shape across two sites — pattern consolidation (R5 I5) still latent. |
| Forward extensibility | 71 | **72** | +1 | OrchestratorLockGuard is a substrate for the migration-lock RAII (Security R1 carry-over) — same lifecycle shape, currently still open-coded in `migrations.rs:259-300, 643-647`. The next contributor can copy the guard pattern with confidence; it's no longer "invent the abstraction first." |
| Coupling debt (cfg-forked visibility + duplicated patterns) | 55 | **56** | +1 | Advisory-unlock cluster (3 sites, was the top I5 sub-pattern) closes — but four other sibling-pattern clusters remain or grew. See Δ analysis below. Net: one cluster closed, one grew (empty-RETURNING now 3 sites, not 2). |
| Error rail discipline | 84 | **84** | 0 | No movement on `replication.rs` / `auth/*` sweep (R5 I3, carried). `replication.rs:248` still does `.into_string()` at the guard boundary; the 29 `Result<_, String>` signatures across replication + auth are unchanged. `8ff1b2de` (typed-error rail on auto_tx) already landed pre-R5; nothing new. |
| Security | 90 | **91** | +1 | `mint_subscription`'s alloc-before-subscribe reorder closes a slow leak that could be amplified under sustained allocation pressure (DoS-class, low severity); not a privilege escalation but a resource-exhaustion path. `resolve_*_app_id` helpers carry over unchanged — security-critical-by-deletion remains intact. |
| Performance posture | 75 | **75** | 0 | No new hot-path changes. Broker two-level layout (R5) and per-row tuple gate (R5) continue to carry. `update_backfill_progress`-before-COMMIT trades a tiny extra in-tx UPDATE round-trip for correctness; net perf neutral. |
| API surface | 66 | **70** | +4 | Five `mint_*` helpers demoted to `pub(crate)` (`07205e54`). External-test reach narrows to `mint_db` + `mint_subscription`. The cfg-fork is still there (R5 I2 unchanged) but the legitimate-public surface is smaller. Validate's docstring lie (claimed `SchemaRefused.to_op_error` doesn't stamp `.code`; it does) is fixed — SDK contract docs now match the impl. |
| Pattern consolidation | 48 | **62** | +14 | Advisory unlock (3→0 sites) collapsed into `OrchestratorLockGuard`. Subscriber gate (still 2 sites). Empty-RETURNING (was 2, **now 3 production sites + a 4th similar-shape site at `migrations.rs:326-332`**). App-id stamp (still 2 sites, still borderline-leave-alone). Net: one cluster fully resolved, one grew. Big dimension move because the one that resolved was the deepest (3 commits in 2 days at R4 cycle). |

**Aggregate: 83 → 85.**

Four commits delivered: one closed the largest architectural debt item (advisory-unlock duplication, R4 top finding), one closed a concurrency leak (broker subscribe), one tightened the API surface (5 demotions), one fixed a transactional correctness bug (audit write before COMMIT). The crate's architectural posture is **stable and improving** — the cycle is steady at ~+1-2 points/round with no regressions and a clean closure ratio.

### What moved the score

- **Layering (+3)** — `OrchestratorLockGuard` is the single largest layering improvement since R3's pipeline split. `run_pipeline` no longer threads a raw `PooledClient<'p>` plus an obligation; it threads a typed guard whose Drop logs the violation. Three release sites became one. Carrier types are now `OrchestratorLockGuard<'p>` (bootstrap return), explicit `release().await` (mod.rs error branch + apply.rs pass-1/pass-2 split). The shape of the pipeline is now legible in 12 lines of `run_pipeline`.
- **API surface (+4)** — five `pub fn mint_*` → `pub(crate) fn mint_*` demotions remove externally-reachable functions that had no external callers. The plugin's published surface is now meaningfully tighter; downstream crates can only mint `Db` + `Subscription` directly, which is the actually-tested contract.
- **Pattern consolidation (+14)** — the largest single-round dimension move on this axis, driven entirely by the lock-guard collapse. Counterweight: empty-RETURNING grew from 2 → 3 (audit.rs:329, audit.rs:617, replication.rs:245) **and** the migrations.rs:326 site is a fourth same-shape variant (different rail — `Coded` instead of `Internal` — but identical "RETURNING returned nothing" semantics). The R5 prediction held: the third RETURNING site landed; the `first_row_or_internal()` helper is now overdue, not aspirational.
- **Security (+1)** — the broker subscribe reorder closes a slow leak that wasn't on the threat model but is the kind of resource-exhaustion path a determined attacker probes for.

### Trajectory narrative

- **R4 → R5** was the perf round (broker two-level, per-row gate, replication empty-RETURNING).
- **R5 → R6** is the structure round (RAII lock guard, broker subscribe ordering, API tightening). Smaller line counts (~30 LOC net additions from `lock_guard.rs`, ~15 LOC reorder for subscription) but each commit pins an invariant that previously lived as a sibling-pattern.

The next dimension to move is **error rail discipline** (84, no movement R5→R6) — `replication.rs` + `auth/*` sweep is the largest mechanical remaining item, ~29 functions. The work is rote (`.map_err(|e| format!(...))` → `?` against the existing `From` impls). Holding at 84 because the R5 evidence (replication's `.into_string()` at the guard boundary) is still in the code — the typed `DbError::Internal` constructs an envelope and then flattens it.

---

## 2. Closed Since R5

| Finding | Source | How closed | Evidence |
|---|---|---|---|
| Advisory-unlock duplicated 3× across pipeline stages | R4 I1 / R5 I1 (highest-value carry) | `OrchestratorLockGuard` RAII abstraction; `pg_advisory_unlock` SQL appears exactly once in non-comment code (`lock_guard.rs:125`); three release sites collapse into `bootstrap.rs:137` (err-branch), `mod.rs:217` (plan/validate err), `apply.rs:226` (between-passes). | `cbd12944`; `lock_guard.rs:114-130`; `bootstrap.rs:137`; `mod.rs:217`; `apply.rs:226` |
| `mint_collection`, `mint_migrations`, `mint_replication`, `mint_transaction`, `migration_start_with_spec` all `pub` with no external caller | R5 carry from api-surface r2/r3/r4 M1 | All five demoted to `pub(crate)` in one commit; external tests reach only `mint_db` + `mint_subscription` which remain `pub`. | `07205e54`; `collection.rs:353`, `migration.rs:598`, `migrations.rs:251`, `replication.rs:117`, `transaction.rs:288` |
| `mint_subscription` registered broker entry BEFORE fallible V8 alloc — any `?` propagation between subscribe and wrapper install leaked an entry the `is_closed`-pruner couldn't reach | new R6 concurrency finding (caught by code-critique r4 M-NEW-2) | All fallible V8 ops (`new_instance`, `get_function`, prototype lookup) moved BEFORE `broker::subscribe`; infallible steps (`Box::new`, `External::new`, `set_internal_field`, `with_guaranteed_finalizer`) follow. Structural-invariant unit test enforces source ordering. | `4cbe9fa1`; `subscription.rs:172-227`, `subscription.rs:287-342` (test) |
| `update_backfill_progress` ran AFTER COMMIT; row lock released first; operator `migrations.reset` racing between the data UPDATEs and the progress write silently clobbered the cursor, causing stale-cursor resume | new R6 correctness finding (migration-pipeline r3 R3-I3 / backlog [I41]) | Progress UPDATE moved BEFORE COMMIT; error path uses `rollback_and_return` instead of plain `return_lock_client`; row lock now spans the whole window. | `37e61803`; `migrations.rs:594-619` |
| `validate.rs` preamble docstring claimed `SchemaRefused` arm did NOT stamp `.code` (it does — confirmed in `error.rs:194-205`) | new R6 docs lie | Preamble rewritten to match impl; SDK callers documented as able to branch on `err.code === "validation_refused"` directly. | `07205e54`; `validate.rs:14-32` |

Five closures. The advisory-unlock one is structural (an abstraction that didn't exist now does); the others are tactical but each pins an invariant.

---

## 3. New + Carried Findings (R6)

### CRITICAL

None.

---

### IMPORTANT

**I1 (carried from R5 I2, since R4). `cfg`-forked module visibility is now eight pairs.**

`lib.rs:62-101` defines eight modules twice — `pub(crate)` in normal builds, `pub` under the `test-helpers` feature: `audit`, `auth`, `exec`, `migrations`, `orchestrator`, `replication`, `replication_ops`, `wal_consumer`. R5 counted seven; the eighth (`orchestrator`) became cfg-forked when the orchestrator's submodules grew the integration-test surface for the lock guard and the auto_tx error rail.

  Why: architectural impact

  Each new module-with-test-reach adds another pair. The pattern is now load-bearing — eight is enough that the next contributor copies the shape without thinking, and the architectural inversion (`pub(crate)` modules carrying `pub` shape under a feature flag) compounds. A downstream crate that enables `test-helpers` gets the entire internal surface across eight modules, not the curated set tests need.

  Two judgment-call observations:

  1. **The cfg-fork shape is now a convention.** R4 + R5 flagged it; commit `90d992d5` codified it (April 13). R6 has eight pairs. The architectural-correct fix (`test_support` re-export module) hasn't been picked up across four review rounds because the cheap shim works. At this point the question shifts from "is it correct?" to "is the convention cost-effective?"
  2. **The cost surface is real but unrealised.** No downstream crate has yet enabled `test-helpers`. The leak is hypothetical until that happens. But the next time `test-helpers` gets turned on (by an integration crate that wants a single helper), the whole eight-module internal surface lights up.

  Fix: from R4/R5 verbatim — `pub(crate) mod X;` everywhere + a single `#[cfg(feature = "test-helpers")] pub mod test_support { pub use crate::audit::write_audit_row; pub use crate::migrations::exec_begin_with_pool; ... }` module. Integration tests change their import paths once.

  Alternative path (the judgment call I'd make at this round count): rename the feature `test-helpers` → `__internal_test_surface` (double-underscore convention) and document at the top of `lib.rs` that this flag exposes the full internal surface. Cheaper than the `test_support` module, makes the contract explicit in the feature name itself. **Either fix is acceptable. The current state is "documented architectural inversion" — which is worse than either alternative.**

  Verification: `lib.rs:62-101` (eight `#[cfg(not(feature = "test-helpers"))] pub(crate)` / `#[cfg(feature = "test-helpers")] pub` pairs).

  ---

**I2 (carried from R5 I3, since R3). `replication.rs`, `auth/*` still on `Result<_, String>`.**

Status: **unchanged** since R5. The R5 smoking gun (`replication.rs:248` `.into_string()` at the typed-error boundary) is exactly where it was. Counts:

- `replication.rs`: 7 `pub async fn` signatures returning `Result<_, String>` (lines 82, 98, 103, 148, 327, 424, 509).
- `auth/bootstrap.rs`: 13 sites (lines 52, 169, 187, 225, 272, 316, 347, 414, 494, 599, 630, 696, 965).
- `auth/keys.rs`: 3 sites (lines 54, 86, 113).
- `auth/session.rs`: 1 confirmed (line 152) — earlier counts of 6 may have included now-typed sites; verified current via Grep.
- `replication_ops.rs`: wrap sites depend on the upstream rail.

  Why: architectural impact

  The `replication.rs:243-249` construction remains the canonical bug: a typed `DbError::Internal` is built, then `.into_string()` flattens it because the function's signature is `Result<SetupOutcome, String>`. The SDK's `.code` discriminator loses replication-specific classification — a transient slot-creation failure reaches JS as `internal` (after the wrapping shim re-classifies the string), not `transient`. The SDK gives up instead of retrying.

  Each round this sits, the cost grows: every new replication-side site that needs typed classification gets paid into the `into_string()` rail and has to be un-flattened later. The R3-era estimate "~29 functions to sweep" is unchanged.

  Fix (verbatim from R5): mechanical sweep — `.map_err(|e| format!(...))` → `?` against the existing `From<compio_postgres::Error> for DbError` impl. Boundary wrappers in `replication_ops.rs` collapse to `?` or `e.to_op_error()` directly.

  Why not addressed this round: each of the four commits was surgical correctness/architecture. The mechanical sweep is a different shape of work — it has no per-line tactical value, only an aggregate signal-restoration. I'd queue it as one large PR rather than dripping it in alongside other fixes.

  Verification: `replication.rs:82, 98, 103, 148, 327, 424, 509`; `auth/bootstrap.rs:52, 169, 187, 225, 272, 316, 347, 414, 494, 599, 630, 696, 965`; `auth/keys.rs:54, 86, 113`; `auth/session.rs:152`.

  ---

**I3 (carried from R5 I4, since R3). `Backend` trait still half-applied: `migrations.rs` (7 sites) + `register_model/mod.rs::run_pipeline` (1 site) take `&PostgresBackend`.**

Status: **unchanged**. `migrations.rs:215, 364, 449, 658, 698, ...` (sample) and `register_model/mod.rs:160` all type their backend arg as `&PostgresBackend`. The trait is consumed generically in `register_model/{plan,validate,apply}.rs` (apply is `B: Backend` since commit `b94fbdeb`).

  Why: architectural impact

  Unchanged from R5. The trait's stated mission (`backend/mod.rs:6-14`) is to name the seams before a second backend lands. `migrations.rs` and `run_pipeline` pin the concrete type. The deferral is documented (`bootstrap.rs:196` cited an open `TODO` in R5; still there).

  R6 evidence reinforces this: the lock guard takes `B: Backend` (`lock_guard.rs:87`), but `run_pipeline` calls it with a concrete `&PostgresBackend` (`mod.rs:159-160`). The trait is generic-capable at the abstraction; the consumer pins it concrete. The single-letter generic at the leaf isn't useful when every caller upstream is concrete.

  Fix: as in R5 — type the 7 `migrations.rs` fns + `run_pipeline` over `B: Backend`. The compile-time pin in `backend/mod.rs:366-444` is the safety net.

  Alternative path (judgment call): commit to "this file is Postgres-only" by removing the trait from `migrations.rs` callers and explicitly documenting at the top. Same shape as `replication.rs` / `wal_consumer.rs`. Pros: PG-advisory-lock + `CREATE INDEX CONCURRENTLY` are PG-only primitives anyway. Cons: the trait scope statement in `backend/mod.rs:6-14` says "everything the orchestrator asks of the database" — that would have to shrink.

  Verification: `migrations.rs:215, 364, 449, 658, 698` (sample); `register_model/mod.rs:160`.

  ---

**I4 (carried from R5 I5, partially closed). Three sibling-pattern clusters still active; one grew from N=2 to N=3+.**

R5 listed four sibling-pattern clusters. R6 status per cluster:

| Cluster | R5 sites | R6 sites | Status |
|---|---:|---:|---|
| Advisory unlock | 3 | 0 | **Closed** by `OrchestratorLockGuard` (`cbd12944`) |
| Subscriber gate | 2 | 2 | Unchanged (`wal_consumer.rs:548`, `exec.rs:201-205`) |
| Empty RETURNING | 2 | **3 + a 4th near-sibling** | **Grew** |
| App-id stamp | 2 | 2 | Unchanged; still borderline-leave-alone |

  Why: architectural impact

  The advisory-unlock collapse is the high-value closure of R6 — three sites became one type. The empty-RETURNING **growth** is the new signal: where R5 had two sites (`audit.rs:325-330` + `replication.rs:240-249`), R6 has three (`audit.rs:329` + `audit.rs:617` + `replication.rs:245`) plus a fourth structurally-similar site at `migrations.rs:326-332` that uses the `Coded` rail instead of `Internal { message }` but checks the same invariant ("ID came back as 0 / empty, meaning RETURNING returned no row"). The R5 prediction landed: the third sibling appeared.

  Additional signal: the auth string-rail has its own copies — `auth/keys.rs:72` and `auth/session.rs:101` — using `.ok_or_else(|| "<msg>".to_string())` against the same predicate. These are structurally identical to the production sites but typed differently (String rail, not DbError). When I2 lands (typed-error sweep), those two sites will join the cluster, taking it to **5 typed-rail sites + 1 Coded-rail near-sibling = 6**.

  This is now well past "abstract" by R5's own criteria: load-bearing (correctness invariant, RLS-bypass class — `audit.rs:608-612` cites the regression test), recurring across commits, sites would diverge if the contract changed.

  Fix (verbatim from R5): add a helper to `crate::error`:

  ```rust
  pub(crate) fn first_row_or_internal<T>(
      rows: &[compio_postgres::Row],
      column: &str,
      op_label: &str,
  ) -> Result<T, DbError>
  where T: FromSqlOwned { /* rows.first().map(|r| r.get::<_, T>(column)).ok_or_else(|| DbError::Internal { message: format!("{op_label}: returned no row") }) */ }
  ```

  Both audit sites + replication site collapse to one-liners. The migrations.rs:326 site can stay on `Coded` if its caller wants that rail, but the helper's signature ensures the predicate is named once.

  Subscriber-gate (unchanged from R5): two sites with structurally identical "is_app_suppressed → has_subscribers → build event" prelude. Same R5 fix: a `should_emit_change(app_id, collection) -> bool` helper in `crate::broker` (or new `crate::emit`).

  App-id stamp (unchanged from R5): the helpers ARE the abstraction. The duplication is the function shape, not the body. Leave as-is until a third v8_class entry point with caller-controllable scope lands.

  Verification:
  - Empty RETURNING: `audit.rs:325-330`, `audit.rs:613-618`, `replication.rs:240-249`. Near-sibling at `migrations.rs:326-332`. Untyped twins at `auth/keys.rs:72`, `auth/session.rs:101`.
  - Subscriber gate: `wal_consumer.rs:548`, `exec.rs:201-205`.
  - App-id stamp: `v8_classes/db.rs:323-333`, `v8_classes/replication.rs:107-112`.

  ---

**I5 (carried from R5 I6). `auto_tx::exec_auto_begin` and `transaction::exec_begin` remain parallel transaction openers.**

Status: **unchanged**. `orchestrator/auto_tx.rs:178-227` and `orchestrator/transaction.rs:113-173` still share the same six structural steps (URL lookup, connect, spawn task, run BEGIN, install tx client, clear pending emits) in two files. R5 noted they're currently consistent but the next change diverges.

  Why: architectural impact

  No new divergence this round — both paths got the `DbError::Transient` mapping on connect failure pre-R5 (`auto_tx.rs:202-204`, `transaction.rs:149-151`). But both still call `compio_postgres::connect` directly, both still spawn the connection task, both still call `install_tx_client` + `clear_pending_emits`. The next change to "how we open a tx" — pool variant, SET LOCAL preamble, OTel span injection — has to land in two places.

  Fix (verbatim from R5): extract `pub(crate) async fn open_tx_session(begin_sql: &str) -> Result<compio_postgres::Client, DbError>` to `crate::context` (or a new `crate::tx_session` submodule). Each caller runs the appropriate BEGIN, calls `install_tx_client`, stamps its ownership flag.

  Alternative: leave them parallel. The flag value is "tripwire for the *next* change" — when it lands, refactor first.

  R6 note: the lock guard pattern is a precedent. The shape "extract the lifecycle into a type/helper" worked for the advisory-unlock cluster. The tx-open extraction is a smaller version of the same move — but only worth doing when the next tx-open change forces the issue. **Defer until then.**

  Verification: `auto_tx.rs:178-227`, `transaction.rs:113-173`.

  ---

### MINOR

**M1 (carried from R5 M1). `validate.rs` returns `Result<_, String>` envelope rail.**

R5 unchanged. The R6 docstring fix at `validate.rs:14-32` clarifies the wire contract but doesn't change the rail. `validate.rs:55-59` still returns `Result<ApprovedPlan, String>`; `run_pipeline` wraps in `DbError::SchemaRefused`. The newtype `ValidationRefusedEnvelope(String)` would document the SDK contract at the type level — still an option, still not actioned. Low priority.

  Verification: `validate.rs:55-59`, `register_model/mod.rs:200-206`.

  ---

**M2 (carried from R5 M2). `AuditExecutor::query_text` returns `Result<Vec<Row>, compio_postgres::Error>`.**

Unchanged. Two impls (`for Pool`, `for Client`) carry the driver type; callers re-classify via `coded_db` / `coded_sql`. Trivial change to `Result<_, DbError>` using existing `From` impl. Same recommendation as R5.

  Verification: `audit.rs:431-458` (carried from R5).

  ---

**M3 (carried from R5 M3). `register_model_dispatch` returns `ResolveValue::String("null".to_string())`.**

Unchanged. `register_model/mod.rs:91`. Constant-time JS work per call; the only `ResolveValue::String("null")` in the crate. Swap to `ResolveValue::Null` if the runtime exposes such a variant. Carried.

  Verification: `register_model/mod.rs:91`.

  ---

**M4 (carried from R5 M4). `IsolateDbContext` fields remain `pub(crate)`.**

Unchanged. Eleven fields, all `pub(crate)`, all reachable via accessors. Carried as a minor cleanup; no architectural impact.

  Verification: `context.rs:70-159`.

  ---

**M5 (carried from R5 M5 / R4 M5). Seven `mint_*` minters duplicate the boxed-instance + Weak finalizer dance.**

Five of the seven demoted from `pub` to `pub(crate)` this round (`07205e54`) — visibility tightened, but the **duplication** itself is unchanged. The pattern still belongs in `#[v8_class]` itself: `mint_db`, `mint_collection`, `mint_replication`, `mint_migrations`, `mint_migration` (via `migration_start_with_spec`), `mint_subscription`, `mint_transaction`. Flag for `runtime-macros` to absorb a `#[v8_class(default_minter)]` or `#[v8_constructor(state = ...)]` shape.

  Verification: seven `mint_*` / `migration_start_with_spec` functions in `v8_classes/{db,collection,replication,migrations,migration,subscription,transaction}.rs`.

  ---

**M6 (carried from R5 M6). `broker.rs::Debug` impl walks two-level HashMap.**

Unchanged. `broker.rs:580`-ish: `Debug::fmt` sums `len()` over inner HashMaps to preserve the prior `buckets` semantics. Cosmetic in test output / panic prints. R5 fix proposed: cache `buckets` on `Broker` as `usize`. Low priority.

  Verification: `broker.rs:576-588`.

  ---

**M7 (new R6, low). `OrchestratorLockGuard::into_held` is `#[allow(dead_code)]`.**

`lock_guard.rs:143` marks `into_held()` dead-code-allowed because no current caller needs to thread the raw `PooledClient` out of the guard. The methods' justification in the docstring (`lock_guard.rs:132-142`) explicitly says "flag it `dead_code` until that arrives."

  Why: architectural impact

  Low. The method exists because the previous inline code DID hand the client around without releasing — `into_held()` codifies that exit mode for any future stage that needs it. Today, no stage needs it. The annotation is honest about that.

  Two judgment-call observations:

  1. **The method documents the guard's contract.** Without `into_held()`, the only exit mode is `release().await` — which doesn't match the historical "bootstrap hands the client to apply still-locked" shape. Even though `release()` now subsumes that path (apply gets back an unlocked client + the lock just got re-acquired implicitly? No — apply gets the unlocked client, the lock was released BETWEEN passes inside apply, not by bootstrap). The current pipeline never uses `into_held` because the guard *itself* is what's threaded between bootstrap and apply, not the raw client.
  2. **So the method might never be used.** If `into_held()` stays dead-code for another four review rounds, the right move is to delete it and shrink the guard's contract to just `release()`. Dead-code annotations are honest at first commit; by round 10 they're just clutter.

  Fix: leave as-is for now. Revisit at R10 — if still dead, delete.

  Verification: `lock_guard.rs:143-156`.

  ---

**M8 (new R6, low). `mint_subscription`'s structural-invariant test is text-grepping its own source.**

`subscription.rs:287-342` — the test `mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure` reads the source via `include_str!` and asserts every `?` byte position is BEFORE the `broker::subscribe(` byte position.

  Why: architectural impact

  Low. The test pins a real invariant — the same invariant that the production reorder fix enforces — but it does so by string-searching its own source file. The clever-test-meter is high; the regression-catching value is real.

  Issue: the test will break on any cosmetic refactor of `mint_subscription` (split into helper functions, refactored error handling) even when the invariant still holds — e.g., if a `?` gets factored into a `try_alloc()` helper, the source grep no longer sees it, but the invariant could still be violated through the helper. **The test is a good first-line guard but not a structural proof.**

  Better path: a `cfg(test)` shim that flag-controls when V8 allocation returns `None` (e.g., a hook into the `instance_template.new_instance` path) so the test can drive a real failure and assert `broker::live_subscription_count() == 0` afterward. The current happy-path test does exactly that (`mint_subscription_happy_path_registers_exactly_one_broker_entry`); the unhappy-path test could mirror it.

  Fix: leave as-is for now. The structural test is a sentinel, not a permanent fixture. If a future refactor breaks it, replace with the V8-failure-shim approach.

  Verification: `subscription.rs:287-342`.

  ---

## 4. Direct Answers to the R6 Prompt Probes

**Q: `OrchestratorLockGuard` — RAII abstraction shape any good?**

Yes — substantially better than the open-coded sites it replaced.

Strengths:

- `Option<PooledClient<'p>>` internal storage so `release()` and `into_held()` move the client out cleanly without `mem::replace` / `ManuallyDrop` gymnastics. The `released` flag suppresses the warning branch on the second drop.
- `Drop` logs `tracing::error!` if the guard hits the catastrophic path (panic, missed release) — the SQL it would have run is named in the log message.
- Idempotent `release()` — calling on an already-released guard returns `Ok(None)`. Defensive.
- The async/sync impedance match is explicit in the docstring: "we can't run `pg_advisory_unlock` from `Drop` because it's async; the pooled session-scoped lock auto-releases when the session ends; this branch is the catastrophic-path fallback only."

Weaknesses:

- `into_held()` is dead-code-allowed (M7). Worth keeping for one more round; delete if still dead at R10.
- The `tag: &'static str` field is currently always `"register_model"`. That's correct — the lock guard is specific to the orchestrator's `register_model` stage. But it makes the type slightly narrower than the name suggests. **Alternative**: name it `RegisterModelLockGuard` and remove the `tag` field. Or keep the name, lift `tag` to a const, and document. The current shape is fine; this is a naming nit.
- The guard's `acquire` method takes a `key: String`, which feels unnecessary — the call site is always `lock_key(app_id)` for the same `app_id` in `RegisterContext`. Could be `acquire(backend, client, app_id)` and compute the key internally. **Minor**: the current shape lets `acquire` work without knowing about `lock_key`, which is decoupled-correct. Defensible either way.

Verdict: the abstraction is right. One unused method to revisit at R10; otherwise solid.

**Q: Does the broker subscribe defer (4cbe9fa1) close the entire class, or is there a sibling?**

Closes the immediate class; the sibling pattern is worth flagging. The general invariant is "if a fallible step Y depends on the success of a registration X, register X AFTER Y." Other v8_class minters in the crate:

- `mint_db` (`db.rs:367`) — no broker-equivalent registration; just V8 alloc + state install. Clean.
- `mint_collection` (`collection.rs:353`) — same shape; no external registration before V8 alloc.
- `mint_transaction` (`transaction.rs:288`) — allocated by `begin_transaction_dispatch`; the transaction connection is opened in the spawned async closure AFTER the wrapper exists. The pattern *here* is "wrapper before connect"; the closure's `?` on connect failure leaves the wrapper with an unmatched token (`tx_token == 0`), which is documented (`transaction.rs:38-41`). Different shape, same correctness property.
- `mint_replication`, `mint_migrations`, `mint_migration` — all wrap state that the *caller* allocates; the wrapper is the storage cell. No external-registration-before-alloc footgun.

So `mint_subscription` was unique in having an external registration (broker) that had to happen *during* mint. The defer fix closes the class. The structural test (`subscription.rs:287-342`) is a sentinel; if a future minter adds a similar registration shape, the convention "register after V8 alloc" is now codified in `subscription.rs`'s doc comment and visible in code review.

**Q: `update_backfill_progress` BEFORE COMMIT — was the previous shape architecturally wrong, or just a race nobody saw?**

Architecturally wrong. The `lock_audit_row_for_update` call at `migrations.rs:496-505` issues `SELECT … FOR UPDATE`, which holds the row lock until COMMIT. The progress UPDATE was conceptually inside the "row-protected" window but was *executed* after COMMIT, when the lock had already released. The window between "data UPDATEs applied" and "progress write lands" was unprotected; an operator's `migrations.reset(...)` could land in that window, clobber the cursor, and the progress write would then advance past the reset point.

The fix is correct: the progress UPDATE joins the transaction; the row lock now spans the whole window; reset blocks until COMMIT.

The architectural lesson: when a row lock is acquired FOR UPDATE inside a transaction, **all writes that mutate the same row's state should be inside the same transaction**. The previous code violated this and the bug was latent until an integration test hit the race.

Net: solid correctness fix. The commit message explicitly cites the race (`migration-pipeline r3 R3-I3 / backlog [I41]`); the fix mirrors the pattern. Low risk of further sibling races on this audit row — the same `lock_audit_row_for_update` → mutate → COMMIT shape is the canonical pattern.

**Q: API surface tightening (07205e54): is `mint_db` + `mint_subscription` the right "stable public mint surface"?**

Yes — but the deeper question is whether `mint_*` should be public AT ALL. The v8_class wrappers are minted internally by:

- `Db.collection(name)` (v8_method on Db) → `mint_collection`
- `Db.beginTransaction(opts?)` (v8_method on Db) → orchestrator → `mint_transaction`
- `Db.openSubscription(name)` (v8_method on Db) → `mint_subscription`
- `Db.migrations` (v8_getter on Db) → `mint_migrations`
- `Db.replication` (v8_getter on Db) → `mint_replication`
- `Migrations.start(spec)` (v8_method) → `migration_start_with_spec`

Every v8_class instance the SDK touches is minted via a JS-visible method on a parent wrapper. The minters are the *implementation* of those methods; they shouldn't need to be publicly callable.

External tests reach `mint_db` (to construct a `Db` from outside a full runtime) and `mint_subscription` (to test the broker integration without going through a JS callback). Those two are honest public surface for testing — *but they're public for tests, which is exactly what the `test-helpers` feature is for*.

**Architecturally cleaner**: demote `mint_db` and `mint_subscription` to `pub(crate)` AND move both behind the `test-helpers` cfg-fork (or a `test_support` module per I1). The published surface of `plugin-db` becomes `DbPlugin::new(url)`, full stop — everything else is internal.

This is a follow-up worth doing post-I1: once the `test_support` re-export module exists, both `mint_db` and `mint_subscription` can move into it, completing the API tightening.

Defer until I1 lands.

**Q: Is the pattern-consolidation score (62) a meaningful number, or is it just averaging?**

It's load-bearing. The 48 → 62 jump reflects one specific architectural transition: the largest sibling-pattern cluster (advisory unlock, 3 sites, 3 commits citing each other) collapsed into a typed primitive (`OrchestratorLockGuard`). That's the moment a "missing primitive" stopped being missing.

The remaining clusters:
- Empty RETURNING (3 sites, growing) — overdue, see I4.
- Subscriber gate (2 sites, stable) — abstract-worth, see I4.
- App-id stamp (2 sites, stable, intentional separation) — leave-alone.

If empty-RETURNING gets a `first_row_or_internal()` helper next round, expect Pattern Consolidation to move +6 → +8 (largest remaining cluster closes). Beyond that, the score is in the 70s range, blocked on the App-id-stamp judgment call — which is "borderline-leave-alone" per R5's rubric.

**Q: `@zeroship/bootstrap` boundary — clean across the four R6 commits?**

Yes. The four commits touched:

- Orchestrator pipeline internals (`cbd12944`) — invisible to JS.
- v8_class visibility (`07205e54`) — narrows internal surface; the JS-visible API on the `Db` / `Migrations` / `Replication` wrappers is untouched.
- `mint_subscription` reordering (`4cbe9fa1`) — the JS-visible `openSubscription()` contract is unchanged; the failure shape (no broker entry on V8 alloc failure) is documented in the doc-comment.
- `migrations.exec_commit_batch` (`37e61803`) — the JS-visible `commitBatch(...)` resolves with the same JSON shape as before. The SDK error path on `audit row update` failure now triggers `rollback_and_return` correctly; the JS-visible error code is `internal` (unchanged from pre-fix).

The `installSchema` path in `sdks/bootstrap/src/install-schema.ts` is not on the changed code paths. The `native.registerModel(...)` invariant (`.call(native, ...)` to preserve receiver) is still load-bearing on the v8_class brand check on the Db wrapper.

Net: bootstrap boundary is clean. No new contract debt this round.

**Q: Anything accumulating debt across multiple lenses, too small to flag CRITICAL?**

Two patterns worth naming, neither rising to CRITICAL:

1. **Validate's `Result<_, String>` envelope rail is becoming a permanent island.** It's a one-function exception inside an otherwise-typed pipeline. The justification (SDK wire contract) is real, but the architectural odor is that one stage of a four-stage pipeline runs on a different error rail than the other three. R5 M1 suggested `ValidationRefusedEnvelope(String)` newtype. Each round it stays, the "validate is special" exception becomes more entrenched.

2. **The mig-lock + tx-lock + orchestrator-lock + replication-slot lifecycle are four parallel session-scoped resources, each with its own acquire/release pair, each prone to the same leak class.** OrchestratorLockGuard solved one. The other three still open-code: `migrations.rs:259-300` (mig lock acquire/release via `try_acquire_advisory_lock` + `release_advisory_lock`), `transaction.rs:147-167` (tx connection lifecycle), `replication.rs` (slot creation via `pg_create_logical_replication_slot`). All four would benefit from the same RAII shape. The lock guard is the template. **R7 priority: extract `MigrationLockGuard` using the same shape.** Saves the next "we forgot to release on err path" commit cycle.

---

## 5. Still Deferred (Carry-Over)

| Item | Origin | Actionability | R6 movement |
|---|---|---|---|
| `query.rs` 4277 LOC, `build_aggregate` ≈ 211 LOC inline match | R1 | Defer until a real aggregator-extension PR forces the issue | None |
| Audit table write-only — no `db.audit.*` JS surface | R1 S5 | Low priority | None |
| WAL cross-tenant isolation is Rust-only | Security R1 | Deferred to P8c SECURITY DEFINER work | None |
| Migration advisory-lock has no RAII guard | Security R1 | Same template as I1, now exists (lock_guard.rs) — extract MigrationLockGuard | Template available; flagged as R7 priority |

---

## 6. Overall Score: 85/100

**Trajectory: 64 → 76 → 81 → 82 → 83 → 85.**

R6 movement is +2 aggregate, driven by:
- Pattern Consolidation: +14 (advisory-unlock cluster fully closed; empty-RETURNING grew).
- API Surface: +4 (five `mint_*` demotions).
- Layering: +3 (RAII guard makes pipeline contract legible).
- Smaller bumps on Module Boundaries, Coupling, Forward Extensibility, Security (+1 each).

Four commits delivered: one structural (`OrchestratorLockGuard` RAII — the R4 top finding closed); one concurrency-class leak (mint_subscription ordering); one transactional correctness (audit progress before COMMIT); one visibility tightening (five mint demotions). The cycle's signal: every R5 IMPORTANT that mapped to a single-commit fix landed in R5→R6. The IMPORTANT items remaining (I1 cfg-fork, I2 error-rail sweep, I3 Backend trait, I4 sibling patterns, I5 tx-open extract) are larger or judgment-deferred.

The crate's architectural posture is **stable and improving** at ~+1.5 points/round. The cycle math projects R10 ≈ 90 if the empty-RETURNING helper (I4 sub-fix), the cfg-fork test_support module (I1), and the error-rail sweep (I2) all land. The remaining items beyond that are either judgment-calls (App-id stamp, validate envelope rail) or scoped-out (query.rs 4277 LOC).

**The single highest-value follow-up for R7 is the empty-RETURNING helper** — three sites grew to a fourth structurally-different one this round, the predicate is mechanical, and the helper signature pins the contract. Net code: -10 lines, -1 invariant to maintain across N sites.

**Second-priority follow-up: `MigrationLockGuard` extract** using the `OrchestratorLockGuard` template. Same advisory-lock-with-release-discipline shape; saves the next "we forgot to release" cycle in migrations.

I3 (error-rail sweep, ~29 functions) is the largest mechanical follow-up. Worth one focused PR.

---

## Relevant Files

- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/lib.rs` — eight cfg-fork pairs (lines 62-101, I1)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/mod.rs` — module map; `lock_guard` is `pub(crate)` (line 29)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/lock_guard.rs` — new RAII guard (274 LOC, lock_guard.rs:114-130 = the `release()` body; `into_held` dead-code at lines 143-156, M7)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/mod.rs` — `run_pipeline` now sequences `bootstrap → plan → validate → apply` cleanly (lines 159-226); concrete `&PostgresBackend` arg unchanged (line 160, I3)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/bootstrap.rs` — `OrchestratorLockGuard::acquire` (line 107); release-on-err uses `guard.release().await` (line 137)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/apply.rs` — `lock_guard` parameter; between-passes release at line 226
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/validate.rs` — docstring fix at lines 14-32 (matches actual impl); still on `Result<_, String>` rail (line 59, M1)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/auto_tx.rs` — parallel `exec_auto_begin` (lines 178-227, I5)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/transaction.rs` — parallel `exec_begin` (lines 113-173, I5)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/subscription.rs` — defer-subscribe reorder (lines 172-227); source-grep structural test (lines 287-342, M8)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/db.rs` — `mint_db` still `pub` (line 367); `resolve_consumer_app_id` (lines 322-333, I4 app-id stamp)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/collection.rs` — `mint_collection` demoted to `pub(crate)` (line 353)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/migrations.rs` — `mint_migrations` demoted (line 251)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/migration.rs` — `migration_start_with_spec` demoted (line 598)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/replication.rs` — `mint_replication` demoted (line 117); `resolve_setup_app_id` (lines 107-112, I4 app-id stamp)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/transaction.rs` — `mint_transaction` demoted (line 288)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/migrations.rs` — `update_backfill_progress` BEFORE COMMIT (lines 594-619); empty-row-error at line 326-332 (I4 near-sibling); 7 `&PostgresBackend` signatures (I3)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/audit.rs` — empty-RETURNING sites at lines 325-330 + 613-618 (I4)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/replication.rs` — empty-RETURNING site at lines 240-249 (I4); 7 `Result<_, String>` signatures (I2); `.into_string()` boundary at line 248
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/keys.rs` — string-rail empty-RETURNING twin at line 72 (I4); 3 `Result<_, String>` signatures (I2)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/session.rs` — string-rail empty-RETURNING twin at line 101 (I4)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/bootstrap.rs` — 13 `Result<_, String>` signatures (I2)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/broker.rs` — two-level HashMap (lines 402-468); `has_subscribers` (line 460, I4 sub-pattern); Debug recomputes buckets (lines 576-588, M6)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/exec.rs` — subscriber gate (lines 201-205, I4 sub-pattern)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/wal_consumer.rs` — sibling subscriber gate (line 548, I4 sub-pattern)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/backend/mod.rs` — `Backend` trait + compile-time tests (lines 68-356, 366-444); `release_advisory_lock` unused by orchestrator (line 154 — orchestrator uses inline SQL in `lock_guard.rs:125`)
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-architecture-review-2026-05-22-r5.md` — prior round
