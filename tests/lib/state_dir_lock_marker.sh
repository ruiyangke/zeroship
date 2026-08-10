#!/usr/bin/env bash
# Gate: the state-dir-lock marker must be spelled identically in Rust and TS.
#
# WHY THIS EXISTS. `RedbBackend::open` appends a token when the KV state dir is
# locked, and `dev-server.ts` keys its banner off that token to choose between
# two CONTRADICTORY remedies (kill the holder / change the port). The token is
# declared once per language. Edit one and both test suites still pass: the Rust
# test asserts the marker reaches a real error, the TS test asserts the banner
# keys off its own copy, and neither can see the other. This is the only thing
# that compares them.
#
# THE CHECK CANNOT RUN IS NOT THE SAME AS PASSING. If either declaration cannot
# be found - renamed, moved, reformatted - this exits 2 rather than 0. A grep
# that matches nothing is the classic way a gate goes quietly green forever.
#
# Self-test: `state_dir_lock_marker.sh --selftest` proves the comparison can
# actually fail, by running it against a mismatched pair in a temp copy.
set -uo pipefail

REPO="${REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

RUST_FILE="crates/plugin-kv/src/backend/redb.rs"
TS_FILE="sdks/vite-plugin/src/dev-server.ts"

# Pull the string literal out of each declaration.
extract_rust() {
  sed -n 's/^pub const STATE_DIR_LOCK_MARKER: &str = "\([^"]*\)".*/\1/p' "$1" | head -1
}
extract_ts() {
  sed -n 's/^const STATE_DIR_LOCK_MARKER = "\([^"]*\)".*/\1/p' "$1" | head -1
}

check_pair() {
  local rust_path="$1" ts_path="$2" label="${3:-}"
  local rust ts
  rust="$(extract_rust "$rust_path")"
  ts="$(extract_ts "$ts_path")"

  if [ -z "$rust" ]; then
    echo "[marker-gate] ${label}CANNOT RUN: no STATE_DIR_LOCK_MARKER declaration found in $rust_path" >&2
    echo "[marker-gate] the declaration was renamed or reformatted; fix this gate, do not delete it" >&2
    return 2
  fi
  if [ -z "$ts" ]; then
    echo "[marker-gate] ${label}CANNOT RUN: no STATE_DIR_LOCK_MARKER declaration found in $ts_path" >&2
    echo "[marker-gate] the declaration was renamed or reformatted; fix this gate, do not delete it" >&2
    return 2
  fi
  if [ "$rust" != "$ts" ]; then
    echo "[marker-gate] ${label}MISMATCH: rust='$rust' ts='$ts'" >&2
    echo "[marker-gate] the dev banner keys off the TS spelling, so it will now treat a" >&2
    echo "[marker-gate] state-dir lock as a port clash and advise changing devServerPort," >&2
    echo "[marker-gate] which cannot help. See task #221." >&2
    return 1
  fi
  echo "[marker-gate] ${label}ok: both declare '$rust'"
  return 0
}

if [ "${1:-}" = "--selftest" ]; then
  # One variable: an identical pair must pass, a mismatched pair must fail.
  # A self-test that only checks the failing case would not distinguish a
  # working comparison from one that rejects everything.
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  printf 'pub const STATE_DIR_LOCK_MARKER: &str = "zs-state-dir-lock";\n' > "$tmp/same.rs"
  printf 'const STATE_DIR_LOCK_MARKER = "zs-state-dir-lock";\n'          > "$tmp/same.ts"
  printf 'pub const STATE_DIR_LOCK_MARKER: &str = "zs-state-dir-lock";\n' > "$tmp/diff.rs"
  printf 'const STATE_DIR_LOCK_MARKER = "zs-state-dir-lok";\n'           > "$tmp/diff.ts"
  printf '// nothing here\n'                                             > "$tmp/absent.ts"

  fails=0
  check_pair "$tmp/same.rs" "$tmp/same.ts" "selftest[match] " >/dev/null 2>&1 \
    || { echo "SELFTEST FAIL: identical pair was rejected"; fails=1; }
  check_pair "$tmp/diff.rs" "$tmp/diff.ts" "selftest[differ] " >/dev/null 2>&1 \
    && { echo "SELFTEST FAIL: a one-character mismatch was accepted"; fails=1; }
  check_pair "$tmp/diff.rs" "$tmp/absent.ts" "selftest[absent] " >/dev/null 2>&1
  [ $? -eq 2 ] || { echo "SELFTEST FAIL: a missing declaration did not exit 2"; fails=1; }

  if [ $fails -eq 0 ]; then echo "[marker-gate] selftest ok: 3/3"; exit 0; fi
  exit 1
fi

check_pair "$REPO/$RUST_FILE" "$REPO/$TS_FILE"
