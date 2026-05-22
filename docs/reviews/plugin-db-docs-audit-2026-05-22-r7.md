# plugin-db docs audit — round 7 (2026-05-22)

**Scope:** `crates/plugin-db/src/` preambles + docstrings on public APIs +
inline comments that explain WHY. Re-anchored on r6 (83/100).

**Brief commits verified:**

- `403b3891` plugin-db/query: `validate_field_name` rejects non-ASCII (I12)
- `4cab871a` plugin-db: 3 doc/visibility cleanups from cycle 11:17 reviewers
- `ae5570dc` plugin-db/exec: unit tests for queue_or_emit / drain / clear (I13)
- `2fa9472e` plugin-db/auth: gate dormant auth subtree behind `hardening` feature

**TL;DR.** Three of the four cycle 11:17 commits land their docs cleanly.
`2fa9472e`'s feature-gating, however, is incompletely documented: the
new Cargo feature gate is referenced from `error.rs` §2 (per the cleanup
commit) but the two upstream-most preambles that should advertise it
(`auth/mod.rs` and `lib.rs::Module visibility note`) do not mention it.
A reader landing on `auth/mod.rs` sees a "Backwards compatibility — opt-in
`--harden` flag" section that pre-dates the cargo feature and now
misdescribes how the subtree is reached. NEW IMPORTANT × 2.

The r6 hold-out list got hammered down hard in the cycles between r6 and
r7 (commits `3d79d2da`, `bed655c1`, `09e32998`, `757026e3`, `9e392ba1`,
`389749ca`, `7d0bc4c5`, `bc4363f0` visible in the log, not in this brief).
Concretely closed: the `migrations.rs:67-86` four-round CRITICAL; the
`replication.rs:751` test docstring reframed as a wire-shape regression
guard; `orchestrator/mod.rs:22` "three of four submodules are `pub`"; the
`wal_consumer.rs:49` `replicationConsumerStart` rename; the `error.rs:357-373`
`prefix_message` preamble picks up `migrations::coded_db` as the seventh
consumer; `audit.rs:5` "(future)" parenthetical gone; `query.rs:443`
TODO(A1) replaced with a present-tense paragraph; the TX_CONN / TX_TOKEN /
MIG_LOCK drift sweep ran (`09e32998`) and the residual count is 12 across
4 files, all in correctly-framed historical context.

Two new docs are exemplary:

- `validate_field_name`'s docstring (`403b3891`) names the specific
  collision class (Postgres 63-byte truncation), the cross-policy
  (`validate_collection`), and the SQL-injection ortho-defense
  (`quote_ident`). The accompanying tests cite the same rationale.
- The `exec.rs` I13 test-module header (`ae5570dc`) explicitly
  enumerates which of four branches are covered + names the deferred
  branch + cross-refs the integration-test fallback
  (`gap_b_subscriber_does_not_observe_pre_commit_state`).

---

## Dimension 1 — `validate_field_name` docstring + tests (403b3891)

```
[OK] crates/plugin-db/src/query.rs:101-110 — docstring matches the new ASCII allowlist
  Why: lines 105-110 explicitly describe the policy ("ASCII allowlist matches
  [`validate_collection`]'s policy") + the collision class ("a multi-byte
  identifier like `\"café\"` is 4 chars / 5 bytes, and two distinct unicode-
  spelled fields could collide on the same Postgres-truncated column if either
  side approached the 63-byte ceiling"). Body at 127-134 matches verbatim.

[OK] crates/plugin-db/src/query.rs:4280-4293 — `validate_field_name_rejects_non_ascii` cites the same rationale
  Why: test docstring (lines 4280-4283) carries "closes [I12] / test-coverage GAP-1 /
  security MINOR. A multi-byte identifier could collide with another after Postgres'
  63-byte truncation; ASCII-only matches `validate_collection`." Same collision
  class, same cross-policy reference.

[OK] crates/plugin-db/src/query.rs:4295-4305 — positive test pins the contract
  Why: `validate_field_name_accepts_ascii_allowlist` enumerates `id`, `user_id`,
  `createdAt`, `v2`, `_private` — covers underscore-prefix, camelCase, alphanumeric.
```

## Dimension 2 — Cycle 11:17 doc/visibility cleanups (4cab871a)

### (a) `replication.rs::slot_status` now `pub(crate)` with corrected docstring

```
[OK] crates/plugin-db/src/replication.rs:606-613 — docstring + visibility match
  Why: signature reads `pub(crate) async fn slot_status(...)`. Docstring at
  lines 606-612 honestly states "No production caller today (api-surface r9
  NEW-R9-1 noted the prior 'V8 `replicationStatus` callback' docstring was
  aspirational); kept `pub(crate)` so a future `replicationStatus` v8_class
  method can adopt it without surface churn." The "V8 callback" claim that
  was a lie is now explicitly retracted in-line.
  Cross-check: grep "fn slot_status" finds only the definition; zero call
  sites in production code.
```

### (b) `context.rs::return_mig_client` docstring cites `tracing::error!`

```
[OK] crates/plugin-db/src/context.rs:399-405 — docstring matches the actual logging primitive
  Why: lines 403-404 read "paired with the `tracing::error!` on
  `set_mig_lock`'s shadow-replace branch above" — matches the body at
  context.rs:375-381 which is a `tracing::error!{ ... }` block. No
  `debug_assert` reference remains on the mig_lock slot. The remaining
  `debug_assert!` hits in context.rs (lines 294, 325, 527, 530, 698, 712)
  are all on the tx_token / tx_conn slots — intentional and the docstring
  was never about them.
```

### (c) `error.rs` §2 preamble notes the `hardening` cfg-gate

```
[OK] crates/plugin-db/src/error.rs:20-27 — §2 preamble updated
  Why: lines 22-27 add "The whole `auth/*` subtree is cfg-gated behind
  `--features hardening` post-`2fa9472e`, so these holdouts are invisible
  in default builds — but the hold-out class still applies inside the
  SECURITY DEFINER bootstrap flow when the feature is on." Accurate
  (cross-check: `lib.rs:68-71` cfg-gates the auth module on
  `feature = "hardening"`).
```

## Dimension 3 — exec.rs I13 test-module header (ae5570dc)

```
[OK] crates/plugin-db/src/exec.rs:453-470 — header enumerates covered vs deferred branches
  Why: the four-branch state machine is named explicitly ("queue_or_emit in
  autocommit / drain_pending_emits_on_commit / clear_pending_emits / queue_or_emit
  in-tx with a real `compio_postgres::Client`"). The header (lines 456-461) cites
  the upstream constraint ("the slot can only hold a real `compio_postgres::Client`,
  which the context-module test docs explicitly note is not constructible outside
  `compio-postgres`") and the integration-test fallback
  (`gap_b_subscriber_does_not_observe_pre_commit_state` in tests/integration.rs).
  Each test's docstring matches the labelled branch:
    - line 472-475: "In autocommit mode (`has_tx() == false`)..."
    - line 506-510: "drain_pending_emits_on_commit must publish every event..."
    - line 555-557: "clear_pending_emits must drop the queue WITHOUT publishing..."

[OK] crates/plugin-db/src/exec.rs:546-551 + 581-583 — second-drain / post-clear no-op assertions documented inline
  Why: lines 548-549 note "Drain a second time → nothing left (queue is consumed,
  not copied)." The assertion at 551 matches. Same pattern at 581-583 for the
  post-clear no-op invariant.
```

## Dimension 4 — `2fa9472e` hardening feature gate — preamble coverage

```
[NEW IMPORTANT] crates/plugin-db/src/auth/mod.rs:56-62 — "Backwards compatibility" misdescribes the gate
  Why: The section reads:
  > "The hardened path is **opt-in** per the proposal's gradual-migration
  > guidance. `replication::ensure_publication_and_slot` and the rest of
  > P8a/P8b continue to function unchanged when bootstrap has not been
  > run; callers that pass `--harden` (or invoke
  > `bootstrap::ensure_admin_schema` themselves) get the hardened path."
  Two drifts:
    1. The `--harden` CLI flag is described as the opt-in mechanism, but
       since `2fa9472e` the entire subtree is unreachable in default
       builds — there's nothing to opt INTO at runtime; it's a compile-
       time switch via `--features hardening` now.
    2. "P8a/P8b continue to function unchanged when bootstrap has not been
       run" remains accurate for runtime behaviour, but the reader is
       left to discover that the bootstrap function isn't even compiled
       in by default by hitting a "no such function" error or by reading
       the cfg-gate lines in `lib.rs`.
  Fix: Replace with:
  > "Compiled-out by default. The whole `auth/*` subtree is gated behind
  > the `hardening` Cargo feature (added at `2fa9472e`, cycle 10:47).
  > Default builds link without it and `replication::ensure_publication_and_slot`
  > + the rest of P8a/P8b continue to function unchanged. The control
  > plane's eventual wire-up flips `--features hardening` on (per the
  > auth-r1 design); integration tests already require both `test-helpers`
  > and `hardening` per `Cargo.toml` [[test]] config."
  Verification: grep -rn "hardening\|--harden" crates/plugin-db/src/auth/

[NEW IMPORTANT] crates/plugin-db/src/lib.rs:34-46 — Module visibility note omits the new cfg-fork
  Why: The block explains the `test-helpers` cfg-fork only:
  > "Most modules are `pub(crate)` in normal builds. Several are also
  > consumed by external test crates under `tests/`..."
  But `auth` now has a THREE-arm cfg ladder:
    - `#[cfg(all(feature = "hardening", not(feature = "test-helpers")))] pub(crate) mod auth;`
    - `#[cfg(all(feature = "hardening", feature = "test-helpers"))] pub mod auth;`
    - (default — not compiled in at all)
  A reader scrolling the mod-declaration block sees `auth` with a feature
  gate the visibility note hasn't mentioned. The note should append:
  "`auth/*` is additionally gated behind `--features hardening` (since
  `2fa9472e`) — see auth/mod.rs for the per-subtree rationale."
  Verification: sed -n '34,72p' crates/plugin-db/src/lib.rs

[OK] crates/plugin-db/src/error.rs:20-27 — §2 preamble correctly cross-references the gate (Dimension 2c).

[OK] crates/plugin-db/Cargo.toml:42-55 — Cargo.toml feature stanza is self-documenting
  Why: the `hardening = []` line carries the full rationale ("Architect r7
  I5 + code-critique r9 MAJOR-R9-5: zero production callers today; enables
  58 dead-code warnings in default builds. The eventual control-plane
  wire-up (per the auth-r1 design) flips this feature on"). Matches the
  commit body verbatim.
```

## Dimension 5 — r6 hold-outs since cycle 11:17

```
[CLOSED — was r6 CRITICAL/WORSENED] crates/plugin-db/src/migrations.rs:67-86 — `coded_db` doc block
  Why: commit `3d79d2da` (in the log, not in this brief) rewrote the doc block.
  Lines 67-93 now carry ONE coherent paragraph; the duplicate opening sentence
  + the non-existent `coded_sql` reference + the "classify the Postgres error"
  intro are all gone. History bullet at lines 75-79 cross-references cbbc9059
  and deeefe18.

[CLOSED — was r6 IMPORTANT] crates/plugin-db/src/orchestrator/mod.rs:22 — pub/pub(crate) mismatch
  Why: line 22 now reads "Three of the four submodules (`auto_tx`, `register_model`,
  `transaction`) are `pub`... `lock_guard` is `pub(crate)`...". Matches mod
  declarations at 29-32.

[CLOSED — was r6 IMPORTANT] crates/plugin-db/src/replication.rs:745-769 — test docstring reframed
  Why: commit `bed655c1` reframed the test as a "Wire-shape regression guard"
  (lines 751-759). The `Result<_, String>` claim is gone; the docstring now
  correctly describes what the test pins (`replication:` prefix preservation).

[CLOSED — was r6 IMPORTANT] crates/plugin-db/src/wal_consumer.rs:47-54 — `replicationConsumerStart` rename
  Why: lines 49-50 now read "apps call `db.startReplicationConsumer()` (the
  `#[v8_method]` on the `Db` v8_class — see
  `v8_classes/db.rs::start_replication_consumer`)". Matches the live `#[v8_name]`
  at `v8_classes/db.rs:242`.

[CLOSED — was r6 IMPORTANT] crates/plugin-db/src/error.rs:357-373 — `prefix_message` preamble
  Why: enumeration at 360-363 now includes "`migrations::coded_db` (deeefe18 —
  last inline copy collapsed)" as the seventh consumer. Structured-variant
  carve-out at 367-373 also explicit.

[CLOSED — was r6 MINOR] crates/plugin-db/src/audit.rs:5 — "(future) backfill"
  Why: line 5 now reads "The A1 `create_index_with_recovery` retry path that
  used to log via `tracing::warn!` now writes structured audit rows here."
  Zero "future" hits in the file.

[CLOSED — was r6 CRITICAL] crates/plugin-db/src/query.rs:443 — TODO(A1) composite indexes
  Why: replaced by a present-tense paragraph at lines 451-457: "Composite indexes
  (the proposal's `schema(...).index(name, fields)` builder) are wired separately
  via [`build_named_indexes`] — callers merge that `Vec` with this function's
  output at `bootstrap.rs`."

[CLOSED — was r6 CRITICAL/IMPORTANT] crates/plugin-db/src/v8_classes/transaction.rs — TX_CONN / TX_TOKEN
  Why: commit `09e32998` swept the file. `git grep -cn "TX_CONN\|TX_TOKEN"`
  on the file post-sweep returns 0. The residual 12 occurrences across 4
  files are in correctly-framed historical context (e.g. context.rs:9,
  lib.rs:1, orchestrator/mod.rs:1, v8_classes/mod.rs:1 — preambles
  describing the now-folded thread-local predecessors).

[CLOSED] crud.rs:53, exec.rs:329, backend/mod.rs:66, lib.rs:110/216,
  v8_classes/migration.rs:216, orchestrator/transaction.rs
  Why: same sweep (09e32998). Spot-checked — no stale TX_CONN / TX_TOKEN /
  MIG_LOCK references remain in any of these sites.
```

## Dimension 6 — net-NEW drift since r6

```
[NEW IMPORTANT] crates/plugin-db/src/auth/mod.rs:56-62 — see Dimension 4
  The `--harden` CLI flag is presented as the opt-in mechanism; the new
  `--features hardening` Cargo gate (compile-time, not runtime) replaces
  it but the section was not updated by 2fa9472e or the follow-up 4cab871a.

[NEW IMPORTANT] crates/plugin-db/src/lib.rs:34-46 — see Dimension 4
  The Module visibility note explains only the `test-helpers` cfg-fork
  while `auth` now has a three-arm cfg ladder post-2fa9472e.

[NIT] crates/plugin-db/src/auth/{bootstrap,keys,session}.rs:1-3 — preambles could cite the gate
  Why: a reader landing on these modules from auth/mod.rs:64-66 may not
  realise they're in a feature-gated subtree. One sentence each ("Compiled
  only when `--features hardening` is on; see `auth/mod.rs` for the gating
  rationale.") would close the loop. Optional.
```

## Dimension 7 — Module preambles still present

```
[OK] Every `crates/plugin-db/src/**/*.rs` file opens with a `//!` preamble.
  Verification: for f in crates/plugin-db/src/*.rs crates/plugin-db/src/{auth,backend,orchestrator,v8_classes}/*.rs crates/plugin-db/src/orchestrator/register_model/*.rs; do head -1 "$f" | grep -q "^//!" || echo "MISSING: $f"; done — zero hits
```

## Dimension 8 — Stale R1/R2/R3, semantic drift, magic numbers

```
[OK] No stale R1/R2/R3 references in src/ tree.
  Verification: grep -rn "R1\|R2\|R3" crates/plugin-db/src/ shows only live
  review-cycle annotations (MAJOR-R5/R6/R7/R8/R9, NEW-R9-*, api-surface-r9).

[OK] No "code claims X but does Y" semantic drift detected this round.
  The cycle 11:17 cleanup 4cab871a specifically closed the
  `return_mig_client` → `debug_assert` drift; that was the largest
  outstanding semantic-drift item.

[OK] No magic-number drift found.
  Spot-checked: NAMEDATALEN=63 cited in `validate_field_name` (matches
  Postgres source, NAMEDATALEN=64 minus terminator); 63-byte limit
  also cited in `validate_collection`. `DEFAULT_TOKEN_TTL_SECS = 300`
  + `NONCE_RETENTION_SECS = 25 * 3600` at auth/mod.rs:96/102 carry
  rationale; values match.
```

## Dimension 9 — Carry-overs as one-liners

```
[NEW IMPORTANT] auth/mod.rs:56-62 — "opt-in --harden" framing predates the cargo feature gate; should describe `--features hardening` instead.
[NEW IMPORTANT] lib.rs:34-46 — Module visibility note doesn't mention the new `hardening` cfg-fork.
[NIT] auth/bootstrap.rs:1-3, auth/keys.rs:1, auth/session.rs:1 — preambles could cite the cargo gate (one sentence each).
[NIT — UNCHANGED from r3/r4/r5/r6] docs/reference/plugin-system.md:315-344 — stale "Crate structure" tree (out-of-scope NOTE; AGENTS.md routes here, but the tree lists six paths that don't exist). FIVE rounds carry-over.
```

---

## Score: 87 / 100  (r6: 83;  +4)

**Delta breakdown (+4 from r6):**

- **+8** — r6 hold-out list hammered down hard between cycles. Closed:
  `migrations.rs:67-86` four-round CRITICAL (3d79d2da);
  `replication.rs:751` test docstring reframed (bed655c1);
  `orchestrator/mod.rs:22` pub/pub(crate) (3d79d2da);
  `wal_consumer.rs:49` `replicationConsumerStart` rename;
  `error.rs:357-373` `prefix_message` preamble picks up `migrations::coded_db`;
  `audit.rs:5` "(future)" parenthetical;
  `query.rs:443` TODO(A1);
  `v8_classes/transaction.rs` + 6 sibling sites TX_CONN/TX_TOKEN sweep (09e32998).
- **+2** — `403b3891` `validate_field_name` docstring + two tests carry
  the same rationale (collision class + cross-policy) cleanly.
- **+1** — `4cab871a` cleanup commit closes all three items it claimed
  (visibility + docstring + preamble) cleanly.
- **+1** — `ae5570dc` test-module header is exemplary: explicit branch-
  coverage enumeration + names the deferred branch + cross-refs the
  integration-test fallback.
- **−2** — NEW IMPORTANT × 2 introduced by `2fa9472e`, not picked up by
  the cleanup commit: `auth/mod.rs:56-62` describes a `--harden` runtime
  flag instead of `--features hardening`; `lib.rs:34-46` Module visibility
  note omits the new cfg-fork. Both one-paragraph edits.
- **−1** — `auth/{bootstrap,keys,session}.rs` preambles could each cite
  the new gate (NIT class; reader-experience).
- **−1** — five-round-old `docs/reference/plugin-system.md` stale Crate
  structure tree carries forward (out-of-scope NOTE).

**To break 90 next round:**

1. **Fix `auth/mod.rs:56-62`.** One-paragraph rewrite per Dimension 4
   suggested wording. Closes the larger of the two NEW IMPORTANTs.
2. **Fix `lib.rs:34-46`.** One sentence appended to the Module
   visibility note: "`auth/*` is additionally gated behind
   `--features hardening` (since `2fa9472e`) — see auth/mod.rs for the
   per-subtree rationale."
3. **Add the NIT cross-references to `auth/{bootstrap,keys,session}.rs`
   preambles.** One sentence each. Low effort, high reader-experience
   value (new contributors discover the gate before hitting a "no such
   function" compile error).
4. **Close the FIVE-round `plugin-system.md` stale Crate structure tree.**
   Out-of-scope for the crate itself but the entry-point experience for
   new contributors has been degrading for five rounds; cycle 12 should
   batch it with another doc sweep.

If items 1+2+3 land, the score breaks 91 next round.
