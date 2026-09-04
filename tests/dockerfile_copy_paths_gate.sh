#!/usr/bin/env bash
# ============================================================================
# Every path deploy/Dockerfile COPYs from the build context must EXIST in the
# repo.
#
# THE DEFECT THIS EXISTS FOR, measured 2026-08-11 on a real host:
#
#   ERROR: failed to compute cache key: "/apps": not found
#   Dockerfile:42  >>> COPY apps/ apps/
#
# `apps/zeroship-builder` was extracted out of this repo by 8dbe4a8d4, but
# deploy/Dockerfile still copies apps/, builds `zeroship-builder`, and asserts
# its dist/app.zship exists - and deploy/compose/docker-compose.yml still passes
# --bootstrap-console --console-zship pointing at the artifact that build would
# have produced. So the deploy path AGENTS.md documents
# (`docker compose -f deploy/compose/docker-compose.yml up -d`) could not build
# at all, and had not been able to since that extraction.
#
# WHY NOTHING CAUGHT IT: grep over .github/workflows/*.yml for `deploy/Dockerfile`,
# `docker build` and `docker compose` returns ZERO hits. The platform image is
# built in no workflow. That is the finding behind this gate - not an instrument
# reporting falsely (#280, #312) but a path carrying no instrument at all.
#
# WHY THIS SHAPE. Building the image in CI is the thorough answer and costs a
# full Rust + V8 compile; this is the cheap check that would have caught THIS
# defect in under a second. It is a lint, not a substitute: it proves the context
# paths resolve, and NOTHING about whether the build then succeeds.
#
# WHAT IT DELIBERATELY DOES NOT CHECK, so nobody reads a green as more than it is:
#   - that the image builds (it does not run docker at all)
#   - COPY --from=<stage> lines, which read from a previous stage, not the
#     context, so the repo has nothing to say about them
#   - that a path which EXISTS holds what the Dockerfile expects
#   - whether compose's flags match what the image actually contains, which is
#     the other half of the apps/ defect and is not a path question
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DF="$ROOT/deploy/Dockerfile"
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

# Per-arm anti-vacuity accounting (tests/lib/gate_arms.sh). The two checks
# below read two independent regions of the Dockerfile - every COPY line, and
# just the `AS builder` stage body - so either can collapse to zero on its own
# if the shape it depends on moves.
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init dockerfile_copy_paths

echo "============================================"
echo "  deploy/Dockerfile COPY paths resolve in the repo"
echo "============================================"

[ -f "$DF" ] || { echo "  x REFUSED: $DF not found - nothing to check." >&2; exit 1; }

# Parse COPY lines that read from the BUILD CONTEXT (no --from=). The last field
# is the destination; every field before it is a source. Flags are dropped.
mapfile -t SRCS < <(
  awk '
    toupper($1) == "COPY" {
      if ($0 ~ /--from=/) next
      n = 0; delete f
      for (i = 2; i <= NF; i++) { if ($i ~ /^--/) continue; f[++n] = $i }
      for (i = 1; i < n; i++) print f[i]          # all but the destination
    }
  ' "$DF"
)

# ---------------------------------------------------------------------------
# THE SAME NUMBER, DERIVED A SECOND WAY.
#
# This replaces a hand-maintained `EXPECT_RAN=30` that sat at the bottom of this
# file, was measured on 2026-08-28, and was WRONG by eight on 2026-09-04 - so
# this gate exited 1 on every tree, including main, from `18c6816aa`
# ("record the measurement behind the wholesale crate copy") forward. That commit
# collapsed the `sdks` stage's eight individually-named crate COPYs into one
# `COPY crates/ crates/`. Nothing about that is a defect; it is exactly the
# ordinary edit a pinned census cannot survive, and the repository's convention
# (tests/lib/gate_arms.sh) is that a number lives beside the code producing it
# rather than in a pinned table. Four gates went red the same week two crates
# landed, for that reason.
#
# What the pin was actually FOR is worth keeping: "fewer means sources went
# missing from the parse", i.e. the awk above silently losing its match. A census
# is a poor instrument for that and a SECOND EXTRACTOR is a good one, because the
# two are blind differently - awk matches on field 1 and splits on fields, this
# one matches with grep and counts whitespace-separated tokens. Per context COPY
# line the tokens are: the COPY keyword, zero or more `--flags`, one or more
# sources, and exactly one destination. So
#
#   sources = tokens - flags - 2 * lines
#
# MEASURED 2026-09-04 at 6b3cc0641: lines=17 tokens=55 flags=0 -> 21, which is
# what the awk parse also yields. They agree by derivation, not by a number
# anyone typed, so adding or removing a COPY moves both together.
CTX_COPY_LINES="$(grep -iE '^[[:space:]]*COPY[[:space:]]' "$DF" | grep -v -- '--from=')"
N_CTX_LINES=$(printf '%s\n' "$CTX_COPY_LINES" | grep -c .)
N_CTX_TOKENS=$(printf '%s\n' "$CTX_COPY_LINES" | tr -s '[:space:]' '\n' | grep -c .)
N_CTX_FLAGS=$(printf '%s\n' "$CTX_COPY_LINES" | tr -s '[:space:]' '\n' | grep -c -- '^--')
DERIVED_SRCS=$((N_CTX_TOKENS - N_CTX_FLAGS - 2 * N_CTX_LINES))

N_SRCS=${#SRCS[@]}
# MEASURED 2026-08-20: 14 context COPY sources in deploy/Dockerfile. Floor well
# under that: adding or dropping a COPY line for one workspace member should
# not trip this, while the failure this guards against - the awk COPY-line
# parser losing its match - drops it to zero, not to single digits.
if ! gate_arm context_copy_sources "$N_SRCS" 5; then
  echo "  x REFUSED: parsed too few context COPY sources out of $DF." >&2
  echo "    Either the Dockerfile changed shape or this parser is broken." >&2
  echo "    A gate that checks nothing must not report success." >&2
  exit 1
fi

# The two extractors must agree. This is the check the deleted `EXPECT_RAN` pin
# was reaching for, done against a second reading of the same file instead of
# against a number from a previous week.
if [ "$N_SRCS" -ne "$DERIVED_SRCS" ]; then
  echo "  x REFUSED: the two COPY-source extractors disagree." >&2
  echo "    awk field parse: $N_SRCS   grep/token parse: $DERIVED_SRCS" >&2
  echo "    ($N_CTX_LINES context COPY lines, $N_CTX_TOKENS tokens, $N_CTX_FLAGS flags)" >&2
  echo "    One of them has lost its match on a Dockerfile shape it does not" >&2
  echo "    handle - a line continuation, a quoted path, or the JSON array form." >&2
  echo "    Neither count can be trusted until they agree; fix the parser, do" >&2
  echo "    not pin the number." >&2
  exit 1
fi

for s in "${SRCS[@]}"; do
  p="${s%/}"
  if [ -e "$ROOT/$p" ]; then
    pass "COPY $s -> $p exists"
  else
    fail "COPY $s -> $p DOES NOT EXIST in the repo; the image cannot build (see task #318)"
  fi
done

# ---------------------------------------------------------------------------
# SECOND CHECK, and it points the OPPOSITE WAY from the first.
#
# The check above asks "does every COPY name a path that exists". By construction
# it cannot see a path that is NEEDED and never COPYed at all. That has now
# happened three times on the real deploy path, and each one cost a container
# build to find:
#
#   blocker 2  third_party/ needed by the NODE stage for pnpm's workspace members
#   blocker 7  libs/ never copied at all; cargo could not load the workspace
#   blocker 8  .cargo/ never copied, so the workspace's own linker flag was
#              absent and the release build died at LINK time:
#                rust-lld: error: duplicate symbol: XXH_versionNumber
#              libpg_query and librdkafka each vendor xxhash, and
#              zeroship-platform-migrate links both.
#
# This checks ONLY the third one, and deliberately so. A general "every needed
# root is copied" check needs a definition of "needed" that nothing in the repo
# supplies - my first attempt derived cargo roots from Cargo.toml, which scooped
# `refs` out of [workspace.exclude] and reported two failures on a correct tree.
# A gate that lies is worse than a gate that is narrow, so this is narrow: one
# fact, mechanically checkable, no derivation.
#
# WHAT IT STILL DOES NOT CATCH: any other root that is needed and uncopied. The
# blocker-2 and blocker-7 shapes remain uncovered by anything here.
#
# IT IS CONDITIONAL, so it contributes to the assertion count only when its
# premise holds. `BUILDER_CHECKS` carries that, rather than the tail of this file
# assuming it always ran: if `.cargo/config.toml` ever stops setting rustflags
# the premise is genuinely gone, and the bookkeeping below must shrink with it
# instead of reporting a lost assertion.
# ---------------------------------------------------------------------------
BUILDER_CHECKS=0
if [ -f "$ROOT/.cargo/config.toml" ] && grep -q "rustflags" "$ROOT/.cargo/config.toml"; then
  BUILDER_CHECKS=1
  BUILDER_BODY="$(awk '/^FROM .* AS builder/{f=1;next} /^FROM /{f=0} f' "$DF")"
  N_BUILDER_LINES=$(printf '%s\n' "$BUILDER_BODY" | grep -c .)
  # MEASURED 2026-08-20: 100 lines in the `AS builder` stage body. Floor well
  # under that: ordinary edits to the builder stage move this by a handful of
  # lines, while the failure this guards against - the awk stage-boundary
  # match losing its anchor - drops it to zero, which "no builder body found"
  # alone cannot be told apart from a builder stage that legitimately shrank.
  if ! gate_arm builder_stage_body "$N_BUILDER_LINES" 20; then
    echo "  x REFUSED: 'AS builder' stage body too small to trust - this check's premise is gone." >&2
    exit 1
  fi
  if printf '%s\n' "$BUILDER_BODY" | grep -qE '^COPY[[:space:]]+\.cargo/'; then
    pass ".cargo/ (sets rustflags) is COPYed into the builder stage"
  else
    fail ".cargo/ sets rustflags but the builder stage never COPYs it; the release link fails on duplicate xxhash symbols (blocker 8)"
  fi
fi

echo ""
echo "  $PASS passed, $FAIL failed, $((PASS+FAIL)) ran"

# Counts assertions that RAN, not that PASSED: a mutation moves an outcome
# BETWEEN those columns, so only a LOST assertion drops the sum.
#
# EXACT, and DERIVED rather than pinned. Every context COPY source gets exactly
# one assertion in the loop above, and the conditional builder-stage check
# contributes `BUILDER_CHECKS`, so the sum is fully determined by the Dockerfile
# this run just read. It cannot go stale, and it still catches what a pin caught:
# an assertion that stopped being emitted while its source was still parsed.
#
# A PINNED `EXPECT_RAN=30` STOOD HERE UNTIL 2026-09-04 AND WAS RED ON MAIN. It was
# re-measured three times in ten days (14 -> 15 -> 19 -> 29 -> 30, each with a
# paragraph of arithmetic), and then `18c6816aa` collapsed the `sdks` stage's
# eight per-crate COPYs into one `COPY crates/ crates/`. 22 ran, 30 was expected,
# and the gate failed for eight commits on a Dockerfile that was correct - while
# reporting "0 failed" one line above, which is what a census does when it
# outlives its measurement. The history above is deleted with it: it recorded how
# the number reached 30, which is of no use once nothing is pinned to 30.
EXPECT_RAN=$((N_SRCS + BUILDER_CHECKS))
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -ne "$EXPECT_RAN" ]; then
  echo "  x COUNT: $RAN assertions ran, but the Dockerfile just parsed yields" >&2
  echo "    $EXPECT_RAN ($N_SRCS context COPY sources + $BUILDER_CHECKS builder-stage check)." >&2
  echo "    An assertion was lost between the parse and the loop that emits it;" >&2
  echo "    a smaller green is not a pass." >&2
  rc=1
fi

gate_arms_finish || rc=1
exit $rc
