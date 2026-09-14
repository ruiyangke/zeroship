/**
 * Tests for `resolveEnv` (`src/internal/env.ts`) — the ambient-globals resolver.
 *
 * Regression focus: the default platform `fetch` must be **bound to the global**.
 * `Transport` invokes it as a property (`this.fetchImpl(...)`), which calls the
 * platform `fetch` with `this` = the Transport instance — and browsers reject
 * that with `TypeError: Illegal invocation`, failing EVERY auth network call
 * before a request is even made. The unit suites inject a fake `env.fetch`, so
 * they never exercised the default binding; this test does.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { resolveEnv } from "../src/internal/env.js";

describe("resolveEnv — platform fetch binding", () => {
  test("default fetch is bound to the global (no Illegal invocation when called as a property)", async () => {
    const g = globalThis as unknown as { fetch?: unknown };
    const prev = g.fetch;
    const seen: string[] = [];
    // A fake platform fetch that mimics the browser: throw if `this` is not the
    // global (exactly what `window.fetch` does when called unbound as a property).
    g.fetch = function (this: unknown, url: unknown) {
      if (this !== undefined && this !== globalThis) {
        throw new TypeError("Illegal invocation");
      }
      seen.push(String(url));
      return Promise.resolve(new Response("{}"));
    };
    try {
      const env = resolveEnv();
      // Invoke as a PROPERTY of another object — mirrors `transport.fetchImpl(...)`.
      const holder = { f: env.fetch };
      await holder.f("https://example.test/x");
      assert.deepEqual(seen, ["https://example.test/x"], "the bound fetch must run with the right `this`");
    } finally {
      g.fetch = prev;
    }
  });

  test("an explicitly-supplied fetch is used verbatim (no surprise rebinding)", async () => {
    const seen: string[] = [];
    const custom = (url: unknown) => {
      seen.push(String(url));
      return Promise.resolve(new Response("{}"));
    };
    const env = resolveEnv({ fetch: custom });
    await env.fetch("https://example.test/y");
    assert.deepEqual(seen, ["https://example.test/y"]);
  });
});
