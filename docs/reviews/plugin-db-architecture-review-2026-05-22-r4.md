# `crates/plugin-db` — Architecture Review, Round 4

HEAD `309ed52f`. Prior rounds: R1 `7c253fb6` (64), R2 `718b65fe` (76), R3 `29b8a013` (81).

Re-audit fresh. Four commits since R3:

- `309ed52f` — drop cross-app `appId` override in `v8_classes/replication.rs` + `v8_classes/db.rs` (CRITICAL security)
- `ed697c45` — extract `map_audit_bootstrap_err` for 4 audit-write sites in `migrations.rs`
- `3bb41fa1` — `bootstrap.rs` advisory-lock leak fix (third advisory-lock RAII miss patched in this codebase)
- `90d992d5` — cfg-fork module visibility on `test-helpers` feature in `lib.rs`

---

## 1. Score Per Dimension (R3 → R4)

| Dimension | R3 | R4 | Δ | Note |
|---|---:|---:|---:|---|
| Module boundaries (Backend trait) | — | **62** | new lens | Trait stayed half-applied; `migrations.rs` still takes `&PostgresBackend`. No movement. |
| Layering (orchestrator pipeline) | 83 | **84** | +1 | Pass boundaries crisp. `bootstrap` advisory-lock leak fix tightened the entry pass. But 3 nearly-identical advisory-unlock blocks now duplicated across the pipeline — see I1. |
| Extension points | 63 | **63** | 0 | `query.rs` 4275 LOC unchanged; `Backend` trait still surfaces `&PostgresBackend` for half the surface. |
| Coupling (replication / broker / wal_consumer) | 78 | **78** | 0 | Module graph identical to R3. |
| Forward extensibility (schema strictness, migration safety net, replication backfill) | — | **70** | new lens | Schema strictness fork in place (`strict` / `lenient` / `off`); migration audit-row state machine survives finaliser; replication backfill via `start_lsn` already plumbed. Soft spots: `validate.rs` rail still `Result<_, String>` by docstring policy, and replication ops still leak `Result<_, String>` to `DbError::Internal`. |
| Coupling debt (NEW R4 dim — cfg-forked visibility + duplicated unlock blocks) | — | **55** | new | `lib.rs` carries 7 `#[cfg(...)] pub(crate) mod X; #[cfg(...)] pub mod X;` pairs. Acceptable shim but not free — see I2. |
| Error rail discipline | 80 | **82** | +2 | `migrations.rs` 4 sites now route through `map_audit_bootstrap_err`. `replication.rs` 7 functions still `Result<_, String>`. |
| Security | 82 | **90** | +8 | Cross-app appId override CRITICAL closed; both v8_class entry points now route through `resolve_*_app_id` helpers (unit-testable invariants). |
| Performance posture | 70 | **70** | 0 | No new perf work since R3. |
| API surface | 52 | **66** | +14 | Demote → cfg-fork is net positive for release surface (the symbols disappear in non-test builds). Release-build `pub` is now: `broker`, `error`, `query`, `v8_classes` + the four already-`pub` helpers. The cfg-fork pattern is shim debt — see I2. |

**Aggregate: 81 → 82.**

The four commits closed two CRITICALs (security + bootstrap advisory leak), refactored one duplication (audit-error mapping), and dropped release-build API surface. Net architectural movement is +1: the advisory-unlock duplication and cfg-forked visibility add a small amount of structural debt that the API tightening offsets.

---

## 2. Closed Since R3

| Finding | Source | How closed | Evidence |
|---|---|---|---|
| `Db::startReplicationConsumer` / `Replication::setup` honoured JS-supplied `appId` (cross-app WAL hijack) | security r2 CRITICAL | `resolve_consumer_app_id` / `resolve_setup_app_id` lifted out as unit-testable helpers; ignore `_opts` verbatim | `v8_classes/db.rs:322-333`, `v8_classes/replication.rs:107-112` |
| `bootstrap.rs` early-return after `acquire_advisory_lock` leaks the lock back to pool | pre-existing leak | `inner` future captures inner work; error path issues explicit `pg_advisory_unlock` before drop | `bootstrap.rs:140-187` |
| 4 audit-bootstrap-failed sites in `migrations.rs` flatten typed `DbError` to opaque string | error UX | extracted `map_audit_bootstrap_err` (unit-tested 3 ways) | `migrations.rs:117-126`, `migrations.rs:253,646,687,720` |
| 14 `pub mod` declarations in `lib.rs` expose every internal | API surface (R3 I2) | Reorganised into 4 always-`pub` / 4 always-`pub(crate)` / 7 cfg-forked | `lib.rs:47-101` |

---

## 3. New Findings (R4)

### CRITICAL

None. The two CRITICALs in the queue (cross-app override + bootstrap lock leak) both landed.

### IMPORTANT

**I1. Three duplicated advisory-unlock blocks across the orchestrator — missing `lock_guard` abstraction.**

`bootstrap.rs:179-184`, `register_model/mod.rs:219-225`, `apply.rs:203-209` each carry a copy of:

```rust
let unlock_sql =
    "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
let key = bootstrap::lock_key(app_id);
let _ = lock_client
    .query_text_params(unlock_sql, &[key.as_str(), bootstrap::LOCK_TAG])
    .await;
drop(lock_client);
```

Three slightly different invocations, same SQL, same key shape, same drop order. The three landed in three separate commits (`b4e533e2`, `37a0ef76`, `3bb41fa1`) each plugging the *same* leak in a different lifecycle stage. That's the signature of a missing primitive.

  Why: architectural impact
  The lock contract (held across multiple awaits, must explicitly unlock before dropping the pooled client, MUST unlock on every error path) is a load-bearing invariant — three near-misses have produced the p8a2 ordering hang in different forms. Encoding it three times means the fourth lifecycle stage that needs the lock will get it wrong again. The `Backend::release_advisory_lock` method on the trait (`backend/mod.rs:154`) already exists — but only `migrations.rs:288,624` calls it; the orchestrator code hand-rolls the SQL because it threads the `lock_client` through stage boundaries.

  Fix: concrete recommendation
  Introduce `OrchestratorLockGuard<'p>` (or similar) in `backend/mod.rs`:

  ```rust
  pub struct OrchestratorLockGuard<'p> {
      client: PooledClient<'p>,
      key: String,
      tag: &'static str,
      released: bool,
  }

  impl OrchestratorLockGuard<'_> {
      pub async fn release(mut self) {
          self.released = true;
          let _ = self.client.query_text_params(
              "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)",
              &[self.key.as_str(), self.tag],
          ).await;
      }
  }

  impl Drop for OrchestratorLockGuard<'_> {
      fn drop(&mut self) {
          if !self.released {
              // Synchronous warn; the pooled connection will return to pool with lock held —
              // matches the pre-fix bug exactly, but at least it's loud.
              tracing::error!(key = %self.key, "OrchestratorLockGuard dropped without release");
          }
      }
  }
  ```

  Then `bootstrap::bootstrap` returns `(RegisterContext, OrchestratorLockGuard<'p>)`; `run_pipeline` calls `guard.release().await` on the error branch; `apply.rs` calls it between Pass 1 and Pass 2. The three callsites collapse to one method invocation. `Drop` cannot run async, so this isn't true RAII — but a `release()` invariant that lints (and the `Drop` warn) keeps the type system pointing at the missing call.

  Verification: file:line evidence
  Three duplicated blocks: `bootstrap.rs:179-184`, `register_model/mod.rs:219-225`, `apply.rs:203-209`. All three commits explicitly cited each other (`3bb41fa1` says "mirrors apply.rs pattern", `b4e533e2` says "mirror discipline").

  ---

**I2. cfg-forked module visibility in `lib.rs` is structural debt, not just a shim.**

`lib.rs:62-101` defines seven modules twice — once `pub(crate)`, once `pub`, gated on the `test-helpers` feature. The commit message (`90d992d5`) justifies it as preferable to rewriting "~1500 lines of integration test imports for no surface gain."

  Why: architectural impact
  - Each affected module (`audit`, `auth`, `exec`, `migrations`, `orchestrator`, `replication`, `replication_ops`, `wal_consumer`) is *architecturally* `pub(crate)` — the release-build invariant is "internal." But the build artifact under `--features test-helpers` exposes the entire surface, including helpers that were not designed as a public API. A test that imports `replication::publication_name` gets a `pub fn` rather than a `pub(crate) fn`; if production code in another crate ever depends on `zeroship-plugin-db/test-helpers` (e.g. a control-plane orchestrator borrowing the test harness), the same symbol becomes a real API contract.
  - The visibility split is **conceptually wrong**: the modules ARE `pub(crate)` in the architecture. The cfg-fork is a workaround for the rustc rule that an integration test crate cannot reach into `pub(crate)` items. The right answer is a curated `test_support` module — gated on `test-helpers` — that re-exports exactly the helpers tests need (`pub use crate::migrations::exec_begin_with_pool;` etc.). That preserves the `pub(crate)` invariant on the underlying module and gives tests one stable surface.
  - The current shape also surfaces helper-vs-production confusion: every `pub fn` in (e.g.) `migrations.rs` is reachable under `test-helpers`, but `exec_begin` is genuinely an internal helper (the v8_class layer calls it), not a test wrapper. The cfg-fork conflates "test helper" with "production internal that tests also need."

  Fix: concrete recommendation
  Replace the seven cfg-fork pairs with `pub(crate)` module declarations plus a single `test_support` module:

  ```rust
  pub(crate) mod audit;
  pub(crate) mod migrations;
  // ... etc

  #[cfg(feature = "test-helpers")]
  pub mod test_support {
      pub use crate::migrations::{
          exec_begin, exec_fetch_batch, exec_commit_batch,
          exec_begin_with_pool, exec_status_with_pool, /* ... */
      };
      pub use crate::audit::{ensure_audit_table_exists, write_audit_row, /* ... */};
      pub use crate::orchestrator::register_model::{run_pipeline, exec_register_model_with_pool};
      pub use crate::replication::{ensure_publication_and_slot, watchdog_query, /* ... */};
      // every test reach-in gets named here
  }
  ```

  Integration tests then write `use zeroship_plugin_db::test_support::exec_begin_with_pool;` instead of `use zeroship_plugin_db::migrations::exec_begin_with_pool;`. The commit message's "no surface gain" claim is true for line count, but false for the architectural invariant.

  Verification: file:line evidence
  `lib.rs:62-101` — seven cfg-fork pairs. Commit `90d992d5` rationale.

  ---

**I3. `replication.rs` (the entire module) and `auth/*` (`bootstrap.rs`, `keys.rs`, `session.rs`) still return `Result<_, String>`.**

R3 listed `replication_ops.rs` as the surface needing migration (closed in `a0fec06a`). But the actual leak is one layer deeper: `replication_ops` *wraps* the underlying `replication.rs` errors as `DbError::Internal { message: e }` at the dispatch boundary (`replication_ops.rs:84, 118, 155, 215`). Every replication operator op therefore reaches JS as `.code = "internal"` — `setup`, `watchdog`, `dropAbandoned`, and `startReplicationConsumer` all collapse to one bucket.

`replication.rs` has 7 `pub async fn` returning `Result<_, String>` (`replication.rs:81,97,102,147,314,411,496`) and `auth/*` has 17 (`auth/bootstrap.rs` alone has 15). Same pattern: the rail breaks before it reaches the user.

  Why: architectural impact
  The SDK contract — `err.code` is the stable discriminator — is being silently degraded for replication and auth surfaces. Errors that should classify as `transient` (the operator wants the SDK to retry) come out as `internal` (the SDK gives up). `replication.rs:178` returns `Err(format!("replication: probe pg_publication: {e}"))` — that's a Postgres error that *could* be `Transient`, but loses its classification at the `format!` boundary.

  This is the same finding R3 logged for `audit.rs` (closed) and `replication_ops` (closed by wrapping, but the wrap is the problem). The audit module's migration pattern (`From<compio_postgres::Error> for DbError`, then `?`) is the template. The auth and replication modules predate it.

  Fix: concrete recommendation
  Sweep both modules through `?` against the existing `From<compio_postgres::Error> for DbError` impl. The 7 functions in `replication.rs` change signature to `Result<_, DbError>`. Each `.map_err(|e| format!(...))` becomes `?`. `replication_ops.rs`'s `DbError::Internal { message: e }` wrappers become `?` (or `e.to_op_error()` directly). Same shape for auth.

  Verification: file:line evidence
  `replication.rs:81,97,102,147,314,411,496` — 7 `Result<_, String>` signatures. `auth/bootstrap.rs:52,169,187,225,272,316,347,414,494,599,630,696,965` — 13 sites. `replication_ops.rs:84,118,155,215` — 4 wrapping sites that depend on the upstream rail.

  ---

**I4. `Backend` trait remains half-applied: `migrations.rs` still takes `&PostgresBackend`, the entire trait surface is unreachable from outside the crate.**

R3 IMPORTANT (deferred). No movement at R4. `migrations.rs:211,355,440,467,638,679,712` all take `backend: &PostgresBackend`. The `Backend` trait is consumed generically only by `orchestrator/register_model/{plan,validate,apply}.rs` — three files. Every other "backend-using" file talks to the concrete impl.

  Why: architectural impact
  The trait's stated purpose (`backend/mod.rs:6-39`) is "name the seams BEFORE a second backend lands so we don't accidentally bake `compio_postgres::Pool` / `Client` into every consumer file." But `migrations.rs` does exactly that — it names `LockClient = <PostgresBackend as Backend>::Client` (line 57) but then types its functions over `&PostgresBackend` so the trait abstraction is bypassed in the place it matters most for a future sqlite/planetscale prototype. The trait is half-applied: usable for the three pipeline stages, decorative everywhere else.

  Fix: concrete recommendation
  Make `migrations.rs`'s seven `pub async fn`s generic over `B: Backend`. `make_test_backend` becomes `make_test_backend<B>` parameterised by the same. Compile-time test in `backend/mod.rs` already covers `B = PostgresBackend`, so no runtime risk.

  Verification: file:line evidence
  `migrations.rs:211,355,440,467,638,679,712` — 7 `backend: &PostgresBackend` signatures. `orchestrator/register_model/{plan,validate,apply}.rs` — the trait-generic counter-examples.

  ---

### MINOR

**M1. `validate.rs`'s `Result<_, String>` rail is documented but isolated; the boundary wrap is awkward.**

`validate.rs:53-57` returns `Result<ApprovedPlan, String>` where the `Err` is the validation_refused envelope JSON. `run_pipeline` wraps it in `DbError::SchemaRefused { code: "validation_refused", envelope_json }` (`orchestrator/register_model/mod.rs:201-209`). The docstring `validate.rs:16-30` explains the choice ("the envelope IS the wire contract").

  Why: architectural impact
  Holds — the SDK genuinely needs `JSON.parse(err.message)`. But the SIGNATURE `Result<ApprovedPlan, String>` doesn't communicate "this String is a JSON envelope, not an error message." A future reader sees a `String` rail and either pollutes it (adds a non-envelope `Err(format!(...))`) or misses the implicit contract.

  Fix: concrete recommendation
  Introduce a newtype: `pub struct ValidationRefusedEnvelope(pub String)` with a `from_destructive(deploy_id, ops)` constructor. Signature becomes `Result<ApprovedPlan, ValidationRefusedEnvelope>`. The boundary wrap stays one line, but the type tells future readers "this is the envelope, not an opaque error."

  Verification: file:line evidence
  `validate.rs:57`, `orchestrator/register_model/mod.rs:204-208`.

  ---

**M2. `AuditExecutor::query_text` still returns `Result<Vec<Row>, compio_postgres::Error>`.**

R3 M3 still open. The trait was demoted to `pub(crate)` in R2, so no external compat concern — and `From<compio_postgres::Error> for DbError` (`error.rs:296`) exists. Trivial change.

  Why: architectural impact
  The trait's two impls (`for Pool`, `for Client`) carry the driver type; callers (e.g. `migrations.rs:264` via `coded_db`) re-classify. A `Result<Vec<Row>, DbError>` return would let callers `?`-flow through `coded_db`'s contextual prefix without the wrapper.

  Fix: change return type to `Result<Vec<Row>, DbError>`, propagate `From` impl at the boundary.

  Verification: file:line evidence
  `audit.rs:431-458`.

  ---

**M3. `register_model_dispatch` carries a `ResolveValue::String("null".to_string())` for the success arm.**

`orchestrator/register_model/mod.rs:91`. Dispatch returns the literal string `"null"` so the JS side does `JSON.parse("null")` → `null`. Works, but the v8_classes layer renders this back to JS via the runtime's `ResolveValue::String` arm — a serialise / parse round-trip for a constant.

  Why: architectural impact
  Minor cost (~constant-time JS work per `registerModel`), but it's the only `ResolveValue::String("null")` in the crate. `ResolveValue::Undefined` or `ResolveValue::Null` (if the runtime exposes such variants) would skip the parse step.

  Fix: check whether `ResolveValue::Null` exists in `zeroship_runtime::state`; if so, swap.

  Verification: file:line evidence
  `orchestrator/register_model/mod.rs:91`.

  ---

**M4. `IsolateDbContext` fields remain `pub(crate)`.**

R3 M2 unchanged. `context.rs:70-159`. Eleven fields, all `pub(crate)`, all reachable via accessors. The accessor pattern is fully established; the field-direct path is dead weight.

  Verification: file:line evidence
  `context.rs:70,74,78,89,99,113,119,136,143,149,158`.

  ---

**M5. Three v8_class files duplicate the `mint_X` + `External` + `Box::into_raw` pattern with weak-finalizer-and-forget.**

`v8_classes/db.rs`, `v8_classes/replication.rs:117-155`, `v8_classes/migrations.rs`, `v8_classes/subscription.rs`, `v8_classes/migration.rs:543`, `v8_classes/transaction.rs`. Six files, six copies of the boxed-instance + weak finalizer dance.

  Why: architectural impact
  This is the same pattern as I1 — repeated code patches the same invariant in multiple places. The runtime-macros crate (`runtime-macros/`) already owns `#[v8_class]`; the `mint_*` pattern is its dual. Should likely move into `#[v8_class]` itself as `Type::mint(scope, state)`.

  Fix: out of scope for plugin-db; flag for `crates/runtime-macros` to absorb. Architecture call only.

  Verification: file:line evidence
  Six `mint_*` functions, similar shape. (e.g. `v8_classes/replication.rs:117-155`.)

  ---

## 4. The R4-Specific Probe Answers

The user prompted three pointed questions; here are direct answers.

**Q: Does cfg-gated module visibility constitute "architectural" coupling debt?**

Yes — see I2. The cfg-fork pattern preserves the release surface but inverts the architecture: modules conceptually `pub(crate)` carry a `pub` shape under a feature flag. The intent is "tests only", but Cargo features are not access-control — any downstream that turns on `test-helpers` (or any other crate that depends on `zeroship-plugin-db` with `test-helpers` in its feature graph) gets the full surface. A curated `pub mod test_support { pub use ... }` module preserves the architectural invariant without the cfg duplication.

**Q: Does the new `map_audit_bootstrap_err` helper indicate a missing trait or just a local refactor?**

Local refactor, correctly scoped. It maps `DbError → OpError` for one specific lifecycle stage (the `ensure_audit_table` failure path) and only one variant arm needs wrapping. The R3 hypothesis that this might need a trait was overweighted — three lines of pattern-match dispatch don't justify another trait. The unit tests (three of them, covering `Transient`, `LockContention`, `Internal`) lock the discipline in.

But: the *pattern* here — "preserve SQLSTATE classification verbatim, wrap only the catch-all" — repeats throughout the crate. `apply.rs:87-101`, `bootstrap.rs:110-119`, `migrations.rs:117-126` all do it. That's three sites, all manual match arms. A `DbError::with_operator_context(prefix: &str)` method on `DbError` itself (preserves SQLSTATE variants, prefixes only the `Internal` message) would collapse the three sites into one method call each. Lower priority than I1 / I3.

**Q: Do the three advisory-lock leak fixes screaming for a `lock_guard` abstraction?**

Yes — see I1. Three commits (`b4e533e2`, `37a0ef76`, `3bb41fa1`) patched the *same* invariant violation in three lifecycle stages. The pattern is duplicated verbatim across `bootstrap.rs:179-184`, `register_model/mod.rs:219-225`, `apply.rs:203-209`. The fourth call site (whenever the migration lifecycle gets a new stage) will get it wrong again unless the invariant is encoded as a type. The recommended fix is in I1.

This is the strongest "missing primitive" signal the codebase carries right now. The R3 review missed it because only one of the three fixes had landed by HEAD@R3 (`37a0ef76`); the second (`b4e533e2`) landed mid-cycle and the third (`3bb41fa1`) was the trigger for this R4 prompt.

---

## 5. Still Deferred (Carry-Over)

These were deferred at R3 and remain unaddressed:

| Item | Origin | Actionability |
|---|---|---|
| `query.rs` 4275 LOC, `build_aggregate` ≈ 211 LOC inline match | R1 | Defer until a real aggregator-extension PR forces the issue |
| Serde round-trips: `exec_mutation_with_emit` re-parses JSON | Perf R1 C1 | Actionable, single-file change in `exec.rs` |
| `crud.rs` `first_row_or_null` / `row_count_as_f64` re-parse | Perf R1 C2 | Actionable, multi-file |
| Audit table write-only — no `db.audit.*` JS surface | R1 S5 | Low priority; SDK reads via privileged-find exemption |
| WAL cross-tenant isolation is Rust-only | Security R1 | Deferred to P8c SECURITY DEFINER work |
| Migration advisory-lock has no RAII guard | Security R1 | I1 here addresses the orchestrator's version; migration version separate |

---

## 6. Overall Score: 82/100

**Trajectory: 64 → 76 → 81 → 82.**

R4 movement is small. Two CRITICALs closed (cross-app override, bootstrap leak), one duplication captured (`map_audit_bootstrap_err`), API surface tightened via cfg-fork. The advisory-unlock duplication (I1) and cfg-forked visibility (I2) add small structural debt; the cross-app override fix and audit-error refactor offset.

The crate's architectural posture is **stable**. The remaining issues are not structural — they're "should have been one type / one trait / one re-export module instead of N copies." None of them block shipping. The four CRITICAL-class issues this codebase has shipped since R2 (`audit.rs` string rail, cross-app override, bootstrap lock leak, validation guards) were all caught and closed within ~24h; the velocity is right.

**The single highest-value follow-up is I1**: an `OrchestratorLockGuard` type collapses three commits-worth of pattern into one primitive and prevents the fourth occurrence. Followed by I3 (sweep `replication.rs` and `auth/*` to `Result<_, DbError>`) — same template as the audit migration, ~25 sites of mechanical work.

---

## Relevant Files

- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/lib.rs` — cfg-fork visibility (lines 62-101), the I2 source
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/bootstrap.rs` — advisory-unlock block lines 179-184 (one of three copies)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/apply.rs` — advisory-unlock block lines 203-209 (copy 2)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/mod.rs` — advisory-unlock block lines 219-225 (copy 3); `run_pipeline` still takes `&PostgresBackend` (line 160)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/migrations.rs` — `map_audit_bootstrap_err` (lines 117-126); 7 functions still `&PostgresBackend` (lines 211, 355, 440, 467, 638, 679, 712); 4 sites using helper (lines 253, 646, 687, 720); 3 unit tests (lines 869, 897, 915)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/replication.rs` — 7 `Result<_, String>` signatures (lines 81, 97, 102, 147, 314, 411, 496) — I3
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/replication_ops.rs` — 4 `DbError::Internal { message: e }` wrap sites (lines 84, 118, 155, 215) — I3 downstream
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/bootstrap.rs` — 13 `Result<_, String>` signatures — I3
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/backend/mod.rs` — `Backend` trait (lines 68-356); compile-time tests (lines 366-444); `release_advisory_lock` already on the trait (line 154) but not used by orchestrator
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/audit.rs` — `AuditExecutor::query_text` still returns `compio_postgres::Error` (lines 431-458) — M2
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/validate.rs` — `Result<_, String>` envelope rail (line 57); docstring rationale (lines 16-30) — M1
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/db.rs` — `resolve_consumer_app_id` security-critical helper (lines 322-333); unit tests for the regression guard (in the file)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/replication.rs` — `resolve_setup_app_id` security-critical helper (lines 107-112); unit tests (lines 157-213)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/context.rs` — `IsolateDbContext` fields `pub(crate)` (lines 70-159) — M4
