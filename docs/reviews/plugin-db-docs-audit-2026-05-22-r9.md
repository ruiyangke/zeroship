# plugin-db docs audit — round 9 (2026-05-22)

**Scope:** `crates/plugin-db/src/` preambles + docstrings + inline WHY
comments. Re-anchored on r8 (91/100).

**Cycle 15:17 commits verified:**

- `14d7608f` plugin-db/validate: terminalize Pending rows (F2 — initial
  "Failed + validation_refused marker" partial fix, +50 LOC)
- `6afab751` plugin-db/audit + validate + integration: F2 upgrade to
  dedicated `ValidationRefused` terminal (migration-pipeline r13)
- `44ec83db` docs/reviews: cycle 15:17 reviewer reports + design-loop
  rounds 1-2 critiques + deferred-backlog F2 closure

**TL;DR.** F2 supersedes cleanly. The 14d7608f docstrings ("Failed +
validation_refused marker") were fully replaced — no stale references
to the partial pattern survive in source (only one honest retrospective
mention in audit.rs:126 explaining the prior shape). The 6afab751
preambles (audit.rs `InitialStatus`/`TerminalStatus` doc-comments at
lines 119-176, validate.rs module preamble lines 1-33, validate.rs
inline at lines 68-81) accurately describe the SQL emitted by
`ensure_audit_table_exists` and `write_audit_row`, and are
self-consistent across both files + the integration-test comment at
tests/integration.rs:1114-1133. One pre-existing docstring drift surfaces
this round (`update_audit_status` at audit.rs:368-371 still lists
"running -> applied | failed" — was already stale for
`AppliedWithDeadLetter`/`Cancelled` in earlier rounds, now also stale
for `ValidationRefused`); flagged as NEW NIT. The deferred-backlog header
correctly reflects F2 closure. `docs/reference/db.md` does not document
the operator-facing `validation_refused` terminal state for
strictness=lenient — NEW MINOR (one-paragraph addition would close it).
Three r7→r8 auth-preamble NITs and the SEVEN-round-old plugin-system.md
tree drift carry forward unchanged.

---

## Dimension 1 — `6afab751` audit.rs `InitialStatus::ValidationRefused`

```
[OK] crates/plugin-db/src/audit.rs:119-147 — preamble + variant docstring accurate
  Why: the enum-level doc-comment at 119-128 names the asymmetry honestly
  ("`ValidationRefused` is a terminal status that may ALSO appear on INSERT")
  and cross-references `ensure_audit_table_exists` for the CHECK-constraint
  extension. The per-variant doc at 133-135 names the orphan-Pending closure
  ("Lands terminal at INSERT so there is no orphan-Pending window for
  operators to chase") and pins the symmetry to `TerminalStatus::
  ValidationRefused`.
  Cross-verified `as_sql()` at line 144 returns `"validation_refused"` —
  matches the SQL string in the CHECK constraint at lines 240 and 285,
  the integration-test assertion at integration.rs:1129, and the SDK's
  documented `err.code === "validation_refused"` branch (validate.rs:31).
```

## Dimension 2 — `6afab751` audit.rs `TerminalStatus::ValidationRefused`

```
[OK] crates/plugin-db/src/audit.rs:149-176 — variant docstring symmetric + accurate
  Why: lines 156-163 carry the four key facts: (1) the proposal anchor
  ("proposal A2 strict mode"), (2) the operator-facing distinction
  ("'platform refused this DDL' vs. 'DDL ran and failed'"), (3) the
  operator-grep contract ("status = 'validation_refused' without parsing
  error_message"), (4) the symmetry with `InitialStatus::ValidationRefused`.
  `as_sql()` at line 173 returns `"validation_refused"` — same string both
  variants share. The `assert_eq!`s at the test in lines 938 + 943 lock
  the symmetric mapping.
```

## Dimension 3 — `6afab751` `ensure_audit_table_exists` SQL + WHY comment

```
[OK] crates/plugin-db/src/audit.rs:263-290 — comment matches emitted SQL verbatim
  Why: the WHY paragraph at 263-274 walks all three branches honestly:
  (a) freshly created table (DROP+ADD is no-op rewrite — same constraint
  name was emitted by `CREATE TABLE` above), (b) old table from before this
  commit (DROP succeeds, ADD installs wider list), (c) really old table
  without the named constraint (`IF EXISTS` swallows the miss, ADD installs).
  Cross-checked against the CREATE TABLE constraint definition at lines
  239-241: the constraint name `__zeroship_migrations_status_chk` matches
  the DROP at 277 + the ADD at 284 byte-for-byte; the status list at line
  240 (`'pending','running','applied','applied_with_dead_letter','failed',
  'cancelled','rolled_back','validation_refused'`) matches the ADD at line
  285 verbatim. The post-state is identical regardless of which branch ran,
  as the comment claims.
  Comment also honestly names the F2 anchor ("F2 (r13)") so the line
  archaeology is traceable.
```

## Dimension 4 — `6afab751` validate.rs preamble + inline F2 comment

```
[OK] crates/plugin-db/src/orchestrator/register_model/validate.rs:1-33
  Why: the preamble rewrite replaces the prior "destructive ops produce a
  validation_refused envelope" with explicit per-strictness behaviour:
  - strict (line 6-9): "INSERTed terminal as validation_refused so operators
    can see what was refused (no orphan-Pending row)"
  - lenient (line 10-12): "INSERTed terminal as validation_refused and
    silently dropped from the apply set"
  - off (line 12-13): "fall through into apply (test/CI mode — proposal A2
    line 122)"
  The "Why this stage stays on Result<_, String>" section (17-33) carries
  through unchanged, still correctly anchored on the SDK wire contract.

[OK] crates/plugin-db/src/orchestrator/register_model/validate.rs:68-81
  Why: the inline F2 comment names the upgrade anchor ("F2 resolution
  upgrade (migration-pipeline r13)"), the closed shape ("`Pending` then
  drove it to `Failed` via a second UPDATE"), the failure mode it closed
  ("stranded in `pending` if the worker crashed or the UPDATE failed
  between the two writes"), and the cycle linkage to the prior partial
  commit ("the prior commit `14d7608f` warned about this on the
  second-write error path"). Then names the new shape: "INSERT with
  `status = 'validation_refused'`. The audit table's status CHECK now
  accepts `'validation_refused'`". Cross-references `ensure_audit_table_
  exists` for the CHECK extension.
  Verified at line 90: the AuditRow's `status` field is set to
  `crate::audit::InitialStatus::ValidationRefused` — matches the WHY
  comment's claim verbatim. `write_audit_row` at audit.rs:336-341 passes
  `row.status.as_sql()` through to the `status` column, so the INSERT
  lands status='validation_refused' as documented.
```

## Dimension 5 — Cross-consistency across audit.rs ↔ validate.rs ↔ integration.rs

```
[OK] Three sites tell the same story:
  - audit.rs:159 ("Operators can grep on `status = 'validation_refused'`
    without parsing `error_message`")
  - validate.rs:77-78 ("INSERT with `status = 'validation_refused'` …
    audit table's status CHECK now accepts `'validation_refused'`")
  - integration.rs:1114-1117 ("migration-pipeline r13: INSERT-direct
    terminal — no orphan-Pending window. Distinguishes 'platform refused
    this DDL' from 'DDL ran and failed' without parsing `error`")
  All three use the same noun phrases ("INSERT-direct", "orphan-Pending",
  "validation_refused") so an operator reading any one site sees the
  others as natural cross-references.

[OK] The validate.rs preamble's "strict" / "lenient" split (lines 6-12)
  matches the integration-test assertion at integration.rs:1129
  (`assert_eq!(st, "validation_refused", "refused destructive ops land
  in validation_refused terminal")`). The integration-test comment update
  at 1114-1117 retracted the prior wording ("refused destructive ops
  stay pending for operator review") — the retraction is clean, the new
  wording self-consistent with the production docstrings.
```

## Dimension 6 — Stale-pattern hunt: "Failed + marker" residue

```
[OK] Zero stale "Failed + validation_refused marker" references in source.
  Verification: rg 'Failed \+ validation_refused|Failed\+marker|
  validation_refused marker|marker pattern|error_message marker'
  crates/plugin-db/src → 1 hit (audit.rs:126 "the previous Failed+marker
  pattern carried" — honest past-tense retrospective explaining why the
  enum is asymmetric; this is correct documentation, NOT residue).
  The 14d7608f-shape docstrings were fully overwritten by 6afab751.
```

## Dimension 7 — error.rs preamble + ValidationRefused path

```
[OK] crates/plugin-db/src/error.rs:57 + 80-88 — DbError::SchemaRefused
  description still correct.
  Why: lines 57 + 80-88 describe `SchemaRefused` as the variant the
  `validation_refused` envelope rides through; nothing changed about
  that path (the envelope is still built by validate.rs and wrapped
  at the run_pipeline boundary in mod.rs:206). The audit-row-write
  side is a separate concern — the SchemaRefused envelope is what
  reaches the SDK; the audit row's `status='validation_refused'`
  terminal is what operators see in the migrations table. error.rs
  correctly documents only the envelope/wire side.
  No drift introduced by 6afab751; the ValidationRefused enum is an
  internal audit-table concern that doesn't surface through DbError
  (audit-write failures bubble through `coded_sql` → `DbError::Coded`
  unchanged).
```

## Dimension 8 — `docs/reference/db.md` operator-facing coverage

```
[NEW MINOR] docs/reference/db.md does NOT document the validation_refused
  terminal state for strictness=lenient operators.
  Why: db.md:202-203 documents `.strictness("strict" | "lenient" | "off")`
  but says only "deploy-time data-validation policy" — no mention that:
  - strict deploys surface validation_refused as the wire error
  - lenient deploys silently drop destructive ops BUT write
    `status='validation_refused'` to `__zeroship_migrations` so
    operators can audit what got skipped
  - `__zeroship_migrations.status = 'validation_refused'` is the
    operator-grep contract
  The migration-pipeline r13 review explicitly named this as an operator
  surface ("Operators can grep on `status = 'validation_refused'`"), but
  db.md never exposes the audit-table contract — operators learning
  through the SDK docs will not discover the terminal state without
  reading the source. One paragraph in the strictness section would
  close it.
  Net cost: small (operators tracking deploy outcomes will hit the
  control-plane API first; only those debugging stuck rows would look
  at `__zeroship_migrations` directly), but the surface-area asymmetry
  vs. the source-side docs is real.
```

## Dimension 9 — `update_audit_status` docstring drift

```
[NEW NIT] crates/plugin-db/src/audit.rs:368-371 — allowed-transitions
  list is stale by two rounds.
  Why: the docstring says "allowed transitions mirror the proposal A3
  state machine: `running -> applied | failed`" — but `TerminalStatus`
  has FIVE variants (Applied, AppliedWithDeadLetter, Failed, Cancelled,
  ValidationRefused) and the WHERE clause at line 390 accepts both
  `running` and `pending` as source states. This was already out-of-date
  for `AppliedWithDeadLetter`/`Cancelled` in earlier rounds; 6afab751
  added `ValidationRefused` to the enum without updating this docstring.
  Closure: one sentence — "Accepts any `TerminalStatus` variant; source
  row must be in `running` or `pending`." 
```

## Dimension 10 — Deferred backlog header reflects F2 closure

```
[OK] docs/reviews/plugin-db-deferred.md header rewrite at 44ec83db is
  honest about the two-step landing.
  Why: the cycle 15:17 paragraph names BOTH commits explicitly
  ("F2 resolved via two commits — 14d7608f (initial Failed+marker
  pattern) then 6afab751 (upgrade to dedicated ValidationRefused
  terminal per migration-pipeline r13's recommendation)"), credits r13
  for catching the semantic mismatch, and pulls forward the
  refactor-safe filter rule from cycle 14:47. The "Open backlog" line
  in the commit body ("12 IMPORTANT, was 13; F2 closed") is consistent
  with the deferred-doc state.

[OK] [C3] honesty correction is acknowledged in the same paragraph.
  Why: "performance r13's honest read — the Rust-side residual (4.24 µs
  at 50-col) IS real, but the V8 `JSON.parse` half … isn't measured by
  either bench. [C3] is actionable for design work, not yet for sizing
  the win. r14 forcing function: V8-side bench measuring
  `serde_json::Value` → V8 boundary." This is the right shape — the
  user's standing "never estimate without a measurement" instruction
  is upheld by deferring the win-size claim until the V8 half is
  benched.
```

## Dimension 11 — Module preambles + state-machine completeness

```
[OK] No state-machine preamble forgot to add ValidationRefused.
  Verification: the only inline state-machine diagram in audit.rs lives
  at lines 414-423, scoped specifically to the **B1 backfill helpers**
  (`migrations.start` / fetchBatch / cancel / commit). ValidationRefused
  is a `phase='ddl'` concern from the validate stage — it never enters
  the backfill subtree, so the diagram correctly omits it. The
  top-of-file audit.rs preamble (1-41) describes the audit table
  generally and does not enumerate statuses; no drift.

[OK] `register_model/mod.rs:14-17` preamble's strictness summary
  ("Lenient deploys log + skip; off proceeds") is still consistent.
  Lenient now ALSO writes the validation_refused audit row before
  skipping; the preamble's "log + skip" wording covers this naturally
  (the audit row IS the log). No drift.
```

## Dimension 12 — Carry-overs as one-liners

```
[NIT — UNCHANGED from r7/r8] auth/bootstrap.rs:1-9 — preamble could cite the hardening cargo gate (one sentence).
[NIT — UNCHANGED from r7/r8] auth/keys.rs:1-9 — preamble could cite the hardening cargo gate.
[NIT — UNCHANGED from r7/r8] auth/session.rs:1-19 — preamble could cite the hardening cargo gate.
[NIT — UNCHANGED from r3/r4/r5/r6/r7/r8] docs/reference/plugin-system.md:315-344 — stale "Crate structure" tree (six paths that don't exist); SEVEN rounds carry-over.
[NEW NIT — round 9] audit.rs:368-371 — `update_audit_status` docstring lists only 2 of 5 TerminalStatus variants ("running -> applied | failed"); ValidationRefused + AppliedWithDeadLetter + Cancelled missing. One-sentence rewrite would close.
[NEW MINOR — round 9] docs/reference/db.md `.strictness("lenient")` description does not mention the `__zeroship_migrations.status = 'validation_refused'` operator-grep contract. One paragraph in the strictness section would close.
[NIT — round 9] validate.rs:95-97 "Best-effort: a failure to write the audit row should not mask the envelope" applies literally only to strict mode (lenient has no envelope to mask). Comment is carried verbatim from the strict-only shape pre-F2; would benefit from "(in strict mode; lenient still surfaces nothing)" clarification.
```

---

## Score: 92 / 100  (r8: 91;  +1)

**Delta breakdown (+1 from r8):**

- **+3** — `6afab751` ships exemplary cross-referenced F2 docs: audit.rs's
  `InitialStatus::ValidationRefused`/`TerminalStatus::ValidationRefused`
  enum docstrings explicitly name the symmetry; the
  `ensure_audit_table_exists` WHY paragraph walks all three CHECK-
  rewrite branches (fresh, old, really-old) honestly; the validate.rs
  inline F2 comment names the prior shape + the failure mode + the
  upgrade anchor + the cycle linkage to 14d7608f. All three sites are
  self-consistent and the integration-test comment at 1114-1117 carries
  the same noun phrases — operators see the same story from any entry
  point.
- **+1** — `14d7608f`→`6afab751` supersession is clean. Zero stale
  "Failed + marker" references in source (only one honest
  retrospective at audit.rs:126). No half-state where two docstrings
  disagree.
- **+1** — `44ec83db` deferred-backlog header is honest about the
  two-step landing (names BOTH commits, credits r13 for the
  catch) and honest about the [C3] half-measurement (refuses to size
  the V8-side win without `bench_v8_json_parse`).
- **−1** — NEW NIT (`update_audit_status` docstring at audit.rs:368-371
  lists 2 of 5 TerminalStatus variants). Latent through prior rounds
  but the ValidationRefused addition made the omission more glaring.
- **−1** — NEW MINOR (db.md does not document the validation_refused
  operator-grep contract for strictness=lenient). Source-side docs
  are complete; the reference-doc side trails.
- **−0.5** — three NIT carry-overs from r7/r8 (auth/{bootstrap,keys,
  session}.rs preambles) still don't cross-reference the cargo gate.
- **−0.5** — SEVEN-round-old `docs/reference/plugin-system.md` stale
  Crate structure tree carries forward.
- **−0** — validate.rs:95-97 "Best-effort … envelope" comment is a
  minor NIT (literally accurate for strict, inert for lenient); not
  counted against the score because the user-facing impact is zero.

**To break 94 next round:**

1. **Rewrite the `update_audit_status` docstring** to list all 5
   TerminalStatus variants + both source states (`running`/`pending`).
   One-sentence fix; closes the NEW NIT. +1.
2. **Add a paragraph to `docs/reference/db.md`'s `.strictness()`
   section** documenting the `__zeroship_migrations.status =
   'validation_refused'` operator-grep contract for strict + lenient.
   One paragraph; closes the NEW MINOR. +1.
3. **Land the three NIT preamble cross-refs** for
   `auth/{bootstrap,keys,session}.rs`. Three-file edit; closes the
   last of the r7→r8→r9 carry-overs. +0.5.
4. **Close the SEVEN-round `plugin-system.md` Crate structure tree.**
   Replace six stale paths with the actual layout. +0.5.

If items 1+2 land, the score breaks 94 next round. Items 3+4 are
the +1 ceiling move toward 95.
