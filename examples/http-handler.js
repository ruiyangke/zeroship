// HTTP routing demo — new zeroship handler contract.
//
// The bootstrap dispatches every request to `default.fetch(req, env, ctx)`.
// `env` is the module-singleton bindings object; `ctx` carries the cancel
// flag and `waitUntil` registrar. This demo doesn't need either.
//
// The `ping` / `add` exports below stay available as RPC methods — the
// same module can mix an HTTP handler with `"use server"` functions
// (the bootstrap's /_rpc router dispatches them, the fetch handler
// sees everything else).

export default {
  async fetch(request) {
    const url = new URL(request.url);
    const path = url.pathname;
    const method = request.method;

    if (method === "GET" && path === "/") {
      return Response.json({ message: "Welcome to the API" });
    }

    if (method === "GET" && path === "/health") {
      return new Response("OK", { status: 200 });
    }

    if (method === "GET" && path.startsWith("/echo/")) {
      const text = decodeURIComponent(path.slice(6));
      return new Response(text, {
        status: 200,
        headers: { "content-type": "text/plain", "x-echo": "true" },
      });
    }

    if (method === "POST" && path === "/json") {
      const data = await request.json();
      return Response.json({ received: data, timestamp: Date.now() }, { status: 201 });
    }

    return Response.json({ error: "Not Found", path }, { status: 404 });
  },
};

export function ping() { return "pong"; }
export function add(a, b) { return a + b; }
