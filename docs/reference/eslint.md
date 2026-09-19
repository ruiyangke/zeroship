# ESLint

zeroship defines two flat-config ESLint packages for app code:

- `@zeroship/eslint-config` — the shareable config. It carries one rule,
  `no-unindexed-query`, which warns when an app-database query looks like a
  sequential scan.
- `@zeroship/eslint-plugin-workflow` — correctness rules for durable workflows:
  `no-nested-step`, `no-nondeterministic-between-steps`,
  `no-nondeterministic-step-name`, `no-parallel-steps`, and
  `no-step-catch-without-rethrow`.

Both packages export a `recommended` preset for ESLint's flat config, plus the
individual rules so you can select or re-weight them. ESLint itself is not
bundled.

Install the config with your package manager:

```
npm install --save-dev @zeroship/eslint-config
```

`@zeroship/eslint-config` is published to the zeroship registry.
`@zeroship/eslint-plugin-workflow` is not in the current publish list, so it
does not resolve from the registry yet; the workflow section below describes the
rules it ships. Neither package declares an `eslint` dependency or peer range,
so install an ESLint release that supports flat config — the version floor is
not pinned here.

## Context

The examples below use the same runtime surfaces as the rest of this reference:

- `env.db` — the app's typed database handle from `@zeroship/db`.
- `env.workflows` — the namespace for starting and controlling durable workflow
  runs.
- `step` — the `WorkflowStep` argument to a workflow's `run()`; `step.run(name,
  fn)` is a durable effect whose result is journaled.
- `trigger` — the `WorkflowTrigger` argument to `run()`; `trigger.input` carries
  the start input.
- `journal` — the app-scoped record of a run's progress. Replay reads it instead
  of re-running completed steps.
- `dispatch` — one execution of a workflow body against the journal; a run is
  dispatched again after a wait or a lost lease.

A *sequential scan* is a query that reads the table without an index. A
*durable frontier* is the single ordered line of step calls a run advances
through. A *durable effect boundary* is a `step.run` body, whose result is
journaled. See [db.md](db.md) and [workflows.md](workflows.md) for the full
contracts.

## Minimal config

An `eslint.config.js` that enables both presets:

```js
import zeroship from "@zeroship/eslint-config";
import workflow from "@zeroship/eslint-plugin-workflow";

export default [zeroship.recommended, workflow.recommended];
```

Each `recommended` is a self-contained config object; include just one if you
want only that package's rules. To change a severity, add an object after the
preset:

```js
export default [
  zeroship.recommended,
  workflow.recommended,
  { rules: { "@zeroship/workflow/no-parallel-steps": "error" } },
];
```

The same object re-weights any rule by its full key — for example,
`"@zeroship/workflow/no-nondeterministic-between-steps": "warn"` or
`"@zeroship/no-unindexed-query": "off"`.

The rule keys and the severity each preset assigns:

| Rule key | Preset | Catches |
| --- | --- | --- |
| `@zeroship/no-unindexed-query` | `warn` | An app-database filter that may not hit an index. |
| `@zeroship/workflow/no-nondeterministic-step-name` | `error` | A clock, random, or UUID value used as a step name. |
| `@zeroship/workflow/no-nondeterministic-between-steps` | `error` | A clock, random, UUID, or `env.workflows` call in the workflow body outside a step. |
| `@zeroship/workflow/no-nested-step` | `error` | A step call made from inside a step body. |
| `@zeroship/workflow/no-parallel-steps` | `warn` | A `Promise` combinator over step calls, supported or not. |
| `@zeroship/workflow/no-step-catch-without-rethrow` | `error` | A catch around a step call that can finish without rethrowing what it caught. |

## `no-unindexed-query`

Flags an app-database `.find({ ... })` or `.deleteMany({ ... })` whose filter is
an object literal with at least one static key that is not known to be indexed.
It ships as a warning. The runtime side of the same check only logs: outside
production, the first such query per filter shape emits a one-time
`console.warn` naming the collection and the declared indexes, and it never
throws. Raising the rule to `error` in a later config object is what makes it
fail a build.

```ts
// Warned: `status` may not be indexed.
await env.db.todos.find({ status: "active" });

// Warned: `userId` may not be indexed.
await env.db.sessions.deleteMany({ userId });

// Not warned: `id` is the primary key and is always indexed.
await env.db.users.find({ id: userId });

// Not warned: a filter the rule cannot read statically.
await env.db.todos.find(filter);
```

The rule does not read your schema, so it warns on any static filter key other
than `id` or `_id` — or an operator key (`$and`, `$or`, `$not`, or any key
beginning with `$`) — even one you have already indexed, so a false positive is
expected. At runtime the platform does read the schema: a filter covered by a
declared index is silent, and one that is not emits the warning above. The rule
is the earlier nudge in the editor.

To fix a report, declare the index for the field the filter uses, or accept the
sequential scan and silence the rule. Named indexes are declared in the
migration that creates the table, under `indexes`:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_todos",
  schema() {
    table("todos").create({
      columns: {
        userId: t.text().references("users", "id"),
        status: t.text(),
      },
      indexes: [{ name: "by_status", on: ["status"] }],
    });
  },
};
```

Field order matters: a multi-column index covers any leftmost prefix of its
fields, and a single field can carry `.unique()`. See
[Named indexes](db.md#named-indexes).

The rule reads only a direct object-literal argument. A spread, a computed or
dynamic key, a non-object filter, and an empty filter are all left alone. It
does not inspect `.update`, `.updateMany`, or any other method. `.get` is not
covered either: only `.find` and `.deleteMany` are matched, so a filtered
`.get({ email })` is not reported by the rule, even though the runtime warning
covers `.get` and it can scan.

## Workflow rules

Four of the workflow rules follow the determinism contract in
[workflows.md](workflows.md#determinism): the workflow body may observe the
outside world only through journaled step output, and a replayed run must take
the same path it took the first time. The fifth,
[`no-step-catch-without-rethrow`](#no-step-catch-without-rethrow), guards the
other half of that bargain: the platform stops a body by throwing through it, so
a catch that swallows the throw runs the body on past the point it was meant to
stop. Each rule below states what it catches and what to do instead. Where a
rule guards a runtime condition, the error the platform records is named: bare
body I/O, journal mismatch, and unsupported step-promise control flow fail
closed with `NondeterministicError`, and a `step.*` call from inside a step body
raises `NestedStepError` ([workflows.md](workflows.md#errors)). The plugin also matches a `step.do` call
the way it matches `step.run`; `step.do` is not declared on the documented
`WorkflowStep` surface.

### `no-nondeterministic-step-name`

A step name must be the same on every replay, because the journal matches steps
by name; a name that changes fails the replay closed with
`NondeterministicError`. This rule reports a step-name argument that contains
`Date.now()`, `Math.random()`, `crypto.randomUUID()`, or `new Date()` —
including a value embedded in a template literal.

```ts
// Reported: the name changes on every dispatch.
await step.run(`order-${Date.now()}`, () => loadOrder(id));

// Reported: a random name cannot be matched on replay.
await step.sleep(`wait-${Math.random()}`, "1m");

// Fine: a name built from stable input.
await step.run(`order-${trigger.input.id}`, () => loadOrder(id));
```

To fix a report, compute the value inside a prior `step.run` body and build the
name from the journaled result, so replay derives the same name.

The rule checks the name argument of the methods that take one: `run`, `do`,
`sleep`, `sleepUntil`, and `waitForSignal`. It does not match `sideEffect`,
`startMany`, or `continueAsNew`, so a nondeterministic `sideEffect` name is not
reported.

### `no-nondeterministic-between-steps`

The workflow body must not read the clock, randomness, or request-scoped
runtime state directly, because a replay would read different values and the
runtime would fail closed with `NondeterministicError`. This rule reports
`Date.now()`, `Math.random()`, `crypto.randomUUID()`, `new Date()`, and any
`env.workflows.*` call that appears outside a step body.

```ts
// Reported: a live clock read in the workflow body.
const createdAt = Date.now();

// Reported: a request-scoped runtime call in the workflow body.
await env.workflows.Email.start({ to: user.email });

// Fine: both live inside a step, so the result is journaled once.
const createdAt = await step.run("created-at", () => Date.now());
await step.run("send-email", () => env.workflows.Email.start({ to: user.email }));
```

To fix a report, move the call into a `step.run` body and use the journaled
result on every later dispatch; `step.run(name, fn)` returns that recorded value
on replay.

This rule treats a `step.run`/`step.do` body as a safe place to read the clock, random
values, and `env.workflows`. It does not recognize a `step.sideEffect` callback
as a step body, so `step.sideEffect("created-at", () => Date.now())` — the form
recommended in [Determinism](workflows.md#determinism) for small inline values —
is still reported. The runtime accepts it and journals the value, but the preset
sets this rule to `error`, so the recommended form fails a build. Either wrap
the read in `step.run`, or keep `step.sideEffect` and re-weight the rule:

```js
export default [
  zeroship.recommended,
  workflow.recommended,
  { rules: { "@zeroship/workflow/no-nondeterministic-between-steps": "warn" } },
];
```

### `no-nested-step`

A step body is a durable effect boundary, and a step call made from inside one
is unsupported: the runtime raises `NestedStepError` and the run cannot trust
replay. This rule reports a step call nested inside a step body.

```ts
// Reported: step.sleep inside the step.run body.
await step.run("outer", async () => {
  await step.sleep("inner", "1m");
  return "done";
});

// Fine: the sleep is its own top-level step, awaited in order.
await step.run("outer", async () => "done");
await step.sleep("inner", "1m");
```

To fix a report, split the nested call out into its own top-level step after the
enclosing one and await them in sequence.

The rule recognizes the step methods `run`, `do`, `sleep`, `sleepUntil`,
`waitForSignal`, and `call`, and it looks for them inside a `step.run`/`step.do`
body. It does not match `sideEffect`, `startMany`, or `continueAsNew`, so a
nested `step.continueAsNew` — for which the runtime also raises
`NestedStepError` — is not reported. The
[workflows contract](workflows.md#dos-and-donts) forbids any `step.*` call from
inside a step body; the linter covers `run`, `do`, `sleep`, `sleepUntil`,
`waitForSignal`, and `call`.

### `no-parallel-steps`

Step calls form a single durable frontier. Issuing them before awaiting, usually
with `Promise.all`, is supported but serializes through that frontier.
`Promise.race`, `Promise.any`, and `Promise.allSettled` over step promises are
not supported because they can corrupt replay; the runtime fails closed with
`NondeterministicError` for that control flow.

```ts
// Reported (warning): safe, but serialized through the frontier.
const [a, b] = await Promise.all([
  step.run("a", loadA),
  step.run("b", loadB),
]);

// Reported: race/allSettled/any over step promises are unsupported.
const first = await Promise.race([step.run("a", loadA), step.run("b", loadB)]);

// Fine: await steps sequentially when order is not fixed.
const a = await step.run("a", loadA);
const b = await step.run("b", loadB);
```

`Promise.all` reports a warning that recommends sequential awaits or a
deliberate choice. `Promise.race`, `Promise.allSettled`, and `Promise.any`
report that they are unsupported. In the `recommended` preset the whole rule is
`warn`, so even the unsupported-combinator report is a warning; raise the rule
to `error` by its full key (see [Minimal config](#minimal-config)) if you want
it to fail a build.

The rule reads the first argument of a `Promise` combinator and recognizes the
step methods `run`, `do`, `sleep`, `sleepUntil`, `waitForSignal`, and `call`. It
does not match `sideEffect`, `startMany`, or `continueAsNew`.

### `no-step-catch-without-rethrow`

The platform stops a workflow body mid-flight by throwing through it: a
suspension at the step the run is waiting on, a `step.continueAsNew`, and the
end of the replay a rollback does to find its compensators all arrive that way.
None of them carries a class `@zeroship/workflows` exports, so a catch that
discards what it did not match discards those too, and the body runs on past the
point it was meant to stop. In a forward dispatch the step calls it makes after
that execute for real and are then thrown away unrecorded, so their effects land
again on the dispatch that replaces them. This rule reports a `try` whose block
holds a step call when the `catch` can finish without rethrowing what it caught,
and the same shape written as `.catch()` on a step promise. See
[Matching an error](workflows.md#matching-an-error).

```ts
// Reported: the catch swallows the stop along with the failure.
try {
  total = await step.run("charge", chargeCard);
} catch {
  total = null;
}

// Reported: only the matched class is rethrown; everything else is kept.
try {
  await step.run("charge", chargeCard);
} catch (e) {
  if (e instanceof PermanentError) throw e;
  total = null;
}

// Reported: a new error replaces the one that carried the stop.
try {
  await step.call(Fulfill, order);
} catch (e) {
  throw new Error(`fulfillment failed: ${e.message}`);
}

// Reported: .catch() on a step promise swallows the same throw.
const total = await step.run("charge", chargeCard).catch(() => null);

// Fine: the matched class is handled and the rest is rethrown.
try {
  await step.run("charge", chargeCard);
} catch (e) {
  if (e instanceof StepTimeoutError) return retryLater();
  throw e;
}
```

To fix a report, rethrow the caught value itself on the path you do not handle.
Claim the classes you mean to handle by name and end the handler with
`throw e`, or write the handling in the branch and the `throw e` in its `else`.

The rule follows the path an unclaimed error takes through the handler. At an
`if` that is the `else` of a positive test, the `then` branch of a negated one
(`!x`, `x !== y`, `x === false`, and `&&`/`||` over those), and otherwise the
code after the `if`. A handler is reported when that path runs off the end of
the block, returns, or throws a value other than the one it caught. The
platform recognizes its own stop by identity, so a wrapped error loses it. What
the other branches do is yours to decide and is not read.

The rule does not model a loop, a `switch`, a label or a nested `try` inside a
handler: a handler built from one of those is left alone rather than guessed at.
It reads only the handler's own statements, so a call to a helper that always
rethrows is still reported: write the `throw` where the rule can see it. A
`finally` is not a rescue: it cannot name the caught binding, so a `throw` there
replaces the value that carried the stop rather than passing it on. A `.catch()`
argument that is not a function literal is not read, and
`.then(onFulfilled, onRejected)` is not matched at all.

The rule recognizes the step methods `run`, `do`, `sleep`, `sleepUntil`,
`waitForSignal`, and `call`, in the `try` block and in the expression a
`.catch()` hangs off. It does not match `sideEffect`, `startMany`, or
`continueAsNew`, so a `try` whose block holds only `step.continueAsNew`, one of
the three stops named above, is not reported.
