// sdks/vite-plugin/src/environment.ts
//
// Vite Environment API integration for zeroship's V8 runtime.
// Implements the same pattern as @cloudflare/vite-plugin.

import * as vite from "vite";
import type { WebSocket } from "ws";

// ── Types ──────────────────────────────────────────────────────────────────

/** Buffers outbound messages until the WebSocket connection is established. */
export interface WsContainer {
  ws?: WebSocket;
  buffer: string[];
  /** Stored onMessage handler so setWebSocket() can wire it directly. */
  onMessage?: (data: Buffer | string) => void;
}

// ── HotChannel ─────────────────────────────────────────────────────────────

/**
 * Creates a `vite.HotChannel` backed by a WebSocket container.
 *
 * - `send`   — JSON-stringifies the payload and writes to the WS; buffers if
 *              the WS is not yet connected.
 * - `on/off` — maintain a per-event listener map.
 * - `listen` — attaches the `onMessage` handler so incoming WS frames are
 *              dispatched to registered listeners.
 * - `close`  — removes the `onMessage` handler.
 */
export function createHotChannel(container: WsContainer): vite.HotChannel {
  // event → Set<listener>
  const listeners = new Map<string, Set<Function>>();

  /** The handler attached to ws.on("message", ...) — kept as a reference so
   *  we can detach it in close().  Also stored on the container so that
   *  setWebSocket() can attach it directly without re-calling listen(). */
  let messageHandler: ((data: Buffer | string) => void) | null = null;

  function onMessage(data: Buffer | string): void {
    let parsed: { event?: string; type?: string; data?: unknown };
    try {
      parsed = JSON.parse(typeof data === "string" ? data : data.toString("utf8"));
    } catch {
      return;
    }

    const event = parsed.event ?? parsed.type;
    if (!event) return;

    const eventListeners = listeners.get(event);
    if (!eventListeners) return;

    // Construct a minimal NormalizedHotChannelClient-compatible object.
    const client: vite.HotChannelClient = {
      send(payload) {
        const raw = JSON.stringify(payload);
        if (container.ws && container.ws.readyState === 1 /* OPEN */) {
          container.ws.send(raw);
        } else {
          container.buffer.push(raw);
        }
      },
    };

    for (const listener of eventListeners) {
      listener(parsed.data, client);
    }
  }

  return {
    // Let Vite know this transport is local (no fs-security restriction).
    skipFsCheck: true,

    send(payload: vite.HotPayload): void {
      const raw = JSON.stringify(payload);
      if (container.ws && container.ws.readyState === 1 /* OPEN */) {
        container.ws.send(raw);
      } else {
        container.buffer.push(raw);
      }
    },

    on(event: string, listener: Function): void {
      let set = listeners.get(event);
      if (!set) {
        set = new Set();
        listeners.set(event, set);
      }
      set.add(listener);
    },

    off(event: string, listener: Function): void {
      listeners.get(event)?.delete(listener);
    },

    listen(): void {
      // Always store the ref so setWebSocket() can attach it directly.
      messageHandler = onMessage;
      container.onMessage = onMessage;

      if (!container.ws) return;

      // Remove any previously registered handler before re-attaching.
      container.ws.off("message", messageHandler);
      container.ws.on("message", messageHandler);
    },

    close(): void {
      if (container.ws && messageHandler) {
        container.ws.off("message", messageHandler);
      }
      messageHandler = null;
      container.onMessage = undefined;
    },
  };
}

// ── DevEnvironment ─────────────────────────────────────────────────────────

/**
 * A `vite.DevEnvironment` that routes HMR messages over a WebSocket
 * connection to the zeroship V8 worker child process.
 *
 * Lifecycle:
 *  1. Constructed before the WS connection exists — messages are buffered.
 *  2. Once the child process connects, `setWebSocket()` is called:
 *     - The WS reference is stored on the container.
 *     - Buffered messages are flushed.
 *     - `this.hot.listen()` is re-called to wire the inbound message handler.
 */
export class ZeroshipDevEnvironment extends vite.DevEnvironment {
  private readonly _wsContainer: WsContainer;

  constructor(name: string, config: vite.ResolvedConfig) {
    const wsContainer: WsContainer = { buffer: [] };

    super(name, config, {
      hot: true,
      transport: createHotChannel(wsContainer),
    });

    this._wsContainer = wsContainer;
  }

  /**
   * Called by the dev-server plugin once the worker's WebSocket handshake
   * completes.  Flushes any buffered outbound messages and wires the inbound
   * message handler.
   */
  setWebSocket(ws: WebSocket): void {
    this._wsContainer.ws = ws;

    // Flush buffered outbound messages.
    for (const msg of this._wsContainer.buffer) {
      if (ws.readyState === 1 /* OPEN */) {
        ws.send(msg);
      }
    }
    this._wsContainer.buffer = [];

    // Attach the stored handler directly — avoids a double-call to listen()
    // and works correctly even if listen() was called before the WS arrived.
    if (this._wsContainer.onMessage) {
      ws.on("message", this._wsContainer.onMessage);
    } else {
      // listen() hasn't been called yet; fall back to calling it now.
      this.hot.listen();
    }
  }
}

// ── Environment options factory ────────────────────────────────────────────

/**
 * Returns the `vite.EnvironmentOptions` block to register the zeroship
 * server environment.  Place this under
 * `environments: { zeroship: createZeroshipEnvironmentOptions() }` in your
 * `vite.config.ts`.
 */
export function createZeroshipEnvironmentOptions(): vite.EnvironmentOptions {
  return {
    consumer: "server",
    resolve: {
      conditions: ["zeroship", "worker", "module"],
    },
    dev: {
      createEnvironment(name: string, config: vite.ResolvedConfig, _context?: any): ZeroshipDevEnvironment {
        return new ZeroshipDevEnvironment(name, config);
      },
    },
    build: { target: "es2024" },
    keepProcessEnv: true,
  };
}
