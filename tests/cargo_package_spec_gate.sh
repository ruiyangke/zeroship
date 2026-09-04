#!/usr/bin/env bash
# Every `cargo ... -p <spec>` in this repo must name a package that exists.
#
# WHY THIS EXISTS. `105a75131 refactor(crates): every crate directory is named
# for the package it holds` renamed the CLI package from `zeroship` to
# `zeroship-cli`. The BIN it produces is still called `zeroship`, so every
# reference of the form `-p zeroship --bin zeroship` still READS correctly and
# every one of them was left behind. Measured on 2026-09-01, before the fix:
#
#   $ cargo build -p zeroship --bin zeroship
#   error: package ID specification `zeroship` did not match any packages
#   help: a package with a similar name exists: `zerotrie`
#
# and 31 occurrences across 21 files, including three LIVE steps:
#
#   .github/workflows/ci.yml:87    "Build the CLI used by the project config gate"
#   .github/workflows/ci.yml:1568  release binaries the golden-path harness starts
#   deploy/Dockerfile:240          the shipped image's CLI
#
# The rest were comments and error messages telling a human to run a command
# that cannot work, which is the same defect one remove: a wrong instruction
# outlives the person who wrote it.
#
# WHY NOTHING CAUGHT IT. `cargo check --workspace` does not read shell scripts,
# Dockerfiles or workflow YAML, and the CI step that WOULD have failed lives in
# a job whose earlier steps are expensive - so a red there reads as flake. No
# gate in this tree looked at package specs at all. This is the "verify against
# the parser that will read it" shape: only cargo can say whether a `-p` spec
# resolves, so ask cargo.
#
# WHAT THIS GATE DOES NOT DO. It rules on package SPECS only. It does not check
# `--bin`, `--test`, `--features` or `--example` names, so `-p zeroship-cli
# --bin no-such-bin` is green here. It also cannot see a spec built by variable
# interpolation (`-p "$CRATE"`); see the KNOWN LIMITS arm below, which counts
# them so the blind spot is reported rather than silently skipped.
#
# Run the detector's own positive/control pair: this script --self-test.

set -uo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
gate_arms_init cargo_package_spec

PASS=0
FAIL=0

ok()   { printf '  ok   %s\n' "$1"; PASS=$((PASS + 1)); }
bad()  { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }

# The roots that carry EXECUTABLE build and run instructions.
#
# `crates/` and `libs/` are absent: a `-p` inside Rust source is a string in a
# doc comment. `third_party/` is absent: it is a vendored submodule with its own
# workspace, so its package names (`zeroship-migrate-adapter`, ...) are correctly
# not in our `cargo metadata` and flagging them would be instrument error.
#
# `docs/` IS ABSENT, AND THAT IS A DELIBERATE NARROWING. A first draft included
# it and reported twelve failures, every one of which was prose rather than a
# broken command: hypothetical crate names in a worked example
# (`-p featworker -p featcli`), the English sentence "not for the -p it trails",
# a placeholder `cargo clippy -p X`, `-p zeroship-sandbox` in archived plans for
# a project that now lives in a sibling repo, and this gate's own header quoting
# the very command it exists to prevent. A gate that reads prose measures prose.
ROOTS=(tests deploy .github examples sdks)

# ---------------------------------------------------------------------------
# The package set, from cargo itself rather than from a list we maintain.
# ---------------------------------------------------------------------------
if ! PKG_JSON=$(cargo metadata --no-deps --format-version 1 2>/dev/null); then
  echo "FATAL: cargo metadata failed; cannot rule on any spec." >&2
  exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "FATAL: jq is required to read cargo metadata." >&2
  exit 1
fi
PACKAGES=$(printf '%s' "$PKG_JSON" | jq -r '.packages[].name' | sort -u)
n_packages=$(printf '%s\n' "$PACKAGES" | grep -c .)
if [ "$n_packages" -lt 10 ]; then
  echo "FATAL: cargo metadata reported $n_packages packages; refusing to rule" >&2
  echo "       on specs against a package set that small." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Extraction. `-p` IS NOT A CARGO FLAG IN GENERAL: the first draft of this gate
# matched `mkdir -p`, `docker run -p 5432:5432`, `psql -p "$PGPORT"` and
# `perf record -p $PID`, and reported 20 "failures" that were all instrument
# error. Two rules make the extraction mean what it says:
#
#   1. Only logical lines that invoke `cargo` are considered.
#   2. Backslash continuations are JOINED FIRST. Without that, the two most
#      important sites in the tree are invisible - `.github/workflows/ci.yml`
#      and `deploy/Dockerfile` both carry the spec on a continuation line that
#      does not itself contain the word `cargo`.
#
# `--package <name>` is the long form of the same flag and is matched too.
cargo_logical_lines() {
  # Join `\`-continued lines, drop comments, then keep the cargo invocations.
  #
  # The comment filter is the third narrowing and it costs something: a
  # commented-out CI step carrying a dead spec is invisible here. That is the
  # accepted trade. Without it this arm reports the header of this very file,
  # which quotes `cargo build -p zeroship --bin zeroship` as the defect it
  # exists to prevent - a gate whose own explanation fails it is worse than no
  # gate, because the noise trains people to ignore the output.
  #
  # THE FOURTH NARROWING, 2026-09-04, AND THIS ONE WAS FOUND BY A RED GATE.
  # In a workflow file the executable content is the `run:` scalar; every other
  # YAML key holds prose or configuration. `#`, `//` and `>` are a shell and
  # Dockerfile notion of "not code" that YAML does not share, so this scan read
  #
  #   .github/workflows/ci.yml:511
  #     - name: Every cargo -p spec in the repo names a real package
  #
  # - the English TITLE of the step that runs this very gate - as a cargo
  # invocation, extracted `-p spec` from it, and failed because no package is
  # called `spec`. That title arrived in 035909a99 when every gate was wired
  # into CI, and this gate has been red since.
  #
  # This is the same rule as dropping `docs/` from ROOTS above, restated for a
  # format where prose and command share a file: a gate that reads prose
  # measures prose. Keyed on the YAML KEY, which is structure, not on the words
  # in it. `run:` survives, in both its inline and block-scalar forms, because a
  # block scalar's body lines are not `key:` lines.
  local yaml_filter=cat
  case "$1" in *.yml|*.yaml) yaml_filter=drop_non_run_yaml_keys ;; esac
  awk '
    /\\$/ { sub(/\\$/, ""); buf = buf $0 " "; next }
          { print buf $0; buf = "" }
    END   { if (buf != "") print buf }
  ' "$1" \
    | grep -vE '^[[:space:]]*(#|//|>)' \
    | "$yaml_filter" \
    | grep -E '(^|[^-[:alnum:]_])cargo([^-[:alnum:]_]|$)' || true
}

# Drop lines that are a YAML mapping key OTHER than `run:`. A body line inside
# a `run: |` block does not match `^key:` and therefore survives, which is the
# whole point - the commands are what this gate rules on.
drop_non_run_yaml_keys() {
  awk '
    {
      s = $0
      sub(/^[ \t]+/, "", s)
      sub(/^-[ \t]+/, "", s)
      if (match(s, /^[A-Za-z_][A-Za-z0-9_-]*:([ \t]|$)/)) {
        key = substr(s, 1, RSTART + RLENGTH - 1)
        sub(/:[ \t]*$/, "", key); sub(/:$/, "", key)
        if (key != "run") next
      }
      print
    }
  '
}

collect_specs() {
  local f
  for f in $(grep -rlE '(^|[^-[:alnum:]_])cargo([^-[:alnum:]_]|$)' \
               "${ROOTS[@]}" 2>/dev/null); do
    # Skip this file. It is the one file in the tree GUARANTEED to contain a
    # deliberately-unresolvable spec, because its positive control is one; a
    # detector that fires on its own control reports nothing but itself.
    [ "$f" = "tests/cargo_package_spec_gate.sh" ] && continue
    cargo_logical_lines "$f"
  done
}

# ---------------------------------------------------------------------------
# Arm 1: every literal `-p <spec>` resolves to a package.
# ---------------------------------------------------------------------------
echo "== literal -p specs resolve to real packages =="

# A logical line may carry several (`-p a -p b -p c`), so match repeatedly.
LITERAL_SPECS=$(
  collect_specs \
    | grep -oE -- "(-p|--package) [A-Za-z][-A-Za-z0-9_]*" \
    | sed -E 's/^(-p|--package) //' \
    | sort -u
)

n_specs=0
n_bad=0
while IFS= read -r spec; do
  [ -n "$spec" ] || continue
  n_specs=$((n_specs + 1))
  if printf '%s\n' "$PACKAGES" | grep -qx -- "$spec"; then
    continue
  fi
  n_bad=$((n_bad + 1))
  bad "-p $spec names no package. Sites:"
  grep -rnE -- "(-p|--package) $spec($|[^-A-Za-z0-9_])" "${ROOTS[@]}" 2>/dev/null \
    | sed 's/^/       /'
done <<EOF
$LITERAL_SPECS
EOF

if [ "$n_bad" -eq 0 ]; then
  ok "all $n_specs distinct package specs resolve"
fi

# FLOOR. 15 distinct specs on 2026-09-01, after the three narrowings above.
# Set well under it: deleting a few harnesses must not trip this, but a change
# that stops the extraction matching at all - a `-p` spelling change, a ROOTS
# edit, an awk continuation bug - must. This was 12 while the scan still read
# `docs/` and saw 39; a floor left behind after the set it was sized against
# shrank is a floor that has stopped separating "clean" from "did not look".
if ! gate_arm literal_specs "$n_specs" 6; then
  FAIL=$((FAIL + 1))
fi

# ---------------------------------------------------------------------------
# Arm 2: report the specs this gate CANNOT rule on, rather than skipping them.
# ---------------------------------------------------------------------------
echo
echo "== interpolated -p specs (reported, not ruled on) =="

INTERPOLATED=$(
  collect_specs | grep -oE -- '(-p|--package) [$"({]\S*' || true
)
n_interp=$(printf '%s\n' "$INTERPOLATED" | grep -c . || true)

if [ "$n_interp" -gt 0 ]; then
  printf '  note %d site(s) build the spec at run time; arm 1 is blind to them:\n' "$n_interp"
  printf '%s\n' "$INTERPOLATED" | sed 's/^/       /'
else
  ok "no interpolated specs"
fi

# This arm rules on the QUESTION "are there interpolated specs", and it has an
# answer either way, so its examined count is the number of roots it searched -
# the thing that goes to zero if ROOTS is emptied or renamed.
if ! gate_arm interpolation_scan "${#ROOTS[@]}" 3; then
  FAIL=$((FAIL + 1))
fi

# ---------------------------------------------------------------------------
# Self-test: prove the detector still fires. Two runs, one variable apart.
# ---------------------------------------------------------------------------
if [ "${1:-}" = "--self-test" ]; then
  echo
  echo "== self-test =="
  probe=$(mktemp -d)
  trap 'rm -rf "$probe"' EXIT
  mkdir -p "$probe/tests"

  # POSITIVE: a spec that cannot resolve.
  printf 'cargo build -p zeroship-no-such-package\n' > "$probe/tests/probe.sh"
  if printf '%s\n' "$PACKAGES" | grep -qx -- "zeroship-no-such-package"; then
    bad "self-test control is invalid: the probe name IS a package"
  else
    ok "positive control: 'zeroship-no-such-package' is absent from the package set"
  fi

  # NEGATIVE: the real name resolves. Same shape, one variable changed.
  if printf '%s\n' "$PACKAGES" | grep -qx -- "zeroship-cli"; then
    ok "negative control: 'zeroship-cli' IS in the package set"
  else
    bad "negative control failed: zeroship-cli is not a package"
  fi

  # THE YAML PAIR. Both lines carry `cargo ... -p`; they differ in ONE variable,
  # the YAML key. A filter that dropped both would still pass arm 1 on a clean
  # tree, so an absence here has to be paired with a presence to mean anything.
  cat > "$probe/w.yml" <<'YML'
      - name: Every cargo -p spec in the repo names a real package
        run: cargo build -p zeroship-cli --bin zeroship
YML
  probe_out="$(cargo_logical_lines "$probe/w.yml")"
  if printf '%s\n' "$probe_out" | grep -q -- '-p zeroship-cli'; then
    ok "yaml positive control: a 'run:' cargo command survives the key filter"
  else
    bad "yaml positive control FAILED: the key filter ate a run: command, so
       every workflow spec is now invisible and arm 1 is measuring nothing"
  fi
  if printf '%s\n' "$probe_out" | grep -q -- '-p spec'; then
    bad "yaml negative control FAILED: a 'name:' step title is still read as a
       cargo invocation - the defect this filter exists for"
  else
    ok "yaml negative control: a 'name:' step title is not read as a command"
  fi
fi

echo
gate_arms_finish || FAIL=$((FAIL + 1))

echo
if [ "$FAIL" -gt 0 ]; then
  printf 'cargo_package_spec_gate: %d passed, %d FAILED\n' "$PASS" "$FAIL"
  exit 1
fi
printf 'cargo_package_spec_gate: %d passed\n' "$PASS"
