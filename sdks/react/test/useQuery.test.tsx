/**
 * Tests for `useQuery` (P8b stage 4).
 *
 * Strategy
 * --------
 *
 * These are pure-React tests. The hook depends on:
 *
 *   - A `QueryClient` (broker subscription factory)
 *   - A `QueryLike` factory (anything thenable with `_collection`)
 *
 * Both are stubbed in-test: we hand-roll a `MockSubscription` with a
 * push-driven AsyncIterable, and a `mockQuery(...)` helper that returns
 * a thenable with a `_collection` field. No native runtime required.
 *
 * Tests live under the `b8b4_` prefix to track Stage 4 of the P8b
 * milestone (b = phase 8, 8b = read-set narrowing, 4 = useQuery layer).
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import * as React from "react";
import * as TestRenderer from "react-test-renderer";

import {
  useQuery,
  QueryClientProvider,
  type QueryClient,
  type SubscriptionLike,
} from "../src/index.js";

// ---------------------------------------------------------------------------
// Mock subscription — push-driven AsyncIterable
// ---------------------------------------------------------------------------

/**
 * A controllable subscription stand-in. Tests `.push()` events; the
 * iterator drains them in order. `.close()` ends iteration. The
 * iterator parks on a Promise until a new event or close.
 */
class MockSubscription implements SubscriptionLike {
  private queue: unknown[] = [];
  private waiters: Array<(v: IteratorResult<unknown>) => void> = [];
  closed = false;
  closeCalls = 0;

  push(event: unknown): void {
    if (this.closed) return;
    const waiter = this.waiters.shift();
    if (waiter) waiter({ value: event, done: false });
    else this.queue.push(event);
  }

  close(): void {
    this.closeCalls++;
    if (this.closed) return;
    this.closed = true;
    while (this.waiters.length) {
      this.waiters.shift()!({ value: undefined, done: true });
    }
  }

  [Symbol.asyncIterator](): AsyncIterator<unknown> {
    return {
      next: (): Promise<IteratorResult<unknown>> => {
        if (this.queue.length > 0) {
          return Promise.resolve({ value: this.queue.shift(), done: false });
        }
        if (this.closed) {
          return Promise.resolve({ value: undefined, done: true });
        }
        return new Promise((resolve) => this.waiters.push(resolve));
      },
      return: (): Promise<IteratorResult<unknown>> => {
        this.close();
        return Promise.resolve({ value: undefined, done: true });
      },
    };
  }
}

interface MockClient extends QueryClient {
  subs: MockSubscription[];
  byCollection: Map<string, MockSubscription[]>;
}

function makeClient(): MockClient {
  const subs: MockSubscription[] = [];
  const byCollection = new Map<string, MockSubscription[]>();
  return {
    subs,
    byCollection,
    subscribe(collection: string): SubscriptionLike {
      const sub = new MockSubscription();
      subs.push(sub);
      const list = byCollection.get(collection) ?? [];
      list.push(sub);
      byCollection.set(collection, list);
      return sub;
    },
  };
}

/**
 * Build a Query-shaped thenable. `_collection` is read by the hook to
 * open the broker subscription; `then` returns the canonical
 * `{ data, error }` envelope to match @zeroship/db's `Query.then`.
 */
function mockQuery<T>(collection: string, dataFn: () => T | Promise<T>): {
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
  // Two macrotask hops cover: (1) factory promise resolution,
  // (2) the iterator's `next()` registering its waiter so push() routes
  // to the live waiter, and (3) React state-commit batching.
  for (let i = 0; i < 3; i++) {
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  }
}

// ---------------------------------------------------------------------------
// b8b4_useQuery_initial_render_returns_undefined
// ---------------------------------------------------------------------------

describe("useQuery", () => {
  test("b8b4_useQuery_initial_render_returns_undefined", () => {
    const client = makeClient();
    let observed: unknown = "not-set";

    function Probe(): React.ReactElement | null {
      observed = useQuery(() => mockQuery("users", () => [{ id: 1 }]));
      return null;
    }

    TestRenderer.act(() => {
      TestRenderer.create(
        <QueryClientProvider client={client}>
          <Probe />
        </QueryClientProvider>,
      );
    });

    assert.equal(observed, undefined, "first render should observe undefined");
  });

  // -------------------------------------------------------------------------
  // b8b4_useQuery_resolves_with_initial_snapshot
  // -------------------------------------------------------------------------

  test("b8b4_useQuery_resolves_with_initial_snapshot", async () => {
    const client = makeClient();
    const rows = [{ id: 1, body: "hello" }, { id: 2, body: "world" }];
    let observed: unknown;

    function Probe(): React.ReactElement | null {
      observed = useQuery(() => mockQuery("messages", () => rows));
      return null;
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <QueryClientProvider client={client}>
          <Probe />
        </QueryClientProvider>,
      );
    });

    await TestRenderer.act(async () => {
      await flush();
    });

    assert.deepEqual(observed, rows, "snapshot should match returned rows");
    await TestRenderer.act(async () => { renderer?.unmount(); });
  });

  // -------------------------------------------------------------------------
  // b8b4_useQuery_re_renders_on_broker_event
  // -------------------------------------------------------------------------

  test("b8b4_useQuery_re_renders_on_broker_event", async () => {
    const client = makeClient();
    let counter = 0;
    const renders: unknown[] = [];

    function Probe(): React.ReactElement | null {
      const data = useQuery(() =>
        mockQuery("counter", () => {
          counter++;
          return [{ id: counter }];
        }),
      );
      renders.push(data);
      return null;
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <QueryClientProvider client={client}>
          <Probe />
        </QueryClientProvider>,
      );
    });

    // Drain the initial fetch.
    await TestRenderer.act(async () => {
      await flush();
    });

    const initialRenderCount = renders.length;
    const lastBefore = renders[renders.length - 1];
    assert.deepEqual(lastBefore, [{ id: 1 }], "initial snapshot id=1");

    // Push a broker event — should trigger a refetch and a re-render
    // with the next counter value.
    assert.equal(client.subs.length, 1, "exactly one broker subscription opened");
    const sub = client.subs[0]!;
    await TestRenderer.act(async () => {
      sub.push({ kind: "change", op: "insert", collection: "counter", pk: 99 });
      await flush();
    });

    const lastAfter = renders[renders.length - 1];
    assert.deepEqual(lastAfter, [{ id: 2 }], "post-event snapshot id=2");
    assert.ok(renders.length > initialRenderCount, "additional render committed");

    await TestRenderer.act(async () => { renderer?.unmount(); });
  });

  // -------------------------------------------------------------------------
  // b8b4_useQuery_cleanup_on_unmount
  // -------------------------------------------------------------------------

  test("b8b4_useQuery_cleanup_on_unmount", async () => {
    const client = makeClient();

    function Probe(): React.ReactElement | null {
      useQuery(() => mockQuery("cleanup", () => [{ id: 1 }]));
      return null;
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <QueryClientProvider client={client}>
          <Probe />
        </QueryClientProvider>,
      );
    });
    await TestRenderer.act(async () => { await flush(); });

    assert.equal(client.subs.length, 1, "one subscription opened");
    const sub = client.subs[0]!;
    assert.equal(sub.closeCalls, 0, "not yet closed");

    await TestRenderer.act(async () => { renderer?.unmount(); });
    assert.ok(sub.closeCalls >= 1, "subscription.close() called on unmount");
    assert.equal(sub.closed, true, "subscription marked closed");
  });

  // -------------------------------------------------------------------------
  // b8b4_useQuery_strictmode_safe
  // -------------------------------------------------------------------------
  //
  // Under React.StrictMode in development, components mount, immediately
  // unmount, and mount again. The hook's effect cleanup runs between
  // the two mounts. We expect:
  //
  //   - Two subscriptions opened (one per mount), but only ONE LIVE at
  //     any moment — the first mount's cleanup tears its sub down before
  //     the second mount opens its own.
  //   - Final state has exactly one live subscription whose unmount
  //     closes cleanly.
  //
  // This proves: no broker-side leak across StrictMode double-mount, AND
  // the steady-state is single-subscription per component instance.

  test("b8b4_useQuery_strictmode_safe", async () => {
    const client = makeClient();

    function Probe(): React.ReactElement | null {
      useQuery(() => mockQuery("strict", () => [{ id: 1 }]));
      return null;
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <QueryClientProvider client={client}>
          <React.StrictMode>
            <Probe />
          </React.StrictMode>
        </QueryClientProvider>,
      );
    });
    await TestRenderer.act(async () => { await flush(); });

    // StrictMode opens-then-closes-then-opens.
    // We accept >= 1 subs; what we INSIST on is that the closed-vs-open
    // count makes sense: exactly one alive at end of mount.
    const aliveAfterMount = client.subs.filter((s) => !s.closed).length;
    assert.equal(aliveAfterMount, 1, "exactly one live subscription after StrictMode mount");

    // Push an event — only the live sub should fire.
    const aliveSub = client.subs.find((s) => !s.closed)!;
    await TestRenderer.act(async () => {
      aliveSub.push({ kind: "change", op: "insert", collection: "strict", pk: 1 });
      await flush();
    });

    // Unmount fully — all subs should be closed.
    await TestRenderer.act(async () => { renderer?.unmount(); });
    const aliveAfterUnmount = client.subs.filter((s) => !s.closed).length;
    assert.equal(aliveAfterUnmount, 0, "no live subscriptions after unmount");
  });

  // -------------------------------------------------------------------------
  // b8b4_useQuery_handles_query_throw
  // -------------------------------------------------------------------------

  test("b8b4_useQuery_handles_query_throw", async () => {
    const client = makeClient();
    let observed: unknown = "not-set";

    function Probe(): React.ReactElement | null {
      try {
        observed = useQuery<unknown>(() =>
          mockQuery("bad", () => { throw new Error("boom"); }),
        );
      } catch (e) {
        observed = e;
      }
      return null;
    }

    function Boundary(props: { children: React.ReactNode }): React.ReactElement {
      // Minimal error boundary — useQuery throws on first-error/no-snapshot.
      const [err, setErr] = React.useState<Error | null>(null);
      return (
        <ErrorCatcher onError={setErr}>
          {err ? React.createElement("div", null, err.message) : props.children}
        </ErrorCatcher>
      );
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <QueryClientProvider client={client}>
          <Boundary>
            <Probe />
          </Boundary>
        </QueryClientProvider>,
      );
    });
    await TestRenderer.act(async () => { await flush(); });

    // The hook throws on first error w/ no snapshot. Either we observe
    // an Error in `observed` (if the error surfaced through the Probe's
    // try/catch) OR the error boundary caught and rendered the message.
    const tree = renderer?.toJSON();
    const errorRendered =
      tree !== null &&
      tree !== undefined &&
      JSON.stringify(tree).includes("boom");
    const observedError = observed instanceof Error && observed.message === "boom";
    assert.ok(
      errorRendered || observedError,
      "error should propagate to the boundary or the probe",
    );

    await TestRenderer.act(async () => { renderer?.unmount(); });
  });

  // -------------------------------------------------------------------------
  // b8b4_useQuery_dependency_change
  // -------------------------------------------------------------------------

  test("b8b4_useQuery_dependency_change", async () => {
    const client = makeClient();
    const renders: Array<unknown> = [];

    function Probe({ userId }: { userId: number }): React.ReactElement | null {
      const data = useQuery(() =>
        mockQuery("messages", () => [{ id: userId * 10 }]),
      );
      renders.push(data);
      return null;
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <QueryClientProvider client={client}>
          <Probe userId={1} />
        </QueryClientProvider>,
      );
    });
    await TestRenderer.act(async () => { await flush(); });

    const before = renders[renders.length - 1];
    assert.deepEqual(before, [{ id: 10 }], "snapshot reflects userId=1");

    // Change props — the factory closes over userId=2 now. Pushing a
    // broker event re-runs the factory and the hook commits the new
    // snapshot.
    await TestRenderer.act(async () => {
      renderer!.update(
        <QueryClientProvider client={client}>
          <Probe userId={2} />
        </QueryClientProvider>,
      );
    });
    const sub = client.subs.find((s) => !s.closed)!;
    await TestRenderer.act(async () => {
      sub.push({ kind: "change", op: "update", collection: "messages", pk: 1 });
      await flush();
    });

    const after = renders[renders.length - 1];
    assert.deepEqual(after, [{ id: 20 }], "post-event snapshot reflects userId=2");

    await TestRenderer.act(async () => { renderer?.unmount(); });
  });
});

// ---------------------------------------------------------------------------
// Minimal error boundary (React class — simplest hook-free option).
// ---------------------------------------------------------------------------

class ErrorCatcher extends React.Component<
  { children: React.ReactNode; onError: (e: Error) => void },
  { caught: boolean }
> {
  constructor(props: { children: React.ReactNode; onError: (e: Error) => void }) {
    super(props);
    this.state = { caught: false };
  }
  static getDerivedStateFromError(): { caught: boolean } {
    return { caught: true };
  }
  componentDidCatch(error: Error): void {
    this.props.onError(error);
  }
  render(): React.ReactNode {
    if (this.state.caught) return React.createElement("div", null, "error");
    return this.props.children;
  }
}
