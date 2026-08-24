// The host extension claim is the SAME lock the Rust suites take, it excludes a
// second run, and it says so when it loses.
//
// `extension-claim.ts` is the reason `rollback-live.test.ts` and
// `citext-prerequisite.test.ts` can install - or require the absence of - a
// DATABASE-GLOBAL object while a sibling gate run does the same. Its Rust counterpart
// is guarded by `crates/zero-migrate/tests/rollback/extension_claim_is_exclusive.rs`
// and this file is the matching guard on this side, plus the one property neither
// language can assert alone:
//
//   0. THE TWO HALVES TAKE ONE LOCK. The key is a string both languages hand to the
//      server's `hashtext`, so the lock is shared exactly when the two strings are.
//      Nothing else couples them: a rename on either side would leave two suites each
//      holding "their" claim and still colliding, and every test in both trees would
//      stay green. So the Rust `claim_key` is READ from its source here and compared
//      against `claimKey`. This is the only assertion in either tree that can see a
//      one-sided rename.
//   1. THE KEY NAMES THE RESOURCE. Not the suite, not the binary, not the pid. The
//      first version of the Rust claim was keyed by the SUITE, which is how a
//      neighbour's installation came to be reported as a defect.
//   2. IT ACTUALLY EXCLUDES. While one connection holds the claim a second cannot take
//      it, and once the first releases, the second can. The contender uses a raw
//      `pg_try_advisory_lock` rather than `claim`, because a WAIT and a REFUSAL are
//      indistinguishable from a test that only ever waits: the try answers `false` at
//      once and that false is the observable.
//   3. LOSING IS LOUD. A claim that cannot be obtained inside its bound THROWS, naming
//      the extension and the key. It is not a skip and it does not hang, which asserts
//      the bound is a real `lock_timeout` rather than decoration.
//
// WHY THE LIVE HALVES LOCK A NAME THAT IS NOT AN EXTENSION. The key is a string the
// server hashes; nothing about `pg_advisory_lock` requires the name to resolve to an
// installed extension, and `DROP EXTENSION IF EXISTS` on an unknown name is a notice.
// Locking the real `citext` here would have this file contend with the two suites that
// claim it for real, where the wedge in (3) - a `pg_try_advisory_lock` that MUST
// succeed for the test to be about anything - would answer `false` whenever one of
// them happened to hold it, and the guard would go red for a scheduling accident. The
// probe carries this process's pid so two concurrent runs of this suite do not wedge
// each other. The shared-key property those probes give up is what (0) and (1) assert
// directly, on the real names.
//
// GATE: `connectLivePg` (see `live-db.ts`) for (2) and (3); (0) and (1) need no server.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import type { Client } from "pg";

import { CLAIM_WAIT, claim, claimKey, release } from "./extension-claim.js";
import { connectLivePg } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));

/** The Rust half of the claim, whose key this file is pinned against. */
const RUST_CLAIM = resolve(
  HERE,
  "../../../../crates/zero-migrate/tests/support/extension_claim.rs",
);

/** A lock name of this run's own, distinct per case. See the header. */
function probe(tag: string): string {
  return `zm_host_claim_probe_${tag}_${process.pid}`;
}

async function tryTake(client: Client, name: string): Promise<boolean> {
  const { rows } = await client.query<{ got: boolean }>(
    `SELECT pg_try_advisory_lock(hashtext($1)::bigint) AS got`,
    [claimKey(name)],
  );
  return rows[0].got;
}

/**
 * `claim_key`'s key as the RUST source spells it, with `extension` substituted.
 *
 * Reads the source rather than a copy of it, because a copy is the thing under test.
 * A source that cannot be parsed is a CANNOT-ANSWER and fails here: a regex that
 * silently stopped matching would turn this guard into an unconditional pass, which is
 * the exact shape of failure the claim itself exists to prevent.
 */
function rustClaimKey(extension: string): string {
  const source = readFileSync(RUST_CLAIM, "utf8");
  const fn = /pub fn claim_key\(extension: &str\) -> String \{\s*format!\("([^"]*)"\)\s*\}/.exec(
    source,
  );
  assert.ok(
    fn,
    `could not find claim_key's format literal in ${RUST_CLAIM}. This test cannot ` +
      `compare the two halves' keys without it, and a silent pass here would let the ` +
      `two suites hold different locks while both reported green - so the unreadable ` +
      `source is the failure.`,
  );
  const template = fn[1];
  assert.ok(
    template.includes("{extension}"),
    `claim_key's literal ${JSON.stringify(template)} no longer interpolates the ` +
      `extension name, so the Rust claim is keyed by something this test cannot model`,
  );
  return template.replaceAll("{extension}", extension);
}

test("the host claim key is the key the Rust claim hashes", () => {
  for (const extension of ["citext", "pgcrypto", "unaccent", "uuid-ossp"]) {
    assert.equal(
      claimKey(extension),
      rustClaimKey(extension),
      `the host suite and the Rust suites must hand the server the SAME string for ` +
        `${extension}, or each holds a claim the other cannot see and both install ` +
        `the extension anyway - which is "already installed in this database" in one ` +
        `process and "does not exist" in the other, the two failures this claim was ` +
        `written for`,
    );
  }
});

test("the extension claim key names the extension and nothing else", () => {
  assert.equal(
    claimKey("citext"),
    "zero-migrate:pg-extension:citext",
    "the key is the contract BETWEEN BINARIES, not a private detail: changing its " +
      "shape unshares the claim, and an unshared claim is indistinguishable from no " +
      "claim until two suites meet on one server",
  );
  assert.notEqual(
    claimKey("citext"),
    claimKey("pgcrypto"),
    "two extensions are two resources; one key for both would serialize cases that " +
      "never contend",
  );
  for (const key of [claimKey("citext"), claimKey("pgcrypto"), claimKey("unaccent")]) {
    for (const claimant of ["rollback", "prerequisite", "host", "cli", "live", "test"]) {
      assert.ok(
        !key.includes(claimant),
        `the key must name the RESOURCE, not the claimant: ${key} contains ${claimant}, ` +
          `which would give each suite a private lock and protect nothing`,
      );
    }
    assert.ok(
      !key.includes(String(process.pid)),
      `a per-process key would make every run its own sole claimant: ${key}`,
    );
  }
});

test("a held extension claim excludes a second run and is released for the next", async () => {
  const name = probe("exclusion");
  const holder = await connectLivePg();
  const contender = await connectLivePg();

  try {
    await claim(holder, name);

    assert.equal(
      await tryTake(contender, name),
      false,
      `a second run must NOT be able to take the ${name} claim while a first holds it. ` +
        `Without this exclusion both runs install the extension and one drops it under ` +
        `the other.`,
    );

    await release(holder, name);

    // No `assert.doesNotReject` wrapper: a throw here IS the failure, and the message
    // the claim carries is more useful than any this test could add.
    await claim(contender, name);
    await release(contender, name);
  } finally {
    await holder.end().catch(() => {});
    await contender.end().catch(() => {});
  }
});

test("an unobtainable extension claim throws an error that names the key", async () => {
  const name = probe("timeout");
  const wedged = await connectLivePg();
  const loser = await connectLivePg();

  try {
    // Wedge the key from a connection that will not give it back until this test is
    // done with it. Taken with the raw verb rather than `claim`, so nothing here
    // depends on the function under test.
    assert.equal(
      await tryTake(wedged, name),
      true,
      "the wedge itself has to succeed, or the rest of this test is vacuous",
    );

    const refusal = await claim(loser, name, "300ms").then(
      () => undefined,
      (e: unknown) => e as Error,
    );
    assert.ok(refusal, "a claim on a wedged key must not be obtained");
    assert.ok(
      refusal.message.includes(name) && refusal.message.includes(claimKey(name)),
      `the failure has to name the extension and the key an operator would look for, ` +
        `got: ${refusal.message}`,
    );
    assert.ok(
      refusal.message.includes("never asked its question"),
      `a lost claim means the case did not run, which the message must SAY rather than ` +
        `let it read as a pass: ${refusal.message}`,
    );
    assert.notEqual(
      CLAIM_WAIT,
      "300ms",
      "the bound above has to be shorter than the default, or this case would be " +
        "asserting the default rather than an explicit one",
    );

    await wedged.query(`SELECT pg_advisory_unlock(hashtext($1)::bigint)`, [claimKey(name)]);

    // The loser holds nothing and has no `lock_timeout` left armed - `claim` resets it
    // on every path. Assert the first half explicitly, so a future change that left a
    // failed acquire half-held is caught here rather than as a hang in an unrelated
    // suite.
    assert.equal(
      await tryTake(loser, name),
      true,
      "once the wedge lets go the key must be obtainable; if it is not, the failed " +
        "claim left a lock behind",
    );
    await release(loser, name);
  } finally {
    await wedged.end().catch(() => {});
    await loser.end().catch(() => {});
  }
});
