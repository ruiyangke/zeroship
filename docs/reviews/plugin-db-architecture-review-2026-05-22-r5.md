# `crates/plugin-db` — Architecture Review, Round 5

HEAD `c83d6a8c`. Prior rounds: R1 (64) → R2 (76) → R3 (81) → R4 (82).

Five commits since R4 (chronological):

- `3ef6a170` — `apply.rs::run_op` hard-errors on `DropColumn`/`DropIndex` outside `ChangeClass::Destructive` (mirrors the destructive-skip filter at the `match` arm site).
- `49b0b98e` — `exec.rs::emit_for_rows` gates `(columns, tuple)` build behind `!is_app_suppressed && has_subscribers` (mirrors `wal_consumer.rs::emit_for_tuple`'s R2 N-C1 fix — second sibling).
- `e37b188f` — `error.rs` + docs cleanup (docs critique CRITICALs).
- `0e58c4e8` + `b32ba383` — `broker.rs` two-level `HashMap<app, HashMap<collection, Vec<Subscription>>>` replaces the prior tuple-key map; `has_subscribers(&str, &str)` becomes alloc-free; `b32ba383` patches a cherry-pick duplicate.
- `c83d6a8c` — `replication.rs::ensure_publication_and_slot` surfaces empty-RETURNING as `DbError::Internal` (mirrors the `audit.rs` `d7cfc089` fix from R4 cycle — second sibling).

---

## 1. Score Per Dimension (R4 → R5)

| Dimension | R4 | R5 | Δ | Driver |
|---|---:|---:|---:|---|
| Module boundaries (`Backend` trait still half-applied) | 62 | **62** | 0 | No movement. `migrations.rs` and `register_model/mod.rs::run_pipeline` still take `&PostgresBackend` (7 + 1 sites). |
| Layering (orchestrator pipeline) | 84 | **85** | +1 | `apply.rs` invariant check makes the destructive-class contract explicit; pass-1/pass-2 boundary tighter. Still three duplicated unlock blocks (I1 unchanged). |
| Extension points | 63 | **63** | 0 | `ChangeKind::DropColumn`/`DropIndex` now match-exhaustive in `apply.rs::run_op` (good — adding a variant is a compile error in apply); no movement on aggregator / error variant. |
| Coupling (replication / broker / wal_consumer) | 78 | **80** | +2 | `broker.rs` two-level layout drops the only `(String, String)` alloc on the WAL fan-out path; `wal_consumer::emit_for_tuple` + `exec::emit_for_rows` now share the same `has_subscribers + is_app_suppressed` gate shape across producer paths. |
| Forward extensibility | 70 | **71** | +1 | `broker.rs` two-level layout is also the substrate for a future `(app, collection, fingerprint)` three-level index (P8b read-set fingerprint matching). Adding the third level is now a one-line nest. |
| Coupling debt (cfg-forked visibility + duplicated unlock blocks) | 55 | **55** | 0 | No movement. I2 (cfg-fork) unchanged. I1 (advisory-unlock duplication) unchanged. |
| Error rail discipline | 82 | **84** | +2 | `replication.rs:243-249` now surfaces empty-RETURNING through `DbError::Internal` instead of `.unwrap_or_default()` → sentinel. The two-sibling pattern (audit + replication) is a missing helper signal — see I5. |
| Security | 90 | **90** | 0 | No new attack surface. R4's `resolve_*_app_id` helpers (cross-app fix) carry over; the broker two-level layout preserves per-app isolation in the data structure (was `(String, String)` key before — still isolated, but now structurally per-app). |
| Performance posture | 70 | **75** | +5 | Two-level broker + `has_subscribers` fast probe removes the per-WAL-frame allocation entirely on the no-subscriber path; `exec::emit_for_rows` skips per-row tuple build under the same gate. Both are direct hot-path wins. |
| API surface | 66 | **66** | 0 | No public re-exports changed since R4. `broker::has_subscribers` is `pub(crate)` — correctly hidden. |
| Pattern consolidation (NEW R5 dim) | — | **48** | new | Three sibling-pattern signals now active: subscriber gate (2 sites), empty-RETURNING (2 sites), stamp `.app_id` from self (2 sites). Plus the carry-over advisory-unlock (3 sites). See §3 I5. |

**Aggregate: 82 → 83.**

Five commits closed two perf CRITICALs (broker tuple-key alloc; per-row tuple build), one correctness gap (replication sentinel LSN), one safety invariant (destructive-class hard-error). The architecture is **stable** — net +1 — and the highest-value follow-up remains `OrchestratorLockGuard` (I1, carried from R4), now joined by `I5` (the empty-RETURNING + subscriber-gate sibling patterns).

Trajectory narrative:

- **Performance posture (+5)** — the broker two-level refactor is the single largest dimension move this round. It deletes the `(String, String)` allocation on the publish hot path AND introduces an alloc-free `has_subscribers` probe that the WAL consumer + autocommit `exec_mutation_with_emit` use to short-circuit tuple construction. Two perf CRITICALs cleared by one structural change.
- **Coupling (+2)** — same broker refactor: producer paths (WAL + local-emit) now share an identical "is_app_suppressed → has_subscribers → build event" prelude. The shapes match by convention, not yet by type.
- **Error rail (+2)** — the replication empty-RETURNING fix is the second sibling of an audit-row pattern that already exists. The third sibling will be the trigger for a `first_row_or_internal()` helper (I5).
- **Pattern consolidation (new dim, 48)** — four sibling-pattern clusters now visible; one (advisory-unlock) is three commits deep, the others (subscriber gate, empty-RETURNING, app-id stamp) are two each. The codebase is at the inflection point where ad-hoc duplication crosses into "missing primitive."

---

## 2. Closed Since R4

| Finding | Source | How closed | Evidence |
|---|---|---|---|
| Broker publish allocates `(String, String)` per call | new R5 perf (regression from R2 N-C1 subscriber gate) | Two-level `HashMap<app, HashMap<collection, _>>` keyed on `&str`; `has_subscribers` is alloc-free | `broker.rs:402-468`; commits `0e58c4e8`, `b32ba383` |
| `exec::emit_for_rows` built per-row `(columns, tuple)` HashMap before checking subscribers | new R5 perf | Gate on `is_app_suppressed \|\| !has_subscribers` BEFORE the per-row build loop | `exec.rs:201-205`; commit `49b0b98e` |
| `replication.rs:240` `.unwrap_or_default()` silently produced `lsn=""` sentinel | new R5 correctness | `.ok_or_else(\|\| DbError::Internal { ... }.into_string())` mirrors `audit.rs:328` | `replication.rs:240-249`; commit `c83d6a8c` |
| `apply.rs::run_op` silently returned `Ok(())` for `DropColumn`/`DropIndex` outside destructive class | new R5 invariant | `check_destructive_invariant` errors first; match arm also errors so the compiler enforces exhaustiveness on future `ChangeKind` additions | `apply.rs:62, 155-157, 266-`; commit `3ef6a170` |

---

## 3. New + Carried Findings (R5)

### CRITICAL

None.

### IMPORTANT

**I1 (carried from R4). `OrchestratorLockGuard` abstraction still missing — three duplicated unlock blocks.**

Status: **unchanged**. Three near-identical advisory-unlock blocks remain at:

- `register_model/bootstrap.rs:179-184`
- `register_model/mod.rs:219-225`
- `register_model/apply.rs:224-230`

Each is `SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)` against the same key shape (`zs_reg:<app>`, `register_model`). The trait already names this as `Backend::release_advisory_lock` (`backend/mod.rs:154`) but no orchestrator caller uses it — they each hand-roll the same SQL because the pooled client lifetime threads through stage boundaries.

  Why: architectural impact
  Three lifecycle stages, three near-misses (commits `b4e533e2`, `37a0ef76`, `3bb41fa1` all patched the *same* leak in different stages). The pattern signals a missing type; the fourth stage that needs the lock will get it wrong again. R4 proposed `OrchestratorLockGuard<'p>` with `release().await` + `Drop` warn; the proposal stands.

  Fix: from R4 verbatim — introduce `OrchestratorLockGuard<'p>` in `backend/mod.rs` with explicit `release().await` and a `Drop` impl that emits `tracing::error!` if `released == false`. `bootstrap::bootstrap` returns `(RegisterContext, OrchestratorLockGuard<'p>)`; `run_pipeline` calls `guard.release().await` on the validate-err branch; `apply` calls it between Pass 1 and Pass 2. Three callsites collapse to one. Alternative: keep the trait's `release_advisory_lock` and add a `Backend::release_orchestrator_lock(key: &str, lock_client: &Self::Client)` convenience that hides the key/tag pair — narrower scope, doesn't introduce a new type, but doesn't catch the "forgot to call release" footgun either.

  Verification: `bootstrap.rs:179-184`, `mod.rs:219-225`, `apply.rs:224-230` (carried from R4).

  ---

**I2 (carried from R4). cfg-forked module visibility is still structural debt.**

Status: **unchanged**. `lib.rs:62-101` defines seven modules twice — once `pub(crate)`, once `pub` under the `test-helpers` feature.

  Why: architectural impact
  R4 proposed a curated `pub mod test_support { pub use ... }` re-export module. The proposal stands as the architecturally-correct fix:

  1. **Cargo features are not access control.** Any downstream that turns on `test-helpers` (a sibling crate that wants the test harness, an integration suite in another crate) gets the whole surface, not just the helpers tests need.
  2. **Conflates "test helper" with "production internal that tests reach into."** `replication::publication_name` is the latter (internal helper); `exec_begin_with_pool` is the latter (pool-driven shim); `set_db_url_for_tests` is the former (true test helper, has `_for_tests` suffix). The cfg-fork treats them identically.
  3. **A `test_support` module is reversible without touching test code's import sites** if the test_support layer adds prefix harmonisation (e.g. `pub use crate::migrations::exec_begin as migration_exec_begin`).

  Fix: replace the seven cfg-fork pairs with `pub(crate) mod X;` plus a single `#[cfg(feature = "test-helpers")] pub mod test_support { pub use ... }` module that names exactly the helpers tests reach into. Integration tests change `use zeroship_plugin_db::migrations::exec_begin_with_pool;` → `use zeroship_plugin_db::test_support::exec_begin_with_pool;`. The commit message's "no surface gain" claim is true for line count, false for the architectural invariant.

  Alternative path (judgment call): keep the cfg-fork but commit to it explicitly — rename the feature `test-helpers` → `__internal_test_surface` (double-underscore convention, signalling "do not use") and document at the top of `lib.rs:34` that this exposes the full internal surface, not a curated test API. This is cheaper than the `test_support` module but doesn't fix the actual architectural inversion.

  Verification: `lib.rs:62-101` (unchanged since R4 commit `90d992d5`).

  ---

**I3 (carried from R4). `replication.rs`, `auth/*` still on `Result<_, String>`.**

Status: **mixed**. The empty-RETURNING fix in `replication.rs:240-249` (commit `c83d6a8c`) constructs a `DbError::Internal` and then calls `.into_string()` on it to fit the function's `Result<_, String>` signature — confirming the rail is the constraint, not just the call site. Counts:

- `replication.rs`: 7 `pub async fn` signatures returning `Result<_, String>` (lines 82, 98, 103, 148, 327, 424, 509).
- `auth/bootstrap.rs`: 13 sites (lines 52, 169, 187, 225, 272, 316, 347, 414, 494, 599, 630, 696, 965).
- `auth/keys.rs`: 3 sites (lines 54, 86, 113).
- `auth/session.rs`: 6 sites (lines 75, 152, 206, 219, 314, 328).
- `replication_ops.rs`: 4 wrap sites (`DbError::Internal { message: e }`) that depend on the upstream rail.

  Why: architectural impact
  The `replication.rs:243-249` construction is the smoking gun — the typed `DbError::Internal` exists, the typed error rail receives it everywhere else, but the function signature forces `.into_string()` at the boundary. The SDK's `.code` discriminator loses replication-specific classification: a transient FATAL on slot creation reaches JS as `internal` instead of `transient`, the SDK gives up instead of retrying.

  Fix: sweep `replication.rs` (7 fns) and `auth/*` (22 fns) through `?` against the existing `From<compio_postgres::Error> for DbError` impl. Same template that closed `audit.rs` (R3) and `replication_ops` (R3-by-wrapping). Each `.map_err(|e| format!(...))` becomes `?`. Boundary wrappers in `replication_ops.rs:84,118,155,215` become `?` or `e.to_op_error()` directly.

  Verification: `replication.rs:82, 98, 103, 148, 327, 424, 509`; `auth/bootstrap.rs:52, 169, 187, 225, 272, 316, 347, 414, 494, 599, 630, 696, 965`; `replication_ops.rs:84, 118, 155, 215`.

  ---

**I4 (carried from R4). `Backend` trait still half-applied: `migrations.rs` (7 sites) + `register_model/mod.rs::run_pipeline` (1 site) take `&PostgresBackend`.**

Status: **unchanged**. `migrations.rs:211, 355, 440, 467, 638, 679, 712` and `register_model/mod.rs:160` all type their backend arg as `&PostgresBackend` rather than `&impl Backend`. The trait is consumed generically only in `register_model/{plan,validate,apply}.rs` (commit `b94fbdeb` made `apply` generic; `plan` and `validate` already were).

  Why: architectural impact
  The trait's stated mission (`backend/mod.rs:6-14`) is to name the seams before a second backend lands. But `migrations.rs` and `run_pipeline` pin the concrete type, so a future sqlite/planetscale prototype has to either fork the file or chase `&PostgresBackend` casts. `bootstrap.rs:196` even acknowledges this in a doc comment: "If a future refactor lifts `bootstrap()` onto the `Backend` trait..." — the deferral is documented.

  Fix: type the 7 `migrations.rs` fns + `run_pipeline` over `B: Backend`. `register_model/bootstrap::bootstrap` likewise (it already only calls methods on the trait — the concrete type bleeds in through the `lock_client: PooledClient<'p>` return). The compile-time test in `backend/mod.rs:366-444` already pins `B = PostgresBackend`, so no runtime risk.

  Alternative (judgment call): keep the migration lifecycle on `&PostgresBackend` and explicitly mark the file Postgres-only at the top — same shape as `replication.rs` / `wal_consumer.rs` already are. Defensible: pg-advisory-lock + `CREATE INDEX CONCURRENTLY` are PG-only primitives anyway; a sqlite migration backend would not share this file. But then move the trait-generic constraint statement out of `backend/mod.rs:6-14` ("everything the orchestrator and audit-row layer ask of the database") so the trait's scope matches reality.

  Verification: `migrations.rs:211, 355, 440, 467, 638, 679, 712`; `register_model/mod.rs:160`.

  ---

**I5 (NEW R5). Three sibling-pattern clusters are now load-bearing — pattern consolidation overdue.**

The user's r5 prompt called out three patterns:

- **"explicit unlock on Err then drop"** — three sites (bootstrap, mod, apply). This is I1, carried from R4.
- **"subscriber gate before build"** — two sites now: `wal_consumer.rs:548` and `exec.rs:201-205`. Both consult `is_app_suppressed + has_subscribers` (the second is suppressed branch is wal-specific, the autocommit path inverts it). The gate shape is structurally identical.
- **"`ok_or_else` on empty RETURNING"** — two sites: `audit.rs:325-330` (`audit: INSERT returned no row`) and `replication.rs:240-249` (`replication: pg_create_logical_replication_slot returned no row`). Both `rows.first().map(|r| r.get::<_, T>("col")).ok_or_else(|| DbError::Internal { message: "..." })`.
- **"stamp `.app_id` from self, ignore opts override"** — two sites: `v8_classes/db.rs:323-333` (`resolve_consumer_app_id`) and `v8_classes/replication.rs:107-112` (`resolve_setup_app_id`). Both ignore `_opts` and return `stamped.to_string()`; both have parallel unit-test modules (cross-app override rejection).

  Why: architectural impact
  N=2 is the threshold where ad-hoc duplication transitions into a missing primitive — particularly when the patterns are *load-bearing*, each handles a security/correctness/perf invariant, and the duplication recurs across commits (the empty-RETURNING fix in `c83d6a8c` explicitly cited `d7cfc089` in its commit message as the template).

  Sibling-pattern judgment criteria (when to abstract):

  | Pattern | Sites | Architecturally load-bearing? | Cost of next divergence | Decision |
  |---|---|---|---|---|
  | Advisory unlock | 3 | Yes (held-lock leak → cross-app stall) | Fourth lifecycle stage gets it wrong | Abstract — I1 |
  | Subscriber gate | 2 | Yes (perf, was R2 CRITICAL) | Third producer path forgets the gate | Abstract — see fix below |
  | Empty RETURNING | 2 | Yes (silent sentinel data, audit-row id=0 bug) | Next RETURNING-yields-pk call site silently degrades | Abstract — see fix below |
  | App-id stamp | 2 | Yes (cross-app hijack — CVE-class) | Next v8_class entry point forgets to stamp | Borderline — the *helpers* exist already (the abstraction); the duplication is the function shape itself |

  Fix: concrete recommendations per cluster

  - **Subscriber gate.** Promote `has_subscribers` + `is_app_suppressed` into a single helper `should_emit_change(app_id: &str, collection: &str) -> bool` exposed from `crate::broker` (or a new `crate::emit` module). Both producer paths call the same helper. Third path (P8b read-set fingerprint pre-screen) extends the helper.

    ```rust
    // crate::broker (or new crate::emit)
    pub(crate) fn should_emit_change(app_id: &str, collection: &str) -> bool {
        if crate::wal_consumer::is_app_suppressed(app_id) {
            return false;
        }
        has_subscribers(app_id, collection)
    }
    ```

    Callers: `exec.rs:201-205` and `wal_consumer.rs:548` both call `should_emit_change`. Net code: -8 lines, -1 invariant to maintain.

  - **Empty RETURNING.** Add a helper to `crate::error` (or a private `crate::sql_helpers`):

    ```rust
    pub(crate) fn first_row_or_internal<T, F>(
        rows: &[compio_postgres::Row],
        column: &str,
        op_label: &str,
    ) -> Result<T, DbError>
    where T: FromSql<'_> { /* rows.first().map(|r| r.get::<_, T>(column)).ok_or_else(|| DbError::Internal { message: format!("{op_label}: INSERT/RETURNING returned no row") }) */ }
    ```

    Both sites become a one-liner. The third site (next RETURNING-yields-pk call) cannot forget the `.ok_or_else` because the helper signature requires it.

    Alternative path: a typed wrapper `RequiredRow(Row)` constructed from `rows.into_iter().next().ok_or(...)?`. Heavier; the function helper is the smaller surgical fix.

  - **App-id stamp.** This is the borderline one. The two `resolve_*_app_id` helpers ARE the abstraction — they exist precisely so the security policy is unit-testable and so a future contributor restoring opts-override has to delete the helper (with its tests). What's duplicated is the *shape* of each helper, not its body. Defensible argument either way:

    1. **Leave as-is.** Two v8_class entry points, two security-critical guards, each with its own unit-test module. The helpers' independence is a feature: a future change to one doesn't accidentally touch the other.
    2. **Consolidate into a trait method.** `trait AppIdScoped { fn resolve_app_id(&self, _opts: &Value) -> String { self.app_id().to_string() } }` on the v8_class wrappers. Default impl ignores opts; tests assert via `<Db as AppIdScoped>::resolve_app_id`. One unit-test module covers all wrappers.

    I'd recommend (1) — the security-critical-by-deletion property is more valuable than the LOC saving. The pattern's existence is not yet enough signal; a third v8_class with caller-controllable scope would be.

  Verification:
  - Subscriber gate: `exec.rs:201-205`, `wal_consumer.rs:548`.
  - Empty RETURNING: `audit.rs:325-330`, `replication.rs:240-249`.
  - App-id stamp: `v8_classes/db.rs:323-333`, `v8_classes/replication.rs:107-112`.
  - Advisory unlock (= I1): three sites listed in I1.

  ---

**I6 (NEW R5). `auto_tx::exec_auto_begin` and `transaction::exec_begin` are parallel transaction openers — backlog I30 confirmed.**

`orchestrator/auto_tx.rs:152-201` and `orchestrator/transaction.rs:113-173` both:

1. Read the per-isolate db_url.
2. Open a `compio_postgres::connect`.
3. Spawn the connection task as a detached compio task.
4. Run a `BEGIN ...` statement.
5. Install the client into `IsolateDbContext::tx_conn`.
6. `clear_pending_emits()`.

Differences:

- `auto_tx` sets `auto_tx_owned(true)` after install; `transaction` stamps `tx_token` instead.
- Error mapping diverges: `auto_tx` returns `DbError::Transient` for the `compio_postgres::connect` failure (`auto_tx.rs:176-178`); `transaction` likewise (`transaction.rs:149-151`) — actually identical here.
- `auto_tx` doesn't open a wrapper (it's defense-in-depth, owned by `__zsEndAutoTx`); `transaction` mints a `Transaction` v8_class synchronously before the await (`transaction.rs:55`).

  Why: architectural impact
  Two open paths, two error rails, two install-tx-client points. Today they're consistent. The next change that affects "how we open a tx" — e.g. a connection pool variant for short-lived auto-tx, or a SET LOCAL statement before BEGIN — has to land in two places. The deferred-emits behaviour is also coupled: both call `clear_pending_emits()` at install time, but the rationales differ (auto_tx: prior auto-tx cleanup; transaction: defensive). A regression in one drifts from the other silently.

  Fix: extract a `pub(crate) async fn open_tx_session(begin_sql: &str) -> Result<compio_postgres::Client, DbError>` to `crate::context` (or a new `crate::tx_session` submodule) that owns steps 1-3 (URL lookup, connect, spawn task). Each caller runs the appropriate BEGIN, calls `install_tx_client`, and stamps its ownership flag. The shared helper handles the configuration → connect → spawn lifecycle uniformly; the caller-specific bits (which BEGIN, which ownership marker) stay local.

  Alternative path: leave them parallel. The two are read together in code review (both ~50 lines, contiguous in their respective files), and the divergence cost is currently low. Take this finding as a tripwire for the *next* tx-open change: when it lands, refactor first.

  Verification: `auto_tx.rs:152-201`, `transaction.rs:113-173`. Both call `compio_postgres::connect`, both spawn the connection task, both `install_tx_client`, both `clear_pending_emits`.

  ---

### MINOR

**M1 (carried from R4). `validate.rs` returns `Result<_, String>` envelope rail — newtype `ValidationRefusedEnvelope` would clarify.**

Unchanged. `validate.rs:53-57` returns `Result<ApprovedPlan, String>`; `run_pipeline` wraps in `DbError::SchemaRefused`. The `String` semantics ("this is the validation_refused JSON envelope, not an error message") is encoded only in the docstring.

  Verification: `validate.rs:57`, `register_model/mod.rs:204-208`.

  ---

**M2 (carried from R4). `AuditExecutor::query_text` still returns `Result<Vec<Row>, compio_postgres::Error>`.**

Unchanged. The two impls (`for Pool`, `for Client`) carry the driver type; callers re-classify via `coded_db` / `coded_sql`. Trivial change to `Result<_, DbError>` using the existing `From` impl.

  Verification: `audit.rs:431-458` (carried from R4).

  ---

**M3 (carried from R4). `register_model_dispatch` returns `ResolveValue::String("null".to_string())`.**

Unchanged. Constant-time JS work per `registerModel`; the only `ResolveValue::String("null")` in the crate. Swap to `ResolveValue::Null` if the runtime exposes such a variant.

  Verification: `register_model/mod.rs:91` (carried from R4).

  ---

**M4 (carried from R4). `IsolateDbContext` fields remain `pub(crate)`.**

Unchanged. Eleven fields, all `pub(crate)`, all reachable via accessors. The accessor pattern is fully established; field-direct path is dead weight.

  Verification: `context.rs:70-159` (carried from R4).

  ---

**M5 (carried from R4). Six `mint_*` v8_class minters duplicate the boxed-instance + weak finalizer dance.**

Unchanged. `mint_db`, `mint_collection`, `mint_replication`, `mint_migrations`, `mint_migration`, `mint_subscription`, `mint_transaction` — seven `mint_*` functions (R4 said six; `mint_subscription` is the seventh, confirmed in this round's grep). The pattern belongs in `#[v8_class]` itself; flag for `runtime-macros` to absorb.

  Verification: `v8_classes/{db,collection,replication,migrations,migration,subscription,transaction}.rs:*` — seven mint_* functions.

  ---

**M6 (NEW R5). `broker.rs::Debug` impl recomputes `buckets` (sum over inner HashMaps).**

`broker.rs:580` walks `by_key.values()` and sums each inner `len()` to preserve the prior single-level `buckets` field meaning. Tiny — but the inner sum is `O(apps)` for every `Debug::fmt`. Cheap when called from test output / panic prints; surprising if it ever lands on a hot path (tracing field rendering, metrics dumps).

  Why: architectural impact
  None today. Cosmetic. Flag because the broker doc-comment (`broker.rs:386-401`) explicitly cites alloc-free hot paths; `Debug` slipping into a metric serialisation path would silently regress that promise.

  Fix: cache `buckets` on `Broker` as `usize` updated on `subscribe` / `publish` prune. Or rename the field in `Debug` to make the cost visible — `total_collection_keys: O(apps)`. Lowest priority.

  Verification: `broker.rs:576-588`.

  ---

## 4. Direct Answers to the R5 Prompt Probes

**Q: Did the `broker.rs` two-level HashMap refactor change anything architecturally?**

Yes — moderately positive. The shape changed from `HashMap<(String, String), Vec<Sub>>` to `HashMap<String, HashMap<String, Vec<Sub>>>`. Three downstream effects:

1. **Hot-path alloc removed.** The R2 N-C1 fix added `has_subscribers` as a gate; under the tuple-key shape, each `has_subscribers(&str, &str)` call still allocated `(String::from(app), String::from(collection))` to satisfy `HashMap::get`. The two-level layout makes the lookup go through `Borrow<str>` directly. The R2 N-C1 fix was effectively cancelled by its own implementation cost; this refactor finishes the job.
2. **Per-app `drop_app` is now `O(1) + O(apps_collections)`.** Was `O(total_subscriptions)` via tuple-key iteration. Architectural — not just perf — because the WAL consumer's per-app teardown path is bounded by the per-app subgraph instead of the global broker.
3. **P8b read-set fingerprint substrate.** A third level (`HashMap<fingerprint, Vec<Sub>>`) nested under collection is now structurally available with no key-type change.

The only downside is the `Debug` impl recomputing buckets (M6). Net: structurally better, not just a refactor.

**Q: Does the second sibling for empty-RETURNING (`c83d6a8c` mirroring `d7cfc089`) signal a missing helper trait/macro?**

Yes. See I5. Two sites with identical shape:

```rust
let id_or_lsn: T = rows
    .first()
    .map(|r| r.get::<_, T>("col"))
    .ok_or_else(|| DbError::Internal { message: "<op>: returned no row".to_string() })?;
```

The fact that the second commit (`c83d6a8c`) explicitly cited the first (`d7cfc089`) as the template means the pattern is being documented as ad-hoc reuse. A `first_row_or_internal<T>(rows, column, op_label)` helper makes the contract typed: if your `RETURNING` query expects a row and gets none, you get `DbError::Internal` automatically. The third occurrence (which will land, given the codebase's growing surface of `RETURNING` queries) cannot silently degrade.

**Q: When does N=2 warrant a shared abstraction vs. accept the duplication?**

My criteria (from this codebase's experience):

| Signal | Threshold | Action |
|---|---|---|
| Both sites maintain a security/correctness/perf invariant | N >= 2 | Abstract |
| Both sites cite each other in commit messages | N >= 2 | Abstract |
| Pattern is structural (allocation, lifetime, lock) | N >= 2 | Abstract |
| Pattern is shape (signature, error variant choice) only | N >= 3 | Wait |
| Sites diverge between commits | N >= 2 | Abstract |
| Sites would diverge if the pattern's contract changed | N >= 2 | Abstract |
| Abstraction would force coupling that the duplication avoids | any N | Don't abstract |

By this rubric:

- Advisory unlock (3 sites, security-critical invariant, three commits citing each other): **abstract** — I1.
- Subscriber gate (2 sites, perf invariant, commit cited the precedent): **abstract** — I5 sub-fix.
- Empty RETURNING (2 sites, correctness invariant, commit cited the precedent): **abstract** — I5 sub-fix.
- App-id stamp (2 sites, security-critical, each has its own test module): **leave as-is** — the helpers ARE the abstraction; their independence is intentional.

**Q: `auto_tx` vs `transaction.rs` divergence — architectural smell?**

Yes, mild. See I6. The two paths share six structural steps but live in different files with no shared helper. Currently they don't diverge in observable behaviour (modulo the `auto_tx_owned` vs `tx_token` ownership semantics, which are by design). The smell is "the next change to how we open a tx has to land in two places, no compile-time enforcement of consistency."

The fix is small (a shared `open_tx_session` helper for steps 1-3); the cost is low. Whether to do it now or after the next change is a judgment call — I'd defer until the next tx-open change forces the issue. Flag is just enough.

**Q: cfg-fork visibility — still architecturally correct fix?**

Still no, see I2 (carried). The R4 conclusion holds: a `test_support` module is the architecturally-correct fix. R5 brings no new evidence either way; commit `90d992d5` was the only movement, and the architectural inversion (`pub(crate)` modules carrying `pub` shape under a feature flag) is intact.

The pragmatic argument for keeping the cfg-fork ("no test-import rewrite") remains valid, and the fix is non-blocking. But the architectural-correctness statement remains: `test_support` re-export module is the right shape; cfg-fork is the cheaper shim.

**Q: `@zeroship/bootstrap` boundary cleanliness — how does the JS package consume `plugin-db`?**

Clean, at the contract level. The JS surface is exactly the v8_class wrappers:

- `installSchema` (sdks/bootstrap/src/install-schema.ts:362-370) calls `native.registerModel(name, dbSchema, wireIndexes)` via `.call(native, ...)` — the brand check on the wrapper. This is the `Db.registerModel` v8_method registered in `crates/plugin-db/src/v8_classes/db.rs`.
- The native handle (`NativeDb` in TS, `Db` v8_class instance in Rust) is the only JS-side import touchpoint.
- The TS package builds `dbSchema` and `wireIndexes` shapes (`ZeroshipDbSchema`, `ZeroshipDbNamedIndex`) that mirror the Rust-side `serde_json::Value` extraction in `register_model_dispatch`. Mismatch would be caught at the v8_class boundary (`schema` is a `Value` arg, indexes too).

Edge: the TS side uses `(native.registerModel as unknown as ...).call(native, ...)` (`install-schema.ts:364-369`). The unbound-fn form drops `this` and triggers "Illegal invocation" — the comment cites commit `e564c010`. That's a v8_class wrapper-receiver invariant the Rust side enforces; the TS side has to know it. Architecturally clean, but the contract is informal (a comment in TS pointing at a Rust commit).

Fix (low priority): a TS type guard `assertBoundReceiver(native)` in `install-schema.ts` that throws a clearer error than "Illegal invocation" if a future caller drops the receiver. Or a TS helper `callBound(method, native, ...args)` that encodes the `.call` pattern. Either documents the contract in TS.

The architecture itself — `installSchema` is the only orchestrator that touches `native.registerModel`, all CRUD goes through `Collection` wrappers minted in-Rust — is the right shape. The contract is informal but tight. No CRITICAL.

---

## 5. Still Deferred (Carry-Over)

| Item | Origin | Actionability |
|---|---|---|
| `query.rs` 4275 LOC, `build_aggregate` ≈ 211 LOC inline match | R1 | Defer until a real aggregator-extension PR forces the issue |
| Audit table write-only — no `db.audit.*` JS surface | R1 S5 | Low priority |
| WAL cross-tenant isolation is Rust-only | Security R1 | Deferred to P8c SECURITY DEFINER work |
| Migration advisory-lock has no RAII guard (separate from orchestrator's I1) | Security R1 | Same template as I1 once landed |

---

## 6. Overall Score: 83/100

**Trajectory: 64 → 76 → 81 → 82 → 83.**

R5 movement is small (+1 aggregate, +5 on perf, +2 on coupling and error rail, +1 on layering and forward extensibility). Five commits closed two perf CRITICALs (broker tuple-key alloc, per-row tuple build), one correctness gap (replication empty-RETURNING sentinel LSN), one safety invariant (destructive-class hard-error). The new dimension this round — "pattern consolidation" at 48 — reflects four sibling-pattern clusters now active; one (advisory-unlock, 3 sites) is overdue, two (subscriber gate, empty-RETURNING, 2 sites each) are at the abstraction-warranted threshold, one (app-id stamp, 2 sites) is borderline-leave-alone.

The crate's architectural posture is **stable**. None of the remaining issues block shipping; all are "should have been one type / one helper / one re-export module instead of N copies." The velocity on tightening is right: every CRITICAL flagged in R2–R4 has landed within ~24h of identification; the perf wins this round (the broker refactor especially) materialised a clean structural improvement out of what could have been a tactical patch.

**The single highest-value follow-up remains I1** (`OrchestratorLockGuard`) — three sites, three commits citing each other, security/availability invariant. Now joined by **I5** (the two-sibling patterns at the abstraction threshold) — closing both would drop ~30 lines of duplication and pin three invariants at the type level.

I3 (sweep `replication.rs` + `auth/*` from `Result<_, String>` to `Result<_, DbError>`) is the largest mechanical follow-up, ~29 fns. The `replication.rs:243-249` empty-RETURNING fix is the canonical example of why: the typed error exists, the string rail forces `.into_string()` at the boundary, the SDK loses classification.

---

## Relevant Files

- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/lib.rs` — cfg-fork visibility lines 62-101 (I2)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/broker.rs` — two-level HashMap structure (lines 402-468); `has_subscribers` alloc-free probe (lines 460-468); `Debug` impl recomputes buckets (lines 576-588, M6); 6 new unit tests at lines 1224-1295
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/exec.rs` — subscriber gate (lines 201-205, I5 sub-pattern); per-row tuple build inside gate (lines 218-249)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/wal_consumer.rs` — sibling subscriber gate (line 548, I5 sub-pattern); `is_app_suppressed` API (line 124)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/audit.rs` — `first_row_or_internal` sibling shape (lines 325-330, I5 sub-pattern)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/replication.rs` — second `first_row_or_internal` sibling (lines 240-249, I5 sub-pattern); 7 `Result<_, String>` signatures (lines 82, 98, 103, 148, 327, 424, 509, I3); the `.into_string()` boundary at line 248 is the smoking gun
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/apply.rs` — destructive invariant check (lines 62, 155-157, 266-); advisory unlock block 3 (lines 224-230, I1); generic `B: Backend` (line 37)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/bootstrap.rs` — advisory unlock block 1 (lines 179-184, I1); inner-block error-path discipline (lines 140-187)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/mod.rs` — advisory unlock block 2 (lines 219-225, I1); `run_pipeline` still concrete `&PostgresBackend` (line 160, I4)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/auto_tx.rs` — parallel `exec_auto_begin` (lines 152-201, I6)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/transaction.rs` — parallel `exec_begin` (lines 113-173, I6)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/db.rs` — `resolve_consumer_app_id` (lines 322-333, I5 borderline sub-pattern)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/replication.rs` — `resolve_setup_app_id` (lines 106-112, I5 borderline sub-pattern)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/migrations.rs` — 7 `&PostgresBackend` signatures (lines 211, 355, 440, 467, 638, 679, 712, I4)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/backend/mod.rs` — `Backend` trait + compile-time tests (lines 68-356, 366-444); `release_advisory_lock` declared but unused by orchestrator (line 154)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/bootstrap.rs` — 13 `Result<_, String>` signatures (I3)
- `/home/ruiyang/Projects/appbase/sdks/bootstrap/src/install-schema.ts` — JS-side `installSchema` calling `native.registerModel.call(native, ...)` (lines 362-370)
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-architecture-review-2026-05-22-r4.md` — prior round
