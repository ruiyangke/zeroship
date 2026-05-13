/**
 * Unit tests for the D1 ESLint rule. We mock the bare ESLint RuleContext
 * shape so the test stays free of the `eslint` dep (which is a peer
 * dependency consumers supply). The mock simulates ESLint walking
 * call-expression nodes; we hand it ESTree-ish literal nodes and assert
 * that `context.report` was called with the expected messageId.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import rule from "../src/rules/no-unindexed-query.js";

interface Report {
  messageId: string;
  data?: Record<string, string>;
}

function runOn(callNode: object): Report[] {
  const reports: Report[] = [];
  const visitor = rule.create({
    report({ messageId, data }) {
      reports.push({ messageId, data });
    },
  });
  visitor.CallExpression?.(callNode as never);
  return reports;
}

// Helper: shape an ESTree-ish `obj.method({ key: literal })` call.
function callExpr(method: string, props: Array<[string, unknown]>): object {
  return {
    type: "CallExpression",
    callee: {
      type: "MemberExpression",
      computed: false,
      property: { type: "Identifier", name: method },
      object: { type: "Identifier", name: "db" },
    },
    arguments: [
      {
        type: "ObjectExpression",
        properties: props.map(([key, value]) => ({
          type: "Property",
          computed: false,
          key: { type: "Identifier", name: key },
          value: { type: "Literal", value },
        })),
      },
    ],
  };
}

describe("D1 — no-unindexed-query rule", () => {
  test("rule metadata is well-formed", () => {
    assert.equal(rule.meta.type, "suggestion");
    assert.ok(rule.meta.docs.description);
    assert.ok(rule.meta.messages.unindexedQuery);
  });

  test("flags find({ name: 'x' })", () => {
    const reports = runOn(callExpr("find", [["name", "Alice"]]));
    assert.equal(reports.length, 1);
    assert.equal(reports[0].messageId, "unindexedQuery");
    assert.equal(reports[0].data?.method, "find");
    assert.equal(reports[0].data?.keys, "name");
  });

  test("flags findOne({ email: 'x' })", () => {
    const reports = runOn(callExpr("findOne", [["email", "a@b.com"]]));
    assert.equal(reports.length, 1);
    assert.equal(reports[0].data?.method, "findOne");
  });

  test("flags deleteMany({ status: 'x' })", () => {
    const reports = runOn(callExpr("deleteMany", [["status", "stale"]]));
    assert.equal(reports.length, 1);
    assert.equal(reports[0].data?.method, "deleteMany");
  });

  test("does NOT flag find({ id: 1 }) — id is always indexed (PRIMARY KEY)", () => {
    const reports = runOn(callExpr("find", [["id", 1]]));
    assert.equal(reports.length, 0);
  });

  test("does NOT flag find({}) — no filter keys", () => {
    const reports = runOn(callExpr("find", []));
    assert.equal(reports.length, 0);
  });

  test("does NOT flag find({ $or: [...] }) — operator key only", () => {
    // $or is a sentinel — wouldn't statically appear as ObjectExpression
    // literal but we still want to ensure operator keys don't trigger.
    const reports = runOn({
      type: "CallExpression",
      callee: {
        type: "MemberExpression",
        computed: false,
        property: { type: "Identifier", name: "find" },
        object: { type: "Identifier", name: "db" },
      },
      arguments: [
        {
          type: "ObjectExpression",
          properties: [
            {
              type: "Property",
              computed: false,
              key: { type: "Identifier", name: "$or" },
              value: { type: "ArrayExpression" },
            },
          ],
        },
      ],
    });
    assert.equal(reports.length, 0);
  });

  test("does NOT flag updateOne — out of scope", () => {
    const reports = runOn(callExpr("updateOne", [["name", "Alice"]]));
    assert.equal(reports.length, 0);
  });

  test("does NOT flag dynamic filter shape (e.g. .find(filter))", () => {
    // Filter passed as an Identifier — can't analyse statically, so we
    // bail without warning. Matches the "no false positives on dynamic"
    // posture from the rule docs.
    const reports = runOn({
      type: "CallExpression",
      callee: {
        type: "MemberExpression",
        computed: false,
        property: { type: "Identifier", name: "find" },
        object: { type: "Identifier", name: "db" },
      },
      arguments: [{ type: "Identifier", name: "filter" }],
    });
    assert.equal(reports.length, 0);
  });

  test("flags multi-key filter and lists all non-operator keys", () => {
    const reports = runOn(callExpr("find", [
      ["status", "active"],
      ["role", "admin"],
    ]));
    assert.equal(reports.length, 1);
    assert.ok(
      reports[0].data?.keys?.includes("status") &&
        reports[0].data?.keys?.includes("role"),
      `expected status+role in keys, got ${reports[0].data?.keys}`,
    );
  });

  test("does NOT flag computed keys (dynamic) — bails statically", () => {
    const reports = runOn({
      type: "CallExpression",
      callee: {
        type: "MemberExpression",
        computed: false,
        property: { type: "Identifier", name: "find" },
        object: { type: "Identifier", name: "db" },
      },
      arguments: [
        {
          type: "ObjectExpression",
          properties: [
            {
              type: "Property",
              computed: true,
              key: { type: "Identifier", name: "dynamic" },
              value: { type: "Literal", value: "x" },
            },
          ],
        },
      ],
    });
    assert.equal(reports.length, 0);
  });
});
