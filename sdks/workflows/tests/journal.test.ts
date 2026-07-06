import assert from "node:assert/strict";
import { test } from "node:test";

import type { JournalEnvelope } from "../src/journal.ts";
import {
  createJournalStep,
  isWorkflowStepPromise,
  SuspendSignal,
  withWorkflowPromiseGuards,
} from "../src/journal.ts";
import {
  WorkflowNestedStepError,
  WorkflowStepTimeoutError,
  WorkflowUnsupportedError,
} from "../src/index.ts";

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

test("step.run journal hit returns memoized output without running fn", async () => {
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

test("first step.run miss runs once, captures output, then suspends", async () => {
  const step = createJournalStep(envelope());
  let calls = 0;

  await assert.rejects(
    async () => step.run("reserve", () => {
      calls++;
      return { reserved: true };
    }),
    (err) => {
      assert.ok(err instanceof SuspendSignal);
      assert.equal(err.outcome.kind, "run");
      assert.equal(err.outcome.state, "completed");
      assert.equal(err.outcome.ordinal, 0);
      assert.equal(err.outcome.name, "reserve");
      assert.deepEqual(err.outcome.output, { reserved: true });
      return true;
    },
  );
  assert.equal(calls, 1);
});

test("Promise.all over step misses runs one frontier per dispatch", async () => {
  const firstDispatch = createJournalStep(envelope());
  const firstCalls: string[] = [];

  await assert.rejects(
    () => Promise.all([
      firstDispatch.run("a", async () => {
        firstCalls.push("a");
        return "A";
      }),
      firstDispatch.run("b", async () => {
        firstCalls.push("b");
        return "B";
      }),
    ]),
    (err) => {
      assert.ok(err instanceof SuspendSignal);
      assert.equal(err.outcome.kind, "run");
      assert.equal(err.outcome.ordinal, 0);
      assert.equal(err.outcome.state, "completed");
      assert.equal(err.outcome.output, "A");
      return true;
    },
  );
  assert.deepEqual(firstCalls, ["a"]);

  const secondDispatch = createJournalStep(envelope([
    {
      ordinal: 0,
      name: "a",
      nameOccurrence: 0,
      kind: "run",
      state: "completed",
      output: "A",
    },
  ]));
  const secondCalls: string[] = [];

  await assert.rejects(
    () => Promise.all([
      secondDispatch.run("a", () => {
        secondCalls.push("a");
        return "wrong";
      }),
      secondDispatch.run("b", async () => {
        secondCalls.push("b");
        return "B";
      }),
    ]),
    (err) => {
      assert.ok(err instanceof SuspendSignal);
      assert.equal(err.outcome.kind, "run");
      assert.equal(err.outcome.ordinal, 1);
      assert.equal(err.outcome.state, "completed");
      assert.equal(err.outcome.output, "B");
      return true;
    },
  );
  assert.deepEqual(secondCalls, ["b"]);
});

test("second concurrent miss returns the same never-settling latch without running callback", async () => {
  const step = createJournalStep(envelope());
  let firstResolve: (value: string) => void = () => {};
  let secondCalls = 0;
  let thirdCalls = 0;

  const first = step.run("first", () =>
    new Promise<string>((resolve) => {
      firstResolve = resolve;
    })
  );
  const second = step.run("second", () => {
    secondCalls++;
    return "second";
  });
  const third = step.run("third", () => {
    thirdCalls++;
    return "third";
  });

  assert.equal(isWorkflowStepPromise(first), true);
  assert.equal(isWorkflowStepPromise(second), true);
  assert.equal(second, third);
  assert.equal(secondCalls, 0);
  assert.equal(thirdCalls, 0);

  firstResolve("first");
  await assert.rejects(first, (err) => {
    assert.ok(err instanceof SuspendSignal);
    assert.equal(err.outcome.kind, "run");
    assert.equal(err.outcome.output, "first");
    return true;
  });
});

test("Promise.race over step promises throws WorkflowUnsupportedError synchronously", () => {
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

test("Promise.allSettled and Promise.any over step promises are unsupported", () => {
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

test("nested step call inside a frontier callback throws WorkflowNestedStepError", async () => {
  const step = createJournalStep(envelope());
  await assert.rejects(
    () =>
      step.run("outer", () =>
        step.run("inner", () => "inner")
      ),
    WorkflowNestedStepError,
  );
});

test("nested step call after an await inside a frontier callback throws WorkflowNestedStepError", async () => {
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

test("step.run timeout records a retryable WorkflowStepTimeoutError", async () => {
  const step = createJournalStep(envelope());
  await assert.rejects(
    () =>
      step.run("slow", { timeout: "5ms" }, () =>
        new Promise<string>(() => {})
      ),
    (err) => {
      assert.ok(err instanceof SuspendSignal);
      assert.equal(err.outcome.kind, "run");
      assert.equal(err.outcome.state, "failed");
      assert.equal(err.outcome.error.type, "WorkflowStepTimeoutError");
      assert.equal(err.outcome.error.retryable, true);
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

test("sleep and waitForSignal misses suspend without a thrown SignalTimeout class", async () => {
  const sleepStep = createJournalStep(envelope());
  await assert.rejects(
    async () => sleepStep.sleep("cooldown", "5m"),
    (err) => {
      assert.ok(err instanceof SuspendSignal);
      assert.equal(err.outcome.kind, "sleep");
      assert.equal(err.outcome.wakeAt, "5m");
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
      assert.ok(err instanceof SuspendSignal);
      assert.equal(err.outcome.kind, "wait_signal");
      assert.equal(err.outcome.signalType, "approved");
      assert.equal(err.outcome.timeout, "1h");
      return true;
    },
  );

  const timedOut = createJournalStep(envelope([
    {
      ordinal: 0,
      name: "approved",
      nameOccurrence: 0,
      kind: "wait_signal",
      state: "completed",
      output: null,
    },
  ]));
  assert.equal(await timedOut.waitForSignal("approved", { timeout: "1h" }), null);
});
