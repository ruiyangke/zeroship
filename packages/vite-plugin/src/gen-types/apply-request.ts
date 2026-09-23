/**
 * The migration service's apply-request body — built in memory, never written.
 *
 * `zeroship migrate` needs a creator's recorded migration set on the wire, and
 * the migration service must never evaluate creator TypeScript: it is Rust, it
 * holds the privileged credential, and "privilege follows the process". So the
 * recording runs HERE, in the same Node engine that records for the build, and
 * the CLI spawns this module and posts what it reads back off the pipe.
 *
 * THE RECORDING CANNOT MOVE TO RUST, and not merely because it would be
 * inconvenient. Nothing in `crates/` transpiles TypeScript — a creator's `.ts`
 * is stripped by esbuild in this toolchain and nowhere else — and the recorder
 * drains a module-level singleton in `@zeroship/migrate`, so a second
 * implementation of the authoring DSL is what a Rust recorder would have to be.
 *
 * THE BODY IS NEVER A FILE, and that is the point of recording it here. A
 * creator's migrations exist once, as the `.ts` they wrote. A second encoding
 * of them under `generated/` would be hand-editable, could go stale against
 * that source, and would have to be regenerated before every apply.
 */

import { createHash } from "node:crypto";

import { foldMigrations } from "./index.js";

/**
 * Record every `.ts` migration under `migrationsDir` into the apply-request
 * body `zeroship migrate` posts, as the exact bytes to send.
 *
 * Keeping the whole envelope (`{ kind, descriptor_sha256, documents }`) rather
 * than a bare array means the CLI never has to know the wire shape, so a change
 * to `ApplyMigrationsRequest` moves this producer and nothing else.
 */
export async function recordApplyRequest(migrationsDir: string): Promise<string> {
  const { documents, runtimeJson } = await foldMigrations(migrationsDir);
  if (documents.length === 0) {
    // Refused HERE, where the directory is known, rather than posted. The
    // service rejects a zero-document apply outright ("at least one .ir.json
    // document is required"), and a 422 naming nothing the creator can see is
    // worse than a local message naming the directory that is empty.
    throw new Error(
      `no migrations under ${migrationsDir} — an app with no migrations has none to apply`,
    );
  }
  // THE ORDERING ANCHOR. `foldMigrations` calls `genArtifacts` ONCE, so this
  // hash names the descriptor that belongs to exactly these documents.
  // `migrated` records it on the ledger row for the apply request.
  //
  // IT MUST BE THE HASH OF THE BYTES THAT REACH THE PACKER, not of some
  // re-serialisation of the same value. gen-types writes `runtimeJson` verbatim
  // with `fs.writeFile(..., "utf8")`, and `zship.ts` hashes the file it reads
  // back (`sha256Hex(descriptorBytes)`), so hashing the string here as utf8
  // yields the same digest. A pretty-print, a re-`JSON.stringify`, or a
  // trailing newline added on either side would produce two hashes that can
  // never agree, and the failure would look like the guard misfiring.
  const descriptorSha256 = createHash("sha256").update(runtimeJson, "utf8").digest("hex");
  // Two-space JSON, trailing newline. Nobody reviews these bytes, but they are
  // what a failed apply quotes back at a creator, and a stable rendering is
  // what lets the CLI's regression test pin them against a committed set as a
  // byte claim rather than a shape claim.
  return `${JSON.stringify(
    { kind: "ir", descriptor_sha256: descriptorSha256, documents },
    null,
    2,
  )}\n`;
}
