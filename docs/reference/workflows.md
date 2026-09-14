# `@zeroship/workflows`

`@zeroship/workflows` is the creator-facing SDK for durable workflows. A
workflow is a named TypeScript class whose `run(trigger, step)` body can pause,
wait for signals, call child workflows, and survive process restarts because all
durable progress is recorded in an app-scoped workflow journal.

The SDK types live in `packages/workflows/`. The native `env.workflows` binding
starts and controls runs from app code, and the control client exposes the
token and topic broadcast helpers used by systems outside the app.

This reference describes the current implementation. The revised
[manager and job queue design](../proposals/2026-09-11-workflow-worker.md) assigns
cron, timers and durable job delivery to the workflow server. Workers consume
jobs and keep execution history and payloads in creator storage. Native manager
scheduling and creator Cron acceptance exist; production and local host
composition remain incomplete. The creator calendar loop has been removed,
so the current CLI task loop does not generate scheduled jobs. The provisioning instructions
below still apply today.

## Workflow class exports

The retained app entry names workflows through its named class exports or an
explicit `default.workflows` dictionary. The export key is the workflow name;
the constructor's JavaScript `name` property has no routing meaning. Synthetic
entries can preserve named exports with `export *` from the creator entry.

`step.call(Child, input)` and `step.startMany(Child, items)` resolve the actual
constructor against these bindings before producing a child frontier. Frozen
classes and minified identifiers work without renaming the constructor. An
unexported constructor cannot select another workflow by copying its name.
Each constructor must have an unambiguous export name, and each name must select
the same constructor wherever it is declared. Repeating the same binding in
named exports and `default.workflows` is allowed. Resolving a conflicting binding
fails dispatch. Unrelated callable exports are not instantiated during lookup.
Both prototype methods and instance-field implementations of `run` are supported.
Inherited dictionary properties and a bare default constructor do not
declare workflows, and a missing name never falls back to another class.

Development workflow execution uses the retained normal app bundle. Live
HTTP/RPC development snapshots do not supply workflow lookup, so reloading the
page handler does not replace the code selected by an existing workflow run.

## Workflow class exports

The retained app entry names workflows through its named class exports or an
explicit `default.workflows` dictionary. The export key is the workflow name;
the constructor's JavaScript `name` property has no routing meaning. Synthetic
entries can preserve named exports with `export *` from the creator entry.

`step.call(Child, input)` and `step.startMany(Child, items)` resolve the actual
constructor against these bindings before producing a child frontier. Frozen
classes and minified identifiers work without renaming the constructor. An
unexported constructor cannot select another workflow by copying its name.
Each constructor must have an unambiguous export name, and each name must select
the same constructor wherever it is declared. Repeating the same binding in
named exports and `default.workflows` is allowed. Conflicting bindings fail the
dispatch. Inherited dictionary properties and a bare default constructor do not
declare workflows, and a missing name never falls back to another class.

Development workflow execution uses the retained normal app bundle. Live
HTTP/RPC development snapshots do not supply workflow lookup, so reloading the
page handler does not replace the code selected by an existing workflow run.

## Rust integration

`zeroship-workflow` owns the journal engine, claim/apply protocol, scoped HTTP
client, and customer persistence through `zeroship-data-orm`. It has no V8 dependency.
`zeroship-workflow-v8` installs `env.workflows` and supplies the V8 executor
used by the local engine. The control plane uses the Rust engine directly.

The replacement service accepts an `OrmStore` built from the host's `OrmContext`,
`DbBinding` and `BackendHandle`. The ORM selects the configured database and owns
transactions. Journal operations use ORM collections and migration-derived Rust
models. Rust and creator ORM access permits workflow table
names within the bound customer schema; prefixes are not an authorization check.
`AppWorkflows::into_backend` exposes a bounded client for V8 and Rust callers on
other runtime threads; database operations remain on the engine's owning thread.
The existing Control PostgreSQL store remains until production cutover.

The replacement service retains accepted app-operation `RequestId` receipts in
the creator journal. Retrying the same request returns its original result even
after later lifecycle changes; changing its operation or body conflicts. These
receipts have no time-based expiry. A signal-token retry returns the original
token and preserves its expiration and revocation rules. Safe receipt retirement
requires a protocol that fences future retries.

A trusted Rust host can use `HttpWorkflowBackend` through `WorkflowBackend`
to start runs, read status, signal, restart, or change lifecycle state. Construct
it with `WorkflowClientConfig`, binding the app identity and its scoped token
once. Individual operations cannot select another app. The control plane still
authorizes the request; possession of a Rust handle does not bypass it.

The `operations` module defines typed requests and responses for these calls:
`StartOptions`, `SignalOptions`, `RestartOptions`, `RunOperation` and `RunState`.
Only workflow input, output and signal payloads are arbitrary JSON. Transport
serialization belongs to the HTTP client and V8 binding.
Operations return `WorkflowServiceError`, so Rust callers can match not-found,
conflict, authorization and capacity failures without interpreting HTTP status
codes. The V8 adapter exposes the corresponding `workflow_*` error codes.

`WorkflowBinding` performs the same binding for JavaScript. The host derives
its app-scoped credential with `app_scoped_token`; the control key stays outside
V8. Workflow execution remains replay of the deployed JavaScript class.

Native executors receive `WorkflowInvocation` and return `WorkflowExecution`
outcomes. Local development and deployed workers use the same replay input,
journal types and outcome decoder. The invocation carries no lease credential;
the Rust host binds outcomes to its claimed run before applying them. Run IDs
or nonces returned by JavaScript do not select the mutation target.

## Local development

Workflows belong to the app's normal `.zship` deployment. Runs pin that app
deployment and load its code and runtime descriptor through the app bundle
loader. Activation acquires a durable deployment hold before selecting code;
replay verifies the held app manifest and its referenced blobs. Local development
uses the same app build and retains normal deployment metadata beside the
artifacts. The [worker design](../proposals/2026-09-11-workflow-worker.md) describes
the remaining production retention cutover, which keeps platform bundle
collection independent of customer SQL.

The CLI runs workflows through the same native manager and delivered jobs as a
deployment, independently of HTTP requests. `zeroship serve dist/app.zship`
loads the app's server modules for HTTP and workflow execution. Vite builds and
publishes the local app artifact automatically while serving client assets and
live modules through its dev bridge, and restarts the CLI when the artifact
changes. There is no workflow-only archive argument or TOML bundle setting.
Workflow execution reads from the retained app bundle store without making
separate executable copies.

A manager thread owns the local platform metadata file,
`.zeroship/platform/metadata.sqlite`, which holds both the normal deployment
catalog and the manager's queue, placement, scheduling and recovery records.
Starting the CLI with an archive records that deployment, registers its
schedules and activates it unless it is already selected; each restart with a
new archive activates the new deployment while existing runs keep their pinned
code. The creator applies the activation as a delivered job, and the CLI accepts
requests only after that activation has committed. A second thread runs the
ordinary job consumer over the app database with a trusted local worker
identity. Starts, signals and settled jobs publish their pending jobs
immediately; periodic manager reconciliation publishes anything a crash left
behind, and periodic collection removes abandoned payload uploads. Restart keeps
queued jobs, timers, receipts and retained bundles.

The CLI resolves one app identity for HTTP handlers, workflow execution and app
storage. A configured `APP_ID` must be canonical; when absent, the CLI uses the
shared `local_dev_app_id()` identity. Restart with the same configuration selects
the same app and rediscovers its durable work.

The journal uses the app database selected by `DATABASE_URL`, and payloads use
the app's normal storage configuration. With default SQLite configuration, the
ORM places the app tables and workflow journal in `.zeroship/zs-<app_id>.sqlite`;
object storage defaults to `.zeroship/storage`. Workflow execution uses this
host-selected identity without a separate identity file, database or object
directory. Incompatible journals are refused without resetting them, and
initialization preserves business tables. An incompatible platform metadata file
is likewise refused without being rewritten.

`--workflow-config=workflow.toml` configures native execution limits. Its optional
`consumer` table bounds execution slots, claim polling, backoff, execution and
per-operation time; `manager` bounds delivery leases, the worker's placement
lifetime, maintenance cadence and lanes, and the reconciliation and collection
interval; `payloads` configures `TaskPayloadLimits`. Database paths and object
storage belong to normal app configuration; workflow TOML rejects separate
`journal`, `objects` and database settings. The CLI has no dedicated workflow
reset command.

## Testing

Run `cargo xtask test workflow` after building the workspace SDKs. Rust tests
own backing services through Testcontainers and include API isolation, journal
fencing, scheduling, real worker replay, and gateway dispatch authorization.
Docker is required; unavailable services fail the tests.

The workflow examples own their Vitest and Playwright tests, fixtures, and
configuration. Run `pnpm test` from `examples/workflow-probe` or
`examples/workflows-order` to test an example independently. The test runner
builds the example and platform binaries before starting its services.

## Journal provisioning

Before starting deployed workflows, provision their app journal through the
migration service:

```http
POST /v1/apps/{app_id}/workflows/provision
Authorization: Bearer <creator-access-token>
```

The caller needs deployment permission for that app. The operation is
idempotent and creates no creator database tables. Applying creator migrations
also provisions the journal schema, so apps already using that path need no
additional request. The control origin routes this endpoint to the migration
service; control and workers only create journal tables inside the provisioned
schema. Local workflows create their SQLite journal automatically.

## Model

A workflow is a class extending `Workflow<Params, Output>`:

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

`trigger` is:

```ts
interface WorkflowTrigger<Params = unknown> {
  input: Params;
  startedAt: Date;
  runId: string;
  workflowName: string;
}
```

The platform runs `run()` once per dispatch against the run's journal. Completed
steps are memoized: on replay, the SDK returns the journaled value instead of
calling the step body again. Sleeps, waits, child calls, failed steps, large
output refs, and compensator progress are all journal rows.

Durability is the journal plus the deploy pin. A run survives crashes, worker
eviction, process restarts, and redeploys because replay uses the deploy that
created the retained journal prefix. One dispatch executes on one node, but a
run can move across nodes between dispatches because state lives in the journal,
not in process memory.

Workflow classes can be exported by name:

```ts
export class Checkout extends Workflow<{ orderId: string }, { ok: boolean }> {
  async run(trigger: WorkflowTrigger<{ orderId: string }>, step: Step) {
    await step.run("work", () => doWork(trigger.input.orderId));
    return { ok: true };
  }
}
```

Raw JavaScript deploys can also expose a workflow namespace on the default
export:

```ts
export default {
  workflows: { Checkout },
};
```

The active deploy manifest must declare the workflow name before
`env.workflows.<Name>.start(...)` can create a run.

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
do not need retries, timeout, compensation, or blob output:

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

The public `Step` type is:

```ts
interface WorkflowStep {
  run<T>(name: string, fn: () => T | Promise<T>): Promise<T>;
  run<T>(
    name: string,
    config: StepConfig<T> & {
      output:
        | "ref"
        | "blob"
        | "stream"
        | { as: "ref" | "blob" | "stream"; contentType?: string };
    },
    fn: () => T | Promise<T>,
  ): Promise<StepOutputRef>;
  run<T>(name: string, config: StepConfig<T>, fn: () => T | Promise<T>): Promise<T>;

  sideEffect<T>(name: string, fn: () => T | Promise<T>): Promise<T>;
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

interface BackoffConfig {
  base?: string;
  max?: string;
  factor?: number;
}

interface StepConfig<T = unknown> {
  retries?: RetryConfig;
  backoff?: BackoffConfig;
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

Semantics:

- The first miss runs `fn`, records the result or failure, and suspends the
  dispatch so the control plane can commit the journal row.
- A replay hit returns the recorded result and does not call `fn`.
- `timeout` bounds the step body. Timeout failures surface as the step timeout
  error class.
- `retries` and `backoff` are step execution policy, not workflow-body control
  flow.
- `output` controls how the step output is represented. Explicit `"ref"`,
  `"blob"`, or `"stream"` returns a `StepOutputRef`.
- `compensate` attaches a rollback function for terminal failure.

Duration strings accepted by workflow sleeps and timeouts include suffixes such
as `ms`, `s`, `m`, `h`, and `d`; plain positive numbers are milliseconds.

### `step.sideEffect`

`step.sideEffect(name, fn)` computes a small value once and journals it inline.
It has no retry, timeout, output mode, child, or compensation behavior.

Use it for values such as timestamps, UUIDs, random choices, and small
configuration reads whose value must be stable across replay.

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
  readonly origin?: "app" | "ingress" | "system";
  readonly delivery?: "direct" | "topic";
  readonly topic?: string;
  readonly provider?: string;
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
child. Without it, the child remains independent.

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

Results are returned in issue order. The shipped batch cap is 1,000 items; over
the cap throws `LimitExceededError`.

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

`step.continueAsNew` cannot be called from inside a step body. If the current
generation has pending compensators, the transition is rejected with
`CompensableCarryError` and no successor generation is created.

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
workflow, and key while the run is live. Once that run is completed, failed or
cancelled, an app start may reuse the key. This does not make the key a
permanent receipt for retrying an ambiguous transport request.

`onConflict` accepts:

- `"join"`: return the existing run for the key. This is the default.
- `"reject"`: fail when a live run already owns the key.
- `"replace"`: cancel the incumbent run, clear its key, and create a new run.
- `{ policy: "join" | "reject" | "replace" }`: object form of the same policy.

The native binding exposes a typed handle per workflow name. Rehydrate a known
run with:

```ts
const run = env.workflows.Checkout.get(runId);
```

The SDK `WorkflowRun` interface is:

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
  | "cancelled";

interface WorkflowRun<Output = unknown> {
  readonly id: string;
  signal(opts: { type: string; payload?: unknown; idempotencyKey?: string }): Promise<void>;
  status(): Promise<{ state: WorkflowRunState; output?: Output | StepOutputRef; error?: unknown }>;
  readStepOutput(name: string, occurrence: number): Promise<Uint8Array>;
  pause(): Promise<void>;
  resume(): Promise<void>;
  cancel(opts?: { mode?: "abort" | "compensate" }): Promise<void>;
  restart(opts?: RestartOptions): Promise<WorkflowRun<Output>>;
  createSignalToken(opts: { types: string[]; ttl: string }): Promise<string>;
}
```

`pause()` stops dispatching until `resume()`. `cancel()` aborts the run
without running compensators, and ends it as `cancelled`.

> **`mode` is not implemented on either backend.** `cancel()` accepts the
> options object and ignores it, so `{ mode: "compensate" }` aborts exactly
> like `{ mode: "abort" }` and no rollback runs. This is not a gap at one call
> site: the backend contract is
> `transition(run_id, op: RunOperation)`, so there is nowhere for a
> runtime-chosen mode to travel. Implementing it means changing that contract,
> not forwarding an argument. Until then, do not read a `compensate` cancel as
> a rollback: to roll back completed steps, let the run FAIL, which is the
> path that does run compensators.

`restart(opts?)` requeues the same run ID:

The SQLite and PostgreSQL adapters share deploy-policy and quiescence checks.
A restart rejects live execution leases, active descendants and active
compensation. A partial restart retains the prefix and its original deploy;
it cannot retain steps whose compensation already finished. SQLite rewrites
the discarded checkpoints, their signal consumption and run state in a
transaction, so a failed restart preserves the previous journal.

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
dropped. The run input is retained; use a new `start()` to change input.

## Local Development

`zeroship serve` runs `env.workflows` through an in-process SQLite mini-engine.
It uses the same `@zeroship/workflows` SDK surface and the same runtime replay
shim as deployed runs, but stores the journal in a dev-local SQLite database
owned by the local process.

Local `start()`, `signal()` and `restart()` return after their journal
transaction commits. The local runner executes accepted work separately;
execution failure does not turn an accepted start into a failed API call.
Checkpoint batches and their resulting run state also commit together.

The local engine is dev-only by construction: the CLI serve path is the only
construction vector that can create the SQLite backend. Production workers build
`env.workflows` with the HTTP control-plane backend and never select the local
engine.

Intentional local divergences:

- Single process only. There are no multi-node leases, lease reclaim races, or
  cross-worker handoffs in the SQLite engine.
- No gateway ingress edge. `run.signal(...)` works locally; public signal
  routes and topic ingress are deployed-only features.
- SQLite lifetime is local to the dev process and configured file path. Deleting
  the file deletes the local workflow journal.
- Schedules and large workflow blobs are deployed-engine parity items unless
  explicitly listed as local support.

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

Schedule registrations are discovered at build time and stored in the deploy
manifest. The manager evaluates calendar metadata and persists each occurrence
with its deployment, activation revision and run identity. The creator worker
verifies that deployment, resolves its static input and atomically accepts the
occurrence with a run and Advance publication intent. It performs no calendar
evaluation. Completed acceptance and overlap skips survive redelivery;
unavailable prerequisites and capacity failures remain retryable.

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

Invalid schedules throw `InvalidScheduleError` during build/deploy
compilation, not at fire time. Sub-minute cron, unknown timezones, and
unsupported cron tokens are rejected.

## External Signals And Broadcast

There are three signal producers:

- `run.signal({ type, payload, idempotencyKey? })` from app code.
- A public signal route for systems outside the app.
- Topic broadcast, which fans one signal out to all matching topic waits.

A run-addressed public signal uses:

```text
POST https://{app}.zeroship.ai/__zeroship/signals/v1/run/{runId}
Authorization: Bearer <signal-token>
Content-Type: application/json
Idempotency-Key: <event-id>

{ "type": "payment.approved", "payload": { "approved": true } }
```

A topic signal uses:

```text
POST https://{app}.zeroship.ai/__zeroship/signals/v1/topic/{topic}
Authorization: Bearer <signal-token>
Content-Type: application/json
Idempotency-Key: <event-id>

{ "type": "price.updated", "payload": { "price": 42 } }
```

Public route handling verifies the token, checks the allowed signal types, and
writes journal rows. It does not run app code on the ingress path. The matching
workflow dispatch happens later.

Mint a narrow per-run token through the run handle where that helper is
available:

```ts
const token = await run.createSignalToken({
  types: ["payment.approved", "payment.failed"],
  ttl: "48h",
});
```

The control client exposes the concrete token and broadcast helpers:

```ts
import { createControlClient } from "@zeroship/control";

const control = createControlClient({ baseUrl, token });

const runToken = await control.workflows.createSignalToken(runId, {
  appId,
  types: ["payment.approved"],
  ttl: "48h",
});

await control.workflows.publishTopic(`order:${orderId}`, {
  appId,
  type: "payment.approved",
  payload: { approved: true },
  idempotencyKey: eventId,
});
```

Inside a workflow, subscribe to a topic by passing `topic`:

```ts
const signal = await step.waitForSignal("market-tick", {
  type: "price.updated",
  topic: `market:${trigger.input.symbol}`,
  timeout: "1h",
});
```

## Large Outputs

Saved step outputs are read through the app-scoped native workflow backend.
`run.readStepOutput(name, occurrence)` returns bytes; replay uses that same
operation for lazy `StepOutputRef` reads. The host keeps the control endpoint
and credential in Rust. Local development reads the saved SQLite checkpoint.

Small JSON outputs are inlined in the journal. Larger outputs, or outputs with
an explicit by-reference mode, are stored as workflow blobs and replayed as
`StepOutputRef`.

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
returns a `StepOutputRef`; use `ref.stream()` to read it. `run.status()` returns
a ref for a blob-backed final output instead of inlining it into the status JSON.

If an output exceeds the platform blob cap, the run fails with
`LimitExceededError`.

## Compensation

A compensator is attached to a `step.run` with `config.compensate`. It is a
function, reconstructed from the deploy-pinned workflow code during rollback:

```ts
interface CompensationContext {
  readonly idempotencyKey: string;
  readonly trigger: WorkflowTrigger<unknown>;
  readonly cause?: unknown;
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
    retries: { maxAttempts: 3 },
    compensate: (out: { reservationId: string }, ctx) =>
      releaseReservation(out.reservationId, ctx.idempotencyKey),
  },
  () => reserveInventory(trigger.input.orderId),
);
```

When the run reaches terminal failure, the engine walks completed compensable
steps in reverse journal order and runs their compensators. A compensator may
run more than once after crash, retry, or lease handoff. Make the undo effect
idempotent by using `ctx.idempotencyKey` with the external system or durable
record that performs the undo.

Only completed `step.run` steps with a compensator are rolled back. Sleeps,
signals, child waits, incomplete steps, and steps whose errors were caught and
handled are not compensated.

`run.cancel()` is a hard abort and does not run compensators. Nor does
`run.cancel({ mode: "compensate" })`: the `mode` option is accepted and
ignored on both backends, so cancelling is never a rollback today. See the
note on `cancel()` above. A failing run is currently the only path that runs
compensators.

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
  outcome: "completed" | "partial";
}
```

A `partial` rollback always ends the run `failed`. Local development and deployed
apps run compensators the same way.

## Errors

The SDK exports these workflow error classes:

| Error | When it fires | Catchable? |
| --- | --- | --- |
| `PermanentError` | Business failure that should not retry. If it escapes `run()`, the run fails and eligible compensators run. | Yes, if you intend to handle it and continue. |
| `StepTimeoutError` | A step exceeds its configured timeout. The replay shim may serialize the internal step-timeout name in stored errors. | Yes around `step.run`; if uncaught, normal failure handling applies. |
| `NondeterministicError` | Bare workflow-body I/O/timers, journal name/kind/order mismatch, or unsupported step-promise control flow. | Treat as terminal misuse; do not swallow it. No rollback. |
| `StalledError` | The engine detects repeated dispatches with no durable progress. | Terminal engine error. No rollback. |
| `ChildCancelledError` | A `step.call` child is cancelled before the parent join completes. | Yes around `step.call`; if uncaught, normal failure handling applies. |
| `ChildTimeoutError` | A `step.call` child exceeds `ChildWorkflowOptions.timeout`. | Yes around `step.call`; if uncaught, normal failure handling applies. |
| `LimitExceededError` | A platform cap is exceeded, such as `step.startMany` over 1,000 items or output over the blob cap. | Sometimes. Local `step.startMany` cap is catchable; committed cap failures are terminal. |
| `CompensableCarryError` | `step.continueAsNew` is requested while the current generation still has pending compensators. | No. Finish or clear compensation first; no successor generation is created. |
| `RestartError` | A run restart request is invalid or cannot be applied. | Outside `run()` only, around `run.restart(...)`. |

The package also exports compatibility classes for stored wait timeouts and
definition/runtime misuse. Prefer the specific classes above and branch on
structured status/error fields for run monitoring.

## Dos And Donts

Do:

- Put all I/O in `step.run`.
- Put inline clocks, random values, and UUIDs in `step.sideEffect`.
- Use stable step names. If a name appears in a loop, `occurrence` identifies
  which issuance restart should target.
- Use `Promise.all` for durable fan-out over step promises.
- Use idempotency keys in forward steps and compensators. Step bodies and
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

## Gotchas

- `waitForSignal` returns `null` on normal timeout; it does not need an error
  branch for the common timeout path.
- A run can replay many times. Module-level mutable state is not workflow state.
- `step.sideEffect` is not a cheaper `step.run` for I/O. It is for small values
  that are safe to compute inline once.
- `step.call` joins the child. Use top-level starts from handlers for detached
  work.
- Blob-backed outputs are read lazily through `StepOutputRef`; `status()` will
  not inline them.
- A compensator receives the original step output. Use that output to undo the
  exact effect the forward step produced.
