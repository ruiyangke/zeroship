const MARK = "__MARK__";

export class Checkout {
  async run(trigger, step) {
const first = await step.run("first", () => {
  globalThis.__bodyRuns = (globalThis.__bodyRuns ?? 0) + 1;
  return { mark: MARK, step: "first", bodyRuns: globalThis.__bodyRuns, input: trigger.input };
});
const second = await step.run("second", () => {
  globalThis.__bodyRuns = (globalThis.__bodyRuns ?? 0) + 1;
  return { mark: MARK, step: "second", bodyRuns: globalThis.__bodyRuns, first };
});
await step.sleep("nap", "PT1S");
return { mark: MARK, second };
  }
}

export class ConcurrentWorkflow {
  async run(trigger, step) {
const values = await Promise.all([
  step.run("a", () => ({ mark: MARK, step: "a", input: trigger.input })),
  step.run("b", () => ({ mark: MARK, step: "b", input: trigger.input })),
  step.run("c", () => ({ mark: MARK, step: "c", input: trigger.input })),
]);
const final = await step.run("final", () => ({ mark: MARK, values }));
return { values, final };
  }
}

export default { workflows: { Checkout, ConcurrentWorkflow } };
