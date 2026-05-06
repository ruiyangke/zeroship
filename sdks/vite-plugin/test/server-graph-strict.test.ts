/**
 * RPC v2 Phase 2 — strict-mode posture resolution + dev-vs-production
 * gate behavior (proposal §1).
 *
 *   - "auto" (default): strict in production, lenient in dev.
 *   - "always": strict regardless of mode.
 *   - "never": lenient regardless of mode.
 *
 * The strict gate applies AFTER the reference-graph walk and only
 * touches bindings whose `marker === "graph"`. The walk itself doesn't
 * change behavior between modes.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { resolveRpcStrict } from "../src/index.js";
import { strictModeGate, type ServerBinding } from "../src/server-graph.js";

function graphOnlyBinding(): Map<string, ServerBinding> {
  return new Map([
    [
      "/src/foo.ts::stub",
      {
        wireId: "stub",
        sourceFile: "/src/foo.ts",
        exportName: "stub",
        kind: "mutation",
        marker: "graph",
        chain: ["/src/client.ts", "/src/foo.ts"],
      },
    ],
  ]);
}

describe("resolveRpcStrict — posture from option + mode", () => {
  test("'auto' + production → 'always'", () => {
    assert.equal(resolveRpcStrict("auto", "production"), "always");
  });

  test("'auto' + development → 'never'", () => {
    assert.equal(resolveRpcStrict("auto", "development"), "never");
  });

  test("undefined defaults to 'auto'", () => {
    assert.equal(resolveRpcStrict(undefined, "production"), "always");
    assert.equal(resolveRpcStrict(undefined, "development"), "never");
  });

  test("'always' overrides mode", () => {
    assert.equal(resolveRpcStrict("always", "development"), "always");
  });

  test("'never' overrides mode", () => {
    assert.equal(resolveRpcStrict("never", "production"), "never");
  });
});

describe("strict-mode gate — production rejects, dev accepts", () => {
  test("production: graph-only binding → build error", () => {
    const bindings = graphOnlyBinding();
    const posture = resolveRpcStrict("auto", "production");
    assert.throws(() => strictModeGate(bindings, posture), /strict-mode/i);
  });

  test("development: graph-only binding allowed", () => {
    const bindings = graphOnlyBinding();
    const posture = resolveRpcStrict("auto", "development");
    // No throw.
    strictModeGate(bindings, posture);
  });

  test("'always' rejects in dev too", () => {
    const bindings = graphOnlyBinding();
    assert.throws(
      () => strictModeGate(bindings, resolveRpcStrict("always", "development")),
      /strict-mode/i,
    );
  });

  test("'never' tolerates in production", () => {
    const bindings = graphOnlyBinding();
    strictModeGate(bindings, resolveRpcStrict("never", "production"));
  });
});
