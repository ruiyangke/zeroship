/**
 * `subscribe()` over WebSocket.
 *
 * Surface:
 *
 *   const handle = rpc.todoTicker.subscribe(undefined, {
 *     onData: (v) => ...,
 *     onError: (err) => ...,
 *     onEnd: () => ...,
 *     signal: controller.signal,
 *   });
 *   handle.unsubscribe();   // or controller.abort()
 *
 * Wire — see `docs/proposals/rpc.md` §6 (Subscription wire) and
 * `crates/runtime/src/init.rs::_zsAcceptSubscription`.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";

import { subscribeCall } from "../src/transport.js";
import type { TransportConfig } from "../src/transport.js";
import { RpcError } from "../src/error.js";

// ── MockWebSocket ──────────────────────────────────────────────────────
//
// Drop-in for the global WebSocket constructor with manual, test-driven
// control over the open / message / close / error events. Tests inject
// it via the `wsFactory` knob on `subscribeCall` (or equivalently by
// constructing the subscription URL and asserting on the recorded
// instance).

interface MockWsRecord {
  url: string;
  protocols: string[];
  ws: MockWebSocket;
  sent: string[];
  closed: { code: number; reason: string } | null;
}

class MockWebSocket extends EventEmitter implements WebSocket {
  // WebSocket constants the client reads to coerce readyState.
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  CONNECTING = 0;
  OPEN = 1;
  CLOSING = 2;
  CLOSED = 3;

  readyState: number = MockWebSocket.CONNECTING;
  url: string;
  protocol = "";
  extensions = "";
  bufferedAmount = 0;
  binaryType: BinaryType = "blob";

  // Spy state.
  sent: string[] = [];
  closed: { code: number; reason: string } | null = null;
  private listeners: Record<string, ((ev: unknown) => void)[]> = {};

  // We don't use `onopen` etc. — the client subscribes via
  // addEventListener.
  onopen: ((ev: Event) => unknown) | null = null;
  onmessage: ((ev: MessageEvent) => unknown) | null = null;
  onclose: ((ev: CloseEvent) => unknown) | null = null;
  onerror: ((ev: Event) => unknown) | null = null;

  constructor(url: string, _protocols?: string | string[]) {
    super();
    this.url = url;
  }

  // Browser-style listener API used by `subscribeCall`.
  addEventListener(type: string, listener: (ev: unknown) => void): void {
    (this.listeners[type] ??= []).push(listener);
  }
  removeEventListener(type: string, listener: (ev: unknown) => void): void {
    const list = this.listeners[type] ?? [];
    const idx = list.indexOf(listener);
    if (idx >= 0) list.splice(idx, 1);
  }
  dispatchEvent(ev: Event): boolean {
    const list = this.listeners[ev.type] ?? [];
    for (const l of list) l(ev as unknown);
    return true;
  }

  send(data: string | ArrayBufferLike | ArrayBufferView | Blob): void {
    if (this.readyState !== MockWebSocket.OPEN) {
      throw new Error(`MockWebSocket.send while readyState=${this.readyState}`);
    }
    this.sent.push(typeof data === "string" ? data : String(data));
  }

  close(code?: number, reason?: string): void {
    if (
      this.readyState === MockWebSocket.CLOSING ||
      this.readyState === MockWebSocket.CLOSED
    ) {
      return;
    }
    this.readyState = MockWebSocket.CLOSING;
    this.closed = { code: code ?? 1005, reason: reason ?? "" };
    // Schedule the close event on the next tick so callers can attach
    // listeners after `close()`.
    setImmediate(() => this.emitClose(code ?? 1005, reason ?? ""));
  }

  // ── Test driver methods ──────────────────────────────────────────────

  /** Mark the socket as open and dispatch the `open` event. */
  emitOpen(): void {
    this.readyState = MockWebSocket.OPEN;
    this.dispatchEvent({ type: "open" } as Event);
  }
  /** Dispatch a server-sent message (text frame). */
  emitMessage(data: string): void {
    this.dispatchEvent({ type: "message", data } as MessageEvent);
  }
  /** Dispatch a close event (and flip readyState). */
  emitClose(code: number, reason = ""): void {
    if (this.readyState === MockWebSocket.CLOSED) return;
    this.readyState = MockWebSocket.CLOSED;
    this.dispatchEvent({
      type: "close",
      code,
      reason,
      wasClean: code === 1000,
    } as unknown as CloseEvent);
  }
  /** Dispatch an error event (typically followed by a 1006 close). */
  emitError(): void {
    this.dispatchEvent({ type: "error" } as Event);
  }
}

/**
 * Spawn a fresh factory + recorder for a single test. The factory is
 * the function we hand to the client (via `subscribeCall`); each
 * invocation pushes a record onto `records[]`.
 */
function makeWsFactory(): {
  factory: (url: string, protocols?: string | string[]) => WebSocket;
  records: MockWsRecord[];
  next: () => MockWsRecord;
} {
  const records: MockWsRecord[] = [];
  return {
    records,
    factory: (url, protocols) => {
      const protos = Array.isArray(protocols)
        ? protocols
        : protocols
          ? [protocols]
          : [];
      const ws = new MockWebSocket(url, protos);
      const rec: MockWsRecord = { url, protocols: protos, ws, sent: ws.sent, closed: null };
      records.push(rec);
      return ws as unknown as WebSocket;
    },
    next: () => {
      const rec = records[records.length - 1];
      if (!rec) throw new Error("no MockWebSocket has been constructed yet");
      return rec;
    },
  };
}

/** Wait `n` ticks of the event loop. */
async function ticks(n: number): Promise<void> {
  for (let i = 0; i < n; i++) await new Promise<void>((r) => setImmediate(r));
}

/** Stub transport config — no real fetch, json transformer, no auth. */
function makeCfg(): TransportConfig {
  return {
    baseUrl: "https://api.test",
    fetch: globalThis.fetch ?? (async () => new Response("")),
    transformer: "json",
    authResolver: () => null,
  };
}

// ── Tests ──────────────────────────────────────────────────────────────

describe("subscribeCall — happy path", () => {
  test("opens WS, sends hello, dispatches data frames", async () => {
    const { factory, records } = makeWsFactory();
    const data: unknown[] = [];
    let ended = false;
    const handle = subscribeCall(
      "todoTicker",
      undefined,
      makeCfg(),
      {
        onData: (v) => data.push(v),
        onEnd: () => {
          ended = true;
        },
      },
      factory,
    );

    // Wait for the deferred connect to fire.
    await ticks(2);
    const rec = records[0];
    assert.ok(rec, "WebSocket constructed");
    assert.equal(rec.url, "wss://api.test/_zs/v1/todoTicker");
    assert.ok(
      rec.protocols.includes("zs.v1"),
      `expected zs.v1 protocol, got: ${rec.protocols.join(", ")}`,
    );

    // Open the socket and verify the hello frame went out.
    rec.ws.emitOpen();
    await ticks(1);
    assert.equal(rec.ws.sent.length, 1);
    const hello = JSON.parse(rec.ws.sent[0]);
    assert.equal(hello.t, "hello");
    assert.equal(hello.input, undefined);

    // Drive 3 data frames + an end frame.
    rec.ws.emitMessage(JSON.stringify({ t: "data", value: { tick: 0 } }));
    rec.ws.emitMessage(JSON.stringify({ t: "data", value: { tick: 1 } }));
    rec.ws.emitMessage(JSON.stringify({ t: "data", value: { tick: 2 } }));
    rec.ws.emitMessage(JSON.stringify({ t: "end" }));
    await ticks(2);

    assert.deepEqual(data, [{ tick: 0 }, { tick: 1 }, { tick: 2 }]);
    assert.equal(ended, true);
    handle.unsubscribe();
  });

  test("input non-undefined is wrapped per-transformer and sent in hello", async () => {
    const { factory, records } = makeWsFactory();
    subscribeCall("search", { q: "build" }, makeCfg(), { onData: () => {} }, factory);
    await ticks(2);
    const rec = records[0];
    rec.ws.emitOpen();
    await ticks(1);
    const hello = JSON.parse(rec.ws.sent[0]);
    assert.equal(hello.t, "hello");
    // json transformer → hello.input is the input value verbatim.
    assert.deepEqual(hello.input, { q: "build" });
  });

  test("ping is auto-acked with pong", async () => {
    const { factory, records } = makeWsFactory();
    subscribeCall("s", undefined, makeCfg(), { onData: () => {} }, factory);
    await ticks(2);
    const rec = records[0];
    rec.ws.emitOpen();
    await ticks(1);
    rec.ws.sent.length = 0; // discard hello
    rec.ws.emitMessage(JSON.stringify({ t: "ping" }));
    await ticks(1);
    assert.deepEqual(rec.ws.sent, [JSON.stringify({ t: "pong" })]);
  });
});

describe("subscribeCall — errors", () => {
  test("error frame fires onError with envelope code/message", async () => {
    const { factory, records } = makeWsFactory();
    let received: RpcError | null = null;
    subscribeCall(
      "s",
      undefined,
      makeCfg(),
      {
        onData: () => {},
        onError: (err) => {
          received = err;
        },
      },
      factory,
    );
    await ticks(2);
    const rec = records[0];
    rec.ws.emitOpen();
    await ticks(1);
    rec.ws.emitMessage(
      JSON.stringify({
        t: "error",
        error: {
          code: "PERMISSION_DENIED",
          message: "no go",
          retryable: false,
        },
      }),
    );
    await ticks(1);
    assert.ok(received instanceof RpcError, "onError fired");
    const err = received as unknown as RpcError;
    assert.equal(err.code, "PERMISSION_DENIED");
    assert.equal(err.message, "no go");
  });

  test("unknown frame tags are ignored (forward-compat)", async () => {
    const { factory, records } = makeWsFactory();
    let dataCount = 0;
    let errorCount = 0;
    subscribeCall(
      "s",
      undefined,
      makeCfg(),
      {
        onData: () => {
          dataCount += 1;
        },
        onError: () => {
          errorCount += 1;
        },
      },
      factory,
    );
    await ticks(2);
    const rec = records[0];
    rec.ws.emitOpen();
    await ticks(1);
    rec.ws.emitMessage(JSON.stringify({ t: "future-tag", x: 1 }));
    rec.ws.emitMessage(JSON.stringify({ t: "data", value: 1 }));
    await ticks(1);
    assert.equal(dataCount, 1);
    assert.equal(errorCount, 0);
  });
});

describe("subscribeCall — lifecycle", () => {
  test("AbortSignal closes the connection cleanly", async () => {
    const { factory, records } = makeWsFactory();
    const ctrl = new AbortController();
    subscribeCall(
      "s",
      undefined,
      makeCfg(),
      { onData: () => {}, signal: ctrl.signal },
      factory,
    );
    await ticks(2);
    const rec = records[0];
    rec.ws.emitOpen();
    await ticks(1);
    ctrl.abort();
    // The handle's teardown path calls `close(1000)`.
    assert.equal(rec.ws.closed?.code, 1000);
  });

  test("unsubscribe() closes once open", async () => {
    const { factory, records } = makeWsFactory();
    const handle = subscribeCall(
      "s",
      undefined,
      makeCfg(),
      { onData: () => {} },
      factory,
    );
    await ticks(2);
    const rec = records[0];
    rec.ws.emitOpen();
    await ticks(1);
    handle.unsubscribe();
    assert.equal(rec.ws.closed?.code, 1000);
  });
});

describe("subscribeCall — reconnect", () => {
  test("abnormal closure (1006) triggers reconnect; second WS sends hello again", async () => {
    const { factory, records } = makeWsFactory();
    subscribeCall(
      "s",
      undefined,
      makeCfg(),
      {
        onData: () => {},
        maxReconnectMs: 10,
      },
      factory,
    );
    await ticks(2);
    const first = records[0];
    first.ws.emitOpen();
    await ticks(1);
    // Abnormal close.
    first.ws.emitClose(1006, "");
    // Wait for the reconnect timer (10ms cap + jitter ≤ 15ms).
    await new Promise((r) => setTimeout(r, 30));
    assert.equal(records.length, 2, "should have reconnected");
    const second = records[1];
    second.ws.emitOpen();
    await ticks(1);
    // Second connection sent the same hello again.
    assert.equal(second.ws.sent.length, 1);
    const hello = JSON.parse(second.ws.sent[0]);
    assert.equal(hello.t, "hello");
  });

  test("normal closure (1000) does NOT reconnect", async () => {
    const { factory, records } = makeWsFactory();
    subscribeCall(
      "s",
      undefined,
      makeCfg(),
      { onData: () => {}, maxReconnectMs: 10 },
      factory,
    );
    await ticks(2);
    const first = records[0];
    first.ws.emitOpen();
    await ticks(1);
    first.ws.emitClose(1000, "");
    await new Promise((r) => setTimeout(r, 30));
    assert.equal(records.length, 1, "should NOT reconnect on 1000");
  });

  test("noReconnect: true surfaces 1006 as onError ABORTED", async () => {
    const { factory, records } = makeWsFactory();
    let err: RpcError | null = null;
    subscribeCall(
      "s",
      undefined,
      makeCfg(),
      {
        onData: () => {},
        onError: (e) => {
          err = e;
        },
        noReconnect: true,
      },
      factory,
    );
    await ticks(2);
    const first = records[0];
    first.ws.emitOpen();
    await ticks(1);
    first.ws.emitClose(1006, "");
    await ticks(1);
    assert.ok(err);
    assert.equal((err as unknown as RpcError).code, "ABORTED");
    assert.equal(records.length, 1);
  });
});
