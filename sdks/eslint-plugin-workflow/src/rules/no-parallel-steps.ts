import {
  containsStepCall,
  promiseCombinatorName,
} from "../ast.js";
import type { RuleModule } from "../types.js";

const SUPPORTED_SERIALIZED = new Set(["all"]);
const UNSUPPORTED = new Set(["race", "allSettled", "any"]);

const rule: RuleModule = {
  meta: {
    type: "problem",
    docs: {
      description:
        "Warn on Promise.all over workflow steps and error on unsupported Promise.race/allSettled/any over step promises.",
      recommended: true,
      url: "https://github.com/zeroship-dev/zeroship/blob/main/docs/proposals/2026-07-05-durable-workflows-design.md",
    },
    schema: [],
    messages: {
      promiseAllSteps:
        "Promise.all over workflow steps is safe but serializes through the single frontier. Prefer sequential awaits or document the shape deliberately.",
      unsupportedPromiseCombinator:
        "Promise.{{method}} over workflow step promises is unsupported because it can corrupt replay. Await steps sequentially or use Promise.all.",
    },
  },
  create(context) {
    return {
      CallExpression(node) {
        const method = promiseCombinatorName(node);
        if (!method) return;
        if (!containsStepCall(node.arguments?.[0])) return;
        if (SUPPORTED_SERIALIZED.has(method)) {
          context.report({ node, messageId: "promiseAllSteps" });
          return;
        }
        if (UNSUPPORTED.has(method)) {
          context.report({
            node,
            messageId: "unsupportedPromiseCombinator",
            data: { method },
          });
        }
      },
    };
  },
};

export default rule;
