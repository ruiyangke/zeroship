import {
  isInsideStepBody,
  stepMethodName,
} from "../ast.js";
import type { RuleModule } from "../types.js";

const rule: RuleModule = {
  meta: {
    type: "problem",
    docs: {
      description:
        "Disallow calling step.* from inside a workflow step body.",
      recommended: true,
      url: "https://github.com/zeroship-dev/zeroship/blob/main/docs/proposals/2026-07-05-durable-workflows-design.md",
    },
    schema: [],
    messages: {
      nestedStep:
        "Do not call step.{{method}} from inside a step body. Split it into a later top-level workflow step.",
    },
  },
  create(context) {
    return {
      CallExpression(node) {
        const method = stepMethodName(node);
        if (!method) return;
        if (!isInsideStepBody(node)) return;
        context.report({
          node,
          messageId: "nestedStep",
          data: { method },
        });
      },
    };
  },
};

export default rule;
