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
