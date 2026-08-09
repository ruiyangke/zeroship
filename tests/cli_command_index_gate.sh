#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# The crate index in AGENTS.md must name the commands the CLI actually has.
#
# AGENTS.md is the AI-agent landing page, and its crate index is a table -- the
# part a reader scans rather than reads. It drifted: the index advertised
# `build` and `inspect` long after both were removed in the artifact-layout
# redesign, while docs/feature-map.md correctly recorded them as removed and the
# CLI's own `print_usage` said outright "There is no `zeroship build`". Three
# artefacts, two right, and the wrong one was the one agents read first.
#
# Nothing caught it. The doc-citation gate resolves PATHS, not command names, so
# an index entry naming a command that does not exist resolves nothing and is
# checked by nothing.
#
# This compares two sets: the match arms in crates/cli/src/main.rs, and the
# comma-separated list on the `cli/` row of the AGENTS.md crate index. A command
# in either set and not the other fails the gate.
#
# Deliberately NOT compared against `print_usage`: that text is prose with flags
# and prose wraps, so parsing it would make the gate fragile in a way the match
# arms are not. The dispatch table is the authority on what the binary accepts.
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAIN="$ROOT/crates/cli/src/main.rs"
AGENTS="$ROOT/AGENTS.md"

fail=0
note() { echo "  $1"; }

for f in "$MAIN" "$AGENTS"; do
  [ -f "$f" ] || { echo "FAIL: missing $f"; exit 1; }
done

# Command names dispatched by the top-level match. The arms are `"name" => ...`;
# the leading-whitespace anchor keeps this from matching string literals that
# merely look like arms elsewhere in the file.
actual=$(grep -oE '^\s+"[a-z]+" =>' "$MAIN" | grep -oE '"[a-z]+"' | tr -d '"' | sort -u)

# The `cli/` row of the crate index, e.g. `└── cli/   CLI: serve, deploy, ...`.
indexed=$(grep -E '^[^|]*cli/ +CLI:' "$AGENTS" \
  | sed -E 's/.*CLI: *//' | tr ',' '\n' | sed -E 's/^ +| +$//g' | grep -E '^[a-z]+$' | sort -u)

# A gate that silently matched nothing on either side would compare two empty
# sets and pass. Both extractions must find something before any comparison is
# believed -- the same failure this repo has already shipped twice, in the
# billing and JS gates that passed over zero tests.
n_actual=$(printf '%s\n' "$actual" | grep -c . || true)
n_indexed=$(printf '%s\n' "$indexed" | grep -c . || true)
if [ "$n_actual" -lt 2 ]; then
  echo "FAIL: extracted $n_actual commands from main.rs - the parser is broken, not the docs."
  exit 1
fi
if [ "$n_indexed" -lt 2 ]; then
  echo "FAIL: extracted $n_indexed commands from the AGENTS.md crate index - the parser is broken, not the docs."
  exit 1
fi

echo "  main.rs dispatches ($n_actual): $(echo "$actual" | tr '\n' ' ')"
echo "  AGENTS.md indexes  ($n_indexed): $(echo "$indexed" | tr '\n' ' ')"

missing=$(comm -23 <(printf '%s\n' "$actual") <(printf '%s\n' "$indexed"))
phantom=$(comm -13 <(printf '%s\n' "$actual") <(printf '%s\n' "$indexed"))

if [ -n "$missing" ]; then
  fail=1
  note "FAIL: the CLI has these commands and the AGENTS.md index omits them:"
  for c in $missing; do note "        $c"; done
fi
if [ -n "$phantom" ]; then
  fail=1
  note "FAIL: the AGENTS.md index advertises these and the CLI does not dispatch them:"
  for c in $phantom; do note "        $c  (running it prints usage and exits)"; done
fi

if [ "$fail" -eq 0 ]; then
  echo "  ok: crate index and dispatch table agree on all $n_actual commands"
fi
exit "$fail"
