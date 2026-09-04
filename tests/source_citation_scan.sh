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
#      which ~89 are fine: `tests/net_grants_test.rs` cited inside
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
# Five categories are deliberately NOT defects, and a gate that flags them is
# wrong rather than strict:
#
#   * NEGATIVE citations, where the comment names a file precisely to say it
#     does not exist — "there's no top-level `tests/common.rs`", "ported from
#     the deleted `tests/http.rs`", "No such file exists in the engine
#     repository, and it CANNOT". The citation is the point of the sentence.
#   * UPSTREAM paths that belong to another project — `plugin-db` citing the
#     `sqlite-vec` crate's own `examples/simple-rust/demo.rs`.
#   * Paths that are DATA rather than pointers. `config-contract`'s inventory
#     scanner is fed in-memory sources keyed by an invented crate name, and its
#     raw-env half carries the sealed-module suffix it MATCHES paths against.
#     Both are values the code reads, not places a reader is being sent. The
#     tell: renaming the invented crate would satisfy the scan and change
#     nothing real.
#   * GENERATED build outputs, which exist only after `pnpm build` and are never
#     tracked. Naming one is correct - it is where the artifact really lands -
#     so the alternative would be to stop naming build outputs in comments,
#     which is worse. They are UNRESOLVABLE by construction here as of
#     2026-09-04 (see the resolution oracle below), which is the point: the gate
#     now gives the same verdict on a built tree and a fresh checkout, and each
#     such citation has to be ruled on once rather than silently passing
#     locally.
#   * DELETED SUBJECTS of claims that are still true history - a dated session
#     note recording a measurement taken against a file that has since been
#     removed. Repointing one does not fix a stale pointer; it says the
#     measurement was taken somewhere it was not.
#
# All five are listed in ALLOW below, by "file:citation" pair rather than by
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
#
# THE TWO SIDES OF A PAIR ARE NOT THE SAME KIND OF THING, and a bulk rename will
# get this wrong. The LEFT side is a path in the tree and moves when the file
# moves. The RIGHT side is the literal text the citing file contains, and must
# be edited only when that text is. The crate reorg's sweep rewrote both here,
# which silently unhooked the init.rs entry: its citation names the pre-reorg
# `crates/runtime/src/embed/websocket.js` inside a `git log --diff-filter=D`
# recipe for a DELETED file, so it can never resolve, and a "corrected"
# right-hand side simply stops matching and lets the citation be reported again.
#
# THE LIST GREW FROM 22 TO 43 PAIRS ON 2026-09-04, and the twenty-one are worth
# naming as groups rather than leaving as a wall. All of them were reported for
# the first time that day, when this gate was wired into CI - the crate-directory
# rename of 2026-08-26 (`105a75131`) had left 696 unresolvable citations and
# nothing ran the scan between the two dates. 663 of the 696 were repointed at
# the file that MOVED. These are the rest, and the split is deliberate: a moved
# file gets a new address, a DELETED one does not get a fake one.
#
#   * NEGATIVE, and the sentence needs the dead path. `id.rs` records that a
#     promised `tests/core_id_parity.rs` guard cannot exist here;
#     `migrate-sqlite/backend/mod.rs` says outright "there is no
#     `tests/sqlite_journal.rs` and there never was"; `raw_env.rs` names the
#     value a path CONSTANT held until it stopped matching - repointing that one
#     makes the sentence contradict itself, and this sweep did exactly that
#     before it was caught. The compose-gate and `cross_app_fk.rs` rows are the
#     same shape: each names a file to say it was deleted and what went unheld
#     with it.
#   * DELETED SUBJECT of a claim that is still true history. `ccda4bb42`
#     (2026-08-28) deleted the platform-migrate binary - `src/platform.rs`,
#     `src/platform/cluster_lock.rs`, `tests/platform_migrate.rs`. Four files
#     cite it for what it DID, and each was edited on 2026-09-04 to say the
#     target is gone rather than to point somewhere plausible. The two
#     `NOTES-s*.md` are dated session records: they measured against that file
#     when it existed, so repointing would falsify a measurement.
#   * FIXTURE DATA, not pointers. `ci_wiring_gate.sh` and `sync_claim_gate.sh`
#     build synthetic trees in `$tmp` and assert on them; `tests/x_gate.sh` and
#     `crates/zeroship-alpha/src/lib.rs` are inputs those gates WRITE. Renaming
#     them would satisfy the scan and change nothing real - the same tell the
#     `config-contract` rows below carry.
#   * AN ELISION. `inject_policy_mirror_gate.sh` spells one path with `.../` in
#     a two-column summary; the full form is in the same file, twice.
ALLOW="
crates/zeroship-auth/tests/common/mod.rs:tests/common.rs
crates/zeroship-runtime/tests/call_fetch_handler.rs:tests/http.rs
crates/zeroship-migrate-server/tests/typed_id_parity.rs:tests/core_id_parity.rs
crates/zeroship-runtime/src/core/init.rs:crates/runtime/src/embed/websocket.js
crates/zeroship-data-sqlite/src/session.rs:examples/simple-rust/demo.rs
tests/golden_path.sh:tests/m0_gate.sh
crates/zeroship-migrate-ir/src/id.rs:tests/core_id_parity.rs
crates/zeroship-migrate-sqlite/src/backend/mod.rs:tests/sqlite_journal.rs
crates/zeroship-config-contract/src/raw_env.rs:crates/core/src/config/env.rs
crates/zeroship-auth/src/headers.rs:tests/compose_port_exposure_gate.sh
docs/runbooks/deploy-server.md:tests/compose_port_exposure_gate.sh
tests/service_credential_boot_gate.sh:tests/compose_secret_strength_gate.sh
tests/lib/scratch_db.sh:crates/zeroship-migrate-adapter/src/platform/cluster_lock.rs
tests/run_doc_gate.sh:crates/plugin-db/src/cross_app_fk.rs
docs/feature-map.md:crates/zeroship-plugin-db/src/cross_app_fk.rs
docs/reference/db.md:crates/zeroship-plugin-db/src/cross_app_fk.rs
crates/zeroship-testkit/src/fingerprint.rs:crates/zeroship-migrate-adapter/src/platform.rs
db/migrations-ts/20260811000400_rate_limits_write_grants.ts:crates/auth/src/store/ratelimit.rs
db/migrations-ts/20260816000100_service_assertion_replay.ts:crates/zeroship-migrate-adapter/tests/platform_migrate.rs
db/migrations-ts/20260818000000_auth_principal_grants_select.ts:crates/zeroship-migrate-adapter/src/platform.rs
NOTES-s31.md:crates/zeroship-migrate-adapter/tests/platform_migrate.rs
NOTES-s40.md:crates/zeroship-migrate-adapter/src/platform.rs
NOTES-s40.md:crates/zeroship-migrate-adapter/tests/platform_migrate.rs
tests/ci_wiring_gate.sh:tests/other.sh
tests/ci_wiring_gate.sh:tests/x_gate.sh
tests/sync_claim_gate.sh:crates/zeroship-alpha/src/lib.rs
tests/inject_policy_mirror_gate.sh:sdks/vite-plugin/.../confined-system-shape.generated.ts
libs/compio-s3/tests/common/mod.rs:tests/common/env.rs
crates/zeroship-config-contract/src/raw_env.rs:tests/common/env.rs
crates/zeroship-config-contract/tests/raw_env_contract.rs:tests/common/env.rs
crates/zeroship-config-contract/src/inventory.rs:crates/alpha/src/config.rs
crates/zeroship-config-contract/src/inventory.rs:crates/demo/src/config.rs
crates/zeroship-config-contract/src/inventory.rs:crates/demo/src/main.rs
crates/zeroship-config-contract/src/inventory.rs:crates/x/src/lib.rs
tests/bench_platform.sh:examples/bench/dist/server/index.js
tests/e2e-browser/src/dev-server.ts:sdks/vite-plugin/dist/cli/migrate-dev.js
tests/e2e_dev_vs_deployed_stream.sh:sdks/vite-plugin/dist/dev-bootstrap.js
tests/golden_path.sh:sdks/vite-plugin/dist/index.js
tests/lib/binary_freshness.sh:sdks/vite-plugin/dist/index.js
AGENTS.md:sdks/db/dist/internal.js
CONTRIBUTING.md:sdks/db/dist/internal.js
ISSUES.md:sdks/bootstrap/dist/runtime-entry.js
docs/build-and-deploy-golden-path.md:examples/workflows-order/dist/index.js
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

# THE RESOLUTION ORACLE IS `git ls-files`, NOT THE FILESYSTEM, and that is the
# whole of the fix for the divergence described below. A citation resolves iff
# its target is TRACKED. It is not asked whether the file happens to be on this
# disk right now.
#
# Both are needed and they are different questions. `git ls-files` was already
# the CORPUS (see the note further down): which files get SCANNED. It was never
# the oracle for whether a scanned citation RESOLVES, and that half stayed on
# `-e`, which reads the working tree. So the gate scanned the commit and judged
# the disk, and the two disagree on exactly one thing - generated build output.
#
# MEASURED 2026-09-04: five targets resolved on a built tree that a fresh
# checkout does not have (`sdks/{bootstrap,db}/dist/*`, `sdks/vite-plugin/
# dist/*`). All five were already covered pair-by-pair in ALLOW, so the CI
# verdict was the same as the local one this time - but only by the grace of
# somebody having listed them. The SIXTH such citation, written tomorrow, would
# have passed here and failed in CI, which is how this gate spent 2026-08-10
# red in CI and green on every developer machine.
#
# Asking git closes that by construction rather than by allowlist: a build
# output is untracked, so it is unresolvable HERE too, immediately, on the
# machine of whoever wrote it. The gate can no longer pass locally and fail in
# CI, because it no longer has access to the fact the two environments differ
# on. The warning that used to report the divergence is gone with it - there is
# nothing left for it to report.
TRACKED_SET="$(mktemp)"
trap 'rm -f "$TRACKED_SET"' EXIT
git ls-files | LC_ALL=C sort -u > "$TRACKED_SET"
if [ ! -s "$TRACKED_SET" ]; then
  echo "::error::git ls-files returned nothing - every citation would report unresolvable"
  exit 1
fi

resolves() {
  # A leading ./ never appears in git's spelling; normalise before asking.
  LC_ALL=C grep -qxF "${1#./}" "$TRACKED_SET"
}

# Told apart from "does not exist at all" because the two need different
# actions: an untracked-but-present target is a build output or a scratch file,
# and the citation has to move to ALLOW or point at the SOURCE that produces it.
on_disk_but_untracked() {
  [ -e "${1#./}" ]
}

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

# THE CORPUS IS `git ls-files`, not the filesystem, for both passes below. A
# working tree carries files git does not: untracked scratch docs, planted
# probes, build output. Scanning the filesystem makes the verdict depend on
# what happens to be lying around locally rather than on the commit under
# test - two people on the same commit can get different answers, and it errs
# in both directions (a stray untracked doc turns a clean commit red; a
# gitignored `dist/` turns a broken one green). `git ls-files` is exactly what
# a fresh checkout has, which is what CI has, so scanning it is what makes the
# gate reproducible AND CI-representative. (MEASURED 2026-08-20: an untracked,
# undated `docs/pilot/e2e-scenarios.md` citing a genuinely-deleted
# `tests/supabase_deploy_e2e.sh` flips this gate red with the filesystem as
# corpus and has no effect once the corpus is `git ls-files`.)

while IFS= read -r line; do
  src="${line%%:*}"
  cite="${line#*:}"
  [ -n "$cite" ] || continue
  found=$((found + 1))

  if resolves "$cite"; then continue; fi
  base="$(pkg_root "$src")"
  # "$base/$cite" verbatim, matching the test on the line above. An earlier
  # draft wrote "${base#./}$cite" and silently dropped the separator, turning
  # crates/auth + tests/x.rs into crates/authtests/x.rs - 101 tracked files
  # reported as untracked. The join has to be the same string the test used.
  if resolves "$base/$cite"; then continue; fi

  if printf '%s' "$ALLOW" | grep -qxF "$src:$cite"; then
    allowed=$((allowed + 1))
    continue
  fi

  if on_disk_but_untracked "$cite" || on_disk_but_untracked "$base/$cite"; then
    echo "::error::$src cites $cite, which is in your working tree but NOT TRACKED - a fresh checkout, which is what CI has, does not have it. Cite the source that produces it, or add the pair to ALLOW."
  else
    echo "::error::$src cites $cite, which resolves neither from the repo root nor from $base/"
  fi
  missing=$((missing + 1))
done < <(git ls-files -- $ROOTS \
    | grep -E '\.(rs|ts|tsx|js|toml|sh)$' \
    | grep -v -e '/wpt/' -e '/dist/' -e '/node_modules/' -e '/target/' \
    | grep -vx -e 'tests/source_citation_scan.sh' -e 'tests/source_citation_selftest.sh' \
    | xargs -d '\n' -r grep -oP "$PAT" -- 2>/dev/null | sort -u)

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
  #
  # realpath -m, because a doc-relative hit spells out as
  # `docs/architecture/../../crates/x.rs` and `git ls-files` only ever lists
  # NORMALISED paths - so the unnormalised form matches nothing and every one of
  # the 79 doc hits would report unresolvable. Asking git means spelling the
  # path the way git spells it. `-m` so a path whose target is absent still
  # normalises rather than failing.
  case "$cite" in
    ../*) doc_target="$(realpath -m --relative-to=. "$(dirname "$src")/$cite")" ;;
    *)    doc_target="$cite" ;;
  esac
  if resolves "$doc_target"; then continue; fi

  if printf '%s' "$ALLOW" | grep -qxF "$src:$cite"; then
    doc_allowed=$((doc_allowed + 1))
    continue
  fi

  if on_disk_but_untracked "$doc_target"; then
    echo "::error::$src cites $cite, which is in your working tree but NOT TRACKED - a fresh checkout, which is what CI has, does not have it. Cite the source that produces it, or add the pair to ALLOW."
  else
    echo "::error::$src cites $cite, which does not resolve"
  fi
  missing=$((missing + 1))
done < <( {
    git ls-files -- docs \
      | grep -E '\.md$' \
      | grep -v -e '/node_modules/' -e '/target/' \
      | grep -Ev "^docs/($(printf '%s' "$DOC_EXCLUDED" | tr ' ' '|'))/" \
      | grep -Ev '/[0-9]{4}-[0-9]{2}-[0-9]{2}-[^/]*\.md$' \
      | xargs -d '\n' -r grep -oP "$DOC_PAT" -- 2>/dev/null
    # The repo-root pages are the most-read docs in the tree and belong to no
    # docs/ subdirectory, so they need naming separately or they are missed.
    git ls-files -- '*.md' | grep -v '/' | xargs -d '\n' -r grep -oP "$DOC_PAT" -- 2>/dev/null
  } | sort -u )

echo "doc citations checked: $doc_found across docs/ + root *.md, excluding $DOC_EXCLUDED and dated records (allowed: $doc_allowed, unresolvable: $((missing - src_missing)))"
# A "this run is not CI-representative" warning used to be printed here, listing
# the citation targets that had resolved only because this working tree was
# built. It is GONE, and its absence is the point: with `resolves()` asking git
# instead of the filesystem, no such target can resolve any more, so the warning
# could only ever print an empty list. A check that cannot fire is worse than no
# check - it reads as coverage. The condition it warned about is now a plain
# unresolvable citation, reported above with the message that says which of the
# two fixes applies.

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
