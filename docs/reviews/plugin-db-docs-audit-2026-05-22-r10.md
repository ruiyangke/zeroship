# plugin-db docs audit — round 10 (2026-05-22)

**Scope:** `crates/plugin-db/src/` preambles + docstrings + inline WHY
comments at HEAD `baa262c1`. Re-anchored on r9 (92/100).

**Cycle 16:17 commits verified:**

- `7506bd73` plugin-db/tests: NEW-R14-1 CHECK ALTER upgrade-path integration test
- `cbd21112` plugin-db/apply: F1 warn-shape unification at `apply.rs:84` (MINOR-R12-1)
- `baa262c1` docs/reviews: cycle 16:17 reviewer reports + design-loop rounds 3-4 critiques + deferred update

**TL;DR.** `7506bd73`'s integration-test docstring is accurate end-to-end:
the inline `// 23b.` block names the gap honestly ("the prior … only
exercises the no-op rewrite branch") and the test does exactly what its
docstring claims (seed OLD CHECK → sanity-refuse → ALTER → grep
`pg_get_constraintdef` → success-insert → bad-insert sanity).
**However, `cbd21112` lands the F1 unification at apply.rs:84-95 cleanly
in the call site, but introduces a NEW NIT (count drift):** the inline
comment at apply.rs:85 says "(6+ sites pinned at cycle-12:47 `7c6bd2ec`)"
which is now stale — the F1 family is **8 production sites at HEAD**
(commit message's "8th F1 family site" is the correct count). The same
unification surfaces SEVEN pre-existing site-count drifts already-latent
in source: migrations.rs:648 ("6 F1 warn sites"), migrations.rs:650
("the other 5 sites"), apply.rs:188 ("all 5 F1 sites"), apply.rs:563
("all 6 F1 sites"), apply.rs:577 ("differ across the 6 F1 sites"),
apply.rs:592 ("8-site F1 family … 5 sites … 3 sites" — total is right,
split is wrong: actual is 6 audit_id + 2 collection), apply.rs:636
("all 3 insert-failure sites" — only 2 exist). The drifts all
under-count, so operator-grep contracts still WORK; the prose has fallen
behind the code. The `baa262c1` deferred-backlog header itself echoes
the same off-by-one ("F1 family now 7 sites with uniform shape" via
error-ux r12 — actually 8). Three r7→r10 NIT carry-overs (auth/{bootstrap,
keys,session}.rs preambles) and the EIGHT-round-old plugin-system.md
Crate structure tree carry forward unchanged.

---

## Dimension 1 — `7506bd73` integration test docstring accuracy

```
[OK] crates/plugin-db/tests/integration.rs:1006-1024 — block + fn docstring honest
  Why: the 23b. block at 1007-1017 names the gap honestly — "the prior
  `a3_audit_table_created_and_idempotent` only exercised the fresh-table
  branch (CREATE with new CHECK, then ALTER to the same body — no-op
  rewrite). The actual migration operators hit on every existing-app
  deploy is DROP-old / ADD-new". The fn docstring at 1020-1024 summarises:
  "Seeds the audit table with the pre-`6afab751` OLD CHECK constraint
  (7 statuses, no `validation_refused`), runs `ensure_audit_table_exists`,
  and asserts the constraint was widened and now accepts the new value."
  Cross-checked against the test body:
  - 1042-1080: seeds the table with a hand-rolled CREATE whose status
    CHECK lists exactly 7 statuses (`pending`, `running`, `applied`,
    `applied_with_dead_letter`, `failed`, `cancelled`, `rolled_back`) —
    matches the pre-`6afab751` shape verbatim. ✓
  - 1086-1108: sanity-asserts `validation_refused` insert fails with
    SQLSTATE 23514 (`pre_code` assert). ✓
  - 1112: calls `ensure_audit_table_exists`. ✓
  - 1118-1138: greps `pg_get_constraintdef` for `validation_refused`.
    ✓ Uses `pg_get_constraintdef` not constraint-name match — comment
    at 1118-1121 explains why ("the most reliable thing to grep — names
    alone could match a stale leftover"); accurate.
  - 1141-1149: post-ALTER insert of `validation_refused` succeeds. ✓
  - 1153-1172: bad-insert sanity (unknown status → 23514) — proves the
    widening didn't accidentally drop+forget-to-readd. ✓
  The docstring lists 4 assertions; the test runs 4 + 1 extra
  (bad-insert sanity). Honest under-claim is fine — the docstring is
  the floor, not the ceiling. Comment at 1153-1156 names why the bad
  insert exists. No drift.
```

## Dimension 2 — `cbd21112` apply.rs:84 inline comment accuracy

```
[NEW NIT] crates/plugin-db/src/orchestrator/register_model/apply.rs:84-88
  comment says "(6+ sites pinned at cycle-12:47 `7c6bd2ec`)".
  Why: at HEAD the F1 family is 8 production warn sites (commit
  message's claim of "8th F1 family site" is correct). "6+" was true
  immediately post-`7c6bd2ec` (cycle 12:47) but the validate.rs site
  landed at cycle 15:47 (`d07616a2`) and the apply.rs site landed at
  cycle 16:17 (`cbd21112` itself). The comment is the freshest inline
  WHY in the F1 family yet refers to a stale moment-in-time count.
  Closure: rewrite to "8 sites pinned at cycle-16:17 (`cbd21112`),
  unification anchor cycle-12:47 (`7c6bd2ec`)". One line.

[OK] cbd21112 field-shape matches the F1 contract verbatim.
  Why: the warn at apply.rs:89-95 emits `app_id` + `collection` +
  `transition = "Running/insert_failed"` + `audit_err`. Same fields
  as validate.rs:102-108's `"ValidationRefused/insert_failed"` variant
  — both share the collection-slot identifier (no `audit_id` yet
  because the insert FAILED, so there's no row to identify). The
  `f1_warn_shape_collection_slot_documentation_snapshot` test at
  apply.rs:606-653 verifies the contract holds.
```

## Dimension 3 — Pre-existing F1 site-count drifts surfaced by cbd21112

```
[NEW NIT cluster — pre-existing but now glaring] Seven site-count
  claims in source comments are now stale at the 8-site count:

  - migrations.rs:648 "unified across the 6 F1 warn sites" → 8 now.
  - migrations.rs:650 "the other 5 sites' string-literal discriminators"
    → 7 now (migrations.rs uses `?terminal` Debug form; the other 7
    are literals — but the count is wrong).
  - apply.rs:188 "unified across all 5 F1 sites" → 8 now.
  - apply.rs:563 "MUST be present across all 6 F1 sites (apply.rs /
    backend/postgres.rs / migrations.rs)" → 8 now; the file list is
    also incomplete (now adds validate.rs).
  - apply.rs:577 "differ across the 6 F1 sites" → 8 now.
  - apply.rs:592 "8-site F1 family splits into two identifier-slot
    variants: 5 sites carry `audit_id`; 3 sites carry `collection`"
    → total of 8 is right but the split is wrong: 6 audit_id (apply
    189/220, postgres 496/548/597, migrations 652) + 2 collection
    (apply 89, validate 102) = 8. The "3 sites carry `collection`"
    overcounts by 1.
  - apply.rs:636 "across all 3 insert-failure sites (validate.rs:101,
    apply.rs:84, and the matching secondary-failure paths)" → only 2
    sites match (validate.rs:101 + apply.rs:84). The "matching
    secondary-failure paths" sounds aspirational; no third site exists.

  All seven drifts under-count, so the operator-grep contract still
  WORKS (greppers find more sites than the prose claims). The risk is
  the next contributor reading apply.rs:592 + apply.rs:636 will
  hallucinate a 3rd collection-slot site that doesn't exist.

  Closure: a single sweep updating all seven counts to "8 sites: 6
  audit_id + 2 collection" (with the file list `apply.rs / validate.rs /
  backend/postgres.rs / migrations.rs`) closes the cluster. Same line
  count after the sweep.
```

## Dimension 4 — deferred-backlog header (`baa262c1`)

```
[OK] Header structurally accurate; commit SHAs all resolve.
  Verification: 7506bd73 / cbd21112 / 02ead3f4 / d07616a2 / 0bf71f27 /
  14d7608f / 6afab751 — all 7 SHAs referenced by the deferred header
  resolve in `git cat-file -e`. Reviewer-report filenames
  (`plugin-db-{error-ux,code-critique,migration-pipeline}-2026-05-22-r1{2,4}.md`)
  all exist. Design-loop critique files round-3 and round-4 both exist.

[NEW NIT] deferred-backlog header at line 5 says "F1 family now 7
  sites with uniform shape" (echoing error-ux r12's claim).
  Why: error-ux r12 counted 7 BEFORE the cbd21112 site (i.e. counted
  the pre-cbd21112 state) but the header was written AFTER cbd21112
  landed. So the same paragraph says "8th F1 family site" (commit
  message) AND "F1 family now 7 sites" (reviewer paraphrase). Both
  cannot be correct; at HEAD the count is 8.
  Closure: change "now 7 sites" → "now 8 sites" in the deferred header.

[OK] The cycle 15:47 paragraph at line 9 is unchanged and still
  accurate (refs `02ead3f4`/`d07616a2`/`0bf71f27`).

[OK] The "[C3] actionability correction" at line 17 carries verbatim
  from cycle 15:17 and is still consistent with the user's
  "never estimate without a measurement" instruction.

[OK] Refactor-safe / Design-pending filter (lines 19-23) carries
  unchanged. Items added to "refactor-safe" all still match (F1
  sweeper-half is refactor-safe; F2 is closed but the entry stays
  for line archaeology).
```

## Dimension 5 — Cross-references resolve

```
[OK] All commit SHAs in source comments resolve at HEAD:
  - `6afab751` (audit.rs:263, validate.rs preamble, apply.rs:597,
    audit.rs:374, integration.rs:1008/1022/1038): resolves.
  - `14d7608f` (validate.rs:68-81 inline ref): resolves.
  - `7c6bd2ec` (apply.rs:86 / validate.rs:99 / migrations.rs:647-651
    references "F1 warn-half pinned at cycle 12:47 `7c6bd2ec`"):
    resolves.
  - `18aee490` (apply.rs:533 + test_support/mod.rs:15 "reverted the
    drift after a code review caught it"): resolves.
  - `5d9acab8` / `fcf7ce3c` (test_support/mod.rs:22): resolves.
  - `02ead3f4` (audit.rs `invalid_app_id` error rewrite): resolves.
  - `0bf71f27` (test_support/mod.rs capture-layer commit): resolves.

[OK] Reviewer-report cross-refs in source preambles all match files
  that exist:
  - migration-pipeline r13 (validate.rs F2 inline at lines 68-81):
    file exists.
  - code-critique r11 MINOR-R11-1 (migrations.rs:647-651, apply.rs:187):
    file exists.
  - error-ux r10 (audit.rs:818 alphabet-naming history): r10 file
    exists.
```

## Dimension 6 — No NEW tracing emission added by cycle 16:17 (just shape unification)

```
[OK] cycle 16:17 added zero NEW tracing call sites. cbd21112 REWROTE
  an existing `tracing::warn!(error = ?e, "audit: failed to insert
  running row")` into the F1-family-shape variant; the call site
  itself (apply.rs:84) pre-existed. 7506bd73 is test-only — no
  production tracing.
  Verification: `tracing::(warn|error|info)!` count in
  crates/plugin-db/src/ across cycle 15:47 → 16:17:
  - migrations.rs: 4 unchanged (291, 652, 670, 1010).
  - apply.rs: 5 unchanged (89 rewritten by cbd21112, 189, 220, 544,
    plus 481 outside F1 family).
  - postgres.rs: 3 unchanged (496, 548, 597).
  - validate.rs: 1 unchanged (102).
  - context.rs / v8_bridge.rs / wal_consumer.rs / lock_guard.rs:
    unchanged.
  No drift between docs and code at fresh sites — there ARE no fresh
  sites this cycle.
```

## Dimension 7 — F2 docs from r9 still hold post-cycle 16:17

```
[OK] audit.rs:368-375 `update_audit_status` docstring (closed by
  d07616a2 at cycle 15:47) still accurate post-cycle-16:17.
  Why: lines 369-371 list "running -> applied | applied_with_dead_letter
  | failed | cancelled | validation_refused" — all 5 TerminalStatus
  variants enumerated. Lines 372 names the source-state list
  ("`Ok(true)` if the row transitioned"). Lines 373-375 carry the
  honesty note that `validation_refused` is normally INSERT-direct
  from validate.rs (cycle-15:17 `6afab751`), with this method
  accepting it for symmetry. r9's NEW NIT confirmed CLOSED.

[OK] No NEW state-machine docstrings added by cycle 16:17, so r9's
  Dimension 11 verification carries unchanged: the audit.rs:414-423
  backfill-helpers diagram correctly omits `ValidationRefused` (it's
  `phase='ddl'`, not backfill); the register_model/mod.rs:14-17
  strictness summary is still consistent.
```

## Dimension 8 — `docs/reference/db.md` operator-grep contract for `validation_refused`

```
[MINOR — UNCHANGED from r9] docs/reference/db.md still does NOT
  document the `__zeroship_migrations.status = 'validation_refused'`
  operator-grep contract for `strictness("lenient")`. Carries from r9.
  One paragraph in db.md's `.strictness()` section would close.
```

## Dimension 9 — Carry-overs as one-liners

```
[NIT — UNCHANGED from r7/r8/r9] auth/bootstrap.rs:1-9 — preamble could cite the hardening cargo gate (one sentence). 4-round carry.
[NIT — UNCHANGED from r7/r8/r9] auth/keys.rs:1-9 — preamble could cite the hardening cargo gate. 4-round carry.
[NIT — UNCHANGED from r7/r8/r9] auth/session.rs:1-19 — preamble could cite the hardening cargo gate. 4-round carry.
[NIT — UNCHANGED 8 rounds] docs/reference/plugin-system.md:315-344 stale "Crate structure" tree. Six paths that don't exist (`src/callbacks.rs`, `src/validate.rs`, `src/migrate.rs`, `plugin-auth` crate, `pg/` crate). EIGHT-round carry.
[NIT — UNCHANGED from r9] validate.rs:95-97 "Best-effort: a failure to write the audit row should not mask the envelope" — applies literally only to strict mode; lenient mode has no envelope to mask. Carry from r9.
[MINOR — UNCHANGED from r9] docs/reference/db.md does not document `__zeroship_migrations.status = 'validation_refused'` operator-grep contract for `strictness("lenient")`.
[NEW NIT — round 10] apply.rs:84-88 "(6+ sites pinned at cycle-12:47 `7c6bd2ec`)" — count drift; cbd21112's own inline WHY refers to a stale moment-in-time.
[NEW NIT cluster — round 10] Seven pre-existing F1-site-count claims (migrations.rs:648/650; apply.rs:188/563/577/592/636) under-count after cycle 16:17. All under-count, so operator-grep contracts still WORK — but apply.rs:592 + 636's "3 collection-slot sites" hallucinates a third site that doesn't exist.
[NEW NIT — round 10] deferred-backlog header at line 5 says "F1 family now 7 sites" while the same paragraph credits cbd21112 as the "8th F1 family site". Header internally inconsistent.
```

---

## Score: 91 / 100  (r9: 92;  −1)

**Delta breakdown (−1 from r9):**

- **+1** — `7506bd73`'s integration-test docstring is exemplary: the
  block-level comment names the gap honestly, the fn docstring
  summarises the contract, and the test body matches the claim
  verbatim with one extra sanity check the docstring honestly omits.
  No drift.
- **+0** — `cbd21112` lands the F1 unification at the call site with
  correct field shape; matches the commit message's "8th F1 family
  site" claim against the actual production count. The call site
  itself is clean.
- **−1** — `cbd21112` introduces NEW NIT at apply.rs:84-88 (inline
  comment says "6+ sites" — stale at 8).
- **−1** — NEW NIT cluster (7 pre-existing site-count drifts in
  source comments now glaring after cycle 16:17 — under-counts that
  don't break operator grep but the apply.rs:592 + 636 split numbers
  hallucinate a non-existent 3rd collection-slot site).
- **+1** — `baa262c1`'s deferred-backlog header structurally honest
  (all 7 commit SHAs resolve; reviewer-report and design-loop file
  refs resolve; cycle 15:47 paragraph and refactor-safe filter both
  unchanged correctly).
- **−1** — `baa262c1` deferred-backlog header itself echoes the
  site-count drift ("F1 family now 7 sites" vs. "8th F1 family site"
  in same paragraph). Same paragraph contradicts itself.
- **−0.5** — three r7→r10 NIT carry-overs (auth/{bootstrap,keys,
  session}.rs preambles) still don't cite the hardening cargo gate
  (4-round carry).
- **−0.5** — EIGHT-round-old `docs/reference/plugin-system.md` Crate
  structure tree carries forward.
- **−0** — db.md `.strictness("lenient")` validation_refused
  operator-grep contract MINOR carries from r9 (already counted
  against r9's score; doesn't double-count).

**To break 93 next round:**

1. **One-pass site-count sweep**: rewrite all 8 stale claims (apply.rs
   84-88 + 188 + 563 + 577 + 592 + 636; migrations.rs:648 + 650) to
   uniform "8 sites: 6 audit_id + 2 collection" with the correct file
   list. Closes the entire NEW NIT cluster + apply.rs:592/636 split
   error. One-file-each edit. **+1.5**.
2. **Fix the deferred-backlog header self-contradiction**: change
   "F1 family now 7 sites" → "now 8 sites" in line 5. One edit. **+0.5**.
3. **Land the three NIT preamble cross-refs** for `auth/{bootstrap,
   keys,session}.rs`. Three-file edit; closes the 4-round carry. **+0.5**.
4. **Close the EIGHT-round `plugin-system.md` Crate structure tree.**
   Replace six stale paths with the actual layout. **+0.5**.
5. **Add the db.md `validation_refused` operator-grep paragraph**
   (r9 carry). **+0.5**.

If items 1+2 land, the score breaks 93 next round. Items 3+4+5 push
toward 94. The site-count cluster is the single highest-value sweep
and the easiest to mechanise (search-and-replace on three strings).
