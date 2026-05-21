/**
 * WinterCG `fetch` wrapper that owns the `/_zs/v1/<id>` fall-through:
 *   - GET / POST against `/_zs/v1/<id>` → dispatch through
 *     `__zsDispatch(rpc, id, input, ctx)`. Streaming results are
 *     encoded via the AI-SDK line-prefixed protocol; unary results are
 *     wrapped in the `{ "json": ... }` envelope.
 *   - Any other path → forward to the user's own `default.fetch`. If
 *     the user didn't export one, return 404.
 *
 * Stage 7 moved this out of the Vite plugin's `dev-bootstrap` so the
 * dev path shares the same fall-through logic the runtime crate emits
 * in production (`crates/runtime/src/core/init.rs` — the slow-path
 * fetch handler that wraps `__zsDispatch` for stream encoding). Single
 * implementation; no drift between dev and prod.
 *
 * Note: the production runtime ALSO has a Rust-side fast path that
 * skips the bootstrap fetch handler entirely for non-streaming RPC
 * calls — that path doesn't go through this module. This module is
 * the SLOW-PATH fall-through (and the only path in dev where there's
 * no Rust-side fast path).
 */

import type { NormalizedUserModule } from "./normalize.js";

declare const globalThis: {
  __zsDispatch?: (
    rpc: Record<string, unknown>,
    name: string,
    input: unknown,
    ctx: unknown,
  ) => Promise<unknown>;
  [key: string]: unknown;
};

/**
 * Caller-supplied loader. The dev path re-imports the user module per
 * request (HMR may have invalidated cached evaluations); the
 * production path captures the user module once and returns it on
 * every call. Same interface either way.
 */
export type LoadNormalized = () => Promise<NormalizedUserModule>;

/**
 * Create a WinterCG fetch handler bound to a normalised-user-module
 * loader. Returns the closure the platform exports as `default.fetch`.
 */
export function createFetchHandler(loadNormalized: LoadNormalized): (request: Request, env: unknown, ctx: unknown) => Promise<Response> {
  return async function dispatchFetch(request: Request, env: unknown, ctx: unknown): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname.startsWith("/_zs/v1/")) {
      const id = url.pathname.slice("/_zs/v1/".length);
      if (!id) return errResponse(400, "INVALID_ARGUMENT", "missing wireId");

      let input: unknown = undefined;
      if (request.method === "GET") {
        const param = url.searchParams.get("input");
        if (param) {
          try {
            const b64 = param.replace(/-/g, "+").replace(/_/g, "/");
            const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
            const e = JSON.parse(atob(padded));
            input = (e && typeof e === "object" && "json" in e) ? e.json : e;
          } catch (e) {
            const msg = e instanceof Error ? e.message : String(e);
            return errResponse(400, "INVALID_ARGUMENT", `invalid base64url input: ${msg}`);
          }
        }
      } else if (request.method === "POST") {
        const text = await request.text();
        if (text) {
          try {
            const e = JSON.parse(text);
            input = (e && typeof e === "object" && "json" in e) ? e.json : e;
          } catch (e) {
            const msg = e instanceof Error ? e.message : String(e);
            return errResponse(400, "INVALID_ARGUMENT", `invalid JSON body: ${msg}`);
          }
        }
      } else {
        return errResponse(405, "FAILED_PRECONDITION", `method ${request.method} not allowed on /_zs/v1/`);
      }

      return rpcAndRespond(loadNormalized, id, input, ctx);
    }

    // Fall through to user's own default.fetch (when present).
    let normalized: NormalizedUserModule;
    try {
      normalized = await loadNormalized();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      return errResponse(500, "INTERNAL", `Module import failed: ${msg}`);
    }
    const userFetch = normalized.fetch;
    if (typeof userFetch === "function") {
      return userFetch.call(normalized.userDefault, request, env, ctx);
    }
    return new Response("Not Found", { status: 404 });
  };
}

async function rpcAndRespond(
  loadNormalized: LoadNormalized,
  name: string,
  input: unknown,
  ctx: unknown,
): Promise<Response> {
  try {
    const normalized = await loadNormalized();
    const dispatch = globalThis.__zsDispatch;
    if (typeof dispatch !== "function") {
      throw Object.assign(new Error("__zsDispatch is not installed"), {
        status: 500,
        code: "INTERNAL",
      });
    }
    const result = await dispatch(normalized.rpc, name, input, ctx);

    if (isAsyncIterator(result)) {
      const outputIsString = !!(result as { __zsOutputIsString?: boolean }).__zsOutputIsString;
      const encoder = new TextEncoder();
      const body = new ReadableStream({
        async start(controller) {
          try {
            while (true) {
              const step = await (result as AsyncIterator<unknown>).next();
              if (step.done) {
                controller.enqueue(encoder.encode("d:{}\n"));
                break;
              }
              const v = step.value;
              if (outputIsString || typeof v === "string") {
                controller.enqueue(encoder.encode("0:" + JSON.stringify(String(v)) + "\n"));
              } else {
                controller.enqueue(encoder.encode("2:[" + JSON.stringify(v) + "]\n"));
              }
            }
          } catch (e) {
            const err = e as { message?: string; name?: string; code?: unknown; details?: unknown; retryable?: unknown };
            const env: Record<string, unknown> = {
              message: err?.message ?? String(e),
              name: err?.name ?? "Error",
            };
            if (typeof err?.code === "string") env.code = err.code;
            if (err?.details !== undefined) env.details = err.details;
            if (typeof err?.retryable === "boolean") env.retryable = err.retryable;
            controller.enqueue(encoder.encode("e:" + JSON.stringify(env) + "\n"));
            controller.enqueue(encoder.encode("d:{}\n"));
          } finally {
            controller.close();
          }
        },
      });
      return new Response(body, {
        status: 200,
        headers: {
          "content-type": "text/event-stream",
          "cache-control": "no-cache, no-transform",
          "x-accel-buffering": "no",
        },
      });
    }

    if (result instanceof Response) return result;

    return new Response(
      JSON.stringify({ json: result === undefined ? null : result }),
      { status: 200, headers: { "content-type": "application/json" } },
    );
  } catch (e) {
    const err = e as { message?: string; name?: string; status?: number; code?: unknown; details?: unknown; retryable?: unknown };
    const status = statusFromError(err);
    const body: Record<string, unknown> = {
      message: err?.message ?? String(e),
      name: err?.name ?? "Error",
    };
    if (typeof err?.code === "string") body.code = err.code;
    if (err?.details !== undefined) body.details = err.details;
    if (typeof err?.retryable === "boolean") body.retryable = err.retryable;
    return new Response(
      JSON.stringify(body),
      { status, headers: { "content-type": "application/json" } },
    );
  }
}

function isAsyncIterator(x: unknown): boolean {
  return (
    x != null &&
    typeof x === "object" &&
    typeof (x as { [k: symbol]: unknown })[Symbol.asyncIterator] === "function" &&
    typeof (x as { next?: unknown }).next === "function"
  );
}

function statusFromError(e: { status?: unknown } | null | undefined): number {
  const s = e?.status;
  if (typeof s === "number" && s >= 400 && s < 600) return s;
  return 500;
}

function errResponse(status: number, code: string, message: string, details?: unknown) {
  const body: Record<string, unknown> = { message, name: "Error", code };
  if (details !== undefined) body.details = details;
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}
