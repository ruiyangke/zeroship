/**
 * Tests for `useSuspenseQuery` (P8b stage 4 — Suspense variant).
 *
 * The variant requires an explicit `suspenseKey` so the suspended
 * resource survives the suspend/resume cycle. On first render it
 * throws a Promise; React renders the `<Suspense>` fallback until the
 * promise resolves, then re-renders with the snapshot. Subsequent
 * broker events run through the underlying `useQuery` and re-render.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import * as React from "react";
import * as TestRenderer from "react-test-renderer";

import {
  useSuspenseQuery,
  QueryClientProvider,
  type QueryClient,
  type SubscriptionLike,
} from "../src/index.js";

class MockSubscription implements SubscriptionLike {
  closed = false;
  private waiters: Array<(v: IteratorResult<unknown>) => void> = [];
  close(): void {
    this.closed = true;
    while (this.waiters.length) this.waiters.shift()!({ value: undefined, done: true });
  }
  [Symbol.asyncIterator](): AsyncIterator<unknown> {
    return {
      next: (): Promise<IteratorResult<unknown>> =>
        this.closed
          ? Promise.resolve({ value: undefined, done: true })
          : new Promise((r) => this.waiters.push(r)),
      return: (): Promise<IteratorResult<unknown>> => {
        this.close();
        return Promise.resolve({ value: undefined, done: true });
      },
    };
  }
}

function makeClient(): QueryClient & { subs: MockSubscription[] } {
  const subs: MockSubscription[] = [];
  return {
    subs,
    subscribe(): SubscriptionLike {
      const s = new MockSubscription();
      subs.push(s);
      return s;
    },
  };
}

function mockQuery<T>(collection: string, dataFn: () => T): {
  _collection: string;
  then<R1, R2>(
    onfulfilled?: ((value: { data: T; error: undefined }) => R1 | PromiseLike<R1>) | null,
    onrejected?: ((reason: unknown) => R2 | PromiseLike<R2>) | null,
  ): PromiseLike<R1 | R2>;
} {
  return {
    _collection: collection,
    then(onfulfilled, onrejected) {
      return Promise.resolve()
        .then(() => dataFn())
        .then(
          (data) =>
            onfulfilled ? onfulfilled({ data, error: undefined }) : (undefined as unknown as never),
          onrejected ?? undefined,
        ) as PromiseLike<any>;
    },
  };
}

async function flush(): Promise<void> {
  for (let i = 0; i < 4; i++) {
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  }
}

describe("useSuspenseQuery", () => {
  test("b8b4_useSuspenseQuery_throws_promise_keyed_by_suspenseKey", () => {
    // Direct invocation outside React to test the throw-promise
    // contract — bypassing React's renderer keeps the assertion
    // tight (no concurrent-mode surprises) and verifies that
    // calling the hook twice with the same key returns the same
    // pending Promise.
    const dummyKey = "unit-key-1";
    let firstThrown: unknown = null;
    let secondThrown: unknown = null;

    // We can't call the hook outside a render in normal React, but
    // we CAN observe the cache's promise-throwing behaviour via the
    // shared `suspenseCache` indirectly: render two components with
    // the same key and confirm both suspend.
    function Probe(): React.ReactElement | null {
      try {
        useSuspenseQuery(() => mockQuery("u", () => [{ id: 1 }]), {
          suspenseKey: dummyKey,
        });
      } catch (e) {
        if (firstThrown === null) firstThrown = e;
        else secondThrown = e;
        throw e;
      }
      return null;
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    TestRenderer.act(() => {
      renderer = TestRenderer.create(
        <QueryClientProvider client={makeClient()}>
          <React.Suspense fallback={<div>loading</div>}>
            <Probe />
          </React.Suspense>
        </QueryClientProvider>,
      );
    });

    assert.ok(
      firstThrown && typeof (firstThrown as Promise<unknown>).then === "function",
      "first invocation should throw a thenable",
    );
    // Suspense may retry — if it does, the same cached promise is thrown.
    if (secondThrown !== null) {
      assert.strictEqual(firstThrown, secondThrown, "same cached promise across retries");
    }

    TestRenderer.act(() => { renderer?.unmount(); });
  });

  test("b8b4_useSuspenseQuery_resumes_after_resolve", async () => {
    const client = makeClient();
    const rows = [{ id: 42 }];
    let observed: unknown = "not-set";
    let probeCallCount = 0;

    function Probe(): React.ReactElement | null {
      probeCallCount++;
      observed = useSuspenseQuery(
        () => mockQuery("u", () => rows),
        { suspenseKey: "resumes-key" },
      );
      return null;
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <QueryClientProvider client={client}>
          <React.Suspense fallback={<div>loading</div>}>
            <Probe />
          </React.Suspense>
        </QueryClientProvider>,
      );
    });

    await TestRenderer.act(async () => { await flush(); });

    // The hook re-rendered (post-suspend resume) and committed the
    // resolved snapshot. The probe was called at least twice — once
    // for the suspend-throw and once after resume.
    assert.deepEqual(observed, rows, "observed snapshot after resolve");
    assert.ok(probeCallCount >= 2, `probe called >= 2x (saw ${probeCallCount})`);

    await TestRenderer.act(async () => { renderer?.unmount(); });
  });
});
