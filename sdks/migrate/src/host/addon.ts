// Addon loader (design §D.3/§E) — resolve + load the prebuilt N-API `.node` from
// `crates/zeroship-migrate-node` (Phase C) and type its exposed surface.
//
// The addon is the V8-free Rust core (`host-pg` + bundled rusqlite, no compio, no
// io_uring, cross-platform). It exposes:
//  - sync, DB-free: `irVersion()`, `loadVerify(...)` (§C.5)
//  - async, host-driven (fire-and-resolve over a `hostDriver` TSFN): `applyIr(...)`,
//    `apply(...)`, `status(...)`, `history(...)` (§C.5/§D.1)
//
// The host-driver callback contract is `hostDriver([request, done]) => void` —
// napi delivers `(request, done)` as a SINGLE array arg (§B.3). See `driver-pg.ts`.

import { createRequire } from "node:module";

/** The `hostDriver([request, done]) => void` callback the async entrypoints drive
 *  (§B.3). Shaped exactly by `driver-pg.ts`'s `HostDriver`. */
export type AddonHostDriver = (args: [request: unknown, done: (err: unknown, reply: unknown) => void]) => void;

/** The addon's exposed surface (mirrors `crates/zeroship-migrate-node/index.d.ts`
 *  plus the Phase-D `applyIr`). */
export interface MigrateAddon {
  /** The IR-format version this addon was built against — the single source of
   *  truth across the boundary (§D.1). */
  irVersion(): number;

  /** Sync, DB-free load + verify of an `.ir.json` document (§C.5). Returns a JSON
   *  `LoadVerifyReport`. */
  loadVerify(irJson: string, deployingApp: string, dialect: string, registryJson: string): string;

  /** HOST-AUTHORING apply (§D.1): take a pure-JS `.ir.json` ENVELOPE, LOWER it in
   *  Rust (stamp `owner_app` + fold `Checksum::of_ir`), then drive `executor::apply`
   *  over the host driver. Resolves to a JSON `ApplyOutcome`. */
  applyIr(
    hostDriver: AddonHostDriver,
    ownerApp: string,
    projectSchema: string,
    migratorRole: string | undefined | null,
    dialect: string,
    registryJson: string,
    envelopeJson: string,
    approved: boolean,
    appliedBy: string,
  ): Promise<string>;

  /** Apply a pre-lowered `Vec<Migration>` (JSON) over the host driver (§C.5). */
  apply(
    hostDriver: AddonHostDriver,
    projectId: string,
    projectSchema: string,
    migratorRole: string | undefined | null,
    migrationsJson: string,
    approved: boolean,
    appliedBy: string,
  ): Promise<string>;

  /** `status` over the host driver (§C.5). Resolves to a JSON `MigrationStatus`. */
  status(
    hostDriver: AddonHostDriver,
    projectId: string,
    projectSchema: string,
    migrationsJson: string,
  ): Promise<string>;

  /** `history` over the host driver (§C.5). Resolves to a JSON `Vec<HistoryEvent>`. */
  history(hostDriver: AddonHostDriver, projectId: string, projectSchema: string): Promise<string>;
}

let cached: MigrateAddon | null = null;

/**
 * Load the prebuilt `.node`. Resolution order (§E — napi-rs prebuild convention):
 *  1. `ZEROSHIP_MIGRATE_NATIVE` env override (an explicit `.node` path — used by the
 *     oracle to point at the freshly-`napi build`'d artifact in the addon crate).
 *  2. the bundled `native/migrate.<platform>.node` shipped with this package.
 *  3. the sibling addon crate's `zeroship-migrate-node.<triple>.node` (dev, when
 *     this package's `native/` is not yet populated).
 *
 * @throws if no `.node` can be resolved/loaded (with the tried paths).
 */
export function loadAddon(): MigrateAddon {
  if (cached) return cached;
  const require = createRequire(import.meta.url);
  const tried: string[] = [];

  const candidates = addonCandidates();
  for (const p of candidates) {
    try {
      // eslint-disable-next-line @typescript-eslint/no-var-requires
      const mod = require(p) as MigrateAddon;
      if (mod && typeof mod.irVersion === "function") {
        cached = mod;
        return mod;
      }
      tried.push(`${p} (loaded but missing irVersion)`);
    } catch (e) {
      tried.push(`${p} (${(e as Error).message})`);
    }
  }
  throw new Error(
    "@zeroship/migrate/host: could not load the native addon (.node). Tried:\n  " +
      tried.join("\n  ") +
      "\nSet ZEROSHIP_MIGRATE_NATIVE to an explicit .node path, or run `napi build` in " +
      "crates/zeroship-migrate-node and ship native/.",
  );
}

/** The ordered `.node` candidate paths (see {@link loadAddon}). */
function addonCandidates(): string[] {
  const out: string[] = [];
  const override = process.env.ZEROSHIP_MIGRATE_NATIVE;
  if (override) out.push(override);
  const { platform, arch } = process;
  // The napi-rs default triple spelling (linux-x64-gnu, darwin-arm64, win32-x64, …).
  const abi = platform === "linux" ? "-gnu" : "";
  const triple = `${platform}-${arch}${abi}`;
  // Bundled with the package (populated by the prebuild step). `import.meta.url` is
  // the compiled `dist/host.js`, so the package root is `../` and `native/` sits
  // beside `dist/`.
  out.push(new URL(`../native/migrate.${triple}.node`, import.meta.url).pathname);
  out.push(new URL(`../native/index.node`, import.meta.url).pathname);
  // Dev fallback: the sibling addon crate's build output. From `sdks/migrate/dist/`
  // the worktree root is `../../../`.
  out.push(
    new URL(
      `../../../crates/zeroship-migrate-node/zeroship-migrate-node.${triple}.node`,
      import.meta.url,
    ).pathname,
  );
  out.push(
    new URL(`../../../crates/zeroship-migrate-node/index.node`, import.meta.url).pathname,
  );
  return out;
}

/** The addon's IR-format version — the single source of truth (§D.1). */
export function currentIrVersion(): number {
  return loadAddon().irVersion();
}
