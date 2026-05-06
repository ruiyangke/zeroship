/**
 * Function-level `"use server"` detection.
 *
 * `docs/proposals/rpc-v2.md` §1 says a function whose first statement
 * is `"use server"`
 * is a server function regardless of the enclosing file's directive.
 * The detector walks every function-like node — declarations,
 * expressions, arrows — both top-level and nested.
 *
 * The function-level directive only marks the function itself; other
 * code in the file remains client-side. The detector returns the set
 * of names that resolve to a marked function via VariableDeclarator,
 * AssignmentExpression, or FunctionDeclaration.id.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { parse as acornParse } from "acorn";

import { detectFunctionLevelUseServer } from "../src/transform.js";

function parse(code: string): { body: unknown[] } {
  return acornParse(code, {
    ecmaVersion: 2024,
    sourceType: "module",
    allowImportExportEverywhere: true,
  }) as unknown as { body: unknown[] };
}

describe("detectFunctionLevelUseServer — directive walks", () => {
  test("FunctionDeclaration with directive at body[0]", () => {
    const ast = parse(`
async function updatePost(formData) {
  "use server";
  await db.posts.update(formData);
}
`);
    const found = detectFunctionLevelUseServer(ast);
    assert.deepEqual([...found], ["updatePost"]);
  });

  test("Arrow assigned to const captures the binding name", () => {
    const ast = parse(`
const ping = async () => {
  "use server";
  return "pong";
};
`);
    const found = detectFunctionLevelUseServer(ast);
    assert.deepEqual([...found], ["ping"]);
  });

  test("Function expression assigned to const captures the binding name", () => {
    const ast = parse(`
const greet = async function() {
  "use server";
  return "hi";
};
`);
    const found = detectFunctionLevelUseServer(ast);
    assert.deepEqual([...found], ["greet"]);
  });

  test("AssignmentExpression captures the LHS name", () => {
    const ast = parse(`
let h;
h = function() {
  "use server";
  return 1;
};
`);
    const found = detectFunctionLevelUseServer(ast);
    assert.deepEqual([...found], ["h"]);
  });

  test("Nested function inside another function is detected", () => {
    // The `docs/proposals/rpc-v2.md` §1 RSC example: a server function
    // declared inside a server-component render body.
    const ast = parse(`
export default async function PostPage({ params }) {
  const post = await getPost(params.id);

  async function updatePost(formData) {
    "use server";
    await db.posts.update(params.id, formData);
  }

  return null;
}
`);
    const found = detectFunctionLevelUseServer(ast);
    // PostPage itself isn't marked (no directive); updatePost is.
    assert.ok(found.has("updatePost"));
    assert.ok(!found.has("PostPage"));
  });

  test("Function without directive is not detected", () => {
    const ast = parse(`
async function regular() {
  return 1;
}
`);
    const found = detectFunctionLevelUseServer(ast);
    assert.equal(found.size, 0);
  });

  test("Directive must be body[0], not deeper", () => {
    const ast = parse(`
async function nope() {
  const x = 1;
  "use server";
  return x;
}
`);
    const found = detectFunctionLevelUseServer(ast);
    assert.equal(found.size, 0, "directive after a statement is not a directive");
  });

  test("Anonymous arrow without binding context is dropped silently", () => {
    // No binding name to anchor the directive on. The detector cannot
    // report a name for these; they're typically passed inline as
    // callbacks where graph-walk doesn't find them either.
    const ast = parse(`
[1, 2, 3].map(async () => {
  "use server";
  return 0;
});
`);
    const found = detectFunctionLevelUseServer(ast);
    assert.equal(found.size, 0);
  });

  test("Multiple marked functions in one file", () => {
    const ast = parse(`
async function a() { "use server"; return 1; }
const b = async () => { "use server"; return 2; };
async function c() { return 3; }   // no directive
`);
    const found = detectFunctionLevelUseServer(ast);
    assert.deepEqual([...found].sort(), ["a", "b"]);
  });

  test("Export-prefix doesn't change the binding name", () => {
    const ast = parse(`
export async function exported() {
  "use server";
  return 42;
}
`);
    const found = detectFunctionLevelUseServer(ast);
    assert.deepEqual([...found], ["exported"]);
  });
});
