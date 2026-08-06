import {
  containsNondeterministicCall,
  stepMethodName,
} from "../ast.js";
import type { RuleModule } from "../types.js";

const rule: RuleModule = {
  meta: {
    type: "problem",
    docs: {
      description:
        "Disallow Date.now(), Math.random(), crypto.randomUUID(), or new Date() in workflow step names.",
      recommended: true,
      url: "https://github.com/zeroship-dev/zeroship/blob/main/docs/proposals/2026-07-05-durable-workflows-design.md",
    },
    schema: [],
    messages: {
      nondeterministicStepName:
        "Workflow step names must be replay-deterministic. Move {{source}} inside a prior step and build this name from journaled data.",
    },
  },
  create(context) {
    return {
      CallExpression(node) {
        const method = stepMethodName(node);
        if (!method) return;
        const nameArg = node.arguments?.[0];
        if (!containsNondeterministicCall(nameArg)) return;
        context.report({
          node: nameArg ?? node,
          messageId: "nondeterministicStepName",
          data: { source: "clock/random data" },
        });
      },
    };
  },
};

export default rule;
