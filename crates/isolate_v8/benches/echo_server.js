// Minimal echo server — benchmark target for fetch tests.
// Returns the request body as JSON response.
// Usage: node echo_server.js [port]

const http = require("http");
const port = parseInt(process.argv[2] || "9999", 10);

http.createServer((req, res) => {
  let body = "";
  req.on("data", c => body += c);
  req.on("end", () => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(body || "{}");
  });
}).listen(port, () => {
  console.error(`[echo] http://0.0.0.0:${port}`);
});
