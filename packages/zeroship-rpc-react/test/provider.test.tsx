/**
 * Phase 5 — `<ZeroshipProvider>` populates the rpc-client hook registry.
 *
 * Importing `@zeroship/rpc-react` should:
 *
 *   - Side-effect-populate `_hookRegistry.useQuery` / `useMutation` /
 *     `useInfiniteQuery` / `useSuspenseQuery` / `useStream` so any
 *     procedure created with `__makeProcedure` from `@zeroship/rpc-client`
 *     can resolve its hook getters.
 *   - Mounting `<ZeroshipProvider client={qc}>` additionally stashes
 *     `qc` on `_hookRegistry.queryClient` so `proc.invalidate(...)`,
 *     `proc.prefetch(...)`, and the `rpcInvalidate("prefix.")` helper
 *     work without threading the QueryClient through every call site.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import * as React from "react";
import * as TestRenderer from "react-test-renderer";
import { QueryClient } from "@tanstack/react-query";

// Side-effect import: populates `_hookRegistry` on module evaluation.
import { ZeroshipProvider } from "../src/index.js";
import { _hookRegistry } from "@zeroship/rpc-client/_hooks";

describe("ZeroshipProvider — registry side effects", () => {
  test("importing @zeroship/rpc-react populates hook slots", () => {
    assert.equal(typeof _hookRegistry.useQuery, "function");
    assert.equal(typeof _hookRegistry.useMutation, "function");
    assert.equal(typeof _hookRegistry.useInfiniteQuery, "function");
    assert.equal(typeof _hookRegistry.useSuspenseQuery, "function");
    assert.equal(typeof _hookRegistry.useStream, "function");
  });

  test("mounting <ZeroshipProvider> stashes the QueryClient", async () => {
    const qc = new QueryClient();
    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <ZeroshipProvider client={qc}>
          <span>x</span>
        </ZeroshipProvider>,
      );
    });
    assert.equal(_hookRegistry.queryClient, qc);
    await TestRenderer.act(async () => {
      renderer?.unmount();
    });
  });
});
