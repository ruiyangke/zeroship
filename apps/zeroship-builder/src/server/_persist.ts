"use server";
// Persistence helper — wraps `@zeroship/kv` with a same-process Map
// fallback so the rest of the server doesn't have to care which one
// is doing the work. The KV native primitive (`zeroship.kv.*`) is
// in-memory in dev — but the V8 worker process keeps running across
// HMR module re-evaluations, so KV state survives "save the file →
// dev refreshes the bundle" cycles. Module-level Maps in agents.ts
// don't, which was the whole motivation for moving these stubs to
// KV (per ISSUES.md ISS-14, ISS-19, ISS-26).
//
// Wire fallthrough: if `@zeroship/kv` throws (env.kv missing — e.g.
// a future test harness without KvPlugin registered), we silently
// fall back to a process-local Map and emit a one-shot warning so
// the symptom isn't invisible. Behaviour from the canvas's POV is
// unchanged in either case.
//
// Naming: underscore-prefixed so the vite-plugin's RPC discovery
// loop (per ISS-02) does NOT publish anything from this module as
// a public endpoint. Only re-exported by other server-modules; the
// `server.ts` barrel does NOT re-export this file.

import { kv } from "@zeroship/kv";

// Process-local fallback. Keyed by the same string key we'd pass to
// KV, so swapping which leg of the if-tree we're on doesn't change
// the visible state shape.
const FALLBACK = new Map<string, unknown>();

let kvAvailable: boolean | null = null;
let kvWarned = false;

/**
 * Probe the native KV namespace once. We attempt a no-op `get` against
 * a sentinel key; any thrown error is treated as "KV unavailable" and
 * downstream calls flip to the in-memory fallback. The probe is cached
 * for the life of the V8 worker.
 *
 * Cheaper than a `set/get/delete` round-trip and idempotent — multiple
 * concurrent probes see the same Promise via the cache.
 */
async function probeKv(): Promise<boolean> {
  if (kvAvailable !== null) return kvAvailable;
  try {
    await kv.get<unknown>("__zeroship_kv_probe__");
    kvAvailable = true;
  } catch (e) {
    kvAvailable = false;
    if (!kvWarned) {
      kvWarned = true;
      console.warn(
        "[zeroship:_persist] @zeroship/kv unavailable — falling back to in-process Map. " +
          "State will not survive HMR. Reason:",
        e instanceof Error ? e.message : String(e),
      );
    }
  }
  return kvAvailable!;
}

/**
 * Read a JSON-serializable value at `key`, returning `fallback` if
 * either KV says null or the wire fails. Errors are swallowed — the
 * canvas always renders something (per spec §26 empty/error states),
 * and a transient KV blip should degrade to "fresh project state"
 * rather than a render crash.
 */
export async function persistGet<T>(key: string, fallback: T): Promise<T> {
  if (await probeKv()) {
    try {
      const r = await kv.get<T>(key);
      if (r.error) {
        // KV reachable but threw on get (rare — likely a JSON parse
        // failure). Warn and use the fallback.
        console.warn(
          `[zeroship:_persist] kv.get(${key}) errored: ${r.error.message}`,
        );
        return fallback;
      }
      return r.data ?? fallback;
    } catch (e) {
      console.warn(
        `[zeroship:_persist] kv.get(${key}) threw: ${e instanceof Error ? e.message : String(e)}`,
      );
      return fallback;
    }
  }
  // Map fallback
  return (FALLBACK.get(key) as T | undefined) ?? fallback;
}

/**
 * Write a JSON-serializable value at `key`. Errors propagate as
 * console warnings rather than throws — same rationale as `persistGet`:
 * the canvas should never blow up because KV had a hiccup.
 */
export async function persistSet<T>(key: string, value: T): Promise<void> {
  if (await probeKv()) {
    try {
      const r = await kv.set<T>(key, value);
      if (r.error) {
        console.warn(
          `[zeroship:_persist] kv.set(${key}) errored: ${r.error.message}`,
        );
      }
      return;
    } catch (e) {
      console.warn(
        `[zeroship:_persist] kv.set(${key}) threw: ${e instanceof Error ? e.message : String(e)}`,
      );
      return;
    }
  }
  FALLBACK.set(key, value);
}

/**
 * Delete a key. Best-effort; errors swallowed. No return value — callers
 * just need to know it's gone (or wasn't there to begin with).
 */
export async function persistDelete(key: string): Promise<void> {
  if (await probeKv()) {
    try {
      await kv.delete(key);
      return;
    } catch (e) {
      console.warn(
        `[zeroship:_persist] kv.delete(${key}) threw: ${e instanceof Error ? e.message : String(e)}`,
      );
      return;
    }
  }
  FALLBACK.delete(key);
}
