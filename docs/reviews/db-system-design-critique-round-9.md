# plugin-db System Design — Round 9 Critique (Convergence Check)

**Target**: `docs/proposals/db-system-design.md` (1513 lines, post round-8 revision).
**Date**: 2026-05-22.
**Prior rounds**: R1 62 → R2 77 → R3 85 → R4 87 → R5 89 → R6 89 → R7 89.7.
**Mandate**: convergence check (≥90 + 0 CRITICAL + 0 IMPORTANT → END).

---

## Scores

| Dimension | Score | Δ vs R7 | Rationale |
|---|---|---|---|
| Completeness | 94 | +1 | §6.5 now a real subsection (line 257); §11.5 cross-anchor resolves; commit_id allocation site pinned in both backends; joint-window precedence policy specified; disengage resync semantics + pre-`bundle_invalidated` race both added as named subsections. AUTOINCREMENT 2^63 ceiling acknowledged. The five R7 MINORs and three R7 missing concepts are addressed. The doc still does not state how `Broker::suppress_app` propagates across the worker fleet (control event? routing-pull?) — see MINOR #2 below. |
| Correctness | 90 | +3 | The R7 CRITICAL is closed (§6.5 exists as a real subsection at line 257, properly anchored from §11.5 line 743). The R7 IMPORTANT shim-placement contradiction is structurally resolved: every site now says "upstream" / "wrapping the decoder" and `grep "in front of"` is clean. The math on the 2^63 / 10^9 ev/s / 292-year ceiling is correct. RS06 attribution is now appropriately narrow ("follows RS06 §3.2's SIV-via-PRF paradigm" rather than "instantiates"). Two residual correctness wobbles, neither rising to IMPORTANT: (a) the §7.2 ConsumerHandle abstraction places the decoder *inside* what `spawn_consumer` returns; "upstream of ConsumerHandle, wrapping the pgoutput decoder" is plausible but mildly underspecified about whether the shim composes around the decoder or sits between decoder and ConsumerHandle's output sink (MINOR #1 below); (b) §11.5 line 779 still says "`commit_id` is a per-tx monotonic stamped at drain entry" before the divergent-allocator paragraph, reading as if SQLite semantics applied to both backends (MINOR #3 below). |
| Extensibility | 89 | +1 | P6 split into P6a (correctness) + P6b (operational) reduces phase-scope-creep risk that R7 MINOR #5 flagged. Both ship together at day-1 readiness so the split is structural, not chronological. The §6.5 anchor now formalises the system-tables roster for future-backend authors; a new backend in P-far-future has one canonical list of internal tables to honour. |
| Operational | 92 | 0 | Sample-after-COMMIT gauge discipline, `db_schema_pending_dropped_events` alert threshold, joint-window precedence, and disengage-resync upper-bound (`subscriptions_active` per-app) are all pinned with the right counter semantics. The pre-`bundle_invalidated` race paragraph correctly accepts "one watchdog reconnect per deploy per worker" as a quantum rather than a loop. Residual gap: §11.6 still does not say whether `Broker::suppress_app` is per-worker (and how it propagates) — MINOR #2 below. Not blocking. |
| Security | 89 | +1 | RS06 attribution tightening is the right audit posture — "instantiates" was loose, "follows RS06 §3.2's SIV-via-PRF paradigm" is tight and disclaimable. PG-side decoder error path under schema-pending preserves the §17.5 slot-ownership invariant (no schema-tolerant decoder mode that could mask a real schema-corruption signal outside deploys). Per-app PG role × `REPLICATION` separation re-anchored in §19 P6a. |
| Developer Experience | 91 | +1 | Joint-window precedence resolves the deploy-with-DDL subscribe(...) ambiguity an AI-generated app would otherwise hit; `schema_pending` retry budget (50ms × 2^n, cap 2s, terminal 30s) is documented and matches typical reload windows. P6a/P6b split clarifies phase gates for implementers. Glossary mirror of the round-8 changes is consistent with body language. |
| Industry Alignment | 90 | 0 | RS06 §3.2-paradigm attribution + explicit disclaimers (RFC 5297, RFC 8452, AWS DB Encryption SDK beacons) match a security-audit-credible bibliography. The pre-`bundle_invalidated` race "one reconnect per deploy per worker" matches Debezium's `wal2json`-style at-least-once decoder reconnect quantum. The "shim wraps decoder, drops on DecodeError while engaged, fatal outside the window" pattern matches Kafka Connect's `errors.tolerance=none|all` posture (the doc picks `none` outside windows and `all` inside, which is conventional). |
| **Overall** | **90.7** | **+1.0** | 0 CRITICAL, 0 IMPORTANT, 4 MINOR. Crosses the convergence threshold. |

**Convergence check**: Score ≥ 90 (90.7), CRITICAL = 0, IMPORTANT = 0.

---

## Round 8 closures — verified

### R7 CRITICAL #1 (§11.5 → §6.5 dangling cross-reference)

§6.5 now exists as a numbered subsection at line 257 ("### 6.5 System tables per app") with substantive content: roster of `__zeroship_migrations`, `__zeroship_pre_image` (with the load-bearing `INTEGER PRIMARY KEY AUTOINCREMENT` flag re-stated for the cross-anchor reader), `__zeroship_pre_image_seq`, `__zeroship_audit_<collection>`, `__zeroship_mv_<name>` (with the §13.5 trigger-install exclusion call-out), and `__zeroship_admin.*`. The closing sentence "This roster anchors §11.5, §10, §13.5, §17.5" is the cross-anchor breadcrumb. §11.5 line 743 now reads "Cross-references §6.5 (system-tables roster) and §10 (audit row state machine ordering)" and lands on a real anchor. **Closed.**

### R7 IMPORTANT #1 (shim placement contradiction)

`grep "in front of"` returns no matches in the doc. The §16.7 paragraph (line 1144) now reads: "Thin shim **upstream** of the worker's `ChangeStream::ConsumerHandle`, **wrapping the pgoutput decoder** (PG) / outbox-drain projector (SQLite), so it sees raw frames before they reach `ConsumerHandle`'s event sink." The decode-error-handling story (line 1186) is now consistent with the placement: "The shim wraps the decoder; any `DecodeError` ... raised while engaged is converted to a dropped event tagged `schema_pending_drop`." The "downstream shim ... would see only successfully decoded ChangeEvents" sentence at line 1149 is the contrastive rejection, not a placement claim — correct rhetorical use. **Structurally closed.** (Residual underspecification — does the shim compose around the decoder or sit between decoder output and ConsumerHandle's sink? — is MINOR #1 below, not a regression of the round-7 IMPORTANT.)

### R7 MINOR #1 (RS06 attribution narrowing)

§7.2 lines 414–420 add the round-8 narrowing: "The doc's construction borrows only the SIV-via-PRF **paradigm** described in RS06 §3.2 (derive the IV by a PRF over the plaintext, then encrypt under that IV), with HMAC-SHA256 as PRF and AES-GCM as the AEAD primitive. 'Follows RS06 §3.2's SIV-via-PRF paradigm' is the precise attribution; 'instantiates Rogaway-Shrimpton' would overclaim, since RS06's concrete SIV construction is S2V + AES-CTR, neither of which appears here." This is a tighter audit-credible attribution; "follows the paradigm" is exactly the level of claim a cryptographer would accept. Glossary line 1510 also updated to match. **Closed.** Note on §3.2 reference: RS06's actual section numbering would need verification against the paper itself; "§3.2" is plausible but unverified — see MINOR #4 below.

### R7 MINOR #2 (joint-window policy)

§16.7 lines 1174–1181 add the "Joint-window precedence" paragraph: "When both `register_model` (§11.6) and `schema_pending` are engaged on the same worker (the deploy-with-DDL case), `schema_pending` takes precedence on `subscribe(...)` — loud `Conflict { code: "schema_pending" }`, SDK bounded retry. The §11.6 silent-acceptance rule applies only when `register_model` is engaged alone." Rationale is given ("`schema_pending` carries the stronger signal: worker cannot correctly decode CDC for any subscriber until reload"). Sound — the policy correctly prefers the stronger signal. **Closed.**

### R7 MINOR #3 (`commit_id` allocation rule)

§11.5 lines 783–794 add "**`commit_id` allocation.**": SQLite uses an in-process `AtomicU64` on the `SqliteSession` actor, allocated at drain entry, resets to 0 on session boot alongside `last_seen_lsn_seq`. PG uses the pgoutput commit-LSN (durable in the WAL). The two backends meet at the broker contract despite divergent allocators. Both per-session and durable variants are correctly typed: subscribers crossing a restart receive `resync` (which papers over the SQLite reset). **Closed.** (Residual: line 779 reads as if commit_id-at-drain-entry applies to PG too — see MINOR #3 below.)

### R7 MINOR #4 (AUTOINCREMENT 2^63 ceiling)

§11.5 lines 748–753 add: "`sqlite_sequence` is signed 64-bit; counter caps at 2^63 − 1 (INSERT fails `SQLITE_FULL` after exhaustion). Not load-bearing at dev-tier rates (§14): 10^9 ev/s sustained for 292 years to reach the ceiling. Noted to forestall a future reviewer flagging an unbounded counter." Math checks (2^63 ≈ 9.22e18; 9.22e18 / 1e9 / 31_557_600 ≈ 292.5 years). SQLite's actual error code on AUTOINCREMENT exhaustion is `SQLITE_FULL` per docs — accurate. **Closed.**

### R7 MINOR #5 (P6 split)

§19 splits P6 into P6a (correctness: F1 sweeper, F2 terminal-state, per-app PG role, drop-namespace sequencing) and P6b (operational: column-key rotation, advisory-lock 64-bit, alert threshold, dashboards). The split is reasonable; "P6a precedes P6b; both ship together at day-1 readiness" preserves the original "all 25 ship at day-1" contract. Edge: F1 sweeper-half could arguably go in P6b (it's a background sweeper, operational by nature), but the categorisation isn't load-bearing because both ship together. **Closed.**

### R7 Missing-concept closures

- **MC #1 (`commit_id` allocation rule)** — Closed via R7 MINOR #3 closure.
- **MC #2 (joint-window policy)** — Closed via R7 MINOR #2 closure.
- **MC #3 (pre-`bundle_invalidated` race)** — §16.7 lines 1197–1208 add the "Pre-`bundle_invalidated` race" paragraph: "When DDL on worker A produces frames worker B sees via shared PG replication, byte-level ordering of those frames against B's `bundle_invalidated` arrival is not guaranteed: a frame may arrive ≤ 1× control-plane RTT (typically ≤ 50ms; §16.7 durability rail) before its event. Posture: a `DecodeError` in that pre-engagement gap fires the §17.6 watchdog once; the control event lands during reconnect; the shim engages; the re-streamed frame is dropped cleanly. The watchdog therefore absorbs at most one reconnect per deploy per worker — a quantum, not a loop." The "tolerant-decoder always" alternative is explicitly rejected with rationale ("it would mask schema corruption outside deploys"). **Closed.**
- **MC #4 (disengage resync trigger semantics)** — §16.7 lines 1210–1217 add "Disengage resync semantics. On shim disengage the broker emits one synthetic `resync` per active subscription (not one broker-wide): each subscriber's `(collection, predicate)` read-set (§11.8) refetches independently. `subscriptions_active` is the exact upper bound on `resync` count per disengage." Cross-references §11.8 (within-app multi-tenancy interaction). **Closed.**

---

## Round 9 findings

### CRITICAL

None.

### IMPORTANT

None.

### MINOR

1. **§16.7 line 1144 — "upstream of ConsumerHandle, wrapping the pgoutput decoder" is mildly underspecified about whether the shim composes around the decoder or sits in the decoder→sink wire.** §7.2 line 336 defines `ChangeStream::spawn_consumer` as returning a `ConsumerHandle`; in PG the consumer "streams pgoutput, pre-image + post-image in the frame" — i.e., the decoder is owned by what `spawn_consumer` produces. Saying the shim is *upstream of* the produced handle AND *wraps* the decoder owned by the handle is a layering claim an implementer could read two ways: (a) the shim is a decorator inside the consumer, intercepting the decoder's output before it reaches the handle's outward sink (a within-handle layer); (b) the shim sits between two pieces — a raw-frame producer (e.g., `compio_postgres::Replication::frames()`) and the `ConsumerHandle` constructor — and what was previously called "the decoder" is now extracted out of `ConsumerHandle`. The decode-error story works in (a); in (b) the §7.2 definition of `ConsumerHandle` would need an editorial update saying "the decoder is *not* internal — `ConsumerHandle` is a sink." The doc currently reads as (a) but doesn't say so explicitly. **Action**: one sentence — "the shim is an in-handle decorator over the decoder's output, before the handle's broker-facing sink." One line; no architectural change.

2. **§11.6 still doesn't say how `Broker::suppress_app` propagates across the worker fleet.** §11.6 line 826: "register_model calls `Broker::suppress_app(app_id, true)` before Pass 1 of the migration pipeline and clears it after Pass 2 or on error." §16.7 line 1126: "`register_model` is not concurrent in production; §10's advisory lock is defence-in-depth." This reads as if suppress_app fires at the call site of `register_model` — which in production is the control plane (§16.7 line 1122: "runs register_model against PG once (NOT per worker)"). But the broker is per-isolate, per-worker (§11.3). For every active worker to receive `suppress_app(app_id, true)` before any pgoutput frames from the DDL begin reaching its WAL consumer, suppress_app must propagate via a control event (presumably the same rail that carries `bundle_invalidated` — see §16.7 durability paragraph at lines 1219–1226). The doc doesn't say. If suppress_app is fire-and-forget local-only, then DDL on worker A produces frames worker B's broker fans out before B has been told to suppress — same race the §16.7 pre-`bundle_invalidated` paragraph closes for the schema-pending decoder, but not closed here. **Action**: one sentence in §11.6 — "suppress_app propagates via the same at-least-once control event rail as `bundle_invalidated` (§16.7); workers dedupe by `(app_id, suppress_token)`."

3. **§11.5 line 779 reads as if `commit_id`-at-drain-entry applies to PG.** "build `ChangeEvent`s in `(commit_id, lsn_seq)` order — `commit_id` is a per-tx monotonic stamped at drain entry, matching pgoutput 'all-at-commit-LSN' on PG." The "stamped at drain entry" clause is SQLite-only; PG's commit_id is the pgoutput COMMIT record's LSN, allocated by the WAL writer at commit time, not by anything in plugin-db's drain. The follow-up paragraph at lines 783–794 disambiguates ("SQLite ... at drain entry ... PG: commit_id is the pgoutput commit-LSN"), so the inconsistency is local to line 779. **Action**: one-word fix — change "is a per-tx monotonic stamped at drain entry" to "is a per-tx monotonic (allocation by backend; see below)" and let the next paragraph define it.

4. **Cover line 8 still says "Last updated: 2026-05-22 (round 6 — ...)".** Rounds 7 and 8 added the §6.5 promotion, shim-placement reconciliation, joint-window policy, AUTOINCREMENT ceiling note, `commit_id` allocation rule, disengage-resync semantics, pre-`bundle_invalidated` race, and P6 split. The "Last updated" header doesn't mention any of this; a reader skimming the cover would not know rounds 7 and 8 landed. Not a correctness issue; doc-hygiene only. **Action**: append " ... round 7 was a no-op (review only); round 8 closes the §6.5 anchor regression and shim-placement contradiction, splits P6 into P6a/P6b, pins `commit_id` allocation, adds joint-window precedence + pre-`bundle_invalidated` race + disengage-resync semantics."

### Missing concepts

None that block convergence. The doc still does not specify:
- How `Broker::suppress_app` propagates fleet-wide (see MINOR #2 — same rail as `bundle_invalidated` is the obvious answer but not written).

Both items are sub-CRITICAL polish.

---

## Cross-anchor integrity check

- §17.5 ↔ §19 P6a: bidirectional, language matches (P6a row at line 1454 calls out §17.5 explicitly). **Pass.**
- §11.5 ↔ §6.5: §6.5 now exists at line 257; line 743 references it; round-7 CRITICAL closed. **Pass.**
- §11.5 ↔ §10 (audit row state machine): line 743 anchor lands on §10.3's state machine. **Pass.**
- Glossary §20 ↔ body: `schema-pending decoder` (line 1508), `Deterministic-IV-via-HMAC AEAD` (line 1510), `slot-ownership-stays-platform` (line 1511), `schema_pending` (line 1512) all have body anchors. **Pass.**
- §16.3 alert table ↔ §19 P6b alert-threshold row: language matches at "> 1000 over 5 min while `schema_pending = true`". **Pass.**
- §13.5 ↔ §11.5 allow-list reference: §13.5 exists and discusses MV-as-broker-invisible-cache; §11.5 line 712 cross-references "load-bearing — see §13.5". **Pass.**
- §11.8 ↔ §16.7 disengage-resync read-set reference: §11.8 exists and discusses scope-key predicate; §16.7 line 1212 anchors there for "each subscriber's `(collection, predicate)` read-set." **Pass.**
- §17.5 ↔ §19 P6a "per_app_role_cannot_create_or_read_slot" fence (line 1461): matches §17.5 invariant. **Pass.**
- Round-8 markers present at lines 256, 414, 748, 783, 1143, 1173, 1196, 1209, 1452, 1475; ten markers, all paired with the change they introduce. **Pass.**

---

## Hard-constraint check (no implementation code in the design doc)

`grep -n "fn " ` / `impl ` / `let ` / `Rust code block` / ``` blocks audit: only one ``` block (lines 613–617, audit row state machine textual diagram); no Rust function bodies, struct field-list code, or executable snippets. Capability-trait shapes (`SqlExecutor::Client`, `LockManager::Guard`, `ChangeStream::ConsumerHandle`, etc.) appear as identifier mentions inside prose, not as `trait` declarations. **Pass.**

---

## Convergence verdict

Round-9 score 90.7 (+1.0 vs R7 89.7; +1.7 vs R6 89.0). **0 CRITICAL, 0 IMPORTANT, 4 MINOR.** Both round-7 findings closed. All five round-7 MINORs and four round-7 missing concepts closed. No new regressions introduced by round 8 — the reviser's targeted closures (§6.5 promotion, shim-placement reconciliation, P6a/P6b split, commit_id allocation, joint-window precedence, AUTOINCREMENT ceiling note, pre-`bundle_invalidated` race, disengage-resync semantics) all read clean.

**Termination rule per round-9 mandate:**
- Score ≥ 90 + 0 CRITICAL + 0 IMPORTANT → END (converged). **Met** (90.7, 0 CRITICAL, 0 IMPORTANT).

# **CONVERGED**

Score progression R1 62 → R2 77 → R3 85 → R4 87 → R5 89 → R6 89 → R7 89.7 → R8 (no critique) → **R9 90.7**.

The doc plateaued at 89 for two rounds (R6, R7) waiting on two structural fixes; round 8 cleared both and the doc crosses 90 on the next critique. Four MINORs remain — all one-line editorial fixes — but none meet the IMPORTANT bar (no load-bearing contradictions, no broken anchors, no spec-level ambiguities that would block an implementer). Recommend landing the doc as a stable contract and pushing the four MINORs into the implementation PR's commit message or a follow-up doc-polish pass; none gate P0 start.

**No round 10 needed.**
