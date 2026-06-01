import http from 'node:http';

const PORT = parseInt(process.argv[2] || '4002');

// Dynamic import for ESM scenarios
const scenarios = await import('./scenarios.js');

const server = http.createServer((req, res) => {
    if (req.method === 'GET' && req.url === '/health') {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end('{"status":"ok"}');
        return;
    }

    // URL-path RPC wire (zeroship v1): POST /__zeroship/v1/<id> with body =
    // superjson `{ json: <input> }` envelope. Response is the raw
    // return value wrapped as `{ json: <result> }` with Content-Type
    // application/json. Single-arg dispatch: fn(input).
    if (req.method === 'POST' && req.url.startsWith('/__zeroship/v1/')) {
        const method = req.url.slice('/__zeroship/v1/'.length);
        let body = '';
        req.on('data', chunk => body += chunk);
        req.on('end', async () => {
            try {
                let input;
                if (body) {
                    const env = JSON.parse(body);
                    input = env && typeof env === 'object' && 'json' in env ? env.json : env;
                }
                const fn = scenarios[method];
                if (!fn) {
                    res.writeHead(404, { 'Content-Type': 'application/json' });
                    res.end(JSON.stringify({ message: `method ${method} not found`, name: 'Error' }));
                    return;
                }
                let result = fn(input);
                if (result && typeof result.then === 'function') result = await result;
                res.writeHead(200, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ json: result === undefined ? null : result }));
            } catch (e) {
                res.writeHead(500, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ message: e.message, name: e.name || 'Error' }));
            }
        });
        return;
    }

    // SSE streaming endpoint: /sse?chunks=N&delay=M&size=S
    if (req.method === 'GET' && req.url.startsWith('/sse')) {
        const url = new URL(req.url, `http://localhost:${PORT}`);
        const chunks = parseInt(url.searchParams.get('chunks') || '100');
        const delayMs = parseInt(url.searchParams.get('delay') || '0');
        const chunkSize = parseInt(url.searchParams.get('size') || '50');
        const payload = 'x'.repeat(chunkSize);

        res.writeHead(200, {
            'Content-Type': 'text/event-stream',
            'Cache-Control': 'no-cache',
            'Connection': 'keep-alive',
        });

        let i = 0;
        const send = () => {
            if (i < chunks) {
                res.write(`data: ${JSON.stringify({ i, t: Date.now(), d: payload })}\n\n`);
                i++;
                if (delayMs > 0) {
                    setTimeout(send, delayMs);
                } else {
                    setImmediate(send);
                }
            } else {
                res.write('data: [DONE]\n\n');
                res.end();
            }
        };
        send();
        return;
    }

    res.writeHead(404);
    res.end('Not Found');
});

server.listen(PORT, () => {
    console.log(`[node] http://0.0.0.0:${PORT} (Node.js ${process.version})`);
});
