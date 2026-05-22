# plugin-db System Design — Round 1 Critique

**Target:** `docs/proposals/db-system-design.md` (987 lines, last updated 2026-05-22)
**Reviewer mode:** harsh. First-draft expectations.

## Overall Score: 62 / 100

Weighted average. The doc is structured and ambitious, but the abstraction-level discipline collapses in §7 (the doc's own self-declared centrepiece), several backend-mechanism claims need sharpening to be safely implementable, and §19 mixes phase ordering with LOC counts that look like fabricated estimates. A solid skeleton with serious round-1 work to do.

---

## Per-Dimension Scores

| # | Dimension | Score | Rationale |
|---|---|---:|---|
| 1 | Clarity | 72 | Reading order in §1 is genuinely useful. §4's 25-row PG/SQLite split is the doc's strongest passage. But §7's trait-by-trait dump reads as code-review material, not architecture; a senior engineer drops down to syntax-parsing instead of system-shape building. The 15-trait list in §5.4 and the §7.1/§7.2 list are duplicated and slightly inconsistent (`Backup` in §7.2 super-trait composition vs. the 15-trait list — both have 15, fine; but §5.4 names them in a different order than §7.2). |
| 2 | Completeness | 68 | All 20 sections present. But §3 personas are a checkbox section with no scenarios driving the design. §11.6 (broker pause/resume) is a single short paragraph hiding non-trivial semantics. §16 observability is mostly metric names without alerting thresholds or runbook hooks. §17 threat model omits the SQLite ATTACH cross-app vector explicitly (mentioned in passing but no enumeration of what `cache=shared` exposes between attached schemas on the same connection). §20 glossary is solid. The doc waves hands on: connection-pool semantics under §13.2 wrapping, broker thread-locality vs. cross-isolate auth (§11.3), and exactly when each `Metering` counter gets recorded across the trait stack. |
| 3 | Soundness | 60 | Several specific concerns. (a) §7.2 `SqlExecutor::pool_query` returns `Vec<Value>` without saying which `Value` — this is the load-bearing row representation; calling it `Value` is hand-wave. (b) §8.7 / §11.5: `Connection::update_hook` fires inside the writing transaction on the writer's thread and exposes only `(action, db, table, rowid)`, not row contents. The doc's "INSERT fast path" via update_hook is plausible but the doc never explains how the new tuple is recovered (presumably the same RETURNING set the orchestrator already holds; never spelled out). It also doesn't address that update_hook does NOT fire on ROLLBACK and does NOT fire on virtual-table updates (FTS5, vec0), meaning FTS/vector INSERTs need a different path or trigger. (c) §8.5 claims `BEGIN EXCLUSIVE` provides "best-effort cross-process" advisory locking. EXCLUSIVE blocks all readers under WAL too, which is heavier than the doc admits — `BEGIN IMMEDIATE` is the usual "writer reservation"; EXCLUSIVE escalates further. The PG vs SQLite divergence note ("`SQLITE_BUSY` not 'lock not available'") is correct but the underlying mechanism is mis-described. (d) §6.1 claims SQLite per-app file is `ATTACH DATABASE 'file:zs-<id>?cache=shared' AS ...`. `cache=shared` requires shared-cache to be enabled at the connection level via the URI or `sqlite3_enable_shared_cache`, and rusqlite's bundled SQLite ships with shared-cache disabled by default at compile time on some configurations. The doc relies on this without flagging the build flag. (e) §7.2 `LockManager` makes `acquire` take `&Self::Client`. For PG advisory locks tied to session lifetime, this is right; for the SQLite in-process HashMap path, the client argument is unused — flag and document, or two impls do different things with the same signature. (f) §10.6 says backfill batches `ROLLBACK each commit` for dry-run; but per-batch `commitBatch` followed by ROLLBACK isn't a transaction model — describe whether dry-run uses one outer tx with savepoints per batch, or per-batch tx that always ROLLBACKs. (g) §11.5 says drain "happens on commit in the same tx" — that contradicts itself; if you're in the tx you haven't committed yet. The intended mechanism (drain inside the same tx, just before COMMIT) needs to be stated. (h) §13.2 wrapping snippet shows `Metering` wrapping `inner_pool_exec` — implying decorator/composition over `SqlExecutor` — but §7.2 declares `Metering: SqlExecutor` (sub-trait), implying inheritance. These are different composition models and contradict. |
| 4 | Consistency | 65 | §5.4 lists 15 capabilities; §7.2 super-trait composes 15 (matches); §19 P0 says "Split the 26-method `Backend` trait into 15 capability sub-traits" (matches). Good. BUT: §5.4 ordering differs from §7.2 super-trait composition order, which is minor. §6.2 `Tsvector` type maps to "separate FTS5 vtable" — meaning there's no first-class column type on SQLite; the table is misleading. §6.5 says `__zeroship_pre_image` is "SQLite only" but §11.5 implies the broker uses it for pre-image join — but §7.2 `ChangeStream` description for PG says "publication + replication slot" with no pre-image join — fine, but the doc never says "PG pre-image arrives in WAL pgoutput frames natively" so the contrast isn't drawn. §10.5 names the lock `zs_reg:` + app vs. §10.3 says `LockScope::GlobalApp { name: "register_model" }` — these are not the same key; pick one. §13 metric `rows_written_quota_exceeded` is in §15.7 errors table but §13.1 lists "rows_written" as a counter and §13.4 mentions only "rows_written_quota_exceeded"; the spelling of metric vs. error code should be stated as deliberately different or aligned. |
| 5 | Feasibility | 65 | The §7 split + §19 P0 is the riskiest piece. The doc says ~22 files modified and "4-6 PRs" without enumerating the actual call sites. `IsolateDbContext::backend` going from `Rc<PostgresBackend>` to `Rc<dyn Backend>` is non-trivial because `dyn Backend` precludes associated types like `SqlExecutor::Client` and `LockManager::Guard` — the doc declares those associated types in §7.2 then proposes a `dyn Backend` consumer in §7.4 step 5. This is a real impossibility unless the doc either (a) names `Box<dyn Any>` Client/Guard, (b) makes `Backend` non-object-safe and consumers stay generic, or (c) introduces an object-safe facade. The doc says nothing about this cliff. Similarly, §7.3's `apply<'p, B>` generic shape is what static dispatch wants — but §7.4 step 5 wants dyn dispatch. Pick one; admit the cost. §8.16 "row-by-row INSERT in single tx; 10-100× slower than PG COPY" is a numeric claim with no measurement (and the user's memory explicitly bans estimates without measurement). §13.3 "5s flusher" cadence + §13.4 "60s quota refresh" need an explicit answer for what happens when the flusher fails / the control-plane is down — the doc doesn't say whether the worker fails open or closed. |
| 6 | Scope-discipline | 70 | Non-goals (§2) are clearly stated and substantive (multi-master, federation, query-language, wire-compat). Good. But the doc gold-plates in places: §13.4 quota enforcement and §13.5 dashboard surface belong in `docs/reference/billing-metering.md`, not the DB system design. §12 auth subsystem has substantial overlap with `docs/reference/auth.md`; the doc should reference rather than re-specify. §15 SDK shape duplicates `docs/reference/db.md` material — fine to summarise, but §15.1-15.7 reads like spec rather than design. §18 open question 10 ("PG → SQLite migration tooling") is CLI scope, not plugin-db scope, and recommends a CLI command — drop. |
| 7 | Abstraction-level | 38 | This is where the doc most clearly fails the round-1 bar. §7.2 contains EIGHT separate `rust` code blocks declaring `pub trait` with `async fn signature(...) -> Result<...>` bodies. That is "concrete impl" by the doc's own contract. The `Metering` block (§7.2) goes further and embeds a multi-line `async fn pool_exec(&self, sql, params) -> Result<u64, DbError>` BODY with `Instant::now()` and timing arithmetic — pure implementation. §7.3 contains a generic `pub(crate) async fn apply` signature in a `rust` block. §11.5 contains a 9-line SQL `CREATE TRIGGER` block — a SQL fragment by the constraint definition. §12.2 token-payload schema is in a `rust`-ish fence and inline-shape but is borderline (~3 lines, OK). §15.1-15.5 contain four substantial TypeScript code blocks, several well over 3 lines (§15.1 is ~20 lines; §15.5 is ~7). §10.4 audit row state machine is text-art (OK). §19 P1 says `~800 LOC`, `~400 LOC`, `~250 LOC`, `~30 test files` and §7.4 says `+1800 / -300 LOC` — these are LOC predictions, which read as estimates without measurement. The doc's stated contract is "code replaced with prose"; instead the doc replaces prose with code in its most important section. |

---

## Findings

### CRITICAL (must fix before round 2)

1. **§7.2 — trait declarations in `rust` blocks violate the no-impl-code rule.** Each of `SqlExecutor`, `NamespaceManager`, `LockManager`, `ChangeStream`, `SchemaIntrospect`, `IndexBuilder`, `DialectBuilder`, `SessionMinter`, `Metering`, `VectorIndex`, `MaterializedView`, `EncryptedColumn`, `Backup` is presented as a Rust trait body with full method signatures, parameter types, generic bounds, return types. Per the round-1 hard constraint, every one of these is a finding. The capability surface should be described in prose tables ("trait name; one-line purpose; which methods, by intent; PG impl pointer; SQLite impl pointer") — names inline are fine, full signatures are not.

2. **§7.2 `Metering` includes an implementation body.** Lines around 405-413 embed `async fn pool_exec(...) { self.check_quota(...).await?; let t0 = Instant::now(); let res = self.inner_pool_exec(...).await; ... }`. This is implementation, not design. Worse: it conflicts with the trait declaration directly above it that has `Metering: SqlExecutor` (sub-trait) — the body uses `inner_pool_exec` which implies decorator/wrapper composition. Pick one composition story and describe it in prose.

3. **§7.4 step 5 vs. §7.2 associated types — design impossibility.** `IsolateDbContext::backend: Option<Rc<dyn Backend>>` is incompatible with `SqlExecutor` having `type Client` and `LockManager` having `type Guard`. `dyn Backend` is not object-safe with those associated types. The doc proposes both without acknowledging the conflict. This is the single largest hidden cliff for the implementor.

4. **§7.3 — generic `apply<'p, B>` function declaration in a `rust` block.** Same constraint violation as §7.2. Replace with prose: "consumers take the narrowest capability bound they need, e.g. `apply` needs SqlExecutor + IndexBuilder + LockManager + Metering."

5. **§11.5 — 9-line SQL `CREATE TRIGGER` fragment.** Per the hard constraint, every SQL fragment in a `sql` block is a finding. Describe the trigger's intent in prose: "on every registered collection, install a BEFORE UPDATE/DELETE trigger that captures (rel_id, pk, op, old_tuple_json, monotonic_lsn, ts) into the pre-image outbox table." Trigger shape, not trigger source.

6. **§15.1, §15.3, §15.4, §15.5 — TypeScript code blocks longer than ~3 lines.** §15.1 schema-declaration example is ~20 lines of TS in a code block; §15.5 transaction example is ~7 lines. Per the constraint each is a finding. Compress to prose descriptions of the surface, or link to `docs/reference/db.md` for the canonical example.

7. **§7.2 `SqlExecutor::pool_query` returns `Vec<Value>` — `Value` is undefined.** Whether this is `serde_json::Value`, a custom row enum, or a typed cursor is load-bearing for the rest of the design (especially metering, CDC, encryption boundary). The design must commit to the row-representation choice in prose, not punt with `Value`.

### IMPORTANT (should fix this round)

8. **§8.7 / §11.5 — CDC mechanism mis-specifies how INSERT new-tuple is captured.** `Connection::update_hook` only delivers `(action, db, table, rowid)`. The doc says "INSERT events use `Connection::update_hook` directly (no pre-image needed)" but never says how the new tuple bytes reach the broker. Presumably from the orchestrator's own RETURNING set, but that requires the writer's call-site to push the new tuple into the broker — making `update_hook` redundant. Resolve in prose: either (a) drop update_hook and have the orchestrator publish on every successful mutation (simpler, doc should say so), or (b) explain how update_hook + a side channel reconstruct the tuple.

9. **§11.5 — "drain happens on commit in the same tx" is contradictory.** State explicitly: pre-image rows are inserted via trigger during the tx; orchestrator SELECTs them just before issuing COMMIT; if commit succeeds the rows are durably gone with the tx and ChangeEvents are published post-commit; if ROLLBACK they vanish with the tx. This is the actual mechanism but the doc's phrasing scrambles it.

10. **§8.5 — `BEGIN EXCLUSIVE` is mis-characterised.** EXCLUSIVE blocks readers under WAL mode (since `wal-index` mmap acquisition fails), which the doc treats as "best-effort cross-process lock." Either (a) use `BEGIN IMMEDIATE` and accept it as a writer-reservation only, (b) document that EXCLUSIVE freezes all readers for the duration of the held lock and decide whether that's tolerable, or (c) admit cross-process locking is not provided and rely solely on the file-locking semantics SQLite already offers.

11. **§6.1 — `ATTACH DATABASE ... cache=shared` requires shared-cache mode.** rusqlite's bundled SQLite has shared-cache controlled by build features / runtime call. The doc relies on `cache=shared` without naming the build flag or runtime enable. SQLite's own docs discourage shared cache for new code. State whether shared cache is enabled and why, or drop `cache=shared` from the URI.

12. **§6.2 — `Tsvector` row in the type table is misleading.** SQLite has no column type for FTS; FTS5 is a virtual table. Saying "separate FTS5 vtable" in the type column conflates a column type with a side index. Either remove `Tsvector` from the user-facing type table (it's an internal implementation detail) or note that on PG it's a column and on SQLite it's a hidden vtable maintained by triggers.

13. **§7.4 / §19 — LOC estimates are fabricated.** "~22 files", "+1800 / -300 LOC", "P1: ~800 LOC + ~400 LOC + ~250 LOC + ~30 test files", "PG ~200 LOC, SQLite ~250 LOC" appear repeatedly. Per `feedback_never_estimate`, drop or replace with structural counts that can be derived from the existing code (e.g. "every `&PostgresBackend` parameter site, currently N=… as of HEAD …"). If the count isn't measured, say "unknown".

14. **§10.5 — lock-key naming inconsistency.** §10.3 says `LockScope::GlobalApp { name: "register_model" }`; §10.5 says PG advisory uses `hashtext('zs_reg:' + app)::int4, hashtext('register_model')::int4` (which has a `zs_reg:` prefix not present in the `LockScope` `name`). Decide whether the prefix is applied by the LockManager impl or is part of the `name` field, and state it once.

15. **§13.2 — Metering composition contradicts §7.2.** §7.2 says `trait Metering: SqlExecutor` (so any backend is automatically also a Metering). §13.2 says "wraps SqlExecutor (§7)" — a decorator pattern (Metering owns an inner SqlExecutor). Both can't be true. Pick one and describe it in prose. The decorator pattern is the more flexible answer for keeping `Metering` shareable across backends; the sub-trait pattern is what's actually written in §7.2.

16. **§3 personas are inert.** Each persona gets one sentence then never appears again. Either tie scenarios in §3 to feature drivers in §4 (e.g. "Operator scenario X drives capability Y") or compress §3 into one paragraph.

17. **§14.2 — within-app multi-tenancy treats `org_id` as if it's free.** The SDK helper `db.scoped({ org_id })` injects WHERE clauses, but the doc says nothing about how the original schema declares which columns are scopable, what happens when the scope key is missing from a collection, or how this composes with reactive subscriptions (does a subscriber see events for other orgs they shouldn't?). The reactive-query interaction is the load-bearing question; §11 doesn't address it either.

### MINOR

18. **§1 — "Sits above: deferred backlog" lists [C1] but no other items.** Either list all the deferred items the doc closes (§9 names [I20], [I31]/F1, [I32]/F2) or drop the listing.

19. **§4 numbered list mixes capability descriptions with implementation hints.** Items 8-12 are short (one PG/SQLite line); items 1, 22 are paragraphs. Pick a row format and stick to it.

20. **§5.3 names "OrchestratorLockGuard" but §7.2 LockManager uses associated `type Guard`.** Two names for the same concept. Reconcile.

21. **§6.2 — Decimal `(p,s)` "lex-sortable rep" on SQLite needs one sentence explaining the chosen representation (zero-padded? sign-prefixed? scientific notation guarded?).** Otherwise this is a hidden implementation choice with correctness implications.

22. **§8.16 — "10-100× slower than PG COPY" is an estimate without measurement.** Drop the number; say "materially slower; acceptable at dev scale" or measure.

23. **§9 — "Mostly already shipped"** is fine framing, but the section then doesn't say which sub-traits are landed and which are pending. Add a one-line status per sub-trait or drop the section.

24. **§11.3 cross-isolate delivery via WAL** is plausible for PG but the doc never explicitly says "every worker process runs its own WAL consumer per app it has loaded" — meaning N workers × M loaded apps replication slots. Slot budget is a real PG constraint (`max_replication_slots`, default 10). Operational implication is missing.

25. **§16.3 metric names use both `db_pool_acquired` (snake) and `db_broker_fanout_latency_ms` (snake but with `_ms` suffix) — consistent.** But `db_sqlite_update_hook_latency_us` (microseconds) vs. broker (ms) — unit choice is design-relevant; either unify or document the rationale.

26. **§17.6 PG replication slot DoS section gestures at `dropAbandoned` and `replication.rs::watchdog` — these are implementation pointers, not design statements.** State the design intent ("inactive-slot detection on a configurable interval; thresholded reaping with operator alert") instead of file names.

27. **§19 P0-P6 phases never name a single integration test gate.** Each phase should name the test or set of tests that closes it. "New integration test suite mirroring PG (~30 test files)" in P1 is a LOC-style estimate, not a gate.

28. **§20 glossary — `dev tier` and `production tier` are defined; "hardening" / `--features hardening` is used in §5.9, §7.2 `SessionMinter`, §12, §17 but never glossed.** Add it.

---

## Missing Concepts

- **Connection lifecycle under V8 isolate eviction.** Worker LRU-evicts isolates; what happens to in-flight transactions, replication slots (PG), or open `rusqlite::Connection`s (SQLite) on eviction? Not addressed.
- **`Drop` ordering of guards across capability boundaries.** When an isolate panics mid-tx, who runs ROLLBACK, drops advisory locks, and clears `pending_emit`? `OrchestratorLockGuard` is named but the Drop contract isn't stated.
- **Schema-version coordination across worker fleet.** Multiple workers serve the same app; control-plane pushes a new model version; how do they converge? §10 is single-writer-centric.
- **`compio::runtime::spawn_blocking` pool sizing for SQLite.** §18 Q8 names "1 writer + 4 readers"; the blocking-thread-pool size in compio is a separate dimension. Not addressed.
- **Replication-slot lifecycle on app deletion.** §17.6 covers dead slots from crashed consumers; nothing on `drop_namespace` → must drop publication + slot, with what ordering against in-flight WAL.
- **PG `wal_level=logical` requirement.** Logical decoding needs `wal_level=logical`, `max_replication_slots`, `max_wal_senders` — production prerequisites. Doc doesn't enumerate.

---

## Overall Verdict

The doc has the right shape — 20 sections, reading order up front, scope discipline in §2, a useful PG↔SQLite capability matrix in §4 — and the capability-trait split is the right answer to deferred [C1]. But the abstraction-level discipline collapses precisely where the doc says the foundational refactor lives (§7), with eight trait-declaration code blocks and an embedded async fn body. The single biggest hidden cliff — `Rc<dyn Backend>` with associated types — is unaddressed. Several backend claims (SQLite shared-cache, `BEGIN EXCLUSIVE` semantics, `update_hook` new-tuple capture) need sharpening to be safely implementable. LOC and "10-100×" estimates violate the no-estimates-without-measurement rule. A revising pass that (a) replaces every trait declaration with prose-tables of method intent, (b) deletes the SQL/TS code blocks, (c) explicitly chooses static-generic vs dyn dispatch, (d) fixes the metering composition contradiction, and (e) strips LOC predictions would push this into the high-70s. Round-1 score reflects current state, not potential.
