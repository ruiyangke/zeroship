# plugin-db System Design — Round 5 Critique

**Target**: `docs/proposals/db-system-design.md` (1354 lines, post round-4 revision).
**Date**: 2026-05-22.
**Prior rounds**: R1 62, R2 77, R3 85, R4 87.

## Scores

| Dimension | Score | Δ vs R4 | Rationale |
|---|---|---|---|
| Completeness | 92 | +3 | Backfill × CDC broker pause landed (§10.6, §11.6), schema-pending decoder defined (§16.7), per-app role × slot invariant pinned (§17.5), `db_pre_image_outbox_rows` gauge added (§16.3). Glossary grew by four entries that match new prose. Residual gaps: column-key rotation still out-of-scope (acknowledged); `subscription_app_dropped` SDK error code still not in §15.7 table (R4 IMPORTANT #3 — see below). |
| Correctness | 88 | +4 | Symmetric BEFORE-trigger choice resolves the R4 CRITICAL on intra-tx INSERT ordering by collapsing two row populations into one. `rowid` is reused as the trigger-fire-order witness, which under the SQLite writer lock is monotonic per-tx for an ordinary `INTEGER PRIMARY KEY` table — load-bearing assumption that the doc does NOT spell out (R5 IMPORTANT #1). New citation in §7.2 ("AWS DynamoDB Encryption Client deterministic mode") is questionable — see R5 IMPORTANT #2. |
| Extensibility | 88 | 0 | No regressions. Symmetric-trigger choice is more uniform than the asymmetric R4 mechanism — easier to port to libSQL or Turso replicated SQLite. |
| Operational | 92 | +2 | `db_pre_image_outbox_rows` + alerting threshold (>1000 sustained 30s) closes the R4 observability gap. Backfill pause = no million-event flood. Schema-pending decoder records dropped-event count. |
| Security | 88 | +4 | §17.5 makes the slot-ownership-stays-platform invariant explicit and unconditional — locks down the principal P6 (per-app role) → CDC interaction before P6 lands. The `EncryptedColumn` randomised-mode PK-as-AAD assumption (R4 MINOR #2) still unstated; `validate_collection` SDK rejection rule for "encrypted column under DB-generated PK" still absent. |
| Developer Experience | 88 | +1 | Schema-pending decoder behaviour (drain-then-swap + resync per active subscription) gives SDK consumers a story for "events I never saw during deploy." But: doc still lacks an explicit statement of what new subscribers entering during a schema-pending window see — see R5 IMPORTANT #3. |
| Industry Alignment | 89 | +1 | Drop-then-swap during schema-pending follows the Debezium "snapshot pause + resync" pattern. `(commit_id, rowid → lsn_seq)` matches Debezium intra-tx ordering. AES-SIV-pattern rename is more honest than R4's "AES-GCM-SIV pattern", though the AWS DDB citation in the renamed body text is itself suspect (R5 IMPORTANT #2). |
| **Overall** | **89** | **+2** | Round-4 CRITICAL closed. Five new MINORs and three IMPORTANTs surfaced, none of which block convergence. **One IMPORTANT short of clean.** |

**Convergence check.** Score ≥ 90: no (89). CRITICAL count: 0. IMPORTANT count: 3. **Not converged-early; one more pass needed.**

---

## Round 4 CRITICAL closure — verified

- **R4 CRITICAL #1 (intra-tx INSERT vs UPDATE/DELETE order).** §11.5 lines 691–710 replace the R4 split mechanism with **symmetric BEFORE INSERT/UPDATE/DELETE triggers**, all three writing to the same `__zeroship_pre_image` outbox. The drain no longer merges two row populations: it walks the outbox in `rowid` order (= trigger-fire order under SQLite's per-database writer lock = SDK call order), stamps `lsn_seq` from the seq counter, builds events, COMMITs. One mechanism, one allocator. The justification paragraph "Why symmetric" (lines 702–710) explicitly retires the R4 asymmetric design. Closed.

## Round 4 IMPORTANT closures — verified

- **R4 IMPORTANT #1 (AES-GCM-SIV misnaming).** §7.2 lines 395–405 rename to "AES-SIV-pattern (RFC 5297-flavoured) deterministic AEAD" and explicitly disclaim RFC 8452 ("not AES-GCM-SIV; calling it AES-GCM-SIV would mislead an auditor expecting POLYVAL"). The audit anchor is now correct. **Partial regression**: replacing the bad name with a real-system citation ("the construction the AWS DynamoDB Encryption Client applies in deterministic mode") is itself unverifiable — see R5 IMPORTANT #2.
- **R4 IMPORTANT #2 (`_seq` durability).** §11.5 lines 723–734 pin the consumer as "the drain, to project rowid ordering into the published stream" and explicitly state durability across restart is NOT required (truncate at boot clears the outbox; `_seq` row persists only "to avoid re-creating it on every boot"). Closed.
- **R4 IMPORTANT #3 (`subscription_app_dropped` not in `.code`).** Glossary mentions the term but I do NOT see it in the §15.7 `.code` table. R5 finding promoted — see R5 IMPORTANT #3.
- **R4 IMPORTANT #4 (schema-pending decoder undefined).** §16.7 lines 1057–1078 add the "Schema-pending decoder, defined" subsection: drain-then-swap, drops decoded events, advances the consumer cursor (so WAL does not accumulate against an inactive slot), records `db_schema_pending_dropped_events`, emits one synthetic `resync` per active subscription at disengage. Closed at the conceptual level; **subscriber-side semantics during the window are under-specified** — see R5 IMPORTANT #3.
- **R4 IMPORTANT #5 (backfill × CDC).** §10.6 lines 611–623 add a broker-pause/resume in the backfill orchestrator, sharing the §11.6 `suppress_app` rail with `register_model`. Glossary entry "backfill broker pause" matches. Closed.

---

## Round 5 findings

### CRITICAL

None.

### IMPORTANT

1. **§11.5 — `__zeroship_pre_image` storage definition does not assert `rowid` semantics, on which the whole ordering proof depends.** Quote (line 695): *"Each writes one row to `__zeroship_pre_image`: op kind, pk, relation id, UNIX-ms ts, tuple JSON ..."* and (lines 706–707): *"the outbox rowid (monotonic under the writer lock) is the per-tx event sequence."* SQLite's `rowid` monotonicity claim holds **only** when (a) the table is a normal `rowid` table (NOT `WITHOUT ROWID`), and (b) the table either uses an unaliased rowid or `INTEGER PRIMARY KEY AUTOINCREMENT` to forbid rowid reuse from gaps left by deletes. The `__zeroship_pre_image` schema is not stated; the GC section (lines 736–741) DELETEs rows after the drain consumes them, which on a non-AUTOINCREMENT rowid table can re-use a freed rowid for the next INSERT — within the same tx this is fine (rowids inside one tx are assigned in INSERT order), but the doc proves correctness with a property (monotonic rowid as serialisation oracle) that's only true under specific table flags. **Spec must commit**: `__zeroship_pre_image` is `(<columns>) WITHOUT ROWID` is wrong because then no rowid; `(... rowid INTEGER PRIMARY KEY AUTOINCREMENT)` is needed to guarantee monotonicity even after the GC DELETE. Without this, an implementer using `INTEGER PRIMARY KEY` (no AUTOINCREMENT) would still PASS the gate test (single tx, no GC interleave), but a long-running session with backfill resuming after a partial drain could observe rowid reuse and end up with an `lsn_seq` stamping ambiguity. Add one sentence to §11.5: "`__zeroship_pre_image` is a normal-rowid table with `INTEGER PRIMARY KEY AUTOINCREMENT` to forbid rowid reuse across the per-tx GC delete." Or assert the equivalent invariant.

2. **§7.2 — replacement citation ("AWS DynamoDB Encryption Client applies in deterministic mode") is itself questionable.** Quote (lines 399–405): *"This is the construction the AWS DynamoDB Encryption Client applies in deterministic mode (and CipherStash uses for `equatable` mode). It is **not** RFC 8452 AES-GCM-SIV (which uses POLYVAL keyed by a per-nonce subkey, not HMAC); calling it AES-GCM-SIV would mislead an auditor."* The disclaimer is correct. The substitute citation is not verifiable. AWS Database Encryption SDK documentation (the current name for the DDB Encryption Client family) lists AES-GCM as the AEAD and HKDF for key derivation, NOT a HMAC-truncate-then-AES-GCM construction as a deterministic mode; deterministic search in the *current* AWS DB Encryption SDK is via "beacons" (truncated HMAC of plaintext indexed alongside the ciphertext), which is **a different mechanism** (the index value is the HMAC truncation; the ciphertext remains random-IV AES-GCM). The doc has swapped one misleading name (RFC 8452) for another misleading citation (DDB Encryption Client deterministic mode = HMAC-derived nonce + AES-GCM). Both fail an auditor reading primary sources. The construction the doc actually describes — synthetic-IV deterministic AEAD via HMAC-derived nonce — does have a name: "Deterministic Authenticated Encryption with synthetic IV via PRF" (Rogaway & Shrimpton 2006, *"Deterministic Authenticated-Encryption"*, generalising SIV). Either cite that, or drop the comparison and stand on the construction's own security argument (HMAC-SHA256 is a PRF; AES-GCM under PRF-derived nonce is collision-bounded by 2^64 in a single column). **Action**: replace the AWS DDB citation with either the Rogaway-Shrimpton paper or no citation. Round-4 reviser fixed a name and introduced a misattribution.

3. **§16.7 — schema-pending decoder behaviour for subscribers that ENTER the window is unspecified.** Quote (lines 1061–1069): *"While active the shim (a) lets the underlying consumer/drain advance its cursor [...], (b) drops every decoded ChangeEvent without invoking the broker [...]. On next request the isolate reloads, installSchema attaches the new LiveSchema, shim disengages, broker emits one synthetic `resync` per active subscription at disengage."* The "per active subscription at disengage" wording covers subscribers that existed **before** the window opened. It does not say what happens to a new subscriber that arrives **during** the schema-pending window: (i) `subscribe(...)` returns an error referencing `schema_pending`; (ii) subscription registers and immediately receives a `resync` once the shim disengages; (iii) registration blocks until reload. The contract is load-bearing: AI-builder app code that retries on terminal errors needs option (i) or (ii) to avoid an unbounded await. PG side has the same question (the slot survives, the worker's broker has the shim) and the same silence. §11.6's `suppress_app` rail handles the analogous DDL window with policy "subscribers registered during the window see only post-DDL events" (line 759); §16.7 should mirror that or commit to a different rule. Add one sentence.

### MINOR

1. **§7.2 — column-key rotation deferral text contradicts the round-5 line in the cover paragraph.** The cover paragraph (line 12) says round 5 closes "column-key rotation deferral", but the body §7.2 lines 420–429 say rotation is "out of scope in current scope" and "deferred to §19 P6." This is fine as deferral; the cover paragraph wording overstates closure ("deferral" closed by re-affirming the deferral). Either rephrase the cover paragraph to "column-key rotation explicit-deferral statement" or drop the bullet. Minor surface inconsistency.

2. **§4 table row 1 (Reactive queries) overstates "rowid = per-tx event sequence".** Quote: *"symmetric BEFORE INSERT/UPDATE/DELETE triggers + per-app outbox (rowid = per-tx event sequence); drain just before COMMIT (§11.5)"*. As caught in R5 IMPORTANT #1, `rowid` is the per-tx sequence only under specific table flags. The §4 table is the load-bearing one-line summary read by the dashboard team; the asterisk applies. Add `(rowid → lsn_seq projection; outbox is AUTOINCREMENT)` or footnote.

3. **§11.5 lines 706–710 "Why symmetric" justification leaks an obsolete-mechanism reference.** Quote: *"Round 4 routed INSERT through drain-side `RETURNING`-materialisation while UPDATE/DELETE went through triggers — the drain then had to merge two populations of unknown relative order."* Self-referential justification (referring to a prior revision) is informative for reviewers but adds nothing for the implementer. In a system design doc the prior-revision narrative should live in the round-N critique files, not in the spec body. Move to a footnote or drop after final convergence.

4. **§17.5 — last sentence "§19 P6 inherits slot-ownership-stays-platform as a non-negotiable" — verify §19 P6 actually reflects this.** Cross-anchor check: §19 P6 backlog list does NOT include a line item "P6 work MUST NOT grant `REPLICATION` to per-app roles." Either add a P6 row in §19 ("per-app PG role × slot-owner: assert invariant; no grant of `REPLICATION`") or drop the §17.5 cross-reference. Otherwise the invariant lives only in §17.5 and a future P6 author may not see it. Same critique-pattern as R4 MINOR #3 (the `pg_advisory_lock(bigint)` deferral that referenced §19 P6 without a row).

5. **§11.5 lines 712–721 "stamping range onto outbox rows in rowid order" reuses an aliased `RETURNING` clause that doesn't exist for `__zeroship_pre_image_seq` updates.** Quote: *"(1) UPDATE __zeroship_pre_image_seq SET v = v + N RETURNING v claims a contiguous range of size N = outbox row count"*. SQLite 3.35+ supports `RETURNING` on `UPDATE`, but the return value is the **post-update** value — so `RETURNING v` returns the new top of the range, not the range start. To get the range `[v0+1, v0+N]`, the UPDATE must return `v` (post-update top) and the caller computes start = top - N + 1. The doc's wording reads ambiguously — an implementer following the text literally could read `v` as start. Tighten the wording: "RETURNING v gives the range end; range = [v - N + 1, v]". Trivial fix, but cdc ordering correctness flows through it.

### Missing concepts (still not in the doc)

1. **Subscriber-side semantics during schema-pending window** — see R5 IMPORTANT #3.

2. **`db_schema_pending_dropped_events` alert threshold.** §16.3 lists the gauge as added (per worker, app) but does not give an alerting threshold. The metric exists for forensics; with no threshold, an operator cannot tell "this is normal during deploys" from "this is a degenerate run-away decoder." A simple "> 1000 over 5 min and `schema_pending = true` for that app" would give the rule. R5 missing concept rather than IMPORTANT because it's tunable post-launch.

3. **`__zeroship_pre_image` row count under sustained writer pressure.** §16.3 alert at >1000 sustained 30s catches stuck drains. But the steady-state guarantee (drain DELETE pre-COMMIT) means the rowcount is observable only at a window between BEFORE-trigger fire and the drain's step (4). If the writer never idles (a busy app), the sampler may always observe non-zero. The doc says (lines 1006–1008): *"sampled at writer idle; steady-state 0 after §11.5 step (4)"* — but a continuously-busy writer never idles. The sampling discipline needs to be event-driven (sample after each COMMIT, expect 0) not idle-driven. Subtle. Worth pinning before P2 ships.

4. **Encryption-key rotation as a missing concept — partially closed by acknowledged deferral, still no rotation rail design.** §7.2 lines 420–429 acknowledge rotation is out-of-scope; the doc does not state where the rotation rail design lives. R4 missing concept #5; R5 reviser acknowledged but did not pin the future home (which proposal doc / which §19 phase). One sentence: "Column-key rotation design lives at `docs/proposals/db-column-key-rotation.md` (to be authored); §19 P6 includes a stub row."

5. **PG-side schema-pending decoder for the in-flight pgoutput frames.** §16.7 specifies the decoder behaviour ("drops every decoded `ChangeEvent`"). On PG, the pgoutput stream arrives as binary frames the worker's wal_consumer decodes against the current `LiveSchema`. If the schema bump renames a column or drops one, the decoder may fail to parse a frame against the old schema — not just produce an event the broker would reject. The doc states the shim "drops" events; it does not state the recovery path if the decoder itself errors (typed-column → relation OID mismatch). Probably: the shim wraps the decoder and converts decode-errors-during-schema-pending to dropped events with the same `db_schema_pending_dropped_events` counter. Pin this; otherwise a deploy with a non-trivial column rename can panic the wal_consumer.

---

## Cross-anchor integrity check

- Round 5 markers present at lines 8–12 (cover), 99 (§4 row 1), 254–257 (§6 system tables), 395 (§7.2), 611, 691, 760, 1005, 1057, 1119, 1350–1354 (glossary). Eleven markers; all paired with the change they introduce. Section numbering remains stable.
- Glossary (§20) gained four entries — `schema-pending decoder`, `backfill broker pause`, `AES-SIV-pattern deterministic AEAD`, `slot-ownership-stays-platform`, `db_pre_image_outbox_rows` — all anchored to live sections.
- §15.7 `.code` table — I did not see `subscription_app_dropped` added (R4 IMPORTANT #3 closure I cannot verify from grep). Re-check; if absent, promote to a round-5 IMPORTANT for round-6.
- §19 P6 backlog: §17.5's "slot-ownership-stays-platform as non-negotiable in P6" assertion is not reflected in §19 P6's task list (R5 MINOR #4).
- Cover paragraph mentions "column-key rotation deferral" as a round-5 closure; body §7.2 only acknowledges the deferral without designing the rotation rail (R5 MINOR #1).

---

## Convergence verdict

Round-5 score 89 (+2 vs R4's 87). 0 CRITICAL, 3 IMPORTANT, 5 MINOR, 5 missing concepts. Round-4 CRITICAL stayed closed; four of five R4 IMPORTANTs stayed closed; R4 IMPORTANT #3 (`subscription_app_dropped` in `.code` taxonomy) cannot be verified from the doc as currently grepped.

**Termination rule per the round-5 mandate**:
- Score ≥ 90 + 0 CRITICAL + 0 IMPORTANT → END (converged-early). **Not met** (89, three IMPORTANTs).
- 0 CRITICAL + 0 IMPORTANT but score < 90 → END (clean for at least 3 rounds; we've had 0 clean rounds so far). **Not met** (three IMPORTANTs).
- Otherwise → **CONTINUE to round 6.**

**Verdict: CONTINUE.** One CRITICAL absent, but three IMPORTANTs and two cross-anchor MINORs require a round-6 reviser pass. The IMPORTANTs are mechanical (one sentence each in §11.5, §7.2, §16.7 + glossary cross-checks); a round 6 will close them and a round 7 critique should be clean. Strong forward motion (R3 85 → R4 87 → R5 89). The doc is one revision pass from convergence — recommend the user mandate a round 6 specifically targeted at the three IMPORTANTs (rowid table-flags assertion; AWS DDB citation rework or removal; new-subscriber-during-schema-pending policy) and the §19 P6 backlog row cross-references.

If the user's "5 rounds" was a hard floor (now satisfied) but the convergence rule is the binding stop criterion, then the convergence rule says CONTINUE. If the user's "5 rounds" was a hard ceiling, then the loop terminates HERE at 89/100 with the three IMPORTANTs as known-unclosed punch list. **Defer to user on which reading binds.**
