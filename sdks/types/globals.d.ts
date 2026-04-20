/**
 * @zeroship/types — TypeScript declarations for the zeroship runtime.
 *
 * Usage:
 *   npm install -D @zeroship/types
 *   // tsconfig.json: { "types": ["@zeroship/types"] }
 *
 * The primary entry is the `"zeroship"` module declaration in
 * `zeroship.d.ts` — SDK code does `import { env } from "zeroship"` and
 * reads plugin namespaces off `env.db`, `env.auth`, etc.
 *
 * The `zeroship` global below is SOFT-DEPRECATED — it was the old
 * pre-kernel-cut access path (`zeroship.db.find(...)` at global scope)
 * and no longer backs SDK calls. The interface descriptions in
 * `db.d.ts` and `auth.d.ts` are still useful as the authoritative
 * shape for plugin namespaces surfaced on `env.*`, which is why they
 * stay in place. Apps that still reach for `globalThis.zeroship` will
 * TypeScript-compile against this declaration but fail at runtime.
 */

/// <reference path="shared.d.ts" />
/// <reference path="db.d.ts" />
/// <reference path="auth.d.ts" />
/// <reference path="zeroship.d.ts" />

// ---------------------------------------------------------------------------
// Global namespace (soft-deprecated — use `import { env } from "zeroship"` instead)
// ---------------------------------------------------------------------------

/**
 * The old zeroship runtime global.
 *
 * @deprecated Import `env` from the `"zeroship"` module and read
 * `env.db` / `env.auth` instead. This global is left in place for
 * documentation of the plugin namespace shape; it no longer exists on
 * the V8 isolate after the kernel-cut refactor (PR 1 D1).
 */
interface ZeroshipGlobal {
  db: ZeroshipDb;
  auth: ZeroshipAuth;
}

declare var zeroship: ZeroshipGlobal;
