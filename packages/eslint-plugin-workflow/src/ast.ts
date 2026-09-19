import type { AstNode } from "./types.js";

export const STEP_METHODS = new Set([
  "run",
  "do",
  "sleep",
  "sleepUntil",
  "waitForSignal",
  "call",
]);

const STEP_BODY_METHODS = new Set(["run", "do"]);

export function staticPropertyName(node: AstNode | undefined): string | undefined {
  if (!node) return undefined;
  if (node.type === "Identifier" && typeof node.name === "string") return node.name;
  if (node.type === "Literal" && typeof node.value === "string") return node.value;
  return undefined;
}

export function isMemberCall(node: AstNode, objectName: string, propertyName: string): boolean {
  const callee = node.callee;
  if (!callee || callee.type !== "MemberExpression") return false;
  if (callee.computed === true) return false;
  return (
    callee.object?.type === "Identifier" &&
    callee.object.name === objectName &&
    staticPropertyName(callee.property) === propertyName
  );
}

export function stepMethodName(node: AstNode): string | undefined {
  const callee = node.callee;
  if (!callee || callee.type !== "MemberExpression") return undefined;
  if (callee.computed === true) return undefined;
  if (callee.object?.type !== "Identifier" || callee.object.name !== "step") return undefined;
  const method = staticPropertyName(callee.property);
  return method && STEP_METHODS.has(method) ? method : undefined;
}

export function promiseCombinatorName(node: AstNode): string | undefined {
  const callee = node.callee;
  if (!callee || callee.type !== "MemberExpression") return undefined;
  if (callee.computed === true) return undefined;
  if (callee.object?.type !== "Identifier" || callee.object.name !== "Promise") return undefined;
  return staticPropertyName(callee.property);
}

export function isStepBodyFunction(node: AstNode): boolean {
  if (node.type !== "ArrowFunctionExpression" && node.type !== "FunctionExpression") {
    return false;
  }
  const parent = node.parent;
  if (!parent || parent.type !== "CallExpression") return false;
  const method = stepMethodName(parent);
  if (!method || !STEP_BODY_METHODS.has(method)) return false;
  const args = parent.arguments ?? [];
  return args.includes(node) && args.indexOf(node) > 0;
}

export function isInsideStepBody(node: AstNode): boolean {
  let cursor = node.parent;
  while (cursor) {
    if (isStepBodyFunction(cursor)) return true;
    cursor = cursor.parent;
  }
  return false;
}

export function containsStepCall(node: AstNode | null | undefined): boolean {
  if (!node) return false;
  if (node.type === "CallExpression" && stepMethodName(node)) return true;
  return childNodes(node).some(containsStepCall);
}

export function containsNondeterministicCall(node: AstNode | null | undefined): boolean {
  if (!node) return false;
  if (node.type === "CallExpression" && nondeterministicCallName(node)) return true;
  if (node.type === "NewExpression" && isNewDate(node)) return true;
  return childNodes(node).some(containsNondeterministicCall);
}

export function nondeterministicCallName(node: AstNode): string | undefined {
  if (isMemberCall(node, "Date", "now")) return "Date.now";
  if (isMemberCall(node, "Math", "random")) return "Math.random";
  if (isMemberCall(node, "crypto", "randomUUID")) return "crypto.randomUUID";
  return undefined;
}

export function isNewDate(node: AstNode): boolean {
  return node.type === "NewExpression" && node.callee?.type === "Identifier" && node.callee.name === "Date";
}

export function isEnvWorkflowsCall(node: AstNode): boolean {
  if (node.type !== "CallExpression") return false;
  let cursor = node.callee;
  while (cursor?.type === "MemberExpression") {
    const object = cursor.object;
    if (
      object?.type === "MemberExpression" &&
      object.object?.type === "Identifier" &&
      object.object.name === "env" &&
      staticPropertyName(object.property) === "workflows"
    ) {
      return true;
    }
    cursor = object;
  }
  return false;
}

// How a handler treats the value it did not claim. The bridge stops a workflow
// body by throwing through it, and it recognizes its own stop by identity, so
// only a path that rethrows the caught binding lets the stop reach the
// dispatcher. "unknown" is the verdict for a body shape this analysis does not
// model; a rule reads it as "stay quiet".
export type UnmatchedPathVerdict = "rethrows" | "throwsNew" | "swallows" | "unknown";

type StatementVerdict = UnmatchedPathVerdict | "continue";

// Every `try` whose *block* encloses the node, innermost first. A node reached
// through a handler or a finalizer is not inside that try's block: the value
// thrown there has already left it.
export function enclosingTryStatements(node: AstNode): AstNode[] {
  const out: AstNode[] = [];
  let child = node;
  let cursor = node.parent;
  while (cursor) {
    if (cursor.type === "TryStatement" && cursor.block === child) out.push(cursor);
    child = cursor;
    cursor = cursor.parent;
  }
  return out;
}

// The step method behind `<expression>.catch(handler)`, when the expression
// carries a step call.
export function stepPromiseCatchMethod(node: AstNode): string | undefined {
  const callee = node.callee;
  if (!callee || callee.type !== "MemberExpression") return undefined;
  if (callee.computed === true) return undefined;
  if (staticPropertyName(callee.property) !== "catch") return undefined;
  return firstStepMethodName(callee.object);
}

export function firstStepMethodName(node: AstNode | undefined): string | undefined {
  if (!node) return undefined;
  let found: string | undefined;
  walk(node, (current) => {
    if (found !== undefined) return;
    if (current.type !== "CallExpression") return;
    found = stepMethodName(current);
  });
  return found;
}

export function catchClauseVerdict(handler: AstNode | null | undefined): UnmatchedPathVerdict {
  if (!handler) return "unknown";
  const body = handler.body;
  if (!body || Array.isArray(body) || body.type !== "BlockStatement") return "unknown";
  return blockVerdict(body, boundErrorName(handler.param));
}

export function catchCallbackVerdict(handler: AstNode | undefined): UnmatchedPathVerdict {
  if (!handler) return "unknown";
  if (handler.type !== "ArrowFunctionExpression" && handler.type !== "FunctionExpression") {
    return "unknown";
  }
  const body = handler.body;
  if (!body || Array.isArray(body)) return "unknown";
  const bound = boundErrorName(handler.params?.[0]);
  // A concise arrow body is one expression, so there is no `throw` to reach.
  if (body.type !== "BlockStatement") return "swallows";
  return blockVerdict(body, bound);
}

function boundErrorName(param: AstNode | null | undefined): string | undefined {
  if (!param || param.type !== "Identifier") return undefined;
  return typeof param.name === "string" ? param.name : undefined;
}

function blockVerdict(block: AstNode, bound: string | undefined): UnmatchedPathVerdict {
  const verdict = sequenceVerdict(statementList(block), bound);
  // Running off the end of the handler is an exit that does not throw.
  return verdict === "continue" ? "swallows" : verdict;
}

function statementList(block: AstNode): AstNode[] {
  return Array.isArray(block.body) ? block.body : [];
}

function sequenceVerdict(statements: AstNode[], bound: string | undefined): StatementVerdict {
  for (const statement of statements) {
    const verdict = statementVerdict(statement, bound);
    if (verdict !== "continue") return verdict;
  }
  return "continue";
}

function statementVerdict(statement: AstNode, bound: string | undefined): StatementVerdict {
  switch (statement.type) {
    case "ThrowStatement":
      return throwsBoundError(statement.argument, bound) ? "rethrows" : "throwsNew";
    case "ReturnStatement":
      return "swallows";
    case "BlockStatement":
      return sequenceVerdict(statementList(statement), bound);
    case "IfStatement":
      return unmatchedBranchVerdict(statement, bound);
    case "ExpressionStatement":
    case "VariableDeclaration":
    case "FunctionDeclaration":
    case "ClassDeclaration":
    case "EmptyStatement":
    case "DebuggerStatement":
      return "continue";
    default:
      // Loops, `switch`, labels and a nested `try` are not modeled. Reading
      // them as "unknown" keeps the rule quiet rather than guessing.
      return "unknown";
  }
}

// The branch an unclaimed value takes: the `else` of a positive test, the
// `then` of a negated one, and otherwise the code after the `if`.
function unmatchedBranchVerdict(statement: AstNode, bound: string | undefined): StatementVerdict {
  const branch = isNegatedTest(statement.test) ? statement.consequent : statement.alternate;
  if (!branch) return "continue";
  return statementVerdict(branch, bound);
}

function isNegatedTest(test: AstNode | null | undefined): boolean {
  if (!test) return false;
  if (test.type === "UnaryExpression") return test.operator === "!";
  if (test.type === "LogicalExpression") {
    if (test.operator !== "&&" && test.operator !== "||") return false;
    return isNegatedTest(test.left) && isNegatedTest(test.right);
  }
  if (test.type !== "BinaryExpression") return false;
  if (test.operator === "!==" || test.operator === "!=") return true;
  if (test.operator !== "===" && test.operator !== "==") return false;
  return isFalseLiteral(test.left) || isFalseLiteral(test.right);
}

function isFalseLiteral(node: AstNode | undefined): boolean {
  return node?.type === "Literal" && node.value === false;
}

function throwsBoundError(argument: AstNode | null | undefined, bound: string | undefined): boolean {
  if (!bound || !argument) return false;
  return argument.type === "Identifier" && argument.name === bound;
}

export function walk(node: AstNode, visit: (node: AstNode) => void): void {
  visit(node);
  for (const child of childNodes(node)) {
    walk(child, visit);
  }
}

function childNodes(node: AstNode): AstNode[] {
  const out: AstNode[] = [];
  for (const key of Object.keys(node) as Array<keyof AstNode>) {
    if (key === "parent") continue;
    const value = node[key];
    if (Array.isArray(value)) {
      for (const item of value) {
        if (item && typeof item === "object" && "type" in item) out.push(item as AstNode);
      }
    } else if (value && typeof value === "object" && "type" in value) {
      out.push(value as AstNode);
    }
  }
  return out;
}
