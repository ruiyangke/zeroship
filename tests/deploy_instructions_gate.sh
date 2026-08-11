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
# `deploy` help line in crates/cli/src/main.rs. A hand list here would be the
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
CLI_MAIN="$ROOT/crates/cli/src/main.rs"

fail=0
note() { printf '  %s\n' "$1"; }

# --- derive the allowed flag set from the CLI's own help lines ----------------
# Each line looks like:
#   zeroship deploy   <path-to-.zship> --app=<name> [--control=URL] [--token=PAT] [--no-create]
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
HELP_LINES="$(grep -oE 'zeroship deploy[^"]*--app=[^"]*' "$CLI_MAIN" || true)"
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
if [ "$ALLOWED_COUNT" -lt 2 ]; then
  echo "FAIL: parsed only $ALLOWED_COUNT flag(s) from the deploy help lines." >&2
  echo "      Expected at least --app and one optional flag; a near-empty set" >&2
  echo "      would make every instruction look wrong. Lines were:" >&2
  printf '%s\n' "$HELP_LINES" >&2
  exit 1
fi

echo "allowed deploy flags, parsed from crates/cli/src/main.rs:"
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
  pos="$(printf '%s\n' "$cmd" \
        | sed -E 's/.*zeroship deploy[[:space:]]+//; s/[[:space:]]+--.*//; s/[[:space:]]*\\$//')"
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
MIN_INSTRUCTIONS="${DEPLOY_GATE_MIN:-5}"
echo
echo "deploy instructions checked: $checked (floor $MIN_INSTRUCTIONS)"
if [ "$checked" -lt "$MIN_INSTRUCTIONS" ]; then
  echo "FAIL: inspected only $checked deploy instruction(s), fewer than the" >&2
  echo "      $MIN_INSTRUCTIONS this gate expects. Instructions do not vanish by" >&2
  echo "      accident: either the scan path is wrong or the grep no longer" >&2
  echo "      matches how they are written. Fix the scan, do not lower the floor." >&2
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "DEPLOY INSTRUCTIONS GATE: FAILED" >&2
  exit 1
fi
echo "DEPLOY INSTRUCTIONS GATE: passed"
