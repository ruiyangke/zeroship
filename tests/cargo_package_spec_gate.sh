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
# WHAT THIS GATE DOES NOT DO. It rules on package specs and `--bin` TARGET names.
# It does not check `--test`, `--features` or `--example` names. It also cannot
# see a spec built by variable interpolation (`-p "$CRATE"`); see the KNOWN
# LIMITS arm below, which counts them so the blind spot is reported rather than
# silently skipped.
#
# `--bin` WAS A STATED LIMIT UNTIL 2026-09-04 ("`-p zeroship-cli --bin
# no-such-bin` is green here"), and closing it was not cosmetic. The five doc
# sites that ran `-p zeroship-migrate-adapter --bin zeroship-platform-migrate`
# were broken TWICE OVER: both the package and the binary were deleted together
# on 2026-08-28. Checking only the package would have left the second half green
# after the first was repaired, which is the shape where a fix reads as complete
# and the command still does not run.
#
# Run the detector's own positive/control pair: this script --self-test.

set -uo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
# shellcheck source=tests/lib/live_docs.sh
. "$(dirname "$0")/lib/live_docs.sh"
gate_arms_init cargo_package_spec

PASS=0
FAIL=0

ok()   { printf '  ok   %s\n' "$1"; PASS=$((PASS + 1)); }
bad()  { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }

# The roots that carry EXECUTABLE build and run instructions.
#
# `crates/` and `libs/` are absent: a `-p` inside Rust source is a string in a
# doc comment.
#
# THIS COMMENT ALSO EXCLUDED `third_party/` UNTIL 2026-09-04, explaining that it
# was "a vendored submodule with its own workspace, so its package names
# (`zeroship-migrate-adapter`, ...) are correctly not in our `cargo metadata`".
# THERE IS NO `third_party/` DIRECTORY AND NO `.gitmodules` IN THIS REPOSITORY.
# The gate whose job is to catch a dead package spec was explaining its own scope
# by pointing at a vendored copy of the dead package - so a reader consulting it
# would conclude `zeroship-migrate-adapter` lives somewhere legitimate, which is
# exactly the wrong conclusion and exactly what this gate exists to prevent.
#
# `docs/` IS ABSENT AS A ROOT, AND THAT IS A DELIBERATE NARROWING. A first draft
# included it and reported twelve failures, every one of which was prose rather
# than a broken command: hypothetical crate names in a worked example
# (`-p featworker -p featcli`), the English sentence "not for the -p it trails",
# a placeholder `cargo clippy -p X`, `-p zeroship-sandbox` in archived plans for
# a project that now lives in a sibling repo, and this gate's own header quoting
# the very command it exists to prevent. A gate that reads prose measures prose.
#
# THAT NARROWING WAS RIGHT AS A WHOLE-FILE DECISION AND WRONG AS A PERMANENT ONE,
# because it also excluded the EXECUTABLE content documents carry. Arm 3 below
# recovers exactly that, by the same structural trick this gate already invented
# for YAML: key on structure, not on the words. In markdown the executable
# content is inside a fenced code block. All twelve prose false positives above
# are outside fences; every broken command found was inside one.
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

# The `--bin` target names, from the same metadata already in hand. Same
# sanity refusal as above and for the same reason: an empty or tiny set would
# make every `--bin` look broken, which is instrument error reported as rot.
BINS=$(printf '%s' "$PKG_JSON" \
  | jq -r '.packages[].targets[] | select(.kind[] == "bin") | .name' | sort -u)
n_bins=$(printf '%s\n' "$BINS" | grep -c .)
if [ "$n_bins" -lt 5 ]; then
  echo "FATAL: cargo metadata reported $n_bins bin targets; refusing to rule" >&2
  echo "       on --bin names against a target set that small." >&2
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
# THE MARKDOWN NARROWING, and it is the exact analogue of the YAML one above.
#
# In a workflow the executable content is the `run:` scalar. In a document it is
# a FENCED CODE BLOCK. Everything outside a fence is prose - which is what made
# `docs/` unusable as a plain ROOT (twelve prose false positives, recorded
# above). Keying on the fence recovers the commands without the prose: measured
# 2026-09-04 over the 70 live documents, ZERO of those twelve survive the filter
# and every broken command found is inside a fence.
#
# THE ESCAPE HATCH IS THE SAME WORD BOTH GATES ALREADY USE. A document may
# legitimately show a command that no longer works, in a fence, while explaining
# that it does not - this gate's own header does precisely that in a shell
# context and escapes only by skipping its own file, which a document cannot do.
# So a logical line saying DELETED is skipped. One convention, two gates.
#
# THE KNOWN WEAKNESS, stated rather than discovered later: this inherits
# markdown's fence ambiguity. An unclosed fence, or an indented fence inside a
# list, flips the in-block state for the rest of the file - silently emptying the
# arm, or silently admitting prose. The floor notices only the emptying
# direction. ``` is a weaker structural signal than a YAML mapping key, and
# there is no stronger one available in markdown.
fenced_only() {
  awk '/^[[:space:]]*```/ { inblk = !inblk; next } inblk { print }' "$1"
}

# Fenced content of one document, run through the SAME cargo-line extraction the
# ROOTS use - continuation join, comment filter and the `cargo` word test. That
# reuse is the point: a second extractor would drift from the first, and the
# counts either arm reports would stop being comparable.
doc_cargo_lines() {
  local tmp
  tmp="$(mktemp "${TMPDIR:-/tmp}/zsdocfence.XXXXXX")" || return 0
  fenced_only "$1" > "$tmp"
  cargo_logical_lines "$tmp" | grep -v 'DELETED' || true
  rm -f "$tmp"
}

collect_doc_selectors() {
  local d
  while IFS= read -r d; do
    [ -n "$d" ] || continue
    doc_cargo_lines "$d"
  done < <(live_docs)
}

# ---------------------------------------------------------------------------
# Arm 1: every literal `-p <spec>` resolves to a package.
# ---------------------------------------------------------------------------
echo "== literal cargo selectors resolve to real packages and bin targets =="

ROOT_LINES=$(collect_specs)

# A logical line may carry several (`-p a -p b -p c`), so match repeatedly.
LITERAL_SPECS=$(
  printf '%s\n' "$ROOT_LINES" \
    | grep -oE -- "(-p|--package) [A-Za-z][-A-Za-z0-9_]*" \
    | sed -E 's/^(-p|--package) //' \
    | sort -u
)
LITERAL_BINS=$(
  printf '%s\n' "$ROOT_LINES" \
    | grep -oE -- "--bin [A-Za-z][-A-Za-z0-9_]*" \
    | sed -E 's/^--bin //' \
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

# The `--bin` half. It is checked HERE as well as in arm 3 because the doc arm's
# bin population is EMPTY today - the only fenced `--bin` names in live docs were
# the five dead `zeroship-platform-migrate` invocations this change repaired.
# A resolution rule whose only population is zero is a rule nothing binds, so the
# ROOTS give it a real one: 3 distinct bin names that must keep resolving.
n_root_bins=0
while IFS= read -r b; do
  [ -n "$b" ] || continue
  n_root_bins=$((n_root_bins + 1))
  n_specs=$((n_specs + 1))
  if printf '%s\n' "$BINS" | grep -qx -- "$b"; then
    continue
  fi
  n_bad=$((n_bad + 1))
  bad "--bin $b names no bin target. Sites:"
  grep -rnE -- "--bin $b($|[^-A-Za-z0-9_])" "${ROOTS[@]}" 2>/dev/null \
    | sed 's/^/       /'
done <<EOF
$LITERAL_BINS
EOF

if [ "$n_bad" -eq 0 ]; then
  ok "all $n_specs distinct cargo selectors resolve ($n_root_bins of them --bin)"
fi

# FLOOR. 15 distinct package specs on 2026-09-01, after the three narrowings
# above; 18 from 2026-09-04, when the 3 distinct `--bin` names joined the same
# count. Set well under it: deleting a few harnesses must not trip this, but a
# change that stops the extraction matching at all - a `-p` spelling change, a
# ROOTS edit, an awk continuation bug - must. This was 12 while the scan still
# read `docs/` and saw 39; a floor left behind after the set it was sized against
# shrank is a floor that has stopped separating "clean" from "did not look".
#
# The floor stays 6 rather than rising with the +3: it is sized against the
# COLLAPSE it must catch, not against the current total. 6 is already the value
# that separates "a few harnesses were deleted" from "the extractor stopped
# matching", and adding three items does not move that boundary.
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
# Arm 3: the cargo selectors in LIVE DOCUMENTS' fenced command blocks.
#
# WHAT THIS CAUGHT, and it is why the arm exists. Five user-facing runbooks and
# references told the reader to run
#     cargo build --release -p zeroship-migrate-adapter \
#       --features platform-cli --bin zeroship-platform-migrate
# - docs/reference/auth.md, docs/runbooks/auth-deploy.md, and the three stripe
# e2e runbooks. That crate and that binary were deleted together on 2026-08-28.
# In auth-deploy.md it was STEP 1 OF FIRST BOOT, so an operator following the
# documented procedure got a build error and never ran a migration. Five sites,
# ten broken selectors, invisible to every gate in the tree: this one excluded
# `docs/` outright, and the citation gates check PATHS, and `-p <name>` is a
# package name rather than a path.
# ---------------------------------------------------------------------------
echo
echo "== cargo selectors in live docs' fenced command blocks =="

DOC_LINES=$(collect_doc_selectors)

DOC_SPECS=$(
  printf '%s\n' "$DOC_LINES" \
    | grep -oE -- "(-p|--package) [A-Za-z][-A-Za-z0-9_]*" \
    | sed -E 's/^(-p|--package) //' | sort -u
)
DOC_BINS=$(
  printf '%s\n' "$DOC_LINES" \
    | grep -oE -- "--bin [A-Za-z][-A-Za-z0-9_]*" \
    | sed -E 's/^--bin //' | sort -u
)

n_doc=0
n_doc_bad=0
while IFS= read -r spec; do
  [ -n "$spec" ] || continue
  n_doc=$((n_doc + 1))
  printf '%s\n' "$PACKAGES" | grep -qx -- "$spec" && continue
  n_doc_bad=$((n_doc_bad + 1))
  bad "-p $spec in a doc names no package. Sites:"
  grep -rnE -- "(-p|--package) $spec($|[^-A-Za-z0-9_])" $(live_docs) 2>/dev/null \
    | sed 's/^/       /'
done <<EOF
$DOC_SPECS
EOF

while IFS= read -r b; do
  [ -n "$b" ] || continue
  n_doc=$((n_doc + 1))
  printf '%s\n' "$BINS" | grep -qx -- "$b" && continue
  n_doc_bad=$((n_doc_bad + 1))
  bad "--bin $b in a doc names no bin target. Sites:"
  grep -rnE -- "--bin $b($|[^-A-Za-z0-9_])" $(live_docs) 2>/dev/null \
    | sed 's/^/       /'
done <<EOF
$DOC_BINS
EOF

if [ "$n_doc_bad" -eq 0 ]; then
  ok "all $n_doc distinct doc cargo selectors resolve"
else
  FAIL=$((FAIL + n_doc_bad))
fi

# FLOOR. Measured 2026-09-04, AFTER the five broken sites were repaired: 11
# distinct package specs and ZERO `--bin` names across 70 live documents.
#
# THE ZERO IS REPORTED RATHER THAN HIDDEN. The only fenced `--bin` names live
# documents carried were the five dead `zeroship-platform-migrate` invocations
# this change repaired, so the doc arm's bin population is now empty and the bin
# rule is bound in arm 1 instead, over ROOTS, where 3 names resolve. An earlier
# draft of this comment claimed "11 + 1 bin"; that came from a scratch
# measurement that omitted this gate's own `cargo`-word filter and so counted a
# bare `zeroship-bench-server --bin ...` invocation as a cargo command.
#
# 5 is set to survive ordinary editing and to catch a collapse. The population is
# a UNION over 70 documents, so deleting any one runbook moves it by one or two;
# what takes it under 5 is the extraction ceasing to work - the fence filter
# inverting, live_docs() returning nothing, or the `cargo` word test changing.
# Deliberately NOT set near 12: a floor a one-step collapse can clear is not a
# floor, and this tree found exactly that defect today in another gate, where a
# floor of 25 against a population of 77 was cleared by a collapse to 27.
if ! gate_arm doc_cargo_selectors "$n_doc" 5; then
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

  # THE MARKDOWN PAIR, the direct analogue. Both lines carry a cargo `-p`; they
  # differ in ONE variable, whether they are inside a fence. The NEGATIVE arm is
  # not optional: a filter that dropped EVERYTHING would still pass arm 3 on a
  # clean tree, because an empty set has no unresolvable member. Only the pair
  # separates "the filter is selective" from "the filter is blind".
  cat > "$probe/d.md" <<'MD'
Prose mentioning cargo build -p zeroship-no-such-package in a sentence.

```bash
cargo build -p zeroship-cli --bin zeroship
```
MD
  md_out="$(doc_cargo_lines "$probe/d.md")"
  if printf '%s\n' "$md_out" | grep -q -- '-p zeroship-cli'; then
    ok "markdown positive control: a fenced cargo command survives the filter"
  else
    bad "markdown positive control FAILED: the fence filter ate a fenced
       command, so every doc selector is invisible and arm 3 measures nothing"
  fi
  if printf '%s\n' "$md_out" | grep -q -- '-p zeroship-no-such-package'; then
    bad "markdown negative control FAILED: unfenced prose is still read as a
       cargo invocation - the defect that made docs/ unusable as a ROOT"
  else
    ok "markdown negative control: unfenced prose is not read as a command"
  fi

  # THE DELETED ESCAPE, same word both gates use. One variable: the marker.
  #
  # The escaped line is deliberately NOT a `#` comment. `cargo_logical_lines`
  # already drops lines that START with `#`, so a commented probe would be eaten
  # by the comment filter and this pair would pass while testing nothing - a
  # control that cannot fail for the reason it names.
  cat > "$probe/e.md" <<'MD'
```bash
cargo build -p zeroship-gone-package    # DELETED 2026-08-28
cargo build -p zeroship-cli
```
MD
  esc_out="$(doc_cargo_lines "$probe/e.md")"
  if printf '%s\n' "$esc_out" | grep -q -- '-p zeroship-gone-package'; then
    bad "DELETED escape FAILED: a line saying DELETED still yields a spec, so a
       doc cannot record a historical command without wedging this gate"
  else
    ok "DELETED escape: a fenced line saying DELETED yields no spec"
  fi
  if printf '%s\n' "$esc_out" | grep -q -- '-p zeroship-cli'; then
    ok "DELETED escape control: the neighbouring live line still yields its spec"
  else
    bad "DELETED escape control FAILED: the escape ate the whole block, so one
       historical command would silently un-cover every command beside it"
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
