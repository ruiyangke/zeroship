# plugin-db docs audit — round 5 (2026-05-22)

**Scope:** `crates/plugin-db/src/` preambles + inline comments, plus the
`docs/reference/db.md` / `docs/reference/plugin-system.md` / `AGENTS.md`
surfaces that point into this crate.

**Prior rounds:** r1 (68), r2 (74), r3 (80), r4 (cycle 05:25 — 75/100).

This pass re-audits after the four commits in the brief:

- `f7d0961c` — error.rs preamble rewrite (closes r4 NEW CRITICAL).
- `cbbc9059` — dedupe `coded_sql` / `prefix_message` across 5 sites;
  `replication.rs::prefix_message` dropped entirely; audit/auth-*/diff
  become thin module-prefixed wrappers over `crate::error::coded_sql`.
- `51ced4a0` — lock_guard.rs preamble Hardening-history block;
  finalise_backfill warn-on-err.
- `34d209b5` — `ConsumerRunningGuard::new` now performs the mark INSIDE
  the spawned future (atomic mark+unmark lifecycle vs the old "mark
  before spawn" sequence).

**TL;DR.** Three of r4's four NEW findings are cleanly closed:

- error.rs:9-19 preamble inventory — rewritten by `f7d0961c` to the
  narrow, accurate hold-out set (validate stage + `hex_decode` /
  `hex_nibble`). +1 step backward versus reality is now +0; one
  qualifier missing (see Dimension 3, IMPORTANT).
- lock_guard.rs preamble — Hardening-history block added by `51ced4a0`
  (cbd12944 / bd1e7ce1 / 808a32af / ffb1e101 enumerated with the bug
  class each closed). Matches the body comments exactly. Clean close.
- The `cbbc9059` dedupe leaves audit / auth-{bootstrap,keys,session} /
  diff / replication routed through one shared `crate::error::coded_sql`;
  the per-file preambles correctly document the thin-wrapper shape
  ("variant-walking is shared … this is the `auth/bootstrap`-scoped
  thin wrapper"). Replication's old in-file `prefix_message` is gone;
  replication.rs:52 imports the shared one.

But — **`34d209b5` introduced a new inline-comment drift** the brief
called out: the 4-line block at `replication_ops.rs:255-257` ("Mark the
app as running BEFORE the spawn so a racing second call …") describes
the *pre*-34d209b5 sequence. The actual code now marks inside the
spawned future via `ConsumerRunningGuard::new`; the lines below
(`264-276`) describe the new behaviour correctly, but the contradiction
lives in the same comment block. New CRITICAL this round.

The five r4 hold-outs are entirely unchanged: v8_classes/transaction.rs
TX_CONN/TX_TOKEN drift, orchestrator/mod.rs pub-vs-pub(crate),
query.rs:445 composite-indexes TODO, migrations.rs:78 duplicate doc
summary, audit.rs:5 "(future) backfill". The `replication.rs` test
docstring at the (now-relocated) line 726 also still claims a return
shape that no longer exists.

One r4 IMPORTANT closed cleanly:

- error.rs:10 "(see backlog [I28])" inline reference is gone — the new
  preamble cites the closed commit `0049d9be` instead of an open
  backlog ticket. +1.

---

## Dimension-by-dimension findings

### 1. Stale historical names — TX_CONN / TX_TOKEN / MIG_LOCK / callbacks.rs sweep

```
[CRITICAL] crates/plugin-db/src/v8_classes/transaction.rs:68,161,192,214 — rustdoc intra-doc links to deleted symbols (UNCHANGED FROM r3 / r4)
  Why: `[`crate::TX_TOKEN`]` and `[`crate::TX_CONN`]` resolve to nothing — the symbols folded into `IsolateDbContext` in Stage 8d-R4. `cargo doc` emits a broken-link warning per build. Flagged r3, r4; no follow-up touched this file.
  Fix: Replace `[`crate::TX_TOKEN`]` → `IsolateDbContext::tx_token`; same for TX_CONN.
  Verification: grep -n "crate::TX_CONN\|crate::TX_TOKEN" crates/plugin-db/src/v8_classes/transaction.rs

[IMPORTANT] crates/plugin-db/src/v8_classes/transaction.rs:68-71,107-116,161,192,214-218,278,284-285,324 — bare TX_CONN / TX_TOKEN tokens in prose (UNCHANGED FROM r3 / r4)
  Why: ~14 sites in this one file still spell "TX_CONN" / "TX_TOKEN" as live symbols. Sibling files (orchestrator/transaction.rs preamble, v8_classes/mod.rs, orchestrator/mod.rs) use "IsolateDbContext::tx_conn" — inconsistency is across-file, easy to chase the wrong term.
  Fix: Mechanical s/TX_TOKEN/tx_token/, s/TX_CONN/tx_conn/.
  Verification: grep -cn "TX_CONN\|TX_TOKEN" crates/plugin-db/src/v8_classes/transaction.rs

[IMPORTANT] crates/plugin-db/src/v8_classes/migration.rs:216 — `MIG_LOCK` thread-local reference (UNCHANGED FROM r3 / r4)
  Why: "checks the `MIG_LOCK` thread-local for ownership / cancellation". The lock lives on `IsolateDbContext::mig_lock`; no top-level MIG_LOCK exists.
  Fix: s/`MIG_LOCK` thread-local/`IsolateDbContext::mig_lock` slot/.
  Verification: grep -n "MIG_LOCK" crates/plugin-db/src/v8_classes/migration.rs

[MINOR] crates/plugin-db/src/lib.rs:110,216 — caps-name idiom drift (UNCHANGED FROM r3 / r4)
  Why: Doc on `crate::next_tx_token_for_tests` reads "Allocate a fresh non-zero TX_TOKEN value"; on `clear_mig_lock_for_tests` reads "clear `MIG_LOCK` for the current thread". lib.rs:251 already models the corrected idiom ("isolate's `IsolateDbContext::tx_conn` slot (formerly the `TX_CONN` thread-local)") — apply the same shape here.
  Fix: Either retitle to `tx_token` / `mig_lock` slot terms, or append "(formerly …)" footnote.
  Verification: grep -n "TX_TOKEN\|MIG_LOCK" crates/plugin-db/src/lib.rs

[MINOR] crates/plugin-db/src/crud.rs:53 — "the active TX_CONN" (UNCHANGED FROM r3 / r4)
  Why: Same drift class on the hottest CRUD path.
  Fix: s/the active TX_CONN/the active tx connection (IsolateDbContext::tx_conn)/.
  Verification: grep -n "TX_CONN" crates/plugin-db/src/crud.rs

[MINOR] crates/plugin-db/src/exec.rs:329 — test-helper doc references "TX_CONN" (UNCHANGED FROM r3 / r4)
  Why: `exec_mutation_with_emit_for_tests` doc says "setting `TX_CONN`"; the setter is `install_tx_marker_for_tests` which writes to `IsolateDbContext::tx_conn`.
  Fix: s/setting `TX_CONN`/installing the tx-conn slot/.
  Verification: grep -n "TX_CONN" crates/plugin-db/src/exec.rs

[MINOR] crates/plugin-db/src/orchestrator/transaction.rs:51,52,89,143 — inline "TX_TOKEN" / "TX_CONN" tokens (UNCHANGED FROM r3 / r4)
  Why: Preamble (lines 1-14) is correct; inline comments inside the impl lapse back to constant names.
  Fix: s/TX_TOKEN/tx_token/, s/TX_CONN/tx_conn/ for the four sites.
  Verification: grep -n "TX_CONN\|TX_TOKEN" crates/plugin-db/src/orchestrator/transaction.rs

[MINOR] crates/plugin-db/src/backend/mod.rs:66 — "in a thread-local (e.g. `MigrationLock::client`, `tx_conn`)" (UNCHANGED FROM r3 / r4)
  Why: Both items live in `RefCell<IsolateDbContext>`, not thread-locals. Trait doc — externally visible.
  Fix: "in the per-isolate context (e.g. `MigrationLock::client`, `IsolateDbContext::tx_conn`)".
  Verification: grep -n "thread-local" crates/plugin-db/src/backend/mod.rs
```

`callbacks.rs` was deleted in Stage 8b. `grep -rn "callbacks\.rs\|crate::callbacks" crates/plugin-db/src/` finds two live references:

```
[OK] orchestrator/mod.rs:24 — "`crate::callbacks` was deleted in Stage 8b" — historical reference in present-tense narrative, correctly framed as past-tense.
```

No new stale `callbacks.rs` mentions surfaced.

**Net change vs r4:** zero. d53f90b0's sweep still hasn't reached
v8_classes. Surface-level grep hits stay at ~16 across 7 files.

### 2. Recent commits' inline comment accuracy

#### `// [I42]` and `// [I44]` references in lock_guard.rs

```
[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:156-159 — `[I42]` annotation accurate
  Why: Comment reads "the prior version flipped `released = true` BEFORE the await, so a cancellation here silently leaked the lock with no Drop log. Defer the state flip to AFTER the await completes." This matches commit `bd1e7ce1` ("defer released-flag flip to AFTER unlock await") exactly. The annotation lives next to the `.await` line followed by `self.released = true` on line 181 — accurate placement.
  Verification: git show bd1e7ce1 -- crates/plugin-db/src/orchestrator/lock_guard.rs

[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:163-167 — `[I44]` annotation accurate
  Why: "a bare `let _ =` silently swallows runtime errors from the unlock SQL — operator never sees that the lock might still be held. Log warnings on error so a leak is visible". Followed by `if let Err(e) = client.query_text_params(...).await { tracing::warn!(...) }`. Matches commit `ffb1e101`.
  Verification: git show ffb1e101 -- crates/plugin-db/src/orchestrator/lock_guard.rs

[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:212-235 — Drop log accurate
  Why: The `tracing::error!` body has "leak:" prefix, the operator-facing consequence ("Concurrent register_model callers for this app will stall in the meantime") and the diagnostic checklist (async-cancellation / panic / missed release). Matches the [I39] commit body verbatim.
  Verification: sed -n '212,236p' crates/plugin-db/src/orchestrator/lock_guard.rs
```

#### `// MAJOR-R6-1` references in replication_ops.rs (34d209b5)

```
[CRITICAL] crates/plugin-db/src/replication_ops.rs:255-257 — comment block contradicts the code below it
  Why: The first paragraph reads
  > "Mark the app as running BEFORE the spawn so a racing second call to startReplicationConsumer() short-circuits even if the consumer task hasn't yet entered its decode loop."
  This describes the *pre*-34d209b5 sequence (mark synchronous, BEFORE the spawn). The actual code now marks inside the spawned future:
  ```rust
  // (line 297)
  let _guard = ConsumerRunningGuard::new(app_for_task);
  ```
  Lines 264-276 (the MAJOR-R6-1 paragraph) correctly describe the new behaviour: "a previous version called `mark_consumer_running` synchronously BEFORE the spawn … Move both mark + unmark inside the guard so the lifecycle is atomic with the future's existence." But the surrounding comment block leads with the now-incorrect rationale before pivoting to the corrected one. New reader on line 255-257 walks away with the wrong mental model.
  Fix: Drop lines 255-257 (the pre-34d209b5 framing). The block at 259-276 already captures the rationale; remove the contradicted lede so the comment block opens with "Code-critique r5 MAJOR-R5-2: …".
  Verification: sed -n '250,300p' crates/plugin-db/src/replication_ops.rs

[OK] crates/plugin-db/src/replication_ops.rs:264-276 — MAJOR-R6-1 paragraph accurate
  Why: Explicitly names the failure mode `34d209b5` closed (spawn-or-future-first-poll panic leaving the mark stuck), the new atomic-lifecycle pattern (`ConsumerRunningGuard::new` marks, `Drop` unmarks), and the race-window justification (compio runs callbacks single-threaded per isolate). Mirrors the commit message body.
  Verification: sed -n '264,276p' crates/plugin-db/src/replication_ops.rs

[OK] crates/plugin-db/src/replication_ops.rs:295-301 — guard-binding pattern + drop note
  Why: `let _guard = ConsumerRunningGuard::new(app_for_task);` followed by `// _guard drops here on graceful exit; Drop also fires on panic-unwind, so the running marker is always cleared.` Both the constructor name and the lifecycle claim match the impl above (lines 280-294). The Drop guard fully covers the atomic-lifecycle claim in the MAJOR-R6-1 comment.
  Verification: sed -n '277,302p' crates/plugin-db/src/replication_ops.rs
```

#### `// [I28]` references after the cbbc9059 dedupe

```
[OK] crates/plugin-db/src/auth/{bootstrap,keys,session}.rs — "Typed-error sweep [I28]" test-block headers
  Why: Each file's "[I28]" test block introduces a compile-time signature guard pinning the function's Result error type as `DbError`. Headers reference commit `0049d9be`; contract description consistent across three files. cbbc9059 did not touch the test bodies.
  Verification: grep -n "Typed-error sweep" crates/plugin-db/src/auth/

[OK] crates/plugin-db/src/replication.rs:813-… — [I28] tests for sanitise_app_id / publication_name / slot_name + `prefix_message` variant preservation
  Why: Unit tests still pin `.code` for each helper + the variant-preservation contract for the shared `prefix_message` (now imported from `crate::error`). Header references the right backlog ticket. cbbc9059 dropped the local copies but the contract tests at error.rs::tests::prefix_message_* still cover the invariant (3 tests live there now per the cbbc9059 commit message).
  Verification: grep -n "Typed-error sweep" crates/plugin-db/src/replication.rs
```

### 3. lock_guard.rs Hardening history block (51ced4a0)

```
[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:51-69 — Hardening history block accurate and complete
  Why: Lists the four commits in chronological order with the bug class each closed:
    1. `cbd12944` — extract guard from 3 open-coded sites
    2. `bd1e7ce1` (cycle 04:00, [I42]) — defer `released = true` flip until AFTER the unlock-SQL await
    3. `808a32af` (cycle 04:35, [I39]) — `#[must_use]` + Drop log "leak:" prefix
    4. `ffb1e101` (cycle 04:35, [I44]) — `if let Err(e) =` + `tracing::warn!` on unlock-SQL failure
  Each bullet correctly attributes the commit, the cycle, and the bug class.
  Cross-validation:
    - `git log --oneline -- crates/plugin-db/src/orchestrator/lock_guard.rs` shows exactly those four commits (plus 51ced4a0 itself).
    - The body of the file matches: the [I42] annotation at line 156, the [I44] annotation at line 163, the `#[must_use]` at line 86-87, the Drop log at line 227 all line up with the Hardening-history entries.
  Single suggestion (NIT): The block could explicitly note that the in-file [I42]/[I44] annotations are the matching body comments; readers skimming the preamble might miss the cross-reference. Optional.
  Verification: git log --oneline -- crates/plugin-db/src/orchestrator/lock_guard.rs; sed -n '51,69p' crates/plugin-db/src/orchestrator/lock_guard.rs

[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:1-50 — preamble Why-not-full-RAII section unchanged
  Why: The cbd12944-era prose ("Why not full RAII?", three exit modes, internal representation) is intact. 51ced4a0 appended the Hardening-history block AFTER this prose without touching it — the right surgery.
  Verification: sed -n '25,50p' crates/plugin-db/src/orchestrator/lock_guard.rs
```

This **closes r4's IMPORTANT** ("lock_guard.rs preamble silent on
[I42] / [I39] / [I44]"). The replacement is exactly the shape r4
recommended.

### 4. ConsumerRunningGuard inline comment (34d209b5)

```
[OK] crates/plugin-db/src/replication_ops.rs:280-286 — `ConsumerRunningGuard::new` body
  Why: The constructor performs the mark, returns the guard. Body matches the commit's claim of atomic mark+unmark via the constructor.
  Verification: sed -n '280,286p' crates/plugin-db/src/replication_ops.rs

[OK] crates/plugin-db/src/replication_ops.rs:288-294 — Drop impl
  Why: `Drop` calls `unmark_consumer_running(&self.app_id)`. Matches the lifecycle claim (any exit path — graceful, panic, future dropped — clears the mark).
  Verification: sed -n '288,294p' crates/plugin-db/src/replication_ops.rs

[ISSUE — see Dimension 2 CRITICAL above] crates/plugin-db/src/replication_ops.rs:255-257
  The "Mark the app as running BEFORE the spawn …" lede contradicts the constructor placement at line 297. Same finding; not duplicated here.
```

### 5. error.rs preamble (f7d0961c) — still accurate after cbbc9059?

```
[OK] crates/plugin-db/src/error.rs:1-28 — preamble accurate post-f7d0961c, survives cbbc9059
  Why: The rewritten preamble enumerates the narrow set of remaining `Result<_, String>` hold-outs:
    - validate stage (wire-contract JSON envelope)
    - `hex_decode` / `hex_nibble` in `auth/session.rs`
  Cross-validated:
    - `grep -rn "Result<.*, String>"` finds zero production sites outside that pair PLUS three v8-arg-parser holdouts (see IMPORTANT below) and one init helper.
    - cbbc9059 added `crate::error::coded_sql` and `crate::error::prefix_message` (pub(crate) — lines 327, 357) without altering the preamble's claim set; the new helpers are documented separately at lines 311-326 and 347-356 with their own preambles.
  Verification: sed -n '1,28p' crates/plugin-db/src/error.rs

[IMPORTANT] crates/plugin-db/src/error.rs:9-23 — inventory misses three categories of legitimate hold-outs
  Why: The preamble says "Every fallible helper that touches Postgres or the V8 boundary now returns `Result<_, DbError>`." The honest claim is narrower:
    - `v8_classes/migration.rs:451` (`parse_commit_spec`), `:739` (`parse_spec`) — JS-input parsers that return `Result<_, String>` and the caller rejects with `v8::Exception::type_error`. These touch the V8 boundary but the throw path is intentionally a bare `TypeError` (no `.code` needed — TypeErrors are caught by the SDK's input-validation layer, not the err.code branch).
    - `v8_classes/migrations.rs:218` (`parse_name_and_collection`) — same pattern.
    - `lib.rs:349` (`init_pool_async`) — one-shot crate init helper used by integration tests; never reached at the V8 boundary in production (the runtime calls `init_pool_blocking_on_compio`).
  Net impact: a reader trying to migrate one of those parsers to `DbError` finds no guidance — should they? The preamble's "every fallible helper" framing implies yes. The honest answer is "no, these are JS-input parsers that throw `TypeError` and that's intentional".
  Fix: Add a third bullet point after `hex_decode` / `hex_nibble`:
    "- A handful of JS-input parsers (`parse_commit_spec`, `parse_spec`, `parse_name_and_collection` in `v8_classes/migration{,s}.rs`) — these throw `TypeError` for shape violations at the V8 boundary, not a coded `DbError`. The SDK's input-validation layer handles them; `.code`-branching does not apply."
  Verification: grep -rn "Result<.*, String>" crates/plugin-db/src/ | grep -v test

[OK] crates/plugin-db/src/error.rs:311-326 — `prefix_message` helper preamble
  Why: Explicitly enumerates "in `audit`, `auth::bootstrap`, `auth::keys`, `auth::session`, `diff`, or `replication`" and names the variant set the helper walks (`UniqueViolation`, `FkViolation`, `NotNullViolation`, `CheckViolation`, `Serialization`, `LockContention`, `Transient`, `Internal`) versus the structured-variants it skips (`ValidationFailed`, `Configuration`, `Coded`, `SchemaRefused`). The "preserving the wire format" rationale is explicit. Cross-validated against the impl (lines 327-345) — the documented variant set matches the match-arm verbatim.
  Verification: sed -n '311,346p' crates/plugin-db/src/error.rs

[OK] crates/plugin-db/src/error.rs:347-361 — `coded_sql` helper preamble
  Why: Names the per-file duplicates it replaced ("audit, auth::bootstrap, auth::keys, auth::session, and diff"). Documents how callers compose the module prefix into the context phrase. Cross-validated:
    - `audit.rs:58` reads `crate::error::coded_sql(&format!("audit: {context}"), e)` — matches.
    - `auth/bootstrap.rs:24-26` reads `crate::error::coded_sql(&format!("auth/bootstrap: {context}"), e)` — matches.
    - `diff.rs:40` reads `crate::error::coded_sql(&format!("diff: {context}"), e)` — matches.
    - `replication.rs` does NOT have a local `coded_sql` wrapper post-cbbc9059; the file calls `prefix_message` directly with its inline `"replication: <op>: "` prefix (verified at replication.rs:203, 215, 231, 268, 398, 540, 570, 602). This is consistent with the cbbc9059 commit message claim that "replication.rs's prefix_message is dropped entirely in favor of the crate::error import" — replication consumes `prefix_message`, not `coded_sql`, because its callers already have a `DbError` by the time they want a prefix (they don't start from `compio_postgres::Error`).
  Suggestion (NIT): the `coded_sql` preamble could acknowledge replication is the one site that does NOT use this helper (uses `prefix_message` directly instead). One-sentence add-on.
  Verification: grep -n "crate::error::coded_sql\|crate::error::prefix_message" crates/plugin-db/src/
```

### 6. prefix_message + coded_sql + first_row_or_internal preambles — exemplary or drift-prone?

```
[OK] crates/plugin-db/src/error.rs:311-326 — `prefix_message` is exemplary
  Why: Names the call sites by file path (six modules), names the structured-variant skip set verbatim, names the rationale ("prefixing them would distort a wire payload the SDK parses verbatim"). Drift surface is low — if a new variant were added, the variant table at lines 30-45 + this match would both need updating, but they're in the same file.
  Verification: sed -n '311,346p' crates/plugin-db/src/error.rs

[OK] crates/plugin-db/src/error.rs:347-361 — `coded_sql` is exemplary
  Why: Names the per-file thin wrappers ("audit, auth::bootstrap, auth::keys, auth::session, and diff"). Single drift risk: replication.rs is omitted; see Dimension 5 NIT above.
  Verification: sed -n '347,361p' crates/plugin-db/src/error.rs

[OK] crates/plugin-db/src/error.rs:363-385 — `first_row_or_internal` is exemplary
  Why: Names the bug class ("silent-empty-RETURNING"), the canonical commit (`d7cfc089`), the sibling case (replication-slot empty-LSN twin closed via `eda96ead`), the unified shape (`"<op>: returned no row"`), the regression test (`audit.rs::tests::insert_backfill_running_empty_returning_is_internal_error`). Drift surface low — the function signature is small and the bug class is closed.
  Verification: sed -n '363,386p' crates/plugin-db/src/error.rs

[CRITICAL — UNCHANGED FROM r3 / r4] crates/plugin-db/src/migrations.rs:67-81 — `coded_db` doc block has a duplicated first-paragraph summary
  Why: The doc comment runs `///` from line 67 through line 81 as one block but contains TWO distinct opening sentences:
    - line 67: "SQL-error helper — classify the Postgres error through `DbError` …"
    - line 78: "Stamp a `DbError` with a context phrase and convert to `OpError`."
  Each opens a new doc paragraph without a `///` blank-line separator; rustdoc renders the second sentence inline after the first. Most likely a dec2bd42 / ed697c45 restore artefact.
  Also: line 79 reads "Replaces the previous `coded_sql(context, compio_postgres::Error)` helper" — true of THIS file (migrations.rs no longer defines its own `coded_sql`), but the same name is alive in `crate::error::coded_sql` (post-cbbc9059) plus thin wrappers in audit/auth-{bootstrap,keys,session}/diff. A scope qualifier ("in this file") would prevent a reader from concluding `coded_sql` is gone crate-wide.
  Additional note (cbbc9059 follow-up): `coded_db`'s body (lines 82-102) reimplements the same SQLSTATE-variant match arm as `crate::error::prefix_message`. The function signature differs (`DbError → OpError` vs the helper's `&mut DbError`), but the prefix-message logic could be shared via `crate::error::prefix_message(&mut db_err, &format!("{context}: "))`. Not a docs issue — but the doc claim "Replaces the previous `coded_sql` helper" undersells the actual relationship.
  Fix: Either (a) drop line 78 entirely + add "in this file" to line 79, OR (b) split into two `/// ` paragraph-separated blocks with the line-78 sentence as a follow-up that names the relationship to `crate::error::coded_sql` / `prefix_message`.
  Verification: sed -n '67,82p' crates/plugin-db/src/migrations.rs
```

### 7. AGENTS.md task router — file paths

```
[OK] Every plugin-db-relevant row in AGENTS.md resolves on HEAD:
  - "**Adding a native primitive** … `docs/reference/plugin-system.md` · `crates/runtime-macros/` · `crates/plugin-{db,kv,storage}/`" — all four exist
  - "**The DB SDK** (`@zeroship/db`) … `docs/reference/db.md` · `crates/plugin-db/`" — both exist
  - "**ZS deploy contract** … `sdks/bootstrap/src/{dispatcher,runtime-entry}.ts` · `crates/runtime/src/core/init.rs`" — all three exist
  Verification: ls docs/reference/{db,plugin-system}.md crates/plugin-{db,kv,storage} crates/runtime-macros sdks/bootstrap/src/{dispatcher,runtime-entry}.ts crates/runtime/src/core/init.rs

[NOTE — out of scope, carry-over from r3/r4] docs/reference/plugin-system.md:315-344 — "Crate structure" tree still stale (UNCHANGED FROM r3 / r4)
  Why: AGENTS.md row 3 sends "adding a native primitive" readers here, and the tree still lists non-existent paths:
    - crates/runtime/src/init.rs           → today: crates/runtime/src/core/init.rs
    - crates/runtime/src/runtime.rs        → does not exist
    - crates/runtime/src/plugin.rs         → does not exist
    - crates/plugin-db/src/callbacks.rs    → deleted Stage 8b
    - crates/plugin-db/src/validate.rs     → exists only at orchestrator/register_model/validate.rs
    - crates/plugin-db/src/migrate.rs      → does not exist (today: migrations.rs + audit.rs)
    - crates/plugin-auth/                  → does not exist (auth lives in plugin-db/src/auth/)
    - crates/pg/                           → renamed to compio-postgres
  Same finding as r3 / r4; not adjacent enough to be in-scope (brief is plugin-db), but the entry-point experience for new contributors is degraded — three rounds of carry-over without a fix is a signal.
  Verification: ls crates/runtime/src/init.rs crates/plugin-db/src/callbacks.rs crates/plugin-auth/ crates/pg/ → all ENOENT
```

### 8. r4 hold-outs

```
[CRITICAL — UNCHANGED FROM r3 / r4] crates/plugin-db/src/v8_classes/transaction.rs (TX_CONN / TX_TOKEN drift)
  Covered as Dimension 1 CRITICAL + IMPORTANT above. Zero commits since r4 touched this file.
  Verification: git log --oneline -- crates/plugin-db/src/v8_classes/transaction.rs | head -3

[IMPORTANT — UNCHANGED FROM r3 / r4] crates/plugin-db/src/orchestrator/mod.rs:22 — "Each submodule is `pub(crate)` to scope visibility"
  Why: Look at lines 28-31:
  ```rust
  pub mod auto_tx;
  pub(crate) mod lock_guard;
  pub mod register_model;
  pub mod transaction;
  ```
  Three of four are `pub`, not `pub(crate)`. The effective visibility is pub(crate) because lib.rs:84 has `pub(crate) mod orchestrator;` under `#[cfg(not(feature = "test-helpers"))]` (line 86 promotes to `pub` for test-helpers builds — also worth noting in the prose). Three rounds flagged the same drift; no follow-up commit.
  Fix: Either (a) downgrade three `pub mod` → `pub(crate) mod` (matches the comment + cbd12944's lock_guard choice), or (b) rewrite the comment: "Each submodule is `pub` to the orchestrator parent; the parent is `pub(crate)` under default features and `pub` under `test-helpers` in `lib.rs:84-86`."
  Verification: grep -n "^pub" crates/plugin-db/src/orchestrator/mod.rs

[IMPORTANT — UNCHANGED FROM r4] crates/plugin-db/src/replication.rs:726-732 — test docstring claims a return type that no longer exists
  Why: After cbbc9059, the file no longer has its own `prefix_message`; `ensure_publication_and_slot` returns `Result<SetupOutcome, DbError>` (verified by reading `pub async fn ensure_publication_and_slot` around lines 188-340). But the docstring above `empty_returning_string_shape_keeps_replication_prefix` reads:
  > "`ensure_publication_and_slot` returns `Result<_, String>` (not `Result<_, DbError>` like audit.rs), so the runtime fix calls `.into_string()` on the `DbError::Internal` before flowing it through `?`."
  Both clauses are false post-[I28]. The test below still has value (pins the operator-facing message body the legacy code emitted) — log scrapers / dashboards key off the `"replication:"` prefix + operation tag — but the docstring describes a `?`-flow that does not exist. Was flagged at line 751 in r4; cbbc9059's removals reshuffled to line 726, doc still untouched.
  Fix: Reframe as historical-shape regression guard:
    "Historic shape: `ensure_publication_and_slot` once returned `Result<_, String>` and the runtime called `.into_string()` on the typed error. Post-[I28] (`0049d9be`) the function returns `Result<_, DbError>` directly. This test pins the operator-facing *message body* (`replication:` prefix + operation tag) so a future refactor can't drop those substrings — log scrapers / dashboards key off them."
  Verification: grep -n "fn ensure_publication_and_slot\|Result<.*, String>" crates/plugin-db/src/replication.rs

[CRITICAL — UNCHANGED FROM r3 / r4] crates/plugin-db/src/query.rs:443-446 — "TODO: A1 composite indexes"
  Why: Composite indexes ARE wired up. `build_named_indexes` at query.rs:529 is called from `orchestrator/register_model/bootstrap.rs:175`; the SDK builder is live. The TODO + the surrounding "not yet surfaced by the SDK" prose predate the ship.
  Fix: Either delete the TODO (composite indexes are present-tense) or rewrite to be specific:
    "TODO(A1): the alternative `schema._meta.indexes` declaration form (today indexes are passed as a separate `registerModel(coll, schema, indexes)` arg)."
  Verification: grep -n "build_named_indexes" crates/plugin-db/src/orchestrator/register_model/bootstrap.rs

[MINOR — UNCHANGED FROM r3 / r4] crates/plugin-db/src/migrations.rs:78 — stray "duplicate doc summary" line
  Covered as Dimension 6 CRITICAL (the artefact rises in severity because cbbc9059's relationship is now part of the picture). See Dimension 6.
  Verification: sed -n '67,82p' crates/plugin-db/src/migrations.rs

[MINOR — UNCHANGED FROM r3 / r4] crates/plugin-db/src/audit.rs:5 — "(future) backfill writes a row"
  Why: B1 backfill orchestrator ships and writes Backfill-phase rows (see `Phase::Backfill` at audit.rs:97-99 and the migrations.rs B1 functions including the new `finalise_backfill` warn path landed in `51ced4a0`). The "(future)" parenthetical predates the B1 ship by ~30 commits and is now THREE rounds stale.
  Fix: Drop "(future)" — backfill is present-tense.
  Verification: grep -n "future" crates/plugin-db/src/audit.rs

[CLOSED — was the r4 IMPORTANT-from-r3 lock_guard hardening-history] crates/plugin-db/src/orchestrator/lock_guard.rs:51-69
  Why: Added by 51ced4a0. See Dimension 3 OK.
```

### 9. New since r4

```
[CRITICAL] crates/plugin-db/src/replication_ops.rs:255-257 — comment block opens with stale rationale
  Why: The 34d209b5 commit moved the mark into `ConsumerRunningGuard::new`. The comment block at lines 255-276 was edited to add the MAJOR-R6-1 paragraph but kept the original "Mark the app as running BEFORE the spawn so a racing second call to startReplicationConsumer() short-circuits" lede from the e399eeea (pre-34d209b5) era. The lede contradicts the MAJOR-R6-1 paragraph that follows it.
  Fix: Drop lines 255-257 entirely so the block opens with "Code-critique r5 MAJOR-R5-2: …" (the unmark-on-panic motivation, which is still accurate).
  Verification: sed -n '250,280p' crates/plugin-db/src/replication_ops.rs

[IMPORTANT] crates/plugin-db/src/error.rs:9-23 — preamble's "Every fallible helper that touches … the V8 boundary now returns `Result<_, DbError>`" misses three JS-input-parser hold-outs
  Why: `v8_classes/migration.rs:{451, 739}` and `v8_classes/migrations.rs:218` still return `Result<_, String>` — JS-input-arg parsers that throw `TypeError` via `v8::Exception::type_error`. Plus `lib.rs:349 init_pool_async` returns `Result<(), String>` (one-shot init helper, not a V8 boundary). The preamble's claim is *almost* right; the new hold-outs are a legitimate "throw TypeError, don't carry .code" category.
  Fix: Add a third bullet (suggested wording in Dimension 5 IMPORTANT). Preserves the "production code path" framing while being honest about the parsers.
  Verification: grep -rn "Result<.*, String>" crates/plugin-db/src/ | grep -v test

[OK — for context] cbbc9059 dedupe — clean execution
  Why: The shared `prefix_message` / `coded_sql` helpers in `crate::error` are pub(crate), exhaustively documented, and the per-file callers thread through them correctly. The new test bodies live at `error.rs::tests::prefix_message_*` (3 tests pinning the variant-preservation contract). audit / auth-{bootstrap,keys,session} / diff each compose their module prefix into the context phrase. replication.rs imports `prefix_message` directly (no local `coded_sql` wrapper — its callers always have a `DbError` already). The cbbc9059 commit message claim matches the on-disk shape.

[OK] crates/plugin-db/src/migrations.rs:638-657 — finalise_backfill warn-on-err comment + tracing call
  Why: Comment (lines 639-643) names the F1-family discipline regression and the operator-facing reason; `tracing::warn!` body includes `app_id`, `audit_id`, `terminal`, `error` fields plus a free-text hint ("audit row may stay in 'running' status until next reset() — investigate if the operator sees stuck migrations"). Matches the 51ced4a0 commit message verbatim.
  Verification: sed -n '639,658p' crates/plugin-db/src/migrations.rs
```

### 10. Carry-over no-action items

```
[MINOR — UNCHANGED] crates/plugin-db/src/audit.rs:5 — see Dimension 8.
[MINOR — UNCHANGED] crates/plugin-db/src/lib.rs:110,216 — see Dimension 1.
[MINOR — UNCHANGED] crates/plugin-db/src/crud.rs:53 — see Dimension 1.
[MINOR — UNCHANGED] crates/plugin-db/src/exec.rs:329 — see Dimension 1.
[MINOR — UNCHANGED] crates/plugin-db/src/backend/mod.rs:66 — see Dimension 1.
[MINOR — UNCHANGED] crates/plugin-db/src/orchestrator/transaction.rs:51,52,89,143 — see Dimension 1.
```

---

## What got cleanly fixed since r4

- **r4 NEW CRITICAL — error.rs:9-19 preamble inventory stale**: closed
  at `f7d0961c`. The new preamble enumerates the actual narrow set of
  hold-outs (validate stage + `hex_decode` / `hex_nibble`). Cites the
  closing commit `0049d9be` instead of an open backlog reference.
- **r4 CONTRADICTION — error.rs:10 "(see backlog [I28])"**: closed at
  `f7d0961c`. New preamble cites the closed commit.
- **r4 IMPORTANT — lock_guard.rs preamble silent on [I42] / [I39] /
  [I44]**: closed at `51ced4a0`. Hardening-history block lists all
  four commits in chronological order with the bug class each closed.
- **cbbc9059 dedupe**: clean execution — five per-file `coded_sql`
  duplicates collapse into one shared `crate::error::coded_sql`; the
  per-file thin-wrapper preambles correctly document the relationship;
  replication.rs's local `prefix_message` is gone in favour of the
  shared `crate::error::prefix_message`; 3 contract tests pin the
  variant-preservation invariant; no functional change.
- **51ced4a0 finalise_backfill warn**: the comment (lines 639-643)
  names the F1-family discipline regression that motivated it; the
  `tracing::warn!` body matches the commit message exactly.

## What did NOT change since r4

- **`v8_classes/transaction.rs` TX_CONN/TX_TOKEN drift**: ~14 prose
  sites + 4 rustdoc-broken intra-doc links remain. THREE rounds without
  a sweep.
- **`orchestrator/mod.rs:22` `pub` vs `pub(crate)` mismatch**: THREE
  rounds. Three of four submodules still declared `pub` while the
  preamble claims `pub(crate)`.
- **`query.rs:443-446` composite-indexes TODO**: THREE rounds.
  `build_named_indexes` shipped + wired in `orchestrator/register_model/bootstrap.rs:175`.
- **`audit.rs:5` "(future) backfill"**: THREE rounds. B1 backfill
  shipped and `51ced4a0` literally added a new error-handling site on
  the backfill code path while leaving the comment "(future)".
- **`migrations.rs:78` stray duplicate doc summary**: THREE rounds.
  dec2bd42 restore artefact.
- **`lib.rs:110/216` caps-name idiom drift**: THREE rounds.
- **`crud.rs:53` / `exec.rs:329` / `backend/mod.rs:66` thread-local
  references**: THREE rounds. Six files, same drift class.
- **`replication.rs:726-732` test docstring `Result<_, String>` claim**:
  flagged at r4 line 751; cbbc9059 moved it to line 726; still uncorrected.

## NEW since r4

- **CRITICAL (Dimension 2 / 9) — `replication_ops.rs:255-257`**: the
  34d209b5 commit moved the mark inside `ConsumerRunningGuard::new`
  but left the "Mark the app as running BEFORE the spawn …" lede
  in the surrounding comment block. The block now contains both
  contradictory rationales. New reader on line 255 walks away with
  the wrong mental model; the corrected paragraph at line 264-276
  reads as if it's adding to (rather than replacing) the lede.
- **IMPORTANT (Dimension 3 / 5 / 9) — `error.rs:9-23` "every fallible
  helper that touches … the V8 boundary"**: misses three JS-input-parser
  sites (`parse_commit_spec`, `parse_spec`, `parse_name_and_collection`)
  + `init_pool_async`. Legitimate hold-outs (throw `TypeError`, not
  `.code`-bearing `DbError`); preamble doesn't acknowledge the category.

---

## Score: 81 / 100  (r4: 75)

**Delta breakdown (+6 from r4):**

- +5 — `f7d0961c` closed the r4 NEW CRITICAL (error.rs:9-19 preamble
  inventory). The new preamble is the cleanest narrow-hold-out
  enumeration in the crate; explicitly names the wire-contract
  rationale for validate.rs and the pure-function reason for
  `hex_decode` / `hex_nibble`.
- +3 — `51ced4a0` closed the r4 IMPORTANT (lock_guard.rs preamble
  silent on [I42] / [I39] / [I44]). Hardening-history block lists all
  four commits with the bug class each closed; the in-file
  annotations cross-reference cleanly.
- +2 — `cbbc9059` dedupe shipped without introducing new docs drift
  in the per-file thin wrappers; the shared `prefix_message` /
  `coded_sql` preambles are exemplary (name the call sites, the
  variant skip set, the rationale).
- +1 — `34d209b5`'s body content (MAJOR-R6-1 paragraph + Drop guard
  drop note + guard struct) is accurate even though the surrounding
  comment block has a contradicted lede.
- −2 — NEW CRITICAL: `replication_ops.rs:255-257` contradiction with
  the rest of the comment block. The same revision that fixed the
  underlying behaviour introduced a new docs-vs-code split.
- −1 — NEW IMPORTANT: error.rs preamble's "every fallible helper …"
  claim is *almost* right but glosses over the JS-input-parser
  category. Minor framing miss; easy fix.
- −2 — Five r4 hold-outs (transaction.rs TX_CONN/TX_TOKEN,
  orchestrator/mod.rs pub-vs-pub(crate), query.rs:443 TODO,
  migrations.rs:78 stray summary, audit.rs:5 "(future) backfill")
  all UNCHANGED. The cumulative cost of THREE-round un-actioned
  recommendations starts to compound — the audit's signal-value
  erodes when nine months of carry-over items aren't picked up.
- −2 — Replication.rs:726-732 (was r4 line 751) docstring still
  describes a `?`-flow that doesn't exist post-[I28].
- −1 — Backend/mod.rs:66 + crud.rs:53 + exec.rs:329 thread-local
  references — six-file drift class, no commit since r3.
- (no change) — AGENTS.md paths resolve; first_row_or_internal preamble
  remains exemplary; plugin-system.md crate-structure stale tree is
  out-of-scope NOTE.

**To break 90 next round:**

1. **Fix `replication_ops.rs:255-257`.** Drop the contradicted lede.
   Highest-impact + smallest edit this cycle; the comment block
   should open with the MAJOR-R5-2 paragraph (which is accurate).
2. **Pick up the five three-round hold-outs in one batch.** All are
   mechanical, each under 5 minutes:
   - `audit.rs:5` — drop "(future)" (1 word).
   - `query.rs:443-446` — delete or specificate the TODO.
   - `orchestrator/mod.rs:22` — either downgrade three `pub mod` or
     rewrite the comment.
   - `migrations.rs:78` — drop the duplicate sentence or split into
     properly separated `///` blocks; add "in this file" qualifier
     to the line-79 claim.
   - `replication.rs:726-732` — reframe as historical-shape regression
     guard.
3. **Mechanical sweep on `v8_classes/transaction.rs`** — replace
   TX_CONN/TX_TOKEN with `IsolateDbContext::tx_conn` / `tx_token`.
   Fixes 4 rustdoc-broken intra-doc links + ~14 prose sites in one
   file. Carry the sweep into `lib.rs:110/216`, `crud.rs:53`,
   `exec.rs:329`, `backend/mod.rs:66`, `orchestrator/transaction.rs`,
   `v8_classes/migration.rs:216` so the drift class closes
   crate-wide.
4. **Add the JS-input-parser bullet to error.rs:9-23.** Suggested
   wording in Dimension 5 IMPORTANT.
5. **One-line fix for the `coded_sql` preamble at error.rs:347-361**
   acknowledging replication.rs uses `prefix_message` directly.

If 1+2 alone land before next round, the score breaks 87. The cleanest
path to 90 requires also clearing the v8_classes transaction.rs sweep
(item 3) — that's the largest single docs-debt parcel in the crate.
