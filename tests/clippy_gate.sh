#!/usr/bin/env bash
# The clippy gate: lint the whole workspace, and PROVE the whole workspace was
# linted.
#
# WHY THIS EXISTS
#
# Main's clippy job has gone red unnoticed twice in a week. Both times the fix
# was trivial and both times nobody saw it for days, for a structural reason:
# the per-crate `cargo test` runs that everyone does locally never invoke
# clippy, so CI was the only thing that did, and CI's clippy result is one tick
# among many.
#
#   - crates/core/src/config/env.rs lost its `#[allow(clippy::disallowed_methods)]`
#     and produced three deny-level errors. clippy.toml's own header names that
#     file as one of exactly two sanctioned raw-environment boundaries, so the
#     fix was a dropped attribute, not a design question.
#   - a later break hit crates/core/src/config/test_overlay.rs,
#     crates/auth/src/oidc/issuer.rs and crates/control/src/oauth_clients.rs.
#
# THE SECOND ONE CARRIES THE LESSON THIS SCRIPT IS BUILT AROUND. The `core`
# failure ABORTED `cargo clippy --workspace` before `auth` or `control` were
# ever linted. A run that reported "0 errors in auth" had not found auth clean -
# it had never reached auth. That was only noticed because a different worktree
# reported zero for auth while the first reported errors there.
#
# A tool that stops early and a tool that finds nothing print the same thing.
# Everything below exists to make those two outcomes print differently.
#
# WHAT COVERAGE EVIDENCE THIS USES, AND WHY THAT ONE
#
# The requirement is that a green here means every crate was actually linted,
# not that nothing was said about the crates that were reached. Three shapes
# were available:
#
#   (a) a crate COUNT               - rejected. A count is a proxy, and a proxy
#                                     that drifts gets its floor lowered until
#                                     it is a rubber stamp. tests/test_target_census_gate.sh
#                                     has the long version of that argument.
#   (b) per-package `cargo clippy -p X`
#                                   - rejected, and the reason is not cost.
#                                     `-p X` resolves features over X's subgraph
#                                     ALONE, so the unified feature set differs
#                                     from what `--workspace` builds. The gate
#                                     would then lint a different configuration
#                                     from the one CI lints, which is the exact
#                                     drift this script exists to prevent, and it
#                                     would rebuild dependencies under a second
#                                     feature resolution besides.
#   (c) PER-TARGET ARTIFACT ACCOUNTING out of one `--workspace` run   <- chosen.
#
# (c) keeps the invocation byte-identical to what CI ran before, and reads the
# coverage out of cargo's own `--message-format=json` stream. Cargo emits a
# `compiler-artifact` message for every unit it finishes - including cached ones,
# which carry `"fresh": true` - and emits NOTHING for a unit it never scheduled.
# So the set of targets that were linted is directly observable, and the
# expectation it is checked against comes from `cargo metadata`, which reads
# Cargo.toml rather than the run being audited. A gate that derives its
# expectation from its own subject agrees with itself by construction; this one
# cannot.
#
# `--keep-going` IS DELIBERATELY ABSENT, and that is a measurement rather than an
# oversight. It looks like the obvious mitigation - "keep scheduling units in
# crates that do not depend on the one that failed" - so it was tried, twice,
# each time against a control differing in that flag alone:
#
#   on main as it stands (two independent crates failing):
#     with --keep-going    136 workspace artifacts, errors in 2 packages
#     without              136 workspace artifacts, errors in the same 2
#
#   with a deny lint planted in crates/gateway:
#     with --keep-going    107 workspace artifacts
#     without              107 workspace artifacts
#
# Identical. In the second run NINE zeroship-core test targets - which do not
# depend on the gateway in any direction - produced no artifact under BOTH
# arrangements, having produced one in the green run minutes earlier. Cargo
# stops draining its queue when a unit fails, and this flag did not change that
# on this tree.
#
# It could not have fixed the failure that motivated this gate anyway: there,
# crates/core failed and auth and control are DOWNSTREAM of core, so no amount
# of keeping going reaches them. The audit below is what works, because it
# reports the shortfall instead of trying to prevent it.
#
# A PREFLIGHT, BEFORE ANY OF IT
#
# Two build inputs of this workspace are gitignored and produced by commands
# cargo knows nothing about (`pnpm build`, `setup-wpt.sh`). Without them
# crates/runtime does not compile and the run aborts having linted almost
# nothing. The gate REFUSES with exit 2 in that case rather than returning a
# narrower green. See the preflight block below for how the list is derived.
#
# FOUR ARMS
#
#   1. LINT VERDICT. Any diagnostic at level "error" owned by a workspace
#      package is a failure. Errors are SPLIT by whether their code is a
#      `clippy::*` lint or anything else, because those are different problems:
#      a lint means the code needs fixing, while a rustc error means the crate
#      could not be linted AT ALL and every other verdict about it is void.
#
#   2. PACKAGE COVERAGE. Every workspace member `cargo metadata` knows about
#      must have produced at least one artifact. A member with no artifact and
#      no error is the #64 failure exactly: not clean, NOT REACHED.
#
#   3. TARGET COVERAGE. Every target those members declare, whose
#      `required-features` the run actually enabled, must have produced an
#      artifact. This is what arm 2 cannot see: a crate that keeps its lib
#      linted while a test target quietly stops being built still passes arm 2.
#      The enabled feature set is resolved by asking `cargo metadata` for it
#      under the SAME `--features` flags the lint run uses, so the two cannot
#      disagree about which targets were in scope.
#
#   4. FEATURE COVERAGE. Every non-`default` feature the workspace members
#      declare must have been ENABLED by the run. This is the arm that was
#      missing until 2026-08-20, and its absence is arm 3's own failure mode one
#      level out - see the next section.
#
# WHY ARM 4 EXISTS: A TARGET THAT DOES NOT EXIST CANNOT BE "UNLINTED"
#
# Arms 2 and 3 audit per-package and per-target coverage OF ONE FEATURE
# RESOLUTION. A target whose `required-features` that resolution does not
# satisfy is not counted as unlinted - it is filtered out of the expectation by
# the `select` that builds `expected.tsv`, so it is not in arm 3's bookkeeping
# at all. The gate then reports full coverage of a workspace it has silently
# made smaller, which is the same "a tool that stopped early and a tool that
# found nothing print the same thing" confusion the whole file is built around.
#
# MEASURED on main at 6d3ca2d84, with the two-feature list this gate used to
# carry (`zeroship-control/live-db-tests,zeroship-migrated/live-db-tests`):
#
#   declared non-default features across the 30 members   28
#   features that resolution enabled                      10
#   features it did not                                   18
#   DECLARED TARGETS dropped by the required-features
#     filter, and therefore absent from arm 3             10
#
# One of those ten was `zeroship-migrate-adapter`'s `platform_migrate` test.
# Linting it needs only `--features platform-cli`, and doing so reports ELEVEN
# deny-level `clippy::await_holding_lock` errors plus, in the sibling
# `zeroship-platform-migrate` bin the same feature gates, one
# `clippy::items_after_test_module`. Twelve deny-level errors in a workspace
# this gate had just called clean. None of them are new; the gate had never
# compiled the code they are in.
#
# THE FIX IS NOT "ADD platform-cli TO THE LIST". That closes one hole and leaves
# the mechanism blind, and a hand-maintained list of features is a census - the
# shape this repo's gates keep failing at. So the run enables `--all-features`
# and arm 4 checks, against `cargo metadata` resolved under the same flag, that
# every declared feature really came out enabled. A feature added to any crate
# tomorrow is linted without anyone touching this file; a feature that CANNOT be
# enabled is NAMED, with the targets it gates, instead of vanishing.
#
# WHAT IT COSTS, measured on this workspace: see the CARGO_ARGS comment below.
#
# READING A RED RUN: THE COVERAGE NUMBER IS ONLY STABLE WHEN IT IS GREEN
#
# Once a unit fails, how much of the rest cargo had already got through depends
# on what was warm in target/. MEASURED on the SAME source tree, main as it
# stands, two runs an hour apart:
#
#   after a full build, almost everything fresh    131 of 134 targets linted
#   with several crates dirty from other work      100 of 134 targets linted
#
# Same three lints both times. So on a red run, arm 3's list is a snapshot of
# where cargo stopped, not a property of the tree - and on CI, where nothing is
# warm, expect it to be long. Fix the errors and re-run; a green run linted
# every target by construction, because nothing stopped it. That asymmetry is
# fine for a gate: it only ever has to be trustworthy about GREEN.
#
# WHAT THIS CANNOT SEE
#
# Read this before treating three green arms as completeness. They are checks on
# the RUN, and the run's scope is set by the FEATURES constant below.
#
#   - THE `off` ARM OF EVERY FEATURE. `--all-features` compiles the code behind
#     `#[cfg(feature = "x")]` for every x, and therefore compiles the code behind
#     `#[cfg(not(feature = "x"))]` for NONE of them. crates/runtime's WebSocket
#     polyfill fallback and plugin-db's no-backend arms are real code this run
#     never sees. Covering both arms of n features needs 2^n runs; this gate
#     covers one point of that space and arm 4 states which point. A
#     `not(feature)` arm is a review question, not a gate question.
#   - Code behind a non-feature `cfg` this build does not enable, or a target
#     platform this machine is not. Clippy lints what it compiles.
#   - A target deleted from Cargo.toml. It leaves both the expected and the
#     observed set in the same commit, so the comparison stays balanced and
#     silent. Deletions are a review question, not a gate question.
#   - Whether the lint LEVELS are right. `pedantic` and `nursery` are warn on
#     purpose (root Cargo.toml `[workspace.lints.clippy]`); this gate reports
#     the warning count and gates on none of it.
#
# USAGE
#
#   tests/clippy_gate.sh              lint and audit (what CI runs)
#   tests/clippy_gate.sh --preflight-only
#                                     answer "can this machine lint at all?"
#                                     without spending a workspace build on it
#   tests/clippy_gate.sh --audit-only <json>
#                                     audit a previously captured json stream
#                                     without re-running cargo
#
# EXIT CODES
#   0  every target linted, no deny-level errors
#   1  a lint failed, a target went unlinted, or a declared feature was not
#      enabled
#   2  the gate could not run, or could not be trusted to have measured anything
#
# REDIRECTING THE CORPUS, and why each group is all-or-nothing.
#
#   --metadata <file> --min-members <n> --min-targets <n> --min-features <n>
#                        pre-captured `cargo metadata` json (skips cargo), plus
#                        the bounds that json's own arms are held to
#   --src-roots "<dirs>" --min-literals <n>
#                        directories the include_str! preflight scans, plus the
#                        bound its arm is held to
#
# These were ambient `ZS_CLIPPY_*` environment variables until 2026-08-20, and
# the floors that went with them were constants. That pairing was the bug: a
# floor is a claim about a corpus, and tests/clippy_gate_selftest.sh drives every
# arm below over deliberately tiny fixtures - a three-package metadata blob, a
# two-literal source root - through this same code path. The floors had
# therefore been set to the FIXTURE's size (3 members, 4 targets, 1 literal)
# while the real workspace offers 30, 146 and 239. Each arm passed on the real
# tree having ruled on a tenth to a two-hundredth of it, which is the vacuity
# the arm contract exists to catch, arrived at by the exact repair the contract
# warns against: lower the floor until it stops firing.
#
# So the caller that supplies a corpus supplies its bounds too, in the same
# invocation, and a group given in part is a refusal. Argv rather than the
# environment because an ambient variable can be set by something that is not
# the invocation, and then the real run gates against a fixture's numbers with
# nothing in the command line to show it.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Per-arm anti-vacuity accounting. Every guard this gate already had for "the
# instrument read nothing" is expressed through it below, so a refusal names
# WHICH enumeration collapsed instead of only which gate did.
#
# EVERY FLOOR BELOW IS A FUNCTION OF THE CORPUS, NOT A CONSTANT. Arm 2's is
# DERIVED from the metadata in hand - it asserts that every workspace member the
# metadata declares got a verdict, which scales to whatever corpus arrives and
# is strictly stronger than any fixed number. The other two cannot be derived
# (nothing in the input states how many include_str! literals or feature-enabled
# targets there ought to be), so their bounds are declared by whoever supplies
# the corpus, defaulting to this workspace's measured numbers. See the header.
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init clippy

# The feature selection lives HERE and nowhere else, so the local run and the CI
# run cannot lint different sets of targets, and `cargo metadata` below is asked
# with the SAME flag so the two cannot disagree about what was in scope.
#
# It was `--features zeroship-control/live-db-tests,zeroship-migrated/live-db-tests`
# until 2026-08-20, carried over verbatim from the ci.yml step this replaced:
#
#   `--all-targets` only reaches targets whose `required-features` are
#   satisfied, so the 45 live-database test files would silently stop being
#   linted the moment they were gated. Naming the feature keeps exactly the set
#   that was linted before still linted.
#
# That reasoning is right and the list was the wrong instrument for it. It named
# the two features somebody had noticed; the workspace declared 28, and the 18 it
# omitted gated 10 declared targets and, in one of them, 12 standing deny-level
# errors. A list of features somebody remembered to add is a census, and this
# repo's gates keep failing that way. `--all-features` needs nobody to remember.
#
# WHAT IT COSTS. Nothing at steady state: this is still ONE invocation resolving
# ONE feature set into ONE target directory, so it is not a matrix and does not
# multiply CI time or disk the way a second `--features` pass would. What it does
# add is the extra work that set implies - the ~10 targets the old list filtered
# out, the ~290 `#[cfg(feature = "test-helpers")]` sites in plugin-db that now
# compile, and the type-mapping dependencies compio-postgres's `with-*` features
# pull in (jiff, geo-types, cidr, eui48, bit-vec, smol_str, time). MEASURED on
# this workspace, cold target dir, in the commit that introduced this line:
# see docs below the arm-4 block for the before/after target counts.
FEATURE_ARGS=(--all-features)

# Not `-D warnings`. The workspace grades its lints in the root Cargo.toml
# `[workspace.lints]` table - `clippy::all` deny, `pedantic` and `nursery` warn -
# and flattening that grading would promote every pedantic and nursery warning
# to a hard error, which the workspace does not satisfy and is not trying to.
# Deny-level lints still fail, because they are declared deny.
CARGO_ARGS=(clippy --workspace --all-targets "${FEATURE_ARGS[@]}")

# MEASURED 2026-08-21 on this workspace, each by running the gate and reading
# the arm line it printed. The previous reading here was 2026-08-20's; three of
# the five numbers had drifted since, which is why they are dated and why none
# of them is a floor - every floor below is either derived or set far under.
#
#   workspace members            29   (--audit-only, arm workspace_members)
#                                     31 on 2026-08-21 before a secret-policy
#                                     leaf was folded back into core and
#                                     zeroship-gatekit was deleted
#   feature-enabled targets     163   (--audit-only, arm expected_targets)
#                                     172 before the five zeroship-gatekit
#                                     compose-gate binaries, their two
#                                     integration tests and finally the crate
#                                     itself were deleted; 158 at the 2026-08-20
#                                     reading; that in turn was 148 under the
#                                     two-feature list the --all-features switch
#                                     replaced, the 10 new ones being the targets
#                                     whose required-features that list did not
#                                     satisfy (arm-4 comment below)
#   declared features            28   (--audit-only, arm declared_features)
#   include_str! literals       246   (--preflight-only, arm preflight_include_str)
#
# MIN_MEMBERS is a bound on arm 2's DENOMINATOR, not on arm 2: the arm's floor is
# the member count itself, so a metadata blob that collapsed would satisfy
# completeness with nothing in it. The other three are floors on the arm
# directly, set well under today's number - far enough that ordinary editing does
# not reach them, close enough that a collapse does.
MIN_MEMBERS_DEFAULT=20
MIN_TARGETS_DEFAULT=100
MIN_FEATURES_DEFAULT=18
MIN_LITERALS_DEFAULT=150

MODE="run"
CAPTURED=""
META=""
SRC_ROOTS=""
MIN_MEMBERS=""
MIN_TARGETS=""
MIN_FEATURES=""
MIN_LITERALS=""
meta_group=0
roots_group=0

usage() {
  echo "usage: $0 [--audit-only <cargo-json> | --preflight-only]" >&2
  echo "          [--metadata F --min-members N --min-targets N --min-features N]" >&2
  echo "          [--src-roots \"DIRS\" --min-literals N]" >&2
}

need_value() {
  if [ "$2" -lt 2 ]; then
    echo "error: $1 needs a value" >&2
    usage
    exit 2
  fi
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --audit-only)
      MODE="audit"
      need_value "$1" "$#"
      CAPTURED="$2"
      shift 2
      ;;
    --preflight-only) MODE="preflight"; shift ;;
    --metadata)     need_value "$1" "$#"; META="$2";         meta_group=$((meta_group + 1));  shift 2 ;;
    --min-members)  need_value "$1" "$#"; MIN_MEMBERS="$2";  meta_group=$((meta_group + 1));  shift 2 ;;
    --min-targets)  need_value "$1" "$#"; MIN_TARGETS="$2";  meta_group=$((meta_group + 1));  shift 2 ;;
    --min-features) need_value "$1" "$#"; MIN_FEATURES="$2"; meta_group=$((meta_group + 1));  shift 2 ;;
    --src-roots)    need_value "$1" "$#"; SRC_ROOTS="$2";    roots_group=$((roots_group + 1)); shift 2 ;;
    --min-literals) need_value "$1" "$#"; MIN_LITERALS="$2"; roots_group=$((roots_group + 1)); shift 2 ;;
    *) usage; exit 2 ;;
  esac
done

# A corpus half-redirected is a corpus measured against somebody else's floor.
if [ "$meta_group" -ne 0 ] && [ "$meta_group" -ne 4 ]; then
  echo "error: --metadata, --min-members, --min-targets, --min-features must be given together" >&2
  echo "       ($meta_group of 4 present). A supplied metadata corpus gated against" >&2
  echo "       this workspace's numbers is the defect these flags replaced." >&2
  exit 2
fi
if [ "$roots_group" -ne 0 ] && [ "$roots_group" -ne 2 ]; then
  echo "error: --src-roots and --min-literals must be given together" >&2
  echo "       ($roots_group of 2 present)." >&2
  exit 2
fi

[ "$meta_group" -eq 4 ] || {
  MIN_MEMBERS="$MIN_MEMBERS_DEFAULT"
  MIN_TARGETS="$MIN_TARGETS_DEFAULT"
  MIN_FEATURES="$MIN_FEATURES_DEFAULT"
}
[ "$roots_group" -eq 2 ] || { SRC_ROOTS="crates libs"; MIN_LITERALS="$MIN_LITERALS_DEFAULT"; }

if [ "$MODE" = "audit" ] && { [ -z "$CAPTURED" ] || [ ! -f "$CAPTURED" ]; }; then
  echo "error: --audit-only needs a readable cargo json stream" >&2
  exit 2
fi

# jq is not optional, and its absence must not read as a clean tree. Every
# extraction below would yield nothing without it, and "no packages built any
# target" is a confusing red at best and, if anyone ever relaxed an arm, a quiet
# green. Say which it is.
if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required by $0 and is not on PATH" >&2
  exit 2
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- preflight: generated files the workspace `include_str!`s ---------------
#
# Two whole build inputs of this workspace are GITIGNORED and produced by
# commands cargo does not know about:
#
#   crates/runtime/tests/wpt/       ~1.1G, fetched by crates/runtime/tests/setup-wpt.sh
#   sdks/*/dist/*.js                emitted by `pnpm build`
#
# crates/runtime `include_str!`s both. When either is absent the crate does not
# COMPILE, and `--all-targets` turns that into a wall of "couldn't read"
# errors - 3 from the missing dist files, ~200 more from the missing WPT tree.
# MEASURED in this worktree on 2026-08-20: a bare
# `cargo clippy --workspace` on a fresh checkout reports
#
#   error: could not compile `zeroship-runtime` (lib) due to 3 previous errors
#
# and stops, so nothing downstream of the runtime is linted at all. That is the
# same shape as the failure this gate exists for, except that here the tree is
# fine and the MACHINE is not set up. Reporting it as a lint result would be a
# lie in both directions.
#
# So this refuses, with exit 2. Exit 2 says "no measurement was taken"; exit 1
# says "the measurement came back bad". CI does `pnpm install && pnpm build` and
# fetches (and caches) the WPT tree before it gets here, so this only ever fires
# locally - and it fires naming the command that fixes it.
#
# The list is DERIVED, not hardcoded. Every `include_str!` literal under the
# scanned roots is resolved against its own file's directory and checked for
# existence. A hardcoded pair of paths would have been wrong the day it was
# written: the multi-line form at crates/runtime/src/core/init.rs:391 puts the
# literal on the line AFTER `include_str!(`, and the first version of this scan
# - single-line only - missed it and three others like it. It also misses,
# still, the two `include_str!(concat!(...))` forms, which build their path from
# macros this cannot evaluate; those resolve under CARGO_MANIFEST_DIR to tracked
# files, so they are not a generated-artifact risk.
#
# It can also over-refuse: an `include_str!` inside a cfg this build disables is
# never expanded, so a missing file there is not really an error. None exists
# today, and the failure is a refusal to measure rather than a wrong verdict.
#
# SRC_ROOTS and MIN_LITERALS arrive together from argv; see the header.

INC_AWK="$TMP/include_str.awk"
cat > "$INC_AWK" <<'AWKEOF'
FNR == 1 { dir = FILENAME; sub(/\/[^\/]*$/, "", dir); pending = 0 }
{
  line = $0
  while (match(line, /include_str!\([ \t]*"[^"]*"/)) {
    s = substr(line, RSTART, RLENGTH)
    sub(/^include_str!\([ \t]*"/, "", s); sub(/"$/, "", s)
    print dir "/" s
    line = substr(line, RSTART + RLENGTH)
    pending = 0
  }
  # A bare `include_str!(` at end of line: the literal is on the next one.
  if (line ~ /include_str!\([ \t]*$/) { pending = 1; next }
  if (pending == 1) {
    if (match(line, /"[^"]*"/)) { print dir "/" substr(line, RSTART + 1, RLENGTH - 2) }
    pending = 0
  }
}
AWKEOF

if [ "$MODE" = "run" ] || [ "$MODE" = "preflight" ]; then
  # `-not -path '*/wpt/*'` because upstream WPT is a foreign tree we keep
  # pristine; nothing there is a build input of ours.
  ( cd "$ROOT" && find $SRC_ROOTS -name '*.rs' -not -path '*/wpt/*' -print0 2>/dev/null \
      | xargs -0 -r awk -f "$INC_AWK" ) | sort -u > "$TMP/included.txt"

  scanned="$(grep -c . "$TMP/included.txt" || true)"
  # A scan that reads nothing and a tree that is genuinely complete produce the
  # same empty missing-list. Only the count tells them apart.
  #
  # The count is every literal the scan resolved and then checked for existence,
  # which is exactly the set this arm rules on - there is no filter between the
  # two. MEASURED in this worktree 2026-08-20: 239, of which 194 were missing
  # (no WPT tree, no pnpm build).
  #
  # The floor was 1 until 2026-08-20, with a comment saying it "CANNOT be raised
  # towards 238" because clippy_gate_selftest.sh drives this same block over a
  # two-literal fixture. That was true of a CONSTANT and it is the reason the
  # constant had to go: a floor of 1 against 239 lets this whole scan collapse to
  # a single literal and still print what a healthy tree prints. MIN_LITERALS
  # comes from whoever supplies SRC_ROOTS, so the self-test's fixture declares 2
  # and this workspace declares 150, and neither is measured against the other.
  if ! gate_arm preflight_include_str "$scanned" "$MIN_LITERALS"; then
    echo "error: the include_str! preflight found no literals at all under: $SRC_ROOTS" >&2
    echo "       That is a broken scan, not a clean tree; refusing to lint on it." >&2
    exit 2
  fi

  # Existence is checked on the ABSOLUTE path; only the DISPLAY form is made
  # repo-relative, and only when the file is actually inside the repo. Checking
  # a relative path would silently depend on the caller's cwd.
  : > "$TMP/missing.txt"
  while IFS= read -r p; do
    abs="$(cd "$ROOT" && realpath -m "$p")"
    [ -e "$abs" ] && continue
    printf '%s\n' "${abs#"$ROOT"/}" >> "$TMP/missing.txt"
  done < "$TMP/included.txt"
  # Two files can include_str! the same generated path; report it once.
  sort -u "$TMP/missing.txt" -o "$TMP/missing.txt"

  if [ -s "$TMP/missing.txt" ]; then
    nmiss="$(grep -c . "$TMP/missing.txt" || true)"
    echo "error: $nmiss file(s) the workspace include_str!s do not exist, so the crates that" >&2
    echo "       read them cannot COMPILE, let alone be linted. Refusing: a narrower" >&2
    echo "       green is worse than no answer. ($scanned literals scanned.)" >&2
    echo "" >&2
    head -8 "$TMP/missing.txt" | sed 's/^/         /' >&2
    [ "$nmiss" -gt 8 ] && echo "         ... and $((nmiss - 8)) more" >&2
    echo "" >&2
    if grep -q '/wpt/' "$TMP/missing.txt"; then
      echo "       Fetch the WPT tree (~1.1G, idempotent):" >&2
      echo "         ./crates/runtime/tests/setup-wpt.sh" >&2
    fi
    if grep -q '^sdks/' "$TMP/missing.txt"; then
      echo "       Build the SDK dist files the runtime embeds:" >&2
      echo "         pnpm install --frozen-lockfile && pnpm build" >&2
    fi
    exit 2
  fi

  if [ "$MODE" = "preflight" ]; then
    echo "preflight ok: $scanned include_str! literals all resolve to files on disk"
    # A completed run, so it owes the trailer. Exit 2 rather than 1 if an arm
    # refused, because every other way this mode can end badly means "no
    # measurement was taken", and an arm that ruled on too little is that.
    gate_arms_finish || exit 2
    exit 0
  fi
fi

# --- package id -> name, and the expected target set -----------------------
#
# Parsing a name out of `package_id` by hand is a trap that bites only SOME
# packages: cargo 1.94 emits a PURL whose fragment carries the name only when it
# differs from the last path component.
#
#   path+file:///.../crates/auth#zeroship-auth@0.1.0    <- name present
#   path+file:///.../libs/compio-postgres#0.1.0         <- name absent
#
# `cargo metadata` states the mapping instead of inferring it. It is asked with
# the SAME feature flags as the lint run, so `.resolve.nodes[].features` is the
# feature set the run actually had, and the required-features filter below is
# exact rather than a guess.
if [ -z "$META" ]; then
  META="$TMP/metadata.json"
  if ! (cd "$ROOT" && cargo metadata --format-version 1 "${FEATURE_ARGS[@]}") > "$META" 2>"$TMP/meta.err"; then
    echo "error: cargo metadata failed; the expected target set cannot be built" >&2
    tail -5 "$TMP/meta.err" >&2
    exit 2
  fi
fi

# id<TAB>name for every workspace member.
jq -r '
  (.workspace_members // []) as $ws
  | .packages[] | select(.id as $i | $ws | index($i))
  | [.id, .name] | @tsv
' "$META" | sort -u > "$TMP/members.tsv"

# Arm 2's expectation, and the thing it rules on: one verdict per member.
#
# THE FLOOR IS DERIVED, so this is a COMPLETENESS assertion - every member the
# metadata declares must have come through the jq join above with a row. It was
# the constant 3 until 2026-08-20, chosen because clippy_gate_selftest.sh drives
# every audit case over a three-package fixture; on this workspace's members
# that passed while ruling on a tenth of them, and it would have kept passing at
# 4 of 29. `.workspace_members | length` is the same number the join selects
# against, so a join that stops matching a future cargo's shape shows up here as
# 0 of 29 rather than as an empty green.
#
# MEASURED 2026-08-21 by running this gate: 29 members, and the arm line reads
# examined=29 floor=29 (31/31 earlier the same day). The floor
# being DERIVED is what keeps this line from needing an edit per new crate; the
# denominator is bounded by MIN_MEMBERS, without which a metadata blob that
# collapsed would satisfy completeness with nothing in it.
DECLARED_MEMBERS="$(jq -r '(.workspace_members // []) | length' "$META" 2>/dev/null || true)"
if ! [ "${DECLARED_MEMBERS:-0}" -ge "$MIN_MEMBERS" ] 2>/dev/null; then
  echo "error: the metadata declares ${DECLARED_MEMBERS:-<unreadable>} workspace member(s)," >&2
  echo "       below the declared minimum of $MIN_MEMBERS. That number IS arm 2's floor," >&2
  echo "       so a metadata blob that shrank would shrink the arm with it." >&2
  exit 2
fi
if ! gate_arm workspace_members "$(grep -c . "$TMP/members.tsv" || true)" "$DECLARED_MEMBERS"; then
  echo "error: cargo metadata listed no usable set of workspace members" >&2
  exit 2
fi

# pkg<TAB>target<TAB>kind for every target whose required-features are satisfied
# by the features this run resolved for its package.
jq -r '
  (.workspace_members // []) as $ws
  | (reduce (.resolve.nodes // [])[] as $n ({}; .[$n.id] = ($n.features // []))) as $feat
  | .packages[]
  | select(.id as $i | $ws | index($i))
  | . as $p
  | ($feat[$p.id] // []) as $enabled
  | $p.targets[]
  | select(((."required-features") // []) - $enabled | length == 0)
  | [$p.name, .name, (.kind[0] // "?")] | @tsv
' "$META" | sort -u > "$TMP/expected.tsv"

# Arm 3's expectation. This is the POST-FILTER number: the `select` above drops
# every target whose required-features this run did not enable, and those are
# out of scope rather than unlinted. MEASURED 2026-08-20, via --audit-only: 146
# targets across the 30 members.
#
# It cannot be derived the way arm 2's floor is - nothing in the metadata states
# how many targets OUGHT to survive the filter, and the pre-filter total is a
# different set - so the bound is declared by whoever supplies the metadata. It
# was the constant 4, sized to clippy_gate_selftest.sh case 6, which resolves its
# fixture down to five expected targets on purpose. That case now declares 5 for
# itself and this workspace declares 100, so the self-test no longer sets the
# bound the real tree is held to.
if ! gate_arm expected_targets "$(grep -c . "$TMP/expected.tsv" || true)" "$MIN_TARGETS"; then
  echo "error: too few expected targets derived from cargo metadata" >&2
  exit 2
fi

# --- arm 4's corpus: declared features vs enabled features -----------------
#
# THIS IS THE ARM THAT WAS MISSING, and its absence is the failure the header
# describes: arm 3's expectation is FILTERED by the run's own feature set, so a
# target the run cannot build is not "unlinted" in its bookkeeping - it is not
# there. Arm 3 then reports 148 of 148 on a workspace that declares 158.
#
# What the two-feature list this gate carried until 2026-08-20 left out, taken
# from this same jq under that list:
#
#   compio-postgres/live-tls-tests       tls_live             (test)
#   compio-postgres/tls                  "                    "
#   zeroship-migrate-adapter/platform-cli  platform_migrate   (test)
#                                          zeroship-platform-migrate (bin)
#   zeroship-plugin-db/live-db-tests     distributed_live     (test)
#   zeroship-plugin-db/test-helpers      integration          (test)
#                                        missing_role         (test)
#                                        native_transaction   (test)
#                                        sqlite_integration   (test)
#   zeroship-runtime/bench-bins          echo-server          (bin)
#                                        zeroship-bench-server (bin)
#
# Ten targets, and `platform_migrate` alone held eleven standing deny-level
# `clippy::await_holding_lock` errors while this gate reported the workspace
# clean. `test-helpers` additionally gates ~290 `#[cfg(feature = ...)]` sites
# INSIDE plugin-db's already-linted lib, which no target-level accounting could
# ever have noticed were missing.
#
# The verdict is deferred to the reporting section below so the arms print in
# order; only the corpus and its anti-vacuity floor are established here.
jq -r '
  (.workspace_members // []) as $ws
  | (reduce (.resolve.nodes // [])[] as $n ({}; .[$n.id] = ($n.features // []))) as $feat
  | .packages[]
  | select(.id as $i | $ws | index($i))
  | . as $p
  | ($feat[$p.id] // []) as $enabled
  | (($p.features // {}) | keys | map(select(. != "default"))[])
  | . as $f
  | [$p.name, $f, (if ($enabled | index($f)) then "on" else "OFF" end)] | @tsv
' "$META" | sort -u > "$TMP/features.tsv"

# feature<TAB>target<TAB>kind, so a feature reported OFF can be printed with the
# targets it gates rather than as a bare name nobody can act on. Keyed
# `pkg/feature` because `required-features` entries are bare names scoped to
# their own package, and two packages can declare the same one - `live-db-tests`
# is declared by three.
jq -r '
  (.workspace_members // []) as $ws
  | .packages[]
  | select(.id as $i | $ws | index($i))
  | . as $p
  | $p.targets[]
  | . as $t
  | (((($t."required-features") // [])[]))
  | [($p.name + "/" + .), $t.name, ($t.kind[0] // "?")] | @tsv
' "$META" | sort -u > "$TMP/feature_targets.tsv"

# Arm 4 rules on every declared non-`default` feature: one on/OFF verdict each.
# `default` is excluded because it is not a thing that can go unlinted - cargo
# enables it unless told otherwise, and counting it would inflate this number by
# one per package for no verdict.
#
# The floor cannot be derived the way arm 2's is: nothing in the metadata says
# how many features a workspace OUGHT to declare. So it travels with the corpus,
# like MIN_TARGETS. MEASURED 2026-08-20 on this workspace: 28.
if ! gate_arm declared_features "$(grep -c . "$TMP/features.tsv" || true)" "$MIN_FEATURES"; then
  echo "error: too few declared features derived from cargo metadata - either the" >&2
  echo "       .packages[].features shape changed or the join dropped every member," >&2
  echo "       and an empty feature list reads as full feature coverage." >&2
  exit 2
fi

UNENABLED_FEATURES="$(awk -F'\t' '$3 == "OFF" {print $1 "/" $2}' "$TMP/features.tsv")"

# --- run clippy ------------------------------------------------------------
JSON="$TMP/clippy.json"
cargo_rc=0
if [ "$MODE" = "audit" ]; then
  JSON="$CAPTURED"
else
  # Diagnostics are re-rendered to stderr as they arrive, so a developer sees
  # exactly what the bare `cargo clippy` printed. cargo's own progress lines
  # already go to stderr untouched.
  (cd "$ROOT" && cargo "${CARGO_ARGS[@]}" --message-format=json) \
    | tee "$JSON" \
    | jq -r 'select(.reason == "compiler-message") | .message.rendered // empty' >&2
  cargo_rc="${PIPESTATUS[0]}"
fi

if [ ! -s "$JSON" ]; then
  echo "error: cargo emitted no json at all; nothing was measured (cargo exited $cargo_rc)" >&2
  exit 2
fi

# --- observed targets ------------------------------------------------------
jq -r 'select(.reason == "compiler-artifact")
       | [.package_id, .target.name, (.target.kind[0] // "?")] | @tsv' "$JSON" \
  | sort -u > "$TMP/artifacts.tsv"

# Join to package names, dropping every artifact from a non-workspace crate.
awk -F'\t' '
  NR == FNR { name[$1] = $2; next }
  ($1 in name) { print name[$1] "\t" $2 "\t" $3 }
' "$TMP/members.tsv" "$TMP/artifacts.tsv" | sort -u > "$TMP/observed.tsv"

# --- errors, split by whose fault they are ---------------------------------
jq -r 'select(.reason == "compiler-message")
       | select(.message.level == "error")
       | [.package_id, (.message.code.code // "-"), (.message.message | gsub("\t"; " ")),
          (.target.name // "?"), (.target.kind[0] // "?")]
       | @tsv' "$JSON" | sort -u > "$TMP/errors.tsv"

# Errors from crates.io dependencies are dropped, not renamed: they are not this
# workspace's code and failing on them would make the gate red for something no
# commit here can fix. They cannot hide a real problem, because a dependency
# that fails to build takes every workspace crate downstream of it with it, and
# THAT is what arm 2 reports. The count is printed so the drop is never silent.
awk -F'\t' '
  NR == FNR { name[$1] = $2; next }
  ($1 in name) { print name[$1] "\t" $2 "\t" $3 "\t" $4 "\t" $5; next }
  { ext++ }
  END { if (ext) print ext > "/dev/stderr" }
' "$TMP/members.tsv" "$TMP/errors.tsv" > "$TMP/errors_named.tsv" 2> "$TMP/errors_external.txt"

EXTERNAL_ERRS="$(tr -d '[:space:]' < "$TMP/errors_external.txt")"
EXTERNAL_ERRS="${EXTERNAL_ERRS:-0}"

# Projected to (package, code, message) and deduped AGAIN. The same source line
# is re-linted once per target that compiles it, so a doc comment in a module
# that several test binaries in a crate share reports once per binary. Deduping
# on the full row, target included, printed one such comment three times and
# made the count read as three separate faults - which is what this projection
# was added to stop.
LINT_ERRS="$(awk -F'\t' '$2 ~ /^clippy::/ {printf "%s\t%s\t%s\n", $1, $2, $3}' "$TMP/errors_named.tsv" | sort -u)"
HARD_ERRS="$(awk -F'\t' '$2 !~ /^clippy::/ {printf "%s\t%s\t%s\n", $1, $2, $3}' "$TMP/errors_named.tsv" | sort -u)"

# The (package, target, kind) triples that FAILED. A target that errors emits no
# artifact, so without this arm 3 would name every failing target a second time
# under a heading that is supposed to mean "never reached" - and a heading that
# fires for two different things stops being read as either.
awk -F'\t' '{printf "%s\t%s\t%s\n", $1, $4, $5}' "$TMP/errors_named.tsv" \
  | sort -u > "$TMP/errored_targets.tsv"

WARN_COUNT="$(jq -r 'select(.reason == "compiler-message")
                     | select(.message.level == "warning")
                     | .message.code.code // "-"' "$JSON" | sort -u | wc -l)"

rc=0

# --- arm 1: lint verdict ---------------------------------------------------
if [ -n "$HARD_ERRS" ]; then
  rc=1
  n="$(printf '%s\n' "$HARD_ERRS" | grep -c . || true)"
  echo "::error::$n compiler error(s): these crates could not be LINTED AT ALL, so no verdict about them is valid"
  printf '%s\n' "$HARD_ERRS" | awk -F'\t' '{printf "  %-28s %-12s %s\n", $1, $2, $3}' | head -20
  printf '%s\n' "$HARD_ERRS" | cut -f1 | sort -u | tr '\n' ' ' | sed 's/^/  crates affected: /;s/ $/\n/'
fi

if [ -n "$LINT_ERRS" ]; then
  rc=1
  n="$(printf '%s\n' "$LINT_ERRS" | grep -c . || true)"
  echo "::error::$n deny-level clippy lint(s)"
  printf '%s\n' "$LINT_ERRS" | awk -F'\t' '{printf "  %-28s %-40s %s\n", $1, $2, $3}' | head -20
fi

# --- arm 2: package coverage -----------------------------------------------
cut -f2 "$TMP/members.tsv" | sort -u > "$TMP/expected_pkgs.txt"
cut -f1 "$TMP/observed.tsv" | sort -u > "$TMP/observed_pkgs.txt"
UNREACHED_PKGS="$(comm -23 "$TMP/expected_pkgs.txt" "$TMP/observed_pkgs.txt")"

if [ -n "$UNREACHED_PKGS" ]; then
  rc=1
  echo "::error::these workspace packages produced NO linted target - they are NOT clean, they were NOT REACHED"
  printf '  %s\n' $UNREACHED_PKGS
  echo "  A clippy run that stops on the first failing crate never schedules the"
  echo "  crates downstream of it. Fix the errors above and re-run before reading"
  echo "  any verdict about these."
fi

# --- arm 3: target coverage ------------------------------------------------
#
# Expected minus observed, then minus the two sets that are already reported:
# targets that failed (arm 1 named the error) and whole packages that were never
# scheduled (arm 2 named the package). What is left is the residue neither can
# see - a target that quietly stopped being built while its package kept going.
comm -23 "$TMP/expected.tsv" "$TMP/observed.tsv" > "$TMP/unreached_all.tsv"
UNREACHED_TARGETS="$(comm -23 "$TMP/unreached_all.tsv" "$TMP/errored_targets.tsv")"
if [ -n "$UNREACHED_PKGS" ]; then
  UNREACHED_TARGETS="$(printf '%s\n' "$UNREACHED_TARGETS" \
    | grep -v -F -f <(printf '%s\n' $UNREACHED_PKGS | sed 's/$/\t/') || true)"
fi

if [ -n "$UNREACHED_TARGETS" ]; then
  rc=1
  n="$(printf '%s\n' "$UNREACHED_TARGETS" | grep -c . || true)"
  echo "::error::$n declared target(s) whose required-features were satisfied produced no artifact and reported no error - they went unlinted"
  printf '%s\n' "$UNREACHED_TARGETS" | awk -F'\t' '{printf "  %s  %s (%s)\n", $1, $2, $3}' | head -20
  if [ -n "$LINT_ERRS$HARD_ERRS" ]; then
    echo "  Cargo stops draining its queue when a unit fails, so on a red run this"
    echo "  list is mostly collateral: targets nothing was wrong with that simply"
    echo "  never got their turn. Fix the errors above and re-run - this list is"
    echo "  the reason the run above is not evidence that they are clean."
  else
    echo "  Nothing errored in these, and nothing built them. A target that stops"
    echo "  being built stops being linted, silently, which is the whole failure"
    echo "  this arm exists to catch."
  fi
fi

# --- arm 4: feature coverage -----------------------------------------------
#
# The three arms above all audit ONE feature resolution. This one audits the
# resolution itself, against the features the manifests declare - a source the
# run cannot influence. It is what makes the `--all-features` above checkable
# rather than merely intended: edit the invocation back to a narrow
# `--features a,b` and arms 1-3 stay green on a smaller workspace, exactly as
# they did for the two years the platform-cli targets went uncompiled, while
# this arm names every feature that dropped out.
#
# A feature that CANNOT be enabled - one that conflicts with another, or needs a
# toolchain this machine has not got - belongs here as a named, reviewed failure,
# not as an exemption list. An exemption list is a census and would rot the same
# way the feature list did. There is no such feature on this workspace today;
# `--all-features` resolves and compiles.
if [ -n "$UNENABLED_FEATURES" ]; then
  rc=1
  n="$(printf '%s\n' "$UNENABLED_FEATURES" | grep -c . || true)"
  echo "::error::$n declared workspace feature(s) were NOT enabled by this run - the code and targets they gate were never compiled, so nothing above says anything about them"
  while IFS= read -r feat; do
    [ -n "$feat" ] || continue
    echo "  $feat"
    awk -F'\t' -v f="$feat" '$1 == f {printf "      gates target %s (%s)\n", $2, $3}' \
      "$TMP/feature_targets.tsv"
  done <<< "$UNENABLED_FEATURES"
  echo "  A target whose required-features are unmet is not counted as unlinted by"
  echo "  arm 3 - it is filtered out of arm 3's expectation, so the coverage"
  echo "  numbers above balance on a workspace this run made smaller. Enable the"
  echo "  feature, or delete it if nothing needs it."
fi

# --- the numbers -----------------------------------------------------------
OBS_T="$(grep -c . "$TMP/observed.tsv" || true)"
EXP_T="$(grep -c . "$TMP/expected.tsv" || true)"
OBS_P="$(grep -c . "$TMP/observed_pkgs.txt" || true)"
EXP_P="$(grep -c . "$TMP/expected_pkgs.txt" || true)"

# Observed and expected are printed side by side rather than as one ratio,
# because observed can legitimately EXCEED expected: cargo unifies features
# across the workspace, so a target can be built under a feature its own package
# did not resolve. An X/Y that read "6/5" would look like a bug in the gate.
# The observed side of both coverage arms, declared separately because it can
# collapse on its own: `expected` comes from cargo metadata and `observed` from
# the json stream, and a stream this gate cannot parse (a cargo that changed its
# artifact shape, an --audit-only file from another tool) leaves expected intact
# while observed goes to zero. Arm 2 would then name all 30 packages as "not
# reached", which is a true statement about the stream and a wrong one about the
# tree.
#
# THE FLOOR IS 1 AND MUST STAY THERE, which is the opposite of the advice for
# the arms above. Observed is the one number here that is legitimately small on
# a RED run: this file's own header records 131 of 134 targets on a warm tree
# and 100 of 134 on a dirty one, same source, and clippy_gate_selftest.sh case 3
# pins a stream with a single artifact in it. Any floor that would mean
# something on a green run fires on every ordinary red one.
gate_arm linted_targets "$OBS_T" 1 || true

FEAT_ALL="$(grep -c . "$TMP/features.tsv" || true)"
FEAT_OFF="$(printf '%s' "$UNENABLED_FEATURES" | grep -c . || true)"

echo "linted:   $OBS_T targets in $OBS_P packages (expected $EXP_T in $EXP_P)"
echo "features: $((FEAT_ALL - FEAT_OFF)) of $FEAT_ALL declared non-default workspace features enabled"
echo "warnings: $WARN_COUNT distinct lint codes (pedantic/nursery are warn by design; not gated)"
if [ "$EXTERNAL_ERRS" -gt 0 ]; then
  echo "note:     $EXTERNAL_ERRS error(s) in non-workspace crates, not gated on here"
fi

# --- cargo's own verdict ---------------------------------------------------
#
# Checked LAST and only for the case the arms above cannot explain. cargo can
# fail for reasons that never reach the json stream at all - a linker error, a
# build script that died, a lockfile it refused to update - and every one of
# those leaves the arms above looking clean. Passing then would be the same
# class of mistake as the one this gate exists to stop.
# The arm trailer prints BEFORE the block below, so both of the exits that
# follow carry it, and its verdict is folded in AFTERWARDS so the block's own
# `rc -eq 0` test still means what it meant: "no arm above found anything
# wrong", not "no arm above found anything wrong AND the accounting was fine".
arms_rc=0
gate_arms_finish || arms_rc=1

if [ "$cargo_rc" -ne 0 ] && [ "$rc" -eq 0 ]; then
  echo "::error::cargo exited $cargo_rc but emitted no error diagnostic and left no target unlinted"
  echo "  Nothing in the json stream explains this, so the gate cannot say the"
  echo "  workspace is clean. Re-run without --message-format=json to see it."
  exit 2
fi

[ "$arms_rc" -eq 0 ] || rc=1

# --- a verdict the LAST LINE can carry -------------------------------------
#
# Printed only on failure, so a passing run's output is byte-identical to what
# it always was and anything parsing the arm trailer keeps working.
#
# It exists because this gate's final lines are the SAME on pass and fail: the
# feature count, the warning count and `zsgate-arms ... refusals=0` all print
# either way, and the one line that distinguishes them -- `linted: N targets`
# -- comes EARLIER and is not printed at all when a deny-level error aborts
# cargo's queue. A reader tailing the output therefore sees `refusals=0` last
# and reads it as a pass, which is what happened on 2026-08-23: a red gate was
# reported green for roughly eight merges, with most of the workspace never
# linted, because `refusals=0` is the ARM CENSUS and not the lint verdict.
if [ "$rc" -ne 0 ]; then
  echo "::error::clippy gate FAILED (rc=$rc)"
  echo "::error::  The lines above print on a PASSING run too. 'refusals=0' is the arm"
  echo "::error::  census, not the verdict. Trust this line, the exit code, or 'linted:'."
fi
exit $rc
