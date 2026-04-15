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

    if (req.method === 'POST' && req.url === '/rpc') {
        let body = '';
        req.on('data', chunk => body += chunk);
        req.on('end', async () => {
            try {
                const rpc = JSON.parse(body);
                const fn = scenarios[rpc.method];
                if (!fn) {
                    res.writeHead(200, { 'Content-Type': 'application/json' });
                    res.end(JSON.stringify({ jsonrpc: '2.0', error: { code: -32601, message: 'not found' }, id: rpc.id }));
                    return;
                }
                let result = fn.apply(null, rpc.params || []);
                if (result && typeof result.then === 'function') result = await result;
                res.writeHead(200, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ jsonrpc: '2.0', result, id: rpc.id }));
            } catch (e) {
                res.writeHead(200, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ jsonrpc: '2.0', error: { code: -32000, message: e.message }, id: null }));
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
