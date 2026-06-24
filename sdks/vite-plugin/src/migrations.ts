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
import { createHash } from "node:crypto";
import { join } from "node:path";
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
   *  recorder-unreachable falls back to the LOCAL recorder (NOT a build failure). */
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
