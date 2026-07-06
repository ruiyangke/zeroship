import assert from "node:assert/strict";
import { test } from "node:test";

import type { JournalEnvelope } from "../src/journal.ts";
import { createJournalStep, SuspendSignal } from "../src/journal.ts";

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
