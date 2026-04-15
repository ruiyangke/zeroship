/**
 * @zeroship/types — TypeScript declarations for the zeroship runtime.
 *
 * These types describe the `zeroship` global object available inside
 * V8 isolates. Install as a dev dependency for autocomplete and type
 * checking when calling native primitives directly.
 *
 * Usage:
 *   npm install -D @zeroship/types
 *   // tsconfig.json: { "types": ["@zeroship/types"] }
 */

/// <reference path="shared.d.ts" />
/// <reference path="db.d.ts" />
/// <reference path="auth.d.ts" />

// ---------------------------------------------------------------------------
// Global namespace
// ---------------------------------------------------------------------------

/** The zeroship runtime global — available in V8 isolates. */
interface ZeroshipGlobal {
  db: ZeroshipDb;
  auth: ZeroshipAuth;
}

declare var zeroship: ZeroshipGlobal;
