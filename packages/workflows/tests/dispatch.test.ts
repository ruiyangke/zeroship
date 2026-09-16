import assert from "node:assert/strict";
import { test } from "node:test";

// The dispatcher under test is the one the platform ships: the host module
// `crates/zeroship-workflow-v8` embeds with `include_str!` and registers as
// `zeroship:workflows/dispatch`. It is plain ESM over `node:async_hooks`, so
// Node drives the same bytes V8 runs. There is no second implementation, and a
// case here that passes describes the deployed replay bridge.
//
// `crates/zeroship-workflow-v8/tests/dispatch/` drives the same file through
// the real runtime and binding. That suite owns module registration, isolate
// lifetime and native readers; this one owns replay semantics.
import {
  dispatch,
  installBodyGuards,
} from "../../../crates/zeroship-workflow-v8/js/dispatch.js";

import { Workflow } from "../src/index.ts";
import type { StepContext, WorkflowStep, WorkflowTrigger } from "../src/index.ts";

// Native startup calls this once per isolate, before any creator module
// evaluates. The guards it installs are scoped to a dispatching body, so the
// process keeps unguarded `fetch` and timers everywhere else.
installBodyGuards();

const TEST_TIMEOUT_MS = 5_000;
const RUN_ID = "wfr_checkout";
const TRIGGER_INPUT = { orderId: "ord_1" };

type StepError = {
  type: string;
  message: string;
  stack?: string;
  retryable?: boolean;
};

interface Outcome {
  kind: string;
  ordinal?: number;
  name?: string;
  nameOccurrence?: number;
  stepKind?: string;
  output?: unknown;
  outputMode?: string;
  outputContentType?: string;
  compensable?: boolean;
  compensationMaxAttempts?: number;
  wakeAt?: string;
  signalType?: string;
  timeout?: string;
  maxSignalAge?: string;
  topic?: string;
  childWorkflowName?: string;
  input?: unknown;
  options?: unknown;
  error?: StepError;
}

interface DispatchResult extends Outcome {
  runId: string;
  workflowName: string;
  dispatchNonce: string;
  outcomes: Outcome[];
}

interface JournalRow {
  ordinal: number;
  name: string;
  nameOccurrence?: number;
  kind: "run" | "sideEffect" | "sleep" | "wait_signal" | "child";
  state: "running" | "completed" | "failed";
  output?: unknown;
  outputRef?: {
    kind?: string;
    ref?: string;
    hash: string;
    size: number;
    contentType?: string;
  };
  error?: StepError;
  wakeAt?: string;
  signalType?: string;
  consumedSignal?: unknown;
  childRunId?: string;
  compensationState?: string;
}

type WorkflowClass = new () => {
  run(trigger: WorkflowTrigger<unknown>, step: WorkflowStep): unknown;
};

/**
 * Replays one dispatch of `Main` against `journal`. `Main` is exported under
 * the stable name the envelope asks for, so a test may declare its class
 * inline; children are exported under their own names because `step.call`
 * resolves a child by its export binding.
 */
function replay(
  Main: WorkflowClass,
  journal: JournalRow[] = [],
  options: { generation?: number; children?: WorkflowClass[] } = {},
): Promise<DispatchResult> {
  const exports: Record<string, WorkflowClass> = { Checkout: Main };
  for (const child of options.children ?? []) exports[child.name] = child;
  const namespace = { ...exports, default: { workflows: { ...exports } } };
  return dispatch(namespace, {
    runId: RUN_ID,
    generation: options.generation ?? 0,
    nonce: "wfd_checkout",
    workflowName: "Checkout",
    phase: "running",
    trigger: {
      runId: RUN_ID,
      workflowName: "Checkout",
      startedAt: "2026-07-05T00:00:00.000Z",
      input: TRIGGER_INPUT,
    },
    journal,
  }) as Promise<DispatchResult>;
}

function completedRow(
  ordinal: number,
  name: string,
  output: unknown,
  kind: JournalRow["kind"] = "run",
): JournalRow {
  return { ordinal, name, nameOccurrence: 0, kind, state: "completed", output };
}

/** The whole result, quoted, so a mismatch names the dispatch that produced it. */
function show(result: DispatchResult): string {
  return JSON.stringify(result);
}

function assertRunCompleted(result: DispatchResult, output: unknown): void {
  assert.equal(result.kind, "RunCompleted", show(result));
  assert.deepEqual(result.output, output, show(result));
}

function assertRunFailed(
  result: DispatchResult,
  type: string,
  messageIncludes: string,
): StepError {
  assert.equal(result.kind, "RunFailed", show(result));
  const error = result.error;
  assert.ok(error, show(result));
  assert.equal(error.type, type, show(result));
  assert.ok(error.message.includes(messageIncludes), show(result));
  return error;
}

function assertStepCompleted(
  outcome: Outcome,
  expected: {
    ordinal: number;
    name: string;
    output: unknown;
    stepKind?: "run" | "sideEffect";
    nameOccurrence?: number;
  },
  result: DispatchResult,
): void {
  assert.equal(outcome.kind, "StepCompleted", show(result));
  assert.equal(outcome.ordinal, expected.ordinal, show(result));
  assert.equal(outcome.name, expected.name, show(result));
  assert.deepEqual(outcome.output, expected.output, show(result));
  if (expected.stepKind !== undefined) {
    assert.equal(outcome.stepKind, expected.stepKind, show(result));
  }
  if (expected.nameOccurrence !== undefined) {
    assert.equal(outcome.nameOccurrence, expected.nameOccurrence, show(result));
  }
}

// ---------------------------------------------------------------------------
// Journal replay: a committed row is the answer, and the body does not re-run.
// ---------------------------------------------------------------------------

test("a committed step row answers the step without running its body", { timeout: TEST_TIMEOUT_MS }, async () => {
  let calls = 0;
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      const loaded = await step.run("load", () => {
        calls++;
        return { orderId: "ord_2", total: 99 };
      });
      return { loaded, calls };
    }
  }

  const result = await replay(Checkout, [
    completedRow(0, "load", { orderId: "ord_1", total: 42 }),
  ]);

  assertRunCompleted(result, { loaded: { orderId: "ord_1", total: 42 }, calls: 0 });
});

test("a committed outputRef row reads through the run reader once", { timeout: TEST_TIMEOUT_MS }, async () => {
  const body = Buffer.from(JSON.stringify({ orderId: "ord_1", total: 42 }));
  let reads = 0;
  const reader = {
    workflows: {
      Checkout: {
        get(id: string) {
          assert.equal(id, RUN_ID);
          return {
            async readStepOutput(name: string, occurrence: number) {
              assert.equal(name, "load");
              assert.equal(occurrence, 0);
              reads++;
              return body;
            },
          };
        },
      },
    },
  };

  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      const ref = (await step.run("load", () => {
        throw new Error("committed row ran its body");
      })) as {
        kind: string;
        ref: string;
        hash: string;
        size: number;
        json(): Promise<unknown>;
        text(): Promise<string>;
      };
      return {
        kind: ref.kind,
        ref: ref.ref,
        hash: ref.hash,
        size: ref.size,
        json: await ref.json(),
        text: await ref.text(),
        reads,
      };
    }
  }

  const host = globalThis as { __zs_env?: () => unknown };
  host.__zs_env = () => reader;
  try {
    const result = await replay(Checkout, [
      {
        ordinal: 0,
        name: "load",
        nameOccurrence: 0,
        kind: "run",
        state: "completed",
        outputRef: {
          hash: "a".repeat(64),
          size: body.byteLength,
          contentType: "application/json",
        },
      },
    ]);

    assertRunCompleted(result, {
      kind: "workflow-step-output-ref",
      ref: `wfblob:sha256:${"a".repeat(64)}`,
      hash: "a".repeat(64),
      size: body.byteLength,
      json: { orderId: "ord_1", total: 42 },
      text: body.toString("utf8"),
      reads: 1,
    });
  } finally {
    delete host.__zs_env;
  }
});

test("a committed sideEffect row answers without running its body", { timeout: TEST_TIMEOUT_MS }, async () => {
  let calls = 0;
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      const value = await step.sideEffect("v", () => {
        calls++;
        return { value: "fresh" };
      });
      return { value, calls };
    }
  }

  const result = await replay(Checkout, [
    completedRow(0, "v", { value: "frozen" }, "sideEffect"),
  ]);

  assertRunCompleted(result, { value: { value: "frozen" }, calls: 0 });
});

test("a committed prefix replays through Promise.all without a false frontier", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      const first = await step.run("first", () => "wrong");
      const rest = await Promise.all([
        step.run("second", () => "wrong"),
        step.run("third", () => "wrong"),
      ]);
      return [first, ...rest];
    }
  }

  const result = await replay(Checkout, [
    completedRow(0, "first", "A"),
    completedRow(1, "second", "B"),
    completedRow(2, "third", "C"),
  ]);

  assertRunCompleted(result, ["A", "B", "C"]);
});

test("a committed row under a different name fails the run as nondeterministic", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run("actual", () => "wrong");
    }
  }

  const result = await replay(Checkout, [completedRow(0, "expected", "memoized")]);

  assertRunFailed(
    result,
    "NondeterministicError",
    "expected run actual#0, got run expected#0",
  );
});

test("a committed row of a different kind fails the run as nondeterministic", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.sideEffect("v", () => ({ value: "side-effect" }));
    }
  }

  const result = await replay(Checkout, [completedRow(0, "v", { value: "run" })]);

  assertRunFailed(
    result,
    "NondeterministicError",
    "expected sideEffect v#0, got run v#0",
  );
});

test("a committed failure row is rethrown into the body under its recorded type", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run("slow", () => "wrong");
    }
  }

  const result = await replay(Checkout, [
    {
      ordinal: 0,
      name: "slow",
      nameOccurrence: 0,
      kind: "run",
      state: "failed",
      error: {
        type: "WorkflowStepTimeoutError",
        message: "workflow step timed out after 5ms",
        retryable: true,
      },
    },
  ]);

  assertRunFailed(result, "WorkflowStepTimeoutError", "workflow step timed out after 5ms");
});

// ---------------------------------------------------------------------------
// Frontier: an uncommitted step runs once, and the batch is what suspends.
// ---------------------------------------------------------------------------

test("an uncommitted step runs its body once and suspends with the output", { timeout: TEST_TIMEOUT_MS }, async () => {
  let calls = 0;
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run("reserve", () => {
        calls++;
        return { reserved: true };
      });
    }
  }

  const result = await replay(Checkout);

  assert.equal(result.outcomes.length, 1, show(result));
  assertStepCompleted(
    result.outcomes[0]!,
    { ordinal: 0, name: "reserve", output: { reserved: true }, stepKind: "run" },
    result,
  );
  assert.equal(calls, 1);
});

test("an uncommitted sideEffect runs its body once and suspends with the output", { timeout: TEST_TIMEOUT_MS }, async () => {
  let calls = 0;
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.sideEffect("v", async () => {
        calls++;
        return { value: calls };
      });
    }
  }

  const result = await replay(Checkout);

  assert.equal(result.outcomes.length, 1, show(result));
  assertStepCompleted(
    result.outcomes[0]!,
    { ordinal: 0, name: "v", output: { value: 1 }, stepKind: "sideEffect" },
    result,
  );
  assert.equal(calls, 1);
});

test("a step's output mode rides its completed outcome", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Blob extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run("reserve", { output: "blob" }, () => ({ reserved: true }));
    }
  }
  const blob = await replay(Blob);
  assert.equal(blob.outputMode, "blob", show(blob));
  assert.equal(blob.outputContentType, undefined, show(blob));
  assertStepCompleted(
    blob.outcomes[0]!,
    { ordinal: 0, name: "reserve", output: { reserved: true } },
    blob,
  );

  class Ref extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run(
        "reserve",
        { output: { as: "ref", contentType: "application/json" } },
        () => ({ reserved: true }),
      );
    }
  }
  const ref = await replay(Ref);
  assert.equal(ref.outputMode, "ref", show(ref));
  assert.equal(ref.outputContentType, "application/json", show(ref));
});

test("a compensable step declares its rollback budget on the completed outcome", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run("charge", { compensate: () => {} }, () => ({ charged: true }));
    }
  }

  const result = await replay(Checkout);

  assert.equal(result.compensable, true, show(result));
  assert.equal(result.compensationMaxAttempts, 1, show(result));
});

test("Promise.all over uncommitted steps collects one ordered frontier batch", { timeout: TEST_TIMEOUT_MS }, async () => {
  const calls: string[] = [];
  let release: () => void = () => {};
  const allStarted = new Promise<void>((resolve) => {
    release = resolve;
  });

  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      const body = (label: string, output: string) => async () => {
        calls.push(label);
        if (calls.length === 3) release();
        await allStarted;
        return output;
      };
      return await Promise.all([
        step.run("a", body("a", "A")),
        step.run("b", body("b", "B")),
        step.run("c", body("c", "C")),
      ]);
    }
  }

  const result = await replay(Checkout);

  assert.equal(result.outcomes.length, 3, show(result));
  assertStepCompleted(result.outcomes[0]!, { ordinal: 0, name: "a", output: "A" }, result);
  assertStepCompleted(result.outcomes[1]!, { ordinal: 1, name: "b", output: "B" }, result);
  assertStepCompleted(result.outcomes[2]!, { ordinal: 2, name: "c", output: "C" }, result);
  assert.deepEqual(calls, ["a", "b", "c"]);
});

test("Promise.all over a committed row and an uncommitted step batches only the uncommitted one", { timeout: TEST_TIMEOUT_MS }, async () => {
  const calls: string[] = [];
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await Promise.all([
        step.run("a", () => {
          calls.push("a");
          return "wrong";
        }),
        step.run("b", async () => {
          calls.push("b");
          return "B";
        }),
      ]);
    }
  }

  const result = await replay(Checkout, [completedRow(0, "a", "A")]);

  assert.equal(result.outcomes.length, 1, show(result));
  assertStepCompleted(result.outcomes[0]!, { ordinal: 1, name: "b", output: "B" }, result);
  assert.deepEqual(calls, ["b"]);
});

test("Promise.all over a step and a sleep collects the settled sibling with the suspension", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await Promise.all([
        step.run("a", async () => {
          await Promise.resolve();
          return "A";
        }),
        step.sleep("cooldown", "1s"),
      ]);
    }
  }

  const result = await replay(Checkout);

  assert.equal(result.outcomes.length, 2, show(result));
  assertStepCompleted(result.outcomes[0]!, { ordinal: 0, name: "a", output: "A" }, result);
  const sleep = result.outcomes[1]!;
  assert.equal(sleep.kind, "Sleep", show(result));
  assert.equal(sleep.ordinal, 1, show(result));
  assert.equal(sleep.name, "cooldown", show(result));
  assert.equal(sleep.wakeAt, "1s", show(result));
});

test("steps issued in one window share a single dispatch latch", { timeout: TEST_TIMEOUT_MS }, async () => {
  let shared: { firstIsSecond: boolean; secondIsThird: boolean } | undefined;
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      const first = step.run("first", () => "first");
      const second = step.run("second", () => "second");
      const third = step.run("third", () => "third");
      shared = { firstIsSecond: first === second, secondIsThird: second === third };
      return await first;
    }
  }

  const result = await replay(Checkout);

  assert.deepEqual(shared, { firstIsSecond: true, secondIsThird: true });
  assert.equal(result.outcomes.length, 3, show(result));
  assertStepCompleted(result.outcomes[0]!, { ordinal: 0, name: "first", output: "first" }, result);
  assertStepCompleted(result.outcomes[1]!, { ordinal: 1, name: "second", output: "second" }, result);
  assertStepCompleted(result.outcomes[2]!, { ordinal: 2, name: "third", output: "third" }, result);
});

test("a step promise held across only synchronous work suspends cleanly", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      const pending = step.run("first", () => "ok");
      const syncOnly = "still synchronous";
      await pending;
      return syncOnly;
    }
  }

  const result = await replay(Checkout);

  assert.equal(result.outcomes.length, 1, show(result));
  assertStepCompleted(result.outcomes[0]!, { ordinal: 0, name: "first", output: "ok" }, result);
});

test("a bare await while a frontier is pending fails the run as nondeterministic", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      const frontier = step.run("frontier", () => "ok");
      await Promise.resolve();
      return await frontier;
    }
  }

  const result = await replay(Checkout);

  assertRunFailed(
    result,
    "NondeterministicError",
    "awaited non-step work while a frontier was pending",
  );
});

// ---------------------------------------------------------------------------
// Body guards: a dispatching body may not reach for ambient I/O or timers, and
// the guard is scoped to that body rather than to the process.
// ---------------------------------------------------------------------------

test("a body that fetches directly fails the run", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run() {
      await fetch("data:text/plain,body");
      return "unreachable";
    }
  }

  const result = await replay(Checkout);

  assertRunFailed(result, "NondeterministicError", "may not perform I/O directly");
});

test("a body that uses a timer directly fails the run", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run() {
      await new Promise((resolve) => setTimeout(resolve, 0));
      return "unreachable";
    }
  }

  const result = await replay(Checkout);

  assertRunFailed(result, "NondeterministicError", "may not use timers directly");
});

test("a body that holds a step promise across a timer fails the run", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      const pending = step.run("first", () => "ok");
      await new Promise((resolve) => setTimeout(resolve, 0));
      await pending;
      return "unreachable";
    }
  }

  const result = await replay(Checkout);

  assertRunFailed(result, "NondeterministicError", "may not use timers directly");
});

test("a body that runs a timer between committed steps fails the run", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      await step.run("first", () => "wrong");
      await new Promise((resolve) => setTimeout(resolve, 0));
      await step.run("second", () => "second");
      return "unreachable";
    }
  }

  const result = await replay(Checkout, [completedRow(0, "first", "first")]);

  assertRunFailed(result, "NondeterministicError", "may not use timers directly");
});

test("a step body may fetch across its own awaits", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run("fetch", async () => {
        const a = await (await fetch("data:text/plain,a")).text();
        const b = await (await fetch("data:text/plain,b")).text();
        return `${a}:${b}`;
      });
    }
  }

  const result = await replay(Checkout);

  assertStepCompleted(result.outcomes[0]!, { ordinal: 0, name: "fetch", output: "a:b" }, result);
});

test("a step body may use timers", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run(
        "timer",
        () => new Promise((resolve) => setTimeout(() => resolve("timer-ok"), 0)),
      );
    }
  }

  const result = await replay(Checkout);

  assertStepCompleted(result.outcomes[0]!, { ordinal: 0, name: "timer", output: "timer-ok" }, result);
});

test("fetch outside a dispatching body is untouched", { timeout: TEST_TIMEOUT_MS }, async () => {
  const response = await fetch("data:text/plain,no-store");
  assert.equal(await response.text(), "no-store");
});

test("timers outside a dispatching body are untouched", { timeout: TEST_TIMEOUT_MS }, async () => {
  const result = await new Promise((resolve) => setTimeout(() => resolve("no-store"), 0));
  assert.equal(result, "no-store");
});

// ---------------------------------------------------------------------------
// Step methods reached from inside a step body, and the terminal signals.
// ---------------------------------------------------------------------------

test("a step issued from inside a step body fails the run", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run("outer", () => step.run("inner", () => "inner"));
    }
  }

  const result = await replay(Checkout);

  assertRunFailed(result, "Error", "cannot be called from inside a step body");
  assert.equal(result.name, "outer", show(result));
});

test("a step issued after an await inside a step body fails the run", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run("outer", async () => {
        await Promise.resolve();
        return step.run("inner", () => "inner");
      });
    }
  }

  const result = await replay(Checkout);

  assertRunFailed(result, "Error", "cannot be called from inside a step body");
});

test("a step issued without a body fails the run", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await (step.run as (name: string, fn?: unknown) => Promise<unknown>)("x");
    }
  }

  const result = await replay(Checkout);

  assertRunFailed(result, "Error", "step.run requires a function body");
});

test("continueAsNew ends the dispatch with its seed input", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.continueAsNew({ generation: 1 });
    }
  }

  const result = await replay(Checkout);

  assert.equal(result.kind, "ContinueAsNew", show(result));
  assert.deepEqual(result.input, { generation: 1 }, show(result));
});

test("continueAsNew from inside a step body fails the run", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run("outer", () => step.continueAsNew({ generation: 1 }));
    }
  }

  const result = await replay(Checkout);

  assertRunFailed(result, "Error", "cannot be called from inside a step body");
});

// ---------------------------------------------------------------------------
// Sleep, signals and children: the suspensions the engine schedules against.
// ---------------------------------------------------------------------------

test("an uncommitted sleep suspends with its wake target", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Duration extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      await step.sleep("cooldown", "5m");
      return "after";
    }
  }
  const duration = await replay(Duration);
  assert.equal(duration.kind, "Sleep", show(duration));
  assert.equal(duration.wakeAt, "5m", show(duration));

  class Until extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      await step.sleepUntil("wake", new Date("2026-10-01T00:00:00.000Z"));
      return "after";
    }
  }
  const until = await replay(Until);
  assert.equal(until.kind, "Sleep", show(until));
  assert.equal(until.wakeAt, "2026-10-01T00:00:00.000Z", show(until));
});

test("an uncommitted signal wait suspends with its matching options", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.waitForSignal("approved", {
        type: "approved",
        timeout: "1h",
        maxSignalAge: "10m",
      });
    }
  }

  const result = await replay(Checkout);

  assert.equal(result.kind, "Wait", show(result));
  assert.equal(result.signalType, "approved", show(result));
  assert.equal(result.timeout, "1h", show(result));
  assert.equal(result.maxSignalAge, "10m", show(result));
});

test("a timed-out signal wait replays as its recorded failure", { timeout: TEST_TIMEOUT_MS }, async () => {
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.waitForSignal("approved", { timeout: "1h" });
    }
  }

  const result = await replay(Checkout, [
    {
      ordinal: 0,
      name: "approved",
      nameOccurrence: 0,
      kind: "wait_signal",
      state: "failed",
      error: {
        type: "WorkflowTimeoutError",
        message: "workflow signal wait timed out for approved",
        retryable: false,
      },
    },
  ]);

  assertRunFailed(result, "WorkflowTimeoutError", "workflow signal wait timed out for approved");
});

test("an uncommitted child call suspends and a committed one returns its output", { timeout: TEST_TIMEOUT_MS }, async () => {
  class EchoChildWorkflow extends Workflow<{ value: string }, { value: string }> {
    run(): { value: string } {
      return { value: "unused" };
    }
  }
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.call(EchoChildWorkflow, { value: "input" }, {
        cascade: true,
        timeout: "5m",
      });
    }
  }

  const suspended = await replay(Checkout, [], { children: [EchoChildWorkflow] });
  assert.equal(suspended.kind, "Child", show(suspended));
  assert.equal(suspended.ordinal, 0, show(suspended));
  assert.equal(suspended.name, "EchoChildWorkflow", show(suspended));
  assert.equal(suspended.childWorkflowName, "EchoChildWorkflow", show(suspended));
  assert.deepEqual(suspended.input, { value: "input" }, show(suspended));
  assert.deepEqual(suspended.options, { cascade: true, timeout: "5m" }, show(suspended));

  const completed = await replay(
    Checkout,
    [
      {
        ordinal: 0,
        name: "EchoChildWorkflow",
        nameOccurrence: 0,
        kind: "child",
        state: "completed",
        output: { value: "child-output" },
        childRunId: "run_child",
      },
    ],
    { children: [EchoChildWorkflow] },
  );
  assertRunCompleted(completed, { value: "child-output" });
});

test("a failed child row surfaces its cancellation or timeout type", { timeout: TEST_TIMEOUT_MS }, async () => {
  class EchoChildWorkflow extends Workflow<{ value: string }, { value: string }> {
    run(): { value: string } {
      return { value: "unused" };
    }
  }
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.call(EchoChildWorkflow, { value: "input" });
    }
  }
  const failedChild = (type: string, message: string): JournalRow => ({
    ordinal: 0,
    name: "EchoChildWorkflow",
    nameOccurrence: 0,
    kind: "child",
    state: "failed",
    error: { type, message, retryable: false },
  });

  const cancelled = await replay(
    Checkout,
    [failedChild("ChildCancelledError", "child workflow was cancelled")],
    { children: [EchoChildWorkflow] },
  );
  assertRunFailed(cancelled, "ChildCancelledError", "child workflow was cancelled");

  const timedOut = await replay(
    Checkout,
    [failedChild("ChildTimeoutError", "child workflow timed out")],
    { children: [EchoChildWorkflow] },
  );
  assertRunFailed(timedOut, "ChildTimeoutError", "child workflow timed out");
});

test("startMany emits one child outcome per item in issue order", { timeout: TEST_TIMEOUT_MS }, async () => {
  class EchoChildWorkflow extends Workflow<{ value: string }, { value: string }> {
    run(): { value: string } {
      return { value: "unused" };
    }
  }
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.startMany(
        EchoChildWorkflow,
        [
          { input: { value: "a" }, key: "child-a" },
          { input: { value: "b" }, key: "child-b", options: { timeout: "1m" } },
        ],
        { cascade: true },
      );
    }
  }

  const result = await replay(Checkout, [], { children: [EchoChildWorkflow] });

  assert.equal(result.outcomes.length, 2, show(result));
  const [first, second] = result.outcomes as [Outcome, Outcome];
  assert.equal(first.kind, "Child", show(result));
  assert.equal(first.ordinal, 0, show(result));
  assert.equal(first.nameOccurrence, 0, show(result));
  assert.deepEqual(first.input, { value: "a" }, show(result));
  assert.deepEqual(first.options, { cascade: true, key: "child-a" }, show(result));
  assert.equal(second.kind, "Child", show(result));
  assert.equal(second.ordinal, 1, show(result));
  assert.equal(second.nameOccurrence, 1, show(result));
  assert.deepEqual(second.input, { value: "b" }, show(result));
  assert.deepEqual(
    second.options,
    { cascade: true, timeout: "1m", key: "child-b" },
    show(result),
  );
});

// ---------------------------------------------------------------------------
// Step identity. The Rust suite pins the same keys through the real runtime;
// these bind the shape the SDK's own StepContext type promises a creator.
// ---------------------------------------------------------------------------

/** Issues one step and reports the context its body received. */
async function stepContextOf(
  issue: (step: WorkflowStep, record: (ctx: StepContext) => string) => Promise<unknown>,
  journal: JournalRow[] = [],
  generation = 0,
): Promise<StepContext> {
  let captured: StepContext | undefined;
  const record = (ctx: StepContext): string => {
    captured = ctx;
    return ctx.idempotencyKey;
  };
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await issue(step, record);
    }
  }
  await replay(Checkout, journal, { generation });
  assert.ok(captured, "step body did not receive a context");
  return captured;
}

test("a step body receives a journal-derived context", { timeout: TEST_TIMEOUT_MS }, async () => {
  const ctx = await stepContextOf((step, record) => step.run("charge", record), [], 7);

  assert.equal(ctx.runId, RUN_ID);
  assert.equal(ctx.workflowName, "Checkout");
  assert.equal(ctx.ordinal, 0);
  assert.equal(ctx.name, "charge");
  assert.equal(ctx.occurrence, 0);
  assert.equal(ctx.idempotencyKey, `step:${RUN_ID}:7:0:0`);
  assert.deepEqual(ctx.trigger.input, TRIGGER_INPUT);
  assert.equal(ctx.trigger.runId, RUN_ID);
});

test("a re-executed step body observes the same idempotency key", { timeout: TEST_TIMEOUT_MS }, async () => {
  const first = await stepContextOf((step, record) => step.run("charge", record), [], 7);
  const second = await stepContextOf((step, record) => step.run("charge", record), [], 7);

  assert.equal(first.idempotencyKey, second.idempotencyKey);
  assert.ok(first.idempotencyKey.length > 0);
});

test("a restarted generation does not reuse the key it replaces", { timeout: TEST_TIMEOUT_MS }, async () => {
  const first = await stepContextOf((step, record) => step.run("charge", record), [], 1);
  const restarted = await stepContextOf((step, record) => step.run("charge", record), [], 2);

  assert.equal(first.ordinal, restarted.ordinal);
  assert.notEqual(first.idempotencyKey, restarted.idempotencyKey);
});

test("the step ordinal counts the committed journal prefix", { timeout: TEST_TIMEOUT_MS }, async () => {
  const ctx = await stepContextOf(
    async (step, record) => {
      await step.run("reserve", () => {
        throw new Error("committed row ran its body");
      });
      return step.run("charge", record);
    },
    [completedRow(0, "reserve", "r")],
    3,
  );

  assert.equal(ctx.ordinal, 1);
  assert.equal(ctx.idempotencyKey, `step:${RUN_ID}:3:1:0`);
});

test("a sideEffect body receives the same context shape", { timeout: TEST_TIMEOUT_MS }, async () => {
  const ctx = await stepContextOf((step, record) => step.sideEffect("stamp", record), [], 9);

  assert.equal(ctx.name, "stamp");
  assert.equal(ctx.ordinal, 0);
  assert.equal(ctx.idempotencyKey, `step:${RUN_ID}:9:0:0`);
});

// ---------------------------------------------------------------------------
// Gaps. Each case below pins behaviour the dispatcher does NOT have, against a
// surface the SDK declares. They are here so the gap is visible rather than
// implied, and so closing one fails loudly at the case that documented it.
// ---------------------------------------------------------------------------

test("StepConfig.timeout does not bound a step body", { timeout: TEST_TIMEOUT_MS }, async () => {
  // `StepConfig.timeout` is declared in the SDK and read by nothing: the
  // dispatcher inspects only `compensate` and `output`. A body outliving its
  // configured timeout still commits its output. The worker's own per-job
  // execution timeout is the only bound a slow step meets.
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run(
        "slow",
        { timeout: "5ms" },
        () => new Promise((resolve) => setTimeout(() => resolve("late"), 60)),
      );
    }
  }

  const result = await replay(Checkout);

  assertStepCompleted(result.outcomes[0]!, { ordinal: 0, name: "slow", output: "late" }, result);
});

test("a failed step's recorded error carries no retryable flag", { timeout: TEST_TIMEOUT_MS }, async () => {
  // Every error class the SDK exports declares a `retryable` field, and the
  // engine's RunFailed shape carries one. The dispatcher's serializer keeps
  // type, message and stack only, so a creator error's own flag is dropped on
  // the way to the journal.
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.run("boom", () => {
        const failure = new Error("kaboom") as Error & { retryable?: boolean };
        failure.name = "WorkflowStepTimeoutError";
        failure.retryable = true;
        throw failure;
      });
    }
  }

  const result = await replay(Checkout);

  const error = assertRunFailed(result, "WorkflowStepTimeoutError", "kaboom");
  assert.equal(error.retryable, undefined, show(result));
});

test("Promise combinators over step promises are not refused", { timeout: TEST_TIMEOUT_MS }, async () => {
  // `Promise.race`, `.allSettled` and `.any` settle on the first branch, which
  // over step promises means abandoning a frontier the engine still has to
  // commit. The dispatcher installs no combinator guard, so each one resolves
  // through the shared latch instead of being refused.
  for (const method of ["race", "allSettled", "any"] as const) {
    class Checkout extends Workflow<unknown, unknown> {
      async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
        return await Promise[method]([step.run("combinator", () => "output")]);
      }
    }

    const result = await replay(Checkout);

    assertStepCompleted(
      result.outcomes[0]!,
      { ordinal: 0, name: "combinator", output: "output" },
      result,
    );
  }
});

test("startMany past its batch bound fails as nondeterministic rather than by limit", { timeout: TEST_TIMEOUT_MS }, async () => {
  // The refusal itself is a LimitExceededError, but it rejects a step promise
  // that was never observed, so the drain barrier reaches its verdict first and
  // the engine is told the body awaited non-step work. The bound holds; the
  // reason the engine records for it does not name the bound.
  class EchoChildWorkflow extends Workflow<{ value: string }, { value: string }> {
    run(): { value: string } {
      return { value: "unused" };
    }
  }
  class Checkout extends Workflow<unknown, unknown> {
    async run(_trigger: WorkflowTrigger<unknown>, step: WorkflowStep) {
      return await step.startMany(
        EchoChildWorkflow,
        Array.from({ length: 1_001 }, (_, index) => ({ input: { value: String(index) } })),
      );
    }
  }

  const result = await replay(Checkout, [], { children: [EchoChildWorkflow] });

  assertRunFailed(
    result,
    "NondeterministicError",
    "awaited non-step work outside the microtask replay boundary",
  );
});
