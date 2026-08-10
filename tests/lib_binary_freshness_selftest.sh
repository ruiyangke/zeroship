#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Self-test for tests/lib/binary_freshness.sh.
#
# The function it checks exists to stop a stale binary being reported as a
# dev-vs-deployed divergence. A freshness check that silently never fires would
# reinstate exactly that failure while looking like protection, so this drives
# it over synthetic directories where the answer is known by construction:
# a binary deliberately older than a source file must be caught, and one
# deliberately newer must not be.
#
# Synthetic rather than the real tree on purpose. Proving the check fires by
# touching a real crate source would mutate the working tree and, worse, would
# only prove it fires in whatever state the tree happens to be in today.
#
# Run: bash tests/lib_binary_freshness_selftest.sh
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"

PASS=0; FAIL=0
ok() { PASS=$((PASS+1)); echo "  ok   $1"; }
no() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/crates/demo/src" "$WORK/bin"

# --- case 1: binary NEWER than source -> fresh, quiet, rc 0 ----------------
echo 'fn main() {}' > "$WORK/crates/demo/src/lib.rs"
touch -d '2020-01-01' "$WORK/crates/demo/src/lib.rs"
printf '#!/bin/sh\n' > "$WORK/bin/demo-bin"; chmod +x "$WORK/bin/demo-bin"
touch -d '2021-01-01' "$WORK/bin/demo-bin"

out=$(zs_check_binary_freshness "$WORK" "$WORK/bin" "crates/demo/src" "demo-bin" 2>&1); rc=$?
[ "$rc" -eq 0 ] && ok "fresh binary returns 0" || no "fresh binary returned $rc, want 0"
case "$out" in
  *WARN*) no "fresh binary produced a WARN: $out" ;;
  *"built after the newest source"*) ok "fresh binary reports the ok line" ;;
  *) no "fresh binary produced no recognisable output: $out" ;;
esac

# --- case 2: source NEWER than binary -> stale, warns, still rc 0 ----------
# THE POSITIVE CONTROL. If this does not fire the whole check is decorative.
touch -d '2022-01-01' "$WORK/crates/demo/src/lib.rs"

out=$(zs_check_binary_freshness "$WORK" "$WORK/bin" "crates/demo/src" "demo-bin" 2>&1); rc=$?
[ "$rc" -eq 0 ] && ok "stale binary still returns 0 by default (warn, not refuse)" \
  || no "stale binary returned $rc, want 0 in non-strict mode"
case "$out" in
  *"WARN demo-bin is OLDER than crates/demo"*) ok "stale binary names the BINARY and the CRATE" ;;
  *WARN*) no "warned, but without naming binary and crate: $out" ;;
  *) no "stale binary produced NO warning -- the check is decorative: $out" ;;
esac

# --- case 3: strict mode refuses -------------------------------------------
out=$(ZS_FRESHNESS_STRICT=1 zs_check_binary_freshness "$WORK" "$WORK/bin" "crates/demo/src" "demo-bin" 2>&1); rc=$?
[ "$rc" -eq 1 ] && ok "ZS_FRESHNESS_STRICT=1 refuses a stale binary (rc 1)" \
  || no "strict mode returned $rc on a stale binary, want 1"

# --- case 4: cannot-answer cases return 2, never a silent pass -------------
# Both of these would look identical to "everything is fresh" if the function
# reported success, which is the failure mode this repo has shipped twice in
# gates that passed over zero inputs.
out=$(zs_check_binary_freshness "$WORK" "$WORK/bin" "crates/does-not-exist/src" "demo-bin" 2>&1); rc=$?
[ "$rc" -eq 2 ] && ok "nonexistent source dir returns 2, not a pass" \
  || no "nonexistent source dir returned $rc, want 2"

mkdir -p "$WORK/crates/empty/src"
out=$(zs_check_binary_freshness "$WORK" "$WORK/bin" "crates/empty/src" "demo-bin" 2>&1); rc=$?
[ "$rc" -eq 2 ] && ok "source dir with no .rs/.ts returns 2, not a pass" \
  || no "empty source dir returned $rc, want 2"

out=$(zs_check_binary_freshness "$WORK" "$WORK/bin" "crates/demo/src" "no-such-bin" 2>&1); rc=$?
[ "$rc" -eq 2 ] && ok "missing binary returns 2, not a pass" \
  || no "missing binary returned $rc, want 2"

# --- case 5: one stale among several is still caught -----------------------
printf '#!/bin/sh\n' > "$WORK/bin/fresh-bin"; chmod +x "$WORK/bin/fresh-bin"
touch -d '2023-01-01' "$WORK/bin/fresh-bin"
out=$(zs_check_binary_freshness "$WORK" "$WORK/bin" "crates/demo/src" "fresh-bin demo-bin" 2>&1)
case "$out" in
  *"WARN demo-bin"*) ok "a single stale binary among fresh ones is still named" ;;
  *) no "stale binary was masked by a fresh sibling: $out" ;;
esac

# --- case 6: the verdict is PUBLISHED, not just printed --------------------
# Why this exists, from a real run (2026-08-10, tests/e2e_dev_vs_deployed_db.sh):
# the check fired correctly, named `dispatch.rs`, and warned that six binaries
# predated it. The run then reported a three-row dev-vs-deployed divergence, two
# rows of which were exactly the predicted skew. I read the diff at the BOTTOM of
# a 160-line log and never saw the warning at the top, and spent the next several
# minutes proving the skew by hand from binary mtimes.
#
# A warning is only useful where the reader is. The reader is at the divergence.
# So the function must leave its verdict in a variable the caller can re-print
# beside the diff, rather than only echoing it once at boot.
#
# Deliberately asserts on the COUNT and the NAMES separately: a caller that only
# knows "something was stale" cannot tell the reader which side to distrust.
unset ZS_FRESHNESS_STALE_COUNT ZS_FRESHNESS_STALE_NAMES
zs_check_binary_freshness "$WORK" "$WORK/bin" "crates/demo/src" "fresh-bin demo-bin" >/dev/null 2>&1
[ "${ZS_FRESHNESS_STALE_COUNT:-unset}" = "1" ] \
  && ok "the stale COUNT is published for the caller to re-report" \
  || no "ZS_FRESHNESS_STALE_COUNT is '${ZS_FRESHNESS_STALE_COUNT:-unset}', want 1"
case "${ZS_FRESHNESS_STALE_NAMES:-}" in
  *demo-bin*) ok "the stale NAMES are published, so the caller can say which side" ;;
  *) no "ZS_FRESHNESS_STALE_NAMES is '${ZS_FRESHNESS_STALE_NAMES:-unset}', want it to name demo-bin" ;;
esac

# The all-fresh case must publish ZERO, not leave a previous run's value
# standing. A stale variable here would make an honest run advertise a
# staleness that had already been fixed -- the inverse error, equally wrong.
touch "$WORK/bin/fresh-bin" "$WORK/bin/demo-bin"
zs_check_binary_freshness "$WORK" "$WORK/bin" "crates/demo/src" "fresh-bin demo-bin" >/dev/null 2>&1
[ "${ZS_FRESHNESS_STALE_COUNT:-unset}" = "0" ] \
  && ok "an all-fresh run publishes 0, and does not leak the prior count" \
  || no "after a fresh run ZS_FRESHNESS_STALE_COUNT is '${ZS_FRESHNESS_STALE_COUNT:-unset}', want 0"

echo ""
echo "  binary-freshness selftest: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
