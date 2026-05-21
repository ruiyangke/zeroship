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
});
