#!/usr/bin/env bash
#
# Every code path a document points at must exist, and every cited line must
# land inside its file.
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
# CURRENT SCOPE AND EXPLICIT HISTORICAL EXEMPTIONS. This gate covers AGENTS.md,
# the named proposal/design set, docs/feature-map.md, docs/runbooks/*.md,
# docs/build-and-deploy-golden-path.md, and docs/reference/*.md. It does not
# silently treat every other document as live.
#
# `docs/decisions/` is an immutable record of what was true when each ADR
# landed. `docs/archive/` is superseded material retained deliberately. A dead
# path in either directory may be historically correct, so both are EXEMPT by
# policy. `check_citations` enforces that exemption even if a future caller
# hands it a broad glob. Other documents remain outside this gate's declared
# scope until they are deliberately cleaned and added.
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
# Arm 2 - every file citation in the proposals resolves, and a cited line is
# inside the file. A past-EOF citation is the quieter half: the path looks
# right, so a reader who does not open it believes the claim is anchored.
# ---------------------------------------------------------------------------

# Sets `cites_examined` and `cites_bad` for the documents passed in.
#
# Longer extensions come FIRST so the alternation cannot truncate `Foo.tsx` to
# `Foo.ts` or `config.jsonc` to `config.json`. The former blind spot silently
# skipped line citations; the same truncation in a measurement script invented
# 143 missing paths under sdks/ui that were never wrong.
#
# Extract the whole path-shaped token before selecting repository roots. A
# regex that begins at `schema/` also finds that suffix inside the shorthand
# `zeroship-schema/src/query.rs`; that is not a repository-root citation.
check_citations() {
  cites_examined=0
  cites_bad=0
  local f cite path line eof
  for f in "$@"; do
    # Historical documents preserve their contemporary citations. Keep this
    # executable exemption beside the scope policy above so a broad future
    # glob cannot silently turn either directory into a live-doc arm.
    case "$f" in
      docs/decisions/*|docs/archive/*) continue ;;
    esac
    [ -f "$f" ] || continue
    for cite in $(grep -oE '[A-Za-z0-9_./-]+\.[A-Za-z0-9]+(:[0-9]+)?' "$f" \
                  | sed -E 's#^((\.\.?)/)+##' \
                  | grep -E '^(crates|libs|sdks|tests|db|deploy|policies|schema|examples|docs)/[A-Za-z0-9_./-]+\.(tsx|jsx|jsonc|mjs|cjs|json|rs|ts|js|sh|toml|md)(:[0-9]+)?$' \
                  | sort -u); do
      case "$cite" in
        *:[0-9]*)
          path="${cite%:*}"
          line="${cite##*:}"
          ;;
        *)
          path="$cite"
          line=""
          ;;
      esac
      cites_examined=$((cites_examined + 1))

      # Every occurrence must say DELETED on its own line. One historical use
      # must not exempt a second, live use of the same path elsewhere in a doc.
      if grep -F "$cite" "$f" >/dev/null \
          && ! grep -F "$cite" "$f" | grep -qv 'DELETED'; then
        continue
      fi

      if [ ! -f "$path" ]; then
        echo "$f cites a file that does not exist: $cite" >&2
        echo "  (if the file was deleted on purpose, say DELETED on that line)" >&2
        cites_bad=$((cites_bad + 1))
        continue
      fi
      if [ -n "$line" ]; then
        eof=$(wc -l < "$path")
      fi
      if [ -n "$line" ] && [ "$line" -gt "$eof" ]; then
        echo "$f cites $cite but that file has only $eof lines" >&2
        cites_bad=$((cites_bad + 1))
      fi
    done
  done
}

check_citations docs/proposals/2026-08-26-*.md \
                docs/proposals/2026-08-31-*.md \
                docs/proposals/2026-07-10-migrate-*.md \
                docs/proposals/2026-07-11-migrate-*.md \
                docs/proposals/2026-07-12-zero-migrate-redesign-plan.md
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
#
# THAT WARNING CAME TRUE AND WAS NOT ACTED ON. Measured 2026-09-03: arm 2's
# `2026-08-26-*` and this arm's `2026-08-28-*` between them left SEVEN proposals
# unexamined by any arm - `2026-08-31-data-crate-shape.md`, the design document
# for the crate split then under active revision, and the six `2026-07-*` migrate
# proposals. All seven were rewritten that day and the gate printed a clean green
# about none of them. Arm 2 now names them.
#
# SWEEPING IN `docs/proposals/*.md` WAS TRIED AND REJECTED, and the measurement
# is the reason to leave it rejected: the full glob takes the examined count from
# 303 to 1292 and surfaces 105 dead citations across 24 OTHER proposals. Those
# are not rot this gate should fail on today - per the scope policy at the top of
# this file, a document joins the gate when it has been deliberately CLEANED and
# added, and `docs/decisions/` and `docs/archive/` are exempt outright because a
# dead path in a historical record may be correct. Adding 24 uncleaned documents
# at once would either wedge the gate red or force 105 repairs nobody scoped.
# Clean a proposal, then add it by name on the same commit.
# ---------------------------------------------------------------------------
check_citations docs/architecture/data-system.md \
                docs/proposals/2026-08-28-*.md
gate_arm design_set_citations "$cites_examined" 40 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
design_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 3 - the live feature inventory.
# ---------------------------------------------------------------------------
check_citations docs/feature-map.md
gate_arm feature_map_citations "$cites_examined" 200 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
feature_map_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 4 - operational runbooks.
# ---------------------------------------------------------------------------
check_citations docs/runbooks/*.md
gate_arm runbook_citations "$cites_examined" 20 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
runbook_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 5 - the primary creator build-and-deploy path.
# ---------------------------------------------------------------------------
check_citations docs/build-and-deploy-golden-path.md
gate_arm golden_path_citations "$cites_examined" 5 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
golden_path_cites=$cites_examined

# ---------------------------------------------------------------------------
# Arm 6 - docs/reference, the stable-contract set AGENTS.md sends readers to.
# It reached zero broken paths on 2026-08-29 and nothing was stopping it drifting
# back; every one of its 15 citations had pointed into `third_party/zero-migrate/`
# for as long as the engine had been in-sourced out of it. The widened extractor
# now rules on bare paths and document/schema citations too, so its floor rises
# with that materially larger population while retaining ample deletion room.
# ---------------------------------------------------------------------------
check_citations docs/reference/*.md
gate_arm reference_citations "$cites_examined" 50 || FAILED=1
[ "$cites_bad" -eq 0 ] || FAILED=1
reference_cites=$cites_examined

gate_arms_finish || FAILED=1

if [ "$FAILED" -ne 0 ]; then
  echo "::error::doc citation gate FAILED" >&2
  echo "::error::  A path in prose is a claim. Fix the path, or say DELETED." >&2
  exit 1
fi

echo "doc citations: $agents_examined AGENTS.md paths, $proposal_cites proposal citations, $design_cites design-set citations, $feature_map_cites feature-map citations, $runbook_cites runbook citations, $golden_path_cites golden-path citations, $reference_cites reference citations, all resolve"
