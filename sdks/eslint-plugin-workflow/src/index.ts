import noNestedStep from "./rules/no-nested-step.js";
import noNondeterministicBetweenSteps from "./rules/no-nondeterministic-between-steps.js";
import noNondeterministicStepName from "./rules/no-nondeterministic-step-name.js";
import noParallelSteps from "./rules/no-parallel-steps.js";

export const plugin = {
  rules: {
    "no-nondeterministic-step-name": noNondeterministicStepName,
    "no-nondeterministic-between-steps": noNondeterministicBetweenSteps,
    "no-parallel-steps": noParallelSteps,
    "no-nested-step": noNestedStep,
  },
};

export const recommended = {
  plugins: {
    "@zeroship/workflow": plugin,
  },
  rules: {
    "@zeroship/workflow/no-nondeterministic-step-name": "error",
    "@zeroship/workflow/no-nondeterministic-between-steps": "error",
    "@zeroship/workflow/no-parallel-steps": "warn",
    "@zeroship/workflow/no-nested-step": "error",
  },
};

export {
  noNestedStep,
  noNondeterministicBetweenSteps,
  noNondeterministicStepName,
  noParallelSteps,
};

export default {
  plugin,
  recommended,
  noNestedStep,
  noNondeterministicBetweenSteps,
  noNondeterministicStepName,
  noParallelSteps,
};
