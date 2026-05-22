# `crates/plugin-db` — Architecture Review, Round 9

HEAD: `3d79d2da`. Prior rounds: R1 (64) → R2 (76) → R3 (81) → R4 (82) → R5 (83) → R6 (85) → R7 (89) → R8 (91).

This is a fresh re-audit. Four commits landed in the R8 → R9 window — all small, all mechanical, all on the patterns R8 already named. No new structural classes emerged; no judgment-call IMPORTANT closed; one R8 MINOR (M11) closed, one R8 IMPORTANT (I3 — `migrations::coded_db` re-implementing `prefix_message`) closed at the source by `deeefe18`, one R8 MINOR (M10) closed verified, one new docstring drift fixed at `3d79d2da`. The DETAIL-token classifier (R8's signature pattern) graduated to having direct unit tests via the `classify_detail_token` extraction (`f6043126`). The `Configuration` variant grew a `hint` field (`f1c5184e`) — long overdue, neat closure on the error-ux side, but it's a single-line shape change rather than a structural shift.

**The trajectory has plateaued.** R9 is +1 aggregate (91 → 92), the smallest movement since R3 → R4 (+1). Two of the four R8 → R9 commits are documentation hygiene. The crate has clearly entered the **asymptotic-polish stage**: the headroom remaining is concentrated in (a) the five carry-IMPORTANTs (I1/I2/I3/I4/I5), all of which are now judgment-call rather than correctness gaps; (b) the structural cosmetics (mint duplication M5, query.rs LOC).

What R8 → R9 added:

1. **`coded_db` collapse onto `prefix_message`** (`deeefe18`) — closes R8 M11. The last per-module variant-walker (in `migrations.rs:82-102`) now routes through the shared primitive. Net `prefix_message` consumer count: **7 modules** (audit, auth/bootstrap, auth/keys, auth/session, diff, replication, migrations). Pattern consolidation cluster is now the largest it has been in any round.
2. **SQLSTATE-typed checks in `replication.rs`** (`f6043126`) — two `msg.contains("42710")` / `msg.contains("55000")` substring matches replaced with `e.as_db_error()?.code() == &SqlState::DUPLICATE_OBJECT` / `OBJECT_NOT_IN_PREREQUISITE_STATE`. The pattern R8 closed in `auth/session.rs` propagated cleanly to its next site. Production-path substring matches on PG error message bodies now: **0** (vs 1 entering R8, 6 entering R7). The runtime-side `is_fatal` (wal_consumer.rs:710-722) is the lone remaining site, intentionally bounded (R8 M12 — operator-log-only, never crosses the SDK boundary).
3. **`classify_detail_token` extraction + tests** (`f6043126`) — the DETAIL-token map split from `classify_p0001_detail` into a pure `&str → Option<(&'static str, &'static str)>` helper. 7 new direct unit tests at `auth/session.rs:541-606` pin all 5 codes, unknown-token-returns-None (critical SDK-contract invariant), and the codes-are-distinct property. **The DETAIL-token pattern is now contract-tested**, closing the test-coverage gap R8 flagged in passing.
4. **`DbError::Configuration` gains `hint: Option<String>`** (`f1c5184e`) — 5 literal construction sites updated; 2 of them (wal_consumer.rs `not_provisioned`, replication.rs `wal_level_not_logical`) now ship meaningful operator-remediation hints. Adds `DbError::config_hinted()` constructor for the hinted form; existing `config()` stays hint-less. **First time the Configuration variant has had operator-remediation prose in a structured field** — the prior shape baked hints into the message body.
5. **Docs-audit r6 fixes** (`3d79d2da`) — three inline drift fixes (one CRITICAL where `deeefe18` left contradictory preamble blocks in `migrations.rs:67-86`; two IMPORTANT mid-list drifts). All cosmetic; no code change.

What didn't move: I1 (cfg-fork), I2 (Backend trait half-application), I4 (migration lock RAII), I5 (auth/* dormancy). Four of R8's five carry IMPORTANTs unchanged. Only I3 (`auto_tx`/`transaction` parallel openers) is unchanged at the code level — there was no tx-open change to force the extract.

---

## 1. Score Per Dimension (R8 → R9)

| Dimension | R8 | R9 | Δ | Driver |
|---|---:|---:|---:|---|
| Module boundaries | 65 | **65** | 0 | No new module moves. The `classify_detail_token` extraction is a same-file split (private helper inside `auth/session.rs`), not a module-boundary change. |
| Layering (orchestrator pipeline) | 89 | **89** | 0 | `run_pipeline` unchanged. The Configuration hint addition affects two lazy-init error sites in `register_model/mod.rs:119-123, 126-130` but their shape is unchanged. |
| Extension points (new `ChangeKind`, error variant, aggregator) | 64 | **66** | +2 | Two extension-point refinements: (a) `DbError::Configuration` is now `{code, message, hint}`, matching the `ValidationFailed` shape — extension paths for both variants are now symmetric (one constructor + optional remediation prose); (b) the DETAIL-token classifier is now extension-testable in isolation via `classify_detail_token(&str)`. Adding a 6th DETAIL token = one PG-side constant + one match arm + one test. |
| Coupling (replication / broker / wal_consumer) | 85 | **85** | 0 | No movement. The 3-commit lifecycle hardening from R7→R8 (`34d209b5`/`70921112`/`4b2e7046`) is stable; nothing in R8→R9 touched the WAL consumer except via the Configuration hint addition (one site in `wal_consumer.rs:349-358`). |
| Forward extensibility | 74 | **76** | +2 | (a) The DETAIL-token pattern is now exercised by unit tests, so adding a 6th token is mechanical (PG constant + Rust arm + test). (b) The SQLSTATE-typed pattern propagated from 1 file (auth/session.rs) to 2 (now also replication.rs) — the discipline isn't just present, it's reused. New SECURITY DEFINER / new SQLSTATE classification = one match arm in the matching classifier. |
| Coupling debt (cfg-fork visibility + duplicated patterns) | 62 | **63** | +1 | One small win: the last cluster-of-duplicates (`migrations::coded_db` re-implementing `prefix_message`) collapsed at `deeefe18`. Pattern duplication is at its lowest count of any round: **0 cross-module re-implementations of `prefix_message`**. Cfg-fork still 8 pairs unchanged (I1 carry). |
| Error rail discipline | 95 | **96** | +1 | (a) `Configuration` variant gains structured `hint` — the last unstructured operator-remediation channel (hint baked into message body) is now a typed field. (b) Two substring-on-error-message sites in `replication.rs` (lines 213, 257 pre-`f6043126`) became SQLSTATE-typed via `as_db_error()?.code()`. The error rail is now structured all the way down except for `wal_consumer::is_fatal` (R8 M12, intentionally bounded). |
| Security | 94 | **94** | 0 | No new security surface this round. The Configuration hint addition is operator-visible prose, not a privilege change. The SQLSTATE-typed checks tighten classification but the prior substring-based gates were correct on the happy path; the gain is robustness against locale change, not a vulnerability close. |
| Performance posture | 75 | **75** | 0 | No hot-path changes. The DETAIL-token classifier was already small; the extraction into `classify_detail_token` is a function-call indirection that the compiler inlines. The SQLSTATE classification is now slightly cheaper (no `format!("{e:#}")` allocation on the happy duplicate-object path — `as_db_error()` is borrow-only) but the path is not hot. |
| API surface | 71 | **72** | +1 | (a) `DbError::config_hinted` is a new public constructor — small surface expansion that fills an obvious gap; both validation and configuration variants now have `_hinted` siblings. (b) `Configuration { hint }` is technically a breaking change to a `#[non_exhaustive]` variant, but the variant is non-exhaustive precisely to allow this. (c) `classify_detail_token` is private (correctly — it's an implementation detail of `classify_p0001_detail`). |
| Pattern consolidation | 80 | **82** | +2 | (a) M11 closed at `deeefe18`: the last `prefix_message`-replicator collapsed. Cluster count: **7 modules routing through 1 shared variant-walker**, the largest single-pattern cluster in the crate. (b) SQLSTATE-typed PG-error classification propagated from 1 file to 2 (`replication.rs:209-228, 259-294` joins `auth/session.rs:174-215`). The pattern now has two production examples — early evidence of stickiness. (c) `DbError::config_hinted` mirrors `DbError::validation_hinted`; the variant-with-hint constructor pattern is now symmetric across Configuration and ValidationFailed. |

**Aggregate: 91 → 92.**

R9 movement is **+1 aggregate**. The driver mix:

- **Extension points (+2)** — DETAIL-token classifier is now unit-testable; Configuration variant gained symmetric `hint` field. Extension friction is the lowest it's been.
- **Forward extensibility (+2)** — SQLSTATE-typed pattern propagated to a 2nd file. DETAIL-token map graduated to direct unit tests.
- **Pattern consolidation (+2)** — M11 closed; SQLSTATE-typed PG-error classification cluster grew from 1 → 2 sites.
- **Coupling debt (+1), Error rail (+1), API surface (+1)** — small individual movements tied to the four commits above.
- **All other dimensions (0)** — R9 didn't touch the orchestrator pipeline, security surface, or hot paths. The lack of motion is correct, not regression.

The +1 aggregate is the smallest delta since R3→R4. **Trajectory: 64 → 76 → 81 → 82 → 83 → 85 → 89 → 91 → 92.** The asymptote is now visible — R3-R4 was the prior +1 jump (the bootstrap stage post-foundation); R8-R9 is the polish-stage +1 jump. R10 is likely +0 or +1 unless one of the carry-IMPORTANTs lands.

### What moved the score

- **DETAIL-token unit tests + `Configuration { hint }` (combined +5 across 4 dimensions)** — `f6043126` and `f1c5184e` together. The DETAIL-token classifier was R8's signature contribution but lacked direct test coverage (only 3 of 5 codes hit via integration tests; substring assertions on `msg.contains("invalid signature")` were doing dual duty as classifier-verification, which is exactly the antipattern the classifier was built to eliminate). The extraction split out a pure `&str → Option<(&'static str, &'static str)>` helper that 7 tests pin directly, including the critical SDK-contract invariant `classify_detail_token("session_unknown_future_token").is_none()` (the unknown-token-falls-through property — without it, a typo on the PG side could silently re-bucket into an existing code). The `Configuration { hint }` addition is a 1-LOC variant-shape change but a long-overdue closure — `Configuration` was the only structured variant carrying an operator-remediation channel via message-body convention rather than a typed field. The two changes together move the error-rail discipline from "structured at the boundary" to "structured all the way down except `is_fatal` (which is documented and bounded)."

- **`migrations::coded_db` collapse (+1 coupling debt, +1 pattern consolidation, indirect +1 forward extensibility)** — `deeefe18`. Mechanically a 6-LOC delta (the prior inline match-on-DbError-variant + format!-on-message at `migrations.rs:82-102` shrank to a one-liner `prefix_message(&mut e, ...); e.to_op_error()`). The deeper win: the `prefix_message` consumer count is now **7 modules**, which is the largest single-pattern cluster in the crate's history. The pattern was named in R6 (R6 carryover from R5 sweep), refined in R7 (`cbbc9059`), and finalised in R9 (`deeefe18`). The cluster covers every Postgres-touching subsystem of plugin-db. **Pattern consolidation 80 → 82 is driven mostly by this one commit** — when a pattern lands in 7 of 7 candidate sites, it's saturated; the cluster won't grow because there are no more candidates.

- **SQLSTATE-typed propagation to `replication.rs` (+2 forward extensibility, +1 error rail, +1 pattern consolidation)** — `f6043126`. R8 closed substring matching in `auth/session.rs` via DETAIL tokens; R9 took the *non-RAISE* version of the same pattern (`as_db_error()?.code() == &SqlState::CONSTANT`) and applied it to two sites in `replication.rs` (the `42710` `DUPLICATE_OBJECT` benign-race path and the `55000` `OBJECT_NOT_IN_PREREQUISITE_STATE` wal-level-not-logical path). The discipline is now visible in 2 files, not 1 — early evidence that it's not auth-specific. The pattern will spread further: every site in the crate that currently uses `format!("{e:#}").contains("<SQLSTATE>")` is now a candidate. Production-path substring matches on PG error message bodies: **0**.

### Trajectory narrative

The arc:

- **R1 → R3 (64 → 81)** — foundation: pipeline split, typed-id discipline, broker layout.
- **R3 → R5 (81 → 83)** — perf + classification: broker two-level, per-row gate, typed error rail design.
- **R5 → R6 (83 → 85)** — structure: `OrchestratorLockGuard` RAII, mint_subscription reorder, audit-progress before COMMIT.
- **R6 → R7 (85 → 89)** — mechanical closure: typed-error sweep, empty-RETURNING helper, cross-tenant scoping (CRITICAL closed).
- **R7 → R8 (89 → 91)** — lifecycle hardening: WAL-consumer Drop guard converges to its final atomic shape, construction-error rail typed, DETAIL-token classification.
- **R8 → R9 (91 → 92)** — **polish stage**: DETAIL-token classifier gains direct unit tests, last `prefix_message` replicator collapses, SQLSTATE-typed pattern propagates to a 2nd file, Configuration variant gains structured `hint` field.

R8 was "the last hostile pattern in the production path was removed." R9 is "the patterns that closed those gaps are now contract-tested and the structured variants are symmetric." This is the round where everything that *was* a deliberate-but-unproven discipline (DETAIL-token classification, SQLSTATE-typed checks, hint-on-structured-variants) became a discipline with **direct test coverage and >1 production instance**. Discipline that holds in one place is convention; discipline that holds across multiple sites with regression-test coverage is architecture.

**The trajectory has plateaued.** R7→R8 was +2 (1 step away from R5→R6's +2 norm); R8→R9 is +1 (1 step short of the norm, in line with the asymptote prediction R7 made for R10). R10 will not be +2 unless one of the 5 carry-IMPORTANTs lands code-side: the four "judgment-landed" closures (I1, I2, I4) are documentation-only and won't move the score; only I5 (wire up `--harden`) would be a substantive code change.

**Is the marginal improvement worth the cycle cost?** Yes-but-diminishing. R9 closed two test-coverage gaps (DETAIL-token unit tests, M11 via `deeefe18`'s `coded_db` shim test) and one Configuration-hint shape gap that the error-ux reviews had flagged for 3 rounds. Cycle output: 4 commits, ~150 LOC delta total, +1 aggregate. The cost is now > the marginal benefit by ~50%. R10 should focus on **either** landing I5 (the wire-up commit, which has been pending since R5) **or** accepting the asymptote and downshifting the review cadence.

---

## 2. Closed Since R8

| Finding | Source | How closed | Evidence |
|---|---|---|---|
| `migrations::coded_db` re-implements `prefix_message` inline | R8 M11 | `coded_db` now delegates to `crate::error::prefix_message` then `to_op_error()`. Variant-walk logic lives in one file. | `deeefe18`; `migrations.rs:84-93` (was inline match-on-variant; now `prefix_message(&mut db_err, &format!("{context}: ")); db_err.to_op_error()`) |
| `DbError::Configuration` lacked structured operator-remediation channel — hint was baked into message body | error-ux r7 INFO | `Configuration { hint: Option<String> }` field added; `config_hinted()` constructor mirrors `validation_hinted()`. 2 sites (wal_consumer.rs `not_provisioned`, replication.rs `wal_level_not_logical`) now ship hints. | `f1c5184e`; `error.rs:125-129, 245-247, 292-302`; `wal_consumer.rs:349-358`; `replication.rs:277-286` |
| `classify_p0001_detail` had no direct unit tests — 2 of 5 DETAIL branches uncovered | test-coverage r8 NEW gap | Extracted pure `classify_detail_token(&str) -> Option<(&'static str, &'static str)>`. 7 unit tests at `auth/session.rs:541-606` pin all 5 tokens + unknown-fallthrough + codes-are-distinct invariant. | `f6043126`; `auth/session.rs:184-215, 541-606` (7 new tests) |
| `replication.rs:213, 257` used `msg.contains("42710")` / `msg.contains("55000")` substring matches on rendered error messages | error-ux r7 NEW; same fragility class as R5/R8 MAJOR-R5-1 closed for auth | Both sites switched to `e.as_db_error()?.code() == &SqlState::DUPLICATE_OBJECT` / `&SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE`. Production-path substring matches on PG error messages now: 0. | `f6043126`; `replication.rs:215-227, 259-294` |
| `migrations.rs:67-86` had contradictory preamble blocks after `deeefe18` (old "classify through DbError" + new "Thin wrapper around prefix_message") | docs-audit r6 NEW CRITICAL | Collapsed into single coherent preamble describing post-`deeefe18` behavior. | `3d79d2da`; `migrations.rs:67-93` |
| `wal_consumer.rs:49-51` module preamble named obsolete SDK method `replicationConsumerStart` | docs-audit r6 NEW IMPORTANT | Renamed to `startReplicationConsumer` (actual v8_method name). | `3d79d2da`; `wal_consumer.rs:49-51` |
| `error.rs:336-341` `prefix_message` preamble missed `migrations::coded_db` as 7th consumer | docs-audit r6 NEW IMPORTANT | Updated to 7 consumers; added `deeefe18` commit citation. | `3d79d2da`; `error.rs:337-342` |

**Seven closures** — three are direct correctness/discipline closures (M11, Configuration hint, SQLSTATE-typed substring elimination); one is test-coverage (DETAIL-token unit tests); three are documentation hygiene. The closure ratio is high but the closures are small — R9 is the polish round, not a structural round.

---

## 3. New + Carried Findings (R9)

### CRITICAL

None.

---

### IMPORTANT

**I1 (carried from R6 I1, since R4, 5 rounds). `cfg`-forked module visibility is still eight pairs.**

Status: **unchanged**. `lib.rs:62-101` still defines eight modules twice (`pub(crate)` in normal builds, `pub` under `test-helpers`).

  Why: architectural impact

  Unchanged from R5/R6/R7/R8. This is the *only* IMPORTANT to carry across **five** review rounds without any movement.

  R9-specific observation: no `pub fn ..._for_tests` symbol was added in R8→R9 (the only candidate would be in `migrations.rs` for the `coded_db` rework, but `coded_db` is private and tested via the indirect `prefix_message` route). The 8-pair list is the right partition; it's been stable for 5 rounds. **Stability is itself evidence that the convention works.**

  Fix: same as R7/R8 — write the ADR explaining the convention, leave the code. The R8 recommendation was "close as judgment-landed"; R9 reaffirms.

  R9 recommendation: **close this IMPORTANT.** Write `docs/decisions/2026-05-22-plugin-db-cfg-fork-test-surface.md` (a 30-line ADR documenting the convention) and remove from carry list. The cost of keeping it open is review attention; the benefit of closing it is removing one item from the carry list.

  Verification: `lib.rs:62-101` (eight pairs unchanged); `Cargo.toml:31-37`; no new test-only `pub fn` added R8→R9.

  ---

**I2 (carried from R6 I3, since R3, 6 rounds). `Backend` trait still half-applied: `migrations.rs` (7 sites) + `register_model/{mod,bootstrap}.rs` (3 sites) take `&PostgresBackend` concrete.**

Status: **unchanged**. Concrete-typed signatures (verified at HEAD):

- `migrations.rs:206` (`exec_begin`)
- `migrations.rs:355` (`exec_fetch_batch`)
- `migrations.rs:440` (`exec_commit_batch`)
- `migrations.rs:467` (`rollback_and_return` private helper)
- `migrations.rs:665` (`exec_status`)
- `migrations.rs:706` (`exec_cancel`)
- `migrations.rs:739` (`exec_reset`)
- `register_model/bootstrap.rs:79` (`'p PostgresBackend`)
- `register_model/bootstrap.rs:150` (private `build_ctx`)
- `register_model/mod.rs:162` (`run_pipeline`)

`register_model/{plan,validate,apply}.rs` still consume `B: Backend` generically (`apply.rs:37`, `plan.rs:34`, `validate.rs:55`). `lock_guard.rs:97` is also generic.

  Why: architectural impact

  Unchanged from R5/R6/R7/R8. The R8 observation about `migrations::coded_db` reinforcing the "narrow" recommendation has now itself been resolved (`deeefe18` collapsed `coded_db` onto the shared `prefix_message`), so the new structural argument for closing I2 is even tighter:

  - `migrations.rs` no longer has any PG-specific helper logic of its own (the inline variant-walk is gone).
  - But the 7 PG-specific `exec_*` signatures remain — because their bodies *do* use PG-specific facilities (`pg_try_advisory_lock`, `audit_generation` bumps, `SELECT … FOR UPDATE`).
  - The structural barrier between "PG-specific implementation" (migrations.rs body) and "generic backend abstraction" (Backend trait) is now sharp: the helper layer is shared, the executor layer is PG-specific.

  Fix: **R9 leans even more strongly to "narrow"** than R8 did. The closure of M11 makes the narrow recommendation more defensible — without the inline `coded_db`, migrations.rs's PG-specificity is purely at the SQL-execution layer, exactly where R7's "narrow" recommendation places it.

  R9 recommendation: **close as judgment-landed.** Add `#![doc = "... migrations.rs is PG-specific by design ..."]` at the top of `migrations.rs` and the matching paragraph in `backend/mod.rs`'s module preamble. Remove from carry list. Same proposal as R8; R9 escalates the recommendation from "lean" to "close."

  Verification: `migrations.rs:206, 355, 440, 467, 665, 706, 739`; `register_model/bootstrap.rs:79, 150`; `register_model/mod.rs:162`; the `coded_db` simplification at `migrations.rs:84-93` is new evidence that PG-specific helper logic has been pushed out of migrations.rs entirely.

  ---

**I3 (carried from R6 I5, R7 I3, R8 I3, since R5). `auto_tx::exec_auto_begin` and `transaction::exec_begin` remain parallel transaction openers.**

Status: **unchanged**. `orchestrator/auto_tx.rs:178-227` and `orchestrator/transaction.rs:113-173` still share six structural steps in two files. Differences pinned at R7/R8 hold.

  Why: architectural impact

  No new divergence or convergence this round. The four R8→R9 commits all touched the error-rail / classification side of the crate (auth/session, replication, migrations, wal_consumer, error.rs); the two tx-open files were untouched.

  Fix: extract `pub(crate) async fn open_tx_session(begin_sql: &str, marker: TxMarker) -> Result<compio_postgres::Client, DbError>`. **Defer.** Unchanged from R7/R8.

  R9 observation: the **stability** of these two files across R7→R8→R9 (three review rounds, eight commits) is itself architectural information. The duplicated steps haven't been edited because there's no pressure to edit them. The R7 prediction ("next change to how we open a tx forces the extract") has been falsified by three rounds of no-such-change. **The duplication may be the stable shape** — not a deferred refactor but a deliberately parallel implementation that happens to share six steps. If neither file edits for another 3 rounds, R12 should reclassify I3 from "deferred extract" to "stable parallel pair."

  Verification: `auto_tx.rs:178-227`, `transaction.rs:113-173`. Six structural steps still in two files; no R8→R9 edits to either.

  ---

**I4 (carried from R7, R8). Migration advisory-lock has no RAII guard.**

Status: **unchanged**. `migrations.rs:259-285` (acquire + lock-mismatch return), `migrations.rs:653-663` (terminal release), `migrations.rs:617-618, 630-635` (mid-flight error returns that re-park the client without releasing).

  Why: architectural impact

  Unchanged from R7/R8. The migration lock's lifecycle is a state machine across multiple async dispatches; the OrchestratorLockGuard "acquire-and-release in one function" model doesn't fit.

  R9-specific observation: the **four lock-release semantics** R8 named (orchestrator lock, tx-connection lock, replication slot, migration lock) are still divergent. R8 recommended documenting them in one place; R9 confirms that hasn't happened.

  Fix: **same as R7/R8 — defer; document the 4-lock divergence.** The current code is correct.

  R9 recommendation: **close as judgment-landed (with documentation).** Either (a) write a per-crate README section enumerating the 4 lock semantics, or (b) add a `// LOCK SEMANTICS` block to `crates/plugin-db/src/lib.rs`'s preamble. Mechanical; doesn't change code; removes I4 from the carry list. Cheaper than I2's closure (no `#![doc]` per file).

  Verification: `migrations.rs:259-285, 617-618, 630-635, 644-657`; `context.rs:48-63` (`MigrationLock` struct).

  ---

**I5 (carried from R7, R8, since R3). `auth/*` module is dead code from JS — ~1850 LOC bootstrap+session+keys, zero production consumers.**

Status: **unchanged in wire-up; the dormancy is now in its third actively-recommended-action round.** R8 strengthened the recommendation from "should action" to "the maintenance cost is now clearly higher than the wire-up cost; action this round." R9 confirms zero progress: `--harden` still doesn't exist in `crates/cli/` or `crates/control/` (grep verified at HEAD).

R8 → R9 maintenance cost: **5 sites** in auth/* code were touched by `f6043126` (the SQLSTATE-typed-checks commit edited `auth/session.rs` to extract `classify_detail_token` + add 7 tests), `f1c5184e` (no auth/* edits — Configuration hint addition didn't reach auth/* sites because none of them use `Configuration`; the variant grew but auth code didn't), and `3d79d2da` (no auth/* edits). Net: 1 of 4 commits touched auth/*. Down from R8's 2 of 5 ratio, but still nonzero.

The DETAIL-token unit tests are exactly the kind of high-quality work R8 said "wants to be exercised in CI." The classifier now has direct unit-test coverage (good); it still has no production wire-up (unchanged).

  Why: architectural impact

  R7 called this "good ballast." R8 escalated to "the maintenance cost is clearly higher than the wire-up cost." R9 confirms: every cycle, more high-quality discipline goes into a subsystem that hasn't run a single integration test against a real PG instance in production CI.

  Fix: same three options from R7/R8. **R9 maintains R8's recommendation: action this round.** Adding the `--harden` CLI flag remains one commit. The cost asymmetry has only widened.

  R9-specific suggestion: the SQLSTATE-typed checks pattern from `f6043126` (replication.rs) is *exactly* the pattern auth/* uses. The two subsystems are now applying the same discipline at the same rate. **Wiring up `--harden` would let the integration tests exercise both** — currently they only exercise replication.rs (because the gateway/worker wire-up reaches `ensure_publication_and_slot`); the auth/* SECURITY DEFINERs never run.

  R9 trajectory probe: if R10 also lands without `--harden`, this becomes the longest-running IMPORTANT in the crate's history (5 rounds R5→R6→R7→R8→R9→R10). The pattern suggests this is either (a) an explicit deferral the user has reasons for that aren't documented, or (b) a coordination gap where the work needs to land in another crate (`crates/cli`) and the per-cycle pilot scope is `crates/plugin-db`. If (b), **the recommendation should propagate to a cross-crate pilot scope** — that's outside R9's domain to fix, but worth flagging.

  Verification: zero production consumers outside `tests/integration.rs`; `crates/cli` and `crates/control` have no `harden` subcommand or flag (grep verified); `auth/mod.rs:60-61` still references the non-existent flag.

  ---

### MINOR

**M1 (carried from R6, R7, R8). `validate.rs` returns `Result<_, String>` envelope rail.**

Unchanged. `validate.rs:55-59` still returns `Result<ApprovedPlan, String>`. R9 observation: the carryover is at its 4th round; this is becoming I3-class (stable parallel shape that may be the right shape, not a deferred refactor).

  Verification: `validate.rs:55-59`, `register_model/mod.rs:200-206`.

  ---

**M2 (carried). `AuditExecutor::query_text` returns `Result<Vec<Row>, compio_postgres::Error>`.**

Unchanged. `audit.rs:415-442`. Trivial swap when prompted.

  ---

**M3 (carried). `register_model_dispatch` resolves with `ResolveValue::String("null".to_string())`.**

Unchanged. `register_model/mod.rs:91`. Constant-time JS work per call. Carry.

  ---

**M4 (carried). `IsolateDbContext` fields remain `pub(crate)`.**

Unchanged. R9 observation: the `try_mark_consumer_running` method added in R7→R8 (`context.rs:419-426`) is still the only example of "right shape" (a method, not a public field). The eleven `pub(crate)` field declarations are stable.

  ---

**M5 (carried). Seven `mint_*` minters duplicate the boxed-instance + Weak finalizer dance.**

Unchanged. Seven `mint_*` / `migration_start_with_spec` functions in `v8_classes/{db,collection,replication,migrations,migration,subscription,transaction}.rs`. Flag for `runtime-macros` to absorb.

  R9 observation: the cluster of 7 here is the **same size** as the `prefix_message` consumer cluster (also 7). Both clusters are at their stable shape — `prefix_message` is consolidated (one shared primitive, 7 consumers, 0 re-implementations); `mint_*` is *un*-consolidated (7 ad-hoc implementations, no shared primitive). Two equal-cardinality patterns; opposite consolidation states. M5's closure pattern (extract a default-minter macro) would mirror what M11's closure achieved for `prefix_message`. **R9 recommends flagging M5 to runtime-macros explicitly in a roadmap doc** — it's the next natural pattern to close, and the closure shape is now well-established (consolidate to one primitive, leave thin shims at call sites).

  Verification: seven `mint_*` / `migration_start_with_spec` functions.

  ---

**M6 (carried). `broker.rs::Debug` impl walks two-level HashMap.**

Unchanged. `broker.rs:576-588`. Low priority.

  ---

**M7 (carried). `OrchestratorLockGuard::into_held` is `#[allow(dead_code)]`.**

Unchanged. R7 deadline (`revisit at R10`) is **next round**. R8/R9 didn't add a consumer. R10 should either delete the dead helper or take a position on whether it serves as a documented escape hatch.

  Verification: `lock_guard.rs:196-197`.

  ---

**M8 (carried). `mint_subscription`'s structural-invariant test is text-grepping its own source.**

Unchanged. `subscription.rs:258-342`.

  ---

**M9 (CLOSED at R8 → still closed). `wal_consumer::run_supervised` test coverage for `ConsumerRunningGuard` Drop guarantee on panic.**

Stays closed.

  ---

**M10 (R7 → CLOSED at R9). Stale `Result<_, String>` test comment in `replication.rs`.**

**CLOSED**. Verified at HEAD — the comment at `replication.rs:866-873` now correctly describes the post-`prefix_message`-consolidation state. No "stale `Result<_, String>` test comment" remains.

  ---

**M11 (R8 → CLOSED at R9). `migrations.rs::coded_db` re-implementing `prefix_message`.**

**CLOSED at `deeefe18`.** `coded_db` is now a 4-line shim that delegates to `prefix_message`. Variant-walk logic is in one place (`error.rs:351-369`).

  Verification: `migrations.rs:84-93`.

  ---

**M12 (carried from R8). `ConsumerError` still carries three `String`-shaped variants.**

Unchanged. `wal_consumer.rs:241-263` — `ConsumerError::Connect(String)`, `Io(String)`, `Decode(String)`. Documented at R8 as intentionally bounded (consumed by `is_fatal`, never crosses SDK boundary).

  R9-specific observation: the documentation R8 recommended ("ConsumerError variants are intentionally `(String)`-shaped because ...") still hasn't landed in `wal_consumer.rs`'s module preamble. The `is_fatal` substring matching at `wal_consumer.rs:710-722` is now the *only* remaining substring-on-PG-error-message site in the crate (production-path count: 0; runtime-loop count: 1). Worth making the boundary explicit.

  Fix: still not blocking. Add a one-paragraph note to `wal_consumer.rs:48-60` (module preamble) explaining: "ConsumerError carries `(String)` variants because the supervised loop classifies them via `is_fatal` and the SDK never sees them. SQLSTATE-precise classification belongs on the construction path (`DbError`), not the runtime path." Mechanical; 5-LOC delta.

  Verification: `wal_consumer.rs:241-263, 710-722`.

  ---

**M13 (R8 → carried). The `ConsumerRunningGuard` lifecycle history block has accreted 28 LOC of commentary.**

Unchanged. `replication_ops.rs:252-292`. Same R8 verdict — none actionable; flag.

  ---

**M14 (new R9, low). `replication.rs:275` still allocates `format!("{e:#}")` after the SQLSTATE check.**

`replication.rs:268-294` checks `is_wal_level_misconfig` via `as_db_error()?.code()` (the SQLSTATE-typed path), then *unconditionally* allocates `let msg = format!("{e:#}");` — even on the happy `wal_level_not_logical` branch, where `msg` is only used to inject the underlying error into the configuration message body. The allocation happens before the branch.

  Why: architectural impact

  Cosmetic. The path is not hot (logical-slot creation runs ~once per app provisioning). But the SQLSTATE-typed pattern from `f6043126` was specifically motivated by *not* materialising the error string when the typed check is enough — and this site partially regresses that. The `msg` is genuinely needed in the configuration-error body (the SDK surfaces the underlying PG error to the operator), so it's not pure waste; just an ordering issue.

  Fix: move `let msg = format!("{e:#}");` inside the `if is_wal_level_misconfig` arm. Or accept that the underlying message *is* the user-facing remediation context and document the choice.

  Verification: `replication.rs:268-294`.

  ---

**M15 (new R9, low). `f1c5184e` updated 5 Configuration sites but only 2 use the new `config_hinted()` constructor; the other 3 construct the struct literal directly.**

Of the 5 sites the commit updated for the new `hint` field:

- `wal_consumer.rs:349-358` uses struct literal `DbError::Configuration { code, message, hint: Some(...) }` — has a hint.
- `replication.rs:277-286` uses struct literal — has a hint.
- `backend/postgres.rs:593-600` uses struct literal — `hint: None` (intentional, invariant-class).
- `register_model/mod.rs:119-123, 126-130` (2 sites) use struct literal — `hint: None` (correctly chosen; lazy_init failures don't carry remediation prose).

The newly added `DbError::config_hinted()` constructor is **not used at any of the 5 sites**. The two hint-bearing sites still construct the struct literal directly.

  Why: architectural impact

  Cosmetic. The struct-literal-vs-constructor choice doesn't affect behaviour; the constructor exists for symmetry with `validation_hinted` and to make hint-bearing call sites self-documenting. The two sites that ship hints could switch to `config_hinted()` for one-line constructions; the three hint-less sites are correctly using either struct literal or `DbError::config()` (which already exists for the no-hint case).

  Fix: switch the 2 hint-bearing struct-literal sites to `config_hinted()`. Net delta: ~8 LOC each → ~4 LOC each. Mechanical.

  Verification: `error.rs:292-302` (constructor); `wal_consumer.rs:349-358`, `replication.rs:277-286` (the 2 candidate sites); `config_hinted` has zero callers outside its own definition (grep verified).

  ---

## 4. Direct Answers to the R9 Prompt Probes

**Q: auth/* dormancy (r8 I5) — still cross-crate blocked?**

Still blocked. R9 confirms zero progress: `--harden` does not exist in `crates/cli/` or `crates/control/` (verified via Grep — no matches anywhere in those crates). The wire-up requires editing a crate outside the per-cycle pilot scope of `crates/plugin-db`.

The auth/* maintenance pattern continues: 1 of 4 R8→R9 commits (`f6043126`) edited auth/* code, this time to extract `classify_detail_token` and add 7 tests. **The unit-test count of auth/* code grew by 7 in R9** — most disciplined dormant code in the crate. The asymmetry is now fully inverted relative to R5 ("good ballast"): the auth/* code is so well-maintained that not exercising it in CI is wasteful.

**R9 verdict: I5 is the only IMPORTANT with substantive code-change recommended.** I1/I2/I4 are documentation-landing closures. The +1 R9→R10 forecast hinges on whether I5 lands (in which case R10 is +2 or +3) or doesn't (R10 is +0 or +1).

**Q: Orchestrator pipeline — any new layering refinements?**

No layering refinement this round. The orchestrator pipeline (`run_pipeline`, 4 stages, lock handoff via `lock_guard`) is unchanged in shape. Three R8→R9 commits (`deeefe18`, `f6043126`, `3d79d2da`) didn't touch `orchestrator/`; one (`f1c5184e`) added the `hint: None` field to two `DbError::Configuration` construction sites in `register_model/mod.rs:119-123, 126-130` but the pipeline shape is unchanged.

R9 observation: the pipeline has now been stable for 3 review rounds (R7-R8-R9) at the same shape. **The pipeline is at its final form.** No new sequencing, no new lock-handoff variants, no new error-rail wrapping. Future changes to the pipeline will almost certainly be additions (a new stage between validate and apply, e.g.) rather than refinements of the existing stages.

**Q: Extension points (new ChangeKind, new error variant — Configuration just gained `hint`).**

Currently lowest-friction it's been. The new R9 observations:

- **`DbError::Configuration { hint }`**: adding a 6th hint-bearing construction site = one `DbError::config_hinted("code", "message", "hint")` call. Symmetric with `validation_hinted` since R9. The structured channel for operator remediation is now uniform across the two structured variants (ValidationFailed and Configuration).
- **New `ChangeKind`** — unchanged from R7/R8: compiler-enforced via exhaustive matches.
- **New `DbError` variant** — unchanged: `#[non_exhaustive]` enum at `error.rs:54-56`. `Configuration` gained `hint` non-breakingly via the non_exhaustive contract.
- **New DETAIL token in classifier** — adding a 6th token is now **unit-testable in isolation** via `classify_detail_token(&str)`. The friction is 1 PG-side constant + 1 Rust match arm + 1 test = 3 lines total.
- **New SQLSTATE-typed classification site** — now has 2 production examples (`auth/session.rs::classify_p0001_detail` for RAISE; `replication.rs::is_duplicate_object` and `replication.rs::is_wal_level_misconfig` for SQLSTATE-typed branches). The pattern has propagated and is templatable.

The aggregator path (`query.rs::build_aggregate` ≈ 211 LOC inline match) is the only meaningful remaining friction; scoped out since R2.

**Q: Pattern consolidation — `prefix_message` cluster size = 7 (best so far). Any new cluster emerging?**

Cycle status:

| Cluster | R7 sites | R8 sites | R9 sites | Status |
|---|---:|---:|---:|---|
| Advisory unlock (orchestrator lock) | 0 | 0 | 0 | Closed at R6 |
| Empty RETURNING | 0 | 0 | 0 | Closed at R7 |
| `coded_sql` per-module helpers | 5 thin shims | 5 thin shims | 5 thin shims | Closed at R7 (stable shape) |
| **`prefix_message` consumers** | 6 (with 1 re-implementer at migrations) | 6 (with 1 re-implementer) | **7 (zero re-implementers)** | **Closed R9 at maximum size** |
| Subscriber gate | 2 | 2 | 2 | Unchanged; abstract-worth, deferred |
| App-id stamp | 3 | 3 | 3 | Unchanged; judgment-call leave-alone |
| substring-match on PG error msg (production path) | 1 (auth/session.rs) | 0 | 0 | Closed R8, still closed R9 |
| **SQLSTATE-typed classification** (NEW R9) | 0 | 1 (auth/session.rs via DETAIL) | **2 (replication.rs + auth/session.rs)** | **Emerging cluster — pattern propagated to a 2nd file** |
| **`mint_*` v8_class minters** | 7 (no shared primitive) | 7 | 7 | **Stable un-consolidated** (mirror image of `prefix_message`) |
| substring-match on PG error msg (runtime-loop, bounded) | 1 (wal_consumer::is_fatal) | 1 | 1 | Stable; documented boundary (R8 M12) |
| `_hinted` constructor pattern (NEW R9) | 0 | 0 | **2 (validation_hinted + config_hinted)** | **Emerging cluster — symmetric across 2 structured variants** |

**Two new clusters emerged in R9, but both are at expected cluster sizes (no growth-to-saturation tail).** The SQLSTATE-typed classification cluster went from 1 to 2 — that's the early-evidence-of-stickiness signal. The `_hinted` constructor cluster is at 2 (validation + configuration); there's no third structured variant in the enum that would benefit from a `_hinted` constructor (`Coded` already carries its own hint).

The largest pattern cluster in the crate is now **`prefix_message` at 7 consumers**, which equals the un-consolidated `mint_*` cluster at 7. The closure ratio (consumers ÷ candidates) is now perfect for `prefix_message` (7/7) and zero for `mint_*` (0/7).

**Net: Pattern Consolidation went from 80 → 82 (+2). M11 closure + SQLSTATE-typed cluster grew from 1 to 2 sites.**

**Q: auto_tx vs transaction.rs — still divergent?**

Still parallel. No R8→R9 edits to either file. **Three consecutive review rounds (R7, R8, R9) have observed the same 6-step parallel structure with no editor pressure on either file.** The R7 prediction ("next tx-open change forces the extract") has been falsified by 3 rounds of no-such-change.

R9 observation, escalated from R8's M-ish note: this may be the *stable* shape, not a deferred refactor. If R10 also lands with neither file edited, R10 should reclassify I3 from "deferred extract" to "stable parallel pair" (note the same R12 escalation flag at I3 above). The 6-step parallel may simply be the right factorisation — it reads cleanly in each file and doesn't actually save lines under extraction (an `open_tx_session(begin_sql, marker)` extract would still need the auto-tx site to pass the marker, the transaction site to skip it — 4 LOC saved at most).

**Q: Backend trait half-application — judgment-call status.**

Half-applied at the same 10 sites (verified at HEAD; same count as R8). The trait is sound; consumer coverage is partial; `migrations.rs` is increasingly path-dependent on PG specifics — but the R9 closure of M11 (`coded_db` collapsed onto `prefix_message`) **removed the most-recent piece of evidence for PG-specificity** in helper logic. Migrations is now PG-specific only at the SQL-execution layer, which is sharp and defensible.

R9 leans **strongly to "narrow"** (the R8 lean strengthened by one round of M11 closure). The closure of M11 makes "migrations.rs is PG-specific by design" easier to defend — without inline `coded_db`, the PG-specificity is purely at SQL-execution, exactly where R7's "narrow" recommendation places it.

See I2 above. **R9 recommendation: close as judgment-landed (with documentation).** Same shape as R8, escalated.

**Q: cfg-fork test-helpers visibility — 8 modules.**

Right shape. Unchanged since R5 (5 rounds of stability). No new `_for_tests` symbol added in R8→R9. The convention is stable; the cost of keeping I1 open is review attention.

R9 recommendation: **close as judgment-landed**. Write the ADR; remove from carry. Same shape as R7/R8.

**Q: @zeroship/bootstrap boundary — clean.**

Clean. The four R8→R9 commits touched:

- `deeefe18` (migrations::coded_db → prefix_message): invisible to JS — both shapes produce the same `OpError`.
- `f1c5184e` (Configuration { hint }): the JS-visible shape `{ message, code, hint? }` is the same wire format `OpError` already supported. **Two more sites now ship a `.hint` field** (wal_consumer.rs `not_provisioned` + replication.rs `wal_level_not_logical`) — net additive to the JS-visible contract.
- `f6043126` (SQLSTATE-typed checks + classify_detail tests): `.code` shape unchanged.
- `3d79d2da` (docs): no functional change.

**Net: bootstrap boundary is clean.** One small improvement (two more sites now expose remediation prose in the structured `.hint` field instead of baking it into the message body).

**Q: Has the trajectory plateaued? Is the marginal improvement worth the cycle cost?**

Yes, the trajectory has plateaued.

Evidence:

- **R7→R8 was +2** (the WAL-consumer lifecycle hardening + DETAIL-token classification — three commits at the same subsystem).
- **R8→R9 is +1** (smaller delta, four commits at four different files, two of which are documentation).
- **No CRITICAL since R7** (cross-tenant scoping was the last CRITICAL, closed at `c0590506`).
- **5 carry-IMPORTANTs unchanged**: I1, I2, I3, I4, I5 all carry. Three (I1, I2, I4) are judgment-landed closures — they won't move the score by code change. One (I3) is now an open question of whether to reclassify rather than extract. One (I5) requires cross-crate work.
- **Cluster consolidation reaching saturation**: `prefix_message` at 7/7 candidates, substring-match-on-error-msg at 0 (production path), `_hinted` constructor pattern at 2/2 candidates. The only large un-consolidated cluster is `mint_*` (0/7), which is flagged for `runtime-macros` rather than plugin-db.

Marginal improvement vs cycle cost:

- **R8→R9 cycle cost**: 4 commits, ~150 LOC delta total (including 3 doc-only edits in `3d79d2da`).
- **R8→R9 marginal improvement**: +1 aggregate score, 7 new unit tests, 2 new R9 MINORs flagged (M14, M15).
- **Cost/benefit ratio**: ~1.5× R8's ratio (R8 was 5 commits + ~350 LOC for +2; R9 is 4 commits + ~150 LOC for +1). Per-commit value is roughly equal but per-LOC value has dropped because the commits are smaller.

The honest assessment: **R10 should either land I5 (the substantive cross-crate change) or downshift the review cadence**. The current cadence is generating high-quality refinements that fall below the threshold needed to move the score meaningfully. The crate has crossed 90; the remaining headroom is bounded by the carry-IMPORTANTs.

Three options for R10:

1. **Land I5** (wire up `--harden` in `crates/cli`). Cross-crate work, one substantive commit, likely moves R10 to 93-94. The auth/* dormancy has been pending since R3 (5+ rounds).
2. **Close I1 + I2 + I4 as judgment-landed** (3 small documentation commits). Removes 3 items from the carry list; net effect on the score is ~+0 to +1 (the dimensions are not heavily weighted on documentation-only closures), but **simplifies the deferred backlog substantially**.
3. **Downshift cadence**. Run reviews every other cycle instead of every cycle; the marginal yield doesn't justify weekly review attention any more.

R9 recommendation: **option (1) is highest expected value**. The wire-up commit unblocks the four "judgment-landed" closures (they're being deferred partly because the broader auth wiring is the larger question; once it lands, the documentation closures become trivial). Option (2) is the fallback if (1) is genuinely cross-crate-blocked. Option (3) is the right answer if R10 also fails to move on I5.

---

## 5. Still Deferred (Carry-Over)

| Item | Origin | Actionability | R9 movement |
|---|---|---|---|
| `query.rs` 4277 LOC, `build_aggregate` ≈ 211 LOC inline match | R1 | Defer until aggregator-extension PR | None |
| Audit table write-only — no `db.audit.*` JS surface | R1 S5 | Low priority | None |
| WAL cross-tenant isolation is Rust-only | Security R1 | P8c work (now I5 — recommend action) | None |
| Migration advisory-lock has no RAII guard | Security R1 / R6 / R7 I4 / R8 I4 | Document the 4-lock divergence | R9 escalates "close as judgment-landed" |
| `auto_tx`/`transaction` tx-open extract | R5 I6 / R6 / R7 / R8 / R9 I3 | **Reclassify candidate** (stable parallel pair?) | R9 escalates "reclassify, not refactor" |
| `IsolateDbContext` field privacy (M4) | R5 | Cosmetic | None |
| `Debug` for `Broker` caches buckets count (M6) | R5 | Cosmetic | None |
| `mint_*` duplication (M5) | R4 | Flag for runtime-macros to absorb | R9 strengthens: 7/7 saturation, mirror image of `prefix_message`; same closure shape applies |
| `into_held` dead code (M7) | R6 | **R10 deadline** | Next round |
| `ConsumerError::(String)` runtime variants (M12 R8) | R8 | Document the boundary | R9 confirms the boundary still undocumented in module preamble |
| `replication.rs:275` allocates `format!("{e:#}")` before SQLSTATE branch (M14 R9) | R9 | Mechanical | New |
| `config_hinted` constructor exists but 0 callers (M15 R9) | R9 | Mechanical | New |
| `ConsumerRunningGuard` 28-LOC history block (M13 R8) | R8 | Move to ADR eventually | None |

---

## 6. Overall Score: 92/100

**Trajectory: 64 → 76 → 81 → 82 → 83 → 85 → 89 → 91 → 92.**

R9 movement is **+1 aggregate** — the smallest delta since R3→R4 (+1). The asymptote is now visible:

- **R3→R4: +1** (foundation-to-build-out transition)
- **R7→R8: +2** (lifecycle-hardening transition)
- **R8→R9: +1** (polish-to-asymptote transition)

The driver mix for R9:

- Extension points (+2), Forward extensibility (+2), Pattern consolidation (+2) — the dimensions tied to the SQLSTATE-typed propagation, DETAIL-token unit tests, and M11 closure.
- Coupling debt (+1), Error rail (+1), API surface (+1) — small individual movements tied to `f1c5184e` (Configuration hint) and `deeefe18` (M11).
- All other dimensions (0) — R9 didn't touch the orchestrator pipeline, security surface, hot paths, or module boundaries.

### Verdict per dimension comparison

R8 → R9 net dimension movement:

```
Module boundaries        65 → 65 ( 0)
Layering pipeline        89 → 89 ( 0)
Extension points         64 → 66 (+2)
Coupling                 85 → 85 ( 0)
Forward extensibility    74 → 76 (+2)
Coupling debt            62 → 63 (+1)
Error rail discipline    95 → 96 (+1)
Security                 94 → 94 ( 0)
Performance              75 → 75 ( 0)
API surface              71 → 72 (+1)
Pattern consolidation    80 → 82 (+2)
```

Biggest movers (+2): Extension points, Forward extensibility, Pattern consolidation — all tied to the SQLSTATE-typed propagation + DETAIL-token unit tests + M11 closure.

Dimensions that didn't move (Layering, Coupling, Security, Performance, Module boundaries): R9 didn't touch the relevant code. The lack of motion is correct, not regression.

### R10 forecast

If I5 lands (`--harden` wire-up): R10 ≈ 93-94. The wire-up exercises auth/* in CI for the first time; module boundaries (+2), security (+1), forward extensibility (+1) move.

If I1/I2/I4 land as documentation closures (no I5): R10 ≈ 92 (no score change but 3 fewer carry items).

If nothing lands: R10 ≈ 92, and the recommendation downshifts to "reduce review cadence."

The crate is in **strong architectural shape**. R9 confirmed the asymptote — the patterns that closed correctness gaps in R6-R8 are now contract-tested (DETAIL-token unit tests), saturated (`prefix_message` at 7/7 consumers), and propagating to 2nd sites (SQLSTATE-typed classification). The remaining IMPORTANTs are all judgment-call or wire-up; none are correctness gaps. **The marginal improvement is no longer worth the per-cycle cost at the current cadence.**

### Honest assessment: has the trajectory plateaued?

Yes. Evidence:

- R8→R9 is the smallest aggregate delta since R3→R4 (+1).
- 5 of 5 carry IMPORTANTs are judgment-call rather than correctness gaps.
- 2 of the 4 R8→R9 commits are documentation hygiene.
- Pattern clusters are reaching saturation (`prefix_message` 7/7, substring-match 0 production-path, `_hinted` 2/2 candidates).
- No CRITICAL since R7 (cross-tenant scoping).
- The orchestrator pipeline has been stable for 3 rounds.
- The 6-step parallel between `auto_tx`/`transaction` has been stable for 3 rounds (I3 is candidate for reclassification).

The crate has crossed 90 and is now in the asymptotic-polish stage. R10's marginal improvement is gated on the I5 wire-up; absent that, R10 is +0 or +1.

---

## Relevant Files

- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/migrations.rs` — `coded_db` 4-line shim post-deeefe18 (lines 84-93); 7 `&PostgresBackend` signatures (I2); migration lock state-machine (lines 259-285, 617-618, 630-635, 644-657, I4)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/error.rs` — `prefix_message` (lines 351-369, 7 consumers); `Configuration { hint }` post-f1c5184e (lines 125-129, 245-247, 292-302); `config_hinted` constructor (lines 292-302); preamble (lines 1-46)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/session.rs` — `classify_p0001_detail` (lines 174-182); `classify_detail_token` extraction (lines 184-215); 7 unit tests for classify (lines 541-606); `init_session` typed-error rail (lines 217-263)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/bootstrap.rs` — 5 RAISE EXCEPTION sites with USING DETAIL (lines 525-558); `coded_sql` shim (lines 24-26)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/replication.rs` — SQLSTATE-typed `is_duplicate_object` (lines 215-227); SQLSTATE-typed `is_wal_level_misconfig` (lines 259-294); `Configuration { hint: Some(...) }` post-f1c5184e (lines 277-286); `prefix_message` consumers (lines 203, 224, 240, 289, 419, 561, 591, 623)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/wal_consumer.rs` — `WalConsumer::new` typed-error with `Configuration { hint }` (lines 347-373); `ConsumerError` runtime-only variants (lines 241-263, M12); `is_fatal` substring-match-on-runtime-errors (lines 708-725, intentionally bounded); preamble citing `startReplicationConsumer` (lines 49-51)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/replication_ops.rs` — `ConsumerRunningGuard` module-scope (lines 328-355); 4-test suite (lines 367-413); comment block history (lines 252-292, M13)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/context.rs` — `try_mark_consumer_running` (lines 419-426); 11 `pub(crate)` fields (M4)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/lib.rs` — 8 cfg-fork pairs (lines 62-101, I1); preamble (lines 34-46)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/mod.rs` — `run_pipeline` (lines 161-228); `&PostgresBackend` concrete typing (line 162, I2); two `DbError::Configuration` sites with `hint: None` (lines 119-123, 126-130)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/bootstrap.rs` — `&'p PostgresBackend` (lines 79, 150, I2)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/{apply,plan,validate}.rs` — `B: Backend` generic (apply.rs:37, plan.rs:34, validate.rs:55); `validate.rs:55-59` envelope rail (M1)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/auto_tx.rs` — parallel `exec_auto_begin` (lines 178-227, I3); no R8→R9 edits
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/transaction.rs` — parallel `exec_begin` (lines 113-173, I3); no R8→R9 edits
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/lock_guard.rs` — `into_held` dead code (M7, R10 deadline)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/backend/postgres.rs` — `Configuration { code: "cic_configuration", hint: None }` (lines 593-600, post-f1c5184e)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/audit.rs` — `coded_sql` shim (lines 58-60); `AuditExecutor::query_text` returns `compio_postgres::Error` (lines 415-442, M2)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/broker.rs` — `Debug` impl walks two-level HashMap (M6)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/mod.rs` — dormant module preamble (lines 56-62) referencing non-existent `--harden` flag (I5)
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-architecture-review-2026-05-22-r8.md` — prior round (91)
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-deferred.md` — deferred backlog (last triaged 2026-05-22 08:30)
