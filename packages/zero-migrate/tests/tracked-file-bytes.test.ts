// No tracked file may carry a NUL byte.
//
// A NUL turns a text file binary for most of the toolchain. The local `grep` is
// ugrep, which SILENTLY reports no matches in such a file and exits 1 - no error,
// no warning - so a NUL anywhere in the tree makes every subsequent search over
// that file return a clean-looking nothing. That is how a real one got written
// here: `dialect-support.toml`'s generator built a composite key with a literal
// 0x00 separator, and searches over the generator read as empty for as long as it
// was there (fixed in 301ac74 by spelling the separator as the two-character
// escape `\0`, which keeps the collision-free key and leaves the file readable).
//
// The instrument here is a byte scan, deliberately NOT a grep. The obvious grep
// spelling of this check is not merely unable to see the byte, it INVERTS: the
// shell cannot put a NUL in an argument, so `grep -qU $'\0'` degenerates to the
// empty pattern and matches every CLEAN file, while ugrep declines to read the one
// file that actually carries the byte and reports it clean. Both answers backwards,
// with no signal that anything went wrong.
//
// Scoped to `git ls-files` rather than a directory walk: a walk reaches the
// gitignored compiled addon (`crates/zero-migrate-node/*.node`), which is a
// legitimate binary and would be the gate's first false positive. There is no
// allowlist and no extension filter, because every tracked file in this repo is
// text - if that ever stops being true, the right response is a deliberate
// decision recorded here, not a silently growing exemption list.
//
// Does NOT cover untracked or gitignored files, does NOT cover other control
// characters, and does NOT check file ENCODING (a valid UTF-8 file with unusual
// codepoints passes; that is `tsc`/`cargo fmt`'s business, not this gate's).

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "../../..");

/** The lower bound exists so a broken invocation cannot pass as a clean tree.
 *  This gate's failure mode is a FALSE GREEN: a scan that reads nothing and a
 *  scan that reads everything and finds nothing look identical from the outside.
 *
 *  THIS BOUND IS AN IRREDUCIBLE TOLERANCE, unlike the read check below. The read
 *  check is proportional because it has a denominator - what git enumerated. An
 *  enumeration check has none, so a truncated listing that is fully read cannot
 *  be distinguished from a smaller repository, and everything above this floor is
 *  invisible. Measured: with the floor at 300 against 408 tracked files, feeding
 *  the gate 301 paths that all read cleanly PASSED, leaving 107 files unscanned
 *  and the tree reported clean. Keep it close to the real count for that reason,
 *  and expect to raise it as the repository grows rather than leaving slack. */
const MIN_TRACKED_FILES = 380;

/** Tracked paths, read through git so the scope matches what CI would clone.
 *  `-z` separates with NUL, which is the byte this test is about - the separator
 *  is fine precisely because it never reaches a file's CONTENTS. */
function trackedFiles(): string[] {
  const out = execFileSync("git", ["ls-files", "-z"], {
    cwd: repoRoot,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  return out.split("\0").filter((path) => path !== "");
}

test("no tracked file contains a NUL byte", () => {
  const files = trackedFiles();
  assert.ok(
    files.length >= MIN_TRACKED_FILES,
    `git ls-files returned ${files.length} paths, fewer than the ${MIN_TRACKED_FILES} ` +
      "this gate expects to scan. Either the repository shrank dramatically or the " +
      "invocation is broken; a scan of nothing must not report a clean tree.",
  );

  const offenders: string[] = [];
  let scanned = 0;
  for (const relative of files) {
    const absolute = join(repoRoot, relative);
    // A tracked path can be absent from the working tree (a submodule gitlink, or
    // a sparse checkout). Skip what is not a readable regular file rather than
    // failing the gate on it: this test is about CONTENTS, and absent contents
    // carry no byte.
    let size: number;
    try {
      const stats = statSync(absolute);
      if (!stats.isFile()) continue;
      size = stats.size;
    } catch {
      continue;
    }
    if (size === 0) continue;

    const index = readFileSync(absolute).indexOf(0);
    scanned += 1;
    if (index !== -1) offenders.push(`${relative} (first NUL at byte ${index})`);
  }

  // The count above proves git ENUMERATED files; this proves we READ them. Without
  // it a wrong `repoRoot` makes every statSync throw, the catch skips every file,
  // and the gate reports a clean tree having opened nothing - green for a reason
  // unrelated to the property it asserts. Verified by pointing repoRoot at a
  // non-existent directory: the gate passed before this assertion existed.
  // Proportional, not a fixed floor. A floor stops scaling: at 408 tracked files a
  // floor of 300 tolerates 108 silently unread, and the gap widens with every file
  // added. Tie the bound to what git enumerated so the tolerance stays the size of
  // the exception it exists for.
  const minScanned = Math.ceil(files.length * 0.9);
  assert.ok(
    scanned >= minScanned,
    `only ${scanned} of ${files.length} tracked files were actually opened and read ` +
      `(at least ${minScanned} required). The per-file skip is meant for a submodule ` +
      "gitlink or a sparse checkout, not for a repository root that does not resolve; " +
      "a scan that reads nothing must not report a clean tree.",
  );

  assert.deepEqual(
    offenders,
    [],
    `${offenders.length} tracked file(s) carry a NUL byte, which makes every ugrep ` +
      "search over them silently return nothing:\n  " +
      offenders.join("\n  ") +
      "\nIf the byte is a deliberate separator, spell it as the escape \\0 in source " +
      "rather than embedding it (see 301ac74). If the file is genuinely binary, it " +
      "does not belong in this tree without a decision recorded in this test.",
  );
});
