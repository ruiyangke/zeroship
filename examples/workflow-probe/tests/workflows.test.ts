import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { expect, inject, test } from "vitest";
import { targets, type Target } from "./targets";

type Run = { workflow: string; runId: string };
type Status = { state: string; output: unknown; error: null | { type: string; message: string; compensation: unknown } };

async function rpc<T>(target: Target, procedure: string, input: unknown): Promise<T> {
  const response = await fetch(`${target.apiUrl}/__zeroship/v1/wf.${procedure}`, {
    method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ json: input }), signal: AbortSignal.timeout(15_000),
  });
  const text = await response.text();
  expect(response.ok, `${target.name} ${procedure}: ${response.status}: ${text}`).toBe(true);
  const body = JSON.parse(text);
  expect(body).toHaveProperty("json");
  return body.json;
}

async function start(target: Target, name: string): Promise<Run> {
  const run = await rpc<Run>(target, "start", { case: name });
  if (typeof run.runId !== "string") throw new Error(`${target.name} workflow start failed: ${JSON.stringify(run)}`);
  expect(run.runId).toMatch(/^run_/);
  return run;
}

async function until(target: Target, run: Run, state: string): Promise<Status> {
  const deadline = Date.now() + 60_000;
  let status: Status;
  do {
    status = await rpc<Status>(target, "status", run);
    if (status.state === state) return status;
    if (["completed", "failed", "cancelled", "stalled", "continuedAsNew"].includes(status.state)) {
      throw new Error(`${target.name}: expected ${state}, received ${JSON.stringify(status)}`);
    }
    await sleep(100);
  } while (Date.now() < deadline);
  throw new Error(`${target.name}: did not reach ${state}: ${JSON.stringify(status)}`);
}

test("journaled steps preserve their input and derived outputs across local and deployed runs", async () => {
  const results = [];
  for (const target of targets()) {
    const result = await until(target, await start(target, "basic"), "completed");
    expect(result).toEqual({ state: "completed", error: null, output: {
      first: { label: "probe", n: 1 }, second: { n: 2 }, frozen: 42, frozenAgain: 42, steps: 4,
    } });
    results.push(result);
  }
  expect(results[0]).toEqual(results[1]);
});

test("sleep persists a suspended run before resuming the following step", async () => {
  for (const target of targets()) {
    const started = performance.now();
    const run = await start(target, "sleep");
    await until(target, run, "sleeping");
    expect((await until(target, run, "completed")).output).toEqual({ before: "before", after: "after", slept: true });
    expect(performance.now() - started).toBeGreaterThanOrEqual(20_000);
  }
});

test("signals resume an observed waiting run with the supplied payload", async () => {
  for (const target of targets()) {
    const run = await start(target, "signal");
    await until(target, run, "waiting");
    expect(await rpc(target, "signal", { ...run, token: "probe-token" })).toEqual({ signalled: true });
    expect((await until(target, run, "completed")).output).toEqual({ received: true, payload: { token: "probe-token" }, type: "probe.go" });
  }
});

test("child workflows return their output in local and deployed runs", async () => {
  for (const target of targets()) {
    const run = await start(target, "child");
    expect((await until(target, run, "completed")).output).toEqual({ child: { doubled: 42 }, parentSaw: 42 });
  }
});

test("compensation reverses the effect and preserves the original error in both tiers", async () => {
  for (const target of targets()) {
    expect(await rpc(target, "resetTrail", {})).toEqual({ reset: true });
    const result = await until(target, await start(target, "compensate"), "failed");
    const trail = await rpc(target, "trail", {});
    // Compensation is at-least-once, so the compensator may be dispatched again
    // after its effect landed; the app dedupes on `ctx.idempotencyKey`, which is
    // what keeps the trail below exact. The count of those re-runs is RECORDED,
    // never asserted: a fixed expectation would fail the run for behaviour the
    // platform documents as permitted. It is recorded ahead of the assertions so
    // a failing run keeps the value too, and into a file because vitest does not
    // surface a test's console output here -- a counter nothing can read
    // measures nothing. Only its shape is checked.
    const { redispatches } = await rpc<{ redispatches: number }>(target, "compensatorRedispatches", {});
    writeFileSync(
      join(inject("workflowArtifacts"), `compensator-redispatches-${target.name}.json`),
      `${JSON.stringify({ target: target.name, redispatches })}\n`,
    );
    expect(typeof redispatches).toBe("number");
    expect(result.error?.message).toContain("probe-intentional-failure");
    expect(trail).toEqual({ trail: "do:reserve,undo:reserve" });
    expect(result.error?.compensation).toMatchObject({ outcome: "completed" });
  }
});
