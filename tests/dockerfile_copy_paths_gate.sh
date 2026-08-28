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
# ---------------------------------------------------------------------------
if [ -f "$ROOT/.cargo/config.toml" ] && grep -q "rustflags" "$ROOT/.cargo/config.toml"; then
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
# EXACT, not a floor, and not overridable. Pure parse of a tracked Dockerfile, so
# deterministic; when a COPY is added or removed, re-measure and change this line
# in the same commit.
#
# RE-MEASURED 2026-08-28: 19 - 18 context COPY sources plus the .cargo/
# builder-stage check. It was 15 (14 sources) on 2026-08-19, and before that a
# floor of 14 that had gone slack by one, so a tree that LOST a COPY source still
# cleared it - which is why this is exact.
#
# The four sources before that were the `migrate` stage the compose one-shot
# builds, added when the platform schema moved off the deleted
# `zeroship-platform-migrate` binary and onto the `zero-migrate` CLI: the pnpm
# store and package tree the CLI resolves through, the charter and table-owner
# registry it applies under, and its entrypoint wrapper.
#
# RE-MEASURED 2026-08-28 (later the same day): 29. The `sdks` stage gained eleven
# context sources when the `migrate` target was built for the FIRST time and did
# not work - `Cargo.toml` and `Cargo.lock` plus the addon's eight remaining local
# crates (its `cargo metadata` closure is nine, one of which was already copied),
# without which `napi build` cannot find a workspace root, and `db/migrations-ts`,
# which is now a pnpm workspace member. One source left the count in exchange: the
# migrate stage's corpus COPY became `COPY --from=sdks`, which is not a context
# source, because the corpus has to arrive carrying the `node_modules` link that
# resolves its `zero-migrate` import. 19 + 11 - 1 = 29, plus `libs/` (the root
# manifest's second members glob refuses to match nothing) = 30.
EXPECT_RAN=30
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -ne "$EXPECT_RAN" ]; then
  echo "  x COUNT: $RAN COPY sources checked, expected exactly $EXPECT_RAN." >&2
  echo "    Fewer means sources went missing from the parse - a smaller green is" >&2
  echo "    not a pass. More means a COPY was added; re-measure and bump this line." >&2
  rc=1
fi

gate_arms_finish || rc=1
exit $rc
