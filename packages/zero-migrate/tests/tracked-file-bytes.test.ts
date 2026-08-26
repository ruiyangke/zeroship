// No tracked file may carry a NUL byte.
//
// A NUL turns a text file binary for the search tools, and BOTH greps in play here
// are blinded by it - in different ways, neither of which is loud. Measured on a
// clean file and a copy with one NUL appended, exit codes captured separately:
//
//                                    LINE MODE          -c
//   interactive shell, ugrep 7.5.0   EMPTY, exit 1      EMPTY, exit 1
//   nix devShell, GNU grep 3.12      EMPTY, exit 0      "1",   exit 0
//
// (Control: the same line-mode search on the clean file prints the line, exit 0.)
//
// GNU GREP IS THE WORSE OF THE TWO IN THE MODE PEOPLE USE. It omits the matching
// line and RETURNS SUCCESS, which is byte-identical to a pattern that genuinely is
// not there - ugrep at least exits 1, so a caller checking status has a signal.
// Line mode is what a human or agent runs (`grep -rn pattern .`); the counting
// mode, where GNU grep answers correctly, is the one nobody audits with.
//
// This comment has been wrong twice today, the same way each time: first it said
// "reports no matches" when the tool prints NOTHING, then it said the devShell grep
// "counts the match happily" - true of `-c`, false of line mode, measured in one
// mode and written as a claim about the tool. Both errors were WHAT THE OUTPUT
// MEANT rather than WHAT IT PRINTED.
//
// This is not asserted in a test below, deliberately: ugrep is not on the PATH the
// test runs under, so an assertion could only pin one of the two greps while
// reading as though it pinned the behaviour. The gate itself needs no grep - it
// reads bytes - which is why it is environment-independent and this header is not.
// That is how a real NUL got written
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
// Does NOT cover untracked or gitignored files, and no other gate scans them either -
// deliberately, per the scoping decision above: the gitignored set here is build
// output (the compiled addon, `packages/*/dist` bar the one tracked bundle), which is
// legitimately binary and has nothing to be protected from.
//
// Does NOT cover other control characters, and NOTHING ELSE COVERS THEM: no gate in
// this repo looks for any byte but NUL. A hole, not a handoff - NUL earned its own
// gate because it is the byte that makes grep and the tooling around it report
// backwards, and the rest were never measured.
//
// Does NOT reject VALID UTF-8 carrying unusual codepoints - a confusable, a
// zero-width joiner, a bidi override. Those render oddly and search fine, which is
// `tsc`'s and a reviewer's business rather than this gate's. Nothing here covers
// them; that is a hole, and a deliberate one.
//
// It DOES reject invalid UTF-8, and that arm exists because the earlier version of
// this comment named the exclusion above as if it were the whole encoding story. It
// was not: an ill-formed byte sequence blinds a search exactly as a NUL does and
// carries no NUL to find. The gate would have passed such a file while claiming to
// protect searches from precisely that.
//
// The general lesson, and the reason the arm is here rather than in a ticket: a
// planted control proves the gate catches THE THING YOU PLANTED. It is silent about
// what else produces the same symptom, because you chose the input from the same
// belief that built the gate. Ask the second question separately.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const here = dirname(fileURLToPath(import.meta.url));

/** `fatal: true` is the whole point: the lenient decoder replaces every bad byte
 *  with U+FFFD and reports success, which is the same false green this gate exists
 *  to prevent. */
const STRICT_UTF8 = new TextDecoder("utf-8", { fatal: true });
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

test("no tracked file contains a NUL byte", (t) => {
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

    const bytes = readFileSync(absolute);
    scanned += 1;
    const index = bytes.indexOf(0);
    if (index !== -1) {
      offenders.push(`${relative} (first NUL at byte ${index})`);
      continue;
    }
    // Invalid UTF-8 blinds a search the same way a NUL does and carries no NUL to
    // find, so a gate that looked only for the byte would pass the file and leave
    // the search silently empty. MEASURED: a file containing 0xFF with no NUL gives
    // `indexOf(0) === -1`, and `grep -c <pattern>` over it exits 1 printing nothing
    // while the identical clean file exits 0 printing 1.
    //
    // This is the question a planted control could not have answered. Planting a NUL
    // proves the gate catches a NUL; it says nothing about what ELSE produces the
    // same symptom, and that second question is the one that found this.
    try {
      STRICT_UTF8.decode(bytes);
    } catch {
      offenders.push(`${relative} (not valid UTF-8)`);
    }
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

  // Report coverage on SUCCESS, not only inside the failure strings above. A gate
  // that reports its numbers only when it fails withholds them in the one case
  // nobody investigates: the green run. These two counts are what separate a real
  // pass from a pass produced by scanning nothing, and a reader should not have to
  // break the plumbing on purpose to see them.
  //
  // Survives the default and `spec` reporters; `--test-reporter=dot` swallows this
  // line and every other channel, so adding one to tidy CI output silently removes
  // the only evidence a green run offers. The assertions above still fail loudly
  // under any reporter - what is lost is the reader's view of a PASS.
  t.diagnostic(`scanned ${scanned} of ${files.length} tracked files, 0 carry a NUL byte`);
});

