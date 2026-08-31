#!/usr/bin/env bash
# Self-test for tests/lib/measurement_integrity.sh.
#
# The library exists so a gate can distinguish "the thing under test is broken"
# from "the measurement could not run". That distinction is only worth anything if
# it is right in BOTH directions, so this checks both:
#
#   a full disk MUST be recognised        - else we are back to reading a wall of
#                                           fake "linking with cc failed" and
#                                           blaming the in-flight change
#   a real failure MUST NOT be           - a classifier that fired on every
#     recognised                           failure would relabel genuine breakage
#                                          as infrastructure, which is strictly
#                                          worse than the problem it replaces
#
# Run directly: tests/lib_measurement_integrity_selftest.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$ROOT/tests/lib/measurement_integrity.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
pass=0

# expect_detect <expected: yes|no> <label> <log body>
expect_detect() {
  local expected="$1" label="$2" body="$3"
  local log="$TMP/log"
  printf '%s\n' "$body" > "$log"
  if log_shows_disk_full "$log"; then
    local actual="yes"
  else
    local actual="no"
  fi
  if [ "$actual" = "$expected" ]; then
    pass=$((pass + 1))
    echo "ok   - $label (detect=$actual)"
  else
    fail=$((fail + 1))
    echo "FAIL - $label: expected detect=$expected, got detect=$actual" >&2
  fi
}

echo "=== must DETECT: the spellings a full disk actually produces ==="
# rustc / cc, the spelling that produced the fake compile-error walls.
expect_detect yes "rustc write failure" \
  'error: failed to write output: No space left on device'
# std::io through cargo, which renders errno numerically rather than by name.
expect_detect yes "os error 28" \
  'error: failed to create directory: Os { code: 28, kind: StorageFull, message: "No space left on device" }'
expect_detect yes "bare os error 28" \
  'thread panicked: failed to write bundle (os error 28)'
# PostgreSQL, which is how the billing DB died mid-WAL-redo.
expect_detect yes "postgres WAL redo" \
  'FATAL:  could not write to file "base/448846/PG_VERSION": No space left on device'
# Buried mid-log, which is where it always is. A head/tail-only check misses this.
expect_detect yes "buried in a long log" \
  "$(printf 'compiling a\ncompiling b\nNo space left on device\ncompiling c\ncompiling d\n')"

echo
echo "=== must NOT detect: real failures, the false-positive direction ==="
# The exact text a full disk CAUSES. It must not be the trigger, or every genuine
# link error gets excused as infrastructure.
expect_detect no "real link failure" \
  'error: linking with `cc` failed: exit status: 1'
expect_detect no "real compile error" \
  "$(printf 'error[E0425]: cannot find function `foo` in this scope\nerror: could not compile `zeroship-worker`\n')"
expect_detect no "real test failure" \
  'test result: FAILED. 39 passed; 1 failed; 0 ignored'
expect_detect no "empty log" ''
# Adjacent-but-different exhaustion. Out of MEMORY is not out of DISK, and
# reporting the wrong resource sends the reader to the wrong fix.
expect_detect no "out of memory is a different resource" \
  'error: rustc interrupted by SIGKILL, kernel OOM killer likely'
# A different errno that merely contains the digits. `os error 2` is ENOENT.
expect_detect no "os error 2 is not os error 28" \
  'error: opening file: No such file or directory (os error 2)'

# --- log_is_binary, both directions ---------------------------------------
#
# Same discipline as above: it must fire on a NUL, and it must NOT fire on a log
# that merely contains multibyte UTF-8. compio-postgres has a deliberate
# dollar-quote fixture with non-ASCII bytes, and valid UTF-8 does not blind grep,
# so treating it as unreadable would relabel healthy logs.
expect_binary() {
  local expected="$1" label="$2"; shift 2
  local f="$TMP/bin_$pass$fail.log"
  printf "$@" > "$f"
  if log_is_binary "$f"; then got=yes; else got=no; fi
  if [ "$got" = "$expected" ]; then
    pass=$((pass+1)); echo "  ok   log_is_binary=$got  $label"
  else
    fail=$((fail+1)); echo "  FAIL log_is_binary=$got expected=$expected  $label" >&2
  fi
}

expect_binary yes "a NUL from a wire-protocol dump" \
  'PostgreSQL specifies: p\000(md55d57ce5b45b131e\ntest result: ok. 9 passed\n'
expect_binary no  "ordinary cargo output" \
  'test result: FAILED. 900 passed; 555 failed; 0 ignored\n'
expect_binary no  "empty log" ''
expect_binary no  "multibyte UTF-8 is not binary" \
  'SELECT $\303\251$ dollar quote fixture\ntest result: ok. 1 passed\n'

echo
echo "------------------------------------------------------------------"
echo "passed=$pass failed=$fail"
if [ "$pass" -eq 0 ]; then
  echo "FAIL: the self-test asserted nothing - it cannot pass over zero cases." >&2
  exit 1
fi
[ "$fail" -eq 0 ] || exit 1
echo "measurement-integrity self-test OK"
