// Example: multi-file TypeScript app with npm dependency (zod)
import { z } from "zod";
import { validateUser, type User } from "./schema";

export function onRequest(request: Request): Response {
    const url = new URL(request.url);

    if (url.pathname === "/validate" && request.method === "POST") {
        try {
            const body = request._bodyText || "{}";
            const data = JSON.parse(body);
            const user = validateUser(data);
            return new Response(JSON.stringify({ ok: true, user }), {
                status: 200,
                headers: { "Content-Type": "application/json" },
            });
        } catch (e: unknown) {
            const msg = e instanceof z.ZodError
                ? e.errors.map(err => `${err.path.join(".")}: ${err.message}`).join(", ")
                : e instanceof Error ? e.message : String(e);
            return new Response(JSON.stringify({ ok: false, error: msg }), {
                status: 400,
                headers: { "Content-Type": "application/json" },
            });
        }
    }

    return new Response(JSON.stringify({ routes: ["/validate POST"] }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
    });
}

export function ping(): string {
    return "pong";
}
