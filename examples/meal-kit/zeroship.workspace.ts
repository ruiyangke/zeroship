// The two facts every process in this workspace has to agree on, in one place.
//
// `locateProjectConfig` does no upward directory walk, so an app started from
// `apps/<label>/` finds no `zeroship.jsonc` at all unless it is told where one
// is. The Vite plugin takes `configPath`; the runtime the dev server spawns is
// a separate process with the app directory as its cwd, and reads the
// `ZEROSHIP_CONFIG` environment variable instead. Both have to name this file
// or the app boots with no database.
//
// `DATABASE_URL` is the same problem for the data: the dev default is
// `.zeroship/dev.sqlite` relative to the process root, which would give each
// app a private database in its own directory. Naming the workspace file makes
// the two apps share one, which is what this example demonstrates.

import { fileURLToPath } from "node:url";

/** `examples/meal-kit/`, whatever the caller's cwd is. */
export const workspaceRoot = fileURLToPath(new URL("./", import.meta.url));

/** The one `zeroship.jsonc` both apps are declared in. */
export const workspaceConfigPath = fileURLToPath(
  new URL("./zeroship.jsonc", import.meta.url),
);

/** The dev SQLite file both apps open, absolute so cwd cannot redirect it. */
export const sharedDatabaseUrl = `sqlite:${fileURLToPath(
  new URL("./.zeroship/dev.sqlite", import.meta.url),
)}`;

/**
 * Put both on `process.env` so the spawned runtime inherits them.
 *
 * Called from each app's `vite.config.ts` at module scope, which is the one
 * file loaded by every way of starting an app - `pnpm dev`, `pnpm exec vite`
 * in an app directory, `vite build`, the browser fixture. A shell value wins,
 * so `DATABASE_URL=... pnpm dev` still redirects the pair.
 */
export function useWorkspaceEnvironment(): void {
  process.env.ZEROSHIP_CONFIG ??= workspaceConfigPath;
  process.env.DATABASE_URL ??= sharedDatabaseUrl;
}
