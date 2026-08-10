#!/usr/bin/env bash
#
# source_citation_scan.sh — resolve in-comment citations that name a SOURCE path.
#
# The sibling check in CI resolves citations that name a `docs/**.md` file. This
# one covers the other half: a comment that points at `crates/foo/src/bar.rs` or
# `tests/baz.rs`. Those rot the same way and are read the same way — as "the
# thing that justifies this is over there" — so a citation naming a file that
# moved stops the reader looking and nothing notices.
#
# The resolution rules below were MEASURED against this tree (376 citations),
# not assumed. Each one exists because getting it wrong produced a confidently
# wrong answer:
#
#   1. TWO BASES, not one. A repo-root-only check reports 106 unresolvable, of
#      which ~89 are fine: `tests/admin_handlers_test.rs` cited inside
#      `crates/control/src/api.rs` means `crates/control/tests/...`, resolved
#      against the nearest ancestor holding a Cargo.toml or package.json.
#      Distribution: 235 repo-relative, 124 package-relative, 17 neither.
#
#   2. KEEP THE FILENAME. `grep -rhoP` (with -h) drops it, and the package base
#      cannot be computed without knowing which file the citation is in. The
#      output shape has to carry the field the question depends on.
#
#   3. THE EXTENSION ALTERNATION NEEDS A RIGHT BOUNDARY. `(rs|ts|js|tsx)` in
#      that order matches `ts` inside `Input.tsx` and `js` inside `.stack.json`,
#      manufacturing dead citations out of files that exist. That accounted for
#      4 of the 17 above. `tsx` precedes `ts`, and a trailing `(?![A-Za-z0-9])`
#      closes the rest.
#
# Two categories are deliberately NOT defects, and a gate that flags them is
# wrong rather than strict:
#
#   * NEGATIVE citations, where the comment names a file precisely to say it
#     does not exist — "there's no top-level `tests/common.rs`", "ported from
#     the deleted `tests/http.rs`", "No such file exists in the engine
#     repository, and it CANNOT". The citation is the point of the sentence.
#   * UPSTREAM paths that belong to another project — `plugin-db` citing the
#     `sqlite-vec` crate's own `examples/simple-rust/demo.rs`.
#
# Both are listed in ALLOW below, by "file:citation" pair rather than by
# citation alone, so the same path cited wrongly somewhere else is still caught.
#
# This file and its self-test are excluded from the scan. Both spell out paths
# that deliberately do not exist - examples here, a planted probe there - so a
# self-scan reports them as broken citations. That is a tool describing paths,
# not citing them. The cost is that neither can police its own comments, which is
# the right trade only because these two are the files in the tree whose job is
# to contain path spellings.
#
# Worth knowing before adding a third: this trap has bitten three times. The
# scanner flagged itself; a comment explaining why a bare path is wrong was
# flagged for containing the bare path; and the self-test was flagged for naming
# its own probe. Any file whose subject is paths will need the same treatment,
# and the alternative - building the strings at runtime so they never appear
# literally - makes those files harder to read for no gain.
#
# BOTH DIRECTIONS ARE COVERED HERE, and until 2026-08-10 only one was.
#
# The scan above reads SOURCE files. Its sibling in CI reads source files too,
# for citations naming a `docs/**.md` file. Between them they covered every
# arrow that STARTS in source. Nothing read the doc trees, so an arrow that
# starts in a doc and points at source - the form the task router, the feature
# map, and every architecture write-up are built out of - had no gate at all.
# A doc could name `crates/foo/src/bar.rs` forever after that file moved or was
# deleted. Measured on the day this was added: 116 unresolvable of 943 in the
# enforced doc corpus, dominated by two whole trees that had MOVED (the
# `compio-*` drivers `crates/` -> `libs/`, the migration engine into
# `third_party/zero-migrate/`) and one that was EXTRACTED to a sibling
# repository (`crates/sandbox*`). Every one of them still read as a live
# pointer.
#
# WHICH DOCS ARE IN SCOPE, and why the list is a set of EXCLUSIONS.
#
# The question this gate asks is "does this doc's claim about the tree as it
# stands resolve". Only documents that make such a claim can answer it. Four
# kinds of document do not, and flagging them would be wrong rather than
# strict - the citation is doing a different job:
#
#   * PRE-SHIP PROPOSALS and PLANS (`proposals/`, `superpowers/`) name the file
#     they intend to create. `docs/AGENTS.md` says so of both. Requiring those
#     to exist would forbid writing a design before building it.
#   * DATED RECORDS - `reviews/` (audit reports), `decisions/` (ADRs, which
#     `docs/AGENTS.md` calls immutable once landed), and any `YYYY-MM-DD-*.md`
#     elsewhere. These describe a past HEAD on purpose. `docs/pilot/`'s decision
#     log cites a zero-byte test file precisely to record that it was deleted,
#     and one citation there is commit-qualified (`<sha>:path`). Repointing
#     those at today's tree would not fix a stale pointer, it would falsify the
#     record.
#   * `archive/` - "Frozen documents kept for history ... nothing in here is
#     load-bearing" (`docs/AGENTS.md`).
#   * `research/` - competitive landscape, whose paths belong to other projects.
#
# Stated as EXCLUSIONS rather than as a list of included roots, and that
# direction is load-bearing. A new doc directory is then covered by default; the
# failure mode of the other direction is a whole tree silently outside the gate,
# which is the same defect this file exists to catch. Removing coverage costs a
# deliberate edit here, gaining it is free. Each excluded name is asserted to
# EXIST for the same reason the roots are: a rename would otherwise widen the
# scan by accident and blame the wrong change.
#
# TWO PARSING DIFFERENCES from the source scan, both measured:
#
#   * `../` PREFIXES. Docs cite relatively - `[dispatch](../../crates/gateway/
#     src/router/dispatch.rs)` - and the anchoring lookbehind rejects a match
#     that starts after a `/`, so without an explicit `(?:\.\./)*` in the
#     pattern those citations are invisible. 80 of the 943 carry one.
#   * NO PACKAGE BASE, doc-relative instead. The source scan resolves a bare
#     `tests/foo.rs` against the nearest Cargo.toml, because a comment inside a
#     crate is read from inside that crate. A doc has no package context, so a
#     bare citation is repo-relative and a `../`-prefixed one resolves against
#     the doc's own directory - which is also what makes the markdown link
#     work. Measured before choosing: resolving the `../` forms doc-relatively
#     and resolving them by stripping the prefix and using the repo root agree
#     on all 80 today, so the stricter rule is currently free and will catch a
#     link written with the wrong number of `../`.
#
# Exit 0 when every citation resolves or is allowed; 1 otherwise.

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

ROOTS="crates sdks libs tests db examples"
PAT='(?<![A-Za-z0-9_/.-])(?:crates|sdks|libs|db|examples|tests)/[A-Za-z0-9_./-]+\.(?:tsx|rs|ts|js|sh|toml)(?![A-Za-z0-9])'

# The doc half. Same extension alternation and the same right boundary; the
# `(?:\.\./)*` is the one addition. See the two parsing differences above.
DOC_PAT='(?<![A-Za-z0-9_/.-])(?:\.\./)*(?:crates|sdks|libs|db|examples|tests)/[A-Za-z0-9_./-]+\.(?:tsx|rs|ts|js|sh|toml)(?![A-Za-z0-9])'
# Doc trees that do not describe the tree as it stands. See the four kinds above.
DOC_EXCLUDED="archive decisions proposals superpowers reviews research"

# file:citation pairs that are correct as written. See the two categories above.
ALLOW="
crates/auth/tests/common/mod.rs:tests/common.rs
crates/runtime/tests/call_fetch_handler.rs:tests/http.rs
crates/migrated/tests/typed_id_parity.rs:tests/core_id_parity.rs
crates/runtime/src/core/init.rs:crates/runtime/src/embed/websocket.js
crates/plugin-db/src/backend/sqlite/session.rs:examples/simple-rust/demo.rs
"

# A corpus check the count cannot do: a renamed root silently stops being
# scanned, and the total stays plausible because the other roots still yield.
for r in $ROOTS; do
  if [ ! -d "$r" ]; then
    echo "::error::source-citation scan lists $r/, which does not exist - the scan silently stopped covering it"
    exit 1
  fi
done

# The mirror of that check for the doc pass, and it guards the opposite error.
# The roots above are what IS scanned, so a rename makes the scan smaller. The
# names below are what is NOT scanned, so a rename makes it BIGGER - a frozen
# tree quietly re-enters the corpus and the failures land on whoever renamed it
# rather than on whoever wrote the citations. Both are silent; assert both.
for d in $DOC_EXCLUDED; do
  if [ ! -d "docs/$d" ]; then
    echo "::error::doc-citation scan excludes docs/$d/, which does not exist - the exclusion is stale and the scan's scope moved without anyone deciding to"
    exit 1
  fi
done

# The root pages are the third arm of the doc pass and the one the count floor
# cannot defend: they carry ~40 of ~900 doc citations, so losing all of them
# moves the total by less than an ordinary week of doc edits. `AGENTS.md` is the
# repo's documented landing page and the single largest source of them.
if [ ! -f AGENTS.md ]; then
  echo "::error::doc-citation scan reads the repo-root *.md pages, but AGENTS.md is not there - the landing page moved and the root arm is scanning less than it claims"
  exit 1
fi

pkg_root() {
  local d
  d="$(dirname "$1")"
  while [ "$d" != "." ] && [ "$d" != "/" ]; do
    if [ -f "$d/Cargo.toml" ] || [ -f "$d/package.json" ]; then
      printf '%s\n' "$d"
      return
    fi
    d="$(dirname "$d")"
  done
  printf '.\n'
}

found=0
allowed=0
missing=0

while IFS= read -r line; do
  src="${line%%:*}"
  cite="${line#*:}"
  [ -n "$cite" ] || continue
  found=$((found + 1))

  [ -e "$cite" ] && continue
  base="$(pkg_root "$src")"
  [ -e "$base/$cite" ] && continue

  if printf '%s' "$ALLOW" | grep -qxF "$src:$cite"; then
    allowed=$((allowed + 1))
    continue
  fi

  echo "::error::$src cites $cite, which resolves neither from the repo root nor from $base/"
  missing=$((missing + 1))
done < <(grep -roP "$PAT" \
    --include='*.rs' --include='*.ts' --include='*.js' --include='*.tsx' \
    --include='*.toml' --include='*.sh' \
    --exclude-dir=wpt --exclude-dir=dist \
    --exclude-dir=node_modules --exclude-dir=target \
    --exclude=source_citation_scan.sh \
    --exclude=source_citation_selftest.sh \
    $ROOTS 2>/dev/null | sort -u)

# Printed on success too. A number nobody sees until the gate has already failed
# cannot warn anyone, and this is the channel where a false green differs from a
# true one.
echo "source citations checked: $found across $ROOTS (allowed: $allowed, unresolvable: $missing)"
src_missing=$missing

# --- The doc half: arrows that START in a doc and point at source. -----------

doc_found=0
# Counted separately from the source pass, not folded into it: the two summary
# lines are read as a per-corpus report, and a shared counter makes the second
# line print the first line's allowances as if they were its own.
doc_allowed=0

while IFS= read -r line; do
  src="${line%%:*}"
  cite="${line#*:}"
  [ -n "$cite" ] || continue
  doc_found=$((doc_found + 1))

  # A `../` citation is resolved against the doc that wrote it, not the repo
  # root: that is what the prefix means, and it is what makes the link work.
  # A bare one is repo-relative - a doc has no package to be relative to.
  case "$cite" in
    ../*) [ -e "$(dirname "$src")/$cite" ] && continue ;;
    *)    [ -e "$cite" ] && continue ;;
  esac

  if printf '%s' "$ALLOW" | grep -qxF "$src:$cite"; then
    doc_allowed=$((doc_allowed + 1))
    continue
  fi

  echo "::error::$src cites $cite, which does not resolve"
  missing=$((missing + 1))
done < <( { grep -roP "$DOC_PAT" --include='*.md' \
      --exclude-dir=node_modules --exclude-dir=target \
      docs 2>/dev/null \
    | grep -Ev "^docs/($(printf '%s' "$DOC_EXCLUDED" | tr ' ' '|'))/" \
    | grep -Ev '/[0-9]{4}-[0-9]{2}-[0-9]{2}-[^/]*\.md:'
    # The repo-root pages are the most-read docs in the tree and belong to no
    # docs/ subdirectory, so they need naming separately or they are missed.
    grep -oP "$DOC_PAT" --include='*.md' ./*.md 2>/dev/null | sed 's|^\./||'
  } | sort -u )

echo "doc citations checked: $doc_found across docs/ + root *.md, excluding $DOC_EXCLUDED and dated records (allowed: $doc_allowed, unresolvable: $((missing - src_missing)))"

# The degenerate-case guard, one per corpus. This step counts what is MISSING
# and passes at zero, so a pattern that stopped matching, a wrong root list, or
# a grep that declined to read anything all report a clean tree. Counting what
# was checked is what separates those from a tree that is genuinely clean. Left
# loose on purpose: both totals move with any ordinary comment or doc edit.
#
# Separate floors, not one on the sum. A sum floor cannot see either corpus
# going to zero on its own, and the doc corpus is the larger of the two - it
# could vanish entirely and the combined number would still clear a floor set
# for both.
if [ "$found" -lt 200 ]; then
  echo "::error::only $found source citations found (expected ~577); the extraction returned nothing usable, so a clean result would mean nothing"
  exit 1
fi
if [ "$doc_found" -lt 400 ]; then
  echo "::error::only $doc_found doc citations found (expected ~878); the extraction returned nothing usable, so a clean result would mean nothing"
  exit 1
fi

[ "$missing" -eq 0 ]
