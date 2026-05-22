# plugin-db System Design — Round 4 Critique

**Target**: `docs/proposals/db-system-design.md` (1242 lines, post round-3 revision).
**Date**: 2026-05-22.
**Prior rounds**: R1 62, R2 77, R3 85.

## Scores

| Dimension | Score | Δ vs R3 | Rationale |
|---|---|---|---|
| Completeness | 89 | +2 | Outbox GC, project/`pid` definition, `bundle_invalidated` durability + isolate TTL, advisory-lock collision posture all landed. Backfill×CDC interaction still missing; `subscription_app_dropped` missing from `.code` table. |
| Correctness | 84 | +6 | VACUUM INTO mechanism fixed; `(commit_id, lsn_seq)` added; drop-namespace DDL-while-writer-alive fixed. New gap: deterministic-encryption is mis-named "AES-GCM-SIV"; intra-tx INSERT vs UPDATE/DELETE ordering still under-specified despite the new ordering tuple. |
| Extensibility | 88 | +2 | No regressions. `EncryptedColumn` randomised/deterministic split is generalisable. `Metering` decorator pattern composes cleanly. |
| Operational | 90 | +2 | Outbox startup-truncate + steady-state pre-COMMIT DELETE bounds outbox to one tx. Idle-isolate TTL caps `bundle_invalidated` failure window. §17.6 watchdog reaffirmed. Missing: backfill broker pause; `db_outbox_rows` gauge. |
| Security | 84 | +1 | Round-3's deterministic-mode AAD reasoning is now explicit and correct (omits PK, retains collection ‖ column). §17.4 collision posture honestly states accidental-collision is statistically expected at scale. Lingering: PK as AAD on randomised mode assumes client-generated PK (typed_id) — not asserted. |
| Developer Experience | 87 | +1 | `pid` glossary entry + §10.7 cross-reference closes the project-concept dangle. §15.7 grew but misses `subscription_app_dropped`. Encrypted-column SDK validation rule (`invalid_filter`) explicit. |
| Industry Alignment | 88 | 0 | `(commit_id, lsn_seq)` matches Debezium's `txId` + intra-tx offset pattern. VACUUM INTO read-snapshot now accurate. Naming a HMAC-truncate scheme "AES-GCM-SIV pattern" overstates — real RFC 8452 SIV derivation uses POLYVAL keyed by KDF subkey, not HMAC-SHA256. |
| **Overall** | **87** | **+2** | Round-3 reviser closed all four R3 CRITICALs and most IMPORTANTs. Net forward motion, but the doc surfaces two new substantive issues and one unfixed gap from R3's missing-concepts list. **Not converged.** |

**Convergence check.** Score ≥ 85: yes. ≥ 1 CRITICAL: yes (deterministic-encryption naming + intra-tx ordering). ≥ 1 IMPORTANT: yes. **Not converged. Round 5 needed.**

---

## Round 3 CRITICAL closures — verified still closed

- **R3 CRITICAL #1 (deterministic-IV nonce derivation).** §7.2 line 394 now reads `nonce = HMAC(k_siv, plaintext)[..12]` — derived from plaintext, not row PK. The fix is correct in mechanism; the naming "AES-GCM-SIV pattern" introduces a new MINOR (see R4 MINOR #2). Closed for correctness.
- **R3 CRITICAL #2 (INSERT-path `lsn_seq` gap).** §11.5 lines 663–675 add a per-tx drain that allocates a contiguous `lsn_seq` range via `__zeroship_pre_image_seq` and stamps it onto INSERT outbox rows materialised from `RETURNING`. Closed at the row level. **New gap** — the *relative* order of INSERT vs UPDATE/DELETE within a tx is still under-specified; see R4 CRITICAL #1.
- **R3 CRITICAL #3 (VACUUM INTO lock claim).** §16.1 lines 914–931 correctly describe WAL read-snapshot semantics, remove the wrong `BEGIN IMMEDIATE` claim, narrow `SQLITE_BUSY` to checkpointer/schema-change ops, and add the `migration_in_progress` interlock. Closed.
- **R3 CRITICAL #4 (drop-namespace DDL without writer).** §17.7 lines 1076–1103 reorder: (1) sub gate, (2) DDL via live `SqliteSession`, (3) DETACH from peers, (4) `SqliteSession` shutdown, (5) POSIX unlink. The DDL step now precedes the writer shutdown. Closed.

---

## Round 4 findings

### CRITICAL

1. **§11.5 — intra-tx INSERT vs UPDATE/DELETE order is still under-specified despite `(commit_id, lsn_seq)`.** Quote (lines 665–671): *"(1) UPDATE __zeroship_pre_image_seq SET v = v + N RETURNING v to claim a contiguous range; (2) stamps it onto the UPDATE/DELETE rows BEFORE-triggers wrote, and inserts INSERT outbox rows from RETURNING; (3) SELECTs the stamped rows, builds ChangeEvents in (commit_id, lsn_seq) order."* The drain stamps `lsn_seq` onto two row populations: (a) UPDATE/DELETE rows already written by BEFORE triggers in DB-execution order (`__zeroship_pre_image` rowid order), and (b) INSERT rows inserted by the drain itself "from `RETURNING`". The spec does not say *which lsn_seq slot* a given INSERT receives relative to UPDATEs/DELETEs that appeared interleaved in the SDK call order. Two natural implementations are possible:
   - **(i) all UPDATE/DELETE rows first, then INSERTs** — drives all INSERTs to the tail of the tx regardless of call order; breaks the round-3 promise that "cross-op ordering within a tx is the drain's visit order, which matches the SDK call order on Collection."
   - **(ii) merge by SDK timestamp** — requires the orchestrator to remember per-call sequence; nothing in the spec captures this.

   The P2 gate `mixed_ops_in_one_tx_ordered_by_commit_then_lsn_seq` (§19 line 1177) names the property but the prose does not pin the implementation. PG side does not suffer this (pgoutput WAL record order is monotonic across ops); SQLite side needs an explicit per-call counter or trigger-side stamping. Spec must commit to a mechanism, not just an ordering tuple.

### IMPORTANT

1. **§7.2 — "AES-GCM-SIV pattern" misnames the construction.** Quote (lines 392–397): *"indexed equality via the AES-GCM-SIV pattern (matches AWS DynamoDB Encryption Client deterministic mode, CipherStash's equatable mode): `nonce = HMAC(k_siv, plaintext)[..12]`, then encrypt under `k_enc` with that nonce."* AES-GCM-SIV (RFC 8452) is a specific AEAD that derives the SIV via POLYVAL over the message + AAD with a per-nonce-derived hashing key, then uses the SIV as both the tag and the AES-CTR nonce. The construction described here is HMAC-derived deterministic-nonce on top of AES-GCM — sometimes called "synthetic IV via HMAC" or "deterministic AEAD via PRF-IV." Calling it "AES-GCM-SIV pattern" suggests RFC 8452 compliance, which it is not. AWS DDB Encryption Client's deterministic mode actually uses AES-SIV (RFC 5297), distinct again. The construction described is fine on its own merits (it's what CipherStash does for `equatable`), but the name is wrong. Either cite the actual scheme (HMAC-IV deterministic AEAD, with security argument tied to HMAC being a PRF) or switch to a real RFC 8452 AES-GCM-SIV implementation (requires a different crate; `aes-gcm-siv` exists in `RustCrypto`). Without this, an auditor reading "AES-GCM-SIV" will expect POLYVAL and not find it.

2. **§11.5 — `__zeroship_pre_image_seq` persistence has no purpose if `commit_id` is process-local.** Quote (lines 669, 677): *"commit_id is a per-tx monotonic stamped at drain entry [...] **Durability + restart.** __zeroship_pre_image_seq persists across restart. The companion cursor last_seen_lsn_seq is process-local; SQLite CDC is in-process only, so restart resets it to 0 and the orchestrator skips outbox replay."* If subscribers reset on restart (in-process only) and `commit_id` is process-local, then `lsn_seq` durability is wasted state — the seq number's only consumer is within a single tx within a single process boot. The startup truncate (§ Outbox GC) deletes the rows that referenced it. Either drop the persistence claim (initialise `lsn_seq` from 0 on every boot) or explain what subscriber consumes the value across restarts. Carries no immediate bug, but the durability commitment narrows future evolution (e.g., adding cross-process subscribers would require revisiting both `commit_id` and `lsn_seq` semantics).

3. **§17.7 — `subscription_app_dropped` not in the `.code` taxonomy.** Quote (line 1059, 1064, 1081): the event is fired to active subscribers on `--force` drop and "the SDK surfaces it as a terminal error on the subscription iterator for graceful shutdown." §15.7 (`.code` table) does not list it. SDK consumers branching on `.code` to distinguish "app deleted under me" from generic terminal errors have no stable string to match. Add `subscription_app_dropped` (or `app_dropped`) to the table under Conflict or a new Terminal variant.

4. **§16.7 — "schema-pending decoder" is introduced but never specified.** Quote (lines 988–990): *"Worker drops the old LiveSchema ref and attaches a 'schema-pending' decoder that suppresses events until reload, publishing one synthetic resync at the end."* New construct. No definition of what the decoder does to in-flight pgoutput frames it cannot decode (drop? buffer? error?), no specification of when "reload" completes (next request? control-plane push of new bundle?), no test in §19 P6 (or anywhere). The mechanism is the close-the-staleness-window primitive; under-specifying it leaves the gap R3 IMPORTANT #4 only partly closed.

5. **Backfill × CDC interaction (R3 missing-concept #3 unresolved).** §10.6 backfill loops per-batch with per-batch COMMIT; each batch UPDATE fires BEFORE triggers → outbox → broker. Active subscriptions during backfill see a flood of events. §11.6 only documents broker pause for DDL (`register_model`), not for `migrations.run` backfill. Either backfill needs its own broker pause/resume (parallel to §11.6), or the doc must state explicitly that subscribers will see one ChangeEvent per backfilled row — and the SDK must document this for AI-builder app authors. A million-row backfill emitting a million events through the broker is operationally damaging.

### MINOR

1. **§6.2.1 — zero-handedness in the sign encoding is ambiguous.** Convention says "byte `1` for non-negatives". `+0` and `-0` (numerically equal but distinct in the encoding) both arise from `Decimal(p, s)` with all zeros: `1` + zero-padded zeros vs `0` + nines-complement of zeros = `9...9`. Round-trip equality on lookup will treat them as unequal. Either canonicalise `-0` → `+0` at SDK validation, or document the asymmetry. (At dev tier this is unlikely to fire, but worth pinning.)

2. **§7.2 EncryptedColumn — AAD on randomised mode assumes client-generated PK.** Quote (line 379): *"AAD = collection id ‖ column name ‖ row PK bytes (binds ciphertext to its row)."* If PK is a DB-generated identity column (rare in this stack, since typed_id is client-generated per §20), the row PK is not known until after INSERT, but the encrypted ciphertext is a column value that must be set *during* the INSERT. The doc doesn't assert "PK MUST be client-generated for encrypted columns" — and the §15 SDK doesn't prevent declaring a DB-generated identity PK with an encrypted column. Either add an SDK-side validation rule (`invalid_schema { code: ... }` rejecting encrypted columns under non-client-PK schemas) or specify a two-pass INSERT (insert with placeholder, encrypt+UPDATE with returned PK as AAD — which leaks the placeholder briefly).

3. **§17.4 — `pg_advisory_lock(bigint)` deferral references "§19 P6" without a backlog row.** Quote (line 1034): *"partitioning by 64-bit keys (PG's single-bigint pg_advisory_lock variant) would close the window and is deferred to §19 P6 as a tunable."* §19 P6 (line 1203) lists six tasks; the 64-bit advisory-lock variant is not among them. Either add it explicitly or remove the §19 P6 reference.

4. **§17.7 — POSIX-unlink behaviour on shared filesystems.** Quote (lines 1098–1102): *"If a worker did not ack step 3 within the grace, unlink proceeds — POSIX semantics let that worker continue writing into the freed inode until its FD closes; those writes discard with the inode at final close (acceptable: the app is being deleted)."* True on local Linux; NFS, FUSE, and S3-FUSE backends have weaker unlink-while-open semantics. The doc previously committed to LocalFs in dev (AGENTS.md "Object Storage: LocalFs in dev"), but doesn't restrict `${db_dir}` to local POSIX. If `${db_dir}` is ever mounted from a network FS, this guarantee breaks. State the local-FS prerequisite, mirror the §9.1 PG prereq style.

5. **§11.5 — outbox JSON encoding handling of NULL columns.** Quote (lines 655–659): *"UPDATE/DELETE BEFORE triggers write old-tuple JSON (json_object(...) over the collection's columns; Bytes/Vector base64-inline as TEXT; Json wrapped in json(...) to avoid double-encoding), op kind, pk, relation id, UNIX-ms timestamp."* `json_object('a', NULL)` in SQLite produces `{"a":null}`, but the doc does not say whether downstream `ChangeEvent` distinguishes "column absent" from "column null". For NOT-NULL columns this is moot; for nullable columns the broker predicate evaluator (`Predicate::matches`) needs to know. Spec a coercion or assert that all collection columns appear with `null` placeholder.

### Missing concepts (still not in the doc)

1. **Backfill × CDC broker pause** (see R4 IMPORTANT #5). Round 3 missing-concept #3 not addressed.

2. **`db_outbox_rows` / `__zeroship_pre_image` size gauge.** §16.3 lists `db_pending_emit_queue_depth` (in-memory) but no metric for outbox rows in-flight. Operational visibility for the round-3 outbox-leak class is incomplete.

3. **Per-app PG role + replication-slot owner (R3 missing-concept #5 unaddressed).** §17 P6 mentions per-app PG roles. The replication slot needs the `REPLICATION` attribute. Per-app roles can't own slots (security boundary); slots stay platform-owned. The cross-reference between §9.1 and the deferred §19 P6 work is still implicit.

4. **`schema-pending decoder` definition** (see R4 IMPORTANT #4). New construct introduced without spec.

5. **Encryption key rotation.** §7.2 specifies `hkdf(ZEROSHIP_COLUMN_KEY, salt=...)`. Rotation policy for `ZEROSHIP_COLUMN_KEY` itself is not stated. After a rotation, old ciphertexts decrypt only with the old key. PG hardening §12 covers session-secret rotation; there is no parallel for column-encryption key rotation. At minimum, the doc should state the rotation is out-of-scope and old ciphertext remains decryptable only with the previous key (a recovery-only mode).

---

## Cross-anchor integrity check

- Round 4 markers present at lines 223, 372, 652, 737, 808, 914, 979, 1020, 1076. Nine markers, all paired with the change they introduce. Section numbering remains stable (§1–§20 with `.N` subsections in §6, §7, §8, §10, §11, §13, §15, §16, §17, §18); no renumber-induced cross-reference drift detected.
- Glossary (§20) gained `pid` (project id) and `REPLICA IDENTITY FULL`, `hashtext`, `EncryptedColumn`, `MV shadow table`. All cross-reference live anchors.
- §15.7 `.code` table gained `subscriptions_active`, `replica_identity_required`, `pitr_pg_only`, `migration_in_progress`. Missing: `subscription_app_dropped` (R4 IMPORTANT #3).
- §19 implementation phases name CRITICAL-fence tests for R3 closures (R3 #1 → `deterministic_encrypted_equality_via_index`; R3 #2 → `mixed_ops_in_one_tx_ordered_by_commit_then_lsn_seq`; R3 #3 → `vacuum_into_snapshot_consistent_under_concurrent_writer`; R3 #4 → `sqlite_drop_namespace_runs_ddl_while_writer_alive`). All present.

---

## Convergence note

Round-4 score 87 (+2 vs R3's 85). One CRITICAL (intra-tx ordering mechanism) plus five IMPORTANTs plus five MINORs plus five missing concepts. **Not converged.** Round 5 should focus on:

1. Pinning the SQLite intra-tx event-ordering mechanism (per-call counter vs trigger-side stamping).
2. Renaming "AES-GCM-SIV pattern" to a construction that matches the implementation, or switching to a real RFC 8452 implementation.
3. Closing the backfill × CDC interaction.
4. Specifying the `schema-pending decoder` or removing the construct.
5. Adding `subscription_app_dropped` to `.code`.

Two clean rounds at ≥ 90 with 0 CRITICAL + 0 IMPORTANT are the stop condition; this round is one CRITICAL short of that bar.
