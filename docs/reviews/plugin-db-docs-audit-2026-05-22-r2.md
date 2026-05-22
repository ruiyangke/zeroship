# plugin-db Documentation Accuracy Audit — 2026-05-22 r2

**Commit:** `4d80651e`
**Scope:** inline rustdoc across `crates/plugin-db/src/`, `docs/reference/db.md`, `docs/proposals/zeroship-db.md`, `AGENTS.md` task router, `sdks/bootstrap/README.md`. Re-audited fresh against r1.

---

## 1. Headline

The high-visibility public-doc drift identified in r1 (`docs/reference/db.md` "TX_CONN thread-local") and the `callbacks.rs` proposal references are cleanly closed by `d53f90b0`. The remaining drift is concentrated in inline source comments — the [I26] sweep (9 known stale TX_CONN sites) is still open, and several adjacent drift classes surface in this round that were not part of r1's lens:

- One **CRITICAL false statement** in `error.rs:9-12` ("lone hold-out") that's contradicted by ~30 surviving `Result<_, String>` return-types across `auth/`, `replication.rs`, `diff.rs`, `lib.rs`, `v8_classes/migration.rs`.
- One **CRITICAL broken docs-link** in `docs/reference/db.md:90` pointing at `crates/runtime/src/bootstrap/db_init.js`, a path that does not exist — the real entry is `sdks/bootstrap/src/runtime-entry.ts`.
- Two **IMPORTANT stale API-name references** in `wal_consumer.rs:49` (`replicationConsumerStart` → actual name `startReplicationConsumer`) and `v8_classes/migration.rs:216` / `lib.rs:216` (`MIG_LOCK` shorthand for what is now `IsolateDbContext::mig_lock`).
- One **IMPORTANT broken AGENTS.md task-router link** (`docs/runbooks/docker-compose.md` does not exist).
- The 9 [I26] stale TX_CONN/TX_TOKEN sites (all rated IMPORTANT — newcomer-confusion, not production risk).
- Two cross-crate **IMPORTANT** stale references in `crates/runtime/src/rpc/capability.rs:18,25` pointing at deleted `plugin-db/src/callbacks.rs` and `runtime/src/bootstrap/rpc_dispatch.js`.

Module preambles are universally present (32/32 files in `crates/plugin-db/src/`). The single TODO in the crate (`query.rs:443`) is genuinely outstanding.

---

## 2. Findings

### 2.1 Type lies — error.rs "lone hold-out" claim

[CRITICAL] `crates/plugin-db/src/error.rs:9-14` — Preamble says "The lone hold-out is the `validate` stage in `crate::orchestrator::register_model`, which still returns `Result<_, String>` because its `Err` is the `validation_refused` JSON envelope … the typed-error invariant still holds at every public surface."
  Why: A reader of the crate's top-level error contract will believe every other helper has been migrated. In reality ~30 sites still return `Result<_, String>` across:
  - `auth/bootstrap.rs`: 15 sites (lines 52, 169, 187, 225, 272, 316, 347, 414, 494, 599, 630, 696, 965)
  - `auth/keys.rs`: 3 sites (54, 86, 113)
  - `auth/session.rs`: 6 sites (75, 152, 206, 219, 314, 328)
  - `replication.rs`: 7 sites (81, 97, 102, 147, 314, 411, 496)
  - `diff.rs`: 3 sites (184, 355, 380)
  - `v8_classes/migration.rs`: 2 sites (454, 742)
  - `v8_classes/migrations.rs`: 1 site (221)
  - `lib.rs`: 1 site (349 — `init_pool_async`)

  The deferred backlog [I28] explicitly tracks this gap ("~50 Result<_, String> sites in replication.rs + auth/bootstrap.rs"). The preamble's "lone hold-out" + "typed-error invariant still holds at every public surface" is therefore not just stale framing but a substantively false statement about the current code shape.
  Fix: Rewrite §"lone hold-out" to enumerate the remaining un-migrated submodules (`auth/*`, `replication.rs`, `diff.rs`, the two v8_classes/migration sites) and the `init_pool_async` entrypoint, with forward-pointer to [I28]. Either widen the "invariant holds" claim's qualifier or drop the claim.
  Verification: `rg -n 'Result<.*, String>' crates/plugin-db/src/ | wc -l` (currently 38 hits inc. signature splits); the count should match the new preamble or trend to zero as [I28] closes.

[IMPORTANT] `crates/plugin-db/src/error.rs:241-246` — `into_string()` doc says "Used as a bridge while the conversion sweep proceeds; new code should prefer `to_op_error()`."
  Why: Present continuous "while the conversion sweep proceeds" reads as active migration, but the sweep stalled when [I28] was deferred. The function is the load-bearing bridge for [I28]'s 38-call-site backlog, not a transitional helper.
  Fix: "Bridges the `Result<_, String>` rail used by the un-migrated paths enumerated in the module preamble (see [I28] in `plugin-db-deferred.md`); new code should prefer `to_op_error()`."
  Verification: `rg -n 'into_string' crates/plugin-db/src/ | wc -l`.

### 2.2 Broken or stale path references in public docs

[CRITICAL] `docs/reference/db.md:90` — "The runtime's bootstrap (`crates/runtime/src/bootstrap/db_init.js`) reads `default.schema` directly off the loaded entry."
  Why: Path does not exist on HEAD. The actual production splice site is `sdks/bootstrap/src/runtime-entry.ts` (TS), compiled to `sdks/bootstrap/dist/runtime-entry.js` and `include_str!`ed by `crates/runtime/src/core/init.rs:273`. Anyone clicking through will fail to find the file; anyone grepping `db_init.js` will find nothing.
  Fix: Replace with: "The runtime's bootstrap orchestrator (`sdks/bootstrap/src/runtime-entry.ts`, embedded via `include_str!` from `crates/runtime/src/core/init.rs::DB_INIT_JS`) reads `default.schema` directly off the loaded entry."
  Verification: `ls crates/runtime/src/bootstrap/` → ENOENT; `grep -n DB_INIT_JS crates/runtime/src/core/init.rs`.

[IMPORTANT] `AGENTS.md:28` — Task-router row "Multi-node / Docker Compose" points to `docs/runbooks/docker-compose.md`.
  Why: File does not exist. Actual runbooks: `local-dev.md`, `local-k3s-crun-krun.md`, `sandbox-agent.md`, `sandbox-nomad-ch.md`. The compose flow described in the root README/AGENTS.md (`docker compose up -d --scale worker=10`) has no runbook backing it.
  Fix: Either rename/create `docs/runbooks/docker-compose.md`, or repoint the task-router row to `docs/runbooks/local-k3s-crun-krun.md` if that's the new canonical local multi-node story, or remove the row.
  Verification: `ls docs/runbooks/docker-compose.md` → ENOENT.

[IMPORTANT] `crates/runtime/src/rpc/capability.rs:18` — "`crates/plugin-db/src/callbacks.rs` — write callbacks refuse when `current_kind() == Some(Query)`."
  Why: `callbacks.rs` was deleted in Stage 8b. Write callbacks now live in `crates/plugin-db/src/crud.rs` (`dispatch_insert` / `dispatch_update` / `dispatch_delete`) which call into `crates/plugin-db/src/v8_classes/collection.rs`. This is a cross-crate stale ref to plugin-db that escaped r1's plugin-db-only scope and the cycle-01:10 sweep.
  Fix: Replace with: "`crates/plugin-db/src/v8_classes/collection.rs` write methods (`insert`, `update*`, `delete*`) refuse via `refuse_if_query_capability` when `current_kind() == Some(Query)`."
  Verification: `rg -n 'callbacks\.rs' crates/runtime/src/`.

[IMPORTANT] `crates/runtime/src/rpc/capability.rs:25` — "The runtime's `__zsDispatch` (`crates/runtime/src/bootstrap/rpc_dispatch.js`) knows the procedure `kind`…"
  Why: That path does not exist. Real path is `sdks/bootstrap/src/dispatcher.ts` (per `core/init.rs:291` `RPC_DISPATCH_JS`).
  Fix: Replace with: "`sdks/bootstrap/src/dispatcher.ts` (embedded via `crates/runtime/src/core/init.rs::RPC_DISPATCH_JS`)".
  Verification: `ls crates/runtime/src/bootstrap/rpc_dispatch.js` → ENOENT.

### 2.3 [I26] stale TX_CONN / TX_TOKEN refs — re-verification

All 9 sites confirmed at HEAD; one site is slightly off from the backlog's line number (`exec.rs:329`, not `exec.rs:277`). Severity: IMPORTANT (newcomer-confusion; broken intra-doc-link risk because `crate::TX_CONN` / `crate::TX_TOKEN` are not exported anywhere — they don't resolve, but `cargo doc` doesn't complain because the `[crate::TX_CONN]` references appear inside non-public items where the broken-link lint is downgraded).

| # | File:Line | Stale text | Suggested fix |
|---|-----------|------------|---------------|
| 1 | `crates/plugin-db/src/v8_classes/transaction.rs:68` | `Ownership token stamped into [\`crate::TX_TOKEN\`] at successful BEGIN` | `IsolateDbContext::tx_token` |
| 2 | `crates/plugin-db/src/v8_classes/transaction.rs:69` | `Commit / rollback / GC compare to the current TX_TOKEN` | `compare to IsolateDbContext::tx_token` |
| 3 | `crates/plugin-db/src/v8_classes/transaction.rs:71` | `TX_TOKEN, the others see the mismatch` | `the current token, the others see…` |
| 4 | `crates/plugin-db/src/v8_classes/transaction.rs:107-108` | `If our token still matches the current TX_TOKEN, this wrapper is the live owner of TX_CONN` | `…IsolateDbContext::tx_token, this wrapper is the live owner of IsolateDbContext::tx_conn` |
| 5 | `crates/plugin-db/src/v8_classes/transaction.rs:116` | `auto-tx wrapper repurposed TX_CONN — we leave TX_CONN alone` | `…repurposed the tx slot — we leave it alone` |
| 6 | `crates/plugin-db/src/v8_classes/transaction.rs:161` | `each CRUD callback consults [\`crate::TX_CONN\`] via run_sql` | `consults IsolateDbContext::tx_conn via run_sql` |
| 7 | `crates/plugin-db/src/v8_classes/transaction.rs:192` | `clear [\`crate::TX_CONN\`], and mark this wrapper settled` | `clear IsolateDbContext::tx_conn` |
| 8 | `crates/plugin-db/src/v8_classes/transaction.rs:214` | `then clear [\`crate::TX_CONN\`] / [\`crate::TX_TOKEN\`]` | `then clear IsolateDbContext::tx_conn / tx_token` |
| 9 | `crates/plugin-db/src/v8_classes/transaction.rs:218` | `or \`TX_TOKEN\` has been claimed` | `or the tx_token slot has been claimed` |
| 10 | `crates/plugin-db/src/v8_classes/transaction.rs:278` | `The matching \`TX_TOKEN\` write happens in…` | `The matching IsolateDbContext::tx_token write…` |
| 11 | `crates/plugin-db/src/v8_classes/transaction.rs:284-285` | `Caller invariant: TX_CONN has just been set by a successful BEGIN and no other Transaction wrapper is alive for the same TX_CONN` | `…IsolateDbContext::tx_conn… same connection` |
| 12 | `crates/plugin-db/src/v8_classes/transaction.rs:324` | `above checks the current TX_TOKEN and auto-rollbacks` | `IsolateDbContext::tx_token` |
| 13 | `crates/plugin-db/src/crud.rs:53` | `exec runs against either the pool or the active TX_CONN` | `the active IsolateDbContext::tx_conn` |
| 14 | `crates/plugin-db/src/exec.rs:329` (backlog says `:277`) | `responsible for setting \`TX_CONN\` (via [crate::install_tx_marker_for_tests])` | `setting IsolateDbContext::tx_conn (via [crate::install_tx_marker_for_tests])` |
| 15 | `crates/plugin-db/src/orchestrator/transaction.rs:143` | `store the Client in TX_CONN` | `store the Client in IsolateDbContext::tx_conn` |
| 16 | `crates/plugin-db/src/orchestrator/transaction.rs:51-52` | `On BEGIN success the future stamps TX_TOKEN with this same token; on failure TX_TOKEN stays 0…` | `…stamps IsolateDbContext::tx_token… stays 0` |
| 17 | `crates/plugin-db/src/orchestrator/transaction.rs:89` | `Wrapper's \`token\` never matches TX_TOKEN(=0)` | `…IsolateDbContext::tx_token(=0)` |

Count: 17 stale tokens across 4 files (the backlog estimate of "9 sites" undercounts because some sites have multiple tokens on adjacent lines). Net unchanged from r1's #5/#6/#7 finding — the cycle-01:10 sweep deliberately deferred all of these as out-of-scope-of-the-8-site-scope-guard.

  Why: Confuses readers searching the codebase for `TX_CONN` / `TX_TOKEN` (they will find no definition); contributes ambient drift between in-source comments and the now-typed `IsolateDbContext` model. Not production-risk.
  Fix: Single mechanical sweep — replace each token with its `IsolateDbContext::*` counterpart (mapping above). Drop the `[\`crate::TX_CONN\`]` / `[\`crate::TX_TOKEN\`]` rustdoc link braces since the symbols no longer exist as crate-level items.
  Verification: `rg -n 'TX_CONN|TX_TOKEN' crates/plugin-db/src/ | grep -v "//! .* formerly" | grep -v context.rs` should return zero non-historical hits.

### 2.4 Other retired-name shorthand

[IMPORTANT] `crates/plugin-db/src/v8_classes/migration.rs:216` — "Delegates to `exec_fetch_batch` (which itself checks the `MIG_LOCK` thread-local for ownership / cancellation)."
  Why: `MIG_LOCK` is no longer a thread-local; it's `IsolateDbContext::mig_lock` per `context.rs:355` ("MIG_LOCK state machine"). Same shape as the TX_CONN drift — retired standalone name still used as shorthand.
  Fix: "checks the `IsolateDbContext::mig_lock` slot for ownership / cancellation".
  Verification: `rg -n 'MIG_LOCK' crates/plugin-db/src/v8_classes/`.

[IMPORTANT] `crates/plugin-db/src/lib.rs:216` — Test-only helper doc: "clear `MIG_LOCK` for the current thread."
  Why: Same as above. The helper actually drops `IsolateDbContext::mig_lock` (see body — `take_mig_client`, `migrations::release_active_lock`).
  Fix: "clear the `IsolateDbContext::mig_lock` slot for the current thread (formerly the `MIG_LOCK` thread-local, folded into `IsolateDbContext` in Stage 8d-R4)".
  Verification: `rg -n 'MIG_LOCK' crates/plugin-db/src/lib.rs`.

### 2.5 Stale API names (not retired internals — wrong user-visible name)

[IMPORTANT] `crates/plugin-db/src/wal_consumer.rs:48-52` — "the V8 callback that spawns it is opt-in: apps call `db.replicationConsumerStart()` to enable cross-worker propagation. Spawning automatically on isolate boot is one `r.add("replicationConsumerStart", …)` + a callback away in `replication_ops.rs`"
  Why: The user-visible API is now `db.startReplicationConsumer()` (per `v8_classes/db.rs:242` `#[v8_name = "startReplicationConsumer"]` and `replication_ops.rs:163`). It is also no longer a `r.add(…)` flat callback — it's a `#[v8_method]` on the `Db` v8_class instance. Both pieces of guidance are wrong.
  Fix: "apps call `db.startReplicationConsumer()` (the `#[v8_method]` on the `Db` v8_class in `v8_classes/db.rs`) to enable cross-worker propagation. Automatic spawn on isolate boot would require wiring through `DbPlugin::build_instance` in `lib.rs` to call `start_replication_consumer_dispatch` after pool init."
  Verification: `rg -n 'replicationConsumerStart' crates/plugin-db/` should return zero hits after fix; `rg -n 'startReplicationConsumer' crates/plugin-db/` returns 5 production hits.

### 2.6 Tense drift / migration-narrative framing

[MINOR] `crates/plugin-db/src/context.rs:27-29` — "The fields stay `pub(crate)` so the lib.rs shim thread-locals can be removed slot-by-slot in subsequent commits without churn."
  Why: Reads as forward-looking but the shim removal is complete — `grep -n thread_local crates/plugin-db/src/lib.rs` returns zero hits. The Stage 8d-R4 consolidation has fully landed.
  Fix: "The fields stay `pub(crate)` so the consolidation could be done slot-by-slot (now complete — `lib.rs` carries no thread_locals as of Stage 8d-R4)."
  Verification: `rg -n 'thread_local' crates/plugin-db/src/lib.rs` (currently zero).

[MINOR] `docs/proposals/zeroship-db.md:604` — "Implementation: `exec_register_model` writes one row per DDL operation it runs."
  Why: The proposal's A3 implementation pointer names `exec_register_model` without the `callbacks.rs::` prefix scrubbed at lines 76 and 196. The function name now lives as `exec_register_model_with_pool` in `orchestrator/register_model/mod.rs`. Not a structural lie (the function does exist with a near-identical name) but inconsistent with the scrub the previous cycle did on the two prefixed callsites.
  Fix: Either annotate with `<!-- superseded reference; see register_model_dispatch + run_pipeline in orchestrator/register_model/mod.rs -->` or update to the current name.
  Verification: `rg -n 'exec_register_model' docs/`.

### 2.7 Bootstrap README accuracy (`sdks/bootstrap/README.md`)

[MINOR] `sdks/bootstrap/README.md:24` — "Helpers: `model()`, `validateRefTargets()`, `topoSortByRefs()`, `normalizeSchema()`."
  Why: `topoSortByRefs` is not exported from `install-schema.ts` (it's a file-private `function topoSortByRefs(...)` at line 392). The other three are real `export function` declarations. Listing a private helper alongside the exports misleads anyone hunting for the symbol.
  Fix: Drop `topoSortByRefs()` from the helpers list, or document it explicitly as private (`internal: topoSortByRefs`).
  Verification: `rg -n '^export function' sdks/bootstrap/src/install-schema.ts` returns 5 exports; `topoSortByRefs` not among them.

The rest of the bootstrap README is accurate: `installSchema` return type `{ collections, ready }` matches the source at line 679; `createFetchHandler` matches `fetch-handler.ts:47`; the `_zs/v1/<id>` routing claim matches the dispatcher logic in `fetch-handler.ts:51`. The build-ordering claim ("`pnpm -F @zeroship/bootstrap build` MUST run before `cargo build -p zeroship-runtime`") matches the `include_str!` reality of `core/init.rs:273,291`.

### 2.8 Module preambles

All 32 files under `crates/plugin-db/src/` (including subdirs `v8_classes/`, `orchestrator/`, `orchestrator/register_model/`, `auth/`, `backend/`) carry a `//!` preamble at the top. The r1 gaps (`crud.rs`, `diff.rs`, `replication_ops.rs`) are all closed (the crud.rs and diff.rs preambles landed in commit `29b8a013`, replication_ops.rs in an earlier sweep).

No new preamble gaps.

### 2.9 Orphan TODO / FIXME

Single TODO in the crate: `crates/plugin-db/src/query.rs:443` — "TODO: A1 composite indexes — wire through `schema_meta.indexes` once the SDK builder exists." Genuinely outstanding (composite-index SDK builder is unshipped). Accurate, not orphan.

No FIXME / XXX / HACK markers in plugin-db source.

### 2.10 AGENTS.md task router

Beyond the broken `docs/runbooks/docker-compose.md` link (covered in 2.2), every row's path was verified:

- `docs/architecture/{gateway-routing,runtime,control-plane,blob-store,builder,distributed,overview}.md` — all exist
- `crates/gateway/src/router/dispatch.rs` — exists
- `crates/bundle/src/{manifest,rule}.rs` — both exist
- `crates/control/src/api.rs` / `registry.rs` — both exist
- `docs/reference/{plugin-system,db,zship,auth,zs-standard,billing-metering,websocket-design,vite-plugin,vite-environment-api,node-compat,zerobench}.md` — all exist
- `sdks/bootstrap/src/{dispatcher,runtime-entry}.ts` — both exist
- `sdks/bootstrap/README.md` — exists
- `crates/sandbox/src/backend/nomad_ch.rs` — exists
- `crates/sandbox/scripts/nomad-vm-wrapper.sh` — exists
- `docs/runbooks/{local-dev,sandbox-nomad-ch}.md` — exist
- `docs/research/ai-builder-features.md` — exists

Net: AGENTS.md is accurate modulo the single broken Docker Compose runbook reference.

---

## 3. Summary table

| Severity | Count |
|----------|-------|
| CRITICAL | 2 (error.rs "lone hold-out" + db.md broken bootstrap path) |
| IMPORTANT | 7 (5 stale-name classes + 2 cross-crate capability.rs refs + AGENTS.md broken link) |
| MINOR | 3 (context.rs forward-looking tense, proposal §A3 line 604, bootstrap README helpers list) |
| [I26] sites | 17 token replacements across 4 files (rolled up as one IMPORTANT) |

Drift trend vs. r1:
- r1's #1, #2 (db.md TX_CONN public-doc claims) — **closed**.
- r1's #3, #4 (zeroship-db.md callbacks.rs proposal refs) — **closed via `<!-- superseded -->` annotations**.
- r1's #5, #6, #7 (lib.rs, orchestrator/mod.rs, v8_classes/mod.rs TX_CONN/TX_TOKEN shorthand) — **closed**.
- r1's #8 (validate.rs `Result<_, String>` "future hook") — **verified accurate; no edit was needed and `d53f90b0` correctly skipped it**.
- r1's #9 (error.rs "is being migrated") — **closed** … BUT the rewrite introduced a new false statement ("lone hold-out") that's worse than the original tense issue (r2 §2.1 CRITICAL).
- r1's #10, #11 (v8_bridge.rs / exec.rs `Result<_, String>` framing) — **partially closed**; v8_bridge.rs:321 is now accurate as a deprecation guardrail; exec.rs:26 is accurate.
- New surface area uncovered in r2:
  - db.md:90 broken path (CRITICAL) — missed by r1's lens
  - cross-crate refs in `runtime/src/rpc/capability.rs` (2 sites) — out of r1's plugin-db-only scope
  - wal_consumer.rs API-name drift (`replicationConsumerStart`) — missed by r1
  - migration.rs:216 + lib.rs:216 `MIG_LOCK` shorthand — missed by r1
  - bootstrap README `topoSortByRefs` claim — out of r1's scope
  - AGENTS.md broken `docker-compose.md` link — out of r1's scope

---

## 4. Score

**74 / 100** (r1: 68 / 100, +6)

The score improves because the public-facing `docs/reference/db.md` is now fully corrected on the TX_CONN front, the `docs/proposals/zeroship-db.md` callbacks.rs lies carry inline `<!-- superseded -->` markers, and every file has a module preamble. The score is held back by:

- One genuinely new CRITICAL false statement introduced by the r1-driven rewrite of `error.rs` ("lone hold-out") — the rewrite traded a present-tense framing issue for a load-bearing factual error about the surface area of `Result<_, String>` returns. (-8)
- One CRITICAL broken path in the public reference doc (`db.md:90` → nonexistent `crates/runtime/src/bootstrap/db_init.js`). (-4)
- The 17 TX_CONN/TX_TOKEN shorthand sites the cycle-01:10 sweep deferred remain in production rustdoc and will read as broken intra-doc-links to anyone trying to `Click → Go to Definition` on `[crate::TX_CONN]`. (-4)
- Two cross-crate stale refs in `runtime/src/rpc/capability.rs` re-naming plugin-db files that no longer exist. (-3)
- One broken AGENTS.md task-router entry + assorted MINOR drift. (-2)

If both CRITICAL findings (2.1 error.rs + 2.2 db.md:90) plus the [I26] sweep land cleanly, the next round should score ~88 / 100. The remaining drift is the [I28] `Result<_, String>` migration backlog itself — once that closes, the error.rs preamble can be honestly absolute and the score breaks 90.
