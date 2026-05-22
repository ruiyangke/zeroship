# plugin-db System Design — Round 7 Critique

**Target**: `docs/proposals/db-system-design.md` (1449 lines, post round-6 revision).
**Date**: 2026-05-22.
**Prior rounds**: R1 62, R2 77, R3 85, R4 87, R5 89.
**Mandate**: convergence check (≥90 + 0 CRITICAL + 0 IMPORTANT → END).

## Scores

| Dimension | Score | Δ vs R6 | Rationale |
|---|---|---|---|
| Completeness | 93 | +1 | R5 missing concepts all addressed in round 6: §16.3 gauge sampling-after-COMMIT, §16.7 PG-side decoder error path, §19 P6 row for column-key rotation + slot-owner invariant + alert threshold, and `subscription_app_dropped` + `schema_pending` both listed in §15.7. Residual hole: §6 has no §6.5 subsection but §11.5 (line 735) cross-anchors to it — see Critical below. |
| Correctness | 87 | −1 | The AUTOINCREMENT/`sqlite_sequence` claim is accurate (post-DELETE rowid reuse is forbidden; counter survives boot). The Rogaway-Shrimpton 2006 citation is correctly attributed at the conceptual level. **But** §16.7 contains a placement contradiction for the schema-pending shim — "in front of the `ConsumerHandle`" (line 1118) vs. "wraps the decoder" (line 1147) describe two different layers and the doc never resolves which is canonical. The decode-error-as-drop story only works if the shim wraps the decoder; correctness of the §17.6-watchdog-doesn't-loop claim depends on that placement. |
| Extensibility | 88 | 0 | No regressions. New §16.7 PG-side language ties the shim tightly to pgoutput decoder error types but does not invalidate future backends. |
| Operational | 92 | 0 | Sample-after-COMMIT gauge discipline + alerting threshold for `db_schema_pending_dropped_events` are correctly pinned. Steady-state expectation (zero across consecutive post-COMMIT samples) is now stated, distinguishing wedged drain from busy writer. |
| Security | 88 | 0 | §17.5 / §19 P6 cross-anchor is now bidirectional. The Rogaway-Shrimpton citation is conservative and audit-credible; the AWS DB Encryption SDK beacon comparison correctly notes "different mechanism." Randomised-mode PK-as-AAD-on-DB-generated-PK regression (R5 leftover) still unaddressed but R6 wasn't mandated to fix it. |
| Developer Experience | 90 | +2 | `subscribe()` returning `Conflict { schema_pending }` synchronously + SDK retry budget (50ms × 2^n, cap 2s, terminal 30s) is a clean contract. AI-generated apps now have a bounded retry rail. Subscriber-during-window policy diverges from §11.6's DDL rule, but the doc justifies the asymmetry on expected window duration. |
| Industry Alignment | 90 | +1 | Rogaway-Shrimpton attribution is the correct prior-art lineage for synthetic-IV deterministic AEAD; the doc correctly disclaims RFC 5297 / RFC 8452 / AWS DB Encryption SDK beacons. The schema-pending drain-then-swap pattern continues to mirror Debezium. AUTOINCREMENT-as-monotonic-event-sequence is the same technique used by SQLite-WAL-CDC implementations (e.g., Litestream-style audit tables). |
| **Overall** | **89.7** | **+0.7** | One CRITICAL (broken §6.5 cross-anchor) + one IMPORTANT (shim placement ambiguity). Neither blocks a round-7 implementation start, but both block the strict "0 IMPORTANT" rule. |

**Convergence check.** Score ≥ 90: marginal (89.7, rounds to 90 only with a generous rounding rule). CRITICAL count: 1. IMPORTANT count: 1. **Not converged.**

---

## Round 6 closures — verified

### R5 IMPORTANT closures

- **R5 IMPORTANT #1 (rowid table-flags assertion).** §11.5 lines 724–739 add the explicit "Outbox table flags (load-bearing)" subsection: `INTEGER PRIMARY KEY AUTOINCREMENT`, normal-rowid (not `WITHOUT ROWID`), justified against the per-tx GC DELETE + reuse hazard. The SQLite semantics claim — "`AUTOINCREMENT` enforces strict monotonicity via `sqlite_sequence`" and "startup truncate ... `sqlite_sequence` continues forward across boots" — is accurate per the SQLite documentation: `AUTOINCREMENT` causes the engine to consult `sqlite_sequence` and never reuse rowids, surviving `DELETE FROM <table>` (only `DELETE FROM sqlite_sequence WHERE name='...'` or `DROP TABLE` would reset). The "no semantic dependency on its value" line correctly notes that the in-process `last_seen_lsn_seq` starts at 0 each boot, so the persistent counter is solely there to enforce monotonicity, not for cross-restart correlation. **Closed.**

- **R5 IMPORTANT #2 (AWS DDB citation rework).** §7.2 lines 401–427 replace the AWS DDB attribution with Rogaway-Shrimpton 2006 ("Deterministic Authenticated-Encryption"). The paper is correctly titled. The doc characterises it as generalising deterministic AEAD as "synthetic-IV-via-PRF" — accurate at the conceptual level; RS06 introduces both the DAE security definition AND the SIV construction (with S2V/CMAC as the PRF). The doc explicitly disclaims being the literal RFC 5297 SIV ("which uses S2V — a CMAC-based PRF — over AES-CTR, not AES-GCM"), correctly disclaims RFC 8452, and correctly notes that the AWS Database Encryption SDK uses beacons (separate HMAC-truncate column over randomised AES-GCM, **not** the doc's construction). The construction-stands-on-its-own security argument ("HMAC-SHA256 is a PRF, AES-GCM under PRF-derived nonce is collision-bounded by 2^64 in a single column; collision implies plaintext repeat") is correct and audit-credible. **Closed.** Minor expository nit: lines 408–411 say the construction "instantiates Rogaway-Shrimpton with HMAC-SHA256-as-PRF + AES-GCM-as-AEAD" — a literalist auditor would point out RS06's SIV construction is more specific than "synthetic-IV-via-PRF," but the doc covers this by explicitly disclaiming the literal SIV. Not a finding; informational.

- **R5 IMPORTANT #3 (new-subscriber-during-schema-pending).** §16.7 lines 1130–1141 add "New subscribers during the window": `subscribe(...)` returns `Conflict { code: "schema_pending" }` synchronously; SDK retries with bounded exponential backoff (50ms × 2^n, cap 2s, terminal at 30s). The asymmetry vs. §11.6's "register during window, see only post-DDL events" is justified (typical window <100ms for reload vs. minutes for `register_model`). `subscriptions_active` does not count rejected attempts — correctly notes the §17.7 interaction. **Closed.**

### R5 MINOR closures

- **R5 MINOR #1 (column-key rotation cover-paragraph wording).** Cover paragraph (line 12) now reads "re-affirmed column-key rotation as deferred to §19 P6" — accurate; no closure overstatement. **Closed.**
- **R5 MINOR #2 (§4 row-1 rowid asterisk).** Line 103: "outbox is `INTEGER PRIMARY KEY AUTOINCREMENT`; rowid → `lsn_seq` projection is the per-tx event sequence." **Closed.**
- **R5 MINOR #3 (obsolete-mechanism narrative in §11.5).** Lines 747–751 move the round-4 asymmetric-design retirement narrative to a footnote-style aside referencing `docs/reviews/db-system-design-critique-round-5.md`. **Closed.**
- **R5 MINOR #4 (§19 P6 row for slot-owner invariant).** §19 P6 lines 1390–1396 add a row referencing §17.5 with explicit language ("per-app role MUST NOT be granted the `REPLICATION` attribute, MUST NOT have `pg_create_logical_replication_slot` execute permission, and slot ownership stays platform-side — this is the non-negotiable §17.5 invariant tracked here so a future P6 author sees it"). **Closed.**
- **R5 MINOR #5 (`RETURNING v` disambiguation).** §11.5 lines 755–760 add: "SQLite's `RETURNING` clause on `UPDATE` returns the **post-update** column value, so the single returned `v` is the range **end** (highest `lsn_seq` in the range); the range is `[v - N + 1, v]` and the caller computes the start." Stamping ordering ("lowest rowid receiving `v - N + 1`") is now unambiguous. **Closed.**

### R5 Missing-concept closures

- **MC #1 (subscriber-side semantics).** Closed via R5 IMPORTANT #3 closure.
- **MC #2 (`db_schema_pending_dropped_events` alert threshold).** §16.3 lines 1071–1077 add "`> 1000 over 5 min while `schema_pending = true` for that app → warn (wedged schema-pending decoder; normal deploy bursts clear within the §16.7 reload window)." §19 P6 also lists "alert threshold (round-6 — `> 1000 over 5 min while schema_pending = true`)." **Closed.**
- **MC #3 (busy-writer outbox-rowcount sampling discipline).** §16.3 lines 1057–1064 add "sampled after each COMMIT in the writer actor — not idle-driven, since a continuously-busy writer never idles; the post-COMMIT sample expects 0 because the drain's step (4) deleted the consumed rows pre-COMMIT and ROLLBACK paths invalidate the outbox rows entirely; sustained non-zero across consecutive COMMIT samples = stuck drain or unpaused backfill." Glossary line 1449 mirrors this. **Closed.**
- **MC #4 (column-key rotation future-home).** §19 P6 lines 1396–1400: "column-encryption key rotation (re-encryption pass under broker pause, §7.2 deferral; design lives at `docs/proposals/db-column-key-rotation.md`, to be authored)." **Closed.**
- **MC #5 (PG-side schema-pending decoder error path).** §16.7 lines 1143–1155 add the "PG-side decoder error path during the window" subsection: shim wraps decoder, `DecodeError` raised while engaged is converted to dropped event tagged `schema_pending_drop`, counted against the gauge, not propagated to `wal_consumer::run_supervised`'s panic path. **Conceptually closed** — but see Round 7 IMPORTANT #1 below on the placement contradiction.

---

## Round 7 findings

### CRITICAL

1. **§11.5 references a non-existent §6.5.** Quote (line 735): *"Cross-references §6.5 (system-tables roster) and §10 (audit row state machine ordering)."* §6 contains only §6.1 (Collections), §6.2 (Columns / types), and §6.2.1 (Decimal). There is no §6.5. The "System tables per app" paragraph at line 256 is the intended target, but it is an unnumbered paragraph in §6, not §6.5. A reader following the round-6 cross-anchor lands nowhere. Two valid fixes: (a) promote the "System tables per app" paragraph to "### 6.5 System tables per app", or (b) change the line-735 reference to "§6 'System tables per app' paragraph." This is a regression introduced by round 6's cross-anchor effort — the same effort that correctly anchored §17.5 ↔ §19 P6 mis-anchored §11.5 ↔ §6.5. Cross-reference correctness is load-bearing for an AI-readable design doc; broken anchors degrade tool-driven navigation.

### IMPORTANT

1. **§16.7 schema-pending shim placement is internally inconsistent.** Two statements conflict:
   - Line 1118: *"Thin shim **in front of** the worker's `ChangeStream::ConsumerHandle`, engaged from `bundle_invalidated` until the next bundle reload for that app."*
   - Line 1147: *"The shim **wraps the decoder**; any `DecodeError` (column-not-found, type-mismatch, OID resolves to a since-renamed column) raised while engaged is converted to a dropped event."*

   "In front of `ConsumerHandle`" places the shim downstream of decoding (between the consumer's output and the broker — it would see `ChangeEvent`s, not raw pgoutput frames). "Wraps the decoder" places it upstream of decoding (intercepting pgoutput byte frames before they reach the decoder). These are two different shim placements and the decode-error-handling story only works in the upstream placement: if the shim is downstream, a `DecodeError` fires inside `wal_consumer::run_supervised` BEFORE the shim sees anything, and the doc's claim that the watchdog "would otherwise reconnect into the same stale-schema loop" (line 1153) is exactly the failure mode the upstream placement is supposed to prevent. The contradiction is small in line count but load-bearing: an implementer following the line-1118 placement would build a downstream shim and the line-1143 paragraph would not actually solve the column-rename-during-deploy crash. **Action required**: pick one placement and rewrite both paragraphs consistently. The upstream placement (shim wraps decoder) is what the round-6 reviser appears to have intended, so line 1118 needs the rewording.

### MINOR

1. **§7.2 line 408 — "instantiates Rogaway-Shrimpton" is a small overclaim.** RS06's primary contribution is the **DAE security definition** plus the specific **SIV construction** (S2V + AES-CTR). The "synthetic-IV-via-PRF" generalisation is present as a definitional frame, but "instantiates Rogaway-Shrimpton" elides the gap between the abstract paradigm and the concrete SIV construction. Tighter wording: "follows the synthetic-IV-via-PRF paradigm introduced in Rogaway-Shrimpton 2006 (DAE), with HMAC-SHA256 as PRF and AES-GCM as the AEAD primitive — not the SIV/S2V construction RS06 defines." The current paragraph is close enough that an auditor reading the security argument lands in the right place; tightening would push it from "credible" to "tight."

2. **§16.7 vs §11.6 policy asymmetry — joint-window case unspecified.** §11.6 says subscribers registering during a `register_model` window "see only post-DDL events" (silent acceptance); §16.7 says subscribers registering during a schema-pending window get `Conflict { schema_pending }` (loud rejection). When both windows are active simultaneously on the same worker (control plane fires `register_model` AND `bundle_invalidated` in rapid sequence during deploy), which policy wins? Doc does not say. The asymmetry-justification text (lines 1136–1141) covers the rationale for divergence but not the joint case. Easy fix: one sentence — "When both windows are engaged for the same app, `schema_pending` takes precedence (loud rejection); the SDK's bounded-retry budget handles the typical deploy span."

3. **§11.5 line 763 — `(commit_id, lsn_seq)` ordering — what allocates `commit_id`?** The doc says (line 765): *"`commit_id` is a per-tx monotonic stamped at drain entry, matching pgoutput 'all-at-commit-LSN' on PG."* In-process counter? Persistent? If in-process, restart resets to 0 (matching `last_seen_lsn_seq`); if persistent, where stored? The round-4 → round-5 evolution clarified `lsn_seq` durability but `commit_id` is referenced as if it were equally well-defined. One sentence on `commit_id` allocation would close this.

4. **AUTOINCREMENT 64-bit ceiling not noted.** §11.5 lines 724–739 establish AUTOINCREMENT as load-bearing without noting that the `sqlite_sequence` counter caps at 2^63 - 1; INSERT after exhaustion fails with `SQLITE_FULL`. Practically irrelevant (10^9 events/sec sustained for 292 years), but a "non-issue at scale" line would forestall a future reviewer noticing the gap.

5. **§19 P6 backlog row count is growing; phase scope creep.** Round-6 added: column-key rotation row, alert-threshold row, single-`bigint` advisory-lock tunable, slot-ownership-stays-platform invariant, dashboards, drop-namespace sequencing test, plus the original F1/F2/RLS/REPLICATION items. P6 is now ~10 distinct work items. Consider splitting into P6a (correctness-hardening: F1/F2/per-app role) and P6b (operational hardening: dashboards/alerts/rotation/advisory-lock-64-bit) so the gate test list is achievable in one phase.

### Missing concepts (still not in the doc)

1. **`commit_id` allocation rule** — see MINOR #3 above; load-bearing for the §11.5 ordering proof but specified only implicitly.

2. **Concurrent `register_model` + `bundle_invalidated` joint-window policy** — see MINOR #2; the deploy sequence naturally produces both, the policy table for subscribe(...) under joint engagement is not specified.

3. **PG decoder error path: what happens if the shim is NOT engaged (i.e., between worker boot and first `bundle_invalidated`) and the WAL consumer races a DDL that arrived from another worker's `register_model`?** §16.7 (line 1154) says: *"Outside the window a `DecodeError` remains fatal."* The implicit assumption is the worker always knows about a DDL via `bundle_invalidated` before the resulting pgoutput frame arrives. This is bounded by the durable control-plane queue but not guaranteed at byte-level ordering. If a frame arrives 50ms before its `bundle_invalidated` event, the decoder panics and the watchdog reconnects — same loop the shim is designed to prevent. A "tolerant-decoder" mode (drop on DecodeError, log, count, never panic the consumer) would be a more conservative posture; the doc's choice to panic outside the window is defensible but the deploy-ordering assumption deserves a sentence.

---

## Cross-anchor integrity check

- §17.5 ↔ §19 P6: bidirectional, language matches. **Pass.**
- §11.5 ↔ §6.5: §6.5 does NOT exist. **Fail** — Round 7 CRITICAL above.
- §11.5 ↔ §10 (audit row state machine): the round-6 line *"and §10 (audit row state machine ordering)"* anchors to a section that does have audit row state language (§10.3). Anchor valid; not a finding.
- Glossary §20 ↔ body: `Deterministic-IV-via-HMAC AEAD` (line 1446), `schema_pending` (line 1448) both have body anchors. **Pass.**
- §16.3 alert table ↔ §19 P6 alert-threshold row: language matches at "> 1000 over 5 min while `schema_pending = true`". **Pass.**
- §13.5 ↔ §11.5 allow-list reference ("install allow-list (load-bearing — see §13.5)"): §13.5 exists and discusses the MV-as-broker-invisible-cache contract; cross-anchor valid. **Pass.**
- Round 6 markers present at lines 14–16 (cover), 103 (§4 row 1), 260–261 (§6 system tables), 402, 724, 747, 760, 1054, 1116, 1393, 1400, 1446, 1448 (glossary). Twelve markers; all paired with the change they introduce except the §6.5 cross-anchor at line 735 which dangles.

---

## Convergence verdict

Round-7 score 89.7 (+0.7 vs R6 89.0). 1 CRITICAL (broken §6.5 cross-anchor), 1 IMPORTANT (schema-pending shim placement contradiction), 5 MINOR, 3 missing concepts. Round-5 CRITICAL stayed closed; all R5 IMPORTANTs closed; all five R5 missing concepts closed; five R5 MINORs closed. The R6 reviser made strong forward motion on the three R5 IMPORTANTs (mechanical fixes per the IMPORTANT-1, -2, -3 items) but introduced two new defects: one dangling cross-reference (CRITICAL by the doc's own cross-anchor discipline) and one internally inconsistent placement claim in the new §16.7 PG-side error-path paragraph (IMPORTANT — load-bearing for the watchdog-loop-prevention claim).

**Termination rule per round-7 mandate:**
- Score ≥ 90 + 0 CRITICAL + 0 IMPORTANT → END (converged). **Not met** (89.7, 1 CRITICAL, 1 IMPORTANT).
- Otherwise → CONTINUE.

**Verdict: CONTINUE — 1 round estimated to convergence.**

Both round-7 findings are mechanical, one-paragraph fixes:
1. CRITICAL: promote the "System tables per app" paragraph in §6 to "### 6.5 System tables per app" (or change the §11.5 line-735 cross-reference to "the system-tables-per-app paragraph in §6"). One-line change either way.
2. IMPORTANT: pick the upstream placement for the schema-pending shim and rewrite the line-1118 description from "in front of the `ConsumerHandle`" to "wrapping the worker's pgoutput decoder, sitting upstream of `ChangeStream::ConsumerHandle`." Reconcile with the §17.6 watchdog-loop-prevention claim.

The five MINORs and three missing concepts can carry into a final-polish pass alongside the round-8 reviser's CRITICAL/IMPORTANT closure; none block the round-8 convergence check.

Score progression R1 62 → R2 77 → R3 85 → R4 87 → R5 89 → R6 89 → R7 89.7. The doc has plateaued in the 89-range for two consecutive rounds; the round-8 reviser must close the two structural defects above to cross 90, after which a round-9 critique should be clean.

**Recommendation**: mandate round 8 specifically targeted at:
- §6.5 promotion or §11.5 line-735 rephrase (CRITICAL #1),
- §16.7 shim-placement reconciliation (IMPORTANT #1),
- and one expository pass over the five MINORs (esp. `commit_id` allocation rule and joint-window policy, both load-bearing).
