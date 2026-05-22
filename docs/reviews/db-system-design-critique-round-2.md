# plugin-db System Design — Round 2 Critique

**Target:** `docs/proposals/db-system-design.md` (985 lines, round-2 revision dated 2026-05-22).
**Prior round:** 62/100.
**Reviewer mode:** harsh; subtler than round 1.

## Overall Score: 77 / 100 (Δ +15)

The reviser closed every round-1 CRITICAL and IMPORTANT cleanly. The hard constraint (no `pub trait` / `sql` / `toml` / multi-line TS blocks) is now satisfied — only the §10.4 audit-row state-machine code block remains and round 1 explicitly accepted it as text-art. Abstraction-level rises from 38 → 86. The remaining bar is the new layer of subtle inconsistencies the global revision introduced and a small set of genuinely missing concepts that surface once the prose tightens.

---

## Per-Dimension Scores

| # | Dimension | Round 1 | Round 2 | Δ | Rationale |
|---|---|---:|---:|---:|---|
| 1 | Clarity | 72 | 82 | +10 | §7.2 reads as a designer's brief rather than a header file. §4 matrix is unchanged and still load-bearing. §5.5 explicitly names the static-vs-dyn cliff and resolves it. §11.5 commit-ordering paragraph is exact. Some new cross-references compound (§16.6 → §10.3 → §11.6 → §17.6 → §17.7); a reader chasing pause/resume hops three sections before landing. §11.6 ("pause/resume during DDL") referenced from §16.6 does not exist as a numbered subsection. |
| 2 | Completeness | 68 | 80 | +12 | All 6 missing concepts from round 1 added (§16.6 eviction, §16.7 schema-version, §17.7 slot lifecycle, §18A blocking pool, §8.5 cross-process locks, §9.1 PG prereqs). Open gaps that surfaced now the prose is tight: (a) `MaterializedView` refresh interaction with reactive subscriptions on the underlying base tables is not described; the shadow-table DELETE+INSERT inside `BEGIN IMMEDIATE` will fire BEFORE-DELETE triggers and flood the outbox unless explicitly suppressed (see CRITICAL #1); (b) `__zeroship_pre_image_seq` mentioned only in §11.5 with no recovery path on worker restart — is `lsn_seq` per-process or persisted?; (c) `EncryptedColumn` design lists AEAD + HKDF but never names the storage shape (nonce-prefixed BLOB? side-table?) or query-time decryption strategy — the filter language semantics over encrypted values is undefined; (d) `Backup` on SQLite mentions `VACUUM INTO` snapshot but says nothing about snapshot consistency with an in-flight writer tx; (e) `__zeroship_audit_<collection>` is named twice (§4 row 15, §6.5) but the schema/columns/retention/who-writes are nowhere. |
| 3 | Soundness | 60 | 72 | +12 | Round-1 soundness failures (`BEGIN EXCLUSIVE`, `cache=shared`, update_hook misuse, drain ordering, metering composition) are correctly resolved. Surviving / new soundness issues: §17.7 step (1) "signal every worker holding a slot to drain" — but in PG logical decoding a replication slot is **bound to the publisher (the PG cluster)**, not to a consumer process; the consumer can be killed at any time and the slot remains until DROP_REPLICATION_SLOT. The "signal every worker" model is correct as a graceful step but the doc should say the consumer cancellation is courtesy, not necessary, before the slot drop. §5.5 says PG WAL "delivers pre-image + post-image natively in the frame" — only true under `REPLICA IDENTITY FULL` (which §11 mentions in passing but §5.5 does not gate). §11.5 says SQLite outbox uses `json_object(...)` over the collection's columns — fine for scalars, but breaks for `Bytes`/`Vector` (BLOB) and `Json` (already TEXT), and SQLite's `json_object` requires explicit `json()` wrapping for already-JSON text to avoid double-encoding. §10.5 says "PG hashes `(app_id, name)` into the two int4 arguments" — `pg_advisory_lock(bigint)` and `pg_advisory_lock(int4, int4)` both exist; the previous round-1 spec was `hashtext(...)::int4` and the round-2 prose hand-waves "hashes." Hash function and width must be named for correctness (collision domain is the entire cluster across all apps; an int4×int4 collision rate at N apps × M lock names matters). |
| 4 | Consistency | 65 | 70 | +5 | Several inconsistencies the global rewrite introduced (see findings). The biggest: §2 line 47 says "**three-phase** migration pipeline (DDL + backfill)" but §5.3 line 136 and §10 line 481 say "**four-phase** pipeline" (Bootstrap / Plan / Validate / Apply). §11.5 line 578 references "Pass 1 of the migration pipeline (§10.3)" — there is no §10.3 numbered subsection in §10 (only 10.5, 10.6 are labelled; the four passes are inline). §16.6 line 791 references "the watchdog's job (§17.6)" — §17 contains a "Replication slot DoS (PG)" paragraph but no numbered §17.6. §11.6 referenced from §16.6 broker pause/resume similarly absent. |
| 5 | Feasibility | 65 | 78 | +13 | The static-dispatch cliff is resolved cleanly. LOC estimates removed; "site count is measured at the start of P0, not estimated" is the correct posture. P0–P6 phases each name an integration-test gate. New feasibility concern: §13 fail-open / fail-closed window has a sensible default but the doc does not say which subsystem holds the state machine (control plane? worker?). Quota cache at line 670 "Cache miss falls back to the next flush ACK" — but the flusher push is one-way (worker → control plane); the doc does not describe how an ACK carries quota state back. The two paths (read-side cache refresh every 60s vs flusher ACK) are at risk of disagreeing. |
| 6 | Scope-discipline | 70 | 84 | +14 | §12 / §13 / §15 all open with explicit "this section captures only what plugin-db owns" + reference to the canonical doc. §18 dropped Q10 (migration tooling). §3 personas reduced and tied to capability set in one paragraph (§3 line 79–88). Minor remaining gold-plating: §13 "Failure modes" paragraph still re-specifies `meter_dropped_samples` semantics that belong in `docs/reference/billing-metering.md`. §16.6 isolate eviction is genuinely DB-design scope; §16.7 schema-version coordination is in scope. |
| 7 | Abstraction-level | 38 | 86 | +48 | **All eight round-1 `pub trait` blocks removed**; replaced with bold-name prose. **No SQL fragments** in code blocks remain. **No TypeScript** code blocks longer than 3 lines. The single surviving `code` fence (audit row state machine, §10 lines 489–493) was explicitly accepted as text-art in round 1. The §7.2 prose preserves enough signature information (`RowValue` named, associated types named, return shapes named) to be implementable without re-introducing implementation. The decorator/sub-trait contradiction is now consistent across §7.2 and §13. Loss: §10.6 backfill description dropped the round-1 `fetchBatch` / `migrateOne` / `commitBatch` method names — the prose retains them, fine; but the per-batch tx model is now described in one paragraph that compresses dry-run vs normal in a way that needs one more read than necessary. |

---

## Confirmation: round-1 CRITICAL findings closed

| # | Round-1 CRITICAL | Status |
|---|---|---|
| C1 | §7.2 eight `pub trait` blocks | CLOSED — prose tables; names + intent preserved |
| C2 | §7.2 Metering impl body | CLOSED — decorator described as `MeteredSqlExecutor<E>`, consistent with §13 |
| C3 | §7.4 `dyn Backend` cliff vs associated types | CLOSED — §5.5 introduces `BackendHandle` enum; consumer code generic over narrowest bound; "NO `Rc<dyn Backend>`" restated three times |
| C4 | §7.3 generic `apply<'p, B>` in rust block | CLOSED — §7.3 is prose now |
| C5 | §11.5 9-line SQL CREATE TRIGGER | CLOSED — prose-only; `json_object(...)` named inline as intent, no SQL fence |
| C6 | §15 TS code blocks | CLOSED — §15 is prose; example reference is `docs/reference/db.md` |
| C7 | `Vec<Value>` undefined row representation | CLOSED — `RowValue` named, tagged-enum variants enumerated, rationale (encryption boundary + metering) given |

All seven closed cleanly. The reviser did the work.

---

## Findings (Round 2)

### CRITICAL

1. **§7.2 `MaterializedView` (SQLite path) blows up CDC.** Refresh is described as "`DELETE FROM <mv>; INSERT INTO <mv> SELECT …;` in a single `BEGIN IMMEDIATE` tx." But §11.5 installs BEFORE UPDATE/DELETE triggers on every registered collection — the `<mv>` shadow table is itself a collection at the SQL level. The DELETE inside refresh will fire BEFORE-DELETE triggers, blow up `__zeroship_pre_image`, and either (a) emit a `ChangeEvent` storm to subscribers of the MV "collection" or (b) require the orchestrator to gate trigger installation by collection kind. Neither is stated. PG side has the analogous question: is `REFRESH MATERIALIZED VIEW CONCURRENTLY` covered by the publication, and does pgoutput emit per-row events? (CONCURRENTLY does a swap; non-CONCURRENTLY is TRUNCATE+INSERT — different visibility on the slot.)

2. **§17.7 SQLite drop-namespace ordering does not address an open writer.** "Drop the trigger set on each registered collection, drop the outbox table, DETACH the per-app file, delete the file." If a worker isolate currently holds an open `SqliteSession` with the per-app file attached, DETACH on that connection's main `<app_id>` alias will fail (`SQLITE_LOCKED`/`SQLITE_BUSY`). The ordering says nothing about quiescing the writer, terminating the session, or reclaiming the LRU slot before file deletion. On POSIX, deleting an open file is legal but the worker keeps writing into the freed inode until close. Compare to §16.6 which describes the eviction-side drop; the two sides are not joined.

3. **§5.5 "PG via `compio-postgres`" vs §11 "replication connection."** §5.5 says PG uses `compio-postgres` for all DB traffic. §11.5 says `wal_consumer::run_supervised` "opens a replication connection, streams pgoutput frames." A PostgreSQL replication connection uses the streaming-replication sub-protocol (START_REPLICATION, COPY-both mode, pgoutput) — distinct from the normal extended query protocol. The doc never says whether `compio-postgres` implements the replication sub-protocol. Given the zero-tokio invariant, this is implementation-bearing: the consumer cannot just open a normal pooled client. Either name the support level in `compio-postgres` or call this out as a P0 dependency.

### IMPORTANT

4. **Three-phase vs four-phase inconsistency.** §2 line 47 advertises a "three-phase migration pipeline (DDL + backfill)" as a goal. §5.3 line 136 says "register_model four-phase pipeline." §10 line 481 enumerates four phases (Bootstrap / Plan / Validate / Apply). The §2 prose appears to count "DDL + backfill" as a structural decomposition, but a reader checking "what does plugin-db deliver" against §2 will see three; against §10 they see four. Unify.

5. **Dangling section anchors.** §11.5 (line 578) cites "Pass 1 of the migration pipeline (§10.3)" — §10 has subsections 10.5, 10.6 only; the four passes are inline under §10 with no number. §16.6 (line 797) cites "slot reaping is the watchdog's job (§17.6)" — no §17.6. §16.6 references "§11.6" implicitly via mention of pause/resume which is in §11.5 not 11.6. §3 (line 75) cites §16.3 for observability — §16 has subsections 16.6 / 16.7, observability is the third unlabelled paragraph. After a heavy re-section pass the anchors did not get renumbered.

6. **§10.5 PG advisory-lock hash function is hand-waved.** Round 1 IMPORTANT #14 asked for a single, decided spec; the revision now says "PG hashes `(app_id, name)` into the two int4 arguments." Which hash? `hashtext`? `digest(...,'sha256')`? `pg_advisory_lock(bigint)` vs `(int4,int4)`? Collision probability across N apps × M lock-names is non-trivial at platform scale; a collision means two unrelated migrations serialise. Birthday-bound on int4 is ~65k. The earlier `(int4, int4)` form widens to int8 but the doc does not declare which form is canonical.

7. **§11.5 SQLite `lsn_seq` durability undefined.** "A monotonic local sequence number (private `__zeroship_pre_image_seq` table)" — the §11.5 paragraph reads as a counter table whose value persists in the per-app file. But the "join by pk to the in-hand `RETURNING` post-image, builds `ChangeEvent`s, COMMITs, and on success publishes" workflow uses `lsn_seq > last_seen_lsn_seq` — and `last_seen_lsn_seq` is not described as persisted. On worker restart mid-tx, is the next event guaranteed to have `lsn_seq > last_seen_lsn_seq` from the *previous* worker process? If `last_seen_lsn_seq` lives in worker memory only, restart skews delivery semantics. (Probably fine because SQLite CDC is in-process only, but the doc should state the reset rule.)

8. **§7.2 `SqlExecutor` row representation: `RowValue` enum is `Null | Bool | Int | Float | Text | Bytes | Json | Uuid | Decimal | Timestamp | Vector` — missing `GeoPoint` and `Array`.** §6.2 declares 13 ZsTypes including `GeoPoint` and `Array(T)`. The revision argued `RowValue` covers "the ZsType space" but the enumeration in the prose drops two. Either explicitly say GeoPoint is stored as two floats and reconstructed by the SDK / Array is stored as JSON in `RowValue::Json`, or extend the enum. The encryption-boundary argument (`Bytes` distinguishable from `Text`) implies the enum is canonical, in which case it must cover all 13.

9. **§7.2 `EncryptedColumn` storage shape unspecified.** "Symmetric AEAD over column values. Both backends use Rust-side `aes-gcm` keyed by HKDF over `ZEROSHIP_COLUMN_KEY` and the column identifier." Missing: nonce strategy (per-row random? deterministic-from-pk?), AAD content, ciphertext-vs-plaintext on disk (PG `BYTEA`? a `__zsenc__` sentinel prefix?), and the filter semantics — `find({ where: { ssn: "123-45-6789" } })` on an encrypted column with per-row nonces requires either a blind-index column (not declared) or full table scan + decrypt. Round 1's "deferred (§19 P6)" handled this; round 2 promotes EncryptedColumn into the §7.2 capability surface but did not promote the design with it.

10. **§13 metering quota cache vs flusher ACK channel.** "Quota cache. Worker-local cache of the control-plane quota table, refreshed every 60s. … Cache miss falls back to the next flush ACK." The flusher is described as a 5s push (UsageReport); the doc does not describe an ACK channel or what data it carries. Two read paths (60s pull refresh; 5s push ACK) without a consistency story risk one path stale and the other not.

11. **§17.7 PG step 2 "5s timeout, otherwise killed."** What does "killed" mean here — `pg_terminate_backend` on the replication-slot consumer connection, OS SIGKILL on the worker, or `pg_drop_replication_slot` with FORCE? Different answers have different recovery semantics (force-drop loses any in-flight ChangeEvents not yet acked by the consumer; backend termination is graceful from PG's side). Name the operation.

12. **§6.2 FTS5 maintained by triggers — trigger interaction with `__zeroship_pre_image`.** §11.5 installs BEFORE UPDATE/DELETE triggers on every collection; §6.2 says FTS5 is "maintained by triggers." The doc does not say whether the FTS-maintenance triggers are AFTER the row mutation or interleave with the pre-image triggers, nor what order they fire in. SQLite trigger ordering is by `name` (lexicographic) within the same TIMING+EVENT bucket — a real correctness concern, especially when the FTS trigger updates the FTS5 vtable and the orchestrator outbox drains all triggered work pre-COMMIT.

13. **§7.2 `LockManager` PG-vs-SQLite signature divergence acknowledged but not minimised.** "On SQLite the client argument is unused at the lock-table level; the SQLite impl documents that." This is an acceptable compromise, but it preserves the round-1 awkwardness (one signature, two effective semantics). A two-method split (`acquire_session_bound(client)` for PG advisory + `acquire_local(app_id, name)` for in-process) would be the cleaner contract. Document the choice rationale explicitly: "the unified signature is preferred because consumer call-sites should not branch on backend at the lock-acquire site."

### MINOR

14. **§4 row 22 lock-naming.** Says "`register_model` + `__zeroship_migrations` + advisory locks." The `LockScope::GlobalApp { name: "register_model" }` formalism is now in §10.5; §4 should say "register_model lock scope" or omit the word to avoid suggesting `register_model` IS the lock.

15. **§4 row 1 SQLite cell.** Reads "BEFORE UPDATE/DELETE triggers + per-app outbox; drain just before COMMIT (§11.5)." Missing INSERT path entirely; round 1 specifically called this out and §11.5 prose handles it ("INSERT events do not need a pre-image"). Mention "INSERT via RETURNING" in the matrix row.

16. **§5.5 "CDC layer" labels.** Says "PG: WAL consumer streams pgoutput; logical decoding delivers pre-image + post-image natively in the frame." Correct only with `REPLICA IDENTITY FULL` (or `INDEX`/`DEFAULT` for partial pre-image). The doc names REPLICA IDENTITY FULL in §11.5 but not in §5.5 or §4 row 1; for a load-bearing PG configuration prerequisite this should be in §9.1 alongside `wal_level=logical`.

17. **§6.2.1 Decimal lex-sort spec needs explicit invariants.** "value normalised to fixed scale `s`, zero-padded on the integer side to `p - s` digits, sign-prefixed (`-` for negatives, `+` for non-negatives) so TEXT ordering compares numerically within the declared precision." A `Decimal(10,2)` value `-99999999.99` and `+12345678.90` — does the sign character sort lexicographically? `+` (0x2B) < `-` (0x2D) in ASCII, so negatives sort AFTER positives, inverting numeric order. Either use `0`/`1` sign nibbles + two's-complement-style flip for negatives, or name a different encoding. As written the rule is wrong.

18. **§6.2 table inconsistency for `Bool`.** "`INTEGER` (0/1, CHECK)" — the CHECK constraint shape is unspecified; SQLite has no `BOOLEAN` affinity, so the CHECK must reject NULL-equivalent-to-0 confusion. Minor but the design says "the SDK consumer cannot tell which backend they're on"; type-coercion edges matter.

19. **§8 "No SQL fragments inline" claim.** Line 386 declares "No SQL fragments inline — every 'trigger' / 'BEGIN' / 'ATTACH' claim is described in intent." §11.5 nonetheless inlines `json_object(...)`, `BEGIN IMMEDIATE`, and `RETURNING` as SQL-ish identifiers. These are fine (function/clause names, not statements) but the declaration is overstated; "no multi-line SQL fragments" would be more accurate.

20. **§13 `bytes_stored` is a periodic refresh** but the source (per-app `pg_database_size`? `PRAGMA page_count`?) is unstated. For PG schemas, `pg_total_relation_size` per schema is the usual answer; for SQLite-attached files, file size. Name the source.

21. **§16 observability list missing a key metric.** `db_pending_emit_queue_depth` or equivalent — the in-tx pending-emit queue is described in §5.3 and §11.5 but no metric exists. For diagnosing CDC backpressure / drain failures this is the obvious instrument.

22. **§18 Q1 "Recommend: parse-time check added during P0"** — cross-app FK enforcement. P0 is named in §19 as "capability trait split + `PostgresBackend` migration"; adding parse-time FK checks is unrelated work that doesn't belong in P0's scope. Move to P1 or §19's housekeeping.

23. **§19 phase gates name integration-test fixtures but don't name fixture locations.** P2 gate `update_publishes_change_event_with_pre_image` — under `crates/plugin-db/tests/`? `sdks/db/tests/`? The phase rubric is good; the gate locations make it operational.

24. **§20 glossary `pid` in token payload (§12 line 619).** `actor_kind:actor_id:pid:nonce:expires_at` uses `pid` without glossing. Project ID? Process ID? Permission ID?

### Missing concepts (newly visible after round-1 fixes)

1. **`MaterializedView` ↔ CDC interaction** (CRITICAL #1).
2. **`EncryptedColumn` storage / filter semantics** (IMPORTANT #9).
3. **`__zeroship_audit_<collection>` schema/retention.** Named in §4 row 15 and §6.5 (system tables list) with no schema, no retention rule, no who-writes-on-commit.
4. **Snapshot consistency on SQLite.** `VACUUM INTO` while a writer holds `BEGIN IMMEDIATE` — the doc says "`.backup` API fallback when the source DB is busy" but the consistency level of the resulting file is not stated (a snapshot taken during a long migration may be a half-committed view).
5. **`compio-postgres` replication-protocol support** (CRITICAL #3 implication).
6. **PG-to-SQLite SDK behavioural divergence beyond `pitr_pg_only`.** The doc claims "the SDK consumer cannot tell which backend they're on; no `unsupported_op` envelope reaches creator code" (§1). But `pitr_pg_only` is exactly such an envelope (in §15.7). The contract bends. Either say "ops the dashboard exposes never surface unsupported_op; ops the platform invokes may" or accept that PITR is admin-surface only and tighten the §1 claim.
7. **Cross-isolate cache coherence for `registered_models`.** Two workers hold the same app's isolate; one workers' `register_model` was the source-of-truth call (§16.7) but the other workers' isolate cache still holds the old `schema_version`. §16.7 says "stale → isolate evicted and reloaded" — fine, but the trigger is "next request"; for an idle isolate the stale cache lives until next traffic. CDC events flowing through that worker's broker between deploy and next request are decoded against the old `LiveSchema`. Document the staleness window.

---

## Overall Verdict

The reviser took the round-1 critique seriously and executed cleanly. Abstraction-level discipline is now where round 1 demanded it (86 vs 38); the static-vs-dyn cliff is the canonical example of the reviser solving — not papering over — the design problem (`BackendHandle` enum + per-capability generic bounds is the right answer, named explicitly three times). Soundness moves from 60 → 72 because the round-1 sharpening (BEGIN IMMEDIATE, no shared-cache, commit ordering, decorator vs sub-trait) is correct in the new prose.

The remaining 23-point gap is real:
- CRITICAL #1 (`MaterializedView` refresh emits ChangeEvent storm on its own shadow table) is a load-bearing soundness bug the round-2 revision introduced by promoting `MaterializedView` into §7.2 alongside CDC without resolving the interaction.
- CRITICAL #2 (drop-namespace ordering on SQLite vs an open writer) is a real lifecycle hole.
- CRITICAL #3 (`compio-postgres` replication-protocol support) is a P0 dependency that may force a separate driver path or extension; the doc cannot afford to be silent on it given the zero-tokio invariant.
- IMPORTANT #4 (three- vs four-phase) and #5 (dangling section anchors) are global-revision artefacts a §-anchor sweep would close in one pass.
- IMPORTANT #6 (PG hash function for advisory lock keys) is the load-bearing detail that round 1 IMPORTANT #14 surfaced and round 2 only partially closed.

Push CRITICAL #1–#3 closed and IMPORTANT #4–#6 closed and the doc reaches the high-80s. The capability-trait split (P0) is now safely implementable as written; the rest is detail work.

**Round-2 score: 77 / 100, Δ +15.**
