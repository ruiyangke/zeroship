#!/usr/bin/env bash
# ============================================================================
# NO WORKSPACE MEMBER MAY ROUTE ONE OF ITS OWN FEATURES ONTO A DEV-DEPENDENCY.
#
# That is, no `[features]` entry of the form `dep/feat` (or the weak `dep?/feat`)
# where `dep` is declared ONLY in `[dev-dependencies]`.
#
# WHAT WENT WRONG. Until 2026-09-04 `libs/compio-postgres/Cargo.toml` carried
# nine such routes, all onto the `tokio-postgres` differential oracle:
#
#     with-serde_json-1 = [
#         "postgres-types/with-serde_json-1",
#         "tokio-postgres/with-serde_json-1",   <- the dev-dependency
#     ]
#
# `cargo check --workspace --all-targets` could not compile the workspace. It
# exited 101 with 8 errors, NONE of them in code anybody here wrote:
#
#     error[E0432]: unresolved import `serde_1`
#       --> .../postgres-types-0.2.14/src/serde_json_1.rs:3:5
#
# THE MECHANISM, measured off the verbose rustc invocation for the registry
# postgres-types. Cargo 1.94.0 propagates the feature NAME down a `dep/feat`
# chain whose `dep` is a dev-dependency, but not the optional-dependency
# activation that feature carries, once a DIFFERENT package turns the feature on
# across a normal dependency edge. The unit came out as:
#
#     --cfg feature="with-serde_json-1" --cfg feature="with-uuid-1" ...
#     --extern bytes --extern chrono_04 --extern postgres_protocol --extern time_03
#
# Features on, `--extern serde_json_1` and `--extern uuid_1` absent, so
# postgres-types compiled `src/serde_json_1.rs` against a crate that was never
# linked. The split is diagnostic: the externs that WERE passed are the ones the
# dev-dep declaration named directly; the ones missing are exactly the ones
# routed through `[features]`.
#
# It needs BOTH `--all-targets` (without dev targets the oracle is not in the
# unit graph at all) and a second selected package (a package activating its own
# feature is handled correctly). `cargo check -p compio-postgres --all-targets
# --all-features` is green; `cargo check -p compio-postgres -p zeroship-authz
# --all-targets` is red. Resolver version is not the variable - "1", "2" and "3"
# all reproduce.
#
# WHY A GATE AND NOT JUST THE COMPILER. Only two of the nine routes ever bit,
# because only `with-serde_json-1` and `with-uuid-1` are requested by any
# workspace member today. The other seven were latent and would have fired the
# moment a member asked for them, or under `--all-features`. Seven-ninths of the
# fix therefore has no failing compile attached to it, and this gate is what
# holds them. It also states the rule, which the error message never did: a
# reader who hits those 8 errors has no path from them to the manifest line.
#
# THE FIX SHAPE, for whoever is tempted to "tidy" the routes back. Declare the
# features on the dev-dependency itself:
#
#     tokio-postgres = { version = "=0.7.18", default-features = false,
#                        features = ["runtime", "with-serde_json-1", ...] }
#
# It costs no capability - a dev-only oracle going from conditionally-built
# codecs to always-built ones is strictly more, never less - and it does not
# touch what the crate SHIPS, whose features route onto the vendored
# postgres-types fork.
#
# WHAT THIS DOES NOT RULE ON. Whether the features a dev-dependency declares are
# the right ones, and whether `dep/feat` routes onto NORMAL dependencies are
# correct. Both are out of scope; this asks one question about one manifest
# shape.
#
# Run:  tests/dev_dep_feature_route_gate.sh
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init dev_dep_feature_route

command -v jq >/dev/null 2>&1 || {
  echo "  x REFUSED: jq is not on PATH; this gate cannot read cargo metadata." >&2
  exit 1
}
command -v cargo >/dev/null 2>&1 || {
  echo "  x REFUSED: cargo is not on PATH." >&2
  exit 1
}

# ---------------------------------------------------------------------------
# THE CLASSIFIER. One jq program, used against the real workspace AND against
# the synthetic control below, so the control cannot drift away from the thing
# it is controlling.
#
# A dependency counts as dev-only when it appears with kind "dev" and never with
# any other kind. A crate declared in BOTH [dependencies] and [dev-dependencies]
# is fine - cargo has the normal edge to hang the activation off - and the
# control exercises that case rather than assuming it.
#
# `dep:` is stripped after the slash filter, so plain `dep:foo` activations (no
# slash, and not routable at a feature anyway) never reach the classifier.
# `dep?/feat` weak routes are normalised to `dep`, because the defect does not
# care whether the route is weak.
#
# Emits one TSV row per route: package, feature, entry, dep, verdict.
# ---------------------------------------------------------------------------
CLASSIFY='
  .packages[] as $p
  | ($p.dependencies | map(select(.kind == "dev"))  | map(.rename // .name) | unique) as $dev
  | ($p.dependencies | map(select(.kind != "dev")) | map(.rename // .name) | unique) as $nondev
  | $p.features | to_entries[] as $f
  | $f.value[]
  | select(test("/"))
  | sub("^dep:"; "") as $entry
  | ($entry | sub("\\??/.*$"; "")) as $dep
  | [ $p.name, $f.key, $entry, $dep,
      (if (($dev | index($dep)) != null and (($nondev | index($dep)) == null))
       then "DEVONLY" else "ok" end) ]
  | @tsv
'

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# ---------------------------------------------------------------------------
# ARM 1: the synthetic control. Does the classifier still CLASSIFY?
#
# THIS ARM EXISTS BECAUSE ARM 2 AND ARM 3 CANNOT SEE THEIR OWN BLINDNESS. If the
# jq program stops matching - a renamed metadata field, a broken regex, an
# `index` that always returns null - the workspace scan enumerates its usual
# routes, flags none of them, and prints exactly what a clean tree prints. The
# arm counts are no help: they would be unchanged. So the same program is run
# over a fixture whose answer is known.
#
# It is a ONE-VARIABLE CONTROL. `pos` and `neg` differ only in the `kind` of the
# dependency the route points at; the feature shape, the entry spelling and the
# route count are identical. `both` covers the crate declared as normal AND dev.
#
# THE FLOOR EQUALS THE COUNT here, unlike the tree-derived arms below, and that
# is deliberate: the fixture is written into this file, so its size cannot drift
# by ordinary editing. Any number below 5 means the fixture stopped being read,
# which is the failure this arm is for.
# ---------------------------------------------------------------------------
cat > "$WORK/control.json" <<'CONTROL_JSON'
{
  "packages": [
    {
      "name": "pos",
      "dependencies": [ { "name": "oracle", "kind": "dev" } ],
      "features": { "f1": ["oracle/alpha"], "f2": ["oracle?/beta"] }
    },
    {
      "name": "neg",
      "dependencies": [ { "name": "oracle", "kind": null } ],
      "features": { "f1": ["oracle/alpha"], "f2": ["oracle?/beta"] }
    },
    {
      "name": "both",
      "dependencies": [
        { "name": "oracle", "kind": null },
        { "name": "oracle", "kind": "dev" }
      ],
      "features": { "f1": ["oracle/alpha"] }
    }
  ]
}
CONTROL_JSON

ctl_rows="$(jq -r "$CLASSIFY" "$WORK/control.json")" || ctl_rows=""
ctl_examined="$(printf '%s\n' "$ctl_rows" | grep -c . || true)"
ctl_flagged="$(printf '%s\n' "$ctl_rows" | grep -c 'DEVONLY$' || true)"
ctl_flagged_pkgs="$(printf '%s\n' "$ctl_rows" | awk -F'\t' '$5 == "DEVONLY" { print $1 }' | sort -u | tr '\n' ' ')"

CONTROL_OK=1
if [ "$ctl_flagged" -ne 2 ] || [ "$ctl_flagged_pkgs" != "pos " ]; then
  CONTROL_OK=0
  {
    echo "  FAIL control: the classifier flagged $ctl_flagged route(s) in package(s)" \
         "'${ctl_flagged_pkgs% }', expected 2 in 'pos'."
    echo "    The fixture is fixed, so this is a statement about the CLASSIFIER, not"
    echo "    about the tree. Until it is right, a clean workspace scan means nothing."
  } >&2
else
  echo "  ok   control: 2 dev-only route(s) flagged in 'pos', 0 in 'neg' and 'both'"
fi
gate_arm control "$ctl_examined" 5

# ---------------------------------------------------------------------------
# ARM 2 and ARM 3: the real workspace.
#
# `--no-deps` keeps this to members we own. Registry crates may route features
# onto their own dev-dependencies as much as they like; we cannot change them,
# and the defect is triggered by OUR manifests.
# ---------------------------------------------------------------------------
if ! cargo metadata --no-deps --format-version 1 --manifest-path "$ROOT/Cargo.toml" \
     > "$WORK/metadata.json" 2> "$WORK/metadata.err"; then
  echo "  x REFUSED: cargo metadata failed; this gate ruled on nothing." >&2
  sed 's/^/    /' "$WORK/metadata.err" >&2
  exit 1
fi

members="$(jq -r '.packages | length' "$WORK/metadata.json")"
rows="$(jq -r "$CLASSIFY" "$WORK/metadata.json")" || rows=""
routes="$(printf '%s\n' "$rows" | grep -c . || true)"
violations="$(printf '%s\n' "$rows" | grep -c 'DEVONLY$' || true)"

echo "  workspace members: $members; dep/feature routes: $routes; dev-only: $violations"

# MEASURED 2026-09-04: 44 members. The floor sits well under it - a workspace
# does not lose fifteen crates by accident, and a `--no-deps` enumeration that
# collapses (wrong manifest path, a cargo that started resolving differently)
# lands far below this rather than just under it.
gate_arm members "$members" 25

# MEASURED 2026-09-04, immediately after the fix: 22 `dep/feat` routes across
# all members, down from 31 - the fix deleted exactly the nine dev-only ones
# (31 - 9 = 22), leaving every `postgres-types/...` route in place. Floor at 12,
# roughly half, so that ordinary feature-table editing never reaches it but a
# classifier or enumeration that stops matching does.
gate_arm routes "$routes" 12

FAILED=0
if [ "$violations" -ne 0 ]; then
  FAILED=1
  {
    echo
    echo "DEV-DEPENDENCY FEATURE ROUTE(S) FOUND: $violations"
    printf '%s\n' "$rows" | awk -F'\t' '$5 == "DEVONLY" {
      printf "  %s: feature `%s` routes onto `%s`, which is declared only in [dev-dependencies]\n", $1, $2, $3
    }'
    echo
    echo "  Cargo 1.94.0 turns the feature ON down this chain without linking the"
    echo "  optional dependencies it activates, so a transitive crate compiles with"
    echo "  the cfg set and the extern missing. It breaks"
    echo "  \`cargo check --workspace --all-targets\` with errors in registry code,"
    echo "  pointing nowhere near the manifest that caused them."
    echo
    echo "  Declare the feature on the dev-dependency itself instead:"
    echo "    <dep> = { version = \"...\", features = [\"<feat>\", ...] }"
    echo "  A dev-only dependency built with more codecs than it needs costs"
    echo "  nothing; a workspace that cannot be checked costs every gate downstream."
  } >&2
else
  echo "  ok   no workspace member routes a feature onto a dev-only dependency"
fi

if [ "$CONTROL_OK" -ne 1 ]; then FAILED=1; fi

gate_arms_finish || exit 1
[ "$FAILED" -eq 0 ] || { echo "DEV-DEP FEATURE ROUTE GATE: FAILED" >&2; exit 1; }
echo "DEV-DEP FEATURE ROUTE GATE: passed"
