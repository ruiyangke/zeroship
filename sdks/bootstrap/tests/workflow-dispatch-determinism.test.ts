import assert from "node:assert/strict";
import { describe, test } from "node:test";

// Importing the dispatcher installs `globalThis.__zsWorkflowDispatch`.
import "../src/dispatcher.js";

type WorkflowDispatch = (
  userNamespace: unknown,
  envelope: unknown,
  ctx?: unknown,
) => Promise<Record<string, unknown>>;

type DispatchResult = Record<string, unknown> & {
  error?: { type?: string; message?: string };
  outcomes?: Array<Record<string, unknown> & { error?: { type?: string; message?: string } }>;
};

const TEST_TIMEOUT_MS = 5_000;

const g = globalThis as typeof globalThis & {
  __zsWorkflowDispatch: WorkflowDispatch;
};

function envelope(journal: Array<Record<string, unknown>> = []): Record<string, unknown> {
  return {
    runId: "run_0123456789ABCDEFGHIJKL",
    nonce: "nonce_1",
    workflowName: "WorkflowUnderTest",
    input: { orderId: "ord_1" },
    trigger: {
      input: { orderId: "ord_1" },
      startedAt: "2026-07-05T00:00:00.000Z",
      runId: "run_0123456789ABCDEFGHIJKL",
      workflowName: "WorkflowUnderTest",
    },
    journal,
  };
}

function completedRun(
  ordinal: number,
  name: string,
  output: unknown,
): Record<string, unknown> {
  return {
    ordinal,
    name,
    nameOccurrence: 0,
    kind: "run",
    state: "completed",
    output,
  };
}

function completedSideEffect(
  ordinal: number,
  name: string,
  output: unknown,
): Record<string, unknown> {
  return {
    ordinal,
    name,
    nameOccurrence: 0,
    kind: "sideEffect",
    state: "completed",
    output,
  };
}

function macrotask(): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, 0);
  });
}

async function dispatch(
  WorkflowUnderTest: new () => { run: (trigger: unknown, step: unknown) => unknown },
  journal: Array<Record<string, unknown>> = [],
): Promise<DispatchResult> {
  return g.__zsWorkflowDispatch({ WorkflowUnderTest }, envelope(journal), {});
}

function assertNondeterministicRunFailed(result: DispatchResult): void {
  assert.equal(result.kind, "RunFailed");
  assert.equal(result.error?.type, "NondeterministicError");
  assert.match(
    result.error?.message ?? "",
    /non-step work|nondeterministic|frontier was pending|I\/O directly|timers directly/,
  );
  assert.deepEqual(result.outcomes?.map((outcome) => outcome.kind), ["RunFailed"]);
}

describe("__zsWorkflowDispatch workflow determinism guard", () => {
  test("fails an initial dispatch that parks on a macrotask before awaiting a pending step", { timeout: TEST_TIMEOUT_MS }, async () => {
    class WorkflowUnderTest {
      async run(_trigger: unknown, step: { run<T>(name: string, fn: () => T): Promise<T> }) {
        const pending = step.run("first", () => "first-output");
        await macrotask();
        await pending;
        return { unreachable: true };
      }
    }

    const result = await dispatch(WorkflowUnderTest);

    assertNondeterministicRunFailed(result);
  });

  test("fails a replay dispatch parked on a macrotask before any new frontier exists", { timeout: TEST_TIMEOUT_MS }, async () => {
    class WorkflowUnderTest {
      async run(_trigger: unknown, step: { run<T>(name: string, fn: () => T): Promise<T> }) {
        await step.run("first", () => "wrong");
        await macrotask();
        await step.run("second", () => "second-output");
        return { unreachable: true };
      }
    }

    const result = await dispatch(WorkflowUnderTest, [
      completedRun(0, "first", "first-output"),
    ]);

    assertNondeterministicRunFailed(result);
  });

  test("allows normal replay, concurrent step frontier, and sleep suspension", { timeout: TEST_TIMEOUT_MS }, async () => {
    class CompleteReplayWorkflow {
      async run(_trigger: unknown, step: { run<T>(name: string, fn: () => T): Promise<T> }) {
        const first = await step.run("first", () => "wrong");
        const rest = await Promise.all([
          step.run("second", () => "wrong"),
          step.run("third", () => "wrong"),
        ]);
        return [first, ...rest];
      }
    }

    class ConcurrentFrontierWorkflow {
      async run(_trigger: unknown, step: { run<T>(name: string, fn: () => T): Promise<T> }) {
        const first = await step.run("first", () => "wrong");
        const rest = await Promise.all([
          step.run("second", () => `${first}:second`),
          step.run("third", () => `${first}:third`),
        ]);
        return rest;
      }
    }

    class SleepWorkflow {
      async run(
        _trigger: unknown,
        step: {
          run<T>(name: string, fn: () => T): Promise<T>;
          sleep(name: string, duration: string): Promise<void>;
        },
      ) {
        await step.run("first", () => "wrong");
        await step.sleep("cooldown", "1s");
        return { unreachable: true };
      }
    }

    const completed = await dispatch(CompleteReplayWorkflow, [
      completedRun(0, "first", "A"),
      completedRun(1, "second", "B"),
      completedRun(2, "third", "C"),
    ]);
    assert.equal(completed.kind, "RunCompleted");
    assert.deepEqual(completed.output, ["A", "B", "C"]);

    const concurrent = await dispatch(ConcurrentFrontierWorkflow, [
      completedRun(0, "first", "A"),
    ]);
    assert.deepEqual(
      concurrent.outcomes?.map((outcome) => ({
        kind: outcome.kind,
        name: outcome.name,
        output: outcome.output,
      })),
      [
        { kind: "StepCompleted", name: "second", output: "A:second" },
        { kind: "StepCompleted", name: "third", output: "A:third" },
      ],
    );

    const sleep = await dispatch(SleepWorkflow, [
      completedRun(0, "first", "A"),
    ]);
    assert.equal(sleep.kind, "Sleep");
    assert.equal(sleep.name, "cooldown");
    assert.equal(sleep.wakeAt, "1s");
  });

  test("sideEffect journals once and replays without running the callback", { timeout: TEST_TIMEOUT_MS }, async () => {
    let calls = 0;

    class SideEffectWorkflow {
      async run(
        _trigger: unknown,
        step: {
          sideEffect<T>(name: string, fn: () => T | Promise<T>): Promise<T>;
          run<T>(name: string, fn: () => T): Promise<T>;
        },
      ) {
        const frozen = await step.sideEffect("v", async () => {
          calls++;
          return { value: `fresh-${calls}` };
        });
        const after = await step.run("after", () => ({ frozen }));
        return { frozen, after };
      }
    }

    const first = await dispatch(SideEffectWorkflow);
    assert.equal(calls, 1);
    assert.equal(first.kind, "StepCompleted");
    assert.equal(first.stepKind, "sideEffect");
    assert.equal(first.name, "v");
    assert.deepEqual(first.output, { value: "fresh-1" });

    const replay = await dispatch(SideEffectWorkflow, [
      completedSideEffect(0, "v", { value: "frozen" }),
    ]);
    assert.equal(calls, 1);
    assert.equal(replay.kind, "StepCompleted");
    assert.equal(replay.stepKind, "run");
    assert.equal(replay.name, "after");
    assert.deepEqual(replay.output, { frozen: { value: "frozen" } });
  });

  test("sideEffect kind divergence fails with NondeterministicError", { timeout: TEST_TIMEOUT_MS }, async () => {
    class SideEffectWorkflow {
      async run(
        _trigger: unknown,
        step: { sideEffect<T>(name: string, fn: () => T): Promise<T> },
      ) {
        return await step.sideEffect("v", () => "wrong");
      }
    }

    const result = await dispatch(SideEffectWorkflow, [
      completedRun(0, "v", "run-output"),
    ]);

    assert.equal(result.kind, "RunFailed");
    assert.equal(result.error?.type, "NondeterministicError");
    assert.match(result.error?.message ?? "", /expected sideEffect v#0, got run v#0/);
  });

  test("rejects bare workflow-body fetch with NondeterministicError", { timeout: TEST_TIMEOUT_MS }, async () => {
    class BodyFetchWorkflow {
      async run() {
        await fetch("data:text/plain,body");
        return { unreachable: true };
      }
    }

    const result = await dispatch(BodyFetchWorkflow);

    assert.equal(result.kind, "RunFailed");
    assert.equal(result.error?.type, "NondeterministicError");
    assert.match(result.error?.message ?? "", /I\/O directly/);
  });

  test("allows fetch inside a step callback", { timeout: TEST_TIMEOUT_MS }, async () => {
    class StepFetchWorkflow {
      async run(
        _trigger: unknown,
        step: { run<T>(name: string, fn: () => T | Promise<T>): Promise<T> },
      ) {
        return await step.run("fetch", async () => {
          const response = await fetch("data:text/plain,step");
          const a = await response.text();
          const b = await (await fetch("data:text/plain,again")).text();
          return `${a}:${b}`;
        });
      }
    }

    const result = await dispatch(StepFetchWorkflow);

    assert.equal(result.kind, "StepCompleted");
    assert.equal(result.name, "fetch");
    assert.equal(result.output, "step:again");
  });

  test("leaves fetch outside workflow dispatch untouched", { timeout: TEST_TIMEOUT_MS }, async () => {
    const response = await fetch("data:text/plain,no-store");
    assert.equal(await response.text(), "no-store");
  });

  test("rejects bare workflow-body timers with NondeterministicError", { timeout: TEST_TIMEOUT_MS }, async () => {
    class BodyTimerWorkflow {
      async run() {
        await new Promise((resolve) => setTimeout(resolve, 0));
        return { unreachable: true };
      }
    }

    const result = await dispatch(BodyTimerWorkflow);

    assert.equal(result.kind, "RunFailed");
    assert.equal(result.error?.type, "NondeterministicError");
    assert.match(result.error?.message ?? "", /timers directly/);
  });

  test("allows timers inside a step callback", { timeout: TEST_TIMEOUT_MS }, async () => {
    class StepTimerWorkflow {
      async run(
        _trigger: unknown,
        step: { run<T>(name: string, fn: () => T | Promise<T>): Promise<T> },
      ) {
        return await step.run("timer", () =>
          new Promise((resolve) => setTimeout(() => resolve("timer-ok"), 0))
        );
      }
    }

    const result = await dispatch(StepTimerWorkflow);

    assert.equal(result.kind, "StepCompleted");
    assert.equal(result.name, "timer");
    assert.equal(result.output, "timer-ok");
  });
});
