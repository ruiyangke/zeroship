// Appbase core runtime - injected before user code and plugin bridges
// Contains: console, error handling, RPC dispatch, Web API globals.

const { core } = Deno;

// Expose Web APIs as globals (deno extensions export them but don't assign to globalThis)
import { fetch } from "ext:deno_fetch/26_fetch.js";
import { Headers } from "ext:deno_fetch/20_headers.js";
import { Request } from "ext:deno_fetch/23_request.js";
import { Response } from "ext:deno_fetch/23_response.js";
globalThis.fetch = fetch;
globalThis.Request = Request;
globalThis.Response = Response;
globalThis.Headers = Headers;

// Console
globalThis.console = {
  log: (...args) => {
    core.print(args.map(a => typeof a === 'string' ? a : JSON.stringify(a)).join(' ') + '\n', false);
  },
  error: (...args) => {
    core.print(args.map(a => typeof a === 'string' ? a : JSON.stringify(a)).join(' ') + '\n', true);
  },
};

// Unhandled promise rejection handler - prevents silent failures
// Without this, rejected promises with no .catch() would be silently swallowed
core.setUnhandledPromiseRejectionHandler((promise, reason) => {
  console.error('[appbase] Unhandled promise rejection:', reason);
});

// Report uncaught exceptions so they surface in Rust
core.setReportExceptionCallback((error) => {
  console.error('[appbase] Uncaught exception:', error.message || error);
});

// RPC dispatcher - user code registers functions here
globalThis.__rpc = {};

// Pre-compiled RPC dispatch function - called from Rust via op bridge
// Reads request JSON from Rust op, dispatches to user function, writes response back
globalThis.__handleRpc = async function(requestJson) {
  const request = JSON.parse(requestJson);
  if (Array.isArray(request)) {
    return JSON.stringify(await Promise.all(request.map(__dispatch)));
  }
  return JSON.stringify(await __dispatch(request));
};

async function __dispatch(req) {
  try {
    const fn_ = globalThis.__rpc[req.method];
    if (!fn_) {
      return { jsonrpc: '2.0', error: { code: -32601, message: 'Method not found: ' + req.method }, id: req.id };
    }
    const result = await fn_(...(req.params || []));
    return { jsonrpc: '2.0', result, id: req.id };
  } catch (e) {
    return { jsonrpc: '2.0', error: { code: -32000, message: e.message || String(e) }, id: req.id };
  }
}
