// The claim on a PostgreSQL EXTENSION that this suite's test files take in turn.
//
// WHY THIS EXISTS. `tests/host/run.ts` starts every test file in a process of its
// own, concurrently, against the run's one PostgreSQL container and its one
// database. Every other name a file creates - a schema, a role, a project id -
// carries something unique to that file's process, so two files never meet. An
// extension has no such freedom: it is installed per DATABASE, and its name is a
// lookup into the server's installed library, so `citext_<pid>` is not an isolated
// extension, it is `could not open extension control file`. Isolation in SPACE is
// unavailable, so the claimants isolate in TIME.
//
// Without it, two files that each install and drop `citext` report each other's
// work as a defect:
//
//   rollback-live.test.ts   extension "citext" is already installed in this database
//   another claimant        extension "citext" does not exist
//
// The second line is the sharper one: the referent went missing because ANOTHER FILE
// removed it, which is exactly the confusion a live suite must not manufacture.
//
// KEYED BY THE RESOURCE, NOT BY THE CLAIMANT. [`claimKey`] names the extension and
// nothing else - no file, no pid - because two locks with different keys protect
// nothing while looking exactly like protection.
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

import type { Client } from "pg";

const CLAIM_PREFIX = "zero-migrate:pg-extension:";

/**
 * How long a run waits for a claim before it REPORTS rather than hangs.
 *
 * Spelled as a `lock_timeout`, which PostgreSQL applies to a `pg_advisory_lock` wait.
 * The claimed span of any one case here is an apply and a rollback through the CLI, so
 * this bound is reached by a WEDGED holder, not by a queue.
 */
export const CLAIM_WAIT = "180s";

/**
 * The advisory-lock key for one extension, keyed by the RESOURCE alone.
 *
 * Every claimant of `citext` in this suite must hash this same string, or the claim
 * serializes a file against itself and nothing against the others.
 */
export function claimKey(extension: string): string {
  return `${CLAIM_PREFIX}${extension}`;
}

function quoted(extension: string): string {
  return `"${extension.replaceAll('"', '""')}"`;
}

/**
 * Take the claim on `extension`, and start it from a known state.
 *
 * The `DROP EXTENSION IF EXISTS` here is INSIDE the claim and is a fixture
 * precondition: a test file killed between its CREATE and its DROP leaves the
 * extension installed, and the next claimant's `CREATE EXTENSION` would then answer
 * `already installed in this database` - an answer about the leftover, not about the
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
