// Raw-JS dict-shape RPC demo — no Vite, no tooling.
//
// Demonstrates the ZS-standard contract introduced in Stage 5a of the
// zs-standard-and-vite-v2 refactor: `default.rpc` is a plain object
// mapping wireIds to handler functions. The runtime owns dispatch
// (input validation, capability frames, auto-tx, stream framing) via
// the embedded `__zsDispatch` dispatcher.
//
// Run locally with:
//   zeroship serve examples/raw-rpc.js --port 3000
//
// Then hit:
//   curl -X POST http://localhost:3000/_zs/v1/ping \
//        -H 'content-type: application/json' -d '{"json":null}'
//   → {"json":"pong"}
//
//   curl -X POST http://localhost:3000/_zs/v1/echo \
//        -H 'content-type: application/json' -d '{"json":{"hello":"world"}}'
//   → {"json":{"echo":{"hello":"world"}}}
//
//   curl -X POST http://localhost:3000/_zs/v1/add \
//        -H 'content-type: application/json' -d '{"json":[2,3]}'
//   → {"json":5}
//
// A procedure may attach `.config = { kind, input, output, isolation }`.
// The dispatcher reads `config.kind` to drive auto-tx (for "query" /
// "mutation") and capability frames; `config.input.parse(...)` to
// validate input pre-handler; `config.output` to tag string streams.

const ping = (_input) => "pong";

const echo = (input) => ({ echo: input });

const add = (input) => {
  if (!Array.isArray(input) || input.length !== 2) {
    const err = new Error("add expects [a, b]");
    err.status = 400;
    err.code = "INVALID_ARGUMENT";
    throw err;
  }
  return input[0] + input[1];
};

// Demonstrate the optional procedure-config surface. Attaching
// `config.kind` lets the dispatcher apply the right capability frame —
// in a real app with a db plugin loaded, "query" would also open a
// READ ONLY transaction.
const status = (_input) => ({ ok: true, ts: Date.now() });
status.config = { kind: "query" };

export default {
  rpc: { ping, echo, add, status },
};
