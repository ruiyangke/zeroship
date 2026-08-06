import assert from "node:assert/strict";
import { describe, test } from "node:test";

import plugin, { recommended } from "../src/index.js";
import noNestedStep from "../src/rules/no-nested-step.js";
import noNondeterministicBetweenSteps from "../src/rules/no-nondeterministic-between-steps.js";
import noNondeterministicStepName from "../src/rules/no-nondeterministic-step-name.js";
import noParallelSteps from "../src/rules/no-parallel-steps.js";
import type { AstNode, RuleModule } from "../src/types.js";

interface Report {
  messageId: string;
  data?: Record<string, string>;
}

function runRule(rule: RuleModule, root: AstNode): Report[] {
  attachParents(root);
  const reports: Report[] = [];
  const visitor = rule.create({
    report({ messageId, data }) {
      reports.push({ messageId, data });
    },
  });
  visit(root, (node) => {
    if (node.type === "CallExpression") visitor.CallExpression?.(node);
    if (node.type === "NewExpression") visitor.NewExpression?.(node);
  });
  return reports;
}

function attachParents(node: AstNode, parent?: AstNode): void {
  if (parent) node.parent = parent;
  for (const child of childNodes(node)) attachParents(child, node);
}

function visit(node: AstNode, fn: (node: AstNode) => void): void {
  fn(node);
  for (const child of childNodes(node)) visit(child, fn);
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

function id(name: string): AstNode {
  return { type: "Identifier", name };
}

function lit(value: unknown): AstNode {
  return { type: "Literal", value };
}

function member(object: AstNode, property: string): AstNode {
  return {
    type: "MemberExpression",
    computed: false,
    object,
    property: id(property),
  };
}

function call(callee: AstNode, args: AstNode[] = []): AstNode {
  return { type: "CallExpression", callee, arguments: args };
}

function stepCall(method: string, args: AstNode[]): AstNode {
  return call(member(id("step"), method), args);
}

function promiseCall(method: string, args: AstNode[]): AstNode {
  return call(member(id("Promise"), method), args);
}

function arrow(body: AstNode): AstNode {
  return {
    type: "ArrowFunctionExpression",
    params: [],
    body,
  };
}

function array(elements: AstNode[]): AstNode {
  return { type: "ArrayExpression", elements };
}

function block(body: AstNode[]): AstNode {
  return { type: "BlockStatement", body };
}

function expr(expression: AstNode): AstNode {
  return { type: "ExpressionStatement", expression };
}

function program(body: AstNode[]): AstNode {
  return { type: "Program", body };
}

function templateWith(expression: AstNode): AstNode {
  return {
    type: "TemplateLiteral",
    quasis: [],
    expressions: [expression],
  };
}

function newDate(): AstNode {
  return { type: "NewExpression", callee: id("Date"), arguments: [] };
}

function envWorkflowStart(): AstNode {
  return call(member(member(member(id("env"), "workflows"), "Email"), "start"), [lit("x")]);
}

describe("@zeroship/eslint-plugin-workflow", () => {
  test("exports all workflow rules and recommended severities", () => {
    assert.deepEqual(Object.keys(plugin.plugin.rules).sort(), [
      "no-nested-step",
      "no-nondeterministic-between-steps",
      "no-nondeterministic-step-name",
      "no-parallel-steps",
    ]);
    assert.equal(
      recommended.rules["@zeroship/workflow/no-parallel-steps"],
      "warn",
    );
    assert.equal(
      recommended.rules["@zeroship/workflow/no-nested-step"],
      "error",
    );
  });

  test("no-nondeterministic-step-name flags nondeterministic step names", () => {
    const reports = runRule(
      noNondeterministicStepName,
      stepCall("run", [
        templateWith(call(member(id("Date"), "now"))),
        arrow(lit("ok")),
      ]),
    );
    assert.equal(reports.length, 1);
    assert.equal(reports[0].messageId, "nondeterministicStepName");
  });

  test("no-nondeterministic-between-steps flags clocks, random UUIDs, new Date, and env.workflows outside step.do", () => {
    const reports = runRule(
      noNondeterministicBetweenSteps,
      program([
        expr(call(member(id("Date"), "now"))),
        expr(call(member(id("Math"), "random"))),
        expr(call(member(id("crypto"), "randomUUID"))),
        expr(newDate()),
        expr(envWorkflowStart()),
        expr(stepCall("do", [
          lit("inside"),
          arrow(block([
            expr(call(member(id("Date"), "now"))),
          ])),
        ])),
      ]),
    );
    assert.deepEqual(reports.map((r) => r.data?.source), [
      "Date.now",
      "Math.random",
      "crypto.randomUUID",
      "new Date()",
      "env.workflows.*",
    ]);
  });

  test("no-parallel-steps reports Promise.all as serialized and race/allSettled/any as unsupported", () => {
    const promiseAll = runRule(
      noParallelSteps,
      promiseCall("all", [
        array([stepCall("run", [lit("a"), arrow(lit("A"))])]),
      ]),
    );
    assert.equal(promiseAll.length, 1);
    assert.equal(promiseAll[0].messageId, "promiseAllSteps");

    for (const method of ["race", "allSettled", "any"]) {
      const reports = runRule(
        noParallelSteps,
        promiseCall(method, [
          array([stepCall("run", [lit(method), arrow(lit("x"))])]),
        ]),
      );
      assert.equal(reports.length, 1);
      assert.equal(reports[0].messageId, "unsupportedPromiseCombinator");
      assert.equal(reports[0].data?.method, method);
    }
  });

  test("no-nested-step flags step.* inside a step body", () => {
    const reports = runRule(
      noNestedStep,
      stepCall("run", [
        lit("outer"),
        arrow(block([
          expr(stepCall("sleep", [lit("inner"), lit("1s")])),
        ])),
      ]),
    );
    assert.equal(reports.length, 1);
    assert.equal(reports[0].messageId, "nestedStep");
    assert.equal(reports[0].data?.method, "sleep");
  });
});
