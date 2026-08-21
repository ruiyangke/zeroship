#!/usr/bin/env bash
# The zero-tokio invariant, as a check rather than a paragraph.
#
# AGENTS.md, "Key invariants": "Zero tokio in the stack. Everything is
# compio/io_uring." It then says the honest half out loud - the rule holds for
# code we write, and NOT for the dependency graph, because `cyper` pulls
# `hyper`, which pulls tokio. Nothing has ever checked either half.
#
# WHY A GATE THAT SIMPLY BANS TOKIO WOULD BE WORSE THAN NOTHING. It would be red
# on main the day it landed, and a gate that is red on arrival gets disabled or
# ignored. Three checks in this repo were red in CI for days with nobody acting
# on them, and in each case the reason was that nobody COULD act. So this gate
# does not ban the tokio edge. It PINS it, and rules on any movement.
#
# THE THING THIS PROTECTS AGAINST HAS HAPPENED, once, and was caught by a human:
# b72fee096 (2026-04-30) added crates/sandbox-agent with
#   tokio = { version = "1", features = ["rt-multi-thread", ...] }
# and a comment arguing the crate was isolated from the workspace stack. Two
# commits later 64e738f64 replaced it with compio/ntex. It was reverted the same
# day only because somebody read the diff; nothing in CI had an opinion.
#
# ---------------------------------------------------------------------------
# THE THREE ARMS, and what each one alone would miss
# ---------------------------------------------------------------------------
#
# 1. `declared` - no manifest WE OWN names tokio, in any dependency table.
#    This is the invariant's own wording and the only arm that is a plain ban.
#    It reads cargo's parse of each manifest, not the text, so a dependency
#    renamed onto tokio (`foo = { package = "tokio" }`) is caught by the crate
#    NAME rather than by the key, and dev- and build-dependencies count -
#    `cargo tree -e normal` cannot see either, and a dev-dependency still links
#    tokio into the test binaries.
#
# 2. `carriers` - the exact set of THIRD-PARTY packages that reach tokio in the
#    built graph, pinned. Fails when the set changes IN EITHER DIRECTION.
#    Arm 1 cannot see this: nothing we own declares tokio today and the edge is
#    there anyway.
#
# 3. `entrypoints` - the exact set of OUR crates that take a direct dependency
#    on one of those carriers, pinned. This is the "who has to change" list, and
#    it is the arm that answers a question arms 1 and 2 cannot: a new crate of
#    ours reaching for the tokio-carrying HTTP client is invisible to both, and
#    is precisely the drift that grows the edge.
#
# WHY THE PINNED SETS ARE NOT THE FULL REACHABILITY CLOSURE. 21 of our 29 crates
# can reach tokio today, purely by depending on zeroship-core. Pinning that set
# would go red on ordinary internal dependency edits that have nothing to do
# with tokio, which is the churn that gets a gate deleted. The two sets pinned
# here move only when someone changes what carries tokio or who touches it.
#
# THE PRICE OF THAT CHOICE, stated because it was paid on 2026-08-21.
# zeroship-gatekit LEFT the reachable set that day - it stopped depending on
# zeroship-core, so it stopped linking hyper, and was then deleted outright
# hours later - and this gate was green before
# the change and green after it, in the same words. Neither pinned set moved,
# because gatekit never NAMED a carrier; it reached tokio through zeroship-core,
# and core is still on both lists. So: this gate rules on whether the accepted
# edge has moved, not on how many crates sit behind it. A crate leaving or
# joining the reachable closure without touching a carrier directly is invisible
# here, by design, and the only number that shifts is the "packages reaching
# tokio ruled on" line, which nothing compares against a pin. Read that line if
# you want to know the closure size; it went 26 to 25 that day.
#
# WHY NOT A COUNT-BASED RATCHET ("at most N packages reach tokio"). A count
# cannot tell "we removed one" from "we added one and removed two". The sets are
# small enough - 4 and 9 - that naming them costs nothing and says which.
#
# THIS GATE WILL GO RED WHEN THE EDGE IS REMOVED, and that is deliberate. The
# `investigate/cyper-tokio-removal` branch exists to delete this edge; when it
# lands, arm 2 finds an empty carrier set, refuses, and says so. The fix is to
# delete the pins here and rewrite the AGENTS.md paragraph in the same commit.
# That is the point: the alternative is a docs paragraph that still describes an
# edge nobody has had since.
#
# WHY SHELL AND NOT A RUST GATE CRATE. The whole gate is set arithmetic over
# `cargo metadata` and `cargo tree`, both of which resolve from the manifests
# and the lockfile without compiling anything - this runs in about two seconds
# on a cold target/. A Rust gate would mean COMPILING a crate graph in order to
# read manifests, and while `crates/zeroship-gatekit` existed it would have
# meant compiling hyper and tokio to notice hyper and tokio, because that crate
# reached the edge through zeroship-core. It was deleted on 2026-08-21 and
# every gate is shell again.
#
# MEASURED DISCRIMINATION, 2026-08-20, each mutation confirmed present with
# `git diff` before the red run and absent after, and each followed by a green
# re-run on the restored tree:
#
#   tokio = { version = "1", ... } in crates/config-contract  -> red, arm
#       `declared`, naming zeroship-config-contract
#   rt = { package = "tokio", version = "1" } in the same file -> red, arm
#       `declared`, naming tokio; a text grep for `tokio =` sees nothing here
#   tokio in the root [workspace.dependencies], and the renamed form there
#       -> red, arm `declared`, naming Cargo.toml; cargo metadata carries no
#       virtual-manifest table, so this is the arm's second, hand-rolled half
#   "tokio1" added to lettre's feature list in crates/mailer -> red, arm
#       `carriers`, naming lettre. NO manifest declares tokio and NO package
#       enters the lockfile; one feature word moves the edge. This is why the
#       carrier arm reads `cargo tree`, which resolves features, and not
#       `cargo metadata`, whose graph already lists lettre's optional tokio
#       dependency and so cannot tell the two states apart.
#   cyper = { workspace = true } in crates/config-contract -> red, arm
#       `entrypoints`, naming zeroship-config-contract
#   a name added to PINNED_CARRIERS that nothing reaches -> red, "pinned but
#       no longer reaching tokio", which is the removal direction
#
# THE ONE-VARIABLE CONTROL: `url = { version = "2", features = ["serde"] }`,
# added to the same file, in the same table, in the same edit shape -> GREEN.
# The gate reacts to tokio, not to a Cargo.toml having been touched.
#
# WHAT THIS DOES NOT CHECK. It does not read source: a crate could use tokio
# types re-exported by something else and this would not know. It does not rule
# on `third_party/zero-migrate`, which is its own cargo workspace and is
# excluded from ours. It says nothing about whether the accepted edge SHOULD be
# accepted - only that it has not moved.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init zero_tokio

# ---------------------------------------------------------------------------
# THE PINS. Re-measured 2026-08-21 by running this gate, whose two arms print
# the sets they derive. Both came back IDENTICAL to the 2026-08-20 measurement
# on 050f95508, across the commit that moved the secret table out of
# zeroship-core and the commit that moved it back.
# ---------------------------------------------------------------------------
#
# Third-party packages that reach tokio in the built graph. Measured with
#   cargo tree --workspace -e normal -i tokio --prefix none --format '{p}'
# The chain is one entry point wide: cyper is the only one of these our own
# crates name. cyper-core and hyper-util are cyper's, and hyper is the package
# that actually declares tokio.
PINNED_CARRIERS="cyper cyper-core hyper hyper-util"
#
# Our crates that name a carrier directly. Measured from `cargo metadata`
# `.packages[].dependencies[].name`, so it includes dev- and build-dependencies;
# all nine are plain normal dependencies on cyper today. NOTE that
# zeroship-core is here AND is reached a second way, through zeroship-bundle ->
# compio-s3 -> cyper; both were still live at the re-measurement.
PINNED_ENTRYPOINTS="compio-s3 zeroship-auth zeroship-control zeroship-core \
zeroship-gateway zeroship-mailer zeroship-plugin-workflow zeroship-runtime \
zeroship-worker"

fail=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

command -v cargo >/dev/null 2>&1 || {
  echo "REFUSED: no cargo on PATH. This gate derives every set from cargo's own" >&2
  echo "         resolution and would otherwise inspect nothing and exit 0." >&2
  exit 1
}

if ! (cd "$ROOT" && cargo metadata --format-version 1) > "$TMP/meta.json" 2> "$TMP/meta.err"; then
  echo "REFUSED: cargo metadata failed; no set can be derived." >&2
  cat "$TMP/meta.err" >&2
  exit 1
fi

# `sort -u` over a whitespace-separated list, so pins may be written in any
# order and compared as sets. The unquoted expansion is the point: the pins are
# written as one whitespace-separated string and word splitting is what turns
# them into elements.
# shellcheck disable=SC2086
as_set() { printf '%s\n' $1 | sed '/^$/d' | LC_ALL=C sort -u; }

# ---------------------------------------------------------------------------
# ARM 1: no manifest we own declares tokio.
# ---------------------------------------------------------------------------
#
# `.packages[].dependencies[].name` is the DEPENDED-ON CRATE's name after cargo
# has applied any `package = ` rename, and the array covers normal, dev, build
# and target-specific tables alike.
jq -r '
  (.workspace_members) as $ws
  | .packages[]
  | select(.id as $id | $ws | index($id))
  | . as $p
  | $p.dependencies[]
  | select(.name == "tokio" or (.name | startswith("tokio-")))
  | "\($p.name)\t\(.name)\t\(.kind // "normal")\t\($p.manifest_path)"
' "$TMP/meta.json" > "$TMP/declared.txt" 2>"$TMP/declared.err" || {
  echo "REFUSED: the manifest query failed." >&2; cat "$TMP/declared.err" >&2; exit 1
}

members_checked=$(jq -r '(.workspace_members) as $ws
  | [.packages[] | select(.id as $id | $ws | index($id))] | length' "$TMP/meta.json")

# The virtual root manifest is not a package, so cargo metadata does not carry
# its `[workspace.dependencies]` table. Read the keys, and the renamed form.
# A section header is `[` followed by a letter, so a value that happens to start
# a line with `[` does not silently end the table scan.
awk '
  /^[[:space:]]*\[[A-Za-z]/ { in_ws = ($0 ~ /^[[:space:]]*\[workspace\.dependencies\][[:space:]]*$/); next }
  !in_ws { next }
  /^[[:space:]]*#/ { next }
  /^[[:space:]]*[A-Za-z0-9_-]+[[:space:]]*=/ {
    key = $1
    if (key ~ /^tokio(-|$)/) { print "Cargo.toml\t[workspace.dependencies]\t" key }
    else if ($0 ~ /package[[:space:]]*=[[:space:]]*"tokio(-[A-Za-z0-9_-]+)?"/) {
      print "Cargo.toml\t[workspace.dependencies]\t" key " (renamed onto tokio)"
    }
  }
' "$ROOT/Cargo.toml" > "$TMP/root-declared.txt"

manifests_checked=$((members_checked + 1))
declared_hits=$(( $(wc -l < "$TMP/declared.txt") + $(wc -l < "$TMP/root-declared.txt") ))

# DERIVED, not written down. This line carried "(30 workspace members + the
# virtual root)" as a literal, and a crate landing made it 31 while the
# parenthetical still said 30 - a parenthetical that disagrees with the number
# beside it is the census problem AGENTS.md warns about, in miniature. The
# count is back to 30 today and this line does not care.
echo "manifests ruled on: $manifests_checked ($members_checked workspace members + the virtual root)"
if [ "$declared_hits" -ne 0 ]; then
  echo "FAIL: a manifest in this workspace declares tokio."
  echo "      AGENTS.md, Key invariants: zero tokio in the stack. The transitive"
  echo "      edge through cyper is the documented exception; a DECLARATION is"
  echo "      not covered by it. This happened once before, in b72fee096, and"
  echo "      was reverted two commits later."
  sed 's/^/      /' "$TMP/declared.txt"
  sed 's/^/      /' "$TMP/root-declared.txt"
  fail=1
fi
# Floor 20 against 30: a query that stops matching the metadata shape returns an
# empty array and this arm would otherwise print the same clean line.
gate_arm declared "$manifests_checked" 20 || fail=1

# ---------------------------------------------------------------------------
# ARM 2: the third-party carriers of tokio, pinned.
# ---------------------------------------------------------------------------
#
# Run under default features AND --all-features, and rule on the union. The two
# agree today; they would not if a carrier hid behind an off-by-default feature,
# and CI lints this workspace with a features flag of its own.
jq -r '(.workspace_members) as $ws
  | [.packages[] | select(.id as $id | $ws | index($id)) | .name] | sort | .[]' \
  "$TMP/meta.json" > "$TMP/ws-names.txt"

tree_run() {
  (cd "$ROOT" && cargo tree --workspace -e normal -i tokio \
      --prefix none --format '{p}' "$@") 2> "$TMP/tree.err"
}

: > "$TMP/reachers.txt"
edge_gone=0
for feature_mode in default all; do
  if [ "$feature_mode" = all ]; then
    tree_run --all-features > "$TMP/tree.out"
  else
    tree_run > "$TMP/tree.out"
  fi
  status=$?
  if [ "$status" -ne 0 ]; then
    if grep -q "did not match any packages" "$TMP/tree.err"; then
      edge_gone=1
      continue
    fi
    echo "REFUSED: cargo tree failed under $feature_mode features." >&2
    cat "$TMP/tree.err" >&2
    exit 1
  fi
  # `--prefix none` still marks a repeated subtree with a trailing ' (*)'.
  sed 's/ (\*)$//' "$TMP/tree.out" | awk 'NF { print $1 }' >> "$TMP/reachers.txt"
done

LC_ALL=C sort -u "$TMP/reachers.txt" | sed '/^tokio$/d' > "$TMP/reachers-set.txt"
reachers_examined=$(wc -l < "$TMP/reachers-set.txt")
LC_ALL=C comm -23 "$TMP/reachers-set.txt" "$TMP/ws-names.txt" > "$TMP/carriers.txt"

if [ "$edge_gone" -eq 1 ]; then
  echo "FAIL: tokio is no longer in this workspace's dependency graph."
  echo "      THIS IS THE GOAL, NOT A REGRESSION - see the"
  echo "      investigate/cyper-tokio-removal branch. It is red because the"
  echo "      accepted edge is written down in two places that must now change"
  echo "      together: PINNED_CARRIERS and PINNED_ENTRYPOINTS in this file, and"
  echo "      the 'Key invariants' paragraph in AGENTS.md that still says"
  echo "      libtokio-*.rlib is built. Delete both in the commit that removes"
  echo "      the edge, and delete the two pinned arms with them."
  fail=1
fi

echo "packages reaching tokio ruled on: $reachers_examined"
printf 'third-party carriers: %s\n' "$(tr '\n' ' ' < "$TMP/carriers.txt")"

as_set "$PINNED_CARRIERS" > "$TMP/carriers-pinned.txt"
if ! LC_ALL=C diff -q "$TMP/carriers-pinned.txt" "$TMP/carriers.txt" >/dev/null; then
  added=$(LC_ALL=C comm -13 "$TMP/carriers-pinned.txt" "$TMP/carriers.txt" | tr '\n' ' ')
  gone=$(LC_ALL=C comm -23 "$TMP/carriers-pinned.txt" "$TMP/carriers.txt" | tr '\n' ' ')
  echo "FAIL: the set of third-party packages that reach tokio has changed."
  [ -n "${added// }" ] && echo "      NOW REACHING TOKIO and not pinned: $added"
  [ -n "${gone// }" ]  && echo "      pinned but no longer reaching tokio: $gone"
  echo "      An addition means a new dependency brought an async runtime in."
  echo "      Removing it is the default answer; accepting it means editing"
  echo "      PINNED_CARRIERS here and the AGENTS.md invariant in one commit."
  fail=1
fi
# Floor 8 against 25 (4 carriers plus 21 of our crates plus nothing else - the
# parenthetical used to say 21 when the total beside it said 26, which cannot
# both be true; the count was 22 of ours until zeroship-gatekit left on
# 2026-08-21, and the arithmetic has agreed since). This
# arm's real self-check is the pinned comparison - any extractor breakage that
# changes the set shows up as a diff - but the floor catches the one shape the
# comparison cannot: a tree that stops being produced at all.
gate_arm carriers "$reachers_examined" 8 || fail=1

# ---------------------------------------------------------------------------
# ARM 3: our crates that name a carrier directly, pinned.
# ---------------------------------------------------------------------------
#
# Computed against the OBSERVED carriers, not the pinned ones, so that when arm
# 2 is red this arm reports what is actually there rather than a fiction.
jq -r --rawfile carriers "$TMP/carriers.txt" '
  ($carriers | split("\n") | map(select(length > 0))) as $carrier_names
  | (.workspace_members) as $ws
  | .packages[]
  | select(.id as $id | $ws | index($id))
  | . as $p
  | $p.dependencies[]
  | select(.name as $n | $carrier_names | index($n))
  | $p.name
' "$TMP/meta.json" | LC_ALL=C sort -u > "$TMP/entrypoints.txt"

echo "workspace crates ruled on: $members_checked"
printf 'crates naming a carrier directly: %s\n' "$(tr '\n' ' ' < "$TMP/entrypoints.txt")"

as_set "$PINNED_ENTRYPOINTS" > "$TMP/entrypoints-pinned.txt"
if ! LC_ALL=C diff -q "$TMP/entrypoints-pinned.txt" "$TMP/entrypoints.txt" >/dev/null; then
  added=$(LC_ALL=C comm -13 "$TMP/entrypoints-pinned.txt" "$TMP/entrypoints.txt" | tr '\n' ' ')
  gone=$(LC_ALL=C comm -23 "$TMP/entrypoints-pinned.txt" "$TMP/entrypoints.txt" | tr '\n' ' ')
  echo "FAIL: the set of our crates that depend directly on a tokio carrier has"
  echo "      changed."
  [ -n "${added// }" ] && echo "      newly on the boundary: $added"
  [ -n "${gone// }" ]  && echo "      no longer on the boundary: $gone"
  echo "      Every name here is a crate the cyper-tokio removal has to touch."
  echo "      Adding one grows that job; if it is deliberate, add it to"
  echo "      PINNED_ENTRYPOINTS in the same commit."
  fail=1
fi
gate_arm entrypoints "$members_checked" 20 || fail=1

gate_arms_finish || fail=1
[ "$fail" -eq 0 ] || { echo "ZERO TOKIO GATE: FAILED" >&2; exit 1; }
echo "ZERO TOKIO GATE: passed"
