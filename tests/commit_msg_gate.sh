#!/usr/bin/env bash
# Commit-message conventions, enforced. See CONTRIBUTING.md -> Commit messages.
#
# Three call sites, one implementation:
#   --file <path>       .githooks/commit-msg, at the moment the commit is written
#   --range <A>..<B>    CI, over a PR range
#   --self-test         proves the gate can FAIL; see WHY below
#
# WHY A SELF-TEST. A linter that accepts everything and a linter that is
# silently broken produce identical output: silence. This repo has been bitten
# by that shape repeatedly (a suite that exited 0 without a database, a gate
# whose cargo flag made it unable to run). So the gate carries known-bad
# messages and asserts each is rejected for the expected reason.
#
# WHAT THIS DOES NOT CATCH. Only the message. It cannot tell whether the
# subject describes the diff, whether the body's claims are true, or whether a
# fix carries the regression test CONTRIBUTING.md requires. Those stay human.

set -uo pipefail

MAX_SUBJECT=100          # measured: accepts 97% of a week of unconstrained
                         # subjects, rejects the multi-clause outliers
BODY_WARN_LINES=20       # advisory only, never fails
MAX_BODY_LINE=80         # git log indents the body by 4; keep it inside 80 cols

TYPES='fix|feat|refactor|test|docs|merge|chore|style|build|ci|perf|bench|revert'

fail_count=0
last_reason=""

# Prints each violation; returns 1 if the message is bad.
lint_message() {
  local msg="$1" label="${2:-}" subject rest bad=0
  subject="$(printf '%s' "$msg" | sed -n '1p')"
  rest="$(printf '%s' "$msg" | sed '1d')"

  # git's own auto-generated merges and reverts, and rebase fixups, are not
  # authored prose; blocking them would break ordinary git operations.
  case "$subject" in
    "Merge branch "*|"Merge remote-tracking "*|"Merge tag "*|"Revert \""*|fixup!*|squash!*|amend!*)
      return 0 ;;
  esac
  # A comment-only or empty message: git aborts on its own.
  [ -z "${subject//[[:space:]]/}" ] && return 0

  say() { printf '  %s\n' "$1"; last_reason="$2"; bad=1; }

  if ! [[ $subject =~ ^($TYPES)\([a-z0-9-]+\)!?:\ .+ ]]; then
    if [[ $subject =~ ^($TYPES)!?:\  ]]; then
      say "missing scope: write type(scope): ..., never a bare type:" NO_SCOPE
    elif [[ $subject =~ ^($TYPES)\([a-z0-9-]+,[a-z0-9-]+ ]]; then
      say "two scopes: pick the one that dominates the change" MULTI_SCOPE
    elif [[ $subject =~ ^[a-z-]+:\  ]]; then
      say "'${subject%%:*}' is not a type. Types: ${TYPES//|/ }" BAD_TYPE
    else
      say "not 'type(scope): summary'. Types: ${TYPES//|/ }" BAD_FORMAT
    fi
  fi

  if [ "${#subject}" -gt "$MAX_SUBJECT" ]; then
    say "subject is ${#subject} chars, limit is $MAX_SUBJECT" TOO_LONG
  fi

  case "$subject" in
    *.) say "subject ends with a period" TRAILING_DOT ;;
  esac

  # Orchestration artifacts: meaningless to anyone reading the log later.
  if [[ $subject =~ \#[0-9]+ ]]; then
    say "issue/PR number in the subject; state the outcome instead" ISSUE_NUM
  fi
  if [[ $subject =~ [0-9]" of "[0-9] ]]; then
    say "sweep counter ('N of M') in the subject" SWEEP_COUNTER
  fi
  if [[ $subject =~ (^|[^a-zA-Z])([Pp]hase|[Ss]tage|[Ww]ave|[Mm]ilestone)([^a-zA-Z]|$) ]]; then
    say "process marker (phase/stage/wave/milestone) in the subject" PROCESS_MARKER
  fi

  # An ordinary capitalised word after the colon. Identifiers (ZeroshipDb, S3,
  # RFC, SQLite) are fine and must stay allowed, so this only fires on
  # Capital+lowercase, which is prose, not a symbol.
  local after="${subject#*: }"
  if [[ $after =~ ^[A-Z][a-z]+([[:space:]]|$) ]]; then
    say "capitalised first word after the colon; lowercase unless it is an identifier" CAPITALISED
  fi

  if printf '%s' "$msg" | LC_ALL=C grep -q '[^ -~	]'; then
    say "non-ASCII character (em-dash, curly quote, arrow); use ASCII" NON_ASCII
  fi

  # A body must be separated by one blank line, or git tooling folds it in.
  if [ -n "$rest" ]; then
    local second
    second="$(printf '%s' "$msg" | sed -n '2p')"
    if [ -n "${second//[[:space:]]/}" ]; then
      say "body must be separated from the subject by a blank line" NO_BLANK_LINE
    fi
  fi

  # Body lines must be hard-wrapped. `git log` indents the body by four and
  # does not re-flow, so an unwrapped paragraph runs off the terminal. Only
  # prose is checked: an indented block (code, command output, a table) is
  # verbatim by intent, and a line whose first word already passes the limit
  # is an unbreakable token such as a URL or a path.
  local line
  while IFS= read -r line; do
    [ "${#line}" -le "$MAX_BODY_LINE" ] && continue
    case "$line" in
      " "*|"	"*|"|"*|'```'*|"- "*|"* "*) continue ;;
    esac
    local first="${line%% *}"
    [ "$line" = "$first" ] && continue                 # single long token
    [ "${#first}" -gt "$MAX_BODY_LINE" ] && continue   # unbreakable leading token
    say "body line is ${#line} chars; wrap the body at $MAX_BODY_LINE" LONG_BODY_LINE
    break
  done <<< "$rest"

  if [ "$bad" -ne 0 ]; then
    [ -n "$label" ] && printf '  (%s)\n' "$label"
    return 1
  fi

  # Advisory, never fatal: the guide asks for a body only when the why is not
  # obvious, and for prose rather than a dump.
  local body_lines
  body_lines="$(printf '%s' "$rest" | grep -c . || true)"
  if [ "${body_lines:-0}" -gt "$BODY_WARN_LINES" ]; then
    printf '  note: body is %s lines. A good subject carries most changes;\n' "$body_lines"
    printf '        keep the why, drop the narration.\n'
  fi
  return 0
}

usage() { sed -n '2,18p' "$0"; exit 2; }

main() {
  [ $# -ge 1 ] || usage
  case "$1" in
    --file)
      [ $# -eq 2 ] || usage
      # Strip comment lines git appends to the editor buffer.
      local msg
      msg="$(sed '/^#/d' "$2")"
      if ! lint_message "$msg"; then
        printf '\ncommit-msg: rejected. See CONTRIBUTING.md -> Commit messages.\n'
        printf 'Subject: %s\n' "$(printf '%s' "$msg" | sed -n '1p')"
        return 1
      fi
      return 0
      ;;
    --range)
      [ $# -eq 2 ] || usage
      local sha n=0
      for sha in $(git rev-list --no-merges "$2"); do
        n=$((n + 1))
        if ! lint_message "$(git log -1 --pretty=format:%B "$sha")" "$sha"; then
          printf '  ^ %s %s\n\n' "$(git rev-parse --short "$sha")" \
            "$(git log -1 --pretty=format:%s "$sha")"
          fail_count=$((fail_count + 1))
        fi
      done
      # A range that resolves to nothing is not a pass. Same failure shape this
      # repo has hit before: an empty run is indistinguishable from a clean one.
      if [ "$n" -eq 0 ]; then
        printf 'commit-msg gate: range %s matched 0 commits; nothing was checked.\n' "$2"
        return 1
      fi
      printf 'commit-msg gate: checked %d commits, %d rejected.\n' "$n" "$fail_count"
      [ "$fail_count" -eq 0 ]
      return
      ;;
    --self-test)
      local bad_ok=0 bad_total=0 good_ok=0 good_total=0
      check_bad() { # <expected-reason> <message>
        bad_total=$((bad_total + 1))
        last_reason=""
        if lint_message "$2" >/dev/null 2>&1; then
          printf 'SELF-TEST FAIL: accepted a message it must reject (%s): %s\n' "$1" "$2"
        elif [ "$last_reason" != "$1" ]; then
          printf 'SELF-TEST FAIL: rejected for %s, expected %s: %s\n' "$last_reason" "$1" "$2"
        else
          bad_ok=$((bad_ok + 1))
        fi
      }
      check_good() {
        good_total=$((good_total + 1))
        if lint_message "$1" >/dev/null 2>&1; then good_ok=$((good_ok + 1))
        else printf 'SELF-TEST FAIL: rejected a valid message: %s\n' "$1"; fi
      }

      check_bad NO_SCOPE       'ci: wire the harness'
      check_bad BAD_TYPE       'deploy: unblock the image build'
      check_bad BAD_FORMAT     'made the thing faster'
      check_bad MULTI_SCOPE    'fix(gateway,runtime): two authorization bypasses'
      check_bad TRAILING_DOT   'fix(db): keep decimal defaults.'
      check_bad ISSUE_NUM      'test(e2e): wait for a query-able Postgres (#274)'
      check_bad SWEEP_COUNTER  'test(e2e): private vite port for auth and db, 8 of 8'
      check_bad PROCESS_MARKER 'chore(reorg) phase 5: libs extraction'
      check_bad CAPITALISED    'fix(db): Keep decimal-literal column defaults'
      check_bad NO_BLANK_LINE  "$(printf 'fix(db): keep decimal defaults\nbody with no blank line')"
      # A one-variable partner for the length rule: identical but for length.
      check_bad TOO_LONG "fix(db): $(printf 'x%.0s' $(seq 1 $MAX_SUBJECT))"
      check_good        "fix(db): $(printf 'x%.0s' $(seq 1 $((MAX_SUBJECT - 10))))"

      check_good 'fix(plugin-db): keep decimal-literal column defaults in DDL'
      check_good 'refactor(core)!: move wrapper_revocation out of core into authz'
      check_good 'fix(types): ZeroshipDb.transaction declared its own union'
      check_good 'fix(bundle): S3 dedup must verify the bytes the caller supplied'
      check_good "$(printf 'fix(db): keep decimal defaults\n\nA real body, correctly separated.')"
      check_good 'Merge branch '"'"'main'"'"' of github.com:ruiyangke/zeroship'

      # Body wrapping, with its one-variable partner: the same paragraph, once
      # over the limit and once under. Then the three exemptions, each of which
      # would make the rule unusable if it fired.
      local long_para short_para
      long_para="$(printf 'word %.0s' $(seq 1 25))"
      short_para="$(printf 'word %.0s' $(seq 1 12))"
      check_bad  LONG_BODY_LINE "$(printf 'fix(db): keep decimal defaults\n\n%s' "$long_para")"
      check_good "$(printf 'fix(db): keep decimal defaults\n\n%s' "$short_para")"
      check_good "$(printf 'fix(db): keep decimal defaults\n\n    %s' "$long_para")"
      check_good "$(printf 'fix(db): keep decimal defaults\n\nhttps://example.com/%s' \
        "$(printf 'x%.0s' $(seq 1 90))")"
      check_good "$(printf 'fix(db): keep decimal defaults\n\n| a | markdown table row that is deliberately much longer than the limit |')"

      printf 'self-test: %d/%d bad messages rejected for the right reason, %d/%d good accepted\n' \
        "$bad_ok" "$bad_total" "$good_ok" "$good_total"
      [ "$bad_ok" -eq "$bad_total" ] && [ "$good_ok" -eq "$good_total" ]
      return
      ;;
    *) usage ;;
  esac
}

main "$@"
