#!/usr/bin/env bash
#
# The data crates must not be able to REACH a driver or the V8 runtime.
#
# ## Why this is not the vendor embedding gate
#
# `tests/vendor_embedding_gate.sh` reads SOURCE: it greps files for vendor type
# names. That catches a `compio_postgres::Client` written into a signature, and
# it is the right instrument for plugin-db, which is being carved up file by
# file.
#
# It cannot catch this: a crate acquires a dependency, that dependency pulls the
# driver, and NOT ONE LINE of the crate's own source changes. Nothing is grepped
# because nothing was written. The crate is still vendor-neutral to read and no
# longer vendor-neutral to link.
#
# That is not hypothetical. #97 was filed on exactly this mechanism - data-core
# depends on zeroship-schema, zeroship-schema was believed to carry the driver,
# so data-core carried it too - and the ticket named a public field as the
# symptom when the manifest was the cause. Measured 2026-09-02, the edge is gone
# and data-core's closure is clean. Nothing kept it that way; this does.
#
# ## What it asserts
#
# For each crate below, the NORMAL dependency closure (`cargo tree -e normal`,
# so dev-dependencies are out of scope by construction) contains none of the
# forbidden crates. Normal-only is deliberate and matches the zero-tokio gate's
# reasoning: a test binary is not a shipped one.
#
# ## What it does NOT assert
#
# That the crates are correct, or that their source is tier-clean. A crate can
# pass this and still be a mess; it just cannot be a mess that LINKS a driver.
# The signature census and the direction census cover source. Neither of the
# three substitutes for the others - the lesson #103 and #128 both landed on.

set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
gate_arms_init data_crate_closure

FAIL=0
PASS=0
bad() {
  printf '  FAIL %s\n' "$1"
  FAIL=$((FAIL + 1))
}
good() {
  printf '  ok   %s\n' "$1"
  PASS=$((PASS + 1))
}

# --------------------------------------------------------------------------
# The crates whose closure is a contract, and what they may not reach.
#
# Every name here is a CRATE name as `cargo tree` prints it, not a module path.
# --------------------------------------------------------------------------
GUARDED_CRATES="
zeroship-data-core
zeroship-data-query-builder
zeroship-data-macros
"

# compio-postgres  the PostgreSQL driver. data-core holding one is the whole
#                  reason the split exists.
# rusqlite         the SQLite driver, same argument.
# zeroship-runtime the V8 runtime. A data crate that links it cannot be used by
#                  anything that is not a worker - the migration service and the
#                  CDC relay both need these types without an isolate.
# v8               the same edge one level down, named separately because a
#                  crate could acquire it without going through our runtime.
FORBIDDEN_CRATES="
compio-postgres
rusqlite
zeroship-runtime
v8
"

# --------------------------------------------------------------------------
# Arm 1: no guarded crate REACHES a forbidden crate.
# --------------------------------------------------------------------------
echo "== data crates must not link a driver or the runtime =="

n_pairs=0
for crate in $(printf '%s\n' "$GUARDED_CRATES" | grep -v '^[[:space:]]*$'); do
  if ! tree_out=$(cargo tree -p "$crate" -e normal 2>/dev/null); then
    bad "cargo tree failed for $crate - the gate cannot rule, which is a refusal"
    continue
  fi
  # A crate that resolves to nothing would make every check below vacuously
  # true. Its own name is always the first line, so an empty tree is a bug in
  # the invocation, not a clean result.
  if [ -z "$tree_out" ]; then
    bad "cargo tree printed nothing for $crate - refusing to read that as clean"
    continue
  fi

  for forbidden in $(printf '%s\n' "$FORBIDDEN_CRATES" | grep -v '^[[:space:]]*$'); do
    n_pairs=$((n_pairs + 1))
    # Match the crate name at a tree position, not as a substring: `compio-postgres`
    # must not be matched by `compio-postgres-derive`, and `v8` must not be
    # matched by `v8-something`. cargo prints `<name> v<version>`.
    hits=$(printf '%s\n' "$tree_out" | grep -cE "(^|[^a-zA-Z0-9_-])${forbidden} v[0-9]" || true)
    if [ "$hits" -gt 0 ]; then
      bad "$crate reaches $forbidden ($hits occurrence(s) in its normal closure)"
      printf '%s\n' "$tree_out" | grep -nE "(^|[^a-zA-Z0-9_-])${forbidden} v[0-9]" | head -3 | sed 's/^/       /'
    else
      good "$crate does not reach $forbidden"
    fi
  done
done

# 8 = 2 guarded crates x 4 forbidden crates. The floor is one BELOW the full
# product on purpose: it must fail loudly if a crate is dropped from the list or
# a `cargo tree` call starts failing silently, without going red merely because
# someone legitimately adds a fifth forbidden name.
if ! gate_arm crate_closure "$n_pairs" 8; then
  FAIL=$((FAIL + 1))
fi

# --------------------------------------------------------------------------
# Arm 2: every guarded crate exists and is a workspace member.
#
# Arm 1 reports a refusal when `cargo tree` fails, but a name that was renamed
# or deleted would take its four pair-checks out of the count silently if the
# floor above ever slipped. This arm rules on the LIST, not the closures.
# --------------------------------------------------------------------------
echo
echo "== every guarded crate is a real workspace member =="

n_members=0
for crate in $(printf '%s\n' "$GUARDED_CRATES" | grep -v '^[[:space:]]*$'); do
  n_members=$((n_members + 1))
  if [ -f "crates/$crate/Cargo.toml" ]; then
    good "$crate is a workspace member"
  else
    bad "$crate is guarded but crates/$crate/Cargo.toml does not exist - the list names a crate that is gone"
  fi
done

if ! gate_arm guarded_list "$n_members" 2; then
  FAIL=$((FAIL + 1))
fi

# --------------------------------------------------------------------------
# Arm 3: NOTHING SHIPPED LINKS THE CDC RELAY.
#
# `zeroship-data-cdc-server` is a BINARY, and the reason it is a binary is a
# privilege boundary: it holds REPLICATION so the worker does not. A boundary
# any crate can link is not a boundary, it is the appearance of one
# (AGENTS.md, "Privilege follows the PROCESS, not the function").
#
# WHY THE RULE IS NOT LITERALLY "NO CRATE MAY DEPEND ON IT". A workspace bin
# target must carry a class (`zeroship-config-contract`'s
# `validate_target_classifications`), and a `platform` class then obliges the
# configuration checker to LINK that binary's generated registry
# (`crates/zeroship-config-contract/tests/real_registry.rs`, `every_platform_target_declares_its_configuration`,
# which compares the manifest `platform` set against `DECLARING_BINARIES` for
# exact equality with no exception list). Classifying the relay anything else
# would leave a shipped service's configuration surface unaudited, which is the
# vacuity that test exists to prevent. So the enforceable property is:
#
#   no SHIPPED binary's normal-dependency closure contains the relay,
#   with the non-shipped `zeroship-config-contract` as the single exception.
#
# THIS ARM CARRIES ITS OWN INVERTED CONTROL, which is the point of ruling on
# config-contract rather than skipping it. An inverted `cargo tree` prints an
# error to stderr and NOTHING to stdout when the subject is absent from the
# package's graph - and a
# mistyped package name, a bad flag or a cargo failure produce exactly the same
# empty stdout. An absence is therefore only evidence if the same command shape
# is seen to FIND something. config-contract is the one package that must match,
# so the arm proves its own instrument on every run.
#
# NORMAL-ONLY, deliberately, and it is the known hole: `cargo tree -e normal`
# cannot see dev-dependencies, so a dev-dependency on the relay would pass here.
# `zeroship-migrate-server`, the relay's declared peer, already carries five of
# them. Both manifests say so in prose; this is the half a machine can check.
# --------------------------------------------------------------------------
echo
echo "== nothing shipped links the CDC relay =="

RELAY="zeroship-data-cdc-server"
# The ONE package allowed to reach it, and the one that must.
RELAY_LINKER="zeroship-config-contract"

# Derived from cargo metadata, never from a list in this file: a hard-coded set
# of binaries is satisfiable by deleting a name from it, and a new platform
# service would be checked by nobody.
bin_packages=$(cargo metadata --format-version 1 --no-deps 2>/dev/null |
  jq -r '.packages[] | select(.targets[]?.kind[]? == "bin") | .name' | sort -u)

n_bins=0
relay_linker_seen=0
if [ -z "$bin_packages" ]; then
  bad "cargo metadata produced no bin packages - arm 3 would rule on nothing"
else
  for pkg in $bin_packages; do
    # The relay's own tree trivially contains the relay; ruling on it would be
    # a free pass padding the count.
    [ "$pkg" = "$RELAY" ] && continue
    n_bins=$((n_bins + 1))
    hits=$(cargo tree -p "$pkg" -i "$RELAY" -e normal 2>/dev/null |
      grep -cE "(^|[^a-zA-Z0-9_-])${RELAY} v[0-9]" || true)
    if [ "$pkg" = "$RELAY_LINKER" ]; then
      relay_linker_seen=1
      if [ "$hits" -gt 0 ]; then
        good "$pkg links $RELAY, as the single named exception requires (this is arm 3's inverted control)"
      else
        bad "$pkg does NOT link $RELAY - either the exception was removed, or the inverted cargo tree no longer finds a real edge and every absence below is meaningless"
      fi
    elif [ "$hits" -gt 0 ]; then
      bad "$pkg reaches $RELAY in its normal closure - a shipped binary now links the process that exists to hold REPLICATION away from it"
      cargo tree -p "$pkg" -i "$RELAY" -e normal 2>/dev/null | head -5 | sed 's/^/       /'
    else
      good "$pkg does not reach $RELAY"
    fi
  done
fi

if [ "$relay_linker_seen" -eq 0 ]; then
  bad "$RELAY_LINKER is not a bin package in cargo metadata - arm 3 ran without its control"
fi

# 11 bin packages today, minus the relay itself = 10 ruled on. Floor 7, three
# below: it must go red if the metadata query stops finding binaries or a filter
# starts excluding them, without going red because a tool binary is retired.
if ! gate_arm relay_is_unlinked "$n_bins" 7; then
  FAIL=$((FAIL + 1))
fi

echo
gate_arms_finish || FAIL=$((FAIL + 1))

echo
if [ "$FAIL" -eq 0 ]; then
  echo "data_crate_closure_gate: $PASS passed"
  exit 0
fi
echo "data_crate_closure_gate: $PASS passed, $FAIL FAILED"
exit 1
