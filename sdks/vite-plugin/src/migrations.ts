/**
 * PR4 deliverable A4 — migration discovery + bundling for the `.zship` packer.
 *
 * The vite-plugin is a THIN client of the SAME PR4a kernel-sandboxed recorder the
 * platform uses. It does NOT evaluate untrusted migration `.ts` in-process: the
 * recorder is Rust, the vite-plugin shells the `zeroship-migrate-js` CLI (the
 * `build`/`record` subcommands). This keeps the TS↔Rust boundary clean and the
 * security-critical evaluation inside the kernel sandbox.
 *
 * Flow (per the design §5.1 build-once authority):
 *  1. Discover `migrations/*.ts` (configurable dir, default `migrations/`).
 *  2. For each `.ts` lacking a committed `<name>.ir.json`, invoke the recorder via
 *     the CLI to produce the committed artifact. If `ZEROSHIP_RECORDER_URL` (or the
 *     plugin option) is set, the CLI uses the hosted thin client with a local
 *     fallback; otherwise the LOCAL recorder records directly.
 *  3. Read each committed `<name>.ir.json` VERBATIM, content-hash it (sha256), and
 *     contribute `{ name, hash }` entries into `manifest.migrations`. The committed
 *     bytes are staged into the archive exactly like other content blobs — consumed
 *     verbatim, never re-emitted.
 */

import { promises as fs } from "node:fs";
import { existsSync } from "node:fs";
import { createHash } from "node:crypto";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

/** A 64-char lowercase sha256 hex string. */
export type Sha256Hex = string;

/** One migration file contributed to `manifest.migrations` + its committed bytes
 *  (so the packer can stage the blob — consumed verbatim, never re-emitted). */
export interface MigrationBundleEntry {
  /** The committed `.ir.json` filename, e.g. `20240617123000_create_users.ir.json`. */
  name: string;
  /** sha256 (lowercase, 64 hex) of the committed `.ir.json` bytes on disk. */
  hash: Sha256Hex;
  /** The committed `.ir.json` bytes — exactly what the packer stages + hashes. */
  bytes: Buffer;
}

export interface DiscoverMigrationsOptions {
  /** Project root. */
  root: string;
  /** Migrations dir relative to root (default `migrations`). */
  migrationsDir?: string;
  /** The declaring/deploying app (`app_…`) stamped on the IR. */
  ownerApp?: string;
  /** The hosted recorder URL (the §8.9.2 thin client). Falls back to
   *  `ZEROSHIP_RECORDER_URL`. When set, the CLI ships each `.ts` to the recorder;
   *  recorder-unreachable falls back to the LOCAL recorder (NOT a build failure).
   *
   *  NOTE: the dev `zeroship-migrate-js` CLI does NOT embed a hosted-recorder HTTP
   *  client — setting this makes the CLI's `build --recorder-url` ALWAYS fall back to
   *  LOCAL recording (it prints a loud per-file warning). Hosted, canonical
   *  kernel-sandboxed recording happens on the platform via the control plane at
   *  deploy (the §5.1 provenance re-record), not from this dev tool. Security is
   *  preserved either way; this option exists for forward-compat + parity. */
  recorderUrl?: string;
  /** Path to the `zeroship-migrate-js` CLI binary (default `zeroship-migrate-js`
   *  on PATH; override for tests / packaged installs). */
  cliPath?: string;
  /** When true, do NOT shell the recorder for a `.ts` lacking a committed
   *  `.ir.json` — fail instead (used by tests asserting the verbatim-consume
   *  contract over a pre-committed dir). Default false. */
  noRecord?: boolean;
}

/** The `<14-digit>_<desc>` migration filename grammar (desc = [A-Za-z0-9_]+). */
const MIGRATION_TS_RE = /^(\d{14})_([A-Za-z0-9_]+)\.ts$/;

/**
 * Discover + (record if needed) + read the committed `.ir.json` artifacts under
 * `<root>/<migrationsDir>`. Returns the ordered (by 14-digit version) bundle
 * entries. An empty / missing migrations dir yields `[]` (the app ships no
 * migrations — a no-op).
 */
export async function discoverMigrations(
  opts: DiscoverMigrationsOptions
): Promise<MigrationBundleEntry[]> {
  const migrationsDir = join(opts.root, opts.migrationsDir ?? "migrations");
  let names: string[];
  try {
    names = await fs.readdir(migrationsDir);
  } catch {
    return []; // no migrations dir → ship no migrations
  }

  // Discover the `.ts` sources, sorted by the 14-digit version prefix.
  const tsFiles: { stem: string; version: string; tsName: string }[] = [];
  for (const name of names) {
    const m = MIGRATION_TS_RE.exec(name);
    if (!m) {
      if (name.endsWith(".ts")) {
        throw new Error(
          `migrations: ${name} violates the <14-digit>_<desc>.ts filename grammar`
        );
      }
      continue;
    }
    tsFiles.push({ stem: name.slice(0, -3), version: m[1], tsName: name });
  }
  tsFiles.sort((a, b) =>
    a.version < b.version ? -1 : a.version > b.version ? 1 : a.stem < b.stem ? -1 : 1
  );

  const entries: MigrationBundleEntry[] = [];
  for (const f of tsFiles) {
    const irName = `${f.stem}.ir.json`;
    const irPath = join(migrationsDir, irName);
    let committed: Buffer | null = null;
    try {
      committed = await fs.readFile(irPath);
    } catch {
      committed = null;
    }
    if (committed == null) {
      if (opts.noRecord) {
        throw new Error(
          `migrations: ${f.tsName} has no committed ${irName} (recording disabled)`
        );
      }
      // Shell the recorder CLI to produce the committed artifact (LOCAL, or hosted
      // thin client with local fallback when a recorder URL is set).
      recordViaCli(migrationsDir, f.tsName, opts);
      committed = await fs.readFile(irPath);
    }
    // Read VERBATIM, content-hash exactly the on-disk bytes (packer copies).
    const hash = sha256Hex(committed);
    entries.push({ name: irName, hash, bytes: committed });
  }
  return entries;
}

/** Shell the `zeroship-migrate-js record <file.ts>` CLI (the LOCAL recorder) — or
 *  `build --recorder-url` when a hosted URL is set — to produce the committed
 *  `.ir.json`. The recorder is Rust + kernel-sandboxed; the vite-plugin never
 *  evaluates the untrusted `.ts` in-process. */
function recordViaCli(
  migrationsDir: string,
  tsName: string,
  opts: DiscoverMigrationsOptions
): void {
  const cli = opts.cliPath ?? "zeroship-migrate-js";
  const ownerApp = opts.ownerApp ?? "app_local";
  const recorderUrl = opts.recorderUrl ?? process.env.ZEROSHIP_RECORDER_URL;
  const args = recorderUrl
    ? ["build", "--dir", migrationsDir, "--owner-app", ownerApp, "--recorder-url", recorderUrl]
    : ["record", join(migrationsDir, tsName), "--owner-app", ownerApp];
  const res = spawnSync(cli, args, { encoding: "utf8" });
  if (res.error) {
    throw new Error(
      `migrations: failed to invoke the recorder CLI (${cli}): ${res.error.message}`
    );
  }
  if (res.status !== 0) {
    throw new Error(
      `migrations: recorder CLI (${cli} ${args.join(" ")}) exited ${res.status}: ${res.stderr}`
    );
  }
}

/** sha256 hex of a buffer — the SAME convention the `.zship` packer + the Rust
 *  `bundle::sha256_hex` use (lowercase, 64 hex chars). */
export function sha256Hex(bytes: Buffer): Sha256Hex {
  return createHash("sha256").update(bytes).digest("hex");
}

// ── Migration-first P3 — `gen-types` wiring ──────────────────────────────────
//
// The vite-plugin is a thin client of the SAME `zeroship-migrate-js` CLI it
// already shells for `record`/`build`. P3 wires in the EXISTING `gen-types`
// subcommand, which folds the committed `.ir.json` set and emits the typed
// `env.db` surface (`env.db.ts` + `schema.runtime.json`) into an output DIR.
//
// P5 activation: the generated `env.db.ts` is the canonical app-level
// `declare module "zeroship" { interface Env { db } }`. Apps commit the
// default output dir and include `generated/zeroship/env.db.ts` in tsconfig.
// The old `@zeroship/db/env` + `zeroship-schema` declared-schema alias is retired.

/** The default `gen-types` output dir (relative to root). Chosen to be COMMITTED
 *  (NOT `.zeroship/`, which is gitignored); apps include its `env.db.ts` in
 *  tsconfig for strong `env.db` typing. */
export const GEN_TYPES_OUT_DEFAULT = "generated/zeroship";

/** The runtime schema descriptor artifact filename `gen-types` emits into
 *  `GEN_TYPES_OUT_DEFAULT`. MUST match the Rust
 *  `zeroship_migrate::frontend::gen_types::RUNTIME_DESCRIPTOR_FILE` — the `.zship`
 *  packer reads this file (when present) and carries it as the manifest's
 *  content-addressed `runtime_descriptor` blob (migration-first P4a). */
export const RUNTIME_DESCRIPTOR_FILE = "schema.runtime.json";

/** The bare PATH name of the `gen-types` CLI — the same binary `recordViaCli`
 *  shells (migrations.ts `record`/`build`). When the binary is on `$PATH` (the
 *  natural `cargo install` location) but not in `node_modules/.bin`, the prod
 *  drift gate falls through to this so `spawnSync` resolves it via `$PATH`,
 *  exactly as the recorder does — instead of spuriously hard-failing "not found"
 *  on a legitimately-configured CI host. */
const GEN_TYPES_CLI_BIN = "zeroship-migrate-js";

/** Options shared by the gen-types helpers (binary resolution + dir). */
export interface GenTypesOptions {
  /** Project root. */
  root: string;
  /** Migrations dir relative to root (default `migrations`). */
  migrationsDir?: string;
  /** The gen-types output dir relative to root (default `generated/zeroship`). */
  genTypesOut?: string;
  /** Explicit path to the `zeroship-migrate-js` CLI binary. When set, it is
   *  used verbatim (tests / packaged installs); no graceful dev-skip probing. */
  cliPath?: string;
}

/** Outcome of {@link genTypesViaCli}. `skipped` is the graceful dev no-op when
 *  the CLI binary is absent (the committed `env.db.ts` stays valid). */
export type GenTypesResult =
  | { status: "ok"; cli: string }
  | { status: "skipped"; reason: string };

/**
 * Resolve the `zeroship-migrate-js` CLI binary, MIRRORING the dev-server's
 * graceful resolution (`dev-server.ts:442-443` `ZEROSHIP_BIN || node_modules/.bin
 * || bare "zeroship"`) and the recorder's `recordViaCli` (which falls through to
 * the bare `"zeroship-migrate-js"` PATH name):
 *
 *  1. an explicit `cliPath` option (tests / packaged installs) — used verbatim;
 *  2. the `ZEROSHIP_MIGRATE_JS_BIN` env override;
 *  3. `<root>/node_modules/.bin/zeroship-migrate-js` if it exists;
 *  4a. when `requireBinary` (the prod/CI drift gate) → the bare `"zeroship-migrate-js"`
 *      PATH name, so `spawnSync` resolves it via `$PATH` exactly as `recordViaCli`
 *      does. A genuinely-missing binary then surfaces a real `spawnSync` ENOENT
 *      (which `genTypesViaCli` throws on) — NOT a spurious "not found" on a host
 *      where the binary is on `$PATH` but not in `node_modules/.bin`;
 *  4b. otherwise (dev) → `null`, so the caller can detect true absence and
 *      warn-once + no-op without a stack trace (the committed env.db.ts stays valid).
 *
 * The `requireBinary` split is the faithful mirror: `record`/`build` always reach
 * the bare PATH name, so the prod drift gate must too — else `record` succeeds in
 * the same build while `--check` hard-fails on the same legitimately-configured machine.
 */
export function resolveGenTypesCli(
  root: string,
  cliPath?: string,
  requireBinary = false
): string | null {
  if (cliPath) return cliPath;
  const fromEnv = process.env.ZEROSHIP_MIGRATE_JS_BIN;
  if (fromEnv) return fromEnv;
  const local = resolve(root, "node_modules/.bin/zeroship-migrate-js");
  if (existsSync(local)) return local;
  // Prod/CI: fall through to the bare PATH name (mirror of recordViaCli) so
  // `$PATH`-installed binaries resolve; a real absence surfaces as ENOENT.
  // Dev: return null so the caller can no-op gracefully.
  return requireBinary ? GEN_TYPES_CLI_BIN : null;
}

/**
 * Shell the EXISTING `zeroship-migrate-js gen-types --dir <migrations> --out
 * <outDir> [--check]` subcommand — the migration-first type emitter. Mirrors
 * `recordViaCli`'s subprocess shape; never evaluates untrusted `.ts` in-process.
 *
 * Binary resolution is graceful (see {@link resolveGenTypesCli}). When the
 * binary is ABSENT:
 *  - `requireBinary` (CI / `--check` gate) → resolution falls through to the bare
 *    `"zeroship-migrate-js"` PATH name (mirror of `recordViaCli`); a genuinely
 *    missing binary then throws via the `spawnSync` ENOENT below. A misconfigured
 *    CI must not silently pass — but a `$PATH`-installed binary must resolve.
 *  - otherwise (dev) → resolution returns `null` and we return `{ status:
 *    "skipped" }` so the caller can warn once and continue — committed artifacts stay valid.
 *
 * A present-binary non-zero exit (e.g. a `--check` drift) ALWAYS throws.
 */
export function genTypesViaCli(
  opts: GenTypesOptions & { check?: boolean; requireBinary?: boolean }
): GenTypesResult {
  const migrationsDir = join(opts.root, opts.migrationsDir ?? "migrations");
  const outDir = join(opts.root, opts.genTypesOut ?? GEN_TYPES_OUT_DEFAULT);

  // `requireBinary` (prod/CI) lets resolution fall through to the bare PATH name;
  // dev gets `null` here only — never the bare name — so the no-op stays graceful.
  const cli = resolveGenTypesCli(opts.root, opts.cliPath, opts.requireBinary);
  if (cli == null) {
    return {
      status: "skipped",
      reason:
        "zeroship-migrate-js not found (ZEROSHIP_MIGRATE_JS_BIN / node_modules/.bin); " +
        "the committed env.db.ts is used as-is",
    };
  }

  const args = [
    "gen-types",
    "--dir",
    migrationsDir,
    "--out",
    outDir,
    ...(opts.check ? ["--check"] : []),
  ];
  const res = spawnSync(cli, args, { encoding: "utf8" });
  if (res.error) {
    // Includes the prod/CI case where the bare PATH name failed to resolve
    // (ENOENT) — a real hard-fail for the drift gate, not a silent pass.
    throw new Error(
      `migrations: failed to invoke the gen-types CLI (${cli}): ${res.error.message}`
    );
  }
  if (res.status !== 0) {
    throw new Error(
      `migrations: gen-types CLI (${cli} ${args.join(" ")}) exited ${res.status}: ${res.stderr}`
    );
  }
  return { status: "ok", cli };
}
