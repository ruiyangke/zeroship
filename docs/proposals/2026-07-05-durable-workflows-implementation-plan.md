# Durable workflows — implementation plan (approved design → shipped & launched)

- **Status:** PLAN (companion to `2026-07-05-durable-workflows-design.md`; commits with the implementing PR-train per `feedback_proposal_workflow`).
- **Date:** 2026-07-05
- **Design base:** the dual-reviewed design doc. Section references below use the **design doc's own numbering** (§15 build plan, §16 residual limits & open questions, §17 blob rail, §18 ingress/broadcast, §20 children, §21 compensation, §22 restart).
- **Scope stance:** **full-featured day-1** — every feature in the design ships in this train. No v1/v2 deferral. Pre-launch, no back-compat: DDL and wire shapes land once, in the create scripts, complete.
- **No fabricated numbers.** Work is sized by relative complexity (S/M/L/XL) + risk (Low/Med/High). Anything gated on a measurement or spike is marked **to-measure** — most prominently the capacity-measurement gate (DW-23) that seeds every §13 numeric default.

---

## 1. Executive summary

We are building a **replay-per-dispatch durable-workflow engine**: creators author `Workflow<Params, Output>` classes in TypeScript; the control plane owns a Postgres journal (`zeroship.workflow_*`, 8 tables) and a compio dispatch scheduler; the worker replays the workflow function in the app's V8 isolate against the journal prefix and returns a dispatch-completion envelope; the control plane commits one atomic, lease-guarded txn per dispatch. Day-1 the engine ships the full surface: **concurrent frontier execution, sleep/sleepUntil/signals, blob-backed & streaming outputs, external signal ingress + topic broadcast, child orchestration + `startMany`, compensation/saga rollback, schedules + the fluent DSL, and `run.restart`** — all riding one dispatch loop, one commit txn shape, and one sweep family.

**Shape of the effort:** ~24 PRs in 5 milestones. A **sequenced spine** builds the core (typed ids → DDL → SDK/bootstrap shim → scheduler → worker replay host → single-frontier replay → concurrent frontier → suspension → controls/restart → hardening); after the spine stabilizes, **four feature rails fan out in parallel worktrees** (schedules, blob rail, ingress/broadcast, dev-tier) because they are file-disjoint, while the two rails that edit the replay shim + commit-txn fold (children, compensation) stay **sequenced on the spine**. The train closes with chaos/adversarial testing, retention GC, docs, the capacity-measurement gate that sets tier defaults, and rollout scaffolding (false-by-default flag + two kill-switches).

**Critical path in one paragraph:** SDK-first is structural — the runtime crate `include_str!`s `sdks/bootstrap/dist/*`, so the bootstrap workflow-dispatch entry + `@zeroship/workflows` shim (DW-03) must build before any Rust that consumes them; the journal DDL (DW-02) must be **complete on day one** (every feature's columns, no later `ALTER`) and precedes the scheduler (DW-04); the scheduler precedes the worker replay host + gateway dispatch edge (DW-05, itself blocked on the **G1 isolate-pinning go/no-go**); the replay host precedes the single-frontier core (DW-06), which precedes the concurrent frontier (DW-08), which precedes suspension (DW-09), controls/restart (DW-10), and the fold-refinement hardening (DW-13) that compensation requires. Everything else — schedules, blobs, ingress, dev-tier, metering, docs — hangs off that spine and can run beside it.

---

## 2. Milestones

Each milestone has hard exit criteria. Nothing advances on "looks done" — exit criteria are green tests + verified commits.

### M0 — Foundations (DW-00 … DW-03)

Ids, schema, SDK skeleton. No engine yet.

**Exit criteria:**
- All w-family typed ids (`run_`, `sig_`, `sch_`, `wfd_`, `wsk_`, `wbc_`, `wsb_`) + the stateless `wst_` token codec exist in `crates/zeroship-core/src/typed_id.rs` with pairwise-disjointness tests (explicitly asserting `wfd_ ≠ dsp_`, `wsb_ ≠ sub_`).
- The **complete** `zeroship.workflow_*` create scripts (all 8 tables, every day-1 column group: concurrency/batch, output-representation, compensation, restart audit, parent-edge, signal provenance/`signal_epoch`, schedule registry) apply clean on the live `:5440` PG; DB-role test proves `zeroship_gateway`/`zeroship_worker` have **no journal-write grant**.
- `@zeroship/workflows` + the bootstrap workflow entry build in the pnpm graph; `pnpm build && cargo build -p zeroship-runtime` passes (the `include_str!` contract holds).

### G1 — operator go/no-go: isolate pinning (blocks DW-05, hence all of M1+)

See §8 (decision gates). Decided **before** M1 starts.

### M1 — Walking skeleton (DW-04 … DW-07)

A real deployed workflow runs end-to-end: control claims → gateway forwards → worker replays pinned deploy → control commits.

**Exit criteria (the keystone e2e):**
- A real `.zship`-deployed 3-step workflow, started via the control-plane API, completes across (a) a `kill -9` of the worker mid-run and (b) a control-plane restart — on the real path (live control + gateway + worker + PG `:5440`), no shims.
- Journal shows exactly-once step rows (PK + `ON CONFLICT DO NOTHING` verified under a forced lease handoff).
- A redeploy mid-run does not change the in-flight run's behavior (deploy-pinning e2e); the pinned deploy's bundle is **not GC-eligible** while the run is live (retention guard test).
- Zero tokio audit passes (dependency check on all touched crates).

### M2 — Full core semantics (DW-08 … DW-13)

Concurrency, suspension, controls, restart, metering, determinism/liveness — the complete §5/§6/§8/§9/§10/§11 surface.

**Exit criteria:**
- Concurrent frontier: crash-mid-frontier commits zero-of-N and re-runs all N; `effN = 1` produces a byte-identical journal to the single-frontier baseline (§6.6/§11 C8 equivalence test); mixed `step.all` (run + sleep + waitForSignal) resolves across ≥3 dispatches.
- `step.sleepUntil` is never woken early (DB-clock one-sided guarantee test); `MIN(pending)` re-eval-all wake proven under two pending branches.
- The full DW-10 restart e2e list (design §15 PR 6) green: partial restart retains prefix with no `NondeterministicError`; live-dispatch eviction via `claim_epoch`; `deploy:"latest"` + `from` rejected; terminal revive; creator cap 429 / operator exempt+audited; blob refcount decrement on drop.
- Metering: one dispatch = one metered unit; a spend-**Block**ed app's dispatch 402s at the edge; counters are app-unforgeable (no JS handle).
- Determinism guards: a bare non-`step` `await` fails `NondeterministicError`; `stuck_strikes` → `StalledError`; wall-budget rollover leaves `UNSETTLED` steps unjournaled and re-run; the §9 re-dispatch-to-observe-catch fold lands (prerequisite for compensation).

### M3 — Feature rails, all day-1 features (DW-14 … DW-18, DW-21)

Schedules, blob rail, ingress/broadcast, children/startMany, compensation, dev-tier.

**Exit criteria:** every per-feature faithful-e2e list from design §15 (PRs 9, 11, 12, 13, 14) green on the real path — enumerated per-PR in §3 below. Plus: the dev-tier mini-engine runs the golden-path workflow example locally with contract parity.

### M4 — Launch-ready (DW-19 … DW-24)

Cross-feature chaos, retention, docs, measurement, rollout scaffolding.

**Exit criteria:**
- Chaos/adversarial suite green: crash-mid-frontier, lease-handoff double-commit, crash-mid-fan-out, crash-mid-compensator, crash-mid-blob-write, restart-during-compensation rejection, cascade under redeploy.
- Terminal-run retention sweep live (leaf-up, restart-window-aware) — journal growth is bounded by policy, not by hope (**G4** decided).
- Capacity measurement (DW-23) executed; every §13 knob seeded **from a measurement**, not a guess (**G3** satisfied).
- `docs/reference/workflows.md` + AGENTS.md router row + starter example shipped; the §6.3 two-line semantic contract (intra-frontier unordered; effects at-least-once) is prominent in creator docs.
- Both kill-switches exist and have been **drill-tested** (flip on under load, verify safe freeze / ingress 503, flip off, verify resume with no lost runs).
- Dual-review (critic + reviser) completed for every High-risk PR; every fix in the train carries its regression test.

### M-launch — staged enablement (post-M4, §9 below)

**Exit criteria:** dogfood soak clean → cohort soak clean → GA checklist signed. Enablement flag defaults stay false until each gate passes.

---

## 3. Work breakdown — the PR-train

Legend: **Kind** = DDL / Rust / JS / Mixed / Docs / Ops. **Size** = S/M/L/XL (relative complexity). **Risk** = Low/Med/High. Every PR: commit-only (never push), dual-review for Med+ risk, regression-test-per-fix, and the implementing agent must **self-verify** (run the stated gate, grep for its own artifacts) — never punt to a background job and return empty (`feedback_workflow_implement_must_not_punt`).

Design-§15 seed mapping is noted as “(seed N)”.

### M0 — Foundations

| id | title | kind | size | risk |
|---|---|---|---|---|
| **DW-00** | Commit design doc + this plan | Docs | S | Low |
| **DW-01** | w-family typed ids + core wire types (seed 1, part) | Rust | S | Low |
| **DW-02** | Complete `zeroship.workflow_*` journal DDL (seed 1) | DDL | M | Med |
| **DW-03** | `@zeroship/workflows` SDK skeleton + bootstrap workflow entry | JS (+S Rust) | L | Med |

- **DW-00** — scope: land the two proposal docs (per `feedback_proposal_workflow` they commit with the train's opening PR). Deps: none. Gate: n/a. Verify: files present, links resolve.
- **DW-01** — scope: `run_`/`sig_`/`sch_`/`wfd_`/`wsk_`/`wbc_`/`wsb_` prefixes in `crates/zeroship-core/src/typed_id.rs`; `wst_` token codec skeleton; the control↔worker **dispatch-completion envelope** wire types (frontier outcomes + blob refs + subscription/consumption requests, §4) in `crates/core`. Deps: none. Test gate: prefix disjointness unit tests incl. `wfd_`≠`dsp_`, `wsb_`≠`sub_`; envelope serde round-trip. Verify: `cargo test -p zeroship-core`.
- **DW-02** — scope: create scripts for `workflow_runs` / `_steps` / `_signals` / `_blobs` / `_signal_keys` / `_broadcasts` / `_subscriptions` / `_schedules` with **every** day-1 column group already present (§7.1–§7.9): ordinal PK, `batch_id`/`batch_width`, `concurrency`/`next_ordinal`/`stuck_strikes`, output-representation (`output_kind`/`output_hash`/`output_size`/`output_content_type` + blob-input columns + `journal_bytes`/`blob_bytes`), compensation columns + `compensating` in the wake index, restart audit (`restart_count`/`restarted_at`/`restarted_from_ordinal`/`restarted_by`), parent-edge (`parent_run_id` ON DELETE RESTRICT, `tree_depth`, `cancel_requested`, `child_run_id`, `kind='child'`), signal provenance + `signal_epoch`, schedule registry. DB roles: gateway/worker get **no** write grant. Deps: DW-01. Test gate: apply-clean on live PG `:5440`; CHECK/index/grant assertions; `UNIQUE (app_id, workflow_name, dedup_key)` conflict test. Verify: DDL integration test against live PG. **Rule for the whole train: this is the only schema PR. Later PRs consume columns; they never add them.** (One sanctioned exception path: if a rail discovers a genuinely missing column, the fix edits the DW-02 create scripts in that rail's PR — pre-launch, no `ALTER` — and is called out to the operator in the PR summary.)
- **DW-03** — scope: `sdks/workflows/` package — `Workflow<Params, Output>` base, `WorkflowTrigger`, `Step`, `StepConfig<T>`, `StepAllOptions`, error classes (`PermanentError`, `StepTimeoutError`, `NondeterministicError`, `StalledError`, `ChildTimeoutError`, `ChildCancelledError`, `LimitExceededError`, `RestartError`, …); the framework-internal **replay/step shim** module (ordinal assignment, memoization table, interrupt exception) living behind the bootstrap boundary; a `__zsWorkflowDispatch` entry in `sdks/bootstrap/src/dispatcher.ts` + `runtime-entry.ts` wiring; pnpm build-graph edge (bootstrap → workflows). Small Rust: runtime `include_str!`/init acknowledgment of the new entry. Deps: none (parallel with DW-01/02). Test gate: pure-JS shim unit tests (ordinal determinism, memoized resolve, interrupt); `pnpm build` then `cargo build -p zeroship-runtime` green. Verify: vitest + the two builds in order. **This PR is why SDK-first shapes the critical path.**

### M1 — Walking skeleton

| id | title | kind | size | risk |
|---|---|---|---|---|
| **DW-04** | Journal store + claim/lease + dispatch scheduler (seed 2) | Rust | M | High |
| **DW-05** | Worker replay host + gateway dispatch edge + pinned-deploy loading | Mixed | XL | High |
| **DW-06** | Single-frontier replay core + §7.4 commit (N=1) (seed 3) | Mixed | L | High |
| **DW-07** | Faithful-e2e harness + crash-injection helpers | Mixed | M | Med |

- **DW-04** — scope: `crates/control` workflow module — journal store over `compio-postgres`; advisory-lock claim; CAS `claimed_by`/`claim_epoch`/`lease_expires`; `wake_at`-unified compio timer; claim sweep (the crash backstop); the `lease_ttl > wall_budget` config guard. Deps: DW-01, DW-02. Test gate: two concurrent claimers → one wins; expired lease reclaimed by sweep; epoch-guarded commit of an evicted claim rolls back — all against live PG. Verify: `cargo test -p zeroship-control` (full suite, not `--lib` — `feedback_verify_full_suite_not_lib`).
- **DW-05** — scope: the dispatch transport: control hands `(run, journal prefix, deploy_id)` to a worker via the gateway edge (gateway stays dumb — forward + meter unit only); the worker loads the **pinned** deploy's bundle and enters/creates the workflow isolate — this **re-keys the worker isolate cache from `app_id` to `(app_id, deploy_hash)`** for workflow dispatches (`crates/zeroship-worker/src/cache.rs`); worker invokes `__zsWorkflowDispatch`, returns the dispatch-completion envelope; **deploy-retention guard**: `app_deploys` rows (and their bundle blobs) referenced by any non-terminal `workflow_runs.deploy_id` are not GC-eligible. Deps: DW-03, DW-04; **blocked by gate G1**. Test gate (faithful e2e): dispatch of a run pinned to a *non-current* deploy executes the old code; LRU eviction pressure with two live deploys of one app doesn't cross-contaminate isolates; retention guard blocks deploy GC while a run sleeps. Verify: e2e on live stack + `cargo test -p zeroship-worker -p zeroship-gateway`.
- **DW-06** — scope: the `effN = 1` engine: deterministic ordinals, journal memoization, name-divergence determinism guard, interrupt-after-frontier model, the single-row §7.4 commit (idempotent, lease-guarded), immediate-re-dispatch scheduling. Explicitly the `concurrency = 1` reduction of the unified loop (no throwaway code). Deps: DW-05. Test gate: **the M1 keystone e2e** (3-step workflow survives worker `kill -9` + control restart; exactly-once rows under lease handoff). Verify: e2e + JS shim unit suite.
- **DW-07** — scope: the reusable faithful-e2e harness (`tests/` script or control-crate integration tests): boots control + gateway + worker against PG `:5440`, builds + deploys a real example `.zship`, exposes crash-injection (`kill -9` at named barriers), lease-handoff forcing, and clock-advance helpers for sleeps/schedules. Deps: DW-06 (co-developed; DW-06's keystone runs on it). Test gate: harness self-test (boot, deploy, run, teardown deterministic). Verify: CI-runnable script exits 0. **Everything after this PR states its gate in harness terms.**

### M2 — Full core semantics

| id | title | kind | size | risk |
|---|---|---|---|---|
| **DW-08** | Concurrent frontier (seed 4) | Mixed | L | High |
| **DW-09** | Suspension & signals: sleep / sleepUntil / waitForSignal / run.signal (seed 5) | Mixed | L | Med |
| **DW-10** | Run controls + status + **restart** (seed 6) | Mixed | L | High |
| **DW-11** | `env.workflows` namespace + `@zeroship/workflows` client (seed 7) | JS | M | Med |
| **DW-12** | Metering + spend enforcement (seed 8) | Rust | M | Med |
| **DW-13** | Determinism/liveness hardening + §9 fold refinement (seed 10) | Mixed | L | High |
| **DW-13f/DW-13h** | `step.sideEffect` + dispatch-scoped body-I/O prevention | Mixed | M | High |

- **DW-08** — scope: `frontierCandidates` collection + macrotask drain; generalized never-settling barrier; N-row idempotent commit + outcome fold (§6.4); `Workflow.concurrency`, `step.all` + `StepAllOptions`, bare-`Promise.all` structural detection; `effN = min(...)` clamping. Deps: DW-06. Test gate (faithful e2e): 3-wide frontier commits atomically; crash-mid-frontier journals zero-of-N, all N re-run; **`effN=1` byte-identical-journal equivalence test** (§11 C8); overflow candidates roll to the next dispatch. Verify: e2e + drain-determinism unit tests (frontier set is a pure function of code+prefix).
- **DW-09** — scope: `step.sleep`, `step.sleepUntil` (absolute-target `wake_at`, zero DDL), `step.waitForSignal` (+ `timeout`, `maxSignalAge`, resolves `SignalEnvelope<P> | null`), `run.signal` (app-credentialed), `MIN(pending)` wake + re-eval-all, §8 surfacing rule (`sleeping` vs `waiting`). All as legal frontier members. Deps: DW-08. Test gate (faithful e2e): mixed `step.all([run, sleep, waitForSignal])` per the §6.2 example resolves across dispatches; sleepUntil never wakes early (DB clock); signal delivered before the wait binds on freshness rules; timeout resolves null. Verify: e2e + `cargo test -p zeroship-control`.
- **DW-10** — scope: `start({ input, key, onConflict })` idempotent start; `pause`/`resume`/`cancel`/`status`; the full **restart** surface — §7.10 txn (advisory-lock + `claim_epoch` evict + drop `ordinal ≥ t` + blob/signal/subscription prune + reset + grants), `RestartOptions`/`RestartTarget`/`RestartError`, creator route `…/workflows/runs/:runId/restart` + operator `/control/ops/…` peer, restart cap, deploy-pin rules (partial = original-immutable; full = current-default, `deploy:"started"` escape; re-pin bumps `signal_epoch`); the 4 audit columns already exist (DW-02). Deps: DW-09. Test gate: the **complete design-§15 PR-6 e2e list** (partial-restart prefix retention with identical ordinals; live-dispatch eviction; `deploy:"latest"`+`from` → 409; terminal revive; creator cap 429 / operator exempt+audited; blob-row refcount decrement + re-reference). Verify: e2e + control full suite.
- **DW-11** — scope: the creator-facing client — `env.workflows.X.start/…`, `run.{signal,cancel,pause,resume,status,restart,createSignalToken*}` handle objects, typed `WorkflowRun<O>`; worker-side `env.workflows` namespace registration (credential injection from the server-stamped `app_id`, mirroring `MeterHandle` discipline). (*`createSignalToken` activates with DW-16.*) Deps: DW-10 (API surface frozen). Test gate: e2e drives every control verb through `env.workflows` from deployed app code (not raw HTTP). Verify: e2e + vitest. **Pure-JS + thin plumbing; parallelizable.**
- **DW-12** — scope: per-dispatch platform counters (`requests`/`cpu_us`/`wall_us`/`ingress`/`egress`) emitted by the worker for workflow dispatches; data-primitive metrics inside step bodies unchanged; spend enforcement parity (Warn→Degrade→Block; **Block = 402 before dispatch**, including compensation dispatches later). Deps: DW-08 (frontier aggregation semantics). Test gate: e2e asserts one metered unit per dispatch incl. a 3-wide frontier; a Blocked app's queued run does not dispatch (402 at edge) and resumes on unblock. Verify: e2e + `cargo test -p zeroship-metering -p zeroship-control`.
- **DW-13** — scope: `NondeterministicError` detection (pending non-step promise at the macrotask boundary; name-divergence already in DW-06); `stuck_strikes` → `StalledError` (fail-closed `stalled` terminal); wall-budget rollover behavior (`UNSETTLED` = no row); **the §9 terminal-failure fold refinement** — commit the failed row, immediate re-dispatch, terminal failure = throw escaping `run()` (makes `try/catch` observable; hard prerequisite for DW-18). Deps: DW-09. Test gate (faithful e2e + regression style): seeded journal name/ordinal divergence on the real dispatch path → `NondeterministicError`; bare body I/O fails with `NondeterministicError`; a zero-progress wide frontier trips `stuck_strikes` → `stalled`; a **caught** step failure continues the run; an uncaught one lands `failed` after exactly one extra deterministic re-dispatch. Verify: e2e + shim unit tests.
- **DW-13f** — backlog: `step.sideEffect(name, fn)` (`kind='sideEffect'`, inline output, no new columns). Deps: DW-13. Test gate: `step.sideEffect` computes once and replays the frozen value.
- **DW-13h** — backlog: dispatch-scoped `fetch`/timer prevention outside journal callbacks. Deps: DW-13. Test gate: bare body I/O throws reliably; in-step and non-workflow request `fetch` pass through unchanged.

### M3 — Feature rails

| id | title | kind | size | risk | execution |
|---|---|---|---|---|---|
| **DW-14** | Schedules: DSL + discovery + reconciliation + sweep (seed 9) | Mixed | L | Med | parallel rail A |
| **DW-15** | Blob-backed & streaming outputs + GC (seed 11) | Mixed | XL | High | parallel rail B |
| **DW-16** | External signal ingress + broadcast (seed 12) | Mixed | XL | High | parallel rail C |
| **DW-21** | Dev-tier mini-engine (local inner loop) | Mixed | L | Med | parallel rail D |
| **DW-17** | Child orchestration + `startMany` + cascade (seed 13) | Mixed | XL | High | **spine (sequenced)** |
| **DW-18** | Compensation / saga rollback (seed 14) | Mixed | XL | High | **spine (after DW-17)** |

- **DW-14** — scope: SDK `@zeroship/workflows/schedule` — `every` fluent builder + `cronExpr` + `compileSchedule` (one pure build-time function) + `InvalidScheduleError` + typed `schedule({ name, schedule, workflow, input, overlap?, catchUp? })`; vite-plugin `schedules[]` manifest discovery (reusing RPC/route discovery; bad schedule **fails the build**); control-plane deploy reconciliation (one-txn upsert + delete-absent, deploy-pinned); the schedule sweep (peer of the claim sweep — same compio task family, `claimed_by` lease + advisory lock, `FOR UPDATE SKIP LOCKED`, DB-clock `now()`, `first_fire_at_strictly_after` with pinned tzdb + DST rules per §16, `sched:{id}:{epoch(planned)}` dedup via `start({key, onConflict:"join"})`, `overlap`/`catchUp` advance); schedule caps. Deps: DW-10, DW-07. Test gate: the design-§15 PR-9 e2e list (fluent + raw round-trip to a real fired run with `trigger.startedAt` = planned instant; redeploy drops row; two concurrent sweepers → exactly one run; backfill ≤ max each on its own instant; bad zone / sub-minute cron fails the build). Verify: e2e + vitest (DSL compile table incl. DST spring-forward/fall-back vectors) + control suite.
- **DW-15** — scope: `WorkflowBlobStore` (`wfblob/` namespace over the existing `crates/bundle` blob resolver, `compio-s3`/local); worker-side content-addressed write path (auto-spill over the 1 MiB inline cap; `output: "blob" | "stream"`; mid-stream `maxStepBlobBytes` abort + partial cleanup); `workflow_blobs` refcount upsert co-committed in §7.4; `StepOutputRef` (`.json()`/`.stream()`/…) + `step.run` overloads + `StatusOutput`; control-plane `…/runs/{runId}/output` + `…/steps/{name}/output` streaming reads; the two advisory-lock GC sweeps (ref-table + orphan, orphan grace strictly longer); dispatch-scoped read memoization; metering ride on `storage_ops`/`storage_bytes`/`egress_bytes`. Deps: DW-06 (replay rematerialize-vs-handle), DW-12 (metering), DW-07. Test gate: design-§15 PR-11 e2e list (over-cap output round-trips identical value across crash-mid-write replay; stream aborts + cleans partial at ceiling; GC never deletes a referenced blob) + refcount-vs-restart interaction (with DW-10). Verify: e2e + bundle/control suites. **Bundle-store invariant untouched (deploy blobs are a separate GC domain) — assert in review.**
- **DW-16** — scope: gateway route family `POST /__zeroship/signals/v1/{run|topic}/{addr}` (gateway = rate-limit token bucket + forward only; verifies nothing, writes nothing); control-plane ingress terminus with the three verifiers (`zeroship-hmac` reusing `stripe_handlers.rs` constant-time/timestamp-tolerance logic; `bearer` `wst_` verify; `provider:stripe` foreign-signature + `topicFrom`); deploy-pinned `externalSignals` allowlist; `wst_` mint/rotate/revoke control endpoints + envelope-encrypted key storage (P5 data key); topics + subscriptions + `workflow_broadcasts`; `step.waitForSignal` `opts.topic` late-bind; `env.workflows.publish` + `run.createSignalToken`; resumable fan-out + subscription GC on the claim sweep; accept-arm-only edge metering (`wf_signals_ingress`/`wf_broadcasts`/`wf_fanout_deliveries`/`ingress_bytes`); caps (`max_qps`, `max_body_bytes`, `max_topics`, `max_fanout`, `max_subscribers_per_topic`, token `ttl`); `signal_epoch` invalidation; keep `verifier='bearer-signing'` (key row) vs `bearer` (request) distinct in the codec/registry (§16 note). **Gate G5 (rate-limit backing store: `env.kv`/redis edge bucket vs persisted PG config) is decided at this PR's kickoff.** Deps: DW-09, DW-11, DW-12, DW-07. Test gate: design-§15 PR-12 e2e list (genuine HMAC POST binds a wait; forged/expired token 401; non-allowlisted type 403; retried idempotency key double-inserts zero rows; 10k-subscriber publish fans out resumably with exactly-one delivery across a crash mid-fan-out; bumped `signal_epoch` kills outstanding tokens) + rejects are unmetered and PG-write-free. Verify: e2e + gateway/control suites. **This is the security-critical PR — extra adversarial review mandatory (§6 risk register).**
- **DW-21** — scope: the local dev-tier peer (pattern of `env.db`→SQLite / `env.kv`→redb / dev-auth): an in-process mini-engine for `zeroship serve` / `pnpm dev` — SQLite-backed journal, in-process scheduler/timer, same SDK surface, same replay shim, dev-only-by-construction. Contract parity documented; intentional divergences (no multi-node lease races, no gateway edge) listed. Deps: DW-09 (semantics frozen), DW-03. Test gate: the starter workflow example runs locally: start → sleep → signal → complete; the same example deploys unchanged to the real stack and passes there. Verify: golden-path script extension + vitest. **This is the fast inner loop for creators and for our own examples/tests — worth landing before the spine's heavy tail.**
- **DW-17** — scope: `step.call(WorkflowClass, input, opts?)` shim (spawn co-commit in the parent §7.4 txn, park as `wait_signal`-flavored `kind='child'` step, bind + rethrow on join, blob-ref passthrough for large child outputs); `ChildWorkflowOptions`, child error classes; the terminal-child hook co-committed in the child's terminal txn; reserved `__zs.` type guard (403 on user-supplied); `startMany` batch endpoint (one txn, per-row `ON CONFLICT DO NOTHING`, `maxStartManyBatch`); cascade cancel (`cancel_requested` + cooperative pickup at claim); child pins to the **parent's** deploy; caps (`maxChildDepth`, `maxLiveDescendants` — accounting approach is an open implementation question, §16: aggregated counter vs O(tree) walk; **spike inside this PR, pick one, justify in the PR summary**). Deps: DW-08, DW-09, DW-10, DW-15 (blob-ref passthrough), DW-07. **Sequenced on the spine** — edits the replay shim, the §7.4 commit, and the §9 fold, same files as DW-18. Test gate: design-§15 PR-13 e2e list (crash-mid-park spawns exactly one child; 100-wide `Promise.all(step.call…)` joins in issue order; child `PermanentError` rethrows while siblings commit; cascade cancels sub-tree not independent sibling; retried 1k `startMany` exactly-once per key; `__zs.` forge 403). Verify: e2e + full control/worker suites.
- **DW-18** — scope: `compensation_state='pending'` stamping on completing compensable steps; `compensatorRegistry` from replay; reverse-ordinal frontier selection + compensation fold + §7.4 commit variant; `compensating` phase in §9 + `status()`; `cancel({ mode: "compensate" })`; `StepConfig<T>.compensate`, `Compensator<T>`, `CompensationContext` (`ctx.idempotencyKey`), `static compensationConcurrency` (reuses DW-08 clamping — no new machinery); engine-integrity failures fail closed with **no** rollback; restart × compensation guard (partial restart past a settled compensation → `RestartError`, already in DW-10's txn — the e2e that proves it lands here). Deps: DW-13 (fold refinement), DW-08, DW-10; sequenced after DW-17. Test gate: the full design-§15 PR-14 e2e list (a)–(g), incl. crash-mid-compensator at-least-once body / exactly-once marker, strict `3→2→1` reverse order, `partial` outcome on exhausted compensator, `NondeterministicError` fails closed. Verify: e2e + full suites.

### M4 — Launch-ready

| id | title | kind | size | risk |
|---|---|---|---|---|
| **DW-19** | Chaos + cross-feature interaction suite | Mixed | L | High |
| **DW-20** | Terminal-run retention/GC sweep | Rust | M | Med |
| **DW-22** | Docs + starter example + golden path | Docs | M | Low |
| **DW-23** | Capacity measurement + tier-default seeding (**the load gate**) | Ops | M | **unknown-outcome** |
| **DW-24** | Rollout scaffolding: enable flag + two kill-switches + runbook + alerts | Mixed | M | Med |

- **DW-19** — scope: the adversarial matrix the per-feature e2e lists don't cover pairwise: restart × blob refcount re-reference; restart rejected past settled compensation (proven in DW-18, extended here across crash); external signal to a `compensating`/terminal run → 404; cascade cancel racing a redeploy; schedule fire racing a restart of the previous run (`overlap:"skipIfRunning"` best-effort documented behavior); lease-handoff during fan-out; blob GC racing a restart's refcount decrement. Every finding → fix PR with a pre-fix-failing regression test. Deps: all of M3. Verify: suite green ×3 consecutive runs (flake check).
- **DW-20** — scope: the terminal-run retention sweep (operator policy from **G4**): prune `workflow_steps`/`workflow_signals`/broadcast rows for terminal runs past the window; **leaf-up ordering** (`tree_depth` DESC; `parent_run_id` RESTRICT makes violations loud); co-committed `workflow_blobs` refcount decrements; restart-eligibility window respected (never prune a prefix inside the restart policy window); optional restart-history log if G4 wants it. Deps: DW-15, DW-17; **gated on G4**. Test gate: e2e — pruned parent/child tree in correct order; restart of a within-window run still works post-prune-pass; refcounts consistent. Verify: e2e + control suite.
- **DW-22** — scope: `docs/reference/workflows.md` (creator API + the §6.3 two-line contract + §16 honest residual limits verbatim — do not regress them), schedule-DSL reference, AGENTS.md task-router row + SDK-layers note, `examples/starter` workflow + schedule example, `tests/golden_path.sh` extension, `sdks/workflows/README.md`. Deps: API frozen (post-DW-18). Verify: docs build/lint; golden path green; a doc-review pass checks every documented name against the shipped surface.
- **DW-23** — scope: workflow scenarios for the bench harness (zerobench-style): dispatch throughput vs frontier width; wide-frontier behavior under wall-budget rollover (the §11 C7 to-measure); 10k-subscriber fan-out latency vs sweep cadence; blob spill/stream write+read costs vs size; schedule sweep tick capacity; child-tree depth/width costs; control-plane PG pool pressure under sustained dispatch. Output: a measurement report + **seeded operator config** for every §13 knob (`platform_ceiling[tier]`, `wall_budget`, `lease_ttl`, `stuck_strikes` bound, per-step output cap, blob ceilings + GC grace, ingress caps + token ttl + timestamp tolerance, schedule floors/caps/backfill max/sweep cadence, child caps, compensation defaults, restart cap). Deps: all features. Risk: **unknown-outcome by definition** — no number in this plan pre-empts it; if a measurement invalidates a default assumption (e.g. sweep cadence can't hold fan-out latency), that's a finding routed back as a fix PR, not a doc edit. Verify: report + config committed; the seeded values referenced by DW-24's runbook. **Satisfies gate G3; blocks cohort/GA.**
- **DW-24** — scope: per-app `workflows_enabled` flag, **false by default**, plan-catalog-gated; **kill-switch 1 — dispatch pause**: scheduler stops claiming globally (or per-app); in-flight dispatches finish their commit; everything else freezes durably as `queued`/`sleeping`/`waiting` (safe by construction — durability is the journal); resume loses nothing. **Kill-switch 2 — ingress disable**: gateway/control return 503 on the public signal route family (and optionally pause the schedule sweep as a half-switch); journal untouched; point-to-point app-credentialed `run.signal` unaffected. Ops runbook (enable/disable, drain, restart-a-run, read-a-journal, GC status); dashboards/alerts: lease-reclaim rate, `stuck_strikes`/`stalled` rate, sweep lag (claim/schedule/fan-out/GC), journal + blob growth, ingress reject rate, `compensation_outcome='partial'` rate. Deps: DW-12, DW-16. Test gate: **drill e2e** — flip each switch under live load, assert safe freeze / 503, flip back, assert full resume with zero lost or duplicated runs. Verify: e2e + runbook review.

---

## 4. Dependency graph + critical path

```
DW-00 (docs)
DW-01 (typed ids/envelope) ──▶ DW-02 (DDL, complete day-1)
DW-03 (SDK + bootstrap entry)      │
        │                          ▼
        │                    DW-04 (claim/lease/scheduler)
        │                          │
        └────────────┬─────────────┘
              [G1 go/no-go]
                     ▼
               DW-05 (worker replay host + gateway edge + pinning)
                     ▼
               DW-06 (single-frontier core, N=1)──DW-07 (e2e harness, co-lands)
                     ▼
               DW-08 (concurrent frontier)
                     ▼
               DW-09 (suspension & signals) ──────────────┐
                     ▼                                    │
               DW-10 (controls + restart)                 │
                 │         │                              │
                 │         ├──▶ DW-11 (env.workflows)     │
                 │         │      [parallel]              │
                 │         └──▶ DW-14 (schedules)         │
                 │                [rail A]                │
                 ▼                                        ▼
               DW-13 (hardening + §9 fold)      DW-12 (metering) [parallel]
                 │                                   │
                 │            ┌──────────────────────┤
                 │            ▼                      ▼
                 │      DW-15 (blob rail)      DW-16 (ingress+broadcast)
                 │        [rail B]               [rail C]  ← G5 decided at kickoff
                 │            │                      │
                 │            │        DW-21 (dev-tier) [rail D, after DW-09]
                 ▼            ▼
               DW-17 (children + startMany)   ← spine, needs DW-08/09/10/15
                     ▼
               DW-18 (compensation)           ← spine, needs DW-13 + DW-17 (same files)
                     ▼
               DW-19 (chaos matrix) ──▶ DW-20 (retention, G4) ──▶ DW-23 (load gate, G3)
                                              DW-22 (docs)   ──▶ DW-24 (rollout)
                                                                    ▼
                                                          M-launch (dogfood→cohort→GA)
```

**Critical path:** `DW-03 → [G1] → DW-05 → DW-06 → DW-08 → DW-09 → DW-10 → DW-13 → DW-17 → DW-18 → DW-19 → DW-23 → DW-24 → launch`, with `DW-01 → DW-02 → DW-04` feeding in before DW-05. The three structural orderings baked in: **SDK-first** (DW-03's bootstrap dist must exist before the runtime/worker Rust builds that `include_str!` it), **journal-before-engine** (DW-02 before DW-04; DDL is complete on day one so no rail ever waits on schema), **engine-before-gateway-dispatch** (DW-05's edge route only carries what DW-04 can claim; DW-16's public ingress only lands after DW-09's signal semantics exist).

**Off the critical path** (pull forward whenever a stream is free): DW-11, DW-12, DW-14, DW-15, DW-16, DW-21, DW-22. Note DW-15 re-joins the critical path as a DW-17 input (blob-ref passthrough) — start rail B early so it never becomes the blocker.

---

## 5. Parallelization / execution plan

**Ground rules** (from hard-won pilot lessons): one codex/agent per worktree, never git-op a worktree with a live agent (`ps` check first); commit-only, NEVER push; concurrency is bounded by **file-locality**, not by ambition — same-file edits conflict, so anything touching the replay shim (`sdks/workflows` + bootstrap dispatcher), the §7.4 commit txn, or the §9 fold is **spine-sequenced** in one worktree. Implement-phase agents must **self-verify** (run the stated test gate, grep that their files/tests exist) and never punt to a background sub-job returning empty — the brief for every dispatch says so explicitly, and the pilot verifies each phase actually committed via `git log` before dispatching the next.

**Same-file hotspots (must sequence):**
- `sdks/workflows/` shim + `sdks/bootstrap/src/dispatcher.ts`: DW-03 → DW-06 → DW-08 → DW-09 → DW-13 → DW-17 → DW-18 (strict order).
- Journal store / §7.4 commit / fold in `crates/control`: DW-04 → DW-06 → DW-08 → DW-10 → DW-17 → DW-18.
- The DDL create scripts: DW-02 only (rails consume, never add — see the DW-02 rule).
- Sweep task family (`crates/control` sweeps): claim sweep (DW-04) → schedule sweep (DW-14) → fan-out/subscription GC (DW-16) → blob GC (DW-15) → retention (DW-20). These add **peer sweeps in separate modules** — parallel-safe if each rail owns its own file and only DW-04's registration point is the shared seam (keep that seam a one-line registry append to minimize conflicts).

**File-disjoint (parallel-safe):**
- DW-01/DW-02 ∥ DW-03 (M0: two streams).
- DW-11 ∥ DW-12 ∥ (DW-09→DW-10 spine) — client JS, metering Rust, and spine touch different files.
- Rails A (DW-14: vite-plugin + schedule sweep module) ∥ B (DW-15: blob store + worker write path + GC module) ∥ C (DW-16: gateway route + ingress terminus module) ∥ D (DW-21: dev-tier), while the spine runs DW-13 then DW-17/DW-18. Rails B and C both add commit-txn co-writes (blob refcount / signal consumption) — pre-carve the §7.4 txn builder in DW-08 with explicit extension points (a commit-set struct rails append to) so B/C add rows without editing the same function body.

**Recommended stream count: 3 concurrent (max 4 briefly in mid-M3).**

| phase | worktree 1 (spine — this one, `.worktrees/durable-workflows`) | worktree 2 | worktree 3 (+4) |
|---|---|---|---|
| M0 | DW-00, DW-01→DW-02 | DW-03 | — |
| M1 | DW-04→DW-05→DW-06 (+DW-07) | e2e harness co-dev (pairs with spine, merges into DW-06/07) | — |
| M2 | DW-08→DW-09→DW-10→DW-13 | DW-11 then DW-12 | — |
| M3 | DW-17→DW-18 | rail B: DW-15 (start at M2-end) | rail C: DW-16; rail A: DW-14 / rail D: DW-21 as slots free |
| M4 | DW-19→DW-20 | DW-22, DW-24 | DW-23 bench runs |

Merging: rails merge back into the spine worktree between spine PRs (trial-merge, run the full suite, then commit the merge). The spine worktree is the single source of truth for what "done so far" means; satellite worktrees rebase onto it at each pickup. Sequenced PR-train on one worktree for the spine — the 2026-07-04 pilot showed parallel-isolated worktrees on same-file work produce conflict churn; don't repeat it for DW-17/DW-18.

Dual-review cadence: every Med+ risk PR gets critic → reviser (two-agent, per `feedback_review_pattern`); High-risk PRs (DW-05/06/08/10/13/16/17/18) additionally get a targeted adversarial pass scoped to their risk-register entry before merge into the spine.

---

## 6. Risk register

| # | risk | impact | mitigation |
|---|---|---|---|
| R1 | **Concurrent frontier determinism** — the macrotask drain admits a nondeterministic candidate set, or bare-`Promise.all` detection misses a structure, corrupting ordinals | Journal corruption class: `NondeterministicError` storms or, worse, silently wrong memoization. Everything downstream sits on §11 C1. | DW-08 carries drain-determinism unit tests (frontier = pure fn of code+prefix) + the `effN=1` byte-identical equivalence gate; DW-13 proves structural name/ordinal divergence on the real dispatch path; DW-13f adds dispatch-scoped I/O prevention; targeted adversarial review on the shim; chaos suite (DW-19) replays the same journal N× asserting identical frontiers. |
| R2 | **Compensation reverse-replay** — the re-dispatch-to-observe-catch fold (§9) or `compensatorRegistry` reconstruction diverges from forward replay; lease handoff double-settles a compensator | Wrong rollback = real-world money/inventory damage in creator apps; the hardest-to-reason-about code in the train. | Sequenced (DW-13 fold lands + is e2e-proven before DW-18 starts); full design (a)–(g) e2e incl. crash-mid-compensator marker-once test; CC1–CC8 each mapped to a named test; dual-review + adversarial pass mandatory; `NondeterministicError` fails closed with no rollback (verified). |
| R3 | **External-ingress auth + token minting** — a public, unauthenticated-by-default edge: verifier confusion (`bearer` vs `bearer-signing`), HMAC timing/tolerance bugs, `wst_` scope escape, `__zs.` forge, `signal_epoch` gaps | Security incident class: forged signals drive creator workflows (charge/approve flows). Highest external blast radius in the train. | Reuse the battle-tested `stripe_handlers.rs` constant-time verify; envelope-encrypted keys (P5); deploy-pinned allowlist walls type injection; DW-16 e2e covers forge/expiry/allowlist/epoch-bump; **dedicated red-team review pass** (the OAuth2-review playbook) before the route ships; kill-switch 2 gives an instant off. Rejects never write PG or meter (verified). |
| R4 | **Deploy-pinning + isolate-cache re-key (G1)** — relaxing one-isolate-per-app to `(app_id, deploy_hash)` for workflow dispatches: cache pressure, eviction interplay, cross-deploy contamination | Worker-wide blast radius: touches `crates/zeroship-worker/src/cache.rs`, the platform's hottest invariant. A bug degrades *request* serving, not just workflows. | Explicit operator go/no-go **before** DW-05; the PR isolates the re-key behind the workflow dispatch path (request path keying untouched — assert in review); e2e: two live deploys of one app under LRU pressure, no contamination; memory behavior under multi-deploy load is **to-measure** in DW-23. |
| R5 | **Blob-retention + deploy GC** — a pinned deploy's bundle GC'd under a sleeping run; or workflow-blob GC deletes a referenced/racing blob (restart refcount decrement vs sweep) | A run that can never dispatch again (dead pinned code) — silent durability break; or lost step outputs. | DW-05's retention guard with its own e2e; refcount co-commits in the §7.4/§7.10 txns (atomic with the journal); orphan grace strictly > ref grace; DW-19 chaos case: GC racing restart; GC sweeps are advisory-lock-serialized. Grace/cadence defaults **to-measure** (DW-23). |
| R6 | **Child orchestration vs single-frontier + lease** — spawn co-commit under the parent lease, terminal hook co-commit under the child's, cascade under per-run leases: three lease domains interacting | Double-spawned children, lost joins (parent waits forever), or cascade deadlock. | CW1–CW7 each mapped to a named e2e (crash-mid-park exactly-one-child is the keystone); the safety-net re-arm scan (§18.4 pattern) for lost parent wakes; cascade is cooperative-only (no cross-run lease — enforced by construction, asserted in review); `maxLiveDescendants` accounting spiked inside DW-17 before committing to a mechanism. |
| R7 | **At-least-once duplicate-effect window** — not a bug but the engine's honest contract; concurrency amplifies it up to N× per crashed dispatch; creators will be surprised | Creator-facing incidents (double charges) blamed on the platform; support/reputation cost at launch. | Cannot be engineered away (§11 C3) — mitigate by surface: the §6.3 two-line contract + §16 residual limits go verbatim into `docs/reference/workflows.md` and the starter example demonstrates idempotency keys; `ctx.idempotencyKey` + stable step keys make the right thing easy; dogfood phase explicitly exercises crash-under-load to see the window in practice. |
| R8 | **Wide-frontier liveness under wall-budget rollover** — `UNSETTLED` steps re-run forever if no step ever settles inside the budget; throughput unknown | Stuck runs, wasted metered dispatches, creator-visible stalls. | `stuck_strikes` → `StalledError` backstop (DW-13, e2e-proven); explicitly **to-measure** in DW-23 (this is the load gate's core question — it sets `wall_budget`, `platform_ceiling[tier]`, `lease_ttl`); no number claimed until measured. |
| R9 | **Control-plane PG pressure / journal growth** — every dispatch is a claim + prefix load + N-row commit on the shared control DB; fan-out and journals grow unbounded pre-retention | Platform-wide control-plane degradation (deploys, billing share the DB). | DW-23 measures dispatch cost + pool pressure; retention (DW-20) bounds growth before GA; `journal_bytes` tight-bounded by `BLOB_REF_COST` design; sweeps batch with `SKIP LOCKED`; alerting on sweep lag + journal growth (DW-24). If measurement says the shared DB can't hold projected load, the escalation path (separate PG / partitioning) is an operator decision — flagged, not assumed. |
| R10 | **Schedule correctness at calendar edges** — DST, tzdb pinning, epoch-anchor surprises, concurrent sweepers | Double-fired or skipped business-critical jobs. | The §16 DST rules are implemented as a fixture table (spring-forward/fall-back/ambiguous vectors) in DW-14; dedup key makes double-create impossible by construction (e2e: two sweepers, one run); tzdb pinned to the control binary; `L`/`#`/day-29–31 rejected at build time. |

---

## 7. Operator decision gates

Each gate is a named go/no-go that **blocks specific PRs**. The pilot surfaces each with a one-page brief (options, recommendation, consequences) and does not proceed past a blocked PR without the decision recorded (in the PR description + an ADR where durable).

| gate | decision | blocks | default recommendation to bring to the operator |
|---|---|---|---|
| **G1 — isolate-pinning relaxation** | Relax "one isolate per app" to `(app_id, deploy_hash)` for **workflow dispatches only**, so in-flight runs replay their pinned deploy's code | **DW-05** (hence all of M1+ — this is the first gate on the critical path) | Go, scoped: the relaxation applies only to the workflow dispatch path; request-path cache keying is untouched; LRU sizing implications measured in DW-23. The alternative (replay on current deploy) breaks §4's core determinism invariant — effectively no alternative exists if deploy-pinning stays. |
| **G2 — journal PII at rest** | Run inputs/outputs, step outputs, and signal payloads live in control-plane PG (and `wfblob/`) — plaintext + retention policy, or P5-style envelope encryption at rest? (The design already envelope-encrypts ingress *keys*; this gate is about *payloads*.) | Finalizes DW-02's payload-column representation (decide before M1 exit to avoid re-cutting DDL); hard-blocks **cohort** (external creator data) either way | Bring both options with the P5 machinery cost sketch. Dogfood may proceed on internal data regardless; no external creator payload lands before the decision. Related §16 flag: content-addressed dedup defers hard-delete — any GDPR/data-residency stance is decided here too. |
| **G3 — capacity measurement → tier defaults** | Accept DW-23's measured seeds for every §13 knob (`platform_ceiling[tier]`, `wall_budget`, `lease_ttl`, blob ceilings + GC grace, ingress caps, schedule floors/cadence, child caps, compensation defaults, restart cap) | **Cohort and GA** (dogfood may run on conservative placeholder config, explicitly labeled placeholder) | Run the bench, present the report, seed the plan catalog. Anything the bench can't answer stays labeled **unknown/to-measure** with a conservative placeholder + an alert. |
| **G4 — terminal-run retention policy** | Retention window(s) for terminal `workflow_steps`/`workflow_signals`/broadcasts; interaction with the restart window; whether to keep a restart-history log | **DW-20**; hard-blocks **GA** (unbounded journal growth is not launchable) | Propose: retention ≥ restart-eligibility window; leaf-up prune order is structural (already enforced by RESTRICT); log opt-in. |
| **G5 — ingress rate-limit backing store** | Edge token bucket in `env.kv`/redis (no PG write on reject) vs persisted per-app PG config rows; per-source bucket eviction policy | **DW-16** kickoff | Recommend redis/kv at the edge (matches "gateway is dumb", rejects stay off PG), with plan-default seeding; per-source eviction TTL-based. |
| **G6 — launch go/no-go** | The §9 rollout checklist, per stage (dogfood → cohort → GA) | Each **M-launch** stage transition | See §9. |

---

## 8. Testing & verification strategy

**The faithful-e2e keystone chain (non-negotiable, per `feedback_faithful_e2e_tests`):** every milestone's exit criterion runs on the **real path** — live control + gateway + worker, a real `.zship` deploy of real workflow code, real PG on `:5440`, the real replay/dispatch/commit txn. No dispatcher shims, no PG-gated skips, no unit-stub "e2e." The DW-07 harness is the single vehicle; if a test can't run on it, it isn't the e2e gate for its PR. (Ops note: a parallel job can tear down the `appbase-migrate-postgres-1` container — `docker start` it before declaring red; render/unit tests are the DB-free clean signal.)

**Keystone tests per milestone** (the one test that proves the milestone, beyond the per-PR gates):
- **M1:** 3-step workflow survives worker `kill -9` + control restart; exactly-once journal rows under forced lease handoff.
- **M2:** crash-mid-frontier zero-of-N atomicity + the `effN=1` byte-identical-journal equivalence run; the full restart e2e list.
- **M3:** one **composite** workflow exercising every rail in a single run — scheduled start → `step.all` fan-out of `step.call` children → child streams a blob-backed output → parent `waitForSignal(topic)` bound by a real HMAC-signed external POST → an injected failure triggers compensation → operator `restart` revives and completes it. If that one run is green on the real stack, the features compose.
- **M4:** the chaos matrix ×3 consecutive green runs + both kill-switch drills under load.

**Regression-test-per-fix (per `feedback_regression_test_per_fix`):** every bug found in review, chaos, dogfood, or cohort ships its fix with a test that **fails pre-fix** — independently re-proven RED (check out the pre-fix commit, run it) before the fix is accepted. The pilot verifies the test *exists*, not just that the suite is green.

**Dual-review gate (per `feedback_review_pattern`):** critic + reviser (two agents) on every Med+ PR; High-risk PRs get an additional adversarial pass scoped to their §6 risk entry; DW-16 gets the full red-team treatment (the OAuth2-review playbook: findings → each regression-tested → critic'd).

**Verification discipline:** full per-crate suites (`cargo test -p <crate>`, all targets — never `--lib`-only, per `feedback_verify_full_suite_not_lib`); `pnpm build && cargo build` order enforced in CI so the `include_str!` contract can't silently stale; zero-tokio dependency audit on every Rust PR.

**The dev-tier as the fast inner loop:** DW-21's SQLite mini-engine is where shim semantics, examples, and most JS unit iteration happen (seconds, no stack boot). It is a *development* loop, never a verification substitute — anything green on the dev-tier still passes the harness before merge. Divergences (no lease races, no edge) are documented so nobody mistakes dev-green for done.

**Determinism-specific verification:** replay-N-times-identical-journal property tests on the shim (same code + prefix ⇒ same frontier/ordinals, run repeatedly under randomized timer jitter); the equivalence test (`effN=1` vs single-frontier baseline) is kept alive for the whole train as a canary.

---

## 9. Rollout / launch plan

**Enablement is data, not deploy:** the engine ships fully built but **`workflows_enabled = false` by default** per app (plan-catalog-gated). Turning it on is a control-plane flag flip, per app or per plan tier — no binary difference between off and on.

**Two kill-switches (built + drill-tested in DW-24):**
1. **Dispatch pause** (global or per-app): the scheduler stops claiming; in-flight dispatches complete their commit; every run freezes durably in `queued`/`sleeping`/`waiting`. Safe by construction — the journal *is* the state; resume loses nothing. This is the "something is wrong with the engine" switch.
2. **Ingress disable**: the public signal route family returns 503 at the edge (with an optional half-switch pausing the schedule sweep); journal untouched; app-credentialed `run.signal` unaffected. This is the "something is wrong at the public edge" switch.

**Staged enablement:**
1. **Dogfood** — enable for platform-internal apps only (our own starter/example apps + an internal operational workflow, e.g. a nightly rollup). Runs on placeholder-labeled conservative config. Soak: exercise crash-under-load deliberately (the R7 duplicate-effect window observed in practice); watch the DW-24 dashboards. Exit: soak window clean (no unexplained `stalled`, no lease-reclaim anomalies, no GC backlog growth), G2 decided.
2. **Cohort** — invited creators, per-app flag flips, measured (G3) config live, retention (G4) live. Support loop: every cohort-reported bug → regression-tested fix. Exit: cohort soak clean; docs validated by a creator who didn't write the engine; no P0/P1 open.
3. **GA** — flag default flips to plan-gated-on for new apps; launch announcement.

**Launch go/no-go checklist (G6, per stage):**
- [ ] All M4 exit criteria green (chaos ×3, drills, retention, docs).
- [ ] G1–G5 decided and recorded (ADRs where durable).
- [ ] §13 knobs seeded from DW-23 measurements (or explicitly placeholder-labeled — placeholders block cohort/GA, allowed only in dogfood).
- [ ] Dashboards + alerts live; on-call runbook exercised (one operator who didn't write it executes a drill from the doc alone).
- [ ] Both kill-switches drill-tested within the stage's config.
- [ ] Metering/billing reconciliation verified: workflow dispatch usage flows into the existing invoice pipeline for a dogfood app (a real invoice line item, end to end).
- [ ] Honest-limits docs shipped (§16 verbatim: at-least-once effects, unordered intra-frontier, broadcast retention, restart re-execution) — support can point at them.
- [ ] No open High-risk register item without an accepted mitigation.

---

## 10. Definition of Done

**Per PR:**
- Scope delivered exactly as stated in §3 (no silent scope cuts; discovered gaps are called out in the PR summary, not absorbed).
- The stated faithful-e2e test gate green on the real path; full per-crate suites green (all targets); `pnpm build && cargo build` order green.
- Dual-review completed at the PR's risk tier; every review finding fixed-with-regression-test or explicitly waived by the operator.
- Every bug fixed inside the PR carries its pre-fix-failing regression test (independently re-proven RED).
- Committed to the correct worktree/branch, commit-only (**never pushed**); the pilot has verified the commit exists and re-run its gate.
- No new tokio dependency; no journal-write grant leakage to gateway/worker; the DW-02 no-later-ALTER rule respected.
- Reference docs / README deltas for any surface the PR changes land **in the same PR** (wire formats are explicit contracts).

**Project level:**
- Every feature in the design doc is shipped and covered by its design-§15 e2e list — no deferred slice, no "v2" remainder (day-1 full-featured is the mandate).
- The M3 composite keystone (§8) passes: all rails compose in one real run.
- All six operator gates decided; all §13 numeric knobs measured-or-placeholder-labeled with placeholders eliminated before GA.
- The design doc's §16 honest residual limits appear, unregressed, in creator-facing docs.
- Rollout executed through cohort with the G6 checklist signed per stage; kill-switches proven under load.
- The design doc + this plan + ADRs for G1/G2/G4 committed; `docs/reference/workflows.md` is the living contract going forward.
