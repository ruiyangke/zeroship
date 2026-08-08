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
# Exit 0 when every citation resolves or is allowed; 1 otherwise.

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

ROOTS="crates sdks libs tests db examples"
PAT='(?<![A-Za-z0-9_/.-])(?:crates|sdks|libs|db|examples|tests)/[A-Za-z0-9_./-]+\.(?:tsx|rs|ts|js|sh|toml)(?![A-Za-z0-9])'

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

# The degenerate-case guard. This step counts what is MISSING and passes at zero,
# so a pattern that stopped matching, a wrong root list, or a grep that declined
# to read anything all report a clean tree. Counting what was checked is what
# separates those from a tree that is genuinely clean. Left loose on purpose:
# the total moves with any ordinary comment edit.
if [ "$found" -lt 200 ]; then
  echo "::error::only $found source citations found (expected ~376); the extraction returned nothing usable, so a clean result would mean nothing"
  exit 1
fi

[ "$missing" -eq 0 ]
