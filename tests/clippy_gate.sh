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
# `--keep-going` is added so that a failure in one crate does not stop cargo
# scheduling units in crates that do not depend on it. It shrinks how much of
# the workspace goes unreached on a red run; it cannot eliminate it, because a
# crate whose dependency failed genuinely cannot be linted. That residue is
# precisely what arm 2 reports by name instead of leaving silent.
#
# A PREFLIGHT, BEFORE ANY OF IT
#
# Two build inputs of this workspace are gitignored and produced by commands
# cargo knows nothing about (`pnpm build`, `setup-wpt.sh`). Without them
# crates/runtime does not compile and the run aborts having linted almost
# nothing. The gate REFUSES with exit 2 in that case rather than returning a
# narrower green. See the preflight block below for how the list is derived.
#
# THREE ARMS
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
# WHAT THIS CANNOT SEE
#
#   - Code behind a `cfg` this build does not enable, or a target platform this
#     machine is not. Clippy lints what it compiles.
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
#   tests/clippy_gate.sh --audit-only <json>
#                                     audit a previously captured json stream
#                                     without re-running cargo
#
# EXIT CODES
#   0  every target linted, no deny-level errors
#   1  a lint failed, or a target went unlinted
#   2  the gate could not run, or could not be trusted to have measured anything
#
# Env overrides exist for tests/clippy_gate_selftest.sh:
#   ZS_CLIPPY_METADATA   pre-captured `cargo metadata` json (skips cargo)
#   ZS_CLIPPY_SRC_ROOTS  directories the include_str! preflight scans
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The feature list lives HERE and nowhere else, so the local run and the CI run
# cannot lint different sets of targets. It was carried over verbatim from the
# ci.yml step this replaced, with its reasoning:
#
#   `--all-targets` only reaches targets whose `required-features` are
#   satisfied, so the 45 live-database test files would silently stop being
#   linted the moment they were gated. Naming the feature keeps exactly the set
#   that was linted before still linted.
#
# Adding a feature here widens what is linted and is always safe. Removing one
# narrows it, which is the failure mode arm 3 exists to catch - and arm 3 will
# NOT catch it, because it resolves its expectation under this same list. That
# is the one place the expectation and the subject share a source; a removal
# here has to be caught in review.
FEATURES="zeroship-control/live-db-tests,zeroship-migrated/live-db-tests"

# Not `-D warnings`. The workspace grades its lints in the root Cargo.toml
# `[workspace.lints]` table - `clippy::all` deny, `pedantic` and `nursery` warn -
# and flattening that grading would promote every pedantic and nursery warning
# to a hard error, which the workspace does not satisfy and is not trying to.
# Deny-level lints still fail, because they are declared deny.
CARGO_ARGS=(clippy --workspace --all-targets --keep-going --features "$FEATURES")

MODE="run"
CAPTURED=""
case "${1:-}" in
  --audit-only)
    MODE="audit"
    CAPTURED="${2:-}"
    if [ -z "$CAPTURED" ] || [ ! -f "$CAPTURED" ]; then
      echo "error: --audit-only needs a readable cargo json stream" >&2
      exit 2
    fi
    ;;
  "") ;;
  *)
    echo "usage: $0 [--audit-only <cargo-json>]" >&2
    exit 2
    ;;
esac

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
SRC_ROOTS="${ZS_CLIPPY_SRC_ROOTS:-crates libs}"

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

if [ "$MODE" = "run" ]; then
  # `-not -path '*/wpt/*'` because upstream WPT is a foreign tree we keep
  # pristine; nothing there is a build input of ours.
  ( cd "$ROOT" && find $SRC_ROOTS -name '*.rs' -not -path '*/wpt/*' -print0 2>/dev/null \
      | xargs -0 -r awk -f "$INC_AWK" ) | sort -u > "$TMP/included.txt"

  scanned="$(grep -c . "$TMP/included.txt" || true)"
  # A scan that reads nothing and a tree that is genuinely complete produce the
  # same empty missing-list. Only the count tells them apart.
  if [ "$scanned" -eq 0 ]; then
    echo "error: the include_str! preflight found no literals at all under: $SRC_ROOTS" >&2
    echo "       That is a broken scan, not a clean tree; refusing to lint on it." >&2
    exit 2
  fi

  : > "$TMP/missing.txt"
  while IFS= read -r p; do
    r="$(cd "$ROOT" && realpath -m --relative-to=. "$p")"
    [ -e "$ROOT/$r" ] || printf '%s\n' "$r" >> "$TMP/missing.txt"
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
    if grep -q '^crates/runtime/tests/wpt/' "$TMP/missing.txt"; then
      echo "       Fetch the WPT tree (~1.1G, idempotent):" >&2
      echo "         ./crates/runtime/tests/setup-wpt.sh" >&2
    fi
    if grep -q '^sdks/' "$TMP/missing.txt"; then
      echo "       Build the SDK dist files the runtime embeds:" >&2
      echo "         pnpm install --frozen-lockfile && pnpm build" >&2
    fi
    exit 2
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
# the SAME --features as the lint run, so `.resolve.nodes[].features` is the
# feature set the run actually had, and the required-features filter below is
# exact rather than a guess.
META="${ZS_CLIPPY_METADATA:-}"
if [ -z "$META" ]; then
  META="$TMP/metadata.json"
  if ! (cd "$ROOT" && cargo metadata --format-version 1 --features "$FEATURES") > "$META" 2>"$TMP/meta.err"; then
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

if [ ! -s "$TMP/members.tsv" ]; then
  echo "error: cargo metadata listed no workspace members" >&2
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

if [ ! -s "$TMP/expected.tsv" ]; then
  echo "error: no expected targets derived from cargo metadata" >&2
  exit 2
fi

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
       | [.package_id, (.message.code.code // "-"), (.message.message | gsub("\t"; " "))]
       | @tsv' "$JSON" | sort -u > "$TMP/errors.tsv"

awk -F'\t' '
  NR == FNR { name[$1] = $2; next }
  { print (($1 in name) ? name[$1] : "(external)") "\t" $2 "\t" $3 }
' "$TMP/members.tsv" "$TMP/errors.tsv" > "$TMP/errors_named.tsv"

LINT_ERRS="$(awk -F'\t' '$2 ~ /^clippy::/' "$TMP/errors_named.tsv")"
HARD_ERRS="$(awk -F'\t' '$2 !~ /^clippy::/' "$TMP/errors_named.tsv")"

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
UNREACHED_TARGETS="$(comm -23 "$TMP/expected.tsv" "$TMP/observed.tsv")"
# A package already named by arm 2 would repeat every one of its targets here.
if [ -n "$UNREACHED_PKGS" ]; then
  UNREACHED_TARGETS="$(printf '%s\n' "$UNREACHED_TARGETS" \
    | grep -v -F -f <(printf '%s\n' $UNREACHED_PKGS | sed 's/$/\t/') || true)"
fi

if [ -n "$UNREACHED_TARGETS" ]; then
  rc=1
  n="$(printf '%s\n' "$UNREACHED_TARGETS" | grep -c . || true)"
  echo "::error::$n declared target(s) whose required-features were satisfied produced no artifact - they went unlinted"
  printf '%s\n' "$UNREACHED_TARGETS" | awk -F'\t' '{printf "  %s  %s (%s)\n", $1, $2, $3}' | head -20
fi

# --- the numbers -----------------------------------------------------------
OBS_T="$(grep -c . "$TMP/observed.tsv" || true)"
EXP_T="$(grep -c . "$TMP/expected.tsv" || true)"
OBS_P="$(grep -c . "$TMP/observed_pkgs.txt" || true)"
EXP_P="$(grep -c . "$TMP/expected_pkgs.txt" || true)"

echo "linted:   $OBS_T/$EXP_T targets in $OBS_P/$EXP_P workspace packages"
echo "warnings: $WARN_COUNT distinct lint codes (pedantic/nursery are warn by design; not gated)"

# --- cargo's own verdict ---------------------------------------------------
#
# Checked LAST and only for the case the arms above cannot explain. cargo can
# fail for reasons that never reach the json stream at all - a linker error, a
# build script that died, a lockfile it refused to update - and every one of
# those leaves the arms above looking clean. Passing then would be the same
# class of mistake as the one this gate exists to stop.
if [ "$cargo_rc" -ne 0 ] && [ "$rc" -eq 0 ]; then
  echo "::error::cargo exited $cargo_rc but emitted no error diagnostic and left no target unlinted"
  echo "  Nothing in the json stream explains this, so the gate cannot say the"
  echo "  workspace is clean. Re-run without --message-format=json to see it."
  exit 2
fi

exit $rc
