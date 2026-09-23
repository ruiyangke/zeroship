/**
 * `zeroship-migrate-node` addon loader — the single place the gen-types orchestrator
 * requires the napi `genArtifacts` verb.
 *
 * The addon is a napi-rs package whose `index.js` locates its prebuilt
 * `*.node` binary next to itself (or via `NAPI_RS_NATIVE_LIBRARY_PATH`). In a
 * *published* install the binary ships in the package tarball, so a bare
 * `require("zeroship-migrate-node")` resolves it.
 *
 * In dev the engine dep is `workspace:*`: pnpm resolves a workspace member by
 * SYMLINK to `crates/zeroship-migrate-node`, not by packing a tarball, so dev
 * points at the in-tree crate in place - binary included.
 *
 * The fallback below is kept for the cases the symlink does not cover.
 *
 * This loader makes both cases work with no build-graph coupling: it first tries
 * the ordinary require, and on failure re-points napi's own
 * `NAPI_RS_NATIVE_LIBRARY_PATH` env hook at the `*.node` the addon crate built
 * in place, then retries. When the addon is eventually extracted/published this
 * file's fallback simply never fires — a move, not an untangle.
 */

import { createRequire } from "node:module";
import { existsSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";

// The addon's own generated declarations, not a copy of them.
//
// Importing rather than mirroring turns every REQUIRED field the engine adds
// into a red build here instead of a runtime failure at a creator's build: a
// hand-mirrored copy would let a source this file no longer satisfies still
// compile.
//
// The runtime error is worse than "it fails later", which is why importing
// matters more than it looks: a source missing BOTH `dialect` and
// `charterLayers` reports
//
//     Missing field `dialect`
//
// and stops. No function name, no argument position, and no mention of the
// second missing field - the deserializer names the first and returns. Fix what
// it told you, rebuild, and you meet the next one. An N-field contract change
// becomes N builds, each looking like the last.
//
// tsc reports every missing property in one error instead, and does so
// regardless of `strict` - it is ordinary assignability, not a strictness
// feature - so a consumer who has never turned strict on still gets the whole
// list. The engine exports these types (`types: index.d.ts`, and the file is in
// `files`).
import type {
  CollectionDescriptorDto,
  FieldDescriptorDto,
  GenArtifactsReply as AddonGenArtifactsReply,
  GenArtifactsSource,
  IndexDescriptorDto,
  RuntimeOptionsDto,
  ApplyRequest,
  ApplyReply,
} from "zeroship-migrate-node";

export type {
  CollectionDescriptorDto,
  FieldDescriptorDto,
  GenArtifactsSource,
  IndexDescriptorDto,
  RuntimeOptionsDto,
  ApplyRequest,
  ApplyReply,
};

const require = createRequire(import.meta.url);

/**
 * The reply gen-types consumes (or a soft error).
 *
 * The addon also returns an `envDbTs`, deliberately NOT declared here: it
 * re-authors the folded IR in the *migration* DSL (`from "@zeroship/migrate"`,
 * `t.text().primaryKey()`), which is the engine's own artifact and neither
 * resolvable nor type-bearing in a creator app. gen-types renders the typed
 * `env.db` surface itself from `runtimeJson` (see `render-env-db.ts`).
 *
 * WHAT THIS DOES NOT CATCH, because `Pick` enumerates: a field the engine ADDS
 * to `GenArtifactsReply` is invisible here until someone edits this line. That
 * is the one protection the type-only import does NOT buy for this type - the
 * seven types re-exported verbatim above do get it. Stated because the engine
 * adds reply keys without any consumer edit - `hasDialectalOps` arrived
 * exactly that way and is triaged below.
 *
 * What the narrowing DOES buy, and the reason it stays: if a picked field
 * changes shape or is removed, that is a compile error rather than a runtime
 * `undefined`. Widening to the full reply to catch future additions would
 * re-admit `envDbTs`, which is excluded above for a stated reason - so the
 * deliberate trade is a known one-line edit when a needed field lands.
 */
export type GenArtifactsReply = Pick<AddonGenArtifactsReply, "ok" | "runtimeJson" | "error">;

/**
 * Closes the `Pick` blind spot described above WITHOUT widening the `Pick`.
 *
 * Every key of the engine's reply must be one this file has triaged: either
 * consumed by the `Pick`, or deliberately dropped (`envDbTs`). A key the engine
 * ADDS belongs to neither set, so `UntriagedReplyKeys` stops being `never` and
 * the `AssertNever` instantiation below fails to compile - naming the new key in
 * the error. That is the compile-time signal the `Pick`
 * alone could not give, because `Pick` enumerates what it wants rather than
 * reacting to what arrives.
 *
 * Deleting a name from the exclusion list is what proves this load-bearing:
 * drop `"envDbTs"` and the build goes red with TS2344 naming `"envDbTs"`.
 * This is type-only and erases at compile time - it loads no addon binary.
 */
// `hasDialectalOps` is triaged as a deliberate DROP rather than added to the `Pick`: it
// reports whether the fold emitted any dialect-specific op, which is a fact
// about the migration set, not about the types gen-types renders. gen-types
// builds the `env.db` surface from `runtimeJson` alone and has no branch that
// would consult it. Its own doc is explicit that `false` does not even mean the
// artifacts are dialect-independent, so it could not serve as such a branch.
// `collections` + `dialect` are triaged TOGETHER because the engine says they
// are only meaningful together: `collections` is the structured export (the
// folded schema as typed `CollectionDescriptorDto`s, offered so a host can
// render artifacts instead of re-parsing `runtimeJson`), and `Op::Dialectal`
// leg selection changes WHICH COLUMNS EXIST - so the export is meaningless
// without the dialect it was folded under, and anything storing or forwarding
// one must carry the other.
//
// Both are dropped only because gen-types renders from `runtimeJson` today.
// The structured export is arguably the better source for exactly this file and
// would remove a parse. Recorded as a deliberate deferral, not a rejection: if
// that switch happens, BOTH names move into the `Pick` above, never just one.
type UntriagedReplyKeys = Exclude<
  keyof AddonGenArtifactsReply,
  | "ok"
  | "runtimeJson"
  | "error"
  | "envDbTs"
  | "hasDialectalOps"
  | "collections"
  | "dialect"
>;
type AssertNever<T extends never> = T;
export type _NoUntriagedAddonReplyKeys = AssertNever<UntriagedReplyKeys>;

/** The verbs the dev tier needs, so the addon's larger apply-side surface is
 *  not reachable from here.
 *
 *  `applyIr` is the dev tier's schema authority: the dev server applies the
 *  committed migrations to the dev SQLite file AHEAD of the worker, in authored
 *  order, exactly as `migrated` does for Postgres at deploy. The worker never
 *  renders the descriptor into DDL.
 *
 *  The host-driver parameter is typed `null` here rather than omitted. The addon
 *  accepts a callback there, and one verb serves both transports; narrowing the
 *  type is what keeps the dev tier from reaching a networked database through it.
 *  Which side opens the connection is `req.driver`.
 *
 *  Its request/reply types are IMPORTED from `zeroship-migrate-node`, never
 *  re-declared here. A hand-mirrored copy is how a field the engine adds turns
 *  into a runtime failure instead of a compile error. */
interface MigrateAddon {
  genArtifacts(source: GenArtifactsSource): GenArtifactsReply;
  irVersion(): number;
  applyIr(hostDriver: null, req: ApplyRequest): Promise<ApplyReply>;
}

let cached: MigrateAddon | undefined;

/**
 * Load the `zeroship-migrate-node` addon, caching the module. Throws a descriptive
 * error (never a bare napi "Cannot find native binding") when neither the normal
 * resolution nor the standalone-build fallback locate the binary.
 */
export function loadMigrateAddon(): MigrateAddon {
  if (cached) return cached;

  try {
    cached = require("zeroship-migrate-node") as MigrateAddon;
    return cached;
  } catch (primary) {
    const bindingPath = findStandaloneBinding();
    if (bindingPath) {
      const prev = process.env.NAPI_RS_NATIVE_LIBRARY_PATH;
      process.env.NAPI_RS_NATIVE_LIBRARY_PATH = bindingPath;
      try {
        cached = require("zeroship-migrate-node") as MigrateAddon;
        return cached;
      } catch (retry) {
        throw addonLoadError(primary, retry, bindingPath);
      } finally {
        if (prev === undefined) delete process.env.NAPI_RS_NATIVE_LIBRARY_PATH;
        else process.env.NAPI_RS_NATIVE_LIBRARY_PATH = prev;
      }
    }
    throw addonLoadError(primary, undefined, undefined);
  }
}

/**
 * Locate the standalone repo's prebuilt `*.node` binary. napi-rs names it
 * `zeroship-migrate-node.<platform>-<arch>[-<abi>].node`. Returns the first such file
 * found, or `null` when none is found.
 *
 * THE "PUBLISHED INSTALL" ARM DESCRIBED BELOW DOES NOT EXIST YET.
 * `zeroship-migrate-node` is NOT published and is not in `publish_packages` in
 * `deploy/scripts/publish-packages.sh`, which REFUSES to publish
 * `@zeroship/vite-plugin` for exactly that reason -
 *   "2 dependency(ies) on workspace packages that are not published:
 *    @zeroship/vite-plugin [dependencies] zeroship-migrate-node@workspace:*"
 * So in a real registry install the bare `require` cannot succeed and this
 * fallback cannot find a binary either; the creator gets `addonLoadError`. The
 * monorepo arm below is the ONLY arm that works. Tracked as the SDK
 * distribution blocker; do not read this loader as evidence that the published
 * path is covered.
 *
 * One candidate dir is scanned: the RESOLVED addon package dir
 * (`require.resolve("zeroship-migrate-node/…")`). In a published install that is
 * where the binary WOULD ship, next to `index.js` (unverified - see above); in
 * this monorepo the
 * `workspace:*` dep makes it a symlink straight to
 * `crates/zeroship-migrate-node`, where a dev `napi build`
 * leaves the freshly-built `.node` in place.
 */
function findStandaloneBinding(): string | null {
  return scanForBinding(resolvedAddonDir());
}

/** Scan one dir for a `zeroship-migrate-node.*.node` file; return its path or null. */
function scanForBinding(dir: string | null): string | null {
  if (!dir || !existsSync(dir)) return null;
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return null;
  }
  const hit = names.find(
    (f) => f.startsWith("zeroship-migrate-node.") && f.endsWith(".node"),
  );
  return hit ? join(dir, hit) : null;
}

/** The resolved `zeroship-migrate-node` package dir (a workspace symlink in dev). */
function resolvedAddonDir(): string | null {
  try {
    return dirname(require.resolve("zeroship-migrate-node/package.json"));
  } catch {
    return null;
  }
}

function addonLoadError(
  primary: unknown,
  retry: unknown,
  bindingPath: string | undefined,
): Error {
  const reasons = [
    `primary: ${(primary as Error)?.message ?? String(primary)}`,
    bindingPath ? `fallback (${bindingPath}): ${(retry as Error)?.message ?? String(retry)}` : null,
  ]
    .filter(Boolean)
    .join("; ");
  return new Error(
    "gen-types: failed to load the zeroship-migrate-node addon — the schema-artifact " +
      "emitter cannot run. Ensure the addon's native binary is built/installed. " +
      `(${reasons})`,
  );
}

/** Re-export for tests: whether a standalone binding path is discoverable. */
export { findStandaloneBinding as _findStandaloneBinding };
