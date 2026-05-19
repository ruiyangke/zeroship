/**
 * @zeroship/types — TypeScript declarations for the zeroship runtime.
 *
 * Usage:
 *   npm install -D @zeroship/types
 *   // tsconfig.json: { "types": ["@zeroship/types"] }
 *
 * The entry is the `"zeroship"` module declaration in `zeroship.d.ts`:
 * SDK code does `import { env } from "zeroship"` and reads plugin
 * namespaces off `env.db`, `env.auth`, etc. The interface shapes in
 * `db.d.ts` / `auth.d.ts` are the authoritative spec for the plugin
 * namespaces surfaced on `env.*`.
 */

/// <reference path="shared.d.ts" />
/// <reference path="db.d.ts" />
/// <reference path="auth.d.ts" />
/// <reference path="zeroship.d.ts" />
