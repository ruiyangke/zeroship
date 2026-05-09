// Node.js bench server — WHATWG `(request: Request) => Response` pipeline.
//
// Mirrors zeroship's `default.fetch` contract from scenarios.js. Where
// node_server.js answers /hello with a 404 and only handles RPC (unfair
// httpGet baseline), this server wraps node:http in a thin adapter that
// builds a real Request, dispatches through the WHATWG handler shape, and
// converts the Response back to ServerResponse — paying the same allocation
// cost zeroship pays in default.fetch.
//
// Architectural choice: RPC paths (POST /_zs/v1/<id>) bypass the WHATWG
// wrap and dispatch through the legacy node:http body-collect path, the
// same way zeroship's runtime calls default.rpc directly without ever
// constructing a Request. This keeps the comparison apples-to-apples:
//   - WHATWG wrap pays Request/Response/Headers cost  → /hello, /wjson, /wping
//   - RPC fast path skips Request/Response             → /_zs/v1/<id>
//
// Globals: Node 18+ ships built-in Request/Response/Headers (from undici).
// Verified on Node 22.22.2.

import http from 'node:http';

const PORT = parseInt(process.argv[2] || '4004');

// Reuse the same scenarios module the v8 runtime loads — keeps the
// procedure set identical across runtimes.
const scenarios = await import('./scenarios.js');

const PONG_RESPONSE_BYTES = new TextEncoder().encode('"pong"');

// WHATWG handler — mirrors scenarios.js `default.fetch`. Lives here
// rather than imported from scenarios.js because scenarios.js's
// default.fetch references WebSocketPair (zeroship-only). Same body
// bytes, same status, same headers as the zeroship path.
async function handleWhatwg(request) {
    const url = new URL(request.url);

    // We don't implement WS upgrade through this path (the zerobench
    // wsEchoRtt scenario hits a different server). Return 501 if asked.
    if (request.headers.get('upgrade') === 'websocket') {
        return new Response('websocket not supported', { status: 501 });
    }

    if (url.pathname === '/sse') {
        return handleSse(url);
    }
    if (url.pathname === '/wping') {
        return new Response(PONG_RESPONSE_BYTES, {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
        });
    }
    if (url.pathname === '/wjson') {
        return Response.json({ ok: true });
    }
    if (url.pathname === '/ping') {
        return new Response(PONG_RESPONSE_BYTES, {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
        });
    }
    return Response.json({ method: request.method, url: request.url });
}

function handleSse(url) {
    const chunks = parseInt(url.searchParams.get('chunks') || '100', 10);
    const delayMs = parseInt(url.searchParams.get('delay') || '0', 10);
    const size = parseInt(url.searchParams.get('size') || '50', 10);
    const payload = 'x'.repeat(size);
    const encoder = new TextEncoder();

    const stream = new ReadableStream({
        async start(controller) {
            for (let i = 0; i < chunks; i++) {
                const frame = `data: ${JSON.stringify({ i, t: Date.now(), d: payload })}\n\n`;
                controller.enqueue(encoder.encode(frame));
                if (delayMs > 0) {
                    await new Promise((r) => setTimeout(r, delayMs));
                }
            }
            controller.enqueue(encoder.encode('data: [DONE]\n\n'));
            controller.close();
        },
    });
    return new Response(stream, {
        status: 200,
        headers: {
            'Content-Type': 'text/event-stream',
            'Cache-Control': 'no-cache',
        },
    });
}

// node:http IncomingMessage → WHATWG Request.
//
// rawHeaders is a flat [k, v, k, v, ...] array preserving original
// case + multiple values for the same key — closer to wire than
// req.headers (which lowercases + collapses). We reconstruct
// Headers from rawHeaders so multi-valued headers (Set-Cookie etc.)
// survive the trip.
function buildRequest(req) {
    const host = req.headers.host || '127.0.0.1';
    const url = `http://${host}${req.url}`;
    const headers = new Headers();
    for (let i = 0; i < req.rawHeaders.length; i += 2) {
        try {
            headers.append(req.rawHeaders[i], req.rawHeaders[i + 1]);
        } catch {
            // Forbidden header names (e.g., :method in HTTP/2 pseudo-headers).
            // node:http shouldn't emit these on HTTP/1.1 but be defensive.
        }
    }

    const init = {
        method: req.method,
        headers,
    };

    // GET/HEAD have no body. For everything else, pipe the
    // IncomingMessage as a stream so the handler can read it via
    // request.text() / request.json() / request.body.
    if (req.method !== 'GET' && req.method !== 'HEAD') {
        init.body = new ReadableStream({
            start(controller) {
                req.on('data', (chunk) => controller.enqueue(chunk));
                req.on('end', () => controller.close());
                req.on('error', (e) => controller.error(e));
            },
        });
        init.duplex = 'half';
    }

    return new Request(url, init);
}

// WHATWG Response → node:http ServerResponse.
async function writeResponse(response, res) {
    const headers = Object.fromEntries(response.headers);
    res.writeHead(response.status, headers);
    if (response.body == null) {
        res.end();
        return;
    }
    // Stream the body — works for both buffered (text/json) and
    // streaming (SSE) Response objects without buffering everything
    // into memory.
    const reader = response.body.getReader();
    try {
        while (true) {
            const { value, done } = await reader.read();
            if (done) break;
            // value is a Uint8Array; ServerResponse.write accepts it directly.
            if (!res.write(Buffer.from(value.buffer, value.byteOffset, value.byteLength))) {
                // backpressure
                await new Promise((r) => res.once('drain', r));
            }
        }
    } finally {
        reader.releaseLock();
    }
    res.end();
}

const server = http.createServer((req, res) => {
    // Health check — kept identical to node_server.js so the runner
    // probe (POST /_zs/v1/ping) is the canonical readiness signal but
    // /health stays available.
    if (req.method === 'GET' && req.url === '/health') {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end('{"status":"ok"}');
        return;
    }

    // RPC fast path — POST /_zs/v1/<id>, superjson `{ json }` envelope.
    // Bypasses the WHATWG wrap (mirrors zeroship's default.rpc kernel
    // entry which also bypasses Request construction).
    if (req.method === 'POST' && req.url.startsWith('/_zs/v1/')) {
        const method = req.url.slice('/_zs/v1/'.length);
        let body = '';
        req.on('data', (chunk) => (body += chunk));
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

    // Everything else goes through the WHATWG pipeline.
    Promise.resolve()
        .then(async () => {
            const request = buildRequest(req);
            const response = await handleWhatwg(request);
            await writeResponse(response, res);
        })
        .catch((e) => {
            try {
                res.writeHead(500, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ message: e.message, name: e.name || 'Error' }));
            } catch {
                // headers already sent
                res.end();
            }
        });
});

server.listen(PORT, () => {
    console.log(`[node-whatwg] http://0.0.0.0:${PORT} (Node.js ${process.version})`);
});
