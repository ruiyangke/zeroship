//
// Wire encoding for inputs and outputs. Two transformers:
//
//   "json" (default)
//      Plain JSON.stringify / JSON.parse. JSON mode also accepts the
//      runtime's `{ json, meta? }` envelope on response and returns the
//      `json` shadow without rich-type revival.
//
//   "superjson"  (opt-in; must match manifest.transformer)
//      Wraps a value as `{ json, meta? }` so Date / BigInt / Map /
//      Set / Decimal round-trip across the wire faithfully. Drives
//      the "lossless" guarantee in `docs/proposals/rpc.md` §6.
//
// superjson is an OPTIONAL peer dep. We import it lazily so consumers
// who only use the "json" transformer (or who compile-time tree-shake)
// don't need to install it.

export type Transformer = "superjson" | "json";

interface WireEnvelope {
  json: unknown;
  meta?: unknown;
}

interface SuperjsonCodec {
  serialize: (value: unknown) => WireEnvelope;
  deserialize: <T = unknown>(payload: WireEnvelope) => T;
}

function isWireEnvelope(value: unknown): value is WireEnvelope {
  return (
    value !== null &&
    typeof value === "object" &&
    Object.prototype.hasOwnProperty.call(value, "json")
  );
}

function isObjectLike(value: unknown): value is object {
  return (typeof value === "object" && value !== null) || typeof value === "function";
}

function isSuperjsonCodec(value: unknown): value is SuperjsonCodec {
  return (
    isObjectLike(value) &&
    "serialize" in value &&
    "deserialize" in value &&
    typeof value.serialize === "function" &&
    typeof value.deserialize === "function"
  );
}

/**
 * Lazily-resolved superjson module. We `await import("superjson")` once
 * the first time we need it; subsequent calls hit the cached promise.
 * Loading this way keeps the module-init path synchronous for users of
 * the "json" transformer who never trigger the import.
 */
let _superjsonPromise: Promise<SuperjsonCodec> | null = null;
const SUPERJSON_MODULE: string = "superjson";

function loadSuperjson() {
  if (!_superjsonPromise) {
    _superjsonPromise = import(/* @vite-ignore */ SUPERJSON_MODULE)
      .then((m: unknown) => {
        // superjson exports both a default class and named functions.
        // Prefer the named functions; fall back to the default's static
        // methods for older versions.
        if (isSuperjsonCodec(m)) {
          return m;
        }
        if (
          isObjectLike(m) &&
          "default" in m &&
          isSuperjsonCodec(m.default)
        ) {
          return m.default;
        }
        throw new Error("module does not export serialize/deserialize");
      })
      .catch((e) => {
        const msg = `[zeroship/rpc] superjson is required for transformer: "superjson" but couldn't be loaded: ${(
          e as Error
        ).message}. Install \`superjson\` or set \`transformer: "json"\`.`;
        // Surface in the console too — this is a setup/config error the
        // developer must fix, and consumers often only render the thrown
        // error in a transient toast where it's easy to miss.
        console.error(msg, e);
        throw new Error(msg);
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
 *   json      → plain JSON.parse; unwraps `{ json }` runtime envelopes
 */
export async function decodeBody<T = unknown>(
  text: string,
  transformer: Transformer,
): Promise<T> {
  if (!text) return undefined as T;
  const parsed = JSON.parse(text) as unknown;
  if (transformer === "json") {
    if (isWireEnvelope(parsed)) {
      return parsed.json as T;
    }
    return parsed as T;
  }
  const sj = await loadSuperjson();
  // Some servers (legacy / hand-rolled) return the bare value at the
  // top level instead of `{ json, meta }`. In that case, treat the
  // payload itself as the json side with no meta.
  if (isWireEnvelope(parsed)) {
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
