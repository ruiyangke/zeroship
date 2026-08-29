# Durable workflows: `@zeroship/workflows` — replay-per-dispatch engine on the control plane

- **Status:** DRAFT (uncommitted design; commits with the implementing PR-train per `feedback_proposal_workflow`).
- **Date:** 2026-07-05
- **Decision:** Add a first-class **durable workflow** primitive to the platform — creators author a `Workflow<Params, Output>` class in TypeScript, start **runs** with `env.workflows.X.start(...)`, and the engine drives them to completion across crashes, deploys, sleeps, and signals by **replaying** the workflow function once per dispatch against a **control-plane Postgres journal**. The engine is **zero-tokio** (compio/io_uring), **gateway-edge metered**, and **deploy-pinned**.
- **Scope:** DAY-1 the engine ships with **concurrent frontier execution** (a single dispatch may execute up to *N* independent frontier steps concurrently — §6). The single-frontier engine is the `concurrency = 1` special case of the same code path; there is no separate "add concurrency later" phase.
- **Scope (large & streaming outputs):** DAY-1 the engine also ships a **blob-backed output rail** (§17): a step result, the run input, or the run's final output that exceeds the 1 MiB inline journal cap spills to content-addressed object storage and is journaled **by-reference**, with an opt-in **streaming** handle (`StepOutputRef`) that never buffers the whole payload in the isolate. This adds **no** new frontier state, **no** new suspension reason, and **no** new creator-facing `env.*` primitive — it changes only the *representation* of a recorded output.
- **Scope (external signal ingress & broadcast):** DAY-1 the engine also ships an **external signal ingress + broadcast** rail (§18): a public, signed, rate-limited gateway-edge endpoint lets systems *outside* the app (a Stripe webhook, a partner callback, an IoT device) deliver a signal to a run **without** the app's control credential, and a topic **broadcast** fans one publish out to many runs matched by key — alongside today's point-to-point `run.signal` delivery. Both halves are **producers of journal rows only**; the single-frontier replay core (§5/§6) and `wake_at`-unified suspension (§8) are untouched, and every run stays individually deterministic off its own journaled bindings.
- **Pre-launch stance:** no back-compat shims, no `@deprecated` aliases, no `ALTER…backfill`. The journal DDL (§7) is an explicit wire contract; it lands directly in the `zeroship.workflow_*` create scripts and every producer/consumer changes in the same patch (per `feedback_no_backward_compat`).
- **No fabricated numbers.** Where throughput/latency is not yet measured, this doc says **unknown / to-measure**.

---

## 1. Motivation

Creator apps need work that outlives a single request: send-a-campaign fan-outs, multi-step
checkouts with a cooldown, "wait for the user to approve, then charge," nightly rollups. Doing
this on the bare runtime forces creators to hand-roll ret/idempotency/state around `env.db`,
and any worker crash or redeploy loses in-flight progress. A **durable workflow** primitive
makes the *happy-path code* the *durable code*: the creator writes ordinary `async` TypeScript;
the platform guarantees each step's *result* is journaled exactly once and the function is driven
to completion regardless of crashes, evictions, or deploys.

The engine is deliberately built on the same invariants as the rest of zeroship: **V8-per-thread
/ one isolate per app**, **zero tokio** (compio timers, bespoke drivers), **gateway-dumb /
worker-does-the-work**, **typed_id everywhere**, and **metering is infrastructure** (unforgeable,
emitted by the trusted worker, never by app code).

---

## 2. Goals / non-goals

**Goals.**

- Durable execution of creator-authored `Workflow` classes with per-step exactly-once *results*.
- Survive worker crash, LRU eviction, redeploy (deploy-pinning), and long sleeps/signals.
- Bounded **concurrent** frontier execution day-1 (§6) — I/O overlap inside one isolate.
- All persistence in the **control-plane Postgres** (`zeroship.workflow_*`), reusing the
  existing control-plane journal + advisory-lock discipline.
- Zero tokio; all timers/deadlines on compio.
- **Large & streaming step outputs** — a payload over the inline journal cap spills to
  content-addressed object storage, journaled by-reference and streamable on **both** the write and
  read sides (§3.5, §17), so a step never fails merely for producing a large-but-JSON result.
- **External signal ingress & broadcast** — a signed, rate-limited public edge lets an outside system
  deliver a signal to a run without the control credential, and a topic publish fans out to many runs
  (§3.6, §18), all as journal-row producers that never touch the replay core or app-forgeable metering.
- **Scheduled workflows day-1** — a typo-safe fluent schedule DSL (`every.monday.at(...)`) *and* raw cron,
  both compiling **build-time in the SDK** to one of two primitive stored shapes the control-plane sweep
  fires (§3.4, §12); scheduled runs are ordinary runs that add zero replay surface.
- **Compensation / saga rollback day-1** — attach `config.compensate` to any `step.run`; on a **terminal
  run failure** the engine walks the run's own journal in **reverse ordinal order** and runs each completed
  compensable step's compensator as its own durable, journaled, retried, at-least-once unit (§3.8, §21). It
  is the §5 dispatch loop with the frontier predicate flipped from *forward-pending* to *reverse-completed*;
  it adds a `compensating` phase between `running`/`sleeping`/`waiting` and `failed`/`cancelled`, and **no** new table,
  typed_id, or suspension reason — only annotation columns on `workflow_steps`, two columns + one state on
  `workflow_runs`, and a reversed frontier query.
- **Child / sub-workflow orchestration & batch fan-out day-1** — `step.call(WorkflowClass, input, opts?)`
  spawns (idempotently, deterministic key) and awaits a **child** run's typed `Output` from inside a run
  (`Promise.all`/`step.all` over it is fan-out/join via the §6 frontier); `env.workflows.X.startMany([...])`
  fans an idempotent batch out from outside a run (§3.7, §20). A child is an ordinary run — **no** new
  typed_id prefix, engine, or suspension reason (a `wait_signal`-flavored park + a reserved terminal hook).
- **Absolute-deadline sleep day-1** — `step.sleepUntil(name, when)`, the absolute-instant sibling of
  `step.sleep`, reusing `kind='sleep'` + `wake_at` with **zero DDL** (§3.2, §8).
- **Replay-from-step / restart day-1** — `run.restart({ from?, deploy? })` keeps the journaled prefix before a
  target step, drops the rest, and re-queues the same run; the one audited edge that legitimately revives a
  terminal run (§3.1, §7.10, §22). Four audit columns on `workflow_runs`, **no** new table/typed_id/state.

**Non-goals.**

- Not a general DAG/orchestration DSL — control flow is *ordinary JS*, structure is discovered by
  replay, not declared.
- Not multi-core parallelism. Concurrency is **cooperative I/O overlap**, not threads (§6, §16).
- Not cross-step external transactionality — the journal is atomic, external side effects are not
  (§16).
- Not a general file/object API — `StepOutputRef` (§3.5) is a step-output rail, not a replacement for
  `env.storage`; the bytes it addresses are workflow-owned journal data, not creator-visible objects.

---

## 3. Developer-facing API — `@zeroship/workflows`

A workflow is a class extending `Workflow<Params, Output>`. Its `run(trigger, step)` method is the
**replayable body**: it must be deterministic between `step.*` calls, and every side effect must be
wrapped in a `step`.

```ts
import {
  Workflow,
  type WorkflowTrigger,
  type Step,
  PermanentError,
  StepTimeoutError,
} from "@zeroship/workflows";

export class Checkout extends Workflow<{ orderId: string }, { charged: boolean }> {
  async run(trigger: WorkflowTrigger<{ orderId: string }>, step: Step) {
    // trigger = { input, startedAt, runId, workflowName }
    const order = await step.run("load", () => loadOrder(trigger.input.orderId));

    // Durable retry: config.retries.maxAttempts bounds re-runs of THIS step body.
    await step.run("reserve", { retries: { maxAttempts: 3 } }, () => reserveStock(order));

    // Durable sleep — the run suspends; no worker is held.
    await step.sleep("cooldown", "5m");

    // Wait for an external signal (type defaults to the name); bounded by timeout + freshness.
    const approval = await step.waitForSignal("approved", {
      type: "approved",
      timeout: "1h",
      maxSignalAge: "10m",
    });

    // waitForSignal resolves to SignalEnvelope<P> | null (null on timeout — §3.2/§3.6).
    if (!approval) throw new PermanentError("declined");
    await step.run("charge", () => charge(order));
    return { charged: true };
  }
}
```

### 3.1 Starting and controlling runs

Runs are started from app/handler code through the `env.workflows` namespace (a `@zeroship/workflows`
wrapper over the control-plane API; the engine itself lives in the control plane, §4):

```ts
import { env } from "zeroship";

const run = await env.workflows.Checkout.start({
  input: { orderId },
  key: `checkout:${orderId}`,   // dedup key (optional)
  onConflict: "join",           // "join" existing run (default) | "reject" (error) | "replace" (start a new run)
                                // — also accepts the object form { policy: "join" | "reject" | "replace" } (§3.1, C7)
});
// run: WorkflowRun, run.id === "run_…"

await run.signal({ type: "approved", payload: { by: userId } });

const { state, output, error } = await run.status();
await run.pause();
await run.resume();
await run.cancel();                              // default: mode "abort" — hard abort, NO compensation (§9)
await run.cancel({ mode: "compensate" });        // roll back completed compensable steps, THEN → cancelled (§3.8, §21)

// Replay-from-step / restart (§7.10): keep the journal prefix before `from`, drop it + everything after,
// re-queue. Creator/operator-credentialed (same tier as cancel/pause/resume), NEVER end-user (§22.4).
await run.restart({ from: { name: "charge" } }); // keep steps before "charge"; re-run charge → forward
await run.restart();                             // FULL restart (drop whole journal), deploy:"latest" (default)
await run.restart({ deploy: "started" });        // full restart onto the EXACT deploy the run started under
```

- `start({ input, key, onConflict })` mints a `WorkflowRun` with `id` `run_…` (typed_id). `key` +
  `onConflict` give **idempotent start** against the `UNIQUE (app_id, workflow_name, dedup_key)` guard (§7):
  `"join"` (default) returns the incumbent run handle, `"reject"` errors, and `"replace"` starts a **new**
  run. `onConflict` accepts either the bare string or the object form
  `{ policy: "join" | "reject" | "replace" }` (additive, room to grow, §3.1/C7). `"replace"` is one atomic
  control-plane txn — it **terminates the incumbent** holding `key`
  (→ `cancelled`, a hard abort; `mode: "compensate"` is *not* implied, so a replace does not roll back the
  incumbent's steps) and, in the same txn, **nulls that incumbent's `dedup_key`** to free the partial-unique
  slot, *then* inserts the new run carrying `key`. Terminate-null-insert commits together, so the partial
  UNIQUE is never transiently double-occupied and a concurrent `replace`/`join` resolves to a single live run
  for the key (the loser joins the winner's new run rather than seeing two live incumbents).
- `run.signal({ type, payload })` delivers a signal (persisted to `zeroship.workflow_signals`,
  §7) that a `step.waitForSignal(name, { type })` branch consumes.
- `run.cancel(opts?) / pause() / resume() / status()` are the run-lifecycle controls; `status()` returns
  `{ state, output, error }` (state ∈ §9). When `output` is blob-backed it is returned as a
  `StatusOutput` ref, not inlined (§3.5). `run.cancel()` defaults to `mode: "abort"` (a hard abort with
  **no** compensation); `run.cancel({ mode: "compensate" })` rolls back completed compensable steps before reaching `cancelled`
  (§3.8, §21). While `state === "compensating"`, `status().output` is absent and `error` carries a
  compensation progress annotation (`RunError.compensation`, §3.8). `run.cancel()` on a parent **cascades**
  to a live child only
  where that child was spawned with `step.call(..., { cascade: true })` (§3.7, §20.6); an
  independent child (the default) keeps running.
- `run.restart(opts?)` is **replay-from-step**: it keeps the journaled steps *before* a target step, drops
  the target + everything after, and re-queues the **same** run handle (same `run_…` id) so the next
  dispatch replays the retained prefix and executes forward from the target (§7.10). It is
  **creator/operator-credentialed, never end-user** — the same authorization tier as `cancel/pause/resume`
  (§22.4). Omitting `from` is a **full restart** (drop the whole journal, re-run from ordinal 0). It is the
  one control-plane transition that legitimately leaves a **terminal** state (`completed`/`failed`/
  `cancelled`) — a deliberate, audited revive (§9). The run **input is retained** (restart re-runs the same
  input; changing input is a new `start()`, not a restart).

```ts
interface RestartTarget {
  name: string;         // step name (matches workflow_steps.name)
  occurrence?: number;  // nth issuance of `name` in loops (= name_occurrence, §7.2). Default 0.
}
interface RestartOptions {
  /** Replay-from target. Omit → FULL restart (drop the whole journal, re-run from ordinal 0). */
  from?: RestartTarget;
  /** Deploy pin for a FULL restart only. Default "latest".
   *  "started" reproduces on the exact deploy the run started under; "latest" picks up the app's current
   *  active deploy. The object form `{ pin: "started" | "latest" }` is additive (room to grow).
   *  Rejected with RestartError when `from` is set — a retained prefix is only valid against the
   *  deploy that produced it (§22.3/§7.10). */
  deploy?: "started" | "latest" | { pin: "started" | "latest" };
}
interface WorkflowRun {
  // …existing: signal, cancel, pause, resume, status, createSignalToken…
  /** Keep journaled steps BEFORE `from`, drop the target + everything after, re-queue from there.
   *  Returns the same run handle (same run_… id), now re-queued. */
  restart(opts?: RestartOptions): Promise<WorkflowRun>;
}
export class RestartError extends Error {}  // target unknown / illegal deploy pin / partial-restart past a completed compensation / restart cap (sibling of PermanentError)
```

```ts
// Programmatic (owner app) — re-run everything from "charge" after a downstream fix.
const run = env.workflows.get(runId);            // rehydrate a handle to an existing run (as run.signal/cancel do)
await run.restart({ from: { name: "charge" } }); // keep load/reserve/cooldown/approval; re-run charge → return
await run.restart();                             // full restart, deploy:"latest" (pick up latest code)
await run.restart({ deploy: "started" });        // reproduce on the exact deploy the run started under
```
- `env.workflows.X.startMany([{ input, key? }])` fans a **batch** of idempotent runs out from *outside*
  a run in one control-plane round-trip — the outside-a-run peer of `step.call`; both ride the same
  `UNIQUE (app_id, workflow_name, dedup_key)` start guard (§3.7, §20.5).

### 3.2 The `step` surface

| Call | Durable semantics |
| --- | --- |
| `step.run(name, config?, fn)` | Run `fn` once, journal its result under `name`. Re-runs on crash/retry (at-least-once *effect*, exactly-once *result*). `config.retries.maxAttempts`, `config.timeout` (`StepTimeoutError`). `config.output` selects the output representation (`"auto"` default · `"inline"` · `"ref"` · `{ as: "stream" }`), returning the value or a `StepOutputRef` (§3.5). `config.compensate` attaches an **undo closure** run in reverse order on terminal run failure (§3.8, §21) — valid **only** on `step.run` (sleeps/signals produce no external effect). |
| `step.sideEffect(name, fn)` | Compute `fn` once and freeze the returned value into the journal. On replay, return the frozen value inline without re-running `fn`. This is the sanctioned way to capture inline non-determinism (`Date.now()`, random bytes, UUIDs, reading a small config value) when the work does **not** warrant a full `step.run`: no retries, no timeout, no compensation, no blob/stream output mode. Journal representation: one completed `workflow_steps` row with `kind='sideEffect'`, `output_kind='inline'`, and the value in `output`; no new columns. |
| `step.sleep(name, duration)` | Suspend the run until `now + duration`; `wake_at`-unified (§8). |
| `step.sleepUntil(name, when)` | Absolute-deadline sibling of `step.sleep`: suspend until the instant `when` (a `Date` or epoch **ms**). Same `kind='sleep'` / `wake_at` machinery (§8), differing only in that `wake_at = to_timestamptz(when)` (absolute target) instead of `now + duration`. One-sided guarantee: **never before `when`** (single DB clock), best-effort `≥ when`; a past `when` is a zero-length sleep. Zero DDL delta. |
| `step.waitForSignal(name, { type?, timeout, maxSignalAge })` | Suspend until a fresh matching signal arrives; `type` defaults to `name`; **resolves to `null` on `timeout`** (the timeout→null convention — the await does not throw, there is no thrown timeout class); `maxSignalAge` rejects stale signals. Returns `SignalEnvelope<P> \| null` (§3.6). |
| `step.all(steps, { concurrency? })` | **Durable, order-preserving `Promise.all`** — the ergonomic form of the concurrent frontier (§3.3, §6). |
| `step.call(WorkflowClass, input, opts?)` | Spawn (idempotent, deterministic key) + await a **child** run's typed `Output`; a child error rethrows into the parent. Modelled as a `wait_signal`-flavored suspension on the child's terminal join signal (§3.7, §20). `opts.cascade` opts into cancel-cascade; `opts.timeout` → `ChildTimeoutError`. |

Example:

```ts
const id = await step.sideEffect("id", () => crypto.randomUUID());
```

**Error classes.** `PermanentError` (non-retryable **business** failure — fail the run immediately),
`StepTimeoutError` (thrown on a per-step `config.timeout`), `ChildTimeoutError`/`ChildCancelledError` (§3.7),
`ChildLimitError` (fan-out/depth cap, §20.9), `RestartError` (§3.1), plus two **misuse** classes distinct
from the business `PermanentError` (B5): `WorkflowDefinitionError` (invalid config / author mistake — e.g. a
compensator on a `step.sleep`) and `LimitExceededError` (an output/size/batch cap — e.g. a blob over
`maxStepBlobBytes`, a `startMany` over `maxStartManyBatch`), plus engine-raised `NondeterministicError` and
`StalledError` (§11, §13). A thrown `PermanentError` transitions the run to `failed` even if the step still
had retry budget. **`waitForSignal`'s timeout is a return convention, not a thrown class:** its `timeout`
resolves the await to `null` (the app decides what to do), it does not throw — there is no exported
timeout-signal error symbol.

### 3.2.1 I/O & determinism contract

The workflow body may only observe the outside world through journaled `step.run` / `step.sideEffect`
output — never live I/O. A replay must be a pure function of `(workflow code, trigger, journal prefix)`;
anything observed outside the journal can differ on the next dispatch and corrupt the replay.

Wrong:

```ts
export class SyncOrder extends Workflow<{ id: string }, Order> {
  async run(trigger: WorkflowTrigger<{ id: string }>, step: Step) {
    const response = await fetch(`https://api.example.test/orders/${trigger.input.id}`);
    return await response.json();
  }
}
```

Right:

```ts
export class SyncOrder extends Workflow<{ id: string }, Order> {
  async run(trigger: WorkflowTrigger<{ id: string }>, step: Step) {
    return await step.run("load-order", async () => {
      const response = await fetch(`https://api.example.test/orders/${trigger.input.id}`);
      return await response.json();
    });
  }
}
```

Use `step.run` for external I/O and effectful work. Use `step.sideEffect` for lightweight inline
non-determinism whose value must be captured once and replayed thereafter.

### 3.3 Concurrency is opt-in-by-shape (`static concurrency` + `step.all`)

A bare `Promise.all` of `step.run` calls now **overlaps** — the engine detects the concurrent batch of
independent steps structurally (§6.1). `step.all` is the recommended, bounded, order-preserving form:

```ts
export class SendCampaign extends Workflow<{ campaignId: string }, { sent: number }> {
  // Effective per-dispatch concurrent-batch width for THIS workflow.
  // Clamped by the platform ceiling (§13). Omit → platform default.
  static concurrency = 8;

  async run(trigger: WorkflowTrigger<{ campaignId: string }>, step: Step) {
    const recipients = await step.run("load-recipients", () =>
      loadRecipients(trigger.input.campaignId),
    );

    // Concurrent batch of independent steps. Up to `concurrency` run at once per dispatch, the rest
    // roll to the next dispatch. Results returned in ISSUE order, never settlement order.
    const outcomes = await step.all(
      recipients.map((r) =>
        step.run(`send:${r.id}`, { retries: { maxAttempts: 3 } }, () => sendEmail(r)),
      ),
      { concurrency: 8 }, // optional local cap; min(local, static, platform ceiling) wins
    );

    return { sent: outcomes.filter((o) => o.ok).length };
  }
}
```

`step.all` is preferred over bare `Promise.all` because it (a) documents the concurrent,
unordered-effect semantics at the call site, (b) carries a local `concurrency` bound the engine reads,
and (c) pins the result-array order to **issue order** (never settlement order). The full API surface,
options type (`StepAllOptions`), and mixed-suspension composition are specified in **§6.2**. The two-line
semantic contract every creator must know (steps in one concurrent batch are **unordered**; effects are
**at-least-once**, amplified up to N× per crashed dispatch) is in **§6.3** and surfaced again in §16.

### 3.4 Scheduled workflows — `schedule` + the friendly DSL

`schedule` registers a recurring trigger that starts a fresh run of the named `Workflow` class each fire.
(It is named `schedule`, not `cron` — you don't "cron" an interval; the one registration function authors
both clock-time cron and fixed-interval cadences.) The schedule
row lives alongside the journal in the control plane; firing goes through the same gateway-edge metered
dispatch as any other run. A raw 5-field cron expression stays first-class, but a one-character typo
(`"0 9 * * 1"` vs `"0 9 * * 2"`) is silent until it misfires a week later — so `schedule` also accepts a
**typo-safe fluent DSL** (`every`) that is **pure build-time authoring sugar**: both the fluent builder and the raw string
**compile in the SDK, at build/deploy time, down to one of exactly two primitive stored shapes** the engine
sweeps. The engine never sees `every.monday`; it sees only `(cron_expr, tz)` or `(interval_ms, anchor)`.
The full compile pipeline, DDL, sweep mechanics, and correctness argument are **§12**.

```ts
import { Workflow } from "@zeroship/workflows";
import { schedule, every } from "@zeroship/workflows/schedule";

schedule({
  name: "nightly-report-us",                          // unique per (app, name)
  schedule: every.day.at("03:00", "America/New_York"), // clock-time → stored cron shape (DST-aware)
  workflow: NightlyReport,
  input: { region: "us" },                             // typed = Params of NightlyReport
});

schedule({ name: "poll-inbox",   schedule: every(15, "minutes"), workflow: PollInbox });   // stored interval shape
schedule({ name: "legacy-cron",  schedule: "*/5 * * * *",        workflow: HeartBeat });    // raw string, UTC
```

**The two primitive stored shapes.** Everything normalizes to one internal union — the *only* shape the
engine sweeps. To TS consumers `Schedule` is an **opaque, frozen descriptor** (a builder/`compileSchedule`
result you pass to `schedule({...})`); the `kind` discriminator is an **internal stored enum**, deliberately
**not** exposed as an exhaustive `"cron" | "interval"` union to switch on — so a future stored kind
(`rrule`, `solar`, §12/§16) is additive and never a breaking change for consumers:

```ts
/** Opaque to consumers; you never construct or switch on it — the builder / compileSchedule produce it. */
export type Schedule = { readonly __schedule: unique symbol };

/** INTERNAL stored representation (control-plane / SDK compiler only). The JSON-serializable shape written
 *  to zeroship.workflow_schedules (§7.9). Consumers do not import or discriminate on this. */
type StoredSchedule =
  | { kind: "cron";     expr: string; tz: string }        // POSIX 5-field, minute-granular, IANA tz, DST-aware
  | { kind: "interval"; everyMs: number; anchor: "epoch" | "deploy" };  // fixed period, DST-immune
```

The split is **semantic, not incidental**: cron cannot express "every 90 minutes" or sub-minute periods,
and a fixed interval cannot express "03:00 *local* time across DST." Neither is a superset, so both are
exposed, each for what it is good at.

| Author intent | Compiles to | Semantics |
| --- | --- | --- |
| "at a *clock time*" — `every.day.at("03:00")`, `every.monday.at(...)`, `every.hour()` | stored `cron` kind + IANA `tz` | Calendar-aligned, **DST-aware** |
| "every *N of a duration*" — `every(15,"minutes")`, `every(90,"minutes")`, `every(30,"seconds")` | stored `interval` kind | Fixed wall-clock period, **DST-immune**, non-cron periods |

**The fluent builder (`every`).** Typo-safety is three compile-time mechanisms: cadence names are real
object properties (`every.mondey` is a TS error), units are a string-literal union (`every(15,"minute")`
errors — it is `"minutes"`), and times are a template-literal type (`every.day.at("3:00")` errors;
`"03:00"` matches `${D}${D}:${D}${D}`). The `every.*` builder always **terminates through an explicit
method** — `every.day.at(...)`, `every.monday.at(...)`, `every.hour()`, `every.minute()` — never a bare
property, so timezone/jitter/window modifiers can be added to a terminal later without a breaking shape
change. Each terminal call returns a **frozen `Schedule` descriptor
directly** — no hidden AST — so `every.day.at("03:00","America/New_York")` *is*
`Object.freeze({ kind:"cron", expr:"0 3 * * *", tz:"America/New_York" })` (opaque to consumers).
`every.hour()`/`every.minute()` are terminal calls usable on their own, and `every.hour().at(30)` refines
the hour cadence.

```ts
// @zeroship/workflows/schedule
export type IntervalUnit = "seconds" | "minutes" | "hours" | "days";
type Digit = "0"|"1"|"2"|"3"|"4"|"5"|"6"|"7"|"8"|"9";
export type TimeOfDay = `${Digit}${Digit}:${Digit}${Digit}`;   // "HH:MM", 24h; range re-checked at compile
export type TimeZone  = string;                                 // IANA id; validated vs the pinned tzdb

interface Every {
  (n: number, unit: IntervalUnit): Schedule;         // every(15,"minutes"), every(30,"seconds")
  minute(): Schedule;                                 // "* * * * *"  (explicit terminal method)
  hour(): HourCadence;                                // "0 * * * *"; .at(30) -> "30 * * * *"
  readonly day: DayCadence;                           // .at("03:00", tz?) -> "0 3 * * *"
  readonly month: MonthCadence;                       // .on(1).at("00:00", tz?) -> "0 0 1 * *"
  readonly sunday: WeekdayCadence; readonly monday: WeekdayCadence; readonly tuesday: WeekdayCadence;
  readonly wednesday: WeekdayCadence; readonly thursday: WeekdayCadence; readonly friday: WeekdayCadence;
  readonly saturday: WeekdayCadence;                  // one typed property each — typo-safe by construction
}
interface DayCadence     { at(time: TimeOfDay, tz?: TimeZone): Schedule; }   // time REQUIRED (daily w/o clock = bug)
interface WeekdayCadence { at(time: TimeOfDay, tz?: TimeZone): Schedule; }
interface HourCadence extends Schedule { at(minute: number): Schedule; }     // minute-of-hour 0..59
interface MonthCadence   { on(dayOfMonth: number): { at(time: TimeOfDay, tz?: TimeZone): Schedule }; }  // 1..28 (§16)
export declare const every: Every;
```

**Raw cron stays first-class.** A bare string in `schedule({ schedule })` is parsed as 5-field POSIX (or a
named macro `@hourly`/`@daily`/`@weekly`/`@monthly`/`@yearly`) with `tz = "UTC"`. Use `cronExpr(expr, tz)`
for a timezone with a raw expression. Sub-minute cron and non-POSIX extensions (`L`, `#`) are **rejected**
at compile (§16).

```ts
export function cronExpr(expr: string, tz?: TimeZone): Schedule;              // tz default "UTC"
export class InvalidScheduleError extends Error {}                            // sibling of PermanentError et al.
export function compileSchedule(input: Schedule | string): Schedule;          // deterministic, pure, build-time
```

`compileSchedule` is the one pure function: a fluent object is already canonical (validate `tz` is a known
IANA zone, ranges, `everyMs ≥ floor`); a string is parsed + range-checked → `{ kind:"cron", expr, tz:"UTC" }`.
Any failure throws `InvalidScheduleError` **at build/deploy time, never at fire time** (§8, §12).

**`schedule(...)` registration signature.** `input` is required iff the workflow's `Params` is non-void;
`overlap` and `catchUp` policies are specified in §12:

```ts
type ScheduleInput = Schedule | string;
interface ScheduleOptions<W extends Workflow<any, any>> {
  name: string;                                     // unique per (app, name)
  schedule: ScheduleInput;
  workflow: new () => W;
  overlap?: "allow" | "skipIfRunning";              // default "allow"                       (§12)
  catchUp?: { mode: "skip" | "backfill"; max?: number }; // default { mode: "skip" }; max applies to "backfill" (§12)
  input: ParamsOf<W>;                               // collapses to input?: void when Params extends void
}
export function schedule<W extends Workflow<any, any>>(opts: ScheduleOptions<W>): void;
```

Because compilation is a pure SDK function and the engine only ever consumes the two primitive kinds, the
friendly surface and the engine evolve independently — new cadences (`every.quarter`, `every.weekday`) or
new stored kinds (`rrule`, `solar`) are additive (§12), exactly the native-primitive/npm split the platform
already enforces.

### 3.5 Large & streaming step outputs — blob-backed by-reference

The journal records each step's result as inline `jsonb` (`workflow_steps.output`), capped at **1 MiB**
— the inline cap that keeps control-plane Postgres pressure and per-dispatch replay reads tight. A step
whose result legitimately exceeds that (an image render, a CSV export, an LLM transcript, a scraped
page) rides a **blob-backed by-reference path**: the payload lands in content-addressed object storage, the
journal stores a small **reference**, and reads stream lazily. This subsection is the developer
surface; the engine mechanics, DDL, and correctness argument are §17. The by-reference path does **not** raise the
inline cap — it adds a reference path beside it.

**Automatic spill (default, zero API surface).** Any step whose serialized JSON output exceeds the
inline cap is **automatically** written to the workflow blob store and journaled by-reference; on
replay the developer gets the **identical value** back, rematerialized transparently. `step.run<T>`'s
contract is unchanged — same `T`, same replay semantics; the value merely round-trips through object
storage instead of `jsonb`. This is a correctness safety valve: a step never fails merely for
producing a large-but-still-JSON result.

**Opt-in by-reference handle (streaming / large-on-purpose).** When the output is known-large — or
should **stream without ever buffering the whole payload in the isolate**, or should avoid
rematerializing on every replay — the creator opts into a handle via `config.output`:

```ts
type StepOutput =
  | "inline"                                 // force inline; errors if > 1 MiB
  | "auto"                                   // default: spill only if > 1 MiB (above)
  | "ref"                                    // always by-reference; fn returns a JSON value
  | { as: "ref"; contentType?: string }      // JSON value, explicit content-type
  | { as: "stream"; contentType?: string };  // fn returns a byte stream, never buffered whole

// Public mode names say `ref`; the internal journal column keeps `output_kind='blob'` (B3, §7 / §17).
type BackoffPolicy = {                        // (new §3.2; no default numbers claimed — see §13)
  strategy: "exponential" | "fixed" | "linear";
  baseMs?: number; maxMs?: number; jitter?: boolean;
};

// The single generic StepConfig (A7): one shape, parameterized by the step's output T. `compensate` (§3.8)
// is typed against T. There is no non-generic alias.
interface StepConfig<T = unknown> {
  retries?: { maxAttempts: number };         // (§3.2; DB column stays snake `max_attempts`)
  backoff?: BackoffPolicy;                    // (new §3.2; retry backoff between attempts)
  timeout?: string;                          // (existing → StepTimeoutError, §3.2)
  output?: StepOutput;                       // (new; default "auto")
  // compensate?: … added in §3.8 (the full StepConfig<T> definition lives there)
}

// auto / inline  → the value itself (unchanged)
step.run<T>(name: string, fn: () => T | Promise<T>): Promise<T>;
step.run<T>(name: string, config: StepConfig<T> & { output?: "auto" | "inline" },
            fn: () => T | Promise<T>): Promise<T>;
// ref (JSON)     → a typed handle, NOT the value
step.run<T>(name: string, config: StepConfig<T> & { output: "ref" | { as: "ref"; contentType?: string } },
            fn: () => T | Promise<T>): Promise<StepOutputRef<T>>;
// stream (bytes) → an untyped byte handle
step.run(name: string, config: StepConfig<Uint8Array> & { output: { as: "stream"; contentType?: string } },
         fn: () => ReadableStream<Uint8Array> | AsyncIterable<Uint8Array> | Blob): Promise<StepOutputRef<Uint8Array>>;

// sideEffect → inline journaled value only; no retries/timeouts/compensation/output handles
step.sideEffect<T>(name: string, fn: () => T | Promise<T>): Promise<T>;
```

The handle is reconstructed from the journal row on **every** replay with no I/O; reads are lazy and
hit object storage:

```ts
/** Content-addressed reference to a step's blob-backed output. */
interface StepOutputRef<T = Uint8Array> {
  readonly ref: string;          // opaque, stable across replays: "wfblob:sha256:<64-hex>"
  readonly hash: string;         // 64-char lowercase sha256 hex (BlobStore addressing)
  readonly size: number;         // exact payload byte length
  readonly contentType: string;  // "application/json" for blob-JSON; caller-set for stream
  json(): Promise<T>;            // fetch + JSON.parse (materializes T in the isolate)
  text(): Promise<string>;
  arrayBuffer(): Promise<ArrayBuffer>;
  stream(): ReadableStream<Uint8Array>;  // lazy, backpressured; never buffers whole
}
```

Streaming both sides — the write `fn` returns a `ReadableStream` the engine pipes to storage while
hashing (never holding the whole payload), and the read consumes the ref lazily:

```ts
export class GenerateReport extends Workflow<{ month: string }, { bytes: number }> {
  async run(trigger: WorkflowTrigger<{ month: string }>, step: Step) {
    const pdf = await step.run(
      "render-pdf",
      { output: { as: "stream", contentType: "application/pdf" } },
      () => renderReportStream(trigger.input.month),   // ReadableStream<Uint8Array>
    );                                                 // pdf: StepOutputRef<Uint8Array>
    await step.run("upload", () => uploadTo(env.storage, `reports/${trigger.input.month}.pdf`, pdf.stream()));
    return { bytes: pdf.size };
  }
}
```

**Recommended pattern (documented, not enforced):** *read a blob ref inside a `step.run`, never in
orchestration code.* A read in orchestration re-executes (and re-fetches) on every dispatch; a read
inside a step executes once and the derived result is journaled. Reads are deterministic either way
(immutable content-addressed bytes — §17.6), so this is guidance, not a constraint.

**Run input and final output.** The by-reference path applies symmetrically. The **run input**
(`env.workflows.X.start({ input })`) auto-spills on `start()` when `JSON.stringify(input) > 1 MiB`
(`input_hash`/`input_size` on the run row, §7.1); `trigger.input` is rematerialized transparently each
dispatch. The **run's final output** follows the step rules (auto-spill by default; a `Workflow<…,
StepOutputRef<…>>` yields a by-ref final output). There is no by-ref opt-in for *input* — it
must be materialized to be passed to `run` (§16).

**Observing a blob-backed output.** `run.status()` does **not** inline a blob-backed `output`; it
returns a ref descriptor, and external callers stream the bytes via a control-plane read endpoint:

```ts
type StatusOutput<O> =
  | { kind: "inline"; value: O }
  | { kind: "ref"; ref: string; hash: string; size: number; contentType: string };
// status(): Promise<{ state: RunState; output?: StatusOutput<Output>; error?: RunError }>
```

`@zeroship/control` / the dashboard stream a blob-backed output via
`GET /v1/apps/{app}/workflows/runs/{runId}/output` (and `…/steps/{name}/output`), which authorizes on
the run's app and streams the blob from the workflow blob store — keeping large outputs off the JSON
status path entirely (§17.2).

### 3.6 External signal ingress & broadcast — signed public delivery + topics

Today's `run.signal({ type, payload })` (§3.1) requires the app's control credential — it is the *inside*
path. Two more producers of `workflow_signals` rows are day-1: an **external ingress** edge (an outside
caller delivers a signal to a run with an app-scoped *inbound* credential that is **never** the control
credential), and **broadcast / topics** (one publish fans out to many runs subscribed to a key). This
subsection is the developer surface; the engine mechanics, DDL, and correctness argument are §18. Neither
half touches the replay core — both only *produce* the same journal rows a run binds at `waitForSignal`.

**The `externalSignals` wall.** A workflow declares a **deploy-pinned allowlist** of signal types that may
arrive from outside the app. An external caller can *never* inject a type outside it — the wall between
"external input" and privileged internal transitions. A run can `waitForSignal("internal.approve")` and be
sure no webhook satisfies it unless the author explicitly opted that type into `externalSignals`.

```ts
import { Workflow, PermanentError } from "@zeroship/workflows";
import { env } from "zeroship";

// ── (a) External point-to-point ingress ──────────────────────────────
export class FulfillOrder extends Workflow<{ orderId: string }, { shipped: boolean }> {
  // Deploy-pinned allowlist: ONLY these types may arrive from outside the app.
  static externalSignals = ["payment.succeeded", "payment.failed"] as const;

  // Optional: declare the inbound verifiers this workflow accepts (else the app default).
  static inbound = {
    stripe: {
      verifier: "provider:stripe",          // foreign signature — Stripe signs with its own scheme
      topicFrom: "payload.data.object.metadata.orderId", // → topic `order:<orderId>`
      topicPrefix: "order",
    },
  } as const;

  async run(trigger, step) {
    const { orderId } = trigger.input;
    // An external POST to /__zeroship/signals/v1/run/<runId> (type=payment.succeeded)
    // satisfies THIS run's mailbox — identical bind to an in-app run.signal.
    const paid = await step.waitForSignal("payment.succeeded", {
      timeout: "24h",
      maxSignalAge: "10m",                   // ignore a delivery older than 10m at bind time (§8)
    });
    if (!paid) throw new PermanentError("payment window elapsed"); // timeout → null
    const shipped = await step.run("ship", { retries: { maxAttempts: 5 } }, () =>
      shipCarrier(orderId, paid.payload),
    );
    return { shipped };
  }
}

// Mint a narrow per-run callback token to hand to an external system.
const run = await env.workflows.FulfillOrder.start({ input: { orderId }, key: orderId });
const token = await run.createSignalToken({
  types: ["payment.succeeded", "payment.failed"],
  ttl: "48h",
});                                          // → "wst_…"; put it in your webhook router / Stripe metadata

// ── (b) Broadcast / topics ───────────────────────────────────────────
export class WatchPrice extends Workflow<{ symbol: string }, void> {
  static externalSignals = ["price.updated"] as const;

  async run(trigger, step) {
    // MANY runs subscribe to one topic key; one publish wakes them all.
    const tick = await step.waitForSignal("price.updated", {
      topic: `market:${trigger.input.symbol}`,
      timeout: "1h",
    });
    if (tick) await step.run("rebalance", () => rebalance(tick.payload));
  }
}

// Publish from inside the app (in-app fan-out).
await env.workflows.publish({
  topic: `market:${symbol}`,
  type: "price.updated",
  payload: { price },
  idempotencyKey: tickId,                    // exactly-once ingest per (app, topic, key)
});
```

`step.waitForSignal(name, opts)` gains one optional field, `opts.topic`: with it, the await subscribes to
a **topic** (`workflow_subscriptions`, §7.8) instead of the run's own `(run, type)` mailbox; everything
else — `type`, `timeout`, `maxSignalAge`, the `wake_at`-unified suspension (§8), the timeout→null
convention — is identical. The return type widens its provenance tags only — provenance is split into
**where it came from** (`origin`) and **how it was addressed** (`delivery`), never one conflated `source`
(A9):

```ts
type SignalOrigin   = "app" | "ingress" | "system";   // in-app publish · external edge · engine-internal
type SignalDelivery = "direct" | "topic";             // point-to-point (run,type) mailbox · topic fan-out
interface SignalEnvelope<P> {
  type: string; payload: P; receivedAt: string;
  origin: SignalOrigin; delivery: SignalDelivery;
}
// step.waitForSignal<P>(name, opts?): Promise<SignalEnvelope<P> | null>   // null on timeout
```

`run.createSignalToken({ types, ttl })` requests a stateless signed per-run capability token (`wst_…`) — a
**control-plane mint** (the ingress terminus signs it; the raw signing key never leaves the control plane),
not a worker-side signing. See the auth model (§18.1/§18.8). `env.workflows.publish({ topic, type, payload, idempotencyKey })` is the in-app
publish; the external equivalent is a signed `POST …/topic/{key}` (below). Both `run.signal` (§3.1) and the
external run-addressed POST land in the **same** `(run, type)` mailbox; a topic publish lands in
`workflow_broadcasts` and fans out to per-run delivery rows (§18.2).

#### External HTTP contract

```
POST https://{app}.zeroship.ai/__zeroship/signals/v1/run/{runId}[?delivery=waitingOnly]
POST https://{app}.zeroship.ai/__zeroship/signals/v1/topic/{topicKey}

Auth (one of — none is the control credential, §18.1):
  Zeroship-Signature: t=<unix>,v1=<hex>          # verifier: zeroship-hmac (per-app shared secret)
  Authorization: Bearer wst_…                     # verifier: bearer (per-run capability token)
  Stripe-Signature: t=…,v1=…                      # verifier: provider:stripe (foreign signature)
Content-Type: application/json
Idempotency-Key: <caller-event-id>                # optional; strongly recommended (exactly-once ingest)
delivery  (run/… only)  query param              # "buffered" (default): park the signal until the run reaches its
   ∈ { "buffered", "waitingOnly" }               #   await (bounded by maxSignalAge). "waitingOnly": 404 if not
                                                 #   already waiting. (room to grow: drop/replace/delayed later)
Body: { "type": "payment.succeeded", "payload": { … } }

202 Accepted   { "accepted": true, "delivered": <n> }               # written to journal
200 OK         { "accepted": true, "delivered": 0, "duplicate": true } # idempotent replay (no-op)
401            invalid signature / expired-or-forged token
403            type not in externalSignals, or outside the token's scope
404            run unknown, terminal, or (delivery=waitingOnly) not currently waiting
413            payload over cap
429            rate limited (per-app + per-source-token token bucket)
```

`delivered` is the number of runs the row(s) reached: `1` for a `run/…` bind (or `0` if buffered pending
the run reaching its await), and `N` for the `topic/…` fan-out that committed synchronously — the rest
completes via the claim sweep (§18.2). **No app code runs on the ingress path**: the gateway forwards to the
control-plane ingress terminus (§4), which verifies, meters, and writes journal rows; dispatch happens later
when the sweep picks up the woken run(s).

### 3.7 Child / sub-workflow orchestration & batch fan-out

A workflow can call **another workflow** and await its typed `Output`, and app code can **fan a batch of
runs out** from outside a run. Both reuse the idempotent-start guard (`UNIQUE (app_id, workflow_name, dedup_key)`,
§7.1) — the child path from *inside* `run()`, `startMany` from *outside*. A child workflow is **an
ordinary run** (`run_…`, walks the §9 state machine unchanged); `step.call(...)` is a
`wait_signal`-flavored frontier step that spawns the child idempotently, pins its `run_…` id in the
parent's journal, then parks on the child's terminal internal join signal — reusing §8 `wake_at`, §7.4
lease-guarded commit, and §18.4's signal safety-net verbatim. Fan-out/join is *literally* the §6
concurrent frontier over `kind='child'` steps. This adds **no** parallel engine, **no** new typed_id
prefix, and **no** new suspension reason. This subsection is the developer surface; the engine mechanics,
DDL, and correctness argument are §20.

#### `step.call` — call-and-await a child

```ts
interface ChildWorkflowOptions {
  /** Dedup-key SUFFIX; composed as `child:{parentRunId}:{stepOrdinal}` when omitted.
   *  Supply your own for a stable identity across code edits that reorder steps. */
  key?: string;
  /** false (default): child is INDEPENDENT — cancelling the parent leaves it running.
   *  true: run.cancel() on the parent cascades to this live child (§20.6). */
  cascade?: boolean;
  /** Optional join deadline. On elapse the await throws ChildTimeoutError (sibling of
   *  StepTimeoutError); the child keeps running unless cascade:true. */
  timeout?: string;
}

// Spawns (idempotently) and awaits WorkflowClass; returns the child's typed Output.
// If the child's Output is blob-backed (§3.5) the parent receives the StepOutputRef.
step.call<P, O>(
  WorkflowClass: new () => Workflow<P, O>,
  input: P,
  opts?: ChildWorkflowOptions,
): Promise<O>;
```

```ts
export class Checkout extends Workflow<{ orderId: string }, { charged: boolean }> {
  async run(trigger, step) {
    // Sequential child: parent parks until the child reaches a terminal state.
    const risk = await step.call(RiskScore, { orderId: trigger.input.orderId });
    if (risk.deny) throw new PermanentError("blocked");

    // A thrown child error rethrows INTO the parent as the same class:
    //   child failed(PermanentError) → step.call rejects with PermanentError
    //   child cancelled              → rejects with ChildCancelledError
    const receipt = await step.call(Charge, { orderId: trigger.input.orderId },
      { cascade: true });                 // cancelling Checkout cancels a live Charge
    return { charged: receipt.ok };
  }
}
```

`ChildTimeoutError` and `ChildCancelledError` are siblings of `StepTimeoutError` (§3.2 error classes).

#### Fan-out / join — `Promise.all` / `step.all` over `step.call`

Because `step.call` issues one frontier step **synchronously** (like every `step.*`), a
`Promise.all`/`step.all` of them is a §6.1 antichain frontier of `kind='child'` steps. Up to `effN`
children spawn per dispatch; the parent parks on all of them and joins when **every** child is terminal —
this is the §6 concurrent frontier + §8 unified wake, *unchanged*. No new join primitive.

```ts
export class FanOutInvoices extends Workflow<{ month: string }, { total: number }> {
  static concurrency = 8;
  async run(trigger, step) {
    const accounts = await step.run("load", () => loadAccounts(trigger.input.month));
    // N children, joined; results in ISSUE order (never settlement order), like step.all (§6.2).
    const results = await step.all(
      accounts.map((a) => step.call(InvoiceOne, { accountId: a.id, month: trigger.input.month })),
      { concurrency: 8 },
    );
    return { total: results.reduce((s, r) => s + r.cents, 0) };
  }
}
```

#### `startMany` — idempotent batch fan-out from **outside** a run

```ts
interface StartManyItem<P> { input: P; key?: string; }
type OnConflict = "join" | "reject" | "replace" | { policy: "join" | "reject" | "replace" }; // object form additive (C7)
interface StartManyOptions { onConflict?: OnConflict; } // default "join"

/** Per-item result envelope (C4) — reports created/duplicate/conflict per item, not a bare WorkflowRun. */
interface StartManyResult {
  run: WorkflowRun;                                   // the live run for this item (new or joined)
  created?: boolean;                                  // true = newly started; false = joined an existing run
  conflict?: "duplicate" | "rejected" | "replaced";   // per-item resolution when its key collided
}

// One control-plane round-trip; order-preserving; each keyed item is idempotent (§7.1).
env.workflows.X.startMany<P>(
  items: StartManyItem<P>[],
  opts?: StartManyOptions,
): Promise<StartManyResult[]>;
```

```ts
const results = await env.workflows.SendWelcome.startMany(
  newUsers.map((u) => ({ input: { userId: u.id }, key: `welcome:${u.id}` })),
);   // results[i] ↔ items[i]; results[i].run is the run, results[i].created flags new-vs-joined;
     // a retry of the whole call returns the SAME runs (exactly-once per key)
```

`step.call` (inside a run: awaited, joined, deploy-pinned to the **parent**, cascadeable) and
`startMany` (outside a run: detached handles, no automatic join, deploy-pinned to the app's **current**
deploy) are the two faces of the same idempotent-start guard. Neither adds a native `env.*` primitive —
both are `@zeroship/workflows` wrappers over the control-plane API (§4).

### 3.8 Compensation / saga rollback — undo completed steps on terminal failure

Some multi-step workflows must **undo** what they already did when a later step fails: release a stock
reservation if the charge never lands, refund a charge if the shipment can't be booked. A **compensator**
is attached inline on `step.run` via `config.compensate`. When a run reaches a **terminal failure**, the
engine walks the run's own journal in **reverse ordinal order** and executes the recorded compensator of
every **completed, compensable** step — each as its own durable, journaled, retried, at-least-once unit.
This subsection is the developer surface; the engine mechanics, DDL deltas, and correctness argument are
**§21**.

A compensator is an **ordinary closure captured in the workflow body** — reconstructed on every replay
exactly like the `fn` itself (deploy-pinned, §4), never serialized.

```ts
import {
  Workflow,
  type WorkflowTrigger,
  type Step,
  type CompensationContext,
  PermanentError,
} from "@zeroship/workflows";

export class Checkout extends Workflow<{ orderId: string }, { charged: boolean }> {
  async run(trigger: WorkflowTrigger<{ orderId: string }>, step: Step) {
    const order = await step.run("load", () => loadOrder(trigger.input.orderId));

    // Compensable step: if the RUN later fails terminally, `release` runs to undo `reserve`.
    const reservation = await step.run(
      "reserve",
      {
        retries: { maxAttempts: 3 },
        compensate: (out /* : Reservation */, ctx) => releaseStock(out.id, ctx.idempotencyKey),
      },
      () => reserveStock(order),
    );

    // Compensator with its own retry/timeout budget (long form) — the undo budget is
    // independent of the forward step's.
    const charge = await step.run(
      "charge",
      {
        compensate: {
          handler: (out /* : Charge */, ctx) =>
            refundCharge(out.chargeId, { idempotencyKey: ctx.idempotencyKey }),
          retries: { maxAttempts: 8 },
          timeout: "30s",
        },
      },
      () => chargePayment(order, reservation),
    );

    // A CAUGHT step failure is NOT a terminal run failure → nothing is compensated (§3.8.3).
    let receiptId: string | null = null;
    try {
      receiptId = await step.run("email-receipt", () => sendReceipt(order));
    } catch {
      receiptId = null; // handled; the run keeps going, no rollback
    }

    // A thrown PermanentError that is NOT caught escapes run() → terminal failure →
    // rollback fires: refund `charge`, then release `reserve`, in reverse order.
    if (!order.shippable) throw new PermanentError("cannot ship to region");

    return { charged: true };
  }
}
```

#### 3.8.1 Types

```ts
/** Runs at rollback to undo a completed step's external effect. Runs in the isolate with full
 *  env.* (db/kv/storage/fetch). MUST NOT call step.* — a compensator is itself the durable unit. */
export type Compensator<O> = (output: O, ctx: CompensationContext) => void | Promise<void>;

/** The full `StepConfig<T>` — the SAME single generic introduced in §3.5, shown here with `compensate`.
 *  There is one `StepConfig<T = unknown>` (A7); no non-generic alias exists. */
export interface StepConfig<T = unknown> {
  retries?: { maxAttempts: number };                 // (existing §3.2)
  backoff?: BackoffPolicy;                        // (existing §3.2)
  timeout?: string;                               // (existing §3.2 → StepTimeoutError)
  output?: StepOutput;                            // (existing §3.5; default "auto")
  compensate?:                                    // (new §3.8)
    | Compensator<T>
    | { handler: Compensator<T>; retries?: { maxAttempts: number }; timeout?: string };
}

export interface CompensationContext {
  readonly runId: string;          // run_…
  readonly workflowName: string;
  readonly stepName: string;       // the compensated step's name
  readonly ordinal: number;        // its journal ordinal (§7.2)
  readonly attempt: number;        // 0-based; this compensator body may re-run (at-least-once)
  readonly idempotencyKey: string; // STABLE per (run, ordinal): `${runId}:${ordinal}:comp`
  readonly trigger: WorkflowTrigger<unknown>;
  readonly cause: RunError;        // the terminal error (or { type: "Cancelled" }) that began rollback
}
```

`step.run`'s existing overloads (§3.2/§3.5) are unchanged except that `StepConfig` is now `StepConfig<T>`;
the `output`-handle overloads still resolve. `compensate` is **only** valid on `step.run` —
`step.sleep`/`step.waitForSignal` produce no external effect and take no compensator (enforced by type +
a runtime guard → `WorkflowDefinitionError` if smuggled in — an author mistake, not a business
`PermanentError`, B5). While `state === "compensating"`, `run.status()` is
unchanged in shape; `output` is absent and `error` carries a progress annotation:

```ts
type RunError = {
  type: string; message: string; stack?: string;
  compensation?: { total: number; completed: number; failed: number; outcome?: "completed" | "partial" };
};
```

#### 3.8.2 `run.cancel` gains an opt-in

```ts
interface CancelOptions {
  mode?: "abort" | "compensate";   // default "abort" (hard abort); "compensate" runs reverse-ordinal rollback
  reason?: string;                 // optional operator/author annotation, recorded on the run
}
// run.cancel(opts?: CancelOptions): Promise<void>

await run.cancel();                        // default: mode "abort" — hard abort, NO compensation (unchanged §9)
await run.cancel({ mode: "compensate" });  // roll back completed compensable steps, THEN → cancelled
```

Plain `cancel()` (or `{ mode: "abort" }`) is byte-identical to today (straight to `cancelled`, no app code re-run); the `mode: "compensate"` opt-in runs
the same reverse-ordinal rollback as a terminal failure, reaching `cancelled` instead of `failed`. The
rationale for the safe default is in §16.

#### 3.8.3 The three semantics every author must know

1. **Only *completed* steps with a compensator are rolled back, and only on *terminal* run failure.** A
   step that never completed (still running, failed, or whose rejection was caught) is not compensated. A
   run that `completed`, or that failed with **zero** compensable completed steps, never enters rollback.
2. **Rollback is reverse order (LIFO).** Compensators run in strictly decreasing ordinal order, so when
   compensator *N* runs, every step with ordinal `< N` is still in its committed state and *N*'s undo may
   rely on their outputs (§21, CC4).
3. **Compensator effects are at-least-once** (crash / wall-budget rollover / retry / lease handoff re-run
   the body). **Make them idempotent** — use `ctx.idempotencyKey` (see idempotency guidance below). This is
   the same honest guarantee as forward steps (§11 C3), never fixed by the engine.

#### 3.8.4 Idempotency guidance

Because compensator effects are at-least-once (§21 CC3), author them idempotently:

- **Use `ctx.idempotencyKey`** (stable `${runId}:${ordinal}:comp`) as the idempotency key on the external
  undo — e.g. `stripe.refunds.create({ charge }, { idempotencyKey: ctx.idempotencyKey })`, an `env.db`
  conditional delete keyed on it. The same key is presented on every re-run of the same compensator, so the
  provider dedups.
- **Prefer state-reconciling undos** ("release reservation `X` if held; else no-op") over blind decrements —
  naturally idempotent, robust to partial prior effects.
- **Derive the target from the step's recorded `output`**, not from live re-reads: `output` is the
  journaled, replay-stable value (or a `StepOutputRef`, §17.4), so the compensator addresses exactly what
  the forward step produced.
- **Do not rely on ordering with un-compensable siblings** — only compensable completed steps are undone;
  if an effect must be reversible, give its step a `compensate`.

---

## 4. Architecture

```
app handler ──env.workflows.X.start()──▶ Control Plane
                                          │  mint run_…  → INSERT zeroship.workflow_runs (deploy-pinned)
                                          ▼
                        ┌──────────────────────────────────────────┐
                        │  Dispatch scheduler (control plane)       │
                        │  wake_at-unified timer (compio)           │
                        │  + advisory-lock claim sweep (crash back) │
                        └──────────────────────────────────────────┘
                                          │  claim (lease) + hand run to a worker via gateway edge
                                          ▼
   Gateway (edge, metered) ─────────▶ Worker (V8, one isolate per app)
      one dispatch = one metered unit          │  REPLAY run(trigger, step) against journal prefix
      (wall_us, cpu_us, ingress, egress)       │  execute frontier (≤ N concurrent) → collect outcomes
                                               ▼
                        Control-plane Postgres journal (single atomic commit txn, §7)
                        zeroship.workflow_runs / _steps / _signals / app_deploys
```

**Where the engine lives.** Persistence and scheduling are **control-plane** concerns
(`zeroship.workflow_*`, reusing the control plane's Postgres + advisory-lock + typed-id discipline).
**Execution** is **worker** concerns (replay the workflow function in the app's own V8 isolate). The
**gateway stays dumb**: it forwards a metered dispatch to a worker and forwards the result; it holds no
workflow state.

**Who writes the journal — one topology (authoritative; every section obeys this).** The **control plane is
the sole owner, reader, and writer of `zeroship.workflow_*`.** No other component ever writes a journal row:

- **Forward dispatch.** The control-plane dispatcher **claims** the run, **loads** its journal prefix, and
  ships both to a worker over the gateway edge. The worker replays `run(trigger, step)` in the app isolate,
  writes any output blob directly to object storage (§17.2), and **returns a dispatch-completion envelope**
  (frontier outcomes + blob refs + subscription/consumption requests). The control plane then commits the
  §7.4 txn under the lease it holds. **The worker never opens a Postgres handle; app code never gets one.**
- **Signal ingress (§18).** The gateway forwards the public `POST /__zeroship/signals/v1/{run|topic}/{addr}`
  route to the **control-plane ingress terminus** — it does not run app code and does not touch the journal.
  The control plane verifies the inbound signature/token (it holds the ingress keys, reusing the shipped
  `crates/zeroship-control/src/stripe_handlers.rs` verifier), enforces the `externalSignals` allowlist + caps, **writes
  the `workflow_signals`/`workflow_broadcasts` row**, arms `wake_at`, and emits the accept-arm ingress meter.
- **Gateway's only edge role** for these routes is the cheap, stateless rate-limit token bucket (`env.kv`/redis)
  and forwarding — shedding floods before they reach the control plane. It verifies no signatures and writes
  no rows. This keeps the gateway dumb and the journal control-plane-owned, exactly as the forward path is.

So every producer of a journal row — a forward frontier commit, an external/broadcast ingress, a topic
subscription, a schedule fire, a child spawn/terminal hook, a compensation settle, a restart rewind — is a
**control-plane** write. Workers and the gateway are pure *reporters/forwarders*; app code has no handle.

**Signal-ingress edge (§18).** Per the topology above, the gateway forwards the public
`POST /__zeroship/signals/v1/{run|topic}/{addr}` route family to the control-plane ingress terminus and runs
**no app code**. The control plane verifies the inbound signature/token against the app's deploy-pinned
`externalSignals` + ingress keys, meters on the accept arm, **writes journal rows** (`workflow_signals` /
`workflow_broadcasts`), and arms `wake_at`. The woken run is dispatched later by the same claim sweep, so the
ingress edge is one more journal-row producer, not a second execution path.

**Replay-per-dispatch.** Each dispatch reloads the run's committed `workflow_steps` (the *journal
prefix*), re-runs `run(trigger, step)` from the top, and lets memoized steps resolve instantly from the
journal. Execution stops at the **frontier** — the first pending unmemoized `step.*` promise(s) — which
the dispatch executes, journals atomically, and then **interrupts** the function and re-dispatches. The
workflow function is never held across a suspension; there is no long-lived coroutine to keep alive
across a crash. This is what makes durability cheap: the *only* durable state is the journal.

**Deploy-pinning.** A run records the `deploy_id` it started under (`app_deploys`, §7). Every dispatch
replays against **that** deploy's code, so a redeploy mid-run cannot change the meaning of already-
journaled ordinals. New runs pick up the new deploy; in-flight runs finish on their pinned code.

The governing invariant this yields — **a retained journal prefix is only meaningful against the deploy that
produced it** — is what decides `run.restart`'s deploy-pinning (§3.1, §7.10). A **partial** restart (`from`
set → a prefix `0..t-1` is retained) is pinned **immutably to the original deploy** and is **not**
re-pinnable: the retained ordinals/names were produced by that exact code, so replaying them against it is
call-compatible (no `NondeterministicError`, §11); re-pinning to different code cannot be guaranteed to
reproduce the prefix byte-for-byte, so `deploy:"latest"` with `from` set is **rejected at the API boundary**
(`RestartError`). A **full** restart (`from` omitted → nothing retained) has zero journaled ordinals to be
incompatible with, so any pin is sound; it defaults to `deploy:"latest"` (re-pin to the app's current active
deploy — the semantics of a fresh `start()`; "new runs pick up the new deploy" above), with
`deploy:"started"` as the exact-reproduction escape hatch. When no redeploy happened between start and
restart, `current == original` and the choice is moot. On any actual re-pin (the `deploy_id` changes) the
restart txn bumps `signal_epoch` (§7.1) to invalidate outstanding `wst_` ingress tokens (§18.1), since the
new deploy's `externalSignals`/`inbound`/`topicFrom` allowlist may differ from the one they were minted
against.

**Lease + claim sweep.** A dispatcher **claims** a run by advisory-locking `run_id` and CAS-ing
`claimed_by := me, claim_epoch := e, lease_expires := now + lease_ttl`. A background **claim sweep**
(advisory-lock guarded) reclaims runs whose `lease_expires` has passed — the crash backstop. `lease_ttl`
is required to exceed `max_wall_budget` so the sweep never reclaims a legitimately-running concurrent
dispatch (§6.3, §13). The **same** sweep also drives resumable **broadcast fan-out** and **subscription /
broadcast GC** (§18.2/§18.6), and a peer **schedule sweep** (`zeroship.workflow_schedules`, §12) fires due
schedules by minting ordinary runs via `start()` — all under the same `claimed_by` lease + advisory-lock
discipline. No new scheduler is introduced: schedules are one more journal-row producer, and every run a
schedule fires replays through the untouched single-frontier core (§5). A run in the **`compensating`**
phase (§9, §21) is dispatched by this **same** sweep and holds the **same** `claimed_by` lease exactly like
a `running`/`sleeping`/`waiting` run — rollback is one more phase on the existing loop, not a new scheduler; a
crashed compensation dispatch is reclaimed by the identical `lease_expires` backstop.

**Zero tokio.** `wake_at` timers and per-dispatch deadlines are compio timers. No tokio anywhere in the
path (key invariant).

---

## 5. The dispatch loop (unified: single-frontier is `concurrency = 1`)

The dispatch is **replay-per-dispatch, interrupt-after-frontier**. The concurrent path (§6) generalizes
only the frontier *width* and the commit *cardinality*; the claim/replay/commit/interrupt skeleton is
shared. The single-frontier baseline is exactly this loop with `effN = 1`.

```
1. CLAIM       advisory-lock(run_id); CAS claimed_by/claim_epoch/lease_expires (§4).
               Load workflow_steps ORDER BY ordinal (the journal prefix).
2. REPLAY      run userWorkflow(trigger, stepShim), assigning a deterministic `ordinal` to each
               step.* call. Memoized ordinals resolve from the journal (determinism guard on `name`,
               §11); the first pending unmemoized promise(s) form the frontier; the function parks.
3. FRONTIER    Collect frontier candidates (§6.1). effN := min(local step.all cap, Workflow.concurrency,
               platform ceiling). activeBatch := first effN candidates in ordinal order; the rest roll
               to the next dispatch.
4. EXECUTE     Run the batch concurrently (one isolate, overlapping awaits on the compio loop) — §6.
5. BARRIER     Await all settled outcomes vs the per-dispatch wall deadline (claim_ts + wall_budget).
6. FOLD        Map outcomes → commit set + run transition (§9).
7. COMMIT      One atomic, lease-guarded, idempotent txn (§7.3).
8. INTERRUPT   Throw the frontier-interrupt to abandon the parked function; exit the isolate.
9. SCHEDULE    wake_at == now → immediate re-dispatch; future → wake_at-unified timer; terminal → notify.
```

The mechanics of steps 3–6 (frontier collection, the macrotask drain, the generalized barrier, and the
outcome fold) are specified in **§6**. Baseline equivalence (`effN = 1` ⇒ byte-identical to the
single-row engine) is argued in **§6.6 / §11 C8**.

**Compensation is this same loop, run in reverse (§21).** When a run reaches a terminal failure with ≥1
completed compensable step, it enters the `compensating` phase (§9) and re-dispatches through this exact
skeleton with the FRONTIER predicate (step 3) **flipped** from *"next forward pending unmemoized step"* to
*"next reverse-ordinal completed step whose compensation is not yet journaled."* Claim, replay, barrier,
fold, commit, interrupt, and schedule are unchanged. The full reversed-frontier mechanics are in **§21.2**.

---

## 6. Concurrent frontier execution

*Day-1 scope. Generalizes the single-frontier baseline to a bounded multi-frontier one **without**
changing the replay core, the at-least-once semantics, or any zeroship invariant. Concurrency here is
**cooperative I/O overlap inside one V8 isolate on the compio event loop** — not threads, not tokio, not
parallel CPU.*

**One-paragraph statement.** A single dispatch may execute up to **N independent frontier steps
concurrently** — where "independent" is a *structural* property (they were issued in the same replay pass
before the workflow function blocked on any of them, hence none awaited another), bounded by a
per-dispatch **wall budget**. All settled outcomes are collected by a generalized barrier and their
result rows are applied in **one txn** under the run lease. The single-frontier engine (§5) is the
`concurrency = 1` special case of this same code path.

### 6.1 Why it is safe by construction: the frontier is an antichain

The engine never analyzes data dependencies. It relies on a structural fact of single-threaded JS:

> **Frontier definition.** During a replay pass, the workflow function runs synchronously until it
> *blocks on an unresolved (unmemoized) step promise*. Every `step.*` call issued **before** that block,
> whose promise is pending, is in the frontier, in call order.

```ts
// frontier = {a, b, c}  — all three issued synchronously before Promise.all awaits any
await step.all([step.run("a", fa), step.run("b", fb), step.run("c", fc)]);

// frontier = {a} only — the await blocks before b is ever *called*
const a = await step.run("a", fa);
const b = await step.run("b", () => useOf(a));
```

**Theorem (frontier is a data-dependency antichain).** No two steps in the same frontier can have a
journal-mediated data dependency on each other. *Proof.* If step `b`'s arguments depended on step `a`'s
output, the code must `await step.run("a")` before it can *call* `step.run("b", …)`. That await blocks
the pass (`a` is unmemoized on the pass that first discovers it), so `b` is never issued and is not in the
frontier. ∎

Consequence: intra-frontier steps are provably independent **in the journal**. They may still touch
shared *external* state (two DB writes to one row) — that is an application concern and is **explicitly
unordered** (§6.3, §16). This antichain property is the entire correctness foundation for running them
concurrently.

### 6.2 API: `step.all` and mixed-suspension frontiers

```ts
interface StepAllOptions {
  /** Local frontier-width cap for this batch; clamped by Workflow.concurrency and platform ceiling. */
  concurrency?: number;
}

// Durable, order-preserving Promise.all. Resolves (across ≥1 dispatch) to outputs in the order the
// step promises were ISSUED — independent of real-time settlement.
step.all<T extends readonly unknown[]>(
  steps: readonly [...{ [K in keyof T]: Promise<T[K]> }],
  opts?: StepAllOptions,
): Promise<T>;
step.all<T>(steps: Iterable<Promise<T>>, opts?: StepAllOptions): Promise<T[]>;
```

Bare `Promise.all([...step.run...])` still works (the engine detects the frontier structurally) but has no
local cap and inherits `Workflow.concurrency`. The effective width is
`effN = min(step.all cap, Workflow.concurrency, platform_ceiling[tier])` (§13).

A concurrent group may **mix** `step.run`, `step.sleep`, `step.sleepUntil` (§3.2 — a `kind='sleep'`
member with an absolute-target `wake_at`, composing under the same `MIN(pending)` rule §8 with no special
case), and `step.waitForSignal`. Completing branches journal; suspending branches register a wake; the
group resolves only when **all** branches have resolved (possibly across many dispatches):

```ts
await step.all([
  step.run("charge", () => charge(order)),              // completes → journaled this dispatch
  step.sleep("cooldown", "5m"),                         // suspends → wake_at = +5m
  step.waitForSignal("approved", { type: "approved", timeout: "1h", maxSignalAge: "10m" }),
]);
// Dispatch commits `charge`, registers the sleep + signal waits, run → `waiting` (a signal branch is
// pending, so the §8 surfacing rule picks `waiting` over `sleeping`),
// wake_at = min(+5m, +1h). Resolves fully only once charge is done AND +5m elapsed
// AND `approved` arrives (or its 1h timeout fires → the branch resolves null).
```

`step.sleep`, `step.sleepUntil`, `step.waitForSignal`, `step.call` (a `wait_signal`-flavored child
await, §3.7/§20), `run.signal`, `run.cancel/pause/resume/status`, the error classes, and
`env.workflows.X.start(...)` (§3) are **unchanged** by concurrency — they simply become legal frontier members. The unified wake
(`MIN(pending)` with re-eval-all-on-wake) is specified in §8.

### 6.3 The two-line semantic contract creators must know

1. **Steps in the same concurrent frontier have no ordering guarantee relative to each other.** If you
   need `A` before `B`, `await` A first (put B in a later frontier). This is a **new** semantic surface vs
   single-frontier and must be prominent in creator docs.
2. **A step effect may run more than once** (crash / wall-budget rollover / retry / lease handoff). This is
   unchanged from single-frontier; concurrency can *amplify* it up to N× per crashed dispatch. Make step
   bodies idempotent (idempotency keys). Fundamental — not fixable by this engine (§16).

### 6.4 One concurrent dispatch — mechanics

Extends §5 steps 3–6. Only the frontier width and the commit cardinality change.

```
3. FRONTIER DRAIN
   After the workflow function parks and the microtask queue drains (a macrotask boundary — guarantees
   Promise.all's internal awaits have all been issued), frontierCandidates is stable and deterministic.
   effN := min(local step.all cap, Workflow.concurrency, platform ceiling).
   activeBatch := first effN candidates (ordinal order). Remainder stay parked → next dispatch.

4. EXECUTE activeBatch CONCURRENTLY (one isolate, overlapping awaits on the compio loop)
   run:         invoke fn() with per-step StepTimeoutError + retry policy; race vs dispatch_deadline
   sleep:       outcome = SUSPEND(kind=sleep, wake_at = now + duration)
   wait_signal: probe workflow_signals for a fresh matching unconsumed signal
                  present → COMPLETE(payload)  (mark signal consumed in commit)
                  absent  → SUSPEND(kind=wait_signal, wake_at = now + timeout, signal_type, max_signal_age_ms)

5. BARRIER (generalized never-settling latch — "collect all settled frontier outcomes")
   await raceWithDeadline( settleAll(activeBatch), dispatch_deadline = claim_ts + wall_budget ).
   Per candidate the terminal-for-this-dispatch outcome is one of:
     COMPLETE(output) | PERMANENT_FAIL(err) | RETRY_SCHEDULED(attempt+1, wake_at=+backoff)
     | SUSPEND(kind, wake_at, …) | UNSETTLED(deadline hit → NO row, re-run next dispatch)

6. FOLD OUTCOMES → commit set + run transition (§9)
   COMPLETE        → StepCompleted row (ordinal, name, output, attempt, batch_id, finished_at)
   PERMANENT_FAIL  → StepFailed(terminal) row (siblings still commit); the run does NOT flip to `failed`
                     in THIS fold — it commits the failed row and schedules an immediate re-dispatch so the
                     memoized-failed step's promise rejects at its point in program order. Terminal run
                     state is decided by whether that throw ESCAPES run() (→ `failed`, or `compensating`
                     when ≥1 compensator is pending) or is CAUGHT (run continues). This is the §9
                     terminal-failure fold refinement — required for `try/catch` and compensation (§3.8,
                     §21) to be observable; the common uncaught case is equivalent to "→ `failed`" modulo
                     one deterministic re-dispatch. `NondeterministicError`/`StalledError` skip it and fail
                     closed directly (§9).
   RETRY_SCHEDULED → step row state=running, attempt++, wake_at set (a suspension)
   SUSPEND         → step row kind=sleep|wait_signal, wake_at, signal_type/max_signal_age_ms
   UNSETTLED       → nothing (never journaled → re-discovered as a frontier candidate later)
```

**Determinism of the drain.** The macrotask boundary is only a *drain mechanism*, never a source of
nondeterminism: which steps land in `frontierCandidates` and their ordinals are a pure function of
`(workflow code, journal prefix)`, because all `step.*` issuance is synchronous up to the block, and
admitted step bodies are never awaited-into within the same pass (interrupt model). Timing affects only
*when fn bodies run*, never *which steps exist* or *their ordinals*. (Formal statement: §11 C1.)

### 6.5 Concurrency is not parallelism

Single V8 isolate per app, single JS thread on the compio loop (key invariant). N I/O-bound steps overlap
their **waits**; N CPU-bound steps still **serialize**. There is no speedup for CPU-bound work — by design.
"Concurrency" ≠ "cores." See §16 for the honest residual limits.

### 6.6 Baseline equivalence

With `effN = 1`, §6.4 step 3 selects exactly one candidate, step 5's barrier reduces to "await the single
outcome," step 6 commits one row — byte-identical to the single-frontier engine of §5. There is **one** code
path; single-frontier is `concurrency = 1`. The correctness proofs (§11) collapse to the single-row proofs
under `effN = 1` (C8).

---

## 7. Journal & DDL — `zeroship.workflow_*`

Pre-launch → these land directly in the `zeroship.workflow_*` create scripts; no shims, no `ALTER`
backfills. All ids are typed_id; ordinals are integers.

### 7.1 `zeroship.workflow_runs`

```sql
CREATE TABLE zeroship.workflow_runs (
  id             text        PRIMARY KEY,             -- run_… (typed_id)
  workflow_name  text        NOT NULL,
  app_id         text        NOT NULL,
  deploy_id      text        NOT NULL REFERENCES zeroship.app_deploys(id),  -- deploy-pinning (§4)
  state          text        NOT NULL,                -- §9 state machine (public run-state set, CHECK below)
  input          jsonb,                               -- inline run input; NULL when blob-backed (input_hash set)
  output         jsonb,                               -- inline final output; NULL when output_kind='blob'
  error          jsonb,                               -- {type,message,stack?}

  -- large / streaming outputs (§3.5, §17): final-output representation + blob-backed run input
  output_kind    text        NOT NULL DEFAULT 'inline',  -- 'inline' | 'blob'; blob ⇒ output IS NULL
  output_hash    char(64),                               -- sha256 hex of final output; NULL unless blob
  output_size    bigint,
  output_content_type text,
  input_hash     char(64),                               -- blob-backed run input (auto-spilled on start, §3.5)
  input_size     bigint,
  input_content_type  text,
  -- per-run footprint accumulators (§17.5), maintained in the commit txn (§7.4)
  journal_bytes  bigint      NOT NULL DEFAULT 0,          -- Postgres journal footprint (tight cap)
  blob_bytes     bigint      NOT NULL DEFAULT 0,          -- object-storage consumption (loose cap; → storage_bytes)

  -- suspension (unified — §8): MIN over all pending workflow_steps wake_at
  wake_at        timestamptz,

  -- lease / claim (§4)
  claimed_by     text,
  claim_epoch    integer     NOT NULL DEFAULT 0,
  lease_expires  timestamptz,
  last_dispatch_at timestamptz,

  -- concurrency (§6)
  concurrency    smallint    NOT NULL DEFAULT 1,      -- min(Workflow.concurrency, platform ceiling)
  next_ordinal   integer     NOT NULL DEFAULT 0,      -- high-water committed ordinal (progress witness)
  stuck_strikes  smallint    NOT NULL DEFAULT 0,      -- consecutive zero-progress dispatches (§11 C7)

  -- external signal ingress (§18): bump to invalidate all outstanding wst_ per-run tokens
  signal_epoch   integer     NOT NULL DEFAULT 0,

  -- child / sub-workflow orchestration (§3.7, §20). NULL parent_run_id = a root run. ON DELETE RESTRICT
  -- (NOT SET NULL): SET NULL would null parent_run_id while leaving parent_wait_step_key set, violating the
  -- all-or-nothing CHECK below. Retention prunes leaf-up — a parent cannot be pruned while a child row still
  -- references it (children reach terminal + are pruned first); so the child edge is never torn (§16).
  parent_run_id        text        REFERENCES zeroship.workflow_runs(id) ON DELETE RESTRICT,
  parent_wait_step_key text,                            -- reserved join type '__zs.child:<parentOrdinal>'
  parent_cascade       boolean     NOT NULL DEFAULT false,  -- opts.cascade: parent cancel → this child (§20.6)
  tree_depth           smallint    NOT NULL DEFAULT 0,      -- root=0; child = parent.tree_depth+1 (fork-bomb bound, §20.9)
  cancel_requested     boolean     NOT NULL DEFAULT false,  -- cooperative cascade cancel (§20.6)

  -- compensation / saga rollback (§3.8, §21). NULL unless state='compensating' or a terminal reached via rollback.
  compensation_target  text CHECK (compensation_target IN ('failed','cancelled')),   -- terminal to reach AFTER rollback
  compensation_outcome text CHECK (compensation_outcome IN ('completed','partial')),  -- set at terminal; 'partial' ⇒ ≥1 undo failed

  -- replay-from-step / restart (§7.10) — audit + abuse bound. No new index (target lookup uses the
  -- existing UNIQUE (run_id, name, name_occurrence), §7.2) and no new table (the run row IS the audit record).
  restart_count          smallint    NOT NULL DEFAULT 0,   -- per-run restart cap (§13)
  restarted_at           timestamptz,                      -- last restart instant
  restarted_from_ordinal integer,                          -- last restart target ordinal; NULL = full restart
  restarted_by           text,                             -- principal: app deploy cred id | operator id (op_…)

  -- idempotent start (§3.1)
  dedup_key      text,
  started_at     timestamptz NOT NULL,                -- planned FIRE instant → trigger.startedAt (deterministic replay clock, §9/§12); ≠ created_at
  created_at     timestamptz NOT NULL DEFAULT now(),  -- row INSERT instant (bookkeeping)
  UNIQUE (app_id, workflow_name, dedup_key),           -- start({key,onConflict}) guard; app-SCOPED (per-tenant) so two apps
                                                       -- sharing a workflow class name + key never collide; NULL key = no dedup
  CHECK (state IN (                                    -- the settled public run-state set (§3.2, §9); no 'suspended' mega-state (A3)
    'queued','running','sleeping','waiting','paused','stalled','compensating','completed','failed','cancelled'
  )),
  CHECK (                                             -- final-output shape invariant (§3.5, §17)
    (output_kind = 'inline' AND output_hash IS NULL)
    OR (output_kind = 'blob' AND output_hash IS NOT NULL AND output_size IS NOT NULL
        AND output IS NULL AND output_hash ~ '^[0-9a-f]{64}$')
  ),
  CHECK (input IS NOT NULL OR input_hash IS NOT NULL), -- run input is either inline or blob-backed
  CHECK ((parent_run_id IS NULL) = (parent_wait_step_key IS NULL))  -- child edge is all-or-nothing (§20)
);
-- unified wake sweep — the suspension states ('sleeping'/'waiting') + 'compensating' so rollback retry backoffs are swept (§8, §21).
CREATE INDEX ON zeroship.workflow_runs (wake_at)
  WHERE state IN ('sleeping','waiting','compensating') AND wake_at IS NOT NULL;
CREATE INDEX ON zeroship.workflow_runs (lease_expires) WHERE claimed_by IS NOT NULL;  -- claim sweep
-- terminal-child hook & cascade discovery: find a run's LIVE children by the parent edge (§20.3, §20.6).
CREATE INDEX ON zeroship.workflow_runs (parent_run_id)
  WHERE parent_run_id IS NOT NULL AND state NOT IN ('completed','failed','cancelled','stalled');
-- cooperative-cancel pickup at claim time (§20.6).
CREATE INDEX ON zeroship.workflow_runs (id) WHERE cancel_requested;
```

`parent_run_id`/`parent_wait_step_key`/`parent_cascade`/`tree_depth`/`cancel_requested` are the
child-orchestration additions (§3.7, §20): a child is an ordinary run with a journaled parent edge — the
columns record *provenance*, never a new state (§9). `tree_depth` bounds the fork-bomb (§20.9);
`cancel_requested` is the minimal cooperative-cancel flag the cascade walk sets (§20.6).
`compensation_target`/`compensation_outcome` are the saga-rollback additions (§3.8, §21): `state` gains a
`compensating` value (a dispatchable phase, §9), `compensation_target` records the terminal to reach after
the reverse walk finishes (`failed` for a terminal failure, `cancelled` for `cancel({ mode: "compensate" })`), and
`compensation_outcome` is stamped at the terminal (`partial` iff ≥1 compensator failed). No new table and no
new typed_id — rollback is annotation on the existing run + step rows.

`restart_count`/`restarted_at`/`restarted_from_ordinal`/`restarted_by` are the **replay-from-step / restart**
additions (§3.1, §7.10) — the **only** DDL delta the feature adds: four audit columns on the run row, **no**
new table (the run row is the audit record — consistent with the "no new table for concurrency" ethos; a
full restart-history log is an optional observability add filed under §16 terminal-run retention) and **no**
new index (the restart txn resolves its target ordinal through the existing `UNIQUE (run_id, name,
name_occurrence)` §7.2, and keys the run by PK; the step/subscription/signal drops reuse existing indexes).
`restart_count` bounds restarts per run (§13); `restarted_from_ordinal` is `NULL` for a full restart, the
target ordinal `t` for a partial one; `restarted_by` records the app deploy credential id (owner-app path)
or an operator id `op_…` (operator path). Restart never adds a state — it re-queues into `queued`
(`wake_at := now()`, §8) and lets the ordinary §5 loop run (§7.10, §9).

`started_at` (A1) is the run's **planned fire instant** — the deterministic clock a replay observes as
`trigger.startedAt` (§9/§11). For a `start()`ed run it is the instant the run is intended to begin (stamped
at insertion); for a **schedule fire** it is the *planned* instant, **not** wall-clock (§12.2). It is
distinct from `created_at` (the row-INSERT bookkeeping instant): the mapping is exactly
`trigger.startedAt ↔ workflow_runs.started_at`, frozen at creation so replay is deterministic.

`wake_at` is a **single** column (unified suspension): with multiple pending suspensions it holds
`MIN(wake_at)`; every wake re-evaluates *all* pending `workflow_steps` rows and recomputes the next
`wake_at` (§8). `concurrency`, `next_ordinal`, `stuck_strikes` are the concurrent-frontier additions; the
`output_*` / `input_*` representation columns and the `journal_bytes` / `blob_bytes` accumulators are the
blob-output-rail additions (§3.5, §17); `signal_epoch` is the external-ingress token-revocation counter
(§18.1) — bumping it invalidates every outstanding `wst_` capability token for the run.

### 7.2 `zeroship.workflow_steps` — the per-step journal

The primary key is the **deterministic ordinal**, which is what makes multi-row concurrent commits safe:

```sql
CREATE TABLE zeroship.workflow_steps (
  run_id           text        NOT NULL REFERENCES zeroship.workflow_runs(id) ON DELETE CASCADE,
  ordinal          integer     NOT NULL,              -- deterministic call index (journal order)
  name             text        NOT NULL,              -- observability + determinism guard (§11)
  name_occurrence  integer     NOT NULL DEFAULT 0,    -- nth issuance of `name` (loops)
  kind             text        NOT NULL,              -- 'run' | 'sideEffect' | 'sleep' | 'wait_signal' | 'child'
  state            text        NOT NULL,              -- 'running' | 'completed' | 'failed'
  attempt          integer     NOT NULL DEFAULT 0,
  max_attempts     integer     NOT NULL DEFAULT 1,    -- config.retries.maxAttempts
  output           jsonb,                             -- COMPLETE(run) result, inline; NULL when output_kind='blob'
  error            jsonb,                             -- {type,message,stack?} on failed
  output_kind      text        NOT NULL DEFAULT 'inline', -- 'inline' | 'blob' (§3.5, §17); blob ⇒ output IS NULL
  output_hash      char(64),                          -- sha256 hex; NULL unless blob
  output_size      bigint,                            -- payload bytes; NULL unless blob
  output_content_type text,
  wake_at          timestamptz,                       -- sleep/wait_signal/retry suspension
  signal_type      text,                              -- wait_signal (and kind='child': the reserved '__zs.child:<ordinal>' join type, §20)
  max_signal_age_ms bigint,                           -- wait_signal duration; cutoff is bound as timestamptz
  consumed_signal_id text,                            -- wait_signal / kind='child' → which signal satisfied it
  child_run_id     text,                              -- kind='child': the spawned child run (§20.2); read back on replay, never re-minted
  batch_id         text        NOT NULL,              -- wfd_… : the dispatch that committed this row (§14; wfd_ ≠ billing dsp_)
  batch_width      smallint    NOT NULL DEFAULT 1,    -- observability: concurrent frontier width
  started_at       timestamptz NOT NULL DEFAULT now(),
  finished_at      timestamptz,

  -- compensation / saga rollback (§3.8, §21): annotation on the FORWARD step row.
  -- NULL compensation_state ⇒ not compensable; a value ⇒ this COMPLETEd step.run carried a compensator.
  compensation_state        text,        -- 'pending' | 'running' | 'completed' | 'failed'  (NULL = none)
  compensation_attempt      integer     NOT NULL DEFAULT 0,
  compensation_max_attempts integer     NOT NULL DEFAULT 1,   -- config.compensate.retries.maxAttempts
  compensation_wake_at      timestamptz,                      -- retry backoff; feeds run wake_at MIN (§8)
  compensation_error        jsonb,                            -- {type,message,stack?} on terminal-failed undo
  compensation_batch_id     text,                             -- dispatch that settled the compensator (audit)
  compensation_finished_at  timestamptz,

  PRIMARY KEY (run_id, ordinal),
  UNIQUE (run_id, name, name_occurrence),             -- secondary human key / collision detect
  CHECK (                                             -- output-shape invariant (§3.5, §17)
    (output_kind = 'inline' AND output_hash IS NULL)
    OR (output_kind = 'blob' AND output_hash IS NOT NULL AND output_size IS NOT NULL
        AND output IS NULL AND output_hash ~ '^[0-9a-f]{64}$')
  ),
  CHECK (compensation_state IS NULL                   -- compensation-shape invariant (§3.8, §21)
         OR compensation_state IN ('pending','running','completed','failed')),
  CHECK (compensation_state IS NULL OR kind = 'run')  -- only step.run bodies are compensable
);
CREATE INDEX ON zeroship.workflow_steps (run_id, wake_at) WHERE state = 'running' AND wake_at IS NOT NULL;
-- Reverse-ordinal compensation frontier (§21.2 step 3): hot path, one run at a time.
CREATE INDEX ON zeroship.workflow_steps (run_id, ordinal DESC)
  WHERE compensation_state IN ('pending','running');
-- Compensator retry backoff → run wake_at MIN recompute (§8).
CREATE INDEX ON zeroship.workflow_steps (run_id, compensation_wake_at)
  WHERE compensation_state = 'running' AND compensation_wake_at IS NOT NULL;
```

Workflow durations are stored as integer milliseconds. For a signal-age probe,
the engine computes the cutoff instant from `max_signal_age_ms` and binds that
timestamp in the `created_at` predicate; it does not store or construct a
database-native duration value.

`ordinal`-as-PK + `kind` are shared with the single-frontier baseline; `batch_id` (the dispatch typed id
that grouped the atomically-committed concurrent rows) and `batch_width` are the concurrent-frontier
additions for audit/observability. **No new table is required for concurrency.** The `output_kind` /
`output_hash` / `output_size` / `output_content_type` columns are the blob-rail additions (§3.5, §17):
`output_kind='inline'` carries the result in `output`, `output_kind='blob'` carries a bounded
content-addressed reference (`output` is `NULL`) — a *representation* choice, not a new step lifecycle.
`kind='sideEffect'` uses the same inline output columns as `kind='run'`, but it is always a single
completed row: no retry budget, no timeout, no compensation, no blob/stream output mode, and no new
columns.
`child_run_id` + `kind='child'` are the child-orchestration additions (§3.7, §20): a `kind='child'` step
reuses the exact `running → completed|failed` lifecycle of `kind='wait_signal'` — `signal_type` holds the
reserved join type `'__zs.child:<ordinal>'`, `consumed_signal_id` the bound terminal signal, and the
inline/blob `output`/`error` columns carry the child's `Output` (blob-backed via §17 when large) or the
rethrown child error. **No** other new column — a child await is a `wait_signal` flavor, not a new kind of
row. The `compensation_*` columns are the saga-rollback additions (§3.8, §21): they annotate a *completed*
forward `kind='run'` row (a step that carried `config.compensate`) with its undo lifecycle
(`pending → running → completed|failed`) — so the durable set of things to undo already lives in the journal
before any failure, keyed by the **same** `(run_id, ordinal)` PK that makes forward memoization
exactly-once. Exactly-once compensation is therefore the same PK guarantee; **no** new row, **no** new
typed_id.

### 7.3 `zeroship.workflow_signals`

```sql
CREATE TABLE zeroship.workflow_signals (
  id              text        PRIMARY KEY,            -- sig_… (typed_id)
  run_id          text        NOT NULL REFERENCES zeroship.workflow_runs(id) ON DELETE CASCADE,
  type            text        NOT NULL,               -- matched against waitForSignal { type }
  payload         jsonb,
  created_at      timestamptz NOT NULL DEFAULT now(), -- maxSignalAge freshness check
  consumed_by     text,                               -- run_… once a wait_signal branch consumes it

  -- external signal ingress & broadcast provenance (§18); day-1 columns, not an ALTER.
  -- Provenance is SPLIT (A9): origin = WHERE it came from, delivery = HOW it was addressed.
  origin          text        NOT NULL DEFAULT 'app'        -- 'app' (in-app) | 'ingress' (external edge) | 'system' (engine-internal)
                  CHECK (origin IN ('app','ingress','system')),
  delivery        text        NOT NULL DEFAULT 'direct'     -- 'direct' ((run,type) mailbox) | 'topic' (broadcast fan-out)
                  CHECK (delivery IN ('direct','topic')),
  topic           text,                               -- non-null iff delivery='topic'
  broadcast_id    text        REFERENCES zeroship.workflow_broadcasts(id),  -- the fan-out parent
  idempotency_key text,                               -- caller-supplied or derived (exactly-once ingest)
  provider        text                                -- foreign source e.g. 'stripe', 'partner:acme' (ingress origin)
);
CREATE INDEX ON zeroship.workflow_signals (run_id, type) WHERE consumed_by IS NULL;

-- Exactly-once ingest per app+run+key for directly-addressed (delivery='direct') external signals (§18.5).
CREATE UNIQUE INDEX workflow_signals_ext_idem_uidx
  ON zeroship.workflow_signals (run_id, type, idempotency_key)
  WHERE idempotency_key IS NOT NULL AND delivery <> 'topic';
-- One delivery per (broadcast, run) — makes fan-out idempotent + resumable (§18.2).
CREATE UNIQUE INDEX workflow_signals_bcast_run_uidx
  ON zeroship.workflow_signals (broadcast_id, run_id)
  WHERE broadcast_id IS NOT NULL;
```

`origin`/`delivery`/`topic`/`broadcast_id`/`idempotency_key`/`provider` are the ingress-and-broadcast
additions (§18): a row is still the thing a run binds at `waitForSignal` — provenance only widens, split into
`origin` (`app` | `ingress` | `system`) and `delivery` (`direct` | `topic`). An
`origin='app', delivery='direct'` row is exactly today's `run.signal` (§3.1); `origin='ingress'` is an
edge-verified delivery; `delivery='topic'` is one fan-out leg of a `workflow_broadcasts` publish (§7.7); and
`origin='system'` is an engine-internal row (e.g. a child join, below). The two partial unique
indexes are the exactly-once guarantees (caller-retry dedup, and one-delivery-per-subscriber).

**Reserved internal join type (child orchestration, §3.7/§20) — no new column.** The terminal-child hook
(§20.3) writes a `workflow_signals` row with `origin='system', delivery='direct'`,
`type='__zs.child:<parentOrdinal>'`, and
`idempotency_key='__zs.child:<parentOrdinal>'` — so the **existing** `workflow_signals_ext_idem_uidx`
(`(run_id, type, idempotency_key) WHERE idempotency_key IS NOT NULL AND delivery <> 'topic'`) already
gives **exactly-once** join ingest under a retried child terminal txn. The only addition is one **validation
rule, not DDL**: `run.signal` (§3.1) and every external ingress path (§18) **reject** a user-supplied `type`
that begins with `__zs.` (`403`) — the wall that stops app code forging a child completion (§20.9).

`app_deploys` (deploy-pinning target of `workflow_runs.deploy_id`) is the existing control-plane deploy
record; workflows reference it, they do not redefine it.

### 7.4 The commit txn (idempotent, lease-guarded, atomic N rows)

Every dispatch commits **once** — the whole frontier's outcome (N COMPLETE rows + suspension registrations
+ run transition + signal consumption) or nothing:

```sql
BEGIN;
  -- Lease guard: we still own the run at this epoch, else abort → discard ALL N results.
  UPDATE zeroship.workflow_runs
     SET last_dispatch_at = now()
   WHERE id = $run AND claimed_by = $me AND claim_epoch = $epoch;
  -- 0 rows affected → RAISE → ROLLBACK (belt for a lease-handoff race)

  -- Atomic multi-row checkpoint. First-committer-wins on (run_id, ordinal).
  INSERT INTO zeroship.workflow_steps
      (run_id, ordinal, name, name_occurrence, kind, state, attempt, output, error,
       output_kind, output_hash, output_size, output_content_type,
       wake_at, signal_type, max_signal_age_ms, consumed_signal_id, batch_id, batch_width, finished_at,
       compensation_state, compensation_max_attempts)   -- (§3.8): 'pending' + budget for a COMPLETEd compensable run; NULL otherwise
  VALUES  (…N rows…)
  ON CONFLICT (run_id, ordinal) DO NOTHING;      -- memoization is exactly-once even under a double-commit

  -- Consume any signals that satisfied wait_signal branches (idempotent).
  UPDATE zeroship.workflow_signals SET consumed_by = $run WHERE id = ANY($consumed) AND consumed_by IS NULL;

  -- Co-commit GC refs for any blob-backed step/final-output rows in this batch (§7.5). Idempotent.
  INSERT INTO zeroship.workflow_blobs (hash, size, content_type, refcount, last_referenced_at)
  SELECT h, s, ct, 1, now() FROM unnest($blob_hashes, $blob_sizes, $blob_types) AS b(h, s, ct)
  ON CONFLICT (hash) DO UPDATE
    SET refcount = zeroship.workflow_blobs.refcount + 1, last_referenced_at = now();

  -- Run transition + unified wake + output representation + footprint accumulators.
  UPDATE zeroship.workflow_runs
     SET state = $state, output = $out, error = $err,
         output_kind = $out_kind, output_hash = $out_hash,
         output_size = $out_size, output_content_type = $out_ct,
         wake_at = $wake_at, next_ordinal = GREATEST(next_ordinal, $max_ordinal),
         journal_bytes = journal_bytes + $journal_delta,  -- fixed BLOB_REF_COST per blob row + octet_length for inline (§17.5)
         blob_bytes    = blob_bytes    + $blob_delta,     -- Σ output_size of blob rows written this dispatch
         stuck_strikes = $strikes
   WHERE id = $run AND claimed_by = $me AND claim_epoch = $epoch;
COMMIT;
```

`ON CONFLICT (run_id, ordinal) DO NOTHING` is the exactly-once guarantee on the **journaled result**: if two
dispatchers race at a lease boundary and both ran the effect, the first committer's `output` is canonical
and replay memoizes it. This single spot upgrades "N concurrent effects, at-least-once" into "N deterministic
journal rows, exactly-once" (§11 C2).

**Child-orchestration co-commits (§3.7, §20) — two additions, no new txn.** When the frontier being folded
contains a `kind='child'` step (a `step.call` invocation), the parent's *existing* lease-guarded txn above
also **spawns the child idempotently**: an `INSERT INTO zeroship.workflow_runs (… parent_run_id,
parent_wait_step_key, parent_cascade, tree_depth, dedup_key='child:<parent>:<K>' …) VALUES … ON CONFLICT
(app_id, workflow_name, dedup_key) DO NOTHING`, co-committed with the parent's own `kind='child'` step row (which
carries `child_run_id`). Child spawn and parent park are therefore **all-or-nothing under the parent lease**
(§20.2). Symmetrically, when a **child's own** §7.4 terminal txn (`completed`/`failed`/`cancelled`) finds
`parent_run_id IS NOT NULL`, that same terminal txn additionally `INSERT`s one reserved join signal
(`type='__zs.child:<K>'`, §7.3) and `UPDATE`s the parent's `wake_at = now()` — the **terminal-child hook**
(§20.3), gated on `parent_run_id IS NOT NULL`. Both edits ride commits that already exist; the txn count is
unchanged.

**Forward completion writes compensability (§3.8, §21).** A COMPLETE row for a `step.run` whose
`config.compensate` was present inserts `compensation_state = 'pending'` (plus `compensation_max_attempts`
from the compensator's own retry budget); a step without a compensator inserts `compensation_state = NULL`.
No other change to the forward path — the durable undo set is populated *at completion*, before any failure.

**The compensation commit txn (§3.8, §21) — a §7.4 variant, no new txn shape.** A `compensating` dispatch
(§21.2) commits with the **same** idempotent, lease-guarded, atomic shape as the forward txn; it **updates**
existing step rows rather than inserting new ones:

```sql
BEGIN;
  -- Lease guard (identical to the forward txn): 0 rows → RAISE → ROLLBACK, discard the whole dispatch.
  UPDATE zeroship.workflow_runs SET last_dispatch_at = now()
   WHERE id = $run AND claimed_by = $me AND claim_epoch = $epoch;

  -- Settle this dispatch's compensators. First-committer-wins per ordinal (state guard) makes the
  -- 'completed' marker exactly-once even under a lease-handoff double-run (§21 CC2).
  UPDATE zeroship.workflow_steps AS s SET
     compensation_state       = v.state,      -- 'completed' | 'failed' | 'running'
     compensation_attempt     = v.attempt,
     compensation_wake_at     = v.wake_at,     -- NULL for completed/failed; +backoff for running
     compensation_error       = v.error,
     compensation_batch_id    = $batch,
     compensation_finished_at = CASE WHEN v.state IN ('completed','failed') THEN now() ELSE NULL END
  FROM (VALUES …(ordinal, state, attempt, wake_at, error)…) AS v(ordinal,state,attempt,wake_at,error)
  WHERE s.run_id = $run AND s.ordinal = v.ordinal
    AND s.compensation_state IN ('pending','running');   -- never resurrect a 'completed'/'failed'

  -- Run transition + unified wake (§8) + terminal outcome.
  UPDATE zeroship.workflow_runs SET
     state = $state,                            -- 'compensating' (more remain) | 'failed' | 'cancelled'
     wake_at = $wake_at,                        -- MIN(compensation_wake_at) or now or NULL at terminal
     compensation_outcome = $outcome,           -- 'completed' | 'partial' (only at terminal)
     error = $error,                            -- carries {compensation:{total,completed,failed,outcome?}}
     stuck_strikes = $strikes
   WHERE id = $run AND claimed_by = $me AND claim_epoch = $epoch;
COMMIT;
```

The `compensation_state IN ('pending','running')` guard is the exactly-once marker: a lease-handoff
double-run cannot resurrect or double-settle a `completed`/`failed` row (§21 CC2). Blob-backed step outputs (§17)
that are rolled back keep their `workflow_blobs` refcount until the ordinary terminal-run retention prune
(§7.5/§16) — a compensator receives the rematerialized `output` (§17.4) but does **not** drop the ref;
rollback is undo of the *external effect*, not journal GC.

### 7.5 `zeroship.workflow_blobs` — blob-backed output GC ref index

The blob-backed output rail (§3.5, §17) journals a *reference* (`output_hash`) into `workflow_steps` /
`workflow_runs` while the bytes live in the workflow blob store (§17.3). This table is the GC domain's
reference index — refcount over the journal rows that name a given content hash:

```sql
CREATE TABLE zeroship.workflow_blobs (
  hash               char(64)    PRIMARY KEY,             -- content address (global within the wfblob/ namespace)
  size               bigint      NOT NULL,
  content_type       text        NOT NULL,
  refcount           integer     NOT NULL DEFAULT 0,      -- # of journal rows referencing this hash
  first_seen_at      timestamptz NOT NULL DEFAULT now(),
  last_referenced_at timestamptz NOT NULL DEFAULT now(),
  CHECK (hash ~ '^[0-9a-f]{64}$')
);
-- GC candidate scan: unreferenced blobs past the grace window (§17.6).
CREATE INDEX ON zeroship.workflow_blobs (last_referenced_at) WHERE refcount = 0;
```

Reference lifecycle — **all transitions co-commit with the journal write that causes them** (no
independent bookkeeping pass):

- **On frontier advance / run-input spill / final-output write** — the refcount upsert rides the same
  txn as the referencing row insert (the `INSERT … ON CONFLICT (hash) DO UPDATE SET refcount = … + 1`
  in §7.4).
- **On run/step retention prune** — `UPDATE … SET refcount = refcount - 1 WHERE hash = $1` in the same
  txn as the row delete.

A blob is GC-eligible only when `refcount = 0` **and** past its grace window (§17.6). `app_deploys`
(§7.3) is unchanged; workflow blobs are deploy-independent *data*.

### 7.6 `zeroship.workflow_signal_keys` — per-app inbound signing secrets

The external ingress rail (§18) verifies inbound signatures/tokens against **app-scoped** secrets that are
**never** the control credential (§18.1). Secrets are stored **encrypted at rest** (envelope-encrypted with
the platform data key — the same P5 machinery `zeroship.signing_keys` uses) and decrypted only in the
control-plane ingress terminus to verify (§4 — the gateway holds no secret); HMAC is symmetric, so the platform must hold the secret (mirroring how the Stripe verifier in
`crates/zeroship-control/src/stripe_handlers.rs` already works):

```sql
CREATE TABLE zeroship.workflow_signal_keys (
  id          text        PRIMARY KEY,               -- wsk_… (typed_id)
  app_id      text        NOT NULL,
  kid         text        NOT NULL,                  -- key id in the Zeroship-Signature scheme
  verifier    text        NOT NULL                   -- inbound scheme this key material serves
              CHECK (verifier IN ('zeroship-hmac','bearer-signing','provider:stripe')),
  secret_ct   bytea       NOT NULL,                  -- envelope-encrypted secret (P5 data key)
  secret_kek  text        NOT NULL,                  -- which KEK/version encrypted it
  status      text        NOT NULL DEFAULT 'active'  -- rotation lifecycle (mirrors signing_keys)
              CHECK (status IN ('active','next','retiring','retired')),
  created_at  timestamptz NOT NULL DEFAULT now(),
  rotated_at  timestamptz,
  retired_at  timestamptz,
  UNIQUE (app_id, kid)
);
CREATE INDEX ON zeroship.workflow_signal_keys (app_id, status);
```

The `bearer-signing` key material signs/verifies per-run `wst_` capability tokens (§18.1); `zeroship-hmac`
holds the per-app shared secret; `provider:stripe` reuses the app's stored Stripe webhook secret. The
`active`/`next`/`retiring`/`retired` lifecycle is the same rotation shape as `zeroship.signing_keys`.

### 7.7 `zeroship.workflow_broadcasts` — append-only published-message log

The exactly-once ingest point for a topic publish (§18.2). One row per published message; the fan-out to
subscribers is *derived* (`workflow_signals.broadcast_id`, §7.3). Create order: this table precedes
`workflow_signals` so the latter's `broadcast_id` FK resolves.

```sql
CREATE TABLE zeroship.workflow_broadcasts (
  id              text        PRIMARY KEY,            -- wbc_… (typed_id)
  app_id          text        NOT NULL,
  topic           text        NOT NULL,
  type            text        NOT NULL,
  payload         jsonb       NOT NULL,
  origin          text        NOT NULL                -- 'app' (env.workflows.publish) | 'ingress' (external edge)
                  CHECK (origin IN ('app','ingress')),
  provider        text,                               -- foreign source e.g. 'stripe' (ingress origin)
  idempotency_key text        NOT NULL,               -- required for broadcasts (exactly-once ingest)
  deploy_id       text        NOT NULL REFERENCES zeroship.app_deploys(id),  -- topic-def pinning (§18.5)
  fanout_state    text        NOT NULL DEFAULT 'pending'
                  CHECK (fanout_state IN ('pending','completed')),
  created_at      timestamptz NOT NULL DEFAULT now(),
  expires_at      timestamptz NOT NULL,               -- retention TTL (maxSignalAge ceiling, §18.6)
  UNIQUE (app_id, topic, idempotency_key)             -- exactly-once ingest
);
-- Sweep driver: pending fan-outs to resume (§18.2).
CREATE INDEX ON zeroship.workflow_broadcasts (app_id, topic, created_at) WHERE fanout_state = 'pending';
-- GC candidate scan: broadcasts past retention (§18.6).
CREATE INDEX ON zeroship.workflow_broadcasts (expires_at);
```

`fanout_state` is `pending` at ingest and set `completed` by the sweep pass that finds zero undelivered
subscribers. `expires_at` bounds retention: a subscriber that joins later than `maxSignalAge` after the
publish misses it (§18.7) — broadcast is live-plus-bounded-retention, deliberately not a durable queue.

### 7.8 `zeroship.workflow_subscriptions` — topic subscription registry

One row per `(run, topic)` while a run is suspended in `waitForSignal({ topic })` — written when the run
reaches the await; consumed/deleted when it binds a signal or the subscription expires (§18.2):

```sql
CREATE TABLE zeroship.workflow_subscriptions (
  id           text        PRIMARY KEY,              -- wsb_… (typed_id)
  app_id       text        NOT NULL,
  topic        text        NOT NULL,
  run_id       text        NOT NULL REFERENCES zeroship.workflow_runs(id) ON DELETE CASCADE,
  signal_name  text        NOT NULL,                 -- the await's name
  type_filter  text,                                 -- opts.type (default = signal_name)
  ordinal     integer     NOT NULL,                 -- the Nth await occurrence in this run (= step ordinal)
  max_age_ms   bigint,                               -- opts.maxSignalAge (for late-bind, §18.2)
  created_at   timestamptz NOT NULL DEFAULT now(),
  expires_at   timestamptz,                          -- = the await's timeout deadline
  UNIQUE (run_id, ordinal)                          -- one live subscription per await point
);
CREATE INDEX ON zeroship.workflow_subscriptions (app_id, topic);
```

`ordinal` is the await's deterministic step ordinal (§7.2), so a replay never double-subscribes at the
same await point (`UNIQUE (run_id, ordinal)`). The sweep GCs subscriptions past `expires_at` (§18.6).

**Grants (all three tables).** Per the `db/migrations/V00xx__*.sql` convention (a `DO $g$ … GRANT … TO`
role block guarded by `pg_roles` existence), and per the **one-topology rule** (§4: the control plane is the
sole journal writer): `zeroship_control` gets **full CRUD on all three tables** — `workflow_signal_keys`
(mint/rotate/revoke + decrypt to verify at the ingress terminus, §18.8), `workflow_broadcasts` (INSERT on
publish + UPDATE `fanout_state` + DELETE past `expires_at` in the sweep), `workflow_subscriptions`
(INSERT/DELETE/SELECT — written from the worker's dispatch-completion envelope in the §7.4 commit txn, GC'd by
the sweep), plus INSERT/UPDATE on `workflow_signals` and `UPDATE (wake_at)` on `workflow_runs`.
`zeroship_gateway` and `zeroship_worker` get **no journal-write grant on these tables**: the gateway forwards
the ingress route + rate-limits in `env.kv`/redis (no PG), and the worker reports a dispatch-completion
envelope (subscription upserts, signal consumptions, wake arming are all applied by the control plane in the
commit txn). This is the concrete DB-role expression of "the gateway is dumb, the journal is
control-plane-owned, app code has no handle."

### 7.9 `zeroship.workflow_schedules` — the schedule registry

One row per `(app, schedule name)`. Both the fluent DSL and raw cron compile (SDK, build-time, §3.4/§12) to
one of the two primitive `kind`s stored here; the control-plane schedule sweep (§12) reads `next_fire_at`.
Scheduled runs are **ordinary** `workflow_runs` rows — this table adds **no** run/step/signal structure; a
schedule is a run-*producer*, nothing more. It reuses `workflow_runs.dedup_key` +
`UNIQUE (app_id, workflow_name, dedup_key)` (§7.1) as the at-most-one-run-per-fire mechanism (§12); no new dedup
path is introduced. The typed-id prefix is `sch_` (`SCHEDULE_PREFIX = "sch"` in `crates/zeroship-core/src/typed_id.rs`,
pairwise-disjoint from existing 3-char prefixes).

```sql
CREATE TABLE zeroship.workflow_schedules (
  id            text        PRIMARY KEY,                                   -- sch_… (typed_id)
  app_id        text        NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
  deploy_id     text        NOT NULL REFERENCES zeroship.app_deploys(id) ON DELETE CASCADE,  -- deploy-pinning (§4)
  name          text        NOT NULL,
  workflow_name text        NOT NULL,

  kind          text        NOT NULL CHECK (kind IN ('cron','interval')),
  cron_expr     text,                     -- kind='cron' (normalized POSIX 5-field)
  tz            text,                     -- kind='cron' (IANA)
  interval_ms   bigint,                   -- kind='interval'
  anchor        text        CHECK (anchor IN ('epoch','deploy')),         -- kind='interval'

  input_json    jsonb       NOT NULL DEFAULT '{}'::jsonb,                  -- → trigger.input on each fire
  overlap       text        NOT NULL DEFAULT 'allow'
                  CHECK (overlap IN ('allow','skipIfRunning')),
  catchup       text        NOT NULL DEFAULT 'skip'
                  CHECK (catchup IN ('skip','backfill')),
  catchup_max   integer     NOT NULL DEFAULT 0 CHECK (catchup_max >= 0),

  next_fire_at  timestamptz NOT NULL,     -- planned instant driving the sweep (DB-clock computed)
  last_fire_at  timestamptz,
  paused        boolean     NOT NULL DEFAULT false,

  claimed_by    text,                     -- sweep lease owner (reused convention, §4)
  claimed_at    timestamptz,

  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now(),

  UNIQUE (app_id, name),
  CHECK (
    (kind='cron'     AND cron_expr IS NOT NULL AND tz IS NOT NULL
                     AND interval_ms IS NULL AND anchor IS NULL)
    OR
    (kind='interval' AND interval_ms IS NOT NULL AND anchor IS NOT NULL
                     AND cron_expr IS NULL AND tz IS NULL)
  )
);
-- Sweep hot path: due, not paused, ordered by planned instant.
CREATE INDEX ON zeroship.workflow_schedules (next_fire_at) WHERE NOT paused;
CREATE INDEX ON zeroship.workflow_schedules (app_id);
```

The `CHECK` makes the two-shape union a **stored invariant**: exactly one of the `cron`/`interval` column
groups is populated. The `input_json` and the planned `next_fire_at` are what the scheduler stamps
deterministically onto the runs it mints (`workflow_runs.input` and `started_at`, §12) — a scheduled run's
`trigger.startedAt` is the **planned fire instant, never wall-clock**, so replay stays deterministic (§11).
A new stored `kind` (e.g. `rrule`, `solar`, §12/§16) is an additive CHECK-guarded column group; pre-launch
there are no rows to migrate, so it lands directly in this create script. `zeroship_control` gets full CRUD
on this table (reconcile on deploy + the sweep, §12), guarded by the `pg_roles`-existence `DO` block that
the other `zeroship.workflow_*` scripts use.

### 7.10 The restart txn (`run.restart` — replay-from-step, advisory-lock + epoch-guarded)

`run.restart({ from?, deploy? })` (§3.1) keeps the journal prefix `ordinal < t`, drops the target ordinal `t`
and everything after, resets the run row, and re-queues it via `wake_at := now()` — then lets the ordinary
§5 dispatch loop replay `0..t-1` (memoized) and discover ordinal `t` as a fresh frontier candidate,
executing forward. **The replay core (§5/§6) is untouched**: restart only *edits a run's journal prefix +
run row*. It resolves the target ordinal `t` via the existing `UNIQUE (run_id, name, name_occurrence)`
(§7.2): `t = ordinal` of `(from.name, from.occurrence ?? 0)`; `from` omitted ⇒ `t = 0` (full restart). A
`(name, occurrence)` that names no journaled row → `RestartError` (nothing to rewind to). Restart operates
at **ordinal granularity, not batch granularity**: if `t` falls inside a concurrently-committed batch
(`batch_id`, §7.2) the surviving siblings (`ordinal < t`) stay and the dropped siblings (`ordinal ≥ t`)
re-execute — sound because the retained prefix is still a replay-deterministic antichain-consistent prefix
(§6.1 C1): the next replay re-issues the whole batch synchronously, memoizes the survivors, and re-runs the
dropped ones as frontier candidates.

It reuses the §4 claim discipline and the §7.4 lease-guard verbatim — **no new locking primitive**. The
`claim_epoch` bump is the lever that evicts any in-flight dispatch: its lease-guarded §7.4 commit
(`WHERE … claim_epoch = $epoch`) then matches 0 rows → `ROLLBACK`, so a concurrent dispatch cannot resurrect
dropped ordinals. One atomic control-plane txn:

```sql
-- run.restart({ from, deploy })  — control plane, one atomic txn.
BEGIN;
  -- 0. Serialize against claim/dispatch (SAME advisory-lock domain as §4) + evict any live dispatch (via claim_epoch++ below).
  SELECT pg_advisory_xact_lock(hashtext($run));

  -- 1. Resolve target ordinal t.  from omitted (name NULL) → 0.  No match → RAISE → 404 RestartError.
  --    t := (SELECT ordinal FROM zeroship.workflow_steps
  --            WHERE run_id=$run AND name=$name AND name_occurrence=COALESCE($occ,0));   -- or 0 for full

  -- 1b. GUARD (PARTIAL restart only, t>0): reject if any RETAINED ordinal (ordinal < t) has a COMPLETED
  --     compensation (compensation_finished_at set → 'completed'|'failed', §7.4). Forward replay memoizes the
  --     retained prefix as completed and never re-runs those bodies, so resuming forward would silently
  --     assume a reservation/charge the compensator already released/refunded. There is no safe partial
  --     revive past a settled rollback — RAISE → RestartError "cannot partial-restart past a completed
  --     compensation; use a full restart" (§22.1/§22.5). A FULL restart (t=0) drops the whole journal and
  --     is the sanctioned revive for a rolled-back run.
  --    IF $t > 0 AND EXISTS (SELECT 1 FROM zeroship.workflow_steps
  --                           WHERE run_id=$run AND ordinal < $t
  --                             AND compensation_finished_at IS NOT NULL)
  --       THEN RAISE;   -- → RestartError (no state is mutated: the guard precedes every write below)

  -- 2. Prune blob refs for dropped rows + a dropped blob-backed FINAL output (co-commit prune, §7.5).
  UPDATE zeroship.workflow_blobs b SET refcount = refcount - 1, last_referenced_at = now()
    FROM zeroship.workflow_steps s
   WHERE s.run_id=$run AND s.ordinal >= $t AND s.output_kind='blob' AND b.hash = s.output_hash;
  UPDATE zeroship.workflow_blobs b SET refcount = refcount - 1, last_referenced_at = now()
   WHERE $had_blob_final_output AND b.hash = (SELECT output_hash FROM zeroship.workflow_runs WHERE id=$run);

  -- 3. Restore signal-ledger consistency for dropped wait_signal steps:
  --    un-consume mailbox signals (re-bindable if still fresh), delete derived broadcast deliveries.
  UPDATE zeroship.workflow_signals SET consumed_by = NULL
   WHERE consumed_by=$run AND delivery <> 'topic'
     AND id IN (SELECT consumed_signal_id FROM zeroship.workflow_steps
                 WHERE run_id=$run AND ordinal >= $t AND consumed_signal_id IS NOT NULL);
  DELETE FROM zeroship.workflow_signals
   WHERE run_id=$run AND delivery='topic'
     AND id IN (SELECT consumed_signal_id FROM zeroship.workflow_steps
                 WHERE run_id=$run AND ordinal >= $t AND consumed_signal_id IS NOT NULL);

  -- 4. Drop dropped await subscriptions, then the step rows.
  DELETE FROM zeroship.workflow_subscriptions WHERE run_id=$run AND ordinal >= $t;
  DELETE FROM zeroship.workflow_steps         WHERE run_id=$run AND ordinal  >= $t;

  -- 4b. Normalize IN-FLIGHT compensation bookkeeping on RETAINED rows back to the clean 'pending' baseline
  --     (§21.1) — the guard (step 1b) has already rejected any partial restart whose retained prefix holds a
  --     SETTLED compensation (compensation_finished_at set), so no already-released reservation is silently
  --     resumed. Only an un-settled, mid-flight undo ('running', finished_at NULL) is reset: its effect never
  --     completed, so the reservation still holds and the step stays undoable by a FUTURE failure (undo is
  --     at-least-once — §6.3/C9; idempotency is the author's job). 'pending' and NULL are untouched; 'completed'/
  --     'failed' cannot occur here (rejected above). Full restart (t=0) retains no rows, so this is a no-op.
  UPDATE zeroship.workflow_steps SET
      compensation_state = 'pending', compensation_attempt = 0, compensation_wake_at = NULL,
      compensation_error = NULL, compensation_batch_id = NULL
   WHERE run_id=$run AND ordinal < $t AND compensation_state = 'running';

  -- 5. Reset + re-queue.  deploy_id: partial/started → unchanged; full+latest → app's current active deploy.
  UPDATE zeroship.workflow_runs SET
      state        = 'queued',
      wake_at      = now(),                     -- immediate re-dispatch via the claim/dispatch scheduler (§4) — no new trigger
      output       = NULL, error = NULL,
      output_kind  = 'inline', output_hash = NULL, output_size = NULL, output_content_type = NULL,
      compensation_target = NULL, compensation_outcome = NULL,  -- leave any rollback phase (§9); prefix re-derives forward
      next_ordinal = $t,                        -- high-water reset (retained ordinals 0..t-1)
      stuck_strikes = 0,
      claimed_by   = NULL, lease_expires = NULL,
      claim_epoch  = claim_epoch + 1,           -- EVICT any in-flight dispatch (its §7.4 commit fails the epoch guard)
      deploy_id    = $target_deploy,            -- old deploy_id (partial) | current active deploy (full + deploy=latest)
      signal_epoch = signal_epoch + $bump,      -- +1 IFF deploy re-pinned (invalidate outstanding wst_ tokens, §18.1)
      restart_count          = restart_count + 1,
      restarted_at           = now(),
      restarted_from_ordinal = CASE WHEN $full THEN NULL ELSE $t END,
      restarted_by           = $principal       -- app deploy cred id | operator id (op_…)
   WHERE id = $run;
COMMIT;
```

The run input (`input`/`input_hash`) is **retained** — restart re-runs the same input; changing input is a
new `start()`, not a restart. The blob-ref prune, signal un-consume/delete, subscription/step drops, and run
reset **all commit in one txn** — either the run is fully rewound or not at all (§11 C4). Deploy-pinning
follows §4/§22.3: partial → unchanged (pinned-original, immutable — `deploy:"latest"` with `from` is rejected
at the API boundary before this txn); full → `deploy:"latest"` picks the app's current active deploy
(default), `deploy:"started"` leaves `deploy_id` unchanged. `$bump = 1` iff `deploy_id` actually changes.

**Grants.** `zeroship_control` gets the DELETE/UPDATE-on-`workflow_steps` (drop `≥ t` + normalize retained
in-flight compensation bookkeeping) + DELETE-on-`workflow_subscriptions` + UPDATE-on-`workflow_runs`/`workflow_blobs`/`workflow_signals`
this txn needs, guarded by the same `pg_roles`-existence `DO` block as the other `zeroship.workflow_*` scripts
(§7.9 grants convention). **No new role.** Authz (owner-app deploy credential **or** platform operator) and the
control-plane endpoints are §22.

---

## 8. Suspension & signals — `wake_at`-unified

A run has at most one `wake_at`, holding `MIN` over every pending `workflow_steps` suspension (sleep,
wait_signal, or a scheduled retry). This is a **single** scheduling knob regardless of how many branches a
concurrent frontier suspended.

- **On suspend/register.** The committing dispatch (§7.4) sets `workflow_runs.wake_at = MIN(pending step
  wake_at)` and the public suspension state per the **surfacing rule**: `state = 'waiting'` if **any** pending
  branch awaits a signal or child (`kind IN ('wait_signal','child')`), else `state = 'sleeping'` (all pending
  branches are timers — `step.sleep`/`step.sleepUntil` or a retry backoff). ("suspension" stays the internal
  *mechanism* noun; there is no `'suspended'` state value — A3.)
- **On wake.** When the timer (compio) or claim sweep re-dispatches the run, the replay pass re-evaluates
  **all** pending step rows against `(now, signals)`: a satisfied sleep/retry resolves; a `wait_signal`
  probes `workflow_signals` for a fresh (`created_at ≥ now − maxSignalAge`), matching (`type`), unconsumed
  signal. A branch still unsatisfied re-registers; the run recomputes `wake_at = MIN(pending)`.
- **Composition.** A `step.all` mixing runs + sleeps + signals resolves iff **every** branch has independently
  resolved, across as many dispatches as needed. Because a suspending branch registers its wake atomically
  with its siblings' completions (§7.4), a wake can never be lost and a completed sibling can never be
  "un-completed" (§11 C6).
- **Signals** are delivered by `run.signal({ type, payload })` (§3.1) → an `INSERT` into `workflow_signals`;
  they are consumed at most once (`consumed_by` set inside the commit txn). The timeout→null resolution fires
  when a `wait_signal`'s `timeout` `wake_at` is reached with no matching fresh signal (the await resolves
  `null`, it does not throw — A4). The **external ingress edge**
  and **broadcast fan-out** (§18) `INSERT` the *same* `workflow_signals` rows (with `origin='ingress'` /
  `delivery='topic'`), then arm `wake_at`; a `wait_signal` branch binds them by the identical rule — the only
  new producers, never a new consumer.
- **`step.sleepUntil`** (§3.2) is the **same producer** as `step.sleep` — a `kind='sleep'` row whose
  `wake_at` is the resolved absolute target `to_timestamptz(when)` rather than `now + duration`. It is
  resolved **once, at first discovery**, written into the step row in the ordinary commit txn (§7.4), and
  folded into `workflow_runs.wake_at = MIN(pending)`. On wake it re-evaluates against `now` exactly like a
  sleep (satisfied → resolves `void`; else re-registers the same journaled `wake_at`). **Clock-skew
  honesty** — the guarantee is **one-sided**: the claim/wake predicate is `wake_at ≤ now()` on the
  **single DB clock** (Postgres `now()`, the one clock the whole engine uses, §12.2), so no per-node skew
  can make the deadline "due" early; the run is **never woken before `when`**. It resumes at the *first*
  `wake_at`-unified tick / claim-sweep pass with `now() ≥ when`, so wake is **best-effort `≥ when`, not
  on-the-dot** (sweep cadence + dispatch backlog latency **unknown / to-measure**, mirroring §12.4). If
  `when ≤ now()` at first discovery the fold sets `wake_at := now` and the run re-dispatches immediately (a
  past deadline is a zero-length sleep, not an error). No new outcome variant, suspension reason, or state,
  and **zero DDL delta** — it reuses `kind='sleep'`, `wake_at`, and the `(run_id, wake_at) WHERE
  state='running'` index (§7.2).
- **Topic waits** (`waitForSignal(name, { topic })`, §3.6) probe `broadcast_id` delivery rows instead of the
  run's `(run, type)` mailbox: when a topic await is first discovered on a dispatch and no delivery is yet
  bound, the worker registers a `workflow_subscriptions` row (§7.8), late-binds any retained
  `workflow_broadcasts` message still within `maxSignalAge`, and otherwise suspends on `wake_at = timeout`
  deadline exactly like a mailbox wait. The unified `MIN(pending)` recompute is unchanged.

- **Child awaits** (`step.call`, §3.7/§20) are a `wait_signal` **flavor**: the `kind='child'` step
  suspends exactly like a `wait_signal` on the reserved type `'__zs.child:<ordinal>'`, and the child's
  terminal §7.4 txn `INSERT`s that join signal + arms the parent's `wake_at` (§20.3) — the identical
  producer→`wake_at`→re-eval-all path as `run.signal`. `opts.timeout` sets the branch's `wake_at`
  (`ChildTimeoutError` on elapse); with no timeout the branch registers **no** `wake_at` and waits purely on the
  hook. §18.4's signal safety-net re-arms a lost parent wake — reused verbatim, no new machinery.

- **Compensation backoff** (`compensating` phase, §3.8/§21) joins the same `wake_at = MIN(pending)`: a
  compensator scheduled for retry writes `compensation_wake_at = now + backoff` on its step row (§7.2), and
  the compensation commit (§7.4 variant) sets `workflow_runs.wake_at = MIN(compensation_wake_at over
  'running')` (or `now` if a `pending` ordinal is ready). The compio timer / claim sweep re-dispatches the
  `compensating` run by the identical path as a `sleeping`/`waiting` run — rollback retries are one more producer of
  the wake a dispatch re-evaluates, not a new timer.

The `wake_at`-unified model is unchanged in kind by concurrency, by external/broadcast ingress, by child
orchestration, **or** by compensation; all only add producers of the journal rows a wake re-evaluates.

---

## 9. Run state machine

```
── Core lifecycle ───────────────────────────────────────────────────────────────────────────
     start() · schedule fire (§12) · child SPAWN (§20.2)
                    │
                    ▼
                ┌─────────┐  claim + dispatch   ┌──────────┐  fn RETURNED (all memoized)   ┌───────────┐
                │ queued  │────────────────────▶│ running  │─────────────────────────────▶ │ completed │
                └─────────┘                     └────┬─────┘                                └───────────┘
                    ▲                                │ commit (§7.4) fold (§6.4 step 6)
        resume()    │                                ▼
              ┌──────┴──────┐  pause()      ┌───────────────────┐  suspension surfacing rule (§8):
              │  paused     │◀─────────────▶│ sleeping / waiting │  sleeping = timer-only (sleep/sleepUntil/retry)
              └──────┬──────┘  resume()     └────────┬──────────┘  waiting  = awaits signal/CHILD (§20 SPAWN parks parent)
                     │ cancel()                      │ wake_at (timer / sweep) → running (re-dispatch)
                     ▼                               │       (liveness backstop, §11 C7 → stalled)
                ┌──────────┐ ◀──── cancel() (mode "abort") ──────────────────────────────────────┘
                │cancelled │
                └──────────┘

── Failure / cancel fold (§9 fold refinement · §21) ───────────────────────────────────────────
   running / sleeping / waiting ── uncaught throw escapes run() · or cancel({ mode: "compensate" }) ──┐
                                                                                       ▼
                                                    ┌──── ≥1 'pending' compensator? ────┐
                                                 no │                                   │ yes
                                                    ▼                                   ▼
                                              ┌───────────┐                      ┌─────────────┐
                                              │  failed / │◀── all undo settled ─│compensating │⟲ wake_at
                                              │ cancelled │   (target='failed' |  └─────────────┘  (compensator
                                              └───────────┘    'cancelled')                          backoff /
                                                                                                     next reverse batch)
   Engine-integrity (NondeterministicError → failed; forward StalledError → stalled) directly, NO rollback (§9, §21.1).

── Restart (revive) — orthogonal control-plane edge (§3.1 · §7.10 · §22) ───────────────────────
   restart() from ANY state, INCLUDING terminal completed/failed/cancelled/stalled → queued (wake_at := now()),
   journal truncated to ordinal < t. The one transition that legitimately leaves a terminal state.
```

`status()` (§3.1) returns `{ state, output, error }` where `state ∈ { queued, running, sleeping, waiting,
paused, stalled, compensating, completed, failed, cancelled }` — the settled public run-state set (§3.2, §7.1
CHECK); there is **no** `suspended` mega-state (A3: the suspension mechanism surfaces as `sleeping`/`waiting`).
The `compensating` phase (§3.8, §21) sits between `running`/`sleeping`/`waiting` and the terminal
`failed`/`cancelled`; it is entered only on a terminal failure (or `cancel({ mode: "compensate" })`) that
meets ≥1 completed compensable step. `stalled` is the fail-closed terminal a run reaches when the liveness
backstop trips (`StalledError`, §11 C7 — no forward progress possible); like the other terminals it fires the
run terminal notification and is revivable only by an audited `restart` (§7.10).

**Transition rules at commit (§6.4 step 6 fold), concurrency-aware:**

- workflow function actually **RETURNED** (all steps memoized) → `completed`, `output = return value`.
- any **SUSPEND/RETRY** pending, or **UNSETTLED** remainder, or parked overflow candidates → `sleeping` or
  `waiting` per the §8 surfacing rule (`waiting` if any pending branch awaits a signal/child, else
  `sleeping`); `wake_at := MIN(pending)`. If COMPLETEs made progress **and** more frontier remains **and**
  nothing suspends → `queued`, `wake_at := now` (immediate re-dispatch).
- a **PERMANENT_FAIL** (or a raised `PermanentError`) → `failed`; **sibling COMPLETE rows in the same batch
  still commit** (the frontier is atomic, §11 C4), but the run does not advance past the failure.
- `StalledError` (liveness, §11 C7) → `stalled` (fail-closed terminal); `NondeterministicError` (§11) →
  `failed`, fail-closed. Neither runs rollback (§21.1).
- `pause()` → `paused` (no dispatch scheduled); `resume()` → `sleeping`/`waiting`/`running`; `cancel()` →
  `cancelled` (terminal). `paused`/`cancelled` are control-plane transitions, orthogonal to the frontier fold.
  With `cascade:true` (§3.7), `cancel()` additionally sets live descendants' `cancel_requested` (§20.6); an
  independent-mode (`cascade:false`) cancel touches no child.
- `restart({ from, deploy })` (§3.1, §7.10, §22) is a **control-plane transition, orthogonal to the frontier
  fold** — a sibling of `pause()`/`cancel()`, and the **one** edge that legitimately leaves a terminal state:
    - `restart()` from any **non-dispatching** state (`queued`|`sleeping`|`waiting`|`paused`|`completed`|
      `failed`|`cancelled`|`stalled`) → `queued`, `wake_at := now()`, journal truncated to `ordinal < t`,
      terminal `output`/`error` cleared; dispatched immediately by the claim/dispatch scheduler (§4) via the
      `wake_at := now()` re-queue (§8). This is the **only** transition out of `completed`/`failed`/`cancelled`/
      `stalled` — a deliberate, authorized, audited revive (`restart_count`/`restarted_*`, §7.1). Restart also
      clears any `compensation_target`/`compensation_*` annotations, since the retained prefix's compensable
      steps re-derive on the next forward replay.
    - `restart()` on a **`running`** *or* **`compensating`** run (either may hold a live dispatch) → first
      `claim_epoch += 1` (the in-flight forward/rollback dispatch's §7.4 commit fails its epoch guard and
      rolls back — reusing the lease-handoff race handling), then the same reset → `queued (wake_at :=
      now())`. (A `compensating` run in retry backoff is reset like the non-dispatching case; the
      `claim_epoch` bump is a harmless no-op when nothing is claimed. §7.10 bumps `claim_epoch` on every
      restart, so the two paths differ only descriptively.)
    - Restart is **not** idempotent (each is a distinct operator intent) but is **serialized + atomic**
      (advisory lock + epoch guard, §7.10); concurrent restarts cannot tear state, and the loser sees the
      winner's reset.
- **`kind='child'` SPAWN** (frontier discovers a `step.call` invocation at ordinal `K`) →
  `SUSPEND(kind=child, child_run_id)`: the parent step row commits `state='running'` carrying
  `child_run_id`; the child run is co-inserted `queued` under the parent lease (§7.4, §20.2); the parent
  run → `waiting`, `wake_at := MIN(pending)` (NULL unless `opts.timeout`). *The child then walks this
  same §9 machine from `queued` like any `start()`ed run.*
- **`kind='child'` JOIN** (the child's terminal join signal bound on a later parent dispatch, §20.3) →
  parent step `state='completed'` (`output` = child `Output`, inline or blob-ref) if the child `completed`;
  `state='failed'` (`error` = child error) if the child `failed` or was `cancelled` (rethrown as
  `PermanentError` / `ChildCancelledError`). A failed join follows the ordinary §6.4 `PERMANENT_FAIL` fold —
  sibling children in the same frontier still commit; the parent run fails unless the author catches it.
- **`ChildTimeoutError`** (`opts.timeout` `wake_at` reached with no join bound) → the await rejects `ChildTimeoutError`
  into the parent (sibling of `StepTimeoutError`); the child keeps running unless `cascade:true`.
- **Terminal-failure fold refinement (§3.8, §21).** Compensation must distinguish a **caught** step failure
  from an **uncaught** one, which the engine can observe only by **replaying with the failed step's promise
  rejected** and seeing whether the rejection escapes `run()`. So the `PERMANENT_FAIL` fold is refined: the
  failing ordinal still commits a `state='failed'` row and siblings still commit (unchanged, §11 C4), but
  instead of setting `run.state := failed` in the *same* fold, the dispatch **commits the failed row and
  schedules an immediate re-dispatch** (`wake_at := now`). On that re-dispatch the memoized-failed step's
  promise **rejects at its point in program order**; the body either **catches** it (the run continues, may
  suspend/complete) or lets it **propagate out of `run()`**. **Terminal run-failure is defined as: the
  replay pass throws out of `run()`** — unifying uncaught `PermanentError`, retries-exhausted `StepFailed`,
  and any other uncaught throw. This is equivalent to the shorthand `PERMANENT_FAIL → failed` whenever the
  error is uncaught (the common case) **modulo one deterministic re-dispatch**, and is required for
  `try/catch` to be observable. `NondeterministicError` and `StalledError` are **not** subject to it — they
  fail closed *without* replay-to-observe (below).
- **Enter rollback** — an uncaught throw escaping `run()` (above) with `EXISTS(step WHERE
  compensation_state='pending')` → `state := compensating`, `compensation_target := 'failed'`,
  `error := <triggering error>`, `wake_at := now` (immediate re-dispatch into the rollback phase, §21.2). No
  `pending` rows → `state := failed` directly (nothing to undo, unchanged). `cancel({ mode: "compensate" })`
  produces the identical transition with `compensation_target := 'cancelled'` (or `cancelled` directly when
  no `pending` rows exist). Plain `cancel()` is unchanged: straight to `cancelled`, no rollback.
- **`compensating` fold** (§21.2 step 6) — while any `pending`/`running` compensator remains, `state` stays
  `compensating` and `wake_at := MIN(compensation_wake_at over 'running')` (or `now` if a `pending` is
  ready); when none remain → `state := compensation_target` (`failed`|`cancelled`) with
  `compensation_outcome := 'partial'` if any `compensation_state='failed'` else `'completed'`. `pause()` on a
  `compensating` run → `paused` (`compensation_target` retained); `resume()` → `compensating`. `cancel()`
  (no opt-in) on a `compensating` run → `cancelled`, hard-abandoning remaining undo. An
  external/broadcast signal to a `compensating` run is rejected at the edge (`404`, §3.6/§18) — rollback
  consumes no signals.
- **Engine-integrity failures fail closed WITHOUT rollback.** `NondeterministicError` → `failed` directly
  (the journal/replay is untrustworthy, so reconstructed compensator closures cannot be trusted);
  `StalledError` during the *forward* phase → `stalled` directly (no progress was possible). A `StalledError`
  during the *compensation* phase → terminal (`failed`/`cancelled`) with `compensation_outcome='partial'`.

**Output representation is orthogonal to state.** A run's `output` (and any step's) is recorded either
**inline** (`output_kind='inline'`) or **blob-backed** (`output_kind='blob'`, payload in object
storage, ref journaled) — a *representation* choice (§3.5, §17), never a state. `completed` with a
blob-backed output is reached by exactly the same fold as with an inline one; `status()` surfaces it as
a `StatusOutput` ref (§3.5). Blob writes are synchronous within a dispatch — they add no `wake_at`
suspension and do not touch the `claimed_by` lease.

**Signal provenance is orthogonal to state.** A `wait_signal` branch binds a `workflow_signals` row the
same way whether its `origin` is `app`, `ingress`, or `system` and its `delivery` is `direct` or `topic`
(§18) — provenance is recorded on
the row, never in the run state. An external delivery to a **terminal** run (`completed`/`failed`/
`cancelled`/`stalled`) or a **cancelled/paused** run is rejected at the edge (`404`, §3.6), not a state transition;
external/broadcast ingest advances a run only via the ordinary wake → dispatch → fold path.

**Scheduled-run provenance is orthogonal to state.** A schedule fire (§12) creates an ordinary run at the
same `start()` → `queued` entry as any other; the only difference is journaled *inputs*, not a state:
`workflow_runs.input` comes from the schedule's `input_json` and `started_at` (hence `trigger.startedAt`)
is stamped with the **planned fire instant**, both frozen at creation so replay observes a constant, never
wall-clock (§11, §12). The schedule sweep never mutates a run's state directly; it only mints the run, which
then walks this state machine unchanged.

**Parentage is orthogonal to state.** A child walks the same §9 machine as any run; the only differences
are journaled *edge* columns (`parent_run_id`/`parent_wait_step_key`/`parent_cascade`/`deploy_id`=parent's),
never a state. The terminal-child hook (§20.3) advances the **parent** only via the ordinary wake →
dispatch → fold path, and cascade cancel (§20.6) transitions each descendant under **its own** lease at its
next claim — never a cross-run state write.

**Compensation is a dispatchable phase, orthogonal to output & signal provenance.** `compensating` holds
the `claimed_by` lease and arms `wake_at` for compensator backoff exactly like `running`+`sleeping`/`waiting` (§21).
It is orthogonal to output representation (blob-backed or inline) and to signal provenance. Reaching the
terminal after rollback fires the run terminal notification (§5 step 9) exactly as a direct terminal does;
`compensation_outcome='partial'` means the run failed **and** ≥1 undo could not be applied — surfaced in
`run.error.compensation` (§3.8) for operator attention, but it does **not** change the terminal state
(`failed`/`cancelled`). Compact transition summary (additive to the diagram):

```
running / sleeping / waiting ──uncaught throw escaping run() (§9 fold), ≥1 'pending' compensator──▶ compensating (target='failed')
running / sleeping / waiting ──uncaught throw, 0 compensators──────────────────────────────────────▶ failed
running / sleeping / waiting ──cancel({mode:"compensate"}), ≥1 'pending'───────────────────────────▶ compensating (target='cancelled')
running / sleeping / waiting ──cancel() | cancel({mode:"abort"})───────────────────────────────────▶ cancelled
compensating ──wake_at (timer/sweep)──────────────────────────────────────────────────────────────▶ compensating (next reverse batch)
compensating ──all compensators completed|failed──────────────────────────────────────────────────▶ failed | cancelled (outcome: completed|partial)
compensating ──pause()────────────────────────────────────────────────────────────────────────────▶ paused (target retained) ──resume()──▶ compensating
compensating ──cancel()───────────────────────────────────────────────────────────────────────────▶ cancelled (abandon remaining undo)
```

Terminal states (`completed`, `failed`, `cancelled`, `stalled`) fire the run terminal notification (§5 step 9).

---

## 10. Metering & gateway-edge dispatch

Metering is **infrastructure** — unforgeable, emitted by the trusted worker, never by app code (key
invariant; there is no `env.meter`). For workflows:

- **One dispatch = one metered unit.** The worker emits the five platform counters per dispatch:
  `wall_us` (dispatch wall time), `cpu_us` (summed JS CPU across the batch — still single-threaded, so CPU
  serializes, §6.5), `ingress`/`egress` (summed across the frontier's step bodies), plus `requests`.
- **Data-primitive metrics** (`db_reads`, `kv_writes`, `storage_ops`, …) emit at their op boundary inside
  step bodies exactly as in a normal request (via `env.{db,kv,storage}`).
- **Blob-backed outputs** (§3.5, §17) meter as object storage on the **success arm only**, matching the
  data-plugin convention: a spill/stream write emits `storage_ops += 1` and `storage_bytes += size`; a
  `StepOutputRef` read (`.json()`/`.stream()`/…) emits `storage_ops` (plus `egress_bytes` for the
  external `…/output` read endpoint). Dispatch-scoped memoization collapses N reads of one hash to a
  single metered fetch (§17.6). App code can neither forge nor suppress it — the worker writes the blob.
- **External signal ingress** (§18) meters at the **control-plane ingress terminus** (§4), on the **accept
  arm only** (a rejected `401`/`403`/`413`/`429` is not billable — you cannot bill for rejected spam):
  `wf_signals_ingress += 1` per accepted directly-addressed delivery, `wf_broadcasts += 1` per accepted
  publish, `wf_fanout_deliveries += n` per committed fan-out batch, plus `ingress_bytes += body_len`. The
  gateway's edge rate-limit runs first (redis, no PG); the control plane then verifies the signature and meters
  only on the accept arm (§13). Names follow the `crates/metering` convention; they are infrastructure
  counters attributed to `app_id`, so app code can neither forge nor suppress them.
- **Child / sub-workflow runs** (§3.7, §20) meter as **ordinary runs** — each child dispatch is one metered
  unit on the child's own `app_id`, exactly like a `start()`ed run; the parent's park + join are ordinary
  parent dispatches. There is **no** new child metric and **no** double-count: the spawn co-commit and the
  terminal-child hook are journal writes on commits that already exist (§7.4), not extra dispatches.
- **Compensation dispatches** (§3.8, §21) meter as **ordinary dispatches** — one metered unit each,
  identical to a forward dispatch: the worker emits the five platform counters (`wall_us`, `cpu_us`,
  `ingress`, `egress`, `requests`); compensator bodies emit `env.{db,kv,storage}` usage metrics at their op
  boundary on the success arm. Rollback is subject to the same spend enforcement (Warn → Degrade → Block) — a
  **Blocked** app's rollback stops at the metered dispatch edge like any other dispatch. There is no new
  compensation metric and no `env.meter`; app code can neither forge nor suppress it.
- **Aggregation across a concurrent frontier** happens per-dispatch: the batch's step bodies share one
  metered dispatch unit; there is no per-step billing unit. App code can neither suppress nor forge it.
- **The gateway stays dumb.** Dispatch runs in the worker; the gateway forwards the metered unit. Spend
  enforcement (Warn → Degrade → Block) applies to workflow dispatch exactly as to request dispatch
  (`docs/reference/billing-metering.md`).

---

## 11. Determinism & correctness

Let a *journal prefix* be the set of committed `workflow_steps` rows for a run. All claims are relative to
the antichain theorem (§6.1).

- **C1 — Frontier determinism.** The frontier set and each member's `ordinal` are a pure function of
  `(workflow code, journal prefix)`. The replay pass is deterministic; ordinals are assigned by synchronous
  call order; memoized steps resolve to fixed journaled values; the pass blocks at the first pending
  unmemoized promise; concurrency cannot alter which steps were *issued* before that block nor their call
  order, because admitted bodies are never awaited-into during the pass (interrupt model). ∎
- **C2 — Exactly-once journaled result.** Each `ordinal` is written at most once: PK `(run_id, ordinal)` +
  `ON CONFLICT DO NOTHING`, first committer wins; the lease guard makes concurrent committers of one run
  mutually exclusive except across a lease handoff, where `DO NOTHING` preserves the first result. ∎
- **C3 — At-least-once effects (honest, unchanged).** A step body may run >1×: crash before commit,
  wall-budget `UNSETTLED`, retry, or lease handoff. The journal dedupes the *result*, never the *effect*. An
  aborted N-row commit journals **zero** of the N, so on re-dispatch all N re-run — each at-least-once, none
  skipped, none torn. ∎
- **C4 — Atomic, non-torn frontier.** All COMPLETE rows + suspension registrations + run transition + signal
  consumption commit in one txn (§7.4). Either the whole frontier's outcome is visible or none of it is. ∎
- **C5 — Journal order = ordinal order = single-effect ordering.** `seq`/PK is the deterministic `ordinal`,
  never settlement order. The journal is the *same deterministic total order* the single-frontier engine
  would produce; concurrent real-time interleaving of effects is unobservable in the journal. ∎
- **C6 — Suspension safety among siblings.** A suspending branch registers its wake atomically with its
  siblings' completions; `wake_at = MIN(pending)`; every wake re-evaluates all pending step rows. A wake is
  never lost and a completed sibling is never "un-completed" (§8). ∎
- **C7 — Liveness.** Progress = a committed COMPLETE or a scheduled future wake. A dispatch that journals
  nothing and schedules no future wake increments `stuck_strikes`; after a bounded strike count the run fails
  with `StalledError`. This bounds the pathological "wide frontier where no step ever settles within
  `wall_budget`" case. (Wide-frontier throughput under budget rollover is **unknown / to-measure**.) ∎
- **C8 — Strict generalization / no baseline regression.** Every single-frontier workflow runs with
  `concurrency = 1`, under which §5/§6 is byte-identical to the single-row engine (C1–C7 collapse to the
  single-row proofs). The concurrent path is entered only when `effN > 1` **and** an antichain frontier of
  width > 1 exists. ∎
- **C9 — Replay-prefix soundness of restart (§3.1, §7.10, §22).** After a **partial** restart the retained
  prefix `0..t-1` was produced by the run's pinned deploy, which restart leaves unchanged; so the next
  replay re-derives ordinals `0..t-1` with identical `name`s (C1) and memoizes them, reaches ordinal `t` as
  a fresh frontier candidate, and executes forward — no `NondeterministicError`. A **full** restart retains
  no prefix, so any pin (default `current`) is vacuously prefix-compatible. The invariant "never retain a
  journal prefix against code that did not produce it" is enforced **structurally** by forbidding a `deploy` re-pin
  with `from` (§22.3/§7.10). Restart edits only the journal prefix + run row; the §5 dispatch loop and §6
  frontier machinery are untouched (C1/C5), so the re-dispatched run walks the same deterministic ordinal
  order. Dropped steps **re-execute** — their side effects run again — which is a *deliberate* re-execution
  (the §6.3 at-least-once contract applied on purpose, C3), not a regression; make step bodies idempotent.
  The blob-ref prune, signal un-consume/delete, subscription/step drops, and run reset all commit in **one**
  txn (C4) — the run is fully rewound or not at all. `claim_epoch += 1` evicts any live dispatch via the
  existing §7.4 guard; `wake_at := now()` re-queues via the existing §8 unified wake (no new trigger); the
  replay core is unmodified, so `effN=1` runs are rewound byte-identically to concurrent ones. ∎

**Child / sub-workflow orchestration (§3.7, §20).**

- **CW1 — Spawn determinism / no double-spawn.** `child_key = child:{parentRunId}:{K}` is a pure function of
  `(parentRunId, parent step ordinal)`; child spawn + parent step co-commit in the parent's lease-guarded
  §7.4 txn with `ON CONFLICT (app_id, workflow_name, dedup_key) DO NOTHING` (child) and `ON CONFLICT (run_id,
  ordinal) DO NOTHING` (parent step); the lease guard serializes concurrent parent dispatchers;
  `child_run_id` is journaled in the parent step row and read back on replay. **Exactly one child per
  (parent, ordinal)**, across crashes / retries / lease handoff. ∎
- **CW2 — Join binding determinism.** The parent `kind='child'` step binds exactly one reserved
  `'__zs.child:'+K` internal signal; `idempotency_key = parent_wait_step_key` + the §7.3 unique index →
  **exactly-once** join ingest even under a retried child terminal txn; the bound value (Output or error,
  inline or blob-ref) is journaled → every parent replay yields the identical result. Reuses C1
  signal-binding determinism. ∎
- **CW3 — Terminal-hook atomicity.** Join signal `INSERT` + parent `wake_at` arm co-commit with the child's
  terminal transition → child-terminal ⇔ parent-armed; §18.4's safety-net scan re-arms a lost parent wake. ∎
- **CW4 — Fan-out/join = the concurrent frontier.** N `step.call` calls in one `Promise.all`/`step.all`
  are a §6.1 antichain of `kind='child'` steps; up to `effN` spawn per dispatch (rest roll over, §6.4); the
  parent parks on all and resolves via §8 unified `MIN(pending)` re-eval-all when every child is terminal.
  No new join engine — it *is* §6 + §8. ∎
- **CW5 — At-least-once children, effectively-once join.** A child is an ordinary run → at-least-once
  effects, exactly-once journaled results (C2/C3). The join ingest is exactly-once (CW2) and the bind is
  memoized → the parent processes each child terminal **exactly once**. ∎
- **CW6 — Cascade safety & termination.** Independent by default (no edge traversal). `cascade:true` cancels
  each live descendant under **its own** lease via cooperative `cancel_requested` (§20.6); the walk is
  bounded by `tree_depth` + live-descendant caps and terminates (`cancelled` absorbing). No cross-run lease
  is ever taken. ∎
- **CW7 — `startMany` idempotency.** Each keyed item = an idempotent §7.1 start; a batch retry collapses to
  the same runs (exactly-once per key). The batch `INSERT` is one txn with per-row `ON CONFLICT DO NOTHING`,
  so a duplicate key no-ops its row without failing the batch; unkeyed items are at-least-once (new run per
  call, documented §20.9). ∎

**Compensation / saga rollback (§3.8, §21).**

- **CC1 — Compensator determinism.** The per-dispatch `compensatorRegistry` — the set of `(ordinal, closure,
  output)` for completed compensable steps — is a pure function of `(workflow code, journal prefix)`: by C1
  the replay pass is deterministic up to the throw, and deploy-pinning (§4) guarantees the *same code*
  reconstructs the *same closures*. The reverse-ordinal frontier is a pure SQL function of the journal. ∎
- **CC2 — Exactly-once compensation *result*.** The `compensation_state='completed'` transition is written at
  most once per ordinal: the guarded `UPDATE … WHERE compensation_state IN ('pending','running')` under the
  lease CAS is first-committer-wins; a lease-handoff double-run cannot resurrect or double-settle a `completed`
  row (§7.4 variant). ∎
- **CC3 — At-least-once compensation *effect*.** A compensator body may run >1× (crash before the settle
  commit, wall-budget `UNSETTLED`, retry, lease handoff). The journal dedupes the *marker*, never the
  *effect* — identical to forward steps (C3). Idempotency is the author's obligation, made ergonomic by
  `ctx.idempotencyKey` (stable per `(run, ordinal)`). ∎
- **CC4 — Reverse-order integrity.** Undoing in strictly decreasing ordinal order means when compensator `N`
  runs, every step with ordinal `< N` is still `completed` (not yet compensated) and every step `> N` is
  already `completed`/`failed`. No compensator observes a partially-torn earlier state; a compensator may safely
  read an earlier step's output. For intra-frontier siblings (antichain, §6.1, when
  `compensationConcurrency>1`) the relative undo order is arbitrary-but-deterministic (descending ordinal). ∎
- **CC5 — Single-frontier / lease / wake_at respected.** Compensation is the §5 loop with the frontier
  predicate reversed; `effN=1` baseline runs one compensator per dispatch. It uses the **same** advisory-lock
  claim, `claimed_by` lease + CAS, the **same** deploy-pin, and the **same** `wake_at`-unified suspension
  (compensator backoff joins `MIN(pending)`, §8). The claim sweep (§4) reclaims a crashed compensation
  dispatch exactly as it reclaims a forward one — no new scheduler, one more phase. ∎
- **CC6 — Scope correctness.** Only rows with `compensation_state='pending'` (completed `step.run` bodies
  that carried a compensator) are ever selected. Caught failures, incomplete steps, sleeps, and signals are
  never compensated. Rollback begins only when the replay throws out of `run()` (§9 fold);
  `NondeterministicError` fails closed with no rollback (untrusted journal); plain `cancel()` performs no
  rollback. ∎
- **CC7 — Termination / liveness.** Every committed compensation dispatch strictly shrinks the frontier (a
  `completed`/`failed` marker) or arms a bounded backoff; `compensation_max_attempts` bounds retries per
  compensator; `stuck_strikes` (C7) bounds zero-progress dispatches → `StalledError`. Rollback therefore
  terminates in a bounded number of dispatches (≤ committed compensable steps + retries) — a stuck undo can
  never wedge the run. ∎
- **CC8 — Strict generalization.** A workflow with no `compensate` on any step has `compensation_state IS
  NULL` on every row; the entry check `EXISTS(compensation_state='pending')` is always false, so the run
  fails exactly as in the pre-compensation engine. The forward path (§5/§6) is byte-identical; compensation
  code is entered only when a terminal failure meets a non-empty `pending` set. ∎

**Nondeterminism traps.** The reliable protection is structural, not timing-based: if a replay issues a
step whose `name`, `kind`, or same-name occurrence differs at a committed ordinal, the dispatch fails
`NondeterministicError` and the run fails closed. This catches the journal-corrupting class of
nondeterminism because the check compares the replay against the journal prefix directly.

A raw non-`step` `await` in the workflow *body* (e.g. a bare `fetch`) is forbidden: the workflow body
may only observe the outside world through journaled `step.run` / `step.sideEffect` output (§3.2.1). The
runtime prevents the load-bearing bare-I/O cases at call time with dispatch-scoped I/O guards:
platform-provided `fetch`, `setTimeout`, and `setInterval` throw `NondeterministicError` while the
workflow body is executing, but pass through unchanged inside a journal callback (`step.run` for I/O,
`step.sideEffect` for inline non-determinism) and outside workflow dispatch entirely. This is independent
of promise timing and applies on replay even when no new frontier has been discovered. `step.sleepUntil(name,
when)` (§3.2) freezes `when` into `wake_at` **at first discovery and journals it** — never recomputed on
later replays — exactly like `step.sleep`'s `now + duration`; so even a `when` derived from `Date.now()`
cannot cause replay divergence *of the sleep step* once discovered, and it adds **no** new clause here —
a `sleep`/`sleepUntil` row carries no output value and its
`wake_at` is a journaled constant, covered by C1/C5 unchanged (the idiomatic use derives `when` from a
journaled source — `trigger.input`, `trigger.startedAt`, a prior `step.run` output, or a prior
`step.sideEffect` output).

---

## 12. Scheduled workflows — `schedule` + the DSL

`schedule({ name, schedule, workflow, input, overlap?, catchUp? })` (§3.4) registers a recurring trigger in
the control plane. This section is the engine home: the compile pipeline, deploy reconciliation, the sweep, and the
correctness argument. The one hard rule (§3.4) is what keeps it minimal — the friendly DSL is **pure
build-time authoring sugar in the SDK**; both the fluent builder and the raw string compile to one of the
**two primitive stored shapes** (internal `cron` / `interval` kinds, §3.4/§7.9). **The engine adds zero
replay surface**: schedules only *produce* runs; each run then replays through the untouched single-frontier
core (§5) with a journaled, deterministic `trigger`.

```
every.monday.at("09:00","America/New_York")  ┐
"0 9 * * 1"                                   ├─ compileSchedule() ─▶ Schedule ─▶ zeroship.workflow_schedules
@daily                                        ┘   (SDK, build-time)   (2 kinds)     (control-plane sweep, §7.9)
```

### 12.1 Build-time discovery & deploy reconciliation

Reuses the exact RPC/route-discovery pattern — no new mechanism:

1. **Build time (vite-plugin).** The plugin collects every `schedule({...})` call into the deploy manifest as
   `schedules[]` of `{ name, workflowName, schedule: Schedule, overlap, catchUp, input }`. `compileSchedule`
   (§3.4) runs here, so a bad schedule (unknown IANA zone, out-of-range field, non-POSIX/sub-minute cron,
   `everyMs` below the floor) **fails the build** with `InvalidScheduleError` — never at fire time.
2. **Deploy (control plane).** On `.zship` ingest the control plane reads `manifest.schedules[]` and
   **reconciles** `zeroship.workflow_schedules` for `app_id` against the new deploy **in one transaction**:
   upsert by `(app_id, name)` — set the descriptor columns + `deploy_id = <new>`, recompute `next_fire_at`
   from `now()` (DB clock); schedules **absent** from the new manifest are `DELETE`d (no orphans). One txn
   means a redeploy never leaves a half-registered set. This slots into the doc's **deploy-pinning** (§4): a
   schedule carries `deploy_id` and every run it fires is pinned to that deploy, exactly like a manually
   started run.

### 12.2 The schedule sweep

The schedule sweep is a **peer of the run/wake claim sweep** (§4), on the same compio (zero-tokio) task,
same `claimed_by` lease + advisory-lock discipline, using Postgres `now()` as the **single clock** in both
the claim predicate and the advance so multiple control-plane nodes cannot disagree about "due." One tick:

```
1. Claim a batch (single DB clock, no per-node skew):
     SELECT id, kind, cron_expr, tz, interval_ms, anchor, next_fire_at,
            workflow_name, input_json, overlap, catchup, catchup_max, deploy_id
       FROM zeroship.workflow_schedules
      WHERE NOT paused AND next_fire_at <= now()
      ORDER BY next_fire_at
      FOR UPDATE SKIP LOCKED
      LIMIT :batch;

2. For each claimed row, let planned := next_fire_at (the SCHEDULED instant):
   a. overlap='skipIfRunning' AND an active run of this schedule exists → skip firing (best-effort, §12.4).
   b. Otherwise fire ONE run, deduped on the planned instant:
        env.workflows[workflow_name].start({
          input: input_json,
          key:   `sched:${id}:${epoch_ms(planned)}`,   // deterministic
          onConflict: "join",                          // at-least-once dispatch → at-most-one run
        });
      The run is deploy-pinned to deploy_id and dispatched via the normal gateway-edge metered path (§10),
      with workflow_runs.started_at := planned (NOT wall-clock) and input := input_json.
   c. Advance:
        - catchup='skip'  : next := first_fire_at_strictly_after(schedule, now())   -- fast-forward past all
                            missed instants; fire only `planned` (bounded: one run).
        - catchup=backfill: emit up to catchup_max additional runs for missed instants < now(), each keyed
                            on its OWN instant, then next := first future instant (bounded by catchup_max).
      UPDATE ... SET last_fire_at = planned, next_fire_at = next, updated_at = now().
```

`first_fire_at_strictly_after(schedule, t)` is the one deterministic primitive the Rust engine implements:
- `kind='interval'`, anchor `epoch`: `⌈t / everyMs⌉ · everyMs`; anchor `deploy`: same, offset by `deploy_at`.
- `kind='cron'`: standard "next cron time ≥ t" evaluated in `tz` via a **pinned tzdb** (`chrono-tz` or
  equivalent), DST rules per §16.

### 12.3 Correctness — exactly-once run creation per fire

The sweep is **at-least-once** by construction (a node can claim, fire, then crash before the `UPDATE`, and
another node re-claims after lease expiry). Correctness comes from the **deterministic dedup key**
`sched:{id}:{epoch(planned)}` fed to `start({ key, onConflict:"join" })`, backed by the existing
`UNIQUE (app_id, workflow_name, dedup_key)` guard (§7.1):

- Every attempt to fire the *same* `(schedule, planned_instant)` produces the *same* key.
- The **first** insert wins; all others no-op and return the existing `WorkflowRun`.
- Hence at-least-once dispatch attempts, but **at-most-one run per scheduled instant** ⇒ exactly-once *run
  creation* per fire, across crashes, retries, and concurrent sweeper nodes.

The `UPDATE ... next_fire_at` advance is idempotent under re-claim (computed from the row's own
`next_fire_at`/`now()`), and a duplicate advance can at worst skip a *future* instant that `catchUp`
governs — never double-create a run, since the dedup key guards that independently of the advance. This is
the load-bearing integration claim: **the schedule is a run-producer that lives entirely outside the replay
boundary** (§11). The only clock value a scheduled workflow observes through its trigger — `startedAt` =
`planned_fire_at` — is a journaled constant, so scheduled runs are as replay-deterministic as any other.

**Catch-up determinism.** After downtime spanning K missed instants, the outcome is a pure function of
`(schedule, now(), catchUp)`: `{ mode: "skip" }` → exactly one run for the current period + fast-forward
(bounded, one run); `{ mode: "backfill", max }` → up to `max` runs, each keyed on its own instant (so re-runs
of the sweep still dedup) + fast-forward (bounded by `max`).

### 12.4 What is *not* guaranteed (honest)

- **Fire *timeliness* is at-least-once, not on-the-dot.** A fire happens at the first sweep tick with
  `now() ≥ next_fire_at`; sweep cadence + backlog add latency — **unknown / to-measure**.
- **`overlap:"skipIfRunning"` is best-effort.** It queries for an active prior run before firing; a genuine
  race (two nodes both see "none active") can still create two runs. For a hard singleton, model it inside
  the workflow (e.g. a `step.run` that takes an app-level lock) — we do not fabricate a distributed
  exactly-one-live guarantee.

Residual scheduling limits (sub-second, interval-anchor alignment surprises, DST edge rules, tzdb pinning,
calendar corners like `L`/`#` and day-of-month 29–31) are enumerated in §16.

---

## 13. Limits, quotas & abuse

- **Frontier-width ceiling.** `effN = min(step.all cap, Workflow.concurrency, platform_ceiling[tier])`. The
  platform ceiling is a hard **operator config** cap (not a fabricated number) so a `recipients.map` over
  100k items cannot open 100k concurrent `fetch`es in one isolate → memory/metering blow-up. Overflow
  candidates roll to later dispatches.
- **Wall budget.** `dispatch_deadline = claim_ts + wall_budget`, enforced with a compio timer (zero tokio).
  `UNSETTLED` steps at the deadline are simply not journaled and re-run next dispatch. `lease_ttl > max
  wall_budget` so the claim sweep never reclaims a live concurrent dispatch.
- **Per-step timeout.** `StepTimeoutError` (from `config.timeout`) bounds a single body so one hung step cannot
  burn the whole budget; retryable per `config.retries.maxAttempts`.
- **Effect amplification.** A crashed concurrent dispatch re-runs up to N bodies → up to N× duplicate effects
  vs 1× single-frontier. Documented; mitigated only by idempotent step bodies / idempotency keys.
  **Fundamental — not fixable by the engine** (§16).
- **Output buffering.** Up to N step outputs are held in memory before the single commit → bound
  `output_bytes × N`; enforce a per-step output-size limit.
- **Liveness backstop.** `stuck_strikes` (§11 C7) fails runs that make no progress.
- **Signal/journal growth.** `workflow_signals` and `workflow_steps` grow per run; retention/GC of terminal
  runs is an operator policy (open question, §16).
- **Metering (gateway-edge, unforgeable).** §10 — one dispatch = one metered unit; app code cannot suppress
  or forge.
- **Blob output ceilings (§3.5, §17).** A per-step blob ceiling (`maxStepBlobBytes`, enforced
  *mid-stream* — aborts + cleans the partial, throws `LimitExceededError` — a platform/author misuse cap, B5) and a per-run `blob_bytes` ceiling
  (`maxRunBlobBytes`) bound object-storage growth; the per-app aggregate rides the existing
  `storage_bytes` spend engine (a **Block**ed app gets 402 at the metered dispatch edge, **Degrade**
  throttles — no new mechanism). The 1 MiB **inline** cap is unchanged; `output: "inline"` still errors
  above it. `journal_bytes` (Postgres, tight) stays bounded because each blob row contributes only a
  fixed `BLOB_REF_COST`, not its payload (§17.5).
- **Blob read amplification (§17.6).** Reads are memoized per dispatch (N reads of a hash → 1 fetch);
  reads in *orchestration* code re-fetch every replay (documented "read inside a step" mitigation, not
  enforced). Per-read/replay cost is **unknown / to-measure**.
- **Blob GC (§17.6).** Two advisory-lock-serialized control-plane sweeps (ref-table + orphan), reusing
  the `claimed_by` claim-sweep pattern; grace windows are operator-config, orphan grace strictly longer
  than ref-table grace. Cadence + grace defaults **to-measure**.
- **External signal ingress abuse (§18.6).** The full vector table is §18.6; the load-bearing controls:
  forged signals are rejected by the control-plane ingress verifier (§4) and never reach the journal or bill;
  **type injection** is walled by the deploy-pinned `externalSignals` allowlist (plus per-run token
  `types[]` scope); an **ingress flood** hits a per-app **and** per-source-token token bucket (`max_qps`,
  preferably `env.kv`/redis at the edge so a reject never writes PG) → `429`, enforced *before* the DB
  write and *before* metering; **payload bloat** is capped (`max_body_bytes` → `413`); **fan-out
  amplification** is bounded by `max_fanout` per sweep pass + `max_subscribers_per_topic` (a topic over the
  cap rejects new subscriptions rather than fan out unboundedly); **topic sprawl** by `max_topics` per app
  (app-namespaced, length-capped keys); **retention exhaustion** by `workflow_broadcasts.expires_at` TTL +
  GC. Token theft is bounded by short `ttl` + run/type scope + coarse `signal_epoch` revocation (§18.1).
- **Schedule abuse (§12).** **Minimum period:** `kind='cron'` is minute-granular by construction (≥ 60 s);
  `kind='interval'` enforces `everyMs ≥ SCHEDULE_MIN_INTERVAL_MS` (operator knob — a durable run has
  non-trivial fixed overhead, so a very small interval is abusive; the floor is **to-measure**, seeded
  conservatively). **Max schedules per app** (`SCHEDULE_MAX_PER_APP`): reconciliation rejects a deploy that
  exceeds it with `InvalidScheduleError`. **Backfill ceiling:** `catchup_max` is validated
  `≤ SCHEDULE_BACKFILL_HARD_MAX`; beyond it, missed instants are dropped + logged — no unbounded catch-up
  storm. **Fan-out is naturally throttled:** one schedule creates at most one run per instant (dedup key,
  §12.3), so a slow/stuck workflow cannot stampede the queue. **Validation is build/deploy-time, never
  fire-time** (§12.1). **Sweep fairness:** `FOR UPDATE SKIP LOCKED` + `ORDER BY next_fire_at` + batch limit
  bound per-tick work and let multiple nodes share the sweep without contention.

- **Child / sub-workflow abuse (§3.7, §20.9).** **Reserved `__zs.` type namespace** — `run.signal` /
  external ingress reject a user-supplied `type` with the `__zs.` prefix (`403`), so no app code can forge a
  child completion (§7.3, §20.3). **Fork-bomb bound** — `tree_depth` (stored on the run row,
  `parent.tree_depth+1`) is checked at spawn against `maxChildDepth`; a per-tree live-descendant cap
  (`maxLiveDescendants`, over the `parent_run_id` edge) bounds width; over cap → the `step.call` frontier
  step folds to `PERMANENT_FAIL` (`ChildLimitError`). **`startMany` batch cap** — `maxStartManyBatch`
  bounds one `INSERT`; over cap → `LimitExceededError` at the call site. **No double-spawn** — a crashed parent
  spawn dispatch does not double-spawn (CW1); a crashed child amplifies its own effects at-least-once like
  any run, unchanged. **Independent-mode orphans** keep running to terminal then no-op their join on the
  cancelled parent (intended; costs are the child's own metered dispatches, §20.6).

- **Compensation / saga rollback (§3.8, §21).** **Bounded fan-out** — rollback adds at most `#completed
  compensable steps` dispatches (serial `effN=1` baseline), each bounded by `wall_budget`; no unbounded
  amplification. `static compensationConcurrency` (§21.3) is clamped by the platform ceiling, same as forward
  `concurrency`. **Wedged compensator** — a per-compensator `timeout` (`StepTimeoutError`) +
  `compensation_max_attempts` + `wall_budget` + `stuck_strikes` → `StalledError` bound it; the run terminates
  `failed`/`cancelled` with `compensation_outcome='partial'`, and the walk records the failure and proceeds
  to earlier steps, so a single un-undoable step never wedges the run. **Effect amplification (honest, §16)**
  — crash/rollover re-runs compensator bodies → duplicate undo effects → idempotency required
  (`ctx.idempotencyKey`); fundamental, not fixable by the engine. **No new native primitive, no new
  typed_id, no new table** — annotation-only, so no dispatch-id prefix question and no new GC domain.
  `compensationConcurrency` default, the per-compensator backoff schedule, and the seeded
  `compensation_max_attempts` default are operator-config / to-measure — no numbers claimed.

- **Replay-from-step / restart (§3.1, §7.10, §22).** **Restart cap (creator path)** —
  `workflow_runs.restart_count` bounds restarts per run at `WORKFLOW_MAX_RESTARTS_PER_RUN` (operator config,
  seeded conservatively, **to-measure**), guarding against a restart loop endlessly re-executing effects +
  re-metering dispatch; exceed → `RestartError` (`429`). The **operator path is exempt** (always audited via
  `restarted_by`/`restarted_at`, §7.1). **Metering** — restart is not a free rewind: the re-executed steps
  dispatch through the ordinary gateway-edge metered path (§10) — one dispatch = one metered unit,
  app-unforgeable; there is **no** per-restart bypass. **Blob GC interaction** — dropped blob-backed rows
  decrement `refcount` in the restart txn (§7.5); bytes GC only when `refcount = 0` + past grace (§17.6). A
  re-executed step that reproduces the content re-references the same hash (idempotent write, §17.6) — never
  dangling, and the refcount pruned in the txn comes back on re-commit. **Terminal-run revive** — restart
  intentionally revives terminal runs; it interacts with the open terminal-run retention/GC policy (§16): a
  restarted run's dropped `workflow_steps`/`workflow_signals` are pruned immediately, the retained prefix
  persists. **No new table, no new index** — the run row is the audit record (§7.1).

Concrete numeric defaults for `platform_ceiling[tier]`, `wall_budget`, `lease_ttl`, `stuck_strikes` bound,
per-step output cap, the blob ceilings (`maxStepBlobBytes`, `maxRunBlobBytes`) + GC grace windows, the
ingress caps (`max_qps`, `max_body_bytes`, `max_topics`, `max_fanout`, `max_subscribers_per_topic`, token
`ttl`), the schedule caps (`SCHEDULE_MIN_INTERVAL_MS`, `SCHEDULE_MAX_PER_APP`, `SCHEDULE_BACKFILL_HARD_MAX`,
sweep tick interval + batch), the child-orchestration caps (`maxChildDepth`, `maxLiveDescendants`,
`maxStartManyBatch`), the compensation defaults (`compensationConcurrency`, `compensation_max_attempts`,
backoff schedule), and the restart cap (`WORKFLOW_MAX_RESTARTS_PER_RUN`) are **to-measure / operator-config**,
seeded from the plan catalog — no numbers are claimed here.

---

## 14. Invariants honored

- **Zero tokio.** `wake_at` timers + per-dispatch deadline + claim sweep are compio timers/tasks.
- **V8-per-thread / one isolate per app.** Concurrency is cooperative I/O overlap in one isolate — no
  threads, no data races, no parallel CPU (§6.5).
- **typed_id everywhere.** Creator-visible prefixes stay settled: `run_…` (run), `sig_…` (signal), `sch_…`
  (schedule, §7.9), integer `ordinal`. Internal workflow prefixes use a **`w`-family** rule (added to
  `crates/zeroship-core/src/typed_id.rs`) so they never collide with existing 3-char prefixes on this Stripe-centric
  platform: `wfd_…` (dispatch/batch — not `dsp_`, the billing-dispute prefix), `wsk_…` (signal key),
  `wbc_…` (broadcast), `wsb_…` (subscription — not `sub_`, Stripe's subscription prefix). All 3-char,
  pairwise-disjoint;
  the stateless `wst_` capability token completes the set.
- **Explicit wire formats.** The `zeroship.workflow_*` DDL (§7) is the contract; every producer/consumer
  changes in the same patch.
- **Gateway is dumb.** Dispatch runs in the worker; the gateway forwards a metered unit (§10) and forwards
  the public signal-ingress route to the control plane. It never verifies a signature and never writes the
  journal (§4).
- **Control plane owns the journal.** The control plane is the **sole reader/writer** of `zeroship.workflow_*`;
  workers report dispatch-completion envelopes and the gateway forwards — app code has no handle (§4).
- **Metering is infrastructure.** No `env.meter`; the worker (per-dispatch) and the control-plane ingress
  terminus (accept-arm) emit the counters — app-unforgeable, app-unsuppressable (§10).
- **Replay core & `wake_at`-unified suspension are untouched.** Every feature below is a *producer of journal
  rows* (or a journal-prefix editor, for restart); none changes §5/§6 or §8.
- **Pre-launch.** No back-compat shim, no `ALTER…backfill`, no `@deprecated` alias; all DDL lands in the
  create scripts and every producer/consumer changes in the same patch.

**Every day-1 feature preserves all of the invariants above.** The core-invariant-preservation argument is
stated **once, here** — each feature's §-specific integration checklist (§17.8, §18.9, §20.10, §21.5, §22.6)
and §3 scope bullets **cross-reference this section** rather than restating it. The only feature-specific
*deltas* worth calling out (everything else is identical to the core list):

- **Concurrent frontier (§6).** Cooperative I/O overlap in one isolate — no threads, no parallel CPU (§6.5);
  `effN=1` is byte-identical to the single-row engine (§6.6).
- **Schedule DSL (§3.4, §7.9, §12).** New `sch_` typed_id; the DSL is build-time authoring sugar (the engine
  sees only the two primitive `Schedule` kinds); the schedule sweep is a peer of the claim sweep.
- **Blob-backed outputs (§3.5, §17).** No new creator-facing `env.*` (`StepOutputRef` is framework-internal);
  the `wfblob:sha256:<hex>` ref is a **content address, deliberately not a typed_id**; `compio-s3`/compio files.
- **Child orchestration (§3.7, §20).** **No new typed_id prefix** (children are `run_…`, join signals `sig_…`);
  a child pins to the **parent's** `deploy_id` (§20.7); spawn/hook/cascade ride the existing §7.4 txn / sweep.
- **External ingress & broadcast (§18).** Three fresh prefixes (`wsk_`/`wbc_`/`wsb_`) + the stateless `wst_`
  token, all pairwise-disjoint (`crates/zeroship-core/src/typed_id.rs`); `externalSignals`/`inbound`/`topicFrom`/topic
  defs are deploy-pinned (§18.5); the ingress terminus is control-plane, the gateway forwards (§4).
- **Compensation (§3.8, §21).** **No new table, typed_id, or suspension reason** — annotation columns on
  `workflow_steps` + two on `workflow_runs` + a `compensating` state; compensator closures are the run's
  pinned code; `StepConfig` becomes generic `StepConfig<T>` outright (no alias).
- **Restart (§3.1, §7.10, §22).** 4 audit columns on `workflow_runs`, **no new table/index/typed_id/state** —
  a journal-prefix editor that re-queues via the existing §8 wake; the one edge that revives a terminal run.

---

## 15. Build plan

Sequenced PR-train; each PR self-contained, tested, commit-only (no push) per pilot discipline. Concurrency is
day-1, so it is threaded through from the first journal-schema PR, not bolted on.

1. **Schema + typed ids.** `zeroship.workflow_runs/_steps/_signals` create scripts (§7) *including* `ordinal`
   PK, `batch_id`/`batch_width`, `concurrency`/`next_ordinal`/`stuck_strikes`, the output-representation
   columns (`output_kind`/`output_hash`/`output_size`/`output_content_type` + blob-backed input columns +
   `journal_bytes`/`blob_bytes`), and the `zeroship.workflow_blobs` GC ref index (§7.5) from the start.
   Mint `run_…`, `sig_…`, and the dispatch/batch typed id `wfd_…` (w-family, disjoint from billing `dsp_`). Reference
   existing `app_deploys`.
2. **Claim/lease + dispatch scheduler.** Advisory-lock claim, CAS lease, `wake_at`-unified compio timer,
   claim sweep. `lease_ttl > wall_budget` guard.
3. **Replay core + `step` shim (single-frontier, `effN = 1`).** Deterministic ordinals, memoization, the
   determinism guard, the interrupt model, the single-row commit (§7.4 with N = 1). This is the `concurrency
   = 1` reduction of the unified loop.
4. **Generalize to the concurrent frontier (§6).** `frontierCandidates` collection + macrotask drain +
   generalized barrier + N-row idempotent commit + the outcome fold; `Workflow.concurrency` + `step.all` +
   `StepAllOptions`; bare-`Promise.all` structural detection.
5. **Suspension & signals (§8).** `step.sleep`, `step.sleepUntil` (absolute-target `wake_at`, §3.2 — rides
   with `step.sleep`, **no DDL change**, so PR 1 is untouched), `step.waitForSignal`, `run.signal`,
   `MIN(pending)` re-eval-all wake — as legal frontier members from the outset.
6. **Run controls + status (§3.1, §9).** `start({ input, key, onConflict })`, `pause/resume/cancel/status`,
   and **`run.restart({ from?, deploy? })`** (replay-from-step, §7.10, §22): the one control-plane restart txn
   (advisory-lock + `claim_epoch`-evict + drop `ordinal ≥ t` + blob/signal/subscription prune + reset +
   grants), `RestartOptions`/`RestartTarget`/`RestartError`, the creator + operator control-plane routes
   (`…/workflows/runs/:runId/restart` and the `/control/ops/…` peer, §22.4), the restart cap (§13), and the
   deploy-pin decision (partial=original-immutable / full=current-default, §4/§22.3). The 4 audit columns
   (`restart_count`/`restarted_at`/`restarted_from_ordinal`/`restarted_by`) land in the **PR 1** create
   scripts (no `ALTER`, pre-launch). Faithful e2e (per `feedback_faithful_e2e_tests`,
   `feedback_regression_test_per_fix`), all on the real replay/dispatch/commit path against live
   control-plane Postgres: a partial `restart({ from })` retains `0..t-1`, drops `≥ t`, and re-runs forward
   with identical retained ordinals (no `NondeterministicError`); a restart on a `running` run evicts the
   live dispatch via `claim_epoch` (its commit rolls back); a full restart re-pins to the current deploy by
   default and `deploy:"started"` reproduces on the start deploy; `deploy:"latest"` with `from` is rejected
   `409`/`RestartError`; a terminal (`completed`/`failed`/`cancelled`) run is revived; the per-run cap
   returns `429` on the creator path (operator exempt, audited); dropped blob-backed rows decrement `refcount`
   and a re-executed step re-references the same hash.
7. **`env.workflows` namespace + `@zeroship/workflows` package** (client wrapper over the control-plane API).
8. **Metering integration (§10)** — per-dispatch counter emission, spend enforcement parity.
9. **`cron` + schedule DSL (§3.4, §7.9, §12).** The `zeroship.workflow_schedules` create script (§7.9,
   landing in the PR 1 scripts) + `SCHEDULE_PREFIX = "sch"` (`crates/zeroship-core/src/typed_id.rs`). The SDK
   `@zeroship/workflows/schedule`: the `every` fluent builder + `cronExpr` + `compileSchedule` (the one pure
   build-time function) + `InvalidScheduleError` + the typed `cron(...)` registration (`ScheduleInput`,
   `overlap`, `catchUp`, `input` required-iff-non-void). Vite-plugin `schedules[]` manifest discovery
   (reusing the RPC/route-discovery pattern; `compileSchedule` runs at build so a bad schedule fails the
   build). Control-plane deploy **reconciliation** of `workflow_schedules` for `app_id` in one txn (upsert +
   delete-absent, deploy-pinned, §12.1). The control-plane **schedule sweep** — peer of the claim sweep
   (same compio task + `claimed_by` lease + advisory lock), `FOR UPDATE SKIP LOCKED` batch, DB-clock
   `now()`, `first_fire_at_strictly_after` (interval `epoch`/`deploy` + pinned-tzdb cron), the deterministic
   `sched:{id}:{epoch(planned)}` dedup key via `start({key, onConflict:"join"})`, the `overlap`/`catchUp`
   advance (§12.2), and the schedule caps (§13). Faithful e2e (per `feedback_faithful_e2e_tests`): a fluent
   `every.day.at("03:00", tz)` and a raw `"*/5 * * * *"` both round-trip through `compileSchedule` →
   `workflow_schedules` → a real fired run with `trigger.startedAt` = the planned instant; a redeploy
   dropping a schedule deletes its row without orphaning; two concurrent sweeper nodes firing the same
   `(schedule, instant)` create **exactly one** run (dedup key); a `catchUp:{mode:"backfill",max}` after simulated
   downtime emits ≤ `max` runs each keyed on its own instant; a bad zone / sub-minute cron fails the build.
10. **Determinism/liveness hardening (§11)** — `NondeterministicError` detection, `stuck_strikes` →
    `StalledError`, wall-budget rollover tests.
11. **Streaming & large step outputs — blob-backed output rail (§3.5, §17).** `WorkflowBlobStore` (the
    `wfblob/` namespace over the existing `s3://`-vs-local resolver + verified `put_blob_stream` /
    `get_blob_to_file`); the worker-side content-addressed write path (auto-spill + `output: blob|stream`,
    mid-stream ceiling); the §7 output columns + `blob_bytes`/`journal_bytes` accumulators + the
    `workflow_blobs` refcount upsert co-committed in §7.4; `StepConfig.output` + the `step.run` overloads +
    `StepOutputRef` in `@zeroship/workflows`; the control-plane `…/runs/{runId}/output` +
    `…/steps/{name}/output` streaming read endpoints + `StatusOutput`; the two advisory-lock GC sweeps
    (ref-table + orphan). Threads through PR 1 (columns + `workflow_blobs` land in the create scripts), PR 3
    (replay rematerialize-vs-reconstruct-handle), and PR 8 (the ceilings ride `storage_bytes` metering).
    Faithful e2e: a step whose output exceeds the inline cap round-trips the identical value across a
    crash-mid-write replay; a stream write aborts + cleans the partial at `maxStepBlobBytes`; a GC sweep
    never deletes a still-referenced blob.
12. **External signal ingress & broadcast (§18).** The `workflow_signals` provenance columns +
    `signal_epoch` + the `workflow_signal_keys` / `workflow_broadcasts` / `workflow_subscriptions` tables
    (§7.3/§7.6/§7.7/§7.8) — landing in the PR 1 create scripts, minting the `wsk_`/`wbc_`/`wsb_` typed ids
    and the stateless `wst_` token codec. The gateway-edge route family
    `POST /__zeroship/signals/v1/{run|topic}/{addr}` with the three verifiers (`zeroship-hmac` reusing the
    `stripe_handlers.rs` constant-time/timestamp-tolerance logic, `bearer` `wst_` token verify against the
    app ingress key, `provider:stripe` foreign-signature + `topicFrom` addressing) + caps + edge metering
    (`wf_signals_ingress`/`wf_broadcasts`/`wf_fanout_deliveries`/`ingress_bytes`, accept-arm only). The
    control-plane mint/rotate/revoke endpoints (§18.8) + envelope-encrypted secret storage (P5 data key).
    `step.waitForSignal` `opts.topic` subscription + late-bind (§18.2); `env.workflows.publish` +
    `run.createSignalToken` in `@zeroship/workflows`. Extend the claim sweep with resumable fan-out + subscription
    GC (§18.6). Threads PR 1 (columns + tables in the create scripts), PR 5 (topic waits as legal
    `wait_signal` members), and PR 8 (edge counters ride the metering + spend path). Faithful e2e (per
    `feedback_faithful_e2e_tests`): a genuine HMAC-signed POST binds a `waitForSignal`; a forged/expired
    token is `401`; a type outside `externalSignals` is `403`; a retried idempotency key double-inserts
    zero rows; a 10k-subscriber publish fans out resumably across sweep passes with **exactly one** delivery
    per subscriber across a crash mid-fan-out; a bumped `signal_epoch` invalidates outstanding `wst_` tokens.
13. **Child / sub-workflow orchestration & batch start (§3.7, §20).** The §7.1 parent-edge +
    `tree_depth`/`cancel_requested` columns and indexes, `workflow_steps.child_run_id` + `kind='child'`, and
    the reserved `__zs.` type guard — **landing in the PR 1 create scripts** (pre-launch, no `ALTER`).
    Worker: the `step.call` shim (spawn co-commit in the parent §7.4 txn, park as a
    `wait_signal`-flavored step, bind + rethrow on join, blob-ref passthrough for a large child output),
    `ChildWorkflowOptions`, `ChildTimeoutError`/`ChildCancelledError`. Control plane: the terminal-child hook in the
    child's §7.4 terminal txn, the `startMany` batch endpoint, cascade-cancel (`cancel_requested` set on
    cancel + cooperative pickup at claim, §20.6), deploy-pin child = parent. `@zeroship/workflows`:
    `step.call`, `env.workflows.X.startMany`. **Threads PR 1** (columns/indexes in the create scripts),
    **PR 3** (replay rematerialize of a `kind='child'` step), **PR 4** (fan-out = concurrent frontier, no new
    code), **PR 5** (child await is a legal `wait_signal` member). Faithful e2e (per
    `feedback_faithful_e2e_tests`): a sequential `step.call` round-trips the child `Output` across a
    crash-mid-park replay with **exactly one** child (dedup key); a `Promise.all` of 100 `step.call`
    joins all children with results in issue order; a child `PermanentError` rethrows into and fails the
    parent while sibling children still commit; `run.cancel()` with `cascade:true` cancels a live child
    sub-tree without touching an independent (`cascade:false`) sibling; a retried `startMany` of 1k keyed
    items creates each run exactly once; a `__zs.`-prefixed `run.signal` is rejected `403`.
14. **Compensation / saga rollback (§3.8, §21).** Threads PR 1 / PR 3 / PR 4 / PR 6. **PR 1** create scripts
    gain the `workflow_steps` compensation columns + 2 indexes and the `workflow_runs`
    `compensation_target`/`compensation_outcome` columns + `compensating` in the wake index (schema lands
    day-1). **PR 3** (replay core) gains: the `compensation_state='pending'` stamp on completing compensable
    steps; the §9 re-dispatch-to-observe-catch fold refinement (terminal failure = uncaught throw escaping
    `run()`); and `compensatorRegistry` population during replay. **PR 4** (concurrent frontier) is reused
    for `compensationConcurrency` — no new machinery. **PR 6** (run controls) gains `cancel({ mode: "compensate" })`
    and the `compensating` state in `status()`. New engine surface: the reverse-ordinal frontier selection,
    the compensation fold + commit variant (§7.4), `StepConfig<T>.compensate`, `Compensator<T>`,
    `CompensationContext`, `static compensationConcurrency`. Faithful e2e (per `feedback_faithful_e2e_tests`
    / `feedback_regression_test_per_fix`), all on the real replay/dispatch/commit path against live
    control-plane Postgres: (a) a two-step saga where step 2 throws an uncaught `PermanentError` rolls back
    step 1's compensator exactly once and lands `failed` / `compensation_outcome='completed'`; (b) a **crash
    mid-compensator** re-runs the body (at-least-once) but the `completed` marker commits once (idempotency-key
    dedup verified against the external effect); (c) a **caught** step failure runs **no** compensator and
    the run completes; (d) `cancel()` runs no rollback, `cancel({ mode: "compensate" })` does and lands
    `cancelled`; (e) a compensator that exhausts its retries records `failed`, the walk continues to earlier
    steps, and the run ends `partial`; (f) a three-step chain rolls back in strict `3→2→1` order
    (reverse-ordinal assertion); (g) a `NondeterministicError` fails closed with no rollback.

Test discipline (per `feedback_faithful_e2e_tests`, `feedback_regression_test_per_fix`): e2e runs the **real**
replay/dispatch/commit path against live control-plane Postgres, including crash-mid-frontier, lease-handoff
double-commit, and wall-budget rollover cases; every fix carries a regression test that fails pre-fix.

---

## 16. Residual limits & open questions

**Honest residual limits (do not regress these in creator docs):**

- **Effects are at-least-once, not exactly-once**, and concurrency amplifies duplicates up to N× per crashed
  dispatch. Only the *journaled result* is exactly-once (§11 C2/C3).
- **No cross-step external transactionality.** The N-row atomicity covers the *journal*, not external side
  effects: a `fetch` that already POSTed cannot be rolled back if the commit later aborts.
- **Concurrency is I/O overlap, not parallelism** — no speedup for CPU-bound work (§6.5).
- **Only same-pass antichain steps overlap.** Sequential `await` chains remain one-step-per-dispatch — that
  is genuine data dependency, not an engine limitation.
- **Intra-frontier effects are unordered** (§6.3) — a *new* semantic surface vs single-frontier; must be
  prominent in creator docs.
- **Wide-frontier throughput under budget rollover is unknown / to-measure.** No numbers claimed.
- **Blob-output reads & materialization (§3.5, §17).** Orchestration-code reads of a `StepOutputRef`
  re-fetch (and re-meter) every dispatch — determinism is safe (immutable content-addressed bytes) but
  cost is not amortized; read inside a `step.run` (documented, not enforced). `.json()` materializes the
  whole payload against the V8 heap; `.stream()` is the only bounded-memory read for very large blobs.
  Streams are **not intra-step resumable** (the frontier is per-step, not per-byte) — a crash mid-consume
  re-streams from the start. Max practical blob size is bounded by worker staging temp disk (writes) and
  isolate heap (`.json()` reads) — **to-measure**.
- **Run input has no by-ref opt-in (§3.5).** It auto-spills over the inline cap but must be
  materialized to be passed to `run(...)`, so a very large input is still fully materialized in the
  isolate each dispatch. A `trigger.inputRef` handle is a plausible future addition — genuinely out of
  scope for this design, not a deferred piece of a shipped feature.
- **Genuine Stripe webhooks can't send our header (§18.7).** They are handled by `provider:stripe`
  (foreign-signature verify + `topicFrom` addressing), **not** `zeroship-hmac`. Apps that want a
  run-addressed Stripe delivery still often prefer a thin in-app RPC (already Stripe-verified by
  `@zeroship/payments`) that calls `run.signal` / `env.workflows.publish`. The public ingress shines for
  callers you hand a `wst_` token or who adopt our HMAC.
- **HMAC-without-idempotency-key can double-insert (§18.7).** A `zeroship-hmac` caller with no
  `Idempotency-Key` whose retry carries a fresh `t` derives a different key and *can* double-deliver.
  Mitigation is documentation (always send an idempotency key); we do **not** silently dedup by payload
  hash (that would wrongly drop legitimately-identical signals).
- **Broadcast is live-plus-bounded-retention, not a durable queue (§18.7).** A subscriber joining later
  than `maxSignalAge` after a publish misses it — deliberate (buffering every broadcast for every possible
  future subscriber is unbounded). Point-to-point remains the tool for guaranteed-eventual single-run
  delivery. Fan-out tail latency to very wide topics is batch-bounded by the sweep cadence + `max_fanout` —
  **unknown / to-measure**, not claimed instantaneous.
- **`signal_epoch` revocation is coarse (§18.1)** — whole-run, all outstanding tokens at once. No
  per-token revocation list day-1 (avoids a hot revocation table); add one only if a real need appears.
- **`step.sleepUntil` wake is best-effort `≥ when`, not on-the-dot (§3.2, §8).** The one-sided guarantee
  holds — the run is **never** woken before `when` (the `wake_at ≤ now()` predicate uses the single DB
  clock, so no wall-clock skew between the author's machine, the caller, and the control plane can wake it
  early) — but there is no upper bound on lateness beyond scheduler pressure (sweep cadence + dispatch
  backlog), mirroring the schedule "fire timeliness" residual. Latency is **unknown / to-measure**. No max
  suspension horizon is enforced day-1; if one is later introduced it is a *shared* operator knob for
  `sleep`/`sleepUntil` alike (§13, to-measure).
- **Schedule DSL residual limits (§3.4, §12).** **Sub-second scheduling is unsupported** and intentionally
  so; the useful interval floor is unknown/to-measure. **`interval` + `anchor:"epoch"` alignment can
  surprise** for non-divisor periods: `every(90,"minutes")` fires at epoch-aligned 90-min boundaries
  (…00:00, 01:30, 03:00 UTC…), not "90 min after deploy" — use `anchor:"deploy"` for the latter; both are
  deterministic. **DST edge rules (cron+tz), stated exactly:** spring-forward *nonexistent* local time →
  fire at the **first valid instant after** the nominal time; fall-back *ambiguous* local time → fire
  **once, at the first occurrence** (pinned-tzdb rules — never silently double-fire or skip). **tzdb version
  pinning:** `tz` next-time uses a tzdb pinned to the control-plane binary/deploy; a tzdb bump can shift
  *future* wall-clock fire times for zones whose rules changed but **cannot** cause replay divergence —
  already-created runs have `started_at` frozen in the journal; only not-yet-computed `next_fire_at` values
  move. **Calendar corners genuinely out of scope:** "last day of month" / "nth weekday" (`L`/`#` cron extensions) are
  **not** supported by the fluent DSL or the raw parser, and `every.month.on(d)` is capped at `d ≤ 28` to
  avoid the 29–31 short-month ambiguity — additive future cadences (a new fluent method or a new stored
  `kind`, §12), and pre-launch we owe no back-compat for the gap.

- **Child orchestration residual limits (§3.7, §20).** **Fire-and-forget children are out of scope** —
  `step.call` always awaits (a joined child); "spawn a detached child from inside a run and don't await
  it" is served by `startMany`-style top-level starts, not by `step.call`. A `step.spawn` detached
  handle is a plausible future addition, genuinely out of scope here (not a deferred slice). **Independent-mode
  orphans keep consuming** — after an independent parent cancel, live children run to terminal (metered as
  their own runs) before GC; a hard "kill everything" is only via `cascade:true`. **Cascade cancel is
  cooperative, not instantaneous** — a live cascade-child transitions at its **next claim** (§20.6); a child
  mid-dispatch finishes that dispatch (or fails its lease guard) first. Deep-tree cascade latency is bounded
  by tree depth × sweep cadence — **unknown / to-measure**. **Unkeyed batch items are at-least-once** (a new
  run per `startMany` call); `step.call`'s default key derives from the parent step **ordinal**, so
  inserting a step *before* it across a code edit would change the key — but children pin to the parent's
  `deploy_id` (§20.7), so an in-flight parent never sees a shifted ordinal; only *new* runs on the new deploy
  get the new key. Supply `opts.key` for a code-edit-stable identity. **No child-run sharing across parents**
  — content-addressed dedup (§17) shares *bytes*, not *runs*; two parents awaiting "the same" child each spawn
  their own (distinct `child_key`); a shared-singleton child is modelled with an app-level lock inside the
  child (as with `overlap:"skipIfRunning"`, §12.4), not a fabricated cross-parent join.

- **Compensation / saga rollback residual limits (§3.8, §21).** **Rollback is best-effort, at-least-once
  undo — not a distributed transaction.** A committed external effect a compensator cannot reverse yields
  `compensation_outcome='partial'`; the engine surfaces it (`run.error.compensation`), it does **not**
  guarantee the world is clean (no cross-step external transactionality — unchanged above). **Cancel does not
  compensate by default** — opt in with `cancel({ mode: "compensate" })` (justification below).
  **`NondeterministicError` / forward `StalledError` fail closed without rollback** — the journal is untrusted /
  no progress was possible; deliberate. **Compensators cannot nest `step.*`** — a compensator is a leaf
  durable unit; its own retry budget is its durability. (A "compensation sub-workflow" is a plausible future
  addition, genuinely out of scope, not a deferred slice.) **Effects are at-least-once** — crash/rollover
  re-runs a compensator body, so a duplicate undo effect is possible; idempotency (`ctx.idempotencyKey`) is
  the author's obligation, fundamental and not fixable by the engine.

- **Replay-from-step / restart residual (§3.1, §7.10, §22) — the deploy-pin split is a resolved design
  decision, not an open question.** Dropped steps **re-run at-least-once** — restart is *deliberate*
  re-execution (the §6.3 contract applied on purpose), so make step bodies idempotent; un-consumed mailbox
  signals may re-bind if still within `maxSignalAge` (usually stale → the run waits fresh). A **partial**
  restart is **rejected** (`RestartError`, §22.1/§7.10 step 1b) when any retained ordinal has a *settled*
  compensation (`compensation_finished_at` set) — forward replay would silently resume a reservation the
  compensator already released; the sanctioned revive for a rolled-back run is a **full** restart. The
  deploy-pinning is **decided** (§4/§22.3): a **partial** restart is pinned-original-and-immutable (a retained
  prefix is only valid against the deploy that produced it → `deploy` re-pin with `from` rejected, `RestartError`); a
  **full** restart defaults to re-pin-current (the semantics of a fresh `start()`), `deploy:"started"` for
  exact reproduction; any re-pin bumps `signal_epoch`. This makes the *safe* thing the only thing for partial
  restarts and the *useful* thing the default for full restarts — with **no** path that can silently break
  replay determinism (§11 C9). (Considered and rejected: allowing `deploy:"latest"` with `from`, which would
  require the new code to reproduce the retained prefix byte-for-byte and surface any divergence as
  `NondeterministicError`.)

- **Why `cancel()` defaults to *no* compensation (§3.8/§9).** `cancel()` in this engine is a control-plane
  transition (§9), orthogonal to the frontier fold — today it is instant and terminal. Saga compensation, by
  contrast, is workflow-domain logic that must **hold the lease, re-dispatch, and run app code** (replay +
  compensators that may themselves retry/backoff). Turning "stop this now" into "run more app code, possibly
  for a while" is surprising and can defeat the very intent of cancel (e.g. a wedged app being aborted).
  Compensation is for *business rollback on failure*; abort is *operator intent to stop*. We therefore keep
  the default `cancel()` path byte-identical to today (→ `cancelled`) and expose rollback-on-cancel
  explicitly as `cancel({ mode: "compensate" })` — full power, safe default, a clean addition that leaves the
  existing transition untouched. (Considered and rejected: unconditional compensate-on-cancel, which some
  engines do, because it makes "cancel" unbounded and non-instant.)

**Open questions:**

- **Dispatch/batch id prefix — RESOLVED.** The workflow dispatch/batch id is `wfd_` (a w-family prefix),
  **not** `dsp_` — `dsp_` is already the billing-**dispute** typed-id prefix
  (`crates/zeroship-core/src/typed_id.rs`), so reusing it would collide the disjointness assertions. `wfd_` lands in
  the PR 1 create scripts alongside the other w-family workflow prefixes (`wsk_`/`wbc_`/`wsb_`). No open
  question remains.
- **Terminal-run retention / GC** of `workflow_steps` / `workflow_signals` (operator policy) — and, on
  prune, the co-committed `workflow_blobs` refcount decrement (§7.5). `run.restart` (§7.10, §22) revives
  terminal runs, so retention must not prune a run's journal so aggressively that a restart-eligible prefix
  is lost before the policy window; an optional full restart-history log lives here too. Child runs must be
  pruned **leaf-up** (children before parents) — `workflow_runs.parent_run_id` is `ON DELETE RESTRICT`
  (§7.1), so a parent cannot be pruned while a child row still references it; the retention pass orders
  deletes by `tree_depth` descending.
- **Cron catch-up policy** is now **resolved** and day-1 (§12.2): `catchUp: { mode: "skip" }` (default) fast-forwards
  past all missed instants and fires only the current period; `catchUp: { mode: "backfill", max }` emits up to `max`
  runs for missed instants, each keyed on its own instant. Open only: the seeded default for
  `SCHEDULE_BACKFILL_HARD_MAX` and the sweep tick cadence (operator-config, **to-measure**, §13).
- **Blob GC grace defaults & cadence** (ref-table grace < orphan grace) — operator-config, **to-measure**
  (§17.6).
- **Hard-delete of a specific creator's blob bytes.** Content-addressed dedup defers deletion: "delete my
  run's data" drops the *reference*; bytes persist until global `refcount` hits 0 and grace elapses (a
  second run with identical content keeps them alive). Immediate per-creator hard-delete is not offered —
  flag for any data-residency / GDPR follow-up (§17).
- **External-ingress rate-limit backing store (§18.6).** The edge token bucket lives in `env.kv`/redis
  (preferred — no PG write on reject) vs an optional persisted `zeroship.workflow_ingress_limits` config
  row per app seeded from plan defaults. Which is default, and whether per-source buckets need eviction
  policy, is open.
- **Broadcast retention & subscription GC cadence (§18.6).** `workflow_broadcasts.expires_at` window and
  the sweep cadence for pending fan-out + expired subscriptions — operator-config, **to-measure**.
- **`bearer-signing` vs `bearer` naming.** The request-time verifier is `bearer`; the `workflow_signal_keys`
  row that holds the `wst_` token-signing material carries `verifier='bearer-signing'` (§7.6). The concrete
  implementation should keep these two roles clearly distinct in the codec + verifier registry.
- **Deep-tree `maxLiveDescendants` accounting (§20.9).** Whether the live-descendant cap is enforced by an
  aggregated counter on the root vs. an on-spawn `parent_run_id` walk is an implementation open question (the
  walk is simplest but O(tree); a maintained counter is O(1) but needs co-commit bookkeeping). Defaults
  **to-measure**.
- **Numeric defaults** for `platform_ceiling[tier]`, `wall_budget`, `lease_ttl`, `stuck_strikes` bound, per-step
  output cap, the blob ceilings (`maxStepBlobBytes`, `maxRunBlobBytes`) + GC grace windows, the ingress
  caps (`max_qps`, `max_body_bytes`, `max_topics`, `max_fanout`, `max_subscribers_per_topic`, token `ttl`,
  signature timestamp tolerance), and the child-orchestration caps (`maxChildDepth`, `maxLiveDescendants`,
  `maxStartManyBatch`) — all to-measure / operator-config.

---

## 17. Streaming & large step outputs — blob-backed output rail

*Day-1 scope. The engine mechanics behind the developer surface in §3.5. This rail changes only the
**representation** of a recorded output — it adds **no** new frontier state, **no** new suspension reason
(§8 is untouched), and **no** new creator-facing `env.*` primitive. It reuses the existing
content-addressed blob machinery (`crates/zeroship-bundle/src/blob.rs`: SHA-256 hex addressing,
`put_blob_stream(hash, size, reader)` with verified writes + partial cleanup, `has_blob`,
`get_blob_to_file` verifying SHA-256 before handback, the sharded `<hash[0..2]>/<hash[2..]>` layout, and
the `s3://`-vs-local store resolver).*

### 17.1 Problem & scope

The journal stores each step's result as inline `jsonb` (`workflow_steps.output`), capped at **1 MiB**.
That cap survives this feature — it exists for two reasons we keep intact: (1) **control-plane Postgres
pressure** — inlining multi-MiB blobs into `jsonb` bloats rows/TOAST/WAL and every replay read; (2)
**replay cost** — each dispatch re-reads recorded outputs. A step whose output legitimately exceeds 1 MiB
had no rail; this adds one: payload → object storage (content-addressed), journal → a bounded reference,
reads → lazy streams. **Out of scope / unchanged:** the 1 MiB inline cap itself (a rail beside it, not a
raise); the single-frontier replay core (§5/§6); `wake_at` suspension (§8); `claimed_by` leasing (§4);
deploy-pinning.

### 17.2 Where the bytes are written — the worker, not the control plane

The step `fn` runs in the **worker** V8 isolate, which already holds an object-storage handle (it reads
bundle blobs). So the **worker writes the output blob directly to the workflow blob store** (§17.3),
computes the content hash, and reports only `{ output_kind: "blob", output_hash, output_size,
output_content_type }` back to the control plane in the dispatch-completion envelope. Multi-MiB payloads
never traverse the control-plane request path or land in Postgres. At the moment a step `fn` returns:

1. **Materialize-or-stream to a hashing writer.** *JSON modes* (`auto`/`blob`): `JSON.stringify(value)` →
   UTF-8 bytes; for `auto`, `len ≤ 1 MiB` short-circuits to inline (no blob written). *Stream mode*:
   consume the returned `ReadableStream`/`AsyncIterable`/`Blob` chunk-by-chunk, never buffering the whole
   payload, enforcing the per-step ceiling mid-stream (§13).
2. **Content-address on write.** Stream bytes through a SHA-256 hasher into a bounded compio staging sink
   (temp file — zero tokio), yielding `(hash, size)`, then publish via the verified `put_blob_stream(hash,
   size, reader)` (re-verifies hash+size, removes the partial on mismatch). `has_blob(hash)` is a **perf
   hint only** — the worker **always writes** so the object is present regardless of concurrent GC
   (§17.6). Identical content ⇒ identical hash ⇒ same object (idempotent).
3. **Meter** on the success arm only: `storage_ops += 1`, `storage_bytes += size` (§10). A `has_blob` hit
   still counts `storage_ops`; `storage_bytes` reflects bytes actually stored.
4. **Report** the ref to the control plane, which performs the frontier advance + refcount upsert in one
   txn (§7.4).

**External read path.** Observers (dashboard, `@zeroship/control`) stream a blob-backed output via the
control-plane endpoints `GET /v1/apps/{app}/workflows/runs/{runId}/output` and `…/steps/{name}/output`,
which authorize on the run's app, then stream the blob from the workflow blob store (emitting
`egress_bytes`, §10) — keeping large outputs off the JSON `status()` path entirely.

### 17.3 `WorkflowBlobStore` — a distinct, GC-able namespace

Workflow output blobs live under their **own** store prefix, `wfblob/`, separate from deploy/bundle
blobs (`blobs/`). `WorkflowBlobStore` is a thin wrapper over the same `s3://…`-vs-local resolver and
`LocalDiskBlobStore` / `S3BlobStore` machinery, reusing `sha256_hex`, `validate_hash_format`, and the
verified `put_blob_stream` / `get_blob_to_file` paths (path = `wfblob/<hash[0..2]>/<hash[2..]>`).

- The **bundle BlobStore invariant is untouched** — deploy artifacts and their separate GC domain are
  unaffected.
- Workflow blobs get an **independent GC domain**: a sweep (§17.6) lists only `wfblob/…`, never the
  bundle space.
- Content addressing stays **global by hash** within `wfblob/`, so identical outputs dedup across steps
  and runs. This does **not** leak across apps: reading a blob requires presenting its exact 64-hex
  SHA-256 — an unguessable 256-bit capability you can only hold by already knowing the content (the same
  isolation the deploy BlobStore relies on). App attribution for billing is captured at write-time
  metering, not in the blob path.

### 17.4 Frontier advance & replay — representation only

**Frontier advance** (integrates into §6 / §7.4) is unchanged in *shape and keying* — still one row per
`(run_id, ordinal)`, still `INSERT … ON CONFLICT (run_id, ordinal) DO NOTHING`, still the single linear
frontier. Only the output columns change (inline: `output_kind='inline'`, `output` set; blob:
`output_kind='blob'`, `output_hash`/`output_size`/`output_content_type` set, `output` NULL). In the
**same txn** the control plane upserts the `workflow_blobs` refcount row (§7.5). A blob write is
synchronous within a dispatch — no new frontier state, no interaction with `sleep`/`waitForSignal`, no
lease change.

**Replay** (integrates into §5 step 2 / §11 C1) when it reaches a recorded blob-backed step:

- **auto-spill step** → fetch the blob (`get_blob_to_file`, verifying SHA-256 *before* handback),
  `JSON.parse`, return the value `T` — the identical value the inline path would have returned (reads
  memoized per dispatch, §17.6).
- **by-ref step** → return a `StepOutputRef<T>` **reconstructed purely from the journal row** (`ref`,
  `hash`, `size`, `contentType`) with **no I/O**; bytes are fetched only if user code calls
  `.json()`/`.stream()`/etc. This is why by-ref avoids auto-spill's per-replay rematerialization cost.

Either way the frontier walk returns the same logical result it recorded — replay stays deterministic
(§11 C1/C5).

### 17.5 Accounting — `journal_bytes` (tight) vs `blob_bytes` (loose)

The two pressures of §17.1 map to two per-run accumulators (§7.1), both maintained in the commit txn
(§7.4):

- **`journal_bytes`** (Postgres pressure, tight cap) counts per run: fixed per-row overhead +
  `octet_length(output)` for **inline** rows + a **fixed `BLOB_REF_COST`** (the 64-hex hash + size +
  content-type + column overhead — a small constant, ~128 bytes, pinned in the accounting module) for
  **each blob row**. A blob-backed step therefore adds a *bounded constant* to `journal_bytes` regardless
  of payload size — the mechanism by which a run can emit gigabytes of step output without tripping the
  journal-size cap.
- **`blob_bytes`** (object-storage consumption, loose cap) sums `output_size` across the run's
  blob-backed step + input + final-output rows; it has its own per-run ceiling (§13) and is the value
  reflected into `storage_bytes` metering.

### 17.6 GC & read memoization

**Read memoization.** Within a single dispatch, blob fetches are **memoized by hash** (dispatch-scoped
cache of raw bytes + parsed value): N reads of a ref ⇒ 1 fetch ⇒ 1 metered `storage_ops`. Across
dispatches, orchestration-code reads are cold and re-fetch — real, metered I/O (§13, §16).

**GC — two conservative sweeps**, run as control-plane compio tasks (never creator code), serialized by a
Postgres **advisory lock** — the same claim-sweep pattern the engine uses for `claimed_by` lease
reclamation (§4), so at most one sweeper runs cluster-wide:

- **Ref-table GC** deletes a `wfblob/` object only if its `workflow_blobs` row has `refcount = 0` **and**
  `last_referenced_at` is past the grace window; it row-locks + re-checks `refcount = 0` at delete time,
  losing to any concurrent re-reference. Because the worker **always writes** the object before reporting
  (never trusting `has_blob` for correctness), a concurrent run that re-references a just-GC'd hash
  re-materializes it — never dangling.
- **Orphan GC** handles objects present in `wfblob/` with **no** `workflow_blobs` row (crashed-worker
  orphans, non-deterministic-retry losers), deleting only those past a **strictly longer** grace window,
  so a slow control-plane commit (worker wrote the object; control has not yet inserted the ref row)
  always wins the race.

A blob is deleted only when *no journal row references it* **and** it is *past grace*; a live reference
always keeps its bytes. Cadence + grace defaults are **to-measure / operator-config** (§13, §16).

### 17.7 Correctness (relative to §11)

- **Single-frontier core preserved.** This feature changes only the columns carrying a recorded output's
  bytes — never a row's existence, key, ordinal, ordering, or the `ON CONFLICT DO NOTHING` insert.
  Replay returns the same logical value per step (rematerialized or as a handle). The §11 antichain /
  frontier-determinism argument (C1) is unaffected.
- **Recorded-exactly-once (C2).** Blob writes are idempotent (content addressing); the journal insert is
  `ON CONFLICT (run_id, ordinal) DO NOTHING`, so **exactly one** `hash` is ever recorded per step even
  under competing dispatches, and the refcount upsert co-commits. Effects stay at-least-once (C3); a
  non-deterministic retry that writes a *second* hash leaves a GC-collected orphan — the blob store never
  corrupts.
- **Determinism (C5).** A recorded `hash` addresses immutable bytes verified on read; every replay of
  `.json()`/`.stream()` yields byte-identical content. Auto-spill round-trips the identical value
  (`JSON.stringify` ↔ `JSON.parse`, the same JSON-serializable class the inline path already requires);
  run-input auto-spill is a pure function of the recorded `input_hash`. Handle construction is pure (no
  I/O). Reads in orchestration code re-execute but return identical immutable bytes — determinism is not
  threatened, only cost (§16).
- **GC safety.** No live blob is ever deleted — the `refcount = 0` + past-grace + always-write argument of
  §17.6.
- **Content-type is advisory.** The engine treats blob bytes opaquely; `.json()` on non-JSON bytes throws
  a parse error to user code — deterministic given the fixed bytes, identical on every replay.

### 17.8 Integration checklist

| Touches | Where |
| --- | --- |
| Developer API — `StepConfig.output`, `step.run` overloads, `StepOutputRef`, `StatusOutput`/`status()` | §3.5 |
| Journal / DDL — output columns on `workflow_runs`/`workflow_steps`, `workflow_blobs`, refcount co-commit | §7.1, §7.2, §7.4, §7.5 |
| State machine — output representation is orthogonal to state | §9 |
| Metering — `storage_ops`/`storage_bytes` (write), `storage_ops`/`egress_bytes` (read) | §10 |
| Limits — blob ceilings, read amplification, GC scheduling | §13 |
| Invariants — all core invariants preserved; feature delta (content-address-not-typed_id) | §14 |
| Build plan — PR 11 (threads PR 1 / PR 3 / PR 8) | §15 |
| Residual limits & open questions | §16 |

Everything else — `wake_at` suspension, `claimed_by` leasing, deploy-pinning, the claim sweep,
zero-tokio, the small native surface — is untouched.

---

## 18. External signal ingress & broadcast

*Day-1 scope. The engine mechanics behind the developer surface in §3.6. Two producers of
`workflow_signals` rows: (a) a public, signed, rate-limited **signal ingress** edge so systems outside the
app (a Stripe webhook, a partner callback, an IoT device) deliver a signal to a run **without** the app's
control credential, and (b) **broadcast / topics** — one signal fanned out to many runs matched by key,
alongside today's point-to-point `(run, type)` delivery. **Non-negotiable:** neither half touches the
single-frontier replay core (§5/§6) or `wake_at`-unified suspension (§8). Both only produce journal rows;
the replay engine is unchanged and every run stays individually deterministic off its own journaled
bindings. The HMAC verify reuses the shipped logic in `crates/zeroship-control/src/stripe_handlers.rs` — constant-
time compare, `t=<unix>,v1=<hex>` scheme, timestamp tolerance, hard cap on `v1=` entries.*

### 18.1 Auth model — three inbound verifiers, none the control credential

The load-bearing decision: the control credential (`ctl_…` / deploy PAT) **must never** be the thing a
webhook holds. Three inbound **verifiers**, all app-scoped, none of which is the control credential:

1. **`zeroship-hmac` (per-app shared secret).** For callers who can adopt our scheme.
   `Zeroship-Signature: t=<unix>,v1=<hmac-sha256(secret, "<t>.<raw-body>")>`, verified with the exact
   `stripe_handlers.rs` logic (constant-time compare, `|now − t| ≤ tolerance` default 300s, hard cap on
   `v1=` entries, UTF-8-safe body handling). Grants: deliver **any** `externalSignals`-allowed type to any
   run/topic in the app.
2. **`bearer` (per-run capability token).** Least-privilege. A **stateless signed** token (`wst_…`,
   HMAC-SHA256 over canonical claims `{app_id, run_id | topic, types[], exp, epoch}` signed with the app's
   `bearer-signing` ingress key material, §7.6) that app code **requests** via `run.createSignalToken(...)`. The
   mint itself is a **control-plane call** — the ingress terminus does the signing and the `wsk_` secret
   never leaves the control plane (§18.8); the worker never holds or signs with the raw key. Carried as
   `Authorization: Bearer wst_…` **or** embedded in the path for callers that can't set headers. Grants:
   exactly the addressed run/topic and the token's `types`, until `exp`. Revocation is (i) natural — a
   terminal/cancelled run rejects signals (§9); and (ii) explicit — bump `workflow_runs.signal_epoch`
   (§7.1) to invalidate every outstanding token whose `epoch` no longer matches. No DB row per token (the
   token is a signed string, not stored) — the coarse whole-run `signal_epoch` is the deliberate
   revocation surface (§16).
3. **`provider:<name>` (foreign signature).** For webhooks that sign with their **own** scheme and can't
   carry our header — **the honest Stripe path**. `provider:stripe` reuses the app's stored Stripe webhook
   secret and the existing Stripe verifier. Because the provider doesn't address a zeroship run, addressing
   is derived from a **declared payload path** (`topicFrom`, §3.6): "Stripe event for order X" becomes
   "publish to topic `order:X`". This is precisely why ingress and broadcast are **one** feature.

The per-app HMAC secret and the Stripe webhook secret are stored **encrypted at rest**
(envelope-encrypted with the platform data key — the P5 machinery, same as `zeroship.signing_keys`) in
`workflow_signal_keys` (§7.6) and decrypted only in the **control-plane ingress terminus** to verify (the
gateway holds no secret; §4). HMAC is symmetric, so the platform must hold the secret — mirroring how the
Stripe verifier already works. `externalSignals` is the **code-declared, deploy-pinned allowlist** (§18.5):
an external caller can never inject a type outside it.

### 18.2 Engine mechanics

**Point-to-point external ingress — one topology (§4).** The gateway *forwards* the ingress route to the
control-plane ingress terminus and applies only the edge rate-limit; the control plane does everything below.
No app code runs on the path; the journal write is a **control-plane** write.

1. **Rate-limit (gateway) → forward.** The gateway checks the per-app/per-source-token bucket
   (`env.kv`/redis, no PG) and, if under budget, forwards `POST /__zeroship/signals/v1/run/{runId}` to the
   control-plane ingress terminus. Over rate → `429` at the gateway, never reaching PG or metering.
2. **Verify (control plane).** The terminus loads the app's ingress key(s)/verifier
   (`workflow_signal_keys` + the deploy registry), decrypts in-proc, and runs the matching verifier
   (HMAC / bearer / provider). Failure → `401`/`403` — **not billable** (you can't bill for rejected spam).
3. **Allowlist + caps (control plane).** Parse `type`; reject if `type ∉ deploy.workflow.externalSignals`
   (`403`) or payload over cap (`413`), before any DB write and before metering.
4. **Journal write (control plane — the only state change).** One statement, `ON CONFLICT DO NOTHING` on the
   directly-addressed idempotency index (§7.3):
   ```sql
   INSERT INTO zeroship.workflow_signals
     (id, run_id, type, payload, origin, delivery, idempotency_key, provider, created_at)
   VALUES (…, 'ingress', 'direct', …)
   ON CONFLICT (run_id, type, idempotency_key) WHERE idempotency_key IS NOT NULL AND delivery <> 'topic'
   DO NOTHING RETURNING id;
   ```
   then `UPDATE zeroship.workflow_runs SET wake_at = now() WHERE id = $run AND state IN ('running','sleeping','waiting')`.
   If the run isn't waiting and `delivery=buffered` (default, matching point-to-point buffering + `maxSignalAge`,
   §8) the row sits until the run reaches the await; if `delivery=waitingOnly`, `404`.
5. **Respond `202`** (or `200 {duplicate:true}` when `ON CONFLICT` matched nothing). **No app code runs on
   the ingress path** — dispatch happens later when the claim sweep picks up the woken run (this is what keeps
   the gateway dumb, the journal control-plane-owned, and the replay core untouched).

**Broadcast publish + fan-out (control plane, §4).** Publish (internal `env.workflows.publish` — a
control-plane call, not a worker write — or external `POST …/topic/{key}` forwarded to the ingress terminus)
is *ingest-only*, one atomic control-plane insert into `workflow_broadcasts` (§7.7) with
`ON CONFLICT (app_id, topic, idempotency_key) DO NOTHING`. If the row is **new**, the control plane
**opportunistically** fans out the first batch (up to `max_fanout`) in the same request; the rest is
completed by the sweep. If nothing was inserted (duplicate), it's a no-op (`200`). Fan-out — both the
opportunistic batch and every sweep pass — is idempotent and resumable:

```sql
WITH targets AS (
  SELECT s.run_id
  FROM zeroship.workflow_subscriptions s
  WHERE s.app_id = $app AND s.topic = $topic
    AND (s.type_filter IS NULL OR s.type_filter = $type)
    AND (s.expires_at IS NULL OR s.expires_at > now())
    AND NOT EXISTS (SELECT 1 FROM zeroship.workflow_signals d
                    WHERE d.broadcast_id = $bct AND d.run_id = s.run_id)
  ORDER BY s.created_at
  FOR UPDATE SKIP LOCKED
  LIMIT $batch
)
INSERT INTO zeroship.workflow_signals
     (id, run_id, type, payload, origin, delivery, topic, broadcast_id, created_at)
SELECT gen(), run_id, $type, $payload, $origin, 'topic', $topic, $bct, now() FROM targets  -- $origin = the broadcast's origin ('app'|'ingress')
ON CONFLICT (broadcast_id, run_id) DO NOTHING;
-- then: UPDATE workflow_runs SET wake_at = now() WHERE id = ANY(<delivered run_ids>);
```

A pass that finds zero undelivered targets sets `fanout_state='completed'`. The `(broadcast_id, run_id)`
unique index (§7.3) makes any retry/crash mid-fan-out safe — re-running skips already-delivered runs. This
is how "publish to 10k subscribers" degrades gracefully: bounded per pass, resumable, never double-delivered.

**Subscribe (control plane, from the dispatch envelope — §4).** When a run reaches
`waitForSignal(name, { topic })` on a dispatch and no matching signal is already bound in its journal, the
worker records a *subscribe request* in its dispatch-completion envelope; the **control-plane §7.4 commit
txn** then (i) upserts the `workflow_subscriptions` row `(run_id, ordinal)` (§7.8), (ii) **late-binds** any
retained broadcast within `maxSignalAge` (`SELECT … FROM workflow_broadcasts WHERE app_id=$ AND topic=$ AND
created_at ≥ now()−max_age AND NOT already-delivered-to-this-run ORDER BY created_at DESC LIMIT 1`, then
inserts the delivery row for itself), and (iii) if still nothing, sets `wake_at` to the `timeout` deadline and
suspends — identical to a mailbox wait (§8). The worker never writes the subscription row itself (§4 grants).

**Sweep additions (the one existing scheduler).** The `wake_at` + advisory-lock claim sweep (§4) gains two
idempotent chores under the same lease: **drive `fanout_state='pending'` broadcasts** (batched fan-out) and
**GC** expired subscriptions + broadcasts past `expires_at` (§18.6). No new tokio task, no new scheduler —
the same compio sweep loop, honoring `claimed_by` leases so two nodes never double-fan-out (the
`FOR UPDATE SKIP LOCKED` + `(broadcast_id, run_id)` index make even a racing sweep safe).

### 18.3 Replay determinism (relative to §11)

The replay engine binds the *Nth* `waitForSignal(name)` occurrence in a run to **one** `workflow_signals`
row and memoizes that binding (§8, §11 C1). Ingress and broadcast only ever **produce** such rows — they
never call app code, never re-order the journal, never touch step memoization. At bind time the selection
rule is deterministic: the **earliest matching, unconsumed row by `(created_at, id)`** among rows visible to
that await (its `(run, type)` mailbox, or for a topic its `broadcast_id` deliveries), subject to
`maxSignalAge` measured against the await's **deterministic clock** (`trigger.startedAt` + journaled step
timings), not wall-clock at replay. Once bound, the binding is journaled (`consumed_signal_id`, §7.2), so
every subsequent dispatch replays the identical payload. **External arrival order is external nondeterminism
collapsed to a total order by the journal at first observation** — exactly like which of two concurrent
internal signals wins today (§11 C5). The single frontier is preserved because each run advances against its
own journal alone.

**Determinism of "who received a broadcast."** The *set* of runs a publish reaches is the set subscribed at
fan-out time — inherently racy. That race is **not** a determinism violation: it is resolved once into
concrete per-run delivery rows, after which each recipient replays deterministically off its own row. A run
that wasn't subscribed simply has no row and no bind — identical to a point-to-point signal sent to a run
that wasn't waiting. Determinism is a **per-run** property and it holds (§11 C1/C5 unaffected).

### 18.4 Crash safety & at-least-once

- **Caller → journal is at-least-once, ingest is exactly-once.** Callers (Stripe et al.) retry ⇒
  at-least-once attempts. The `idempotency_key` unique indexes (`(run_id, type, idempotency_key)` for
  direct, `(app_id, topic, idempotency_key)` for broadcast, §7.3/§7.7) collapse retries to **exactly-once
  ingest**.
- **Journal → run is effectively-once.** A woken run dispatches at-least-once (crash after `wake_at` set,
  before/after dispatch, re-dispatches); idempotency comes from the replay core (§11 C2/C3) — step results
  are memoized, the signal binding is fixed, so a re-dispatch replays committed steps rather than re-running
  them. Net effect to app code: **effectively-once processing** of each distinct signal.
- **Publish is atomic; fan-out is idempotent + resumable.** Publish is a single insert (all-or-nothing).
  Fan-out is idempotent per `(broadcast_id, run_id)` and resumable from `fanout_state='pending'`; a crash
  mid-fan-out re-runs only the undelivered remainder.
- **Ingress write + `wake_at` safety net.** If the control-plane ingress terminus (§4) dies after the signal
  `INSERT` but before the `wake_at` update, the sweep's periodic "runs with an unconsumed signal newer than
  their last dispatch" scan re-arms `wake_at` (the same scan the buffered point-to-point case already relies
  on; broadcasts reuse it).

### 18.5 Deploy-pinning & the exactly-once ingest boundary

`externalSignals`, the `inbound` verifiers, and `topicFrom` are **pinned to the run's deploy** (`deploy_id`
on `workflow_runs`, and on `workflow_broadcasts` for the topic definition, §7.7). A run started on deploy A
is validated against deploy A's allowlist even if the app has since redeployed — a redeploy can't
retroactively admit a signal type into an in-flight run. The exactly-once ingest boundary is the two partial
unique indexes: a missing idempotency key is **derived** — `sha256(t . body)` for HMAC callers (a fresh-`t`
retry *can* double-insert — the documented residual, §18.7), and the provider's event id for
`provider:*`. We deliberately do **not** dedup by payload hash (that would drop legitimately-identical
signals).

### 18.6 Abuse / limits & GC

| Vector | Control |
| --- | --- |
| Forged signals | Signature/token verify at the control-plane ingress terminus (§4); failures never reach the journal and never bill. Constant-time compare + timestamp tolerance (reused `stripe_handlers.rs` logic) blocks replay/timing attacks. |
| Type injection | `externalSignals` deploy-pinned allowlist (§18.5); per-run tokens further scope to `types[]`. |
| Ingress flood | Per-app **and** per-source-token token buckets (`max_qps`), preferably in `env.kv`/redis at the edge (no PG write on reject). `429` on breach, enforced *before* the DB write and metering. |
| Payload bloat | `max_body_bytes` (`413`); `payload` stored as `jsonb` under a size cap. |
| Fan-out amplification | `max_fanout` per pass + `max_subscribers_per_topic`; a topic over the subscriber cap rejects new subscriptions rather than fan out unboundedly. Fan-out is batched/bounded per sweep pass so one publish can't monopolize the sweep. |
| Topic sprawl | `max_topics` per app; topic keys are app-namespaced + length-capped. |
| Retention exhaustion | `workflow_broadcasts.expires_at` TTL + GC sweep; `maxSignalAge` bounds how far back a late subscriber looks. |
| Metering integrity | Edge counters (`wf_signals_ingress`, `wf_broadcasts`, `wf_fanout_deliveries`, `ingress_bytes`) on the **accept arm only**, attributed to `app_id` — app-unforgeable, app-unsuppressable (§10). Rejected requests aren't billable. |
| Token theft | Short `ttl`; scope to one run + type set; revoke by bumping `workflow_runs.signal_epoch`. Per-app secret rotation via `status IN ('active','next','retiring','retired')` (§7.6), mirroring `signing_keys`. |

**GC.** The sweep (§18.2) DELETEs `workflow_subscriptions` past `expires_at` and `workflow_broadcasts` past
`expires_at`; both are advisory-lock serialized like the `claimed_by` claim sweep, so at most one sweeper
runs cluster-wide. Terminal-run retention of `workflow_signals` (and the `broadcast_id` rows) rides the same
open retention/GC policy as the rest of the journal (§16). Cap **values are to-measure / operator-config**,
seeded from the plan catalog — no QPS or fan-out numbers are invented here.

### 18.7 Residual limits (honest)

- **Genuine Stripe webhooks can't send our header.** Handled by `provider:stripe` (foreign-signature verify
  + `topicFrom` addressing), not `zeroship-hmac`. Apps wanting run-addressed Stripe delivery still often
  prefer a thin in-app RPC (already Stripe-verified by `@zeroship/payments`) that calls `run.signal` /
  `env.workflows.publish`. The public ingress shines for callers you hand a `wst_` token to, or who adopt
  our HMAC.
- **HMAC-without-idempotency-key double-insert.** A `zeroship-hmac` caller with no `Idempotency-Key` whose
  retry carries a fresh `t` derives a different key and *can* double-deliver. Mitigation is documentation
  (always send an idempotency key). We do not silently dedup by payload hash.
- **Broadcast is live-plus-bounded-retention, not a durable queue.** A subscriber joining later than
  `maxSignalAge` after a publish misses it — deliberate (buffering every broadcast for every possible future
  subscriber is unbounded). Point-to-point remains the guaranteed-eventual single-run tool.
- **Fan-out latency to large subscriber sets is batch-bounded** by the sweep cadence + `max_fanout`. Tail
  latency for very wide topics is **unknown / to-measure**; it is *not* claimed instantaneous.
- **Ordering is per-await only.** The engine guarantees a deterministic bind at each await; it imposes no
  global cross-run or cross-topic order. Two publishes to one topic order by `(created_at, id)`; ties at
  identical timestamps break by `id` (UUIDv7, monotonic-ish) — deterministic but not wall-clock-meaningful.
- **`signal_epoch` revocation is coarse** (whole-run, all outstanding tokens). No per-token revocation list
  day-1 (avoids a hot revocation table); add one only if a real need appears.

### 18.8 Control-plane surface (creator-credentialed, not end-user)

```
POST   /control/apps/:app/workflow-ingress-keys           → mint wsk_ (verifier, shown-once secret)
POST   /control/apps/:app/workflow-ingress-keys/:id/rotate
DELETE /control/apps/:app/workflow-ingress-keys/:id
```

These are **creator-credentialed** (the deploy credential), never exposed to end users. `run.createSignalToken(...)`
is a `@zeroship/workflows` wrapper over the control-plane API — exactly like `start`/`signal`/`cancel` (§3.1):
the worker/isolate **forwards** the mint request to the control-plane ingress terminus, which signs the
`wst_` token with the app's active `bearer-signing` `wsk_` material (§7.6) and returns the finished token.
The raw `wsk_` secret **never leaves the control plane** — the worker neither holds it nor signs with it
(mirroring §18.1: ingress secrets are decrypted only in the control-plane ingress terminus, so a worker
compromise cannot forge `wst_` tokens for the app's runs). End-user app code never sees the signing secret;
it only ever receives the short-lived `wst_` tokens it deliberately mints. **No back-compat shims** (pre-launch): `waitForSignal` gains `opts.topic`,
`workflow_signals` gains columns, `workflow_runs` gains `signal_epoch`, the three new tables + the ingress
route land outright; every producer/consumer/fixture/reference doc changes in the same PR train (§15 PR 12),
and the point-to-point path + single-frontier replay core are untouched in behavior.

### 18.9 Integration checklist

| Touches | Where |
| --- | --- |
| Developer API — `externalSignals`/`inbound`/`topicFrom`, `waitForSignal({ topic })`, `run.createSignalToken`, `env.workflows.publish`, `SignalEnvelope.origin`/`.delivery`, HTTP contract | §3.6 |
| Architecture — dumb gateway forwards the ingress route + rate-limits; control-plane ingress terminus verifies, meters, writes rows; sweep fan-out/GC | §4 |
| Journal / DDL — `workflow_signals` provenance columns + idempotency/fan-out indexes, `workflow_runs.signal_epoch`, `workflow_signal_keys`, `workflow_broadcasts`, `workflow_subscriptions`, grants | §7.1, §7.3, §7.6, §7.7, §7.8 |
| Suspension & signals — external/broadcast producers + topic subscription/late-bind, `wake_at` unchanged | §8 |
| State machine — signal provenance orthogonal to state; terminal/paused runs reject ingress | §9 |
| Metering — `wf_signals_ingress`/`wf_broadcasts`/`wf_fanout_deliveries`/`ingress_bytes` (accept-arm, edge) | §10 |
| Limits — ingress abuse vectors, rate limit before write+meter, fan-out/topic caps, GC | §13, §18.6 |
| Invariants — all core invariants preserved; feature delta (`wsk_`/`wbc_`/`wsb_`/`wst_` ids, control-plane ingress terminus) | §14 |
| Build plan — PR 12 (threads PR 1 / PR 5 / PR 8) | §15 |
| Residual limits & open questions | §16, §18.7 |

Everything else — the replay core (§5/§6), `wake_at`-unified suspension (§8), `claimed_by` leasing, the
claim sweep, deploy-pinning, zero-tokio, and the small native surface — is untouched.

---

## 20. Child / sub-workflow orchestration (engine mechanics)

*Day-1 scope. The mechanics behind §3.7. A child is an ordinary `workflow_runs` row; `step.call` is a
`wait_signal`-flavored step. This feature adds **no** new frontier state, **no** new suspension reason (§8
untouched), **no** new creator-facing `env.*` primitive, and **no** new typed_id prefix (children are
`run_…`, join signals are `sig_…`). It only (a) adds a parent→child edge to `workflow_runs` (§7.1), (b)
co-commits a child spawn inside the parent's existing §7.4 txn, and (c) has the child's terminal §7.4 txn
emit one reserved internal signal to the parent. Everything else — the replay core (§5/§6), `wake_at`
suspension (§8), `claimed_by` leasing (§4), the claim sweep, deploy-pinning — is reused verbatim.*

### 20.1 Model: a child step is a `wait_signal` whose signal is the child's terminal hook

`step.call` is modelled as a single `workflow_steps` row, `kind='child'`, with the exact
`running → completed|failed` lifecycle of `kind='wait_signal'` (§7.2). At spawn the row is written
**`state='running'` carrying `child_run_id`** — that pinned id **is** the durable "started" fact (replay
reads it back, never re-mints: §20.2). At join the row transitions to `completed` (`output` = child Output)
or `failed` (`error` = child error). The "signal" it awaits is a single engine-reserved internal
`workflow_signals` row (§20.3). Because it is a legal `wait_signal` member, it composes in `step.all`
mixed-suspension frontiers (§6.2) and rides `wake_at`-unified suspension (§8) with **zero** new machinery.

### 20.2 Spawn + park — inside the parent's §7.4 commit txn

On the dispatch whose frontier includes an unmemoized `step.call` invocation at ordinal `K`:

1. **EXECUTE (worker).** Compute the deterministic dedup key `child_key = "child:" + parentRunId + ":" + K`
   (or `opts.key`). Mint a fresh `childRunId` (`run_…`, UUIDv7 — **not** derived, so the typed_id invariant
   holds; determinism comes from `child_key`, not from the id). Build the child run row: `state='queued'`,
   `input = <spawn input>`, `deploy_id = parent.deploy_id` (§20.7), `parent_run_id = parentRunId`,
   `parent_wait_step_key = '__zs.child:' + K`, `parent_cascade = opts.cascade`,
   `tree_depth = parent.tree_depth + 1`, `dedup_key = child_key`. Outcome for ordinal `K` =
   `SUSPEND(kind=child, child_run_id=childRunId)`.
2. **COMMIT (parent §7.4 txn, lease-guarded).** In the *same* atomic txn that folds the whole frontier:
   ```sql
   -- (a) spawn the child idempotently — the deterministic key is the no-double-spawn guard
   INSERT INTO zeroship.workflow_runs
     (id, workflow_name, app_id, deploy_id, state, input, dedup_key,
      parent_run_id, parent_wait_step_key, parent_cascade, tree_depth, started_at, created_at)
   VALUES ($childRunId, $childWf, $app, $parentDeploy, 'queued', $childInput, $child_key,
      $parent, '__zs.child:' || $K, $cascade, $parent_tree_depth + 1, now(), now())
   ON CONFLICT (app_id, workflow_name, dedup_key) DO NOTHING;              -- §7.1 guard

   -- (b) the parent's own child-step row (StepSuspended carrying the childRunId)
   INSERT INTO zeroship.workflow_steps
     (run_id, ordinal, name, name_occurrence, kind, state, child_run_id, signal_type,
      batch_id, batch_width, started_at)
   VALUES ($parent, $K, $name, $occ, 'child', 'running', $childRunId, '__zs.child:' || $K, …)
   ON CONFLICT (run_id, ordinal) DO NOTHING;                       -- §7.2 exactly-once
   -- + parent run → 'waiting', wake_at = MIN(pending) (NULL if no timeout), per §7.4 UPDATE
   ```

The child spawn (a) and the parent's park (b) **co-commit** under the parent's lease guard
(`claimed_by=$me AND claim_epoch=$epoch`), so either the child exists **and** the parent points at it, or
neither. On a lease-handoff double-dispatch, the lease-guard `UPDATE` lets **exactly one** parent txn
commit; the loser rolls back its child `INSERT` too. Even in the vanishing true-concurrent window,
`child_key`'s `ON CONFLICT DO NOTHING` collapses to one child and `(run_id, ordinal) DO NOTHING` collapses
to one parent step naming it. The child then walks §9 from `queued` exactly like a `start()`ed run —
dispatched by the ordinary claim sweep (§4). **No child scheduler exists.**

### 20.3 The terminal-child hook — one reserved internal signal, co-committed with the child's terminal txn

When the child's own §7.4 FOLD lands a terminal state (`completed`/`failed`/`cancelled`) and the child row
has `parent_run_id IS NOT NULL`, the **same terminal txn** additionally:

```sql
-- reserved internal join signal → the parent's child-step await. Keyed on the parent step ordinal
-- (parent_wait_step_key), so it binds exactly one await. idempotency_key = parent_wait_step_key gives
-- exactly-once ingest via the §7.3 (run_id,type,idempotency_key) uidx.
INSERT INTO zeroship.workflow_signals
  (id, run_id, type, payload, origin, delivery, idempotency_key, created_at)
VALUES (gen(), $parent_run_id, $parent_wait_step_key,
        jsonb_build_object('ok', $ok, 'output', $inline_out_or_null,
                           'output_ref', $blob_ref_or_null, 'error', $err_or_null),
        'system', 'direct', $parent_wait_step_key, now())
ON CONFLICT (run_id, type, idempotency_key) WHERE idempotency_key IS NOT NULL AND delivery <> 'topic'
DO NOTHING;

UPDATE zeroship.workflow_runs SET wake_at = now()
 WHERE id = $parent_run_id AND state IN ('running','sleeping','waiting');   -- arm parent; no-op if terminal
```

- **Finds the parent** purely via the child row's `parent_run_id` + `parent_wait_step_key` edge — no scan,
  no separate registry.
- **Atomicity (CW3).** The join signal + parent wake-arm co-commit with the child's terminal transition →
  child-terminal ⇔ parent-notified. If any lost-wake path leaves the signal written but the `wake_at`
  `UPDATE` not applied (impossible within one txn, but true across a crash), §18.4's existing safety-net
  scan ("runs with an unconsumed signal newer than last dispatch re-arm `wake_at`") re-arms the parent —
  reused verbatim.
- **Blob-backed child output.** The payload carries only `output_ref` (hash/size/contentType, §3.5); on bind
  the parent writes its child-step row **blob-backed** (`output_kind='blob'`, same hash, refcount++ via
  §7.5) — a multi-MiB child output never inlines into the join signal or the parent step.
- **Reserved-namespace guard.** `run.signal` (§3.1) and external ingress (§18) **reject** any user-supplied
  `type` beginning with `__zs.` (`403`, §7.3), so app code cannot forge a child-completion. This one
  validation rule is the wall between "app signals" and engine-internal joins.

On the next parent dispatch, replay reaches the `kind='child'` step at ordinal `K`, computes its reserved
type `'__zs.child:' + K`, binds the internal signal, and resolves the `step.call` promise to the child's
Output (or rethrows the child's error class). The bound value is journaled in the parent step row
(`consumed_signal_id` + `output`/`error`), so every subsequent parent replay yields the identical result
(§11 C1).

### 20.4 Fan-out / join is the concurrent frontier — nothing new

`Promise.all`/`step.all` over `step.call` yields a §6.1 antichain of `kind='child'` steps. §6.4 spawns
up to `effN` children (each co-committed per §20.2 in the single N-row parent txn), parks the parent on all
of them, and — via §8 unified `wake_at = MIN(pending)` with re-eval-all-on-wake — re-dispatches the parent
whenever **any** child's terminal hook arms it, binds the now-present children, and re-parks on the rest,
across as many dispatches as needed. The `step.all` result array is issue-order (§6.2). Join = §6 + §8; the
engine grows **zero** join-specific code.

### 20.5 `startMany` — N idempotent starts, one round-trip

`startMany` is a control-plane batch over the §7.1 guard — **no new DDL, no new state**:

```sql
-- one txn; each keyed item idempotent; a duplicate key no-ops its row (does NOT fail the batch)
INSERT INTO zeroship.workflow_runs
  (id, workflow_name, app_id, deploy_id, state, input, dedup_key, tree_depth, started_at, created_at)
SELECT gen(), $wf, $app, $currentDeploy, 'queued', i.input, i.key, 0, now(), now()
  FROM unnest($ids, $inputs, $keys) AS i(id, input, key)
ON CONFLICT (app_id, workflow_name, dedup_key) DO NOTHING;
-- then SELECT the full set by (app_id, workflow_name, dedup_key) and map back to input order.
```

Each created run enters **`queued`** (§9) exactly like a single `start()`; the claim sweep dispatches them.
`deploy_id` = the app's **current** deploy (these are top-level runs, not children). Unkeyed items cannot
dedup — each `startMany` call mints a fresh run for them (documented at-least-once, §20.9). Batch size is
capped (`SCHEDULE`-style operator knob `maxStartManyBatch`, §13).

### 20.6 Cascade — INDEPENDENT by default, opt-in `{ cascade: true }`

- **Default (independent).** `run.cancel()` on the parent traverses **no** child edges. Live children keep
  running to their natural terminal; their terminal hook arms `wake_at` on the now-`cancelled` parent, whose
  `WHERE state IN ('running','sleeping','waiting')` guard makes it a **no-op** (the parent never re-dispatches).
  Harmless orphan; GC'd with the parent.
- **`cascade: true`.** The child row's `parent_cascade` is set at spawn. `run.cancel()` on the parent
  additionally:
  ```sql
  UPDATE zeroship.workflow_runs
     SET cancel_requested = true, wake_at = now()
   WHERE parent_run_id = $parent AND parent_cascade
     AND state NOT IN ('completed','failed','cancelled');
  ```
  Each live cascade-child, at its next **claim** (§5 step 1), observes `cancel_requested` and transitions
  **itself** to `cancelled` under **its own lease** — cooperative, never cross-run lease theft. Its own
  terminal transition then recurses to *its* cascade-children (the same `UPDATE`), so the cancel walks the
  sub-tree breadth-first, bounded by `tree_depth` / live-descendant caps (§20.9) and terminating because
  `cancelled` is absorbing. A child racing to complete just before its claim simply completes — no torn
  state (CW6). `cancel_requested` is a minimal cooperative-cancel flag on `workflow_runs`; it also cleanly
  cancels a currently-dispatching run.

### 20.7 Deploy-pinning — child pins to the **parent's** deploy

A child spawned by `step.call` inherits `deploy_id = parent.deploy_id`. A parent replaying on deploy A
references the `WorkflowClass` **as it existed in A**; the child must run *that* definition, so the whole
sub-tree is version-coherent even if the app redeploys mid-parent (§4: in-flight runs finish on pinned code;
the child is part of the parent's in-flight computation). `startMany` runs are top-level → the app's current
deploy, like any `start()`.

### 20.8 Correctness (relative to §11)

CW1–CW7 are stated and proved in **§11** (Child / sub-workflow orchestration): spawn determinism / no
double-spawn (CW1), join binding determinism (CW2), terminal-hook atomicity (CW3), fan-out/join =
concurrent frontier (CW4), at-least-once children / effectively-once join (CW5), cascade safety &
termination (CW6), `startMany` idempotency (CW7). **Single-frontier / lease / `wake_at` respected:** spawn
is one frontier step (§6.1 antichain member); park reuses `wait_signal` suspension (§8 `wake_at`); the
terminal hook is a journal-row producer + `wake_at` arm (exactly like §18 ingress); the child walks §9
unmodified. The replay core (§5/§6) and unified suspension (§8) are **untouched**.

### 20.9 Limits & abuse (feeds §13)

- **Reserved `__zs.` type namespace** — `run.signal` / external ingress reject user types with the `__zs.`
  prefix (`403`): no forged child completions (§7.3, §20.3).
- **Fork-bomb bound** — `tree_depth` (stored on the run row, `parent.tree_depth+1`) is checked at spawn
  against `maxChildDepth`; a per-tree live-descendant cap (`maxLiveDescendants`, derived by walking /
  aggregating the `parent_run_id` edge or a maintained counter) bounds width. Over cap → the `step.call`
  frontier step folds to `PERMANENT_FAIL` (`ChildLimitError`).
- **`startMany` batch cap** — `maxStartManyBatch` bounds one `INSERT`; over cap → `LimitExceededError` at the
  call site.
- **Effect amplification** — a crashed parent spawn dispatch does **not** double-spawn (CW1); a crashed
  child amplifies its own effects at-least-once like any run (§11 C3), unchanged.
- **Orphan children (independent cancel)** — keep running to terminal, then no-op their join on the
  cancelled parent. Intended; costs are the child's own metered dispatches (§10).
- Concrete values (`maxChildDepth`, `maxLiveDescendants`, `maxStartManyBatch`) are **to-measure /
  operator-config**, seeded from the plan catalog — no numbers claimed (§13, §16).

### 20.10 Integration checklist

| Touches | Where |
| --- | --- |
| Developer API — `step.call`, `ChildWorkflowOptions`, `startMany`, `StartManyItem`, cascade, join rethrow, `ChildTimeoutError`/`ChildCancelledError` | §3.7, §3.2, §3.1 |
| Journal / DDL — `workflow_runs` parent edge + `tree_depth` + `cancel_requested`; `workflow_steps.child_run_id` + `kind='child'`; reserved internal join type + idempotency reuse | §7.1, §7.2, §7.3 |
| Commit txn — spawn co-commit + terminal-child hook | §7.4 |
| Suspension — child await is a `wait_signal` flavor; `wake_at` unchanged | §8 |
| State machine — `kind='child'` fold rows; cascade; parentage orthogonal to state | §9 |
| Metering — child runs meter as ordinary runs; no new metric | §10 |
| Correctness — CW1–CW7 | §11 |
| Limits — reserved namespace, tree caps, batch cap | §13, §20.9 |
| Invariants — all core invariants preserved; feature delta (no new typed_id prefix; child pins to parent deploy) | §14 |
| Build plan — PR 13 (threads PR 1 / PR 3 / PR 4 / PR 5) | §15 |
| Residual limits & open questions | §16 |

Everything else — the replay core (§5/§6), `wake_at`-unified suspension (§8), `claimed_by` leasing, the
claim sweep, deploy-pinning, zero-tokio, and the small native surface — is untouched.

---

## 21. Compensation / saga rollback (engine mechanics)

*Day-1 scope. The engine mechanics behind the developer surface in §3.8. Compensation is **not** a new
engine: it is the §5 dispatch loop with the frontier-selection predicate flipped from "next forward pending
unmemoized step" to "next reverse-ordinal completed step whose compensation is not yet journaled." A
`compensating` phase sits between `running`/`sleeping`/`waiting` and the terminal `failed`/`cancelled`. It adds **no
new table, no new typed_id, and no new suspension reason** — only annotation columns on `workflow_steps`
(§7.2), two columns + one state on `workflow_runs` (§7.1), and a reversed frontier query on the existing
loop.*

### 21.1 Problem & scope — entering rollback is a fold outcome, not a new mechanism

When a run reaches a **terminal failure**, the engine walks the run's own journal **in reverse ordinal
order** and executes the recorded compensator of every **completed, compensable** step — each as its own
durable, journaled, retried, at-least-once unit. Compensability is journaled **at step completion** (§7.4):
a `step.run(name, { compensate }, fn)` that COMPLETEs writes its `workflow_steps` row with
`compensation_state = 'pending'` (a step without a compensator writes `NULL`). So *the durable set of things
to undo already exists in the journal* before any failure.

**Terminal run-failure is defined as the replay pass throwing out of `run()`** (§9 fold refinement) — which
requires the one semantic refinement compensation depends on: to distinguish a **caught** step failure from
an **uncaught** one, the engine commits the failed step row, schedules an immediate re-dispatch, and on that
re-dispatch lets the memoized-failed step's promise reject at its point in program order; the body either
catches it (the run continues) or the throw escapes `run()`. This unifies uncaught `PermanentError`,
retries-exhausted `StepFailed`, and any other uncaught throw. `NondeterministicError`/`StalledError` are **not**
subject to it — they fail closed *without* replay-to-observe (§9, CC6). When the replay throws out of
`run()`, the fold is:

```
uncaught throw escaping run()  →
  IF EXISTS(step WHERE run_id=$run AND compensation_state='pending'):
        run.state := 'compensating'
        run.compensation_target := 'failed'          -- terminal to reach after rollback
        run.error := <triggering error>
        run.wake_at := now                            -- immediate re-dispatch into the rollback phase
  ELSE:
        run.state := 'failed', run.error := <triggering error>   -- nothing to undo (unchanged §9)
```

`run.cancel({ mode: "compensate" })` (§3.8.2) produces the identical transition with
`compensation_target := 'cancelled'` (or `cancelled` directly when no `pending` rows exist). Plain
`run.cancel()` is unchanged: straight to `cancelled`, no rollback.

### 21.2 A compensation dispatch — the §5 loop, run in reverse

There are two phases now: `forward` (§5/§6, unchanged) and `compensating`. A `compensating` dispatch reuses
the §5 skeleton verbatim; only the **frontier predicate** changes:

```
1. CLAIM       advisory-lock(run_id); CAS claimed_by/claim_epoch/lease_expires (§4). Same lease.
               Load workflow_steps ORDER BY ordinal (the journal prefix). Same deploy-pin (§4).
2. REPLAY      run userWorkflow(trigger, stepShim). The pass deterministically THROWS out of run() at the
               original failure point (§11 C1 + deploy-pinning: nothing changed). The engine EXPECTS the
               throw in this phase and catches it. Its only purpose is to repopulate the per-dispatch
               compensatorRegistry: Map<ordinal, { name, compensate, output }> for every COMPLETED
               compensable step memoized up to the throw. Sleeps/signals/incomplete/non-compensable steps
               register nothing. (output is rematerialized incl. blob refs, §17.4.)
3. FRONTIER    Compensation frontier := SELECT ordinal FROM workflow_steps
                  WHERE run_id=$run AND compensation_state IN ('pending','running')
                        AND (compensation_wake_at IS NULL OR compensation_wake_at <= now())
                  ORDER BY ordinal DESC LIMIT effN.        -- baseline effN = 1 (serial LIFO)
               For each selected ordinal, take its closure+output from compensatorRegistry. A journaled
               compensable ordinal MISSING from the registry ⇒ NondeterministicError, fail-closed.
4. EXECUTE     Invoke compensate(output, ctx) with per-compensator StepTimeoutError + retry policy, raced vs the
               dispatch wall deadline (claim_ts + wall_budget). Compensators overlap only if effN>1 (same
               compio cooperative I/O overlap as §6; baseline is serial).
5. BARRIER     await raceWithDeadline(settleAll, dispatch_deadline). Per compensator:
                  COMPLETE | PERMANENT_FAIL(err) | RETRY_SCHEDULED(attempt+1, wake=+backoff)
                  | UNSETTLED(deadline → no row change, re-selected next dispatch)
6. FOLD        COMPLETE        → compensation_state='completed',  compensation_finished_at=now()
               PERMANENT_FAIL  → compensation_state='failed', compensation_error=err   (CONTINUE to earlier)
               RETRY_SCHEDULED → compensation_state='running', compensation_attempt++, compensation_wake_at=+backoff
               UNSETTLED       → no change (re-discovered next dispatch)
               Run transition:
                  still EXISTS pending|running compensations → state stays 'compensating';
                     wake_at := MIN(compensation_wake_at over 'running')  (or now if a 'pending' is ready)
                  else → state := compensation_target ('failed' | 'cancelled');
                     compensation_outcome := 'partial' if any compensation_state='failed' else 'completed'
7. COMMIT      One atomic, lease-guarded, idempotent txn (§7.4 variant). INTERRUPT; exit isolate.
8. SCHEDULE    wake_at==now → immediate re-dispatch; future → wake_at-unified compio timer (§8); terminal → notify (§5.9).
```

The frontier **strictly shrinks** every dispatch: each committed dispatch either marks ≥1 ordinal
`completed`/`failed`, or arms a bounded-retry backoff on a `running` one. When a compensator exhausts
`compensation_max_attempts` it becomes terminal-`failed` and the walk continues to the next-earlier
ordinal — a stuck undo can never wedge the run (bounded further by `stuck_strikes` → `StalledError`, §11 C7 /
§13). The one durable set to walk (`compensation_state='pending'`), the reverse-ordinal index (§7.2), and
the §7.4 commit variant are all that is new; claim, replay, barrier, interrupt, deploy-pin, lease, and wake
are the forward loop's.

### 21.3 `effN` for the compensation phase

Baseline is **serial LIFO** (`effN = 1`) — undo ordering usually matters, so the safe default runs one
compensator per dispatch in strict reverse ordinal order. The §6 concurrent-frontier machinery is reused
unchanged when a workflow opts into `static compensationConcurrency = k` (clamped by the platform ceiling,
§13), in which case up to `k` *independent* (antichain — §6.1) compensators of the same reverse batch
overlap their I/O, results still folded atomically. This is a pure generalization;
`compensationConcurrency = 1` is byte-identical to the serial walk (mirrors §6.6). Absent the opt-in,
compensation runs serial.

### 21.4 Correctness (relative to §11)

The compensation correctness argument is **CC1–CC8 in §11** (compensator determinism; exactly-once
compensation *result*; at-least-once compensation *effect*; reverse-order integrity; single-frontier /
lease / wake_at respected; scope correctness; termination / liveness; strict generalization). The
load-bearing points: the `compensatorRegistry` is a pure function of `(code, journal prefix)` (deploy-pinned
closures), the `compensation_state='completed'` marker is exactly-once under the lease CAS + state guard (§7.4
variant), the effect is at-least-once (idempotency the author's obligation via `ctx.idempotencyKey`), and a
workflow with no `compensate` is byte-identical to the pre-compensation engine (the `EXISTS(pending)` entry
check is always false).

### 21.5 Integration checklist

| Touches | Where |
| --- | --- |
| Developer API — `config.compensate`, `Compensator<O>`, `CompensationContext`, `StepConfig<T>`, `run.cancel({ mode: "compensate" })`, `static compensationConcurrency`, three semantics + idempotency guidance | §3.8, §3.2, §3.1 |
| Fold refinement — terminal failure = uncaught throw escaping `run()`; re-dispatch-to-observe-catch | §9, §21.1 |
| Rollback = §5 loop with reversed frontier predicate | §5, §21.2 |
| Journal / DDL — `workflow_steps` compensation columns + 2 indexes; `workflow_runs` `compensation_target`/`compensation_outcome` + `compensating` in wake index; forward-INSERT `compensation_state`; compensation commit variant | §7.1, §7.2, §7.4 |
| Suspension — compensator backoff joins `wake_at = MIN(pending)` | §8 |
| State machine — `compensating` phase, transitions, cancel-with-compensate, partial outcome, fail-closed rules | §9 |
| Metering — a compensation dispatch is one metered unit | §10 |
| Correctness — CC1–CC8 | §11 |
| Limits — bounded fan-out, wedged compensator, amplification, no new id/table | §13 |
| Invariants honored | §14 |
| Build plan — PR 14 (threads PR 1 / PR 3 / PR 4 / PR 6) + faithful e2e | §15 |
| Residual limits + cancel justification | §16 |
| Change log + settled surface | §23.1, §23.2 |

**Net footprint:** 7 new `workflow_steps` columns + 2 indexes; 2 new `workflow_runs` columns + 1 state
value + 1 index-predicate widening; one forward-INSERT column; one commit-txn variant; `StepConfig<T>`,
`Compensator<O>`, `CompensationContext`, `run.cancel({ mode: "compensate" })`, optional `static
compensationConcurrency`. **Zero** new tables, typed_ids, native primitives, suspension reasons, or
schedulers — compensation is the existing single-frontier dispatch loop walking the run's own journal in
reverse. Everything else — the replay core (§5/§6), `wake_at`-unified suspension (§8), `claimed_by` leasing,
the claim sweep, deploy-pinning, zero-tokio, and the small native surface — is untouched.

---

## 22. Replay-from-step — `run.restart` (engine mechanics & control-plane surface)

*Day-1 scope. The mechanics behind the §3.1 developer surface. `run.restart({ from?, deploy? })` keeps the
journaled steps *before* a target step, drops the target + everything after, resets the run row, and
re-queues the run so the ordinary §5 loop replays the retained prefix and executes forward. It **touches the
replay core (§5/§6) not at all** — it only edits a run's journal prefix + run row, in one control-plane txn
(§7.10). It adds **no** new table, **no** new typed_id, **no** new suspension reason, **no** new locking
primitive, and **no** new state — only four audit columns on `workflow_runs` (§7.1) and one txn (§7.10). It
reuses the §4 claim discipline, the §7.4 lease-guard, §8 unified wake, and deploy-pinning verbatim.*

### 22.1 Semantics — exact journal/state-machine

Let the target resolve to **ordinal `t`** via `UNIQUE (run_id, name, name_occurrence)` (§7.2): `t = ordinal`
of `(from.name, from.occurrence ?? 0)`; `from` omitted ⇒ `t = 0` (full restart). No matching row →
`RestartError`. Then, in the single txn (§7.10): **retain** every step row `ordinal < t`, **drop** every row
`ordinal ≥ t` (target + successors), **reset + re-queue** the run row (`state='queued'`, `wake_at := now()`,
`next_ordinal := t`, terminal `output`/`error` cleared, `claim_epoch += 1`), and let §5 replay `0..t-1`
(memoized) and discover ordinal `t` as a fresh frontier candidate. Ordinal — not batch — granularity: a `t`
inside a concurrent batch retains its `ordinal < t` siblings and re-runs its `ordinal ≥ t` siblings; the
retained prefix stays antichain-consistent (§6.1 C1, §7.10). The run **input is retained** — restart re-runs
the same input; changing input is a new `start()`.

**Partial restart is rejected past a completed compensation.** A partial restart (`from` set, `t > 0`) whose
**retained** prefix (`ordinal < t`) contains a step whose compensation already **settled**
(`compensation_finished_at` set → `'completed'`/`'failed'`, §7.4) → **`RestartError`** ("cannot partial-restart
past a completed compensation; use a full restart"). The forward replay memoizes the retained prefix as
completed and never re-runs those bodies, so resuming forward would silently assume a reservation/charge the
compensator already released or refunded — there is no safe partial revive across a finished rollback. A
**full** restart (`t = 0`, drops the whole journal) is the sanctioned revive for a rolled-back run; a partial
restart across an *un-settled*, mid-flight undo is permitted (the reservation still holds — §7.10 step 4b
resets the in-flight bookkeeping to the clean `'pending'` baseline). The guard precedes every write in the
restart txn (§7.10 step 1b), so a rejected restart mutates nothing.

### 22.2 Restart is a control-plane transition (state machine, §9)

`restart` is a sibling of `pause()`/`cancel()`, orthogonal to the frontier fold, and the **one** edge that
legitimately leaves a **terminal** state (`completed`/`failed`/`cancelled`) — a deliberate, authorized,
audited revive. From any **non-`running`** state → `queued (wake_at := now())`, journal truncated to
`ordinal < t`; on a **`running`** run, `claim_epoch += 1` first evicts the in-flight dispatch (its §7.4
commit fails the epoch guard and rolls back — reusing lease-handoff race handling), then the same reset.
Serialized + atomic (advisory lock + epoch guard, §7.10); concurrent restarts cannot tear state (the loser
sees the winner's reset), though restart is deliberately **not** idempotent (each is a distinct intent). Full
transition rows are in §9; correctness is **§11 C9** (replay-prefix soundness).

### 22.3 Deploy-pinning (decided, §4)

**Partial** restart (a prefix `0..t-1` is retained) is pinned **immutably to the original deploy** — the
retained ordinals/names were produced by that exact code, so replaying them against it is call-compatible
(no `NondeterministicError`); `deploy:"latest"` **with `from` set is rejected at the API boundary**
(`RestartError`). **Full** restart (nothing retained) defaults to `deploy:"latest"` (re-pin to the app's
current active deploy — the semantics of a fresh `start()`), overridable to `deploy:"started"` for exact
reproduction. Any actual re-pin bumps `signal_epoch` (§7.1), invalidating outstanding `wst_` ingress tokens
(§18.1) since the new deploy's `externalSignals`/`inbound`/`topicFrom` allowlist may differ.

### 22.4 Authz — owner app **or** operator (both audited)

Two callers, one txn (§7.10), differing only in credential + the `restarted_by` stamp:

- **Programmatic (owner app).** `run.restart(...)` from app/handler code via `env.workflows`, authorized by
  the app's **deploy/control credential** — identical tier to `run.cancel/pause/resume` (§3.1) and the §18.8
  control-plane surface. The run must belong to the caller (`workflow_runs.app_id == caller.app_id`);
  cross-app restart is `403`. Subject to the per-run restart cap (§13). `restarted_by = <app deploy
  credential id>`.
- **Operator.** Platform operator via the control plane / dashboard, authorized by **operator credential**
  (the same operator authority behind plan-catalog edits / billing ops). Cross-app (any run) for incident
  recovery; **exempt from the restart cap** but **always audit-logged** (`restarted_by = op_…`,
  `restarted_at`).

Control-plane surface (mirrors §18.8's creator-vs-operator split):

```
POST /control/apps/:app/workflows/runs/:runId/restart     # creator-credentialed; owner-app only
POST /control/ops/workflows/runs/:runId/restart           # operator-credentialed; any app; audited
Body: { "from"?: { "name": string, "occurrence"?: number }, "deploy"?: "started" | "latest" }
200  { "runId": "run_…", "state": "queued", "restartedFromOrdinal": <t|null>, "pinnedTo": "<deploy_id>" }
403  not the owner app (creator route) / insufficient scope
404  run unknown, or `from` target not in the run's journal
409  illegal deploy pin ("latest" with `from` set), or partial restart past a completed compensation — RestartError
429  per-run restart cap exceeded (creator route only)
```

**Grants.** `zeroship_control` gets the DELETE/UPDATE-on-`workflow_steps` + DELETE-on-`workflow_subscriptions`
+ UPDATE-on-`workflow_runs`/`workflow_blobs`/`workflow_signals` needed for the restart txn (§7.10), guarded by
the same `pg_roles`-existence `DO` block as the other `zeroship.workflow_*` scripts (§7.9 convention). **No
new role.**

### 22.5 Limits, correctness & residual

Restart cap (`WORKFLOW_MAX_RESTARTS_PER_RUN`, creator path; operator exempt), metering (re-executed steps
dispatch through the ordinary gateway-edge metered path — no per-restart bypass), and blob-GC / terminal-revive
interactions are §13. Correctness is **§11 C9**: partial restarts retain only a prefix the pinned deploy
produced (C1), the whole rewind is one atomic txn (C4), dropped steps re-execute at-least-once on purpose
(C3, §6.3), and the replay core / lease / `wake_at` are unmodified. The at-least-once re-execution and the
deploy-pin decision are stated in §16 (a resolved design decision, not open). **Restart × compensation:** a
partial restart is **rejected** (`RestartError`, §22.1 / §7.10 step 1b) when its retained prefix contains a
step whose compensation already settled — a full restart is the sanctioned revive for a rolled-back run.

### 22.6 Integration checklist

| Touches | Where |
| --- | --- |
| Developer API — `run.restart(opts?)`, `RestartOptions`/`RestartTarget`/`RestartError`, example | §3.1 |
| Journal / DDL — 4 audit columns on `workflow_runs` (`restart_count`/`restarted_at`/`restarted_from_ordinal`/`restarted_by`); **no** new index/table | §7.1 |
| The restart txn — advisory-lock + `claim_epoch`-evict + drop `≥ t` + blob/signal/subscription prune + reset + grants | §7.10 |
| Deploy-pinning decision (partial=original-immutable, full=current-default) + `signal_epoch` bump on re-pin | §4, §7.10, §22.3 |
| State machine — restart from any non-running / running-evict → `queued (wake_at := now())`; only edge out of terminal | §9 |
| Correctness — C9 (replay-prefix soundness) + at-least-once/atomic notes | §11 |
| Limits — restart cap, metering, blob-GC / terminal-revive | §13 |
| Authz (owner app + operator) + control-plane endpoints | §22.4 |
| Residual — dropped steps re-run at-least-once; deploy-pin rationale (resolved decision) | §16 |
| Build plan — folded into PR 6 (run controls); the 4 audit columns land in the PR 1 create scripts | §15 |
| Change log + settled surface | §23.1, §23.2 |

**Net footprint:** 4 new `workflow_runs` columns; one control-plane txn; `run.restart` + `RestartOptions`/
`RestartTarget`/`RestartError` in `@zeroship/workflows`; two control-plane routes. **Zero** new tables,
typed_ids, indexes, native primitives, suspension reasons, states, or schedulers — restart is a journal-prefix
editor that re-queues via the existing §8 wake and lets the untouched §5 loop run.

---

## 23. Round-8 revision change log

Round 8 makes the engine **full-featured for day 1**. There is **no v1/v2 split** and nothing on the
happy path is "deferred to a later phase": every feature below ships in the first PR-train, and the
single-frontier baseline is just the `concurrency = 1` special case of the same code path. Earlier
"deferred to v2 / v1-only" qualifiers on now-shipped features were removed throughout, and the only
items still marked out-of-scope are genuinely out-of-scope (they are *not* deferred slices of a shipped
feature).

### 23.1 Features folded in (all day-1)

- **Concurrent frontier execution (§5, §6).** A single dispatch executes up to *N* structurally
  independent frontier steps concurrently (the frontier is a data-dependency antichain, proved in §6.1),
  bounded by `static concurrency` + a per-batch `step.all` cap + the platform ceiling. Cooperative I/O
  overlap inside one V8 isolate on the compio loop — not threads, not tokio, not multi-core. The
  single-frontier engine is `effN = 1`, byte-identical to this path (§6.6). Added `step.all` with
  order-preserving (issue-order) results and mixed-suspension frontiers (§6.2).
- **Large & streaming step outputs — blob-backed output rail (§3.5, §17).** A step result, the run
  input, or the run's final output that exceeds the 1 MiB inline journal cap auto-spills to
  content-addressed object storage and is journaled **by-reference**, with an opt-in `StepOutputRef`
  streaming handle (`config.output`) that never buffers the whole payload in the isolate on either the
  write or read side. Adds **no** new frontier state, **no** new suspension reason, and **no** new
  creator-facing `env.*` primitive — only the *representation* of a recorded output changes.
- **External signal ingress & broadcast (§3.6, §18).** A signed, rate-limited public gateway-edge
  endpoint (`POST /__zeroship/signals/v1/{run|topic}/{addr}`) lets systems *outside* the app deliver a
  signal to a run **without** the control credential (three inbound verifiers — `zeroship-hmac`,
  `bearer` per-run `wst_` token, `provider:stripe` foreign signature — none of which is the control
  credential), gated by a deploy-pinned `externalSignals` allowlist. Topic **broadcast** fans one publish
  out to many runs matched by key (`waitForSignal({ topic })`, `env.workflows.publish`). Both halves are
  **producers of journal rows only**; the replay core (§5/§6) and `wake_at`-unified suspension (§8) are
  untouched.
- **Scheduled workflows — schedule DSL + raw cron (§3.4, §12).** `schedule({ name, schedule, workflow })`
  registers a recurring trigger that starts a fresh run per fire. A typo-safe fluent DSL (`every.monday.at(...)`,
  `every(15,"minutes")`) *and* raw 5-field POSIX cron both compile **build-time in the SDK** to one of
  exactly two primitive stored shapes (internal `cron`/`interval` kinds, not an exposed TS union) the control-plane schedule
  sweep fires. Scheduled runs are ordinary runs that add **zero** replay surface; the sweep runs under
  the same `claimed_by` lease + advisory-lock discipline as the dispatch/fan-out sweeps.
- **Child / sub-workflow orchestration & batch start (§3.7, §20).** `step.call(WorkflowClass, input,
  opts?)` spawns (idempotently, deterministic key) and awaits a child run's typed `Output` from inside a
  run; `Promise.all`/`step.all` over it is fan-out/join via the §6 concurrent frontier. Children are
  **independent by default**, `{ cascade: true }` opts into cancel-cascade. `env.workflows.X.startMany([{
  input, key? }])` fans a batch of idempotent runs out from outside a run. A child is an ordinary run; the
  parent parks in a `wait_signal`-flavored suspension and the child's terminal §7.4 txn emits one reserved
  internal signal that wakes the parent. **Producers of journal rows only** — the replay core (§5/§6) and
  `wake_at`-unified suspension (§8) are untouched; **no new typed_id prefix** (`run_…` / `sig_…`).
- **`step.sleepUntil` — absolute-deadline sleep (§3.2, §8).** Absolute-instant sibling of `step.sleep`:
  `step.sleepUntil(name, when)` (a `Date` or epoch-ms) suspends until `when` instead of `now + duration`.
  It reuses `kind='sleep'` + `wake_at` + the §8 unified wake with **zero DDL** — `wake_at :=
  to_timestamptz(when)` is resolved once and journaled at first discovery (frozen like `sleep`'s duration →
  determinism-safe, C1/C5). One-sided clock guarantee: **never before `when`** (single DB clock),
  best-effort `≥ when`, not on-the-dot; a past `when` is a zero-length sleep. A pure authoring-front-end
  addition to the `step` shim (build PR 5).
- **`run.restart` — replay-from-step (§3.1, §7.10, §22).** Keeps journaled steps before a target step, drops
  the target + everything after, resets the run row, and re-queues via `wake_at := now()` — evicting any live
  dispatch by `claim_epoch += 1` (reusing the §7.4 lease guard). The replay core (§5/§6) is untouched.
  Deploy-pinning is **decided**: a partial restart is pinned-original-and-immutable (a retained prefix is
  only valid against the deploy that produced it → `deploy` re-pin with `from` rejected); a full restart defaults to
  re-pin-current (a fresh `start()`'s semantics), `deploy:"started"` for exact reproduction; any re-pin bumps
  `signal_epoch`. Authz = owner-app deploy credential **or** platform operator, both audited via 4 new
  `workflow_runs` columns (the only DDL delta). New correctness clause **C9**; the one edge that revives a
  terminal run. Build PR 6.
- **Compensation / saga rollback (§3.8, §21).** Attach `config.compensate` to any `step.run`; on a terminal
  run failure the engine walks the journal in reverse and runs each completed step's compensator as its own
  durable, journaled, retried, at-least-once unit. Adds a `compensating` phase between `running`/`sleeping`/`waiting`
  and `failed`/`cancelled`; `cancel({ mode: "compensate" })` is the opt-in cancel path; **no new table, no new
  typed_id, no new suspension reason** — annotation columns on `workflow_steps`, two columns + one state on
  `workflow_runs`, and a reversed frontier query on the existing §5 loop. It is the single-frontier loop with
  the frontier predicate flipped from *forward-pending* to *reverse-completed*; the forward path (§5/§6) is
  byte-identical, and a workflow with no `compensate` fails exactly as before.

### 23.2 Batch-2 API renames (settled naming surface)

The bespoke-elegant naming is now locked. Notable rename: a workflow execution is a **run** (not an
"instance"); `env.workflows.X.start(...)` returns a `WorkflowRun` with `id` `run_…`. The full settled
surface:

- **Start / control:** `env.workflows.X.start({ input, key, onConflict })` → `WorkflowRun`
  (`onConflict` string **or** `{ policy }` object, values `join|reject|replace`, §3.1/C7);
  `env.workflows.X.startMany([{ input, key? }], { onConflict? })` → `StartManyResult[]` (per-item
  `{ run, created?, conflict? }` envelope, C4);
  `env.workflows.get(runId)` / `env.workflows.X.get(runId)` (rehydrate an existing run handle, B1);
  `run.signal({ type, payload })`, `run.cancel(opts?: CancelOptions)` (`{ mode: "abort" | "compensate", reason? }`, default `"abort"`, §3.8/C1),
  `run.pause()`, `run.resume()`, `run.status()` → `{ state, output, error }`;
  `run.restart({ from?, deploy? })` → `WorkflowRun` (replay-from-step; `from` omitted = full restart;
  `deploy: "started" | "latest"`, §3.1/§7.10/§22/C6);
  `run.createSignalToken({ types, ttl })` → `wst_…` (B2); `env.workflows.publish({ topic, type, payload, idempotencyKey })`.
- **Run states:** the public `state` set is `queued | running | sleeping | waiting | paused | stalled |
  compensating | completed | failed | cancelled` (A3 — no `suspended` mega-state; the suspension mechanism
  surfaces as `sleeping`/`waiting`).
- **Authoring:** `Workflow<Params, Output>` with `run(trigger, step)` where
  `trigger = { input, startedAt, runId, workflowName }`; `static compensationConcurrency` (opt-in
  parallel rollback, §21.3).
- **Steps:** `step.run(name, config?, fn)` with `config.retries.maxAttempts` / `config.backoff` /
  `config.timeout` / `config.output` (`"auto"|"inline"|"ref"|{as:"ref"|"stream"}`) / `config.compensate`
  (the single generic `StepConfig<T>`, §3.5/§3.8); `step.sleep(name, duration)`;
  `step.sideEffect(name, fn)` (inline journaled-once value for non-determinism, no retries/timeouts/compensation);
  `step.sleepUntil(name, when)` (absolute-deadline sibling, `when: Date | epoch-ms`, §3.2);
  `step.waitForSignal(name, { type?, timeout, maxSignalAge, topic? })` → `SignalEnvelope<P> | null`
  (`null` on timeout; envelope carries `origin`/`delivery`, A9); `step.all(steps, { concurrency? })`;
  `step.call(WorkflowClass, input, { key?, cascade?, timeout? })` → child `Output` (B4).
- **Compensation:** `Compensator<O>`, `CompensationContext` (`runId`, `stepName`, `ordinal`, `attempt`,
  `idempotencyKey`, `trigger`, `cause`), `RunError` (`{ type, message, stack?, compensation? }`, B6/A5)
  progress annotation (§3.8).
- **Errors:** `PermanentError` (business stop-retrying), `WorkflowDefinitionError` / `LimitExceededError`
  (author/platform misuse, B5), `StepTimeoutError`, `InvalidScheduleError`, `ChildTimeoutError`,
  `ChildCancelledError`, `ChildLimitError`, `RestartError` (§3.1/§22), plus engine-raised
  `NondeterministicError` / `StalledError`. (`waitForSignal` timeout is a `null`-return convention, **not** an
  exported error class — A4.)
- **Scheduling:** `schedule({ name, schedule, workflow, input, overlap?, catchUp? })` (C5 — the registration
  is `schedule(...)`, not `cron(...)`; `catchUp?: { mode: "skip" | "backfill"; max? }`, C2), the `every`
  fluent builder (terminals `every.day.at(...)`, `every.hour()`, `every.minute()`), `cronExpr(expr, tz?)`,
  `compileSchedule(...)` (returns an opaque `Schedule`; `kind` is an internal stored enum).

### 23.3 Preserved invariants

Zero tokio (compio timers + bespoke drivers), V8-per-thread / one isolate per app, dumb gateway
(it forwards + rate-limits; it never writes a journal row), unforgeable platform-emitted metering
(no `env.meter`), typed_id everywhere, a **control-plane-owned** Postgres journal — the control plane is the
sole reader/writer of
`zeroship.workflow_runs/_steps/_signals/_blobs/_broadcasts/_subscriptions/_schedules/_signal_keys`
(+ `app_deploys`), workers only *report* dispatch-completion envelopes (§4) — `wake_at`-unified suspension,
deploy-pinning, and `claimed_by` lease + advisory-lock claim sweep are all unchanged (§14). Pre-launch: no back-compat shims, no `@deprecated` aliases, no
`ALTER…backfill` — every producer/consumer changes in the same patch. No fabricated performance numbers;
throughput/latency that is not yet measured is stated as **unknown / to-measure**.

---

## Round-9 revision change log

Round 9 is a **naming/consistency refactor of the forever-surface API** — the Codex (gpt-5.5) ∩ Fable
reconciliation. No feature was added or removed; the day-1 scope of Round 8 is unchanged. Every change is a
name or shape reconciliation applied uniformly across prose, TypeScript, SQL DDL, index/CHECK predicates,
wire envelopes, the state-machine table, the §23.2 settled surface, the build plan, and the change logs'
active references. Bespoke-elegant, zero-tokio, gateway-dumb, control-plane-owned journal, no back-compat
shims, and no fabricated numbers are all honored.

### Defects fixed (A)

- **A1 — `started_at` added.** `zeroship.workflow_runs` gains `started_at timestamptz NOT NULL` (the planned
  fire instant, ≠ `created_at`); the `trigger.startedAt ↔ workflow_runs.started_at` mapping is stated once
  (§7.1) and threaded through the child-spawn / `startMany` inserts and §12.2.
- **A2 — `dsp_` → `wfd_`.** The dispatch/batch typed-id no longer collides with the billing-dispute `dsp_`
  prefix; the §16 open question is now **resolved**, not deferred.
- **A3 — one public run-state set.** The `suspended` mega-state is gone. The creator-visible `state` is
  exactly `queued | running | sleeping | waiting | paused | stalled | compensating | completed | failed |
  cancelled`, enforced by a §7.1 CHECK and used identically in §3.2/§7.1/§8/§9. `pending → queued`; the
  suspension *mechanism* surfaces as `sleeping` (timer-only) or `waiting` (awaits signal/child); `stalled` is
  the fail-closed terminal of the liveness backstop. "Suspension" survives only as an internal mechanism noun.
- **A4 — exported `SignalTimeout` deleted.** `waitForSignal` documents the `null`-return-on-timeout
  convention in prose; no throwing look-alike symbol remains.
- **A5 — `completed` everywhere.** `compensation_state` (`done`→`completed`), `compensation_outcome`
  (`complete`→`completed`), and `fanout_state` (`complete`→`completed`) share one terminal spelling.
- **A6 — uniform `Error` suffix.** `StepTimeout→StepTimeoutError`, `ChildTimeout→ChildTimeoutError`,
  `ChildCancelled→ChildCancelledError`, `ChildFanoutExceeded→ChildLimitError`.
- **A7 — one `StepConfig<T = unknown>`.** The non-generic `StepConfig` is gone; a single generic is presented
  once (§3.5) and completed in §3.8.
- **A8 — `step_seq → ordinal`** on `workflow_subscriptions` (the journal already used `ordinal`).
- **A9 — signal `source` split.** `source (internal|external|broadcast)` → `origin (app|ingress|system)` +
  `delivery (direct|topic)`; `external_source → provider`. All DDL, indexes, inserts, restart/child/broadcast
  SQL, and `SignalEnvelope` updated.

### Naming / elegance (B)

- **B1** `env.workflows.run(runId) → env.workflows.get(runId)` (+ `X.get`). **B2** `run.ingressToken →
  run.createSignalToken`, table `workflow_ingress_keys → workflow_signal_keys`, prefix `wik_ → wsk_`
  ("ingress" kept only in engine/ops prose). **B3** public output mode `blob → ref` (internal SQL keeps
  `output_kind='blob'`). **B4** `step.workflow → step.call`. **B5** `PermanentError` split — business
  stop-retrying stays `PermanentError`; misuse becomes `WorkflowDefinitionError` (bad config) and
  `LimitExceededError` (size/batch caps). **B6** `RunError.class → RunError.type` (+ stored `{type,…}`).
  **B7** `StuckError → StalledError`. **B8** w-family internal prefixes `sub_ → wsb_`, `bct_ → wbc_` (+
  `wik_ → wsk_`), documented in `typed_id.rs`. **B9** research jargon (`frontier`/`antichain`/`terminus`/
  `rail`) confined to engine sections; creator prose uses batch/group/reference-path. **B10**
  `retries.attempts → retries.maxAttempts` (DB stays `max_attempts`) + a public `backoff?: BackoffPolicy`.

### Extensibility — flat flags → growable shapes (C)

- **C1** `cancel({ compensate: true }) → cancel({ mode: "abort" | "compensate", reason? })`. **C2**
  `catchUp → { mode: "skip" | "backfill"; max? }`. **C3** ingress `?buffer=false → ?delivery=waitingOnly`
  (`buffered` default). **C4** `startMany` returns `StartManyResult[]` (`{ run, created?, conflict? }`).
  **C5** registration `cron({…}) → schedule({…})`; `every.hour()/every.minute()` terminal methods; the
  stored `Schedule.kind` union is internal (opaque `Schedule` to consumers). **C6** `repin → deploy`,
  `"original"/"current" → "started"/"latest"` (+ `{ pin }` object form). **C7** `onConflict` gains an object
  form `{ policy }` — **without** renaming the policy values.

### Held (D) — operator decisions preserved

- **`key`** stays the creator-facing word (internal column `dedup_key`); **`onConflict` values `join | reject
  | replace`** are unchanged. The C7 object form is additive and preserves them.

**Distinct renames applied: 35** (the term/shape renames enumerated in A2/A3/A5/A6/A8/A9 + B1–B10 + C1–C6),
plus structural additions (the `started_at` column, the `stalled` state, `BackoffPolicy`,
`WorkflowDefinitionError`/`LimitExceededError`, and the additive `onConflict`/`deploy` object forms) and the
A4 deletion. **`key` and `onConflict: join | reject | replace` were preserved verbatim** (section D untouched).
