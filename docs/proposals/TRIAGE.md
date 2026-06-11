# Proposals triage — auto-managed by /loop

Each loop cycle: pick 1-2 NOT-yet-done proposals, determine status against
current code + decisions/ + git history, then act:
- **SHIPPED w/ ADR** → the ADR + reference docs already capture it; move proposal to `docs/archive/` (git mv) with a one-line header noting where the live docs are.
- **SHIPPED w/o ADR** → if it holds reference-grade content not in `docs/reference/`, promote that content into the right reference doc (or a new one); then archive the proposal. If fully covered, just archive.
- **ABANDONED / superseded** → archive with a note (pre-launch: deletion is fine, but archive keeps the design rationale).
- **STILL ACTIVE (unshipped)** → leave in place; mark `[ACTIVE]` here.

Verify every claim against code before promoting. Reference docs must be accurate + helpful, repo-relative links, concise.

## Queue

### SHIPPED — has ADR in decisions/ (archive; content already in ADR + reference)
- [x] compression-streams-native.md → 2026-05-01-compression-streams-native ADR
- [x] headers-native.md → 2026-05-01-headers-native ADR
- [x] fetch-native.md → 2026-05-02-fetch-native ADR
- [x] streams-native.md → 2026-05-02-streams-native ADR
- [x] webcrypto-native.md → 2026-05-02-webcrypto-native ADR
- [x] websocket-native.md → 2026-05-02-websocket-native ADR (+ reference/websocket-design.md)
- [x] macro-constructor-post-init.md → 2026-05-04 ADR
- [x] macro-v8-state.md → 2026-05-04 ADR
- [x] node-crypto-native.md → 2026-05-05-node-crypto-native ADR

### SHIPPED — merged to main, no ADR (promote any reference-grade content, then archive)
- [x] nomad-driver-ch.md (merged dde08ca2; ops covered by runbooks/sandbox-nomad-ch.md)
- [x] sandbox-snapshot-restore.md (merged d1054adb)
- [x] sqlite-pg-parity.md (merged; reference/sqlite-divergences.md exists)
- [x] sandbox-pg-state.md (feat/sandbox-pg merged)
- [x] sandbox-preview-urls.md (sandbox preview merged)
- [x] kv-redesign-implementation-plan-2026-05-24.md (kv redesign; reference/kv.md exists)
- [x] zeroship-db.md (db SDK; reference/db.md exists)
- [x] db-system-design.md (db; reference/db.md)
- [x] platform-system-fields.md (db system fields)
- [x] sensitive-field-masking.md (db masking)
- [x] p0-implementation-plan.md (DB P0)
- [x] p1-sqlite-implementation-plan.md (DB P1)
- [x] p4-search-implementation-plan.md (DB P4)
- [x] p5-encryption-backup-implementation-plan.md (DB P5)
- [x] runtime-macros-refactor.md (runtime-macros; reference/plugin-system.md)
- [x] zs-standard-and-vite-v2.md (reference/zeroship-standard.md + vite-plugin.md exist)

### NEEDS DETERMINATION (check shipped vs active)
- [x] rpc.md [ACTIVE — kept]
- [x] plugins-workers-distributed.md [ACTIVE — kept]

### KEEP — living docs, not a ship-once proposal
- [x] feature-roadmap.md  [ACTIVE — keep in place]

## Done log
(loop appends: PROPOSAL — action — commit)

## Phase 2 — architecture docs improvement (after proposals queue drains, OR interleave 1/cycle)

The 7 architecture docs were refreshed for *accuracy* in the prior pass. This phase makes them *helpful*: clear entry narrative, accurate cross-links to reference/ + crate READMEs, fill explanation gaps, ensure each answers "how does this subsystem actually work + where's the code". Verify against current crates. Same rules: concise, repo-relative links, verify cited paths, no decisions/ edits, commit per file, never push.

- [x] docs/architecture/overview.md  — VERIFIED accurate (correct .zship naming, crate map, request/deploy paths match code); already a true entry point cross-linking the 6 + reference. No churn warranted.
- [x] docs/architecture/distributed.md — VERIFIED: pull-based 5s poll model, CHWBL proxy, deploy sequence all match code.
- [x] docs/architecture/gateway-routing.md — VERIFIED: CompiledManifest/EffectivePolicy/rpc_index/execute_resource_tree/ResolvedAction(WorkerRpc,WorkerSsr)/ProcedureKind(incl Subscription) all exist in crates/gateway+bundle.
- [x] docs/architecture/control-plane.md — VERIFIED: Registry::get_routes/get_versions, Manifest::passthrough, /internal/versions all exist.
- [x] docs/architecture/runtime.md — VERIFIED: call_fetch_handler, FetchOutcome, RuntimeBuilder::heap_limit_mb/idle_gc_after_ms, start_pump, src/{core,transport,web}/ layout all match.
- [x] docs/architecture/blob-store.md — VERIFIED: LocalDiskBlobStore, PutOutcome, put_blob_stream, blob.rs all exist.
- [x] docs/architecture/builder.md — VERIFIED: Backend docker/k8s/nomad-ch, nomad_ch.rs, sandbox routes match.

## Phase 3 — cross-cutting (optional, after Phase 2)
- [x] Ensure AGENTS.md task-router links all resolve — VERIFIED: every cited docs/crates/sdks path resolves; 4 broken docs/reference→proposals links repointed to docs/archive/ (c71a6a7a).
- [x] Verify docs/reference/ index in AGENTS.md matches docs/reference/ contents — DONE: added vite-plugin/runtime-limits/sqlite-divergences (fa7c7dcf); index now complete.

- 9 ADR-backed native-API proposals — archived to docs/archive/ + headers — 82adcefd (renames) + e8564b49 (headers)

- sqlite-pg-parity.md — 2 divergences promoted + archived

- sandbox-snapshot-restore.md + nomad-driver-ch.md — archived + runbook ops promotions (MemoryMaxMB, driver-behavior notes)

- 4 DB proposals archived; db.md promotions: strictness/t.ref/collection-names landed; System Fields + Encrypted/Masked CLOBBERED by concurrent write, re-promoting serially

- p0/p1/p4/p5 implementation plans — archived (no promotion needed; db.md covers)

- runtime-macros-refactor.md — archived (design in plugin-system.md + macro ADRs)

- zs-standard-and-vite-v2 + sandbox-pg-state — archived (shipped)

- sandbox-preview-urls + kv-redesign — archived (shipped)

- rpc.md — ACTIVE (Status: Proposal; RPC-v2 seamless-functions vision aspirational — /__zeroship/v1 dispatch primitive shipped but full codegen/declarative-gateway vision not). KEPT in docs/proposals/.
- plugins-workers-distributed.md — ACTIVE (Status: In progress; kv/storage distributed correctness + stateless-worker migration not yet shipped). KEPT in docs/proposals/.

## Phase 1 COMPLETE: 24 proposals archived, 2 ACTIVE (rpc, plugins-workers-distributed), 1 living (feature-roadmap).

## Phase 2 COMPLETE: all 7 architecture docs verified accurate+code-grounded against current crates (27 falsifiable symbol/endpoint/path claims checked, all OK; all bracket-links resolve; no abs-path leaks). No codex churn — docs were already accurate; "improving" verified-correct docs would only risk regressions.

## Phase 3 COMPLETE: AGENTS.md link audit done — all cited paths resolve, reference index complete, 4 stale reference→proposals links repointed to archive.

## TRIAGE FULLY DRAINED — 2026-05-25.

## Round 2 — 2026-06-11 (docs consolidation pass)

Six proposals accumulated since round 1; all triaged against code + git:

- auth-server.md — SHIPPED (crates/auth/ extracted 0de2091f; living docs reference/auth.md + auth-dev-tier.md + runbooks/auth-deploy.md) — **archived**, 18 inbound refs repointed.
- server-config-unification.md — SHIPPED (merged 966266bf; ADR 2026-05-28) — **archived**, ADR refs repointed.
- server-config-inventory.md — spent companion audit — **archived**.
- rpc-client-inferred-contracts.md — shipped subset, self-declared design history — **archived**.
- authorization.md — PARTIALLY SHIPPED: P9 platform RBAC live (crates/authz/, control integration); P12 end-user authz unbuilt — **kept** as the forward P12 spec.
- zeroship-builder-roadmap.md — LIVING — **kept** (broken specs/ paths fixed).

Standing residents of docs/proposals/: feature-roadmap.md (living), zeroship-builder-roadmap.md (living), rpc.md (active), plugins-workers-distributed.md (active; kv leg shipped 5b17dc39), authorization.md (P12 active), TRIAGE.md (this worklist).

Same pass also: archived docs/perf/ + docs/benchmarks/ → archive/, retired docs/briefs/ (74 merged UI work orders deleted, 7 strategy briefs archived), archived 18 completed superpowers specs/plans, archived 10 resolved review reports (reviews/ keeps only the live audit ledger + freshest review).

## Phase 2 ENHANCEMENT pass (beyond verify) — 2026-05-25: all 7 architecture docs got a "Read next"/"Related docs" navigation footer (repo-root-relative cross-links to siblings + relevant reference contracts) plus 1-2 code-grounded clarifying sentences each (CHWBL + polling tradeoff in distributed; compile-step rationale + `*` inheritance root in gateway-routing; master-key vs control-key + polling in control-plane; thread-bound isolate JIT enter/exit + pump event-batching in runtime, verified vs runtime.rs:1044; global-by-hash dedup in blob-store). Mix of lean codex agents (gateway-routing/control-plane/runtime) and direct edits (overview/distributed/blob-store/builder; codex companion dropped several jobs mid-run). All verified: links resolve, fences balanced, single H1, no abs paths. Commits cf1cf952, e9130c87, ec0bd0c7, c6c0188e, f385dfe3, e0c3467b, + runtime.
