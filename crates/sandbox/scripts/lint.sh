#!/usr/bin/env bash
#
# lint.sh — shellcheck regression gate for sandbox shell scripts.
#
# Backlog item R4-T1 (docs/reviews/sandbox-snapshot-restore-deferred.md):
# the test-coverage-r4 review observed that `shellcheck --severity=error`
# would have caught B12/B13/B17-class wrapper-bash regressions pre-cluster
# instead of at $0.50/discovery via real-env smoke. This script encodes
# that gate.
#
# What it does:
#   * Iterates every `*.sh` sibling in this directory (excluding lint.sh
#     itself) and runs `shellcheck --severity=error` on each.
#   * Exits 0 if every script is clean at the error severity.
#   * Exits 1 with a non-zero-aggregate count if any script flags.
#   * Exits 127 (POSIX "command not found") if shellcheck is absent so
#     callers (e.g. the `scripts_lint.rs` integration test) can map the
#     condition to `#[ignore]` rather than a spurious failure.
#
# Severity choice: `--severity=error` is the cheap-first-step (sister of
# R3-T3 full coverage). Errors are real bugs — unquoted globs that will
# expand wrong, `[[` syntax errors, dead branches. Info/style/warning
# levels are deferred until the codebase is ready to enforce them
# wholesale; raising the bar later is a one-line change here.
#
# Usage:
#   crates/sandbox/scripts/lint.sh                      # gate
#   crates/sandbox/scripts/lint.sh --severity=warning   # opt-in stricter

set -euo pipefail

SEVERITY="${1:-error}"
# Allow either positional `error` or full `--severity=foo`.
if [[ "$SEVERITY" == --severity=* ]]; then
  SEVERITY="${SEVERITY#--severity=}"
fi

if ! command -v shellcheck >/dev/null 2>&1; then
  echo "lint.sh: shellcheck not found in PATH" >&2
  echo "lint.sh: install via 'apt-get install shellcheck' or 'brew install shellcheck'" >&2
  exit 127
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELF="$(basename "${BASH_SOURCE[0]}")"

fail=0
checked=0
for script in "$SCRIPT_DIR"/*.sh; do
  name="$(basename "$script")"
  if [[ "$name" == "$SELF" ]]; then
    continue
  fi
  checked=$((checked + 1))
  if ! shellcheck --severity="$SEVERITY" "$script"; then
    fail=$((fail + 1))
  fi
done

if (( fail > 0 )); then
  echo "lint.sh: ${fail} script(s) failed shellcheck --severity=${SEVERITY} (checked ${checked})" >&2
  exit 1
fi

echo "lint.sh: OK — ${checked} script(s) clean at --severity=${SEVERITY}"
