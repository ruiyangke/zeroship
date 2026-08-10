// No tracked source or config file may hardcode a developer's home directory.
//
// A path like `/home/<user>/Projects/...` is correct on exactly one machine. It is
// committed, it typechecks, it passes every gate that reads the file rather than
// following it, and it fails only when somebody else clones the repository - by
// which time the failure surfaces as whatever the consumer of that path does when it
// is missing, which is rarely "this path is wrong".
//
// This gate exists because the sibling project hit it twice in one day: a
// `file:/home/<user>/Projects/zero-migrate/...` dependency, and a Playwright config
// setting an env var to `/home/<user>/Projects/appbase/target/release/<bin>` with no
// existence check, so on any other checkout it spawned a binary that does not exist.
// Neither was found by a gate; both were found by someone looking. This repository is
// clean of the class today - the gate is here so it stays that way, not because it
// found something.
//
// SCOPED TO CODE AND CONFIG, NOT PROSE, and that is a decision rather than an
// allowlist. `docs/review-log.md` and `ISSUES.md` both carry `/home/<user>` paths
// legitimately: they quote commands as they were actually run, and rewriting a
// transcript to be portable would make it a worse record. The distinction is what the
// path IS in each place - in prose it is a citation, and in code or config it is a
// dependency that resolves or does not. So the scan follows extensions, and no file
// is ever exempted by name. If a `.md` ever becomes load-bearing for a path, that is
// a decision to record here, not a filename to add.
//
// DOES scan comments, not just executable lines. A commented-out absolute path is
// dead weight to every reader but its author, and the sibling project's third
// instance today was exactly that - a doc citation inside a `.rs` test. Distinguishing
// comment from code would need a parser per language, and would buy a permission
// nobody wants.
//
// DOES NOT catch a machine-specific path that is not under a home directory:
// `/opt/local/...`, `/mnt/scratch/...`, a Windows drive letter, or a path assembled at
// runtime from pieces. It pins the spelling that actually occurred twice rather than
// the concept, because the concept has no bounded spelling. That is a hole, and a
// deliberate one - a gate that tried to recognise "machine-specific" in general would
// either miss these anyway or reject legitimate absolute paths like `/usr/bin/env`.
//
// DOES NOT run in CI as its own step: it rides `pnpm --filter zero-migrate test`,
// which CI already runs, for the same reason the tracked-file byte gate does.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "../../..");

/** Extensions whose contents are executed, compiled or parsed by a tool, as opposed
 *  to read by a person. A path here is a dependency; a path in prose is a quotation. */
const SCANNED = /\.(rs|ts|tsx|mts|js|mjs|cjs|json|toml|yml|yaml|nix|sh)$/;

/** A home directory on Linux or macOS, followed by a user component. `/home/` alone
 *  would reject the string "/home/" appearing as a prefix constant; requiring the user
 *  segment is what makes it a real path rather than a mount point. The trailing slash
 *  is load-bearing for a second reason: a test asserting that a leak does NOT happen
 *  may hold a deliberately fake `"/home/leak"`, which is not a path into anyone's home. */
const HOME_PATH = /(?:\/home\/|\/Users\/)[A-Za-z0-9._-]+\//;

/** `http`/`https` only, and the exclusion of `file:` is the whole point. `home` is an
 *  ordinary path segment on the web - a URL like `apple.com/v/iphone/home/<id>/images`
 *  carries a `/home/<segment>/` run that is not a home directory - so a web URL must be
 *  removed from the line before the match. (Spelled with a placeholder here on purpose:
 *  a real example inside this comment is itself a hit, which is how this line was first
 *  written and how the gate caught its own documentation.) A `file:` URL is the opposite
 *  case - `file:` followed by `///home/<user>/...` IS a machine-specific path
 *  IS a machine-specific path, and it is the exact shape that prompted this gate, so
 *  stripping it would blind the gate to its own founding instance.
 *
 *  Order matters. Stripping runs BEFORE the match, never as a filter after it: a line
 *  carrying both a web URL and a real home path must still be reported, and a
 *  match-then-discard pass would drop it. */
const WEB_URL = /\bhttps?:\/\/\S+/g;

/** The floor exists because this gate's failure mode is a FALSE GREEN: a scan that
 *  enumerates nothing and a scan that enumerates everything and finds nothing are
 *  indistinguishable from the outside. Measured at 409 scanned files when written.
 *  Keep it close to the real count - everything above the floor is invisible, exactly
 *  as the tracked-file byte gate records for the same reason. */
const MIN_SCANNED_FILES = 350;

function trackedFiles(): string[] {
  const listing = execFileSync("git", ["ls-files", "-z"], {
    cwd: repoRoot,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  return listing.split("\0").filter((path) => path.length > 0);
}

test("no tracked source or config file hardcodes a home directory", () => {
  const scanned = trackedFiles().filter((path) => SCANNED.test(path));
  assert.ok(
    scanned.length >= MIN_SCANNED_FILES,
    `only ${scanned.length} files enumerated, below the ${MIN_SCANNED_FILES} floor - ` +
      "the listing is truncated, so a clean result would be meaningless",
  );

  const offenders: string[] = [];
  for (const relative of scanned) {
    const text = readFileSync(join(repoRoot, relative), "utf8");
    for (const [index, line] of text.split("\n").entries()) {
      const hit = HOME_PATH.exec(line.replace(WEB_URL, ""));
      if (hit) offenders.push(`${relative}:${index + 1}: ${hit[0]}`);
    }
  }

  assert.deepEqual(
    offenders,
    [],
    "a tracked source or config file hardcodes a home directory, which resolves on " +
      "exactly one machine:\n" +
      offenders.join("\n"),
  );
});
