// packages/zeroship-rpc-client/src/encoding.ts
//
// Wire encoding for inputs and outputs. Two transformers:
//
//   "superjson"  (default; matches manifest.transformer)
//      Wraps a value as `{ json, meta? }` so Date / BigInt / Map /
//      Set / Decimal round-trip across the wire faithfully. Drives
//      the "lossless" guarantee in spec §6.
//
//   "json"
//      Plain JSON.stringify / JSON.parse. Use when the server side
//      doesn't have superjson available (e.g. a legacy /_rpc endpoint).
//
// superjson is an OPTIONAL peer dep. We import it lazily so consumers
// who only use the "json" transformer (or who compile-time tree-shake)
// don't need to install it.

export type Transformer = "superjson" | "json";

/**
 * Lazily-resolved superjson module. We `await import("superjson")` once
 * the first time we need it; subsequent calls hit the cached promise.
 * Loading this way keeps the module-init path synchronous for users of
 * the "json" transformer who never trigger the import.
 */
let _superjsonPromise: Promise<{
  serialize: (value: unknown) => { json: unknown; meta?: unknown };
  deserialize: <T = unknown>(payload: { json: unknown; meta?: unknown }) => T;
}> | null = null;

function loadSuperjson() {
  if (!_superjsonPromise) {
    // @ts-ignore — optional peer dep; types may not resolve at compile time.
    _superjsonPromise = import("superjson")
      .then((m: { default?: unknown; serialize?: unknown; deserialize?: unknown }) => {
        // superjson exports both a default class and named functions.
        // Prefer the named functions; fall back to the default's static
        // methods for older versions.
        const named = m as {
          serialize?: (v: unknown) => { json: unknown; meta?: unknown };
          deserialize?: <T = unknown>(p: { json: unknown; meta?: unknown }) => T;
        };
        if (named.serialize && named.deserialize) {
          return { serialize: named.serialize, deserialize: named.deserialize };
        }
        const D = m.default as {
          serialize: (v: unknown) => { json: unknown; meta?: unknown };
          deserialize: <T = unknown>(p: { json: unknown; meta?: unknown }) => T;
        };
        return { serialize: D.serialize, deserialize: D.deserialize };
      })
      .catch((e) => {
        throw new Error(
          `[zeroship/rpc-client] superjson is required for transformer: "superjson" but couldn't be loaded: ${(
            e as Error
          ).message}. Install \`superjson\` or set \`transformer: "json"\`.`,
        );
      });
  }
  return _superjsonPromise;
}

/**
 * Serialize an input value to the wire envelope.
 *
 *   superjson → { json, meta? } (meta only when present)
 *   json      → the value itself, JSON.stringify'd
 */
export async function encodeBody(
  value: unknown,
  transformer: Transformer,
): Promise<string> {
  if (transformer === "json") {
    return JSON.stringify(value);
  }
  const sj = await loadSuperjson();
  const env = sj.serialize(value);
  // superjson omits meta when there's nothing to track; preserve that.
  return JSON.stringify(env);
}

/**
 * Deserialize a wire envelope back to a typed value.
 *
 *   superjson → { json, meta? } → original value (Dates etc. revived)
 *   json      → plain JSON.parse
 */
export async function decodeBody<T = unknown>(
  text: string,
  transformer: Transformer,
): Promise<T> {
  if (!text) return undefined as T;
  if (transformer === "json") {
    return JSON.parse(text) as T;
  }
  const sj = await loadSuperjson();
  const parsed = JSON.parse(text) as { json?: unknown; meta?: unknown };
  // Some servers (legacy / hand-rolled) return the bare value at the
  // top level instead of `{ json, meta }`. In that case, treat the
  // payload itself as the json side with no meta.
  if (parsed && typeof parsed === "object" && "json" in parsed) {
    return sj.deserialize<T>({
      json: parsed.json,
      meta: parsed.meta,
    });
  }
  return parsed as T;
}

/**
 * Encode an input value as a base64url-safe string for use in the
 * `?input=` query parameter on GET queries.
 */
export async function encodeQueryInput(
  value: unknown,
  transformer: Transformer,
): Promise<string> {
  const body = await encodeBody(value, transformer);
  // base64url: standard base64 with `+ /` → `- _`, no `=` padding.
  const b64 = base64Encode(body);
  return b64.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** Cross-environment base64 encoder. */
function base64Encode(text: string): string {
  if (typeof Buffer !== "undefined") {
    return Buffer.from(text, "utf8").toString("base64");
  }
  // Browser fallback: convert UTF-8 → binary string → btoa.
  const bytes = new TextEncoder().encode(text);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return globalThis.btoa(bin);
}
