// Node.js cluster mode server for cross-runtime benchmarks.
// Each worker is a full Node.js process with its own event loop.
// Usage: node node_server_cluster.js [port] [num_workers]

const cluster = require("cluster");
const http = require("http");
const path = require("path");
const os = require("os");
const { webcrypto } = require("crypto");

const port = parseInt(process.argv[2] || "4002", 10);
const numWorkers = parseInt(process.argv[3] || os.cpus().length.toString(), 10);

if (cluster.isPrimary) {
  console.error(
    `[node-cluster] ${numWorkers} workers on port ${port} (Node.js ${process.version})`
  );
  for (let i = 0; i < numWorkers; i++) {
    cluster.fork();
  }
  cluster.on("exit", (worker) => {
    console.error(`Worker ${worker.process.pid} died, restarting...`);
    cluster.fork();
  });
} else {
  // Make Web Crypto available as globals (matching our V8 runtime)
  globalThis.crypto = webcrypto;

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
            res.end(
              JSON.stringify({
                jsonrpc: "2.0",
                error: { code: -32601, message: "not found" },
                id: request.id,
              })
            );
            return;
          }

          const result = await fn(...(request.params || []));
          res.writeHead(200, { "content-type": "application/json" });
          res.end(JSON.stringify({ jsonrpc: "2.0", result, id: request.id }));
        } catch (e) {
          res.writeHead(200, { "content-type": "application/json" });
          res.end(
            JSON.stringify({
              jsonrpc: "2.0",
              error: { code: -32000, message: e.message },
              id: null,
            })
          );
        }
        return;
      }

      res.writeHead(404);
      res.end("Not Found");
    });

    server.listen(port, () => {
      // silent — primary already printed
    });
  });
}
