# plugin-db: Migration / DDL Pipeline Correctness Review (R10)

**HEAD:** `2d34061e` (post-r9 cycles 09:40 → 11:17) · **Date:** 2026-05-22 · **Reviewer:** fresh lens
**Prior rounds:** r1 (72) · r2 (81) · r3 (81) · r4 (82) · r5 (83) · r6 (84) · r7 (85) · r8 (85) · r9 (85/100)

---

## 0. Scope of this round

Migration-pipeline-relevant delta since r9 (`757026e3` → `2d34061e`).
Six commits touched `crates/plugin-db/`:

| Commit | Module | Pipeline relevance |
|---|---|---|
| `bed655c1` | `replication.rs` (docstring) | Test-name fix in a docstring. Not pipeline-adjacent. |
| `2fa9472e` | `Cargo.toml`, `lib.rs` | `hardening` feature gate on `auth/*`. **Auth subtree is orthogonal to the migration pipeline** — verified below. |
| `5d9acab8` | `context.rs:362-413` | `set_mig_lock` shadow-replace logs at `error`; `return_mig_client` empty-slot logs at `warn`. **Touches mig_lock slot lifecycle.** Brief asks us to audit this directly. |
| `403b3891` | `query.rs` | `validate_field_name` rejects non-ASCII. Query layer; not pipeline state-machine. |
| `4cab871a` | (3 files, doc/visibility) | Pure non-semantic cleanup per commit log. |
| `ae5570dc` | `exec.rs` (new tests) | Unit tests for `queue_or_emit` / `drain` / `clear`. Tx-state machine, not migration. |

Verified pipeline core file diff is empty:

```
$ git diff 757026e3..HEAD -- \
    crates/plugin-db/src/migrations.rs \
    crates/plugin-db/src/audit.rs \
    crates/plugin-db/src/orchestrator \
    crates/plugin-db/src/backend/postgres.rs
(empty)
```

The only migration-pipeline-adjacent code change in the window is the
`5d9acab8` `mig_lock` slot-drift tracing pair (29 LOC of inserts /
restructures plus docstrings in `context.rs`). The remaining 200 LOC
diff sits in `exec.rs` (tx-state-machine tests), `query.rs` (field
validation), `replication.rs` (docstring), `Cargo.toml` (feature
gate), and `lib.rs` (gate plumbing).

Net structural change to the migration pipeline this round: **near-
zero** — one observability hook on the mig_lock slot, no state-machine
or audit-row changes.

Re-audit dimensions per the brief: **F1**, **F2**, plus the two
commits called out explicitly (`5d9acab8`, `2fa9472e`) and a re-walk
of strictness mode terminal coherence.

---

## 1. Audit dimensions — re-verified at HEAD (`2d34061e`)

### 1.1 F1 (orphan `Running` DDL audit rows) — OPEN, 9+ cycle carry

Unchanged. Five `let _ = backend.update_audit_status(...).await;`
sites verified at HEAD:

| # | File:line | Site |
|---|---|---|
| 1 | `orchestrator/register_model/apply.rs:163-170` | DDL Applied transition |
| 2 | `orchestrator/register_model/apply.rs:178-186` | DDL Failed transition |
| 3 | `backend/postgres.rs:478-486` | CIC `invalid_index_landed` Failed |
| 4 | `backend/postgres.rs:519-528` | CIC `data_violation` Failed |
| 5 | `backend/postgres.rs:558-567` | CIC `transient_retry` / `non_transient_failure` Failed |

DDL `write_audit_row` INSERT (`audit.rs:283-316`) still omits
`owner_session_id` and `last_heartbeat_at`. Backfill rail
(`audit.rs:525-545` / `561-602`) DOES stamp `pg_backend_pid()::text +
NOW()`. **No movement** since r2. **One-liner status:** still open;
zero code-path change in r9 → r10 window on these five sites.

### 1.2 F2 (orphan validate-refused `Pending`) — OPEN, 9+ cycle carry

Unchanged. `validate.rs:66-95` writes `InitialStatus::Pending` per
destructive op; strict path returns `Err(envelope)` at line 90-92
without flipping the row; lenient path falls through with destructive
ops retained in `plan.ops` (apply skips them at lines 203/233 — the
Pending row is never re-reached). Status CHECK at `audit.rs:218-220`
still:

```sql
status IN ('pending','running','applied','applied_with_dead_letter',
           'failed','cancelled','rolled_back')
```

— missing `validation_refused`. **No movement.** **One-liner status:**
still open; zero code-path change in r9 → r10 window on validate.rs
or the status CHECK.

### 1.3 Other carry-forward findings — unchanged

| Tag | Site | Status |
|---|---|---|
| R9-M1 | `audit.rs:525-545` `set_backfill_running` lacks gen predicate | unchanged |
| R9-M2 | `audit.rs:707-736` cursor monotonicity SDK-trusted | unchanged |
| R9-M3 | `lock_guard.rs:212-240` Drop-path leak | unchanged |
| R9-M4 | `postgres.rs:459-462` `pg_index` interpolation | unchanged |
| R9-M5 | `exec_cancel` strands worker's lock client | unchanged |
| R9-M6 | `postgres.rs:593-600` CIC fallback variant divergence | unchanged |

Every byte of the carry-forward sites is identical to r9.

---

## 2. The two commits the brief asks about

### 2.1 `5d9acab8 plugin-db/context: surface mig_lock state drift via tracing (I23)`

**Diff:** `context.rs:362-413` — `set_mig_lock` adds an `if let
Some(prev) = ...` shadow-replace branch that emits `tracing::error!`
with `prev_name`, `prev_audit_id`, `new_name`, `new_audit_id`, then
proceeds with `self.mig_lock.replace(lock)`. `return_mig_client`
converts the silent `if let Some(lock) = ...` no-op into a `match`
whose `None` arm logs `tracing::warn!`.

**Does it help diagnose stuck migrations?**

Yes, partially. Walk the call graph:

- **`set_mig_lock`** has one production caller:
  `migrations.rs:328-333`, immediately after `has_mig_lock` is checked
  at line 228 (returns `migration_already_active` if true). So the
  shadow-replace branch can only fire when the begin path is re-entered
  on the same isolate **between** the line-228 check and the line-329
  set — i.e. concurrent re-entry on the same V8 isolate (impossible
  given single-threaded V8) **or** a non-clearing failure path that
  left `mig_lock = Some(...)` from a prior run (a real bug). The
  `tracing::error!` makes this exact bug-class observable: previously
  it would have been a silent overwrite that operators would never
  catch except via "audit rows are stuck on the prior `name`/`audit_id`"
  post-mortem analysis.
- **`return_mig_client`** has one production caller:
  `migrations.rs:191-197` (`return_mig_client` wrapper used by
  `exec_fetch_batch` and `exec_commit_batch` after their awaits).
  The empty-slot arm fires when `clear_mig_lock` ran during the await
  (operator-cancel race per the docstring; also lib.rs:245 reaping on
  isolate drop). Previously: silent drop of the `Client` (and the
  PG-side advisory lock auto-releases when the backend session closes,
  i.e. on pool recycle). Now: `tracing::warn!` flags the event so
  operators can correlate against a cancel timestamp.

**Diagnostic value for "stuck migrations":**

- Direct: an operator-side log scan for `"set_mig_lock called while
  another lock is active"` immediately identifies the bug class
  (begin path not gating). Previously invisible.
- Indirect: a `"return_mig_client: mig_lock slot empty"` warn paired
  with an audit row in `cancelled` status confirms the expected
  cancel-race; a warn **without** a matching cancel confirms a
  state-machine bug (clear_mig_lock fired without setting status).

**Caveats:**

- The shadow-replace path **still proceeds with `replace`**. The
  prior lock's parked `Client` (if any was held in `lock.client`) is
  silently dropped on `replace`. That `Client` carries a session-
  scoped advisory lock (`pg_advisory_lock(zs_mig:<app>, <name>)`)
  that releases only on backend session close (pool recycle). The
  `error` log gives operators *a way to know it happened*, but the
  bug is not self-healing — the orphaned advisory lock can block the
  next `register_model` / migration for that `(app, name)` pair for
  the pool recycle interval.
- The `error`/`warn` distinction is sound: shadow-replace is a code
  bug; empty-slot return is an expected operator race.
- The `migration_already_active` gate at `migrations.rs:228` is the
  primary defence. The new tracing is the secondary "if we ever miss
  a gate" tripwire.

**Verdict on `5d9acab8`:** unambiguous positive observability change.
Does not change state semantics; does not close F1 or F2. Surfaces a
previously invisible bug class. Pair with R9-M3 (catch_unwind on the
register_model dispatch boundary) would close most of the residual
"silent advisory-lock leak" surface.

**Score impact:** +0.5 to the "concurrent register_model" component
(was 90/100; observability improvement on top of a known-correct
gate). Net round score: rounding holds; this is genuinely the kind of
small observability hook the plateau-blocking F1/F2 work isn't.

### 2.2 `2fa9472e plugin-db/auth: gate dormant auth subtree behind hardening feature`

**Diff:** `Cargo.toml` adds `hardening = []` feature; integration
test target now requires `["test-helpers", "hardening"]`. `lib.rs`
gates `pub(crate) mod auth;` / `pub mod auth;` on
`#[cfg(all(feature = "hardening", ...))]`.

**Migration-pipeline impact analysis:**

Walked the `auth/` → migration pipeline edge graph:

- `auth/*` (HMAC key rotation, session minting, admin schema
  bootstrap) has zero callers from `migrations.rs`, `audit.rs`,
  `orchestrator/*`, or `backend/postgres.rs`. Grep confirms no
  `crate::auth::` imports in any pipeline file.
- The `admin schema bootstrap` in `auth/*` does **not** intersect
  `ensure_app_schema` / `ensure_audit_table_exists`
  (`audit.rs:148-261`): the auth subtree manages its own `__zeroship_auth_*`
  tables in a separate schema namespace; the migration pipeline owns
  the per-app `"{app_id}"` schema and the `"{app_id}"."__zeroship_migrations"`
  audit table.
- No replication-publication setup intersects `auth/*`. Replication
  is in `replication.rs` (separate module, gated by its own feature
  flags); the only `2fa9472e` adjacency is the `replication.rs`
  docstring-only commit `bed655c1` (irrelevant).
- Default builds now omit `auth/*` (`cargo build -p zeroship-plugin-db
  --lib` drops 58 warnings per the commit message). Migration pipeline
  symbols (`migrations`, `audit`, `orchestrator`, `backend::postgres`)
  remain unconditionally compiled. The integration test target now
  requires `hardening` because the legacy auth surface tests probe
  the gated subtree — those tests are not part of the migration
  pipeline's coverage either.

**Verdict on `2fa9472e`:** as expected, orthogonal. Migration
ordering and replication-publication setup are unaffected. The only
ambient effect is a slightly smaller default-build symbol surface,
which has no semantic impact on the pipeline.

**Score impact:** +0.

---

## 3. Strictness mode terminal-coherence re-walk (per brief)

Brief asks: "Lenient vs strict vs off — does each leave the audit
table in a coherent terminal state?" Walked at HEAD:

| Mode | Destructive op present? | Pending row written? | Terminal status reached? |
|---|---|---|---|
| **strict** | yes | yes (validate.rs:69-88) | **No** — strict short-circuits at line 90-92 (`return Err(envelope)`). Row stays `pending`. **F2 orphan.** |
| **strict** | no | n/a | yes — apply pass 1 flips Running → Applied/Failed (F1 caveat: `let _ =`) |
| **lenient** | yes | yes (validate.rs:69-88) | **No** — destructive op retained in `plan.ops`; apply.rs:203/233 skip it (`if op.class == Destructive { continue; }` pattern). The Pending row written in validate is never re-visited. **F2 orphan.** |
| **lenient** | no | n/a | yes — same as strict-no-destructive |
| **off** | yes | **no** (gate at validate.rs:66) | **Implicit** — destructive op flows to apply. Apply.rs:154-157 trips `check_destructive_invariant` for Drop ops (returns `destructive_invariant_error`), which falls through to the F1 `let _ =` Failed transition. So **only "off" mode** in fact reaches a terminal state for destructive ops — and only because the invariant error fires the apply audit row, not a validate audit row. |
| **off** | no | n/a | yes — same as strict-no-destructive |

**Coherence per mode:**

- `strict` + destructive: **incoherent terminal state** (Pending
  orphan + error returned to caller).
- `lenient` + destructive: **incoherent terminal state** (Pending
  orphan + silent skip; success returned to caller).
- `off` + destructive: **coherent** if the invariant error fires
  (apply rail writes Failed row, modulo F1's silent `let _ =`); the
  validate rail never writes a row in the first place.

The `off` path is the cleanest by virtue of *not writing* the
validate-side audit row. F1 + F2 together leave the audit table in
incoherent terminal state for `strict` and `lenient` modes whenever
the deploy contains a destructive op — the steady-state observation
matches r9 exactly.

---

## 4. Backfill loop — re-walk at HEAD

`exec_fetch_batch`, `exec_commit_batch`, `finalise_backfill` are
byte-identical to r9 per `git diff`. R5-M7 close (warn-on-err) holds.
R4 [I41] close (cursor write inside tx under row lock) holds. R9-M1
(set_backfill_running gen-predicate gap) holds. No movement.

The new mig_lock tracing from `5d9acab8` interacts with the backfill
loop at:

- `migrations.rs:191` — `take_mig_client` (no change to observability).
- `migrations.rs:196` — `return_mig_client` (new `warn` on empty
  slot). This fires only if `clear_mig_lock` ran during the await,
  which means operator cancel or isolate drop. In the operator-cancel
  case the row has already been flipped to `cancelled` by
  `exec_cancel`, so the warn pairs cleanly. In the isolate-drop case
  there's no audit-row update at all — the warn is the only
  diagnostic signal that the in-flight client is gone, which is a
  modest improvement over r9's silent drop.

---

## 5. Score

**85 / 100** (unchanged vs r9: **85**; unchanged vs r8: **85**)

### Δ vs r9: **+0**

### Score components (vs r9)

- **Strict-deploy correctness:** 91/100 (unchanged)
- **Lenient/off-deploy correctness:** 75/100 (unchanged)
- **Audit-row state machine consistency:** 68/100 (unchanged) — F1
  + F2 unchanged
- **Backfill orchestrator:** 82/100 (unchanged)
- **CIC recovery loop:** 90/100 (unchanged)
- **Concurrent `register_model`:** 90/100 (unchanged) — `5d9acab8`
  adds observability on the mig_lock slot but does not change the
  state machine. R9-M3 (catch_unwind) still open.
- **Error-helper consistency / dedup:** 90/100 (unchanged)

### Round-over-round signal

- r2 → r3: +0
- r3 → r4: +1
- r4 → r5: +1
- r5 → r6: +1
- r6 → r7: +1
- r7 → r8: +0
- r8 → r9: +0
- **r9 → r10: +0** (third consecutive zero)

Three consecutive +0 rounds against an asymptote of 86-87 (without
F1/F2 shipping). The plateau is now **empirically confirmed** with
n=3 rounds of evidence.

The `5d9acab8` observability hook is a genuinely useful debug surface
on a known-correct gate (the `has_mig_lock` precondition at
`migrations.rs:228`). It is not a state-machine correctness fix and
does not move any of the seven scoring components, including the one
most directly adjacent ("concurrent register_model"), because that
component was already 90/100 on the strength of the gate itself, not
on observability.

The `2fa9472e` feature gate is orthogonal as suspected — the auth
subtree never intersected the migration pipeline.

### Recommendation

Pause this lens until at least one of:

1. **F2 (R9-I2)** lands — extends status CHECK to include
   `validation_refused`, adds the variant, flips the validate.rs
   Pending writes to Running → Refused. Highest-leverage single
   change; unblocks audit-row state-machine component from 68 → ~78.
2. **F1 (R9-I1) `tracing::warn!` half** lands — converts the five
   `let _ =` sites to `if let Err(e) = ... { tracing::warn!(...); }`.
   Trivial; same shape as the in-tree R5-M7 close at
   `migrations.rs:635-648`. Moves lens floor 1-2 points.
3. **R9-M1** lands — gen-predicate on `set_backfill_running`. ~5 LOC.

Without any of those, future rounds will continue to score 85/100 on
this lens. The marginal value of an r11 in this lens without F1/F2
shipping is approximately **+0**, with three consecutive rounds of
empirical evidence (r8, r9, r10) supporting the projection.

---

## 6. F1 / F2 one-liner status (per brief)

- **F1 (orphan Running DDL audit rows):** open; 9+ cycle carry. Five
  `let _ = update_audit_status(...).await;` sites at
  `apply.rs:163-170`, `apply.rs:178-186`, `postgres.rs:478-486`,
  `postgres.rs:519-528`, `postgres.rs:558-567`. **Zero code change
  since r2.**
- **F2 (orphan validate-refused Pending audit rows):** open; 9+
  cycle carry. `validate.rs:66-95` writes Pending; strict short-
  circuits, lenient falls through; status CHECK at
  `audit.rs:218-220` lacks `validation_refused` so even a fix needs
  schema migration. **Zero code change since r2.**
