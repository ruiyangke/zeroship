/**
 * `proc.useStream(...)` and the `useStream(...)` standalone hook.
 *
 * `docs/proposals/rpc-v2.md` §10 ("Streaming + React Query") defines:
 *
 *   const { chunks, isStreaming, error, cancel } = search.useStream({ query: "..." });
 *   // chunks: T[] — appended as the server yields
 *   // isStreaming: true until the SSE 'd:' frame
 *   // error: Error | null
 *   // cancel: () => void  — also fires on unmount
 *
 * Implementation: thin wrapper over `streamCall` (or any caller-supplied
 * AsyncIterable factory). State accumulates yields in a `chunks` array;
 * `isStreaming` flips false on EOF or error. Cleanup on unmount calls
 * the underlying iterator's `return()` (cancel signal).
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import * as React from "react";
import * as TestRenderer from "react-test-renderer";
import { QueryClient } from "@tanstack/react-query";
import { __makeProcedure } from "@zeroship/rpc-client";

import { ZeroshipProvider } from "../src/index.js";

/** Build an AsyncIterable that yields the given values with a tiny delay. */
function fixtureStream<T>(values: T[]): AsyncIterableIterator<T> {
  let i = 0;
  let cancelled = false;
  const iter: AsyncIterableIterator<T> = {
    async next(): Promise<IteratorResult<T>> {
      if (cancelled) return { value: undefined as unknown as T, done: true };
      if (i >= values.length) return { value: undefined as unknown as T, done: true };
      // Simulate an async wait between yields.
      await new Promise((r) => setTimeout(r, 1));
      return { value: values[i++], done: false };
    },
    async return(): Promise<IteratorResult<T>> {
      cancelled = true;
      return { value: undefined as unknown as T, done: true };
    },
    [Symbol.asyncIterator](): AsyncIterableIterator<T> {
      return iter;
    },
  };
  return iter;
}

describe("useStream — chunks accumulate", () => {
  test("yields collect into a chunks array; isStreaming flips false at EOF", async () => {
    const search = __makeProcedure<{ q: string }, { id: number }>(
      () => fixtureStream([{ id: 1 }, { id: 2 }, { id: 3 }]),
      { id: "todos.search", kind: "stream" },
    );

    const qc = new QueryClient();

    let lastResult: ReturnType<typeof search.useStream> | undefined;
    function Probe() {
      const r = search.useStream({ q: "hi" });
      lastResult = r;
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

    // Drain the stream — three yields × 1ms each plus a render-cycle.
    await TestRenderer.act(async () => {
      await new Promise((r) => setTimeout(r, 50));
    });

    const r = lastResult as
      | { chunks: unknown[]; isStreaming: boolean; error: Error | null; cancel: () => void }
      | undefined;
    assert.ok(r, "useStream must produce a result");
    assert.deepEqual(r!.chunks, [{ id: 1 }, { id: 2 }, { id: 3 }]);
    assert.equal(r!.isStreaming, false);
    assert.equal(r!.error, null);

    await TestRenderer.act(async () => {
      renderer?.unmount();
    });
  });

  test("error during stream populates `error`; isStreaming flips false", async () => {
    async function* badStream(): AsyncGenerator<{ x: number }> {
      yield { x: 1 };
      throw new Error("boom");
    }
    const search = __makeProcedure<void, { x: number }>(
      () => badStream(),
      { id: "broken.stream", kind: "stream" },
    );

    const qc = new QueryClient();
    let lastResult: ReturnType<typeof search.useStream> | undefined;
    function Probe() {
      const r = search.useStream(undefined as unknown as void);
      lastResult = r;
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
      await new Promise((r) => setTimeout(r, 50));
    });

    const r = lastResult as
      | { chunks: unknown[]; isStreaming: boolean; error: Error | null }
      | undefined;
    assert.ok(r);
    assert.deepEqual(r!.chunks, [{ x: 1 }]);
    assert.equal(r!.isStreaming, false);
    assert.ok(r!.error instanceof Error);
    assert.match(r!.error!.message, /boom/);

    await TestRenderer.act(async () => {
      renderer?.unmount();
    });
  });
});
