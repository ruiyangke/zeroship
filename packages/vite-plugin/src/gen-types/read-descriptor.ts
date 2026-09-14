// packages/vite-plugin/src/gen-types/read-descriptor.ts
//
// Read the generated `schema.runtime.json` off disk. Shared by the dev server
// (which injects the descriptor into the runtime) and the migrate CLI (which
// keys the apply's ownership registry on the same collections), so the two
// cannot disagree about what the app declares.

import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { RUNTIME_DESCRIPTOR_FILE } from "./index.js";

/**
 * Return the descriptor JSON in `outDir`, or `undefined` when it is absent or
 * empty. Absent is a normal state (nothing generated yet), not an error.
 */
export function readGeneratedRuntimeDescriptorAt(outDir: string): string | undefined {
  try {
    const json = readFileSync(resolve(outDir, RUNTIME_DESCRIPTOR_FILE), "utf8").trim();
    return json.length > 0 ? json : undefined;
  } catch {
    return undefined;
  }
}

/**
 * The collection names a descriptor declares.
 *
 * `collections` is an OBJECT KEYED BY NAME (`{ todos: { fields: … } }`), not an
 * array. Both call sites previously did `(d.collections ?? []).map(...)` inside a
 * `try`/`catch`, so the `TypeError` was swallowed and the apply's ownership
 * registry was silently ALWAYS EMPTY in dev — a degradation that looked exactly
 * like success. Parsing it in one place, and returning `[]` only when there
 * genuinely are none, is what keeps that from coming back.
 */
export function collectionNamesFrom(descriptorJson: string | undefined): string[] {
  if (!descriptorJson) return [];
  const parsed = JSON.parse(descriptorJson) as {
    collections?: Record<string, unknown> | { name?: string }[];
  };
  const collections = parsed.collections;
  if (!collections) return [];
  // Tolerate the array shape too, so a descriptor-format change surfaces as a
  // schema mismatch rather than a silently empty registry.
  const names = Array.isArray(collections)
    ? collections.map((c) => c?.name)
    : Object.keys(collections);
  return names.filter((n): n is string => typeof n === "string" && n.length > 0);
}
