// Node.js baseline RPC server for cross-runtime benchmarks.
// Loads the same scenarios.js as the V8 server (ESM via dynamic import).
// Usage: node node_server.js [port]

const http = require("http");
const path = require("path");
const { webcrypto } = require("crypto");

// Make Web Crypto available as globals (matching our V8 runtime)
globalThis.crypto = webcrypto;

const port = parseInt(process.argv[2] || "4002", 10);

// Dynamic import for ESM scenarios.js
let rpc = {};

async function loadScenarios() {
  const mod = await import(path.join("file://", __dirname, "scenarios.js"));
  for (const [k, v] of Object.entries(mod)) {
    if (typeof v === "function") rpc[k] = v;
  }
}

loadScenarios().then(() => {
  const server = http.createServer(async (req, res) => {
    if (req.method === "GET" && req.url === "/health") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end('{"status":"ok"}');
      return;
    }

    if (req.method === "POST" && req.url === "/rpc") {
      let body = "";
      for await (const chunk of req) body += chunk;

      try {
        const request = JSON.parse(body);
        const fn = rpc[request.method];
        if (!fn) {
          res.writeHead(200, { "content-type": "application/json" });
          res.end(JSON.stringify({
            jsonrpc: "2.0",
            error: { code: -32601, message: "not found" },
            id: request.id,
          }));
          return;
        }

        const result = await fn(...(request.params || []));
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ jsonrpc: "2.0", result, id: request.id }));
      } catch (e) {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({
          jsonrpc: "2.0",
          error: { code: -32000, message: e.message },
          id: null,
        }));
      }
      return;
    }

    res.writeHead(404);
    res.end("Not Found");
  });

  server.listen(port, () => {
    console.error(`[node] http://0.0.0.0:${port} (Node.js ${process.version})`);
  });
});
