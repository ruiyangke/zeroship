# plugin-db docs audit — round 6 (2026-05-22)

**Scope:** `crates/plugin-db/src/` preambles + inline comments, plus the
`docs/reference/db.md` / `docs/reference/plugin-system.md` / `AGENTS.md`
surfaces that point into this crate.

**Prior rounds:** r1 (68), r2 (74), r3 (80), r4 (75), r5 (cycle 06:55 — 81/100).

Re-audited fresh after the ten commits in the brief:

- `aa639715` — `WalConsumer::new` typed `Result<_, DbError>`; doc on
  the constructor + replication_ops module preamble updated.
- `a272d1af` — P0001 DETAIL classification via `e.detail()`; new
  `classify_p0001_detail` helper.
- `70921112` — atomic `try_mark_consumer_running` closes the dispatch
  race window.
- `34d209b5` — `ConsumerRunningGuard::new` marks INSIDE the spawned
  future.
- `4b2e7046` — replication_ops.rs comment block at lines 255-286
  rewritten (closes r5 NEW CRITICAL).
- `386f9bf5` — structural test for [I42] invariant + 4 lifecycle tests
  for ConsumerRunningGuard; lifts the guard from a local inner struct
  to module scope; ALSO closed a latent bug (mark-on-construct fired
  Drop unmark on lost-race path; switched to `try_claim` returning
  `Option<Self>` with lazy `then(|| ...)` construction).
- `e5315083` — dead-code cleanups; `mark_consumer_running` gated to
  test/test-helpers; replication_ops preamble bullet rewritten to
  document both `WalConsumer::new` failure classes.
- `51ced4a0` — `finalise_backfill` warn-on-err + lock_guard.rs
  Hardening-history block (closed r4 IMPORTANT).
- `deeefe18` — migrations::coded_db routed through shared
  `crate::error::prefix_message`; inline match-arm replaced.
- `f6043126` — SQLSTATE-typed checks at replication.rs:213,257 (was
  `msg.contains("42710") / "55000"`); `classify_detail_token`
  extracted from `classify_p0001_detail`; 7 new tests.

**TL;DR.** The r5 NEW CRITICAL is cleanly closed (`4b2e7046`). The
docs surrounding the new try_claim / atomic-mark machinery are
exemplary — best inline-history block in the crate. Two new
preamble-drift issues land this cycle:

- **NEW IMPORTANT** at `wal_consumer.rs:49,51` — preamble still
  references `db.replicationConsumerStart()`, but the live SDK method
  is `db.startReplicationConsumer()` (renamed pre-stage-8b). r5
  missed this because the brief focused on lock_guard / replication_ops;
  the wal_consumer preamble hadn't been touched.
- **NEW IMPORTANT** at `error.rs:336-341` — `prefix_message` preamble
  enumerates "in `audit`, `auth::bootstrap`, `auth::keys`,
  `auth::session`, `diff`, `replication`" as the consumer set, but
  `deeefe18` adds migrations.rs (via `coded_db`) as a seventh direct
  consumer. The list is now stale at the helper itself.

The five r5 hold-outs continue UNCHANGED into r6 (`v8_classes/transaction.rs`,
`orchestrator/mod.rs:22`, `query.rs:443`, `migrations.rs:78`, `audit.rs:5`)
and the `replication.rs:745-752` test docstring drift is on its FOURTH
round un-actioned.

One bright spot: `migrations.rs:78` got actively WORSE this cycle.
`deeefe18`'s diff rewrote the second paragraph (lines 78-86) but left
the line-78 "Stamp a `DbError` with a context phrase and convert to
`OpError`." stray opening sentence intact AND left the first
paragraph (lines 67-77) which references `coded_sql("...", e)` — a
helper-shape that no longer exists in this file (replaced by
`coded_db`). Reader sees TWO opening sentences and a reference to a
function-name that doesn't appear anywhere in this file's source. The
fix is the same one r3/r4/r5 recommended; it would have taken 30
seconds in the same diff that touched the body.

The `f6043126` SQLSTATE-typed-checks + classify_detail_token
extraction is exemplary. The `classify_detail_token` preamble at
`auth/session.rs:184-192` is the cleanest small-helper docstring in
the crate — names the extraction reason (unit-testability without
fixtures), the SDK contract (5 session refusal codes), the
maintenance invariant (any token change MUST be paired with the
matching DETAIL literal in `auth/bootstrap.rs`), and the contract
test cluster pinning it. Drift surface low because the helper is
pure + the source-of-truth pairing is named.

---

## Dimension-by-dimension findings

### 1. Stale historical names — TX_CONN / TX_TOKEN / MIG_LOCK sweep

```
[CRITICAL — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/v8_classes/transaction.rs:68,161,192,214 — rustdoc intra-doc links to deleted symbols
  Why: `[`crate::TX_TOKEN`]` and `[`crate::TX_CONN`]` resolve to nothing — the symbols folded into `IsolateDbContext` in Stage 8d-R4. `cargo doc` still emits a broken-link warning per build. Four rounds without a sweep.
  Fix: Replace `[`crate::TX_TOKEN`]` → `IsolateDbContext::tx_token`; same for TX_CONN.
  Verification: grep -n "crate::TX_CONN\|crate::TX_TOKEN" crates/plugin-db/src/v8_classes/transaction.rs

[IMPORTANT — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/v8_classes/transaction.rs:68-71,107-116,161,192,214-218,278,284-285,324 — bare TX_CONN / TX_TOKEN tokens in prose
  Why: ~14 sites in this one file still spell "TX_CONN" / "TX_TOKEN" as live symbols. Sibling files (orchestrator/transaction.rs preamble, v8_classes/mod.rs, orchestrator/mod.rs) use "IsolateDbContext::tx_conn" — inconsistency is across-file, easy to chase the wrong term.
  Fix: Mechanical s/TX_TOKEN/tx_token/, s/TX_CONN/tx_conn/.
  Verification: grep -cn "TX_CONN\|TX_TOKEN" crates/plugin-db/src/v8_classes/transaction.rs

[IMPORTANT — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/v8_classes/migration.rs:216 — `MIG_LOCK` thread-local reference
  Why: "checks the `MIG_LOCK` thread-local for ownership / cancellation". The lock lives on `IsolateDbContext::mig_lock`; no top-level MIG_LOCK exists.
  Fix: s/`MIG_LOCK` thread-local/`IsolateDbContext::mig_lock` slot/.
  Verification: grep -n "MIG_LOCK" crates/plugin-db/src/v8_classes/migration.rs

[MINOR — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/lib.rs:110,216 — caps-name idiom drift
  Why: Doc on `crate::next_tx_token_for_tests` reads "Allocate a fresh non-zero TX_TOKEN value"; on `clear_mig_lock_for_tests` reads "clear `MIG_LOCK` for the current thread". lib.rs:251 already models the corrected idiom ("isolate's `IsolateDbContext::tx_conn` slot (formerly the `TX_CONN` thread-local)") — apply the same shape here.
  Fix: Either retitle to `tx_token` / `mig_lock` slot terms, or append "(formerly …)" footnote.
  Verification: grep -n "TX_TOKEN\|MIG_LOCK" crates/plugin-db/src/lib.rs

[MINOR — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/crud.rs:53 — "the active TX_CONN"
  Why: Same drift class on the hottest CRUD path.
  Fix: s/the active TX_CONN/the active tx connection (IsolateDbContext::tx_conn)/.
  Verification: grep -n "TX_CONN" crates/plugin-db/src/crud.rs

[MINOR — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/exec.rs:329 — test-helper doc references "TX_CONN"
  Why: `exec_mutation_with_emit_for_tests` doc says "setting `TX_CONN`"; the setter is `install_tx_marker_for_tests` which writes to `IsolateDbContext::tx_conn`.
  Fix: s/setting `TX_CONN`/installing the tx-conn slot/.
  Verification: grep -n "TX_CONN" crates/plugin-db/src/exec.rs

[MINOR — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/orchestrator/transaction.rs:51,52,89,143 — inline "TX_TOKEN" / "TX_CONN" tokens
  Why: Preamble (lines 1-14) is correct; inline comments inside the impl lapse back to constant names.
  Fix: s/TX_TOKEN/tx_token/, s/TX_CONN/tx_conn/ for the four sites.
  Verification: grep -n "TX_CONN\|TX_TOKEN" crates/plugin-db/src/orchestrator/transaction.rs

[MINOR — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/backend/mod.rs:66 — "in a thread-local (e.g. `MigrationLock::client`, `tx_conn`)"
  Why: Both items live in `RefCell<IsolateDbContext>`, not thread-locals. Trait doc — externally visible.
  Fix: "in the per-isolate context (e.g. `MigrationLock::client`, `IsolateDbContext::tx_conn`)".
  Verification: grep -n "thread-local" crates/plugin-db/src/backend/mod.rs

[OK] crates/plugin-db/src/context.rs:4-8,19,250,301,355,777 — uses TX_CONN / TX_TOKEN / MIG_LOCK as HISTORICAL names
  Why: These are the preamble and section-divider comments in the consolidated-context module. They explicitly frame the names as the predecessors (e.g. line 4: "`TX_CONN`, `AUTO_TX_OWNED`, `TX_TOKEN`, `TX_TOKEN_COUNTER`, `PENDING_EMITS` in `lib.rs`; `MIG_LOCK` in `migrations.rs`" — the canonical "what these slots replaced" inventory). Correct present-tense framing.
```

**Net change vs r5:** zero on the drift class. Surface-level grep
hits stay at ~16 across 7 files. `git log --oneline -- crates/plugin-db/src/v8_classes/transaction.rs`
shows zero commits since `dd1bff63` 4 rounds ago — the sweep simply
hasn't happened. The audit pattern at this point is clear: drift
that's purely doc-typographical (not preamble-shape) survives long
loops of un-action.

`callbacks.rs` mentions:

```
[OK] orchestrator/mod.rs:24 — "`crate::callbacks` was deleted in Stage 8b" — historical reference, correctly framed as past-tense.
```

### 2. Recent commits' inline comment accuracy

#### `// [I42]` and `// [I44]` references in lock_guard.rs

```
[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:156-159 — `[I42]` annotation accurate
  Why: Comment reads "the prior version flipped `released = true` BEFORE the await, so a cancellation here silently leaked the lock with no Drop log. Defer the state flip to AFTER the await completes." Matches commit `bd1e7ce1`. Body comment + Hardening-history entry align.
  Verification: git show bd1e7ce1 -- crates/plugin-db/src/orchestrator/lock_guard.rs

[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:163-167 — `[I44]` annotation accurate
  Why: "a bare `let _ =` silently swallows runtime errors from the unlock SQL — operator never sees that the lock might still be held. Log warnings on error so a leak is visible". Followed by `if let Err(e) = client.query_text_params(...).await { tracing::warn!(...) }`. Matches commit `ffb1e101`.
  Verification: git show ffb1e101 -- crates/plugin-db/src/orchestrator/lock_guard.rs

[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:212-235 — Drop log accurate
  Why: The `tracing::error!` body has "leak:" prefix, the operator-facing consequence ("Concurrent register_model callers for this app will stall in the meantime") and the diagnostic checklist (async-cancellation / panic / missed release). Matches the [I39] commit body verbatim.

[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:349-382 — NEW structural test docstring + assertion accurate
  Why: The `386f9bf5` commit added `release_flips_flag_after_unlock_await_structural`. Doc comment (lines 333-347) names the invariant ("`self.released = true` MUST appear AFTER the `.await`"), the mirroring pattern (`mint_subscription`'s byte-offset test), and the regression motive ("future contributor restoring the pre-bd1e7ce1 order trips this at compile-time without needing a live PG fixture"). The assertion body (lines 374-381) compares `flip_pos > await_pos` and cites bd1e7ce1 in the assert! panic message. Best inline test-doc in the crate this cycle.
  Verification: sed -n '335,382p' crates/plugin-db/src/orchestrator/lock_guard.rs
```

#### `// MAJOR-R6-1` / try_claim references in replication_ops.rs (34d209b5 + 70921112 + 4b2e7046)

```
[OK] crates/plugin-db/src/replication_ops.rs:255-286 — comment block rewritten by `4b2e7046` — accurate end-to-end
  Why: The r5 NEW CRITICAL contradiction is gone. The block now opens with a present-tense description of the lifecycle ("Mark + unmark live on a ConsumerRunningGuard whose lifetime is bound to the spawned future") that's accurate for HEAD, followed by a chronologically-ordered "History" sub-block with three commit annotations:
    - e399eeea (cycle 05:25, MAJOR-R5-2): added Drop-based unmark
    - 34d209b5 (cycle 06:00, MAJOR-R6-1): moved mark INSIDE the guard's constructor
    - 70921112 (cycle 06:25, concurrency r7 NEW MINOR): atomic try_mark
  Followed by a "Defense" paragraph naming `ConsumerRunningGuard::try_claim` as the atomic-claim entry point. Matches the impl below line-for-line.
  Cross-validation:
    - The `try_claim` constructor at line 356 returns `Option<Self>`.
    - `won.then(|| Self { app_id })` matches the "Lazy construction is load-bearing" docstring at lines 346-355.
    - Drop impl at 362-365 calls `unmark_consumer_running`.
  Verification: sed -n '255,300p' crates/plugin-db/src/replication_ops.rs

[OK] crates/plugin-db/src/replication_ops.rs:332-359 — `ConsumerRunningGuard` lifted to module scope (per 386f9bf5)
  Why: The struct + impl block landed at module scope. The struct preamble (lines 332-341) names the design history (cross-references commits e399eeea / 34d209b5 / 70921112) and states two invariants. The `try_claim` docstring (lines 346-355) flags the "Lazy construction is load-bearing" + names the bug `then_some(...)` would re-introduce. Highest signal-to-noise comment block touched this cycle.
  Verification: sed -n '332,366p' crates/plugin-db/src/replication_ops.rs

[OK] crates/plugin-db/src/replication_ops.rs:295-297 — guard binding + Drop note
  Why: `let Some(_guard) = ConsumerRunningGuard::try_claim(app_for_task) else { return; }` followed by `// _guard drops here on graceful exit; Drop also fires on panic-unwind, so the running marker is always cleared.` Both the constructor name and the lifecycle claim match the impl above.
  Verification: sed -n '287,297p' crates/plugin-db/src/replication_ops.rs

[OK] crates/plugin-db/src/replication_ops.rs:236-243 — Step 2 comment after aa639715
  Why: e5315083's docstring overhaul. The body comment names the two error classes (`ValidationFailed { code: "invalid_app_id" }` for developer/deploy, `Configuration { code: "not_provisioned" }` for operator/config) + states "The dispatch boundary no longer re-stamps the error". Matches `WalConsumer::new`'s signature change at `wal_consumer.rs:325`.
  Verification: sed -n '234,253p' crates/plugin-db/src/replication_ops.rs
```

#### `// [I28]` references after the cbbc9059 dedupe + deeefe18 migrations route

```
[OK] crates/plugin-db/src/auth/{bootstrap,keys,session}.rs — "Typed-error sweep [I28]" test-block headers accurate
  Why: Each file's "[I28]" test block still pins the function's Result error type as `DbError`. Headers reference commit `0049d9be`. f6043126 didn't touch the test bodies; aa639715 added an [I28]-style typed-result signature guard at `wal_consumer.rs` covered by the existing pattern.
  Verification: grep -n "Typed-error sweep" crates/plugin-db/src/auth/

[OK] crates/plugin-db/src/replication.rs:813-… — [I28] tests for sanitise_app_id / publication_name / slot_name + `prefix_message` variant preservation
  Why: Headers reference 0049d9be; contract tests live at `error.rs::tests::prefix_message_*` (3 tests pinning variant-preservation invariant). Doc on line 866-869 reads "The `prefix_message` helper's contract (variant preserved, … `crate::error::tests::prefix_message_preserves_variant_and_code` and `prefix_message_leaves_structured_variants_alone`)" — correctly names the canonical tests.
  Verification: grep -n "prefix_message_preserves_variant_and_code\|prefix_message_leaves_structured_variants_alone" crates/plugin-db/src/

[OK] crates/plugin-db/src/error.rs::tests::prefix_message_* — variant-preservation invariant
  Why: Three tests exist (per cbbc9059's commit message claim). f6043126 didn't touch them; deeefe18 routes migrations through `prefix_message` so the same variant-preservation contract now covers a seventh consumer.
```

### 3. lock_guard.rs Hardening history block (51ced4a0 — closed in r5, re-validated)

```
[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:51-69 — Hardening history block accurate and complete
  Why: Four commits listed in chronological order:
    1. `cbd12944` (cycle 02:05) — extract guard from 3 open-coded sites
    2. `bd1e7ce1` ([I42], cycle 04:00) — defer `released = true` flip until AFTER the unlock-SQL await
    3. `808a32af` ([I39], cycle 04:35) — `#[must_use]` + Drop log "leak:" prefix
    4. `ffb1e101` ([I44], cycle 04:35) — `if let Err(e) =` + `tracing::warn!` on unlock-SQL failure
  Each correctly attributes the commit, the cycle, and the bug class. Cross-validation: `git log --oneline -- crates/plugin-db/src/orchestrator/lock_guard.rs` shows exactly those four commits plus `51ced4a0` itself plus `386f9bf5` (the structural test, which is a hardening event but doesn't fix a bug — could be added as a 5th bullet noting "regression guard for [I42]" but optional; the body comments at lines 333-347 already cross-reference).
  Suggestion (NIT): the test-fixture-side hardening from `386f9bf5` (the structural test pinning [I42]) is worth adding as a 5th history bullet. It's not a behaviour fix but it's a class of "future-revert-proofing" the history block doesn't yet mention. Optional.
  Verification: git log --oneline -- crates/plugin-db/src/orchestrator/lock_guard.rs; sed -n '51,69p' crates/plugin-db/src/orchestrator/lock_guard.rs

[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:1-50 — preamble Why-not-full-RAII section unchanged
  Why: The cbd12944-era prose is intact. 51ced4a0 appended the Hardening-history block AFTER this prose without touching it — the right surgery.
  Verification: sed -n '1,50p' crates/plugin-db/src/orchestrator/lock_guard.rs
```

### 4. ConsumerRunningGuard comment block at replication_ops.rs (4b2e7046 + 70921112 + 386f9bf5)

```
[OK] crates/plugin-db/src/replication_ops.rs:255-286 — block rewritten; r5 NEW CRITICAL closed
  Why: See Dimension 2. The pre-r5 lede ("Mark the app as running BEFORE the spawn so a racing second call …") is GONE. New opening: "Mark + unmark live on a ConsumerRunningGuard whose lifetime is bound to the spawned future. Both mark and unmark execute INSIDE the future (mark on guard construction via try_claim; unmark via Drop, on ANY exit — graceful, panic, future dropped before first poll)." Accurate end-to-end. The History sub-block lists three commits chronologically with the bug class each closed.
  +5 from r5: cleanest closure of an r5 NEW CRITICAL this cycle.

[OK] crates/plugin-db/src/replication_ops.rs:346-355 — `try_claim` docstring "Lazy construction is load-bearing"
  Why: Best inline-docstring-of-a-bug-class-prevention I've seen in this crate. Names the rejected alternative (`then_some`), the failure mode it would re-introduce (ephemeral Self → immediate Drop → unmark winner's claim), and the chosen pattern (`then(|| ...)`). A future contributor seeing the unusual `won.then(|| ...)` pattern can read three lines of comments and understand why it can't be the more-idiomatic `then_some`.
  Verification: sed -n '346,360p' crates/plugin-db/src/replication_ops.rs

[OK] crates/plugin-db/src/replication_ops.rs:368-444 — lifecycle test cluster (4 tests + try_claim contract)
  Why: Test names self-document:
    - `consumer_running_guard_new_marks_app` (basic try_claim semantics)
    - `consumer_running_guard_drop_unmarks_app` (Drop unmarks on scope exit)
    - `consumer_running_guard_drop_unmarks_on_panic_unwind` (Drop fires on panic — uses `catch_unwind`)
    - `consumer_running_guard_try_claim_loses_when_already_marked` (atomic check-and-set invariant)
  These pin the four lifecycle paths the surrounding comment block claims. `386f9bf5`'s commit message explicitly cites this cluster.
  Verification: sed -n '368,444p' crates/plugin-db/src/replication_ops.rs
```

### 5. `classify_detail_token` preamble (f6043126) — exemplary?

```
[OK — EXEMPLARY] crates/plugin-db/src/auth/session.rs:184-192 — `classify_detail_token` docstring is best-in-crate
  Why: 9-line docstring carries:
    1. The purpose ("Pure DETAIL-token → (code, operator-facing message) map").
    2. The extraction rationale ("Extracted from `classify_p0001_detail` so the SDK-contract surface (the 5 session refusal codes) is unit-testable without standing up a real `compio_postgres::Error` fixture").
    3. The maintenance invariant ("Any change to one of these tokens MUST be paired with the matching `USING DETAIL = '<token>'` literal in [`crate::auth::bootstrap`]'s CREATE FUNCTION body").
    4. The contract-pinning test cluster ("the `classify_detail_*` test cluster pins the contract").
  Drift surface analysis:
    - Pure function — no state, no I/O. Behaviour is the match-arm body.
    - Source-of-truth pairing (Rust match arms ↔ `auth/bootstrap.rs` CREATE FUNCTION DETAIL literals) is named verbatim. Cross-reference is bidirectional (`auth/session.rs:189-191` → `auth/bootstrap.rs`; `auth/bootstrap.rs:381,524,530,535,549,557,737,773` USING ERRCODE = 'P0001' sites visible via grep).
    - Test cluster pins all 5 tokens + unknown-token fall-through + codes-are-distinct invariant (7 tests per f6043126's commit message).
  Single suggestion (NIT): the docstring could explicitly cross-reference the SECURITY DEFINER `init_session` function in `auth/bootstrap.rs` whose RAISE EXCEPTION sites emit these DETAIL tokens. Today the cross-reference is implicit via "USING DETAIL = '<token>' literal". One sentence would make grep-jumps faster.
  Verification: sed -n '184,215p' crates/plugin-db/src/auth/session.rs

[OK] crates/plugin-db/src/auth/session.rs:163-182 — `classify_p0001_detail` parent docstring
  Why: Names the SECURITY DEFINER `init_session` source, the `e.detail()` reading rationale ("classification is locale- / formatter-independent" — directly references MAJOR-R5-1 motive), the return shape (`Option<(static_code, operator_message)>`), and the caller-side fall-through. Consistent with `classify_detail_token` (lines 184-215). 9 lines, dense with information.

[OK] crates/plugin-db/src/auth/session.rs:243-258 — `map_err` callsite comment
  Why: Names MAJOR-R5-1 ("substring matching was fragile against RAISE additions, formatter changes, locale") AND perf r7 N7-M0 ("the prior implementation built `format!("{e}")` + walked the source chain even though `classify_p0001_detail` reads detail() borrow-only; removed the dead allocation"). The dead-allocation removal lives at e5315083 and is named in the inline comment — both commit-history annotations correctly tied to the line they motivated.
  Verification: sed -n '243,260p' crates/plugin-db/src/auth/session.rs
```

This is the single cleanest small-helper docstring in the crate this
cycle. f6043126 + e5315083 together leave session.rs's
P0001-DETAIL-classification surface at exemplary quality.

### 6. error.rs `prefix_message` / `coded_sql` preambles — drift after deeefe18?

```
[OK] crates/plugin-db/src/error.rs:1-28 — preamble accurate post-f7d0961c, survives all r6 commits
  Why: The narrow hold-out set (validate stage + `hex_decode` / `hex_nibble`) is still right. The "Every fallible helper that touches Postgres or the V8 boundary now returns `Result<_, DbError>`" claim continues to gloss over the JS-input parsers (`parse_commit_spec`, `parse_spec`, `parse_name_and_collection`) + `lib.rs:349 init_pool_async`. This is still the r5 IMPORTANT carry-over; an `Result<_, String>` grep confirms the same 4 production sites + 5 test/inline-sentinel sites:
  ```
  $ grep -rn "Result<.*, String>" crates/plugin-db/src/
  ...
  crates/plugin-db/src/lib.rs:349:pub async fn init_pool_async() -> Result<(), String> {
  crates/plugin-db/src/orchestrator/register_model/validate.rs:59:) -> Result<ApprovedPlan, String> {
  crates/plugin-db/src/v8_classes/migrations.rs:221:) -> Result<(String, String), String> {  # parse_name_and_collection
  crates/plugin-db/src/v8_classes/migration.rs:454:) -> Result<CommitSpec, String> {  # parse_commit_spec
  crates/plugin-db/src/v8_classes/migration.rs:742:) -> Result<SpecParts, String> {  # parse_spec
  ```
  Same finding as r5 IMPORTANT. Unchanged.
  Verification: grep -rn "Result<.*, String>" crates/plugin-db/src/ | grep -v test

[NEW IMPORTANT] crates/plugin-db/src/error.rs:336-341 — `prefix_message` preamble's call-site list is stale post-deeefe18
  Why: The preamble reads
  > "This is the shared primitive every per-module `coded_sql` helper (in `audit`, `auth::bootstrap`, `auth::keys`, `auth::session`, `diff`, `replication`) routes through"
  but `deeefe18` adds `migrations::coded_db` as a SEVENTH direct consumer of `prefix_message`. Confirmed via grep:
  ```
  crates/plugin-db/src/migrations.rs:94:    crate::error::prefix_message(&mut db_err, &format!("{context}: "));
  crates/plugin-db/src/replication.rs:52,203,224,240,289,419,561,591,623 (multiple)
  ```
  Six modules listed in the preamble; seven consumers in the source. The reader of `prefix_message` walks the listed set thinking it's complete; misses that `migrations.rs::coded_db` is also a wrapper.
  Also: the preamble uses "`coded_sql` helper" framing for the consumer set, but `migrations::coded_db` is NOT a `coded_sql` helper (it's a `coded_db` helper — different signature: takes `DbError` not `compio_postgres::Error`). The framing conflates two helper shapes.
  Fix: Either (a) split the preamble's "consumer" enumeration into TWO sub-lists ("`coded_sql` wrappers in `audit`, `auth::bootstrap`, …" + "direct consumers in `migrations::coded_db` (post-deeefe18) and `replication.rs` (10 sites)"), OR (b) drop the call-site list entirely and replace with a one-liner ("Every per-module SQL-error wrapper routes through this helper to preserve variant + .code while adding a context prefix").
  Verification: grep -rn "crate::error::prefix_message" crates/plugin-db/src/

[OK] crates/plugin-db/src/error.rs:370-384 — `coded_sql` helper preamble
  Why: Names the per-file duplicates it replaced ("audit, auth::bootstrap, auth::keys, auth::session, and diff" — five). Cross-validated:
    - `audit.rs:59` reads `crate::error::coded_sql(&format!("audit: {context}"), e)` — matches.
    - `auth/bootstrap.rs:25` reads `crate::error::coded_sql(&format!("auth/bootstrap: {context}"), e)` — matches.
    - `auth/keys.rs:42` reads `crate::error::coded_sql(&format!("auth/keys: {context}"), e)` — matches.
    - `auth/session.rs:36` reads `crate::error::coded_sql(&format!("auth/session: {context}"), e)` — matches.
    - `diff.rs:41` reads `crate::error::coded_sql(&format!("diff: {context}"), e)` — matches.
  Five-file consumer set is correct.
  Single suggestion (NIT — UNCHANGED FROM r5): the preamble could acknowledge that `replication.rs` and `migrations.rs` consume `prefix_message` directly (NOT via `coded_sql`) because their callers already have a `DbError`. One sentence: "Note: `migrations::coded_db` and `replication.rs` callers consume `prefix_message` directly because they hold a `DbError` rather than a `compio_postgres::Error`."
  Verification: grep -n "crate::error::coded_sql" crates/plugin-db/src/

[OK] crates/plugin-db/src/error.rs:386-… — `first_row_or_internal` is exemplary
  Why: Names the bug class ("silent-empty-RETURNING"), the canonical commit (`d7cfc089`), the regression test. Drift surface low — closed bug class.
```

### 7. AGENTS.md task router — file paths

```
[OK] Every plugin-db-relevant row in AGENTS.md resolves on HEAD:
  - "**Adding a native primitive** … `docs/reference/plugin-system.md` · `crates/runtime-macros/` · `crates/plugin-{db,kv,storage}/`" — all four exist
  - "**The DB SDK** (`@zeroship/db`) … `docs/reference/db.md` · `crates/plugin-db/`" — both exist
  - "**ZS deploy contract** … `sdks/bootstrap/src/{dispatcher,runtime-entry}.ts` · `crates/runtime/src/core/init.rs`" — all three exist
  Verification: ls docs/reference/{db,plugin-system}.md crates/plugin-{db,kv,storage} crates/runtime-macros sdks/bootstrap/src/{dispatcher,runtime-entry}.ts crates/runtime/src/core/init.rs

[NOTE — out of scope, FOUR-ROUND CARRY-OVER from r3/r4/r5] docs/reference/plugin-system.md:315-344 — "Crate structure" tree still stale
  Why: AGENTS.md row 3 sends "adding a native primitive" readers here, and the tree lists six paths that don't exist:
    - crates/runtime/src/init.rs           → today: crates/runtime/src/core/init.rs
    - crates/runtime/src/runtime.rs        → does not exist
    - crates/runtime/src/plugin.rs         → does not exist
    - crates/plugin-db/src/callbacks.rs    → deleted Stage 8b
    - crates/plugin-db/src/validate.rs     → exists only at orchestrator/register_model/validate.rs
    - crates/plugin-db/src/migrate.rs      → does not exist (today: migrations.rs + audit.rs)
    - crates/plugin-auth/                  → does not exist (auth lives in plugin-db/src/auth/)
    - crates/pg/                           → renamed to compio-postgres
  Out-of-scope for the plugin-db crate proper but the entry-point experience for new contributors is now FOUR rounds degraded.
  Verification: ls docs/reference/plugin-system.md; grep -n "callbacks.rs\|validate.rs\|migrate.rs\|crates/pg" docs/reference/plugin-system.md
```

### 8. r4/r5 hold-outs — re-walk

```
[CRITICAL — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/v8_classes/transaction.rs (TX_CONN / TX_TOKEN drift)
  Covered as Dimension 1 CRITICAL + IMPORTANT above. Four rounds without a sweep. `git log --oneline -- crates/plugin-db/src/v8_classes/transaction.rs` shows zero commits since pre-r3.
  Verification: git log --oneline -- crates/plugin-db/src/v8_classes/transaction.rs | head -3

[IMPORTANT — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/orchestrator/mod.rs:22 — "Each submodule is `pub(crate)` to scope visibility"
  Why: Lines 28-31:
  ```rust
  pub mod auto_tx;
  pub(crate) mod lock_guard;
  pub mod register_model;
  pub mod transaction;
  ```
  Three of four are `pub`, not `pub(crate)`. The effective visibility is `pub(crate)` because lib.rs:84 has `pub(crate) mod orchestrator;` under `#[cfg(not(feature = "test-helpers"))]` (line 86 promotes to `pub` for test-helpers builds). FOUR rounds flagged; no follow-up.
  Fix: Either (a) downgrade three `pub mod` → `pub(crate) mod` (matches the comment + cbd12944's lock_guard choice), or (b) rewrite the comment.
  Verification: grep -n "^pub" crates/plugin-db/src/orchestrator/mod.rs

[IMPORTANT — UNCHANGED FROM r4 / r5] crates/plugin-db/src/replication.rs:745-752 — test docstring claims a return type that no longer exists
  Why: After cbbc9059, `ensure_publication_and_slot` returns `Result<SetupOutcome, DbError>`. The docstring above `empty_returning_string_shape_keeps_replication_prefix` (now at line 745 in HEAD; was at 751 in r4, 726 in r5) still reads "`ensure_publication_and_slot` returns `Result<_, String>` (not `Result<_, DbError>` like audit.rs), so the runtime fix calls `.into_string()` on the `DbError::Internal` before flowing it through `?`." Both clauses are false post-[I28]. The test below still has value (pins the `"replication:"` log-prefix shape) but the docstring describes a `?`-flow that does not exist. FOUR rounds un-actioned.
  Fix: Reframe as historical-shape regression guard (suggested wording in r5 Dimension 8).
  Verification: grep -n "fn empty_returning_string_shape_keeps_replication_prefix" crates/plugin-db/src/replication.rs

[CRITICAL — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/query.rs:443-446 — "TODO: A1 composite indexes"
  Why: Composite indexes ARE wired up. `build_named_indexes` at query.rs:529 is called from `orchestrator/register_model/bootstrap.rs:175`; the SDK builder is live. The TODO + "not yet surfaced by the SDK" prose predate the ship. FOUR rounds un-actioned.
  Fix: Either delete the TODO (composite indexes are present-tense) or rewrite to be specific:
    "TODO(A1): the alternative `schema._meta.indexes` declaration form (today indexes are passed as a separate `registerModel(coll, schema, indexes)` arg)."
  Verification: grep -n "build_named_indexes" crates/plugin-db/src/orchestrator/register_model/bootstrap.rs

[CRITICAL — WORSENED FROM r3 / r4 / r5] crates/plugin-db/src/migrations.rs:67-86 — `coded_db` doc block now contradicts itself THREE ways
  Why: The `deeefe18` commit (cycle 06:20) routed `coded_db` through `crate::error::prefix_message`. Diff body shows the second paragraph was rewritten (correctly enumerates the shared-helper pattern + cites architecture r8 M11). But:
    1. The doc block STILL has two distinct opening sentences at lines 67 ("SQL-error helper — classify the Postgres error through `DbError` …") and 78 ("Stamp a `DbError` with a context phrase and convert to `OpError`."). Each opens a new doc paragraph without a blank `///` separator; rustdoc renders the second inline.
    2. The line-71 reference reads `coded_sql("...", e)` — a function shape that DOES NOT EXIST in this file. The function is `coded_db`. `coded_sql` is the per-file pattern this function REPLACED (then later was itself extracted to `crate::error::coded_sql`). A reader running `grep -n "fn coded_sql" crates/plugin-db/src/migrations.rs` gets zero hits.
    3. The line-67 first paragraph describes the OLD body (classify the Postgres error through `DbError`) — but `coded_db` no longer takes a Postgres error; it takes a `DbError` (caller has already classified). The phrase "classify the Postgres error" is technically about the operation `prefix_message` participates in (the variant is preserved, the SQLSTATE classification has already happened in `walk_pg_chain`), but reading the first paragraph alone implies `coded_db` does the classification, which it doesn't (its caller does, via the `From<compio_postgres::Error>` impl + Backend method).
  Net effect: a contributor reading this doc block sees:
    - "SQL-error helper — classify the Postgres error through `DbError`" (line 67) — partially wrong; the function consumes an already-classified DbError.
    - "Use for the many `map_err(|e| coded_sql("...", e))`-shaped sites" (line 71) — references a non-existent function in this file.
    - "Stamp a `DbError` with a context phrase and convert to `OpError`" (line 78) — opens a new paragraph contradicting the previous.
    - "Thin wrapper around `crate::error::prefix_message`" (line 80) — TRUE and consistent with HEAD.
  WORSENED vs r5: the deeefe18 diff TOUCHED THIS DOC BLOCK and left the line-67-77 paragraph orphaned. A developer who took 30 seconds to fix the broken intro at the same time would have closed the FOUR-round carry-over.
  Fix: Replace lines 67-86 with one coherent doc block:
  ```rust
  /// Stamp a `DbError` with a context phrase and convert to `OpError`.
  ///
  /// Thin wrapper around [`crate::error::prefix_message`] that adds the
  /// migration-lifecycle context (e.g. `"migration row UPDATE"`) and
  /// converts to the V8-boundary error type. Preserves the SQLSTATE-
  /// derived `.code` of the underlying `DbError` (caller already
  /// classified the Postgres error via the `From<compio_postgres::Error>`
  /// impl in `crate::error`).
  ///
  /// History: was a local `coded_sql(context, compio_postgres::Error)`
  /// in this file (pre-cbbc9059); narrowed to `coded_db(context,
  /// DbError)` when Backend methods began surfacing classified errors;
  /// inline variant-walk replaced with `crate::error::prefix_message`
  /// at `deeefe18` (architecture r8 M11; previously open-coded the
  /// match arms shared with audit.rs / auth/*.rs / diff.rs / replication.rs).
  ```
  Verification: sed -n '67,96p' crates/plugin-db/src/migrations.rs

[MINOR — UNCHANGED FROM r3 / r4 / r5] crates/plugin-db/src/audit.rs:5 — "(future) backfill"
  Why: B1 backfill orchestrator shipped and writes Backfill-phase rows (`Phase::Backfill` at audit.rs:97). The "(future)" parenthetical is now FOUR rounds stale. `51ced4a0` added a new error-handling site on the backfill code path (warn-on-finalise) while leaving the comment "(future)".
  Fix: Drop "(future)" — backfill is present-tense.
  Verification: grep -n "future" crates/plugin-db/src/audit.rs
```

### 9. NEW since r5

```
[CRITICAL — closed] crates/plugin-db/src/replication_ops.rs:255-257 contradiction with surrounding comment block
  Closed at `4b2e7046` — see Dimension 2 / 4 OK above.

[IMPORTANT — NEW] crates/plugin-db/src/wal_consumer.rs:49-51 — preamble still references obsolete SDK method name
  Why: The preamble's "What's NOT in this commit" section says:
  > "apps call `db.replicationConsumerStart()` to enable cross-worker propagation. Spawning automatically on isolate boot is one `r.add("replicationConsumerStart", …)` + a callback away in `replication_ops.rs`"
  But the live SDK method (per `v8_classes/db.rs:242` and `replication_ops.rs:178`) is `startReplicationConsumer`, NOT `replicationConsumerStart`. The wal_consumer preamble pre-dates the V8-class rename and `replication_ops.rs` does NOT have an `r.add(...)` registration site anymore (the method is wired via `#[v8_async_method]` + `#[v8_name = "startReplicationConsumer"]` on the Db class).
  r5 missed this because the brief lensed lock_guard / replication_ops / error.rs surfaces; wal_consumer.rs preamble has been untouched since the rename.
  Fix: s/replicationConsumerStart/startReplicationConsumer/ in lines 49 and 51; rewrite line 51-53 to point at the `#[v8_async_method]` registration site in `v8_classes/db.rs` (the `r.add(...)` shape is gone — registration happens via the macro now).
  Verification: grep -rn "replicationConsumerStart\|startReplicationConsumer" crates/plugin-db/src/

[IMPORTANT — NEW] crates/plugin-db/src/error.rs:336-341 — `prefix_message` preamble's call-site list omits `migrations.rs`
  Why: See Dimension 6 NEW IMPORTANT above. The preamble lists six consumer modules but `deeefe18` adds migrations as a seventh direct consumer. Mismatch between docs and reality is internal-only (the helper compiles fine without the list being right), but it confuses readers tracing the dedupe-helper relationships.
  Fix: Suggested wording in Dimension 6.
  Verification: grep -rn "crate::error::prefix_message" crates/plugin-db/src/

[CRITICAL — WORSENED] crates/plugin-db/src/migrations.rs:67-86 — `coded_db` doc block contradicts itself THREE ways
  Why: See Dimension 8 above. WORSENED vs r5 because `deeefe18` actively touched the doc block and left line 67-77 inconsistent with the rewritten 78-86 body. The "Stamp a `DbError`" line that r5 flagged as duplicate-summary is now embedded in a doc block that ALSO contains a now-wrong "classify the Postgres error" intro and a `coded_sql` reference that doesn't compile-resolve in this file.

[OK — for context] `f6043126` SQLSTATE-typed checks + classify_detail tests
  Why: replication.rs:213,257 substring matches switched to `e.as_db_error()?.code()` against `SqlState::DUPLICATE_OBJECT` and `SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE`. Confirmed at HEAD: grep -n "SqlState::" crates/plugin-db/src/replication.rs shows the typed checks at the comment-cross-referenced locations. Same fragility class as MAJOR-R5-1 (substring-on-rendered-error) eliminated from `auth/session.rs`. Clean fix; the inline comments at replication.rs:213 + :267 cross-reference the auth/session.rs fix. Verification: grep -n "SqlState::" crates/plugin-db/src/replication.rs
  Verification: grep -n "msg.contains" crates/plugin-db/src/replication.rs (should be zero now)

[OK — for context] `aa639715` WalConsumer::new typed Result
  Why: `WalConsumer::new` returns `Result<Self, DbError>` directly. Docstring on the constructor (`wal_consumer.rs:325-345`) names the two error classes + their `.code`s + the developer-vs-operator distinction. `replication_ops.rs` module preamble (lines 44-51) and Step 2 body comment (lines 236-243) match the new shape verbatim (per e5315083's docstring overhaul). Three-site consistency.

[OK — for context] `386f9bf5` structural test + lifecycle tests + latent bug fix
  Why: The lifted `ConsumerRunningGuard` (module scope) + 4 lifecycle tests + 1 structural test cover the four behaviour paths the comment block claims. Doc comment on `release_flips_flag_after_unlock_await_structural` (lines 333-347) is exemplary — names the invariant, the mirroring pattern, the regression motive, and the byte-offset comparison that pins it.

[OK — for context] `e5315083` dead-code cleanups
  Why: Four small fixes, each named in the commit message + linked to its origin (perf r7 N7-M0 / api-surface r6 MAJOR-R6-1/2/3). The `mark_consumer_running` gating to `#[cfg(any(test, feature = "test-helpers"))]` is correctly documented at context.rs:414-422 (preamble names the production replacement `try_mark_consumer_running` + the api-surface r6 MAJOR-R6-1 motivation). Replication_ops module preamble bullet (lines 44-51) correctly documents both `WalConsumer::new` legs.
```

### 10. Per-module preambles present?

```
[OK] every src/ file has a `//!` preamble at line 1
  Verification: for f in crates/plugin-db/src/*.rs crates/plugin-db/src/{auth,backend,orchestrator,v8_classes}/*.rs crates/plugin-db/src/orchestrator/register_model/*.rs; do head -1 "$f" | grep -q "^//!" || echo "MISSING: $f"; done — zero hits
```

### 11. Carry-over no-action items

```
[MINOR — UNCHANGED] crates/plugin-db/src/audit.rs:5 — see Dimension 8.
[MINOR — UNCHANGED] crates/plugin-db/src/lib.rs:110,216 — see Dimension 1.
[MINOR — UNCHANGED] crates/plugin-db/src/crud.rs:53 — see Dimension 1.
[MINOR — UNCHANGED] crates/plugin-db/src/exec.rs:329 — see Dimension 1.
[MINOR — UNCHANGED] crates/plugin-db/src/backend/mod.rs:66 — see Dimension 1.
[MINOR — UNCHANGED] crates/plugin-db/src/orchestrator/transaction.rs:51,52,89,143 — see Dimension 1.
```

---

## What got cleanly fixed since r5

- **r5 NEW CRITICAL — `replication_ops.rs:255-257` contradicted lede**:
  closed at `4b2e7046`. The block now opens with an accurate
  present-tense description of the lifecycle and includes a chronologically-
  ordered History sub-block (commits e399eeea / 34d209b5 / 70921112).
- **`70921112` atomic try-mark race close**: clean closure of the
  dispatch race window the prior `mark_consumer_running` left open;
  documented at context.rs:428-435 (`try_mark_consumer_running` preamble
  names the race and the concurrency-r7-NEW-MINOR origin). The
  dead-non-atomic variant is gated to test/test-helpers (e5315083).
- **`34d209b5` mark-inside-future**: behaviour fix accurate; the
  comment block at `replication_ops.rs:255-286` correctly documents
  the lifecycle change (including the order-of-events shift the
  previous comment got wrong).
- **`386f9bf5` structural test + lifecycle tests**: the `[I42]`
  structural test is the cleanest small-test docstring in the crate
  (mirrors `mint_subscription`'s pattern, names the regression motive,
  uses byte-offset search). Lifts `ConsumerRunningGuard` to module
  scope; 4 lifecycle tests pin the behaviour the surrounding comment
  block claims.
- **`aa639715` WalConsumer typed Result**: three-site consistency
  (constructor docstring + replication_ops module preamble + Step 2
  body comment).
- **`a272d1af` + `f6043126` P0001 DETAIL classification**: best new
  small-helper docstring in the crate (`classify_detail_token` at
  auth/session.rs:184-192). Replaces the MAJOR-R5-1 substring matching
  with a typed `e.detail()`-driven map; the parent `classify_p0001_detail`
  + the inline comment at the `map_err` call site cross-reference cleanly.
- **`f6043126` typed SqlState checks at replication.rs:213,257**:
  same fragility class as MAJOR-R5-1, closed identically — now via
  `e.as_db_error()?.code()` against `SqlState::DUPLICATE_OBJECT` /
  `SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE`. Locale- and
  formatter-independent.
- **`e5315083` dead-code cleanups**: four small fixes, each named in
  the commit message with the originating review citation. The
  `mark_consumer_running` gating (test/test-helpers) is correctly
  documented at context.rs:414-422; `replication_ops` module preamble
  bullet rewritten to document both `WalConsumer::new` failure legs.
- **`51ced4a0` finalise_backfill warn**: (carry-over from r5 — still
  accurate) the comment at migrations.rs:639-643 names the F1-family
  discipline regression that motivated the warn-on-err path.

## What did NOT change since r5 (THREE / FOUR rounds un-actioned)

- **`v8_classes/transaction.rs` TX_CONN/TX_TOKEN drift**: ~14 prose
  sites + 4 rustdoc-broken intra-doc links remain. FOUR rounds without
  a sweep.
- **`orchestrator/mod.rs:22` `pub` vs `pub(crate)` mismatch**: FOUR
  rounds. Three of four submodules still declared `pub` while the
  preamble claims `pub(crate)`.
- **`query.rs:443-446` composite-indexes TODO**: FOUR rounds.
  `build_named_indexes` shipped + wired in `orchestrator/register_model/bootstrap.rs:175`.
- **`audit.rs:5` "(future) backfill"**: FOUR rounds. B1 backfill
  shipped and continues to gain new error-handling sites (51ced4a0 added
  one this cycle) while the comment "(future)" stays.
- **`migrations.rs:67-86` doc block drift**: FOUR rounds, **WORSENED
  this cycle**. `deeefe18` touched the body and left the doc block
  with two opening sentences + a `coded_sql` reference that doesn't
  exist in this file + a "classify the Postgres error" intro that no
  longer describes the function.
- **`lib.rs:110/216` caps-name idiom drift**: FOUR rounds.
- **`crud.rs:53` / `exec.rs:329` / `backend/mod.rs:66` thread-local
  references**: FOUR rounds. Six files, same drift class.
- **`replication.rs:745-752` test docstring `Result<_, String>` claim**:
  FOUR rounds. cbbc9059 reshuffled line numbers (line 751 → 726 → 745)
  but the docstring describing a non-existent `?`-flow remains.

## NEW since r5

- **IMPORTANT (Dimension 9) — `wal_consumer.rs:49-51`**: preamble
  references obsolete SDK method name `replicationConsumerStart`. The
  live name is `startReplicationConsumer`. Pre-stage-8b rename never
  reached this preamble.
- **IMPORTANT (Dimension 6 / 9) — `error.rs:336-341`**: `prefix_message`
  preamble's "in `audit`, `auth::bootstrap`, …" enumeration omits
  `migrations.rs::coded_db` (added as a seventh consumer by `deeefe18`).
- **CRITICAL — WORSENED (Dimension 8 / 9) — `migrations.rs:67-86`**:
  the FOUR-round duplicate-summary issue was actively WORSENED this
  cycle. `deeefe18`'s diff rewrote the second paragraph (correctly)
  but left the first paragraph (now inconsistent with the body) +
  the duplicate opening sentence (now embedded in the inconsistent
  block) + a `coded_sql` reference (doesn't exist in this file).

---

## Score: 83 / 100  (r5: 81)

**Delta breakdown (+2 from r5):**

- +4 — `4b2e7046` closed the r5 NEW CRITICAL cleanly. The block at
  `replication_ops.rs:255-286` is now end-to-end accurate; the
  History sub-block lists three commits chronologically with the bug
  class each closed. The follow-on `ConsumerRunningGuard` struct
  preamble + the `try_claim` "Lazy construction is load-bearing"
  docstring are the highest signal-to-noise comment blocks touched
  this cycle.
- +3 — `f6043126` + `a272d1af` exemplary P0001-DETAIL-classification
  surface. `classify_detail_token` docstring + cross-references +
  contract-test cluster set a new bar for small-helper documentation
  in the crate.
- +2 — `386f9bf5` structural test + lifecycle tests cover the four
  behaviour paths the surrounding comment block claims. The
  byte-offset structural-test pattern (mirroring `mint_subscription`)
  is the cleanest test-doc pattern in the crate.
- +1 — `aa639715` + `e5315083` three-site consistency on
  `WalConsumer::new` (constructor docstring + module preamble + body
  comment); the `e5315083` dead-code-gating of `mark_consumer_running`
  is correctly cross-documented.
- −2 — NEW CRITICAL (WORSENED): `migrations.rs:67-86` `coded_db` doc
  block is now inconsistent THREE ways after `deeefe18` (orphaned
  first paragraph, duplicate opening sentence, non-existent
  `coded_sql` reference). FOUR-round carry-over actively made worse.
- −2 — NEW IMPORTANT: `wal_consumer.rs:49-51` references obsolete
  `replicationConsumerStart` method (the live SDK name is
  `startReplicationConsumer`). Stage-8b rename never reached this
  preamble.
- −1 — NEW IMPORTANT: `error.rs:336-341` `prefix_message` preamble
  list omits `migrations.rs` after `deeefe18` made it a seventh
  consumer.
- −2 — FOUR-round un-actioned hold-outs continue to compound:
  `v8_classes/transaction.rs` TX_CONN/TX_TOKEN (~14 prose sites +
  4 broken intra-doc links), `orchestrator/mod.rs:22` pub-vs-pub(crate),
  `query.rs:443` TODO, `audit.rs:5` "(future)", `lib.rs:110/216`
  caps-name drift, `crud.rs:53` / `exec.rs:329` / `backend/mod.rs:66`
  thread-local refs, `orchestrator/transaction.rs:51,52,89,143`
  inline caps tokens. The audit's signal-value continues to erode
  when N-round un-actioned recommendations don't get picked up by
  the implementing PRs that touch adjacent code.
- −1 — `replication.rs:745-752` test docstring still describes a
  `?`-flow that doesn't exist post-[I28]. FOUR rounds. The test
  itself is valuable (pins the `"replication:"` log-prefix shape);
  only the surrounding docstring is stale.
- (no change) — AGENTS.md paths resolve; first_row_or_internal preamble
  remains exemplary; plugin-system.md crate-structure stale tree is
  out-of-scope NOTE.

**To break 90 next round:**

1. **Fix `migrations.rs:67-86`.** Suggested wording in Dimension 8.
   Highest-impact + ~10-line edit; closes a CRITICAL that has been
   FOUR rounds un-actioned and was actively worsened this cycle.
2. **Fix the two NEW IMPORTANTs.** Both are small:
   - `wal_consumer.rs:49-51`: s/replicationConsumerStart/startReplicationConsumer/
     in two sites; rewrite the "is one `r.add(...)` + a callback away"
     phrase to point at the `#[v8_async_method] #[v8_name = "..."]`
     registration site in `v8_classes/db.rs`.
   - `error.rs:336-341`: add migrations to the enumeration, or replace
     the call-site list with a one-liner.
3. **Pick up the four-round-old hold-outs in one batch.** All are
   mechanical, each under 5 minutes:
   - `audit.rs:5` — drop "(future)" (1 word).
   - `query.rs:443-446` — delete or specificate the TODO.
   - `orchestrator/mod.rs:22` — either downgrade three `pub mod` or
     rewrite the comment.
   - `replication.rs:745-752` — reframe as historical-shape regression
     guard.
4. **Mechanical sweep on `v8_classes/transaction.rs`** — replace
   TX_CONN/TX_TOKEN with `IsolateDbContext::tx_conn` / `tx_token`.
   Fixes 4 rustdoc-broken intra-doc links + ~14 prose sites in one
   file. Carry the sweep into `lib.rs:110/216`, `crud.rs:53`,
   `exec.rs:329`, `backend/mod.rs:66`, `orchestrator/transaction.rs`,
   `v8_classes/migration.rs:216` so the drift class closes
   crate-wide.
5. **Add the JS-input-parser bullet to error.rs:9-23.** Suggested
   wording in r5 Dimension 5 IMPORTANT (unchanged this round; still
   the cleanest 3-line edit on the error.rs preamble).

If 1+2+3 land before next round, the score breaks 88. The cleanest
path to 90 requires also clearing the v8_classes transaction.rs sweep
(item 4) — that's the largest single docs-debt parcel in the crate
and the only remaining drift class with FOUR rounds of carry-over.
