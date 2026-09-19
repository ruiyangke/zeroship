import {
  catchCallbackVerdict,
  catchClauseVerdict,
  enclosingTryStatements,
  stepMethodName,
  stepPromiseCatchMethod,
} from "../ast.js";
import type { AstNode, RuleModule } from "../types.js";

const rule: RuleModule = {
  meta: {
    type: "problem",
    docs: {
      description:
        "Require a catch around a workflow step call to rethrow the error it does not handle.",
      recommended: true,
      url: "https://github.com/zeroship-dev/zeroship/blob/main/docs/proposals/2026-07-05-durable-workflows-design.md",
    },
    schema: [],
    messages: {
      catchWithoutRethrow:
        "This catch around step.{{method}} can finish without rethrowing, and the platform stops a workflow body by throwing through it. Rethrow the caught error on the path you do not handle.",
      catchThrowsNewError:
        "This catch around step.{{method}} throws a new error rather than the one it caught, and the platform recognizes its own stop by identity. Rethrow the caught error on the path you do not handle.",
      promiseCatchWithoutRethrow:
        "A .catch() handler on a step.{{method}} promise can finish without rethrowing, and the platform stops a workflow body by rejecting that promise. Rethrow the caught error on the path you do not handle, or drop the handler.",
    },
  },
  create(context) {
    const reported = new Set<AstNode>();
    return {
      CallExpression(node) {
        const caughtMethod = stepPromiseCatchMethod(node);
        if (caughtMethod) {
          const verdict = catchCallbackVerdict(node.arguments?.[0]);
          if (verdict === "swallows" || verdict === "throwsNew") {
            context.report({
              node,
              messageId:
                verdict === "swallows"
                  ? "promiseCatchWithoutRethrow"
                  : "catchThrowsNewError",
              data: { method: caughtMethod },
            });
          }
          return;
        }
        const method = stepMethodName(node);
        if (!method) return;
        // Every enclosing try, not just the innermost: an outer catch swallows
        // the stop an inner one rethrew.
        for (const tryStatement of enclosingTryStatements(node)) {
          if (reported.has(tryStatement)) continue;
          const handler = tryStatement.handler;
          if (!handler) continue;
          const verdict = catchClauseVerdict(handler);
          if (verdict !== "swallows" && verdict !== "throwsNew") continue;
          reported.add(tryStatement);
          context.report({
            node: handler,
            messageId:
              verdict === "swallows" ? "catchWithoutRethrow" : "catchThrowsNewError",
            data: { method },
          });
        }
      },
    };
  },
};

export default rule;
