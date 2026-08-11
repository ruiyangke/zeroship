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

if [ "${#SRCS[@]}" -eq 0 ]; then
  echo "  x REFUSED: parsed ZERO context COPY sources out of $DF." >&2
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
  if [ -z "$BUILDER_BODY" ]; then
    echo "  x REFUSED: no 'AS builder' stage body found - this check's premise is gone." >&2
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

# Floor counts assertions that RAN, not that PASSED: a mutation moves an outcome
# BETWEEN those columns, so only a LOST assertion drops the sum.
# MEASURED 2026-08-11: 13 context COPY sources + the .cargo/ builder-stage check.
MIN_RAN="${DOCKERFILE_COPY_MIN_RAN:-14}"
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$MIN_RAN" ]; then
  echo "  x FLOOR: only $RAN COPY sources checked, expected at least $MIN_RAN." >&2
  echo "    Sources went missing from the parse - a smaller green is not a pass." >&2
  rc=1
fi
exit $rc
