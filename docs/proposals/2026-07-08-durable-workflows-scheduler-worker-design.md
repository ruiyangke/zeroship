# Durable workflows — scheduler + fungible workers (register model)

- **Status:** draft (uncommitted; lands with the implementing PR train)
- **Date:** 2026-07-09
- **Branch:** `design/durable-workflows`
- **Consolidates & supersedes:** the single-tier draft and the engine-consolidation draft (retired here); supersedes the *architecture* of the shipped V1 (`2026-07-05-durable-workflows-design.md`), which remains the committed record and migrates to this target (§12, §14). **The single authoritative durable-workflows design.**

## 1. Problem

The shipped V1 has a **central** journal in the control-plane DB, a **control-plane cron engine** that polls for due runs, and a **second** state-machine copy in `dev.rs` — giving fold-drift, a central-poll ceiling, and workflow load on the control plane. This design separates **scheduling** (a thin tier) from **execution** (fungible workers running one shared engine), keeps the **execution journal in the customer's own database**, and — the decision that shaped this revision — has **no component cross-read a customer database**: the scheduler owns its *own* timer store, populated by explicit register, never by scanning tenant DBs.

## 2. The shape

```
 creator app ── env.workflows.start/signal (JS SDK) ──┐   external ── capability token ──┐
 ┌── CONTROL PLANE (authority + admission + membership · off hot path) ──┐               │
 │ app/plan/deploy/rollout truth · slot budgets · fleet membership        │               │
 └───────────┬─────────────────────────────────────────────┬──────────────┘               │
    projects config/slots (async)                 provisions bundles + per-tenant keys      │
             ▼                                                │                              ▼
 ┌── GATEWAY (stateless edge) ── routes start/signal/ingress → owning scheduler shard · auth ─────────────┐
 └───────────┬─────────────────────────────────────────────────────────────────────────────────────────┘
             ▼ start / signal
 ┌── SCHEDULER TIER (thin · sharded · NO customer-DB access) ─────────────────────────────────────┐
 │  timer wheel + fires · admission gate · ingress terminus                                        │
 │  reads/writes ONLY its OWN durable store ────────────────────────────┐                          │
 │  MISFIRE guard: every run durably REGISTERED (wake_at) or IN-FLIGHT (deadline);                  │
 │                ack-timeout → re-dispatch (replay re-derives the timer — no customer scan)        │
 └──┬─────────────────────────────────────────────┬─────────────────────┼─────────────────────────┘
    │ dispatch "advance run X" (+ scoped tenant cred, affinity)          │ register/update (from worker ack)
    │                                              ▼                      ▼
    │                     ┌── SCHEDULER STORE (durable · platform-owned · sharded) ────────────────┐
    │                     │  timer index (run_id, app_id, wake_at) · in-flight (deadline)           │
    │                     │  fire-time METADATA only — no journal, no customer data                 │
    │                     └───────────────────────────────────────────────────────────────────────┘
    ▼                                                                                               ▲
 ┌── WORKER FLEET (FUNGIBLE · stateless · engine = plugin-workflow) ───────────────────────────────┐│
 │  claim (lease-CAS + nonce + fence) → read journal → replay+execute step (V8) → fold → apply       ││
 │  (commit journal + next wake_at ATOMICALLY in the customer _sys) → ACK next wake to scheduler ────┘│
 │  OVERFIRE guard: duplicate dispatch → claim-CAS lets one win; done steps memoized = no-op          │
 │  holds ONLY the dispatched tenant's _sys schema, transiently (per-dispatch RLS-scoped cred)        │
 └──┬──────────────────────────────────────┬──────────────────────────┬──────────────────────────────┘
    │ journal R/W (RLS, ONE tenant/dispatch) │ blob payload R/W         │ unwrap DEK, decrypt in-isolate
    ▼                                        ▼                          ▼
 ┌── CUSTOMER DATABASES (customer-owned · PG/SQLite/MySQL) ──┐  ┌ OBJECT STORAGE ┐  ┌ KMS ┐
 │  app_<id>        business data (env.db, app role, RLS)     │  │ .zship + blobs │  │ DEKs │
 │  app_<id>_sys    JOURNAL (tamper-proof; payloads encrypted;│  └────────────────┘  └──────┘
 │                  + wake_at atomic with the journal)        │
 └─────────────────────────────────────────────────────────────┘

 DEV (`zeroship serve`): ONE process = scheduler + worker + engine, SQLite for both stores,
                         no gateway/control/coordinator/auth. Same code, no cross-tenant anything.
```

Three roles: **control plane** = authority + admission + membership; **scheduler** = timers + dispatch over its *own* store; **worker** = execution + apply over the customer's journal.

## 3. The engine lives in `plugin-workflow`

The engine (fold + apply + store port + V8 executor) is what the **worker** runs; it lives in **`crates/plugin-workflow`**, linked into the **worker** (prod) and the **CLI** (dev) — and **not** into the scheduler or control plane (they never fold/apply). Because the control plane no longer runs the engine, putting it in the worker-side crate no longer drags V8 into control. Contents: `engine.rs` (pure `fold_outcomes` + DTOs + `WorkflowError`), `apply.rs` (the apply-tx orchestrator over the `WorkflowStore`/`Tx` port), `store/` (`PgStore`/`SqliteStore`/`MySqlStore`), `execute.rs` (the V8 replay/execute step), `env_workflows.rs` (the `env.workflows` client to the scheduler API). Anti-drift: worker and CLI link the **same** crate — one fold, tested with a `MemStore` + stub executor.

## 4. The scheduler tier (thin, owns its store)

`zeroship-workflow-scheduler` — **no engine, no V8, and no credential into any customer database.** It:

- **Owns a durable, sharded store of its own** holding two things per shard: a **timer index** `(run_id, app_id, wake_at)` and an **in-flight table** `(run_id, dispatch-deadline)`. This is *fire-time metadata only* — no journal, no customer data. It is populated by **register**, never by scanning tenant DBs.
- **Learns every timer by push:** a worker's **ack** reports the next `wake_at` (register/update); `start` registers a due-now timer; a `deliver-signal` ack registers the resulting due-now wake. So the scheduler never reads a customer DB to discover work.
- **Fires** due timers from a **timer wheel** over its store → **dispatches** "advance run X" (+ a per-dispatch scoped tenant credential) to a worker.
- **Handles** the control API (`start`/`signal`/`status`/`cancel`/…) and **external ingress** — always by dispatching a worker to do the durable write; the scheduler writes nothing to a customer DB.
- **Admission:** batched slot leases (from control) gate dispatch (per-tenant fairness).

A compromised scheduler shard exposes *fire-time metadata for its shard* (run ids + when they fire) — never journal contents, never a customer-DB credential. Sharding is by consistent hash over the scheduler fleet; ownership handoff reloads the scheduler's **own** store (§11), not a tenant DB.

## 5. The worker (fungible) and the apply transaction

A worker owns no apps, no timers, no membership. On a dispatch it:

1. **Claims** the run: an atomic claim-CAS installs `claimed_by=me` + a fresh worker-generated `dispatch_nonce` + `lease_epoch+1` + `leased_until = now()+ttl` (§5.1).
2. Reads the **journal** from the customer `_sys` schema, using the **per-dispatch tenant-DB-scoped credential** the scheduler handed it (§5.3) — only *this* tenant, transiently.
3. **Replays** `run()` in the app's deploy-pinned isolate (pin read from the journal — §8) and drives the ready **frontier** (§6.1) to `await`.
4. **Folds** (pure) → checkpoints + `RunUpdate` + the next timer frontier.
5. **Applies** in one customer-DB tx (§5.2): step(s), subscriptions, signal-consume, run-update, blob-ref, child hooks, cascade — **and the next `wake_at`, atomically with the journal**. So the journal and its `wake_at` never drift (single source of truth *in the customer DB*).
6. **Acks** `{ ack | nack, nextWakeAt? }` to the scheduler → the scheduler **registers** `nextWakeAt` in its store. The ack is a register, not a durability requirement: the customer-DB `wake_at` is the recovery truth (§6.3).

### 5.1 Run-lease lifecycle (crash-recovery primitive)
- **Claim-CAS:** `UPDATE run SET claimed_by=:me, dispatch_nonce=:new, lease_epoch=lease_epoch+1, leased_until=now()+:ttl WHERE run_id=:r AND (claimed_by IS NULL OR leased_until < now())`. Exactly one worker wins; a late/duplicate dispatch on an already-leased row **loses the CAS → clean no-op**. `dispatch_nonce` is worker-generated and durable in the CAS — never on the wire.
- **TTL** bounded by the per-isolate CPU/wall limit + apply slack (a single *step* can't outlive one isolate budget); **heartbeat renewal** advances the absolute `leased_until` one budget at a time for multi-step advances (a renewal touching zero rows ⇒ lease stolen ⇒ abort + discard buffered effects). The backstop gates on absolute `leased_until`, not lease *age*.
- **Fencing epoch** (`lease_epoch`): the apply CAS re-checks `claimed_by=me AND dispatch_nonce=mine AND lease_epoch=mine` — strictly stronger than `state` alone, so a stale holder is fenced even if `state='running'`.
- **Expiry ⇒ re-dispatch:** no lease is "released"; it expires, and the scheduler's ack-timeout re-dispatches (§6.3), the fresh claim fencing any zombie.

### 5.2 The apply transaction (guard matrix + fencing)
Fold **before** the tx; then lock **every run row the apply writes** (the dispatched run *and* any foreign runs) under `SELECT … FOR UPDATE` in **globally-ascending `run_id` order** (the dispatched run is not privileged — it sorts into the set); then

```
apply CAS = claimed_by=me AND dispatch_nonce=mine AND lease_epoch=mine AND state IN (…arm…)
```

Arms: **forward** (`running`), **compensation** (`compensating`), **requeue** (`running|compensating`), **rehydrate** (`stalled`), **terminal no-op** (`completed|failed|cancelled` → commit nothing, ack). Effects visible only at commit; a zero-row CAS discards buffered effects and acks a no-op.

- **Global lock order** kills the child-join ↔ cascade inversion: a child→parent join is dispatched on the child but its write set is `{parent, child}` with `parent_run_id < child_run_id`, so it locks parent-first — the *same* order a parent-initiated cascade uses. No two applies acquire an overlapping pair in opposite orders → no deadlock.
- **Recompute `min(frontier)` under the lock**, re-reading the run's pending-timers *and* undelivered-mailbox rows while holding its row `FOR UPDATE` — never from the pre-tx fold snapshot — so a concurrent cross-run apply (a signal delivery / parent-join) that committed a due-now mailbox row is folded into `wake_at`, never clobbered. (The fold governs replay-deterministic *step execution* and stays on the pre-tx snapshot; only the one value another run can concurrently invalidate — `wake_at`/`min(frontier)` — is recomputed under the lock.)
- **`stalled`** is a non-terminal *parked* state (unresolvable pinned bundle, §8): `wake_at` nulled, frontier + mailbox intact-but-suppressed; the **rehydrate arm** (`resume`/`restart`/redeploy re-pin) is the only path back to `running`.

### 5.3 New-run first-dispatch ownership
A new run becomes schedulable **atomically with creation** — no separable "register" to lose:
- **Children:** the parent's apply writes the child run + a **due-now `wake_at`** in the same tx; the parent's ack registers the child (a fast-path notify is a latency optimization, not correctness — a lost register self-heals via §6.3).
- **Child → parent join:** the child's terminal apply writes a **due-now parent `wake_at`** + a child-result **mailbox row** (`event_id = child_run_id`, unique `(run_id, event_id)`), locked parent-first per §5.2, deduped structurally (exactly like a signal, not replay-only).
- **Manual start:** `start()` → scheduler → **synchronous `start` dispatch** → the **worker** creates the run (`queued`, due-now `wake_at`) and acks the durable `runId`; the run row *is* the durable start (no separate "pending-start" shape). The scheduler writes nothing.
- **Cron fire-#1:** the owning shard issues a worker-executed **`install-schedule`** dispatch (execution arm of config projection) that reconciles the schedule row + writes the first `next_fire_at`; **self-healing** — the shard carries the declared schedule set from config and re-issues `install-schedule` (idempotent upsert) for any schedule whose row is absent, on the §6.3 cadence (first-fire skew ≤ one `reconcile_interval`).

The **per-dispatch credential is tenant-DB-scoped** (not single-run) so children/cascade/`replace` can write sibling runs in the shared `_sys` tables.

## 6. Timers — the register model

**`wake_at` is written atomically with the journal in the customer DB** (the recovery truth), and the **scheduler holds a *derived* copy in its own store**, kept current by register (worker acks / start / signal). The scheduler fires from its store; **it never reads the customer DB.** This is a self-healing derived index — the reconcile that makes the two consistent is §6.3, *not* a customer-DB scan.

### 6.1 Timer frontier (many timers per run)
A scalar can't represent a run's concurrency (a timeout racing a signal, concurrent sleeps, one apply arming many wakes), so each run carries a **frontier**: a pending-timers table `(run_id, timer_id, wake_at, kind)` **plus** its mailbox (a buffered signal / child-join result contributes a due-now readiness input). The run's `wake_at = min(frontier)` — the full timers-and-mailbox readiness — **unless `paused`/`stalled`/terminal** (which null `wake_at`; paused/stalled hold the readiness intact-but-suppressed, terminal drains it, §6.4). On fire the worker consumes what came due and re-arms `wake_at = min(frontier)` **under the run-row lock** (§5.2). Each apply's **ack registers the recomputed `wake_at`** with the scheduler. Because a buffered signal to a still-running far-sleeping run *is* the new minimum, it pulls `wake_at` down to due-now as a consequence of the invariant, never a violation.

### 6.2 The wheel (in the scheduler's store)
The scheduler's timer wheel (min-heap + generation-supersede + wakeable compio sleep) holds the near-term window of its **own store**; a loader pulls due entries from the store; far-future entries sit in the store until they approach. On ownership handoff the new owner reloads the wheel from the store (§11). **One clock authority** (the scheduler-store `now()` for firing; the journal `wake_at` is the authoritative due-time), and a worker **validates on advance** against the journal's `wake_at` (skips + re-registers if not actually due — clock-skew safe).

### 6.3 Misfire & overfire (the correctness core of the register model)
The timer lives in two places (customer journal = truth; scheduler store = derived), so the register model must beat both failure modes **without a customer-DB scan**:

- **Every run is always in one of two durable scheduler-store states:** `REGISTERED (wake_at=T)` or `IN-FLIGHT (deadline)`. Both recover.
- **Misfire (a due timer never fires):**
  - **Write-before-dispatch** — the scheduler records `in-flight` durably *before* sending a dispatch; no run is ever in flight without a durable record.
  - **Ack-timeout → re-dispatch** recovers a **lost register** (worker committed the journal `sleep until T` then crashed before acking): the in-flight deadline lapses → re-dispatch → the worker **replays the journal** → **re-produces the ack** → re-registers T. *The journal is authoritative; replay re-derives the timer — no scan.*
  - **Scheduler-store durability** survives a scheduler crash (reload `registered` + `in-flight`). The *only* residual misfire is loss of the scheduler's own store — handled by its replication/backup, with a **DR-only** (rare, off-hot-path) reconcile scan of journals as the last resort — the escape hatch, not the mechanism.
- **Overfire (a timer fires twice):**
  - **Journal-checkpoint level — structurally exactly-once:** any duplicate dispatch must win the **claim-CAS**; the loser no-ops; a re-dispatch after a successful-but-unacked advance replays and finds the step **memoized** → no-op / advance-past. Split-brain resolves the same way.
  - **External-effect level — at-least-once, mitigated:** an effect committed before its journal checkpoint can re-run on re-dispatch → per-step idempotency keys `(run_id, ordinal, attempt)` + the idempotent-steps contract (§10.1).
- **The tuning knob** — the ack-timeout is tied to the **lease TTL + heartbeat**: a slow-but-live worker renews its lease so it is *not* re-dispatched (avoids overfire); a dead worker stops heartbeating so its lease expires and it *is* re-dispatched (cures misfire).

### 6.4 Retry, timeout, terminal cleanup
Retry-backoff and step-timeout are ordinary frontier entries (a delayed `wake_at` from the fold; an `attempt` counter + `max_attempts` → dead-letter/`compensating`). A **terminal** apply nulls `wake_at`, **drains** the frontier + mailbox (a terminal run has no wait to deliver a buffered signal into), and the terminal no-op arm makes any late dispatch a clean no-op; the ack de-registers the run from the scheduler store.

## 7. Flows
- **Manual start** → scheduler → synchronous `start` dispatch → worker creates the run + acks `runId` (scheduler registers + returns it).
- **Cron** → `install-schedule` dispatch at deploy (self-healing) → thereafter the worker computes `next_fire_at`, acks it, the scheduler registers + fires.
- **Wake** → the scheduler's wheel fires a registered `wake_at` → dispatch → worker replays + advances → acks the next wake.
- **Signal** → capability token → scheduler ingress → `deliver-signal` dispatch → the worker persists the signal to the tenant mailbox + advances → acks a due-now/next wake the scheduler registers.

All funnel into the same **worker advance**; the scheduler decides *when* (from its own store) and *dispatches*.

## 8. Dispatch & ack protocol (scheduler ↔ worker)
`POST /__zeroship/internal/workflow-advance` — internal-only (404 public), mTLS.
```
{ kind, runId|scheduleId, appId, scopedTenantCred, payload? }
kind ∈ { start, advance, fire-schedule, deliver-signal, install-schedule,
         pause, resume, cancel, restart }
→ { ack | nack, nextWakeAt?, runId?, reject?, reason? }
```
- **Light:** carries a run reference + scoped credential, **not** the journal; `deployPin` is **not** on the wire (the worker reads it from the journal; the live version is captured into the journal at run creation, §10.5); `dispatch_nonce` is worker-generated in the claim-CAS, not on the wire.
- **`nack`:** an **unresolvable pinned bundle** → worker parks the run (`stalled`, nulls `wake_at`) + scheduler alerts (recovery = §5.2 rehydrate); admission back-pressure → a distinct `nack` the scheduler re-arms as a deferred future `wake_at` (run stays `running`).
- **Register on ack:** `nextWakeAt` updates the scheduler store; `runId`/`reject` carry `onConflict` results (§10.4). Control-op kinds fold like any advance: `pause` nulls `wake_at` + de-registers (readiness intact, no scalar stash); `resume`/`restart` **recompute `min(frontier)` under the run-row lock** (structurally seeing mid-pause buffered signals) and re-register.
- **Affinity:** the scheduler prefers a worker with the bundle warm (CHWBL hint), else any worker cold-loads the pinned bundle.

## 9. Client, protocol, auth
`@zeroship/workflows` is a pure JS SDK (authoring `Workflow` + steps + errors + schedule DSL; `env.workflows` over `fetch` + a worker-injected app-scoped token) reaching the scheduler's control API. **Surface separation:** `/__zeroship/workflows/*` is not on the public listener (fail-closed allowlist); scheduler + workers are private-network only; the browser's only door is RPC → the app's gated server function → `env.workflows` (bundle-stripped; app token required, end-user JWTs refused; `run.app_id` authz). **Auth:** app-scoped token (minted outside V8, gateway-verified + stamped); external capability tokens (AEAD-sealed, one-run scope, per-run-epoch revocation); mTLS internal.

## 10. Data architecture, security & isolation

### 10.1 The six stores + who touches them
| Data | Store | Writer | Reader |
|---|---|---|---|
| Run state, step checkpoints, signals/mailbox, subscriptions, schedules, blob-refs, **`wake_at`** | **customer `app_<id>_sys`** (tamper-proof, encrypted) | **worker** (RLS-scoped, per-dispatch) | **worker** |
| Timer index `(run_id, app_id, wake_at)` + in-flight `(deadline)` | **scheduler's own store** (sharded) | **scheduler** (register from ack/start/signal) | **scheduler** |
| Blob payloads; `.zship` bundles | **object storage** | worker / control | worker |
| Authority/config, membership, admission budgets, metering | **control-plane DB** | control plane (+ scheduler leases) | projected → scheduler/worker |
| Per-tenant DEKs | **KMS** | control plane | worker (unwrap → decrypt in-isolate) |
| App business data | **tenant business DB** (`env.db`) | app isolate (RLS) | app isolate |

**The access invariant:** the **worker** is the *only* thing that touches a **customer** database (the `_sys` journal), one tenant per dispatch, RLS-scoped, transient. The **scheduler** touches *only its own store* — **no customer-DB credential, no cross-tenant read.** That is the property this revision exists to guarantee.

### 10.2 The `_sys` boundary (why a per-app system schema)
The journal underpins exactly-once + billing, so app code must not forge/delete it. `env.db`'s role has RW to `app_<id>`; the journal therefore lives in a **platform-owned `app_<id>_sys` schema with no grants to the app role** — a write boundary encryption *cannot* provide (encryption gives confidentiality, not write-integrity). Per-app `_sys` gives clean isolation + portability (drop-schema = remove a tenant's workflows); a per-database `_wf`+RLS schema is the scalable alternative (fewer schemas, commingled + RLS) if per-app schema count ever bites. Either is platform-owned + app-unwritable. The signal **mailbox**, **subscriptions**, and **schedules** are `_sys` tables too (content, not fire-time) — worker-written, never in the scheduler store.

> **DEFERRED — schema placement (operator, 2026-07-09):** the separate `app_<id>_sys` schema is **not settled**. Operator preference is to keep the journal in the app's **own `app_<id>` schema** (one schema per app, not two). Viable, but the resolution must preserve at the *table* level the two properties `_sys` gave for free at the schema level: **(1) tamper-proofness** — journal tables owned by a platform/system role with **no DML grants to the `env.db` app-runtime role** and no ability to `DROP`/`ALTER` them (the app role has RW on its own schema, so table-level `REVOKE` + non-app ownership must carry the write boundary); **(2) name-collision avoidance** — a **reserved table-name prefix** (e.g. a `zs_`-family) creator migrations are forbidden to use, so platform journal tables can't clash with creator tables in the shared schema. Trade: schema-count down, grant-management complexity up. **Decision recorded, not yet folded into the sections above** — P2 must resolve it before the journal moves off the control-plane DB. This adds a third, preferred option to the §16.2 "per-app `_sys` vs per-database `_wf`+RLS" question: **single `app_<id>` schema + table-level isolation.**

### 10.3 Security & blast radius
- **Execution:** hardened V8 isolate per `(app, deploy)` (seccomp, side-channel mitigations, per-isolate limits) — no microVM; deploy-pinned replay from a bounded per-app pinned-isolate budget.
- **Data (primary defense):** end-to-end-encrypted journal — the worker decrypts only inside the tenant's isolate (key from KMS, zeroized after); a DB breach yields ciphertext.
- **Blast radius:** a **worker** compromise exposes one tenant's `_sys` journal, only for the lease window (per-dispatch scoped cred). A **scheduler** compromise exposes fire-time metadata for its shard — no journal, no customer-DB credential.
- **Integrity:** tamper-proof `_sys` + claim-CAS + fencing epoch + deploy-pin.

### 10.4 `onConflict` + key uniqueness
Partial unique index `(app_id, workflow, key) WHERE state live`: `join` resolves to the existing `runId`; `reject` returns a typed reject (§8); `replace` cancels/compensates the incumbent + creates the replacement in one tx (sibling-run write, §5.2 lock order). The `WHERE live` predicate lets a new run reuse a key once the prior run is terminal.

### 10.5 Side-effect contract, cold-load, continue-as-new, retention
- **Side-effect contract:** exactly-once *checkpoint*, **at-least-once effect** — steps/compensators MUST be idempotent; the runtime threads a deterministic `(run_id, ordinal, attempt)` idempotency key into `env.*`/`fetch`; optionally a write-ahead intent marker.
- **Cold-load:** resolve `deploy pin → blob hash` (version-addressed) into a pinned isolate from the per-app budget; an unresolvable pin `nack`s + parks.
- **Continue-as-new:** one atomic apply (terminal-for-generation + a fresh run seeded with carried state, re-pinned to the *live* deploy) bounding journal/replay growth; permitted only with an **empty pending-compensator set** (else `CompensableCarryError`) so a fresh generation never compensates across a boundary; the new `run_id` gives a clean `(run_id, ordinal, attempt)` namespace.
- **Restart:** appends a generation marker (replay skips superseded suffixes — never truncates the append-only journal) + `from`-validation/`RestartError`.
- **Bundle retention:** a control-visible per-deploy pinned-run refcount (worker-emitted drain events, conservative, ordered after the terminal commit).

### 10.6 Admission back-pressure
Park/backoff, never busy-loop: a throttled fire is re-armed as a future `wake_at`; slot release is tied to lease expiry (no leak); per-tenant fairness over the budget unit (`startMany` rate-limited); connected to the spend engine (Warn→Degrade→Block).

## 11. Long-running & dev
- **Long-running:** stateless-per-dispatch — an idle run is a journal row + a scheduler-store timer entry; **continue-as-new** bounds growth; bundles retained until pinned runs drain. Ownership churn is a non-issue: workers are fungible; the scheduler reloads its wheel from **its own store** on shard handoff (not a customer DB); a lost register self-heals via ack-timeout-redispatch (§6.3).
- **Dev:** the CLI links `plugin-workflow` and runs scheduler + worker in **one process** at N=1 over SQLite (both stores local), no gateway/control/auth. Same engine, same apply — closes the DW-21b parity gap.

## 12. Migration from the shipped V1
Each phase independently landable + e2e-green.
- **P0** — engine → `plugin-workflow` (move `fold_outcomes` + apply + `WorkflowStore`/`Tx` + `PgStore`, SQL lifted verbatim; retire the control-plane cron fold + `dev.rs` fold). Gate: full e2e green on PG.
- **P1** — the scheduler tier + its own store + the register/ack protocol; the worker gains the `advance` endpoint. Gate: keystone + chaos on scheduler↔worker, incl. **misfire** (kill worker pre-ack → re-dispatch recovers) + **overfire** (duplicate dispatch → no-op).
- **P2** — journal into the customer `app_<id>_sys` schema (tamper-proof), `wake_at` atomic with it; per-dispatch scoped credentials. Gate: tamper + portability + no-scheduler-customer-cred tests.
- **P3** — end-to-end journal encryption (per-tenant DEKs, isolate-only decrypt). Gate: DB-dump-yields-ciphertext.
- **P4** — continue-as-new + deploy-bundle retention.

## 13. Alternatives considered (retired drafts, distilled)
- **A. Fat central runtime** — a stateless runtime holds engine+journal+timers+apply, dispatching execution to workers. Rejected: journal shipped per dispatch; runtime needs read/write to *all* tenant journals (broad, sensitive). **Kept:** the engine/ports/apply/exactly-once-guard core (survived critic rounds 30→66).
- **B. Single-tier merged worker** — the worker owns apps + does everything in-process. Rejected as prod topology: kitchen-sink binary, loses fungible-stateless workers, coordinator secrets beside untrusted V8. **Kept:** prod=dev *in dev* (CLI runs both roles) + reversibility (engine=library behind ports).
- **C. DB-read scheduler** (this doc's prior revision) — the scheduler *scans* the tenant `_sys` light index for due timers. Rejected: it gives the scheduler a **standing cross-tenant read credential** into customer DBs. **Replaced by** the register model (§6): the scheduler owns its store, fed by acks, self-healed by ack-timeout-redispatch — no customer-DB access. **Kept:** `wake_at` still atomic-with-the-journal in the customer DB (the recovery truth); the scheduler store is a derived copy.
- **D. Platform-store journal (co-located coordinator)** — journal + timers co-located in a *platform-owned* store, written atomically (no cross-read, no dual-write) but gives up the tenant-owned journal. Rejected here to keep the execution history in the customer's own database; the register model recovers "no cross-read" at the cost of a self-healing derived index. (This is the classic durable-execution shape — a coordinator that owns both state and timers; we diverge only on *where the journal lives*.)

## 14. Relationship to the shipped V1
The committed `2026-07-05` design + plan built a feature-complete, e2e-green V1 (DW-00…DW-24: keystone 3/0, engine 26/0) with a central journal + control-plane cron. It remains the running system; this is the target it migrates to (P0 first — engine extraction). The phases are each e2e-green so nothing here breaks the V1 mid-flight.

## 15. Resolution ledger (the 21 evaluation gaps → where fixed)
A scenario evaluation (10 clusters) found the spine sound but 11 critical + 8 major + 2 minor deferred mechanisms; all are now defined in the body, and a critic-loop closed the residuals the fixes introduced. Index:

- **Lease lifecycle** (claim-CAS, per-step TTL, heartbeat, fencing epoch) → §5.1/§5.2 · **apply guard + global-`run_id` lock order + under-lock `min(frontier)` recompute** → §5.2 · **new-run dispatch ownership** (child due-now, child→parent structural dedup, durable start, self-healing `install-schedule`) → §5.3.
- **Timer frontier** (conditional `wake_at=min(frontier)` incl. mailbox) → §6.1 · **misfire/overfire** (durable registered/in-flight, ack-timeout-redispatch, claim-CAS + memoization, idempotency keys) → §6.3 · **retry/timeout/terminal cleanup** → §6.4.
- **Dispatch protocol** (no wire pin/nonce, nack + park, control-op kinds) → §8 · **signal durability + dedup** (worker-written mailbox, unique `(run_id,event_id)`, topic index, liveness sweep) → §5.3/§6.1/§10.5 · **side-effect idempotency** → §10.5.
- **Tenant-DB-scoped credential + blast radius** → §5.3/§10.3 · **`onConflict`** → §10.4 · **cold-load/continue-as-new/compensable-carry/restart/retention** → §10.5 · **admission back-pressure** → §10.6.

**Validated (do not regress):** atomic `wake_at`+journal in the customer DB; exactly-once *checkpoint* under at-least-once firing; fold-before-tx for step execution with the `wake_at`/`min(frontier)` recompute under the run-row lock; whole-write-set `FOR UPDATE` in global ascending-`run_id` order; reverse-order saga; scheduler holds **no journal** (only fire-time metadata, no customer-DB credential); encrypted tenant journal + isolate-only decryption; single-engine anti-drift; prod=dev reversibility; phased e2e-green migration.

**Model change since the evaluation:** the evaluation graded the **DB-read** scheduler; this revision moved to the **register** model (§13-C). The lease/apply/frontier/idempotency mechanisms are unchanged (they govern the worker's apply over the customer journal); what changed is the scheduler's data source (its own store via register/ack, not a tenant-DB scan) and the liveness path (ack-timeout-redispatch, not a customer-DB backstop) — §6.3.

## 16. Open questions
1. **Scheduler-store backend** — its own PG per shard vs. a shared store; DR-reconcile cadence for the rare scheduler-store-loss case (§6.3).
2. **`_sys` granularity** — per-app schema (portability) vs. per-database `_wf`+RLS (schema count) (§10.2). **Operator lean (2026-07-09, deferred): a third option — single `app_<id>` schema + table-level isolation (no `_sys`); see the §10.2 deferred note.**
3. **Affinity vs. any-worker** dispatch (cold-load cost vs. pure fungibility).
4. **Journal encryption posture** — full zero-knowledge vs. audited break-glass.

## Appendix — role/binary map
| Role | Binary | Engine? | Customer DB? | Own store? |
|---|---|---|---|---|
| Control plane | `zeroship-control` | no | no | authority/membership/admission |
| Scheduler | `zeroship-workflow-scheduler` | no | **no** | timer index + in-flight |
| Worker | `zeroship-worker` (+ `plugin-workflow`) | **yes** | **yes** (one tenant/dispatch, RLS) | no |
| Dev | `zeroship-cli` (+ `plugin-workflow`) | yes | yes (local SQLite) | scheduler+worker in one process |
