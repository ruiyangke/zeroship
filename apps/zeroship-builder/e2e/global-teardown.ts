// ─── Global teardown ─────────────────────────────────────────────
//
// Sweeps any apps prefixed `e2e-` that the suite may have leaked
// (a crash mid-test, a timeout, an aborted run). Always runs even
// if tests failed.

import { request as createRequest } from "@playwright/test";
import { cleanupLeakedTestApps } from "./helpers";

export default async function globalTeardown() {
  const request = await createRequest.newContext();
  try {
    const swept = await cleanupLeakedTestApps(request);
    if (swept > 0) {
      // eslint-disable-next-line no-console
      console.log(`[e2e] swept ${swept} leaked test apps`);
    }
  } catch (e) {
    // eslint-disable-next-line no-console
    console.warn("[e2e] cleanup failed:", e);
  } finally {
    await request.dispose();
  }
}
