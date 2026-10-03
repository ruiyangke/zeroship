// Record step attempts and idempotent commits outside the workflow journal.
import { env } from "zeroship";

const SIDE_EFFECTS = "workflow_e2e_side_effects";
const EFFECT_ATTEMPTS = "workflow_e2e_effect_attempts";
const EFFECT_COMMITS = "workflow_e2e_effect_commits";

async function bump(runId, stepName) {
  const table = env.db.collection(SIDE_EFFECTS);
  const count = await table.count({ run_id: runId, step_name: stepName });
  await table.insert({ run_id: runId, step_name: stepName });
  return { step: stepName, count };
}

async function commit(runId, stepName, key) {
  await env.db.collection(EFFECT_ATTEMPTS).insert({
    run_id: runId,
    step_name: stepName,
    idempotency_key: key,
  });
  try {
    await env.db.collection(EFFECT_COMMITS).insert({
      run_id: runId,
      step_name: stepName,
      idempotency_key: key,
    });
  } catch (error) {
    if (!error || error.code !== "unique_violation") {
      throw error;
    }
  }
  const committed = await env.db
    .collection(EFFECT_COMMITS)
    .count({ idempotency_key: key });
  return { step: stepName, committed };
}

export class KeystoneWorkflow {
  async run(trigger, step) {
    const a = await step.run("a", () => bump(trigger.runId, "a"));
    await step.sleep("sleep", "PT1S");
    const b = await step.run("b", () => bump(trigger.runId, "b"));
    return { a, b };
  }
}

export class SignalWorkflow {
  async run(trigger, step) {
    const a = await step.run("a", () => bump(trigger.runId, "a"));
    try {
      const signal = await step.waitForSignal("go", {
        type: "go",
        timeout: trigger.input.timeout,
        maxSignalAge: trigger.input.maxSignalAge,
      });
      const b = await step.run("b", () => bump(trigger.runId, "b"));
      return { state: "signaled", a, signal, b };
    } catch (error) {
      if (!error || error.name !== "WorkflowTimeoutError") {
        throw error;
      }
      const timeout = await step.run("timeout", () => bump(trigger.runId, "timeout"));
      return { state: "timeout", errorName: error.name, a, timeout };
    }
  }
}

export class TopicSignalWorkflow {
  async run(trigger, step) {
    const a = await step.run("a", () => bump(trigger.runId, "a"));
    const signal = await step.waitForSignal("topic-go", {
      type: "go",
      topic: trigger.input.topic,
      timeout: "PT30S",
    });
    const b = await step.run("b", () => bump(trigger.runId, "b"));
    return { state: "topic-signaled", a, signal, b };
  }
}

export class ConcurrentWorkflow {
  async run(trigger, step) {
    const [a, b, c] = await Promise.all([
      step.run("a", () => bump(trigger.runId, "a")),
      step.run("b", () => bump(trigger.runId, "b")),
      step.run("c", () => bump(trigger.runId, "c")),
    ]);
    const final = await step.run("final", () => bump(trigger.runId, "final"));
    return { a, b, c, final };
  }
}

export class ConcurrentCommitWorkflow {
  async run(trigger, step) {
    const [a, b, c] = await Promise.all([
      step.run("a", () => commit(trigger.runId, "frontier:a", `${trigger.runId}:frontier:a`)),
      step.run("b", () => commit(trigger.runId, "frontier:b", `${trigger.runId}:frontier:b`)),
      step.run("c", () => commit(trigger.runId, "frontier:c", `${trigger.runId}:frontier:c`)),
    ]);
    const final = await step.run("final", () =>
      commit(trigger.runId, "frontier:final", `${trigger.runId}:frontier:final`),
    );
    return { a, b, c, final };
  }
}

export class SingleCommitWorkflow {
  async run(trigger, step) {
    const once = await step.run("once", () =>
      commit(trigger.runId, "once", `${trigger.runId}:once`),
    );
    return { once };
  }
}

export class SideEffectWorkflow {
  async run(trigger, step) {
    const v = await step.sideEffect("v", () => bump(trigger.runId, "v"));
    const after = await step.run("after", () => bump(trigger.runId, "after"));
    return { v, after };
  }
}

export class BareAwaitWorkflow {
  async run(trigger, step) {
    const pending = step.run("first", () => bump(trigger.runId, "first"));
    await fetch("http://127.0.0.1:1/never-connects", { method: "POST" });
    await pending;
    return { unreachable: true };
  }
}

export class NameDivergenceWorkflow {
  async run(trigger, step) {
    return await step.run("actual", () => bump(trigger.runId, "actual"));
  }
}

export class CompensationWorkflow {
  async run(trigger, step) {
    const a = await step.run(
      "a",
      { compensate: (output, ctx) => commit(trigger.runId, `undo:${output.step}`, ctx.idempotencyKey) },
      () => bump(trigger.runId, "a"),
    );
    const b = await step.run(
      "b",
      { compensate: (output, ctx) => commit(trigger.runId, `undo:${output.step}`, ctx.idempotencyKey) },
      () => bump(trigger.runId, "b"),
    );
    if (trigger.input.case === "cancel-compensate") {
      await step.sleep("rollback-wait", "PT30S");
      return { unreachable: true, a, b };
    }
    await step.run("c", () => {
      throw new Error("c failed");
    });
    return { unreachable: true };
  }
}

export class CompensationNameDivergenceWorkflow {
  async run(trigger, step) {
    await step.run(
      "actual",
      { compensate: (output, ctx) => commit(trigger.runId, `undo:${output.step ?? "actual"}`, ctx.idempotencyKey) },
      () => bump(trigger.runId, "actual"),
    );
    return { ok: true };
  }
}

export class ScheduledWorkflow {
  async run(trigger) {
    return {
      input: trigger.input,
      runId: trigger.runId,
      workflowName: trigger.workflowName,
      startedAt: trigger.startedAt.toISOString(),
    };
  }
}

export class BenchWorkflow {
  async run(trigger, step) {
    const checkpoint = await step.run("checkpoint", () => ({
      runId: trigger.runId,
      marker: trigger.input.marker,
    }));
    return { checkpoint };
  }
}

export class BlobOutputWorkflow {
  async run(trigger, step) {
    const payload = "x".repeat(trigger.input.size);
    const digest = payload.length + ":" + payload.charCodeAt(0);
    const output = await step.run("big", { output: "blob" }, () => ({ payload, digest }));
    const value = output && typeof output.json === "function" ? await output.json() : output;
    return { digest: value.digest, len: value.payload.length };
  }
}

export class StreamLimitWorkflow {
  async run(trigger, step) {
    await step.run("too-big", { output: "stream" }, () => "s".repeat(trigger.input.size));
    return { unreachable: true };
  }
}

export class ChildEchoWorkflow {
  async run(trigger, step) {
    const seen = await step.run("child-seen", () => ({
      value: trigger.input.value,
      runId: trigger.runId,
    }));
    return seen;
  }
}

export class ChildFailWorkflow {
  async run() {
    throw new Error("child failed as requested");
  }
}

export class ChildBlockWorkflow {
  async run(trigger, step) {
    await step.sleep("child-block", trigger.input.sleep ?? "PT30S");
    return { unblocked: true };
  }
}

export class ChildTrackedBlockWorkflow {
  async run(trigger, step) {
    const started = await step.run("child-start", () => bump(trigger.runId, "child-start"));
    await step.sleep("child-block", trigger.input.sleep ?? "PT30S");
    return { started, unblocked: true };
  }
}

export class ParentCallWorkflow {
  async run(trigger, step) {
    const child = await step.call(
      ChildEchoWorkflow,
      { value: trigger.input.value },
      { cascade: trigger.input.cascade === true },
    );
    const after = await step.run("after-child", () => ({
      value: child.value,
      childRunId: child.runId,
    }));
    return { child, after };
  }
}

export class ParentStartManyWorkflow {
  async run(trigger, step) {
    const outputs = await step.startMany(
      ChildEchoWorkflow,
      trigger.input.values.map((value) => ({
        input: { value },
        key: `child-${value}`,
      })),
      { cascade: true },
    );
    return { outputs };
  }
}

export class ParentCatchChildFailureWorkflow {
  async run(trigger, step) {
    try {
      await step.call(ChildFailWorkflow, { value: trigger.input.value });
      return { caught: false };
    } catch (error) {
      const marker = await step.run("caught-child-failure", () => ({
        name: error?.name,
        message: error?.message,
      }));
      return { caught: true, marker };
    }
  }
}

export class ParentCascadeWorkflow {
  async run(trigger, step) {
    await step.call(
      ChildBlockWorkflow,
      { sleep: trigger.input.sleep ?? "PT30S" },
      { cascade: true },
    );
    return { unreachable: true };
  }
}

export class ParentManyCascadeWorkflow {
  async run(trigger, step) {
    await step.startMany(
      ChildTrackedBlockWorkflow,
      trigger.input.values.map((value) => ({
        input: { value, sleep: trigger.input.sleep ?? "PT30S" },
        key: `tracked-${value}`,
      })),
      { cascade: true },
    );
    return { unreachable: true };
  }
}

export class ContinueAsNewWorkflow {
  async run(trigger, step) {
    if (!trigger.input.generation) {
      const before = await step.run("before-can", () => bump(trigger.runId, "before-can"));
      await step.continueAsNew({
        generation: 1,
        case: trigger.input.case,
        previousRunId: trigger.runId,
        marker: before,
      });
    }
    const after = await step.run("after-can", () => bump(trigger.runId, "after-can"));
    return {
      generation: trigger.input.generation,
      case: trigger.input.case,
      previousRunId: trigger.input.previousRunId,
      marker: trigger.input.marker,
      after,
    };
  }
}

export class CompensableCarryWorkflow {
  async run(trigger, step) {
    const done = await step.run(
      "compensable",
      {
        compensate: async (output) => {
          await commit(trigger.runId, "undo:compensable", output.step);
        },
      },
      () => bump(trigger.runId, "compensable"),
    );
    await step.continueAsNew({
      generation: 1,
      case: trigger.input.case,
      done,
    });
  }
}

// Reaches nothing outside its own journal, so a completed run proves the host
// delivered and executed it rather than that the app database also worked.
export class HostIngressWorkflow {
  async run(trigger, step) {
    const echoed = await step.run("echo", () => ({ via: trigger.input.via }));
    return { echoed };
  }
}

// A handle the app CLONED out of `env.workflows` during one request and kept
// across the next. The host retires a backend by withdrawing its generation,
// and a handle taken before that must not outlive it; keeping it in module
// scope is how a request isolate proves whether it did.
let retainedHandle = null;

export default {
  // Ordinary app ingress. `/__host/start/<Workflow>` and
  // `/__host/status/<Workflow>/<run>` exercise `env.workflows` from a request
  // isolate, which reaches whatever backend the process made ready for this
  // app - and nothing else.
  async fetch(request, env) {
    const path = new URL(request.url).pathname;
    const retain = path.match(/^\/__host\/retain\/([A-Za-z]+)\/([A-Za-z0-9_]+)$/);
    if (retain) {
      try {
        retainedHandle = env.workflows[retain[1]].get(retain[2]);
        return Response.json({ retained: true });
      } catch (error) {
        return Response.json({ code: error.code, message: error.message }, { status: 503 });
      }
    }
    if (path === "/__host/retained") {
      if (!retainedHandle) {
        return Response.json({ code: "no_retained_handle" }, { status: 409 });
      }
      try {
        const state = await retainedHandle.status();
        return Response.json({ state: state.state });
      } catch (error) {
        return Response.json({ code: error.code, message: error.message }, { status: 503 });
      }
    }
    const start = path.match(/^\/__host\/start\/([A-Za-z]+)$/);
    if (start) {
      try {
        const run = await env.workflows[start[1]].start({ input: { via: "ingress" } });
        return Response.json({ id: run.id });
      } catch (error) {
        return Response.json({ code: error.code, message: error.message }, { status: 503 });
      }
    }
    const status = path.match(/^\/__host\/status\/([A-Za-z]+)\/([A-Za-z0-9_]+)$/);
    if (status) {
      try {
        const run = env.workflows[status[1]].get(status[2]);
        const state = await run.status();
        // A run's result is a blob whatever it weighs, so `status` locates it
        // and `readOutput` is what turns it back into the returned value. A run
        // that returned nothing reports no output and reads nothing.
        const output = state.output
          ? JSON.parse(new TextDecoder().decode(await run.readOutput()))
          : null;
        return Response.json({ state: state.state, output });
      } catch (error) {
        return Response.json({ code: error.code, message: error.message }, { status: 503 });
      }
    }
    return new Response("dw07-ok");
  },
  workflows: {
    HostIngressWorkflow,
    KeystoneWorkflow,
    SignalWorkflow,
    TopicSignalWorkflow,
    ConcurrentWorkflow,
    ConcurrentCommitWorkflow,
    SingleCommitWorkflow,
    SideEffectWorkflow,
    BareAwaitWorkflow,
    NameDivergenceWorkflow,
    CompensationWorkflow,
    CompensationNameDivergenceWorkflow,
    ScheduledWorkflow,
    BenchWorkflow,
    BlobOutputWorkflow,
    StreamLimitWorkflow,
    ChildEchoWorkflow,
    ChildFailWorkflow,
    ChildBlockWorkflow,
    ChildTrackedBlockWorkflow,
    ParentCallWorkflow,
    ParentStartManyWorkflow,
    ParentCatchChildFailureWorkflow,
    ParentCascadeWorkflow,
    ParentManyCascadeWorkflow,
    ContinueAsNewWorkflow,
    CompensableCarryWorkflow,
  },
};
