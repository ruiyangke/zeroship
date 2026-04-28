// Tiny per-request context shim.
//
// The zeroship vite-plugin's RPC machinery dispatches each server
// function call inside a fetch handler. We piggy-back on the
// dispatcher by stashing the current Request + a Response.headers
// scratchpad on a globalThis hook the plugin calls into.
//
// In dev (and production once the platform exposes a first-class
// "current request" API), these helpers return live values. In
// build environments where they're not wired we return undefined
// and the auth proxy degrades gracefully (no cookie passthrough).

interface ReqCtx {
  request: Request;
  responseHeaders: Headers;
}

const KEY = "__zs_builder_req_ctx__";

export function setContext(ctx: ReqCtx | undefined): void {
  (globalThis as any)[KEY] = ctx;
}

export function getRequest(): Request | undefined {
  return ((globalThis as any)[KEY] as ReqCtx | undefined)?.request;
}

export function getResponseHeaders(): Headers | undefined {
  return ((globalThis as any)[KEY] as ReqCtx | undefined)?.responseHeaders;
}
