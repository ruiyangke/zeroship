#!/usr/bin/env bash
# Every `zeroship deploy` command we SHIP AS AN INSTRUCTION must be one the CLI
# would actually accept.
#
# WHY THIS EXISTS. Over 2026-08-11 a basis-varying sweep of examples/ found five
# deploy instructions naming `--key=<master>`, a flag the CLI has never had. It is
# accepted silently by the arg loop and the command then dies on a missing token,
# so the error never mentions the thing the creator typed wrong:
#
#   $ zeroship deploy ./dist/app.zship --app=t --control=... --key=abc
#   zeroship deploy: no API token found; run `zeroship login`, pass `--token=<PAT>`,
#   or set ZEROSHIP_TOKEN
#
# Two of them also passed `.` - a directory - where deploy takes a path to the
# `.zship` archive it uploads. Nothing compared these instructions to the binary,
# so they rotted in place across a flag rename.
#
# THE ALLOWED SET IS DERIVED, NOT HAND-WRITTEN. It is parsed out of the CLI's own
# `deploy` help line in crates/zeroship-cli/src/main.rs. A hand list here would be the
# shape this repo has been burned by twice: an assertion ABOUT a thing, sitting
# beside the thing, satisfiable by editing the assertion. Add a flag to deploy and
# update its help, and this gate follows; add one and skip the help, and the gate
# reports the new flag as unknown, which is also the right answer.
#
# WHAT IT DOES NOT CHECK: that the command would SUCCEED. It never runs deploy,
# has no control plane, and cannot know whether <uuid> resolves. It checks the
# shape only - known flags, archive-shaped positional. A command that is
# well-formed and still wrong for some other reason passes here.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI_MAIN="$ROOT/crates/zeroship-cli/src/main.rs"

# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init deploy_instructions

fail=0
note() { printf '  %s\n' "$1"; }

# --- derive the allowed flag set from the CLI's own help lines ----------------
# Each line looks like:
#   zeroship deploy   <path-to-.zship> --app=<id> [--control=URL] [--token=PAT] [--no-create]
# The set is the UNION of every such line, not the first one. main.rs states the
# deploy usage in THREE places - the module doc, the `expect` on the missing-path
# argument, and the top-level help - and when this gate was written they did not
# all list the same flags: the missing-path message omitted `--no-create`, which
# the arg loop accepts (main.rs, `--no-create` in the args scan). That
# divergence is fixed in the same commit as this file, so all three agree today.
#
# The union is what keeps that from mattering again. An earlier draft took
# `head -1`; hiding just the module-doc line made it fall through to the
# narrower message and silently drop a real flag from the allowed set, still
# printing "passed". That was found by RUNNING the refusal arm below, not by
# reading it, which is the only reason it is not still in here.
# The anchor is `--app=<id>`, and it changed from `--app=<name>` when the CLI
# stopped guessing which of the two a value was. That guess is what the anchor
# has to track: a help line still promising `--app=<name>` would be describing a
# call the parser now refuses, so this gate going dark on the rename is the
# right failure - it says the prose and the parser have parted company.
HELP_LINES="$(grep -oE 'zeroship deploy[^"]*--app=<id>[^"]*' "$CLI_MAIN" || true)"
if [ -z "$HELP_LINES" ]; then
  echo "FAIL: could not find any deploy help line in $CLI_MAIN." >&2
  echo "      This gate derives its allowed flags from those lines; with none it" >&2
  echo "      would allow everything, so it refuses to run instead." >&2
  exit 1
fi
# The two dashes are written as bracket expressions, not as `\-\-`: GNU grep
# 3.12 accepts the escape but warns "stray \ before -" on every call, and a gate
# that routinely prints warnings teaches its readers to ignore its stderr.
ALLOWED="$(printf '%s\n' "$HELP_LINES" | grep -oE '[-][-][a-z-]+' | sort -u)"
ALLOWED_COUNT="$(printf '%s\n' "$ALLOWED" | grep -c . || true)"
# MEASURED 2026-09-06: 7 flags parsed from the deploy help lines. Floor 2 is
# reused unchanged from the pre-library hand-rolled check this replaces -
# below it there are too few flags to tell "the flag set shrank" from "the
# help-line regex stopped matching".
if ! gate_arm help_flags "$ALLOWED_COUNT" 2; then
  echo "FAIL: parsed only $ALLOWED_COUNT flag(s) from the deploy help lines." >&2
  echo "      Expected at least --app and one optional flag; a near-empty set" >&2
  echo "      would make every instruction look wrong. Lines were:" >&2
  printf '%s\n' "$HELP_LINES" >&2
  gate_arms_finish || true
  exit 1
fi

# --- ATTACH THE AUTHORITY TO THE PARSER, NOT TO THE PROSE --------------------
# Everything above derives the allowed set from HELP TEXT, which is a second
# expression of the contract. The thing that decides whether a flag works is
# `DEPLOY_KNOWN_FLAGS` - the const `check_unknown_deploy_flags` tests against
# (148efa04a). If those two ever disagree, this gate certifies documentation
# against the CLI's DESCRIPTION of itself while the CLI behaves differently.
#
# The question that produced this block came from a peer (zero-migrate,
# 2026-08-11): does the comparison EXECUTE the shipped code, or a second
# expression of the same idea? Asked of THIS file, the answer was the latter.
# Measured when asked: both sides gave {--app,--control,--no-create,--token},
# so nothing was wrong - the check exists so that stays true by evidence rather
# than by coincidence.
# `|| true` for the same reason HELP_LINES above has it, and I needed the
# reminder: this file runs under `set -euo pipefail`, so without it a grep that
# matches nothing ABORTS here and the refusal message below never prints. The
# first version of this block exited 1 in silence when I renamed the const to
# test it - a guard that cannot say why it failed, in the commit that is about
# attaching instruments to their subject.
PARSER_FLAGS="$(sed -n '/^const DEPLOY_KNOWN_FLAGS/,/^];/p' "$CLI_MAIN" \
  | grep -oE '"[-][-][a-z-]+"' | tr -d '"' | sort -u || true)"
PARSER_COUNT="$(printf '%s\n' "$PARSER_FLAGS" | grep -c . || true)"
# MEASURED 2026-09-06: 7 flags parsed from DEPLOY_KNOWN_FLAGS. Floor 2 is
# reused unchanged from the pre-library hand-rolled check this replaces, for
# the same reason as help_flags above: too few flags to tell "the const
# shrank" from "the sed range stopped matching the const block".
if ! gate_arm parser_flags "$PARSER_COUNT" 2; then
  echo "FAIL: parsed only $PARSER_COUNT flag(s) from DEPLOY_KNOWN_FLAGS in $CLI_MAIN." >&2
  echo "      That const is what the CLI actually enforces; if it cannot be read" >&2
  echo "      this gate has no authority to check the help text against, so it" >&2
  echo "      refuses rather than falling back to the prose alone." >&2
  gate_arms_finish || true
  exit 1
fi
if [ "$ALLOWED" != "$PARSER_FLAGS" ]; then
  echo "FAIL: the deploy help text and the deploy PARSER disagree about flags." >&2
  echo "      help text says:      $(printf '%s' "$ALLOWED" | tr '\n' ' ')" >&2
  echo "      DEPLOY_KNOWN_FLAGS:  $(printf '%s' "$PARSER_FLAGS" | tr '\n' ' ')" >&2
  echo "      A flag in the parser but not the help is undiscoverable; a flag in" >&2
  echo "      the help but not the parser is now REJECTED at runtime, so the" >&2
  echo "      instructions this gate blesses would fail for a creator." >&2
  exit 1
fi

echo "allowed deploy flags, parsed from crates/zeroship-cli/src/main.rs"
echo "(help text and DEPLOY_KNOWN_FLAGS agree, $PARSER_COUNT flags):"
printf '%s\n' "$ALLOWED" | sed 's/^/    /'
echo

# --- scan every shipped deploy instruction -----------------------------------
# Instructions live in example READMEs, example source headers, and docs/. The
# harness scripts under tests/ are NOT scanned: tests/e2e_docker.sh carries the
# same stale flag and is tracked as repair-or-delete (task #257), an operator
# call this gate must not pre-empt by turning it red.
checked=0
while IFS= read -r line; do
  file="${line%%:*}"
  rest="${line#*:}"
  lineno="${rest%%:*}"
  cmd="${rest#*:}"
  checked=$((checked + 1))

  for flag in $(printf '%s\n' "$cmd" | grep -oE '[-][-][a-z-]+'); do
    if ! printf '%s\n' "$ALLOWED" | grep -qx -- "$flag"; then
      echo "FAIL $file:$lineno"
      note "uses $flag, which the deploy help line does not list."
      note "$cmd"
      fail=1
    fi
  done

  # The positional check applies only to lines that are actually COMMANDS - a
  # line whose first token invokes the binary. Prose mentions it too, and the
  # first draft of this gate reported four failures on a clean tree by treating
  # them as commands: a markdown link `[zeroship deploy contract](...)`, a
  # `<path.zship>` placeholder in a table, a `...`-elided example, and a real
  # command whose path ended in a `\` line-continuation. Every one was the
  # extractor's fault. Narrowing to command lines, and stripping the
  # continuation, is what makes the remaining failures mean something.
  case "$cmd" in
    *']('*) continue ;;                           # markdown link, not a command
  esac
  first_tok="$(printf '%s\n' "$cmd" | sed -E 's/^[[:space:]]*//; s/[[:space:]].*//')"
  case "$first_tok" in
    zeroship|*/zeroship) ;;
    *) continue ;;                                # prose; flags above still checked
  esac
  # A config-backed deploy may start with a flag or have no argument before an
  # inline comment. Neither is a positional artifact. When a positional is
  # present, inspect only its first token; later flags are checked above.
  pos="$(printf '%s\n' "$cmd" \
        | sed -E 's/.*zeroship deploy[[:space:]]+//; s/[[:space:]]*\\$//; s/[[:space:]]*#.*$//; s/^[[:space:]]*--.*$//; s/[[:space:]].*$//')"
  case "$pos" in
    *.zship|*.zship'>') ;;
    "") ;;   # no argument on this line; nothing to check
    *)
      echo "FAIL $file:$lineno"
      note "passes '$pos' as the artifact. deploy uploads a .zship archive and"
      note "takes a path to it, not a directory or a raw script."
      note "$cmd"
      fail=1
      ;;
  esac
done < <(grep -rn 'zeroship deploy [^`]' \
           "$ROOT/examples" "$ROOT/docs" \
           --include='*.md' --include='*.ts' --include='*.js' 2>/dev/null || true)

# --- anti-hollow-gate floor ---------------------------------------------------
# This gate is a scan: if the grep stops matching (a path moves, the wording
# changes), it inspects nothing and exits 0 looking identical to a clean run.
# The repo ships more than a handful of deploy instructions; a run that found
# almost none has found a broken scanner, not a clean tree.
# MEASURED 2026-08-20: 18 deploy instructions checked across examples/ and
# docs/. Floor 5 is reused unchanged from the pre-library hand-rolled
# MIN_INSTRUCTIONS check this replaces.
MIN_INSTRUCTIONS=5
echo
echo "deploy instructions checked: $checked (floor $MIN_INSTRUCTIONS)"
if ! gate_arm instructions_checked "$checked" "$MIN_INSTRUCTIONS"; then
  echo "FAIL: inspected only $checked deploy instruction(s), fewer than the" >&2
  echo "      $MIN_INSTRUCTIONS this gate expects. Instructions do not vanish by" >&2
  echo "      accident: either the scan path is wrong or the grep no longer" >&2
  echo "      matches how they are written. Fix the scan, do not lower the floor." >&2
  fail=1
fi

gate_arms_finish || fail=1
if [ "$fail" -ne 0 ]; then
  echo "DEPLOY INSTRUCTIONS GATE: FAILED" >&2
  exit 1
fi
echo "DEPLOY INSTRUCTIONS GATE: passed"
