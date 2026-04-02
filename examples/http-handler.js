// HTTP Handler — demonstrates onRequest with routing
// Tests: onRequest, Request/Response objects, headers, status codes

export function onRequest(request) {
    const url = new URL(request.url);
    const path = url.pathname;
    const method = request.method;

    // Simple router
    if (method === "GET" && path === "/") {
        return new Response(JSON.stringify({ message: "Welcome to the API" }), {
            headers: { "Content-Type": "application/json" },
        });
    }

    if (method === "GET" && path === "/health") {
        return new Response("OK", { status: 200 });
    }

    if (method === "GET" && path.startsWith("/echo/")) {
        const text = decodeURIComponent(path.slice(6));
        return new Response(text, {
            headers: { "Content-Type": "text/plain", "X-Echo": "true" },
        });
    }

    if (method === "POST" && path === "/json") {
        return request.text().then(body => {
            const data = JSON.parse(body);
            return new Response(JSON.stringify({ received: data, timestamp: Date.now() }), {
                status: 201,
                headers: { "Content-Type": "application/json" },
            });
        });
    }

    return new Response(JSON.stringify({ error: "Not Found", path }), {
        status: 404,
        headers: { "Content-Type": "application/json" },
    });
}

// RPC methods coexist with HTTP handler
export function ping() { return "pong"; }
export function add(a, b) { return a + b; }
