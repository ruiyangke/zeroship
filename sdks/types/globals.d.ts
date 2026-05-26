/**
 * @zeroship/types — TypeScript declarations for the zeroship runtime.
 *
 * Usage:
 *   npm install -D @zeroship/types
 *   // base runtime only:
 *   //   tsconfig.json: { "types": ["@zeroship/types"] }
 *   // typed env.db collections:
 *   //   tsconfig.json: { "types": ["@zeroship/types", "@zeroship/db/env"] }
 *
 * The entry is the `"zeroship"` module declaration in `zeroship.d.ts`:
 * SDK code does `import { env } from "zeroship"` and reads plugin
 * namespaces off `env.db`, `env.auth`, etc. The native interface shapes
 * in `db.d.ts` / `auth.d.ts` are the authoritative spec for the plugin
 * namespaces surfaced on `env.*`. The higher-level `env.db.<collection>`
 * schema narrowing lives in `@zeroship/db/env`; include both packages in
 * tsconfig `"types"` for typed application collections.
 */

/// <reference path="shared.d.ts" />
/// <reference path="db.d.ts" />
/// <reference path="auth.d.ts" />
/// <reference path="zeroship.d.ts" />
