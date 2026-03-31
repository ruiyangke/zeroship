// Appbase core runtime -- primitives only.
// Dispatch loop is started by the actor AFTER OpState and user code are ready.

const { core } = Deno;

// Import ALL deno extension ESM modules so they get evaluated.
// deno_core panics if any registered ESM module is not imported.

// deno_web (22 modules):
import "ext:deno_web/00_infra.js";
import "ext:deno_web/01_dom_exception.js";
import "ext:deno_web/01_mimesniff.js";
import "ext:deno_web/02_event.js";
import "ext:deno_web/02_structured_clone.js";
import "ext:deno_web/02_timers.js";
import "ext:deno_web/03_abort_signal.js";
import "ext:deno_web/04_global_interfaces.js";
import "ext:deno_web/05_base64.js";
import "ext:deno_web/06_streams.js";
import "ext:deno_web/08_text_encoding.js";
import "ext:deno_web/09_file.js";
import "ext:deno_web/10_filereader.js";
import "ext:deno_web/12_location.js";
import "ext:deno_web/13_message_port.js";
import "ext:deno_web/14_compression.js";
import "ext:deno_web/15_performance.js";
import "ext:deno_web/16_image_data.js";
import "ext:deno_web/00_url.js";
import "ext:deno_web/01_urlpattern.js";
import "ext:deno_web/01_console.js";
import "ext:deno_web/01_broadcast_channel.js";

// deno_net (2 modules):
import "ext:deno_net/01_net.js";
import "ext:deno_net/02_tls.js";

// deno_fetch (8 modules):
import "ext:deno_fetch/20_headers.js";
import "ext:deno_fetch/21_formdata.js";
import "ext:deno_fetch/22_body.js";
import "ext:deno_fetch/22_http_client.js";
import "ext:deno_fetch/23_request.js";
import "ext:deno_fetch/23_response.js";
import "ext:deno_fetch/26_fetch.js";
import "ext:deno_fetch/27_eventsource.js";

// Expose Web APIs as globals
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

// Unhandled promise rejection handler
core.setUnhandledPromiseRejectionHandler((promise, reason) => {
  console.error('[appbase] Unhandled promise rejection:', reason);
});

// Report uncaught exceptions
core.setReportExceptionCallback((error) => {
  console.error('[appbase] Uncaught exception:', error.message || error);
});

// RPC method registry -- user code registers functions here
globalThis.__rpc = {};

// Single-request dispatch (called by the concurrent loop)
globalThis.__dispatch = async function(req) {
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
};
