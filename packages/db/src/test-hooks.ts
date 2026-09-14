/**
 * Test-only hooks for `@zeroship/db`'s internal warning sets. Kept in a
 * separate module so the main entry's public surface doesn't carry
 * `__zeroship*` symbols whose only consumer is the test suite. The file
 * is intentionally NOT re-exported from `./index.ts`; tests import it
 * via the explicit `../src/test-hooks.js` subpath.
 *
 * If you're consuming the SDK from app code and you find yourself
 * reaching for any of these helpers — you don't need them. Reset
 * happens automatically; the only legitimate use is a test runner
 * that wants deterministic warning behaviour across cases.
 */

export {
  __zeroshipDbResetIndexWarnings,
  __zeroshipDbWarnedShapesSize,
} from "./collection";

export {
  __zeroshipDbResetAccShapeWarnings,
  __zeroshipDbWarnedAccShapesSize,
} from "./utils";
