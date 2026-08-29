#!/usr/bin/env bash
#
# Every code path a document points at must exist, and every `path:line`
# citation must land inside its file.
#
# WHAT THIS GATE DOES NOT CATCH, measured 2026-08-29. It rules on the PATH and
# on the line being within the file, never on the line being the RIGHT one. Two
# design reviewers independently found four citations in the db proposal set
# that each pointed one line above their symbol - RESERVED_ENV_DB_NAMES cited
# at :1136 when it is declared at :1137, and the same offset on three more.
# Every one of them passed this gate green, because every one named a real file
# and a line inside it.
#
# A line-accuracy arm was measured and REJECTED rather than skipped. Checking
# that a cited line still contains its symbol needs the symbol, and only 5 of
# the 94 path:line citations in those two documents put a backticked identifier
# adjacent to the citation - the rest wrap across lines or name a phrase. The
# pattern that found those 5 also found 0 in data-system.md, a file that
# demonstrably contains such a citation, so the measurement was of the regex
# rather than of the docs. The alternative - recording each cited line's
# content so drift is detectable - is a census of expected values, which is the
# thing this repo's gate discipline exists to refuse.
#
# So line numbers in prose are drift-prone BY CONSTRUCTION and this gate does
# not pretend otherwise. The path is the durable claim; the line is a courtesy.
# If a citation's line matters to an argument, quote the code instead.
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

# Sets `cites_examined` and `cites_bad` for the documents passed in.
#
# `tsx`/`jsx` come FIRST so the longer extension wins the alternation. Listing
# `ts` first truncates `Foo.tsx:12` to `Foo.ts`, which then fails to match the
# `:line` and is silently SKIPPED - a blind spot, not a false alarm, and the
# quieter of the two failure modes. The same truncation in a measurement script
# invented 143 missing paths under sdks/ui that were never wrong.
check_citations() {
  cites_examined=0
  cites_bad=0
  local f cite path line eof
  for f in "$@"; do
    [ -f "$f" ] || continue
    for cite in $(grep -oE '(crates|libs|sdks|tests|db)/[A-Za-z0-9_./-]+\.(tsx|jsx|mjs|cjs|rs|ts|js|sh|toml):[0-9]+' "$f" \
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
}

check_citations docs/proposals/2026-08-26-*.md
gate_arm proposal_citations "$cites_examined" 60 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
proposal_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 2b - the LIVE DESIGN SET, and the reason this arm exists is a failure of
# exactly the kind this gate is for.
#
# Arm 2 globs `docs/proposals/2026-08-26-*.md`. The two documents under active
# revision are `docs/proposals/2026-08-28-app-database-decoupling.md` and
# `docs/architecture/data-system.md`. The date glob excludes the first and no
# arm scanned `docs/architecture/` at all, so BOTH WERE INVISIBLE TO THIS GATE.
#
# Measured 2026-08-29: across a session that edited those two files repeatedly,
# every run printed "all resolve" and none of it was about them. The counts that
# moved belonged to the decision log, which does match the 08-26 glob - so the
# gate looked responsive while examining none of the work. That is this gate's
# own founding failure, one directory over: a green that is silent about the
# thing you were changing.
#
# The lesson generalises past these two files. A date-prefixed glob silently
# stops covering a document set the day someone writes tomorrow's date, and
# nothing announces it. If a third design document appears, it must be added
# here or it is unexamined; the floor below is the only thing that will notice
# a file dropping OUT.
# ---------------------------------------------------------------------------
check_citations docs/architecture/data-system.md \
                docs/proposals/2026-08-28-*.md
gate_arm design_set_citations "$cites_examined" 40 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
design_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 3 - docs/reference, the stable-contract set AGENTS.md sends readers to.
# It reached zero broken paths on 2026-08-29 and nothing was stopping it drifting
# back; every one of its 15 citations had pointed into `third_party/zero-migrate/`
# for as long as the engine had been in-sourced out of it. The floor is low on
# purpose - this arm exists to catch a BROKEN citation, and a reference doc that
# legitimately loses citations should not redden it.
# ---------------------------------------------------------------------------
check_citations docs/reference/*.md
gate_arm reference_citations "$cites_examined" 5 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
reference_cites=$cites_examined
cites_examined=$proposal_cites

gate_arms_finish || FAILED=1

if [ "$FAILED" -ne 0 ]; then
  echo "::error::doc citation gate FAILED" >&2
  echo "::error::  A path in prose is a claim. Fix the path, or say DELETED." >&2
  exit 1
fi

echo "doc citations: $agents_examined AGENTS.md paths, $proposal_cites proposal citations, $design_cites design-set citations, $reference_cites reference citations, all resolve"
