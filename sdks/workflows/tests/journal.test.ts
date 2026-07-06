import assert from "node:assert/strict";
import { test } from "node:test";

import type { FrontierOutcome, JournalEnvelope } from "../src/journal.ts";
import {
  createJournalStep,
  getJournalFrontierDrainPromise,
  isWorkflowStepPromise,
  isJournalFrontierObserved,
  isJournalFrontierPending,
  SuspendSignal,
  withWorkflowPromiseGuards,
  WorkflowMicrotaskQuiescenceBarrier,
} from "../src/journal.ts";
import {
  NondeterministicError,
  WorkflowNestedStepError,
  WorkflowStepTimeoutError,
  WorkflowTimeoutError,
  WorkflowUnsupportedError,
} from "../src/index.ts";

const TEST_TIMEOUT_MS = 5_000;

function envelope(steps: JournalEnvelope["steps"] = []): JournalEnvelope {
  return {
    runId: "run_0123456789ABCDEFGHIJKL",
    workflowName: "Checkout",
    trigger: {
      input: { orderId: "ord_1" },
      startedAt: new Date("2026-07-05T00:00:00.000Z"),
      runId: "run_0123456789ABCDEFGHIJKL",
      workflowName: "Checkout",
    },
    steps,
  };
}

function assertSuspendSignal(err: unknown): SuspendSignal {
  assert.ok(err instanceof SuspendSignal);
  return err;
}

function assertCompletedRun(
  outcome: FrontierOutcome,
  expected: { ordinal: number; name: string; output: unknown },
): void {
  assert.equal(outcome.kind, "run");
  assert.equal(outcome.state, "completed");
  assert.equal(outcome.ordinal, expected.ordinal);
  assert.equal(outcome.name, expected.name);
  assert.deepEqual(outcome.output, expected.output);
}

function assertCompletedSideEffect(
  outcome: FrontierOutcome,
  expected: { ordinal: number; name: string; output: unknown },
): void {
  assert.equal(outcome.kind, "sideEffect");
  assert.equal(outcome.state, "completed");
  assert.equal(outcome.ordinal, expected.ordinal);
  assert.equal(outcome.name, expected.name);
  assert.deepEqual(outcome.output, expected.output);
}

async function runWithDispatcherDrain(
  fn: (step: ReturnType<typeof createJournalStep>) => unknown | Promise<unknown>,
  steps: JournalEnvelope["steps"] = [],
): Promise<unknown> {
  const quiescence = new WorkflowMicrotaskQuiescenceBarrier();
  const step = createJournalStep(envelope(steps), quiescence);
  const blockedByNonStepWork = quiescence.waitUntilBlocked(() =>
    isJournalFrontierObserved(step)
  );
  let outputPromise: Promise<unknown>;
  try {
    outputPromise = Promise.resolve(fn(step));
  } catch (e) {
    quiescence.stop();
    blockedByNonStepWork.catch(() => {});
    getJournalFrontierDrainPromise(step)?.catch(() => {});
    throw e;
  }
  outputPromise.then(
    () => quiescence.stop(),
    () => quiescence.stop(),
  );
  outputPromise.catch(() => {});

  const frontierDrainPromise = getJournalFrontierDrainPromise(step);
  if (frontierDrainPromise) {
    await Promise.race([
      frontierDrainPromise,
      outputPromise.then(
        () => {
          throw new NondeterministicError("workflow completed while a frontier was pending");
        },
        (error) => {
          throw error;
        },
      ),
      blockedByNonStepWork,
    ]);
  }

  const output = await Promise.race([outputPromise, blockedByNonStepWork]);
  if (isJournalFrontierPending(step)) {
    throw new NondeterministicError("workflow completed while a frontier was pending");
  }
  return output;
}

test("step.run journal hit returns memoized output without running fn", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope([
    {
      ordinal: 0,
      name: "load",
      nameOccurrence: 0,
      kind: "run",
      state: "completed",
      output: { orderId: "ord_1", total: 42 },
    },
  ]));
  let calls = 0;

  const result = await step.run("load", () => {
    calls++;
    return { orderId: "ord_2", total: 99 };
  });

  assert.equal(calls, 0);
  assert.deepEqual(result, { orderId: "ord_1", total: 42 });
});

test("first step.run miss runs once, captures output, then suspends", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope());
  let calls = 0;

  await assert.rejects(
    async () => step.run("reserve", () => {
      calls++;
      return { reserved: true };
    }),
    (err) => {
      const signal = assertSuspendSignal(err);
      assert.equal(signal.outcomes.length, 1);
      assert.equal(signal.outcome, signal.outcomes[0]);
      assertCompletedRun(signal.outcomes[0]!, {
        ordinal: 0,
        name: "reserve",
        output: { reserved: true },
      });
      return true;
    },
  );
  assert.equal(calls, 1);
});

test("first step.sideEffect miss runs once, captures output, then suspends", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope());
  let calls = 0;

  await assert.rejects(
    async () => step.sideEffect("v", async () => {
      calls++;
      return { value: calls };
    }),
    (err) => {
      const signal = assertSuspendSignal(err);
      assert.equal(signal.outcomes.length, 1);
      assertCompletedSideEffect(signal.outcomes[0]!, {
        ordinal: 0,
        name: "v",
        output: { value: 1 },
      });
      return true;
    },
  );
  assert.equal(calls, 1);
});

test("step.sideEffect journal hit returns frozen output without running fn", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope([
    {
      ordinal: 0,
      name: "v",
      nameOccurrence: 0,
      kind: "sideEffect",
      state: "completed",
      output: { value: "frozen" },
    },
  ]));
  let calls = 0;

  const result = await step.sideEffect("v", () => {
    calls++;
    return { value: "fresh" };
  });

  assert.equal(calls, 0);
  assert.deepEqual(result, { value: "frozen" });
});

test("step.sideEffect kind divergence throws NondeterministicError", { timeout: TEST_TIMEOUT_MS }, () => {
  const step = createJournalStep(envelope([
    {
      ordinal: 0,
      name: "v",
      nameOccurrence: 0,
      kind: "run",
      state: "completed",
      output: { value: "run" },
    },
  ]));

  assert.throws(
    () => step.sideEffect("v", () => ({ value: "side-effect" })),
    NondeterministicError,
  );
});

test("Promise.all over step misses collects one concurrent frontier batch", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope());
  const calls: string[] = [];
  let resolveAllStarted: () => void = () => {};
  const allStarted = new Promise<void>((resolve) => {
    resolveAllStarted = resolve;
  });

  await assert.rejects(
    () => Promise.all([
      step.run("a", async () => {
        calls.push("a");
        if (calls.length === 3) resolveAllStarted();
        await allStarted;
        return "A";
      }),
      step.run("b", async () => {
        calls.push("b");
        if (calls.length === 3) resolveAllStarted();
        await allStarted;
        return "B";
      }),
      step.run("c", async () => {
        calls.push("c");
        if (calls.length === 3) resolveAllStarted();
        await allStarted;
        return "C";
      }),
    ]),
    (err) => {
      const signal = assertSuspendSignal(err);
      assert.equal(signal.outcomes.length, 3);
      assertCompletedRun(signal.outcomes[0]!, { ordinal: 0, name: "a", output: "A" });
      assertCompletedRun(signal.outcomes[1]!, { ordinal: 1, name: "b", output: "B" });
      assertCompletedRun(signal.outcomes[2]!, { ordinal: 2, name: "c", output: "C" });
      return true;
    },
  );
  assert.deepEqual(calls, ["a", "b", "c"]);
});

test("Promise.all over a replayed hit and miss resolves the hit and batches the miss", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope([
    {
      ordinal: 0,
      name: "a",
      nameOccurrence: 0,
      kind: "run",
      state: "completed",
      output: "A",
    },
  ]));
  const calls: string[] = [];

  await assert.rejects(
    () => Promise.all([
      step.run("a", () => {
        calls.push("a");
        return "wrong";
      }),
      step.run("b", async () => {
        calls.push("b");
        return "B";
      }),
    ]),
    (err) => {
      const signal = assertSuspendSignal(err);
      assert.equal(signal.outcomes.length, 1);
      assertCompletedRun(signal.outcomes[0]!, { ordinal: 1, name: "b", output: "B" });
      return true;
    },
  );
  assert.deepEqual(calls, ["b"]);
});

test("Promise.all over a run and sleep collects settled siblings plus the suspension", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope());
  let calls = 0;

  await assert.rejects(
    () => Promise.all([
      step.run("a", async () => {
        calls++;
        await Promise.resolve();
        return "A";
      }),
      step.sleep("cooldown", "1s"),
    ]),
    (err) => {
      const signal = assertSuspendSignal(err);
      assert.equal(signal.outcomes.length, 2);
      assertCompletedRun(signal.outcomes[0]!, { ordinal: 0, name: "a", output: "A" });
      const sleep = signal.outcomes[1]!;
      assert.equal(sleep.kind, "sleep");
      assert.equal(sleep.ordinal, 1);
      assert.equal(sleep.name, "cooldown");
      assert.equal(sleep.state, "running");
      assert.equal(sleep.wakeAt, "1s");
      return true;
    },
  );
  assert.equal(calls, 1);
});

test("bare workflow-body await while a frontier is pending throws NondeterministicError", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope());

  await assert.rejects(
    async () => {
      const frontier = step.run("frontier", () => "ok");
      await Promise.resolve();
      await frontier;
    },
    NondeterministicError,
  );
});

test("dispatcher drain does not mask stored step promise then bare await", { timeout: TEST_TIMEOUT_MS }, async () => {
  await assert.rejects(
    () => runWithDispatcherDrain(async (step) => {
      const pending = step.run("first", () => "ok");
      await new Promise((resolve) => setTimeout(resolve, 0));
      await pending;
      return { unreachable: true };
    }),
    NondeterministicError,
  );
});

test("dispatcher drain catches bare macrotask replay before any frontier exists", { timeout: TEST_TIMEOUT_MS }, async () => {
  await assert.rejects(
    () => runWithDispatcherDrain(async (step) => {
      await step.run("first", () => "wrong");
      await new Promise((resolve) => setTimeout(resolve, 0));
      await step.run("second", () => "second");
      return { unreachable: true };
    }, [
      {
        ordinal: 0,
        name: "first",
        nameOccurrence: 0,
        kind: "run",
        state: "completed",
        output: "first",
      },
    ]),
    NondeterministicError,
  );
});

test("stored step promise awaited later without bare await suspends cleanly", { timeout: TEST_TIMEOUT_MS }, async () => {
  await assert.rejects(
    () => runWithDispatcherDrain(async (step) => {
      const pending = step.run("first", () => "ok");
      const syncOnly = "still synchronous";
      await pending;
      return syncOnly;
    }),
    (err) => {
      const signal = assertSuspendSignal(err);
      assert.equal(signal.outcomes.length, 1);
      assertCompletedRun(signal.outcomes[0]!, { ordinal: 0, name: "first", output: "ok" });
      return true;
    },
  );
});

test("journal name divergence throws NondeterministicError", { timeout: TEST_TIMEOUT_MS }, () => {
  const step = createJournalStep(envelope([
    {
      ordinal: 0,
      name: "expected",
      nameOccurrence: 0,
      kind: "run",
      state: "completed",
      output: "memoized",
    },
  ]));

  assert.throws(
    () => step.run("actual", () => "wrong"),
    NondeterministicError,
  );
});

test("memoized multi-step replay with Promise.all has no nondeterminism false positive", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope([
    {
      ordinal: 0,
      name: "first",
      nameOccurrence: 0,
      kind: "run",
      state: "completed",
      output: "A",
    },
    {
      ordinal: 1,
      name: "second",
      nameOccurrence: 0,
      kind: "run",
      state: "completed",
      output: "B",
    },
    {
      ordinal: 2,
      name: "third",
      nameOccurrence: 0,
      kind: "run",
      state: "completed",
      output: "C",
    },
  ]));

  const first = await step.run("first", () => "wrong");
  const rest = await Promise.all([
    step.run("second", () => "wrong"),
    step.run("third", () => "wrong"),
  ]);

  assert.deepEqual([first, ...rest], ["A", "B", "C"]);
});

test("concurrent misses share a branded dispatch latch while each callback runs once", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope());
  const calls: string[] = [];

  const first = step.run("first", () => {
    calls.push("first");
    return "first";
  });
  const second = step.run("second", () => {
    calls.push("second");
    return "second";
  });
  const third = step.run("third", () => {
    calls.push("third");
    return "third";
  });

  assert.equal(isWorkflowStepPromise(first), true);
  assert.equal(isWorkflowStepPromise(second), true);
  assert.equal(second, third);
  assert.equal(first, second);
  assert.deepEqual(calls, ["first", "second", "third"]);

  await assert.rejects(first, (err) => {
    const signal = assertSuspendSignal(err);
    assert.equal(signal.outcomes.length, 3);
    assertCompletedRun(signal.outcomes[0]!, { ordinal: 0, name: "first", output: "first" });
    assertCompletedRun(signal.outcomes[1]!, { ordinal: 1, name: "second", output: "second" });
    assertCompletedRun(signal.outcomes[2]!, { ordinal: 2, name: "third", output: "third" });
    return true;
  });
});

test("Promise.race over step promises throws WorkflowUnsupportedError synchronously", { timeout: TEST_TIMEOUT_MS }, () => {
  const step = createJournalStep(envelope());
  assert.throws(
    () =>
      withWorkflowPromiseGuards(() =>
        Promise.race([
          step.run("race", () => "nope"),
        ])
      ),
    WorkflowUnsupportedError,
  );

  const passthrough = withWorkflowPromiseGuards(() =>
    Promise.race([Promise.resolve("ok")])
  );
  assert.equal(isWorkflowStepPromise(passthrough), false);
});

test("Promise.allSettled and Promise.any over step promises are unsupported", { timeout: TEST_TIMEOUT_MS }, () => {
  for (const method of ["allSettled", "any"] as const) {
    const step = createJournalStep(envelope());
    assert.throws(
      () =>
        withWorkflowPromiseGuards(() =>
          Promise[method]([
            step.run(method, () => "nope"),
          ])
        ),
      WorkflowUnsupportedError,
    );
  }
});

test("nested step call inside a frontier callback throws WorkflowNestedStepError", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope());
  await assert.rejects(
    () =>
      step.run("outer", () =>
        step.run("inner", () => "inner")
      ),
    WorkflowNestedStepError,
  );
});

test("nested step call after an await inside a frontier callback throws WorkflowNestedStepError", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope());
  await assert.rejects(
    () =>
      step.run("outer", async () => {
        await Promise.resolve();
        return step.run("inner", () => "inner");
      }),
    WorkflowNestedStepError,
  );
});

test("step.run timeout records a retryable WorkflowStepTimeoutError", { timeout: TEST_TIMEOUT_MS }, async () => {
  const step = createJournalStep(envelope());
  await assert.rejects(
    () =>
      step.run("slow", { timeout: "5ms" }, () =>
        new Promise<string>(() => {})
      ),
    (err) => {
      const signal = assertSuspendSignal(err);
      assert.equal(signal.outcomes.length, 1);
      const outcome = signal.outcomes[0]!;
      assert.equal(outcome.kind, "run");
      assert.equal(outcome.state, "failed");
      assert.equal(outcome.error.type, "WorkflowStepTimeoutError");
      assert.equal(outcome.error.retryable, true);
      return true;
    },
  );

  const replay = createJournalStep(envelope([
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
  ]));
  await assert.rejects(
    () => replay.run("slow", () => "wrong"),
    WorkflowStepTimeoutError,
  );
});

test("sleep and waitForSignal misses suspend and timeout replays throw WorkflowTimeoutError", { timeout: TEST_TIMEOUT_MS }, async () => {
  const sleepStep = createJournalStep(envelope());
  await assert.rejects(
    async () => sleepStep.sleep("cooldown", "5m"),
    (err) => {
      const signal = assertSuspendSignal(err);
      assert.equal(signal.outcomes.length, 1);
      assert.equal(signal.outcome.kind, "sleep");
      assert.equal(signal.outcome.wakeAt, "5m");
      return true;
    },
  );

  const signalStep = createJournalStep(envelope());
  await assert.rejects(
    async () => signalStep.waitForSignal("approved", {
      type: "approved",
      timeout: "1h",
      maxSignalAge: "10m",
    }),
    (err) => {
      const signal = assertSuspendSignal(err);
      assert.equal(signal.outcomes.length, 1);
      assert.equal(signal.outcome.kind, "wait_signal");
      assert.equal(signal.outcome.signalType, "approved");
      assert.equal(signal.outcome.timeout, "1h");
      return true;
    },
  );

  const timedOut = createJournalStep(envelope([
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
  ]));
  await assert.rejects(
    () => timedOut.waitForSignal("approved", { timeout: "1h" }),
    WorkflowTimeoutError,
  );
});
