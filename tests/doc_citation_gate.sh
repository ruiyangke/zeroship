#!/usr/bin/env bash
#
# Every code path a document points at must exist, and every `path:line`
# citation must land inside its file.
#
# THE FAILURE THIS EXISTS FOR, measured 2026-08-28. The zeroship- crate rename
# moved every crate to a `zeroship-` prefix and nothing re-read the prose that
# pointed at them. 1082 of 1497 distinct code paths named under docs/ resolved
# to nothing. AGENTS.md - the file every agent loads first - carried 28 dead
# paths, including line 489, which instructs the reader to run
#     ./crates/runtime/tests/setup-wpt.sh
# a file that has not existed under that name for weeks. An agent following the
# documented setup gets "No such file or directory", which reads as a broken
# REPO rather than a stale DOC, so the cost is paid by whoever trusts the
# documentation most.
#
# It went unnoticed because a rename is mechanically safe for CODE - the
# compiler finds every caller - and mechanically invisible for PROSE. Nothing
# in the tree read documentation as if it made checkable claims. This gate
# does, so the next rename cannot be half-applied in silence.
#
# WHY THE SCOPE IS AGENTS.md PLUS THE 2026-08-26 PROPOSAL SET, AND NOT ALL DOCS.
# 564 paths under docs/ still do not resolve after that repair, and running arm 2
# over every proposal reports about 30 more across a dozen older documents (both
# measured the same day). They are NOT the rename: the sandbox moved to its own
# repository, gatekit and migrate-adapter were deleted outright, the auth crate
# was restructured, and the compio-* drivers live under libs/ rather than crates/.
# Widening this gate before that work is done would commit it RED, and a gate
# that is red on arrival gets disabled rather than fixed - which is how the tree
# ended up with four gates examining nothing in the first place. Widen it when
# those are cleared; both arms are written so adding a glob is a one-line change.
#
# THE `DELETED` ESCAPE. A document may legitimately cite a file that the change
# it describes went on to delete - a design doc naming the code it replaced.
# Such a citation passes only if the citing line also says DELETED, so the
# author has to state the fact rather than leave a pointer that silently rots.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1

# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init doc_citation

FAILED=0

# Two paths AGENTS.md names that are BUILD OUTPUTS, absent from a clean
# checkout and present after the command that makes them. Listed here, beside
# the code that consumes the list, with the reason - not in a central table.
is_generated_artifact() {
  case "$1" in
    sdks/ui/coverage/*|sdks/ui/coverage) return 0 ;;   # pnpm test-storybook:coverage
    tests/data/live/tls_live.conf) return 0 ;;         # tests/tls_live_setup.sh
    *) return 1 ;;
  esac
}

# ---------------------------------------------------------------------------
# Arm 1 - every literal path AGENTS.md names exists.
# ---------------------------------------------------------------------------
agents_examined=0
agents_bad=0
# The character class DELIBERATELY includes `{},` so a brace form such as
# `crates/plugin-{db,kv,storage}/` is captured WHOLE and then skipped below.
# Excluding those characters instead truncates it to `crates/plugin-`, which is
# not a brace form, is not skipped, and is reported as a missing path.
for p in $(grep -oE '(crates|libs|sdks|db|tests|deploy|policies|schema|examples|docs)/[A-Za-z0-9_./{},*-]+' AGENTS.md \
           | tr -d '`' | sed 's/[.,)]*$//' | sort -u); do
  case "$p" in *[{}\*]*) continue ;; esac      # brace/glob forms are prose
  is_generated_artifact "$p" && continue
  agents_examined=$((agents_examined + 1))
  if [ ! -e "$p" ]; then
    echo "AGENTS.md names a path that does not exist: $p" >&2
    agents_bad=$((agents_bad + 1))
  fi
done
gate_arm agents_md_paths "$agents_examined" 40 || FAILED=1
[ "$agents_bad" -eq 0 ] || FAILED=1

# ---------------------------------------------------------------------------
# Arm 2 - every `path:line` citation in the proposals resolves, and the line is
# inside the file. A past-EOF citation is the quieter half: the path looks
# right, so a reader who does not open it believes the claim is anchored.
# ---------------------------------------------------------------------------
cites_examined=0
cites_bad=0
for f in docs/proposals/2026-08-26-*.md; do
  [ -f "$f" ] || continue
  for cite in $(grep -oE '(crates|libs|sdks|tests|db)/[A-Za-z0-9_./-]+\.(rs|ts|sh|toml):[0-9]+' "$f" \
                | sort -u); do
    path="${cite%%:*}"
    line="${cite##*:}"
    cites_examined=$((cites_examined + 1))

    # A citation whose own line says DELETED is stating history on purpose.
    if grep -F "$cite" "$f" | grep -q 'DELETED'; then
      continue
    fi

    if [ ! -f "$path" ]; then
      echo "$f cites a file that does not exist: $cite" >&2
      echo "  (if the file was deleted on purpose, say DELETED on that line)" >&2
      cites_bad=$((cites_bad + 1))
      continue
    fi
    eof=$(wc -l < "$path")
    if [ "$line" -gt "$eof" ]; then
      echo "$f cites $cite but that file has only $eof lines" >&2
      cites_bad=$((cites_bad + 1))
    fi
  done
done
gate_arm proposal_citations "$cites_examined" 60 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1

gate_arms_finish || FAILED=1

if [ "$FAILED" -ne 0 ]; then
  echo "::error::doc citation gate FAILED" >&2
  echo "::error::  A path in prose is a claim. Fix the path, or say DELETED." >&2
  exit 1
fi

echo "doc citations: $agents_examined AGENTS.md paths, $cites_examined proposal citations, all resolve"
