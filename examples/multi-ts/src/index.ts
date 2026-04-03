// Multi-file TypeScript example — entrypoint
// Demonstrates: imports, TypeScript types, tree-shaking, onRequest handler

import { createRouter, type Route } from "./router";
import { jsonResponse, errorResponse } from "./utils";
import { VERSION } from "./config";

const routes: Route[] = [
    {
        path: "/",
        handler: () => jsonResponse({ message: "Hello from appbase!", version: VERSION }),
    },
    {
        path: "/health",
        handler: () => jsonResponse({ status: "ok", uptime: Date.now() }),
    },
    {
        path: "/echo",
        handler: (req) => {
            const url = new URL(req.url);
            const params: Record<string, string> = {};
            url.searchParams.forEach((v, k) => { params[k] = v; });
            return jsonResponse({ method: req.method, path: url.pathname, params });
        },
    },
];

const router = createRouter(routes);

export function onRequest(request: Request): Response {
    try {
        return router(request);
    } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        return errorResponse(msg, 500);
    }
}

// Also export RPC functions for testing
export function ping(): string {
    return "pong";
}

export function getVersion(): string {
    return VERSION;
}
