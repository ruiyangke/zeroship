# `@zeroship/workflows`

> **Availability.** Not published to the creator registry as of 2026-09-17, so
> this SDK is not installable from a scaffolded app; the runtime's workflow host
> runs regardless.

`@zeroship/workflows` is the SDK for durable workflows. A workflow is a named
TypeScript class whose `run(trigger, step)` body can pause, wait for a signal,
call other workflows, and use timers, without writing the orchestration state
itself. The platform records that progress in an app-scoped journal, so a run
survives process restarts, worker replacement, and redeploys: when a dispatch
replays, completed steps return their recorded values instead of running again.

A run is pinned to the deployment that created its journal prefix. Replay always
loads that deployment's code, so a redeploy does not change the code an in-flight
run is halfway through. A run carries its state in the journal, not in process
memory, so it can move between machines between dispatches while one dispatch
executes on a single machine.

## Quick start

Define a workflow as a class written in TypeScript, exported with a stable name:

```ts
import { Workflow, type Step, type WorkflowTrigger } from "@zeroship/workflows";

export class Checkout extends Workflow<{ orderId: string }, { charged: boolean }> {
  async run(
    trigger: WorkflowTrigger<{ orderId: string }>,
    step: Step,
  ): Promise<{ charged: boolean }> {
    const order = await step.run("load-order", () => loadOrder(trigger.input.orderId));
    await step.run("charge", () => chargeOrder(order));
    return { charged: true };
  }
}
```

The type parameters are the run's input and its result. `trigger.input` carries
the input a start supplied; `step.run(name, fn)` marks `fn` as a durable step
whose result is journaled and replayed.

The active deploy must declare the workflow name before it can run. A class
exported by name is declared; a raw-JavaScript deploy declares names through a
`default.workflows` dictionary. See [Workflow class exports](#workflow-class-exports).

Start it from an app handler through `env.workflows`:

```ts
import { env } from "zeroship";

export async function order(request: Request): Promise<Response> {
  const { orderId } = await request.json();
  const run = await env.workflows.Checkout.start({ input: { orderId } });
  return Response.json({ runId: run.id }, { status: 202 });
}
```

Then observe it and await a result:

```ts
const status = await env.workflows.Checkout.get(runId).status();
// status.state is "queued", "running", "sleeping", "waiting", ...
// status.output carries the workflow's result once it is "completed".
```

`start()` returns once the run is accepted and appends its `id`. The steps then
execute asynchronously; the caller polls `status()` until `state` reaches a
terminal value.

## Workflow class exports

Workflows are named by their exported names. Either form declares a workflow:

- A named class export, where the export key is the workflow name.
- A `default.workflows` dictionary, mapping a name to its class.

```ts
export class Checkout extends Workflow<...> { ... }
```

```ts
export default {
  workflows: { Checkout },
};
```

Both may coexist and repeat the same name for the same class. The constructor's
JavaScript `name` property has no routing meaning; the export key is the only
name a workflow has.

Each constructor must resolve to one unambiguous name, and each name must select
the same constructor everywhere it is declared. A name bound to two different
classes is ambiguous, and dispatching it fails. A workflow with no exported name
cannot be started, and an unexported constructor cannot become a child target by
copying a name. Minification and frozen classes do not change these bindings.

`step.call(Child, input)` and `step.startMany(Child, items)` resolve `Child`
against these bindings, so the child you call is the exported class, not a name
string.

## Model

A workflow extends `Workflow<Params, Output>`:

```ts
export class Checkout extends Workflow<{ orderId: string }, { charged: boolean }> {
  async run(
    trigger: WorkflowTrigger<{ orderId: string }>,
    step: Step,
  ): Promise<{ charged: boolean }> { ... }
}
```

`Workflow` is an abstract class with two type parameters — the run's input and
its result — and one abstract method to implement:

```ts
abstract class Workflow<Params = unknown, Output = unknown> {
  abstract run(
    trigger: WorkflowTrigger<Params>,
    step: WorkflowStep,
  ): Output | Promise<Output>;
}
```

It also declares optional static `concurrency` and `compensationConcurrency`
fields. These are reserved declarations and are not yet enforced by the
platform.

`trigger` is:

```ts
interface WorkflowTrigger<Params = unknown> {
  input: Params;
  startedAt: Date;
  runId: string;
  workflowName: string;
}
```

`run()` is invoked once per dispatch against the run's journal. Completed steps
are memoized: on replay the SDK returns the journaled value instead of calling
the step body again. Sleeps, waits, child calls, failed steps, large output
refs, and compensator progress are all journal rows.

## Determinism

The workflow body may only observe the outside world through journaled
`step.run(...)` or `step.sideEffect(...)` output. Live I/O, timers, random
values, dates, and request-scoped runtime calls in the body can produce a
different result on replay. The runtime fails closed with
`NondeterministicError`.

Wrong:

```ts
export class SyncOrder extends Workflow<{ id: string }, unknown> {
  async run(trigger: WorkflowTrigger<{ id: string }>, _step: Step) {
    const response = await fetch(`/api/orders/${trigger.input.id}`);
    return response.json();
  }
}
```

Right:

```ts
export class SyncOrder extends Workflow<{ id: string }, unknown> {
  async run(trigger: WorkflowTrigger<{ id: string }>, step: Step) {
    return step.run("fetch-order", async () => {
      const response = await fetch(`/api/orders/${trigger.input.id}`);
      return response.json();
    });
  }
}
```

`fetch(...)` inside `step.run` runs when the step is first reached. On later
dispatches, the recorded output is replayed and the fetch body does not run.

Use `step.sideEffect(name, fn)` for small inline non-deterministic values that
do not need a timeout, compensation, or blob output:

```ts
const createdAt = await step.sideEffect("created-at", () => Date.now());
const nonce = await step.sideEffect("nonce", () => crypto.randomUUID());
```

The value is computed once, frozen into the journal, and returned unchanged on
replay.

Supported concurrency is the durable frontier formed by issuing several
`step.*` calls before awaiting them, usually with `Promise.all`. `Promise.race`,
`Promise.any`, and `Promise.allSettled` over step promises are unsupported.
Calling another `step.*` method from inside a step body is also unsupported.

## Step Surface

The step parameter is the `WorkflowStep` interface; `Step` is an exported alias
for it, so `step: Step` and `step: WorkflowStep` name the same type:

```ts
type StepBody<T> = (ctx: StepContext) => T | Promise<T>;

interface WorkflowStep {
  run<T>(name: string, fn: StepBody<T>): Promise<T>;
  run<T>(
    name: string,
    config: StepConfig<T> & {
      output:
        | "ref"
        | "blob"
        | "stream"
        | { as: "ref" | "blob" | "stream"; contentType?: string };
    },
    fn: StepBody<T>,
  ): Promise<StepOutputRef>;
  run<T>(name: string, config: StepConfig<T>, fn: StepBody<T>): Promise<T>;

  sideEffect<T>(name: string, fn: StepBody<T>): Promise<T>;
  sleep(name: string, duration: string): Promise<void>;
  sleepUntil(name: string, when: Date | number): Promise<void>;
  waitForSignal<P = unknown>(
    name: string,
    opts?: WaitForSignalOptions,
  ): Promise<SignalEnvelope<P> | null>;
  call<P, O>(
    WorkflowClass: new () => Workflow<P, O>,
    input: P,
    opts?: ChildWorkflowOptions,
  ): Promise<O>;
  startMany<P, O>(
    WorkflowClass: new () => Workflow<P, O>,
    items: readonly StartManyItem<P>[],
    opts?: ChildWorkflowOptions,
  ): Promise<O[]>;
  continueAsNew<P = unknown>(input: P): Promise<never>;
}
```

### `step.run`

`step.run(name, config?, fn)` is the general durable effect boundary. The body
may perform I/O, use timers, call `env.*`, compute output, and throw errors.

```ts
interface RetryConfig {
  maxAttempts?: number;
}

interface StepConfig<T = unknown> {
  retries?: RetryConfig;
  timeout?: string;
  output?:
    | "auto"
    | "inline"
    | "ref"
    | "blob"
    | "stream"
    | { as: "ref" | "blob" | "stream"; contentType?: string };
  compensate?: Compensator<T>;
}
```

#### The step context

Every step body receives a `StepContext`. Each field is a durable journal fact,
so a body that re-executes observes exactly what the discarded execution did.

```ts
interface StepContext {
  readonly runId: string;
  readonly workflowName: string;
  readonly ordinal: number;
  readonly name: string;
  readonly occurrence: number;
  readonly idempotencyKey: string;
  readonly trigger: WorkflowTrigger<unknown>;
}
```

`ctx.idempotencyKey` is the idempotency defence. A step body runs *before* its
journal row is committed. If the worker crashes or its lease expires in that
window, the frontier is discarded, the run is reassigned, and the body runs
again — against an external effect that already landed. Pass the key to the
external system so the duplicate is recognised and dropped:

```ts
const charge = await step.run("charge-card", (ctx) =>
  stripe.paymentIntents.create(
    { amount: order.totalCents, currency: "usd" },
    { idempotencyKey: ctx.idempotencyKey },
  ),
);
```

The key is stable across re-execution of the same step, and distinct for every
step a restart re-runs. Declaring the parameter is optional; a body that does
not need the context omits it.

Semantics:

- The first miss runs `fn`, records the result or failure, and suspends the
  dispatch so the journal row can be committed.
- A replay hit returns the recorded result and does not call `fn`.
- `timeout` bounds the step body. A body still running when the bound expires
  fails the step with `StepTimeoutError`, and the recorded error is retryable.
  The body is not cancellable, so it is abandoned rather than stopped: keep the
  effect idempotent under `ctx.idempotencyKey`. A timeout the runtime cannot
  read as a duration fails the run before the body is invoked.
- `output` controls how the step output is represented. Explicit `"ref"`,
  `"blob"`, or `"stream"` returns a `StepOutputRef`.
- `compensate` attaches a rollback function for terminal failure.
- `retries.maxAttempts` is how many times the body may run. It counts
  executions, not re-executions, so the default of one is the same step you get
  by declaring nothing. A value that is not a positive integer fails the run
  before the body is invoked, the way an unreadable `timeout` does.

A step timeout is not the only bound a slow step meets. The host that executes
the run carries its own execution timeout, invisible to app code, and whichever
bound is smaller ends the step: past the host's, the step's own timeout never
fires and no `StepTimeoutError` is recorded. The host bound is 30 seconds by
default and may be tuned per deployment, so size step timeouts well below
30 seconds if you want the step's own failure in the journal.

#### Retries

A failing step with attempts left does not fail its run. The platform records
the attempt, requeues the run, and re-executes the body on the next dispatch;
only the last failure is recorded as the step's outcome and rethrown into your
`run()`.

What matters when you use it:

- **Attempts share one `ctx.idempotencyKey`.** A retry is at-least-once against
  whatever the body touched, exactly as a re-execution after a lost lease is.
  Pass the key to the external system and let it recognise the duplicate.
  Spacing does not make an effect safe to repeat; the key does.
- **An error that declares `retryable: false` spends no further attempt.**
  `PermanentError` declares it, so a business failure ends the step however many
  attempts remain. `StepTimeoutError` declares `true`. An error that declares
  nothing is retried.
- **The wait between attempts is platform-provisioned, not set by the app.** It
  is durable on the run rather than a timer in the worker — the worker that
  failed the attempt is gone before the next one starts — and it defaults to
  1 second. `RetryConfig` exposes no knob for it. The platform also provisions
  a ceiling on how many attempts a step may declare, defaulting to 8:
  `maxAttempts` above the app's ceiling is refused rather than quietly becoming
  a smaller number.

```ts
const charge = await step.run(
  "charge-card",
  { retries: { maxAttempts: 3 } },
  (ctx) =>
    stripe.paymentIntents.create(
      { amount: order.totalCents, currency: "usd" },
      { idempotencyKey: ctx.idempotencyKey },
    ),
);
```

Duration strings accepted by workflow sleeps and timeouts include suffixes such
as `ms`, `s`, `m`, `h`, and `d`; plain positive numbers are milliseconds.
ISO 8601 durations such as `PT5M` are also accepted.

### `step.sideEffect`

`step.sideEffect(name, fn)` computes a small value once and journals it inline.
It has no retry, timeout, output mode, child, or compensation behavior.

Use it for values such as timestamps, UUIDs, random choices, and small
configuration reads whose value must be stable across replay.

Its body receives the same `StepContext` as `step.run`, on the same terms: the
value is computed before the journal row commits, so the body can re-execute.
The context is there for deriving a stable value, not as licence to do I/O here
— that belongs in `step.run`.

### `step.sleep` and `step.sleepUntil`

`step.sleep(name, duration)` suspends the run until `now + duration`. The worker
does not stay attached while the run sleeps.

`step.sleepUntil(name, when)` suspends until an absolute instant. `when` can be a
`Date` or epoch milliseconds. A past instant behaves like a zero-length sleep.

Both methods journal a sleep frontier. On replay after the wake time, they
resolve without re-sleeping.

### `step.waitForSignal`

`step.waitForSignal(name, opts?)` suspends until a matching signal is available:

```ts
interface WaitForSignalOptions {
  type?: string;
  timeout?: string;
  maxSignalAge?: string;
  topic?: string;
}

interface SignalEnvelope<P = unknown> {
  readonly id: string;
  readonly type: string;
  readonly payload: P;
  readonly createdAt: Date;
  readonly origin?: "app" | "ingress";
  readonly delivery?: "direct" | "topic";
  readonly topic?: string;
}
```

`type` defaults to `name`. `timeout` resolves the wait to `null` when no
matching signal arrives in time. `maxSignalAge` rejects stale signals at bind
time. `topic` subscribes this wait to a broadcast topic instead of the run's
direct mailbox.

```ts
const approval = await step.waitForSignal<{ approved: boolean }>("approved", {
  type: "order.approved",
  timeout: "1h",
  maxSignalAge: "10m",
});

if (!approval?.payload.approved) {
  throw new PermanentError("order approval window elapsed");
}
```

### `step.call` and `step.startMany`

`step.call(WorkflowClass, input, opts?)` starts a child workflow and waits for
its typed output.

```ts
interface ChildWorkflowOptions {
  key?: string;
  cascade?: boolean;
  timeout?: string;
}

const risk = await step.call(RiskReview, {
  orderId: trigger.input.orderId,
}, {
  key: `risk:${trigger.input.orderId}`,
  cascade: true,
  timeout: "5m",
});
```

The child is an ordinary run. Its output is journaled into the parent step. If
the child is cancelled, the parent receives `ChildCancelledError`. If the child
does not finish before `timeout`, the parent receives `ChildTimeoutError`.

`cascade: true` means cancelling the parent also requests cancellation of a live
child. Without it, the child remains independent. The request reaches children
in bounded batches after the parent settles; a cascading child cannot continue
as new or restart while its parent's cancellation is still reaching its
children.

`step.startMany(WorkflowClass, items, opts?)` is the in-workflow fan-out helper:

```ts
interface StartManyItem<P = unknown> {
  input: P;
  key?: string;
  options?: ChildWorkflowOptions;
}

const outputs = await step.startMany(SendReceipt, [
  { input: { userId: "usr_1" }, key: "receipt:usr_1" },
  { input: { userId: "usr_2" }, key: "receipt:usr_2", options: { timeout: "1m" } },
], {
  cascade: true,
});
```

Results are returned in issue order. Every item joins the single frontier that
dispatch submits, so one batch shares a budget with every other step issued in
the same turn. The app plan sets that budget as `maxFrontier`, and the platform
is what holds it: the value reaches no isolate, so the SDK cannot measure a
batch against it before issuing one. A dispatch whose frontier exceeds the
plan's budget is refused rather than journaled, so no step is recorded, the run
does not advance, and the body has nothing to catch. Size a fan-out against the
plan the app runs under, and split a wider one across successive batches,
awaiting each before issuing the next.

### `step.continueAsNew`

`step.continueAsNew(input)` completes the current generation and starts a fresh
root generation of the same workflow with `input`.

```ts
if (trigger.input.page < nextPage) {
  await step.continueAsNew({
    page: nextPage,
    cursor,
  });
}
```

The call never returns. Use it as the final action in the workflow body. The
fresh generation starts from ordinal `0`, receives the supplied input as its
trigger input, and uses the active deploy when the transition is applied.

`step.continueAsNew` cannot be called from inside a step body. A compensator
belongs to the generation whose step registered it, and a successor starts from
an empty journal, so a generation that still owes one cannot carry it across.
The transition is refused and no successor is created: the pending compensators
run, and the generation rests `failed` with `CompensableCarryError` on the run.
Read it from `run.status()`, the same way a `StalledError` is read; the body
cannot catch it, because the call that asked for the transition already ended
the dispatch.

## Instances

App handlers start and control runs through `env.workflows`:

```ts
import { env } from "zeroship";

const run = await env.workflows.Checkout.start({
  input: { orderId },
  key: `checkout:${orderId}`,
  onConflict: "join",
});

await run.signal({ type: "approved", payload: { by: "system" } });
const status = await run.status();
```

`start({ input, key, onConflict })` creates a run and returns a `WorkflowRun`.
`key` is optional. When present, it deduplicates starts for the same app,
workflow, and key while the run is live. Once that run is completed, failed, or
cancelled, an app start may reuse the key. This does not make the key a
permanent receipt for retrying an ambiguous transport request.

`onConflict` accepts:

- `"join"`: return the existing run for the key. This is the default.
- `"reject"`: fail when a live run already owns the key.
- `"replace"`: cancel the incumbent run, clear its key, and create a new run.
- `{ policy: "join" | "reject" | "replace" }`: object form of the same policy.

The namespace exposes a typed handle per workflow name. Rehydrate a known run
with:

```ts
const run = env.workflows.Checkout.get(runId);
```

`@zeroship/types` declares `env` as an open index signature and publishes no
`workflows` namespace, so `env.workflows` is untyped. `@zeroship/workflows`
exports `WorkflowRun`, so a typed handler declares a narrow wrapper and casts
once, in one place, instead of casting `any` throughout:

```ts
import type { WorkflowRun } from "@zeroship/workflows";
import { env } from "zeroship";

const workflows = env.workflows as {
  Checkout: {
    start(opts: {
      input: { orderId: string };
      key?: string;
    }): Promise<WorkflowRun<{ charged: boolean }>>;
    get(runId: string): WorkflowRun<{ charged: boolean }>;
  };
};
```

The `WorkflowRun` interface is:

```ts
type WorkflowRunState =
  | "queued"
  | "running"
  | "sleeping"
  | "waiting"
  | "paused"
  | "stalled"
  | "compensating"
  | "completed"
  | "failed"
  | "cancelled"
  | "continuedAsNew";

interface WorkflowRun<Output = unknown> {
  readonly id: string;
  signal(opts: { type: string; payload?: unknown; idempotencyKey?: string }): Promise<void>;
  status(): Promise<{
    state: WorkflowRunState;
    output?: StatusOutputRef;
    error?: unknown;
    continuedAsNew?: string;
  }>;
  readStepOutput(name: string, occurrence: number): Promise<Uint8Array>;
  readOutput(): Promise<Uint8Array>;
  pause(): Promise<void>;
  resume(): Promise<void>;
  cancel(opts?: { mode?: "abort" | "compensate" }): Promise<void>;
  restart(opts?: RestartOptions): Promise<WorkflowRun<Output>>;
}
```

A run that closed by handing its work to a successor rests in `continuedAsNew`,
and its `status()` reply then carries `continuedAsNew`: the run id of the
successor that generation started. The key is absent on a run that started none.

The SDK type also declares `createSignalToken(opts: { types: string[]; ttl: string })`
on `WorkflowRun`, but the runtime binding that backs `env.workflows` does not
implement it, and minting the scoped signal bearer token is part of the public
ingress surface that is not yet exposed to creator apps. It is therefore not
part of the supported surface above, and calling it is not supported today.

`pause()` stops dispatching until `resume()`. `cancel()` aborts the run and
ends it as `cancelled`.

> **`mode` is not implemented.** `cancel()` accepts the options object and
> ignores it, so `{ mode: "compensate" }` aborts exactly like `{ mode: "abort" }`
> and no rollback runs. Do not read a `compensate` cancel as a rollback: to roll
> back completed steps, let the run FAIL, which is the path that does run
> compensators.

`restart(opts?)` requeues the same run ID:

```ts
interface RestartTarget {
  name: string;
  occurrence?: number;
}

interface RestartOptions {
  from?: RestartTarget;
  deploy?: "started" | "latest" | { pin: "started" | "latest" };
}

await run.restart({ from: { name: "charge" } });
await run.restart();
await run.restart({ deploy: "started" });
```

With `from`, the journal prefix before the named step is retained and the target
step plus everything after it is dropped. Without `from`, the full journal is
dropped. A restart rejects live execution leases, active descendants, active
compensation, and a cascading child whose parent's cancellation is still
propagating. A partial restart retains the prefix and its original deploy; it
cannot retain steps whose compensation already finished. The run input is
retained; use a new `start()` to change input.

`deploy` selects the code the restarted run re-executes under. A full restart
defaults to `"latest"` (the current deploy); a partial restart keeps the
`"started"` pin and rejects `"latest"`.

## Schedules

Schedules start fresh runs at deploy-reconciled times. Use `schedule(...)` with
the fluent `every(...)` DSL, `cronExpr(...)`, or a raw 5-field cron string.

```ts
import { Workflow } from "@zeroship/workflows";
import { every, cronExpr, schedule } from "@zeroship/workflows/schedule";

class NightlyReport extends Workflow<{ region: string }, void> {
  async run(trigger, step) {
    await step.run("report", () => buildReport(trigger.input.region));
  }
}

schedule({
  name: "nightly-report-us",
  schedule: every.day.at("03:00", "America/New_York"),
  workflow: NightlyReport,
  input: { region: "us" },
  overlap: "skipIfRunning",
  catchUp: { mode: "backfill", max: 3 },
});

schedule({ name: "poll", schedule: every(15, "minutes"), workflow: PollInbox });
schedule({ name: "heartbeat", schedule: cronExpr("*/5 * * * *"), workflow: Heartbeat });
```

Registrations are discovered at build time and stored in the deploy manifest.
An occurrence is accepted atomically with a run start when the manager fires it,
exactly as if a handler had called `start`, so scheduled runs are ordinary runs.

Supported schedule forms:

- `every(15, "minutes")`, `every(5).minutes()`, `every(2).hours()`,
  `every(1).days()` for fixed intervals.
- `every.minute()`, `every.hour()`, `every.hour().at(30)`.
- `every.day.at("03:00", tz?)` and `every().day().at("03:00", tz?)`.
- `every.monday.at("09:00", tz?)` and the other weekday properties.
- `every.month.on(day).at("00:00", tz?)`, with day 1 through 28.
- raw 5-field cron strings, using UTC.
- `cronExpr(expr, tz?)` for raw cron with a timezone.

Defaults:

```ts
type ScheduleOverlap = "allow" | "skipIfRunning";
type ScheduleCatchUp =
  | { readonly mode: "skip" }
  | { readonly mode: "backfill"; readonly max: number };
```

`overlap` defaults to `"allow"`. `catchUp` defaults to `{ mode: "skip" }`.
`skipIfRunning` suppresses a fire while an earlier scheduled run is still live.
`backfill` starts up to `max` missed fires after downtime.

Cron schedules carry an IANA timezone and follow local clock time, including
daylight-saving transitions. Fixed intervals are duration based and do not
shift for daylight saving time. Use cron forms for "at this local time" and
interval forms for "every N units".

Invalid schedules throw `InvalidScheduleError` during build/deploy compilation,
not at fire time. Sub-minute cron, unknown timezones, and unsupported cron
tokens are rejected.

## External Signals And Broadcast

A signal is how a run learns about an event outside its own steps. There are two
surfaces today:

- `run.signal({ type, payload })` from app code delivers a signal to a known run.
- `step.waitForSignal({ type, ... })` consumes a matching signal inside a run.
  Pass `topic` to subscribe to a broadcast topic instead of the run's own
  mailbox.

```ts
await run.signal({ type: "payment.approved", payload: { approved: true } });
```

```ts
const signal = await step.waitForSignal("market-tick", {
  type: "price.updated",
  topic: `market:${trigger.input.symbol}`,
  timeout: "1h",
});
```

A signal reaches a run that is waiting; a waiting run that has been signalled
becomes due and its dispatch resumes with the consumed signal.

Public signal ingress — receiving signals from systems outside the app over the
edge, and minting the scoped bearer token those systems would present — is not
yet exposed to creator apps. `createSignalToken`, which mints that bearer token,
is declared on the SDK `WorkflowRun` type but not implemented by the runtime
binding, so there is no way to obtain a token today. Signal delivery from within
the app itself is the supported path.

## Large Outputs

Saved step outputs are read through the run. `run.readStepOutput(name,
occurrence)` returns bytes; replay uses that same operation for lazy
`StepOutputRef` reads. `run.readOutput()` returns the run's final output.

Small JSON step outputs are inlined in the journal. Larger step outputs, or step
outputs with an explicit by-reference mode, are stored as workflow blobs and
replayed as `StepOutputRef`. A run's final output is always a workflow blob.

```ts
interface StepOutputRef {
  readonly kind: "workflow-step-output-ref";
  readonly ref: string;
  readonly hash: string;
  readonly size: number;
  readonly contentType?: string;
  json<T = unknown>(): Promise<T>;
  text(): Promise<string>;
  arrayBuffer(): Promise<ArrayBuffer>;
  bytes(): Promise<Uint8Array>;
  stream(): ReadableStream<Uint8Array>;
}
```

Examples:

```ts
const ref = await step.run(
  "render-report",
  { output: { as: "blob", contentType: "application/json" } },
  () => renderReport(trigger.input.reportId),
);

const report = await ref.json<{ rows: unknown[] }>();
```

`"blob"` and `"ref"` use the same by-reference representation. `"stream"` also
returns a `StepOutputRef`; use `ref.stream()` to read it.

`run.status()` never inlines a final output. It reports a `StatusOutputRef`, a
descriptor the host serialises as JSON, so it locates the blob and carries no
readers. `run.readOutput()` returns the bytes. A run that returned nothing
reports no output at all.

```ts
interface StatusOutputRef {
  readonly kind: "ref";
  readonly ref: string;
  readonly hash: string;
  readonly size: number;
  readonly contentType?: string;
}

const { output } = await run.status();
const bytes = await run.readOutput();
```

The inline threshold is 1 MiB, and it applies to step outputs: a step output at
or under it is journaled inline, and anything larger, or explicitly
by-reference, becomes a workflow blob. A run's input and its final output take
no threshold. Whatever starts a run and whatever it returns are staged as blobs,
so each costs an object against the app's payload budget however small it is.
That covers a run started through `start()`, a child started by `step.call` or
`step.startMany`, a successor seeded by `step.continueAsNew`, and every firing
of a schedule alike. A single blob may not exceed 64 MiB; an output over that
cap fails the `step.run` that produced it with `LimitExceededError`, which a
`catch` around that step can handle, and an input over it fails the start.

## Compensation

A compensator is attached to a `step.run` with `config.compensate`. It is a
function, reconstructed from the deploy-pinned workflow code during rollback:

```ts
interface CompensationContext {
  readonly idempotencyKey: string;
  readonly trigger: WorkflowTrigger<unknown>;
}

type Compensator<T> = (
  output: T,
  ctx: CompensationContext,
) => unknown | Promise<unknown>;
```

Example:

```ts
const reservation = await step.run(
  "reserve-inventory",
  {
    timeout: "30s",
    compensate: (out: { reservationId: string }, ctx) =>
      releaseReservation(out.reservationId, ctx.idempotencyKey),
  },
  () => reserveInventory(trigger.input.orderId),
);
```

When the run reaches terminal failure, the platform walks completed compensable
steps in reverse journal order and runs their compensators. A compensator may
run more than once after crash, retry, or lease handoff. Make the undo effect
idempotent by using `ctx.idempotencyKey` with the external system or durable
record that performs the undo.

Only completed `step.run` steps with a compensator are rolled back. Sleeps,
signals, child waits, incomplete steps, and steps whose errors were caught and
handled are not compensated.

`run.cancel()` is a hard abort and does not run compensators. Nor does
`run.cancel({ mode: "compensate" })`: the `mode` option is accepted and ignored,
so cancelling is never a rollback today. A failing run is currently the only
path that runs compensators.

`NondeterministicError` and `StalledError` fail closed and do not enter
rollback. They indicate the engine cannot trust replay enough to safely rebuild
the compensator registry.

When rollback finishes, the run keeps the failure that started it: `error.type`
and `error.message` are the creator's original error. The same object carries a
`compensation` summary:

```ts
interface CompensationSummary {
  total: number;      // compensators that ran to a final result
  completed: number;
  failed: number;
  outcome: "completed" | "partial" | "abandoned";
  failures?: { ordinal: number; error: unknown }[];
  abandoned?: { ordinal: number; name: string }[];
  reason?: unknown;   // the StalledError that stopped an abandoned rollback
}
```

A `partial` rollback always ends the run `failed`, and `failures` names each
compensator that reported one.

A compensator that never returns is not a `partial` rollback: nothing reported,
so nothing is known. The platform reclaims those dispatches against the same
stuck-dispatch budget a forward run gets — 4 consecutive reclaimed dispatches —
and when it is spent the rollback is abandoned. The run still rests `failed`
with the creator's original error,
because a rollback the platform gave up on does not overturn the verdict the
workflow body produced. The summary reads `abandoned`, `abandoned` lists the
steps whose compensators never reported, and `reason` carries the
`StalledError` that stopped the wait. Those steps are outside `total`,
`completed`, and `failed`, which count only compensators that reached a final
result.

Treat an abandoned obligation as an undo of unknown state: the compensator may
have applied part of its effect before it stopped reporting. The step name and
ordinal are what let you go and check.

## Errors

The SDK exports these workflow error classes:

| Error | When it fires | Catchable? |
| --- | --- | --- |
| `PermanentError` | Business failure that cannot be cleared by running the body again. Declares `retryable: false`, so it ends the step whatever `retries` allowed; if it escapes `run()`, the run fails and eligible compensators run. | Yes, if you intend to handle it and continue. |
| `StepTimeoutError` | A step body is still running when `StepConfig.timeout` expires. Declares `retryable: true`, so it spends an attempt rather than ending the step. | Yes around `step.run`; if uncaught, normal failure handling applies. |
| `NondeterministicError` | Bare workflow-body I/O/timers, journal name/kind/order mismatch, or unsupported step-promise control flow. | Treat as terminal misuse; do not swallow it. No rollback. |
| `StalledError` | The platform reclaimed 4 consecutive dispatches of one frontier without the run reporting an outcome. | Not raised in your body: it is the platform's verdict. On a forward frontier it is recorded on the run, which rests `stalled`. On a rollback frontier it is recorded as `compensation.reason` and the run rests `failed` with the rollback abandoned. Terminal either way. No rollback. |
| `ChildCancelledError` | A `step.call` child is cancelled before the parent join completes. | Yes around `step.call`; if uncaught, normal failure handling applies. |
| `ChildTimeoutError` | A `step.call` child exceeds `ChildWorkflowOptions.timeout`. | Yes around `step.call`; if uncaught, normal failure handling applies. |
| `LimitExceededError` | A platform cap is exceeded, such as an output over the blob cap. | Yes when one `step.run` output busted the cap: that step is recorded `failed` carrying this class with `retryable: false`, the run continues, and the body resumes at the row, so a `catch` around the step can take another path. Every other cap names no step and is the platform's verdict, recorded on the run, which rests `failed`: a `step.sideEffect` output, a whole dispatch result, a run output, a continuation seed. A cap the platform applies to a dispatch rather than to a payload, such as `maxFrontier`, refuses the dispatch without recording anything and raises no class here. |
| `NestedStepError` | A `step.*` method is called from inside a step body or compensator. | Treat as terminal misuse. Fix the body rather than handling it. |
| `CompensableCarryError` | `step.continueAsNew` is requested while the current generation still has pending compensators. | No. It is the platform's verdict: the transition is refused, no successor generation is created, the pending compensators run, and the generation rests `failed` carrying this name. |
| `InvalidScheduleError` | A schedule registration is invalid — a malformed or unsupported cron expression, an unknown IANA timezone, or a non-positive interval count or backfill. Thrown when the schedule is compiled at build/deploy time, never at fire time or on a run. Exported from `@zeroship/workflows/schedule`. | Yes, at registration time; catch it beside the `schedule(...)` call that raised it. |

Except `InvalidScheduleError`, which is thrown at build time, every class above
names a condition the platform records on a run or on a step. A class recorded
on a run is that run's terminal verdict, not something its own body is resumed
to catch; read it off `run.status()`. That verdict still reaches a body one way:
a parent that joined the run with `step.call` is handed the run's recorded error
at that join, so the parent's next dispatch throws it out of `await
step.call(...)`, and a `catch` there matches whatever class the recorded `type`
names. The control operations an app handler calls, `start` on a workflow and
`status`, `signal`, `pause`, `resume`, `cancel`, `restart` and `readStepOutput`
on a `WorkflowRun`, never enter the journal, so they never carry one of those
names. They reject through a different path with a stable failure shape: every
refusal the engine returns is a plain `Error` whose `code` is one of
`workflow_invalid_request`, `workflow_not_found`,
`workflow_conflict`, `workflow_unavailable`,
`workflow_timeout`, `workflow_resource_exhausted`,
`workflow_payload_too_large`, `workflow_permission_denied`,
`workflow_unauthenticated`, `workflow_ingress_fenced` or
`workflow_internal_error`. The message is the engine's own, except under
`workflow_unavailable` and `workflow_internal_error`, which describe a host
condition you cannot act on and say only that.

`workflow_invalid_request` means the engine refused the arguments themselves
and the call did nothing: an argument past a platform limit, such as an
oversized `key` or signal `type`, or one that does not resolve against the run
it names, such as a `restart` target no step in that run's journal matches.
Correct the call rather than retrying it. `workflow_conflict` instead names a
run state that rejects the operation, so the same call can succeed later.

An argument whose *type* is wrong never reaches the engine. The binding rejects
it before the call with a built-in `TypeError` and no `code`. So
`instanceof TypeError` means you passed the wrong kind of value, and an
`error.code` means the engine refused the call. The SDK exports no class for
either shape; branch on `error.code` and on the operation you called, not on
the identity of what threw.

### Matching an error

A workflow error is identified by its `name`, not by the constructor that built
it. A step failure is recorded in the journal as `{ type, message, ... }`, and
the dispatch that rethrows it into the body rebuilds it from that row, so the
object caught is never the object thrown. Each exported class matches on the
recorded name, which makes the ordinary form hold for a replayed failure and for
one raised live:

```ts
try {
  await step.run("charge", chargeCard);
} catch (e) {
  if (e instanceof StepTimeoutError) return retryLater();
  throw e;
}
```

The same holds for an error a body throws itself: `throw new PermanentError(...)`
is caught as `e instanceof PermanentError` after the round trip. Because the
match is on the recorded name, any error carrying that name matches, including
one the runtime rebuilt as a plain `Error`.

A terminal error read off `run.status()` is the recorded JSON itself, not an
`Error`, so no class matches it however it is named. Branch on `error.type`
there.

The trailing `throw e` is required, not a stylistic flourish. The platform stops
a body mid-flight by throwing through it: a suspension at the step the run is
waiting on, a `step.continueAsNew`, and the end of the replay a rollback does to
find its compensators all arrive that way. None of them carries a class this
package exports, so nothing you can name will match them. A catch that discards
what it did not match discards those too, and the body runs on past the point it
was meant to stop, holding values no step produced. In a forward dispatch the
step calls it makes after that execute for real and are then thrown away
unrecorded, so their effects land again on the dispatch that replaces them.
Write catches that claim what they handle and rethrow the rest;
`catch (e) { fallback(); }` around a step call, with no throw on the unmatched
path, is the shape to avoid.

A recorded failure carries `type`, `message`, an optional `stack`, and
`retryable` when the thrown error declared one. An error that declares nothing
records no `retryable` at all, which is a different answer from a declared
`false`. Replay rethrows the flag the journal holds, so a body that catches a
replayed failure reads the same value the journal recorded. Errors raised by the
runtime itself always declare theirs: misuse of the step API is never retryable,
and `StepTimeoutError` always is.

## Dos And Donts

Do:

- Put all I/O in `step.run`.
- Put inline clocks, random values, and UUIDs in `step.sideEffect`.
- Use stable step names. If a name appears in a loop, `occurrence` identifies
  which issuance restart should target.
- Use `Promise.all` for durable fan-out over step promises.
- Use `ctx.idempotencyKey` in forward steps and compensators. Step bodies and
  compensators can run more than once even though their recorded result is used
  once.
- Keep signal types explicit and small. Use `maxSignalAge` for buffered public
  signals that should expire.

Dont:

- Do not call `fetch`, timers, `Date.now`, random, or `env.workflows.*` directly
  in the workflow body unless the value is captured by a step.
- Do not call `step.*` from inside a step body or compensator.
- Do not reorder or rename already-journaled steps for a live run unless you are
  deliberately restarting from before the changed point.
- Do not use `Promise.race`, `Promise.any`, or `Promise.allSettled` over step
  promises.
- Do not catch `NondeterministicError` to keep going. Fix the workflow body.
- Do not let a catch swallow what it did not match. Rethrow the rest.

## Gotchas

- `waitForSignal` returns `null` on normal timeout; it does not need an error
  branch for the common timeout path.
- A run can replay many times. Module-level mutable state is not workflow state.
- `step.sideEffect` is not a cheaper `step.run` for I/O. It is for small values
  that are safe to compute inline once.
- `step.call` joins the child. Use top-level starts from handlers for detached
  work.
- Blob-backed step outputs are read lazily through `StepOutputRef`. `status()`
  reports a run's final output as a `StatusOutputRef` descriptor, never as the
  value itself, and `run.readOutput()` returns the bytes.
- A compensator receives the original step output. Use that output to undo the
  exact effect the forward step produced.

## Local Development

`zeroship serve` runs `env.workflows` through an in-process SQLite engine. It
uses the same `@zeroship/workflows` SDK surface and the same runtime replay
behavior as deployed runs, but stores the journal in a dev-local SQLite
database owned by the local process.

Local `start()`, `signal()`, and `restart()` return after their journal
transaction commits. The local runner executes accepted work separately;
execution failure does not turn an accepted start into a failed API call.
Checkpoint batches and their resulting run state also commit together.

Intentional local divergences:

- Single process only. There are no multi-node leases, lease reclaim races, or
  cross-worker handoffs in the local engine.
- No gateway ingress edge. `run.signal(...)` works locally; public signal
  routes and topic ingress are deployed-only features.
- SQLite lifetime is local to the dev process and its configured file path.
  Deleting the file deletes the local workflow journal.
- Schedules and large workflow blobs are deployed-engine parity items unless
  explicitly listed as local support.