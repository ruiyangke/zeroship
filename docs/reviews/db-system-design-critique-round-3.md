# plugin-db System Design — Round 3 Critique

**Target**: `docs/proposals/db-system-design.md` (1141 lines).
**Date**: 2026-05-22.
**Prior rounds**: R1 62/100, R2 77/100.

## Scores

| Dimension | Score | Δ vs R2 | Rationale |
|---|---|---|---|
| Completeness | 87 | +5 | §10.7 audit schema, §16.7 staleness, §13.5 MV×CDC, §17.7 sequencing all landed. Outbox GC still missing; `pid`/project concept dangles. |
| Correctness | 78 | -2 | New prose introduced four soundness regressions: SQLite deterministic-IV encryption is unusable as specified; `VACUUM INTO` lock claim is wrong; SQLite trigger lex-name ordering is not contract; INSERT-path `lsn_seq` gap. |
| Extensibility | 86 | +4 | Static-dispatch `BackendHandle` + 15-trait split + `MeteredSqlExecutor` decorator generalise cleanly. Capability bounds now consistent across §7/§10/§19. |
| Operational | 88 | +6 | §16.3 slot/lag/queue-depth alerts; §17.6 watchdog; §17.7 drop-sequencing; §16.7 schema-version coord. Outbox-leak on restart and INSERT-ordering hole are operational debt. |
| Security | 83 | +3 | §17.4 collision posture reasonable for non-creator-controllable IDs but overstates safety at scale; PG hardening surface explicit; deterministic-encryption gap is a feature-correctness loss, not a security loss. |
| Developer Experience | 86 | +4 | Stable `.code` table at §15.7 covers new variants (`subscriptions_active`, `replica_identity_required`, `pitr_pg_only`). `EncryptedColumn` filter prose under-specifies billing. |
| Industry Alignment | 88 | +6 | `REPLICA IDENTITY FULL` + pgoutput + `pg_drop_replication_slot` + `pg_terminate_backend` sequencing matches PG ops practice. AEAD pattern matches CMK conventions. AWS-DDB-style metering mismatch on encrypted scans is the lone outlier. |
| **Overall** | **85** | **+8** | Solid revision. Four soundness regressions and one billing gap keep it out of the 90s. |

**Convergence check.** Score ≥ 85, ≥ 1 CRITICAL, ≥ 4 IMPORTANT. **Not converged.** Round 4 needed.

---

## Round 2 CRITICAL closures — verified still closed

- **R2 CRITICAL #1 (MV×CDC storm).** §13.5 + §11.5 install allow-list + §7.2 MV section all align. `__zeroship_mv_*` excluded from `ChangeStream::provision` trigger install. Gate test named (`mv_refresh_does_not_emit_change_events`, P2). Closed.
- **R2 CRITICAL #2 (SQLite drop-namespace).** §17.7 step ordering present. *But* a new sequencing bug appeared in the revision — see Round 3 CRITICAL #4 below. The original race is closed; a different one opened.
- **R2 CRITICAL #3 (PG WAL sub-protocol).** §5.5 + §9.1 spell out `replication=database`, `START_REPLICATION`, `CopyBothResponse`, pgoutput decode, dedicated connection per `(worker, app)`. `compio-postgres/src/replication.rs` named. Closed.

---

## Round 3 findings

### CRITICAL

1. **§7.2 EncryptedColumn — SQLite deterministic-IV scheme is unusable as specified.** Quote (line 384): *"on SQLite deterministic computes `HKDF(key, nonce=row_pk)` so equality compares ciphertexts directly."* For deterministic equality to support `WHERE encrypted_col = ?`, the nonce MUST be derived from the **plaintext value**, not from `row_pk` — the query side does not know the target row's PK before the equality predicate selects it. Standard practice (AWS DynamoDB encryption client, CipherStash): `nonce = HMAC(key, plaintext)` so two encryptions of the same plaintext produce identical ciphertexts. As written, two rows containing the same plaintext encrypted under different `row_pk`s yield different ciphertexts → equality search returns zero rows. Round 2 reviser inverted the derivation. Either remove SQLite deterministic mode entirely or fix the derivation.

2. **§11.5 INSERT-path has no `lsn_seq` — cross-event ordering hole.** Quote (line 634): *"INSERT events need no pre-image — published from the mutation's `RETURNING` post-COMMIT."* INSERT events therefore bypass `__zeroship_pre_image_seq` entirely. UPDATE and DELETE allocate `lsn_seq` from the seq table under the writer tx; INSERTs do not. Consequence: within a single transaction mixing INSERT+UPDATE+DELETE, the broker has no defined ordering between INSERT events and UPDATE/DELETE events. Subscribers materialising aggregates (e.g., `count` deltas, ordered logs) will observe ordering drift. PG side does not suffer this — pgoutput LSNs are monotonic across all ops. SQLite design needs INSERT events to allocate an `lsn_seq` (either from the same seq table, or via post-write outbox stamp before COMMIT) for parity with the PG side.

3. **§16.1 SQLite snapshot consistency — wrong lock claim.** Quote (line 858): *"`VACUUM INTO` runs inside an implicit transaction and takes the same read/write reservations as `BEGIN IMMEDIATE`; while a writer holds the writer reservation (e.g. mid-backfill batch tx) the call blocks until that tx commits or rolls back, so the resulting file is a consistent snapshot."* `VACUUM INTO` does NOT take a writer reservation on the source DB. It opens a read transaction on the source and writes to the destination file. In WAL mode, reads do not block writers, and writers do not block `VACUUM INTO`. The snapshot is still consistent — but via WAL read-snapshot semantics, not via writer-reservation serialisation as the doc claims. The outcome is right; the mechanism is wrong. Either the prose should say "shared read-snapshot in WAL mode" or the `ifBusy: "abort"` ergonomic that returns `LockContention` is misnamed (there is no busy condition for `VACUUM INTO` against a concurrent writer in WAL mode — only against a checkpointer or schema-changing op).

4. **§17.7 SQLite drop-namespace — step 3 lacks a writer connection.** Quote (step 1, line 996): *"the `SqliteSession` actor accepts a `Shutdown` command on its mpsc queue, stops new commands, lets in-flight transactions COMMIT or ROLLBACK."* After Shutdown the actor exits and its `rusqlite::Connection` drops. Step 3 (line 1000): *"Drop the trigger set on each registered collection and drop the outbox table"* — DROP TRIGGER and DROP TABLE are DDL, require a write connection. §18 Q8 specifies "1 writer + 4 readers" — readers cannot run DDL. The sequencing either needs (a) Shutdown to be a quiesce that keeps the underlying Connection alive, or (b) the orchestrator to open a dedicated DDL Connection after step 1 and before step 3. Doc is silent.

### IMPORTANT

1. **§7.2 EncryptedColumn — metering vs scan-cost mismatch.** Quote (line 379): *"the orchestrator decrypts candidate rows in `MeteredSqlExecutor` post-processing, and the filter re-applies client-side; metered as `rows_read` on the post-decrypt set."* If a creator filters only by an encrypted column, the SDK strips the predicate and the DB performs a full scan. `rows_read` is then charged on the *post-filter* count (small), but the DB actually read the full table (large). The platform absorbs the cost; the creator can run unbounded encrypted scans for a `rows_read` charge of 1. AWS DynamoDB meters by *items examined*, not items returned, precisely to prevent this. Decision needed: meter on the candidate set (rows actually fetched from the DB), or refuse equality filters on non-deterministic encrypted columns at SDK validation time.

2. **§11.5 — outbox storage leak on restart.** Quote (line 644): *"committed-but-not-yet-published txs are lost (≤1 tx of notifications). Active subscribers receive `resync` on reconnect."* Committed outbox rows that were never drained sit in `__zeroship_pre_image` forever. The doc has no sweeper for orphan outbox rows. Over months of restarts this accumulates. Need: either a startup-time `DELETE FROM __zeroship_pre_image WHERE lsn_seq <= max(last_published)` (impossible — last_published is process-local), or a TTL-based sweep ("delete rows older than 1h on startup"), or a high-water-mark in another table.

3. **§11.5 trigger ordering — relying on undocumented behaviour.** Quote (line 649): *"SQLite fires triggers in lex-name order within `(timing, event)`; pre-image is BEFORE, FTS is AFTER — different buckets, no interleave."* SQLite's official documentation states trigger firing order within a (timing, event) bucket is **arbitrary** and may change between releases. The "different buckets, no interleave" half of the claim is the load-bearing one and is correct; the lex-name claim is unnecessary and not contract. As written, a future SQLite point release that changes intra-bucket order would silently break the FTS path. Remove the lex-name assertion — it adds nothing beyond what timing-bucket separation already provides.

4. **§16.7 schema-version coordination — circular fallback for idle isolates.** Quote (line 925): *"Fire-and-forget; failure falls back to next-request eviction."* The "next-request eviction" mechanism IS the route-pull check at §16.7 line 909 — same mechanism that the `bundle_invalidated` event is supposed to backstop. If `bundle_invalidated` fails AND no request arrives, the idle isolate's WAL consumer decodes new pgoutput frames against the old `LiveSchema` indefinitely. Either commit to durable invalidation (control-plane retries until ack), or accept the staleness as bounded only by isolate LRU eviction TTL — and state that TTL.

5. **§13.5 MV broker visibility — contradictory commitment.** §7.2 line 360 says MV is "a broker-invisible cache." §13.5 line 766 says: *"Subscribers listening on the MV's logical name (if SDK-exposed) receive a synthetic `resync` from the MV scheduler, not CDC."* Either MV is SDK-exposed for subscriptions (then specify the API and the synthetic-resync emitter) or it is not (then drop the conditional). Round 2 reviser left this open.

6. **§17.4 advisory-lock collision domain at scale.** Quote (line 953): *"the 4.3 B `int4` × cluster-wide domain combined with `app_id` being a non-creator-controllable UUIDv7 base62 typed_id means a malicious creator cannot force collisions against another app's `register_model`."* `hashtext()` produces a 32-bit hash; with N apps, expected first collision at ~√(2^32) = ~65k apps (birthday). The "cannot force" claim is correct for creator-controllable attack, but the doc's overall tone implies collisions don't happen. At Shopify scale (>100k stores) accidental collisions on `(app_id, "register_model")` are statistically likely. Consequence is benign (one tenant waits on another's `register_model` lock, briefly) — but the doc should acknowledge it and either pick a 64-bit advisory-lock variant (PG's `pg_advisory_lock(bigint)`) or accept the rare cross-tenant wait as a documented behaviour.

### MINOR

1. **§6.2.1 Decimal scheme — under-specified for scale and padding.** Worked examples in lines 232–233 use `Decimal(4, 0)`. The scheme requires zero-padding to width `p` for lex-sort to work, and the decimal point is implicit (fixed scale `s`). Neither is stated. A reader implementing this for `Decimal(8, 4)` (e.g., money) would not know whether `1.5` is stored as `"100001.5000"` or `"100015000"` or `"15000"`. The arithmetic in the example *is* correct under the unstated convention; just state the convention.

2. **§20 `pid` — "project" concept never defined elsewhere.** Glossary line 1137 says `pid` is "project id, typed_id of the creator's project under which this session was minted (NOT UNIX pid, NOT permission id). One project may own many apps." But §1, §3, §14, AGENTS.md crate index — none introduce "project" as a unit. The session token carries `pid`, the audit table writes `pid`, but the platform model in the rest of the doc speaks only of apps. Either project is a real concept that needs a §1 intro paragraph, or `pid` should be removed from the session payload and audit row.

3. **§4 row 1 — pre-image qualifier consistent with §9.1.** Quote (line 97): *"requires `REPLICA IDENTITY FULL` for pre-image."* §9.1 expands this; §5.5 references it. All three sections now agree. Note for downstream: pgoutput's `DEFAULT` identity does emit PK columns as pre-image, so "for pre-image" elides "for full-row pre-image." Pedantic, no fix needed.

4. **§11.3 slot count — undercount vs §16.6 lifecycle.** Quote (line 605): *"each worker process runs its own WAL consumer per loaded app, so the cluster carries N_workers × M_loaded_apps active replication slots."* §16.6 line 903 commits that "PG slots are NOT dropped on eviction." Realistic slot count is `N_workers × M_ever_loaded_apps_since_watchdog_reap`, not `M_loaded_apps`. §16.3 alert at 80% of `max_replication_slots` mitigates, but the §11.3 sizing math understates demand.

5. **§13 — `bytes_stored` source consistency.** Line 720 says PG uses `pg_total_relation_size` summed per app schema. This excludes `pg_indexes_size` for indexes outside the per-app schema (there shouldn't be any, but worth noting) and includes TOAST. State whether vacuum-bloat shows up in the metric — for creators this could mean charged-for bytes that disappear after `VACUUM FULL`.

### Missing concepts (still not in the doc)

1. **Outbox row sweep policy** (see IMPORTANT #2). No GC for `__zeroship_pre_image` rows that committed but were never published (worker crash between COMMIT and publish).

2. **Encrypted-column metering policy** (see IMPORTANT #1). The §13/§7.2 interaction for full-scan-with-strip semantics is undefined.

3. **Backfill+CDC interaction.** §10.6 backfill loops per-batch with COMMIT. Each batch UPDATE fires pre-image triggers → outbox rows → broker emits ChangeEvents. Active subscriptions during backfill will see a flood of events. Doc doesn't specify whether backfill suppresses CDC (parallels §11.6 DDL pause). Should the broker pause during backfill? If not, subscribers fan out a million events for a million-row backfill.

4. **`bundle_invalidated` event durability** (see IMPORTANT #4). The event is the close-the-staleness mechanism but has no retry/ack semantics.

5. **Per-app PG role + replication-slot owner.** §17 P6 mentions per-app PG roles. The replication slot needs the `REPLICATION` attribute. Per-app roles can't own slots (security boundary), so slots stay platform-owned. Cross-reference between §9.1 (control-plane DB role with `REPLICATION`) and the deferred §19 P6 work needs to confirm per-app role hardening doesn't break slot provisioning.

---

## Cross-anchor integrity

Verified every `§N.M` reference resolves to a real heading. New §10.3, §10.7, §11.6, §13.5, §16.7, §17.4, §17.6 all exist with the cited content. §18A `compio::runtime::spawn_blocking` pool sizing exists. The dangling round-1 references in earlier drafts (§16.1, §16.3 from §3) are now satisfied — §3 line 82 references both §16 generally and they exist at §16.1 / §16.3.

## Hard constraints

- No `pub trait { ... }` blocks. Verified by `grep`.
- No multi-line SQL / TOML / TS blocks. Verified by ` ``` ` count = 2 (both anchored to the §10 audit-row state-machine fence, which the round-3 mandate exempts).
- No code fences besides the audit-row ASCII state machine. Verified.

## Disposition

R2's 77 → R3's 85 is a real improvement. The four soundness regressions are concentrated in the new prose round 2 added (deterministic-IV encryption derivation, `VACUUM INTO` lock claim, lex-name trigger ordering, INSERT-path `lsn_seq` gap, drop-namespace step-3 connection lifecycle). All four are localised — the surrounding architecture is sound. Round 4 should close them and the five missing concepts; if it does, expect 90+ and convergence.

Round 3 does **not** converge. One CRITICAL + four cross-cutting design holes from R2 reviser slipped past. Run round 4.
