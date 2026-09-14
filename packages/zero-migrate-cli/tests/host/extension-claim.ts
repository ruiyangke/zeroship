// The host half of the cross-run, cross-binary claim on a PostgreSQL EXTENSION.
//
// THE OTHER HALF IS `crates/zeroship-migrate/tests/support/extension_claim.rs`, and that
// file carries the reasoning: an extension is installed per DATABASE, not per schema,
// so the pid every other cluster-visible name here carries buys no isolation
// (`citext_<pid>` is not an isolated extension, it is `could not open extension
// control file`). Isolation in SPACE is unavailable, so the claimants isolate in TIME.
//
// THIS FILE EXISTS BECAUSE ONE OF THE TWO MEASURED FAILURES WAS A HOST FAILURE. Both
// were seen from separate concurrent gate runs sharing 127.0.0.1:5434:
//
//   rollback-live.test.ts:403   extension "citext" is already installed in this database
//   test 385                    extension "citext" does not exist
//
// The first line is this suite. Until this module existed the Rust binaries held a
// claim the host suite knew nothing about, which is the failure mode the Rust file
// warns about in its own words: "two locks with different keys protect nothing at all
// while looking exactly like protection". A claim only one of two contenders takes is
// the degenerate case of that - one key and no key.
//
// Both implementations load the prefix from the same data fixture. The hashing is
// the server's (`hashtext`), so the two languages share one lock space without
// parsing or copying each other's source.
//
// RELEASE ON EVERY PATH. The claim is a SESSION-level advisory lock on the caller's
// ONE `pg.Client` connection, so the server releases it when that connection closes -
// which covers an early return, a thrown assertion AND a killed process. [`release`]
// exists so the claim ends at the CASE boundary rather than whenever the client
// happens to be ended, and so the next claimant is not made to wait on a session that
// is already finished with it.
//
// A FAILED CLAIM IS LOUD. [`claim`] throws, never skips. A caller that turned a lost
// claim into a quiet pass would have a test that reports green without asking its
// question, which this project treats as a defect in itself.

import { readFileSync } from "node:fs";

import type { Client } from "pg";

const CLAIM_PREFIX = readFileSync(
  new URL("../../../../crates/zeroship-migrate/tests/fixtures/extension-claim-prefix.txt", import.meta.url),
  "utf8",
).trimEnd();

/**
 * How long a run waits for a claim before it REPORTS rather than hangs.
 *
 * Spelled as a `lock_timeout`, which PostgreSQL applies to a `pg_advisory_lock` wait -
 * verified on the 18.4 instance these suites run against. The claimed span of any one
 * case here is an apply and a rollback through the CLI, so this bound is reached by a
 * WEDGED holder, not by a queue. Same value as `extension_claim::CLAIM_WAIT`; the two
 * are independent bounds on the same wait, not a shared constant.
 */
export const CLAIM_WAIT = "180s";

/**
 * The advisory-lock key for one extension, keyed by the RESOURCE alone.
 *
 * The literal is the contract with `extension_claim::claim_key` in the Rust tree.
 * Every claimant of `citext` - in whatever binary, in whatever language - must hash
 * this same string, or the claim serializes a suite against itself and nothing against
 * its siblings.
 */
export function claimKey(extension: string): string {
  return `${CLAIM_PREFIX}${extension}`;
}

function quoted(extension: string): string {
  return `"${extension.replaceAll('"', '""')}"`;
}

/**
 * Take the cross-run claim on `extension`, and start it from a known state.
 *
 * The `DROP EXTENSION IF EXISTS` here is INSIDE the claim and is a fixture
 * precondition: a run killed between its CREATE and its DROP leaves the extension
 * installed, and the next claimant's `CREATE EXTENSION` would then answer `already
 * installed in this database` - an answer about the leftover, not about the
 * declaration under test. The identical statement OUTSIDE the claim is the race itself.
 *
 * @param client a connected client the caller owns; the claim lives on ITS session.
 * @param extension the extension name, which is also the resource the key names.
 * @param wait an explicit bound, so the timeout path itself can be tested.
 * @throws when the claim was not obtained. Every caller must let that be LOUD.
 */
export async function claim(
  client: Client,
  extension: string,
  wait: string = CLAIM_WAIT,
): Promise<void> {
  const key = claimKey(extension);
  try {
    await client.query(`SET lock_timeout = '${wait}'`);
  } catch (e) {
    throw new Error(
      `could not bound the wait for the ${extension} claim: ${(e as Error).message}`,
    );
  }

  let refused: Error | undefined;
  try {
    await client.query(`SELECT pg_advisory_lock(hashtext($1)::bigint)`, [key]);
  } catch (e) {
    refused = e as Error;
  }
  // RESET before anything else runs on this connection. The bound belongs to the
  // claim; left armed it would abort the CLI's OWN project-lock wait under load, and
  // the case would be blamed for a server error that is this function's.
  await client.query(`RESET lock_timeout`).catch(() => {});

  if (refused !== undefined) {
    throw new Error(
      `waited ${wait} for the ${extension} claim (${key}) and did not get it, so ` +
        `this case never asked its question: ${refused.message}`,
    );
  }

  try {
    await client.query(`DROP EXTENSION IF EXISTS ${quoted(extension)}`);
  } catch (e) {
    throw new Error(
      `holds the ${extension} claim but could not clear a leftover installation ` +
        `before asking the case: ${(e as Error).message}`,
    );
  }
}

/**
 * Drop what the case installed under the claim, then release the claim.
 *
 * Belongs in a `finally`, and does not need to be anything cleverer - see the header:
 * the connection closing is the backstop that covers the killed process too. Both
 * statements swallow their errors because this runs on the failure path as well, where
 * the caller's own error is the one worth reporting.
 */
export async function release(client: Client, extension: string): Promise<void> {
  await client.query(`DROP EXTENSION IF EXISTS ${quoted(extension)}`).catch(() => {});
  await client
    .query(`SELECT pg_advisory_unlock(hashtext($1)::bigint)`, [claimKey(extension)])
    .catch(() => {});
}
