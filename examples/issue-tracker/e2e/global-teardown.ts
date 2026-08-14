import { request } from "@playwright/test";

import { signDevSession } from "./session";

/**
 * Sweep the fixture groups this suite leaves behind.
 *
 * The dev database persists between runs and every spec that exercises access
 * control creates a group. Nothing removed them, so they accumulated: 95 at
 * the point group deletion was added, 176 a few runs later, on a page whose
 * subject is products. A tracker with 176 groups is not a useful thing to look
 * at while developing.
 *
 * Here rather than in each spec, for two reasons. A spec that fails skips its
 * own teardown, which is exactly when it has left the most mess; and a spec
 * written next month would have to remember. This cannot be forgotten and
 * needs no cooperation.
 *
 * SAFETY: it deletes by calling the same `groups.delete` a person would, so
 * the server's guard still applies -- a group that still restricts an issue or
 * product is refused and left alone. This cannot widen anyone's visibility,
 * and the refusals are reported rather than hidden, because a rising refused
 * count means the restrict specs are leaving restrictions behind.
 *
 * WHAT IT DOES NOT DO: unrestrict anything to make a group deletable. Nothing
 * exposes which issues a group restricts, and guessing is not worth it for a
 * cleanup task.
 */

/** Only names this suite generates. Anything a person made by hand stays. */
const FIXTURE_GROUP = /^(sec|vis|security|mem|unused|in-use|grp)-/;

export default async function globalTeardown(): Promise<void> {
  // Must track playwright.config.ts. It said 5179 while the config said 5183,
  // so every sweep failed with ECONNREFUSED and reported nothing -- a cleanup
  // that quietly stops cleaning is worse than none, because the group count
  // keeps rising and the teardown line keeps looking fine.
  const webPort = Number(process.env.ISSUE_TRACKER_WEB_PORT ?? 5183);
  const runtimePort = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);
  const baseURL = `http://localhost:${webPort}`;

  let context;
  try {
    context = await request.newContext({
      baseURL,
      extraHTTPHeaders: {
        cookie: `__zeroship_dev_session=${signDevSession(runtimePort)}`,
      },
    });
  } catch (err) {
    // Never fail the run over cleanup: the suite's verdict is about the app.
    console.warn(`[teardown] could not open a request context: ${String(err)}`);
    return;
  }

  try {
    const listed = await context.post("/__zeroship/v1/groups.list", { data: { json: {} } });
    if (!listed.ok()) {
      console.warn(`[teardown] groups.list answered ${listed.status()}; skipping sweep`);
      return;
    }
    const groups = (await listed.json()).json as Array<{ id: string; name: string }>;
    const targets = groups.filter((g) => FIXTURE_GROUP.test(g.name));

    let deleted = 0;
    let stillInUse = 0;
    for (const group of targets) {
      const res = await context.post("/__zeroship/v1/groups.delete", {
        data: { json: { id: group.id } },
      });
      if (res.ok()) deleted++;
      else stillInUse++;
    }
    if (targets.length > 0) {
      console.log(
        `[teardown] fixture groups: ${targets.length} found, ${deleted} deleted, ` +
          `${stillInUse} still restricting something and left alone`,
      );
    }
  } catch (err) {
    console.warn(`[teardown] sweep did not complete: ${String(err)}`);
  } finally {
    await context.dispose();
  }
}
