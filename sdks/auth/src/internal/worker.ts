/**
 * Web Worker refresh-token isolation (gateway §4.3) — gating + interface.
 *
 * The Web Worker arm holds the BROWSER refresh family in worker-isolated
 * memory and strips `refresh_token` from main-thread cache entries. It is
 * engaged ONLY when `window.Worker && useRefreshTokens && cacheLocation === 'memory'`.
 *
 * In the DEFAULT `server_anchor` mode the browser holds NO refresh token at
 * all — the server-held anchor family is the source of truth and silent
 * renewal goes through `GET /__zs/auth/session?mint=1`. So the worker holds
 * nothing and `shouldUseWorker` is `false`. The opt-in `useRefreshTokens`
 * browser-family rotation is a sibling slice; this module is the gate + the
 * stable interface the client consults, so the decision lives in one place.
 */

import type { CacheLocation } from "../types";
import type { WindowLike } from "./env";

/**
 * Decide whether the Web Worker refresh-token isolation arm engages.
 * Mirrors the gateway §4.3 condition exactly.
 */
export function shouldUseWorker(opts: {
  window: WindowLike | undefined;
  useRefreshTokens: boolean;
  cacheLocation: CacheLocation;
}): boolean {
  return Boolean(
    opts.window?.Worker && opts.useRefreshTokens && opts.cacheLocation === "memory",
  );
}
