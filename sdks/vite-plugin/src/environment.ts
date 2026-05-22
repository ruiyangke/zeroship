// sdks/vite-plugin/src/environment.ts
//
// Vite Environment API integration for zeroship's V8 runtime.
// Implements the same pattern as @cloudflare/vite-plugin.

import * as vite from "vite";
import type { WebSocket } from "ws";
import type { FetchFunctionOptions } from "vite/module-runner";
import { getNodeCompatId, getCustomPolyfillCode, isRuntimeNative } from "./node-compat.js";

const MAX_WS_BUFFER = 1000;

// ── Types ──────────────────────────────────────────────────────────────────

/** Buffers outbound messages until the WebSocket connection is established. */
export interface WsContainer {
  ws?: WebSocket;
  buffer: string[];
  /** Stored onMessage handler so setWebSocket() can wire it directly. */
  onMessage?: (data: Buffer | string) => void;
}

/** Listener type matching Vite's HotChannel overloaded signatures. */
type HotListener = (...args: any[]) => void;

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
  const listeners = new Map<string, Set<HotListener>>();

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
        } else if (container.buffer.length < MAX_WS_BUFFER) {
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
      } else if (container.buffer.length < MAX_WS_BUFFER) {
        container.buffer.push(raw);
      }
    },

    on(event: string, listener: HotListener): void {
      let set = listeners.get(event);
      if (!set) {
        set = new Set();
        listeners.set(event, set);
      }
      set.add(listener);
    },

    off(event: string, listener: HotListener): void {
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
    if (this._wsContainer.ws) {
      (this._wsContainer.ws as any).removeAllListeners?.("message");
    }
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

  /**
   * Override fetchModule to intercept node:* builtins.
   *
   * Vite treats node:* as builtins and returns `{ externalize }` BEFORE
   * any plugin resolveId hook runs. Since our V8 runtime can't import
   * node: modules natively, we intercept them here and route to either:
   *   - Custom polyfill (crypto) → return code directly
   *   - unenv polyfill (buffer, path, etc.) → rewrite to unenv path, fetch normally
   */
  override async fetchModule(
    id: string,
    importer?: string,
    _options?: FetchFunctionOptions,
  ): Promise<vite.FetchResult> {
    // Check if this is a node:* or bare builtin we can polyfill
    const isNodeish = id.startsWith("node:") || /^(crypto|buffer|path|util|events|stream|os|url|http|https|fs|assert|process|async_hooks|timers|string_decoder|querystring|punycode|net|tls|dns|zlib|worker_threads|diagnostics_channel|perf_hooks|module)(\/.+)?$/.test(id);

    if (isNodeish) {
      // Runtime-native specifiers (`node:async_hooks`, `node:crypto`)
      // are owned by the V8 runtime's SyntheticModule loader. In dev,
      // ModuleRunner can't issue native imports — bridge through the
      // runtime-installed `__zeroshipNodeBuiltin` helper that returns
      // the same namespace object the synthetic module exposes.
      if (isRuntimeNative(id)) {
        const code = `
const m = globalThis.__zeroshipNodeBuiltin && globalThis.__zeroshipNodeBuiltin(${JSON.stringify(id)});
if (!m) throw new Error(${JSON.stringify(`${id}: runtime native module helper missing`)});
Object.assign(__vite_ssr_exports__, m, { default: m.default ?? m });
`;
        return { id, url: id, code, file: id } as vite.FetchResult;
      }

      const compatId = getNodeCompatId(id);
      if (compatId) {
        // Custom polyfill (e.g. crypto) → return code directly
        const code = getCustomPolyfillCode(compatId);
        if (code) {
          return { id, url: id, code, file: id } as vite.FetchResult;
        }
        // unenv polyfill → resolve and transform through Vite's pipeline.
        // We use transformRequest() which runs resolveId → load → transform,
        // converting the unenv module to SSR-compatible code.
        const transformed = await this.transformRequest(compatId);
        if (transformed) {
          return { id, url: compatId, code: transformed.code, file: compatId } as vite.FetchResult;
        }
      }
    }

    return super.fetchModule(id, importer, _options);
  }
}

// ── Environment options factory ────────────────────────────────────────────

/**
 * Returns the `vite.EnvironmentOptions` block to register the zeroship
 * server environment.  Place this under
 * `environments: { zeroship: createZeroshipEnvironmentOptions() }` in your
 * `vite.config.ts`.
 *
 * @param serverEntry  Absolute path to the user's server entry file. Passed
 *                     to Vite as `optimizeDeps.entries` so dep discovery runs
 *                     at Vite startup (before the ModuleRunner issues any
 *                     fetchModule calls). Without this, Vite discovers deps
 *                     lazily as modules are imported; re-optimization then
 *                     invalidates previously-hashed files and the
 *                     ModuleRunner hits "file does not exist" on stale URLs.
 */
export function createZeroshipEnvironmentOptions(
  serverEntry?: string,
): vite.EnvironmentOptions {
  return {
    consumer: "server",
    resolve: {
      conditions: ["zeroship", "worker", "module", "import", "default"],
      noExternal: true,
    },
    dev: {
      createEnvironment(name: string, config: vite.ResolvedConfig, _context?: any): ZeroshipDevEnvironment {
        return new ZeroshipDevEnvironment(name, config);
      },
    },
    build: { target: "es2024" },
    keepProcessEnv: true,
    // Enable dep optimization (CJS → ESM conversion).
    optimizeDeps: {
      noDiscovery: false,
      // Prevent mid-request reloads when new deps are discovered.
      // The ModuleRunner can't handle pre-bundle version changes.
      ignoreOutdatedRequests: true,
      // Pre-crawl the server entry so all deps are discovered at Vite
      // startup, not lazily on first request. If the entry isn't known
      // (no detectable server file), fall back to scanning common paths.
      entries: serverEntry
        ? [serverEntry]
        : ["src/index.{ts,tsx,js,jsx}", "src/server.{ts,js}", "server.{ts,js}"],
      // The framework-internal bootstrap package is wired up via
      // resolve.alias above (pointing at the workspace install).
      // Users don't import it themselves — the dev-bootstrap loads
      // `@zeroship/bootstrap/install-schema` through the ModuleRunner
      // for `instanceof TypeBuilder` identity matching with the
      // user's `t.*` builders.
    },
  };
}
