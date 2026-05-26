//
// Auto-batching link for queries (opt-in via `client({ batch: true })`).
//
// Semantics (`docs/proposals/rpc.md` §6, "Batching"):
//   - Queries fired in the same microtask tick collapse into one
//     POST /_zs/v1/_batch.
//   - Mutations and streams are NEVER batched — they pass through
//     untouched.
//   - The batch endpoint returns a parallel array of `{ id, status,
//     output | error }` entries; we resolve each caller's promise
//     against its own slot.
//
// Wire format:
//   POST /_zs/v1/_batch
//   Content-Type: application/zs-batch+json
//
//   [ {"id":"a","name":"todos.list","input":{...}}, ... ]
//
//   →
//
//   [ {"id":"a","status":200,"output":{...}},
//     {"id":"b","status":429,"error":{"code":"RESOURCE_EXHAUSTED",...}} ]

import { decodeBody, encodeBody, type Transformer } from "./encoding";
import { parseErrorResponse, RpcError, type ErrorCode } from "./error";
import { newUuidV7 } from "./idempotency";
import type { HeaderValue } from "./transport";

interface BatchEntry {
  procId: string;
  input: unknown;
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
  /** Lookup id (also assigned to the wire entry). */
  localId: string;
}

export interface BatchConfig {
  baseUrl: string;
  fetch: typeof globalThis.fetch;
  transformer: Transformer;
  authResolver: () => string | null | undefined | Promise<string | null | undefined>;
  headersResolver?: () => HeaderValue | Promise<HeaderValue>;
  onError?: (err: RpcError) => void;
  onAuthExpired?: () => void;
}

export interface BatchLink {
  /**
   * Enqueue a query in the next microtask's batch. Returns a promise
   * that resolves once the batch flushes and the corresponding entry's
   * status is parsed.
   */
  enqueue<TOut = unknown>(procId: string, input: unknown): Promise<TOut>;
}

/**
 * Build a fresh batch link bound to a config. The link maintains an
 * internal queue that flushes on every microtask boundary — Promise
 * chaining is enough to coalesce same-tick calls.
 */
export function createBatchLink(cfg: BatchConfig): BatchLink {
  let queue: BatchEntry[] = [];
  let scheduled = false;

  function schedule(): void {
    if (scheduled) return;
    scheduled = true;
    queueMicrotask(() => {
      void flush();
    });
  }

  async function flush(): Promise<void> {
    const pending = queue;
    queue = [];
    scheduled = false;
    if (pending.length === 0) return;

    // Resolve auth once for the whole batch.
    let authToken: string | null | undefined;
    try {
      authToken = await cfg.authResolver();
    } catch (e) {
      const err = e instanceof Error ? e : new Error(String(e));
      for (const entry of pending) entry.reject(err);
      return;
    }

    // Encode the wire body.
    const wireEntries = await Promise.all(
      pending.map(async (entry) => ({
        id: entry.localId,
        name: entry.procId,
        input:
          // For superjson, we pre-serialize each input so the inner
          // value still survives the wire faithfully. For json, send
          // the value as-is.
          cfg.transformer === "json"
            ? entry.input
            : JSON.parse(await encodeBody(entry.input, cfg.transformer)),
      })),
    );

    const headers = new Headers();
    headers.set("Accept", "application/zs-batch+json");
    headers.set("Content-Type", "application/zs-batch+json");
    if (authToken) headers.set("Authorization", `Bearer ${authToken}`);
    headers.set("X-Request-Id", newUuidV7());
    if (cfg.headersResolver) {
      const resolved = await cfg.headersResolver();
      if (resolved) {
        const src = new Headers(resolved);
        src.forEach((v, k) => headers.set(k, v));
      }
    }

    let res: Response;
    try {
      res = await cfg.fetch(`${cfg.baseUrl}/_zs/v1/_batch`, {
        method: "POST",
        headers,
        body: JSON.stringify(wireEntries),
      });
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      const err = new RpcError({
        code: "UNAVAILABLE",
        message: `batch transport error: ${message}`,
        retryable: true,
      });
      cfg.onError?.(err);
      for (const entry of pending) entry.reject(err);
      return;
    }

    if (!res.ok) {
      // The batch endpoint itself failed (auth / rate-limit / 500).
      // Reject every pending caller with the same error.
      const err = await parseErrorResponse(res);
      if (err.code === "UNAUTHENTICATED") cfg.onAuthExpired?.();
      cfg.onError?.(err);
      for (const entry of pending) entry.reject(err);
      return;
    }

    let body: Array<{
      id: string;
      status: number;
      output?: unknown;
      error?: {
        code?: string;
        message?: string;
        details?: unknown;
        retryable?: boolean;
        trace_id?: string;
      };
    }>;
    try {
      body = JSON.parse(await res.text());
    } catch (e) {
      const err = new RpcError({
        code: "INTERNAL",
        message: `batch response not JSON: ${(e as Error).message}`,
        retryable: false,
      });
      cfg.onError?.(err);
      for (const entry of pending) entry.reject(err);
      return;
    }

    // Look up each caller's slot by id; resolve / reject accordingly.
    const byId = new Map(pending.map((p) => [p.localId, p]));
    for (const slot of body) {
      const entry = byId.get(slot.id);
      if (!entry) continue; // Server returned an unknown id; skip.
      byId.delete(slot.id);
      if (slot.status >= 200 && slot.status < 300) {
        // Output is wrapped in superjson envelope `{ json, meta? }`
        // when transformer === "superjson"; in json mode it's the
        // bare value. decodeBody handles both shapes.
        try {
          if (cfg.transformer === "json") {
            entry.resolve(slot.output);
          } else {
            const decoded = await decodeBody(
              JSON.stringify(slot.output),
              cfg.transformer,
            );
            entry.resolve(decoded);
          }
        } catch (e) {
          entry.reject(e instanceof Error ? e : new Error(String(e)));
        }
      } else {
        const errEnv = slot.error ?? {};
        const code = (errEnv.code as ErrorCode | undefined) ?? "INTERNAL";
        const err = new RpcError({
          code,
          message: errEnv.message ?? `batch entry failed: ${slot.status}`,
          details: errEnv.details,
          retryable: errEnv.retryable,
          status: slot.status,
          trace_id: errEnv.trace_id,
        });
        if (err.code === "UNAUTHENTICATED") cfg.onAuthExpired?.();
        cfg.onError?.(err);
        entry.reject(err);
      }
    }
    // Any leftover entries the server didn't address: treat as INTERNAL
    // (server-side bug). Better to fail loudly than hang forever.
    for (const entry of byId.values()) {
      const err = new RpcError({
        code: "INTERNAL",
        message: `batch response missing entry for id ${entry.localId}`,
        retryable: false,
      });
      cfg.onError?.(err);
      entry.reject(err);
    }
  }

  return {
    enqueue<TOut = unknown>(procId: string, input: unknown): Promise<TOut> {
      return new Promise<TOut>((resolve, reject) => {
        queue.push({
          procId,
          input,
          localId: shortId(),
          resolve: resolve as (v: unknown) => void,
          reject,
        });
        schedule();
      });
    },
  };
}

/**
 * Compact, batch-local id. We don't need cryptographic uniqueness
 * because the id never leaves the client → batch → response cycle.
 */
function shortId(): string {
  return `b-${Math.random().toString(36).slice(2, 10)}`;
}
