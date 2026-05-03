/**
 * Phase 5 — `proc.useMutation(...)` happy path + idempotency × retry.
 *
 * Two scenarios:
 *
 *   1. Happy path: `mutate(input)` invokes `call(input)`; the hook's
 *      `data` becomes the call result.
 *   2. Idempotency × retry (the load-bearing test from §10): when a
 *      mutation declares `idempotent: true`, all retries of a single
 *      `mutate()` reuse the SAME UUIDv7 idempotency key. Server-side
 *      Phase 6 dedupe replays the first response on retries 2-N.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import * as React from "react";
import * as TestRenderer from "react-test-renderer";
import { QueryClient } from "@tanstack/react-query";
import { __makeProcedure } from "@zeroship/rpc-client";

import { ZeroshipProvider } from "../src/index.js";

describe("useMutation — happy path", () => {
  test("mutate(input) calls the underlying procedure", async () => {
    const seen: Array<{ input: { text: string }; idempotencyKey: string | undefined }> = [];
    const add = __makeProcedure<{ text: string }, { id: number }>(
      async (input, options) => {
        seen.push({ input, idempotencyKey: options?.idempotencyKey });
        return { id: 7 };
      },
      { id: "todos.add", kind: "mutation" },
    );

    const qc = new QueryClient({
      defaultOptions: { mutations: { retry: false } },
    });

    let lastResult: ReturnType<typeof add.useMutation> | undefined;
    function Probe() {
      const m = add.useMutation();
      lastResult = m;
      return null;
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <ZeroshipProvider client={qc}>
          <Probe />
        </ZeroshipProvider>,
      );
    });

    await TestRenderer.act(async () => {
      (lastResult as { mutate: (i: { text: string }) => void }).mutate({
        text: "hi",
      });
      await new Promise((r) => setTimeout(r, 5));
    });

    assert.equal(seen.length, 1);
    assert.deepEqual(seen[0].input, { text: "hi" });
    // Non-idempotent mutation: no idempotency key is generated.
    assert.equal(seen[0].idempotencyKey, undefined);

    await TestRenderer.act(async () => {
      renderer?.unmount();
    });
  });
});

describe("useMutation — idempotency × retry (load-bearing)", () => {
  test("retries reuse the SAME UUIDv7 Idempotency-Key", async () => {
    let attempt = 0;
    const seen: Array<{ idempotencyKey: string | undefined }> = [];
    const add = __makeProcedure<{ text: string }, { id: number }>(
      async (_input, options) => {
        attempt++;
        seen.push({ idempotencyKey: options?.idempotencyKey });
        // First three attempts fail with a retryable error (504-ish);
        // the fourth succeeds. React Query's `retry: 3` config gives
        // us up to 4 total attempts.
        if (attempt < 4) {
          const err: Error & { retryable?: boolean } = new Error("boom");
          err.retryable = true;
          throw err;
        }
        return { id: 1 };
      },
      { id: "todos.add", kind: "mutation", idempotent: true },
    );

    const qc = new QueryClient();

    let lastResult: ReturnType<typeof add.useMutation> | undefined;
    function Probe() {
      const m = add.useMutation({ retry: 3, retryDelay: 1 });
      lastResult = m;
      return null;
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <ZeroshipProvider client={qc}>
          <Probe />
        </ZeroshipProvider>,
      );
    });

    await TestRenderer.act(async () => {
      (lastResult as { mutate: (i: { text: string }) => void }).mutate({
        text: "hi",
      });
      // Wait long enough for 3 retries (each 1ms apart per retryDelay).
      await new Promise((r) => setTimeout(r, 200));
    });

    assert.equal(seen.length, 4, `expected 4 attempts, got ${seen.length}`);
    // Every attempt has an idempotency key.
    for (const s of seen) {
      assert.ok(
        s.idempotencyKey,
        `every attempt should carry an Idempotency-Key, got ${s.idempotencyKey}`,
      );
      assert.match(
        s.idempotencyKey!,
        /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
        "idempotency key must be a UUIDv7",
      );
    }
    // ALL 4 attempts MUST share the same key — that's the contract.
    const k = seen[0].idempotencyKey!;
    for (let i = 1; i < seen.length; i++) {
      assert.equal(
        seen[i].idempotencyKey,
        k,
        `attempt ${i} key must equal attempt 0 key (got ${seen[i].idempotencyKey} vs ${k})`,
      );
    }

    await TestRenderer.act(async () => {
      renderer?.unmount();
    });
  });
});
