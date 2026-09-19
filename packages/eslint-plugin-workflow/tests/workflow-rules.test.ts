import assert from "node:assert/strict";
import { describe, test } from "node:test";

import plugin, { recommended } from "../src/index.js";
import noNestedStep from "../src/rules/no-nested-step.js";
import noNondeterministicBetweenSteps from "../src/rules/no-nondeterministic-between-steps.js";
import noNondeterministicStepName from "../src/rules/no-nondeterministic-step-name.js";
import noParallelSteps from "../src/rules/no-parallel-steps.js";
import noStepCatchWithoutRethrow from "../src/rules/no-step-catch-without-rethrow.js";
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

function tryStatement(
  blockBody: AstNode[],
  handler: AstNode | null,
  finalizer: AstNode | null = null,
): AstNode {
  return { type: "TryStatement", block: block(blockBody), handler, finalizer };
}

function catchClause(param: AstNode | null, body: AstNode[]): AstNode {
  return { type: "CatchClause", param, body: block(body) };
}

function throwStatement(argument: AstNode): AstNode {
  return { type: "ThrowStatement", argument };
}

function returnStatement(argument: AstNode | null = null): AstNode {
  return { type: "ReturnStatement", argument };
}

function ifStatement(
  test: AstNode,
  consequent: AstNode,
  alternate: AstNode | null = null,
): AstNode {
  return { type: "IfStatement", test, consequent, alternate };
}

function instanceOf(left: AstNode, right: AstNode): AstNode {
  return { type: "BinaryExpression", operator: "instanceof", left, right };
}

function not(argument: AstNode): AstNode {
  return { type: "UnaryExpression", operator: "!", argument };
}

function whileStatement(test: AstNode, body: AstNode[]): AstNode {
  return { type: "WhileStatement", test, body: block(body) };
}

function assignNull(name: string): AstNode {
  return expr({
    type: "AssignmentExpression",
    operator: "=",
    left: id(name),
    right: lit(null),
  });
}

function newError(message: string): AstNode {
  return { type: "NewExpression", callee: id("Error"), arguments: [lit(message)] };
}

function chargeStep(): AstNode {
  return stepCall("run", [lit("charge"), arrow(lit("ok"))]);
}

function catchHandler(body: AstNode[]): AstNode {
  return catchClause(id("e"), body);
}

function handlerArrow(params: AstNode[], body: AstNode): AstNode {
  return { type: "ArrowFunctionExpression", params, body };
}

function stepPromiseCatch(handler: AstNode): AstNode {
  return call(member(chargeStep(), "catch"), [handler]);
}

describe("@zeroship/eslint-plugin-workflow", () => {
  test("exports all workflow rules and recommended severities", () => {
    assert.deepEqual(Object.keys(plugin.plugin.rules).sort(), [
      "no-nested-step",
      "no-nondeterministic-between-steps",
      "no-nondeterministic-step-name",
      "no-parallel-steps",
      "no-step-catch-without-rethrow",
    ]);
    assert.equal(
      recommended.rules["@zeroship/workflow/no-parallel-steps"],
      "warn",
    );
    assert.equal(
      recommended.rules["@zeroship/workflow/no-nested-step"],
      "error",
    );
    assert.equal(
      recommended.rules["@zeroship/workflow/no-step-catch-without-rethrow"],
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

  test("no-step-catch-without-rethrow flags a catch that can finish without rethrowing", () => {
    const swallowed = runRule(
      noStepCatchWithoutRethrow,
      tryStatement([expr(chargeStep())], catchHandler([assignNull("x")])),
    );
    assert.equal(swallowed.length, 1);
    assert.equal(swallowed[0].messageId, "catchWithoutRethrow");
    assert.equal(swallowed[0].data?.method, "run");

    // Control: the same try, differing only in the catch body's one statement.
    const rethrown = runRule(
      noStepCatchWithoutRethrow,
      tryStatement([expr(chargeStep())], catchHandler([throwStatement(id("e"))])),
    );
    assert.deepEqual(rethrown, []);

    // Control: the same swallowing catch, differing only in what the try block
    // calls.
    const withoutStep = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(call(id("chargeCard"), []))],
        catchHandler([assignNull("x")]),
      ),
    );
    assert.deepEqual(withoutStep, []);

    // A catch that returns a fallback leaves without throwing.
    const returnedFallback = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(chargeStep())],
        catchHandler([returnStatement(lit(null))]),
      ),
    );
    assert.equal(returnedFallback.length, 1);
    assert.equal(returnedFallback[0].messageId, "catchWithoutRethrow");
  });

  test("no-step-catch-without-rethrow allows the documented match-then-rethrow shape", () => {
    const documented = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(chargeStep())],
        catchHandler([
          ifStatement(
            instanceOf(id("e"), id("StepTimeoutError")),
            returnStatement(call(id("retryLater"), [])),
          ),
          throwStatement(id("e")),
        ]),
      ),
    );
    assert.deepEqual(documented, []);

    // Control: the same guard with the trailing rethrow removed.
    const guardOnly = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(chargeStep())],
        catchHandler([
          ifStatement(
            instanceOf(id("e"), id("StepTimeoutError")),
            returnStatement(call(id("retryLater"), [])),
          ),
        ]),
      ),
    );
    assert.equal(guardOnly.length, 1);
    assert.equal(guardOnly[0].messageId, "catchWithoutRethrow");
  });

  test("no-step-catch-without-rethrow reads the branch an unclaimed error takes", () => {
    const match = instanceOf(id("e"), id("StepTimeoutError"));

    // The `else` of a positive test rethrows: the unclaimed error leaves.
    const elseRethrows = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(chargeStep())],
        catchHandler([
          ifStatement(
            match,
            block([expr(call(id("handle"), [id("e")]))]),
            block([throwStatement(id("e"))]),
          ),
        ]),
      ),
    );
    assert.deepEqual(elseRethrows, []);

    // Control: the same if/else with the two branches swapped, so only the
    // matched error is rethrown.
    const onlyMatchRethrows = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(chargeStep())],
        catchHandler([
          ifStatement(
            match,
            block([throwStatement(id("e"))]),
            block([expr(call(id("handle"), [id("e")]))]),
          ),
        ]),
      ),
    );
    assert.equal(onlyMatchRethrows.length, 1);
    assert.equal(onlyMatchRethrows[0].messageId, "catchWithoutRethrow");

    // The `else` returns a fallback, so a later rethrow is out of that path's
    // reach.
    const elseReturns = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(chargeStep())],
        catchHandler([
          ifStatement(
            match,
            block([expr(call(id("handle"), [id("e")]))]),
            block([returnStatement(lit(null))]),
          ),
          throwStatement(id("e")),
        ]),
      ),
    );
    assert.equal(elseReturns.length, 1);
    assert.equal(elseReturns[0].messageId, "catchWithoutRethrow");

    // A negated guard rethrows on the `then` branch.
    const negatedGuard = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(chargeStep())],
        catchHandler([
          ifStatement(not(match), throwStatement(id("e"))),
          assignNull("x"),
        ]),
      ),
    );
    assert.deepEqual(negatedGuard, []);

    // Control: the same statements with the negation dropped.
    const positiveGuard = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(chargeStep())],
        catchHandler([
          ifStatement(match, throwStatement(id("e"))),
          assignNull("x"),
        ]),
      ),
    );
    assert.equal(positiveGuard.length, 1);
    assert.equal(positiveGuard[0].messageId, "catchWithoutRethrow");
  });

  test("no-step-catch-without-rethrow flags a catch that throws a new error", () => {
    const wrapped = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(chargeStep())],
        catchHandler([throwStatement(newError("charge failed"))]),
      ),
    );
    assert.equal(wrapped.length, 1);
    assert.equal(wrapped[0].messageId, "catchThrowsNewError");
    assert.equal(wrapped[0].data?.method, "run");

    // A catch with no binding has nothing to rethrow.
    const unbound = runRule(
      noStepCatchWithoutRethrow,
      tryStatement([expr(chargeStep())], catchClause(null, [assignNull("x")])),
    );
    assert.equal(unbound.length, 1);
    assert.equal(unbound[0].messageId, "catchWithoutRethrow");
  });

  test("no-step-catch-without-rethrow stays quiet on shapes it does not model", () => {
    const loop = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(chargeStep())],
        catchHandler([
          whileStatement(id("more"), [expr(call(id("handle"), [id("e")]))]),
          assignNull("x"),
        ]),
      ),
    );
    assert.deepEqual(loop, []);

    const nestedTry = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(chargeStep())],
        catchHandler([
          tryStatement(
            [expr(call(id("cleanup"), []))],
            catchClause(id("inner"), [assignNull("x")]),
          ),
        ]),
      ),
    );
    assert.deepEqual(nestedTry, []);

    // A `finally` catches nothing, so it swallows nothing.
    const finallyOnly = runRule(
      noStepCatchWithoutRethrow,
      tryStatement([expr(chargeStep())], null, block([expr(call(id("cleanup"), []))])),
    );
    assert.deepEqual(finallyOnly, []);
  });

  test("no-step-catch-without-rethrow reads every enclosing try block, and only blocks", () => {
    const outerSwallows = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [
          tryStatement(
            [expr(chargeStep())],
            catchHandler([throwStatement(id("e"))]),
          ),
        ],
        catchHandler([assignNull("x")]),
      ),
    );
    assert.equal(outerSwallows.length, 1);
    assert.equal(outerSwallows[0].messageId, "catchWithoutRethrow");

    // The step call runs in the handler, not in the guarded block.
    const stepInHandler = runRule(
      noStepCatchWithoutRethrow,
      tryStatement(
        [expr(call(id("chargeCard"), []))],
        catchHandler([expr(chargeStep()), assignNull("x")]),
      ),
    );
    assert.deepEqual(stepInHandler, []);
  });

  test("no-step-catch-without-rethrow flags a .catch() handler on a step promise", () => {
    const swallowed = runRule(
      noStepCatchWithoutRethrow,
      stepPromiseCatch(handlerArrow([], lit(null))),
    );
    assert.equal(swallowed.length, 1);
    assert.equal(swallowed[0].messageId, "promiseCatchWithoutRethrow");
    assert.equal(swallowed[0].data?.method, "run");

    // Control: the same call, differing only in the handler body.
    const rethrown = runRule(
      noStepCatchWithoutRethrow,
      stepPromiseCatch(
        handlerArrow([id("e")], block([throwStatement(id("e"))])),
      ),
    );
    assert.deepEqual(rethrown, []);

    // Control: the same handler on a promise that carries no step call.
    const withoutStep = runRule(
      noStepCatchWithoutRethrow,
      call(member(call(id("chargeCard"), []), "catch"), [
        handlerArrow([], lit(null)),
      ]),
    );
    assert.deepEqual(withoutStep, []);

    // A handler that throws a new error loses the platform's own stop.
    const wrapped = runRule(
      noStepCatchWithoutRethrow,
      stepPromiseCatch(
        handlerArrow([id("e")], block([throwStatement(newError("charge failed"))])),
      ),
    );
    assert.equal(wrapped.length, 1);
    assert.equal(wrapped[0].messageId, "catchThrowsNewError");

    // A handler the rule cannot read is left alone.
    const opaque = runRule(
      noStepCatchWithoutRethrow,
      stepPromiseCatch(id("reportFailure")),
    );
    assert.deepEqual(opaque, []);
  });
});
