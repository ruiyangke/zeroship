import {
  isEnvWorkflowsCall,
  isInsideStepBody,
  isNewDate,
  nondeterministicCallName,
} from "../ast.js";
import type { RuleModule } from "../types.js";

const rule: RuleModule = {
  meta: {
    type: "problem",
    docs: {
      description:
        "Disallow nondeterministic clock/random/workflow calls in workflow bodies unless they are inside step.do/step.run.",
      recommended: true,
      url: "https://github.com/zeroship-dev/zeroship/blob/main/docs/proposals/2026-07-05-durable-workflows-design.md",
    },
    schema: [],
    messages: {
      nondeterministicBetweenSteps:
        "{{source}} is nondeterministic between workflow steps. Put it inside step.do/step.run and use the journaled result.",
    },
  },
  create(context) {
    return {
      CallExpression(node) {
        if (isInsideStepBody(node)) return;
        const source = nondeterministicCallName(node) ??
          (isEnvWorkflowsCall(node) ? "env.workflows.*" : undefined);
        if (!source) return;
        context.report({
          node,
          messageId: "nondeterministicBetweenSteps",
          data: { source },
        });
      },
      NewExpression(node) {
        if (isInsideStepBody(node)) return;
        if (!isNewDate(node)) return;
        context.report({
          node,
          messageId: "nondeterministicBetweenSteps",
          data: { source: "new Date()" },
        });
      },
    };
  },
};

export default rule;
