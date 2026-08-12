/**
 * `probeUserDefaultExport` — detects whether the user's source carries
 * an own `default.fetch` handler.
 *
 * Stage 5b — the new synthetic entry always emits `fetch:` on its
 * default export (it surfaces the user's fetch when present, else
 * undefined). The .zship emitter still needs to choose between Worker
 * (SSR) and Static SPA catch-all rules, so we probe the USER's source
 * — not the synthetic output.
 *
 * The probe is heuristic on purpose (full AST analysis would over-
 * fit). Conservative on ambiguity: returns true so an unwanted Worker
 * (SSR) 404s instead of letting a stale shell serve on intended SSR
 * routes.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { probeUserDefaultExport } from "../src/build.js";

describe("probeUserDefaultExport", () => {
  test("no default export → false", () => {
    assert.equal(probeUserDefaultExport(`export const x = 1;`), false);
    assert.equal(probeUserDefaultExport(``), false);
  });

  test("export default function fetch(...) → true", () => {
    assert.equal(
      probeUserDefaultExport(`export default function fetch(req) { return new Response("hi"); }`),
      true,
    );
    assert.equal(
      probeUserDefaultExport(`export default async function fetch(req) { return new Response("hi"); }`),
      true,
    );
  });

  test("export default { fetch: ... } → true", () => {
    assert.equal(
      probeUserDefaultExport(`export default { fetch: (req) => new Response("hi") };`),
      true,
    );
    assert.equal(
      probeUserDefaultExport(`export default {\n  fetch(req) { return new Response("hi"); }\n};`),
      true,
    );
    // Shorthand `{ fetch }` form.
    assert.equal(
      probeUserDefaultExport(`function fetch(req) { return new Response("hi"); }\nexport default { fetch };`),
      true,
    );
    // Multiple keys including fetch.
    assert.equal(
      probeUserDefaultExport(
        `export default {\n  schema: {},\n  fetch: (req) => new Response("hi"),\n  rpc: {},\n};`,
      ),
      true,
    );
    assert.equal(
      probeUserDefaultExport(`export default { "fetch": (req) => new Response("hi") };`),
      true,
    );
  });

  test("export default { rpc: ... } (no fetch) → false (RPC-only app)", () => {
    assert.equal(
      probeUserDefaultExport(`export default { rpc: { list: () => [] } };`),
      false,
    );
    assert.equal(
      probeUserDefaultExport(`export default {\n  schema: {},\n  rpc: { list: () => [] },\n};`),
      false,
    );
  });

  test("export default { schema: ... } (no fetch, no rpc) → false", () => {
    assert.equal(
      probeUserDefaultExport(`export default { schema: { todos: {} } };`),
      false,
    );
  });

  test("export default <identifier> with module-level `function fetch` → true", () => {
    assert.equal(
      probeUserDefaultExport(
        `function fetch(req) { return new Response("hi"); }\nexport default fetch;`,
      ),
      true,
    );
  });

  test("export default <opaque expr> → conservative true", () => {
    // Unknown shape — assume yes; an unwanted Worker(SSR) is preferable
    // to a stale-shell SPA fallback.
    assert.equal(
      probeUserDefaultExport(`import handler from "./handler.js"; export default handler;`),
      true,
    );
  });

  test("comments aren't false-positive sources", () => {
    // `// fetch:` in a comment must NOT make a rpc-only app look like
    // it has fetch. The probe strips comments first.
    assert.equal(
      probeUserDefaultExport(
        `// example: export default { fetch: ... }\nexport default { rpc: { x: () => 1 } };`,
      ),
      false,
    );
    assert.equal(
      probeUserDefaultExport(
        `/* example fetch: ... */\nexport default { rpc: {} };`,
      ),
      false,
    );
  });

  test("fetch-like property names stay false when the default has no fetch handler", () => {
    assert.equal(
      probeUserDefaultExport(`export default { fetchTimeout: 1, rpc: {} };`),
      false,
    );
    assert.equal(
      probeUserDefaultExport(`export default { "fetch-ish": 1, rpc: {} };`),
      false,
    );
  });
});

// ---------------------------------------------------------------------------
// Regression: the object-literal scan must stop at the literal's OWN closing
// brace.
//
// The probe used a greedy `/export\s+default\s+(\{[\s\S]*\})/`, which captured
// from the literal's `{` to the LAST `}` in the file. MEASURED 2026-08-11 on
// examples/db-todos (`export default { schema: dbSchema };`): the capture was
// 18506 chars and matched ` fetch(` from an `await fetch(webhookUrl, ...)` in
// an unrelated action, so the probe said "user owns routing", the .zship got a
// Worker(SSR) catch-all instead of the `["$path", "/index.html"]` SPA fallback,
// and the DEPLOYED app 404ed `/` and `/index.html` while /assets/* served 200.
//
// WHAT THESE DO NOT COVER: the probe is still not a parser. A `}` inside a
// regex literal in the entry ends the scan early. That fails toward a shorter
// block and so toward the conservative `true`, but it is untested here.
describe("probeUserDefaultExport — brace matching", () => {
  test("a fetch() CALL after the default object does not count as a fetch handler", () => {
    // This is the db-todos shape, minimised. Greedy capture => true (wrong).
    const src = [
      `export default { schema: dbSchema };`,
      ``,
      `export const share = action(async ({ url }) => {`,
      `  const resp = await fetch(url, { method: "POST" });`,
      `  return resp.ok;`,
      `});`,
    ].join("\n");
    assert.equal(probeUserDefaultExport(src), false);
  });

  test("a NESTED brace before the real top-level fetch still resolves to true", () => {
    // The case a lazy `[\s\S]*?` would break: it stops at the first `}` and
    // never reaches `fetch`, serving a stale shell on a real SSR route.
    assert.equal(
      probeUserDefaultExport(`export default { opts: { a: 1 }, fetch: handler };`),
      true,
    );
  });

  test("a fetch key inside a NESTED object is not a top-level handler", () => {
    const src = [
      `export default { rpc: { helpers: { fetch: internalFetch } } };`,
      `export const x = 1;`,
    ].join("\n");
    assert.equal(probeUserDefaultExport(src), false);
  });
});
