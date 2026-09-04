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
# THE FOUR ARMS, and what each one alone would miss
# ---------------------------------------------------------------------------
#
# 1. `declared` - no manifest WE OWN names tokio, in any dependency table.
#    This is the invariant's own wording and the only arm that is a plain ban.
#    It reads cargo's parse of each manifest, not the text, so a dependency
#    renamed onto tokio (`foo = { package = "tokio" }`) is caught by the crate
#    NAME rather than by the key, and build-dependencies count - `cargo tree
#    -e normal` cannot see them.
#
#    DEV-DEPENDENCIES ARE EXEMPT, by an operator decision on 2026-08-24. They
#    were banned until then, on the reasoning that a dev-dependency still links
#    tokio into the test binaries. That is true and is now accepted: the point
#    of the invariant is that NO TOKIO RUNTIME DRIVES OUR I/O IN A SHIPPED
#    BINARY, and a test binary is not shipped. What it buys is the ability to
#    run tokio-postgres beside compio-postgres in one process and diff their
#    behaviour against the same server - the strongest oracle available for a
#    port, and one this crate has been hardening without.
#
#    The exemption is deliberately narrow. `kind == "dev"` only: a normal or
#    build dependency is still a hard red, and so is tokio in the root
#    `[workspace.dependencies]`, because that table carries no kind and a
#    member inherits it with `workspace = true` into whichever table it likes.
#    A dev-only tokio must therefore be declared in the member's own
#    `[dev-dependencies]` with its own version, where the kind is unambiguous
#    and this arm can see it.
#
# 2. `root_workspace_deps` - the root `[workspace.dependencies]` table, which
#    arm 1 structurally cannot see: the virtual manifest is not a package, so
#    `cargo metadata` carries no entry for it. This half is hand-rolled, and it
#    is the one place in this gate where nothing upstream has already parsed the
#    file for us. It therefore keys on TOML STRUCTURE rather than on line text -
#    see the block above the scanner for the eleven spellings that walked past
#    the text-keyed predicate it replaced, the cheapest of which was a trailing
#    comment on the section header. Its count is the number of dependency
#    entries the scan classified, NOT the manifest count arm 1 reports: arm 1's
#    number is `members + 1` whether this scan read 112 entries or none.
#
# 3. `carriers` - the exact set of THIRD-PARTY packages that reach tokio in the
#    built graph, pinned. Fails when the set changes IN EITHER DIRECTION.
#    Arm 1 cannot see this: nothing we own declares tokio today and the edge is
#    there anyway.
#
# 4. `entrypoints` - the exact set of OUR crates that take a direct dependency
#    on one of those carriers, pinned. This is the "who has to change" list, and
#    it is the arm that answers a question the others cannot: a new crate of
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
#       virtual-manifest table, so this is the hand-rolled scan, whose own
#       population is arm `root_workspace_deps`
#
# RE-MEASURED 2026-09-04, when the root scan stopped keying on line text. Each
# mutation applied to the REAL root Cargo.toml, gate run, tree restored from a
# byte copy and `git diff -- Cargo.toml` confirmed empty afterwards:
#
#   [workspace.dependencies] # shared versions   + tokio = { ... }  -> exit 1
#   tokio={ version = "1" }                                         -> exit 1
#   [workspace.dependencies.tokio] / version = "1"                  -> exit 1
#   [workspace.dependencies.rt]    / package = "tokio"              -> exit 1
#   rationale = "we vendored tokio here" under [workspace.metadata.notes]
#       -> exit 1, via the unclassified-mention backstop
#
#   All five printed exit 0 - clean, indistinguishable from an untouched tree -
#   under the predicate this replaced. The first is the cheapest: one trailing
#   comment.
#
#   `dependencies = { tokio = { version = "1" } }` under `[workspace]` is caught
#   in the corpus but cannot be measured against the real manifest, which
#   already declares `[workspace.dependencies]` as a table: TOML refuses the
#   duplicate and cargo metadata refuses first, so the gate is red for the wrong
#   reason. That is what the 16-manifest corpus in the scratch harness is for.
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
# The gate reacts to tokio, not to a Cargo.toml having been touched. Re-run
# 2026-09-04 in both of the new shapes - `urlx = { ... }` under a header that
# carries a trailing comment, and `rationale = "we vendored compio here"` under
# `[workspace.metadata.notes]` - both exit 0.
#
# WHAT THIS DOES NOT CHECK. It does not read source: a crate could use tokio
# types re-exported by something else and this would not know. It does not rule
# on `third_party/zero-migrate`, which is its own cargo workspace and is
# excluded from ours. It says nothing about whether the accepted edge SHOULD be
# accepted - only that it has not moved. And the root scan is a TOML-structure
# scanner, not a TOML parser: it refuses on a multi-line string rather than read
# past one, and any member manifest's own tables are arm 1's business, not its.
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
# `.packages[].dependencies[].name`, so it includes dev- and build-dependencies.
# NOTE that zeroship-core is here AND is reached a second way, through
# zeroship-bundle -> compio-s3 -> cyper; both were still live at the
# re-measurement.
#
# ALL NINE are plain normal dependencies on cyper. The set went nine -> ten ->
# nine within one day: zeroship-cdc-transport-spike joined on 2026-09-03 with a
# DEV dependency - the only non-normal entry this list has ever carried - and was
# deleted the same day, once the four transport properties it existed to prove
# were measured and written into
# `docs/proposals/2026-08-28-cdc-service.md`.
#
# That round trip is worth recording, because it is the mechanism working in
# BOTH directions: the gate went red on the addition and red again on the
# deletion, and this pin and the AGENTS.md sentence moved with it each time. A
# pin that only resists growth would have stayed green while the set shrank.
#
# This arm reads every dependency kind on purpose, so a new crate cannot reach
# the tokio-carrying HTTP client through the dev-dependency exemption that arm 1
# grants.
PINNED_ENTRYPOINTS="compio-s3 zeroship-auth \
zeroship-control zeroship-core zeroship-gateway zeroship-mailer \
zeroship-plugin-workflow zeroship-runtime zeroship-worker"

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
  | select((.kind // "normal") != "dev")
  | "\($p.name)\t\(.name)\t\(.kind // "normal")\t\($p.manifest_path)"
' "$TMP/meta.json" > "$TMP/declared.txt" 2>"$TMP/declared.err" || {
  echo "REFUSED: the manifest query failed." >&2; cat "$TMP/declared.err" >&2; exit 1
}

members_checked=$(jq -r '(.workspace_members) as $ws
  | [.packages[] | select(.id as $id | $ws | index($id))] | length' "$TMP/meta.json")

# The virtual root manifest is not a package, so cargo metadata does not carry
# its `[workspace.dependencies]` table. This half is hand-rolled, and it keys on
# TOML STRUCTURE - table paths, dotted keys, quoting, comments - rather than on
# a line's exact text.
#
# IT USED TO KEY ON TEXT, and eleven valid spellings walked past it. Every one
# below is TOML that `tomlq -c '.workspace.dependencies | keys'` reports as
# declaring tokio (or a rename onto it), and every one produced ZERO hits from
# the predicate this replaced, measured 2026-09-04 against a 16-manifest corpus:
#
#   [workspace.dependencies] # shared versions    trailing comment on the header
#   [workspace.dependencies]<TAB># versions       the same, with a tab
#   tokio={ version = "1" }                       no space before `=`
#   "tokio" = { version = "1" }                   quoted key
#   [workspace.dependencies.tokio]                the dep as its own table
#   tokio.version = "1"                           dotted key
#   ["workspace".dependencies]                    quoted header segment
#   dependencies.tokio = "1"       under [workspace]
#   dependencies = { tokio = "1" } under [workspace]
#   [workspace.dependencies.rt] + package = "tokio"    rename, split over lines
#   rt.package = "tokio"                               rename, dotted
#
# The cheapest is the first: ONE trailing comment on the section header made the
# old `^\[workspace\.dependencies\]\s*$` test fail, `in_ws` stayed 0, and the
# whole table went unread while the gate printed exactly what a clean tree
# prints. That table is named in AGENTS.md's zero-tokio invariant precisely
# because it carries no dependency kind and a member inherits it with
# `workspace = true` into whichever table it likes - so a bypass there is a
# bypass of the invariant, not of a convenience.
#
# WHAT THE SCANNER DOES. It tracks the current table path across `[a.b.c]` and
# `[[a.b]]` headers, splits dotted keys, unquotes segments, strips comments with
# a quote-aware pass (so a `#` inside a string value is not a comment), and
# tracks bracket depth so a multi-line array's continuation lines are never read
# as headers. A declaration is a full path of `workspace.dependencies.<name>`
# with `<name>` matching `^tokio(-|$)`, or that path carrying `package =
# "tokio…"` in any of its three spellings.
#
# AND THEN A BACKSTOP, because a structural classifier still has arms and any
# arm can be missing one: after classification, ANY surviving `tokio` token in
# the comment-stripped manifest is itself a hit, reported as unclassified. That
# is what catches `dependencies = { tokio = "1" }` and anything nested deeper
# than this scanner walks. It is deliberately fail-closed: the root manifest
# mentions tokio six times today and all six are in whole-line comments, so the
# backstop is silent, and the day a seventh mention appears anywhere outside a
# comment this gate goes red and names the line. Rewording is the fix; the
# alternative is a classifier whose blind spots are silent.
#
# NOT A TOML PARSER, and it does not need to be. It refuses on a multi-line
# string rather than scan past one, and there are none in a Cargo manifest.
awk '
  BEGIN { SQ = sprintf("%c", 39); depth = 0; tn = 0; scanned = 0 }

  function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }

  # Truncate at the first `#` that is NOT inside a string.
  function strip_comment(s,   i, c, n, q) {
    n = length(s); q = ""
    for (i = 1; i <= n; i++) {
      c = substr(s, i, 1)
      if (q == "") {
        if (c == "#") return substr(s, 1, i - 1)
        if (c == "\"" || c == SQ) q = c
      } else if (q == "\"") {
        if (c == "\\") { i++; continue }
        if (c == "\"") q = ""
      } else if (c == SQ) q = ""
    }
    return s
  }

  # Net `[` minus `]` outside strings, so a multi-line array value is tracked by
  # structure and its continuation lines are never read as a table header.
  function bracket_delta(s,   i, c, n, q, d) {
    n = length(s); q = ""; d = 0
    for (i = 1; i <= n; i++) {
      c = substr(s, i, 1)
      if (q == "") {
        if (c == "\"" || c == SQ) { q = c; continue }
        if (c == "[") d++
        else if (c == "]") d--
      } else if (q == "\"") {
        if (c == "\\") { i++; continue }
        if (c == "\"") q = ""
      } else if (c == SQ) q = ""
    }
    return d
  }

  # Offset of the first `=` outside a string: the key/value separator.
  function find_eq(s,   i, c, n, q) {
    n = length(s); q = ""
    for (i = 1; i <= n; i++) {
      c = substr(s, i, 1)
      if (q == "") {
        if (c == "\"" || c == SQ) { q = c; continue }
        if (c == "=") return i
      } else if (q == "\"") {
        if (c == "\\") { i++; continue }
        if (c == "\"") q = ""
      } else if (c == SQ) q = ""
    }
    return 0
  }

  # A TOML dotted key into arr[1..n], unquoting each segment. This is what makes
  # `tokio.version = "1"`, `"tokio" = "1"` and `[workspace.dependencies.tokio]`
  # the same path as a plain `tokio = "1"`.
  function split_key(s, arr,   i, c, n, q, cur, cnt) {
    n = length(s); q = ""; cur = ""; cnt = 0
    for (i = 1; i <= n; i++) {
      c = substr(s, i, 1)
      if (q == "") {
        if (c == "\"" || c == SQ) { q = c; continue }
        if (c == ".") { cnt++; arr[cnt] = trim(cur); cur = ""; continue }
        cur = cur c
      } else if (q == "\"") {
        if (c == "\\") { i++; cur = cur substr(s, i, 1); continue }
        if (c == "\"") { q = ""; continue }
        cur = cur c
      } else {
        if (c == SQ) { q = ""; continue }
        cur = cur c
      }
    }
    cnt++; arr[cnt] = trim(cur)
    return cnt
  }

  function hit(where, what) { print "HIT\t" where "\t" what; fired = 1 }

  {
    fired = 0
    raw = $0
    if (index(raw, "\"\"\"") || index(raw, SQ SQ SQ))
      hit("Cargo.toml", "multi-line string: this scanner cannot classify the rest of the file")
    line = strip_comment(raw)
    t = trim(line)
    if (t == "") next

    if (depth <= 0 && substr(t, 1, 1) == "[") {
      inner = t
      if (substr(inner, 1, 2) == "[[") { sub(/^\[\[/, "", inner); sub(/\]\][ \t]*$/, "", inner) }
      else { sub(/^\[/, "", inner); sub(/\][ \t]*$/, "", inner) }
      tn = split_key(inner, T)
      path = ""
      for (i = 1; i <= tn; i++) path = path (i > 1 ? "." : "") T[i]
      if (tn >= 3 && T[1] == "workspace" && T[2] == "dependencies" && T[3] ~ /^tokio(-|$)/)
        hit("[" path "]", T[3])
      if (!fired && index(line, "tokio"))
        hit("[" path "]", "unclassified `tokio` in a table header")
      depth = 0
      next
    }

    if (depth > 0) {
      depth += bracket_delta(line)
      if (index(line, "tokio")) hit("(multi-line value)", "unclassified `tokio`")
      next
    }

    eq = find_eq(t)
    if (eq > 0) {
      keyspec = trim(substr(t, 1, eq - 1))
      val = trim(substr(t, eq + 1))
      kn = split_key(keyspec, K)
      pn = 0
      for (i = 1; i <= tn; i++) { pn++; P[pn] = T[i] }
      for (i = 1; i <= kn; i++) { pn++; P[pn] = K[i] }
      path = ""
      for (i = 1; i < pn; i++) path = path (i > 1 ? "." : "") P[i]

      if (pn >= 3 && P[1] == "workspace" && P[2] == "dependencies") {
        scanned++
        if (P[3] ~ /^tokio(-|$)/) hit("[" path "]", P[3])
        else if (P[pn] == "package" && val ~ /^"tokio(-[A-Za-z0-9_-]+)?"/)
          hit("[" path "]", P[3] " (renamed onto tokio)")
        else if (val ~ /package[ \t]*=[ \t]*"tokio(-[A-Za-z0-9_-]+)?"/)
          hit("[" path "]", P[3] " (renamed onto tokio)")
      }
      if (!fired && index(line, "tokio"))
        hit("[" path "]", "unclassified `tokio` in key `" keyspec "`")
      depth = bracket_delta(val)
      next
    }

    if (index(line, "tokio")) hit("(unparsed line)", "unclassified `tokio`")
  }

  END { print "COUNT\t" scanned }
' "$ROOT/Cargo.toml" > "$TMP/root-scan.txt"

# Two channels out of one pass, split on the leading field rather than on the
# shape of a message: `HIT` rows are findings, the single `COUNT` row is the
# arm-2 population. A scanner that dies mid-file emits neither, and the empty
# `root_keys` that results is a gate_arm refusal, not a zero.
awk -F'\t' '$1 == "HIT" { print "Cargo.toml\t" $2 "\t" $3 }' \
  "$TMP/root-scan.txt" > "$TMP/root-declared.txt"
root_keys=$(awk -F'\t' '$1 == "COUNT" { print $2 }' "$TMP/root-scan.txt")

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

# The root table gets its OWN arm, because `manifests_checked` cannot see it.
# That number is `members + 1` whatever the root scan did: the scan reading zero
# keys - which is exactly what a trailing comment on the section header used to
# produce - moved it not at all. Counting the entries the scan actually
# classified is the only number whose collapse means what it says.
#
# Floor 40 against 112. The 112 is DERIVED here and agrees exactly with an
# independent parser (`tomlq -r '.workspace.dependencies | keys | length'`
# reports 112 on the same file, measured 2026-09-04), which is the check a
# hand-kept expected-count cannot make. Nothing is pinned: dependencies come and
# go, and only a collapse crosses 40.
echo "root [workspace.dependencies] entries ruled on: $root_keys"
gate_arm root_workspace_deps "$root_keys" 40 || fail=1

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
