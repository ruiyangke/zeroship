// Node.js baseline RPC server for cross-runtime benchmarks.
// Usage: node node_server.js [port]

const http = require("http");

const port = parseInt(process.argv[2] || "4002", 10);

const rpc = {
  ping() { return "pong"; },
  fib(n) {
    function fib(n) { return n <= 1 ? n : fib(n - 1) + fib(n - 2); }
    return fib(n);
  },
  timeout0() {
    return new Promise(resolve => setTimeout(() => resolve("done"), 0));
  },
  promiseChain() {
    return Promise.resolve(1).then(v => v + 10).then(v => v * 2);
  },
  promiseChainTimeout() {
    return new Promise(resolve => setTimeout(() => resolve(1), 100))
      .then(v => v + 10).then(v => v * 2);
  },
};

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
