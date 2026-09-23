"use server";

// The example-owned tests exercise these workflows locally and deployed.
// Results are deterministic apart from run identity and elapsed time.

import { query, mutation } from "@zeroship/rpc/server";
import { env } from "zeroship";
import { Workflow, type WorkflowStep, type WorkflowTrigger } from "@zeroship/workflows";

// `@zeroship/types` declares `env` as `[key: string]: unknown`, so there is no
// `env.workflows` type today (checked: packages/types/zeroship.d.ts has no
// `workflows` mention). The cast below is the shape `env.workflows` exposes;
// keeping it in one place makes the missing declaration obvious
// rather than scattering `as any` through the handlers.
interface ProbeStatusOutputRef {
  readonly kind: "ref";
  readonly ref: string;
  readonly hash: string;
  readonly size: number;
  readonly contentType?: string;
}
interface ProbeRun {
  readonly id: string;
  status(): Promise<{ state: string; output?: ProbeStatusOutputRef; error?: unknown }>;
  readOutput(): Promise<Uint8Array>;
  signal(opts: { type: string; payload?: unknown }): Promise<void>;
  cancel(opts?: { mode?: "abort" | "compensate" }): Promise<void>;
}
interface ProbeWorkflowHandle {
  start(opts: { input?: unknown; key?: string; onConflict?: string }): Promise<ProbeRun>;
  get(runId: string): ProbeRun;
}
const workflows = (env as unknown as { workflows: Record<string, ProbeWorkflowHandle> })
  .workflows;

interface ProbeKv {
  get(key: string): Promise<string | null>;
  set(key: string, value: string, opts?: { ttlMs?: number }): Promise<{ ok: true }>;
  setIfAbsent(
    key: string,
    value: string,
    opts?: { ttlMs?: number },
  ): Promise<{ stored: boolean }>;
  incr(key: string, opts?: { by?: number; ttlMs?: number }): Promise<number>;
  delete(key: string): Promise<{ deleted: boolean }>;
}
const kv = (env as unknown as { kv: ProbeKv }).kv;

// The compensation trail is the only workflow-internal effect a creator can
// observe from outside the run: `status()` returns state/output/error and says
// nothing about whether a compensator fired. Writing the trail to kv gives the
// harness a side channel that exists on both sides (redb in dev, Redis
// deployed) and is already proven identical across them by
// examples/kv-dashboard/tests/rpc.test.ts, so a difference here is a workflow
// difference and not a kv one.
const TRAIL_KEY = "wfprobe:trail";
// Compensation is at-least-once: a compensator may run again after a crash, a
// retry or a lease handoff, and the platform mints `ctx.idempotencyKey` so the
// undo effect can dedupe on it. A
// key under this prefix means that occurrence's undo has already landed.
const COMPENSATED_PREFIX = "wfprobe:compensated:";
// The claim expires, so a compensated run does not leave a key behind for the
// life of the store. It outlives any re-dispatch of the run that wrote it, and
// expiry costs at most a repeated undo, which is the outcome the platform
// already permits.
const COMPENSATED_CLAIM_TTL_MS = 3_600_000;
// A deduped re-run is counted rather than discarded. The trail stays exactly
// what the workflow did once, and this counter is the creator-visible measure
// of how often the platform re-dispatches a compensator that already
// succeeded; the acceptance test records it per run.
const REDISPATCH_KEY = "wfprobe:compensator-redispatches";

async function appendTrail(entry: string): Promise<void> {
  const current = (await kv.get(TRAIL_KEY)) ?? "";
  await kv.set(TRAIL_KEY, current === "" ? entry : `${current},${entry}`);
}

/// Run `effect` once per compensator occurrence, counting the re-dispatches
/// that find it already done.
///
/// The claim is written AFTER the effect, never before. A claim therefore means
/// the undo landed, so losing the isolate between the two costs a repeated undo
/// -- which the platform permits and the claim absorbs -- instead of a skipped
/// undo that the run would still report as a completed rollback.
async function compensateOnce(
  idempotencyKey: string,
  effect: () => Promise<void>,
): Promise<void> {
  const claim = `${COMPENSATED_PREFIX}${idempotencyKey}`;
  if ((await kv.get(claim)) !== null) {
    await kv.incr(REDISPATCH_KEY);
    return;
  }
  await effect();
  // A claim already present here belongs to a dispatch that ran concurrently,
  // which the read above cannot see. Counting it keeps the counter a measure of
  // every re-dispatch that met a landed effect.
  const { stored } = await kv.setIfAbsent(claim, "1", {
    ttlMs: COMPENSATED_CLAIM_TTL_MS,
  });
  if (!stored) await kv.incr(REDISPATCH_KEY);
}

// ── Workflow classes ───────────────────────────────────────────────────────

/** Child of `ChildCase`. Doubles its input through one journaled step. */
export class DoubleChild extends Workflow<{ n: number }, { doubled: number }> {
  async run(
    trigger: WorkflowTrigger<{ n: number }>,
    step: WorkflowStep,
  ): Promise<{ doubled: number }> {
    const doubled = await step.run("double", () => trigger.input.n * 2);
    return { doubled };
  }
}

/** start + step.run + step.sideEffect memoisation. */
export class BasicCase extends Workflow<{ label: string }, unknown> {
  async run(trigger: WorkflowTrigger<{ label: string }>, step: WorkflowStep) {
    const first = await step.run("first", () => ({ label: trigger.input.label, n: 1 }));
    const second = await step.run("second", () => ({ n: first.n + 1 }));
    // sideEffect's contract is that the value is frozen into the journal on
    // first execution and returned unchanged on replay. The harness cannot see
    // that directly, but it CAN see that the two reads below agree, which is
    // false if the value is recomputed per dispatch.
    const frozen = await step.sideEffect("frozen", () => 42);
    const frozenAgain = await step.sideEffect("frozen-2", () => frozen);
    return { first, second, frozen, frozenAgain, steps: 4 };
  }
}

/** step.sleep: the run must suspend and then resume to completion. */
export class SleepCase extends Workflow<{ ms: number }, unknown> {
  async run(trigger: WorkflowTrigger<{ ms: number }>, step: WorkflowStep) {
    const before = await step.run("before", () => "before");
    await step.sleep("nap", `${trigger.input.ms}ms`);
    const after = await step.run("after", () => "after");
    return { before, after, slept: true };
  }
}

/** step.waitForSignal: the run parks until the harness signals it. */
export class SignalCase extends Workflow<{ label: string }, unknown> {
  async run(trigger: WorkflowTrigger<{ label: string }>, step: WorkflowStep) {
    await step.run("armed", () => trigger.input.label);
    const sig = await step.waitForSignal<{ token: string }>("go", {
      type: "probe.go",
      timeout: "60s",
    });
    return { received: sig !== null, payload: sig?.payload ?? null, type: sig?.type ?? null };
  }
}

/** step.call: a child run whose typed output is journaled into the parent. */
export class ChildCase extends Workflow<{ n: number }, unknown> {
  async run(trigger: WorkflowTrigger<{ n: number }>, step: WorkflowStep) {
    const child = await step.call(DoubleChild, { n: trigger.input.n });
    return { child, parentSaw: child.doubled };
  }
}

/**
 * compensate: a completed compensable step, then a permanent failure. The
 * platform must roll the first step back and the run must end non-completed.
 */
export class CompensateCase extends Workflow<{ label: string }, unknown> {
  async run(trigger: WorkflowTrigger<{ label: string }>, step: WorkflowStep) {
    await step.run(
      "reserve",
      {
        compensate: async (_output, ctx) => {
          await compensateOnce(ctx.idempotencyKey, () =>
            appendTrail("undo:reserve"),
          );
        },
      },
      async () => {
        await appendTrail("do:reserve");
        return { reserved: trigger.input.label };
      },
    );
    await step.run("boom", { retries: { maxAttempts: 1 } }, () => {
      throw new Error("probe-intentional-failure");
    });
    return { unreachable: true };
  }
}

// ── RPC surface ────────────────────────────────────────────────────────────

const CASES: Record<string, { workflow: string; input: unknown }> = {
  basic: { workflow: "BasicCase", input: { label: "probe" } },
  // The test observes the sleeping state and checks elapsed time.
  sleep: { workflow: "SleepCase", input: { ms: 20000 } },
  signal: { workflow: "SignalCase", input: { label: "probe" } },
  child: { workflow: "ChildCase", input: { n: 21 } },
  compensate: { workflow: "CompensateCase", input: { label: "probe" } },
};

// Every procedure below is ONE short operation. `zeroship serve` leaves the
// per-request wall clock unbounded
// (crates/zeroship-runtime/src/core/serve.rs, `wall_timeout: None`) while a deployed app
// inherits FREE_TIER_RUNTIME_LIMITS -- 5s wall, 50ms CPU
// (crates/zeroship-core/src/types.rs). Driving the poll loop from the caller keeps every
// request short and keeps the two sides on the same sequence of operations.

export const start = mutation(
  async (input: { case: string }) => {
    const spec = CASES[input.case];
    if (!spec) return { error: `unknown case ${input.case}` };
    // `start` is caught rather than allowed to propagate: an uncaught throw
    // becomes an opaque `{"message":"internal error"}` at the RPC boundary,
    // which tells a creator (and this harness) nothing about WHY the platform
    // refused. Reporting the message keeps a refusal legible and comparable.
    try {
      const run = await workflows[spec.workflow].start({ input: spec.input });
      return { runId: run.id, workflow: spec.workflow };
    } catch (e) {
      return {
        runId: null,
        workflow: spec.workflow,
        startError: (e as Error)?.message ?? String(e),
      };
    }
  },
  { id: "wf.start" },
);

export const status = query(
  async (input: { workflow: string; runId: string }) => {
    const run = workflows[input.workflow].get(input.runId);
    const s = await run.status();
    const err = s.error as Record<string, unknown> | null | undefined;
    // A run's result is a blob whatever it weighs, so `status` locates it and
    // `readOutput` is what turns it back into the value the workflow returned.
    // The probe answers with the value, which is what its tests compare.
    const output = s.output
      ? JSON.parse(new TextDecoder().decode(await run.readOutput())) as unknown
      : null;
    return {
      state: s.state,
      output,
      // Projected, not passed through: the deployed envelope carries a `stack`
      // whose frames name a per-process module id and minified line numbers, so
      // comparing it verbatim would diff on noise. `type` and `message` are the
      // contract; `compensation` is the rollback summary and is the one field
      // that says whether compensators actually ran.
      error: err && typeof err === "object"
        ? {
            type: err.type ?? null,
            message: err.message ?? null,
            compensation: err.compensation ?? null,
          }
        : (s.error ?? null),
    };
  },
  { id: "wf.status" },
);

export const signalRun = mutation(
  async (input: { workflow: string; runId: string; token: string }) => {
    const run = workflows[input.workflow].get(input.runId);
    await run.signal({ type: "probe.go", payload: { token: input.token } });
    return { signalled: true };
  },
  { id: "wf.signal" },
);

export const trail = query(
  async () => ({ trail: (await kv.get(TRAIL_KEY)) ?? "" }),
  { id: "wf.trail" },
);

export const resetTrail = mutation(
  async () => {
    await kv.delete(TRAIL_KEY);
    // The per-occurrence claim keys carry the run id, so a later run never
    // meets an earlier claim, and their TTL collects them. This counter is
    // app-wide and has to be cleared with the trail it accompanies.
    await kv.delete(REDISPATCH_KEY);
    return { reset: true };
  },
  { id: "wf.resetTrail" },
);

/// How many times a compensator was dispatched again after its effect had
/// already landed. Zero on a run where every compensator ran once.
export const compensatorRedispatches = query(
  async () => ({
    redispatches: Number.parseInt((await kv.get(REDISPATCH_KEY)) ?? "0", 10),
  }),
  { id: "wf.compensatorRedispatches" },
);

export const ping = query(
  async () => ({ ok: true, cases: Object.keys(CASES).sort() }),
  { id: "wf.ping" },
);
