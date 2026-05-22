# plugin-db code-quality critique — round 9 (2026-05-22)

**Scope**: `crates/plugin-db/` — 39,605 LOC across 32 .rs files
(20,594 src + ~16,400 tests; `wc -l` over `src/**/*.rs`). Prior rounds
r1–r8. r8 scored 94/100, recommended stopping at 95.

**Lens**: Rust code quality. Fresh re-audit after r8 (cycle 09:00, 94/100).
Four new commits in scope:
- `7d0bc4c5` — exec.rs cold-init code unification (R8-2 *partial* —
  only the code names were unified; the underlying `Result<_,
  String>` return type stayed)
- `389749ca` — backend_not_initialized code unification (twin of R8-2)
- `7bd2187e` — bench harness scaffold (new code; first review pass)
- `757026e3` — 4 doc hold-out closures (preamble drift fixes)

**Method**: Re-ran every regression-baseline grep from r8's appendix.
Walked the bench harness as new code. Sanity-checked the warnings
output (`cargo build -p zeroship-plugin-db`) for dead-code patterns
not visible at grep depth. Confirmed each r8 finding's status —
which closed, which carry forward.

---

## TL;DR

The three doc-only items in `757026e3` closed correctly and r8's
**MAJOR-R8-1 (preamble drift) is now closed**. The two code-unification
commits (`7d0bc4c5`, `389749ca`) closed half of r8's MAJOR-R8-2 — the
SDK now sees one consistent code (`lazy_init_failed`) for cold-init
failures, but `init_pool_async` *itself* still returns `Result<(),
String>` and its two call sites still synthesise `DbError::Configuration`
in two different shapes (one via helper, one via struct literal). So
R8-2 is partly closed but the underlying typed-rail promotion was
deferred.

The new bench harness (`benches/bench_query_build.rs`, 168 LOC) is
clean Rust: well-commented, idiomatic Criterion, no unwrap
surprises. Two micro-points worth noting but neither rises to a
finding.

**One genuinely new finding**: `cargo build` emits **58 unique dead-code
warnings** unique to plugin-db (mostly the `auth/` subsystem: 30+
unused functions/structs/constants). This was present in r8 but
neither r8 nor prior rounds flagged it — grep-based audit misses
dead code that lints catch. The auth/ subsystem appears to be a
work-in-progress staging area that was wired up but never reached
the production path. MAJOR finding because the code is on the
production binary's surface (no `#[cfg(test)]` / feature gate) and
counts toward security audit area without contributing to runtime
behaviour.

**Score: 94/100 (unchanged from r8).** The score is unchanged because:
- one MAJOR closed (R8-1 preamble drift)
- one MAJOR partially closed (R8-2 code names unified, type-rail
  promotion deferred)
- one new MAJOR surfaced (dead `auth/` subsystem)

Net zero. R8's analysis that the crate has reached the "polish >
rewrite" regime is correct. r8's "stop at 95" recommendation still
stands — the dead-code finding below is the only thing keeping r9
from confirming a 95.

---

## Audit dimensions

### 1. RefCell-across-await re-sweep

Re-ran `Grep -n '\.borrow(_mut)?\(\)' crates/plugin-db/src` — 105 hits
across 18 files (up from 85 at r8; growth is in the new bench
harness's *test paths* which don't count, plus the broker/migration
sweep r8 already cleared).

Spot-checked the 5 sites most likely to regress under async refactor:

- **`crud.rs:158, 191, 222, 250, 283, 312, 344, 372, 416, 441, 483,
  518, 557`** — every site is the same pattern: `state.borrow_mut(
  ).spawned_ops.push(Box::pin(...))`. The `RefMut` is the receiver of
  `.push(...)` and drops at end-of-statement. The spawned future
  runs later under its own borrow. **No await held under borrow.**

- **`broker.rs:307-327` (`Subscription::push`)** — borrow held for the
  whole `push()` body including `w.wake()`. `Waker::wake()`
  documentation forbids reentrancy into the originating executor
  during wake, and the broker is single-threaded compio. **r8
  resolved as correct; unchanged.**

- **`v8_classes/migration.rs:160-169, 187, 204, 222`** — `coords.
  borrow()` always cloned out (`.as_ref().cloned()`), borrow drops at
  `;`, then `ensure_backend().await` runs. Verified.

- **`context.rs::with` / `with_mut`** at `:471, :477` — synchronous
  closures only; the `f` parameter cannot be `async` because
  `FnOnce(&IsolateDbContext) -> R` returns by value, and no caller
  passes a future-builder. **Type-system-enforced safety.**

- **`read_set.rs:369-388`** — `record_if_active` calls `is_active()`
  (borrow drops at `;`) THEN `current_kind()` (no borrow) THEN
  `c.borrow_mut().as_mut()` (separate borrow). Sequential, not
  nested.

**Verdict**: zero RefCell-across-await sites. Sweep clean (same as
r8).

### 2. Unsafe count

`Grep -n '\bunsafe\b' crates/plugin-db/src` — 18 hits across 7 files
(same as r8). Every site falls into one of three patterns r8
catalogued:

1. **Weak finalizer drop-Box pattern** at
   `v8_classes/{db,collection,migration,migrations,replication,
   subscription,transaction}.rs` — `Box::from_raw(raw_addr as *mut
   T)` inside the finalizer closure passed to
   `Weak::with_guaranteed_finalizer`. SAFETY comments correctly cite
   the `Box::into_raw` → finalizer one-shot drop invariant.
2. **Pointer-recovery for async state transition** at
   `migration.rs:427, 690` — `unsafe { &*(addr as *const Migration)
   }` after `.await` to flip the wrapper's `inner` field. SAFETY
   citations include the `Global` capture pinning the wrapper and
   the macro rejecting `&mut self` async (so only `&` is recovered).
3. **Module-level `#![allow(unsafe_code)]`** in the 7
   v8_classes files — gates the lint for the finalizer/recovery
   patterns above.

No raw pointer arithmetic. No `mem::transmute`. No lifetime
laundering.

The bench harness has no `unsafe`. Confirmed.

**Verdict**: unchanged from r8. Surface is minimal, every site is
well-justified.

### 3. Panic risks — unwrap / expect / indexing

**Unwraps** (`Grep -n '\.unwrap\(\)' crates/plugin-db/src` — ~200 hits).
Sampled the non-test sites:

- **V8-OOM idiom**: `v8::String::new(...).unwrap()`,
  `v8::PromiseResolver::new(scope).unwrap()`, `v8::Function::new(...
  ).unwrap()`. Aborting the worker on OOM is correct behaviour.
- **`v8_bridge.rs:217`** — `v.try_into().unwrap()` guarded by `if
  v.is_array()` precondition.
- **`v8_bridge.rs:414, 437`** — `from_be_bytes(bytes.try_into().
  unwrap())` guarded by `bytes.len() == 8` / `== 4` match arm.
  Length-checked.
- **`query.rs:694`** — `String::from_utf8(out.to_vec()).expect(
  "ALPHABET is ASCII")` where ALPHABET is a `&[u8; 32]` constant of
  ASCII bytes. Provably safe.
- **`exec.rs:100, audit.rs:314`** — `row.get::<_, i64>("count")` /
  `row.get::<_, i64>("id")`. Not panic-free in general; safe-by-
  construction because the surrounding SQL always uses `SELECT
  COUNT(*) AS count` or `RETURNING id`. Same pattern as r6 closed
  via `first_row_or_internal`; the per-column lookup is the same
  shape one layer down (see [MINOR-R9-3] for a possible extension).

**Expects** (38 production sites). Spot-checked:
- `v8_bridge.rs:95` — `expect("RuntimeState not in isolate slot")`.
  Invariant documented; runtime always installs slot pre-JS.
- `lock_guard.rs:208` — `.expect("OrchestratorLockGuard::into_held
  called on guard with no client")`. r8's MIN-R8-5 (dead-code on
  `into_held()`); still unchanged. Marked `#[allow(dead_code)]`.
- `error.rs:567, 685, 699` — all `#[cfg(test)]` inside tests module.

**Indexing**: 0 instances of unchecked `[i]` outside test code or
length-checked arms. Same as r8.

**New: bench harness panic surfaces.** `benches/bench_query_build.rs:
131, 156` use `.expect("build_find should succeed on benchmark
fixture")`. These are bench-only and the panic message is
informative. Correct pattern for bench code (a panic during bench
should be loud, not silent).

**Verdict**: panic surfaces are well-defended. Every production
unwrap is either a V8-OOM idiom or a length/precondition-guarded
arm. **No findings.**

### 4. Error-handling typing — Result<_, String> count

r8's preamble at `error.rs:9-23` was updated by commit `757026e3`
(the docs hold-out closure). The preamble now enumerates **5
categories** with the correct file:line citations:

1. validate envelope (1 site)
2. Pure ASCII parsers in auth/session.rs (2 sites)
3. JS-input arg parsers in v8_classes (3 sites)
4. Cold-init in lib.rs (1 site)
5. Test helpers — `#[cfg(any(test, feature = "test-helpers"))]`-gated

Verified count: `Grep -nE 'Result<[^,>]+,\s*String\s*>'` returns 7
production-code sites (init_pool_async + validate + hex_decode +
hex_nibble + 3 parse_* helpers) and 1 cfg-gated test helper
(`exec_mutation_with_emit_for_tests`). **Preamble enumeration
matches reality.** r8's MAJOR-R8-1 is **closed**.

**[MAJOR-R8-2 carried forward as MAJOR-R9-1] `init_pool_async` still
on `Result<_, String>`; the typed-rail promotion was *not* part of
the unification commit `7d0bc4c5`.**

`lib.rs:351`:
```rust
pub async fn init_pool_async() -> Result<(), String> {
    ...
    .map_err(|e| {
        let mut msg = format!("db: failed to connect: {e}");
        let mut cur: &dyn std::error::Error = &e;
        while let Some(src) = std::error::Error::source(cur) {
            msg.push_str(&format!(" — caused by: {src}"));
            cur = src;
        }
        msg
    })?;
```

Inline source-chain walker duplicates `error::walk_pg_chain` from
`error.rs:201-216`. Two call sites still synthesise the typed
variant in two different shapes:

- `exec.rs:64, :317` — `DbError::config("lazy_init_failed", format!(
  "db: lazy init failed: {e}"))`. Uses the helper.
- `orchestrator/register_model/mod.rs:117-124` — `DbError::
  Configuration { code: "lazy_init_failed", message: format!(...),
  hint: None }`. Uses struct-literal.

After `7d0bc4c5` the `.code` is consistent (`"lazy_init_failed"`
everywhere). The remaining drift is **construction shape**, not the
SDK-facing code. That's a smaller delta than r8 found, but still
non-zero.

```
Why: The duplicated source-chain walk is dead-weight (`walk_pg_chain`
     already does this); the helper-vs-literal inconsistency
     shouldn't survive a single grep. The promotion to
     `Result<(), DbError>` removes ~16 LOC and the inconsistency in
     one stroke. The `hint` field added in `f1c5184e` is the missing
     piece this finally enables ("ensure DATABASE_URL points at a
     reachable Postgres and the cluster is up").
Fix: Promote `init_pool_async` to `Result<(), DbError>`:
     pub async fn init_pool_async() -> Result<(), DbError> {
         let url = context::with(|c| c.db_url());
         let Some(url) = url else { return Ok(()); };
         let pool = Pool::connect(&url, 8)
             .await
             .map_err(|e| DbError::config_hinted(
                 "lazy_init_failed",
                 format!("db: failed to connect: {}",
                     crate::error::walk_pg_source_chain(&e)),
                 "ensure DATABASE_URL points at a reachable Postgres",
             ))?;
         ctx_mut(|c| c.set_pool(Rc::new(pool)));
         Ok(())
     }
     Then both `exec.rs::ensure_pool` and `register_model/mod.rs`
     just do `crate::init_pool_async().await?;` — 16 LOC deleted,
     drift closed, hint surfaced to SDK consumers.
Verification:
  Grep -n 'pub async fn init_pool_async' crates/plugin-db/src/lib.rs
  Grep -nE 'lazy_init_failed|init_pool_async\(\)\.await' crates/plugin-db/src
```

### 5. Lifetimes — new patterns from cycle

No new patterns introduced since r8. The bench harness's lifetime
surface is trivial (`Vec<(&'static str, Value)>` for workloads;
constants for `app_id` / `collection`).

**No findings.**

### 6. Idiomatic patterns — match vs if let, ? propagation

r8's [MIN-R8-3] `runtime_state` consolidation half-done — **still
open**. Re-verified `Grep -n 'get_slot::<SharedState>()'`:

```
crates/plugin-db/src/v8_bridge.rs:94        ← consolidator definition
crates/plugin-db/src/orchestrator/auto_tx.rs:49
crates/plugin-db/src/orchestrator/auto_tx.rs:85
crates/plugin-db/src/v8_classes/migration.rs:336
crates/plugin-db/src/v8_classes/migration.rs:606
crates/plugin-db/src/v8_classes/migrations.rs:139
```

5 sites bypass the helper. Same count as r8. **No churn — flag forward
as [MIN-R9-2].**

**[MINOR-R9-2] `runtime_state` consolidator still only used by 5 of
~10 callsites.** Same fix as r8: replace the 5 open-coded patterns
with `let state = v8_bridge::runtime_state(scope);`. The
helper's own docstring claims it's "consolidated here to avoid
copy-pasting the expect" — and 5 sites don't.

### 7. Resource lifecycle — Drop impls

7 Drop impls (same as r8). The bench harness adds none.

**[MAJOR-R8-4 carried forward as MAJOR-R9-3]
`OrchestratorLockGuard::release()` still flips `released = true`
even when the unlock SQL erred.**

`lock_guard.rs:144-183` unchanged since r8. The warn-log at `:172-178`
gives observability, but the structural state (`released = true`
after a confirmed-failed unlock) is unchanged:

```rust
if let Err(e) = client.query_text_params(unlock_sql, &[...]).await {
    tracing::warn!(... "pg_advisory_unlock failed");
}
self.released = true;            // ← flips even on Err
Ok(self.client.take())
```

Same analysis as r8's MAJOR-R8-4:
- `Drop` sees `released = true` and stays silent; the "leak:"
  diagnostic at `:227-237` doesn't fire on a confirmed-failed
  unlock.
- The `Result<Option<PooledClient<'p>>, DbError>` return type
  promises an error rail; the `Err` arm is unreachable.

Tristate (r7 recommended, r8 recommended, still right call):
`released_state: { Pending, Confirmed, Failed }`. Drop logs leak
on `Pending | Failed`, suppressed on `Confirmed`. One byte, one
enum, clean contract.

### 8. Type ascription — readability

No new helpers. r8's [MIN-R8-6] (`config_hinted` 1-of-9 adoption) is
**unchanged** — still 1 production site.

`Grep -nE 'DbError::Configuration\s*\{' crates/plugin-db/src` returns
8 struct-literal sites; `DbError::config_hinted(` returns 1
production hit (wal_consumer.rs); `DbError::config(` returns 7 sites.
Same numbers as r8.

**[MIN-R9-4] `config_hinted` adoption still patchy.** Carried from
r8.

### 9. bench harness code quality (`benches/bench_query_build.rs`)

168 LOC. New file in `7bd2187e`. Reviewed as production-grade Rust
(it's executed by `cargo bench`, not run in a worker, so the
audit lens is "is this idiomatic / will it survive Criterion API
churn"):

**Good**:
- Documentation block at the top is honest about the constraint
  (`row_to_json` can't be benched externally because
  `compio_postgres::Row::new` is `pub(crate)`) and proposes a
  forward path. This is the kind of comment that survives a year
  of churn intact.
- Three workload shapes (`empty` / `small` / `complex`) cover the
  realistic SDK distribution; the data shapes are matched to the
  existing `t.string()` SDK conventions (`createdAt` ISO string,
  ULID-shaped `id`).
- Idiomatic Criterion 0.5 API: `iter_batched_ref` with
  `BatchSize::SmallInput` is the right call for sub-microsecond
  routines (the alternative `iter()` over-counts setup overhead).
- `black_box(built)` defeats DCE on the built query.
- `measurement_time(3s)` / `warm_up_time(1s)` are explicit choices
  with reasonable bench latency. Default would be 3s+3s.
- `[lints] workspace = true` in Cargo.toml means the bench inherits
  the workspace lint set; no lax bench-only allow patterns.

**Minor observations** (not findings):

- **(N1) `iter_batched_ref` setup clones `filter` per batch.**
  `build_find` takes `&Value`, not `&mut Value`. The `&mut I`
  routine parameter from `iter_batched_ref` auto-reborrows to `&I`
  at the call site, but the setup closure still allocates one
  `Value::clone()` per batch. Criterion excludes setup from the
  measured time, but on `complex_filter()` (which has nested
  Object/Array) the clone is non-trivial; if the bench grows to
  bigger filter shapes the setup cost could dominate batch latency
  and skew Criterion's internal scheduling. **Possible swap**:
  `iter_with_setup` or `iter` over a single owned `Value` if the
  routine is provably read-only (which it is — `build_find` takes
  `&Value`). Cosmetic.

- **(N2) Closure parameter shadows outer binding.** L114 destructures
  `(name, filter)` from the workloads vec; L121's `|filter|`
  shadows the outer `filter`. Renaming to `|f|` or `|input_filter|`
  would help a future reader. Cosmetic.

- **(N3) The `Vec<_>` construction at L104 could be a
  `&[(&str, fn() -> Value)]` table.** Each `empty_filter()` /
  `small_filter()` / `complex_filter()` is called once at setup
  time; the Vec lives for the duration of the bench. Pure style;
  no perf impact. Cosmetic.

**Verdict**: bench harness code quality is high. No production
issues. The three N-items above are cosmetic.

---

### 10. New finding — dead-code subsystem

**[MAJOR-R9-5] The `auth/` subsystem is dead code (production
binary surface).**

`cargo build -p zeroship-plugin-db` emits **58 unique warnings**
specific to plugin-db. The vast majority are unused items inside
`crates/plugin-db/src/auth/`:

```
warning: function `bootstrap_initial_hmac_key` is never used
warning: function `civil_from_days` is never used
warning: function `classify_detail_token` is never used
warning: function `classify_p0001_detail` is never used
warning: function `coded_sql` is never used                     ← auth/bootstrap.rs
warning: function `create_role_if_missing` is never used
warning: function `current_key_id` is never used
warning: function `ensure_admin_schema` is never used
warning: function `ensure_hmac_keys_table` is never used
warning: function `ensure_nonces_table` is never used
warning: function `ensure_session_ctx_table` is never used
warning: function `format_unix_millis` is never used
warning: function `getrandom_or_fallback` is never used
warning: function `hex_decode` is never used                    ← in r8's "hold-out"!
warning: function `hex_encode` is never used
warning: function `hex_nibble` is never used                    ← in r8's "hold-out"!
warning: function `init_session` is never used
warning: function `install_const_eq_function` is never used
warning: function `install_init_session_function` is never used
warning: function `install_reset_session_function` is never used
warning: function `install_rotate_keys_function` is never used
warning: function `install_sign_session_function` is never used
warning: function `install_slot_wrapper_functions` is never used
warning: function `install_verify_signature_function` is never used
warning: function `iso_timestamp_after` is never used
warning: function `mint_and_init` is never used
warning: function `mint_and_init_via_pool` is never used
warning: function `mint_session_token` is never used
warning: function `previous_key_id` is never used
warning: function `rotate_session_keys` is never used
warning: constant `ADMIN_SCHEMA` is never used
warning: constant `APP_ROLE_TEMPLATE` is never used
warning: constant `DEFAULT_TOKEN_TTL_SECS` is never used
warning: constant `NONCE_RETENTION_SECS` is never used
warning: constant `PLATFORM_ROLE` is never used
warning: struct `BootstrapOutcome` is never constructed
warning: struct `MintedToken` is never constructed
warning: struct `RotationOutcome` is never constructed
warning: struct `SessionInit` is never constructed
warning: unused imports: `BootstrapOutcome` and `ensure_admin_schema`
warning: unused imports: `MintedToken`, `SessionInit`, ...
warning: unused imports: `RotationOutcome` and `rotate_session_keys`
```

`auth/{bootstrap,keys,session}.rs` total ~2,860 LOC. Of that, the
warnings indicate roughly 30+ public items are unreachable from
the production code path. The `auth/mod.rs` re-exports them
(`pub use bootstrap::{BootstrapOutcome, ensure_admin_schema, ...}`)
but nothing in plugin-db itself or the rest of the workspace
imports them — confirmed via `Grep -rn "use zeroship_plugin_db::
auth"`.

The most surprising sub-finding: **r8's "documented hold-outs"
`hex_decode` and `hex_nibble` are dead code.** r8's preamble
classified them as "ASCII parsers internal to auth/session.rs"
that justify the `Result<_, String>` rail. They're not internal —
they're unreachable. The rail-justification is then circular: the
hold-outs are documented because they exist, and they only exist
because nothing has reached them yet.

Other warnings outside `auth/`:
- `read_set.rs:317, 324` — `struct Active` and its `begin` / `take`
  methods. Documented as "RAII guard for thread-local set up by
  the runtime's query-dispatch entry path" but no production
  callsite (runtime imports are the only candidate; not present).
- `v8_bridge.rs:274` — `setup_promise` (the JSON-resolution
  variant) is never used; the `setup_js_promise` variant is the
  current canonical entry.
- `wal_consumer.rs:133, 175` — `any_app_suppressed`,
  `set_local_emit_suppressed` are dead helper functions.
- `diff.rs:101` — variant `DropIndex` of an enum is never
  constructed.
- `diff.rs:149-174` — fields `pg_type`, `not_null`, `default_expr`,
  `default_volatility`, `is_unique`, `columns`, `is_valid`,
  `column`, `target_column`, `deferrable` are never read (only
  populated for Debug/serialization purposes; if they're load-bearing
  for the diff, the lint is right; if they're for future use, they
  belong behind `#[cfg(feature = ...)]`).
- `audit.rs:85` — variants `Validation`, `Backfill` of a status
  enum never constructed (the proposal A3 backfill path is
  documented as future work in `757026e3`'s comment update).

```
Why: Production binary carries dead code that contributes:
     (a) Compile-time noise: 58 unique warnings drown out real
         signal; a future genuine warning gets lost in the volume.
     (b) Security audit area: every unused function in a security-
         sensitive module (auth/keys/session/bootstrap) is one
         more piece of code an auditor must analyse, then
         conclude "unreachable" — repeatedly. The auditor's
         confidence in unreachability is lower than the linter's.
     (c) Misleading rail classification: r8's "ASCII parser
         hold-outs" justification for `Result<_, String>` is
         circular if the parsers are themselves dead.
     (d) Maintenance debt: the dead helpers reference imports
         from compio-postgres, the backend trait, the audit
         module — every refactor in those areas must also rebase
         the dead code, or accept a slowly-rotting branch.
Fix: Three options in increasing intrusiveness:
     (a) `#[allow(dead_code)]` blanket on the auth/ module with
         a load-bearing doc comment ("auth subsystem is staged
         pre-integration; see proposal X"). Cheapest; doesn't
         fix the audit-area concern.
     (b) Feature-gate the entire auth/ subsystem behind
         `#[cfg(feature = "auth-staging")]` so the production
         binary doesn't carry it. Medium; preserves the work
         and removes the binary footprint.
     (c) Delete the dead items and resurrect from git when a
         caller appears. Most aggressive; preserves the rest.
     My recommendation: (b) for the auth/ subsystem (it's
     coherent, large, and clearly staging); (c) for the
     scattered single-item warnings in diff.rs / wal_consumer.rs /
     v8_bridge.rs / read_set.rs.
Verification:
  cargo build -p zeroship-plugin-db 2>&1 \
    | grep -B1 "/plugin-db/" | grep "^warning: " | sort -u | wc -l
  # Expect: 58 (current). Target after fix: <10.
```

This finding is MAJOR (not CRITICAL) because the dead code is
syntactically valid Rust and the binary still functions. It's not
MINOR because: the volume drowns real warnings, the auth subsystem
is security-sensitive, and it directly invalidates one of r8's
documented `Result<_, String>` justifications.

---

## Verification matrix — what each commit closed

| Commit | Closed | Partially closed | Not closed |
|---|---|---|---|
| `7d0bc4c5` exec.rs cold-init unify | SDK sees one `.code` for cold-init failures | r8 MAJOR-R8-2 (typed-rail promotion still pending) | — |
| `389749ca` backend_not_initialized | r8 MAJOR-R8-1's twin (`v8_classes/migration.rs::ensure_backend`, `v8_classes/migrations.rs::dispatch_by_spec`) | — | — |
| `7bd2187e` bench scaffold | — | — | n/a — new code |
| `757026e3` docs holdouts | r8 MAJOR-R8-1 (preamble drift) | — | — |

**Carried forward from r8**:
- MAJOR-R8-2 → MAJOR-R9-1 (`init_pool_async` still `Result<_, String>`)
- MAJOR-R8-4 → MAJOR-R9-3 (`release()` tristate)
- MIN-R8-3 → MIN-R9-2 (`runtime_state` consolidation)
- MIN-R8-5 (`into_held` dead code) — still open, lower priority
- MIN-R8-6 → MIN-R9-4 (`config_hinted` adoption)
- MIN-R8-7..10 — all still open, all rare-path / cosmetic

**New in r9**:
- MAJOR-R9-5 (auth/ subsystem dead code; ~58 warnings)

---

## Score

**94/100 (unchanged from r8).**

| Dimension | r7 | r8 | r9 | Notes |
|---|---|---|---|---|
| Correctness | 95 | 95 | 95 | No new gaps. The `release()` unlock-confirmation gap is the lone open question (MAJOR-R9-3). |
| Performance | 90 | 90 | 90 | No regressions. Bench harness now exists to *catch* regressions. The MIN-R8-7/8/10 rare-path allocs unchanged. |
| Security | 95 | 95 | 92 | -3 for the dead `auth/` subsystem. Not a vulnerability (the code is unreachable), but a security-audit area liability and one of the points r8's hold-out justification was built on. |
| API Design | 95 | 95 | 95 | The `hint` field is well-placed; `config_hinted` adoption is patchy (MIN-R9-4) but not load-bearing. |
| Rust Idioms | 93 | 96 | 96 | `coded_db` consolidation, structural SQLSTATE checks. The preamble drift from r8 closed at 757026e3. |

Average: 94.6 → rounds to 94 (vs r8's 94).

The Security -3 is offset by Rust Idioms staying at 96 (the
preamble drift closure was a doc-discipline contribution, not a
code-quality lift); the rounded average held.

### Comparison vs r8

| | R8 (94) | R9 (94) | Delta |
|---|---|---|---|
| CRITICAL | 0 | 0 | — |
| MAJOR open | 3 (R5-5 remnant, R8-1 preamble, R8-2 init_pool) | 3 (R9-1 init_pool partial, R9-3 release, R9-5 dead code) | -1 closed (R8-1), -0.5 partial (R8-2 → R9-1), +1 new (R9-5) |
| MAJOR closed since prior round | 2 | 1.5 | r8's R8-1 fully closed; R8-2 half-closed (code unified, type not) |
| MINOR open | 8 | 8 | unchanged net (R8 list unchanged; bench harness adds no minors) |
| Structural / contract tests | 13 | 13 | unchanged |
| `Result<_, String>` undocumented sites | 4 | 0 | -4 (preamble now enumerates all 7 production sites) |
| `cargo build` warnings (plugin-db unique) | not measured | 58 | new metric |

### At what point is further investment counter-productive?

r8 said "stop at 95". r9 confirms the recommendation **with one
amendment**: close R9-5 (dead-code) first, then stop at 95.

Rationale: R9-5 isn't a polish item. It directly invalidates the
hold-out justification r8 documented; an audit at r10 would
re-discover it. The fix is mechanical (feature-gate the auth/
module, or delete the scattered dead items). After that the crate
is:

- 95+/100 by every measure
- zero CRITICAL / production-blocking issues
- complete SQLSTATE-typed classification
- bench harness in place to catch regressions
- well-curated unsafe surface
- 392 passing tests

The remaining MAJORs (R9-1 typed-rail completion, R9-3 tristate
release) are both genuinely worth fixing (each affects SDK or
operator-facing contracts), but the marginal LOC-per-quality-point
ratio after that turns sharply negative. The next code-critique
round would surface findings indistinguishable from style
preference. **Honest call: close R9-5 + R9-1 + R9-3, freeze, move
to architectural review.**

---

## Verification commands (regression baseline)

```sh
# RefCell-across-await — expect ~105 hits, every site sync or
# borrow-drops-before-await
Grep -nE '\.borrow(_mut)?\(\)' crates/plugin-db/src

# Unsafe — expect 18 hits across 7 files
Grep -n '\bunsafe\b' crates/plugin-db/src

# Production panic surfaces — every hit should be V8-OOM idiom OR
# guarded-arm precondition.
Grep -nE '^\s*[^/].*\.unwrap\(\)' crates/plugin-db/src \
  | grep -v '#\[cfg(test)\]'

# Result<_, String> — expect exactly 7 production sites:
#   init_pool_async (1, R9-1 to promote)
#   validate (1, documented wire-contract)
#   hex_decode / hex_nibble (2, see R9-5 — currently dead)
#   parse_commit_spec / parse_spec / parse_name_and_collection (3,
#     documented V8-input parsers)
Grep -nE 'Result<[^,>]+,\s*String\s*>' crates/plugin-db/src

# Drop impls — expect 7
Grep -nE '^impl.*Drop\s+for' crates/plugin-db/src

# SQLSTATE rail (R7/R8 cleared)
Grep -nE 'contains\("[0-9P]{5}"\)' crates/plugin-db/src
# Expect: 0 hits.

# get_slot::<SharedState> open-coding (MIN-R9-2)
Grep -n 'get_slot::<SharedState>()' crates/plugin-db/src
# Expect: 1 hit (v8_bridge.rs:94 definition). Target: 1 after the
# MIN-R9-2 sweep.

# Dead-code warning count (MAJOR-R9-5)
cargo build -p zeroship-plugin-db 2>&1 \
  | grep -B1 "/plugin-db/" | grep "^warning: " | sort -u | wc -l
# Current: 58. Target after R9-5 fix: <10.

# Bench compiles
cargo bench --no-run -p zeroship-plugin-db --bench bench_query_build
# Expect: clean compile, one binary produced.
```
