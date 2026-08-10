import { strict as assert } from "node:assert";
import { test } from "node:test";

import { formatFatalBanner } from "../src/dev-server.js";

/**
 * The banner used to print the `devServerPort` remedy for EVERY boot failure.
 * After the plugin-kv change that names the process holding `.zeroship/kv.redb`,
 * that made the two halves of one screen contradict each other: the runtime said
 * changing the port would not help, and the banner underneath said to change the
 * port. Task #221.
 *
 * These two cases differ in ONE variable - whether the captured runtime output
 * carries redb's lock wording. A single case would only prove the banner renders;
 * the pair proves it DISCRIMINATES, which is the whole claim.
 */

function status(logTail: string[]) {
  // Only the fields the banner reads; the rest of RuntimeStatus is irrelevant
  // here and inventing values for it would obscure what drives the branch.
  return { port: 3001, logTail } as unknown as Parameters<typeof formatFatalBanner>[0];
}

test("a state-dir lock suppresses the port remedy and points at the holder", () => {
  const banner = formatFatalBanner(
    status([
      "[zeroship] kv: failed to open redb at '.zeroship/kv.redb': " +
        "kv: redb open '.zeroship/kv.redb': Database already open. Cannot acquire lock. " +
        "-- still held by pid 4242 (zeroship)",
    ]),
  );

  // Assert on the RECOMMENDATION, not on the token. The lock branch mentions
  // `devServerPort` on purpose, to rule it out - a bare `!includes("devServerPort")`
  // measures a proxy for the property and fails on correct output. It did exactly
  // that on the first run of this test.
  assert.ok(
    !banner.includes("zeroship({ devServerPort:"),
    "banner RECOMMENDED setting devServerPort for a state-dir lock, contradicting " +
      `the runtime output it just printed:\n${banner}`,
  );
  assert.ok(banner.includes("will NOT help"), banner);
  assert.ok(banner.includes("STATE DIR lock"), banner);
  assert.ok(banner.includes("Kill"), banner);
});

test("any other failure still gets the port remedy", () => {
  const banner = formatFatalBanner(
    status(["[zeroship] some unrelated boot failure: entry module not found"]),
  );

  assert.ok(
    banner.includes("zeroship({ devServerPort:"),
    `banner dropped the port remedy for a non-lock failure:\n${banner}`,
  );
  assert.ok(!banner.includes("STATE DIR lock"), banner);
});

/**
 * What these do NOT cover: that `RedbBackend::open` really emits this wording.
 * That is pinned on the Rust side by
 * `backend::redb::tests::second_open_names_the_holding_process`. If the two ever
 * drift, both stay green and the banner silently reverts to the wrong remedy -
 * the match string here is the seam, and it is not gated end to end.
 */
