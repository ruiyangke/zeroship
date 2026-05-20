"use server";

/**
 * Internal entry point for @zeroship/db — surfaced as
 * `@zeroship/db/internal` via the package.json `exports` map.
 *
 * Consumers: dev-bootstrap (`sdks/vite-plugin/src/dev-bootstrap/index.ts`)
 * and the production runtime bootstrap (next slice). User code MUST
 * NOT import from this entry point — the surface is unstable and
 * exists to support the `export default { schema }` convention.
 */

export { __registerSchemas } from "./db.js";
