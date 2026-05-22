# plugin-db docs audit — round 4 (2026-05-22)

**Scope:** `crates/plugin-db/src/` preambles + inline comments, plus
the `docs/reference/db.md` / `docs/reference/plugin-system.md` /
`AGENTS.md` surfaces that point into this crate.

**Prior rounds:** r1 (68), r2 (74), r3 (cycle 03:25 — 80/100).

This pass re-audits after the four follow-up commits the brief calls out:

- `0049d9be` [I28] sweep — `Result<_, String>` removed from `auth/*` +
  `replication.rs` (+ `diff.rs`). Drops the bulk of the "remaining
  sites" inventory error.rs:9-19 was tracking.
- `808a32af` [I39] `OrchestratorLockGuard` `#[must_use]` +
  louder Drop log.
- `c0590506` (CRITICAL security) watchdog/dropAbandoned scoped
  to `self.app_id`; new tenancy sections in `replication.rs` +
  per-method docstrings in `v8_classes/replication.rs`.
- `eda96ead` `first_row_or_internal` helper extracted into
  `error.rs`, with its own preamble; the three
  empty-RETURNING sites (audit.rs ×2, replication.rs) now route
  through it.

**TL;DR.** Round 3's CRITICAL r3 finding (`validate.rs` preamble's
SchemaRefused type-lie) was fixed in `07205e54` — clean close. The
big architectural sweep [I28] (`0049d9be`) materially shrank the
crate's `Result<_, String>` footprint to a single local helper pair
in `auth/session.rs` (`hex_decode` / `hex_nibble`) — but
**`error.rs` preamble (lines 9-19) was not updated to match**,
leaving an inventory that no longer reflects the source. This is
the round's only NEW CRITICAL. The new `first_row_or_internal`
preamble (`eda96ead`) is excellent — it explicitly names the
d7cfc089 bug class and the regression-test contract. The new
tenancy docstrings in `v8_classes/replication.rs` (`c0590506`)
correctly attribute `self.app_id` not "cluster-wide". The
`OrchestratorLockGuard` body has accurate `[I42]` / `[I44]` inline
annotations but the **module-level preamble (lines 1-50) is silent
about the three reinforcement passes [I39] / [I42] / [I44] /
`ffb1e101` that landed since `cbd12944`** — historically the
preamble names the three pre-extraction commits, but the next
reader has no signal that the guard was hardened three more times
in the last week. The two r3 hold-out drift classes
(`v8_classes/transaction.rs` TX_CONN/TX_TOKEN; `orchestrator/mod.rs`
`pub` vs `pub(crate)` mismatch; `query.rs:445` composite-index
TODO) survive untouched.

---

## Dimension-by-dimension findings

### 1. Stale historical names (TX_CONN / TX_TOKEN / MIG_LOCK / PENDING_EMITS / callbacks.rs)

```
[CRITICAL] crates/plugin-db/src/v8_classes/transaction.rs:68,161,192,214 — rustdoc intra-doc links to deleted symbols (UNCHANGED FROM r3)
  Why: `[`crate::TX_TOKEN`]` and `[`crate::TX_CONN`]` resolve to nothing — the symbols were folded into IsolateDbContext in Stage 8d-R4. `cargo doc` emits broken-link warnings on every build. r3 flagged the same four sites; no commit since then touched this file. Targeted sweep is still pending.
  Fix: Replace `[`crate::TX_TOKEN`]` → `IsolateDbContext::tx_token`, same for TX_CONN.
  Verification: grep -n "crate::TX_CONN\|crate::TX_TOKEN" crates/plugin-db/src/v8_classes/transaction.rs

[IMPORTANT] crates/plugin-db/src/v8_classes/transaction.rs:69-71,107-116,161,192,214-218,278,284-285,324 — bare TX_CONN / TX_TOKEN tokens in prose (UNCHANGED FROM r3)
  Why: ~12 sites in this one file still use "TX_CONN" / "TX_TOKEN" as if they were the live symbol. Adjacent files (orchestrator/transaction.rs preamble, v8_classes/mod.rs:17, orchestrator/mod.rs:13) already say "IsolateDbContext::tx_conn" — the inconsistency is across-file, not within-file, so it's especially confusing.
  Fix: Mechanical s/TX_TOKEN/tx_token/, s/TX_CONN/tx_conn/ across the file.
  Verification: grep -cn "TX_CONN\|TX_TOKEN" crates/plugin-db/src/v8_classes/transaction.rs

[IMPORTANT] crates/plugin-db/src/v8_classes/migration.rs:216 — `MIG_LOCK` thread-local reference (UNCHANGED FROM r3)
  Why: "checks the `MIG_LOCK` thread-local for ownership / cancellation". The lock now lives on `IsolateDbContext::mig_lock`; no top-level MIG_LOCK exists. Reader chasing cancellation enforcement greps and finds context.rs not a thread-local.
  Fix: s/`MIG_LOCK` thread-local/`IsolateDbContext::mig_lock` slot/.
  Verification: grep -n "MIG_LOCK" crates/plugin-db/src/v8_classes/migration.rs

[MINOR] crates/plugin-db/src/lib.rs:110,216 — caps-name idiom (UNCHANGED FROM r3)
  Why: Doc comments lead with "Allocate a fresh non-zero TX_TOKEN value" and "clear `MIG_LOCK` for the current thread" although the functions manipulate `IsolateDbContext::tx_token` / `IsolateDbContext::mig_lock`. lib.rs:251 already uses the correct `IsolateDbContext::tx_conn` idiom with a "formerly the `TX_CONN` thread-local" footnote — apply the same pattern.
  Fix: Either retitle to `tx_token` / `mig_lock` slot terms, or append the "formerly" footnote consistently.
  Verification: grep -n "TX_TOKEN\|MIG_LOCK" crates/plugin-db/src/lib.rs

[MINOR] crates/plugin-db/src/crud.rs:53 — "the active TX_CONN" in run_op docs (UNCHANGED FROM r3)
  Why: Same drift class; function-level doc on the hottest CRUD path.
  Fix: s/the active TX_CONN/the active tx connection (IsolateDbContext::tx_conn)/.
  Verification: grep -n "TX_CONN" crates/plugin-db/src/crud.rs

[MINOR] crates/plugin-db/src/exec.rs:329 — test-helper doc references "TX_CONN" (UNCHANGED FROM r3)
  Why: `exec_mutation_with_emit_for_tests` doc says "setting `TX_CONN`"; the setter is `install_tx_marker_for_tests` which writes to `IsolateDbContext::tx_conn`.
  Fix: s/setting `TX_CONN`/installing the tx-conn slot/.
  Verification: grep -n "TX_CONN" crates/plugin-db/src/exec.rs

[MINOR] crates/plugin-db/src/orchestrator/transaction.rs:51,52,89,143 — inline "TX_TOKEN" / "TX_CONN" (UNCHANGED FROM r3)
  Why: Preamble at lines 1-14 is correct ("IsolateDbContext::tx_conn"); inline comments below lapse back to constant names.
  Fix: s/TX_TOKEN/tx_token/, s/TX_CONN/tx_conn/ for the four sites.
  Verification: grep -n "TX_CONN\|TX_TOKEN" crates/plugin-db/src/orchestrator/transaction.rs

[MINOR] crates/plugin-db/src/backend/mod.rs:66 — "thread-local (e.g. `MigrationLock::client`, `tx_conn`)" (UNCHANGED FROM r3)
  Why: Both items live in `RefCell<IsolateDbContext>`, not thread-locals. This is trait doc — externally visible.
  Fix: "in the per-isolate context (e.g. `MigrationLock::client`, `IsolateDbContext::tx_conn`)".
  Verification: grep -n "thread-local" crates/plugin-db/src/backend/mod.rs
```

**Net change vs r3:** zero. d53f90b0's sweep did not reach the v8_classes layer and no follow-up commit since r3 has either. Surface-level grep hits stay at ~15 across 7 files. The targeted sweep is the single largest pending docs-audit chore.

### 2. Module preambles after [I28] sweep (auth/* + replication.rs)

```
[OK] crates/plugin-db/src/auth/bootstrap.rs:1-8 — preamble accurate post-[I28]
  Why: The preamble describes the bootstrap responsibility ("Idempotent bootstrap of the `__zeroship_admin` schema, platform roles, key table, nonce table, and every SECURITY DEFINER function") without making error-rail claims. The new local `coded_sql` helper at lines 21-37 has a short preamble that correctly names the `auth/bootstrap:` prefix shape AND points at the "Mirrors the `coded_sql` shape in `crate::audit`" precedent — good cross-reference.
  Verification: head -40 crates/plugin-db/src/auth/bootstrap.rs

[OK] crates/plugin-db/src/auth/session.rs:1-20 — preamble accurate post-[I28]
  Why: Describes the two RPC entry points (mint_session_token, init_session) and the nonce-replay invariant; the new `coded_sql` helper at lines 28-48 mirrors the bootstrap.rs pattern with `auth/session:` prefix. The integration-test cross-references (`b8c_init_session_rejects_*`) referenced in the [I28] commit message are pinned via `tests/integration.rs:b8c_*`.
  Verification: head -50 crates/plugin-db/src/auth/session.rs

[OK] crates/plugin-db/src/auth/keys.rs:1-27 — preamble accurate post-[I28]
  Why: Describes Stage 4 partial (Rust primitives shipped, scheduling deferred). The new `coded_sql` helper preamble at lines 34-54 keeps the same `auth/keys:` shape. The "What's deferred" section is still accurate (control-plane cron scheduling is not yet in `crates/control`).
  Verification: head -30 crates/plugin-db/src/auth/keys.rs

[OK] crates/plugin-db/src/auth/mod.rs:1-103 — preamble accurate; no changes needed for [I28]
  Why: Describes module structure + bootstrap ceremony + backwards-compatibility framing. Doesn't make error-rail claims that the sweep would invalidate.
  Verification: head -110 crates/plugin-db/src/auth/mod.rs

[OK] crates/plugin-db/src/replication.rs:1-48 — preamble accurate; [I28] sweep doesn't invalidate the file-level claims
  Why: The preamble describes the P8a-reduced scope (publication + slot lifecycle + watchdog), the naming convention, and the deferred WAL consumer; it makes no error-rail claims. The new `prefix_message` helper at lines 62-84 (added during the [I28] sweep) has a proper preamble describing variant preservation + `.code` invariance.
  Verification: head -50 crates/plugin-db/src/replication.rs

[OK] crates/plugin-db/src/diff.rs:1-26 — preamble accurate post-[I28]
  Why: Describes the diff engine + volatile-default trap; the new `coded_sql` helper at lines 33-50 is annotated with the same `auth/*` pattern (`Mirrors the `coded_sql` shape in `crate::audit`.`).
  Verification: head -55 crates/plugin-db/src/diff.rs

[OK] crates/plugin-db/src/replication_ops.rs:1-46 — preamble accurate; explicitly documents the post-[I28] error-rail invariant
  Why: The "## Error rail" subsection (lines 32-46) is explicit: "`replication::*` helpers return `Result<_, DbError>` directly — SQLSTATE classification + Configuration/Transient/LockContention variants are picked inside `crate::replication` and flow through here verbatim (no Internal-wrapping at the dispatch boundary)." This is the exact contract the [I28] sweep enforces; reader has a clear top-of-file signal that the typed rail is end-to-end.
  Verification: head -50 crates/plugin-db/src/replication_ops.rs
```

Verdict on Dimension 2: the auth/* and replication.rs preambles are
**internally consistent with the [I28] outcome**. The miss is one
level up — error.rs's preamble (which advertises the *crate-wide*
state of the typed-error rail) still describes the pre-sweep
inventory. See Dimension 3.

### 3. Type lies (return-type / behavioural claims contradicted by code)

```
[CRITICAL] crates/plugin-db/src/error.rs:9-19 — preamble's "known remaining sites" inventory is stale post-[I28]
  Why: The preamble enumerates the supposedly remaining `Result<_, String>` callers:
  > "Known remaining sites (see backlog [I28]): replication.rs (~7 sites), the `auth/*` bootstrap helpers (~15 sites), parts of `diff.rs`, plus the `validate` stage…"
  Actual state after `0049d9be` ([I28] sweep — 30 sites converted) + `91830cca` (drop stale `.into_string()`):
    - replication.rs: 2 `Result<_, String>` mentions, BOTH inside test-only docstring prose (lines 751, 846 — describing the *historical* shape the test is regression-guarding against, not a current signature). Zero production sites.
    - auth/bootstrap.rs: 1 mention at line 1065 — inside a TEST DOCSTRING that names what shape the regression guard prevents reverting to.
    - auth/keys.rs: 1 mention at line 203 — same: a comment in the typed-error test block.
    - auth/session.rs: 4 mentions — 2 are real `Result<Vec<u8>, String>` signatures on the pure-function hex_decode/hex_nibble parsers (acceptable local helpers); the other 2 are inside the same test-regression-guard prose pattern.
    - diff.rs: 0 mentions. Fully converted.
  Net: the inventory's "(~7 sites)" / "(~15 sites)" / "parts of diff.rs" claims describe a state that hasn't existed for several commits. The only remaining `Result<_, String>` is the two hex-parser helpers, and those are deliberate (pure-function local helpers, no SDK-visible code).
  Net impact: reader of error.rs preamble concludes the typed-error sweep is in-flight. It is not — the SDK now gets the typed `.code` on every JS-visible auth/replication failure path. New contributors making decisions on "should I use String or DbError" get the wrong defaults.
  Fix: Rewrite lines 9-19 to read approximately:
    "Every fallible internal helper returns `Result<_, DbError>`. The only exception is the validate stage in `crate::orchestrator::register_model`, whose `Err` is the `validation_refused` JSON envelope — a documented SDK wire contract surfaced byte-for-byte through `DbError::SchemaRefused::to_op_error` (see [`DbError::SchemaRefused`]). The auth/replication/diff sweep landed in `0049d9be`; the only `Result<_, String>` signatures that remain are two pure-function hex-parser helpers in `auth/session.rs`."
  Verification:
    grep -rn "Result<.*, String>" crates/plugin-db/src/ | grep -v test
    git show 0049d9be --stat

[IMPORTANT] crates/plugin-db/src/replication.rs:751-757 — test docstring claims ensure_publication_and_slot returns `Result<_, String>`
  Why: The docstring above `empty_returning_string_shape_keeps_replication_prefix` reads:
  > "`ensure_publication_and_slot` returns `Result<_, String>` (not `Result<_, DbError>` like audit.rs), so the runtime fix calls `.into_string()` on the `DbError::Internal` before flowing it through `?`."
  The function at line 192-195 returns `Result<SetupOutcome, DbError>` (sweep landed in `0049d9be`; the stale `.into_string()` call was specifically removed in `91830cca`). The test below it constructs a `DbError::Internal` and calls `.into_string()` on it; that's a string-shape regression guard against a wire format that NO production code path produces any more. So the test still has value (pins the operator-facing message body the legacy code emitted), but the docstring describes a `?`-flow that does not exist.
  Fix: Reframe the docstring to "Historic shape: `ensure_publication_and_slot` once returned `Result<_, String>` and the runtime called `.into_string()` on the typed error. Post-[I28] (`0049d9be`) the function returns `Result<_, DbError>` directly. This test pins the operator-facing *message body* (`replication:` prefix + operation tag) so a future refactor can't drop those substrings — log scrapers / dashboards key off them."
  Verification: grep -n "fn ensure_publication_and_slot\|Result<.*, String>" crates/plugin-db/src/replication.rs

[IMPORTANT] crates/plugin-db/src/orchestrator/mod.rs:22 — "Each submodule is `pub(crate)` to scope visibility" (UNCHANGED FROM r3)
  Why: Look at lines 28-31:
  ```rust
  pub mod auto_tx;
  pub(crate) mod lock_guard;
  pub mod register_model;
  pub mod transaction;
  ```
  Three of four are `pub`, not `pub(crate)`. The effective visibility is pub(crate) because lib.rs:84 has `pub(crate) mod orchestrator;`, but that's transitive — what the comment claims about each submodule is literally false. r3 flagged the same drift; no follow-up commit touched it.
  Fix: Either (a) downgrade three `pub mod` → `pub(crate) mod` (matches the comment + cbd12944's lock_guard choice), or (b) rewrite the comment: "Each submodule is `pub` to the orchestrator parent, which itself is `pub(crate)`-gated in `lib.rs:84`. There is no longer an aggregating re-export …".
  Verification: grep -n "^pub.*mod" crates/plugin-db/src/orchestrator/mod.rs

[MINOR] crates/plugin-db/src/migrations.rs:67-81 — stray "duplicate doc summary" line 78 (UNCHANGED FROM r3)
  Why: r3 flagged this as a probable dec2bd42 restore artefact. The block still reads:
  > "/// SQL-error helper — classify the Postgres error through `DbError`\n/// ...\n/// the SQLSTATE classification.\n/// Stamp a `DbError` with a context phrase and convert to `OpError`.\n/// Replaces the previous `coded_sql(context, compio_postgres::Error)`..."
  Two distinct first-paragraph summaries inside one doc block.
  Fix: Drop line 78 or split into properly separated `///` paragraphs.
  Verification: sed -n '67,82p' crates/plugin-db/src/migrations.rs

[MINOR] crates/plugin-db/src/migrations.rs:79-80 — "Replaces the previous `coded_sql` helper" (UNCHANGED FROM r3)
  Why: `coded_sql` is alive in crate::audit:55 (and now also in auth/{bootstrap,session,keys}.rs and diff.rs since the [I28] sweep instantiated the same shape there). The "previous" framing is local-only; reader could conclude the helper is gone crate-wide.
  Fix: "Replaces the previous `coded_sql(...)` helper in this file" — add "in this file".
  Verification: grep -rn "fn coded_sql" crates/plugin-db/src/

[OK] crates/plugin-db/src/orchestrator/register_model/validate.rs:16-32 — r3's CRITICAL closed at 07205e54
  Why: r3's CRITICAL claim that the preamble said SchemaRefused.to_op_error() does NOT add `.code` is now reversed — the preamble correctly states "stamps `.code` from the static discriminator (typically `\"validation_refused\"`) AND emits the envelope as the JS `Error.message`. SDK callers can branch on `err.code === \"validation_refused\"` directly or `JSON.parse` the message to recover the structured payload." Matches error.rs:194-205 (the actual arm) + error.rs:425-445 (the regression test). Clean fix.
  Verification: sed -n '16,32p' crates/plugin-db/src/orchestrator/register_model/validate.rs
```

### 4. Missing preambles

```
[OK] Every file in crates/plugin-db/src/ (recursive) has a top-of-file `//!` summary, including the new `first_row_or_internal` helper docs (which live as a function-level doc, not a file). Counts:
  - 16 top-level .rs (audit, broker, context, crud, diff, error, exec, lib, migrations, query, read_set, replication, replication_ops, v8_bridge, wal_consumer + auth/backend/orchestrator/v8_classes mods)
  - 5 in orchestrator/register_model
  - 4 in orchestrator (auto_tx, lock_guard, mod, transaction)
  - 8 in v8_classes (collection, db, migration, migrations, mod, replication, subscription, transaction)
  - 4 in auth (bootstrap, keys, mod, session)
  - 2 in backend (mod, postgres)
  All start with //!; no empty preambles, no // (non-doc) before module body.
  Verification: for f in $(find crates/plugin-db/src -name "*.rs"); do first=$(head -1 "$f"); [[ ! "$first" =~ ^"//!" ]] && echo "MISS: $f"; done
```

### 5. Recent commit inline annotations (accuracy of `// [I42]` / `// [I44]` / `[I28]` references)

```
[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:136-139 — `[I42]` annotation accurate
  Why: The comment says "the prior version flipped `released = true` BEFORE the await, so a cancellation here silently leaked the lock with no Drop log. Defer the state flip to AFTER the await completes." This matches commit bd1e7ce1 ("defer released-flag flip to AFTER unlock await") exactly. The annotation lives next to the `.await` line that's now followed by `self.released = true` on line 161 — accurate placement.
  Verification: git log --oneline -- crates/plugin-db/src/orchestrator/lock_guard.rs | head -5

[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:143-147 — `[I44]` annotation accurate
  Why: "a bare `let _ =` silently swallows runtime errors from the unlock SQL — operator never sees that the lock might still be held. Log warnings on error so a leak is visible". The replaced code is now `if let Err(e) = client.query_text_params(...).await { tracing::warn!(...) }` — the annotation correctly identifies the prior shape AND the fix. Matches commit ffb1e101.
  Verification: git show ffb1e101 -- crates/plugin-db/src/orchestrator/lock_guard.rs

[OK] crates/plugin-db/src/replication.rs:803-812 + tests block — "Typed-error sweep [I28]" header accurate
  Why: The block introduces unit tests pinning `.code` for sanitise_app_id / publication_name / slot_name + prefix_message variant preservation — exactly the contract the [I28] commit shipped. Header references the right backlog ticket.
  Verification: grep -n "Typed-error sweep" crates/plugin-db/src/replication.rs

[OK] crates/plugin-db/src/auth/{bootstrap,keys,session}.rs — "Typed-error sweep [I28]" test-block headers accurate
  Why: Each file's "[I28]" test block introduces a type-level signature guard that pins the function's Result error type as `DbError`. The headers cross-reference the same commit (`0049d9be`) and the contract description is consistent across the three files.
  Verification: grep -n "Typed-error sweep" crates/plugin-db/src/auth/

[CONTRADICTION] crates/plugin-db/src/error.rs:10 — "(see backlog [I28])" reference is misleading post-sweep
  Why: The preamble still links to backlog [I28] as if the sweep is open. The sweep landed in 0049d9be (this commit *is* the [I28] action). Reader following the link expects more work to do, finds none.
  Fix: Drop the backlog reference (replace with citation to commit `0049d9be`). Covered as part of the Dimension 3 CRITICAL above.
  Verification: grep -n "\[I28\]" crates/plugin-db/src/error.rs
```

### 6. AGENTS.md task router — path resolution

```
[OK] Every plugin-db-relevant row in AGENTS.md resolves to a live path on HEAD:
  - "**Adding a native primitive** … `docs/reference/plugin-system.md` · `crates/runtime-macros/` · `crates/plugin-{db,kv,storage}/`" — all four exist
  - "**The DB SDK** (`@zeroship/db`) … `docs/reference/db.md` · `crates/plugin-db/`" — both exist
  - "**ZS deploy contract** … `sdks/bootstrap/src/{dispatcher,runtime-entry}.ts` · `crates/runtime/src/core/init.rs`" — all three exist
  Verification: ls docs/reference/{db,plugin-system}.md crates/plugin-{db,kv,storage} crates/runtime-macros sdks/bootstrap/src/{dispatcher,runtime-entry}.ts crates/runtime/src/core/init.rs

[NOTE — out of scope for this audit but adjacent] docs/reference/plugin-system.md:315-344 — "Crate structure" tree still stale (UNCHANGED FROM r3)
  Why: AGENTS.md row 3 sends "adding a native primitive" readers here, and the tree at lines 315-344 lists non-existent paths:
    - crates/runtime/src/init.rs           → today: crates/runtime/src/core/init.rs
    - crates/runtime/src/runtime.rs        → does not exist
    - crates/runtime/src/plugin.rs         → does not exist
    - crates/plugin-db/src/callbacks.rs    → deleted Stage 8b
    - crates/plugin-db/src/validate.rs     → exists only at orchestrator/register_model/validate.rs
    - crates/plugin-db/src/migrate.rs      → does not exist (today: migrations.rs + audit.rs)
    - crates/plugin-auth/                  → does not exist (auth lives in plugin-db/src/auth/)
    - crates/pg/                           → renamed to compio-postgres
  Same finding as r3; not adjacent enough to be in-scope (the brief is plugin-db), but the entry-point experience for new contributors is degraded.
  Verification: ls crates/runtime/src/init.rs crates/plugin-db/src/callbacks.rs crates/plugin-auth/ crates/pg/ → all ENOENT
```

### 7. lock_guard.rs preamble — accurate after [I42] / [I39] / [I44]?

```
[IMPORTANT] crates/plugin-db/src/orchestrator/lock_guard.rs:1-50 — module preamble silent on three post-extraction reinforcements
  Why: The preamble names cbd12944's three pre-extraction commits (`b4e533e2`, `37a0ef76`, `3bb41fa1`) that open-coded the same release pattern. It does NOT mention the three reinforcement passes that landed AFTER extraction:
    1. bd1e7ce1 / [I42]: "defer released-flag flip to AFTER unlock await" — fixes a cancellation-window leak the original implementation had.
    2. 808a32af / [I39]: "#[must_use] + louder Drop log" — adds the compile-time warning surface and the operator-facing diagnostic checklist on Drop.
    3. ffb1e101 / [I44]: "warn on pg_advisory_unlock errors" — surfaces the lock leak through tracing rather than silently swallowing.
  The struct-level doc comment at lines 56-67 DOES mention `#[must_use]`, but the module preamble's "Why not full RAII?" section (lines 25-41) describes the design WITHOUT the cancellation-window + tracing layers added by [I42] / [I44]. Reader assumes those weren't considered; in fact each gap was discovered and closed in a separate commit. Historical context is helpful here precisely because the bug class is async-cancellation-shaped.
  Fix: Append a "# Hardening passes since extraction" subsection (~6 lines) listing the three commits + the bug class each closed (cancellation-window leak; missing leak-visibility on Drop; unlock-error silent swallowing). Keep brief — readers chasing why the implementation is shaped the way it is need the bread-crumb.
  Verification: git log --oneline -- crates/plugin-db/src/orchestrator/lock_guard.rs; sed -n '1,50p' crates/plugin-db/src/orchestrator/lock_guard.rs

[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:56-67 — struct doc comment correctly documents `#[must_use]`
  Why: "**Must be consumed via `release().await` or `into_held()`.** `Drop` cannot await the unlock SQL, so a guard dropped without one of those calls leaks the session-scoped advisory lock until the PG session ends (typically when the pool recycles the connection — could be tens of seconds to minutes). The `#[must_use]` annotation surfaces accidental drops as compile-time warnings on common patterns (e.g. `let _ = acquire(...).await`)." Accurate, precise, named caveat about edge cases.
  Verification: sed -n '56,68p' crates/plugin-db/src/orchestrator/lock_guard.rs

[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:192-219 — Drop log accurate
  Why: The `tracing::error!` body has the correct "leak:" prefix (matches the [I39] commit), the operator-facing consequence ("Concurrent register_model callers for this app will stall in the meantime") and the diagnostic checklist (async-cancellation / panic / missed release). Matches the [I39] commit description exactly.
  Verification: sed -n '192,220p' crates/plugin-db/src/orchestrator/lock_guard.rs
```

### 8. first_row_or_internal preamble — accurate? Documents the d7cfc089-style bug class?

```
[OK] crates/plugin-db/src/error.rs:307-329 — `first_row_or_internal` preamble is excellent
  Why: The doc comment explicitly names the bug class and the regression-test contract:
  > "Used to close the silent-empty-RETURNING bug class: callers that previously chained `.first().map(...).unwrap_or_default()` coerced an empty RETURNING set into a sentinel value (the audit-id=0 bug fixed in d7cfc089; the replication-slot empty-LSN twin fixed alongside it). The helper names the predicate in one place so every empty-RETURNING site emits the same `DbError::Internal { message: \"<op>: returned no row\" }` shape — preserving the regression test contract in `audit.rs::tests::insert_backfill_running_empty_returning_is_internal_error`."

  Specifically excellent:
  - Names the bug class ("silent-empty-RETURNING") and the canonical commit fixing it (d7cfc089).
  - Names the sibling case (replication-slot empty-LSN twin) — important because [I4] (architecture r6) had a similar shape and `eda96ead` extracted the helper PRECISELY to close the sibling cluster.
  - Specifies the unified shape (`<op>: returned no row`) so callers don't drift into per-site message variations.
  - Names the regression test the helper preserves the contract for.
  - Acknowledges the test-vs-production generic-row type gap with rationale.
  Mirrors the bread-crumb quality of the `lock_guard.rs` preamble — both are exemplary.
  Verification: sed -n '307,330p' crates/plugin-db/src/error.rs

[OK] eda96ead's call-site rationale is on the helper docstring rather than the three callers (audit.rs:325, audit.rs:613, replication.rs:225). Acceptable — the helper-side preamble is the single source of truth.
  Verification: grep -n "first_row_or_internal" crates/plugin-db/src/
```

### 9. Tense drift / stale "future" claims (carry-over from r3)

```
[MINOR] crates/plugin-db/src/audit.rs:5 — "every DDL, validation pass, and (future) backfill writes a row" (UNCHANGED FROM r3)
  Why: B1 backfill orchestrator ships and writes Backfill-phase rows (see Phase::Backfill at audit.rs:97-99 and the migration.rs B1 functions). The "(future)" parenthetical predates the B1 ship by ~30 commits and was already flagged in r3.
  Fix: drop "(future)" — backfill is present-tense.
  Verification: grep -n "future" crates/plugin-db/src/audit.rs
```

### 10. Orphan TODO/FIXME (carry-over from r3)

```
[CRITICAL] crates/plugin-db/src/query.rs:443-446 — "TODO: A1 composite indexes" (UNCHANGED FROM r3)
  Why: Composite indexes ARE wired up. `build_named_indexes` at query.rs:529 is called from `orchestrator/register_model/bootstrap.rs:174-176`; SDK builder at `sdks/db/src/types.ts` (around line 945+) is live; the TODO predates the A1 ship. r3 flagged the same line; no follow-up commit since.
  Fix: Either delete the TODO (composite indexes are present-tense) or rewrite to be specific: "TODO(A1): `schema._meta.indexes` declaration form (today indexes are passed as a separate `registerModel(coll, schema, indexes)` arg)." The phrasing should match what's actually open vs what's shipped.
  Verification: grep -n "build_named_indexes\|schema_meta" crates/plugin-db/src/query.rs; grep -n "\.index(" sdks/db/src/types.ts

[OK] No FIXME / XXX markers anywhere under crates/plugin-db/src/.
  Verification: grep -rn "FIXME\|XXX" crates/plugin-db/src/
```

---

## What got cleanly fixed since r3

- **r3 CRITICAL — validate.rs preamble's SchemaRefused type-lie**: closed at 07205e54 (preamble now correctly states `.code` IS stamped + envelope IS the message). Test at error.rs:425 still pins the contract.
- **r2 CRITICAL — error.rs "lone hold-out" claim**: closed at e37b188f (replaced with the inventory; that inventory is itself now stale per Dimension 3 CRITICAL — fixing one hole opened a new one).
- **r2 CRITICAL — db.md broken path**: closed at e37b188f.
- **Crate-wide `Result<_, String>` discipline**: [I28] sweep at 0049d9be converted ~30 sites across auth/* + replication.rs + diff.rs. Two pure-function hex parsers in auth/session.rs are the only remaining sites; both are local helpers with no SDK-visible surface.
- **`first_row_or_internal` helper**: extracted at eda96ead with a model-quality preamble that names the bug class, the canonical fix commit, the sibling case, the unified shape, and the regression test. Pairs well with the audit.rs / replication.rs callers — each site uses the helper rather than re-inventing the predicate.
- **`v8_classes/replication.rs` tenancy docstrings**: `c0590506` correctly documents that watchdog + dropAbandoned scope to `self.app_id` (not cluster-wide). Per-method doc comments name the sibling vulnerability (`309ed52f` setup hijack) and the per-app slot prefix filter. Resolver helpers (`resolve_watchdog_app_id`, `resolve_drop_abandoned_app_id`) have full doc comments explaining the regression-trip-wire pattern.
- **`replication.rs` per-helper tenancy doc**: `c0590506` adds a "## Tenancy" subsection to both `watchdog_query` and `drop_abandoned_slots`, documenting the parameter-bound `LIKE $1` filter and the cross-tenant DoS / info-disclosure rationale. New `slot_name_like_prefix` helper has its own preamble describing the canonical prefix shape.

## What did NOT change since r3

- **`v8_classes/transaction.rs` TX_CONN/TX_TOKEN drift**: zero commits since r3 touched this file. ~12 prose sites + 4 rustdoc-broken intra-doc links remain. d53f90b0's sweep still hasn't reached the v8_classes layer.
- **`orchestrator/mod.rs:22` `pub` vs `pub(crate)` mismatch**: zero commits. Three of four submodules still declared `pub` while the preamble claims `pub(crate)`.
- **`query.rs:443` composite-indexes TODO**: zero commits. Composite indexes still shipped, TODO still says they're not.
- **`audit.rs:5` "(future) backfill"**: zero commits. B1 backfill still shipped, comment still says "future".
- **`migrations.rs:78` stray duplicate doc summary**: zero commits. dec2bd42 restore artefact.
- **`lib.rs:110/216` caps-name idiom drift**: zero commits.
- **`crud.rs:53` / `exec.rs:329` / `backend/mod.rs:66` thread-local references**: zero commits.

## NEW since r3

- **CRITICAL (Dimension 3) — `error.rs:9-19` preamble inventory stale**: [I28] sweep landed but the preamble's "remaining sites" list was not updated. New contributors are told to expect work that doesn't exist. Single-largest doc/code divergence introduced this cycle.
- **IMPORTANT (Dimension 3) — `replication.rs:751-757` test docstring claims a return type that no longer exists**: post-`91830cca` the function is `Result<_, DbError>`; the docstring still says `Result<_, String>` and frames the test as covering a `?`-flow that has been deleted.
- **IMPORTANT (Dimension 7) — `lock_guard.rs` preamble silent on [I42] / [I39] / [I44] reinforcement passes**: the bread-crumb stops at cbd12944 even though three further commits hardened the same module. The body comments are accurate; the preamble is incomplete.
- **CONTRADICTION (Dimension 5) — `error.rs:10` references "backlog [I28]" as open**: it's not — the file's preamble cites a closed backlog entry as a current TODO.

---

## Score: 75 / 100  (r3: 80)

**Delta breakdown (-5 from r3):**

- +4 — `first_row_or_internal` preamble (eda96ead) is the cleanest new doc landed since r3's lock_guard.rs entry: names the bug class (silent-empty-RETURNING), the canonical fix commit (d7cfc089), the sibling case (replication-slot empty-LSN twin), the regression test it preserves, and the test-vs-prod generic-row rationale. Pairs cleanly with the three call sites.
- +3 — `v8_classes/replication.rs` per-method docstrings (`c0590506`) are precise about tenancy (`self.app_id` not cluster-wide) and cross-reference the sibling vulnerability (`309ed52f`). Mirrors the `setup_app_id_*` regression-test trip-wire pattern.
- +2 — `replication.rs::watchdog_query` + `drop_abandoned_slots` "## Tenancy" subsections (`c0590506`) document the parameter-bound `LIKE $1` filter + per-app prefix, explaining WHY the implementation is shaped the way it is.
- +1 — r3 CRITICAL closed (validate.rs preamble inversion of SchemaRefused contract).
- +1 — `slot_name_like_prefix` helper (replication.rs:138-150) has its own preamble naming the unit-testable single source of truth + parameter-bind discipline.
- -5 — NEW CRITICAL: error.rs preamble's "remaining sites" inventory is stale by ~30 commits. This is exactly the kind of dual-source-of-truth divergence the preamble was meant to fix at r2; reopening the same shape one revision later costs more than fixing a fresh hole.
- -2 — NEW IMPORTANT: replication.rs:751-757 test docstring describes a `?`-flow that no longer exists. Misleads readers about the function signature.
- -2 — NEW IMPORTANT: lock_guard.rs preamble silent on [I42] / [I39] / [I44] reinforcement passes. The body annotations are accurate, but the top-of-file bread-crumb stops at cbd12944.
- -2 — Five r3 hold-outs (transaction.rs TX_CONN/TX_TOKEN, orchestrator/mod.rs pub-vs-pub(crate), query.rs:443 TODO, migrations.rs:78 stray summary, audit.rs:5 "(future) backfill") all UNCHANGED. The cumulative cost of un-actioned r3 findings.
- -2 — `crate-wide thread-local` framing (backend/mod.rs:66, crud.rs:53, exec.rs:329, lib.rs:110/216, migration.rs:216) still un-swept. Same drift class as transaction.rs; spread across 6 files.

**To break 90 next round:**

1. **Fix the error.rs preamble inventory.** Single highest-impact edit this cycle; the preamble is the canonical "is the typed-error rail done?" answer and currently says "no" when the answer is "yes (minus two local helpers)".
2. **Pick up the r3 hold-outs.** All five are mechanical, all under 5 minutes each. Carry-over makes the audit's recommendations look optional.
3. **Append a "Hardening passes since extraction" subsection to lock_guard.rs preamble.** Three commits, one sentence each.
4. **Mechanical sweep on `v8_classes/transaction.rs`** — replace TX_CONN/TX_TOKEN with `IsolateDbContext::tx_conn` / `tx_token`. Fixes 4 rustdoc-broken intra-doc links + ~12 prose sites in one file.
5. **Update replication.rs:751-757 test docstring** to frame as a historical-shape regression guard instead of describing a current `?`-flow.
6. **One-line `audit.rs:5` fix**: drop "(future)".
